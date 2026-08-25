//! TIP 本体。ITfTextInputProcessor(Ex) として TSF に活性化され、
//! ITfKeyEventSink として打鍵を受け、ITfDisplayAttributeProvider として下線属性を提供し、
//! ITfCompositionSink として composition 終了通知を受ける。
//!
//! PART 2: composition/preedit、表示属性、自前候補ウィンドウ、エンジン IPC 連携、
//! エンジン自動起動と劣化動作を実装する。
//!
//! 単一スレッドアパートメント（STA）前提のため、内部状態は `Rc`/`Cell`/`RefCell` で持つ
//! （Send/Sync は不要）。COM 境界を越えて panic させないこと（IPC/COM 失敗は no-op に潰す）。

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{implement, IUnknown, IUnknownImpl, Interface, Ref, Result, GUID, HSTRING};
use windows::Win32::Foundation::{E_FAIL, HWND, RECT};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::{VARIANT, VT_I4};
use windows::Win32::UI::TextServices::{
    CLSID_TF_CategoryMgr, ITfCategoryMgr, ITfCompartment, ITfCompartmentMgr, ITfComposition,
    ITfCompositionSink, ITfCompositionSink_Impl, ITfContext, ITfContextView,
    ITfDisplayAttributeProvider, ITfDocumentMgr, ITfEditRecord, ITfEditSession, ITfFnConfigure,
    ITfFnConfigure_Impl, ITfFunction_Impl, ITfKeyEventSink, ITfKeystrokeMgr, ITfLangBarItemButton,
    ITfLangBarItemMgr, ITfLangBarItemSink, ITfSource, ITfTextEditSink, ITfTextEditSink_Impl,
    ITfTextInputProcessorEx, ITfTextInputProcessorEx_Impl, ITfTextInputProcessor_Impl,
    ITfTextLayoutSink, ITfTextLayoutSink_Impl, ITfThreadFocusSink, ITfThreadFocusSink_Impl,
    ITfThreadMgr, ITfThreadMgrEventSink, ITfThreadMgrEventSink_Impl, ITfThreadMgrEx,
    ITfUIElementMgr, TfLayoutCode, GUID_COMPARTMENT_EMPTYCONTEXT,
    GUID_COMPARTMENT_KEYBOARD_DISABLED, GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION,
    TF_CONTEXT_EDIT_CONTEXT_FLAGS, TF_ES_ASYNC, TF_ES_READ, TF_ES_READWRITE, TF_ES_SYNC,
    TF_LBI_ICON, TF_LBI_STATUS, TF_LBI_TEXT, TF_PRESERVEDKEY, TF_TMF_IMMERSIVEMODE,
};
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};

use crate::candidate_presenter::CandidatePresenter;
use crate::candidate_state::CandidateState;
use crate::candidate_uielement::BehaviorAction;
use crate::candidate_window::CandidateUI;
use crate::edit_session::{
    classify_composition_end_error, CancelComposition, CommitText, CommitUndoStart,
    CompositionEndStatus, EndCompositionOnly, FinishPredictionGhost, QueryCaretRect,
    QueryInputScopes, QueryMonitorAnchorRect, ReconvertCapture, ReconvertStart, RestoreText,
    StartOrUpdatePreedit, StartPredictionGhost,
};
use crate::globals::{
    ComObjectGuard, GUID_DISPLAY_ATTRIBUTE, GUID_DISPLAY_ATTRIBUTE_PREDICTION,
    GUID_DISPLAY_ATTRIBUTE_TARGET,
};
use crate::input_state::is_fresh_live;
use crate::input_state::preedit_after_candidates_closed;
use crate::input_state::InputState;
use crate::input_state::InsertStyle;
use crate::input_state::ReconvertKind;
use crate::llm_worker::{spawn_llm_worker, LlmOutcome, LlmSlot};
use crate::prediction_worker::{
    spawn_ipc_prediction_worker, warm_prediction_artifacts, IpcPredictionResult, PredictionSlot,
};

use ipc::client::EngineClient;
use ipc::protocol::{Request, Response, PROTO_VERSION};

/// プロセス内でエンジン用パイプ名を一意化するための連番（TextService インスタンス毎に +1）。
/// engine_pipe_name が stable_pipe_name を使うようになったため現在は未使用だが、将来の参照のために保持。
#[allow(dead_code)]
static NEXT_PIPE_SEQ: AtomicU32 = AtomicU32::new(0);

/// デバウンス間隔（ms）。打鍵が落ち着いてから変換するまでの待ち。
const DEBOUNCE_MS: u32 = 30;

/// 部分確定後の preedit 張り直しが edit-session 拒否された場合の再試行上限。
/// タイマは単発なので、永続拒否で 30ms ループを作らない範囲だけ再武装する。
const PARTIAL_REDRAW_RETRY_MAX: u8 = 5;

/// SetText 後の close-only 呼出し総数の上限（初回 EndComposition を含む）。
/// TF_E_LOCKED/TF_E_SYNCHRONOUS が恒常的な context であっても、打鍵を無期限に
/// 全消費する barrier へ退化させない。
const COMPOSITION_END_RETRY_MAX: u8 = 3;

/// 次の部分 preedit 再描画試行番号。None は上限到達（純関数＝単体テスト用）。
fn next_partial_redraw_retry(current: u8) -> Option<u8> {
    let next = current.saturating_add(1);
    (next <= PARTIAL_REDRAW_RETRY_MAX).then_some(next)
}

/// 初回 EndComposition 失敗を 1 と数え、総呼出し上限まで追加 retry を許可する。
fn next_composition_end_retry(current: u8) -> Option<u8> {
    let next = current.saturating_add(1);
    // `current` は既に実行済みの EndComposition 呼出し数。初回込み最大3回
    // なので count=2 の失敗後には追加呼出しを作らない。
    (next < COMPOSITION_END_RETRY_MAX).then_some(next)
}

/// TestKeyDown→KeyDown の物理イベント照合に使う軽量署名。
///
/// COM オブジェクトを強参照で保持すると context の lifecycle と相互参照しやすいため、
/// context は IUnknown identity の raw address だけを持つ。署名には正規化前の VK と
/// 正規化後の VK、lParam、修飾キーも含め、同じ VK の別イベントを予約として replay しない。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PendingEndKeySignature {
    pub(crate) context_identity: usize,
    pub(crate) raw_vk: u32,
    pub(crate) normalized_vk: u32,
    pub(crate) lparam: isize,
    pub(crate) modifiers: u8,
}

impl PendingEndKeySignature {
    pub(crate) fn from_context(
        context: &ITfContext,
        raw_vk: u32,
        normalized_vk: u32,
        lparam: isize,
        modifiers: u8,
    ) -> Option<Self> {
        let identity = context.cast::<IUnknown>().ok()?.as_raw() as usize;
        Some(Self {
            context_identity: identity,
            raw_vk,
            normalized_vk,
            lparam,
            modifiers,
        })
    }

    #[cfg(test)]
    fn synthetic(
        context_identity: usize,
        raw_vk: u32,
        normalized_vk: u32,
        lparam: isize,
        modifiers: u8,
    ) -> Self {
        Self {
            context_identity,
            raw_vk,
            normalized_vk,
            lparam,
            modifiers,
        }
    }
}

/// OnTestKeyDown が TRUE を返した pending-end のキーを、一つだけ次の OnKeyDown
/// と対応付ける状態。正規の TSF 契約は Test→Key の直列 pair であり、複数 outstanding は
/// キュー化しない。lifecycle 境界で無効化されるため、将来の同じ VK を誤って食わない。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PendingEndKeyReservation {
    reservation: Option<PendingEndKeyReservationEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingEndKeyReservationEntry {
    signature: PendingEndKeySignature,
    generation: u64,
}

impl PendingEndKeyReservation {
    fn reserve(&mut self, signature: PendingEndKeySignature, generation: u64) -> bool {
        if self.reservation.is_some() {
            return false;
        }
        self.reservation = Some(PendingEndKeyReservationEntry {
            signature,
            generation,
        });
        true
    }

    fn is_stale(&self, generation: u64) -> bool {
        self.reservation
            .is_some_and(|entry| entry.generation != generation)
    }

    fn is_occupied(&self) -> bool {
        self.reservation.is_some()
    }

    /// Test→Key pairの take は必ず slot を消費する。不一致や stale でも捨てることで、
    /// 後続の同じ VK/password/direct 入力を旧予約が食うことを防ぐ。
    fn take_if_matches(&mut self, signature: PendingEndKeySignature, generation: u64) -> bool {
        let Some(entry) = self.reservation.take() else {
            return false;
        };
        entry.generation == generation && entry.signature == signature
    }

    fn invalidate(&mut self) {
        self.reservation = None;
    }

    #[cfg(test)]
    fn signature(&self) -> Option<PendingEndKeySignature> {
        self.reservation.map(|entry| entry.signature)
    }

    #[cfg(test)]
    fn generation(&self) -> Option<u64> {
        self.reservation.map(|entry| entry.generation)
    }
}

/// OnTestKeyDown/OnKeyDown の予約判定。実際の reservation storage は
/// `PendingEndKeyReservation` にあり、両 COM 入口と lifecycle 境界が同じ production
/// methods を利用する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingEndTestDecision {
    Reserve,
    Busy,
    Normal,
}

/// IPC 要求の op 別締め切り。超過すると request_within が TimedOut を返し、既存の劣化枝に合流する。
const IPC_TIMEOUT_FAST: Duration = Duration::from_millis(250); // Insert/Backspace/Commit/Start/EndSession
const IPC_TIMEOUT_CONVERT: Duration = Duration::from_millis(1200); // Convert/Reconvert（Zenzai 推論に余裕）
const IPC_TIMEOUT_LIVE: Duration = Duration::from_millis(400); // LiveConvert（debounce 済・遅ければ捨てる）

/// A' INV5: pending（未読応答を owe）になってからこの時間を超えても drain できなければ
/// engine 真死とみなし drop_engine する（永久劣化の暴走ガード）。
const PENDING_MAX: Duration = Duration::from_millis(3000);

/// A' INV2: ドレインで回収した応答が「engine 側は部分確定を適用済み・TIP 側は未適用」の
/// 不整合を示すか（＝安全側で drop_engine すべきか）を判定する純関数（単体テスト用）。
/// `LiveResult` の committed が非空文字列のときだけ真。それ以外（committed 無し/空、
/// LiveResult 以外の応答、破棄してよいもの）は偽（黙って破棄）。
fn drained_needs_drop(resp: &Response) -> bool {
    matches!(
        resp,
        Response::LiveResult { committed: Some(s), .. } if !s.is_empty()
    )
}

/// A' 送信前ドレインの結果。prepare_send が返し、呼び出し側の要求発行可否を決める。
enum DrainOutcome {
    /// pending を解消（または元々無し）した。この要求を送ってよい。
    Proceed,
    /// pending をドレインできず維持。要求を送らず None（劣化継続、接続は保持）。
    StillPending,
    /// ドレイン中に接続破棄した（INV2 不整合 / パイプ破断 / INV5 暴走ガード）。要求を送らず None。
    Dropped,
}

/// elapsed が tier の半分を超えたら遅延ログを出す（純関数＝単体テスト用）。
fn should_log_slow(elapsed: Duration, tier: Duration) -> bool {
    elapsed > tier / 2
}

/// スリープ復帰世代の刈り取り判定（純関数＝単体テスト用）。
/// 戻り値: None=世代変化なし / Some(true)=復帰かつ idle → drop する / Some(false)=復帰だが busy → 温存。
fn resume_poll_action(gen: u32, last: u32, busy: bool) -> Option<bool> {
    if gen == last {
        None
    } else {
        Some(!busy)
    }
}

/// cold start ②: Activate 時プリスポーンの判定（純関数＝単体テスト用）。
/// client 無し・spawn 未試行・バックオフ許可のときだけ spawn する。既接続/試行済み/クールダウン中は
/// 何もしない（prespawn は best-effort — 状態を変えず、初回打鍵の ensure_engine フルコースを妨げない）。
fn should_prespawn(has_client: bool, spawn_attempted: bool, backoff_allows: bool) -> bool {
    !has_client && !spawn_attempted && backoff_allows
}

/// `request_within` を計測ログ付きで呼ぶ薄いラッパ。挙動は request_within と同一で、
/// 遅い時は ev=ipc_slow、TimedOut 時は ev=ipc_timeout を出す（診断用。劣化自体は呼び出し側の既存枝）。
fn timed_request(
    client: &mut EngineClient,
    req: &Request,
    tier: Duration,
    op: &str,
) -> std::io::Result<Response> {
    let start = std::time::Instant::now();
    let r = client.request_within(req, start + tier);
    let elapsed = start.elapsed();
    let ms = elapsed.as_millis();
    if should_log_slow(elapsed, tier) {
        tip_log(&format!(
            "ev=ipc_slow op={op} ms={ms} tier={}",
            tier.as_millis()
        ));
    }
    if matches!(&r, Err(e) if e.kind() == std::io::ErrorKind::TimedOut) {
        tip_log(&format!("ev=ipc_timeout op={op} ms={ms}"));
    }
    r
}

/// `timed_request` と同一のログ計測だが、タイムアウトで接続を捨てず client 側 pending を立てる
/// `request_within_keep` を用いる（LiveConvert/Insert 専用。呼び出し側は次要求前に drain する）。
fn timed_request_keep(
    client: &mut EngineClient,
    req: &Request,
    tier: Duration,
    op: &str,
) -> std::io::Result<Response> {
    let start = std::time::Instant::now();
    let r = client.request_within_keep(req, start + tier);
    let elapsed = start.elapsed();
    let ms = elapsed.as_millis();
    if should_log_slow(elapsed, tier) {
        tip_log(&format!(
            "ev=ipc_slow op={op} ms={ms} tier={}",
            tier.as_millis()
        ));
    }
    if matches!(&r, Err(e) if e.kind() == std::io::ErrorKind::TimedOut) {
        tip_log(&format!("ev=ipc_timeout op={op} ms={ms}"));
    }
    r
}

/// StartSession 応答から `ensure_session` の次の動作を決める純関数。
/// `Some(id)`=セッション採用 / `None`=接続破棄（drop_engine）。
/// Session 以外（タイムアウト・切断・予期しない応答）で破棄する理由は engine_end_session の
/// ドキュメントと同じ: プロトコルに request-id 相関が無く、正しさが厳密な要求→応答交互性のみに
/// 依存するため、遅延応答フレームがパイプに滞留すると以降そのパイプ上の全リクエストが
/// 「1つ前の応答」を読む恒常 1-off desync になる。
fn plan_start_session(result: std::io::Result<Response>) -> Option<i64> {
    match result {
        Ok(Response::Session { session, proto: _ }) => Some(session),
        _ => None,
    }
}

/// EndSession 応答を ack として受理してよいか決める純関数。false＝接続破棄（drop_engine）。
/// Why not(`Ok(_)` を一律受理する — 従来形): protocol.rs は request-id 相関を持たず、正しさは
/// 要求/応答の交互性だけに依存する。想定外の型を ack として飲むと交互性が崩れていても検出できない。
/// 他の op（`plan_start_session` / `engine_backspace` / `engine_convert`）は全て「期待した型以外は
/// 破棄」で揃っており、EndSession だけ緩いのが規律の穴だった。
fn end_session_ack_accepted(result: &std::io::Result<Response>) -> bool {
    matches!(result, Ok(Response::Ok))
}

/// IPC failure diagnostics must describe only the response shape / I/O class. `Response` carries
/// readings, candidates, committed text and predictions, so formatting it with `Debug` would put
/// user input back into the log even when the caller's normal event is redacted.
fn response_kind(response: &Response) -> &'static str {
    match response {
        Response::Pong => "pong",
        Response::Session { .. } => "session",
        Response::Reading { .. } => "reading",
        Response::Candidates { .. } => "candidates",
        Response::Committed { .. } => "committed",
        Response::Ok => "ok",
        Response::Error { .. } => "error",
        Response::LiveResult { .. } => "live_result",
        Response::LlmResult { .. } => "llm_result",
        Response::Prediction { .. } => "prediction",
        Response::PredictionUnavailable { .. } => "prediction_unavailable",
        Response::ClauseView { .. } => "clause_view",
    }
}

fn engine_failure_event(op: &str, result: &std::io::Result<Response>) -> String {
    match result {
        Ok(response) => format!(
            "ev=engine_failure op={op} response={}",
            response_kind(response)
        ),
        Err(error) => format!("ev=engine_failure op={op} io={:?}", error.kind()),
    }
}

/// version handshake の判定（純関数）。StartSession 応答の proto（互換世代）から、この接続を
/// どう扱うかを決める。副作用（Shutdown 送信・respawn・ログ）は呼び出し側 start_and_store が行う。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum HandshakeAction {
    /// proto 一致。従来どおりセッションを採用する。
    Accept,
    /// proto 不一致かつ未試行。graceful に旧エンジンを止めて新エンジンへ世代交代する。
    ShutdownRespawn,
    /// proto 不一致だが一度試行済み。接続を維持し現行 op 範囲で動作継続する（無限 shutdown ループ防止）。
    DegradeKeep,
}

/// proto=None は handshake 以前の旧エンジン。Some(PROTO_VERSION) 以外は全て不一致として扱う。
fn decide_handshake(proto: Option<u32>, already_attempted: bool) -> HandshakeAction {
    if proto == Some(PROTO_VERSION) {
        HandshakeAction::Accept
    } else if already_attempted {
        // インストーラの停止失敗等で exe が古いままでも、接続を保って旧プロトコル範囲で動かし続ける。
        // proto=None の旧エンジンは現行 op 全対応なので実害はない（Shutdown だけ未対応＝回収は installer）。
        HandshakeAction::DegradeKeep
    } else {
        HandshakeAction::ShutdownRespawn
    }
}

/// UU-5: settings.json の現在値から `ReloadConfig` リクエストを組み立てる純関数（テスト可能）。
/// `api_key_plain` は DPAPI 復号済みの平文鍵（無ければ None）。
/// LLM 無効時は LLM 系フィールドを空で送る（エンジンは非空チェックで disabled に落ちる＝H-1 と整合。
/// resolve_env_map が enabled のときだけ LLM env を注入するのと同じ意味論）。zenzai_weight は
/// 空でもそのまま送り、エンジン側が per-user → exe 隣の順で解決する（3段表）。
///
/// セキュリティ注記（fable レビュー #3・許容済トレードオフ）: 平文 API キーが常駐エンジンへの
/// 名前付きパイプを流れる。パイプ DACL は AppContainer/LPAC SID にも接続を許すため、同一ユーザの
/// サンドボックスプロセスが起動レースでパイプ名を先取り squat すればキーを窃取しうる（env 経由の
/// spawn では読めなかった経路）。パイプは元来入力テキスト全体を運んでおり同一ユーザ信頼が前提な
/// こと・squat は起動レース勝利を要すること・LLM 有効化/キー変更の即時反映という機能価値から、
/// **キー送信を維持する**判断（ユーザ承認）。将来のハードニング候補は
/// GetNamedPipeServerProcessId によるサーバ像検証（送信前に正規エンジンか照合）。
/// なお凍結中(settings::LLM_CONVERT_FROZEN)は llm_effective_enabled が false のためキーは流れない。
fn build_reload_config(
    s: &settings::Settings,
    api_key_plain: Option<&str>,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Request {
    let llm_on = settings::llm_effective_enabled(s);
    let (llm_api_key, llm_endpoint, llm_model, llm_prompt) = if llm_on {
        (
            api_key_plain.unwrap_or("").to_string(),
            s.llm.endpoint.clone(),
            s.llm.model.clone(),
            s.llm.prompt.clone(),
        )
    } else {
        (String::new(), String::new(), String::new(), String::new())
    };
    Request::ReloadConfig {
        llm_enabled: llm_on,
        llm_api_key,
        llm_endpoint,
        llm_model,
        llm_prompt,
        llm_timeout_ms: s.llm.timeout_ms,
        zenzai_enabled: s.zenzai.enabled,
        zenzai_weight: s.zenzai.weight_path.clone(),
        inline_prediction_enabled: s.inline_prediction.enabled,
        learning_enabled: s.learning.enabled,
        typo_learn_enabled: s.typo_correct.learn,
        // D6: 診断 env が既に居るときは None（push 抑止＝spawn/reload とも env が勝つ。
        // resolve_env_map の env_lookup 抑止と対）。居なければクランプ済み値を push する。
        zenzai_inference_limit: if env_lookup("NOSPACEKEY_ZENZAI_INFERENCE_LIMIT").is_some() {
            None
        } else {
            Some(s.zenzai.effective_inference_limit())
        },
    }
}

thread_local! {
    /// デバウンスタイマ proc から現在の TextService を引くための生ポインタ（STA 単一スレッド）。
    static DEBOUNCE_TS: std::cell::Cell<*const TextService_Impl> =
        const { std::cell::Cell::new(std::ptr::null()) };
}

thread_local! {
    /// LLM ポーリングタイマ proc から TextService を引くための生ポインタ（STA 単一スレッド）。
    static LLM_TS: std::cell::Cell<*const TextService_Impl> =
        const { std::cell::Cell::new(std::ptr::null()) };
}

thread_local! {
    /// UIバグ4: RefreshAnchorOnLayout（非同期 edit session）の DoEditSession から
    /// TextService を引くための生ポインタ（STA 単一スレッド）。Activate で set、Drop で null。
    static LAYOUT_TS: std::cell::Cell<*const TextService_Impl> =
        const { std::cell::Cell::new(std::ptr::null()) };
}
/// LLM 結果ポーリング間隔（ms）。数秒の処理に対し十分細かい。
const LLM_POLL_MS: u32 = 50;
/// LLM 変換の上限待ち時間。これを超えたらエンジンがハングしたとみなして待機を解除し、
/// 読み preedit へ劣化する（無応答エンジンで IME が永久フリーズしないための保険）。
const LLM_TIMEOUT: Duration = Duration::from_secs(8);
/// 軽微1: モードトグルのオートリピート抑止窓。直近トグルからこの時間未満に来た
/// OnPreservedKey(ToggleMode) は無視する（キー長押しでモードが偶奇フリッカするのを防ぐ）。
/// 人が意図して押し直す間隔より十分短く、かつオートリピート連射（30/s ≈ 33ms）は確実に潰す。
const MODE_TOGGLE_REPEAT_GUARD: Duration = Duration::from_millis(300);

/// 軽微1: モードトグルをキーリピート抑止するか判定する純関数（テスト可能）。
/// `elapsed` は直近トグルからの経過（初回＝None）。None または threshold 以上なら通す(false)、
/// threshold 未満なら抑止(true)。
fn is_toggle_repeat(elapsed: Option<Duration>, threshold: Duration) -> bool {
    matches!(elapsed, Some(e) if e < threshold)
}

thread_local! {
    /// SP6a: UIElement Behavior(マウス/タッチ)発の確定/取消を STA 自己ポインタ経由で
    /// 引くための生ポインタ。LLM_TS と違い Activate で立て Deactivate で必ず落とすので、
    /// presenter の notify がいつ呼ばれても（活性中なら）有効な self を指す。
    static BEHAVIOR_TS: std::cell::Cell<*const TextService_Impl> =
        const { std::cell::Cell::new(std::ptr::null()) };
}

thread_local! {
    /// 巡3 Z4: ReloadConfig busy 再送タイマ proc から TextService を引くための生ポインタ
    /// （STA 単一スレッド。llm_poll と同じ規律 — Drop で null 化）。
    static RELOAD_RETRY_TS: std::cell::Cell<*const TextService_Impl> =
        const { std::cell::Cell::new(std::ptr::null()) };
}

thread_local! {
    /// 予測 composition 終了の bounded retry 用。Activate–Deactivate 間の STA だけで有効。
    static PREDICTION_RETRY_TS: std::cell::Cell<*const TextService_Impl> =
        const { std::cell::Cell::new(std::ptr::null()) };
}

thread_local! {
    /// 予測 debounce／結果ポーリング用。Activate–Deactivate 間の STA だけで有効。
    static PREDICTION_POLL_TS: std::cell::Cell<*const TextService_Impl> =
        const { std::cell::Cell::new(std::ptr::null()) };
}

const PREDICTION_RETRY_MS: u32 = 50;
const PREDICTION_RETRY_MAX: u8 = 8;
const PREDICTION_DEBOUNCE_MS: u32 = 300;

fn consume_expected_prediction_commit_end_edit(
    deadline: &Cell<Option<Instant>>,
    selection_changed: bool,
    now: Instant,
) -> bool {
    deadline
        .replace(None)
        .is_some_and(|deadline| !selection_changed && now <= deadline)
}
const PREDICTION_POLL_MS: u32 = 15;
const PREDICTION_TIMEOUT: Duration = Duration::from_millis(400);

fn prediction_slot_available(physical_composition: bool, finish_pending: bool) -> bool {
    !physical_composition && !finish_pending
}

fn prediction_mode_allows_display(direct: bool, ephemeral: bool) -> bool {
    !direct && !ephemeral
}

pub(crate) struct DeferredPredictionPreservedKey {
    pub(crate) context: Option<ITfContext>,
    pub(crate) guid: GUID,
}

#[implement(
    ITfTextInputProcessorEx,
    ITfKeyEventSink,
    ITfDisplayAttributeProvider,
    ITfCompositionSink,
    ITfThreadMgrEventSink,
    ITfThreadFocusSink,
    ITfFnConfigure,
    ITfTextLayoutSink,
    ITfTextEditSink
)]
pub struct TextService {
    pub(crate) tid: Cell<u32>,
    pub(crate) thread_mgr: RefCell<Option<ITfThreadMgr>>,
    /// Deactivate 実行中（本体内の COM コールアウトを含む全区間）を示すフラグ。RAII ガード
    /// （DeactivatingGuard）が Deactivate 入口で true にし、return・panic unwind を含む全出口で
    /// false に戻す。この間の同期再入 Activate（RemoveItem 等のコールアウト中にホストが呼び
    /// うる）は即 Err で拒否する — 受け入れると外側 Deactivate の後続清算が新しい世代の登録
    /// （key sink/PreservedKey/langbar）を道連れに消す「活性化済みなのに登録が無い幽霊状態」を
    /// 作る（Medium fix）。ネスト Deactivate もこのフラグで弾き二重清算を防ぐ。
    pub(crate) deactivating: Cell<bool>,
    /// ITfThreadMgrEventSink を AdviseSink した cookie（0=未登録）。Deactivate で UnadviseSink する。
    /// スレッド内 doc フォーカス変化（別ウィンドウ切替）を OnSetFocus で捕捉し、ホストが
    /// OnCompositionTerminated を呼ばずに合成を確定/破棄しても、エンジンセッションの読み残留を防ぐ。
    pub(crate) thread_mgr_event_cookie: Cell<u32>,
    /// ITfThreadFocusSink を AdviseSink した cookie（0=未登録）。Deactivate で UnadviseSink する。
    /// クロスプロセス（別アプリへ前面が移る）でのフォーカス喪失は ITfThreadMgrEventSink::OnSetFocus
    /// では届かないため、前面（スレッド）喪失を OnKillThreadFocus で捕捉して同じ放棄リセットを焚く。
    pub(crate) thread_focus_cookie: Cell<u32>,
    /// UIバグ4: フォーカス context へ AdviseSink した `ITfTextLayoutSink` の cookie（0=未登録）。
    /// スクロール/リフローで OnLayoutChange が届き、表示中の候補窓・読みモニタを追従させる。
    /// advise 先 context を併せて保持し、フォーカス移動（OnSetFocus/OnPush/OnPopContext）で
    /// 対称的に unadvise→advise し直す。Deactivate で掃討する。
    pub(crate) layout_sink_cookie: Cell<u32>,
    /// フォーカス context の選択・本文変更を監視し、ゴーストを stale 化する edit sink。
    pub(crate) text_edit_sink_cookie: Cell<u32>,
    pub(crate) layout_sink_ctx: RefCell<Option<ITfContext>>,
    /// OnLayoutChange 連発（スクロール中）を 1 本の非同期再照会セッションにまとめるフラグ。
    /// RefreshAnchorOnLayout::DoEditSession → layout_refresh_apply で解除する。
    pub(crate) layout_refresh_pending: Cell<bool>,
    /// レイアウト再照会セッションの世代（巡2 E1/E3/E4）。RefreshAnchorOnLayout は
    /// TF_ES_ASYNC で旧 context を保持したまま遅延実行されるため、投入後に Activate/
    /// Deactivate/context 貼替が起きたセッションの結果は現在の表示を汚してはならない。
    /// セッション投入時にこの世代を埋め込み、layout_refresh_apply 側で一致したときだけ
    /// 座標を適用する（不一致は pending 解除のみで座標破棄）。
    pub(crate) layout_sink_gen: Cell<u64>,
    /// 巡3 Z4: ReloadConfig busy 再送の試行回数とタイマID（上限付き bounded retry）。
    pub(crate) reload_retry_count: Cell<u32>,
    pub(crate) reload_retry_timer: Cell<usize>,
    /// 巡4 T1: 遅延 flush タイマの ID（0=未武装）。多重武装の防止と proc 側の照合に使う。
    pub(crate) behavior_flush_timer: Cell<usize>,
    /// 自分を包む `TextService_Impl` への自己ポインタ（Activate で設定）。Drop は outer 型
    /// （&mut TextService）から Impl へ橋渡しできないため、TLS の所有権比較（巡2 E2）は
    /// このフィールド経由で行う。STA 専用なので !Send で問題ない。
    pub(crate) impl_ptr: Cell<*const TextService_Impl>,
    pub(crate) client: RefCell<Option<EngineClient>>,
    pub(crate) engine_session: Cell<i64>,
    /// engine_end_session を呼んだとき client が LLM ワーカへ move 済みで EndSession を送れなかった
    /// セッション id を保留する。client 復帰時(on_llm_outcome)に送って engine 側の取り残しを防ぐ。
    pub(crate) pending_end_session: Cell<i64>,
    pub(crate) state: RefCell<InputState>,
    pub(crate) composition: Rc<RefCell<Option<ITfComposition>>>,
    /// 通常 preedit と直交するインライン予測専用 composition。
    pub(crate) prediction_composition: Rc<RefCell<Option<ITfComposition>>>,
    pub(crate) prediction_context: Rc<RefCell<Option<ITfContext>>>,
    pub(crate) prediction_editing: Rc<Cell<bool>>,
    pub(crate) prediction_state: RefCell<crate::prediction_state::PredictionState>,
    pub(crate) prediction_enabled: Cell<bool>,
    pub(crate) prediction_commit_suppressed: Cell<bool>,
    /// The host may deliver the `OnEndEdit` for an explicit IME commit after the
    /// commit callback returns (VS Code does this). Consume that one expected edit
    /// instead of treating the text we just committed as a later external edit.
    pub(crate) prediction_commit_edit_deadline: Cell<Option<Instant>>,
    pub(crate) prediction_poll_timer: Cell<usize>,
    pub(crate) prediction_slot: RefCell<Option<Arc<PredictionSlot>>>,
    pub(crate) prediction_failed_context: Rc<RefCell<Option<ITfContext>>>,
    /// Some(accept) は終了 edit session の実行待ち／再試行待ち。
    pub(crate) prediction_finish_pending: Rc<Cell<Option<bool>>>,
    pub(crate) prediction_retry_timer: Cell<usize>,
    pub(crate) prediction_retry_count: Cell<u8>,
    pub(crate) prediction_deferred_preserved: RefCell<VecDeque<DeferredPredictionPreservedKey>>,
    pub(crate) prediction_anchor_gen: Cell<u64>,
    /// CommitText の SetText 成功後に EndComposition だけが失敗した状態。true の間も
    /// composition handle を保持し、本文へ再度触れない close-only edit session で再試行する。
    pub(crate) composition_end_pending: Rc<Cell<bool>>,
    /// pending composition を所有する context。通常の current_context と分離し、focus 移動や
    /// 次打鍵で current が変わっても元 context の edit session で close-only を実行する。
    /// ただし terminal/focus/context 境界では bounded liveness のため即座に捨てる。
    pub(crate) composition_end_context: Rc<RefCell<Option<ITfContext>>>,
    /// close-only の最新試行状態。`Retryable` だけを bounded retry へ進める。
    pub(crate) composition_end_status: Rc<Cell<CompositionEndStatus>>,
    /// close-only の試行回数（初回 EndComposition 失敗を 1 とする）。
    pub(crate) composition_end_retry_count: Rc<Cell<u8>>,
    /// composition lifecycle の世代。focus/pop で pending を捨てた後に届く古い
    /// callback は identity とこの世代境界で無害化する。
    pub(crate) composition_generation: Cell<u64>,
    pub(crate) pending_end_generation: Cell<u64>,
    /// Test→Key pair だけの世代。composition_generation とは独立で、pending close の
    /// 成功/callback/quarantine では進めず pair を維持し、focus/context/activation 境界
    /// では進めて reservation を無効化する。
    pub(crate) key_pair_generation: Cell<u64>,
    /// `StartComposition` 成功を edit session から同期再入入口へ伝える one-shot signal。
    /// StartComposition 後の COM callout 中に OnTest/OnKey が再入しても、caller が戻る前に
    /// stale な Test→Key pair を無効化できる。STA 契約上、別の StartComposition はこの
    /// signal を caller が consume する前に nested にはならないため、bool で十分とする。
    pub(crate) composition_started_signal: Rc<Cell<bool>>,
    /// OnTestKeyDown が pending close の回収用として TRUE を返した物理イベントの one-shot
    /// reservation。composition/context の lifecycle と分離して保持する。
    pub(crate) pending_end_test_reservation: RefCell<PendingEndKeyReservation>,
    /// 部分確定の SetText 後、旧 composition の EndComposition だけが保留されて残り読みを
    /// まだ新しい preedit として張れていない状態。close 完了後の STA 安全点で一度だけ再描画する。
    pub(crate) partial_preedit_redraw_pending: Cell<bool>,
    /// 上記再描画の単発タイマ再試行回数。永続 edit-session 拒否時の無限ループを防ぐ。
    pub(crate) partial_preedit_redraw_retries: Cell<u8>,
    /// U9: composition 開始時に捕捉した左文脈（サニタイズ済・最大40字）。
    /// StartOrUpdatePreedit / ReconvertStart が**成否によらず必ず上書き**し、変換系
    /// リクエスト（Convert/LiveConvert/LlmConvert/Reconvert）へ載せる。合成終了経路
    /// （commit_and_reset / cancel / reset_abandoned_composition）で明示 None クリア
    /// — edit session 拒否で取得コード自体が走らないときの前文書残留を塞ぐ（spec §2.1）。
    pub(crate) left_context: Rc<RefCell<Option<String>>>,
    pub(crate) da_atom: Cell<u32>,
    /// 文節ナビゲーションの選択文節（太下線）用の表示属性 atom（0=未登録）。
    pub(crate) da_target_atom: Cell<u32>,
    /// インライン予測ゴースト属性 atom（0=未登録）。
    pub(crate) da_prediction_atom: Cell<u32>,
    pub(crate) showing: Cell<bool>,
    /// 文節ナビゲーション（変換中の←/→）のビュー。Some=文節モード中（不変条件:
    /// Some ⇒ showing。候補窓を閉じる/確定する全経路が clear_clause_nav で None に落とす）。
    /// segments の連結が preedit 全体、selected が太下線を引く文節。候補列そのものは
    /// 従来どおり cand_state が唯一の真実源（文節モード中は「選択文節の候補」が入る）。
    pub(crate) clause_nav: RefCell<Option<ClauseNav>>,
    pub(crate) candidate_ui: RefCell<CandidatePresenter>,
    /// 直近に GetTextExt で取れた有効キャレットアンカー。照会失敗（レイアウト未確定等）
    /// のフォールバック先として使い、失敗のたび無害位置へ跳ねるちらつきを防ぐ
    /// （UIバグ5）。フォーカス切替でクリア — 別 context の座標は無意味なため。
    pub(crate) last_valid_anchor: RefCell<Option<crate::candidate_window::CaretAnchor>>,
    /// SP6a: presenter / UIElement と共有する候補状態（GetCount/GetString 等の読み元）。
    /// TextService も co-owner として保持し、drain_behavior の Finalize で
    /// 選択中候補（Behavior::SetSelection が更新する唯一の真実源）を読む。
    pub(crate) cand_state: Rc<RefCell<CandidateState>>,
    /// SP6a: Behavior(ホスト発)が確定/取消要求を書き込むスロット。drain_behavior が取り出す。
    pub(crate) behavior_outbox: Rc<RefCell<Option<BehaviorAction>>>,
    /// キーボード以外（ホスト Behavior::SetSelection / 自前窓のマウスクリック）が選択を動かした
    /// ことを示す一発フラグ。drain_behavior が消費して preedit を選択候補へ揃える。
    /// 単一スロットの behavior_outbox に相乗りさせない — 保留中の Finalize を選択要求が
    /// 上書きし、クリック確定が黙って消える（outbox は Option 1 枠しか無い）。
    pub(crate) selection_dirty: Rc<Cell<bool>>,
    /// UU-4: TS の COM 操作中（RefCell 借用を保持しつつ presenter 経由でホストへ同期コール
    /// アウトしうる区間）にホストが Behavior 経由で再入して drain を呼んでも、借用衝突 panic を
    /// 起こさず保留→安全点で flush させる門（純粋ロジック＝単体テスト可能）。
    pub(crate) reentrancy: ReentrancyGate,
    pub(crate) last_reading: RefCell<String>,
    /// 読みキャッシュ: 自動確定(live_auto)で消費された読みの累積。accumulate ON のとき
    /// モニタは「これ + last_reading」を表示する。リセットは composition 完全終了の全経路
    /// + PartialReseed(U9 左文脈クリアと同じ規律)。候補窓の開閉では消さない —
    /// Space→候補→Esc で合成へ戻ったとき累積表示を復元するため。
    pub(crate) monitor_committed_reading: RefCell<String>,
    /// 現在のライブ変換結果（preedit に出している漢字かな交じり文）。Enter で確定する文字列。
    pub(crate) live_text: RefCell<String>,
    /// ライブ変換を遅延実行するデバウンスタイマ ID（0=非武装）。
    pub(crate) debounce_timer: Cell<usize>,
    /// 遅延 convert 時に edit session を張るための直近 ITfContext。
    pub(crate) current_context: RefCell<Option<ITfContext>>,
    /// このインスタンス専用エンジンのパイプ名（初回に生成して固定）。
    pub(crate) pipe_name: RefCell<String>,
    /// この活性化中にエンジン起動を既に試みたか（連打での多重起動を防ぐ）。
    pub(crate) spawn_attempted: Cell<bool>,
    /// cold start ② M-3: prespawn の spawn がこのインスタンスで一度失敗したか。ハードン host
    /// （AppContainer 等で spawn が恒常失敗）が Activate のたびに SpawnGuard＋50ms 接続＋
    /// DPAPI 復号＋失敗 CreateProcess を払い続けないための最小ガード。ensure_engine の
    /// spawn 経路（spawn_attempted＋バックオフ）には影響しない。
    pub(crate) prespawn_failed: Cell<bool>,
    /// version handshake: proto 不一致で一度 graceful 世代交代（Shutdown→respawn）を試したか。
    /// 試行済みで再び不一致なら DegradeKeep に落として無限 shutdown ループを防ぐ。Accept でリセット。
    pub(crate) handshake_shutdown_attempted: Cell<bool>,
    /// A7: engine 再接続フルコースの失敗間隔を制御し、キースレッドが死んだ／半死の engine を
    /// 連打で叩き続けないためのバックオフゲート。クールダウン中は一発プローブのみ許し、
    /// session 確立失敗（半死）検出後はプローブも満了まで停止する（ensure_engine が消費）。
    pub(crate) reconnect_backoff: RefCell<crate::engine_link::ReconnectBackoff>,
    /// L-5: この活性化で spawn したエンジンの Child ハンドル（reconnect 経由なら None）。
    /// LLM ハング時に abort_llm が kill して、ブロック中のワーカ ReadFile を解除しスレッド/
    /// ハンドルのリークを即時回収する。PID ではなくハンドルなので PID 再利用の TOCTOU が無い。
    pub(crate) engine_child: RefCell<Option<std::process::Child>>,
    /// LLM 結果ポーリングタイマ ID（0=非武装）。
    pub(crate) llm_poll_timer: Cell<usize>,
    /// ワーカ→UIスレッドの結果スロット（in-flight 中のみ Some）。
    pub(crate) llm_slot: RefCell<Option<LlmSlot>>,
    /// LLM 変換を開始した時刻（タイムアウト判定用。None=待機していない）。
    pub(crate) llm_started: Cell<Option<std::time::Instant>>,
    /// LLM 変換前の preedit（失敗/取りやめ時に復元）。
    pub(crate) pre_llm_text: RefCell<String>,
    /// 再変換 composition 中か（true の間 Esc は元ラテン復元、確定は候補をそのまま使う）。
    pub(crate) reconverting: Cell<bool>,
    /// 部分確定（前方一致候補）で composition を張り替えている最中か。true の間は
    /// OnCompositionTerminated を no-op にして、自分の do_commit が（ホスト依存で）誘発しうる
    /// 合成終了でエンジンセッションを巻き添えに終了させない（残り読みのセッションを保持する）。
    pub(crate) partial_committing: Cell<bool>,
    /// 再変換取消時に復元する元ラテン列。
    pub(crate) reconvert_original: Rc<RefCell<String>>,
    /// 再変換で掴んだ対象の「かな読み」(RecordCorrection のキー)。経路別に採取する:
    /// Surface=掴んだかな表層 / Latin=engine_insert 応答の Reading(ローマ字のかな化は
    /// エンジン側 roman2kana にしか無いため TIP では作れない) / 確定取消=リプレイ用 reading。
    /// 空 = 採取できなかった(送出しない — 深層防御)。クリアは reconvert_original の
    /// clear サイトに併記(上書きサイトは経路別採取が無条件上書きするため対象外)。
    pub(crate) reconvert_reading: Rc<RefCell<String>>,
    /// SP6b: ライブ変換 on/off（設定）。false なら打鍵でデバウンス変換を武装せず、
    /// 読み preedit のまま Space/Enter で SP1 候補フローに任せる。Activate で1度読む(D7)。
    pub(crate) live_enabled: Cell<bool>,
    /// 外部LLM変換(Tab)のフィーチャーフラグ（設定 `llm.enabled`）。false なら Tab を IME 機能として
    /// 扱わず素通しし、LLM 機構を一切起動しない。Activate で1度読む(D7)。既定 false（＝オフ）。
    pub(crate) llm_enabled: Cell<bool>,
    /// 修正変換(Tab)のフィーチャーフラグ（設定 `typo_correct.enabled`）。false なら Tab を IME 機能
    /// として扱わず素通しする。llm_enabled と並行の独立フラグ（Shift+Tab=LLM とは無関係）。
    /// Activate で1度読む(D7)。既定 false（Activate で settings 値へ上書きされるまでの初期値）。
    pub(crate) typo_enabled: Cell<bool>,
    /// SP7: default_direct を「このインスタンスで1度だけ」適用したか。
    /// 真にしたら Deactivate でもリセットしない＝IME 切替往復後の再 Activate で
    /// ユーザの手動トグル（無変換）を巻き戻さない（spec §3.3「以後の手動トグルを尊重」）。
    /// 適用に失敗した Activate では false のまま＝次回 Activate で再試行する
    /// （「1度だけ」は apply_default_direct が成功した時点で確定する）。
    pub(crate) default_direct_applied: Cell<bool>,
    /// TIP が conversion-mode の真実を所有しているか。default_direct 適用・トグル・
    /// ephemeral のあと true。true のとき `is_direct_mode` は compartment ではなく
    /// `langbar_is_direct` を読む（Activate 後のホスト上書きで表示A・入力あ にしない）。
    /// Deactivate ではリセットしない（`default_direct_applied` と同じワンショット契約）。
    pub(crate) direct_mode_owned: Cell<bool>,
    /// SP5/US: 言語バーの あ/A モードインジケータと共有する「現在モード」フラグ（true=半角英数=A）。
    /// toggle_conversion_mode / apply_default_direct が更新し、ModeLangBarItem の GetText が読む。
    pub(crate) langbar_is_direct: Rc<Cell<bool>>,
    /// ephemeral かなモード中かどうかを langbar_is_direct と並行して共有するフラグ。
    /// update_langbar_mode が更新し、ModeLangBarItem の GetText/GetIcon が読む（「あ˙」表示）。
    pub(crate) langbar_ephemeral: Rc<Cell<bool>>,
    /// 言語バーアイテムへシステムが advise した更新 sink。ModeLangBarItem の AdviseSink が書き、
    /// モード切替時にここから読んで OnUpdate を呼び表示を再取得させる（item と Rc 共有）。
    pub(crate) langbar_sink: Rc<RefCell<Option<ITfLangBarItemSink>>>,
    /// 言語バーへ AddItem したインジケータ（生存維持＋Deactivate の RemoveItem 用）。
    pub(crate) langbar_item: RefCell<Option<ITfLangBarItemButton>>,
    /// 言語バー右クリックメニュー「切替」用のトグルコールバック（ModeLangBarItem と Rc 共有）。
    /// Activate で「自身の COM 参照を捕まえ toggle_conversion_mode(None) を呼ぶ closure」を格納し、
    /// Deactivate で None に戻す。なぜ Activate 期間に限定するか: closure が自身の COM 参照
    /// (ITfTextInputProcessorEx) を owned で保持し、TextService はこの Rc を保持するため相互参照
    /// （循環）になる。Deactivate で None にして循環を断ち切りリークを防ぐ。Deactivate が呼ばれない
    /// 経路（プロセス強制終了）はプロセスごと消えるのでリークにならない。
    pub(crate) langbar_on_toggle: crate::langbar::ModeToggleHandle,
    /// SP5/US: モード切替時に あ/A をキャレット近傍へ一瞬出す HUD（Win11 では言語バーが出ない）。
    pub(crate) mode_hud: std::cell::RefCell<crate::mode_hud::ModeHud>,
    /// 読みモニタ: ライブ変換中の生読みをキャレット上側へ常時表示する窓（spec 2026-07-21）。
    pub(crate) reading_monitor: std::cell::RefCell<crate::reading_monitor::ReadingMonitor>,
    /// Task 7: 表示（候補 show / HUD flash）ごとに settings.json の mtime とダークモードを
    /// 再評価して Theme を供給する源。RefCell なのは &self の TSF コールバックから
    /// borrow_mut() で mtime キャッシュを更新するため（STA なので競合しない）。
    pub(crate) appearance: RefCell<crate::theme::AppearanceSource>,
    /// 軽微1: 直近のモードトグル時刻。無変換/Alt+` の長押しオートリピートが OnPreservedKey へ
    /// 連続到達してもモードがフリッカしないよう、直近トグルから MODE_TOGGLE_REPEAT_GUARD 未満の
    /// 連射を抑止する（兄弟の再変換が reconverting ラッチで自衛しているのに倣った自衛ガード）。
    pub(crate) last_mode_toggle: Cell<Option<std::time::Instant>>,
    /// Spec2: 現在の ITfContext が IS_PASSWORD か（コンテキストポインタをキーに 1 段キャッシュ。
    /// キーごとの COM 照会を避ける。key=0 は「未キャッシュ」の番兵）。
    pub(crate) password_ctx: Cell<bool>,
    pub(crate) password_ctx_key: Cell<usize>,
    /// A7: 電源復帰通知の購読ハンドル（Activate で register・Deactivate で None にして Drop に
    /// unregister させる）。None は「未登録/購読失敗」で、poll_power_events は no-op に落ちる
    /// （spec 4.4 の劣化＝従来どおり次打鍵で自己修復）。
    pub(crate) power_notify: RefCell<Option<crate::power::PowerNotifyHandle>>,
    /// A7: 直近にキースレッドで観測したレジューム世代（resume_poll_action の `last`）。
    pub(crate) last_resume_gen: Cell<u32>,
    /// A7: 直近のレジューム刈り取り以降、まだ「復帰後最初の変換系 op」の計測を消費していないか。
    /// poll_power_events が true にし、engine_convert/engine_live_convert/engine_reconvert_surface
    /// のいずれかが最初にヒットした時点で消費（false に戻す）。
    pub(crate) resume_convert_pending: Cell<bool>,
    /// A': IPC タイムアウト応答の遅延ドレイン。LiveConvert/Insert が締め切り超過したとき接続を
    /// 捨てず、engine 側 client の pending と対で「未読応答を owe している」壁時計時刻を記録する。
    /// None=owe 無し。Some(t)=t 時点で pending 化。INV5: pending 化から `PENDING_MAX` を超えても
    /// drain できなければ engine 真死とみなし drop_engine する（永久劣化の暴走ガード）。
    pub(crate) pending_since: Cell<Option<std::time::Instant>>,
    /// 品質ループ③: 直前確定 1 件のバッファ。commit_and_reset / apply_commit_plan /
    /// apply_live_auto_commit が**クリア前に**保存し、Ctrl+変換（OnPreservedKey Feedback）が
    /// 消費して feedback.jsonl へ書く。shift_latin の直接確定（読み無し）は対象外。
    /// F-5 改定（確定取消）: 保存条件は `feedback_enabled || arms_undo(source)` へ拡大済み
    /// （常時保存ではない — remember_last_commit 参照）。
    pub(crate) last_commit: RefCell<Option<LastCommit>>,
    /// 品質ループ③: 誤変換ワンキー記録の opt-in フラグ（settings.feedback.enabled）。
    /// Activate で1度読む（D7 — live_enabled/llm_enabled と同じ流儀）。既定 false。
    pub(crate) feedback_enabled: Cell<bool>,
    /// Activate で実際に OS 登録した PreservedKey(Deactivate の Unpreserve と対称にするため
    /// 登録時の実物を保存する — keymap を再解決して突き合わせると設定変更でズレる)。
    pub(crate) preserved_regs: RefCell<Vec<crate::keymap::PreservedReg>>,
    /// 数字全角設定（settings.number.full_width）のキャッシュ。Activate で1度読む（D7）。
    /// 既定確定の数字全角化に使う。既定 true（全角）。
    pub(crate) number_full_width: Cell<bool>,
    /// 句読点全角設定（settings.punctuation.full_width）のキャッシュ。Activate で1度読む（D7）。
    /// idle 記号確定 / composition 記号畳み込みの ,. 幅に使う。既定 true（全角）。
    pub(crate) punctuation_full_width: Cell<bool>,
    /// 記号全角の overlay 実効値（= `settings.symbol.full_width` かつ実効集合が非空）の
    /// キャッシュ。Activate で1度読む（D7）。idle 記号確定 / composition 記号畳み込み /
    /// Shift+数字行の記号化に使う。既定 false（半角）。素のトグルでなく overlay を保持値に
    /// するのは、全解除（実効空集合）を gate レベルまでトグル OFF と完全同一にするため
    /// （読み出し7箇所すべてが同じ値を見る — 2026-08-02 spec §4 CR-1）。
    pub(crate) symbol_overlay: Cell<bool>,
    /// 全角化対象の記号集合（`settings.symbol.effective_chars()`）のキャッシュ。Activate で
    /// 1度読む（D7）。文字が確定している `zenkaku_symbol` 呼び出しだけが参照する — gate は
    /// VK しか知らず文字を引けないため（spec §4）。Activate 前の初期値は `EMPTY`。
    pub(crate) symbol_chars: Cell<settings::symbol::SymbolCharSet>,
    /// 読みモニタの設定トグル（`reading_monitor.enabled`）。Activate で1度読む(D7)。既定 ON。
    pub(crate) reading_monitor_enabled: Cell<bool>,
    /// 読みモニタの累積設定（reading_monitor.accumulate）。Activate で1度読む(D7)。既定 ON。
    pub(crate) reading_monitor_accumulate: Cell<bool>,
    /// 読みモニタの表示上限（クランプ済み max_chars）。Activate で1度読む(D7)。既定 34。
    pub(crate) reading_monitor_max_chars: Cell<u32>,
    /// Shift+英字設定（settings.shift_latin.mode）のキャッシュ。Activate で1度読む（D7）。
    /// true="compose"（英語未確定モード） / false="commit"（大文字直接確定）。既定 true。
    pub(crate) shift_latin_compose: Cell<bool>,
    /// 確定取消（Ctrl+Backspace）: 直前確定が undo 対象として武装中か。commit_and_reset が
    /// `arms_undo(source)` のとき true を立て、次の非修飾キー押下 or settle/preserved key 経由の
    /// disarm_undo() で false に戻る（Ctrl+Backspace の順押しを壊さないよう is_pure_modifier_vk は
    /// disarm 対象外）。武装中だけ Ctrl+Backspace を OnKeyDown 分岐で食う。
    pub(crate) undo_armed: Cell<bool>,
    /// ephemeral かなモード: direct から一時的にかな入力へ入っている最中か。
    /// `enter_ephemeral_kana` で true、`exit_ephemeral_to_direct` で false（Task 3 で全復帰経路配線）。
    /// compartment 自体は enter/exit が直接 NATIVE/direct へ SetValue する — このフラグは
    /// 「direct へ戻すべき」マーカーに徹する（設計ロック: 開始トリガ節）。
    pub(crate) ephemeral_kana: Cell<bool>,
    /// ephemeral かなの機能フラグ（settings.ephemeral.enabled）のキャッシュ。既定 true。
    /// Activate で1度読む（Task 7、number_full_width 等と同じ D7 流儀）。
    pub(crate) ephemeral_enabled: Cell<bool>,
    /// configurable keymap: Activate で settings から解決した全コマンドのバインド（D7 — 1回読み）。
    pub(crate) keymap: Cell<crate::keymap::Keymap>,
    /// C-1: DLL_REF で生存数を数える RAII ガード。他の全 `#[implement]` COM オブジェクトと
    /// 同一カウンタを共有し、生成で +1 / Drop で -1 する（手動 fetch_add/sub の置き換え）。
    _guard: ComObjectGuard,
}

impl TextService {
    /// AdviseSink 済みの `ITfTextLayoutSink` を解除する。outer 型（TextService）側のメソッド
    /// にしているのは、Drop からも呼ぶため（巡2 F5 — Deactivate を経ない解放での cookie
    /// 残り防止）。Impl 側からは Deref でここへ届く。
    pub(crate) fn unadvise_layout_sink(&self) {
        let layout_cookie = self.layout_sink_cookie.get();
        let edit_cookie = self.text_edit_sink_cookie.get();
        if layout_cookie == 0 && edit_cookie == 0 {
            return;
        }
        // 巡4 T6: if let scrutinee の一時 Ref はブロック末尾まで延命されるため先に束縛 —
        // UnadviseSink コールアウト中に再入した borrow_mut()（context 貼替え等）との衝突を防ぐ。
        let ctx = self.layout_sink_ctx.borrow().clone();
        if let Some(ctx) = ctx {
            if let Ok(source) = ctx.cast::<ITfSource>() {
                unsafe {
                    if layout_cookie != 0 {
                        let _ = source.UnadviseSink(layout_cookie);
                    }
                    if edit_cookie != 0 {
                        let _ = source.UnadviseSink(edit_cookie);
                    }
                }
            }
        }
        self.layout_sink_cookie.set(0);
        self.text_edit_sink_cookie.set(0);
        *self.layout_sink_ctx.borrow_mut() = None;
    }

    pub fn new() -> Self {
        // DLL の生存参照は `_guard`（ComObjectGuard）が生成で +1 / Drop で -1 する。
        // DllCanUnloadNow はこのカウントが 0 のときだけ S_OK を返す。これを怠ると活性中の
        // TextService が居るのに「アンロード可」と答え、ホストが DLL を解放して live vtable
        // 呼び出しでクラッシュしうる。C-1 で全 #[implement] オブジェクトが同一カウンタを共有する。
        let cand_state: Rc<RefCell<CandidateState>> = Rc::new(RefCell::new(CandidateState::new()));
        let behavior_outbox: Rc<RefCell<Option<BehaviorAction>>> = Rc::new(RefCell::new(None));
        // notify: host(マウス/タッチ)発の Behavior が outbox に要求を書いた後に呼ばれる。
        // STA 同一スレッドの自己ポインタ thread_local 経由で drain を起こす（LLM_TS と同型）。
        // 重要: ここで self を捕捉しない（循環参照回避）。thread_local を読むだけ。
        let notify: Rc<dyn Fn()> = Rc::new(|| {
            crate::text_service::drain_behavior_via_tls();
        });
        let selection_dirty: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let candidate_ui = RefCell::new(CandidatePresenter::new(
            cand_state.clone(),
            behavior_outbox.clone(),
            selection_dirty.clone(),
            notify,
        ));
        Self {
            tid: Cell::new(0),
            thread_mgr: RefCell::new(None),
            deactivating: Cell::new(false),
            thread_mgr_event_cookie: Cell::new(0),
            thread_focus_cookie: Cell::new(0),
            layout_sink_cookie: Cell::new(0),
            text_edit_sink_cookie: Cell::new(0),
            layout_sink_ctx: RefCell::new(None),
            layout_refresh_pending: Cell::new(false),
            layout_sink_gen: Cell::new(0),
            reload_retry_count: Cell::new(0),
            reload_retry_timer: Cell::new(0),
            behavior_flush_timer: Cell::new(0),
            impl_ptr: Cell::new(std::ptr::null()),
            client: RefCell::new(None),
            engine_session: Cell::new(0),
            pending_end_session: Cell::new(0),
            state: RefCell::new(InputState::default()),
            composition: Rc::new(RefCell::new(None)),
            prediction_composition: Rc::new(RefCell::new(None)),
            prediction_context: Rc::new(RefCell::new(None)),
            prediction_editing: Rc::new(Cell::new(false)),
            prediction_state: RefCell::new(crate::prediction_state::PredictionState::default()),
            prediction_enabled: Cell::new(false),
            prediction_commit_suppressed: Cell::new(false),
            prediction_commit_edit_deadline: Cell::new(None),
            prediction_poll_timer: Cell::new(0),
            prediction_slot: RefCell::new(None),
            prediction_failed_context: Rc::new(RefCell::new(None)),
            prediction_finish_pending: Rc::new(Cell::new(None)),
            prediction_retry_timer: Cell::new(0),
            prediction_retry_count: Cell::new(0),
            prediction_deferred_preserved: RefCell::new(VecDeque::new()),
            prediction_anchor_gen: Cell::new(0),
            composition_end_pending: Rc::new(Cell::new(false)),
            composition_end_context: Rc::new(RefCell::new(None)),
            composition_end_status: Rc::new(Cell::new(CompositionEndStatus::Idle)),
            composition_end_retry_count: Rc::new(Cell::new(0)),
            composition_generation: Cell::new(0),
            pending_end_generation: Cell::new(0),
            key_pair_generation: Cell::new(0),
            composition_started_signal: Rc::new(Cell::new(false)),
            pending_end_test_reservation: RefCell::new(PendingEndKeyReservation::default()),
            partial_preedit_redraw_pending: Cell::new(false),
            partial_preedit_redraw_retries: Cell::new(0),
            left_context: Rc::new(RefCell::new(None)),
            da_atom: Cell::new(0),
            da_target_atom: Cell::new(0),
            da_prediction_atom: Cell::new(0),
            showing: Cell::new(false),
            clause_nav: RefCell::new(None),
            last_valid_anchor: RefCell::new(None),
            candidate_ui,
            cand_state,
            behavior_outbox,
            selection_dirty,
            reentrancy: ReentrancyGate::new(),
            last_reading: RefCell::new(String::new()),
            monitor_committed_reading: RefCell::new(String::new()),
            live_text: RefCell::new(String::new()),
            debounce_timer: Cell::new(0),
            current_context: RefCell::new(None),
            pipe_name: RefCell::new(String::new()),
            spawn_attempted: Cell::new(false),
            prespawn_failed: Cell::new(false),
            handshake_shutdown_attempted: Cell::new(false),
            reconnect_backoff: RefCell::new(crate::engine_link::ReconnectBackoff::new()),
            engine_child: RefCell::new(None),
            llm_poll_timer: Cell::new(0),
            llm_slot: RefCell::new(None),
            llm_started: Cell::new(None),
            pre_llm_text: RefCell::new(String::new()),
            reconverting: Cell::new(false),
            partial_committing: Cell::new(false),
            reconvert_original: Rc::new(RefCell::new(String::new())),
            reconvert_reading: Rc::new(RefCell::new(String::new())),
            live_enabled: Cell::new(true),
            llm_enabled: Cell::new(false),
            typo_enabled: Cell::new(false),
            default_direct_applied: Cell::new(false),
            direct_mode_owned: Cell::new(false),
            langbar_is_direct: Rc::new(Cell::new(false)),
            langbar_ephemeral: Rc::new(Cell::new(false)),
            langbar_sink: Rc::new(RefCell::new(None)),
            langbar_item: RefCell::new(None),
            langbar_on_toggle: Rc::new(RefCell::new(None)),
            mode_hud: std::cell::RefCell::new(crate::mode_hud::ModeHud::empty()),
            reading_monitor: std::cell::RefCell::new(
                crate::reading_monitor::ReadingMonitor::empty(),
            ),
            appearance: RefCell::new(crate::theme::AppearanceSource::new()),
            last_mode_toggle: Cell::new(None),
            password_ctx: Cell::new(false),
            password_ctx_key: Cell::new(0),
            power_notify: RefCell::new(None),
            last_resume_gen: Cell::new(0),
            resume_convert_pending: Cell::new(false),
            pending_since: Cell::new(None),
            last_commit: RefCell::new(None),
            feedback_enabled: Cell::new(false),
            preserved_regs: RefCell::new(Vec::new()),
            number_full_width: Cell::new(true),
            punctuation_full_width: Cell::new(true),
            symbol_overlay: Cell::new(false),
            symbol_chars: Cell::new(settings::symbol::SymbolCharSet::EMPTY),
            reading_monitor_enabled: Cell::new(true),
            reading_monitor_accumulate: Cell::new(true),
            reading_monitor_max_chars: Cell::new(34),
            shift_latin_compose: Cell::new(true),
            undo_armed: Cell::new(false),
            ephemeral_kana: Cell::new(false),
            ephemeral_enabled: Cell::new(true),
            keymap: Cell::new(crate::keymap::Keymap::default()),
            _guard: ComObjectGuard::new(),
        }
    }
}

impl ITfTextInputProcessor_Impl for TextService_Impl {
    fn Activate(&self, ptim: Ref<'_, ITfThreadMgr>, tid: u32) -> Result<()> {
        // Medium fix: Deactivate 実行中の同期再入 Activate を最初に弾く。このチェックは
        // tid/thread_mgr のセットを含む「一切の状態変更・登録」より前になければならない —
        // RemoveItem 等の清算コールアウト中に Activate が混入すると、作られた新しい登録世代を
        // 外側 Deactivate の後続清算が道連れに消す。ホストは Deactivate 完了後に改めて
        // Activate すればよい（フラグは Deactivate の全出口で RAII 的に外れる）。
        if self.deactivating.get() {
            tip_log("ev=activate_rejected reason=deactivating");
            return Err(E_FAIL.into());
        }
        self.consume_started_composition();
        // Activation is a new key lifecycle.  A reservation from a previous activation must
        // never be replayed into the new context, even if the host reuses the same VK.
        self.invalidate_pending_end_test_reservation();
        // 1) tid とスレッドマネージャを保持する。
        let tm: ITfThreadMgr = ptim.ok()?.clone();
        self.tid.set(tid);
        *self.thread_mgr.borrow_mut() = Some(tm.clone());

        // 診断: イマーシブ（検索/Store）ホストかを記録する。検索面では自前候補窓は上位 DWM
        // バンドの下で不可視になるため、host へインライン描画を委ねる integratable IF が要る。
        // ※「イマーシブだから自前描画を止める」だけは候補が完全に消える退行と検証で判明したので、
        //   ここでは記録のみに留め、描画戦略の切替は host の pbShow と integratable IF に委ねる。
        if let Ok(ex) = tm.cast::<ITfThreadMgrEx>() {
            if let Ok(flags) = unsafe { ex.GetActiveFlags() } {
                tip_log(&format!(
                    "ev=activate_flags raw=0x{:08X} immersive={}",
                    flags,
                    flags & TF_TMF_IMMERSIVEMODE != 0
                ));
            }
        }

        // キーイベントシンクを advise（自分自身を ITfKeyEventSink として渡す）。
        let ksm: ITfKeystrokeMgr = tm.cast()?;
        let sink: ITfKeyEventSink = self.to_interface();
        unsafe {
            ksm.AdviseKeyEventSink(tid, &sink, true)?;
        }

        // フォーカス変更（別ウィンドウ/アプリ切替）を捕捉する ITfThreadMgrEventSink を advise する。
        // ホスト依存で、別ウィンドウへフォーカスが移ると live preedit は文書へ確定/破棄されるが
        // ITfCompositionSink::OnCompositionTerminated が呼ばれないことがある。その場合 OnSetFocus で
        // 検知してエンジンセッション（前の読み）を畳む。怠ると次入力が古い読みへ連結される
        // （にほんご → 別窓クリック → aiueo で にほんごあいうえお＝日本語あいうえお のデータ残留）。
        // 失敗は致命でない（focus 起点のリセットが働かないだけ）。
        // cookie==0 ガード: ITfSource::AdviseSink は AdviseKeyEventSink と違い再 advise を弾かず
        // 毎回新 cookie を返すため、ガード無しだと二重 Activate で前の cookie を取りこぼし self への
        // 強参照を 1 つリークする（現状は上の AdviseKeyEventSink の `?` で二重 Activate が先に中断
        // されるため到達しないが、防御的に）。
        if self.thread_mgr_event_cookie.get() == 0 {
            if let Ok(source) = tm.cast::<ITfSource>() {
                let tmes: ITfThreadMgrEventSink = self.to_interface();
                if let Ok(cookie) = unsafe { source.AdviseSink(&ITfThreadMgrEventSink::IID, &tmes) }
                {
                    self.thread_mgr_event_cookie.set(cookie);
                }
                // クロスプロセス（別アプリへ前面が移る）でのフォーカス喪失は、スレッド内 doc フォーカス
                // 変化を通知する ITfThreadMgrEventSink::OnSetFocus では届かないことがある。前面（スレッド）
                // フォーカス喪失は ITfThreadFocusSink::OnKillThreadFocus が通知するので併せて advise し、
                // 同じ放棄リセットを焚く（実機の別窓クリックはこちらが主経路）。
                let tfs: ITfThreadFocusSink = self.to_interface();
                if let Ok(cookie) = unsafe { source.AdviseSink(&ITfThreadFocusSink::IID, &tfs) } {
                    self.thread_focus_cookie.set(cookie);
                }
            }
            // フォーカス sink の advise 結果を残す。OnKillThreadFocus の実配送はクロスプロセス前面
            // 喪失でしか起きずヘッドレスでは焚けないが、ITfThreadFocusSink の advise 配線退行は
            // この行で検出できる（item18 が thread_advised=true を必須条件にする）。
            tip_log(&format!(
                "ev=focus_sinks mgr_advised={} thread_advised={}",
                self.thread_mgr_event_cookie.get() != 0,
                self.thread_focus_cookie.get() != 0
            ));
        }

        // 巡1レビュー 8c2354e指摘1+2: LAYOUT_TS は debounce のライフサイクルと独立に Activate で
        // 設定する（ライブ変換 OFF でも変換で候補窓は出る=追従が必要）。sink も Activate 時点で
        // 現在フォーカス中の document へ初期適用する（OnSetFocus はフォーカス「変化」にのみ発火し、
        // Activate 時点で既にフォーカス済みの document では再配送されない）。
        // 巡2 E3: 世代も進める — 前ライフサイクルで投入済みの非同期セッションが再 Activate 後に
        // 発火しても、旧世代の座標が現在の self へ適用されないようにする。
        LAYOUT_TS.with(|p| p.set(self as *const TextService_Impl));
        PREDICTION_RETRY_TS.with(|p| p.set(self as *const TextService_Impl));
        PREDICTION_POLL_TS.with(|p| p.set(self as *const TextService_Impl));
        self.impl_ptr.set(self as *const TextService_Impl);
        self.bump_layout_sink_gen();
        self.refresh_layout_sink_target();

        // SP6b/SP7: 設定を活性化時に1度だけ読む（engine 流の「起動時に1回」=D7）。
        // F-1: feedback の PreserveKey 登録可否（opt-in）を決めるため、登録より**前**に読む。
        // UU-7: load_reporting で読み取り要因を診断ログに残す。AppContainer/LPAC ホスト（検索窓）
        // から settings.json が権限で読めないと Loaded ではなく PermissionDenied になり、
        // 「検索窓でだけ設定が既定に戻る」症状を実機ログで切り分けられる（従来は握り潰しで不可視）。
        let (s, load_outcome) = settings::load_reporting();
        tip_log(&format!("ev=settings_load outcome={load_outcome:?}"));
        // 品質ループ③: 誤変換ワンキー記録の opt-in（既定 false）。Activate で1度読む（D7）。
        // Deactivate の Unpreserve と remember_last_commit（F-5）のゲートにも使う。
        self.feedback_enabled.set(s.feedback.enabled);
        self.prediction_enabled.set(s.inline_prediction.enabled);
        if s.inline_prediction.enabled && warm_prediction_artifacts().is_err() {
            tip_log("ev=prediction_unavailable state=warm_worker_spawn_failed");
        }
        self.number_full_width.set(s.number.full_width);
        self.punctuation_full_width.set(s.punctuation.full_width);
        self.symbol_overlay.set(s.symbol.symbol_overlay());
        self.symbol_chars.set(s.symbol.effective_chars());
        self.reading_monitor_enabled.set(s.reading_monitor.enabled);
        self.reading_monitor_accumulate
            .set(s.reading_monitor.accumulate);
        self.reading_monitor_max_chars
            .set(s.reading_monitor.effective_max_chars());
        self.shift_latin_compose
            .set(crate::key_event_sink::shift_latin_is_compose(
                &s.shift_latin.mode,
            ));
        self.ephemeral_enabled.set(s.ephemeral.enabled);
        self.keymap.set(crate::keymap::Keymap::from_settings(&s));

        // SP5/keymap: モードトグル/再変換/フィードバックの PreservedKey を keymap から登録する。
        // 既定は従来どおり JIS+US 二重登録、明示バインドは単一登録、無効は未登録。
        // 登録の成否は per-key でログに残す(実機で「無変換が効かない」「カスタムキーが
        // OS に拒否された(bare 0x1C の 0x80040506 前例)」を診断するため)。
        {
            let regs = crate::keymap::build_preserved_regs(&self.keymap.get(), s.feedback.enabled);
            for r in &regs {
                let pk = TF_PRESERVEDKEY {
                    uVKey: r.vk,
                    uModifiers: r.modifiers,
                };
                let d: Vec<u16> = r.desc.encode_utf16().collect();
                let res = unsafe { ksm.PreserveKey(tid, &r.guid, &pk, &d) };
                let hr = res.as_ref().err().map(|e| e.code().0 as u32).unwrap_or(0);
                tip_log(&format!(
                    "ev=preservekey desc={:?} vk={:#04x} mods={:#x} ok={} hr={hr:#010x}",
                    r.desc,
                    r.vk,
                    r.modifiers,
                    res.is_ok()
                ));
            }
            *self.preserved_regs.borrow_mut() = regs;
        }

        // 2) 表示属性 GUID を登録して atom を保持する（失敗しても致命的ではない）。
        unsafe {
            if let Ok(cat) = CoCreateInstance::<_, ITfCategoryMgr>(
                &CLSID_TF_CategoryMgr,
                None,
                CLSCTX_INPROC_SERVER,
            ) {
                if let Ok(atom) = cat.RegisterGUID(&GUID_DISPLAY_ATTRIBUTE) {
                    self.da_atom.set(atom);
                }
                if let Ok(atom) = cat.RegisterGUID(&GUID_DISPLAY_ATTRIBUTE_TARGET) {
                    self.da_target_atom.set(atom);
                }
                if let Ok(atom) = cat.RegisterGUID(&GUID_DISPLAY_ATTRIBUTE_PREDICTION) {
                    self.da_prediction_atom.set(atom);
                }
            }
        }

        // SP6a: UIElement マネージャを presenter へ渡す（取得失敗なら None=フォールバック自前描画）。
        let ui_mgr: Option<ITfUIElementMgr> = tm.cast::<ITfUIElementMgr>().ok();
        self.candidate_ui.borrow_mut().set_ui_mgr(ui_mgr);

        // SP5/US: 言語バーへ あ/A モードインジケータを追加する（ITfLangBarItemButton）。
        // 無変換/Alt+; で conversion-mode が切り替わってもユーザが現在モードを視認できるように。
        // 二重 Activate ガード: 既に item があれば再追加しない（item と Rc 共有状態を取りこぼさない）。
        if self.langbar_item.borrow().is_none() {
            // default_direct 適用予定かつ compartment が取れるときだけ AddItem 前に A を出す。
            // 取れなければ live 読みのまま（失敗時に表示A・入力あ を残さない）。
            let will_be_direct = crate::conversion_mode::should_apply_default_direct(
                s.default_direct,
                self.default_direct_applied.get(),
            );
            let compartment_available = self.conversion_compartment().is_some();
            self.langbar_is_direct
                .set(crate::conversion_mode::langbar_direct_for_additem(
                    will_be_direct,
                    compartment_available,
                    self.is_direct_mode(),
                ));
            let item: ITfLangBarItemButton = crate::langbar::ModeLangBarItem::new(
                self.langbar_is_direct.clone(),
                self.langbar_ephemeral.clone(),
                self.langbar_sink.clone(),
                self.langbar_on_toggle.clone(),
            )
            .into();
            let added = if let Ok(lbim) = tm.cast::<ITfLangBarItemMgr>() {
                unsafe { lbim.AddItem(&item).is_ok() }
            } else {
                false
            };
            if added {
                *self.langbar_item.borrow_mut() = Some(item);
                // 右クリックメニュー「切替」用のトグル closure を格納する。自身の COM 参照を owned で
                // 捕まえ、呼ばれるたびに cast_object_ref で &TextService_Impl を復元して、打鍵と同じ
                // dispatch_mode_toggle を通す。active/pending composition の settle を迂回して compartment
                // だけ切り替えてはいけない。通常 composition は current_context、pending-only は
                // dispatch 内で専用 owner context を使う。
                // 参照循環（closure→COM 参照→TextService→この Rc→closure）は Deactivate で None にして断つ。
                // to_interface は #[implement] に列挙した interface のみ可（ComObjectInterface 境界）。
                // このオブジェクトは ITfTextInputProcessorEx を実装しているのでそれを owned で握る。
                let self_com: ITfTextInputProcessorEx = self.to_interface();
                *self.langbar_on_toggle.borrow_mut() = Some(Box::new(move || {
                    // cast_object_ref は QI 相当で &TextService_Impl を返す（0.62 の supported API）。
                    if let Ok(ts) = self_com.cast_object_ref::<crate::text_service::TextService>() {
                        let ctx = ts.current_context.borrow().clone();
                        ts.dispatch_mode_toggle(ctx.as_ref());
                    }
                }));
            }
            tip_log(&format!("ev=langbar_additem ok={added}"));
        }

        // SP6a: Behavior(ホスト発)の drain を起こすため自己ポインタを立てる（Deactivate で落とす）。
        BEHAVIOR_TS.with(|c| c.set(self as *const TextService_Impl));

        // SP6b/SP7 の設定反映（settings は F-1 のため上の PreserveKey 登録前に読み込み済み）。
        let live_on = s.live_conversion.enabled;
        self.live_enabled.set(live_on);
        // 外部LLM変換(Shift+Tab)のフィーチャーフラグ。開発凍結中(settings::LLM_CONVERT_FROZEN)に
        // つき settings 由来の有効化は実効判定で無視する。NOSPACEKEY_LLM_ECHO(engine の echo/診断
        // モード)が立つときだけ dev/テスト用に有効化: production では誰も設定せず(resolve_env_map
        // は echo を出さない)、headless ハーネス item12(Shift+Tab→LLM 配線の echo 検証)はこれで
        // 通る=凍結中も配線コードの回帰を検出できる。
        // Why not(この1点で閉じる理由): 実行時の LLM 発動可否は Cell self.llm_enabled に集約されて
        // おり(set は init の false とここだけ)、resolve_action / start_llm_convert ガード /
        // Shift+Tab 素通し判定は全てこの Cell を読む。Cell を経由しない LLM 経路や第2の set
        // サイトを足すと凍結が漏れる。
        let llm_on = settings::llm_effective_enabled(&s)
            || std::env::var_os("NOSPACEKEY_LLM_ECHO").is_some();
        self.llm_enabled.set(llm_on);
        // 修正変換(Tab)のフィーチャーフラグ。既定 ON。off なら Tab は IME 機能として扱わず素通しする
        // （llm_enabled と独立 — Shift+Tab の外部LLM変換には無関係）。
        self.typo_enabled.set(s.typo_correct.enabled);
        // SP7: default_direct なら起動時の conversion-mode を半角英数(直接入力)へ初期化。
        // このインスタンスで1度だけ適用する（default_direct_applied ガード）。Deactivate でも
        // リセットしないので、IME 切替で再 Activate されてもユーザの手動トグルを巻き戻さない。
        // 適用に失敗した間は applied を立てない＝次回 Activate で再試行する（成功が1度だけ）。
        if crate::conversion_mode::should_apply_default_direct(
            s.default_direct,
            self.default_direct_applied.get(),
        ) && self.apply_default_direct()
        {
            self.default_direct_applied.set(true);
        }

        // 3) エンジン**接続**は「最初の打鍵時」に遅延確立する（ensure_engine）。
        //    cold start ②: プロセス自体は本メソッド末尾で先行起動する（prespawn_engine —
        //    singleton＋SpawnGuard 直列化で切替の大量起動にはならない。従来の「活性化では
        //    起こさない」設計を、初回打鍵の重い一拍の解消のため意図的に変更）。
        tip_log("Activate");
        tip_log(&format!(
            "ev=activate live_conversion={live_on} llm={llm_on} typo={} default_direct={} feedback={}",
            s.typo_correct.enabled, s.default_direct, s.feedback.enabled
        ));

        // A7: 電源復帰通知を購読する（既に Some なら再登録しない＝二重 Activate 防御。cookie ガードと
        // 同じ流儀）。register 直後に世代を同期しないと、Deactivate→再 Activate（IME 切替往復）で
        // 新 PowerEvents の gen=0 と旧 last_resume_gen がズレ、初打鍵に偽 resume 反映が出る（I-2）。
        if self.power_notify.borrow().is_none() {
            *self.power_notify.borrow_mut() = crate::power::register(self.engine_pipe_name());
            if let Some(h) = self.power_notify.borrow().as_ref() {
                self.last_resume_gen.set(h.events().resume_gen());
            }
            self.resume_convert_pending.set(false);
        }

        // cold start ②: IME 切替（Activate）の時点でエンジンを先行起動しておく。
        // 接続はしない（初回打鍵の ensure_engine の 200ms 接続が即成功する状態を作るだけ）。
        self.prespawn_engine();

        Ok(())
    }

    fn Deactivate(&self) -> Result<()> {
        // Medium fix: ネスト Deactivate（清算の COM コールアウト中にホストが同期再入する）は
        // 二重清算をしない — 外側の Deactivate が継続中なので即座に戻る（冪等）。
        if self.deactivating.get() {
            tip_log("ev=deactivate_skipped reason=nested");
            return Ok(());
        }
        // Medium fix: 実行中フラグは RAII で立てる — return・panic unwind を含む全出口で
        // 外れる。外れ残ると以後の Activate が永久に拒否される。
        let _deactivating = DeactivatingGuard::new(&self.deactivating);
        // 巡4 T6: Deactivate は多数の COM コールアウト（UnadviseKeyEventSink/RemoveItem/
        // UnadviseSink/SetValue/hide/destroy）を含む入口で、relayout の show() 中の同期再入で
        // 保持中 RefCell の再借用 panic が起きうる — thunk に保護が無く abort になるため、
        // 他入口と同じ規律で包む。
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.deactivate_inner()));
        match r {
            Ok(result) => result,
            Err(_) => {
                // High fix: panic を握り潰して Ok を返すのは嘘になる — ホストは Deactivate
                // 成功と信じたまま再 Activate せず、清算途中の状態が固定化される。失敗を
                // 素直に伝える（フラグは RAII で既に復帰済み＝再 Activate 可能）。
                tip_log("ev=panic site=Deactivate");
                Err(E_FAIL.into())
            }
        }
    }
}

/// High fix: Deactivate preflight の取消方式（純粋ロジック＝単体テスト可能）。
#[derive(Debug, PartialEq, Eq)]
enum DeactivateCancelPlan {
    /// composition 無し — 取消不要、そのまま清算へ。
    Nothing,
    /// composition ありだが所有 context 無し — 取消不能。清算前に中断（再試行可能）。
    AbortNoContext,
    /// 再変換中 — RestoreText で元ラテンを書き戻す cancel_reconvert（do_cancel は原文を消す）。
    CancelReconvert,
    /// 通常合成は CancelComposition、pending-end は close-only で閉じる do_cancel。
    DoCancel,
}

/// composition の有無・取消 context・再変換ラッチ・本文確定済み pending-end から
/// Deactivate preflight の取消方式を決める。pending-end は reconverting より優先し、
/// RestoreText/空文字化をせず close-only の DoCancel へ送る。
fn deactivate_cancel_plan(
    has_composition: bool,
    has_ctx: bool,
    reconverting: bool,
    end_pending: bool,
) -> DeactivateCancelPlan {
    if !has_composition {
        DeactivateCancelPlan::Nothing
    } else if !has_ctx {
        DeactivateCancelPlan::AbortNoContext
    } else if end_pending {
        DeactivateCancelPlan::DoCancel
    } else if reconverting {
        DeactivateCancelPlan::CancelReconvert
    } else {
        DeactivateCancelPlan::DoCancel
    }
}

/// Medium fix: Deactivate 実行中フラグの RAII ガード。new で true を立て、Drop（return・
/// panic unwind を含む全出口）で false に戻す。STA 専用（&Cell のみ保持）で Send 不要。
struct DeactivatingGuard<'a> {
    flag: &'a Cell<bool>,
}

impl<'a> DeactivatingGuard<'a> {
    fn new(flag: &'a Cell<bool>) -> Self {
        flag.set(true);
        Self { flag }
    }
}

impl Drop for DeactivatingGuard<'_> {
    fn drop(&mut self) {
        self.flag.set(false);
    }
}

impl TextService_Impl {
    fn deactivate_inner(&self) -> Result<()> {
        // 通常 preedit と別 slot の予測ゴーストも、登録解除より前に本文を消して閉じる。
        if !self
            .abandon_prediction_for_context_change(crate::prediction_state::Invalidation::Disabled)
        {
            tip_log("ev=deactivate_abort reason=prediction_cancel_rejected");
            return Err(E_FAIL.into());
        }
        // High fix: 取消 preflight を**最初**の操作として行う（unadvise_layout_sink 等の
        // 不可逆清算より前）。従来は sink/cookie 解除の後に取消を試み、その失敗（context 無し・
        // edit session 拒否）を無視して composition を強制クリアしていた — 文書に合成が孤児化し、
        // 再変換中なら reconvert_original ごと消えて元ラテンが復元不能になる。ここでは何も壊す
        // 前に中断して Err を返す（composition・再変換ラッチ/原文・入力状態・engine/session・
        // 登録を全保持＝ホストの再 Deactivate で再試行可能）。成功/元々 composition 無しのみが
        // 続きの清算に進める。通常 composition は current_context、SetText 済みの pending-end
        // composition は専用 owner context を使う。どちらも無い場合だけ取消不能な例外状態。
        let has_composition = self.composition.borrow().is_some();
        let ctx: Option<ITfContext> = if has_composition {
            if self.composition_end_pending.get() {
                self.composition_end_context.borrow().clone()
            } else {
                self.current_context.borrow().clone()
            }
        } else {
            None
        };
        let plan = deactivate_cancel_plan(
            has_composition,
            ctx.is_some(),
            self.reconverting.get(),
            self.composition_end_pending.get(),
        );
        let cancel_ok = match (&plan, ctx.as_ref()) {
            (DeactivateCancelPlan::Nothing, _) => true,
            (DeactivateCancelPlan::AbortNoContext, _) if self.composition_end_pending.get() => {
                // The text is already committed.  No owner context is terminal for this
                // close-only marker, so release it and let Deactivate finish.
                self.abandon_pending_composition_end("deactivate_no_context");
                true
            }
            (DeactivateCancelPlan::AbortNoContext, _) => {
                tip_log("ev=deactivate_abort reason=no_context");
                return Err(E_FAIL.into());
            }
            (DeactivateCancelPlan::CancelReconvert, Some(ctx)) => {
                // 再変換中はユーザの**既存テキスト**の上に composition が張られている。
                // do_cancel(CancelComposition) は range を空文字で潰す＝元テキストを消すため、
                // RestoreText で元ラテンを書き戻す cancel_reconvert を使う（Esc / Behavior::Abort
                // と同じ取消経路）。これを怠ると再変換中の IME 切替でユーザの原文が消失する。
                self.cancel_reconvert(ctx)
            }
            (DeactivateCancelPlan::DoCancel, Some(ctx)) => self.do_cancel(ctx),
            // plan が Cancel* を返すとき ctx は必ず Some（None は AbortNoContext が拾う）。
            _ => unreachable!("cancel plan and context presence must agree"),
        };
        if !cancel_ok {
            if self.composition_end_pending.get() {
                // Deactivate is a terminal lifecycle boundary.  A locked owner context must
                // not survive into an unknown future activation; SetText already committed,
                // so dropping the stale handle is safer than retaining an input barrier.
                self.abandon_pending_composition_end("deactivate");
            } else {
                tip_log("ev=deactivate_abort reason=cancel_rejected");
                return Err(E_FAIL.into());
            }
        }
        // 取消成功（または元々 composition 無し）だけがここを通れる。edit session
        // （RestoreText/CancelComposition）が composition を閉じた後の保険として sink 強参照を
        // 断つ C-2 の解放点 — 失敗経路は上で中断済みなので、composition を None にしてよいのは
        // この行だけ。
        *self.composition.borrow_mut() = None;
        self.composition_end_pending.set(false);
        *self.composition_end_context.borrow_mut() = None;
        self.composition_end_status
            .set(CompositionEndStatus::Closed);
        self.composition_end_retry_count.set(0);
        self.composition_generation
            .set(self.composition_generation.get().wrapping_add(1));
        // 事後検証(2026-08-20): レイアウト sink の解除は（preflight の次＝不可逆清算の）
        // **最初**に行う — ここより後の
        // COM コールアウト（UnadviseKeyEventSink/RemoveItem/hide/destroy 等）で再入 panic が
        // 起きると Deactivate 全体が catch_unwind で握り潰されてここまで到達せず、
        // context→sink→context の循環参照が残る。Drop の保険 unadvise も循環参照が
        // 残る以上は到達しない（巡3 G5 の不変条件）ので、この前方移動が唯一の保険。
        // UIバグ4: レイアウト sink を外す（context の寿命はホスト管理で、Deactivate 後の
        // OnLayoutChange で self を触らせない。次 Activate で再 advise される）。
        self.unadvise_layout_sink();
        // 巡1レビュー 8c2354e指摘4: 非同期セッションの保留も此处で掃討 — Deactivate 後に
        // 遅延実行された旧 context のセッションが layout_refresh_apply で self を触らない
        // ように LAYOUT_TS を null 化し、pending フラグも戻す（次 Activate で再設定）。
        // 巡2 E2: null 化は TLS が自分を指しているときだけ（旧インスタンスの Deactivate が
        // 同一 STA に載る新インスタンスの TLS をワイプしない防御）。
        self.layout_refresh_pending.set(false);
        self.bump_layout_sink_gen();
        LAYOUT_TS.with(|p| {
            if std::ptr::eq(p.get(), self) {
                p.set(std::ptr::null());
            }
        });
        // advise を解除し、保持状態を破棄する。
        // 巡4 T6: if let の一時 Ref はブロック末尾まで延命される（edition 2021）のため先に束縛 —
        // UnadviseKeyEventSink/RemoveItem/UnadviseSink のコールアウト中に再入した
        // borrow_mut()（Activate 等）との衝突を防ぐ。
        let thread_mgr = self.thread_mgr.borrow().clone();
        if let Some(tm) = thread_mgr.as_ref() {
            if let Ok(ksm) = tm.cast::<ITfKeystrokeMgr>() {
                unsafe {
                    let _ = ksm.UnadviseKeyEventSink(self.tid.get());
                }
                // SP5/keymap: Activate で登録した実物(preserved_regs)を対称に解除する。
                for r in self.preserved_regs.borrow().iter() {
                    let pk = TF_PRESERVEDKEY {
                        uVKey: r.vk,
                        uModifiers: r.modifiers,
                    };
                    let _ = unsafe { ksm.UnpreserveKey(&r.guid, &pk) };
                }
                self.preserved_regs.borrow_mut().clear();
            }
            // SP5/US: Activate で追加した言語バーモードインジケータを除去する（AddItem と対）。
            // 右クリックメニュー用トグル closure を落とす（RemoveItem と対）。closure は自身の
            // COM 参照を owned で保持しており、TextService がこの Rc を保持するため相互参照（循環）
            // になっている。ここで None にして循環を断ち、Activate/Deactivate 往復でのリークを防ぐ。
            *self.langbar_on_toggle.borrow_mut() = None;
            // 巡4 T6 と同型: `if let` のスクラッチ式に置いた一時 RefMut はブロック末尾まで
            // 延命される（edition 2021）ため、take() を独立 let 文へ切り出して RefMut を
            // RemoveItem の COM コールアウト前に落とす。コールアウト中の再入（GetText 等
            // 経由で langbar_item を borrow する Activate 等）との borrow-panic を防ぐ。
            let item = self.langbar_item.borrow_mut().take();
            if let Some(item) = item {
                if let Ok(lbim) = tm.cast::<ITfLangBarItemMgr>() {
                    unsafe {
                        let _ = lbim.RemoveItem(&item);
                    }
                }
            }
            *self.langbar_sink.borrow_mut() = None;
            // フォーカス sink（ITfThreadMgrEventSink / ITfThreadFocusSink）を解除する
            // （Activate の AdviseSink と対）。cookie 0 は未登録。残すと TextService への強参照が
            // 居残りリーク/UAF の温床になる。
            let tmes_cookie = self.thread_mgr_event_cookie.replace(0);
            let tfs_cookie = self.thread_focus_cookie.replace(0);
            if tmes_cookie != 0 || tfs_cookie != 0 {
                if let Ok(source) = tm.cast::<ITfSource>() {
                    unsafe {
                        if tmes_cookie != 0 {
                            let _ = source.UnadviseSink(tmes_cookie);
                        }
                        if tfs_cookie != 0 {
                            let _ = source.UnadviseSink(tfs_cookie);
                        }
                    }
                }
            }
        }
        // 巡15(round2): 冒頭の unadvise_layout_sink のあと〜この行までの間に、TMES/TFS 経由の
        // 再入（OnSetFocus/OnPushContext/OnPopContext → refresh_layout_sink_target → AdviseSink）
        // が layout sink を張り直すことがある（thread_mgr はまだ Some・key/langbar sink も
        // コールアウト中に生きていた）。ここ以降は再入源（key/langbar/TMES/TFS の各 sink）が
        // 全て外れているので再 advise は起きない — cookie==0 なら no-op の冪等呼び出しで
        // 張り直された分を最終確認する（巡1と同型の循環参照残存を Deactivate 成功経路でも
        // 残さない。旧配置の「末尾 unadvise が回収する」性質の復元）。
        self.unadvise_layout_sink();
        // C-2（composition 取消と sink 強参照の解放）は preflight（関数冒頭）へ前方移動した。
        // 旧位置の「context 無しでも composition を強制クリア」保険は廃止 — 取消に失敗した
        // 状態でクリアすると文書へ孤児合成を残し再変換元を消失させるので、失敗は何も壊さ
        // ない Err 中断（再試行可能）に置き換えた（High fix）。

        // エンジン接続を破棄する。EndSession の同期往復は送らない — Deactivate は IME 切替時に
        // 切替先プロセスの UI スレッドで走るため、エンジンが多忙（serviceLock 直列化）だと
        // ここでの往復（read tier 250ms + 非有界 write）が切替そのものを塞ぐ（2026-07-10
        // 跨プロセスブロッキング監査 B4）。接続 drop（pipe close）でエンジン側 onDisconnect →
        // cleanupConnection が同じ endSession 経路でこの接続の全セッションを掃除する（Bug 2 で
        // テスト済みの契約）ので、往復は冗長。
        // ⚠この契約は --persist モード限定（oneShot は NamedPipeServer が onDisconnect を呼ばず
        // 学習 flush も走らない）。本 TIP の spawn は常に --persist（spawn_engine_hidden）なので
        // 現行経路では成立するが、oneShot を再有効化する改修はここを再考すること（レビュー I-1）。
        *self.client.borrow_mut() = None;
        self.engine_session.set(0);
        // 保留中の EndSession も破棄する。残すと再活性化で別 oneShot エンジン（id は 1 から振り直し）を
        // 起動した後、古いワーカ由来の LLM 結果が flush され、別エンジンの無関係なセッションを
        // 巻き添えに終了させうる。エンジンが変わる以上、古い保留 id は無効。
        self.pending_end_session.set(0);
        // 次の活性化で（エンジンが死んでいれば）起動し直せるようにする。
        self.spawn_attempted.set(false);
        // A7: 電源復帰通知の購読を解除する（Drop が PowerUnregisterSuspendResumeNotification を呼ぶ）。
        // 未消費の resume_convert_pending も持ち越さない（次の Activate の世代同期で改めて false に揃う
        // が、非活性中の古い状態を残さないため明示的に畳む）。
        *self.power_notify.borrow_mut() = None;
        self.resume_convert_pending.set(false);

        // 保留中のデバウンスタイマを解除し、保持 context を捨てる。
        self.disarm_debounce();
        self.disarm_llm_poll();
        // 巡1検証G2: 非活性化で旧 context のアンカー保持も破棄 — 再 Activate 後の初回照会失敗で
        // 前ライフサイクルの座標を使わない（password_ctx_key と同じ ABA 対策）。
        *self.last_valid_anchor.borrow_mut() = None;
        // 入力状態を全て畳む（raw/composing/phase）。従来は set_awaiting_llm(false) だけで
        // raw/composing を残していたが、それだと再活性化後の初打鍵で needs_session_reseed が
        // 「session==0 かつ raw 非空＝合成途中の喪失」と誤認し、上の do_cancel で取消済みの
        // テキストが新セッションへリプレイされて復活する（2026-07-07 レビュー I-1 の偽陽性
        // リプレイ）。reset() は phase=Composing も含む＝AwaitingLlm 居残り防止も従来どおり。
        self.state.borrow_mut().reset();
        self.partial_preedit_redraw_pending.set(false);
        self.partial_preedit_redraw_retries.set(0);
        self.live_text.borrow_mut().clear();
        *self.llm_slot.borrow_mut() = None;
        self.llm_started.set(None);
        *self.current_context.borrow_mut() = None;
        // U9: Deactivate の保険経路（context 無しで do_cancel/cancel_reconvert が走らなかった
        // 場合）でも左文脈を持ち越さない。取消経路のクリアと重複しても無害（最終レビュー Minor-2）。
        *self.left_context.borrow_mut() = None;
        self.monitor_committed_reading.borrow_mut().clear();
        // SP5: 再変換ラッチも残さない（残ると再活性化後に start_reconvert の
        // 再入ガードに居残り、以降の再変換が不能になる＝awaiting_llm と同じ理由）。
        self.reconverting.set(false);
        self.reconvert_original.borrow_mut().clear();
        self.reconvert_reading.borrow_mut().clear();
        // 品質ループ③: 直前確定バッファも持ち越さない（再活性化後の Ctrl+変換が
        // 非活性前の古い確定を記録しないように）。
        *self.last_commit.borrow_mut() = None;
        // 確定取消: armed も必ず落とす（last_commit クリアと並記 — 再活性化後に
        // 非活性前の武装状態が Ctrl+Backspace を誤発火させないように）。
        self.undo_armed.set(false);
        // ephemeral かな: 非活性化（IME 切替/シャットダウン）でも direct へ復帰する。thread_mgr が
        // まだ有効な（下で None にする前の）この時点で呼ぶ必要がある。
        self.exit_ephemeral_to_direct(None);
        if self.ephemeral_kana.replace(false) {
            // Deactivate は再試行先を失う終端。compartment 取得/書込み失敗で保留が残った場合は
            // 次 Activate へ ephemeral marker を持ち越さず、live 値追従へ戻す。
            self.direct_mode_owned.set(false);
            self.langbar_ephemeral.set(false);
            tip_log("ev=ephemeral_exit abandoned=deactivate");
        }
        // SP7: 上の「ephemeral 復帰失敗」以外では default_direct_applied / direct_mode_owned を
        // **意図的にリセットしない**。毎回リセットすると IME 切替の往復
        // （Deactivate→Activate）でユーザが無変換により選んだモードを巻き戻すため。

        // 候補ウィンドウを隠す（presenter なら UIElement も EndUIElement で畳む）。
        self.candidate_ui.borrow_mut().hide();
        self.showing.set(false);
        self.clear_clause_nav();
        // SP6a: 非活性化後に Behavior が来ても dangling self を触らせない（UAF 防止）。
        // ui_mgr も手放して保持していた COM 参照を解放する。
        // 巡3 G2: null 化は TLS が自分を指しているときだけ（LAYOUT_TS/Drop と同じ E2 規律 —
        // 旧インスタンスの Deactivate が同一 STA に載る新インスタンスの Behavior 経路を
        // ワイプしない）。
        BEHAVIOR_TS.with(|c| {
            if std::ptr::eq(c.get(), self) {
                c.set(std::ptr::null());
            }
        });
        // 巡4 T1/T5: 遅延 flush タイマと busy 再送タイマも掃討 — 非活性化後の発火で self を
        // 触らせない（キュー済み発火は proc 側の ID 照合/TLS null で無害化される）。
        let bf = self.behavior_flush_timer.replace(0);
        if bf != 0 {
            unsafe {
                let _ = KillTimer(None, bf);
            }
        }
        let rt = self.reload_retry_timer.replace(0);
        if rt != 0 {
            unsafe {
                let _ = KillTimer(None, rt);
            }
        }
        self.reload_retry_count.set(0);
        self.disarm_prediction_finish_retry();
        self.disarm_prediction_poll();
        self.invalidate_prediction(crate::prediction_state::Invalidation::Disabled);
        PREDICTION_RETRY_TS.with(|p| {
            if p.get() == self as *const TextService_Impl {
                p.set(std::ptr::null());
            }
        });
        PREDICTION_POLL_TS.with(|p| {
            if p.get() == self as *const TextService_Impl {
                p.set(std::ptr::null());
            }
        });
        self.candidate_ui.borrow_mut().set_ui_mgr(None);
        // レイアウト sink の掃討（unadvise/pending/世代/LAYOUT_TS）は deactivate_inner の
        // 冒頭へ移動済み（panic で握り潰された場合の循環参照残存防止 — 事後検証指摘）。

        // 候補窓・モード HUD の DirectComposition/D3D リソースをここで畳む。畳まずに
        // 放置すると、プロセス終了時の msctf 後始末（LdrShutdownProcess 中の
        // IUnknown::Release 経由の DestroyWindow）で初めて WM_NCDESTROY が飛び、
        // SurfaceRenderer の drop がプロセス終了中に dcomp を触って dxgi の例外
        // （STATUS_FATAL_USER_CALLBACK_EXCEPTION, c000041d）でホストごと落ちる。
        // プロセスが健全な Deactivate 時点で破棄すれば、終了時は hwnd が null で no-op。
        self.candidate_ui.borrow_mut().destroy_window();
        self.mode_hud.borrow_mut().destroy();
        self.reading_monitor.borrow_mut().destroy();

        *self.thread_mgr.borrow_mut() = None;
        self.tid.set(0);
        Ok(())
    }
}

impl TextService_Impl {
    // 巡4 T6: Deactivate の本体（inherent 側へ置く — trait impl 内の非 trait メソッドは E0407）。
}

impl ITfTextInputProcessorEx_Impl for TextService_Impl {
    fn ActivateEx(&self, ptim: Ref<'_, ITfThreadMgr>, tid: u32, _dwflags: u32) -> Result<()> {
        // 拡張活性化は通常の Activate に委譲する。
        self.Activate(ptim, tid)
    }
}

impl ITfTextEditSink_Impl for TextService_Impl {
    fn OnEndEdit(
        &self,
        pic: Ref<'_, ITfContext>,
        _ecreadonly: u32,
        peditrecord: Ref<'_, ITfEditRecord>,
    ) -> Result<()> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let in_our_write_session = pic
                .ok()
                .ok()
                .and_then(|ctx| unsafe { ctx.InWriteSession(self.tid.get()).ok() })
                .is_some_and(|inside| inside.as_bool());
            if self.prediction_editing.get() || in_our_write_session {
                return Ok(());
            }
            let has_prediction_activity = self.prediction_state.borrow().has_private_state()
                || self.prediction_ghost_visible();
            if !has_prediction_activity {
                return Ok(());
            }
            let selection_changed = peditrecord
                .ok()
                .ok()
                .and_then(|record| unsafe { record.GetSelectionStatus().ok() })
                .is_some_and(|changed| changed.as_bool());
            if self.consume_expected_prediction_commit_end_edit(selection_changed) {
                tip_log("ev=prediction_commit_edit state=settled");
                return Ok(());
            }
            tip_log(&format!("ev=prediction_invalidate source=external_edit selection_changed={selection_changed}"));
            // OnEndEdit 内から同期 edit session は要求できないため、range除去だけ非同期に積む。
            self.invalidate_prediction_after_external_edit();
            Ok(())
        }));
        match result {
            Ok(result) => result,
            Err(_) => {
                tip_log("ev=panic site=OnEndEdit");
                Ok(())
            }
        }
    }
}

impl ITfCompositionSink_Impl for TextService_Impl {
    fn OnCompositionTerminated(
        &self,
        _ecwrite: u32,
        pcomposition: Ref<'_, ITfComposition>,
    ) -> Result<()> {
        // 巡2 F2: この COM 入口は windows-implement の生成 thunk に panic 保護が無く、
        // 非 unwind ABI（extern "system"）出口からの unwind は abort になる。このメソッドは
        // relayout（candidate_ui の RefMut 保持中の Begin/UpdateUIElement コールアウト）や
        // drain_behavior の最中にホストから同期再入され、reset_abandoned_composition が
        // 保持中 RefCell を再借用して panic しうる唯一の入口 — OnKeyDown と同じくここで
        // 受け止めないとホストプロセスごと落ちる。
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.on_composition_terminated_inner(_ecwrite, &pcomposition)
        }));
        match r {
            Ok(result) => result,
            Err(_) => {
                tip_log("ev=panic site=OnCompositionTerminated");
                Ok(())
            }
        }
    }
}

impl TextService_Impl {
    fn consume_expected_prediction_commit_end_edit(&self, selection_changed: bool) -> bool {
        consume_expected_prediction_commit_end_edit(
            &self.prediction_commit_edit_deadline,
            selection_changed,
            Instant::now(),
        )
    }

    fn on_composition_terminated_inner(
        &self,
        _ecwrite: u32,
        pcomposition: &Ref<'_, ITfComposition>,
    ) -> Result<()> {
        let is_prediction = match (
            pcomposition.ok().ok(),
            self.prediction_composition.borrow().as_ref(),
        ) {
            (Some(p), Some(cur)) => com_identity_eq(p, cur),
            _ => false,
        };
        if is_prediction {
            let internal = self.prediction_editing.get();
            *self.prediction_composition.borrow_mut() = None;
            *self.prediction_context.borrow_mut() = None;
            self.prediction_finish_pending.set(None);
            if self.prediction_deferred_preserved.borrow().is_empty() {
                self.disarm_prediction_finish_retry();
            } else {
                // internal callback でも、timer 作成失敗後に別経路の async finish が成功した
                // 場合がある。キューが残る限り必ず replay の安全点を予約する。
                self.arm_prediction_finish_retry(false);
            }
            if !internal {
                self.invalidate_prediction(crate::prediction_state::Invalidation::Input);
            }
            tip_log(if internal {
                "ev=prediction_comp_terminated source=internal"
            } else {
                "ev=prediction_comp_terminated source=host"
            });
            return Ok(());
        }
        // 終了通知が現在追跡中の composition のものか確認する。フォーカス喪失 sink
        // （OnSetFocus/OnKillThreadFocus）は composition を End せずに手放す（ホストが既に確定/破棄
        // するため）。その後ユーザが戻って新しい composition を張った後で、ホストが古い（放棄した）
        // composition の終了を遅延配送することがある。識別せず無条件リセットすると、新しい入力/
        // セッションまで巻き添えに畳む。現在の composition と一致しない終了は stale として無視する
        // （self.composition が None＝idle の終了通知も無視で安全）。
        let is_current = match (pcomposition.ok().ok(), self.composition.borrow().as_ref()) {
            (Some(p), Some(cur)) => com_identity_eq(p, cur),
            _ => false,
        };
        if !is_current {
            tip_log("ev=comp_terminated skipped=stale");
            return Ok(());
        }
        if self.composition_end_pending.get() && !self.pending_end_generation_is_current() {
            // A lifecycle boundary advanced the generation while a host callback was in
            // flight.  Do not let that callback clear a newer pending state.
            tip_log("ev=comp_terminated skipped=stale_generation");
            return Ok(());
        }
        // SetText 済みで close-only 再試行待ちだった composition の終了通知。本文確定後の
        // InputState は呼出し側が既に次状態へ進めているため、通常の放棄 reset を掛けず handle と
        // marker だけを清算する（部分確定の残り読みも巻き添えにしない）。
        if self.composition_end_pending.get() {
            let owner_ctx = self.composition_end_context.borrow().clone();
            *self.composition.borrow_mut() = None;
            self.composition_end_pending.set(false);
            *self.composition_end_context.borrow_mut() = None;
            self.composition_end_status
                .set(CompositionEndStatus::Closed);
            self.composition_end_retry_count.set(0);
            // TestKeyDown と KeyDown の間に pending callback が入っても、ここでは key-pair
            // slot/generation に触れない。次の matching KeyDown が一度だけ消費する。
            // commit_and_reset 済みの idle ephemeral だけをここで direct へ戻す。部分確定中は
            // logical composition が継続するため marker/mode を維持する。
            if !self.state.borrow().composing && !self.showing.get() {
                self.partial_preedit_redraw_pending.set(false);
                self.partial_preedit_redraw_retries.set(0);
                self.exit_ephemeral_to_direct(owner_ctx.as_ref());
            } else if self.partial_preedit_redraw_pending.get() {
                // composition sink callback 内から同期 edit session を張らず、STA timer へ逃がす。
                // close 完了という新しい進捗があったので、過去の拒否回数は持ち越さない。
                self.partial_preedit_redraw_retries.set(0);
                self.arm_debounce();
            }
            tip_log("ev=comp_terminated closed=pending_end");
            return Ok(());
        }
        // 部分確定中(commit_candidate)の自己誘発終了なら何もしない。do_commit が（ホスト依存で）
        // OnCompositionTerminated を同期再入させても、ここでセッション/状態を畳むと直後の reseed が
        // 保持したい残り読みセッションを失う。composition Rc は CommitText が既に None 化済みで、
        // 状態は reseed が張り直すので no-op で安全。
        if self.partial_committing.get() {
            tip_log("ev=comp_terminated skipped=partial_commit");
            return Ok(());
        }
        // アプリ側都合で composition が終了した。内部状態を初期化する。
        tip_log("ev=comp_terminated");
        self.reset_abandoned_composition();
        Ok(())
    }
}

impl TextService_Impl {
    /// 合成がアプリ側都合で終わった/放棄されたときの内部状態リセット
    /// （`OnCompositionTerminated` と、別ウィンドウへのフォーカス喪失 `OnSetFocus` で共有）。
    /// 文書側はホストが既に確定/破棄済みなので、ここでは cancel/commit はせず**自分の状態だけ**畳む。
    pub(crate) fn reset_abandoned_composition(&self) {
        // 放棄時点で LLM(Tab変換)が in-flight だったか（client がワーカへ move 済みか）を、
        // state.reset() が phase を畳む前に捕まえる。awaiting_llm ⟺ client はワーカ側。
        let was_awaiting_llm = self.state.borrow().awaiting_llm();

        let keep_pending_end = self.composition_end_pending.get();
        if keep_pending_end {
            // Focus/context abandonment is an explicit liveness boundary.  Do not retain the
            // old owner context waiting for a callback that may never arrive; any late callback
            // is stale after the slot/generation is advanced.
            self.abandon_pending_composition_end("focus_abandon");
        } else {
            *self.composition.borrow_mut() = None;
            *self.composition_end_context.borrow_mut() = None;
        }
        self.state.borrow_mut().reset();
        self.partial_preedit_redraw_pending.set(false);
        self.partial_preedit_redraw_retries.set(0);
        // Abandon/reset is a lifecycle boundary.  The old Test→Key pair belongs to the discarded
        // context/composition and must not be replayed into the next input.
        self.invalidate_pending_end_test_reservation();
        // U9: 合成放棄 — 次 composition の再捕捉まで前文書の左文脈を残さない。
        *self.left_context.borrow_mut() = None;
        self.monitor_committed_reading.borrow_mut().clear();

        if was_awaiting_llm {
            // in-flight LLM の最中に放棄された。client はワーカへ move 済みで、ここでポーリングを
            // 止めると on_llm_outcome が走らず client が戻らない＝engine が orphan 化し、
            // spawn_attempted 立ちっぱで同一活性化中の以後の入力が degraded（Codex P2）。abort_llm と
            // 同じ engine 後始末をする（合成ごと放棄するので restore_pre_llm はしない）: 世代を進めて
            // 遅延結果を stale 化、ポーリング/スロット/起点時刻を片付け、ワーカが掴んだ engine を kill して
            // 新パイプへ切替。drop_engine が spawn_attempted を落とすので次入力で再 spawn/再接続できる。
            self.state.borrow_mut().bump_llm_seq();
            self.disarm_llm_poll();
            *self.llm_slot.borrow_mut() = None;
            self.llm_started.set(None);
            self.pipe_name.borrow_mut().clear();
            // 共有 engine は殺さない（他ホストが接続中の永続 singleton。旧 oneShot 専用 engine 時代の
            // kill をここで行うと設定アプリ等を巻き込んで変換不可にする）。drop_engine が Child ハンドルを
            // 手放す（プロセスは継続）。ブロック中の LLM worker は engine 応答で自然完了し、戻った接続は
            // stale 化済みなので drop される＝その1接続のみ閉じ engine は生存する。
            self.drop_engine();
        } else {
            // 通常経路。「残り読みを保持したまま生きているエンジンセッション」がここで宙に浮く。
            // 終了しないと次 composition の ensure_session が古いセッション（残り読み入り）を再利用し、
            // 新規入力が残骸かなへ連結されて文字化けする（defect#2 / フォーカス喪失データ残留）。
            // 生きている client で EndSession を送る。session==0 ガードで冪等。
            self.engine_end_session();
        }

        self.showing.set(false);
        self.clear_clause_nav();
        // 巡3 P4: 再入 panic しうる借用点は UI hide（candidate_ui/reading_monitor の RefMut）—
        // 状態・フラグの清算を先に確実に済ませ、hide を最後に置く。途中で panic が起きても
        // 残るのは表示だけ（次の hide 経路で回収）で、reconverting 残留（回収不能）を避ける。
        self.disarm_debounce();
        *self.current_context.borrow_mut() = None;
        self.live_text.borrow_mut().clear();
        // SP5: 再変換候補の表示中に放棄された場合（フォーカス喪失等）も reconverting を必ず落とす。
        // 落とさないと start_reconvert の再入ガードに居残り、以降の再変換が不能になる
        // （候補キーは showing=false で食わず解除経路に到達しない）。
        self.reconverting.set(false);
        self.reconvert_original.borrow_mut().clear();
        self.reconvert_reading.borrow_mut().clear();
        // ephemeral かな: 合成が畳まれた以上 direct へ復帰する（OnCompositionTerminated など
        // フォーカス変化を伴わない放棄経路でも残留させない。非 ephemeral 時は no-op）。
        self.exit_ephemeral_to_direct(None);
        // 通常の current_context/読みは捨てる。pending-end は focus/context 境界で既に
        // quarantine 済みなので、旧 owner context をここへ持ち越さない。
        self.candidate_ui.borrow_mut().hide();
        self.reading_monitor.borrow_mut().hide();
    }
}

impl ITfThreadMgrEventSink_Impl for TextService_Impl {
    fn OnInitDocumentMgr(&self, _pdim: Ref<'_, ITfDocumentMgr>) -> Result<()> {
        Ok(())
    }
    fn OnUninitDocumentMgr(&self, _pdim: Ref<'_, ITfDocumentMgr>) -> Result<()> {
        Ok(())
    }
    fn OnPushContext(&self, _pic: Ref<'_, ITfContext>) -> Result<()> {
        self.consume_started_composition();
        // 巡4 T6: refresh_layout_sink_target の Advise/UnadviseSink コールアウト中に同期再入しうる
        // 入口 — 保護なし入口の panic は shim 越えで abort するため、他入口と同じ規律で包む。
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.password_ctx_key.set(0); // Spec2: context 切替で password キャッシュ無効化（ABA 対策・I-3）
            let _ = self.abandon_prediction_for_context_change(
                crate::prediction_state::Invalidation::FocusChanged,
            );
            *self.prediction_failed_context.borrow_mut() = None;
            // UIバグ5: context スタックが変われば旧 context の座標は無意味 — アンカー保持も破棄。
            *self.last_valid_anchor.borrow_mut() = None;
            // UIバグ4: context スタックが変われば focus の top context も変わる — sink を貼り替える。
            self.refresh_layout_sink_target();
            Ok(())
        }));
        match r {
            Ok(result) => result,
            Err(_) => {
                tip_log("ev=panic site=OnPushContext");
                Ok(())
            }
        }
    }
    fn OnPopContext(&self, _pic: Ref<'_, ITfContext>) -> Result<()> {
        self.consume_started_composition();
        // 巡4 T6: OnPushContext と同じ規律。
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.password_ctx_key.set(0); // Spec2: context 切替で password キャッシュ無効化（ABA 対策・I-3）
            let _ = self.abandon_prediction_for_context_change(
                crate::prediction_state::Invalidation::FocusChanged,
            );
            *self.prediction_failed_context.borrow_mut() = None;
            // UIバグ5: 同上（pop 後の context でも旧座標は無意味）。
            *self.last_valid_anchor.borrow_mut() = None;
            // UIバグ4: 同上（pop 後の top context へ追従）。
            self.refresh_layout_sink_target();
            Ok(())
        }));
        match r {
            Ok(result) => result,
            Err(_) => {
                tip_log("ev=panic site=OnPopContext");
                Ok(())
            }
        }
    }

    /// フォーカスが別ドキュメント（別ウィンドウ/アプリ）へ移ったとき、進行中の合成＋エンジン
    /// セッションを放棄する。別窓クリックでホストが live preedit を確定しても
    /// `OnCompositionTerminated` を呼ばないことがあり、その場合エンジンの読みが居残って次入力へ
    /// 連結される（フォーカス喪失データ残留。例: にほんご→別窓→aiueo で 日本語日本語あいうえお）。
    /// 自ドキュメントへ戻る/留まる・進行中状態が無い・部分確定中は何もしない。
    /// 巡3 P2: relayout の show() COM コールアウト中に同期再入しうる入口の一つ —
    /// reset_abandoned_composition が保持中 RefCell を再借用して panic すると shim 越えで
    /// abort するため、OnCompositionTerminated と同じ入口保護を通す。
    fn OnSetFocus(
        &self,
        pdimfocus: Ref<'_, ITfDocumentMgr>,
        _pdimprevfocus: Ref<'_, ITfDocumentMgr>,
    ) -> Result<()> {
        self.consume_started_composition();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.on_set_focus_inner(&pdimfocus)
        }));
        match r {
            Ok(result) => result,
            Err(_) => {
                tip_log("ev=panic site=OnSetFocus");
                Ok(())
            }
        }
    }

    // 巡3 P2: inherent 側へ置く（trait impl 内の非 trait メソッドは E0407）。
}

impl TextService_Impl {
    fn on_set_focus_inner(&self, pdimfocus: &Ref<'_, ITfDocumentMgr>) -> Result<()> {
        self.password_ctx_key.set(0); // Spec2: フォーカス切替で password キャッシュ無効化（ABA 対策・I-3）
        let new_focus: Option<ITfDocumentMgr> = pdimfocus.ok().ok().cloned();
        let new_focus_ctx = new_focus
            .as_ref()
            .and_then(|doc| unsafe { doc.GetTop() }.ok());
        // DocumentMgr ではなく top context で比較する。同じ document manager が
        // 複数の編集欄を再利用するホストでも、欄Aの文脈／failure を欄Bへ持ち越さない。
        let prediction_field = self
            .prediction_context
            .borrow()
            .clone()
            .or_else(|| self.layout_sink_ctx.borrow().clone());
        let prediction_focus_is_same = match (&new_focus_ctx, &prediction_field) {
            (Some(f), Some(o)) => com_identity_eq(f, o),
            _ => false,
        };
        if !prediction_focus_is_same
            && (self.prediction_state.borrow().has_private_state()
                || self.prediction_ghost_visible()
                || self.prediction_failed_context.borrow().is_some())
        {
            let _ = self.abandon_prediction_for_context_change(
                crate::prediction_state::Invalidation::FocusChanged,
            );
            // cleanup failure は旧欄にだけ属する。新欄の予測を巻き添えで停止しない。
            *self.prediction_failed_context.borrow_mut() = None;
        }
        // UIバグ5: キャレットアンカーの保持もフォーカス切替で破棄 — 別ドキュメントの座標は無意味。
        *self.last_valid_anchor.borrow_mut() = None;
        // UIバグ4: フォーカス context が変わったら ITfTextLayoutSink を貼り替える
        // （同一 context なら内部で no-op。スクロール追従は表示中のみ意味を持つ）。
        self.refresh_layout_sink_target();
        let has_active_input =
            self.engine_session.get() != 0 || self.composition.borrow().is_some();
        // 新フォーカス先（NULL=アプリがバックグラウンドへ）と、自分の合成があるドキュメントを
        // COM 同一性で比較する。current_context は borrow を即解放してから GetDocumentMgr を呼ぶ。
        let our_ctx: Option<ITfContext> = self.current_context.borrow().clone();
        let our_doc: Option<ITfDocumentMgr> =
            our_ctx.and_then(|ctx| unsafe { ctx.GetDocumentMgr() }.ok());
        let focus_is_our_doc = match (&new_focus, &our_doc) {
            (Some(f), Some(o)) => com_identity_eq(f, o),
            _ => false, // NULL フォーカス or 自 doc 不明 → 「自分でない」扱い
        };
        if crate::focus::should_abandon_on_focus_change(
            has_active_input,
            focus_is_our_doc,
            self.partial_committing.get(),
        ) {
            tip_log("ev=focus_abandon src=setfocus");
            self.reset_abandoned_composition();
        }
        // I-2: 確定取消は has_active_input に依らずフォーカス喪失で必ず窓を閉じる
        // （armed 残留による別文書での誤発火＝スチール解消。自 doc へ留まる場合も含め、
        // フォーカスが動いた以上は直前確定への Ctrl+Backspace を許さない）。
        if !focus_is_our_doc {
            // NULL/別 document focus is a key lifecycle boundary even when no composition is
            // active.  Clear a pending Test→Key pair before any later document can reuse its VK.
            self.invalidate_pending_end_test_reservation();
            self.disarm_undo();
            // ephemeral かな: 別窓へフォーカスが動いた＝押し忘れの言語モードを持ち越さない
            // （thread compartment を direct へ。ctx 無しでも冪等に呼べる）。
            self.exit_ephemeral_to_direct(None);
        }
        Ok(())
    }
}

impl ITfThreadFocusSink_Impl for TextService_Impl {
    fn OnSetThreadFocus(&self) -> Result<()> {
        Ok(())
    }

    /// 前面（スレッド）フォーカスを失った＝別アプリ/プロセスへ切替わった。クロスプロセスの
    /// フォーカス喪失はこの通知が主経路（`ITfThreadMgrEventSink::OnSetFocus` はスレッド内 doc
    /// フォーカス変化のみで、別プロセス前面化では届かないことがある）。進行中の合成があれば
    /// 放棄リセットを焚き、ホストが `OnCompositionTerminated` を呼ばずに preedit を確定/破棄しても
    /// エンジンの読みが居残らないようにする。スレッドが前面を失った時点で自ドキュメントは
    /// 非フォーカスなので、should_abandon の focus_is_our_doc=false 相当で判定する。
    /// 巡3 P2: 同期再入しうる入口 — OnSetFocus と同じ入口保護を通す（shim 越え abort 防止）。
    fn OnKillThreadFocus(&self) -> Result<()> {
        self.consume_started_composition();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.on_kill_thread_focus_inner()
        }));
        match r {
            Ok(result) => result,
            Err(_) => {
                tip_log("ev=panic site=OnKillThreadFocus");
                Ok(())
            }
        }
    }

    // 巡3 P2: inherent 側へ置く（trait impl 内の非 trait メソッドは E0407）。
}

impl TextService_Impl {
    fn on_kill_thread_focus_inner(&self) -> Result<()> {
        // 巡1検証G2: 前面（スレッド）フォーカス喪失でも旧 context のアンカー保持を破棄 —
        // クロスプロセス前面化では OnSetFocus が届かないことがあるため（password と同じ理屈）。
        // Cross-process focus loss invalidates the pair even when there is no active composition.
        self.invalidate_pending_end_test_reservation();
        *self.last_valid_anchor.borrow_mut() = None;
        let _ = self.abandon_prediction_for_context_change(
            crate::prediction_state::Invalidation::FocusChanged,
        );
        *self.prediction_failed_context.borrow_mut() = None;
        let has_active_input =
            self.engine_session.get() != 0 || self.composition.borrow().is_some();
        if crate::focus::should_abandon_on_focus_change(
            has_active_input,
            false, // 前面喪失＝自ドキュメントは非フォーカス
            self.partial_committing.get(),
        ) {
            tip_log("ev=focus_abandon src=killthreadfocus");
            self.reset_abandoned_composition();
        }
        // I-2: 前面（スレッド）フォーカス喪失も has_active_input に依らず窓を閉じる
        // （OnSetFocus の自 doc 以外分岐と対）。
        self.disarm_undo();
        // ephemeral かな: 前面フォーカスが別プロセスへ移った＝別窓へモードを漏らさない。
        self.exit_ephemeral_to_direct(None);
        Ok(())
    }
}

impl ITfTextLayoutSink_Impl for TextService_Impl {
    /// UIバグ4: ホストのスクロール・リフローで表示中の候補窓・読みモニタをキャレットへ
    /// 追従させる。TSF の再入規律上ここから同期 edit session を要求できないため
    /// （Mozc も非同期化）、非同期 READ セッション（RefreshAnchorOnLayout）で
    /// GetTextExt をやり直し、その中で再配置まで済ませる。
    fn OnLayoutChange(
        &self,
        _pic: Ref<'_, ITfContext>,
        _lcode: TfLayoutCode,
        _pview: Ref<'_, ITfContextView>,
    ) -> Result<()> {
        self.consume_started_composition();
        // 追従を要るのは表示中だけ — 非表示のたびにセッションを張らない。
        if !self.popups_visible() {
            return Ok(());
        }
        // スクロール中は OnLayoutChange が連発する。保留中フラグで 1 本にまとめる
        // （フラグは RefreshAnchorOnLayout::DoEditSession → layout_refresh_apply で解除）。
        if self.layout_refresh_pending.get() {
            return Ok(());
        }
        let Some(ctx) = self.layout_sink_ctx.borrow().clone() else {
            return Ok(());
        };
        let sess: ITfEditSession = crate::edit_session::RefreshAnchorOnLayout {
            context: ctx.clone(),
            composition: Rc::clone(&self.composition),
            // 巡2 E1/E3: 投入時点の世代を埋め込む — 遅延発火までに Activate/Deactivate/
            // context 貼替が起きていたら、layout_refresh_apply が旧座標の適用を断つ。
            gen: self.layout_sink_gen.get(),
            _guard: ComObjectGuard::new(),
        }
        .into();
        self.layout_refresh_pending.set(true);
        let ok = match unsafe {
            ctx.RequestEditSession(
                self.tid.get(),
                &sess,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_ASYNC.0 | TF_ES_READ.0),
            )
        } {
            // 外側 HRESULT とは別に [out] phrSession がセッション確立結果を運ぶ（windows-rs は
            // それを Ok(hr) で返す）。phrSession 失敗（TF_E_LOCKED 等）では DoEditSession が
            // 実行されず pending を下ろす経路が消えるため、内側 HRESULT も判定する（巡2 F3）。
            Ok(hr) => hr.is_ok(),
            Err(_) => false,
        };
        if !ok {
            self.layout_refresh_pending.set(false);
        }
        Ok(())
    }
}

/// 2 つの COM インターフェースが同一オブジェクトを指すか（IUnknown へ QI して raw ポインタ比較）。
fn com_identity_eq<A: Interface, B: Interface>(a: &A, b: &B) -> bool {
    match (a.cast::<IUnknown>(), b.cast::<IUnknown>()) {
        (Ok(x), Ok(y)) => x.as_raw() == y.as_raw(),
        _ => false,
    }
}

// SP6b: TSF の「設定/Options」ボタンは TIP の CLSID を IID_ITfFnConfigure で CoCreate し
// Show を呼ぶ。GUI を in-proc DLL に持ち込まず、別 exe（NospacekeyConfig.exe）を起動して
// 閉じるまで待つ（engine と同じ crash-isolation 思想）。ITfFnConfigure は ITfFunction を
// 継承するので両方を impl する必要がある。
impl ITfFunction_Impl for TextService_Impl {
    fn GetDisplayName(&self) -> Result<windows::core::BSTR> {
        Ok(windows::core::BSTR::from("nospacekey"))
    }
}

impl ITfFnConfigure_Impl for TextService_Impl {
    fn Show(
        &self,
        hwndparent: windows::Win32::Foundation::HWND,
        _langid: u16,
        _rguidprofile: *const windows::core::GUID,
    ) -> Result<()> {
        // GUI を in-proc DLL に持ち込まない: 別 exe を起動して閉じるまで待つ（D3 隔離, engine と同じ思想）。
        // 失敗は no-op 劣化（host を巻き込まない）。
        match config_exe_path() {
            Some(exe) => {
                let mut cmd = std::process::Command::new(exe);
                cmd.arg(format!("{}", hwndparent.0 as isize)); // 親HWND（config 側で owner 化に使える）
                match cmd.spawn() {
                    Ok(mut child) => {
                        let _ = child.wait();
                    } // 「閉じるまで返らない」契約
                    Err(_) => tip_log("ev=configure_spawn_failed"),
                }
            }
            None => tip_log("ev=configure_exe_not_found"),
        }
        Ok(())
    }
}

// ---- エンジン IPC / 編集セッション実行のヘルパ ----
// `OnKeyDown` のフローはここのメソッドを呼ぶだけにして、COM trait 実装側を薄く保つ。
impl TextService_Impl {
    /// logon session で安定なエンジン用パイプ名。
    /// 同一 logon session 内の全 TIP インスタンスが同じ名を返すので、単一の共有エンジンと接続できる。
    /// 初回に算出して `self.pipe_name` にキャッシュし、以後は同値を返す。
    fn engine_pipe_name(&self) -> String {
        {
            let n = self.pipe_name.borrow();
            if !n.is_empty() {
                return n.clone();
            }
        }
        let name = crate::engine_link::stable_pipe_name();
        *self.pipe_name.borrow_mut() = name.clone();
        name
    }

    /// A7: スリープ復帰の世代カウンタをキースレッドで刈り取る（コスト: atomic load 1回）。
    /// 復帰していたら backoff を全リセットし、idle なら接続を捨てて次打鍵で張り直す。
    pub(crate) fn poll_power_events(&self) {
        let gen = match self.power_notify.borrow().as_ref() {
            Some(h) => h.events().resume_gen(),
            None => return,
        };
        let busy = {
            let st = self.state.borrow(); // 1回の borrow で composing/awaiting_llm を読む
            st.composing || st.awaiting_llm()
        } || self.showing.get()
            || self.llm_slot.borrow().is_some();
        match resume_poll_action(gen, self.last_resume_gen.get(), busy) {
            None => (),
            Some(do_drop) => {
                self.last_resume_gen.set(gen);
                self.reconnect_backoff.borrow_mut().reset();
                self.resume_convert_pending.set(true);
                if do_drop {
                    self.drop_engine();
                    tip_log(&format!("ev=resume_reconnect mode=idle_drop gen={gen}"));
                } else {
                    tip_log(&format!(
                        "ev=resume_reconnect mode=composing_keep gen={gen}"
                    ));
                }
            }
        }
    }

    /// StartSession して client/session を確定保持する。失敗時は client を None のまま。
    fn start_and_store(&self, mut c: EngineClient) {
        match timed_request(
            &mut c,
            &Request::StartSession,
            IPC_TIMEOUT_FAST,
            "start_session",
        ) {
            Ok(Response::Session { session, proto }) => {
                // version handshake は接続確立時（fresh StartSession）にだけ効かせる。proto はエンジン
                // プロセスの属性で、一度確立した接続の途中では変わらないため、既存接続に StartSession を
                // 貼り直す ensure_session 側では判定しない（この start_and_store が全 fresh 接続経路の合流点）。
                match decide_handshake(proto, self.handshake_shutdown_attempted.get()) {
                    HandshakeAction::Accept => {
                        self.handshake_shutdown_attempted.set(false);
                        self.engine_session.set(session);
                        *self.client.borrow_mut() = Some(c);
                        // 巡4 T5: busy 再送の予算は接続単位 — 新しい接続で数え直す
                        // （Drop でしか戻さないと過去の busy で予算を使い切り、以後の接続で
                        // 即 giveup して設定反映が永久に再送されなくなる）。
                        self.reload_retry_count.set(0);
                        tip_log(&format!(
                            "ev=engine_proto ok=true proto={PROTO_VERSION} (session={session})"
                        ));
                        // UU-5: この接続で常駐エンジンへ現在の設定を push する。常駐エンジンは起動時
                        // env で LLM/Zenzai 設定を固定するため、接続確立ごとに settings.json の現在値を
                        // 送って「次回接続（≒次回 Activate）」に反映タイミングを統一する。
                        self.engine_reload_config();
                    }
                    HandshakeAction::ShutdownRespawn => {
                        // proto 不一致（更新後に旧エンジンが居座る等）。graceful に止めて世代交代する。
                        // Shutdown 応答（Ok/Error/タイムアウト）は問わず先へ進む: 旧エンジンは Shutdown を
                        // 知らず Error を返し自発終了しないが、その最終回収はインストーラの taskkill が担う。
                        // ここは接続を捨てて respawn を撒くだけ。旧エンジン残存時は spawn_engine_only 冒頭の
                        // connect(50ms) が成功して Some(0) を返し spawn 自体は起きない（二重化しない）。
                        // この打鍵は degrade、次打鍵の ensure_engine が新エンジンへ接続して自己修復する。
                        tip_log(&format!("ev=engine_proto ok=false got={proto:?} want={PROTO_VERSION} -> shutdown"));
                        let _ =
                            timed_request(&mut c, &Request::Shutdown, IPC_TIMEOUT_FAST, "shutdown");
                        drop(c);
                        self.drop_engine();
                        let pipe = self.engine_pipe_name();
                        let _ = spawn_engine_only(&pipe);
                        self.handshake_shutdown_attempted.set(true);
                    }
                    HandshakeAction::DegradeKeep => {
                        // 一度世代交代を試した後も不一致（旧 exe 残存）。接続は維持し現行 op 範囲で継続する。
                        tip_log(&format!("ev=engine_proto ok=false got={proto:?} action=keep (session={session})"));
                        self.engine_session.set(session);
                        *self.client.borrow_mut() = Some(c);
                        // 巡5-B 指摘7: ここも新しい接続 — busy 再送の予算は数え直す
                        // （Accept 枝のみのリセットでは過去の busy で予算を使い切った状態を
                        // 持ち越して以後の接続で即 giveup する）。
                        self.reload_retry_count.set(0);
                        self.engine_reload_config();
                    }
                }
            }
            other => {
                tip_log(&engine_failure_event("start_session", &other));
                *self.client.borrow_mut() = None;
            }
        }
    }

    /// UU-5: 現在の settings.json（LLM/Zenzai）を常駐エンジンへ push して即時反映させる。
    /// StartSession の直後に呼ばれる。プロトコルに request-id 相関が無いため要求→応答の交互性が
    /// 命で、応答を消費できたかどうかで分岐する（UU-1 と同型）:
    /// - `Ok(Ok)`: 正常反映。
    /// - `Ok(Error)`: ReloadConfig 未対応の旧エンジン等。応答は消費済み＝交互性は保たれるので
    ///   接続は維持する（設定反映の失敗で IME を止めない＝best-effort の本体）。
    /// - `Ok(その他)` / `Err(_)`: 予期しない応答型（desync 兆候）／タイムアウト・切断（応答未消費で
    ///   late frame が滞留し以降 1-off desync になる）。いずれも `drop_engine` で接続を破棄し、
    ///   次打鍵の ensure_engine で貼り直す（恒常 desync を防ぐ）。
    pub(crate) fn engine_reload_config(&self) {
        let s = settings::load();
        let key_plain = if s.llm.api_key_dpapi.is_empty() {
            None
        } else {
            settings::dpapi::decrypt(&s.llm.api_key_dpapi)
        };
        let req = build_reload_config(&s, key_plain.as_ref().map(|z| z.as_str()), |k| {
            std::env::var(k).ok()
        });
        let result = {
            let mut guard = self.client.borrow_mut();
            let Some(client) = guard.as_mut() else {
                return;
            };
            timed_request(client, &req, IPC_TIMEOUT_FAST, "reload_config")
        };
        match result {
            Ok(Response::Ok) => {
                tip_log("ev=reload_config ok=true");
                // 巡4 T5: 反映が成功したら予算を戻す（busy エピソード単位のカウントにする）。
                self.reload_retry_count.set(0);
            }
            Ok(Response::Error { message }) => {
                // 応答は消費済み（交互性 OK）。旧エンジン等なので接続は維持する。
                tip_log(if message.starts_with("reload busy") {
                    "ev=reload_config ok=false reason=busy"
                } else {
                    "ev=reload_config ok=false reason=engine_error"
                });
                // 巡3 Z4: busy（warm-up/変換中でスキップ）は一過性 — engine_reload_config は
                // 接続確立時にしか呼ばれず TIP は接続を維持するため、放置すると「次回接続」が
                // 原理的に来ず設定が旧値のまま残る。上限付きの遅延再送で warm-up 明けに反映させる
                // （常時 Error を返す旧エンジン相手の無限再送を防ぐため 2 回で打ち切る）。
                if message.starts_with("reload busy") {
                    self.schedule_reload_retry();
                }
            }
            Ok(other) => {
                // 予期しない応答型＝desync の兆候。安全側で接続を破棄する。
                tip_log(&engine_failure_event("reload_config", &Ok(other)));
                self.drop_engine();
            }
            Err(e) => {
                // タイムアウト/切断: 応答未消費で late frame 滞留 → 恒常 1-off desync を防ぐため破棄。
                tip_log(&engine_failure_event("reload_config", &Err(e)));
                self.drop_engine();
            }
        }
    }

    /// 巡3 Z4: busy 応答を受けた ReloadConfig の遅延再送（上限付き）。0ms スレッドタイマで
    /// メッセージループの次周へ逃がし、タイマID 照合で disarm 済み/新しい再送との衝突を切る。
    /// busy は warm-up の数秒窓で起こるため 1s 間隔・最大 2 回で実務的に十分。
    fn schedule_reload_retry(&self) {
        // 巡4 J5: warm-up は通常環境でも約2.1s・遅環境で 3〜5s+ — t+1s×2 では到達前に
        // 予算切れする。5 回（t+1..t+5s）まで許す。予算は接続確立/成功時にリセットされる。
        const RELOAD_RETRY_MAX: u32 = 5;
        self.reload_retry_count
            .set(self.reload_retry_count.get().saturating_add(1));
        if self.reload_retry_count.get() > RELOAD_RETRY_MAX {
            tip_log("ev=reload_config retry_giveup");
            return;
        }
        let n = self.reload_retry_count.get();
        tip_log(&format!("ev=reload_config retry={n}"));
        // 既存の再送タイマがあれば差し替え（多重武装しない）。
        let old = self.reload_retry_timer.replace(0);
        if old != 0 {
            unsafe {
                let _ = KillTimer(None, old);
            }
        }
        let ts = self as *const TextService_Impl;
        let id = unsafe { SetTimer(None, 0, 1000, Some(reload_retry_timer_proc)) };
        if id != 0 {
            self.reload_retry_timer.set(id);
            RELOAD_RETRY_TS.with(|p| p.set(ts));
        }
    }

    /// 再送タイマの発火口（STA メッセージループ上）。タイマID 照合後に残り回数を消費して再送。
    fn fire_reload_retry(&self, id: usize) {
        if self.reload_retry_timer.get() != id {
            return; // 旧タイマの発火（差し替え後の残り）— 掃除だけして無視。
        }
        self.reload_retry_timer.set(0);
        self.engine_reload_config();
    }

    /// cold start ②: IME 切替（Activate）の時点でエンジンを先行起動しておく。
    /// 接続はしない（初回打鍵の ensure_engine の 200ms 接続が即成功する状態を作るだけ）。
    /// spawn はプロセス起動のみで軽量（<10ms）なので Activate 同期内で完結し、
    /// バックグラウンドスレッド不要 = DLL_REF ガード（プリウォームワーカの教訓）も不要。
    /// `spawn_attempted` は立てない（prespawn は best-effort — 失敗しても初回打鍵の
    /// ensure_engine フルコース（spawn 込み）を妨げない）。二重 spawn は SpawnGuard
    /// （プロセス跨ぎ直列化）＋ spawn_engine_only 内の再確認 connect（既に listening なら
    /// spawn しない）で防ぎ、それでも透き間（prespawn 直後〜listening 前の打鍵で
    /// ensure_engine が 2 個目を spawn）を抜けた場合は engine 側の singleton mutex ガード
    /// （runEngineHost — I-1）が後着プロセスを即終了させる＝恒久二重化しない。
    pub(crate) fn prespawn_engine(&self) {
        // M-4: 直接入力（半角英数）モード中は変換が起きないので起こさない（default_direct ユーザが
        // IME を往復するたびに常駐 engine を立てない）。日本語モードへ切り替えて打鍵すれば
        // 従来どおり ensure_engine が起こす（prespawn の恩恵が無いだけで劣化はしない）。
        if self.is_direct_mode() {
            return;
        }
        // M-3: このインスタンスで prespawn の spawn が一度失敗したら以降の Activate では試みない
        // （ハードン host は spawn が恒常失敗 — Activate 毎の SpawnGuard＋50ms 接続＋DPAPI 復号の
        // 固定費を払い続けない）。ensure_engine 側の自己修復経路はこのガードの影響を受けない。
        if self.prespawn_failed.get() {
            return;
        }
        if should_prespawn(
            self.client.borrow().is_some(),
            self.spawn_attempted.get(),
            self.reconnect_backoff
                .borrow()
                .full_attempt_allowed(std::time::Instant::now()),
        ) {
            // pid=0 は「既に listening（spawn 不要）」、pid>0 は実 spawn（spawn_engine_only 参照）。
            match spawn_engine_only(&self.engine_pipe_name()) {
                Some(pid) => tip_log(&format!("ev=prespawn at=activate ok=true pid={pid}")),
                None => {
                    self.prespawn_failed.set(true);
                    tip_log("ev=prespawn at=activate ok=false pid=0");
                }
            }
        }
    }

    /// エンジンへ接続し、無ければ永続シングルトンとして detached 起動してから短時間接続を試みる。
    /// 「最初の打鍵時」に遅延呼び出しされる。client があれば即 return（連打で無駄打ちしない）。
    /// 起動はこの活性化につき最大1回（spawn_attempted）。全失敗は握り潰す（劣化動作）。
    /// キースレッドを長時間ブロックしない（200ms+50ms+400ms の短時間のみ）。
    pub(crate) fn ensure_engine(&self) {
        if self.client.borrow().is_some() {
            return;
        }
        let pipe = self.engine_pipe_name(); // stable per-session name (Task 1)
        let now = std::time::Instant::now();

        // A7: クールダウン中はフルコース（spawn+200/50/400ms 接続）を止め、無償の一発プローブだけ許す。
        // 半死（session 確立失敗）検出後はプローブも満了まで停止する（probe_suppressed）。
        // borrow はブロックで閉じてから start_and_store/borrow を呼ぶ（二重借用 panic 回避）。
        if !self.reconnect_backoff.borrow().full_attempt_allowed(now) {
            if !self.reconnect_backoff.borrow().probe_allowed() {
                return;
            }
            if let Ok(c) = EngineClient::connect_to(&pipe, Duration::ZERO) {
                self.start_and_store(c);
                if self.client.borrow().is_some() {
                    self.reconnect_backoff.borrow_mut().reset();
                    tip_log("ev=engine_reconnect via=probe");
                } else {
                    // connect 成功＋セッション確立失敗＝半死。以降クールダウン満了までプローブも停止。
                    // 遅延の起算は「失敗を記録した今」— StartSession の 250ms を跨いだ後なので取り直す（I-1）。
                    let mut b = self.reconnect_backoff.borrow_mut();
                    b.on_session_failure(std::time::Instant::now());
                    tip_log(&format!(
                        "ev=engine_backoff kind=session n={}",
                        b.failures()
                    ));
                }
            }
            return; // connect 失敗のプローブは無償（カウントしない）
        }

        // ── フルコース（クールダウンを抜けたときだけ実行）──
        let mut connected_once = false;

        // 1) 既存サーバへ短時間で接続（誰かが起こしていれば即利用）。
        if let Ok(c) = EngineClient::connect_to(&pipe, Duration::from_millis(200)) {
            tip_log(&format!("connected to {pipe}"));
            connected_once = true;
            self.start_and_store(c);
            if self.client.borrow().is_some() {
                self.reconnect_backoff.borrow_mut().reset();
                return;
            }
        }

        match crate::engine_link::decide(false, self.spawn_attempted.get()) {
            crate::engine_link::EngineAction::DegradeNoSpawn => {
                // 起動不可(ハードン host で spawn 失敗済み) or 既に試行済み → degrade。
                // 別の非ハードン host が singleton を起こせば、次打鍵の 1) で接続でき自己修復。
                // ここも末尾判定に落ちてバックオフに記録される（ハードン host の 200ms 連打抑止＝G1）。
            }
            _ => {
                self.spawn_attempted.set(true);
                // singleton 起動をプロセス跨ぎで直列化（spawn+接続待ちの間 guard を保持）。
                let _guard = crate::engine_link::SpawnGuard::acquire(&pipe);
                // guard 取得待ちの間に他ホストが起こした可能性 → 再接続を試す。
                if let Ok(c) = EngineClient::connect_to(&pipe, Duration::from_millis(50)) {
                    connected_once = true;
                    self.start_and_store(c);
                    if self.client.borrow().is_some() {
                        self.reconnect_backoff.borrow_mut().reset();
                        return;
                    }
                }
                match engine_exe_path() {
                    Some(exe) => {
                        tip_log(&format!(
                            "ev=engine_exe path={} exists={}",
                            exe.display(),
                            exe.exists()
                        ));
                        let s = settings::load();
                        let key_plain = if s.llm.api_key_dpapi.is_empty() {
                            None
                        } else {
                            settings::dpapi::decrypt(&s.llm.api_key_dpapi)
                        };
                        let env_map = settings::resolve_env_map(
                            &s,
                            key_plain.as_ref().map(|z| z.as_str()),
                            |k| std::env::var(k).ok(),
                        );
                        match spawn_engine_hidden(&exe, &pipe, &env_map) {
                            Some(child) => {
                                tip_log(&format!(
                                    "ev=engine_spawn pid={} ok=true env_keys={}",
                                    child.id(),
                                    env_map.len()
                                ));
                                *self.engine_child.borrow_mut() = Some(child);
                                // 起動直後は listening まで間があるので短く一度だけ。ダメでも degrade（次打鍵の 1) で拾う）。
                                // 成功しても早期 return せず fall-through で末尾判定に達する（M-4）。
                                // cold start ①: spawn→connect 成功までの所要（engine 側 stage=listening と突き合わせる）。
                                let started = std::time::Instant::now();
                                if let Ok(c) =
                                    EngineClient::connect_to(&pipe, Duration::from_millis(400))
                                {
                                    tip_log(&format!(
                                        "ev=coldstart stage=spawn_to_connect ms={}",
                                        started.elapsed().as_millis()
                                    ));
                                    tip_log("connected after spawn");
                                    connected_once = true;
                                    self.start_and_store(c);
                                } else {
                                    tip_log(
                                        "spawn ok, not yet listening -> degrade this keystroke",
                                    );
                                }
                            }
                            None => {
                                tip_log("ev=engine_spawn pid=0 ok=false");
                                tip_log("ev=degraded reason=spawn_failed");
                            }
                        }
                    }
                    None => {
                        tip_log("engine exe path not found");
                        tip_log("ev=degraded reason=spawn_failed");
                    }
                }
            }
        }

        // 末尾判定: フルコースを抜けた時点で client が有れば成功（reset）、無ければ失敗を記録。
        // 遅延の起算は「失敗が確定した今」— 冒頭の stale な now を使うとフルコース所要（最悪 ~1.15s）
        // 分だけクールダウンが食われ、n=1 の 1s が実質ゼロになる（I-1）。
        if self.client.borrow().is_some() {
            self.reconnect_backoff.borrow_mut().reset();
        } else {
            let end = std::time::Instant::now();
            let mut b = self.reconnect_backoff.borrow_mut();
            if connected_once {
                b.on_session_failure(end);
            } else {
                b.on_connect_failure(end);
            }
            tip_log(&format!(
                "ev=engine_backoff kind={} n={}",
                if connected_once { "session" } else { "connect" },
                b.failures()
            ));
        }
    }

    /// エンジン接続が壊れたとみなして破棄する。client/session を捨て、起動フラグも戻すので、
    /// 次の打鍵の `ensure_engine` で再接続（必要なら再起動）して復帰できる。
    /// 注意: 呼び出し側は `self.client` の borrow を一切持っていないこと（二重借用 panic 防止）。
    /// 巡4 T3: key_event_sink からも呼ぶ（エンジン消費済み確定の挿入拒否時の自己修復）ため pub(crate)。
    pub(crate) fn drop_engine(&self) {
        *self.client.borrow_mut() = None;
        self.engine_session.set(0);
        // 巡4 T5: busy 再送タイマも接続と運命を共にする — 旧接続向けの再送が残っていると
        // 新接続確立後に重複 ReloadConfig を送る。予算は次の start_and_store で数え直す。
        let rt = self.reload_retry_timer.replace(0);
        if rt != 0 {
            unsafe {
                let _ = KillTimer(None, rt);
            }
        }
        // 接続を捨てる＝パイプの切断。契約: サーバは接続断を検知すると、その接続が所有する
        // セッションを掃除する（--persist 常駐サーバでは接続単位のセッション所有マッピングを持ち、
        // 切断時に endSession 相当＋必要なら stopComposition を実行する。Swift サーバ側で並行対応中）。
        //   注: 旧コメントの「接続を捨てる＝engine プロセスごと終了しセッションも消える」は
        //   --persist 常駐サーバの導入で false になった（プロセスは生き続ける）。掃除の責務は
        //   プロセス終了ではなくサーバの接続断ハンドラに移った。
        // 保留 EndSession は無効化する（復帰時は新接続なので古い id を送ってはいけない）。
        self.pending_end_session.set(0);
        // A': owe していた応答も接続ごと消える。新接続には持ち越さない。
        self.pending_since.set(None);
        self.spawn_attempted.set(false);
        // L-5: Child ハンドルを閉じる（kill しない＝従来どおりエンジンは pipe 切断で自走終了）。
        *self.engine_child.borrow_mut() = None;
    }

    /// 新しい composition を始める前に、有効なエンジンセッションを確保する。
    /// commit/cancel 後は `engine_session == 0` になっているので、ここで張り直す。
    /// client が無い（劣化動作中）なら何もしない。
    /// StartSession が Session 以外を返した（タイムアウト/切断/予期しない応答）ときは、
    /// 他の全 IPC 経路と同じく接続ごと破棄する（plan_start_session のドキュメント参照。
    /// 破棄しないと遅延 Session フレームの滞留で恒常 1-off desync になる — UU-1）。
    ///
    /// 戻り値: **今この呼び出しでセッションを新規作成したか**。true のとき engine 側の
    /// ComposingText は空なので、composition 継続中の呼び出し元（input_char）は打鍵1文字では
    /// なく `state.raw` 全体を送り直すこと（ライブ変換タイムアウト等の drop_engine 後に
    /// 積み上げた読みが消える 22→23 文字目データロスの再発防止）。
    pub(crate) fn ensure_session(&self) -> bool {
        if self.engine_session.get() != 0 {
            return false;
        }
        // borrow は result ブロック内で完結させ、drop 後に drop_engine を呼ぶ
        // （二重借用 panic 防止。engine_insert と同じ規律）。
        let result = {
            let mut guard = self.client.borrow_mut();
            guard.as_mut().map(|client| {
                timed_request(
                    client,
                    &Request::StartSession,
                    IPC_TIMEOUT_FAST,
                    "start_session",
                )
            })
        };
        match result.map(plan_start_session) {
            Some(Some(session)) => {
                self.engine_session.set(session);
                true
            }
            Some(None) => {
                tip_log("ev=degraded reason=start_session_failed");
                self.drop_engine();
                false
            }
            None => false, // client 無し（劣化動作中）: 従来どおり何もしない
        }
    }

    /// A' 送信前ドレインの結果。呼び出し側（engine_live_convert/engine_insert）が次の動作を決める。
    /// INV1: pending 中はいかなる要求も送信前にこれで 1 フレーム読み切ってから送る。
    fn prepare_send(&self, op: &str, tier: Duration) -> DrainOutcome {
        // owe していなければそのまま送ってよい。
        let since = match self.pending_since.get() {
            Some(t) => t,
            None => return DrainOutcome::Proceed,
        };
        // INV5: pending 開始から PENDING_MAX 超過 → engine 真死とみなし drop（永久劣化ガード）。
        if since.elapsed() >= PENDING_MAX {
            tip_log(&format!("ev=degraded reason=pending_stuck op={op}"));
            self.drop_engine();
            return DrainOutcome::Dropped;
        }
        // borrow は結果ブロック内で完結させ、drop 後に drop_engine を呼ぶ（二重借用 panic 防止）。
        let drained = {
            let mut guard = self.client.borrow_mut();
            match guard.as_mut() {
                Some(client) => client.drain_pending(std::time::Instant::now() + tier),
                // client 不在（既に劣化）なら pending も無意味。以降は素通し。
                None => {
                    self.pending_since.set(None);
                    return DrainOutcome::Proceed;
                }
            }
        };
        match drained {
            // INV4: 予算内にフレームが来ない。pending 維持で「要求を送らず劣化続行」。
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                tip_log(&format!("ev=degraded reason=drain_timeout op={op}"));
                DrainOutcome::StillPending
            }
            // INV4: パイプ破断 → 即 drop。
            Err(_) => {
                self.drop_engine();
                DrainOutcome::Dropped
            }
            Ok(Some(resp)) => {
                self.pending_since.set(None);
                if drained_needs_drop(&resp) {
                    // INV2: engine 側は部分確定適用済み・TIP 側未適用の不整合。安全側で drop
                    //       （既存の reseed 経路に合流する）。
                    tip_log(&format!("ev=ipc_drained op={op} needs_drop=1"));
                    self.drop_engine();
                    DrainOutcome::Dropped
                } else {
                    tip_log(&format!("ev=ipc_drained op={op}"));
                    DrainOutcome::Proceed
                }
            }
            // pending_since が立っているのに client.pending が無い＝整合が取れないが、素通しで復帰。
            Ok(None) => {
                self.pending_since.set(None);
                DrainOutcome::Proceed
            }
        }
    }

    /// 通常の同期要求用の送信前ゲート。未読応答を期限内に回収できない接続をそのまま
    /// 保持すると、要求を送れなかった打鍵だけ engine 側の composing から欠落する。
    /// そこで user action を伴う通常 op は drain 不成立時に接続ごと捨て、次打鍵の
    /// `needs_session_reseed` で TIP 側 `raw` 全量を新セッションへ送り直す。
    ///
    /// LiveConvert はタイマ起点で本文状態を進めないため、この helper を使わず従来どおり
    /// pending を保持する。EndSession も composition 終端固有の扱いがあるため専用分岐を保つ。
    fn prepare_send_or_drop(&self, op: &str, tier: Duration) -> bool {
        match self.prepare_send(op, tier) {
            DrainOutcome::Proceed => true,
            DrainOutcome::StillPending => {
                tip_log(&format!("ev=degraded reason=pending_before_{op}"));
                self.drop_engine();
                false
            }
            DrainOutcome::Dropped => false,
        }
    }

    /// `text` を挿入して読みを得る。client/session が無い・失敗なら None（劣化）。
    /// 通常の打鍵は 1 文字だが、drop_engine 後の再接続でセッションを張り直した直後は
    /// `state.raw` 全体を 1 回で送り直す（ensure_session のドキュメント参照）。
    /// エンジン側 insert は文字列単位（roman2kana は逐次挿入とバッチ挿入で等価、かなは素通し）。
    /// 失敗時は接続を破棄して次打鍵で復帰できるようにする。
    /// borrow は `result` ブロック内で完結させ、drop 後に `drop_engine` を呼ぶ（二重借用 panic 防止）。
    pub(crate) fn engine_insert(&self, text: &str, style: InsertStyle) -> Option<String> {
        // INV1: pending 中は送信前にドレイン。解消できなければ接続を捨て、今回 TIP 側へ
        // 積んだ文字を次打鍵の raw 全量 reseed で必ず engine へ戻す。
        if !self.prepare_send_or_drop("insert", IPC_TIMEOUT_FAST) {
            return None;
        }
        let session = self.engine_session.get();
        let result = {
            let mut guard = self.client.borrow_mut();
            let client = guard.as_mut()?;
            timed_request_keep(
                client,
                &Request::Insert {
                    session,
                    text: text.to_string(),
                    // ワイヤ既定(roman2kana)は None で省略 — 旧エンジンに繋いでも壊れない。
                    style: match style {
                        InsertStyle::Direct => Some("direct".to_string()),
                        InsertStyle::Kana => None,
                    },
                },
                IPC_TIMEOUT_FAST,
                "insert",
            )
        };
        match result {
            Ok(Response::Reading { reading }) => Some(reading),
            // INV3: Insert のタイムアウトは drop_engine しない。pending をマークし接続・セッションを保つ。
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                if self.pending_since.get().is_none() {
                    self.pending_since.set(Some(std::time::Instant::now()));
                }
                tip_log("ev=degraded reason=insert_pending");
                None
            }
            other => {
                tip_log(&engine_failure_event("insert", &other));
                tip_log("ev=degraded reason=insert_failed");
                self.drop_engine();
                None
            }
        }
    }

    /// 変換候補を要求する。失敗なら None（劣化）し接続を破棄する。
    pub(crate) fn engine_convert(&self) -> Option<Vec<String>> {
        if !self.prepare_send_or_drop("convert", IPC_TIMEOUT_CONVERT) {
            return None;
        }
        let session = self.engine_session.get();
        let left_context = self.left_context.borrow().clone();
        let result = {
            let mut guard = self.client.borrow_mut();
            let client = guard.as_mut()?;
            // A7: 復帰後最初の変換系 op を計測する（client 不在の早期 return より後＝実際にエンジンへ
            // 触れた op でだけ消費する。plan レビュー M-3）。U9: left_context を Convert に載せる。
            let resume_probe = self.resume_convert_pending.replace(false);
            let started = std::time::Instant::now();
            let r = timed_request(
                client,
                &Request::Convert {
                    session,
                    left_context,
                },
                IPC_TIMEOUT_CONVERT,
                "convert",
            );
            if resume_probe {
                tip_log(&format!(
                    "ev=resume_first_convert op=convert ms={} ok={}",
                    started.elapsed().as_millis(),
                    r.is_ok()
                ));
            }
            r
        };
        match result {
            Ok(Response::Candidates { candidates }) => Some(candidates),
            other => {
                tip_log(&engine_failure_event("convert", &other));
                tip_log("ev=degraded reason=convert_failed");
                self.drop_engine();
                None
            }
        }
    }

    /// 修正変換候補を要求する（Tab）。失敗なら None（劣化）し接続を破棄する。手動キー起動なので
    /// A7 の resume_probe 計測（復帰後最初の変換系 op）は対象外（engine_convert と異なり計測しない）。
    pub(crate) fn engine_typo_convert(&self) -> Option<Vec<String>> {
        if !self.prepare_send_or_drop("typo_convert", IPC_TIMEOUT_CONVERT) {
            return None;
        }
        let session = self.engine_session.get();
        let left_context = self.left_context.borrow().clone();
        let result = {
            let mut guard = self.client.borrow_mut();
            let client = guard.as_mut()?;
            timed_request(
                client,
                &Request::TypoConvert {
                    session,
                    left_context,
                },
                IPC_TIMEOUT_CONVERT,
                "typo_convert",
            )
        };
        match result {
            Ok(Response::Candidates { candidates }) => Some(candidates),
            other => {
                tip_log(&engine_failure_event("typo_convert", &other));
                tip_log("ev=degraded reason=typo_convert_failed");
                self.drop_engine();
                None
            }
        }
    }

    /// 選択かな表層を1往復で再変換し候補を得る（SP5 step-6）。失敗なら None（劣化）し接続を破棄する。
    pub(crate) fn engine_reconvert_surface(&self, surface: &str) -> Option<Vec<String>> {
        if !self.prepare_send_or_drop("reconvert", IPC_TIMEOUT_CONVERT) {
            return None;
        }
        let session = self.engine_session.get();
        let left_context = self.left_context.borrow().clone();
        let result = {
            let mut guard = self.client.borrow_mut();
            let client = guard.as_mut()?;
            // A7: 復帰後最初の変換系 op を計測する（M-3: client 不在の早期 return より後）。
            let resume_probe = self.resume_convert_pending.replace(false);
            let started = std::time::Instant::now();
            let r = timed_request(
                client,
                &Request::Reconvert {
                    session,
                    surface: surface.to_string(),
                    left_context,
                },
                IPC_TIMEOUT_CONVERT,
                "reconvert",
            );
            if resume_probe {
                tip_log(&format!(
                    "ev=resume_first_convert op=reconvert ms={} ok={}",
                    started.elapsed().as_millis(),
                    r.is_ok()
                ));
            }
            r
        };
        match result {
            Ok(Response::Candidates { candidates }) => Some(candidates),
            other => {
                tip_log(&engine_failure_event("reconvert", &other));
                tip_log("ev=degraded reason=reconvert_surface_failed");
                self.drop_engine();
                None
            }
        }
    }

    /// 選択候補(index)をエンジンにネイティブ部分確定させ `(確定text, 残り読み)` を得る。
    /// エンジンは選択候補の消費読みだけ確定し、残り読みを保持したセッションを継続する（破棄しない）。
    /// 失敗（未知セッション/キャッシュ無し/index 範囲外/接続断）は None＝劣化し、呼び出し側で従来確定へ。
    /// borrow は `result` ブロック内で完結させ、drop 後に degrade する（二重借用 panic 防止）。
    pub(crate) fn engine_commit(&self, index: usize) -> Option<(String, String)> {
        if !self.prepare_send_or_drop("commit", IPC_TIMEOUT_FAST) {
            return None;
        }
        let session = self.engine_session.get();
        let result = {
            let mut guard = self.client.borrow_mut();
            let client = guard.as_mut()?;
            timed_request(
                client,
                &Request::Commit {
                    session,
                    index: index as u32,
                },
                IPC_TIMEOUT_FAST,
                "commit",
            )
        };
        match result {
            Ok(Response::Committed { text, reading }) => Some((text, reading)),
            // エンジンが確定を拒否（未知セッション/キャッシュ無し/index 範囲外/stale）= 想定内の劣化。
            // convert/insert と違い commit の拒否は接続不良ではないので drop_engine しない
            // （部分確定で保持中の生きたセッションを巻き添えで壊さない）。None を返し全確定へフォールバック。
            Ok(Response::Error { .. }) => {
                tip_log("ev=engine_declined op=commit");
                None
            }
            other => {
                tip_log(&engine_failure_event("commit", &other));
                tip_log("ev=degraded reason=commit_failed");
                self.drop_engine();
                None
            }
        }
    }

    /// 文節ナビゲーション: 選択文節を `offset` だけ動かす（未開始ならエンジンが `base_index` を
    /// 種に開始する）。Error 応答は decline（旧エンジンの unknown method / キャッシュ無し /
    /// stale / 被覆候補無し）= 接続不良ではないので drop しない — 呼び出し側が従来の
    /// 「確定して畳む」へ劣化する（engine_commit の decline と同じ規律）。
    pub(crate) fn engine_move_clause(
        &self,
        offset: i32,
        base_index: usize,
    ) -> Option<ClauseViewData> {
        if !self.prepare_send_or_drop("move_clause", IPC_TIMEOUT_CONVERT) {
            return None;
        }
        let session = self.engine_session.get();
        let left_context = self.left_context.borrow().clone();
        let result = {
            let mut guard = self.client.borrow_mut();
            let client = guard.as_mut()?;
            timed_request(
                client,
                &Request::MoveClause {
                    session,
                    offset,
                    base_index: base_index as u32,
                    left_context,
                },
                IPC_TIMEOUT_CONVERT, // 選択文節の候補生成 = 変換 1 回ぶん
                "move_clause",
            )
        };
        match result {
            Ok(Response::ClauseView {
                segments,
                selected,
                candidates,
                candidate_index,
            }) => Some(ClauseViewData {
                segments,
                selected: selected as usize,
                candidates,
                candidate_index: candidate_index as usize,
            }),
            Ok(Response::Error { .. }) => {
                tip_log("ev=engine_declined op=move_clause");
                None
            }
            other => {
                tip_log(&engine_failure_event("move_clause", &other));
                tip_log("ev=degraded reason=move_clause_failed");
                self.drop_engine();
                None
            }
        }
    }

    /// 文節ナビゲーション: 選択文節の候補を `index` へ差し替える。decline 規約は
    /// engine_move_clause と同じ（Error は drop しない）。
    pub(crate) fn engine_select_clause_candidate(&self, index: usize) -> Option<ClauseViewData> {
        if !self.prepare_send_or_drop("select_clause_candidate", IPC_TIMEOUT_FAST) {
            return None;
        }
        let session = self.engine_session.get();
        let result = {
            let mut guard = self.client.borrow_mut();
            let client = guard.as_mut()?;
            timed_request(
                client,
                &Request::SelectClauseCandidate {
                    session,
                    index: index as u32,
                },
                IPC_TIMEOUT_FAST, // 変換なし（状態差し替えのみ）
                "select_clause_candidate",
            )
        };
        match result {
            Ok(Response::ClauseView {
                segments,
                selected,
                candidates,
                candidate_index,
            }) => Some(ClauseViewData {
                segments,
                selected: selected as usize,
                candidates,
                candidate_index: candidate_index as usize,
            }),
            Ok(Response::Error { .. }) => {
                tip_log("ev=engine_declined op=select_clause_candidate");
                None
            }
            other => {
                tip_log(&engine_failure_event("select_clause_candidate", &other));
                tip_log("ev=degraded reason=select_clause_failed");
                self.drop_engine();
                None
            }
        }
    }

    /// 文節ナビゲーション: 全文節を確定する（文節ごとの学習はエンジン側）。decline 規約は
    /// engine_commit と同じ — None なら呼び出し側が表示中ビューの連結を直確定する（学習に
    /// 乗らないだけで確定は必ず成功する）。
    pub(crate) fn engine_commit_clauses(&self) -> Option<(String, String)> {
        if !self.prepare_send_or_drop("commit_clauses", IPC_TIMEOUT_CONVERT) {
            return None;
        }
        let session = self.engine_session.get();
        let result = {
            let mut guard = self.client.borrow_mut();
            let client = guard.as_mut()?;
            timed_request(
                client,
                &Request::CommitClauses { session },
                // FAST にしないのは、エンジン側 commitClauses が bindConverter(セッション切替時は
                // zenz reset_context のスパイク)+文節数ぶんの setCompletedData/updateLearningData
                // +flush を converterLock 下で行い、作業量が Commit(1候補分)の N 倍だから。
                // タイムアウトはエンジンが確定+学習を完了した後に接続だけ破棄する「見かけ劣化」
                // (次打鍵が再接続フルコース)になるため、太い側に倒す。
                IPC_TIMEOUT_CONVERT,
                "commit_clauses",
            )
        };
        match result {
            Ok(Response::Committed { text, reading }) => Some((text, reading)),
            Ok(Response::Error { .. }) => {
                tip_log("ev=engine_declined op=commit_clauses");
                None
            }
            other => {
                tip_log(&engine_failure_event("commit_clauses", &other));
                tip_log("ev=degraded reason=commit_clauses_failed");
                self.drop_engine();
                None
            }
        }
    }

    /// `engine_live_convert` を呼んでよいか（設定 + 表記固定）。ライブ変換を参照する 3 経路
    /// （VK_RETURN / `settle_active_input` / `restore_live_preedit`）が共有する唯一の入口。
    pub(crate) fn should_consult_live_engine(&self) -> bool {
        crate::input_state::should_consult_live_engine(
            self.live_enabled.get(),
            self.state.borrow().notation_fixed,
        )
    }

    /// ライブ変換を要求し (text, reading, committed) を得る。失敗なら None（劣化）し接続を破棄する。
    /// seq は要求に載せてエコーさせる（A1 では 1:1 のため鮮度判定は不要。A2 で is_fresh_live を使う）。
    /// `auto_commit` はエンジン側の自動確定（iOS nospacekey の先頭文節自動確定）を許可するか。
    /// true を送ってよいのは応答の `committed` を composition へ適用できる経路
    /// （on_debounce_convert → apply_live_auto_commit）だけ。Enter 系の確定経路は false
    /// （直後の Commit{0} が残り読みしか確定できなくなるため — protocol.rs 参照）。
    pub(crate) fn engine_live_convert(
        &self,
        seq: u64,
        auto_commit: bool,
    ) -> Option<(String, String, Option<String>)> {
        // INV1: pending 中は送信前にドレイン。解消できなければ要求は送らず劣化継続。
        match self.prepare_send("live_convert", IPC_TIMEOUT_LIVE) {
            DrainOutcome::Proceed => {}
            DrainOutcome::StillPending | DrainOutcome::Dropped => return None,
        }
        let session = self.engine_session.get();
        let left_context = self.left_context.borrow().clone();
        let result = {
            let mut guard = self.client.borrow_mut();
            let client = guard.as_mut()?;
            // A7: 復帰後最初の変換系 op を計測する（M-3: client 不在の早期 return より後）。
            // U9: left_context を LiveConvert に載せる。
            let resume_probe = self.resume_convert_pending.replace(false);
            let started = std::time::Instant::now();
            let r = timed_request_keep(
                client,
                &Request::LiveConvert {
                    session,
                    seq,
                    left_context,
                    auto_commit,
                },
                IPC_TIMEOUT_LIVE,
                "live_convert",
            );
            if resume_probe {
                tip_log(&format!(
                    "ev=resume_first_convert op=live_convert ms={} ok={}",
                    started.elapsed().as_millis(),
                    r.is_ok()
                ));
            }
            r
        };
        match result {
            Ok(Response::LiveResult {
                seq: _resp_seq,
                text,
                reading,
                committed,
            }) => Some((text, reading, committed)),
            // INV3: LiveConvert のタイムアウトは drop_engine しない。pending をマークし接続・
            //       セッションを保つ（自動確定の安定履歴＝セッション単位を守る＝死のループを断つ）。
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                if self.pending_since.get().is_none() {
                    self.pending_since.set(Some(std::time::Instant::now()));
                }
                tip_log("ev=degraded reason=live_convert_pending");
                None
            }
            other => {
                tip_log(&engine_failure_event("live_convert", &other));
                tip_log("ev=degraded reason=live_convert_failed");
                self.drop_engine();
                None
            }
        }
    }

    /// デバウンスタイマを（再）武装する。既存タイマは解除してから張り直す。
    /// SP6b: ライブ変換 off（設定）なら武装しない＝読み preedit のまま据え置き、
    /// Space/Enter で SP1 候補フローに任せる（既存タイマがあれば畳むだけ）。
    pub(crate) fn arm_debounce(&self) {
        self.disarm_debounce();
        if !self.live_enabled.get() && !self.partial_preedit_redraw_pending.get() {
            return;
        }
        DEBOUNCE_TS.with(|p| p.set(self as *const TextService_Impl));
        let id = unsafe { SetTimer(None, 0, DEBOUNCE_MS, Some(debounce_timer_proc)) };
        // 巡3 P9: 失敗（タイマ資源枯渇。稀）を黙らない — debounce が発火せずライブ変換が
        // 次打鍵まで止まる。フォールバック（即時変換）は preedit 確定前のタイミングで
        // 副作用が大きいのでログで診断可能にする（arm_llm_poll と非対称なまま黙すより良い）。
        if id == 0 {
            tip_log("ev=debounce_arm_failed");
        }
        self.debounce_timer.set(id);
    }

    /// 部分確定後の preedit 再描画だけを上限付きで再武装する。通常の live debounce と異なり、
    /// edit-session が永続拒否されても無限タイマにならない。新しい打鍵/callback は回数を
    /// リセットして別の回復機会を作る。
    pub(crate) fn arm_partial_preedit_redraw_retry(&self) {
        if !self.partial_preedit_redraw_pending.get() {
            self.partial_preedit_redraw_retries.set(0);
            return;
        }
        let Some(next) = next_partial_redraw_retry(self.partial_preedit_redraw_retries.get())
        else {
            tip_log("ev=partial_preedit_redraw retry=exhausted");
            return;
        };
        self.partial_preedit_redraw_retries.set(next);
        self.arm_debounce();
    }

    /// デバウンスタイマを解除する（非武装に戻す）。
    pub(crate) fn disarm_debounce(&self) {
        let id = self.debounce_timer.replace(0);
        if id != 0 {
            unsafe {
                let _ = KillTimer(None, id);
            }
        }
    }

    /// 確定取消（Ctrl+Backspace）: undo_armed を非武装化する。F-5 改定の落とし所——
    /// undo も feedback もできない状態（feedback_enabled=false かつ非武装化した時点）で
    /// メモリに確定文字列を残さない。feedback opt-in 中は last_commit を消費用に残す
    /// （record_feedback が take() する）。現状の呼び出しは settle_active_input 末尾・
    /// on_preserved_key_impl のトグル/再変換/feedback 処理・OnSetFocus（自doc以外）・
    /// OnKillThreadFocus（key_event_sink.rs / 本ファイル）。次キー押下での disarm
    /// （is_pure_modifier_vk 判定）は OnKeyDown 分岐実装（後続 Task）で追加する。
    pub(crate) fn disarm_undo(&self) {
        self.undo_armed.set(false);
        if !self.feedback_enabled.get() {
            *self.last_commit.borrow_mut() = None;
        }
    }

    /// 確定取消（Ctrl+Backspace）: armed 中の Ctrl+Backspace 実処理の入口。
    /// 直前確定（last_commit）の確定文字列を GetText でキャレット手前に照合し、一致したら
    /// その range を composition 化して読みをエンジンで再変換 → 候補表示する。Esc は
    /// `reconvert_original`（=確定文字列）を RestoreText で書き戻す既存経路で無改修に成立する。
    ///
    /// armed ライフサイクル（I-6）: 成功 → armed 維持（連打は composition ガードで no-op 化）／
    /// text_mismatch・NoBuffer・TooLong → disarm／CompositionOpen → 維持（no-op）。
    /// ログは長さのみ（確定本文を出さない — I-3）。
    pub(crate) fn start_commit_undo(&self, ctx: &ITfContext) {
        // 前回確定の EndComposition だけが保留なら先に close-only で回収する。本文は既に
        // SetText 済みなので、ここで新しい composition を重ねてはいけない。
        if !self.finish_pending_composition(ctx) {
            tip_log("ev=commit_undo skipped=pending_end");
            return;
        }
        // 1) 純関数で事前条件を判定する（COM を触る前）。tlen は UTF-16 単位で数える。
        let armed = self.undo_armed.get();
        let has_composition = self.composition.borrow().is_some();
        // 照合に必要な reading/text を取り出す（バッファは take せず、成立確定後に take する）。
        let buf = self
            .last_commit
            .borrow()
            .as_ref()
            .map(|c| (c.reading.clone(), c.text.clone()));
        let has_buffer = buf.is_some();
        let tlen = buf.as_ref().map_or(0, |(_, t)| t.encode_utf16().count());
        match undo_precheck(armed, has_composition, has_buffer, tlen) {
            Ok(()) => {}
            Err(UndoSkip::NotArmed) => {
                tip_log("ev=commit_undo_skip reason=not_armed");
                self.disarm_undo();
                return;
            }
            Err(UndoSkip::CompositionOpen) => {
                // 開いている候補窓/preedit を壊さない no-op。armed は維持する。
                tip_log("ev=commit_undo_skip reason=composition_open");
                return;
            }
            Err(UndoSkip::NoBuffer) => {
                tip_log("ev=commit_undo_skip reason=no_buffer");
                self.disarm_undo();
                return;
            }
            Err(UndoSkip::TooLong) => {
                tip_log(&format!("ev=commit_undo_skip reason=too_long tlen={tlen}"));
                self.disarm_undo();
                return;
            }
        }
        let (reading, text) = buf.expect("has_buffer=true guarantees Some");
        // 深層防御: start_reconvert と同じく開始時にクリア(早期 return 経路で前回値を残さない)。
        self.reconvert_reading.borrow_mut().clear();

        // 2) キャレット手前を既知長ぴったり読み戻し、text にバイト一致したときだけ composition 化する。
        //    不一致・非空選択・読み取り失敗は何も書かない（do-no-harm、ReconvertStart :318-329 と同型）。
        let matched: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let sink: ITfCompositionSink = self.to_interface();
        let sess: ITfEditSession = CommitUndoStart {
            context: ctx.clone(),
            sink,
            composition: Rc::clone(&self.composition),
            started: Rc::clone(&self.composition_started_signal),
            expected: text.clone(),
            out: Rc::clone(&matched),
            left_context_out: Rc::clone(&self.left_context),
            _guard: ComObjectGuard::new(),
        }
        .into();
        unsafe {
            let _ = ctx.RequestEditSession(
                self.tid.get(),
                &sess,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0),
            );
        }
        self.consume_started_composition();
        if !*matched.borrow() {
            // 照合失敗（文書を一切書いていない）。武装を残さず離脱する（I-6）。
            tip_log("ev=commit_undo_skip reason=text_mismatch");
            self.disarm_undo();
            return;
        }

        // 3) 一致。Esc 復元用に確定文字列を原文としてセットし、バッファを消費する。
        //    以降 composition は開いている＝連打は composition ガードで no-op（armed は維持でよい）。
        *self.reconvert_original.borrow_mut() = text.clone();
        *self.reconvert_reading.borrow_mut() = reading.clone();
        *self.last_commit.borrow_mut() = None;

        // 4) 新セッションを張り直して読みをリプレイする（セッション不変条件 — start_reconvert 同型）。
        self.ensure_engine();
        self.engine_end_session();
        self.ensure_session();
        let _ = self.engine_insert(&reading, InsertStyle::Kana);
        let cands = self.engine_convert().unwrap_or_default();
        if cands.is_empty() {
            // 空結果: cancel_reconvert が reconvert_original（=確定文字列）を書き戻して畳む無害離脱。
            self.cancel_reconvert(ctx);
            return;
        }

        // 5) 候補表示（共有尾部）。ev は長さのみ（本文を出さない — I-3）。
        self.show_reconvert_candidates(ctx, &cands);
        let rlen = reading.encode_utf16().count();
        let tlen = text.encode_utf16().count();
        tip_log(&format!(
            "ev=commit_undo_shown n={} rlen={rlen} tlen={tlen}",
            cands.len()
        ));
    }

    /// Tab: 現在の読みを外部LLMへ。接続をワーカへ move し、preedit を「変換中…」にして
    /// 入力ロック（AwaitingLlm）。UI スレッドはポーリングタイマで結果を受け取る。
    pub(crate) fn start_llm_convert(&self, ctx: &ITfContext) {
        // 外部LLM変換が無効(フィーチャーフラグ off)なら何もしない（呼び元でも弾くが多重防御）。
        if !self.llm_enabled.get() {
            return;
        }
        if !self.state.borrow().composing || self.state.borrow().awaiting_llm() {
            return;
        }
        // 巡11(round11): 素材(表示中テキスト)が空なら開始しない — 空 Backspace の cancel
        // 拒否巻き戻し(composing=true・素材空)で Tab が通ると、空セッションへ投げた上に
        // preedit を「変換中…」で上書きし awaiting_llm 中の入力ロックで cancel 再試行を
        // 死なせる。素材が無い変換に意味は無い。
        if self.live_text.borrow().is_empty() {
            tip_log("ev=llm_skip_empty_text");
            return;
        }
        // 接続を取り出して move（無ければ劣化＝何もしない）。
        let client = match self.client.borrow_mut().take() {
            Some(c) => c,
            None => {
                tip_log("ev=llm_no_client");
                return;
            }
        };
        let session = self.engine_session.get();
        *self.pre_llm_text.borrow_mut() = self.live_text.borrow().clone();
        *self.current_context.borrow_mut() = Some(ctx.clone());
        // 修正候補窓が出ていれば閉じる（その上に「変換中…」を出さない）。input_char と同じ片付け。
        if self.showing.get() {
            self.candidate_ui.borrow_mut().hide();
            self.showing.set(false);
            self.clear_clause_nav();
        }
        let seq = self.state.borrow_mut().bump_llm_seq();
        self.state.borrow_mut().set_awaiting_llm(true);
        self.llm_started.set(Some(std::time::Instant::now())); // タイムアウト計測の起点
        self.disarm_debounce(); // 進行中のライブ変換タイマは止める
        self.run_preedit(ctx, "🌐変換中…");
        let slot: LlmSlot = Arc::new(Mutex::new(None));
        *self.llm_slot.borrow_mut() = Some(slot.clone());
        let left_context = self.left_context.borrow().clone();
        // ワーカ上限 30s: UI の LLM_TIMEOUT(8s) 打ち切り後もエンジン側タイムアウト応答
        // （llm_timeout_ms 既定 15s の Error）は受け取って接続を正常返却できる長さ。
        // これを超える無応答は接続破棄（B10: 無期限ブロックでワーカ/エンジン接続スレッドを
        // 永久占有しない）。
        // 巡3 P6: スレッド生成失敗（OS 資源枯渇）は panic させない — awaiting_llm=true の
        // まま poll 未武装で入力ロックが残る。arm_llm_poll 失敗と同じ abort_llm 劣化へ。
        if spawn_llm_worker(
            client,
            session,
            seq,
            left_context,
            slot,
            Duration::from_secs(30),
        )
        .is_err()
        {
            self.abort_llm("worker_spawn_failed");
            return;
        }
        if !self.arm_llm_poll() {
            // 巡2 B3: ポーリングを張れない以上、結果受け取りもタイムアウト判定も走らない。
            // seq を bump 済みの abort_llm で入力ロックを解除し読み preedit へ復元する
            // （in-flight のワーカ結果は seq 不一致で stale 扱いになる）。
            self.abort_llm("poll_arm_failed");
            return;
        }
        tip_log(&format!("ev=llm_request seq={seq} session={session}"));
    }

    /// LLM ポーリングタイマを武装する。失敗（タイマ資源枯渇。稀）は false — 呼び出し側は
    /// 即時 abort へ劣化する（巡2 B3）。awaiting_llm の解除経路は llm_poll_proc と Esc のみ
    /// なので、失敗を無視すると自動タイムアウトが死に、Esc を知らないユーザーは入力
    /// ロックから抜けられない（3窓の「SetTimer 失敗→即時劣化」と同じ規律）。
    fn arm_llm_poll(&self) -> bool {
        self.disarm_llm_poll();
        LLM_TS.with(|p| p.set(self as *const TextService_Impl));
        let id = unsafe { SetTimer(None, 0, LLM_POLL_MS, Some(llm_poll_proc)) };
        if id == 0 {
            return false;
        }
        self.llm_poll_timer.set(id);
        true
    }

    fn disarm_llm_poll(&self) {
        let id = self.llm_poll_timer.replace(0);
        if id != 0 {
            unsafe {
                let _ = KillTimer(None, id);
            }
        }
    }

    /// LLM 待機を中断する共通経路（Esc 手動取消・タイムアウト共用）。世代を進めて in-flight
    /// 結果を確実に stale 化し、入力ロック（AwaitingLlm）を解除、ポーリング/スロット/起点時刻を
    /// 片付け、接続を捨てて読み preedit へ復元する。これが無いと、応答が来ないエンジンでは
    /// `awaiting_llm()` が永久に真のまま残り、IME 全体がフリーズする。
    ///
    /// 注意: 接続（EngineClient）はワーカスレッドへ move 済みで、エンジンが真に無応答の場合は
    /// ワーカが read でブロックしたままになりうる（スレッド/ハンドルのリーク）。これを避けるため、
    /// spawn したエンジンを Child ハンドル経由で kill して pipe を壊し、ブロック中のワーカ read を
    /// 即座に失敗させてスレッド/ハンドルを回収する（L-5）。あわせて pipe_name を破棄し、次打鍵の
    /// `ensure_engine` が stable_pipe_name で同名パイプに再接続できるようにする（engine は永続
    /// singleton — pipe_name のキャッシュを空にするのは engine_pipe_name に再計算させるためで、
    /// 名前自体は logon session 固定で変わらない）。
    pub(crate) fn abort_llm(&self, reason: &str) {
        {
            let mut st = self.state.borrow_mut();
            st.bump_llm_seq(); // 後から届く結果を stale として確実に捨てる
            st.set_awaiting_llm(false); // 入力ロック解除（フリーズからの脱出）
        }
        self.disarm_llm_poll();
        *self.llm_slot.borrow_mut() = None;
        self.llm_started.set(None);
        self.pipe_name.borrow_mut().clear(); // キャッシュを空にし、次回 engine_pipe_name に同名で再解決させる
                                             // 共有 engine は殺さない（他ホストが接続中の永続 singleton。旧 oneShot 専用 engine 時代の kill を
                                             // ここで行うと設定アプリ等を巻き込んで変換不可にする）。drop_engine が Child ハンドルを手放す
                                             // （プロセス継続）。ブロック中の LLM worker は engine 応答で自然完了し、戻った接続は stale 化済みで
                                             // drop される＝その1接続のみ閉じ engine は生存。真にハングした稀ケースは worker リークを許容する。
        self.drop_engine();
        let ctx = self.current_context.borrow().clone();
        self.restore_pre_llm(ctx);
        tip_log(&format!("ev=llm_abort reason={reason}"));
    }

    /// LLM 待機が上限時間を超えたか（llm_poll_proc から呼ぶ）。
    fn llm_timed_out(&self) -> bool {
        self.llm_started
            .get()
            .map(|t| t.elapsed() >= LLM_TIMEOUT)
            .unwrap_or(false)
    }

    /// ワーカ結果を UI スレッドで反映する。seq 最新かつ成功なら適用、古い/空/失敗なら pre-LLM へ復元。
    pub(crate) fn on_llm_outcome(&self, o: LlmOutcome) {
        self.state.borrow_mut().set_awaiting_llm(false);
        self.llm_started.set(None);
        let ctx = self.current_context.borrow().clone();
        let current = self.state.borrow().llm_seq;
        let fresh = is_fresh_live(o.seq, current);
        match o.result {
            Ok(text) if fresh && !text.is_empty() => {
                if let Some(c) = o.client {
                    *self.client.borrow_mut() = Some(c);
                }
                self.flush_pending_end_session(); // 合成が in-flight 中に終了していたら保留 EndSession を送る
                self.state.borrow_mut().mark_good(&text);
                *self.live_text.borrow_mut() = text.clone();
                if let Some(ctx) = ctx {
                    self.run_preedit(&ctx, &text);
                }
                tip_log(&format!("ev=llm_applied seq={}", o.seq));
            }
            Ok(_) => {
                // 古い seq（Esc等）or 空 → 接続を戻し pre-LLM へ復元。
                if let Some(c) = o.client {
                    *self.client.borrow_mut() = Some(c);
                }
                self.flush_pending_end_session(); // 合成が in-flight 中に終了していたら保留 EndSession を送る
                self.restore_pre_llm(ctx);
                tip_log(&format!(
                    "ev=llm_stale_or_empty seq={} current={}",
                    o.seq, current
                ));
            }
            Err(_) => {
                // 失敗 → 接続は drop（戻さない）。次操作で再接続。pre-LLM へ復元。
                self.drop_engine();
                self.restore_pre_llm(ctx);
                tip_log("ev=llm_failed");
            }
        }
    }

    fn restore_pre_llm(&self, ctx: Option<ITfContext>) {
        let pre = self.pre_llm_text.borrow().clone();
        // last_good は live_text でなく「実際に画面へ出す文字列」で記録する — pre が
        // 空のとき表示は last_reading であり、劣化フォールバックの素材はそちらが正しい。
        let shown = if pre.is_empty() {
            self.last_reading.borrow().clone()
        } else {
            pre.clone()
        };
        self.state.borrow_mut().mark_good(&shown);
        *self.live_text.borrow_mut() = pre.clone();
        if let Some(ctx) = ctx {
            if pre.is_empty() {
                // 退避が空なら読みのまま（last_reading）に。
                let r = self.last_reading.borrow().clone();
                self.run_preedit(&ctx, &r);
            } else {
                self.run_preedit(&ctx, &pre);
            }
        }
    }

    /// タイマ発火時（入力が一定時間落ち着いた）の遅延変換。
    /// composing 中なら現在の読みを convert し preedit を漢字へ全置換する。失敗/空は据え置き。
    /// auto_commit=true で要求するのはこの経路だけ: エンジンが自動確定（iOS nospacekey の
    /// 先頭文節自動確定）を返したら apply_live_auto_commit で prefix を確定し残りを継続する。
    pub(crate) fn on_debounce_convert(&self) {
        if !self.state.borrow().composing {
            self.partial_preedit_redraw_pending.set(false);
            self.partial_preedit_redraw_retries.set(0);
            return;
        }
        let ctx = match self.current_context.borrow().clone() {
            Some(c) => c,
            None => return,
        };
        if self.partial_preedit_redraw_pending.get() && !self.redraw_partial_preedit_if_needed(&ctx)
        {
            // 旧 composition がまだ閉じられない間は変換を進めない。タイマは単発なので
            // bounded retry を再武装し、上限後は終了 callback / 次打鍵の障壁へ委ねる。
            self.arm_partial_preedit_redraw_retry();
            return;
        }
        if !self.live_enabled.get() {
            return;
        }
        // INV6: pending（未読応答を owe）中は新規 LiveConvert を発行しない。engine_live_convert
        //       内の prepare_send がドレインを試み、解消できなければ要求は送らず None を返す
        //       （＝この経路はドレイン試行だけ行い、回収できたときのみ次の変換へ進む）。
        let seq = self.state.borrow_mut().bump_live_seq();
        if let Some((text, reading, committed)) = self.engine_live_convert(seq, true) {
            if let Some(prefix) = committed.filter(|p| !p.is_empty()) {
                self.apply_live_auto_commit(&ctx, &prefix, &text, &reading);
            } else if !text.is_empty() {
                self.state.borrow_mut().mark_good(&text);
                *self.live_text.borrow_mut() = text.clone();
                // 表示だけ全角化（mark_good/live_text は半角のまま — 劣化時の確定素材だから）。
                self.run_preedit(&ctx, &self.widen_display_text(&text));
            }
        }
    }

    /// バックスペースを送って更新後の読みを得る。失敗なら None（劣化）し接続を破棄する。
    pub(crate) fn engine_backspace(&self) -> Option<String> {
        // Backspace は TIP 側 raw を先に進める。drain 不成立の接続を保持すると engine 側だけ
        // 削除を見落とすため、Insert と同じく drop→次打鍵 reseed へ倒す。
        if !self.prepare_send_or_drop("backspace", IPC_TIMEOUT_FAST) {
            return None;
        }
        let session = self.engine_session.get();
        let result = {
            let mut guard = self.client.borrow_mut();
            let client = guard.as_mut()?;
            timed_request(
                client,
                &Request::Backspace { session },
                IPC_TIMEOUT_FAST,
                "backspace",
            )
        };
        match result {
            Ok(Response::Reading { reading }) => Some(reading),
            other => {
                tip_log(&engine_failure_event("backspace", &other));
                tip_log("ev=degraded reason=backspace_failed");
                self.drop_engine();
                None
            }
        }
    }

    /// エンジンの現在セッションを終了する。
    /// 終了後は `engine_session` を 0 に戻し、次の composition で張り直せるようにする。
    /// Bug 1: EndSession がタイムアウト/broken pipe で失敗したら **接続を破棄する**
    /// （convert/reconvert/commit/backspace/start_session と同じ形に揃える）。
    /// さもないと遅延応答フレームがパイプに滞留し、以降そのパイプ上の全リクエストが
    /// 「1つ前のリクエストの応答」を読む恒常 1-off desync になる（request-id 相関が無く
    /// 正しさが厳密な要求/応答交互性のみに依存するため）。start_reconvert 等は直後に
    /// ensure_session を呼ぶが、drop 後は client=None なので無害に degrade する。
    ///
    /// 唯一の例外（A' pending+drain）: LiveConvert/Insert のタイムアウトだけは drop_engine せず、
    /// 未読応答を `pending_since` に owe して接続とセッションを保つ（自動確定の安定履歴＝セッション
    /// 単位を守る）。交互性は「次の要求を送る前に prepare_send が drain_pending で滞留フレームを
    /// 1 枚読み切る」ことで回復する（INV1）。ドレインで committed 付き LiveResult を回収したら
    /// engine 側だけ確定適用済みの不整合なので安全側で drop（INV2）。他 op はこの例外に入らない。
    /// borrow は `result` ブロック内で完結させ、drop 後に `drop_engine` を呼ぶ（二重借用 panic 防止）。
    pub(crate) fn engine_end_session(&self) {
        let session = self.engine_session.get();
        if session == 0 {
            return;
        }
        // INV1: owe している応答を読み切ってから送る。
        // Why not(従来どおりドレインせず送って失敗に任せる): desync はしない — `request_within` は
        // pending 中の送信を I/O 前に `InvalidInput` で弾く（client.rs の規律チェック）ので、従来形は
        // 必ず `end_session_failed` → `drop_engine` に落ちていた。問題は EndSession が届かないまま
        // 接続が切れ、次打鍵が再接続＋StartSession（`ensure_engine` のフルコースは最悪 ~1.15s）を
        // 払うこと。先にドレインすれば接続とセッションを保ったまま EndSession を届けられる。
        // 代償は打鍵スレッドの追加ブロックで、owe 中の composition 終端に限り最悪
        // IPC_TIMEOUT_FAST（ドレイン）＋同（送信）。空振り（StillPending）ならその待ちは丸ごと
        // 無駄になるが、250ms で応答を返せない engine なら再接続は避けられない。
        // Why not(StillPending を据え置いて次の要求で再ドレイン — insert/live_convert 形):
        // composition 終端の呼び出しには同じセッションへ送る次の機会が無く、`ensure_engine →
        // engine_end_session → ensure_session` でセッションを張り直す呼び方（`start_commit_undo` /
        // `start_reconvert`）では `engine_session` を 0 にしないと直後の `ensure_session` が
        // 早期 return して古いセッション（残り読み入り）を再利用する（defect#2）。どちらの
        // 呼ばれ方でも据え置きは選べない。接続を捨てれば --persist サーバの切断ハンドラが
        // 孤児セッションを掃除する（drop_engine のドキュメント参照）。
        // Why not(client 不在でもドレインを通す): ドレイン対象は `self.client` 上の滞留フレームで、
        // LLM ワーカへ move 中は読む先が無い。それでも通すと PENDING_MAX 超過枝が `Dropped` を
        // 返し、下の `DrainOutcome::Dropped => return` で「復帰時に送り直す」None 枝に届かず
        // セッション id を落とす。なお `pending_since` は client が Some のときしか立たない
        // （`engine_insert`/`engine_live_convert` の TimedOut 枝はどちらも `guard.as_mut()?` の後）
        // ので、その枝に入るには park 時点で `EngineClient::pending` が真だったことになり、その
        // client は `spawn_llm_worker` の `request_within` が規律ガードで即失敗させて閉じる
        // ＝到達手順は無い。よってこれは深層防御であり、既知の再現ケースは存在しない。
        let parked_in_llm_worker = self.client.borrow().is_none();
        if !parked_in_llm_worker {
            match self.prepare_send("end_session", IPC_TIMEOUT_FAST) {
                DrainOutcome::Proceed => {}
                DrainOutcome::StillPending => {
                    self.drop_engine();
                    return;
                }
                DrainOutcome::Dropped => return,
            }
        }
        let result = {
            let mut guard = self.client.borrow_mut();
            guard.as_mut().map(|client| {
                timed_request(
                    client,
                    &Request::EndSession { session },
                    IPC_TIMEOUT_FAST,
                    "end_session",
                )
            })
        };
        self.engine_session.set(0);
        match result {
            Some(r) => {
                if !end_session_ack_accepted(&r) {
                    tip_log(&engine_failure_event("end_session", &r));
                    tip_log("ev=degraded reason=end_session_failed");
                    self.drop_engine();
                }
            }
            None => {
                // client は LLM ワーカへ move 済みで今は送れない。id を保留し、復帰時に EndSession を送る。
                // さもないと engine 側にセッションが取り残され、ConversionService の stopComposition も
                // 永久に走らない（sessions.isEmpty にならない）。
                self.pending_end_session.set(session);
            }
        }
    }

    /// 再変換訂正の通知。確定は呼び出し前に完了しているため、失敗しても確定動作に影響しない。
    /// タイムアウトを pending owe にしないのは、この op に「後で読む」価値が無く
    /// (応答は Ok のみ)、request-id 無しプロトコルで owe を増やすと desync 面積が広がるだけのため。
    pub(crate) fn engine_record_correction(&self, reading: &str, surface: &str) {
        match self.prepare_send("record_correction", IPC_TIMEOUT_FAST) {
            DrainOutcome::Proceed => {}
            DrainOutcome::StillPending | DrainOutcome::Dropped => return,
        }
        let result = {
            let mut guard = self.client.borrow_mut();
            let client = match guard.as_mut() {
                Some(c) => c,
                None => return,
            };
            timed_request(
                client,
                &Request::RecordCorrection {
                    reading: reading.to_string(),
                    surface: surface.to_string(),
                },
                IPC_TIMEOUT_FAST,
                "record_correction",
            )
        };
        match result {
            Ok(_) => {} // Ok。旧エンジンは Error を返すがどちらも無視(記録は best-effort)
            Err(_) => {
                tip_log("ev=degraded reason=record_correction_failed");
                self.drop_engine();
            }
        }
    }

    /// client 復帰後（on_llm_outcome）に、保留していた EndSession を送って取り残しを掃除する。
    /// Bug 1: engine_end_session と同じ ack 判定（`end_session_ack_accepted`）を使い、受理外なら
    /// 接続を破棄して応答フレームの滞留を防ぐ。
    /// Why not(engine_end_session と同じ送信前ドレインも入れる): ここへ来る client は
    /// `spawn_llm_worker` が **`request_within` 成功時だけ**返したものなので owe を持たない
    /// （失敗時は client を返さず、on_llm_outcome の Err 枝が drop_engine するのでここへ来ない）。
    /// borrow は `result` ブロック内で完結させ、drop 後に `drop_engine` を呼ぶ（二重借用 panic 防止）。
    fn flush_pending_end_session(&self) {
        let session = self.pending_end_session.replace(0);
        if session == 0 {
            return;
        }
        let result = {
            let mut guard = self.client.borrow_mut();
            guard.as_mut().map(|client| {
                timed_request(
                    client,
                    &Request::EndSession { session },
                    IPC_TIMEOUT_FAST,
                    "end_session",
                )
            })
        };
        if let Some(r) = result {
            if !end_session_ack_accepted(&r) {
                tip_log(&engine_failure_event("flush_end_session", &r));
                tip_log("ev=degraded reason=end_session_failed");
                self.drop_engine();
            }
        }
    }

    /// 下線属性 atom を内包した VARIANT を作る（atom 未登録なら i32(0)）。
    fn da_variant(&self) -> VARIANT {
        VARIANT::from(self.da_atom.get() as i32)
    }

    /// 選択文節（太下線）属性 atom を内包した VARIANT を作る（atom 未登録なら i32(0)）。
    fn da_target_variant(&self) -> VARIANT {
        VARIANT::from(self.da_target_atom.get() as i32)
    }

    fn da_prediction_variant(&self) -> VARIANT {
        VARIANT::from(self.da_prediction_atom.get() as i32)
    }

    pub(crate) fn prediction_ghost_visible(&self) -> bool {
        self.prediction_composition.borrow().is_some()
    }

    pub(crate) fn prediction_ghost_actionable(&self) -> bool {
        self.prediction_ghost_visible()
            && self.prediction_finish_pending.get().is_none()
            && self.prediction_state.borrow().ghost().is_some()
            && prediction_mode_allows_display(self.is_direct_mode(), self.ephemeral_kana.get())
    }

    pub(crate) fn prediction_cleanup_in_progress(&self) -> bool {
        self.prediction_ghost_visible() && self.prediction_finish_pending.get().is_some()
    }

    fn prediction_owner_is_focused(&self) -> bool {
        match (
            self.prediction_context.borrow().as_ref(),
            self.layout_sink_ctx.borrow().as_ref(),
        ) {
            (Some(owner), Some(focused)) => com_identity_eq(owner, focused),
            _ => false,
        }
    }

    fn mark_prediction_tsf_failure_for(&self, ctx: &ITfContext) {
        *self.prediction_failed_context.borrow_mut() = Some(ctx.clone());
    }

    fn mark_prediction_tsf_failure_for_owner(&self) {
        if let Some(ctx) = self
            .prediction_context
            .borrow()
            .clone()
            .or_else(|| self.layout_sink_ctx.borrow().clone())
        {
            self.mark_prediction_tsf_failure_for(&ctx);
        }
    }

    fn prediction_failed_for(&self, ctx: &ITfContext) -> bool {
        self.prediction_failed_context
            .borrow()
            .as_ref()
            .is_some_and(|failed| com_identity_eq(failed, ctx))
    }

    pub(crate) fn retry_prediction_cleanup_on_input(&self) {
        if self.prediction_cleanup_in_progress() {
            // 上限到達／SetTimer失敗後でも次打鍵を安全点として再武装する。
            // 打鍵自体は本来の IME/host 判定へ進めるため、永久入力ロックにならない。
            self.arm_prediction_finish_retry(true);
        }
    }

    pub(crate) fn cancel_deferred_prediction_preserved_on_input(&self) {
        self.prediction_deferred_preserved.borrow_mut().clear();
    }

    pub(crate) fn defer_prediction_preserved_key(&self, context: Option<ITfContext>, guid: GUID) {
        let mut deferred = self.prediction_deferred_preserved.borrow_mut();
        // reconvert / feedback は連打しても一度で意味が足りる。GUID 種類数で自然に有界。
        if deferred.iter().any(|event| event.guid == guid) {
            return;
        }
        deferred.push_back(DeferredPredictionPreservedKey { context, guid });
    }

    pub(crate) fn flush_deferred_prediction_preserved_keys_if_ready(&self) {
        if self.prediction_ghost_visible() {
            return;
        }
        let events = std::mem::take(&mut *self.prediction_deferred_preserved.borrow_mut());
        let sink: ITfKeyEventSink = self.to_interface();
        for event in events {
            let result = unsafe { sink.OnPreservedKey(event.context.as_ref(), &event.guid) };
            if result.is_err() {
                tip_log("ev=prediction_preserved_replay result=failed");
            }
        }
    }

    fn resume_preedit_after_prediction_cleanup(&self) {
        if !self.partial_preedit_redraw_pending.get() {
            return;
        }
        let Some(ctx) = self.current_context.borrow().clone() else {
            return;
        };
        if self.redraw_partial_preedit_if_needed(&ctx) {
            self.arm_debounce();
        } else {
            self.arm_partial_preedit_redraw_retry();
        }
    }

    fn next_prediction_anchor(&self) -> crate::prediction_state::PredictionAnchor {
        let next = self.prediction_anchor_gen.get().saturating_add(1);
        self.prediction_anchor_gen.set(next);
        crate::prediction_state::PredictionAnchor::new(next)
    }

    fn prediction_now() -> crate::prediction_state::Timestamp {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
        crate::prediction_state::Timestamp::from_millis(
            elapsed.as_millis().min(u64::MAX as u128) as u64
        )
    }

    pub(crate) fn on_explicit_prediction_commit(
        &self,
        source: crate::prediction_state::CommitSource,
        text: &str,
    ) {
        if !self.prediction_enabled.get() || text.is_empty() {
            return;
        }
        let context = self
            .current_context
            .borrow()
            .clone()
            .or_else(|| self.layout_sink_ctx.borrow().clone());
        let Some(context) = context else {
            self.invalidate_prediction(crate::prediction_state::Invalidation::Disabled);
            tip_log("ev=prediction_unavailable state=no_context");
            return;
        };
        // InputScope の取得不能は通常欄を誤って無効化しないよう許可し、次の確定で再照会する。
        // 明示された password/PIN と keyboard-disabled context だけを抑止する。
        if prediction_scope_is_sensitive(
            query_context_keyboard_disabled(&context),
            self.query_context_is_password(&context),
        ) {
            self.invalidate_prediction(crate::prediction_state::Invalidation::Disabled);
            tip_log("ev=prediction_unavailable state=sensitive_scope");
            return;
        }
        self.cancel_prediction_slot();
        let should_request = {
            let mut state = self.prediction_state.borrow_mut();
            state.on_commit(
                source,
                text,
                self.next_prediction_anchor(),
                Self::prediction_now(),
            );
            state.has_activity()
        };
        self.prediction_commit_edit_deadline.set(Some(
            Instant::now() + Duration::from_millis(u64::from(PREDICTION_DEBOUNCE_MS)),
        ));
        if should_request {
            self.arm_prediction_poll(PREDICTION_DEBOUNCE_MS);
            tip_log("ev=prediction_request stage=debounce");
        } else {
            self.disarm_prediction_poll();
            tip_log("ev=prediction_wait state=context_buffered");
        }
    }

    fn cancel_prediction_slot(&self) {
        if let Some(slot) = self.prediction_slot.borrow_mut().take() {
            slot.cancel();
        }
    }

    pub(crate) fn invalidate_prediction(&self, reason: crate::prediction_state::Invalidation) {
        self.prediction_commit_edit_deadline.set(None);
        self.cancel_prediction_slot();
        self.prediction_state.borrow_mut().invalidate(reason);
    }

    fn arm_prediction_poll(&self, delay_ms: u32) {
        self.disarm_prediction_poll();
        PREDICTION_POLL_TS.with(|p| p.set(self as *const TextService_Impl));
        let id = unsafe { SetTimer(None, 0, delay_ms, Some(prediction_poll_timer_proc)) };
        if id == 0 {
            self.invalidate_prediction(crate::prediction_state::Invalidation::Disabled);
            tip_log("ev=prediction_unavailable state=timer_failed");
        } else {
            self.prediction_poll_timer.set(id);
        }
    }

    fn disarm_prediction_poll(&self) {
        let id = self.prediction_poll_timer.replace(0);
        if id != 0 {
            unsafe {
                let _ = KillTimer(None, id);
            }
        }
    }

    fn fire_prediction_poll(&self, id: usize) {
        if self.prediction_poll_timer.get() != id {
            return;
        }
        self.prediction_poll_timer.set(0);
        // An edit arriving after the debounce is no longer the commit's trailing
        // notification; it must invalidate the request like any other edit.
        self.prediction_commit_edit_deadline.set(None);
        if !prediction_mode_allows_display(self.is_direct_mode(), self.ephemeral_kana.get()) {
            if self.prediction_ghost_visible() {
                let _ = self.dismiss_prediction_ghost(false);
            } else {
                self.invalidate_prediction(crate::prediction_state::Invalidation::ModeChanged);
            }
            tip_log("ev=prediction_invalidate source=mode_poll");
            return;
        }
        let now = Self::prediction_now();
        let outcome = self
            .prediction_slot
            .borrow()
            .as_ref()
            .and_then(|slot| slot.take());
        if let Some(outcome) = outcome {
            self.cancel_prediction_slot();
            tip_log(&format!(
                "ev=prediction_result duration_ms={}",
                outcome.duration_ms
            ));
            match outcome.result {
                IpcPredictionResult::Prediction(text) => {
                    if self
                        .prediction_state
                        .borrow_mut()
                        .on_result(outcome.seq, &text, now)
                        .is_some()
                    {
                        let ctx = self.layout_sink_ctx.borrow().clone();
                        if let Some(ctx) = ctx {
                            let _ = self.show_prediction_ghost(&ctx);
                        }
                    }
                }
                IpcPredictionResult::Unavailable(state) => {
                    let _ = self
                        .prediction_state
                        .borrow_mut()
                        .on_result(outcome.seq, "", now);
                    tip_log(&format!("ev=prediction_unavailable state={state}"));
                }
                IpcPredictionResult::Failed => {
                    let _ = self
                        .prediction_state
                        .borrow_mut()
                        .on_result(outcome.seq, "", now);
                    tip_log("ev=prediction_unavailable state=ipc_failed");
                }
            }
            return;
        }

        if self.prediction_state.borrow_mut().expire_pending(now) {
            self.cancel_prediction_slot();
            tip_log("ev=prediction_timeout");
            return;
        }
        if self.prediction_slot.borrow().is_some() {
            self.arm_prediction_poll(PREDICTION_POLL_MS);
            return;
        }
        let request = self.prediction_state.borrow_mut().poll(now);
        if let Some(request) = request {
            let slot = PredictionSlot::new();
            *self.prediction_slot.borrow_mut() = Some(slot.clone());
            let pipe_name = self.pipe_name.borrow().clone();
            if spawn_ipc_prediction_worker(pipe_name, request, slot, PREDICTION_TIMEOUT).is_err() {
                self.invalidate_prediction(crate::prediction_state::Invalidation::Disabled);
                tip_log("ev=prediction_unavailable state=worker_spawn_failed");
                return;
            }
            tip_log("ev=prediction_request stage=sent");
            self.arm_prediction_poll(PREDICTION_POLL_MS);
        } else if self.prediction_state.borrow().has_activity() {
            self.arm_prediction_poll(PREDICTION_POLL_MS);
        }
    }

    /// `PredictionState` が保持する候補を現在キャレットへ表示する。
    // Task 4 の予測IPC結果から呼ぶまでの遷移期間だけ未使用を許可する。
    #[allow(dead_code)]
    pub(crate) fn show_prediction_ghost(&self, ctx: &ITfContext) -> bool {
        if !prediction_mode_allows_display(self.is_direct_mode(), self.ephemeral_kana.get()) {
            self.invalidate_prediction(crate::prediction_state::Invalidation::ModeChanged);
            tip_log("ev=prediction_invalidate source=mode_show");
            return false;
        }
        let sink_matches_context = self
            .layout_sink_ctx
            .borrow()
            .as_ref()
            .is_some_and(|sink_ctx| com_identity_eq(sink_ctx, ctx));
        if self.text_edit_sink_cookie.get() == 0 || !sink_matches_context {
            self.mark_prediction_tsf_failure_for(ctx);
            self.invalidate_prediction(crate::prediction_state::Invalidation::Disabled);
            tip_log("ev=prediction_disabled reason=edit_sink_context");
            return false;
        }
        let slot_available = prediction_slot_available(
            self.prediction_composition.borrow().is_some(),
            self.prediction_finish_pending.get().is_some(),
        );
        if !slot_available {
            // 別欄の旧 physical slot が回収中なら、この欄の結果は後から表示せず stale に畳む。
            self.invalidate_prediction(crate::prediction_state::Invalidation::Input);
            return false;
        }
        if self.prediction_failed_for(ctx)
            || self.da_prediction_atom.get() == 0
            || self.composition.borrow().is_some()
            || self.composition_end_pending.get()
            || self.showing.get()
            || self.is_password_context(ctx)
        {
            return false;
        }
        let Some(text) = self
            .prediction_state
            .borrow()
            .ghost()
            .map(|ghost| ghost.text.clone())
        else {
            return false;
        };
        let sink: ITfCompositionSink = self.to_interface();
        let session: ITfEditSession = StartPredictionGhost {
            context: ctx.clone(),
            text: HSTRING::from(text),
            sink,
            da_variant: self.da_prediction_variant(),
            composition: Rc::clone(&self.prediction_composition),
            editing: Rc::clone(&self.prediction_editing),
            _guard: ComObjectGuard::new(),
        }
        .into();
        // StartComposition 後の部分失敗でも同じ context で除去・終了を再試行できるよう、
        // RequestEditSession より前に owner を固定する。
        *self.prediction_context.borrow_mut() = Some(ctx.clone());
        let applied = match unsafe {
            ctx.RequestEditSession(
                self.tid.get(),
                &session,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0),
            )
        } {
            Ok(hr) => hr.is_ok() && self.prediction_composition.borrow().is_some(),
            Err(_) => false,
        };
        if applied {
            *self.prediction_context.borrow_mut() = Some(ctx.clone());
            // 外部の compartment 書換えも短時間で検出し、direct mode 上に ghost を残さない。
            self.arm_prediction_poll(PREDICTION_POLL_MS);
            tip_log("ev=prediction_show");
        } else {
            if self.prediction_composition.borrow().is_none() {
                *self.prediction_context.borrow_mut() = None;
            } else {
                self.prediction_finish_pending.set(Some(false));
                self.arm_prediction_finish_retry(true);
            }
            self.mark_prediction_tsf_failure_for(ctx);
            self.invalidate_prediction(crate::prediction_state::Invalidation::Input);
            tip_log("ev=prediction_disabled reason=tsf_show");
        }
        applied
    }

    fn disarm_prediction_finish_retry(&self) {
        let id = self.prediction_retry_timer.replace(0);
        if id != 0 {
            unsafe {
                let _ = KillTimer(None, id);
            }
        }
        self.prediction_retry_count.set(0);
    }

    /// TSF ロック競合で予測 composition を終了できなかった場合の有界再試行。
    /// owner context と composition slot は成功まで保持し、タイマは STA だけで発火する。
    fn arm_prediction_finish_retry(&self, reset_budget: bool) {
        if self.prediction_composition.borrow().is_none()
            && self.prediction_deferred_preserved.borrow().is_empty()
        {
            *self.prediction_context.borrow_mut() = None;
            self.prediction_finish_pending.set(None);
            self.disarm_prediction_finish_retry();
            return;
        }
        if reset_budget {
            self.prediction_retry_count.set(0);
        }
        if self.prediction_retry_timer.get() != 0 {
            return;
        }
        if self.prediction_retry_count.get() >= PREDICTION_RETRY_MAX {
            tip_log("ev=prediction_cleanup retry=exhausted");
            return;
        }
        let id = unsafe {
            SetTimer(
                None,
                0,
                PREDICTION_RETRY_MS,
                Some(prediction_retry_timer_proc),
            )
        };
        if id == 0 {
            if self.prediction_owner_is_focused() {
                self.mark_prediction_tsf_failure_for_owner();
            }
            tip_log("ev=prediction_cleanup retry=timer_failed");
        } else {
            self.prediction_retry_timer.set(id);
        }
    }

    fn fire_prediction_finish_retry(&self, id: usize) {
        if self.prediction_retry_timer.get() != id {
            return;
        }
        self.prediction_retry_timer.set(0);
        if self.prediction_composition.borrow().is_none() {
            *self.prediction_context.borrow_mut() = None;
            self.prediction_finish_pending.set(None);
            self.prediction_retry_count.set(0);
            self.resume_preedit_after_prediction_cleanup();
            self.flush_deferred_prediction_preserved_keys_if_ready();
            return;
        }
        let accept = self.prediction_finish_pending.get().unwrap_or(false);
        if self.request_finish_prediction_ghost(accept, false) {
            self.prediction_retry_count.set(0);
            tip_log("ev=prediction_cleanup retry=success");
            self.resume_preedit_after_prediction_cleanup();
            self.flush_deferred_prediction_preserved_keys_if_ready();
            return;
        }
        if self.prediction_owner_is_focused() {
            self.mark_prediction_tsf_failure_for_owner();
        }
        self.prediction_retry_count
            .set(self.prediction_retry_count.get().saturating_add(1));
        self.arm_prediction_finish_retry(false);
    }

    fn request_finish_prediction_ghost(&self, accept: bool, asynchronous: bool) -> bool {
        let Some(ctx) = self.prediction_context.borrow().clone() else {
            return self.prediction_composition.borrow().is_none();
        };
        self.prediction_finish_pending.set(Some(accept));
        let session: ITfEditSession = FinishPredictionGhost {
            context: ctx.clone(),
            composition: Rc::clone(&self.prediction_composition),
            owner_context: Rc::clone(&self.prediction_context),
            editing: Rc::clone(&self.prediction_editing),
            failure_context: Rc::clone(&self.prediction_failed_context),
            pending: Rc::clone(&self.prediction_finish_pending),
            accept,
            _guard: ComObjectGuard::new(),
        }
        .into();
        let flags = if asynchronous {
            TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_ASYNC.0 | TF_ES_READWRITE.0)
        } else {
            TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0)
        };
        match unsafe { ctx.RequestEditSession(self.tid.get(), &session, flags) } {
            Ok(hr) if asynchronous => hr.is_ok(),
            Ok(hr) => hr.is_ok() && self.prediction_composition.borrow().is_none(),
            Err(_) => false,
        }
    }

    pub(crate) fn accept_prediction_ghost(&self) -> bool {
        if !self.prediction_ghost_visible() {
            return false;
        }
        if !self.request_finish_prediction_ghost(true, false) {
            // accept を後から成功させると、その間に来た入力より ghost が後置されて順序が逆転する。
            // TSF 拒否時は予測だけを捨てる方へ降格し、通常入力を本来の IME 経路で継続させる。
            self.prediction_finish_pending.set(Some(false));
            self.mark_prediction_tsf_failure_for_owner();
            self.invalidate_prediction(crate::prediction_state::Invalidation::Disabled);
            self.arm_prediction_finish_retry(true);
            tip_log("ev=prediction_disabled reason=tsf_accept");
            return false;
        }
        let anchor = self.next_prediction_anchor();
        let accepted = self
            .prediction_state
            .borrow_mut()
            .accept_ghost(anchor, Self::prediction_now())
            .is_some();
        if accepted {
            self.arm_prediction_poll(PREDICTION_DEBOUNCE_MS);
            tip_log("ev=prediction_accept");
        }
        accepted
    }

    pub(crate) fn dismiss_prediction_ghost(&self, suppress_same_context: bool) -> bool {
        if !self.prediction_ghost_visible() {
            return false;
        }
        if !self.request_finish_prediction_ghost(false, false) {
            self.mark_prediction_tsf_failure_for_owner();
            self.invalidate_prediction(crate::prediction_state::Invalidation::Disabled);
            self.arm_prediction_finish_retry(true);
            tip_log("ev=prediction_disabled reason=tsf_dismiss");
            return false;
        }
        if suppress_same_context {
            self.prediction_state.borrow_mut().dismiss_ghost();
            tip_log("ev=prediction_dismiss");
        } else {
            self.invalidate_prediction(crate::prediction_state::Invalidation::Input);
        }
        true
    }

    fn invalidate_prediction_after_external_edit(&self) {
        self.invalidate_prediction(crate::prediction_state::Invalidation::SelectionChanged);
        if !self.prediction_ghost_visible() {
            *self.prediction_context.borrow_mut() = None;
            return;
        }
        if !self.request_finish_prediction_ghost(false, true) {
            self.mark_prediction_tsf_failure_for_owner();
            self.arm_prediction_finish_retry(true);
            tip_log("ev=prediction_disabled reason=tsf_external_edit");
        } else {
            // Request の受理は DoEditSession の成功を保証しない。実行終了まで
            // bounded timer で slot を監視し、後発失敗なら同じ owner context で再試行する。
            self.arm_prediction_finish_retry(true);
        }
    }

    fn abandon_prediction_for_context_change(
        &self,
        reason: crate::prediction_state::Invalidation,
    ) -> bool {
        self.prediction_deferred_preserved.borrow_mut().clear();
        self.invalidate_prediction(reason);
        if self.prediction_ghost_visible() && !self.request_finish_prediction_ghost(false, false) {
            self.mark_prediction_tsf_failure_for_owner();
            self.arm_prediction_finish_retry(true);
            tip_log("ev=prediction_disabled reason=tsf_context_change");
            return false;
        }
        *self.prediction_context.borrow_mut() = None;
        self.prediction_finish_pending.set(None);
        self.disarm_prediction_finish_retry();
        true
    }

    /// preedit を `text` にする編集セッションを同期実行する。失敗は no-op。
    pub(crate) fn run_preedit(&self, ctx: &ITfContext, text: &str) -> bool {
        self.run_preedit_with_target(ctx, text, None)
    }

    /// Edit session 内の `StartComposition` が実際に成功した後だけ、新しい composition
    /// lifecycle を開始する。RequestEditSession 拒否・StartComposition 前の失敗では shared
    /// signal が立たないため、Test→Key reservation と stale callback 世代を変更しない。
    /// OnTest/OnKey/CompositionTerminated などの同期再入入口でも先頭から呼ぶことで、caller
    /// が RequestEditSession から戻る前の COM callout でも同じ one-shot を消費できる。
    pub(crate) fn consume_started_composition(&self) {
        if !self.composition_started_signal.replace(false) {
            return;
        }
        self.invalidate_pending_end_test_reservation();
        self.composition_generation
            .set(self.composition_generation.get().wrapping_add(1));
    }

    /// 部分確定後に保留した残り読みを、旧 composition の close 完了後に一度だけ張り直す。
    /// 戻り値 false は「まだ close/再描画できない」なので、caller は timer または次打鍵へ委ねる。
    pub(crate) fn redraw_partial_preedit_if_needed(&self, ctx: &ITfContext) -> bool {
        if !self.partial_preedit_redraw_pending.get() {
            return true;
        }
        if !self.state.borrow().composing {
            self.partial_preedit_redraw_pending.set(false);
            self.partial_preedit_redraw_retries.set(0);
            return true;
        }
        if self.composition_end_pending.get() && !self.finish_pending_composition(ctx) {
            return false;
        }
        let text = self.live_text.borrow().clone();
        let shown = if text.is_empty() {
            self.last_reading.borrow().clone()
        } else {
            text
        };
        let applied = self.run_preedit(ctx, &self.widen_display_text(&shown));
        let redrawn =
            applied && self.composition.borrow().is_some() && !self.composition_end_pending.get();
        if redrawn {
            self.partial_preedit_redraw_pending.set(false);
            self.partial_preedit_redraw_retries.set(0);
        }
        redrawn
    }

    /// `target` = 選択文節の (UTF-16 開始, 長さ)。Some なら該当区間だけ太下線属性で上書きする
    /// （文節ナビゲーション）。None は従来の run_preedit と同一。
    pub(crate) fn run_preedit_with_target(
        &self,
        ctx: &ITfContext,
        text: &str,
        target: Option<(usize, usize)>,
    ) -> bool {
        // 前回確定は SetText 済みなので、EndComposition の再試行に失敗したまま同じ range を
        // preedit で上書きしない。次打鍵では累積済み text を渡して再試行できる。
        if !self.finish_pending_composition(ctx) {
            tip_log("ev=preedit_rejected reason=pending_end");
            return false;
        }
        // atom 未登録（RegisterGUID 失敗）で target を渡すと、sub-range へ atom 0
        // （TF_INVALID_GUIDATOM）を SetValue して既定下線ごと消す — 太下線を諦め区間を
        // 渡さない方が「選択文節だけ下線が無い」より良い劣化。
        let target = target.filter(|_| self.da_target_atom.get() != 0);
        let sink: ITfCompositionSink = self.to_interface();
        let session_obj: ITfEditSession = StartOrUpdatePreedit {
            context: ctx.clone(),
            text: HSTRING::from(text),
            sink,
            da_variant: self.da_variant(),
            target,
            da_target_variant: self.da_target_variant(),
            composition: Rc::clone(&self.composition),
            started: Rc::clone(&self.composition_started_signal),
            left_context_out: Rc::clone(&self.left_context),
            _guard: ComObjectGuard::new(),
        }
        .into();
        let applied = unsafe {
            // 巡4 T4: 表示系の失敗は状態破棄を伴わないため early-return 不要だが、黙らない —
            // preedit が文書へ反映されない不整合の診断用に phrSession 判定のログを残す。
            match ctx.RequestEditSession(
                self.tid.get(),
                &session_obj,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0),
            ) {
                Ok(hr) => hr.is_ok(),
                Err(_) => false,
            }
        };
        // Consume only the explicit StartComposition success signal.  Do this even when a later
        // SetText/SetSelection step makes the overall session fail: the new composition lifecycle
        // has already begun and must invalidate old callbacks/reservations.
        self.consume_started_composition();
        if !applied {
            tip_log("ev=preedit_rejected");
        }
        // 読みモニタ: preedit を書いた直後に表示を同期する（打鍵/ライブ結果/部分確定の
        // 全経路がここを通る＝フックの一点化。確定/取消系は run_preedit を通らないので
        // 各サイトが明示 hide する）。edit session 失敗時は文書の表示と乖離させない。
        if applied {
            self.update_reading_monitor(ctx);
        }
        applied
    }

    /// composition を確定文字列 `text` で確定する編集セッションを同期実行する。
    /// 巡3 P3: 戻り値はセッション確立+実行の成否。RequestEditSession は外側 HRESULT とは
    /// 別に [out] phrSession（windows-rs では Ok(hr) に載る）へ結果を返し、TF_E_LOCKED 等
    /// の失敗では CommitText が実行されない — 呼び出し側は false を見て状態破棄を止め、
    /// 確定文字の消失を防ぐ（旧実装は `let _ =` で両方捨てていた）。
    pub(crate) fn do_commit(&self, ctx: &ITfContext, text: &str) -> bool {
        // 直前の SetText は成功済み。close-only が通るまでは新しい text を同じ composition へ
        // SetText しない（二重確定・直前確定の置換を防ぐ）。
        if !self.finish_pending_composition(ctx) {
            return false;
        }
        self.pending_end_generation
            .set(self.composition_generation.get());
        let session_obj: ITfEditSession = CommitText {
            context: ctx.clone(),
            text: HSTRING::from(text),
            composition: Rc::clone(&self.composition),
            end_pending: Rc::clone(&self.composition_end_pending),
            end_context: Rc::clone(&self.composition_end_context),
            end_status: Rc::clone(&self.composition_end_status),
            end_retry_count: Rc::clone(&self.composition_end_retry_count),
            _guard: ComObjectGuard::new(),
        }
        .into();
        let inserted = match unsafe {
            ctx.RequestEditSession(
                self.tid.get(),
                &session_obj,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0),
            )
        } {
            Ok(hr) => hr.is_ok(),
            Err(_) => false,
        };
        if inserted && self.composition_end_pending.get() {
            // SetText は成功済みなので確定自体は true のまま。最初の EndComposition が一過性に
            // 失敗した場合を、caller が InputState/context を畳む前に close-only で一度回収する。
            let _ = self.finish_pending_composition(ctx);
        }
        inserted
    }

    /// TestKeyDown の結果を production state machine へ記録する。
    ///
    /// 正規の Test→Key は直列なので、slot が occupied の間は同じ署名でも別署名でも
    /// `Busy`（FALSE）を返す。Replay TRUE を返すと、A の reservation 中に来た B Test が
    /// A slot を使わないまま TRUE になり、B Key が A pair と誤結合する。
    pub(crate) fn pending_end_test_decision(
        &self,
        signature: PendingEndKeySignature,
    ) -> PendingEndTestDecision {
        let generation = self.key_pair_generation.get();
        let mut reservation = self.pending_end_test_reservation.borrow_mut();
        if !self.composition_end_pending.get() {
            // pending close が終わった後にホストが Test だけを再送することがある。旧 pair
            // はその Test の時点で期限切れとし、将来の同じキーへ持ち越さない。
            if reservation.is_occupied() {
                reservation.invalidate();
                drop(reservation);
                self.bump_key_pair_generation();
            }
            return PendingEndTestDecision::Normal;
        }
        if reservation.is_stale(generation) {
            reservation.invalidate();
        }
        if reservation.is_occupied() {
            PendingEndTestDecision::Busy
        } else if reservation.reserve(signature, generation) {
            PendingEndTestDecision::Reserve
        } else {
            // reserve が失敗するのは同一 STA で slot が再入により埋まった場合だけ。安全側に
            // 常に Busy を返し、通常述語へは落とさない。
            PendingEndTestDecision::Busy
        }
    }

    /// OnKeyDown の reservation は署名・pair generation を検証し、結果に関係なく slot を
    /// 必ず消費する。不一致/stale を保持すると将来の同じ VK/password/direct 入力を誤って
    /// 食うため、peek-and-wait は許可しない。
    pub(crate) fn take_pending_end_test(&self, signature: PendingEndKeySignature) -> bool {
        self.pending_end_test_reservation
            .borrow_mut()
            .take_if_matches(signature, self.key_pair_generation.get())
    }

    /// focus/context/activation/new-composition の lifecycle 境界で pair を破棄する。
    /// 世代を進めることで、取りこぼした古い slot が後の reservation と一致しないことも保証する。
    pub(crate) fn invalidate_pending_end_test_reservation(&self) {
        self.pending_end_test_reservation.borrow_mut().invalidate();
        self.bump_key_pair_generation();
    }

    fn bump_key_pair_generation(&self) {
        self.key_pair_generation
            .set(self.key_pair_generation.get().wrapping_add(1));
    }

    pub(crate) fn pending_end_generation_is_current(&self) -> bool {
        self.pending_end_generation.get() == self.composition_generation.get()
    }

    /// EndComposition の実試行結果を共通の production state machine へ適用する。
    /// `false` は bounded retry を残して入力を一回予約する状態、`true` は close 完了または
    /// terminal/quarantine 済みで入力を解放できる状態を表す。初回呼出しは retry_count=1
    /// で記録され、総 EndComposition 呼出し数は COMPOSITION_END_RETRY_MAX 以下になる。
    pub(crate) fn apply_pending_end_attempt(&self, status: CompositionEndStatus) -> bool {
        if !self.composition_end_pending.get() {
            return true;
        }
        self.composition_end_status.set(status);
        match status {
            CompositionEndStatus::Closed => {
                self.clear_pending_composition_end(CompositionEndStatus::Closed);
                true
            }
            CompositionEndStatus::Terminal => {
                self.abandon_pending_composition_end("terminal");
                true
            }
            CompositionEndStatus::Retryable | CompositionEndStatus::Idle => {
                let current = self.composition_end_retry_count.get();
                if let Some(next) = next_composition_end_retry(current) {
                    self.composition_end_retry_count.set(next);
                    self.composition_end_status
                        .set(CompositionEndStatus::Retryable);
                    tip_log("ev=composition_end_retry rejected");
                    false
                } else {
                    self.abandon_pending_composition_end("retry_exhausted");
                    true
                }
            }
        }
    }

    /// SetText 成功後に残った composition を本文へ触れずに閉じ直す。同期終了 callback が
    /// 先に slot を落とした場合も EndCompositionOnly が成功扱いへ収束させる。
    ///
    /// 失敗を無期限に返し続けないことが重要である。terminal/unknown context は handle を
    /// quarantine して入力を解放し、locked/synchronous は初回込み
    /// `COMPOSITION_END_RETRY_MAX` 回まで再試行する。SetText はこの経路では一度も呼ばれない。
    pub(crate) fn finish_pending_composition(&self, ctx: &ITfContext) -> bool {
        if !self.composition_end_pending.get() {
            return true;
        }
        self.composition_end_status.set(CompositionEndStatus::Idle);
        let session_obj: ITfEditSession = EndCompositionOnly {
            composition: Rc::clone(&self.composition),
            end_pending: Rc::clone(&self.composition_end_pending),
            end_context: Rc::clone(&self.composition_end_context),
            end_status: Rc::clone(&self.composition_end_status),
            end_retry_count: Rc::clone(&self.composition_end_retry_count),
            _guard: ComObjectGuard::new(),
        }
        .into();
        // pending 専用 context を正とする。次打鍵が current_context を新文書へ更新済みでも、
        // 古い composition を新文書の edit cookie で閉じようとしてはいけない。
        let request_context = self
            .composition_end_context
            .borrow()
            .clone()
            .unwrap_or_else(|| ctx.clone());
        let request_status = match unsafe {
            request_context.RequestEditSession(
                self.tid.get(),
                &session_obj,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0),
            )
        } {
            Ok(hr) if hr.is_ok() => CompositionEndStatus::Closed,
            Ok(hr) => classify_composition_end_error(hr, None),
            Err(err) => classify_composition_end_error(err.code(), None),
        };

        // DoEditSession の結果（GetRange/EndComposition）を優先し、RequestEditSession の
        // phrSession/外側 HRESULT はセッション自体が走らなかった場合の分類に使う。
        let status = match self.composition_end_status.get() {
            CompositionEndStatus::Idle => request_status,
            status => status,
        };
        // EndCompositionOnly の実結果と RequestEditSession の外側結果を共通 state
        // transition へ通す。予約 VK は transition では触らず、matching KeyDown まで残す。
        self.apply_pending_end_attempt(status)
    }

    fn clear_pending_composition_end(&self, status: CompositionEndStatus) {
        if !self.composition_end_pending.replace(false) {
            return;
        }
        *self.composition.borrow_mut() = None;
        *self.composition_end_context.borrow_mut() = None;
        self.composition_end_status.set(status);
        self.composition_end_retry_count.set(0);
        self.composition_generation
            .set(self.composition_generation.get().wrapping_add(1));
        // Pending close success/quarantine is one of the few transitions that preserves an
        // already-returned Test=TRUE pair.  Deliberately leave key-pair slot/generation untouched.
    }

    /// SetText 済み composition の close-only が終端/上限到達したときの quarantine。
    /// 物理文書側に callback が遅れて届いても、slot を空にしておけば新しい composition
    /// の入力を止めず、identity/generation check が新状態を巻き込まない。
    pub(crate) fn abandon_pending_composition_end(&self, reason: &str) {
        if !self.composition_end_pending.get() {
            return;
        }
        self.clear_pending_composition_end(CompositionEndStatus::Terminal);
        tip_log(&format!("ev=composition_end_abandon reason={reason}"));
    }

    /// composition を確定せず終了する編集セッションを同期実行する。
    /// 巡4 T4: 戻り値は CancelComposition の成否（phrSession 判定込み — do_commit と同じ規律）。
    /// 呼び出し側は false なら TIP 側状態を畳まない — セッション拒否時は文書上に composition が
    /// 残るため、 Esc は「効かなかった」扱いにしてユーザの再操作に任せる。
    /// false でも left_context/読みキャッシュの清算は行う（合成継続でも文脈汚染は防ぐ）。
    pub(crate) fn do_cancel(&self, ctx: &ITfContext) -> bool {
        // 確定文字列は既に SetText 済み。CancelComposition は range を空にしてしまうため、
        // pending_end では close-only を取消成功として扱う。
        if self.composition_end_pending.get() {
            let ok = self.finish_pending_composition(ctx);
            *self.left_context.borrow_mut() = None;
            self.monitor_committed_reading.borrow_mut().clear();
            return ok;
        }
        let session_obj: ITfEditSession = CancelComposition {
            composition: Rc::clone(&self.composition),
            _guard: ComObjectGuard::new(),
        }
        .into();
        let ok = match unsafe {
            ctx.RequestEditSession(
                self.tid.get(),
                &session_obj,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0),
            )
        } {
            Ok(hr) => hr.is_ok(),
            Err(_) => false,
        };
        // U9: 合成終了（取消）— 次 composition の再捕捉まで前文書の左文脈を残さない。
        *self.left_context.borrow_mut() = None;
        self.monitor_committed_reading.borrow_mut().clear();
        ok
    }

    /// 選択中の候補を preedit へ書き戻す。選択を動かす全経路（キー/ホスト Behavior/マウス）が
    /// 通る唯一の出口で、「インラインに見えている文字列＝Enter が確定する文字列」を保つ。
    /// 文字列は cand_state から `resolve_commit` で取る — `string_at` だと sel 範囲外時の
    /// フォールバック先が Enter（key_event_sink の候補確定）とズレて表示と確定が食い違う。
    /// `showing` を見ずに呼ぶと、候補が閉じた後に保留 flush された選択要求が composition の
    /// 無い状態で run_preedit を呼び、新規 composition をキャレット位置に開いてしまう。
    pub(crate) fn sync_preedit_to_selection(&self, ctx: &ITfContext) {
        if !self.showing.get() {
            return;
        }
        // 文節ナビゲーション中: 候補選択の変更は「選択文節の差し替え」。preedit は候補単体でなく
        // 全文節の連結で描くため専用経路へ（キー/ホスト Behavior/マウスの全選択経路がここを通る
        // ＝一点分岐で乖離しない）。
        if self.clause_nav.borrow().is_some() {
            self.sync_clause_to_selection(ctx);
            return;
        }
        // borrow は run_preedit（COM へ同期コールアウトする）より前で必ず落とす —
        // ホスト再入で drain が cand_state を borrow し直す（drain_behavior_inner 参照）。
        let pick = {
            let st = self.cand_state.borrow();
            st.resolve_commit(st.selected())
        };
        let Some((_, text)) = pick else {
            return;
        };
        // 候補確定は幅を変えない契約（should_widen_digits が source=candidate を除外）なので、
        // widen_display_text を通さない生の候補を出す。通すと「全角表示の preedit を半角で確定」
        // が候補経路で再発する（trigger_convert の候補窓オープン時と同じ理由）。
        self.run_preedit(ctx, &text);
    }

    // ---- 文節ナビゲーション（変換中の←/→。MS-IME の文節移動）----

    /// 文節ナビゲーション状態を破棄する。候補窓を閉じる/確定する全経路で呼ぶ
    /// （不変条件: clause_nav が Some ⇒ showing。残すと次に候補窓を開いたとき
    /// 選択同期/確定が文節ビューと取り違える）。
    pub(crate) fn clear_clause_nav(&self) {
        self.clause_nav.borrow_mut().take();
    }

    /// 候補表示中の←/→: 文節ナビゲーションへ入り（未開始ならエンジンが現在選択候補を種に
    /// 分解して開始）、選択文節を `offset` だけ動かす。成功なら候補窓と preedit を文節ビューへ
    /// 差し替えて true。false（旧エンジン/劣化/被覆候補無し）は呼び出し側が従来の
    /// 「確定して畳む」へ落とす。
    pub(crate) fn move_clause(&self, ctx: &ITfContext, offset: i32) -> bool {
        let base_index = self.cand_state.borrow().selected();
        let Some(view) = self.engine_move_clause(offset, base_index) else {
            return false;
        };
        self.apply_clause_view(ctx, view);
        true
    }

    /// エンジンの文節ビューを UI へ反映する: 候補窓を「選択文節の候補」へ差し替え
    /// （show が cand_state を更新＝選択の唯一の真実源を維持）、preedit を全文節の連結
    /// ＋選択文節の太下線で描き直す。
    fn apply_clause_view(&self, ctx: &ITfContext, view: ClauseViewData) {
        tip_log(&format!(
            "ev=clause_move sel={} n={} cands={}",
            view.selected,
            view.segments.len(),
            view.candidates.len()
        ));
        *self.clause_nav.borrow_mut() = Some(ClauseNav {
            segments: view.segments,
            selected: view.selected,
        });
        let anchor = self.caret_point(ctx);
        let theme = self.appearance.borrow_mut().current_theme();
        self.candidate_ui
            .borrow_mut()
            .show(&view.candidates, view.candidate_index, anchor, theme);
        self.run_clause_preedit(ctx);
    }

    /// clause_nav の内容で preedit を描く（全文節の連結＋選択文節に太下線）。
    /// 候補は選んだ幅が正なので widen は通さない（sync_preedit_to_selection と同じ理由）。
    fn run_clause_preedit(&self, ctx: &ITfContext) {
        // borrow は run_preedit（COM へ同期コールアウト）より前で必ず落とす。
        let material = {
            let nav = self.clause_nav.borrow();
            nav.as_ref().map(|n| {
                (
                    n.segments.concat(),
                    crate::input_state::clause_target_utf16(&n.segments, n.selected),
                )
            })
        };
        let Some((text, target)) = material else {
            return;
        };
        self.run_preedit_with_target(ctx, &text, Some(target));
    }

    /// 文節ナビゲーション中の候補選択変更: エンジンへ SelectClauseCandidate を送って正
    /// （確定/学習に使う Candidate）へ反映し、応答ビューで segments を更新する。劣化時は
    /// ローカル反映（表示は保つ。確定は commit_clauses の劣化枝が表示中ビューを直確定する
    /// ので表示＝確定は崩れない）。
    fn sync_clause_to_selection(&self, ctx: &ITfContext) {
        let sel = self.cand_state.borrow().selected();
        if let Some(view) = self.engine_select_clause_candidate(sel) {
            *self.clause_nav.borrow_mut() = Some(ClauseNav {
                segments: view.segments,
                selected: view.selected,
            });
        } else {
            let text = {
                let st = self.cand_state.borrow();
                st.string_at(sel)
            };
            if let Some(text) = text {
                if let Some(nav) = self.clause_nav.borrow_mut().as_mut() {
                    if nav.selected < nav.segments.len() {
                        nav.segments[nav.selected] = text;
                    }
                }
            }
        }
        self.run_clause_preedit(ctx);
    }

    /// 候補窓だけを閉じて composition を残す経路（Esc / Behavior::Abort）で、preedit を候補
    /// プレビューからライブ変換表示へ描き戻す。閉じた後の確定はライブ変換結果を確定するので、
    /// 送った先の候補を残すと `sync_preedit_to_selection` が立てた「見えている文字列＝確定する
    /// 文字列」が閉じた瞬間に崩れる（従来は常に候補 0 が残り、ライブ結果と一致していたので
    /// 見えていなかった）。
    /// 幅の規則は `sync_preedit_to_selection` と**逆**でここは `widen_display_text` を通す —
    /// 閉じた後の確定は source="live" で走り `should_widen_digits` が全角化する側だから。
    /// Why not(表示中の `live_text` をそのまま描き戻す＝エンジン往復を省く): ライブ変換 ON で
    /// 「変換キーが `arm_debounce` の 30ms 以内に来て `disarm_debounce` された」場合、`live_text`
    /// は読みのまま残るのに Enter/settle は `engine_live_convert` を先に試す。読みを描き戻すと
    /// 「かなが見えているのに漢字が確定される」ズレになる（Esc の往復 1 回より表示と確定の一致
    /// を採る）。ライブ変換 OFF ならその Enter/settle も往復しないので、下の述語が両方を止める。
    pub(crate) fn restore_live_preedit(&self, ctx: &ITfContext) {
        // ライブ変換 OFF / 表記固定中は engine のライブ変換を参照しない（VK_RETURN / settle と同じ規律）。
        let live = if self.should_consult_live_engine() {
            let seq = self.state.borrow_mut().bump_live_seq();
            // auto_commit=false: Esc は何も確定しないので、エンジンに読みを消費させてはいけない。
            self.engine_live_convert(seq, false).map(|(t, _, _)| t)
        } else {
            None
        };
        let from_engine = live.as_deref().is_some_and(|t| !t.is_empty());
        // borrow は widen_display_text/run_preedit（どちらも COM へ同期コールアウトする）より
        // 前に必ず落とす（widen_commit_text の is_direct_mode 注記と同じ理由）。
        let material = {
            let live_text = self.live_text.borrow().clone();
            let reading = self.last_reading.borrow().clone();
            preedit_after_candidates_closed(live, &live_text, &reading)
        };
        let Some(text) = material else {
            return;
        };
        if from_engine {
            // 劣化素材を置き直さないと、Esc 後にエンジンが落ちたときの直確定が描き戻す前の
            // 読みへ戻り、また表示と食い違う（`on_debounce_convert` が走ったのと同じ状態にする）。
            self.state.borrow_mut().mark_good(&text);
            *self.live_text.borrow_mut() = text.clone();
        }
        self.run_preedit(ctx, &self.widen_display_text(&text));
    }

    /// 候補表示中に選択を `delta` だけ動かす（`move_selection` が循環＝端で巻き戻る）。
    /// 選択の唯一の真実源は cand_state（`move_selection`→presenter→cand_state が更新）。
    /// `ev=candidate_move` を記録し、preedit を新しい選択候補へ描き直す。
    /// Space（前進）と上下矢印（↓=前進 / ↑=後退）で共有し、両経路が乖離しないようにする。
    /// `ctx` を引数で要求するのは、context を持たない呼び出し元が preedit 更新を伴わない
    /// 選択移動を作れないようにするため（このバグの再発防止を型で縛る）。
    pub(crate) fn move_candidate(&self, ctx: &ITfContext, delta: i32) {
        self.candidate_ui.borrow_mut().move_selection(delta);
        let sel = self.cand_state.borrow().selected();
        tip_log(&format!("ev=candidate_move sel={sel}"));
        self.sync_preedit_to_selection(ctx);
    }

    /// 読みモニタの表示状態を現在の入力状態に同期する。表示条件の唯一の真実源は
    /// reading_monitor::should_show（設定ON && composing && live && 候補窓非表示）。
    /// run_preedit 末尾の一点フック＋候補窓を閉じて composition 継続する枝から呼ぶ。
    /// 同期 read セッション 1 回ぶんのコストだが、呼び出し元は既に書き込みセッション
    /// （preedit 更新）を張った直後で相対的に安価。
    /// 外部LLM変換の待機中（preedit=🌐変換中…）も条件を満たせば表示する — 読み確認として
    /// むしろ有用で、awaiting_llm の除外条件は足さない（条件を複雑化しない — spec §表示ルール）。
    pub(crate) fn update_reading_monitor(&self, ctx: &ITfContext) {
        let visible = crate::reading_monitor::should_show(
            self.reading_monitor_enabled.get(),
            self.state.borrow().composing,
            self.live_enabled.get(),
            self.showing.get(),
        );
        let reading = self.monitor_reading_text();
        if !visible || reading.is_empty() {
            self.reading_monitor.borrow_mut().hide();
            return;
        }
        let max_chars = self.reading_monitor_max_chars.get();
        // caret_point ではなく専用照会を使う理由（ev=caret ログ量産回避）は従来と同じ。
        // 矩形が取れないフレームは None を渡し、窓側 plan_anchor が
        // 表示中=位置保持 / 非表示=無害位置 に振り分ける。
        let anchor = self
            .query_monitor_anchor_rect(ctx)
            .and_then(crate::candidate_window::caret_rect_to_anchor);
        let theme = self.appearance.borrow_mut().current_theme();
        self.reading_monitor
            .borrow_mut()
            .show_or_update(&reading, anchor, max_chars, theme);
    }

    /// 読みモニタの表示文字列（累積設定を反映）。通常更新(update_reading_monitor)と
    /// レイアウト追従(relayout_popups_on_layout)の両方から使う単一の組立。
    fn monitor_reading_text(&self) -> String {
        let max_chars = self.reading_monitor_max_chars.get();
        if self.reading_monitor_accumulate.get() {
            crate::reading_monitor::compose_monitor_text(
                &self.monitor_committed_reading.borrow(),
                &self.last_reading.borrow(),
                crate::reading_monitor::display_bound(max_chars),
            )
        } else {
            self.last_reading.borrow().clone()
        }
    }

    /// 読みモニタ用アンカー矩形（composition 先頭 → キャレットの2段試行を1セッションで）。
    /// query_caret_rect と違いログを一切出さない（打鍵ごとに走る）。
    fn query_monitor_anchor_rect(&self, ctx: &ITfContext) -> Option<RECT> {
        let out: Rc<RefCell<Option<RECT>>> = Rc::new(RefCell::new(None));
        let sess: ITfEditSession = QueryMonitorAnchorRect {
            context: ctx.clone(),
            composition: Rc::clone(&self.composition),
            out: Rc::clone(&out),
            _guard: ComObjectGuard::new(),
        }
        .into();
        unsafe {
            let _ = ctx.RequestEditSession(
                self.tid.get(),
                &sess,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READ.0),
            );
        }
        let rc = *out.borrow();
        rc
    }

    // ------------------------------------------------------------------
    // UIバグ4: ITfTextLayoutSink（スクロール・リフロー追従）
    // ------------------------------------------------------------------

    /// フォーカス document の top context を `ITfTextLayoutSink` の advise 先へ貼り替える。
    /// OnSetFocus / OnPushContext / OnPopContext の 3 点から呼ぶ（context スタックの変化は
    /// この 3 点で捕捉できる）。同一 context なら no-op。
    fn refresh_layout_sink_target(&self) {
        let mgr = self.thread_mgr.borrow().clone();
        let Some(mgr) = mgr else { return };
        let top = unsafe { mgr.GetFocus().ok() }.and_then(|doc| unsafe { doc.GetTop().ok() });
        self.set_layout_sink_context(top.as_ref());
    }

    /// context が前回と同一（COM 同一性）なら no-op、違えば unadvise→advise。None は unadvise のみ。
    /// 失敗（cast/AdviseSink 拒否）は致命でない（追従が効かないだけ＝従来と同じ挙動）。
    fn set_layout_sink_context(&self, new_ctx: Option<&ITfContext>) {
        let same = match (self.layout_sink_ctx.borrow().as_ref(), new_ctx) {
            (Some(prev), Some(next)) => com_identity_eq(prev, next),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        // 巡2 E4: context が変われば旧 context 帰属の保留セッションは無効 — pending を
        // 下ろし、世代を進べて遅延発火した旧セッションの座標適用も断つ（E1/E3 と同じ
        // 世代ガードで、投入後の focus/push/pop 切替をまたいだ汚染を防ぐ）。
        self.layout_refresh_pending.set(false);
        self.bump_layout_sink_gen();
        self.unadvise_layout_sink();
        let Some(ctx) = new_ctx else { return };
        unsafe {
            let Ok(source) = ctx.cast::<ITfSource>() else {
                return;
            };
            let layout_sink: ITfTextLayoutSink = self.to_interface();
            if let Ok(cookie) = source.AdviseSink(&ITfTextLayoutSink::IID, &layout_sink) {
                self.layout_sink_cookie.set(cookie);
            }
            let edit_sink: ITfTextEditSink = self.to_interface();
            if let Ok(cookie) = source.AdviseSink(&ITfTextEditSink::IID, &edit_sink) {
                self.text_edit_sink_cookie.set(cookie);
            }
            if self.layout_sink_cookie.get() != 0 || self.text_edit_sink_cookie.get() != 0 {
                *self.layout_sink_ctx.borrow_mut() = Some(ctx.clone());
            }
        }
    }

    /// レイアウト再照会セッションの世代を進める（Activate / Deactivate / context 負替）。
    /// 巡3 G1: 発行はプロセスグローバルな単調カウンタから — Cell のインスタンス局所値だと
    /// 同一 STA で TextService が作り直された際に新インスタンスが同じ数値を辿り、旧
    /// インスタンスの滞留セッションが世代一致で通ってしまう（popup::next_fade_timer_id
    /// と同じ発行規律）。
    /// 巡3 G3: 世代を進める地点で pending も必ず下ろす（単一チョークポイント化）。
    /// bump は「世界が変わる」ことを意味し、旧 pending は旧世代帰属なので無効。
    fn bump_layout_sink_gen(&self) {
        static ISSUER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let next = ISSUER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.layout_sink_gen.set(next);
        self.layout_refresh_pending.set(false);
    }

    /// 候補窓または読みモニタが表示中か（OnLayoutChange で再照会する価値があるかの判定）。
    fn popups_visible(&self) -> bool {
        self.showing.get() || self.reading_monitor.borrow().is_visible()
    }

    /// 非同期セッション内で取得済みのアンカーへ表示中のポップアップを再配置する。
    /// 矩形取得は既にセッション内で済んでいるため、ここでは同期 edit session を要求しない
    /// （候補列は真実源 cand_state、読みは last_reading 系から — 通常更新と同じ組立）。
    /// 巡1レビュー 8c2354e指摘5 + 巡2 F1: candidate_ui.borrow_mut().show() は BeginUIElement/
    /// UpdateUIElement でホストへ COM 再入するため、OnKeyDown/debounce/llm_poll と同じ
    /// catch_unwind + reentrancy gate（guarded）の二重保護を通す。ゲートが無いと再入した
    /// drain_behavior が outbox を消費した後で保持中 RefCell の再借用 panic を起こし、
    /// 「確定要求が消えた」不整合が ReentrancyGate の設計目的ごと無効化される。
    fn relayout_popups_on_layout(
        &self,
        caret_anchor: Option<crate::candidate_window::CaretAnchor>,
        monitor_anchor: Option<crate::candidate_window::CaretAnchor>,
    ) {
        // 巡2 F7: 握り潰しはログ付きで（catch_com と同じ規律 — 保護発動の可視性）。
        // `.is_err()` は一時値の drop order を変えるため、COM 再入境界では形を維持する。
        #[allow(clippy::redundant_pattern_matching)]
        if let Err(_) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.guarded(|| self.relayout_popups_on_layout_inner(caret_anchor, monitor_anchor))
        })) {
            tip_log("ev=panic site=relayout");
        }
    }

    fn relayout_popups_on_layout_inner(
        &self,
        caret_anchor: Option<crate::candidate_window::CaretAnchor>,
        monitor_anchor: Option<crate::candidate_window::CaretAnchor>,
    ) {
        if let Some(a) = caret_anchor {
            *self.last_valid_anchor.borrow_mut() = Some(a);
        }
        // 巡2 F7: 候補窓のアンカー未取得だけでは読みモニタ更新（composition 先頭矩形は
        // 別に取れている）をスキップしない — 早期離脱はこのブロックに閉じる。
        // 巡3 P1: anchor は if let の scrutininee で borrow() の一時 Ref が**本体終了まで
        // 延命**される（edition 2021 の if let temporary lifetime — .clone() していても
        // Ref は残る）。COM コールアウト中の再入 borrow_mut と衝突して panic → 保護の
        // 無い入口では abort になるため、先に let 束縛して文末で Ref を解放する。
        let anchor = *self.last_valid_anchor.borrow();
        if self.showing.get() {
            if let Some(anchor) = anchor {
                let items = self.cand_state.borrow().items().to_vec();
                if !items.is_empty() {
                    let selected = self.cand_state.borrow().selected();
                    let theme = self.appearance.borrow_mut().current_theme();
                    // items/selected の借用は各行の文末で解放済み。COM コールアウトを
                    // またいで保持されるのは candidate_ui の RefMut のみ（presenter の
                    // 規律上不可避）— 再入は guarded が捌く。
                    self.candidate_ui
                        .borrow_mut()
                        .show(&items, selected, anchor, theme);
                }
            }
        }
        // 読みモニタの表示条件の真実源は should_show（通常更新と同じ）。
        let visible = crate::reading_monitor::should_show(
            self.reading_monitor_enabled.get(),
            self.state.borrow().composing,
            self.live_enabled.get(),
            self.showing.get(),
        );
        let reading = self.monitor_reading_text();
        if visible && !reading.is_empty() {
            let max_chars = self.reading_monitor_max_chars.get();
            let theme = self.appearance.borrow_mut().current_theme();
            self.reading_monitor.borrow_mut().show_or_update(
                &reading,
                monitor_anchor,
                max_chars,
                theme,
            );
        }
    }

    /// キャレットアンカー（スクリーン座標）を返す。`ITfContextView::GetTextExt` で実キャレット
    /// 矩形を読み、その左下（文字を覆わない位置）＋上端（画面下端フリップ用）を返す。
    /// 取得できない場合（レイアウト未確定・view 無し・セッション拒否など）は
    /// **直近に取れた有効アンカー**を再利用し、初回（まだ一度も取れていない）だけ
    /// Win11 Input Indicator と同じ作業領域右下（harmless_anchor）へ劣化する（UIバグ5 —
    /// 旧実装の主モニタ左上 (200,200) は、失敗のたびポップアップが画面端へ跳ねる
    /// 不自然さがあった。保持は同 context 内のみで、フォーカス切替でクリア）。
    /// 候補窓（ライブ変換/再変換）とモード HUD の両方がこのアンカーを使う。
    pub(crate) fn caret_point(&self, ctx: &ITfContext) -> crate::candidate_window::CaretAnchor {
        let rect = self.query_caret_rect(ctx);
        let anchor = match rect.and_then(crate::candidate_window::caret_rect_to_anchor) {
            Some(a) => {
                *self.last_valid_anchor.borrow_mut() = Some(a);
                a
            }
            None => self.last_valid_anchor.borrow().unwrap_or_else(|| {
                let (hx, hy) = crate::popup::harmless_anchor();
                crate::candidate_window::CaretAnchor {
                    x: hx,
                    y: hy,
                    caret_top: None,
                }
            }),
        };
        // 診断: GetTextExt が実矩形を返したか／劣化したか＋最終アンカー座標。
        // イマーシブ検索面で矩形が退化していないか（自前窓の画面外配置の切り分け）を見る。
        match rect {
            Some(r) => tip_log(&format!(
                "ev=caret rect_ok=1 rc=({},{},{},{}) pt=({},{})",
                r.left, r.top, r.right, r.bottom, anchor.x, anchor.y
            )),
            None => tip_log(&format!(
                "ev=caret rect_ok=0 fallback pt=({},{})",
                anchor.x, anchor.y
            )),
        }
        anchor
    }

    /// キャレット（既定選択）のスクリーン矩形を読み取り専用同期セッションで取得する。
    /// `GetTextExt` は編集セッションの内側でしか有効な ec を持てないため、`QueryCaretRect`
    /// セッションを `TF_ES_SYNC | TF_ES_READ` で同期実行して矩形を回収する。失敗時は `None`。
    fn query_caret_rect(&self, ctx: &ITfContext) -> Option<RECT> {
        let out: Rc<RefCell<Option<RECT>>> = Rc::new(RefCell::new(None));
        let sess: ITfEditSession = QueryCaretRect {
            context: ctx.clone(),
            out: Rc::clone(&out),
            _guard: ComObjectGuard::new(),
        }
        .into();
        unsafe {
            let _ = ctx.RequestEditSession(
                self.tid.get(),
                &sess,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READ.0),
            );
        }
        let rc = *out.borrow();
        rc
    }

    /// ctx の InputScope に IS_PASSWORD が含まれるか照会する（Spec2）。
    /// `GetAppProperty(GUID_PROP_INPUTSCOPE)` → 同期読み取り edit session（`QueryInputScopes`）
    /// 内で `GetValue`(VT_UNKNOWN) → `ITfInputScope::GetInputScopes` の呼出し鎖を回す。
    /// **どの段の失敗も None**（呼び出し側が false へ倒す — 通常欄を誤って direct 化しない安全側）。
    fn query_context_is_password(&self, ctx: &ITfContext) -> Option<bool> {
        let out: Rc<RefCell<Option<bool>>> = Rc::new(RefCell::new(None));
        let sess: ITfEditSession = QueryInputScopes {
            context: ctx.clone(),
            out: Rc::clone(&out),
            _guard: ComObjectGuard::new(),
        }
        .into();
        unsafe {
            let _ = ctx.RequestEditSession(
                self.tid.get(),
                &sess,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READ.0),
            );
        }
        let v = *out.borrow();
        v
    }

    /// キーイベントの ctx がパスワード欄（または keyboard-disabled コンテキスト）かを返す
    /// （キャッシュ付き・失敗は false 側）。
    /// 照会失敗（doc ロック中の一過性失敗等）は**キャッシュしない** — false を恒久化すると
    /// パスワード欄を通常欄と誤認したまま直らない（I-3）。次のキーで再照会される。
    ///
    /// バグ#1: Chromium/Edge のパスワード欄は InputScope が IS_PASSWORD にならない
    /// （IS_PRIVATE のみ。IS_PRIVATE はシークレットモードの通常欄でも単独で立つため
    /// password の根拠にできない）。代わりに context compartment
    /// GUID_COMPARTMENT_KEYBOARD_DISABLED=1 で通知されるので、先にそちらを見る
    /// （edit session 不要で軽く、doc ロック中でも失敗しない）。
    /// compartment はフォーカス遷移なしに書き換わり得るが、Chromium はフィールド種別が
    /// 変わるたび別ドキュメントへ SetFocus し直す（tsf_bridge.cc）ため、OnSetFocus での
    /// キャッシュ無効化で追従できる。
    pub(crate) fn is_password_context(&self, ctx: &ITfContext) -> bool {
        let key = ctx.as_raw() as usize;
        if self.password_ctx_key.get() != key {
            if query_context_keyboard_disabled(ctx) {
                self.password_ctx_key.set(key);
                self.password_ctx.set(true);
                tip_log("ev=input_scope password=true source=kbd_disabled");
            } else {
                match self.query_context_is_password(ctx) {
                    Some(is_pw) => {
                        self.password_ctx_key.set(key);
                        self.password_ctx.set(is_pw);
                        tip_log(&format!("ev=input_scope password={is_pw}"));
                    }
                    None => {
                        self.password_ctx_key.set(0); // 未キャッシュのまま（次キーで再照会）
                        self.password_ctx.set(false); // 今回は安全側 false（誤 direct 化しない）
                    }
                }
            }
        }
        self.password_ctx.get()
    }

    /// thread_mgr から conversion-mode compartment を引く。失敗時 None。
    fn conversion_compartment(&self) -> Option<ITfCompartment> {
        let tm = self.thread_mgr.borrow().clone()?;
        let cm: ITfCompartmentMgr = tm.cast().ok()?;
        unsafe {
            cm.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION)
                .ok()
        }
    }

    /// 現在の conversion-mode 値（取得失敗時は NATIVE 既定＝ひらがな扱い）。
    fn conversion_mode_value(&self) -> u32 {
        let Some(c) = self.conversion_compartment() else {
            return crate::conversion_mode::CONVMODE_NATIVE;
        };
        match unsafe { c.GetValue() } {
            // conversion-mode は本来 VT_I4。未設定(VT_EMPTY)は VariantToInt32 で Ok(0) に
            // coerce されてしまうため、vt 判定込みの conversion_mode::mode_from_compartment_value
            // に委譲する（非 VT_I4/未設定なら NATIVE 既定へ）。
            Ok(v) => crate::conversion_mode::mode_from_compartment_value(&v),
            Err(_) => crate::conversion_mode::CONVMODE_NATIVE,
        }
    }

    /// SP5: 半角英数(直接入力)モードか。
    /// TIP がモードを所有しているときは langbar Cell を真実にする。Obsidian 等は
    /// `apply_default_direct` の SetValue 成功後にホストが NATIVE へ戻すため、
    /// ライブ compartment だけを見ると設定 default_direct とタスクバー A に反して
    /// ひらがな入力になる。
    pub(crate) fn is_direct_mode(&self) -> bool {
        crate::conversion_mode::effective_is_direct(
            self.direct_mode_owned.get(),
            self.langbar_is_direct.get(),
            crate::conversion_mode::is_direct(self.conversion_mode_value()),
        )
    }

    /// conversion-mode をひらがな⇄半角英数でトグルする（NATIVE ビット反転）。
    /// `ctx` は HUD をキャレット近傍へ出すための生きた context（OnPreservedKey の pic）。
    /// 取れない呼び出し元は `None` を渡す＝HUD は既定座標に出る。
    pub(crate) fn toggle_conversion_mode(&self, ctx: Option<&ITfContext>) -> bool {
        // langbar click も preserved key と同じく、以前に延期した欄依存操作より新しいユーザー意図。
        self.cancel_deferred_prediction_preserved_on_input();
        self.invalidate_prediction(crate::prediction_state::Invalidation::ModeChanged);
        if self.prediction_ghost_visible() && !self.dismiss_prediction_ghost(false) {
            // pending accept は discard へ降格済みで、ghost は選択の右側にある。mode の論理反映を
            // 遅らせると後続キーが旧モードで処理されるため、cleanup と独立に切替を進める。
            tip_log("ev=mode_toggle prediction_cleanup=pending");
        }
        // 軽微1: キー長押しのオートリピートで OnPreservedKey が連続到達しても、直近トグルから
        // MODE_TOGGLE_REPEAT_GUARD 未満なら無視する（モードが偶奇でフリッカするのを防ぐ）。
        // 兄弟の再変換が reconverting ラッチで連射を自衛しているのに倣った自衛ガード。
        let now = std::time::Instant::now();
        let elapsed = self.last_mode_toggle.get().map(|t| now.duration_since(t));
        if is_toggle_repeat(elapsed, MODE_TOGGLE_REPEAT_GUARD) {
            tip_log("ev=mode_toggle skip=repeat");
            return false;
        }
        self.last_mode_toggle.set(Some(now));
        if self.ephemeral_kana.get() {
            // ephemeral 中の明示トグルは NATIVE を反転して direct へ戻す操作ではなく、現在の
            // 一時かなを「通常かな」へ昇格するユーザー意思。langbar クリックもこの共通経路を
            // 通るため、compartment へ再書込みせず marker/表示だけを永続かなへ確定する。
            // marker=true の不変条件は logical kana（成功 enter、または native のまま exit 失敗）。
            self.ephemeral_kana.set(false);
            self.direct_mode_owned.set(true);
            tip_log("ev=mode_toggle promoted=ephemeral_kana");
            self.update_langbar_mode(false, false, ctx);
            return true;
        }
        let Some(c) = self.conversion_compartment() else {
            tip_log("ev=mode_toggle skip=no_compartment");
            return false;
        };
        let live = self.conversion_mode_value();
        let before = crate::conversion_mode::toggle_before_mode(
            self.direct_mode_owned.get(),
            self.langbar_is_direct.get(),
            live,
        );
        let next = crate::conversion_mode::toggled(before);
        let v = VARIANT::from(next as i32);
        let tid = self.tid.get();
        // 診断: SetValue の成否と、書込直後に読み戻した実値を残す（write 失敗/上書きの切り分け）。
        let set_ok = unsafe { c.SetValue(tid, &v).is_ok() };
        let after = self.conversion_mode_value();
        if !set_ok {
            // 書込みが成立していないのに目標値を内部だけ確定すると、langbar/HUD と打鍵ゲートが
            // 実 compartment から分岐する。所有権を放棄し、HRESULT 失敗後の実値へ同期する。
            self.direct_mode_owned.set(false);
            let live_direct = crate::conversion_mode::is_direct(after);
            // ephemeral からの明示トグルが失敗し native のままなら復帰 marker/表示を保つ。
            // 外部変更で既に direct なら marker だけ解消し、実値 A へ収束する。
            let retry_ephemeral = self.ephemeral_kana.get() && !live_direct;
            if self.ephemeral_kana.get() && live_direct {
                self.ephemeral_kana.set(false);
            }
            tip_log(&format!(
                "ev=mode_toggle failed before={before:#06x} next={next:#06x} after={after:#06x} tid={tid}"
            ));
            self.update_langbar_mode(live_direct, retry_ephemeral, ctx);
            return false;
        }
        self.direct_mode_owned.set(true);
        tip_log(&format!(
            "ev=mode_toggle direct={} set_ok={set_ok} before={before:#06x} next={next:#06x} after={after:#06x} tid={tid}",
            crate::conversion_mode::is_direct(next)
        ));
        // 言語バーの あ/A 表示を新モードへ更新する。
        self.update_langbar_mode(crate::conversion_mode::is_direct(next), false, ctx);
        true
    }

    /// 言語バーのモード表示を更新する。共有フラグ langbar_is_direct/langbar_ephemeral を反映し、
    /// システムの sink へ OnUpdate を投げて GetText（あ/A/あ˙）を再取得させる。sink 未 advise /
    /// 言語バー非表示なら no-op。続けてモード HUD を flash する（toggle / ephemeral の通常経路）。
    /// Activate の `apply_default_direct` だけは起動チラつき防止のため HUD を出さない
    /// （`update_langbar_mode_no_hud`）。`ctx` があれば HUD を実キャレット近傍へ、無ければ作業領域右下
    /// （無害位置）へ出す。`ephemeral`: ephemeral かなモード中（F8 等の一時トリガ中）かどうか。
    fn update_langbar_mode(&self, is_direct: bool, ephemeral: bool, ctx: Option<&ITfContext>) {
        self.update_langbar_mode_inner(is_direct, ephemeral, ctx, true);
    }

    /// `update_langbar_mode` と同じ Cell / OnUpdate だが HUD は出さない。
    /// Activate の default_direct 専用（起動時の右下フラッシュ抑制）。
    fn update_langbar_mode_no_hud(
        &self,
        is_direct: bool,
        ephemeral: bool,
        ctx: Option<&ITfContext>,
    ) {
        self.update_langbar_mode_inner(is_direct, ephemeral, ctx, false);
    }

    fn update_langbar_mode_inner(
        &self,
        is_direct: bool,
        ephemeral: bool,
        ctx: Option<&ITfContext>,
        flash_hud: bool,
    ) {
        self.langbar_is_direct.set(is_direct);
        self.langbar_ephemeral.set(ephemeral);
        // 巡4 T6: if let の一時 Ref が OnUpdate コールアウト中も延命されるため先に束縛
        // （relayout と同種の再入 borrow_mut 衝突対策）。
        let sink = self.langbar_sink.borrow().clone();
        if let Some(sink) = sink {
            unsafe {
                let _ = sink.OnUpdate(TF_LBI_TEXT | TF_LBI_STATUS | TF_LBI_ICON);
            }
        }
        if flash_hud {
            // SP5/US: モード切替を あ/A の HUD でキャレット近傍に一瞬表示する（Win11 では langbar が
            // 出ないため）。生きた context があれば GetTextExt で実キャレット位置に出す。
            // ctx 無し（Activate/Deactivate/focus 切替/langbar クリック等）はキャレットを持たないため、
            // 従来 (200,200) の画面左上固定は不自然だった。Win11 Input Indicator と同じ作業領域右下へ。
            let (x, y) = match ctx {
                Some(ctx) => {
                    let a = self.caret_point(ctx);
                    (a.x, a.y)
                }
                None => crate::popup::harmless_anchor(),
            };
            // Task 7: 表示のたびに settings の mtime とダークモードを再評価した Theme を渡す
            // （設定変更・OS のライト/ダーク切替が次の flash から再起動なしで反映される）。
            let theme = self.appearance.borrow_mut().current_theme();
            self.mode_hud
                .borrow_mut()
                .flash(is_direct, ephemeral, x, y, theme);
        }
    }

    /// SP7: 活性化時に conversion-mode を半角英数(直接入力)へ初期化する（設定 default_direct=true）。
    /// NATIVE と FULLSHAPE を落として半角を保証する（ROMAN 等は保存）。
    /// 成否を返す: 成功 = compartment が取れた、かつ（値が既に direct、または SetValue 成功）
    /// （成否判定は conversion_mode::default_direct_success）。失敗（compartment 無い/
    /// SetValue 失敗）は direct_mode_owned を立てずに langbar Cell をライブ値へ戻し
    /// （HUD なし）false を返す＝Activate は default_direct_applied を立てず、次回
    /// Activate で再試行する。成功後の所有権挙動（owned=true で Cell を真実にする＝
    /// ホストの NATIVE 戻しに合わせない）は意図的に従来どおり。
    /// SetValue は値が変わるときだけ。表示更新は Cell が目標と不一致のときだけ（再描画チラつき防止）。
    /// Activate 内の tid/thread_mgr セット後に1度だけ呼ぶ＝以後のユーザ手動トグルは上書きしない。
    pub(crate) fn apply_default_direct(&self) -> bool {
        let c = self.conversion_compartment();
        let current = self.conversion_mode_value();
        let next = crate::conversion_mode::to_direct(current);
        let needs_write = next != current;
        let write_ok = match (&c, needs_write) {
            (Some(c), true) => {
                let v = VARIANT::from(next as i32);
                let tid = self.tid.get();
                let ok = unsafe { c.SetValue(tid, &v).is_ok() };
                tip_log(&format!("ev=default_direct applied ok={ok}"));
                ok
            }
            (Some(_), false) => {
                tip_log("ev=default_direct skip=already_direct");
                true
            }
            (None, _) => {
                tip_log("ev=default_direct fail=no_compartment");
                false
            }
        };
        if !crate::conversion_mode::default_direct_success(c.is_some(), needs_write, write_ok) {
            return self.default_direct_fail();
        }
        self.direct_mode_owned.set(true);
        let target = crate::conversion_mode::is_direct(next);
        if crate::conversion_mode::should_notify_langbar(self.langbar_is_direct.get(), target) {
            self.update_langbar_mode_no_hud(target, false, None);
        }
        true
    }

    /// apply_default_direct の失敗後始末: モード所有権を放棄し（direct_mode_owned=false ＝
    /// is_direct_mode・打鍵ゲートは live 値を追従）、langbar Cell/表示を実 compartment の
    /// ライブ値へ無条件で戻す（AddItem 前の楽観 A プリセットを残さない。HUD は出さない）。
    fn default_direct_fail(&self) -> bool {
        self.direct_mode_owned.set(false);
        let live = self.conversion_mode_value();
        tip_log(&format!("ev=default_direct rollback live={live:#06x}"));
        self.update_langbar_mode_no_hud(crate::conversion_mode::is_direct(live), false, None);
        false
    }

    /// ephemeral かなモード開始: direct 中にトリガキー（既定 F8）が来たら compartment を
    /// NATIVE へ SetValue（現値に立てる。FULLSHAPE 等は保存）＋ `ephemeral_kana` フラグを立てる。
    /// `toggle_conversion_mode` の repeat guard は経由しない専用経路（設計ロック: repeat guard の非流用）。
    pub(crate) fn enter_ephemeral_kana(&self, ctx: Option<&ITfContext>) {
        let Some(c) = self.conversion_compartment() else {
            tip_log("ev=ephemeral_enter skip=no_compartment");
            return;
        };
        // 現値に NATIVE を立てる（かな入力へ）。FULLSHAPE 等は保存。
        let before = self.conversion_mode_value();
        let next = before | crate::conversion_mode::CONVMODE_NATIVE;
        let v = VARIANT::from(next as i32);
        let ok = unsafe { c.SetValue(self.tid.get(), &v).is_ok() };
        let after = self.conversion_mode_value();
        if !ok {
            // 開始に失敗した打鍵を ephemeral 成功扱いにしない。以後は実 compartment を正とする。
            self.ephemeral_kana.set(false);
            self.direct_mode_owned.set(false);
            tip_log(&format!(
                "ev=ephemeral_enter failed next={next:#06x} after={after:#06x}"
            ));
            self.update_langbar_mode(crate::conversion_mode::is_direct(after), false, ctx);
            return;
        }
        self.ephemeral_kana.set(true);
        self.direct_mode_owned.set(true);
        tip_log(&format!(
            "ev=ephemeral_enter set_ok={ok} next={next:#06x} after={after:#06x}"
        ));
        self.update_langbar_mode(false, true, ctx);
    }

    /// ephemeral かなモード復帰: `ephemeral_kana` が立っているときだけ compartment を
    /// direct へ SetValue ＋ フラグを落とす。立っていなければ no-op（畳んで確定/Esc/フォーカス喪失
    /// 等の全経路から冪等に呼べる。全経路配線は Task 3）。
    pub(crate) fn exit_ephemeral_to_direct(&self, ctx: Option<&ITfContext>) {
        if !self.ephemeral_kana.get() {
            return;
        }
        if let Some(c) = self.conversion_compartment() {
            let next = crate::conversion_mode::to_direct(self.conversion_mode_value());
            let v = VARIANT::from(next as i32);
            let ok = unsafe { c.SetValue(self.tid.get(), &v).is_ok() };
            let after = self.conversion_mode_value();
            if !ok {
                // 失敗後も native なら復帰要求を保留し、次の冪等な exit 呼出しで再試行する。
                // 実値が既に direct なら外部変更で目的は達成済みなので保留だけ解消する。
                let live_direct = crate::conversion_mode::is_direct(after);
                self.ephemeral_kana.set(!live_direct);
                self.direct_mode_owned.set(false);
                tip_log(&format!(
                    "ev=ephemeral_exit failed next={next:#06x} after={after:#06x} retry={}",
                    !live_direct
                ));
                self.update_langbar_mode(live_direct, !live_direct, ctx);
                return;
            }
            self.ephemeral_kana.set(false);
            self.direct_mode_owned.set(true);
            tip_log(&format!(
                "ev=ephemeral_exit set_ok={ok} next={next:#06x} after={after:#06x}"
            ));
            self.update_langbar_mode(true, false, ctx);
        } else {
            // 保留を落とすと direct 復帰を二度と試せないため、取得不能時は marker を維持する。
            tip_log("ev=ephemeral_exit no_compartment(retry)");
        }
    }

    /// 再変換: 直前ラテン列(or 選択)を掴んで composition 化し、g1 リプレイで候補を出す。
    pub(crate) fn start_reconvert(&self, ctx: &ITfContext) {
        if !self.finish_pending_composition(ctx) {
            tip_log("ev=reconvert_skip reason=pending_end");
            return;
        }
        if self.reconverting.get() {
            return;
        }
        // 深層防御: 開始時に必ずクリア(採取できない経路で前回の読みと誤ペアにしない)。
        self.reconvert_reading.borrow_mut().clear();
        // 既に composition が開いている（native の打ちかけ等）なら再変換しない。
        // ReconvertStart は無条件で StartComposition しスロットを上書きするため、
        // ここで弾かないと既存 composition を EndComposition せず孤児化させてしまう。
        if self.composition.borrow().is_some() {
            return;
        }
        // 1) range 読み戻し＋非空 StartComposition（読んだラテンを out へ）。
        let out: Rc<RefCell<ReconvertCapture>> = Rc::new(RefCell::new(ReconvertCapture::default()));
        let sink: ITfCompositionSink = self.to_interface();
        let sess: ITfEditSession = ReconvertStart {
            context: ctx.clone(),
            sink,
            composition: Rc::clone(&self.composition),
            started: Rc::clone(&self.composition_started_signal),
            out: Rc::clone(&out),
            left_context_out: Rc::clone(&self.left_context),
            _guard: ComObjectGuard::new(),
        }
        .into();
        unsafe {
            let _ = ctx.RequestEditSession(
                self.tid.get(),
                &sess,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0),
            );
        }
        self.consume_started_composition();
        let cap = out.borrow().clone();
        match cap.kind {
            ReconvertKind::None => return, // 対象なし（従来の早期 return）
            ReconvertKind::NonKana => {
                // 漢字/混在: 合成していない。無害に離脱。
                tip_log("ev=reconvert_skip reason=non_kana");
                return;
            }
            ReconvertKind::Latin | ReconvertKind::Surface => {}
        }
        let text = cap.text.clone();
        *self.reconvert_original.borrow_mut() = text.clone();

        // 新セッションを張り直してから種別ごとに変換する（セッション不変条件）。
        self.ensure_engine();
        self.engine_end_session();
        self.ensure_session();
        let cands = match cap.kind {
            ReconvertKind::Latin => {
                // 生ラテン列は engine へ渡す前に `-`→`ー` へ写す（nospacekey roman2kana は長音を
                // 欠くため。`wa-rudo`→`waーrudo`→わーるど→ワールド）。reconvert_original は上で
                // 生テキストのまま保持済み — Esc 復元は元の見た目（`wa-rudo`）へ戻す。
                // engine_insert が文字列単位になったので 1 往復でリプレイする（挙動は逐次と等価）。
                let reading = crate::input_state::latin_reconvert_reading(&text);
                // かな読みは insert 応答の Reading から採取する(RecordCorrection のキー)。
                // latin_reconvert_reading の戻り値は ASCII ローマ字で、かな化はエンジン側
                // roman2kana にしか無いため TIP では作れない。
                let kana = self.engine_insert(&reading, InsertStyle::Kana);
                *self.reconvert_reading.borrow_mut() = kana.unwrap_or_default();
                self.engine_convert().unwrap_or_default()
            }
            ReconvertKind::Surface => {
                *self.reconvert_reading.borrow_mut() = text.clone();
                self.engine_reconvert_surface(&text).unwrap_or_default()
            }
            ReconvertKind::None | ReconvertKind::NonKana => unreachable!(),
        };
        if cands.is_empty() {
            self.cancel_reconvert(ctx);
            return;
        }
        self.show_reconvert_candidates(ctx, &cands);
        // ev ログは呼び出し側で各自出す（I-3）。本文は長さだけを残す。
        let kind_str = if matches!(cap.kind, ReconvertKind::Surface) {
            "surface"
        } else {
            "latin"
        };
        tip_log(&format!(
            "ev=reconvert_shown n={} kind={} chars={}",
            cands.len(),
            kind_str,
            text.chars().count()
        ));
    }

    /// 再変換/確定取消の共有尾部: 先頭候補で preedit を張り、候補窓を表示し、`reconverting=true`
    /// にして current_context/テーマをセットする。**ev ログは含めない**（確定本文がログへ漏れるのを
    /// 構造で防ぐ — I-3。呼び出し側 start_reconvert / start_commit_undo が各自の ev を出す）。
    fn show_reconvert_candidates(&self, ctx: &ITfContext, cands: &[String]) {
        // 読みモニタ: showing を run_preedit より先に立てる。run_preedit 末尾の
        // update_reading_monitor が candidate_visible=false の一瞬を見て誤表示
        // （直前入力の残骸 last_reading をフラッシュ）するのを防ぐ。フラグは
        // key 処理経路からしか読まれないため、この順序入れ替えに他の観測者はいない。
        self.showing.set(true);
        self.reconverting.set(true);
        self.run_preedit(ctx, &cands[0]);
        *self.current_context.borrow_mut() = Some(ctx.clone());
        let anchor = self.caret_point(ctx);
        // Task 7: 表示ごとに settings/ダークモードを再評価した Theme を渡す。
        let theme = self.appearance.borrow_mut().current_theme();
        self.candidate_ui.borrow_mut().show(cands, 0, anchor, theme);
    }

    /// 再変換取消: 元ラテンを復元して composition を閉じ、状態を片付ける。
    /// `ctx` は呼び出し元の生きた context を直接使う（変換失敗の早期取消では
    /// `current_context` がまだ未設定なため、ここで current_context に依存しない）。
    /// 戻り値（do_cancel と同じ規律）: false = RequestEditSession の外側失敗 or phrSession
    /// 拒否（RestoreText が走っていない＝文書は未復元。状態もラッチも畳んでいない）。
    /// true は RestoreText 成功とその後始末を完了した後にのみ返す。呼び出し側は false を
    /// 「取消に失敗した」と扱い、Deactivate preflight は中断、キー経路は再操作に任せる。
    pub(crate) fn cancel_reconvert(&self, ctx: &ITfContext) -> bool {
        let original = self.reconvert_original.borrow().clone();
        let sess: ITfEditSession = RestoreText {
            context: ctx.clone(),
            text: HSTRING::from(original.as_str()),
            composition: Rc::clone(&self.composition),
            _guard: ComObjectGuard::new(),
        }
        .into();
        // 巡3 P3: RestoreText が拒否されたら状態を畳まない — 元テキストが文書へ復元されない
        // のに再変換元を消すと選択文字列が消失する（phrSession も判定。F3 と同じ規律）。
        match unsafe {
            ctx.RequestEditSession(
                self.tid.get(),
                &sess,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0),
            )
        } {
            Ok(hr) if hr.is_ok() => {}
            _ => {
                tip_log("ev=reconvert_cancel_rejected");
                return false;
            }
        }
        self.engine_end_session();
        self.reconverting.set(false);
        self.reconvert_original.borrow_mut().clear();
        self.reconvert_reading.borrow_mut().clear();
        self.candidate_ui.borrow_mut().hide();
        self.reading_monitor.borrow_mut().hide();
        self.showing.set(false);
        self.clear_clause_nav();
        *self.current_context.borrow_mut() = None;
        // U9: 第4の合成終了経路（RestoreText）。ReconvertStart が書いた文書本文の左文脈を
        // ここで残すと、次 composition の edit session 拒否時に別文書の要求（特に外部 LLM）へ
        // 漏れる — do_cancel / commit_and_reset / reset_abandoned_composition と同じ規律で必ず消す
        // （最終レビュー Important-1）。
        *self.left_context.borrow_mut() = None;
        self.monitor_committed_reading.borrow_mut().clear();
        tip_log("ev=reconvert_cancel");
        true
    }

    /// UU-4: ホストへ同期コールアウトしうる COM 区間（キー入口・タイマ発火など）をこれで包む。
    /// 区間中はゲートを立て、ホストが Behavior 経由で再入して `drain_behavior` を呼んでも借用
    /// 衝突 panic を起こさず保留させる。区間を抜けて借用が解放された安全点で、保留された
    /// Behavior を遅延 flush（0ms タイマ）で回収する。ネスト時は最外区間だけが予約する。
    /// 巡3 P7/P8: 即時 flush は非同期 READ edit session の内側でも走り、TSF の規律
    /// （READ 保持中の同期 READWRITE 要求は TF_E_SYNCHRONOUS 拒否）で do_commit が落ちるため
    /// 遅延発火（メッセージループの次周=セッション外）に統一する。
    /// 巡4 T1/T5: 予約は pending があるときだけ（無条件予約は「timer→drain(no-op)→再予約」の
    /// 恒常 churn を生む）。`f` の panic 時も catch_unwind して予約してから resume する —
    /// panic 経路で予約が飛ぶと保留が次の外部イベントまで宙吊りになる。
    pub(crate) fn guarded<T>(&self, f: impl FnOnce() -> T) -> T {
        // enter() は直前の値（=区間の中だったか）を返す。prev==false が最外区間。
        let prev = self.reentrancy.enter();
        let flag = InOperationGuard {
            gate: &self.reentrancy,
            prev,
        };
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        drop(flag); // 区間フラグを復元してから（借用未保持の安全点で）flush を予約する
        if !prev && self.reentrancy.has_pending() {
            // 最外区間だけが flush を予約する（ネストした内側 guarded は外側に任せる）。
            self.schedule_behavior_flush();
        }
        match r {
            Ok(v) => v,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// 巡3 P7/P8 + 巡4 T1: 保留 Behavior の回収を 0ms スレッドタイマで遅延発火する。タイマ
    /// proc は現在のコールバック（relayout なら DoEditSession=READ ロック保持中）が完了してから
    /// メッセージループで走るため、drain 内の同期 edit session 要求がセッション外の安全点で届く。
    /// 巡4 T1: pending が立っているときだけ武装し、タイマID を保持して proc 側で照合する
    /// （多重武装の防止と、Deactivate 後の旧タイマ発火切り捨て — fire_reload_retry と同型）。
    /// SetTimer 失敗（資源枯渇。稀）時の即時 flush は READ セッション内で同期要求を出して
    /// TF_E_SYNCHRONOUS 拒否を再発させるため行わない — ログだけ残し、次の入口で回収に任せる。
    fn schedule_behavior_flush(&self) {
        if self.behavior_flush_timer.get() != 0 {
            return; // 既に武装済み — 発火時のループでまとめて回収される。
        }
        if !self.reentrancy.has_pending() {
            return; // 回収すべき保留がない — 武装しない（恒常 churn 防止）。
        }
        unsafe {
            let id = SetTimer(None, 0, 0, Some(behavior_flush_timer_proc));
            if id == 0 {
                tip_log("ev=behavior_flush_arm_failed");
                return;
            }
            self.behavior_flush_timer.set(id);
        }
    }

    /// UU-4: 保留された Behavior 要求を、借用未保持の安全点で outbox が空になるまで実行する。
    /// 呼び出し元は behavior_flush_timer_proc（セッション外のタイマ発火。巡6 C-1 で復帰 —
    /// これが take_pending による pending 清算の唯一の経路）。
    /// `drain_behavior_inner` 実行中の再入も（ゲートにより）保留されるため、ループで回収する。
    /// 巡3 P5: 上限付き — drain 内の COM コールアウトでホストが毎周再入する異常時に無限
    /// ループしない。打ち切り時の消費済み要求は必ず実行して抜け、以降の再入は次の
    /// 発火で回収される。
    pub(crate) fn flush_pending_behavior(&self) {
        // 巡7 M-2: 関数内契約 — 本体の take_pending ループは in_operation を見ないため、
        // 区間中に呼ばれると保留を下ろして強制 drain する（保護のバイパス）。proc の
        // 事前判定に頼らずここでも守り、区間中の呼び出しは保留を残して即返す
        // （回収は最外 guarded 脱出／外側 flush ループ／drain_behavior 尾部のいずれか）。
        if self.reentrancy.in_operation() {
            return;
        }
        let mut rounds = 0u32;
        while self.reentrancy.take_pending() {
            rounds += 1;
            if rounds > 8 {
                tip_log("ev=behavior_flush_truncated");
                self.drain_behavior_inner();
                return;
            }
            self.drain_behavior_inner();
        }
    }

    /// SP6a: UIElement Behavior(マウス/タッチ)発の確定/取消を実行する。notify→TLS 経由の入口。
    /// UU-4: TS 操作中（借用保持中）にホストが再入して呼んだ場合は、outbox を消費せず保留に
    /// 回して panic を避ける。借用未保持（純粋なマウス発など）なら即座に処理する。
    pub(crate) fn drain_behavior(&self) {
        // ゲートが「保留（区間中）」を指示したら outbox は消費せず、保留フラグだけ立てて返す
        // （区間離脱後の安全点＝guarded の flush で処理＝確定ロスト防止）。
        let has_request = self.behavior_outbox.borrow().is_some() || self.selection_dirty.get();
        if self.reentrancy.signal_reentry(has_request) {
            return;
        }
        // 借用未保持のトップレベル。inner が区間フラグを立てるので、その中の再入は保留され、
        // 続く flush 予約で（メッセージループの次周に）回収する。
        // 巡7 M-2: inner の panic がこの尾部を飛ばすと、直前の再入で立った pending の
        // 予約者がいなくなり次の外部イベントまで宙吊りになる — guarded と同じ
        // catch→予約→resume 構造にする（notify 呼び出し側は握り潰すのでここが最後の窓）。
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.drain_behavior_inner();
        }));
        // 巡4 T1: 予約は pending があるときだけ（drain 後に保留が残っていなければ再武装しない
        // — 「timer→drain(no-op)→再予約」の恒常 churn を断つ）。
        if self.reentrancy.has_pending() {
            self.schedule_behavior_flush();
        }
        if let Err(payload) = r {
            std::panic::resume_unwind(payload);
        }
    }

    /// drain の実体。区間フラグを立てて再入を保留させる。
    /// outbox と選択フラグを**先に**取り出してから作用する（borrow 競合・再入防止）。
    /// 選択移動=preedit を選択候補へ揃える / Finalize=現在選択候補を確定 / Abort=取消。
    /// いずれも矢印キー・Enter・Esc と同じ既存経路を再利用する。
    /// 生きた context が無い（current_context=None）なら何もしない（劣化。panic させない）。
    fn drain_behavior_inner(&self) {
        let prev = self.reentrancy.enter();
        let _flag = InOperationGuard {
            gate: &self.reentrancy,
            prev,
        };
        let action = self.behavior_outbox.borrow_mut().take();
        let sync_selection = self.selection_dirty.replace(false);
        if action.is_none() && !sync_selection {
            return;
        }
        let Some(ctx) = self.current_context.borrow().clone() else {
            return;
        };
        // 選択同期は action より先。Finalize と同時に届いた場合でも「見えている文字列を確定する」
        // 順序になり、確定直前だけ preedit が古いまま、という観測可能な隙間を作らない。
        if sync_selection {
            self.sync_preedit_to_selection(&ctx);
        }
        let Some(action) = action else {
            return;
        };
        match action {
            BehaviorAction::Finalize => {
                // UU-4(#4): 保留された Finalize が「候補が既に閉じられた後」（例: Esc で hide したが
                // composition は残る経路）に flush されると、ユーザが破棄したはずの候補を誤確定しうる。
                // 候補表示中(showing)のときだけ確定する（cand_state は hide でクリアされないため）。
                if !self.showing.get() {
                    return;
                }
                // 文節ナビゲーション中のホスト確定は全文節の確定（Enter と同じ分岐 —
                // cand_state の index は「選択文節の候補」の添字であり、エンジンの
                // 全文候補キャッシュへ Commit{index} すると別候補を確定してしまう）。
                if self.clause_nav.borrow().is_some() {
                    self.commit_clauses(&ctx);
                    return;
                }
                // Enter（候補表示中）と同一: 選択中の候補を commit_candidate で確定する
                // （前方一致候補なら部分確定して残り読みを継続）。選択 index は cand_state
                // （＝選択の唯一の真実源。キーボードも Behavior::SetSelection もここを更新）から読む。
                let pick = {
                    let st = self.cand_state.borrow();
                    st.resolve_commit(st.selected())
                };
                let Some((index, text)) = pick else {
                    return;
                }; // 候補空
                self.commit_candidate(&ctx, index, &text);
            }
            BehaviorAction::Abort => {
                // Esc と同一の優先順位: 再変換中→取消 / 候補表示中→候補を閉じる /
                // composition 中→取消。どれにも当たらなければ何もしない。
                if self.reconverting.get() {
                    self.cancel_reconvert(&ctx);
                } else if self.showing.get() {
                    self.candidate_ui.borrow_mut().hide();
                    self.showing.set(false);
                    self.clear_clause_nav();
                    tip_log("ev=candidates_hidden");
                    self.restore_live_preedit(&ctx);
                } else if self.state.borrow().composing {
                    self.disarm_debounce();
                    // 巡4 T4: 拒否時は状態を畳まない（Esc と同じ規律）。
                    if !self.do_cancel(&ctx) {
                        tip_log("ev=cancel_rejected source=behavior_abort");
                        return;
                    }
                    self.state.borrow_mut().on_escape();
                    self.engine_end_session();
                    self.live_text.borrow_mut().clear();
                    *self.current_context.borrow_mut() = None;
                }
            }
        }
    }
}

/// UU-4: ホスト再入を借用未保持の安全点まで遅延させる門（COM 非依存＝単体テスト可能）。
///
/// 候補 UI 更新（presenter の Begin/UpdateUIElement）中にホストが Behavior 経由で SetSelection/
/// Finalize/Abort を **同期再入** すると、TS 側が保持中の RefCell（candidate_ui/cand_state/state）を
/// drain が再度 borrow_mut して panic → notify の catch_unwind に握り潰され outbox は消費済みなのに
/// 確定が実行されない不整合になる。このゲートは「操作区間中の再入」を検知して要求を outbox に
/// 残したまま保留し、区間を抜けた安全点で flush させることで panic と確定ロストの双方を防ぐ。
pub(crate) struct ReentrancyGate {
    /// 借用を保持しつつホストへ同期コールアウトしうる区間の中なら true。
    in_operation: Cell<bool>,
    /// 区間中に届いた（outbox に要求のある）再入を「保留」と記録。安全点で読み出して flush。
    pending: Cell<bool>,
}

impl ReentrancyGate {
    pub(crate) fn new() -> Self {
        Self {
            in_operation: Cell::new(false),
            pending: Cell::new(false),
        }
    }
    /// 現在、操作区間の中か（ゲートの状態を検査するアクセサ。単体テストで区間フラグの
    /// 遷移を確認するのに使う。production では guarded が enter/exit の戻り値で判定し、
    /// behavior_flush_timer_proc が発火可否の判定に、flush_pending_behavior が区間中
    /// 呼び出しの拒否（関数内契約）に使う）。
    pub(crate) fn in_operation(&self) -> bool {
        self.in_operation.get()
    }
    /// 区間に入る。戻り値（直前の値）を `exit` へ渡してネスト復元する。
    pub(crate) fn enter(&self) -> bool {
        self.in_operation.replace(true)
    }
    /// 区間を抜ける（`enter` の戻り値を渡す）。最外なら false に戻る。
    pub(crate) fn exit(&self, prev: bool) {
        self.in_operation.set(prev);
    }
    /// ホスト再入シグナル。区間中なら（要求があれば）保留を記録して true（＝呼び出し側は
    /// 即実行せず戻る）を返す。区間外なら false（＝いま実行してよい）。
    pub(crate) fn signal_reentry(&self, has_action: bool) -> bool {
        if self.in_operation.get() {
            if has_action {
                self.pending.set(true);
            }
            true
        } else {
            false
        }
    }
    /// 保留を1回分読み取ってクリアする（あったら true）。flush ループの回し手。
    pub(crate) fn take_pending(&self) -> bool {
        self.pending.replace(false)
    }
    /// 保留があるか（消費しない読み出し）。遅延 flush の再武装判断に使う — pending が無い
    /// 要求の無駄なタイマ発火（恒常 churn）を防ぐ（巡4 T1）。
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.get()
    }
}

/// UU-4: 区間フラグを立て、抜けたら（panic 時も Drop で）元の値へ戻す RAII。
/// ネストに耐えるよう「元の値」を保存して復元する（最外だけが false に戻る）。
struct InOperationGuard<'a> {
    gate: &'a ReentrancyGate,
    prev: bool,
}
impl Drop for InOperationGuard<'_> {
    fn drop(&mut self) {
        self.gate.exit(self.prev);
    }
}

/// SP6a: Behavior(マウス/タッチ)発の確定/取消を STA 自己ポインタ経由で実行する。
/// presenter の notify クロージャから呼ばれる（self を捕捉しないための間接呼び出し）。
pub(crate) fn drain_behavior_via_tls() {
    BEHAVIOR_TS.with(|c| {
        let p = c.get();
        if !p.is_null() {
            unsafe {
                (*p).drain_behavior();
            }
        }
    });
}

/// 巡3 P7/P8 + 巡4 T1: schedule_behavior_flush が武装する 0ms タイマの発火口。現在のメッセージ
/// （relayout なら DoEditSession=READ ロック保持中）が完了した後のメッセージループで呼ば
/// れるため、drain 内の同期 edit session 要求が TSF の規律に抵触しない安全点になる。
/// タイマID照合で現在インスタンスの武装した物だけを処理し（多重武装時代の残り・旧インスタンス
/// の発火は切り捨て）、flush 後に保留が残っていなければ再武装しない（恒常 churn 防止）。
unsafe extern "system" fn behavior_flush_timer_proc(_hwnd: HWND, _msg: u32, id: usize, _time: u32) {
    let _ = KillTimer(None, id);
    let ptr = BEHAVIOR_TS.with(|c| c.get());
    if ptr.is_null() {
        return;
    }
    let ts: &TextService_Impl = unsafe { &*ptr };
    if ts.behavior_flush_timer.get() != id {
        return; // 自分の武装したタイマではない（差し替え後の旧発火等）— 掃除だけ済ませ無視。
    }
    ts.behavior_flush_timer.set(0);
    // 巡6 C-1/I-1 + 巡8: 発火時にガード区間中（ネストしたメッセージポンプで
    // WM_TIMER が流れ込んだ場合）なら実行も再武装もしない。flush_pending_behavior は
    // 巡6当時 in_operation を見ず take_pending で保留を下ろすため、区間中に呼ぶと
    // ReentrancyGate の保護をバイパスする強制 drain になった — 巡4の proc 直呼びが
    // そうだった（巡5-B 指摘1）。巡8 で入口ガードを関数内契約に移した今、proc 側の
    // 判定が守るのは本体でなく proc 尾部 — flush が区間中に即返しても、その後の
    // has_pending が 0ms タイマをポンプ内で再武装してライブロックするためである。
    // drain_behavior（notify 入口）への付け替えは take_pending の呼び出し元を消して
    // 保留を誰も下ろせなくする（巡6 C-1）。再武装しない以上、保留の回収者は区間を
    // 立てた主体が担う — guarded 由来は最外脱出時の予約（!prev && has_pending）、
    // flush ループ内の inner 由来は外側の while take_pending、drain_behavior 経由の
    // inner 由来はその尾部の再武装、打ち切り／flush 中 panic で残った pending は
    // proc 尾部の再武装が拾う。
    if ts.reentrancy.in_operation() {
        return;
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // セッション外の安全点 — 保留を take_pending ループで回収する（上限付き）。
        // 巡6 C-1: pending を下ろすのは flush_pending_behavior の take_pending だけ。
        // drain_behavior への付け替えは清算者を孤立させ、outbox 空の no-op 発火が
        // pending 残存のまま永久に再武装する恒常 churn を起こす。
        ts.flush_pending_behavior();
    }));
    // 巡5 GLM M-3: 打ち切り（8周上限）や flush 中の panic で pending が残っていれば
    // 再武装（宙吊り防止）。通常終了時は pending が下りているので no-op で止まる。
    if ts.reentrancy.has_pending() {
        ts.schedule_behavior_flush();
    }
}

/// UIバグ4: RefreshAnchorOnLayout（非同期 READ セッション）の DoEditSession から呼ばれる
/// 再配置の適用入口。LAYOUT_TS 経由で TextService を引く（Activate で set / Drop で null —
/// DEBOUNCE_TS/BEHAVIOR_TS と同型の STA 生ポインタ規律）。
/// 巡2 E1/E3 + 巡3 G3: gen はセッション投入時点の世代（プロセスグローバル発行）。投入後に
/// Activate/Deactivate/context 負替が起きていたら（≠現在世代）旧 context の座標で現在の
/// 表示を汚さない。pending の解除も自世代のみ — 旧世代の完了が現世代の coalescing フラグを
/// 下ろすと「1本にまとめる」が崩れる（旧 pending は bump 時点で必ず清算済み）。
pub(crate) fn layout_refresh_apply(
    gen: u64,
    caret_anchor: Option<crate::candidate_window::CaretAnchor>,
    monitor_anchor: Option<crate::candidate_window::CaretAnchor>,
) {
    LAYOUT_TS.with(|p| {
        let ptr = p.get();
        if ptr.is_null() {
            return;
        }
        let ts = unsafe { &*ptr };
        if ts.layout_sink_gen.get() != gen {
            tip_log("ev=layout_refresh skipped=stale_gen");
            return;
        }
        ts.layout_refresh_pending.set(false);
        ts.relayout_popups_on_layout(caret_anchor, monitor_anchor);
    });
}

/// 指定パイプ名を引数に、`NospacekeyEngineHost.exe` を**コンソール無し**で起動する。
/// `CREATE_NO_WINDOW` を付けるので可視ウィンドウは出ない（切替時の大量ウィンドウ対策）。
pub(crate) fn spawn_engine_hidden(
    exe: &std::path::Path,
    pipe: &str,
    env: &[(String, String)],
) -> Option<std::process::Child> {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000; // 親ホストの Job に巻き込まれて道連れにされないため
    let build = |flags: u32| {
        let mut cmd = std::process::Command::new(exe);
        cmd.arg(pipe).arg("--persist").creation_flags(flags);
        if !env.is_empty() {
            cmd.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
        if logging_enabled() {
            if let Some(dir) = std::env::var_os("TEMP") {
                let log = std::path::Path::new(&dir).join("nospacekey-engine.log");
                if let Ok(f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log)
                {
                    if let Ok(f2) = f.try_clone() {
                        cmd.stdout(Stdio::from(f)).stderr(Stdio::from(f2));
                    }
                }
            }
        }
        cmd
    };
    match build(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Ok(child) => Some(child),
        // ERROR_ACCESS_DENIED(5): ホストが breakaway 不許可の Job 内だと CREATE_BREAKAWAY_FROM_JOB
        // 自体が拒否される（Job 制約であり exe の ACL ではない）。その場合は breakaway を諦めて
        // Job 内で spawn する — engine がホストの Job 道連れになるリスクより「engine が一切
        // 立たない」方が実害が大きい（道連れで死んでも次打鍵の respawn で復帰する）。
        Err(e) if e.raw_os_error() == Some(5) => {
            match build(CREATE_NO_WINDOW | DETACHED_PROCESS).spawn() {
                Ok(child) => {
                    tip_log("ev=engine_spawn_retry breakaway=off ok=true");
                    Some(child)
                }
                Err(e2) => {
                    tip_log(&format!(
                        "ev=engine_spawn_err os={:?} kind={:?} msg={} retry=breakaway_off",
                        e2.raw_os_error(),
                        e2.kind(),
                        e2
                    ));
                    None
                }
            }
        }
        Err(e) => {
            tip_log(&format!(
                "ev=engine_spawn_err os={:?} kind={:?} msg={}",
                e.raw_os_error(),
                e.kind(),
                e
            ));
            None
        }
    }
}

/// engine を detached で spawn **だけ**する（接続はしない）。ensure_engine と同型の env
/// （DPAPI 復号鍵含む — 欠くと spawn した engine の LLM 鍵が欠ける）で spawn し、SpawnGuard で
/// プロセス跨ぎの起動を直列化する。guard 待ちの間に他ホスト／別経路が起こした可能性があるため
/// 短時間で再確認し、既に listening なら spawn しない。
/// 戻り値: Some(pid)=spawn 成功 / Some(0)=既に listening（spawn 不要） / None=失敗。
/// Child は pid を返して即 drop する（kill しない — detached/persist で生き続ける）。
/// A7 の respawn_engine（power.rs）と cold start ② の prespawn_engine が共用する。
pub(crate) fn spawn_engine_only(pipe: &str) -> Option<u32> {
    // SpawnGuard でプロセス跨ぎの起動を直列化。取れなくても best-effort で進む。
    let _guard = crate::engine_link::SpawnGuard::acquire(pipe);
    if EngineClient::connect_to(pipe, Duration::from_millis(50)).is_ok() {
        return Some(0); // 既に listening（誰かが起こした）→ spawn 不要
    }
    let exe = engine_exe_path()?;
    let s = settings::load();
    let key_plain = if s.llm.api_key_dpapi.is_empty() {
        None
    } else {
        settings::dpapi::decrypt(&s.llm.api_key_dpapi)
    };
    let env_map = settings::resolve_env_map(&s, key_plain.as_ref().map(|z| z.as_str()), |k| {
        std::env::var(k).ok()
    });
    spawn_engine_hidden(&exe, pipe, &env_map).map(|child| child.id())
}

/// デバウンスタイマ発火 proc（WM_TIMER）。STA 単一スレッドなので thread_local の生ポインタから
/// TextService を引いて遅延変換する。一発限り（発火時に自分を KillTimer）。
extern "system" fn debounce_timer_proc(_hwnd: HWND, _msg: u32, id: usize, _time: u32) {
    unsafe {
        let _ = KillTimer(None, id);
    }
    let ptr = DEBOUNCE_TS.with(|p| p.get());
    if ptr.is_null() {
        return;
    }
    let ts: &TextService_Impl = unsafe { &*ptr };
    // このインスタンスがまさに発火した id を保持しているときだけ作用する
    // （複数 TextService が 1 STA スレッドに同居した場合の取り違え/二重発火を防ぐ）。
    if ts.debounce_timer.get() != id {
        return;
    }
    ts.debounce_timer.set(0);
    // UU-4: 遅延変換も presenter 経由でホスト再入しうる COM 区間なので guarded で包む。
    // guarded の flush は Behavior 確定処理（COM 呼び出し）まで走りうるので、extern "system" の
    // タイマ proc から panic が FFI を越える（=UB）のを catch_unwind で止める（key sink の catch_com と対）。
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ts.guarded(|| ts.on_debounce_convert());
    }));
}

/// LLM 結果ポーリング proc（WM_TIMER）。STA 単一スレッドなので thread_local からインスタンスを引く。
/// スロットに結果が入っていれば取り出して反映し、タイマを止める。
extern "system" fn llm_poll_proc(_hwnd: HWND, _msg: u32, id: usize, _time: u32) {
    let ptr = LLM_TS.with(|p| p.get());
    if ptr.is_null() {
        return;
    }
    let ts: &TextService_Impl = unsafe { &*ptr };
    if ts.llm_poll_timer.get() != id {
        // この id は現在のインスタンスの物ではない（複数インスタンスが 1 STA に同居した場合等）。
        // ポーリングタイマは反復発火するので、放置すると永久に CPU を食う。確実に止める
        // （debounce_timer_proc が先頭で無条件 KillTimer するのと同じ防御）。
        unsafe {
            let _ = KillTimer(None, id);
        }
        return;
    }
    // UU-4(#5/#6): on_llm_outcome/abort_llm も run_preedit（同期 edit session）でホスト再入しうる
    // COM 区間なので guarded で包み、extern "system" 越えの panic は catch_unwind で止める
    // （debounce_timer_proc と対称）。
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ts.guarded(|| {
            let outcome = ts
                .llm_slot
                .borrow()
                .as_ref()
                .and_then(|s| s.lock().ok().and_then(|mut g| g.take()));
            if let Some(o) = outcome {
                ts.disarm_llm_poll();
                *ts.llm_slot.borrow_mut() = None;
                ts.on_llm_outcome(o);
            } else if ts.llm_timed_out() {
                // 上限時間を超えても結果が来ない＝エンジンがハング。待機を解除して劣化する。
                ts.abort_llm("timeout");
            }
        });
    }));
}

/// 巡3 Z4: ReloadConfig busy 再送タイマの発火口（単発 — 先頭で自分を掃除）。
/// engine_reload_config は IPC 送信を含む COM 区間ではないが、応答処理から drop_engine が
/// 走りうるので panic 保護だけ入れる（タイマ proc の共通規律）。
extern "system" fn reload_retry_timer_proc(_hwnd: HWND, _msg: u32, id: usize, _time: u32) {
    unsafe {
        let _ = KillTimer(None, id);
    }
    let ptr = RELOAD_RETRY_TS.with(|p| p.get());
    if ptr.is_null() {
        return;
    }
    let ts: &TextService_Impl = unsafe { &*ptr };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ts.fire_reload_retry(id);
    }));
}

#[cfg(test)]
mod prediction_slot_tests {
    use super::prediction_slot_available;

    #[test]
    fn old_physical_slot_blocks_a_new_field_until_cleanup() {
        assert!(prediction_slot_available(false, false));
        assert!(!prediction_slot_available(true, false));
        assert!(!prediction_slot_available(false, true));
        assert!(!prediction_slot_available(true, true));
    }
}

extern "system" fn prediction_retry_timer_proc(_hwnd: HWND, _msg: u32, id: usize, _time: u32) {
    unsafe {
        let _ = KillTimer(None, id);
    }
    let ptr = PREDICTION_RETRY_TS.with(|p| p.get());
    if ptr.is_null() {
        return;
    }
    let ts: &TextService_Impl = unsafe { &*ptr };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ts.guarded(|| ts.fire_prediction_finish_retry(id));
    }));
}

extern "system" fn prediction_poll_timer_proc(_hwnd: HWND, _msg: u32, id: usize, _time: u32) {
    unsafe {
        let _ = KillTimer(None, id);
    }
    let ptr = PREDICTION_POLL_TS.with(|p| p.get());
    if ptr.is_null() {
        tip_log("ev=prediction_poll state=null_owner");
        return;
    }
    let ts: &TextService_Impl = unsafe { &*ptr };
    if ts.prediction_poll_timer.get() != id {
        tip_log(&format!(
            "ev=prediction_poll state=stale_timer expected={} actual={id}",
            ts.prediction_poll_timer.get(),
        ));
        return;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ts.guarded(|| ts.fire_prediction_poll(id));
    }));
    if result.is_err() {
        tip_log("ev=panic site=prediction_poll");
    }
}

/// TextService が（Deactivate を経ずに）解放された場合の保険: 武装中のデバウンスタイマを
/// 確実に解除し、thread_local の生ポインタを無効化する（タイマ発火時の dangling 参照=UAF を防ぐ）。
impl Drop for TextService {
    fn drop(&mut self) {
        // 巡2 F5: Deactivate を経ない解放（二重 Activate のリーク経路等）でも sink の
        // cookie を返却する — context 側 ITfSource に COM 参照が残るのを防ぐ保険。
        // 巡3 G5: 不変条件 — AdviseSink は context→sink への強参照を作り、TextService は
        // layout_sink_ctx で context を強参照するため、cookie≠0 のまま参照カウントが零に
        // なることはなく「cookie が残ったまま Drop 到達」は論理的に起きない（到達しても
        // 常に no-op）。将来 advise サイトを増やす際の対称性破りへの防御として残す。
        // Drop は outer 型（&mut TextService）なので Impl ポインタは Activate で覚えさせた
        // impl_ptr フィールド経由で比較する。
        let me = self.impl_ptr.get();
        self.unadvise_layout_sink();
        let id = self.debounce_timer.replace(0);
        if id != 0 {
            unsafe {
                let _ = KillTimer(None, id);
            }
        }
        let lid = self.llm_poll_timer.replace(0);
        if lid != 0 {
            unsafe {
                let _ = KillTimer(None, lid);
            }
        }
        // 巡3 Z4: busy 再送タイマも掃除し、カウンタを戻す（接続が変われば busy 状態も無関係）。
        let rid = self.reload_retry_timer.replace(0);
        if rid != 0 {
            unsafe {
                let _ = KillTimer(None, rid);
            }
        }
        self.reload_retry_count.set(0);
        // 巡4 T1: 遅延 flush タイマも掃除。
        let bf = self.behavior_flush_timer.replace(0);
        if bf != 0 {
            unsafe {
                let _ = KillTimer(None, bf);
            }
        }
        let pr = self.prediction_retry_timer.replace(0);
        if pr != 0 {
            unsafe {
                let _ = KillTimer(None, pr);
            }
        }
        let pp = self.prediction_poll_timer.replace(0);
        if pp != 0 {
            unsafe {
                let _ = KillTimer(None, pp);
            }
        }
        if let Some(slot) = self.prediction_slot.borrow_mut().take() {
            slot.cancel();
        }
        RELOAD_RETRY_TS.with(|p| {
            if p.get() == me {
                p.set(std::ptr::null());
            }
        });
        PREDICTION_RETRY_TS.with(|p| {
            if p.get() == me {
                p.set(std::ptr::null());
            }
        });
        PREDICTION_POLL_TS.with(|p| {
            if p.get() == me {
                p.set(std::ptr::null());
            }
        });
        // 巡2 E2: 各 TLS の null 化は「自分が指しているときだけ」— 同一 STA スレッドに複数
        // TextService が載る構成（二重 Activate のリーク等）で、旧インスタンスの Drop が
        // 新インスタンスの TLS をワイプして追従/タイマ処理を止めないための防御。
        DEBOUNCE_TS.with(|p| {
            if p.get() == me {
                p.set(std::ptr::null());
            }
        });
        LLM_TS.with(|p| {
            if p.get() == me {
                p.set(std::ptr::null());
            }
        });
        // UIバグ4: レイアウト再照会セッション用の自己ポインタも無効化。
        LAYOUT_TS.with(|p| {
            if p.get() == me {
                p.set(std::ptr::null());
            }
        });
        // SP6a: Behavior 自己ポインタも無効化（Deactivate を経ない解放での dangling 防止）。
        BEHAVIOR_TS.with(|c| {
            if c.get() == me {
                c.set(std::ptr::null());
            }
        });
        // C-1: DLL 生存参照は `_guard`（ComObjectGuard）の Drop が自動で -1 する
        // （この drop 本体の後にフィールドが drop される）。全 #[implement] オブジェクトが
        // 解放されたら DllCanUnloadNow=S_OK。
    }
}

/// InputScope 配列に password/PIN 系 scope が含まれるか。
pub fn scopes_contain_password(scopes: &[i32]) -> bool {
    use windows::Win32::UI::TextServices::{
        IS_ALPHANUMERIC_PIN, IS_ALPHANUMERIC_PIN_SET, IS_NUMERIC_PASSWORD, IS_NUMERIC_PIN,
        IS_PASSWORD,
    };
    [
        IS_PASSWORD,
        IS_NUMERIC_PASSWORD,
        IS_NUMERIC_PIN,
        IS_ALPHANUMERIC_PIN,
        IS_ALPHANUMERIC_PIN_SET,
    ]
    .iter()
    .any(|scope| scopes.contains(&scope.0))
}

/// 予測へ確定済み本文を渡してよいかの sensitive-scope 判定。
/// `None` は一過性の InputScope 照会失敗なので通常欄として扱い、呼び出し側で再照会する。
fn prediction_scope_is_sensitive(keyboard_disabled: bool, password: Option<bool>) -> bool {
    keyboard_disabled || password.unwrap_or(false)
}

/// compartment の VARIANT 値が「フラグ ON」か（バグ#1 の純判定）。
/// Chromium は VT_I4 の 1 を書く（tsf_bridge.cc InitializeDisabledContext の variant.Set(1)）。
/// 未設定は VT_EMPTY。VT_I4 以外は安全側 false（通常欄を誤って direct 化しない）。
pub fn compartment_flag_is_set(v: &VARIANT) -> bool {
    if v.vt() != VT_I4 {
        return false;
    }
    i32::try_from(v).map(|x| x != 0).unwrap_or(false)
}

/// ctx のコンテキスト compartment に「キーボード無効」系フラグが立っているか（バグ#1）。
/// Chromium/Edge はパスワード欄（TEXT_INPUT_TYPE_PASSWORD）専用の ITfContext に
/// GUID_COMPARTMENT_KEYBOARD_DISABLED=1 を、text store の無い空 context に
/// GUID_COMPARTMENT_EMPTYCONTEXT=1 を立てる（ui/base/ime/win/tsf_bridge.cc
/// InitializeDisabledContext）。どちらも「この context では IME が介入しない」が
/// 規約どおりの振る舞いなので、両方を password 同等（完全 direct 化）に扱う。
/// compartment 読みは edit session 不要で軽量。どの段の失敗も false（安全側）。
fn query_context_keyboard_disabled(ctx: &ITfContext) -> bool {
    let Ok(cm) = ctx.cast::<ITfCompartmentMgr>() else {
        return false;
    };
    [
        GUID_COMPARTMENT_KEYBOARD_DISABLED,
        GUID_COMPARTMENT_EMPTYCONTEXT,
    ]
    .iter()
    .any(|guid| unsafe {
        cm.GetCompartment(guid)
            .and_then(|c| c.GetValue())
            .map(|v| compartment_flag_is_set(&v))
            .unwrap_or(false)
    })
}

/// `NOSPACEKEY_LOG` が有効(非空・"0"以外)のときだけ診断ログを出す。
/// テスト用に env 値を注入できる純関数。
fn log_enabled_from_env(v: Option<&std::ffi::OsStr>) -> bool {
    v.is_some_and(|s| !s.is_empty() && s != "0")
}

/// 診断ログ有効判定。env はプロセス寿命中不変とみなし1回だけ評価してキャッシュする。
fn logging_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| log_enabled_from_env(std::env::var_os("NOSPACEKEY_LOG").as_deref()))
}

/// 現在時刻の UNIX epoch ミリ秒（クロック巻き戻り等の失敗は 0 — ログ用途なので panic しない）。
pub(crate) fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// ログローテーションのサイズ上限（8MB）。超えたら 1 世代（.1）だけ退避する最小形
/// （2026-07-04 spec で非ゴールとされた負債への最小回答 — 品質ループ①）。
const LOG_ROTATE_BYTES: u64 = 8 * 1024 * 1024;

/// `path` のサイズが `max` を超えていれば `<path>.1` へ rename する（1世代のみ・失敗は無視）。
/// 他プロセスが追記オープン中の rename 失敗も「ローテーションしないだけ」で無害。
fn rotate_log_if_larger_than(path: &std::path::Path, max: u64) {
    let too_big = std::fs::metadata(path)
        .map(|m| m.len() > max)
        .unwrap_or(false);
    if too_big {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        let _ = std::fs::rename(path, std::path::Path::new(&rotated));
    }
}

/// `dir`/nospacekey-tip.log に1行追記する実体（テスト用に dir を注入可能）。
/// 行形式は `[pid N] ts=<epoch_ms> <msg>`（ts= は pid prefix 直後の固定位置 —
/// testbench log_parse が pid 除去後に strip する規約。品質ループ①）。
fn tip_log_write_to(dir: &std::ffi::OsStr, msg: &str) {
    use std::io::Write;
    let path = std::path::Path::new(dir).join("nospacekey-tip.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        // 複数プロセスが同時追記するので、行を1回の write_all で書いて行割れを避ける。
        let _ = f.write_all(
            format!("[pid {}] ts={} {}\n", std::process::id(), epoch_ms(), msg).as_bytes(),
        );
    }
}

/// 軽量診断ログ。`NOSPACEKEY_LOG` 有効時のみ `%TEMP%\nospacekey-tip.log` に追記する（失敗は無視）。
/// TIP は任意のホストプロセスに読み込まれて実機 IME は直接観測できないため、
/// 接続/起動/変換の分岐をここに残して事後解析できるようにする。PID を前置する。
/// プロセス初回の書き込み時に (1) 8MB 超なら .1 へ最小ローテーション、
/// (2) `ev=log_open build=<ver>-<githash>` を先行出力する（どのビルドのログかを特定可能に）。
pub(crate) fn tip_log(msg: &str) {
    if !logging_enabled() {
        return;
    }
    let dir = match std::env::var_os("TEMP") {
        Some(d) => d,
        None => return,
    };
    static LOG_OPENED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    LOG_OPENED.get_or_init(|| {
        rotate_log_if_larger_than(
            &std::path::Path::new(&dir).join("nospacekey-tip.log"),
            LOG_ROTATE_BYTES,
        );
        tip_log_write_to(
            &dir,
            &format!(
                "ev=log_open build={}-{}",
                env!("CARGO_PKG_VERSION"),
                env!("GIT_HASH")
            ),
        );
    });
    tip_log_write_to(&dir, msg);
}

// ---- 確定取消（Ctrl+Backspace）: 事前条件の純関数判定 ----

/// 確定取消をスキップする理由（`undo_precheck` の Err 型 — start_commit_undo の分岐と
/// ev=commit_undo_skip の reason に対応）。`LatinReading`（全 ASCII 読みの除外）は本 Task では
/// 実装しない（設計ロック I-5 の任意選択肢は非採用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UndoSkip {
    /// 非武装（直前確定が undo 対象でない or 既に disarm 済み）。
    NotArmed,
    /// composition が開いている（部分確定直後の候補窓/preedit を壊さない — no-op）。
    CompositionOpen,
    /// 直前確定バッファ（last_commit）が無い。
    NoBuffer,
    /// 確定文字列が 64 UTF-16 単位を超える（読み戻しバッファ上限外 — undo 対象外）。
    TooLong,
}

/// 確定取消の事前条件を判定する純関数（COM 部分と分離してユニットテスト可能にする）。
/// `armed`=undo_armed / `has_composition`=composition.is_some() / `has_buffer`=last_commit.is_some()
/// / `tlen_utf16`=確定文字列の UTF-16 単位数。判定順は NotArmed → CompositionOpen → NoBuffer →
/// TooLong（CompositionOpen は「維持」、他は呼び出し側で disarm — I-6 の遷移表）。
pub(crate) fn undo_precheck(
    armed: bool,
    has_composition: bool,
    has_buffer: bool,
    tlen_utf16: usize,
) -> std::result::Result<(), UndoSkip> {
    if !armed {
        return Err(UndoSkip::NotArmed);
    }
    if has_composition {
        return Err(UndoSkip::CompositionOpen);
    }
    if !has_buffer {
        return Err(UndoSkip::NoBuffer);
    }
    if tlen_utf16 > 64 {
        return Err(UndoSkip::TooLong);
    }
    Ok(())
}

// ---- 文節ナビゲーション（変換中の←/→）の純データ ----

/// TIP が保持する文節ビュー。`segments` の連結が preedit 全体、`selected` が太下線を引く
/// 選択文節。候補列は持たない — 候補窓の唯一の真実源は従来どおり cand_state。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ClauseNav {
    pub segments: Vec<String>,
    pub selected: usize,
}

/// エンジン ClauseView 応答の TIP 内表現（engine_move_clause / engine_select_clause_candidate）。
pub(crate) struct ClauseViewData {
    pub segments: Vec<String>,
    pub selected: usize,
    pub candidates: Vec<String>,
    pub candidate_index: usize,
}

// ---- 品質ループ③: 誤変換ワンキー記録（直前確定バッファ → feedback.jsonl）----

/// 直前確定 1 件のバッファ（誤変換ワンキー記録の対象）。sel=-1 はライブ/直接確定
/// （候補選択なし）。commit サイトが**クリア前に**保存する（key_event_sink.rs 参照）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LastCommit {
    pub ts_ms: u64,
    pub reading: String,
    pub text: String,
    pub source: String,
    pub sel: i32,
    pub cand_n: usize,
}

/// JSON 文字列エスケープ（RFC 8259 の必須集合: `"` `\` と制御文字 U+0000..1F）。
/// tip は serde_json 非依存（cdylib の依存を増やさない）のため手書きで最小実装する。
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// LastCommit を 1 行 JSON（jsonl の 1 レコード、改行なし）へ直列化する純関数。
pub(crate) fn feedback_jsonl_line(r: &LastCommit) -> String {
    format!(
        "{{\"ts_ms\":{},\"reading\":\"{}\",\"text\":\"{}\",\"source\":\"{}\",\"sel\":{},\"cand_n\":{}}}",
        r.ts_ms,
        json_escape(&r.reading),
        json_escape(&r.text),
        json_escape(&r.source),
        r.sel,
        r.cand_n
    )
}

/// feedback.jsonl のパス（`%LOCALAPPDATA%\nospacekey\feedback.jsonl` — settings.json /
/// 学習 memory/ と同階層。ディレクトリ名の大小文字は settings::settings_path と同一）。
/// 巡3 Z1: 空文字 LOCALAPPDATA は「無い」扱い — 通すと TIP をホストするアプリの CWD へ
/// nospacekey\ を作って書き続ける（settings_path / dict_path と同じ規律）。
fn feedback_path() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|d| !d.is_empty())
        .map(|d| {
            std::path::PathBuf::from(d)
                .join("nospacekey")
                .join("feedback.jsonl")
        })
}

/// feedback.jsonl へ 1 行追記する（親 dir が無ければ作る）。1 レコードを 1 回の
/// write_all で書いて行割れを避ける（tip_log と同じ流儀）。
fn append_feedback_record(rec: &LastCommit) -> std::io::Result<()> {
    use std::io::Write;
    let path = feedback_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no LOCALAPPDATA"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(format!("{}\n", feedback_jsonl_line(rec)).as_bytes())
}

impl TextService_Impl {
    /// 品質ループ③: 誤変換ワンキー記録（Ctrl+変換 / Ctrl+/ → OnPreservedKey Feedback）。
    /// settings.feedback.enabled（opt-in・既定 false）かつ直前確定バッファが Some のときだけ
    /// feedback.jsonl へ 1 行追記する。バッファは**消費**する（連打で同一確定を重複記録しない）。
    /// 診断ログには長さのみ残し、本文（reading/text）は書かない（診断ログと feedback の分離）。
    /// パスワード欄の握り潰しは OnPreservedKey の共通ガードが先に効く（key_event_sink.rs）。
    pub(crate) fn record_feedback(&self) {
        if !self.feedback_enabled.get() {
            tip_log("ev=feedback_skip reason=disabled");
            return;
        }
        let rec = self.last_commit.borrow_mut().take();
        let Some(rec) = rec else {
            tip_log("ev=feedback_skip reason=no_last_commit");
            return;
        };
        match append_feedback_record(&rec) {
            Ok(()) => tip_log(&format!(
                "ev=feedback_logged rlen={} tlen={}",
                rec.reading.chars().count(),
                rec.text.chars().count()
            )),
            Err(e) => tip_log(&format!("ev=feedback_write_failed kind={:?}", e.kind())),
        }
    }
}

/// この DLL と同じディレクトリにある兄弟 exe（`name`）のパスを解決する。
/// 取得失敗時は None（その場合は起動を諦めて劣化動作）。
fn sibling_exe(name: &str) -> Option<std::path::PathBuf> {
    // 切り詰め検出つきヘルパでこの DLL のフルパスを取り、その隣の exe を指す
    // （固定 260 だと長いパスで切り詰められ、存在しない exe を起動しようとして劣化する）。
    let dll_path = crate::globals::module_file_path()?;
    let dir = std::path::Path::new(&dll_path).parent()?;
    Some(dir.join(name))
}

/// この DLL と同じディレクトリにある `NospacekeyEngineHost.exe` のパスを解決する。
pub(crate) fn engine_exe_path() -> Option<std::path::PathBuf> {
    sibling_exe("NospacekeyEngineHost.exe")
}

/// SP6b: この DLL と同じディレクトリにある `NospacekeyConfig.exe`（設定 GUI）のパスを解決する。
fn config_exe_path() -> Option<std::path::PathBuf> {
    sibling_exe("NospacekeyConfig.exe")
}

#[cfg(test)]
mod prespawn_tests {
    use super::should_prespawn;

    #[test]
    fn prespawn_decision_spawns_only_when_no_client_and_not_attempted() {
        // Activate 時: client 無し・未試行なら spawn。既接続/試行済み/バックオフ中は何もしない。
        assert!(should_prespawn(false, false, true)); // (has_client, spawn_attempted, backoff_allows)
        assert!(!should_prespawn(true, false, true));
        assert!(!should_prespawn(false, true, true));
        assert!(!should_prespawn(false, false, false));
    }
}

#[cfg(test)]
mod uu4_reentrancy_tests {
    use super::ReentrancyGate;

    #[test]
    fn signal_outside_operation_runs_now() {
        // 区間外（借用未保持）の再入シグナルは「いま実行してよい」＝ false を返し、保留しない。
        let g = ReentrancyGate::new();
        assert!(!g.signal_reentry(true), "区間外は即実行(false)のはず");
        assert!(!g.take_pending(), "区間外シグナルは保留を立てない");
    }

    #[test]
    fn signal_inside_operation_defers_and_records_pending() {
        // 区間中（借用保持中）に要求ありで再入 → 保留（true=呼び出し側は戻る）＋ pending 記録。
        let g = ReentrancyGate::new();
        let prev = g.enter();
        assert!(g.in_operation());
        assert!(g.signal_reentry(true), "区間中は保留(true)のはず");
        g.exit(prev);
        assert!(!g.in_operation(), "最外を抜けたら区間フラグは false");
        // 安全点で pending を1回だけ回収できる。
        assert!(g.take_pending());
        assert!(!g.take_pending(), "pending は take で1回でクリア");
    }

    #[test]
    fn signal_inside_operation_without_action_does_not_defer_work() {
        // 要求が無い（outbox 空＝SetSelection 由来など）再入は、保留（戻る）はするが pending は
        // 立てない（flush で無駄に inner を回さない）。
        let g = ReentrancyGate::new();
        let prev = g.enter();
        assert!(
            g.signal_reentry(false),
            "区間中は has_action に依らず戻る(true)"
        );
        g.exit(prev);
        assert!(!g.take_pending(), "要求無しの再入は pending を立てない");
    }

    #[test]
    fn nested_enter_exit_restores_outer_flag() {
        // ネスト（inner drain が enter する）でも、内側 exit で最外 true を保ち、最外 exit で false。
        let g = ReentrancyGate::new();
        let outer = g.enter(); // 最外: prev=false
        let inner = g.enter(); // 内側: prev=true
        assert!(g.in_operation());
        g.exit(inner); // 内側を抜けても最外区間はまだ中
        assert!(g.in_operation(), "内側 exit 後も最外区間は継続(true)");
        g.exit(outer);
        assert!(!g.in_operation(), "最外 exit でようやく false");
    }
}

#[cfg(test)]
mod uu5_reload_config_tests {
    use super::{build_reload_config, Request};
    use settings::Settings;

    #[test]
    fn frozen_llm_sends_empty_fields_even_when_enabled() {
        // 凍結契約(docs/superpowers/specs/2026-07-21-llm-freeze-design.md): enabled=true+鍵ありでも
        // llm_enabled:false+LLM系フィールド空で送る=平文キーがパイプを流れない。timeout_ms は
        // llm_enabled:false でエンジンが読まないスカラなので生値のまま。凍結前の
        // enabled_llm_carries_settings_values は再開時に spec の再開手順で復元する。
        let mut s = Settings::default();
        s.llm.enabled = true;
        s.llm.endpoint = "https://e".into();
        s.llm.model = "gpt-4o-mini".into();
        s.llm.prompt = "p".into();
        s.llm.timeout_ms = 12000;
        s.zenzai.enabled = true;
        s.zenzai.weight_path = "C:/w.gguf".into();
        let req = build_reload_config(&s, Some("sk-x"), |_| None);
        assert_eq!(
            req,
            Request::ReloadConfig {
                llm_enabled: false,
                llm_api_key: "".into(),
                llm_endpoint: "".into(),
                llm_model: "".into(),
                llm_prompt: "".into(),
                llm_timeout_ms: 12000,
                zenzai_enabled: true,
                zenzai_weight: "C:/w.gguf".into(),
                inline_prediction_enabled: false,
                learning_enabled: true,
                typo_learn_enabled: true,
                zenzai_inference_limit: Some(1),
            }
        );
    }

    #[test]
    fn disabled_llm_sends_empty_llm_fields() {
        // LLM 無効時は鍵が復号できても LLM 系は空で送る（エンジンを disabled に落とす＝H-1 整合）。
        let mut s = Settings::default();
        s.llm.enabled = false;
        s.llm.endpoint = "https://leak".into();
        let req = build_reload_config(&s, Some("sk-should-not-leak"), |_| None);
        match req {
            Request::ReloadConfig {
                llm_enabled,
                llm_api_key,
                llm_endpoint,
                ..
            } => {
                assert!(!llm_enabled);
                assert_eq!(llm_api_key, "");
                assert_eq!(llm_endpoint, "", "無効時は endpoint も送らない");
            }
            _ => panic!("ReloadConfig を組み立てるはず"),
        }
    }

    #[test]
    fn zenzai_flag_and_weight_are_forwarded_regardless_of_llm() {
        // Zenzai は LLM の有無に依らず enabled/weight をそのまま送る。
        let mut s = Settings::default();
        s.llm.enabled = false;
        s.zenzai.enabled = false;
        s.zenzai.weight_path = String::new();
        match build_reload_config(&s, None, |_| None) {
            Request::ReloadConfig {
                zenzai_enabled,
                zenzai_weight,
                ..
            } => {
                assert!(!zenzai_enabled);
                assert_eq!(zenzai_weight, "");
            }
            _ => panic!("ReloadConfig を組み立てるはず"),
        }
    }

    #[test]
    fn inference_limit_pushes_clamped_value_or_defers_to_env_override() {
        // D6: TIP env に診断 override が居るときは None（push 抑止）、居なければクランプ済み Some。
        let mut s = Settings::default();
        s.zenzai.inference_limit = 0; // 手編集異常値 → クランプで 1
        match build_reload_config(&s, None, |_| None) {
            Request::ReloadConfig {
                zenzai_inference_limit,
                ..
            } => {
                assert_eq!(zenzai_inference_limit, Some(1));
            }
            _ => panic!("ReloadConfig を組み立てるはず"),
        }
        match build_reload_config(&s, None, |k| {
            (k == "NOSPACEKEY_ZENZAI_INFERENCE_LIMIT").then(|| "5".to_string())
        }) {
            Request::ReloadConfig {
                zenzai_inference_limit,
                ..
            } => {
                assert_eq!(zenzai_inference_limit, None);
            }
            _ => panic!("ReloadConfig を組み立てるはず"),
        }
    }
}

/// 再変換確定の RecordCorrection 送出条件(commit_candidate の reconverting 分岐から呼ぶ)。
/// index 0 は 1 位受諾=訂正ではない。空読みは採取できなかった劣化経路(送らない)。
pub(crate) fn should_record_correction(index: usize, reading: &str) -> bool {
    index != 0 && !reading.is_empty()
}

#[cfg(test)]
mod a8_tests {
    use super::{
        engine_failure_event, is_toggle_repeat, plan_start_session, should_log_slow,
        should_record_correction, Response, IPC_TIMEOUT_CONVERT, IPC_TIMEOUT_FAST,
        IPC_TIMEOUT_LIVE, MODE_TOGGLE_REPEAT_GUARD,
    };
    use std::time::Duration;

    #[test]
    fn record_correction_only_for_non_top_with_reading() {
        // 送出条件: 再変換中の候補確定で「1位以外を選んだ」かつ読みを保持できている時だけ。
        // index 0 は 1 位受諾(訂正ではない)、空読みは経路劣化(深層防御 — spec §2(b))。
        assert!(should_record_correction(1, "みこみっと"));
        assert!(should_record_correction(5, "わーるど"));
        assert!(!should_record_correction(0, "みこみっと"));
        assert!(!should_record_correction(1, ""));
    }

    #[test]
    fn engine_failure_diagnostics_never_include_response_bodies() {
        let sentinel = "秘密の入力と予測";
        for response in [
            Response::Reading {
                reading: sentinel.into(),
            },
            Response::Candidates {
                candidates: vec![sentinel.into()],
            },
            Response::Committed {
                text: sentinel.into(),
                reading: sentinel.into(),
            },
            Response::LlmResult {
                seq: 1,
                text: sentinel.into(),
            },
            Response::Prediction {
                seq: 1,
                text: sentinel.into(),
            },
            Response::Error {
                message: sentinel.into(),
            },
        ] {
            let event = engine_failure_event("sentinel_test", &Ok(response));
            assert!(!event.contains(sentinel), "body leaked: {event}");
        }
    }

    #[test]
    fn start_session_plan_adopts_session_and_drops_otherwise() {
        // 正常応答: セッション採用。
        assert_eq!(
            plan_start_session(Ok(Response::Session {
                session: 7,
                proto: None
            })),
            Some(7)
        );
        // タイムアウト: 遅延 Session フレームが滞留しうるので接続破棄（恒常 1-off desync 防止）。
        assert_eq!(
            plan_start_session(Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "t"))),
            None
        );
        // 切断系エラーも破棄。
        assert_eq!(
            plan_start_session(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "b"
            ))),
            None
        );
        // 予期しない応答型（プロトコル desync の兆候）も破棄。
        assert_eq!(
            plan_start_session(Ok(Response::Error {
                message: "x".into()
            })),
            None
        );
    }

    #[test]
    fn end_session_ack_is_only_the_ok_response() {
        use super::end_session_ack_accepted;
        assert!(end_session_ack_accepted(&Ok(Response::Ok)));
        // EndSession の ack は `Response::Ok` だけ。他 op と同じく「期待した型以外は破棄」。
        assert!(!end_session_ack_accepted(&Ok(Response::Reading {
            reading: "にほんご".into()
        })));
        assert!(!end_session_ack_accepted(&Ok(Response::LiveResult {
            seq: 3,
            text: "日本語".into(),
            reading: "にほんご".into(),
            committed: None,
        })));
        assert!(!end_session_ack_accepted(&Ok(Response::Error {
            message: "x".into()
        })));
        assert!(!end_session_ack_accepted(&Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "t"
        ))));
    }

    #[test]
    fn handshake_decision_table() {
        use super::{decide_handshake, HandshakeAction};
        // 一致 → 採用。
        assert_eq!(
            decide_handshake(Some(super::PROTO_VERSION), false),
            HandshakeAction::Accept
        );
        // 不一致（None=handshake 以前の旧エンジン）で未試行 → 世代交代。
        assert_eq!(
            decide_handshake(None, false),
            HandshakeAction::ShutdownRespawn
        );
        // 不一致（新しすぎる proto）で未試行 → 世代交代。
        assert_eq!(
            decide_handshake(Some(999), false),
            HandshakeAction::ShutdownRespawn
        );
        // 一度試して尚不一致 → 接続維持（無限 shutdown ループ防止）。
        assert_eq!(decide_handshake(None, true), HandshakeAction::DegradeKeep);
    }

    #[test]
    fn toggle_repeat_suppresses_only_within_guard() {
        // 軽微1: 初回（None）は通す、閾値未満は抑止、閾値以上は通す。
        assert!(!is_toggle_repeat(None, MODE_TOGGLE_REPEAT_GUARD)); // 初回
        assert!(is_toggle_repeat(
            Some(Duration::from_millis(33)),
            MODE_TOGGLE_REPEAT_GUARD
        )); // オートリピート連射 → 抑止
        assert!(is_toggle_repeat(
            Some(Duration::from_millis(299)),
            MODE_TOGGLE_REPEAT_GUARD
        )); // 閾値直前 → 抑止
        assert!(!is_toggle_repeat(
            Some(MODE_TOGGLE_REPEAT_GUARD),
            MODE_TOGGLE_REPEAT_GUARD
        )); // ちょうど閾値 → 通す
        assert!(!is_toggle_repeat(
            Some(Duration::from_millis(500)),
            MODE_TOGGLE_REPEAT_GUARD
        )); // 意図した押し直し → 通す
    }

    #[test]
    fn slow_log_fires_past_half_tier() {
        let tier = Duration::from_millis(200);
        assert!(!should_log_slow(Duration::from_millis(50), tier)); // 25% → 出さない
        assert!(!should_log_slow(Duration::from_millis(100), tier)); // ちょうど半分 → 出さない
        assert!(should_log_slow(Duration::from_millis(101), tier)); // 半分超 → 出す
    }

    #[test]
    fn tier_values_are_ordered_as_specified() {
        assert_eq!(IPC_TIMEOUT_FAST, Duration::from_millis(250));
        assert_eq!(IPC_TIMEOUT_LIVE, Duration::from_millis(400));
        assert_eq!(IPC_TIMEOUT_CONVERT, Duration::from_millis(1200));
    }

    /// INV2: ドレイン回収した committed 付き LiveResult だけが drop 判定になること（純関数）。
    #[test]
    fn drained_committed_liveresult_needs_drop() {
        use super::drained_needs_drop;
        use ipc::protocol::Response;
        // committed が非空 → engine 側だけ確定適用済みの不整合 → drop すべき。
        assert!(drained_needs_drop(&Response::LiveResult {
            seq: 1,
            text: "入力".into(),
            reading: "にゅうりょく".into(),
            committed: Some("日本語".into()),
        }));
        // committed が空文字列 → 適用差分なし → 破棄でよい（drop しない）。
        assert!(!drained_needs_drop(&Response::LiveResult {
            seq: 1,
            text: "にほんご".into(),
            reading: "にほんご".into(),
            committed: Some(String::new()),
        }));
        // committed 無し → drop しない。
        assert!(!drained_needs_drop(&Response::LiveResult {
            seq: 1,
            text: "日本語".into(),
            reading: "にほんご".into(),
            committed: None,
        }));
        // LiveResult 以外（Reading 等）→ drop しない。
        assert!(!drained_needs_drop(&Response::Reading {
            reading: "にほんご".into()
        }));
    }

    /// tip 層の統合テスト（Windows 限定）。regsvr32/管理者/VM を要さず、応答を返さない
    /// dead-reply named pipe を相手に、tip ラッパ `timed_request` が `request_within` を通じて
    /// tier 締め切りを実際に適用し、速やかに `TimedOut` を返すことを実機ホスト無しで証明する。
    /// ipc::client::win_pipe_tests::create_server を tip crate 側で再現している。
    #[cfg(all(test, windows))]
    mod win {
        use std::time::{Duration, Instant};
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::CloseHandle;
        // windows 0.62: PIPE_ACCESS_DUPLEX は FILE_FLAGS_AND_ATTRIBUTES 型で Storage::FileSystem に在る
        // （CreateNamedPipeW の dwOpenMode 引数の型）。Pipes モジュールには無いので import 元を分ける。
        use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
        use windows::Win32::System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
        };

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }

        /// サーバ端の pipe インスタンスを1個だけ作って握ったまま返す（応答は返さない）。
        /// クライアントが接続でき、かつ何も返ってこない dead-reply 状況を作る。
        fn create_server(name: &str) -> windows::Win32::Foundation::HANDLE {
            let w = wide(name);
            // windows 0.62: CreateNamedPipeW（W 版）は Result ではなく HANDLE を直接返し、
            // 失敗は INVALID_HANDLE_VALUE。A 版だけが Result を返すため .expect は使えない。
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(w.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1,    // nMaxInstances
                    4096, // out buffer
                    4096, // in buffer
                    0,    // default timeout
                    None, // default security
                )
            };
            assert!(!handle.is_invalid(), "CreateNamedPipeW failed");
            handle
        }

        #[test]
        fn timed_request_times_out_against_dead_pipe() {
            // 一意名（スタックアドレス由来）。Date/rand は使えないのでアドレスで一意化。
            let name = format!(r"\\.\pipe\nospacekey-a8-tip-test-{:p}", &0u8 as *const u8);
            let server = create_server(&name);

            // クライアント接続 → 応答が来ないので timed_request が TimedOut を返すこと。
            let mut client =
                ipc::client::EngineClient::connect_to(&name, Duration::from_secs(1)).unwrap();
            let started = Instant::now();
            let res = super::super::timed_request(
                &mut client,
                &ipc::protocol::Request::Insert {
                    session: 1,
                    text: "n".into(),
                    style: None,
                },
                super::super::IPC_TIMEOUT_FAST,
                "insert",
            );
            let elapsed = started.elapsed();

            unsafe {
                let _ = CloseHandle(server);
            }

            let err = res.expect_err("expected timeout error");
            assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
            // FAST(250ms) tier が実際に適用されたこと（別の長い duration ではない）を、
            // 3倍未満で戻ったことで示す。
            assert!(
                elapsed < super::super::IPC_TIMEOUT_FAST * 3,
                "took too long: {elapsed:?}"
            );
        }

        /// 要求受信後 `delay` してから `resp` を書く応答サーバをスレッドで動かす（ドレイン検証用）。
        fn spawn_delayed_reply_server(
            name: String,
            delay: Duration,
            resp: ipc::protocol::Response,
        ) -> std::thread::JoinHandle<()> {
            use ipc::framing::{read_frame, write_frame};
            use std::os::windows::io::FromRawHandle;
            use windows::Win32::Foundation::HANDLE;
            use windows::Win32::System::Pipes::ConnectNamedPipe;
            let server = create_server(&name);
            // HANDLE(*mut c_void) は Send でないのでスレッド境界は usize で渡す。
            let server_addr = server.0 as usize;
            std::thread::spawn(move || {
                let server = HANDLE(server_addr as *mut core::ffi::c_void);
                unsafe {
                    let _ = ConnectNamedPipe(server, None);
                }
                let mut f = unsafe { std::fs::File::from_raw_handle(server.0 as _) };
                let _: std::io::Result<ipc::protocol::Request> = read_frame(&mut f);
                std::thread::sleep(delay);
                let _ = write_frame(&mut f, &resp);
            })
        }

        /// A' 統合: live 相当の要求が締め切り超過 → keep で pending 化 → サーバ応答到着後に
        /// drain_pending が滞留フレームを回収して交互性を回復し、続く要求が「1つ前の応答」では
        /// なく自分の応答を受けること（1-off desync 非発生）を実 Named Pipe で証明する。
        #[test]
        fn keep_then_drain_recovers_alternation() {
            use ipc::protocol::{Request, Response};
            let name = format!(r"\\.\pipe\nospacekey-tip-drain-{:p}", &0u8 as *const u8);
            // 1 回目応答は ~150ms 遅れ（LIVE 締め切りより後に到着させる）。
            let server = spawn_delayed_reply_server(
                name.clone(),
                Duration::from_millis(150),
                Response::LiveResult {
                    seq: 7,
                    text: "日本語".into(),
                    reading: "にほんご".into(),
                    committed: None,
                },
            );

            let mut client =
                ipc::client::EngineClient::connect_to(&name, Duration::from_secs(1)).unwrap();

            // keep 版: 締め切り 40ms を超過 → TimedOut かつ pending。
            let r = super::super::timed_request_keep(
                &mut client,
                &Request::LiveConvert {
                    session: 1,
                    seq: 7,
                    left_context: None,
                    auto_commit: true,
                },
                Duration::from_millis(40),
                "live_convert",
            );
            assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
            assert!(client.is_pending());

            // 応答到着まで余裕を見て drain → 回収は 1 回目の seq=7。
            let drained = client
                .drain_pending(Instant::now() + Duration::from_millis(600))
                .expect("drain must not error")
                .expect("drain must recover the owed response");
            match drained {
                Response::LiveResult { seq, .. } => assert_eq!(seq, 7),
                other => panic!("unexpected drained response: {other:?}"),
            }
            assert!(!client.is_pending());

            server.join().ok();
        }
    }
}

/// A7: 半死 engine（接続は受理するが StartSession に無応答）に対し、ensure_engine の
/// プローブ枝が辿る遷移を実機ホスト無しで再現する統合テスト（Windows 限定・admin 不要）。
/// TextService インスタンスは組み立てない（ensure_engine の配線自体は item8 headless＋実機で担保）。
#[cfg(all(test, windows))]
mod a7_tests {
    use super::{resume_poll_action, timed_request, Request, IPC_TIMEOUT_FAST};
    use crate::engine_link::ReconnectBackoff;
    use ipc::client::EngineClient;
    use std::time::{Duration, Instant};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 接続は受理するが応答は返さない dead-reply サーバ端を1個握って返す（a8_tests の写し）。
    fn create_server(name: &str) -> HANDLE {
        let w = wide(name);
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(w.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                None,
            )
        };
        assert!(!handle.is_invalid(), "CreateNamedPipeW failed");
        handle
    }

    #[test]
    fn half_dead_probe_transitions() {
        let name = format!(r"\\.\pipe\nospacekey-a7-tip-test-{:p}", &0u8 as *const u8);
        let server = create_server(&name);

        // 1) 接続は受理される（半死 engine への一発プローブが Ok を返す状況）。
        let mut c = EngineClient::connect_to(&name, Duration::ZERO)
            .expect("connect should be accepted by half-dead server");

        // 2) StartSession は無応答なので FAST tier で TimedOut になる（＝session 確立失敗）。
        let res = timed_request(
            &mut c,
            &Request::StartSession,
            IPC_TIMEOUT_FAST,
            "start_session",
        );

        unsafe {
            let _ = CloseHandle(server);
        }
        assert_eq!(
            res.expect_err("StartSession should time out").kind(),
            std::io::ErrorKind::TimedOut,
        );

        // 3) ensure_engine が半死検出時に踏む遷移: on_session_failure でプローブ抑止＋クールダウン。
        let now = Instant::now();
        let mut b = ReconnectBackoff::new();
        b.on_session_failure(now);
        assert!(
            !b.probe_allowed(),
            "probe must be suppressed after session failure"
        );
        assert!(!b.full_attempt_allowed(now + Duration::from_millis(999)));
        assert!(b.full_attempt_allowed(now + Duration::from_secs(1)));
    }

    /// A7: resume_poll_action の判定表（spec 7.2-3）。世代は等値比較であり大小比較でないこと
    /// （wrap 安全）も確認する。
    #[test]
    fn resume_poll_action_transitions() {
        assert_eq!(resume_poll_action(0, 0, false), None); // 世代変化なし
        assert_eq!(resume_poll_action(1, 0, false), Some(true)); // 復帰＋idle → drop
        assert_eq!(resume_poll_action(1, 0, true), Some(false)); // 復帰＋busy → 温存
        assert_eq!(resume_poll_action(2, 1, false), Some(true)); // 連続復帰でも同じ扱い
                                                                 // wrap 安全: 世代は等値比較のみで大小比較しないため u32::MAX → 0 のラップでも復帰扱い。
        assert_eq!(resume_poll_action(0, u32::MAX, false), Some(true));
    }
}

#[cfg(test)]
mod log_gate_tests {
    use super::{log_enabled_from_env, rotate_log_if_larger_than, tip_log_write_to};
    use std::ffi::OsStr;

    #[test]
    fn env_rules_enable_only_nonempty_non_zero() {
        assert!(!log_enabled_from_env(None));
        assert!(!log_enabled_from_env(Some(OsStr::new(""))));
        assert!(!log_enabled_from_env(Some(OsStr::new("0"))));
        assert!(log_enabled_from_env(Some(OsStr::new("1"))));
        assert!(log_enabled_from_env(Some(OsStr::new("true"))));
    }

    #[test]
    fn write_to_appends_pid_prefixed_line_with_ts() {
        let dir = std::env::temp_dir().join(format!("nospacekey-logtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let logp = dir.join("nospacekey-tip.log");
        let _ = std::fs::remove_file(&logp);
        tip_log_write_to(dir.as_os_str(), "ev=unit hello");
        let content = std::fs::read_to_string(&logp).unwrap();
        assert!(content.contains("ev=unit hello"), "got: {content}");
        assert!(content.starts_with("[pid "), "PID 前置が無い: {content}");
        // 品質ループ①: pid prefix 直後に ts=<digits> が固定位置で入る。
        let body = content.split("] ").nth(1).expect("] 区切りが無い");
        assert!(body.starts_with("ts="), "ts= が pid 直後に無い: {content}");
        let ts_val: &str = body["ts=".len()..].split(' ').next().unwrap();
        assert!(
            !ts_val.is_empty() && ts_val.bytes().all(|b| b.is_ascii_digit()),
            "ts 値が数字でない: {content}"
        );
        let _ = std::fs::remove_file(&logp);
    }

    #[test]
    fn rotation_renames_oversized_log_to_dot1_once() {
        let dir =
            std::env::temp_dir().join(format!("nospacekey-rotatetest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let logp = dir.join("nospacekey-tip.log");
        let rotated = dir.join("nospacekey-tip.log.1");
        // 上限以下: ローテーションしない。
        std::fs::write(&logp, "1234").unwrap();
        rotate_log_if_larger_than(&logp, 4);
        assert!(
            logp.exists() && !rotated.exists(),
            "上限以下で回してはいけない"
        );
        // 上限超: .1 へ rename（1世代のみ — 既存 .1 は上書き）。
        std::fs::write(&logp, "12345").unwrap();
        rotate_log_if_larger_than(&logp, 4);
        assert!(!logp.exists(), "元ファイルが残っている");
        assert_eq!(std::fs::read_to_string(&rotated).unwrap(), "12345");
        // ファイルが無い場合は no-op（panic しない）。
        rotate_log_if_larger_than(&logp, 4);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod feedback_tests {
    use super::{feedback_jsonl_line, json_escape, LastCommit};

    #[test]
    fn feedback_record_serializes_one_line() {
        let r = LastCommit {
            ts_ms: 1,
            reading: "にほんご".into(),
            text: "二本後".into(),
            source: "live".into(),
            sel: -1,
            cand_n: 0,
        };
        let line = feedback_jsonl_line(&r);
        assert!(!line.contains('\n'), "jsonl は 1 レコード 1 行: {line}");
        assert!(line.contains("\"reading\":\"にほんご\""), "got: {line}");
        assert!(line.contains("\"text\":\"二本後\""), "got: {line}");
        assert!(line.contains("\"sel\":-1"), "got: {line}");
        assert_eq!(
            line,
            r#"{"ts_ms":1,"reading":"にほんご","text":"二本後","source":"live","sel":-1,"cand_n":0}"#
        );
    }

    #[test]
    fn feedback_record_escapes_json_special_chars() {
        // 確定文字列に " や \ が入っても壊れた JSON を書かない（ラテン合成の raw 確定で現実に起きうる）。
        let r = LastCommit {
            ts_ms: 2,
            reading: "a\"b\\c".into(),
            text: "x\ny".into(),
            source: "candidate".into(),
            sel: 3,
            cand_n: 9,
        };
        let line = feedback_jsonl_line(&r);
        assert!(!line.contains('\n'), "改行はエスケープされる: {line}");
        assert!(line.contains(r#""reading":"a\"b\\c""#), "got: {line}");
        assert!(line.contains(r#""text":"x\ny""#), "got: {line}");
        assert!(line.contains("\"sel\":3"), "got: {line}");
        assert!(line.contains("\"cand_n\":9"), "got: {line}");
        // 制御文字は \u00XX。
        assert_eq!(json_escape("\u{01}"), "\\u0001");
    }
}

#[cfg(test)]
mod input_scope_tests {
    use super::{
        compartment_flag_is_set, prediction_mode_allows_display, prediction_scope_is_sensitive,
        scopes_contain_password,
    };

    #[test]
    fn scopes_contain_password_detects() {
        use windows::Win32::UI::TextServices::{
            IS_ALPHANUMERIC_PIN, IS_ALPHANUMERIC_PIN_SET, IS_DEFAULT, IS_NUMERIC_PASSWORD,
            IS_NUMERIC_PIN, IS_PASSWORD,
        };
        assert!(scopes_contain_password(&[IS_DEFAULT.0, IS_PASSWORD.0]));
        for sensitive in [
            IS_NUMERIC_PASSWORD,
            IS_NUMERIC_PIN,
            IS_ALPHANUMERIC_PIN,
            IS_ALPHANUMERIC_PIN_SET,
        ] {
            assert!(scopes_contain_password(&[sensitive.0]));
        }
        assert!(!scopes_contain_password(&[IS_DEFAULT.0]));
        assert!(!scopes_contain_password(&[]));
    }

    /// バグ#1: Chromium/Edge が書く KEYBOARD_DISABLED compartment 値の判定。
    /// Chromium は VT_I4 の 1（variant.Set(1)）。未設定 VT_EMPTY・非 VT_I4 は安全側 false。
    #[test]
    fn compartment_flag_detects_vt_i4_nonzero() {
        use windows::Win32::System::Variant::VARIANT;
        assert!(compartment_flag_is_set(&VARIANT::from(1i32)));
        assert!(!compartment_flag_is_set(&VARIANT::from(0i32)));
        assert!(!compartment_flag_is_set(&VARIANT::default())); // VT_EMPTY（未設定）
        assert!(!compartment_flag_is_set(&VARIANT::from(true))); // VT_BOOL は安全側 false
    }

    #[test]
    fn prediction_scope_blocks_only_confirmed_sensitive_contexts() {
        assert!(prediction_scope_is_sensitive(true, None));
        assert!(prediction_scope_is_sensitive(false, Some(true)));
        assert!(!prediction_scope_is_sensitive(false, Some(false)));
        assert!(!prediction_scope_is_sensitive(false, None));
    }

    #[test]
    fn prediction_display_requires_stable_native_mode() {
        assert!(prediction_mode_allows_display(false, false));
        assert!(!prediction_mode_allows_display(true, false));
        assert!(!prediction_mode_allows_display(false, true));
    }
}

#[cfg(test)]
mod commit_undo_tests {
    use super::{undo_precheck, UndoSkip};

    #[test]
    fn undo_precheck_gates_all_preconditions() {
        // (armed, has_composition, has_buffer, tlen_utf16) -> Ok / 各 skip reason
        assert!(undo_precheck(true, false, true, 3).is_ok());
        assert_eq!(
            undo_precheck(false, false, true, 3),
            Err(UndoSkip::NotArmed)
        );
        assert_eq!(
            undo_precheck(true, true, true, 3),
            Err(UndoSkip::CompositionOpen)
        ); // 部分確定直後など
        assert_eq!(
            undo_precheck(true, false, false, 3),
            Err(UndoSkip::NoBuffer)
        );
        assert_eq!(undo_precheck(true, false, true, 65), Err(UndoSkip::TooLong));
        // 64 UTF-16 単位上限
    }
}

#[cfg(test)]
mod deactivate_preflight_tests {
    use super::{deactivate_cancel_plan, DeactivateCancelPlan as Plan};

    #[test]
    fn no_composition_needs_no_cancel() {
        // composition 無し = 取消不要。context/reconvert の状態に依らず清算へ直行。
        assert_eq!(
            deactivate_cancel_plan(false, false, false, false),
            Plan::Nothing
        );
        assert_eq!(
            deactivate_cancel_plan(false, true, true, true),
            Plan::Nothing
        );
    }

    #[test]
    fn composition_without_context_aborts_before_cleanup() {
        // 取消不能（context 無し）= 不可逆清算の前に中断。composition・再変換ラッチ共に
        // 残す＝ホストの再 Deactivate で再試行できる。
        assert_eq!(
            deactivate_cancel_plan(true, false, false, false),
            Plan::AbortNoContext
        );
        assert_eq!(
            deactivate_cancel_plan(true, false, true, true),
            Plan::AbortNoContext
        );
    }

    #[test]
    fn reconverting_uses_restore_text_not_cancel_composition() {
        // 再変換中はユーザの既存テキストの上に composition がある — do_cancel は range を
        // 空に潰すため原文が消える。RestoreText（cancel_reconvert）でなければならない。
        assert_eq!(
            deactivate_cancel_plan(true, true, true, false),
            Plan::CancelReconvert
        );
    }

    #[test]
    fn plain_composition_uses_do_cancel() {
        assert_eq!(
            deactivate_cancel_plan(true, true, false, false),
            Plan::DoCancel
        );
    }

    #[test]
    fn committed_pending_end_uses_close_only_even_if_reconvert_latch_is_still_set() {
        assert_eq!(
            deactivate_cancel_plan(true, true, true, true),
            Plan::DoCancel
        );
    }
}

#[cfg(test)]
mod deactivating_guard_tests {
    use super::DeactivatingGuard;
    use std::cell::Cell;

    #[test]
    fn guard_marks_interval_and_resets_on_drop() {
        let f = Cell::new(false);
        {
            let _g = DeactivatingGuard::new(&f);
            assert!(f.get(), "ガード構築直後は Deactivate 実行中");
        }
        assert!(!f.get(), "drop で復帰 — この後の Activate を受け入れられる");
    }

    #[test]
    fn guard_resets_on_panic_unwind() {
        // deactivate_inner の panic（catch_unwind が受け止める前提の unwind）でもフラグは
        // 復帰しなければならない — 残ると以後の Activate が永久に拒否される。
        let f = Cell::new(false);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = DeactivatingGuard::new(&f);
            panic!("unwind through DeactivatingGuard");
        }));
        assert!(r.is_err(), "クロージャ内で panic したはず");
        assert!(!f.get(), "unwind 経由の drop でもフラグは復帰する");
    }
}

#[cfg(test)]
mod partial_redraw_retry_tests {
    use super::{
        next_composition_end_retry, next_partial_redraw_retry, COMPOSITION_END_RETRY_MAX,
        PARTIAL_REDRAW_RETRY_MAX,
    };

    #[test]
    fn retry_sequence_stops_at_bound() {
        let mut count = 0;
        for expected in 1..=PARTIAL_REDRAW_RETRY_MAX {
            count = next_partial_redraw_retry(count).expect("上限までは再試行する");
            assert_eq!(count, expected);
        }
        assert_eq!(next_partial_redraw_retry(count), None);
    }

    #[test]
    fn saturated_counter_never_wraps_into_retry() {
        assert_eq!(next_partial_redraw_retry(u8::MAX), None);
    }

    #[test]
    fn composition_end_retry_is_bounded() {
        assert_eq!(next_composition_end_retry(0), Some(1));
        assert_eq!(
            next_composition_end_retry(COMPOSITION_END_RETRY_MAX - 2),
            Some(COMPOSITION_END_RETRY_MAX - 1)
        );
        assert_eq!(
            next_composition_end_retry(COMPOSITION_END_RETRY_MAX - 1),
            None
        );
        assert_eq!(next_composition_end_retry(COMPOSITION_END_RETRY_MAX), None);
        assert_eq!(next_composition_end_retry(u8::MAX), None);
    }
}

#[cfg(test)]
mod pending_end_liveness_tests {
    use super::{
        CompositionEndStatus, PendingEndKeyReservation, PendingEndKeySignature,
        PendingEndTestDecision, TextService,
    };

    fn sig(context: usize, raw_vk: u32, lparam: isize, modifiers: u8) -> PendingEndKeySignature {
        PendingEndKeySignature::synthetic(context, raw_vk, raw_vk, lparam, modifiers)
    }

    fn a() -> PendingEndKeySignature {
        sig(1, 0x41, 0x001e0001, 0)
    }

    fn b() -> PendingEndKeySignature {
        sig(1, 0x42, 0x00300001, 0)
    }

    #[test]
    fn occupied_slot_returns_false_for_a_second_test_and_a_key_is_one_shot() {
        let service = TextService::new().into_outer();
        service.composition_end_pending.set(true);
        assert_eq!(
            service.pending_end_test_decision(a()),
            PendingEndTestDecision::Reserve
        );
        assert_eq!(
            service.pending_end_test_decision(b()),
            PendingEndTestDecision::Busy
        );
        assert!(service.take_pending_end_test(a()));
        assert!(!service.take_pending_end_test(a()));
    }

    #[test]
    fn mismatched_key_discards_slot_and_future_same_vk_is_normal() {
        let service = TextService::new().into_outer();
        service.composition_end_pending.set(true);
        assert_eq!(
            service.pending_end_test_decision(a()),
            PendingEndTestDecision::Reserve
        );
        assert!(!service.take_pending_end_test(b()));
        service.composition_end_pending.set(false);
        assert_eq!(
            service.pending_end_test_decision(a()),
            PendingEndTestDecision::Normal
        );
        assert!(!service.take_pending_end_test(a()));
    }

    #[test]
    fn close_callback_preserves_exact_pair_once() {
        let service = TextService::new().into_outer();
        service.composition_end_pending.set(true);
        assert_eq!(
            service.pending_end_test_decision(a()),
            PendingEndTestDecision::Reserve
        );
        service.composition_generation.set(7);
        service.composition_end_pending.set(false);
        assert!(service.take_pending_end_test(a()));
        assert!(!service.take_pending_end_test(a()));
    }

    #[test]
    fn lifecycle_invalidation_prevents_old_pair_from_crossing_new_composition() {
        let service = TextService::new().into_outer();
        service.composition_end_pending.set(true);
        assert_eq!(
            service.pending_end_test_decision(a()),
            PendingEndTestDecision::Reserve
        );
        let generation = service.key_pair_generation.get();
        service.invalidate_pending_end_test_reservation();
        assert_ne!(service.key_pair_generation.get(), generation);
        assert_eq!(
            service.pending_end_test_decision(b()),
            PendingEndTestDecision::Reserve
        );
        assert!(service.take_pending_end_test(b()));
        assert!(!service.take_pending_end_test(a()));
    }

    #[test]
    fn signature_and_generation_mismatch_are_consumed_not_retained() {
        let mut reservation = PendingEndKeyReservation::default();
        let first = a();
        let other_context = sig(2, 0x41, first.lparam, first.modifiers);
        let other_physical_event = sig(1, 0x41, first.lparam + 1, first.modifiers | 0x02);
        assert!(reservation.reserve(first, 7));
        assert!(!reservation.take_if_matches(other_context, 7));
        assert_eq!(reservation.signature(), None);
        assert_eq!(reservation.generation(), None);
        assert!(reservation.reserve(first, 8));
        assert!(!reservation.take_if_matches(other_physical_event, 8));
        assert_eq!(reservation.signature(), None);
        assert_eq!(reservation.generation(), None);

        assert!(reservation.reserve(first, 9));
        assert!(!reservation.take_if_matches(first, 8));
        assert_eq!(reservation.signature(), None);
    }

    #[test]
    fn pending_test_without_pending_expires_old_slot() {
        let service = TextService::new().into_outer();
        service.composition_end_pending.set(true);
        assert_eq!(
            service.pending_end_test_decision(a()),
            PendingEndTestDecision::Reserve
        );
        service.composition_end_pending.set(false);
        assert_eq!(
            service.pending_end_test_decision(b()),
            PendingEndTestDecision::Normal
        );
        assert!(!service.take_pending_end_test(a()));
    }

    #[test]
    fn shared_started_signal_preflight_invalidates_reentrant_key_once() {
        let service = TextService::new().into_outer();
        service.composition_end_pending.set(true);
        assert_eq!(
            service.pending_end_test_decision(a()),
            PendingEndTestDecision::Reserve
        );
        let initial_generation = service.key_pair_generation.get();
        let initial_composition_generation = service.composition_generation.get();

        // RequestEditSession rejected / StartComposition was never called: the reservation and
        // pair generation remain unchanged, so the matching Key can still settle the pending close.
        service.consume_started_composition();
        assert_eq!(service.key_pair_generation.get(), initial_generation);
        assert_eq!(
            service.pending_end_test_reservation.borrow().signature(),
            Some(a())
        );

        // A real StartComposition success sets the shared production signal.  An OnKey/OnTest
        // preflight can consume it during the later COM callout, before the caller returns.
        service.composition_started_signal.set(true);
        service.consume_started_composition();
        let after_success = service.key_pair_generation.get();
        assert_ne!(after_success, initial_generation);
        assert_eq!(
            service.composition_generation.get(),
            initial_composition_generation.wrapping_add(1)
        );
        assert_eq!(
            service.pending_end_test_reservation.borrow().signature(),
            None
        );
        assert!(!service.take_pending_end_test(a()));

        // The original RequestEditSession caller consumes the same shared signal after return;
        // the reentrant preflight already consumed it, so this is a no-op.
        service.consume_started_composition();
        assert_eq!(service.key_pair_generation.get(), after_success);
        assert_eq!(
            service.composition_generation.get(),
            initial_composition_generation.wrapping_add(1)
        );
    }

    #[test]
    fn transient_retry_then_success_uses_bounded_production_transition() {
        let service = TextService::new().into_outer();
        service.composition_end_pending.set(true);
        service.composition_end_retry_count.set(1); // initial EndComposition call

        assert!(!service.apply_pending_end_attempt(CompositionEndStatus::Retryable));
        assert_eq!(service.composition_end_retry_count.get(), 2);
        assert!(service.composition_end_pending.get());

        // A later transient retry succeeds without SetText or a second composition commit.
        assert!(service.apply_pending_end_attempt(CompositionEndStatus::Closed));
        assert!(!service.composition_end_pending.get());
        assert_eq!(service.composition_end_retry_count.get(), 0);
        assert_eq!(
            service.composition_end_status.get(),
            CompositionEndStatus::Closed
        );

        // Callback/already-closed cleanup is idempotent: a late close result cannot reopen or
        // otherwise clear a newer state after the first transition.
        assert!(service.apply_pending_end_attempt(CompositionEndStatus::Closed));
    }

    #[test]
    fn terminal_and_retry_limit_quarantine_release_pending_state() {
        let terminal = TextService::new().into_outer();
        terminal.composition_end_pending.set(true);
        terminal.composition_end_retry_count.set(1);
        assert!(terminal.apply_pending_end_attempt(CompositionEndStatus::Terminal));
        assert!(!terminal.composition_end_pending.get());
        assert_eq!(
            terminal.composition_end_status.get(),
            CompositionEndStatus::Terminal
        );
        assert!(terminal.apply_pending_end_attempt(CompositionEndStatus::Terminal));

        let exhausted = TextService::new().into_outer();
        exhausted.composition_end_pending.set(true);
        exhausted.composition_end_retry_count.set(1);
        assert!(!exhausted.apply_pending_end_attempt(CompositionEndStatus::Retryable));
        assert_eq!(exhausted.composition_end_retry_count.get(), 2);
        // Initial + one retry = 2; this failed attempt reaches the total-call budget of 3.
        assert!(exhausted.apply_pending_end_attempt(CompositionEndStatus::Retryable));
        assert!(!exhausted.composition_end_pending.get());
        assert_eq!(exhausted.composition_end_retry_count.get(), 0);
        assert_eq!(
            exhausted.composition_end_status.get(),
            CompositionEndStatus::Terminal
        );
        assert!(exhausted.apply_pending_end_attempt(CompositionEndStatus::Terminal));
    }

    #[test]
    fn late_generation_is_not_current_after_quarantine() {
        let service = TextService::new().into_outer();
        service.composition_end_pending.set(true);
        service
            .pending_end_generation
            .set(service.composition_generation.get());
        assert!(service.pending_end_generation_is_current());

        assert!(service.apply_pending_end_attempt(CompositionEndStatus::Terminal));
        assert!(!service.pending_end_generation_is_current());
    }
}

#[cfg(test)]
mod prediction_commit_end_edit_tests {
    use super::consume_expected_prediction_commit_end_edit;
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    #[test]
    fn expected_commit_end_edit_is_consumed_once() {
        let now = Instant::now();
        let deadline = Cell::new(Some(now + Duration::from_millis(300)));

        assert!(consume_expected_prediction_commit_end_edit(
            &deadline, false, now
        ));
        assert!(!consume_expected_prediction_commit_end_edit(
            &deadline, false, now
        ));
    }

    #[test]
    fn selection_change_is_never_treated_as_the_committed_text_edit() {
        let now = Instant::now();
        let deadline = Cell::new(Some(now + Duration::from_millis(300)));

        assert!(!consume_expected_prediction_commit_end_edit(
            &deadline, true, now
        ));
        assert!(deadline.get().is_none());
    }

    #[test]
    fn delayed_commit_edit_allowance_expires() {
        let now = Instant::now();
        let deadline = Cell::new(Some(now));

        assert!(!consume_expected_prediction_commit_end_edit(
            &deadline,
            false,
            now + Duration::from_millis(1),
        ));
        assert!(deadline.get().is_none());
    }
}
