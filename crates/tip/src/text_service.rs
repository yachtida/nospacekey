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
use std::rc::Rc;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::{implement, Interface, IUnknown, IUnknownImpl, Ref, Result, HSTRING};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::{VARIANT, VT_I4};
use windows::Win32::UI::TextServices::{
    ITfCategoryMgr, ITfCompartment, ITfCompartmentMgr, ITfComposition, ITfCompositionSink,
    ITfCompositionSink_Impl, ITfContext, ITfDisplayAttributeProvider, ITfDocumentMgr, ITfEditSession,
    ITfFnConfigure, ITfFnConfigure_Impl, ITfFunction_Impl, ITfKeyEventSink, ITfKeystrokeMgr,
    ITfLangBarItemButton, ITfLangBarItemMgr, ITfLangBarItemSink,
    ITfSource, ITfThreadFocusSink, ITfThreadFocusSink_Impl, ITfThreadMgr, ITfThreadMgrEx,
    ITfThreadMgrEventSink, ITfThreadMgrEventSink_Impl,
    ITfTextInputProcessor_Impl,
    ITfTextInputProcessorEx, ITfTextInputProcessorEx_Impl, ITfUIElementMgr, CLSID_TF_CategoryMgr,
    GUID_COMPARTMENT_EMPTYCONTEXT, GUID_COMPARTMENT_KEYBOARD_DISABLED,
    GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION, TF_CONTEXT_EDIT_CONTEXT_FLAGS, TF_ES_READ,
    TF_ES_READWRITE, TF_ES_SYNC, TF_LBI_ICON, TF_LBI_STATUS, TF_LBI_TEXT, TF_PRESERVEDKEY,
    TF_TMF_IMMERSIVEMODE,
};
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};

use crate::candidate_presenter::CandidatePresenter;
use crate::candidate_state::CandidateState;
use crate::candidate_uielement::BehaviorAction;
use crate::candidate_window::CandidateUI;
use crate::edit_session::{
    CancelComposition, CommitText, CommitUndoStart, QueryCaretRect, QueryInputScopes,
    QueryMonitorAnchorRect, ReconvertCapture, ReconvertStart, RestoreText, StartOrUpdatePreedit,
};
use crate::globals::{ComObjectGuard, GUID_DISPLAY_ATTRIBUTE};
use crate::input_state::InputState;
use crate::input_state::InsertStyle;
use crate::input_state::is_fresh_live;
use crate::input_state::ReconvertKind;
use crate::llm_worker::{spawn_llm_worker, LlmOutcome, LlmSlot};

use ipc::client::EngineClient;
use ipc::protocol::{Request, Response, PROTO_VERSION};

/// プロセス内でエンジン用パイプ名を一意化するための連番（TextService インスタンス毎に +1）。
/// engine_pipe_name が stable_pipe_name を使うようになったため現在は未使用だが、将来の参照のために保持。
#[allow(dead_code)]
static NEXT_PIPE_SEQ: AtomicU32 = AtomicU32::new(0);

/// デバウンス間隔（ms）。打鍵が落ち着いてから変換するまでの待ち。
const DEBOUNCE_MS: u32 = 30;

/// IPC 要求の op 別締め切り。超過すると request_within が TimedOut を返し、既存の劣化枝に合流する。
const IPC_TIMEOUT_FAST: Duration = Duration::from_millis(250);    // Insert/Backspace/Commit/Start/EndSession
const IPC_TIMEOUT_CONVERT: Duration = Duration::from_millis(1200); // Convert/Reconvert（Zenzai 推論に余裕）
const IPC_TIMEOUT_LIVE: Duration = Duration::from_millis(400);     // LiveConvert（debounce 済・遅ければ捨てる）

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
    if gen == last { None } else { Some(!busy) }
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
        tip_log(&format!("ev=ipc_slow op={op} ms={ms} tier={}", tier.as_millis()));
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
        tip_log(&format!("ev=ipc_slow op={op} ms={ms} tier={}", tier.as_millis()));
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
/// 空なら送り、エンジン側が既定パス（exe 隣）を解決する。
///
/// セキュリティ注記（fable レビュー #3・許容済トレードオフ）: 平文 API キーが常駐エンジンへの
/// 名前付きパイプを流れる。パイプ DACL は AppContainer/LPAC SID にも接続を許すため、同一ユーザの
/// サンドボックスプロセスが起動レースでパイプ名を先取り squat すればキーを窃取しうる（env 経由の
/// spawn では読めなかった経路）。パイプは元来入力テキスト全体を運んでおり同一ユーザ信頼が前提な
/// こと・squat は起動レース勝利を要すること・LLM 有効化/キー変更の即時反映という機能価値から、
/// **キー送信を維持する**判断（ユーザ承認）。将来のハードニング候補は
/// GetNamedPipeServerProcessId によるサーバ像検証（送信前に正規エンジンか照合）。
/// なお凍結中(settings::LLM_CONVERT_FROZEN)は llm_effective_enabled が false のためキーは流れない。
fn build_reload_config(s: &settings::Settings, api_key_plain: Option<&str>) -> Request {
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
        learning_enabled: s.learning.enabled,
        typo_learn_enabled: s.typo_correct.learn,
    }
}

/// キャレット矩形を `GetTextExt` で取得できないときの既定アンカー（スクリーン座標）。
/// 候補窓・モード HUD ともこの座標へフォールバックする（旧 MVP の固定値と同一）。
pub(crate) const DEFAULT_CARET_POS: crate::candidate_window::CaretAnchor =
    crate::candidate_window::CaretAnchor { x: 200, y: 200, caret_top: None };

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

#[implement(
    ITfTextInputProcessorEx,
    ITfKeyEventSink,
    ITfDisplayAttributeProvider,
    ITfCompositionSink,
    ITfThreadMgrEventSink,
    ITfThreadFocusSink,
    ITfFnConfigure
)]
pub struct TextService {
    pub(crate) tid: Cell<u32>,
    pub(crate) thread_mgr: RefCell<Option<ITfThreadMgr>>,
    /// ITfThreadMgrEventSink を AdviseSink した cookie（0=未登録）。Deactivate で UnadviseSink する。
    /// スレッド内 doc フォーカス変化（別ウィンドウ切替）を OnSetFocus で捕捉し、ホストが
    /// OnCompositionTerminated を呼ばずに合成を確定/破棄しても、エンジンセッションの読み残留を防ぐ。
    pub(crate) thread_mgr_event_cookie: Cell<u32>,
    /// ITfThreadFocusSink を AdviseSink した cookie（0=未登録）。Deactivate で UnadviseSink する。
    /// クロスプロセス（別アプリへ前面が移る）でのフォーカス喪失は ITfThreadMgrEventSink::OnSetFocus
    /// では届かないため、前面（スレッド）喪失を OnKillThreadFocus で捕捉して同じ放棄リセットを焚く。
    pub(crate) thread_focus_cookie: Cell<u32>,
    pub(crate) client: RefCell<Option<EngineClient>>,
    pub(crate) engine_session: Cell<i64>,
    /// engine_end_session を呼んだとき client が LLM ワーカへ move 済みで EndSession を送れなかった
    /// セッション id を保留する。client 復帰時(on_llm_outcome)に送って engine 側の取り残しを防ぐ。
    pub(crate) pending_end_session: Cell<i64>,
    pub(crate) state: RefCell<InputState>,
    pub(crate) composition: Rc<RefCell<Option<ITfComposition>>>,
    /// U9: composition 開始時に捕捉した左文脈（サニタイズ済・最大40字）。
    /// StartOrUpdatePreedit / ReconvertStart が**成否によらず必ず上書き**し、変換系
    /// リクエスト（Convert/LiveConvert/LlmConvert/Reconvert）へ載せる。合成終了経路
    /// （commit_and_reset / cancel / reset_abandoned_composition）で明示 None クリア
    /// — edit session 拒否で取得コード自体が走らないときの前文書残留を塞ぐ（spec §2.1）。
    pub(crate) left_context: Rc<RefCell<Option<String>>>,
    pub(crate) da_atom: Cell<u32>,
    pub(crate) showing: Cell<bool>,
    pub(crate) candidate_ui: RefCell<CandidatePresenter>,
    /// SP6a: presenter / UIElement と共有する候補状態（GetCount/GetString 等の読み元）。
    /// TextService も co-owner として保持し、drain_behavior の Finalize で
    /// 選択中候補（Behavior::SetSelection が更新する唯一の真実源）を読む。
    pub(crate) cand_state: Rc<RefCell<CandidateState>>,
    /// SP6a: Behavior(ホスト発)が確定/取消要求を書き込むスロット。drain_behavior が取り出す。
    pub(crate) behavior_outbox: Rc<RefCell<Option<BehaviorAction>>>,
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
    pub(crate) default_direct_applied: Cell<bool>,
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
    /// 消費して feedback.jsonl へ書く。idle_symbol の直接確定（読み無し記号）は対象外。
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
    /// 記号全角設定（settings.symbol.full_width）のキャッシュ。Activate で1度読む（D7）。
    /// idle 記号確定 / composition 記号畳み込み / Shift+数字行の記号化に使う。既定 false（半角）。
    pub(crate) symbol_full_width: Cell<bool>,
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
        let candidate_ui =
            RefCell::new(CandidatePresenter::new(cand_state.clone(), behavior_outbox.clone(), notify));
        Self {
            tid: Cell::new(0),
            thread_mgr: RefCell::new(None),
            thread_mgr_event_cookie: Cell::new(0),
            thread_focus_cookie: Cell::new(0),
            client: RefCell::new(None),
            engine_session: Cell::new(0),
            pending_end_session: Cell::new(0),
            state: RefCell::new(InputState::default()),
            composition: Rc::new(RefCell::new(None)),
            left_context: Rc::new(RefCell::new(None)),
            da_atom: Cell::new(0),
            showing: Cell::new(false),
            candidate_ui,
            cand_state,
            behavior_outbox,
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
            live_enabled: Cell::new(true),
            llm_enabled: Cell::new(false),
            typo_enabled: Cell::new(false),
            default_direct_applied: Cell::new(false),
            langbar_is_direct: Rc::new(Cell::new(false)),
            langbar_ephemeral: Rc::new(Cell::new(false)),
            langbar_sink: Rc::new(RefCell::new(None)),
            langbar_item: RefCell::new(None),
            langbar_on_toggle: Rc::new(RefCell::new(None)),
            mode_hud: std::cell::RefCell::new(crate::mode_hud::ModeHud::empty()),
            reading_monitor: std::cell::RefCell::new(crate::reading_monitor::ReadingMonitor::empty()),
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
            symbol_full_width: Cell::new(false),
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
                    flags, flags & TF_TMF_IMMERSIVEMODE != 0
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
                if let Ok(cookie) = unsafe { source.AdviseSink(&ITfThreadMgrEventSink::IID, &tmes) } {
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
        self.number_full_width.set(s.number.full_width);
        self.punctuation_full_width.set(s.punctuation.full_width);
        self.symbol_full_width.set(s.symbol.full_width);
        self.reading_monitor_enabled.set(s.reading_monitor.enabled);
        self.reading_monitor_accumulate.set(s.reading_monitor.accumulate);
        self.reading_monitor_max_chars.set(s.reading_monitor.effective_max_chars());
        self.shift_latin_compose.set(
            crate::key_event_sink::shift_latin_is_compose(&s.shift_latin.mode));
        self.ephemeral_enabled.set(s.ephemeral.enabled);
        self.keymap.set(crate::keymap::Keymap::from_settings(&s));

        // SP5/keymap: モードトグル/再変換/フィードバックの PreservedKey を keymap から登録する。
        // 既定は従来どおり JIS+US 二重登録、明示バインドは単一登録、無効は未登録。
        // 登録の成否は per-key でログに残す(実機で「無変換が効かない」「カスタムキーが
        // OS に拒否された(bare 0x1C の 0x80040506 前例)」を診断するため)。
        {
            let regs = crate::keymap::build_preserved_regs(&self.keymap.get(), s.feedback.enabled);
            for r in &regs {
                let pk = TF_PRESERVEDKEY { uVKey: r.vk, uModifiers: r.modifiers };
                let d: Vec<u16> = r.desc.encode_utf16().collect();
                let res = unsafe { ksm.PreserveKey(tid, &r.guid, &pk, &d) };
                let hr = res.as_ref().err().map(|e| e.code().0 as u32).unwrap_or(0);
                tip_log(&format!(
                    "ev=preservekey desc={:?} vk={:#04x} mods={:#x} ok={} hr={hr:#010x}",
                    r.desc, r.vk, r.modifiers, res.is_ok()
                ));
            }
            *self.preserved_regs.borrow_mut() = regs;
        }

        // 2) 表示属性 GUID を登録して atom を保持する（失敗しても致命的ではない）。
        unsafe {
            if let Ok(cat) =
                CoCreateInstance::<_, ITfCategoryMgr>(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)
            {
                if let Ok(atom) = cat.RegisterGUID(&GUID_DISPLAY_ATTRIBUTE) {
                    self.da_atom.set(atom);
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
            self.langbar_is_direct.set(self.is_direct_mode());
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
                // 捕まえ、呼ばれるたびに cast_object_ref で &TextService_Impl を復元して正規トグル経路
                // toggle_conversion_mode(None) を叩く（is_direct の直接反転は禁止＝実 compartment 追従）。
                // ctx=None なので HUD は既定座標に出る（メニュー操作時は実キャレット位置が取れない）。
                // 参照循環（closure→COM 参照→TextService→この Rc→closure）は Deactivate で None にして断つ。
                // to_interface は #[implement] に列挙した interface のみ可（ComObjectInterface 境界）。
                // このオブジェクトは ITfTextInputProcessorEx を実装しているのでそれを owned で握る。
                let self_com: ITfTextInputProcessorEx = self.to_interface();
                *self.langbar_on_toggle.borrow_mut() = Some(Box::new(move || {
                    // cast_object_ref は QI 相当で &TextService_Impl を返す（0.62 の supported API）。
                    // toggle_conversion_mode は TextService_Impl のメソッドなのでこの参照から呼べる。
                    if let Ok(ts) = self_com.cast_object_ref::<crate::text_service::TextService>() {
                        ts.toggle_conversion_mode(None);
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
        // おり(set は init の false とここだけ)、compute_hots / start_llm_convert ガード /
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
        if crate::conversion_mode::should_apply_default_direct(s.default_direct, self.default_direct_applied.get()) {
            self.apply_default_direct();
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
        // advise を解除し、保持状態を破棄する。
        if let Some(tm) = self.thread_mgr.borrow().as_ref() {
            if let Ok(ksm) = tm.cast::<ITfKeystrokeMgr>() {
                unsafe {
                    let _ = ksm.UnadviseKeyEventSink(self.tid.get());
                }
                // SP5/keymap: Activate で登録した実物(preserved_regs)を対称に解除する。
                for r in self.preserved_regs.borrow().iter() {
                    let pk = TF_PRESERVEDKEY { uVKey: r.vk, uModifiers: r.modifiers };
                    let _ = unsafe { ksm.UnpreserveKey(&r.guid, &pk) };
                }
                self.preserved_regs.borrow_mut().clear();
            }
            // SP5/US: Activate で追加した言語バーモードインジケータを除去する（AddItem と対）。
            // 右クリックメニュー用トグル closure を落とす（RemoveItem と対）。closure は自身の
            // COM 参照を owned で保持しており、TextService がこの Rc を保持するため相互参照（循環）
            // になっている。ここで None にして循環を断ち、Activate/Deactivate 往復でのリークを防ぐ。
            *self.langbar_on_toggle.borrow_mut() = None;
            if let Some(item) = self.langbar_item.borrow_mut().take() {
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
                        if tmes_cookie != 0 { let _ = source.UnadviseSink(tmes_cookie); }
                        if tfs_cookie != 0 { let _ = source.UnadviseSink(tfs_cookie); }
                    }
                }
            }
        }
        // C-2: コンポジション進行中に IME 切替（Deactivate）されると、生きている ITfComposition が
        // 孤児化する。ITfComposition は作成時に sink(=TextService への ITfCompositionSink 強参照)を
        // 保持するため、これを片付けないと TextService と DLL_REF がリークし、文書側にも宙ぶらりんの
        // preedit が残る（OnCompositionTerminated は composition を片付けるのに Deactivate は未対応だった）。
        // current_context が生きていれば CancelComposition を同期実行して文書から打ちかけを除去し、
        // いずれにせよ composition の Rc を必ず None に戻して sink 参照（強参照）を解放する。
        // ここは tid / current_context がまだ有効なうちに行う（後段でいずれもクリアされる）。
        if self.composition.borrow().is_some() {
            let ctx = self.current_context.borrow().clone();
            if let Some(ctx) = ctx {
                if self.reconverting.get() {
                    // 再変換中はユーザの**既存テキスト**の上に composition が張られている。
                    // do_cancel(CancelComposition) は range を空文字で潰す＝元テキストを消すため、
                    // RestoreText で元ラテンを書き戻す cancel_reconvert を使う（Esc / Behavior::Abort
                    // と同じ取消経路）。これを怠ると再変換中の IME 切替でユーザの原文が消失する。
                    self.cancel_reconvert(&ctx);
                } else {
                    self.do_cancel(&ctx);
                }
            }
            // context 無し等で edit session が走らなかった場合の保険。sink 参照を必ず断つ。
            *self.composition.borrow_mut() = None;
        }

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
        // 入力状態を全て畳む（raw/composing/phase）。従来は set_awaiting_llm(false) だけで
        // raw/composing を残していたが、それだと再活性化後の初打鍵で needs_session_reseed が
        // 「session==0 かつ raw 非空＝合成途中の喪失」と誤認し、上の do_cancel で取消済みの
        // テキストが新セッションへリプレイされて復活する（2026-07-07 レビュー I-1 の偽陽性
        // リプレイ）。reset() は phase=Composing も含む＝AwaitingLlm 居残り防止も従来どおり。
        self.state.borrow_mut().reset();
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
        // 品質ループ③: 直前確定バッファも持ち越さない（再活性化後の Ctrl+変換が
        // 非活性前の古い確定を記録しないように）。
        *self.last_commit.borrow_mut() = None;
        // 確定取消: armed も必ず落とす（last_commit クリアと並記 — 再活性化後に
        // 非活性前の武装状態が Ctrl+Backspace を誤発火させないように）。
        self.undo_armed.set(false);
        // ephemeral かな: 非活性化（IME 切替/シャットダウン）でも direct へ復帰する。thread_mgr が
        // まだ有効な（下で None にする前の）この時点で呼ぶ必要がある。
        self.exit_ephemeral_to_direct(None);
        // SP7: default_direct_applied は **意図的にリセットしない**。リセットすると
        // IME 切替の往復（Deactivate→Activate）のたびに半角英数へ再初期化してしまい、
        // ユーザが無変換でひらがなへ戻した選択を巻き戻す。1度だけ適用を貫く。

        // 候補ウィンドウを隠す（presenter なら UIElement も EndUIElement で畳む）。
        self.candidate_ui.borrow_mut().hide();
        self.showing.set(false);
        // SP6a: 非活性化後に Behavior が来ても dangling self を触らせない（UAF 防止）。
        // ui_mgr も手放して保持していた COM 参照を解放する。
        BEHAVIOR_TS.with(|c| c.set(std::ptr::null()));
        self.candidate_ui.borrow_mut().set_ui_mgr(None);

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

impl ITfTextInputProcessorEx_Impl for TextService_Impl {
    fn ActivateEx(&self, ptim: Ref<'_, ITfThreadMgr>, tid: u32, _dwflags: u32) -> Result<()> {
        // 拡張活性化は通常の Activate に委譲する。
        self.Activate(ptim, tid)
    }
}

impl ITfCompositionSink_Impl for TextService_Impl {
    fn OnCompositionTerminated(
        &self,
        _ecwrite: u32,
        pcomposition: Ref<'_, ITfComposition>,
    ) -> Result<()> {
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

        *self.composition.borrow_mut() = None;
        self.state.borrow_mut().reset();
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
        self.candidate_ui.borrow_mut().hide();
        self.reading_monitor.borrow_mut().hide();
        // composition が消えた以上、保持していた context/読みも捨てる。残すと遅延変換が死んだ
        // composition の context へ preedit を張り直そうとする（無駄な StartComposition を誘発する）。
        self.disarm_debounce();
        *self.current_context.borrow_mut() = None;
        self.live_text.borrow_mut().clear();
        // SP5: 再変換候補の表示中に放棄された場合（フォーカス喪失等）も reconverting を必ず落とす。
        // 落とさないと start_reconvert の再入ガードに居残り、以降の再変換が不能になる
        // （候補キーは showing=false で食わず解除経路に到達しない）。
        self.reconverting.set(false);
        self.reconvert_original.borrow_mut().clear();
        // ephemeral かな: 合成が畳まれた以上 direct へ復帰する（OnCompositionTerminated など
        // フォーカス変化を伴わない放棄経路でも残留させない。非 ephemeral 時は no-op）。
        self.exit_ephemeral_to_direct(None);
    }
}

impl ITfThreadMgrEventSink_Impl for TextService_Impl {
    fn OnInitDocumentMgr(&self, _pdim: Ref<'_, ITfDocumentMgr>) -> Result<()> { Ok(()) }
    fn OnUninitDocumentMgr(&self, _pdim: Ref<'_, ITfDocumentMgr>) -> Result<()> { Ok(()) }
    fn OnPushContext(&self, _pic: Ref<'_, ITfContext>) -> Result<()> {
        self.password_ctx_key.set(0); // Spec2: context 切替で password キャッシュ無効化（ABA 対策・I-3）
        Ok(())
    }
    fn OnPopContext(&self, _pic: Ref<'_, ITfContext>) -> Result<()> {
        self.password_ctx_key.set(0); // Spec2: context 切替で password キャッシュ無効化（ABA 対策・I-3）
        Ok(())
    }

    /// フォーカスが別ドキュメント（別ウィンドウ/アプリ）へ移ったとき、進行中の合成＋エンジン
    /// セッションを放棄する。別窓クリックでホストが live preedit を確定しても
    /// `OnCompositionTerminated` を呼ばないことがあり、その場合エンジンの読みが居残って次入力へ
    /// 連結される（フォーカス喪失データ残留。例: にほんご→別窓→aiueo で 日本語日本語あいうえお）。
    /// 自ドキュメントへ戻る/留まる・進行中状態が無い・部分確定中は何もしない。
    fn OnSetFocus(
        &self,
        pdimfocus: Ref<'_, ITfDocumentMgr>,
        _pdimprevfocus: Ref<'_, ITfDocumentMgr>,
    ) -> Result<()> {
        self.password_ctx_key.set(0); // Spec2: フォーカス切替で password キャッシュ無効化（ABA 対策・I-3）
        let has_active_input =
            self.engine_session.get() != 0 || self.composition.borrow().is_some();
        // 新フォーカス先（NULL=アプリがバックグラウンドへ）と、自分の合成があるドキュメントを
        // COM 同一性で比較する。current_context は borrow を即解放してから GetDocumentMgr を呼ぶ。
        let new_focus: Option<ITfDocumentMgr> = pdimfocus.ok().ok().cloned();
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
            self.disarm_undo();
            // ephemeral かな: 別窓へフォーカスが動いた＝押し忘れの言語モードを持ち越さない
            // （thread compartment を direct へ。ctx 無しでも冪等に呼べる）。
            self.exit_ephemeral_to_direct(None);
        }
        Ok(())
    }
}

impl ITfThreadFocusSink_Impl for TextService_Impl {
    fn OnSetThreadFocus(&self) -> Result<()> { Ok(()) }

    /// 前面（スレッド）フォーカスを失った＝別アプリ/プロセスへ切替わった。クロスプロセスの
    /// フォーカス喪失はこの通知が主経路（`ITfThreadMgrEventSink::OnSetFocus` はスレッド内 doc
    /// フォーカス変化のみで、別プロセス前面化では届かないことがある）。進行中の合成があれば
    /// 放棄リセットを焚き、ホストが `OnCompositionTerminated` を呼ばずに preedit を確定/破棄しても
    /// エンジンの読みが居残らないようにする。スレッドが前面を失った時点で自ドキュメントは
    /// 非フォーカスなので、should_abandon の focus_is_our_doc=false 相当で判定する。
    fn OnKillThreadFocus(&self) -> Result<()> {
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
            let st = self.state.borrow();   // 1回の borrow で composing/awaiting_llm を読む
            st.composing || st.awaiting_llm()
        } || self.showing.get() || self.llm_slot.borrow().is_some();
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
                    tip_log(&format!("ev=resume_reconnect mode=composing_keep gen={gen}"));
                }
            }
        }
    }

    /// StartSession して client/session を確定保持する。失敗時は client を None のまま。
    fn start_and_store(&self, mut c: EngineClient) {
        match timed_request(&mut c, &Request::StartSession, IPC_TIMEOUT_FAST, "start_session") {
            Ok(Response::Session { session, proto }) => {
                // version handshake は接続確立時（fresh StartSession）にだけ効かせる。proto はエンジン
                // プロセスの属性で、一度確立した接続の途中では変わらないため、既存接続に StartSession を
                // 貼り直す ensure_session 側では判定しない（この start_and_store が全 fresh 接続経路の合流点）。
                match decide_handshake(proto, self.handshake_shutdown_attempted.get()) {
                    HandshakeAction::Accept => {
                        self.handshake_shutdown_attempted.set(false);
                        self.engine_session.set(session);
                        *self.client.borrow_mut() = Some(c);
                        tip_log(&format!("ev=engine_proto ok=true proto={PROTO_VERSION} (session={session})"));
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
                        let _ = timed_request(&mut c, &Request::Shutdown, IPC_TIMEOUT_FAST, "shutdown");
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
                        self.engine_reload_config();
                    }
                }
            }
            other => {
                tip_log(&format!("StartSession unexpected: {other:?}"));
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
        let req = build_reload_config(&s, key_plain.as_ref().map(|z| z.as_str()));
        let result = {
            let mut guard = self.client.borrow_mut();
            let Some(client) = guard.as_mut() else { return; };
            timed_request(client, &req, IPC_TIMEOUT_FAST, "reload_config")
        };
        match result {
            Ok(Response::Ok) => tip_log("ev=reload_config ok=true"),
            Ok(Response::Error { message }) => {
                // 応答は消費済み（交互性 OK）。旧エンジン等なので接続は維持する。
                tip_log(&format!("ev=reload_config ok=false reason={message}"));
            }
            Ok(other) => {
                // 予期しない応答型＝desync の兆候。安全側で接続を破棄する。
                tip_log(&format!("ev=reload_config unexpected resp={other:?} -> drop"));
                self.drop_engine();
            }
            Err(e) => {
                // タイムアウト/切断: 応答未消費で late frame 滞留 → 恒常 1-off desync を防ぐため破棄。
                tip_log(&format!("ev=reload_config err={e:?} -> drop"));
                self.drop_engine();
            }
        }
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
            self.reconnect_backoff.borrow().full_attempt_allowed(std::time::Instant::now()),
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
        if self.client.borrow().is_some() { return; }
        let pipe = self.engine_pipe_name(); // stable per-session name (Task 1)
        let now = std::time::Instant::now();

        // A7: クールダウン中はフルコース（spawn+200/50/400ms 接続）を止め、無償の一発プローブだけ許す。
        // 半死（session 確立失敗）検出後はプローブも満了まで停止する（probe_suppressed）。
        // borrow はブロックで閉じてから start_and_store/borrow を呼ぶ（二重借用 panic 回避）。
        if !self.reconnect_backoff.borrow().full_attempt_allowed(now) {
            if !self.reconnect_backoff.borrow().probe_allowed() { return; }
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
                    tip_log(&format!("ev=engine_backoff kind=session n={}", b.failures()));
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
            if self.client.borrow().is_some() { self.reconnect_backoff.borrow_mut().reset(); return; }
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
                    if self.client.borrow().is_some() { self.reconnect_backoff.borrow_mut().reset(); return; }
                }
                match engine_exe_path() {
                    Some(exe) => {
                        tip_log(&format!("ev=engine_exe path={} exists={}", exe.display(), exe.exists()));
                        let s = settings::load();
                        let key_plain = if s.llm.api_key_dpapi.is_empty() { None }
                            else { settings::dpapi::decrypt(&s.llm.api_key_dpapi) };
                        let env_map = settings::resolve_env_map(&s, key_plain.as_ref().map(|z| z.as_str()), |k| std::env::var(k).ok());
                        match spawn_engine_hidden(&exe, &pipe, &env_map) {
                            Some(child) => {
                                tip_log(&format!("ev=engine_spawn pid={} ok=true env_keys={}", child.id(), env_map.len()));
                                *self.engine_child.borrow_mut() = Some(child);
                                // 起動直後は listening まで間があるので短く一度だけ。ダメでも degrade（次打鍵の 1) で拾う）。
                                // 成功しても早期 return せず fall-through で末尾判定に達する（M-4）。
                                // cold start ①: spawn→connect 成功までの所要（engine 側 stage=listening と突き合わせる）。
                                let started = std::time::Instant::now();
                                if let Ok(c) = EngineClient::connect_to(&pipe, Duration::from_millis(400)) {
                                    tip_log(&format!("ev=coldstart stage=spawn_to_connect ms={}", started.elapsed().as_millis()));
                                    tip_log("connected after spawn");
                                    connected_once = true;
                                    self.start_and_store(c);
                                } else {
                                    tip_log("spawn ok, not yet listening -> degrade this keystroke");
                                }
                            }
                            None => { tip_log("ev=engine_spawn pid=0 ok=false"); tip_log("ev=degraded reason=spawn_failed"); }
                        }
                    }
                    None => { tip_log("engine exe path not found"); tip_log("ev=degraded reason=spawn_failed"); }
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
            if connected_once { b.on_session_failure(end); } else { b.on_connect_failure(end); }
            tip_log(&format!("ev=engine_backoff kind={} n={}",
                             if connected_once { "session" } else { "connect" }, b.failures()));
        }
    }

    /// エンジン接続が壊れたとみなして破棄する。client/session を捨て、起動フラグも戻すので、
    /// 次の打鍵の `ensure_engine` で再接続（必要なら再起動）して復帰できる。
    /// 注意: 呼び出し側は `self.client` の borrow を一切持っていないこと（二重借用 panic 防止）。
    fn drop_engine(&self) {
        *self.client.borrow_mut() = None;
        self.engine_session.set(0);
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
                timed_request(client, &Request::StartSession, IPC_TIMEOUT_FAST, "start_session")
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

    /// `text` を挿入して読みを得る。client/session が無い・失敗なら None（劣化）。
    /// 通常の打鍵は 1 文字だが、drop_engine 後の再接続でセッションを張り直した直後は
    /// `state.raw` 全体を 1 回で送り直す（ensure_session のドキュメント参照）。
    /// エンジン側 insert は文字列単位（roman2kana は逐次挿入とバッチ挿入で等価、かなは素通し）。
    /// 失敗時は接続を破棄して次打鍵で復帰できるようにする。
    /// borrow は `result` ブロック内で完結させ、drop 後に `drop_engine` を呼ぶ（二重借用 panic 防止）。
    pub(crate) fn engine_insert(&self, text: &str, style: InsertStyle) -> Option<String> {
        // INV1: pending 中は送信前にドレイン。解消できなければ要求は送らず劣化継続。
        match self.prepare_send("insert", IPC_TIMEOUT_FAST) {
            DrainOutcome::Proceed => {}
            DrainOutcome::StillPending | DrainOutcome::Dropped => return None,
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
                tip_log(&format!("insert('{text}') failed: {other:?}"));
                tip_log("ev=degraded reason=insert_failed");
                self.drop_engine();
                None
            }
        }
    }

    /// 変換候補を要求する。失敗なら None（劣化）し接続を破棄する。
    pub(crate) fn engine_convert(&self) -> Option<Vec<String>> {
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
                &Request::Convert { session, left_context },
                IPC_TIMEOUT_CONVERT,
                "convert",
            );
            if resume_probe {
                tip_log(&format!(
                    "ev=resume_first_convert op=convert ms={} ok={}",
                    started.elapsed().as_millis(), r.is_ok()
                ));
            }
            r
        };
        match result {
            Ok(Response::Candidates { candidates }) => Some(candidates),
            other => {
                tip_log(&format!("convert failed: {other:?}"));
                tip_log("ev=degraded reason=convert_failed");
                self.drop_engine();
                None
            }
        }
    }

    /// 修正変換候補を要求する（Tab）。失敗なら None（劣化）し接続を破棄する。手動キー起動なので
    /// A7 の resume_probe 計測（復帰後最初の変換系 op）は対象外（engine_convert と異なり計測しない）。
    pub(crate) fn engine_typo_convert(&self) -> Option<Vec<String>> {
        let session = self.engine_session.get();
        let left_context = self.left_context.borrow().clone();
        let result = {
            let mut guard = self.client.borrow_mut();
            let client = guard.as_mut()?;
            timed_request(
                client,
                &Request::TypoConvert { session, left_context },
                IPC_TIMEOUT_CONVERT,
                "typo_convert",
            )
        };
        match result {
            Ok(Response::Candidates { candidates }) => Some(candidates),
            other => {
                tip_log(&format!("typo_convert failed: {other:?}"));
                tip_log("ev=degraded reason=typo_convert_failed");
                self.drop_engine();
                None
            }
        }
    }

    /// 選択かな表層を1往復で再変換し候補を得る（SP5 step-6）。失敗なら None（劣化）し接続を破棄する。
    pub(crate) fn engine_reconvert_surface(&self, surface: &str) -> Option<Vec<String>> {
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
                &Request::Reconvert { session, surface: surface.to_string(), left_context },
                IPC_TIMEOUT_CONVERT,
                "reconvert",
            );
            if resume_probe {
                tip_log(&format!(
                    "ev=resume_first_convert op=reconvert ms={} ok={}",
                    started.elapsed().as_millis(), r.is_ok()
                ));
            }
            r
        };
        match result {
            Ok(Response::Candidates { candidates }) => Some(candidates),
            other => {
                tip_log(&format!("reconvert_surface failed: {other:?}"));
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
        let session = self.engine_session.get();
        let result = {
            let mut guard = self.client.borrow_mut();
            let client = guard.as_mut()?;
            timed_request(
                client,
                &Request::Commit { session, index: index as u32 },
                IPC_TIMEOUT_FAST,
                "commit",
            )
        };
        match result {
            Ok(Response::Committed { text, reading }) => Some((text, reading)),
            // エンジンが確定を拒否（未知セッション/キャッシュ無し/index 範囲外/stale）= 想定内の劣化。
            // convert/insert と違い commit の拒否は接続不良ではないので drop_engine しない
            // （部分確定で保持中の生きたセッションを巻き添えで壊さない）。None を返し全確定へフォールバック。
            Ok(Response::Error { message }) => {
                tip_log(&format!("commit declined: {message}"));
                None
            }
            other => {
                tip_log(&format!("commit failed: {other:?}"));
                tip_log("ev=degraded reason=commit_failed");
                self.drop_engine();
                None
            }
        }
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
                &Request::LiveConvert { session, seq, left_context, auto_commit },
                IPC_TIMEOUT_LIVE,
                "live_convert",
            );
            if resume_probe {
                tip_log(&format!(
                    "ev=resume_first_convert op=live_convert ms={} ok={}",
                    started.elapsed().as_millis(), r.is_ok()
                ));
            }
            r
        };
        match result {
            Ok(Response::LiveResult { seq: _resp_seq, text, reading, committed }) => {
                Some((text, reading, committed))
            }
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
                tip_log(&format!("live_convert failed: {other:?}"));
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
        if !self.live_enabled.get() {
            return;
        }
        DEBOUNCE_TS.with(|p| p.set(self as *const TextService_Impl));
        let id = unsafe { SetTimer(None, 0, DEBOUNCE_MS, Some(debounce_timer_proc)) };
        self.debounce_timer.set(id);
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

        // 2) キャレット手前を既知長ぴったり読み戻し、text にバイト一致したときだけ composition 化する。
        //    不一致・非空選択・読み取り失敗は何も書かない（do-no-harm、ReconvertStart :318-329 と同型）。
        let matched: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let sink: ITfCompositionSink = self.to_interface();
        let sess: ITfEditSession = CommitUndoStart {
            context: ctx.clone(),
            sink,
            composition: Rc::clone(&self.composition),
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
        if !*matched.borrow() {
            // 照合失敗（文書を一切書いていない）。武装を残さず離脱する（I-6）。
            tip_log("ev=commit_undo_skip reason=text_mismatch");
            self.disarm_undo();
            return;
        }

        // 3) 一致。Esc 復元用に確定文字列を原文としてセットし、バッファを消費する。
        //    以降 composition は開いている＝連打は composition ガードで no-op（armed は維持でよい）。
        *self.reconvert_original.borrow_mut() = text.clone();
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
        // 接続を取り出して move（無ければ劣化＝何もしない）。
        let client = match self.client.borrow_mut().take() {
            Some(c) => c,
            None => { tip_log("ev=llm_no_client"); return; }
        };
        let session = self.engine_session.get();
        *self.pre_llm_text.borrow_mut() = self.live_text.borrow().clone();
        *self.current_context.borrow_mut() = Some(ctx.clone());
        // 修正候補窓が出ていれば閉じる（その上に「変換中…」を出さない）。input_char と同じ片付け。
        if self.showing.get() {
            self.candidate_ui.borrow_mut().hide();
            self.showing.set(false);
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
        spawn_llm_worker(client, session, seq, left_context, slot, Duration::from_secs(30));
        self.arm_llm_poll();
        tip_log(&format!("ev=llm_request seq={seq} session={session}"));
    }

    fn arm_llm_poll(&self) {
        self.disarm_llm_poll();
        LLM_TS.with(|p| p.set(self as *const TextService_Impl));
        let id = unsafe { SetTimer(None, 0, LLM_POLL_MS, Some(llm_poll_proc)) };
        self.llm_poll_timer.set(id);
    }

    fn disarm_llm_poll(&self) {
        let id = self.llm_poll_timer.replace(0);
        if id != 0 { unsafe { let _ = KillTimer(None, id); } }
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
            st.bump_llm_seq();          // 後から届く結果を stale として確実に捨てる
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
        self.llm_started.get().map(|t| t.elapsed() >= LLM_TIMEOUT).unwrap_or(false)
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
                if let Some(c) = o.client { *self.client.borrow_mut() = Some(c); }
                self.flush_pending_end_session(); // 合成が in-flight 中に終了していたら保留 EndSession を送る
                self.state.borrow_mut().mark_good(&text);
                *self.live_text.borrow_mut() = text.clone();
                if let Some(ctx) = ctx { self.run_preedit(&ctx, &text); }
                tip_log(&format!("ev=llm_applied seq={}", o.seq));
            }
            Ok(_) => {
                // 古い seq（Esc等）or 空 → 接続を戻し pre-LLM へ復元。
                if let Some(c) = o.client { *self.client.borrow_mut() = Some(c); }
                self.flush_pending_end_session(); // 合成が in-flight 中に終了していたら保留 EndSession を送る
                self.restore_pre_llm(ctx);
                tip_log(&format!("ev=llm_stale_or_empty seq={} current={}", o.seq, current));
            }
            Err(msg) => {
                // 失敗 → 接続は drop（戻さない）。次操作で再接続。pre-LLM へ復元。
                self.drop_engine();
                self.restore_pre_llm(ctx);
                tip_log(&format!("ev=llm_failed msg={msg}"));
            }
        }
    }

    fn restore_pre_llm(&self, ctx: Option<ITfContext>) {
        let pre = self.pre_llm_text.borrow().clone();
        // last_good は live_text でなく「実際に画面へ出す文字列」で記録する — pre が
        // 空のとき表示は last_reading であり、劣化フォールバックの素材はそちらが正しい。
        let shown = if pre.is_empty() { self.last_reading.borrow().clone() } else { pre.clone() };
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
            return;
        }
        let ctx = match self.current_context.borrow().clone() {
            Some(c) => c,
            None => return,
        };
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
                self.run_preedit(&ctx, &text);
            }
        }
    }

    /// バックスペースを送って更新後の読みを得る。失敗なら None（劣化）し接続を破棄する。
    pub(crate) fn engine_backspace(&self) -> Option<String> {
        let session = self.engine_session.get();
        let result = {
            let mut guard = self.client.borrow_mut();
            let client = guard.as_mut()?;
            timed_request(client, &Request::Backspace { session }, IPC_TIMEOUT_FAST, "backspace")
        };
        match result {
            Ok(Response::Reading { reading }) => Some(reading),
            other => {
                tip_log(&format!("backspace failed: {other:?}"));
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
        let result = {
            let mut guard = self.client.borrow_mut();
            guard.as_mut().map(|client| {
                timed_request(client, &Request::EndSession { session }, IPC_TIMEOUT_FAST, "end_session")
            })
        };
        self.engine_session.set(0);
        match result {
            Some(Ok(_)) => {}
            Some(Err(e)) => {
                tip_log(&format!("end_session failed: {e:?}"));
                tip_log("ev=degraded reason=end_session_failed");
                self.drop_engine();
            }
            None => {
                // client は LLM ワーカへ move 済みで今は送れない。id を保留し、復帰時に EndSession を送る。
                // さもないと engine 側にセッションが取り残され、ConversionService の stopComposition も
                // 永久に走らない（sessions.isEmpty にならない）。
                self.pending_end_session.set(session);
            }
        }
    }

    /// client 復帰後（on_llm_outcome）に、保留していた EndSession を送って取り残しを掃除する。
    /// Bug 1: engine_end_session と同様、失敗時は接続を破棄して応答フレームの滞留を防ぐ。
    /// borrow は `result` ブロック内で完結させ、drop 後に `drop_engine` を呼ぶ（二重借用 panic 防止）。
    fn flush_pending_end_session(&self) {
        let session = self.pending_end_session.replace(0);
        if session == 0 {
            return;
        }
        let result = {
            let mut guard = self.client.borrow_mut();
            guard.as_mut().map(|client| {
                timed_request(client, &Request::EndSession { session }, IPC_TIMEOUT_FAST, "end_session")
            })
        };
        if let Some(Err(e)) = result {
            tip_log(&format!("flush end_session failed: {e:?}"));
            tip_log("ev=degraded reason=end_session_failed");
            self.drop_engine();
        }
    }

    /// 下線属性 atom を内包した VARIANT を作る（atom 未登録なら i32(0)）。
    fn da_variant(&self) -> VARIANT {
        VARIANT::from(self.da_atom.get() as i32)
    }

    /// preedit を `text` にする編集セッションを同期実行する。失敗は no-op。
    pub(crate) fn run_preedit(&self, ctx: &ITfContext, text: &str) {
        let sink: ITfCompositionSink = self.to_interface();
        let session_obj: ITfEditSession = StartOrUpdatePreedit {
            context: ctx.clone(),
            text: HSTRING::from(text),
            sink,
            da_variant: self.da_variant(),
            composition: Rc::clone(&self.composition),
            left_context_out: Rc::clone(&self.left_context),
            _guard: ComObjectGuard::new(),
        }
        .into();
        unsafe {
            let _ = ctx.RequestEditSession(
                self.tid.get(),
                &session_obj,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0),
            );
        }
        // 読みモニタ: preedit を書いた直後に表示を同期する（打鍵/ライブ結果/部分確定の
        // 全経路がここを通る＝フックの一点化。確定/取消系は run_preedit を通らないので
        // 各サイトが明示 hide する）。
        self.update_reading_monitor(ctx);
    }

    /// composition を確定文字列 `text` で確定する編集セッションを同期実行する。失敗は no-op。
    pub(crate) fn do_commit(&self, ctx: &ITfContext, text: &str) {
        let session_obj: ITfEditSession = CommitText {
            context: ctx.clone(),
            text: HSTRING::from(text),
            composition: Rc::clone(&self.composition),
            _guard: ComObjectGuard::new(),
        }
        .into();
        unsafe {
            let _ = ctx.RequestEditSession(
                self.tid.get(),
                &session_obj,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0),
            );
        }
    }

    /// composition を確定せず終了する編集セッションを同期実行する。失敗は no-op。
    pub(crate) fn do_cancel(&self, ctx: &ITfContext) {
        let session_obj: ITfEditSession = CancelComposition {
            composition: Rc::clone(&self.composition),
            _guard: ComObjectGuard::new(),
        }
        .into();
        unsafe {
            let _ = ctx.RequestEditSession(
                self.tid.get(),
                &session_obj,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0),
            );
        }
        // U9: 合成終了（取消）— 次 composition の再捕捉まで前文書の左文脈を残さない。
        *self.left_context.borrow_mut() = None;
        self.monitor_committed_reading.borrow_mut().clear();
    }

    /// 候補表示中に選択を `delta` だけ動かす（`move_selection` が循環＝端で巻き戻る）。
    /// 選択の唯一の真実源は cand_state（`move_selection`→presenter→cand_state が更新）。
    /// `ev=candidate_move` を記録する。
    /// Space（前進）と上下矢印（↓=前進 / ↑=後退）で共有し、両経路が乖離しないようにする。
    pub(crate) fn move_candidate(&self, delta: i32) {
        self.candidate_ui.borrow_mut().move_selection(delta);
        let sel = self.candidate_ui.borrow().selected();
        tip_log(&format!("ev=candidate_move sel={sel}"));
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
        let max_chars = self.reading_monitor_max_chars.get();
        let reading = if self.reading_monitor_accumulate.get() {
            crate::reading_monitor::compose_monitor_text(
                &self.monitor_committed_reading.borrow(),
                &self.last_reading.borrow(),
                crate::reading_monitor::display_bound(max_chars),
            )
        } else {
            self.last_reading.borrow().clone()
        };
        if !visible || reading.is_empty() {
            self.reading_monitor.borrow_mut().hide();
            return;
        }
        // caret_point ではなく専用照会を使う理由（ev=caret ログ量産回避）は従来と同じ。
        // 矩形が取れないフレームは None を渡し、窓側 plan_anchor が
        // 表示中=位置保持 / 非表示=既定座標 に振り分ける。
        let anchor = self
            .query_monitor_anchor_rect(ctx)
            .and_then(crate::candidate_window::caret_rect_to_anchor);
        let theme = self.appearance.borrow_mut().current_theme();
        self.reading_monitor.borrow_mut().show_or_update(&reading, anchor, max_chars, theme);
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

    /// キャレットアンカー（スクリーン座標）を返す。`ITfContextView::GetTextExt` で実キャレット
    /// 矩形を読み、その左下（文字を覆わない位置）＋上端（画面下端フリップ用）を返す。
    /// 取得できない場合（レイアウト未確定・view 無し・セッション拒否など）は既定座標
    /// `DEFAULT_CARET_POS` へ劣化する（旧 MVP 固定値。caret_top 不明＝フリップなし）。
    /// 候補窓（ライブ変換/再変換）とモード HUD の両方がこのアンカーを使う。
    pub(crate) fn caret_point(&self, ctx: &ITfContext) -> crate::candidate_window::CaretAnchor {
        let rect = self.query_caret_rect(ctx);
        let anchor = rect
            .and_then(crate::candidate_window::caret_rect_to_anchor)
            .unwrap_or(DEFAULT_CARET_POS);
        // 診断: GetTextExt が実矩形を返したか／既定アンカーへ劣化したか＋最終アンカー座標。
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
    pub(crate) fn is_direct_mode(&self) -> bool {
        crate::conversion_mode::is_direct(self.conversion_mode_value())
    }

    /// conversion-mode をひらがな⇄半角英数でトグルする（NATIVE ビット反転）。
    /// `ctx` は HUD をキャレット近傍へ出すための生きた context（OnPreservedKey の pic）。
    /// 取れない呼び出し元は `None` を渡す＝HUD は既定座標に出る。
    pub(crate) fn toggle_conversion_mode(&self, ctx: Option<&ITfContext>) {
        // 軽微1: キー長押しのオートリピートで OnPreservedKey が連続到達しても、直近トグルから
        // MODE_TOGGLE_REPEAT_GUARD 未満なら無視する（モードが偶奇でフリッカするのを防ぐ）。
        // 兄弟の再変換が reconverting ラッチで連射を自衛しているのに倣った自衛ガード。
        let now = std::time::Instant::now();
        let elapsed = self.last_mode_toggle.get().map(|t| now.duration_since(t));
        if is_toggle_repeat(elapsed, MODE_TOGGLE_REPEAT_GUARD) {
            tip_log("ev=mode_toggle skip=repeat");
            return;
        }
        self.last_mode_toggle.set(Some(now));
        let Some(c) = self.conversion_compartment() else {
            tip_log("ev=mode_toggle skip=no_compartment");
            return;
        };
        let before = self.conversion_mode_value();
        let next = crate::conversion_mode::toggled(before);
        let v = VARIANT::from(next as i32);
        let tid = self.tid.get();
        // 診断: SetValue の成否と、書込直後に読み戻した実値を残す（write 失敗/上書きの切り分け）。
        let set_ok = unsafe { c.SetValue(tid, &v).is_ok() };
        let after = self.conversion_mode_value();
        tip_log(&format!(
            "ev=mode_toggle direct={} set_ok={set_ok} before={before:#06x} next={next:#06x} after={after:#06x} tid={tid}",
            crate::conversion_mode::is_direct(next)
        ));
        // 言語バーの あ/A 表示を新モードへ更新する。
        self.update_langbar_mode(crate::conversion_mode::is_direct(next), false, ctx);
    }

    /// 言語バーのモード表示を更新する。共有フラグ langbar_is_direct/langbar_ephemeral を反映し、
    /// システムの sink へ OnUpdate を投げて GetText（あ/A/あ˙）を再取得させる。sink 未 advise /
    /// 言語バー非表示なら no-op。`ctx` があれば HUD を実キャレット近傍へ、無ければ既定座標へ出す。
    /// `ephemeral`: ephemeral かなモード中（F8 等の一時トリガ中）かどうか。
    fn update_langbar_mode(&self, is_direct: bool, ephemeral: bool, ctx: Option<&ITfContext>) {
        self.langbar_is_direct.set(is_direct);
        self.langbar_ephemeral.set(ephemeral);
        if let Some(sink) = self.langbar_sink.borrow().as_ref() {
            unsafe {
                let _ = sink.OnUpdate(TF_LBI_TEXT | TF_LBI_STATUS | TF_LBI_ICON);
            }
        }
        // SP5/US: モード切替を あ/A の HUD でキャレット近傍に一瞬表示する（Win11 では langbar が
        // 出ないため）。生きた context があれば GetTextExt で実キャレット位置に出し、無ければ既定座標。
        let anchor = match ctx {
            Some(ctx) => self.caret_point(ctx),
            None => DEFAULT_CARET_POS,
        };
        // Task 7: 表示のたびに settings の mtime とダークモードを再評価した Theme を渡す
        // （設定変更・OS のライト/ダーク切替が次の flash から再起動なしで反映される）。
        let theme = self.appearance.borrow_mut().current_theme();
        self.mode_hud
            .borrow_mut()
            .flash(is_direct, ephemeral, anchor.x, anchor.y, theme);
    }

    /// SP7: 活性化時に conversion-mode を半角英数(直接入力)へ初期化する（設定 default_direct=true）。
    /// NATIVE と FULLSHAPE を落として半角を保証する（ROMAN 等は保存）。compartment 取得失敗時は劣化（何もしない）。
    /// Activate 内の tid/thread_mgr セット後に1度だけ呼ぶ＝以後のユーザ手動トグルは上書きしない。
    pub(crate) fn apply_default_direct(&self) {
        let Some(c) = self.conversion_compartment() else {
            tip_log("ev=default_direct skip=no_compartment");
            return;
        };
        let next = crate::conversion_mode::to_direct(self.conversion_mode_value());
        let v = VARIANT::from(next as i32);
        let tid = self.tid.get();
        let ok = unsafe { c.SetValue(tid, &v).is_ok() };
        tip_log(&format!("ev=default_direct applied ok={ok}"));
        // 言語バーの あ/A 表示を初期化後のモードへ更新する。Activate 時点では合焦 context が
        // 定まらず実キャレットが取れないため、HUD は既定座標へ出す（ctx=None）。
        self.update_langbar_mode(crate::conversion_mode::is_direct(next), false, None);
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
        self.ephemeral_kana.set(true);
        tip_log(&format!("ev=ephemeral_enter set_ok={ok} next={next:#06x}"));
        self.update_langbar_mode(false, true, ctx);
    }

    /// ephemeral かなモード復帰: `ephemeral_kana` が立っているときだけ compartment を
    /// direct へ SetValue ＋ フラグを落とす。立っていなければ no-op（畳んで確定/Esc/フォーカス喪失
    /// 等の全経路から冪等に呼べる。全経路配線は Task 3）。
    pub(crate) fn exit_ephemeral_to_direct(&self, ctx: Option<&ITfContext>) {
        if !self.ephemeral_kana.get() { return; }
        self.ephemeral_kana.set(false);
        if let Some(c) = self.conversion_compartment() {
            let next = crate::conversion_mode::to_direct(self.conversion_mode_value());
            let v = VARIANT::from(next as i32);
            let ok = unsafe { c.SetValue(self.tid.get(), &v).is_ok() };
            tip_log(&format!("ev=ephemeral_exit set_ok={ok} next={next:#06x}"));
            self.update_langbar_mode(true, false, ctx);
        } else {
            tip_log("ev=ephemeral_exit no_compartment(flag_only)");
        }
    }

    /// 再変換: 直前ラテン列(or 選択)を掴んで composition 化し、g1 リプレイで候補を出す。
    pub(crate) fn start_reconvert(&self, ctx: &ITfContext) {
        if self.reconverting.get() { return; }
        // 既に composition が開いている（native の打ちかけ等）なら再変換しない。
        // ReconvertStart は無条件で StartComposition しスロットを上書きするため、
        // ここで弾かないと既存 composition を EndComposition せず孤児化させてしまう。
        if self.composition.borrow().is_some() { return; }
        // 1) range 読み戻し＋非空 StartComposition（読んだラテンを out へ）。
        let out: Rc<RefCell<ReconvertCapture>> = Rc::new(RefCell::new(ReconvertCapture::default()));
        let sink: ITfCompositionSink = self.to_interface();
        let sess: ITfEditSession = ReconvertStart {
            context: ctx.clone(), sink,
            composition: Rc::clone(&self.composition),
            out: Rc::clone(&out),
            left_context_out: Rc::clone(&self.left_context),
            _guard: ComObjectGuard::new(),
        }.into();
        unsafe {
            let _ = ctx.RequestEditSession(self.tid.get(), &sess,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0));
        }
        let cap = out.borrow().clone();
        match cap.kind {
            ReconvertKind::None => return,                  // 対象なし（従来の早期 return）
            ReconvertKind::NonKana => {                       // 漢字/混在: 合成していない。無害に離脱。
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
                let _ = self.engine_insert(&reading, InsertStyle::Kana);
                self.engine_convert().unwrap_or_default()
            }
            ReconvertKind::Surface => self.engine_reconvert_surface(&text).unwrap_or_default(),
            ReconvertKind::None | ReconvertKind::NonKana => unreachable!(),
        };
        if cands.is_empty() {
            self.cancel_reconvert(ctx);
            return;
        }
        self.show_reconvert_candidates(ctx, &cands);
        // ev ログは呼び出し側で各自出す（I-3）。start_reconvert は本文（latin=）を含む従来ログを残す。
        let kind_str = if matches!(cap.kind, ReconvertKind::Surface) { "surface" } else { "latin" };
        tip_log(&format!("ev=reconvert_shown n={} kind={} latin={}", cands.len(), kind_str, text));
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
    pub(crate) fn cancel_reconvert(&self, ctx: &ITfContext) {
        let original = self.reconvert_original.borrow().clone();
        let sess: ITfEditSession = RestoreText {
            context: ctx.clone(),
            text: HSTRING::from(original.as_str()),
            composition: Rc::clone(&self.composition),
            _guard: ComObjectGuard::new(),
        }.into();
        unsafe {
            let _ = ctx.RequestEditSession(self.tid.get(), &sess,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0));
        }
        self.engine_end_session();
        self.reconverting.set(false);
        self.reconvert_original.borrow_mut().clear();
        self.candidate_ui.borrow_mut().hide();
        self.reading_monitor.borrow_mut().hide();
        self.showing.set(false);
        *self.current_context.borrow_mut() = None;
        // U9: 第4の合成終了経路（RestoreText）。ReconvertStart が書いた文書本文の左文脈を
        // ここで残すと、次 composition の edit session 拒否時に別文書の要求（特に外部 LLM）へ
        // 漏れる — do_cancel / commit_and_reset / reset_abandoned_composition と同じ規律で必ず消す
        // （最終レビュー Important-1）。
        *self.left_context.borrow_mut() = None;
        self.monitor_committed_reading.borrow_mut().clear();
        tip_log("ev=reconvert_cancel");
    }

    /// UU-4: ホストへ同期コールアウトしうる COM 区間（キー入口・タイマ発火など）をこれで包む。
    /// 区間中はゲートを立て、ホストが Behavior 経由で再入して `drain_behavior` を呼んでも借用
    /// 衝突 panic を起こさず保留させる。区間を抜けて借用が解放された安全点で、保留された
    /// Behavior を `flush_pending_behavior` が処理する。ネスト時は最外区間だけが flush する。
    /// `f` が panic しても RAII で区間フラグは必ず復元する（COM 入口の catch_com / タイマ proc の
    /// catch_unwind が panic を受ける前提）。
    pub(crate) fn guarded<T>(&self, f: impl FnOnce() -> T) -> T {
        // enter() は直前の値（=区間の中だったか）を返す。prev==false が最外区間。
        let prev = self.reentrancy.enter();
        let flag = InOperationGuard { gate: &self.reentrancy, prev };
        let r = f();
        drop(flag); // 区間フラグを復元してから（借用未保持の安全点で）flush する
        if !prev {
            // 最外区間だけが flush する（ネストした内側 guarded は外側に任せる）。
            self.flush_pending_behavior();
        }
        r
    }

    /// UU-4: 保留された Behavior 要求を、借用未保持の安全点で outbox が空になるまで実行する。
    /// `drain_behavior_inner` 実行中の再入も（ゲートにより）保留されるため、ループで回収する。
    pub(crate) fn flush_pending_behavior(&self) {
        while self.reentrancy.take_pending() {
            self.drain_behavior_inner();
        }
    }

    /// SP6a: UIElement Behavior(マウス/タッチ)発の確定/取消を実行する。notify→TLS 経由の入口。
    /// UU-4: TS 操作中（借用保持中）にホストが再入して呼んだ場合は、outbox を消費せず保留に
    /// 回して panic を避ける。借用未保持（純粋なマウス発など）なら即座に処理する。
    pub(crate) fn drain_behavior(&self) {
        // ゲートが「保留（区間中）」を指示したら outbox は消費せず、保留フラグだけ立てて返す
        // （区間離脱後の安全点＝guarded の flush で処理＝確定ロスト防止）。
        if self.reentrancy.signal_reentry(self.behavior_outbox.borrow().is_some()) {
            return;
        }
        // 借用未保持のトップレベル。inner が区間フラグを立てるので、その中の再入は保留され、
        // 続く flush ループで回収する。
        self.drain_behavior_inner();
        self.flush_pending_behavior();
    }

    /// drain の実体。区間フラグを立てて再入を保留させる。
    /// outbox を**先に**取り出してから作用する（borrow 競合・再入防止）。
    /// Finalize=現在選択候補を確定 / Abort=取消。いずれも Enter/Esc と同じ既存経路を再利用する。
    /// 生きた context が無い（current_context=None）なら何もしない（劣化。panic させない）。
    fn drain_behavior_inner(&self) {
        let prev = self.reentrancy.enter();
        let _flag = InOperationGuard { gate: &self.reentrancy, prev };
        let action = self.behavior_outbox.borrow_mut().take();
        let Some(action) = action else { return; };
        let Some(ctx) = self.current_context.borrow().clone() else { return; };
        match action {
            BehaviorAction::Finalize => {
                // UU-4(#4): 保留された Finalize が「候補が既に閉じられた後」（例: Esc で hide したが
                // composition は残る経路）に flush されると、ユーザが破棄したはずの候補を誤確定しうる。
                // 候補表示中(showing)のときだけ確定する（cand_state は hide でクリアされないため）。
                if !self.showing.get() {
                    return;
                }
                // Enter（候補表示中）と同一: 選択中の候補を commit_candidate で確定する
                // （前方一致候補なら部分確定して残り読みを継続）。選択 index は cand_state
                // （＝選択の唯一の真実源。キーボードも Behavior::SetSelection もここを更新）から読む。
                let pick = {
                    let st = self.cand_state.borrow();
                    st.resolve_commit(st.selected())
                };
                let Some((index, text)) = pick else { return; }; // 候補空
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
                    tip_log("ev=candidates_hidden");
                } else if self.state.borrow().composing {
                    self.disarm_debounce();
                    self.do_cancel(&ctx);
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
        Self { in_operation: Cell::new(false), pending: Cell::new(false) }
    }
    /// 現在、操作区間の中か（ゲートの状態を検査するアクセサ。単体テストで区間フラグの
    /// 遷移を確認するのに使う。production では guarded が enter/exit の戻り値で判定する）。
    #[allow(dead_code)]
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
            unsafe { (*p).drain_behavior(); }
        }
    });
}

/// 指定パイプ名を引数に、`NospacekeyEngineHost.exe` を**コンソール無し**で起動する。
/// `CREATE_NO_WINDOW` を付けるので可視ウィンドウは出ない（切替時の大量ウィンドウ対策）。
pub(crate) fn spawn_engine_hidden(exe: &std::path::Path, pipe: &str, env: &[(String, String)]) -> Option<std::process::Child> {
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
                if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
                    if let Ok(f2) = f.try_clone() { cmd.stdout(Stdio::from(f)).stderr(Stdio::from(f2)); }
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
        Err(e) if e.raw_os_error() == Some(5) => match build(CREATE_NO_WINDOW | DETACHED_PROCESS).spawn() {
            Ok(child) => {
                tip_log("ev=engine_spawn_retry breakaway=off ok=true");
                Some(child)
            }
            Err(e2) => {
                tip_log(&format!("ev=engine_spawn_err os={:?} kind={:?} msg={} retry=breakaway_off", e2.raw_os_error(), e2.kind(), e2));
                None
            }
        },
        Err(e) => { tip_log(&format!("ev=engine_spawn_err os={:?} kind={:?} msg={}", e.raw_os_error(), e.kind(), e)); None }
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
    let key_plain = if s.llm.api_key_dpapi.is_empty() { None }
        else { settings::dpapi::decrypt(&s.llm.api_key_dpapi) };
    let env_map = settings::resolve_env_map(&s, key_plain.as_ref().map(|z| z.as_str()), |k| std::env::var(k).ok());
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
    if ptr.is_null() { return; }
    let ts: &TextService_Impl = unsafe { &*ptr };
    if ts.llm_poll_timer.get() != id {
        // この id は現在のインスタンスの物ではない（複数インスタンスが 1 STA に同居した場合等）。
        // ポーリングタイマは反復発火するので、放置すると永久に CPU を食う。確実に止める
        // （debounce_timer_proc が先頭で無条件 KillTimer するのと同じ防御）。
        unsafe { let _ = KillTimer(None, id); }
        return;
    }
    // UU-4(#5/#6): on_llm_outcome/abort_llm も run_preedit（同期 edit session）でホスト再入しうる
    // COM 区間なので guarded で包み、extern "system" 越えの panic は catch_unwind で止める
    // （debounce_timer_proc と対称）。
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ts.guarded(|| {
            let outcome = ts.llm_slot.borrow().as_ref()
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

/// TextService が（Deactivate を経ずに）解放された場合の保険: 武装中のデバウンスタイマを
/// 確実に解除し、thread_local の生ポインタを無効化する（タイマ発火時の dangling 参照=UAF を防ぐ）。
impl Drop for TextService {
    fn drop(&mut self) {
        let id = self.debounce_timer.replace(0);
        if id != 0 {
            unsafe {
                let _ = KillTimer(None, id);
            }
        }
        DEBOUNCE_TS.with(|p| p.set(std::ptr::null()));
        let lid = self.llm_poll_timer.replace(0);
        if lid != 0 { unsafe { let _ = KillTimer(None, lid); } }
        LLM_TS.with(|p| p.set(std::ptr::null()));
        // SP6a: Behavior 自己ポインタも無効化（Deactivate を経ない解放での dangling 防止）。
        BEHAVIOR_TS.with(|p| p.set(std::ptr::null()));
        // C-1: DLL 生存参照は `_guard`（ComObjectGuard）の Drop が自動で -1 する
        // （この drop 本体の後にフィールドが drop される）。全 #[implement] オブジェクトが
        // 解放されたら DllCanUnloadNow=S_OK。
    }
}

/// InputScope 配列に IS_PASSWORD が含まれるか（Spec2 パスワード欄尊重の純判定）。
/// 値は windows crate の InputScope(i32) の生値（IS_PASSWORD = 31）。
pub fn scopes_contain_password(scopes: &[i32]) -> bool {
    use windows::Win32::UI::TextServices::IS_PASSWORD;
    scopes.contains(&IS_PASSWORD.0)
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
    [GUID_COMPARTMENT_KEYBOARD_DISABLED, GUID_COMPARTMENT_EMPTYCONTEXT]
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
    let too_big = std::fs::metadata(path).map(|m| m.len() > max).unwrap_or(false);
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
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
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
            &format!("ev=log_open build={}-{}", env!("CARGO_PKG_VERSION"), env!("GIT_HASH")),
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
fn feedback_path() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| std::path::PathBuf::from(d).join("nospacekey").join("feedback.jsonl"))
}

/// feedback.jsonl へ 1 行追記する（親 dir が無ければ作る）。1 レコードを 1 回の
/// write_all で書いて行割れを避ける（tip_log と同じ流儀）。
fn append_feedback_record(rec: &LastCommit) -> std::io::Result<()> {
    use std::io::Write;
    let path = feedback_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no LOCALAPPDATA")
    })?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
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
        assert!(should_prespawn(false, false, true));   // (has_client, spawn_attempted, backoff_allows)
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
        assert!(g.signal_reentry(false), "区間中は has_action に依らず戻る(true)");
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
        let req = build_reload_config(&s, Some("sk-x"));
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
                learning_enabled: true,
                typo_learn_enabled: true,
            }
        );
    }

    #[test]
    fn disabled_llm_sends_empty_llm_fields() {
        // LLM 無効時は鍵が復号できても LLM 系は空で送る（エンジンを disabled に落とす＝H-1 整合）。
        let mut s = Settings::default();
        s.llm.enabled = false;
        s.llm.endpoint = "https://leak".into();
        let req = build_reload_config(&s, Some("sk-should-not-leak"));
        match req {
            Request::ReloadConfig { llm_enabled, llm_api_key, llm_endpoint, .. } => {
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
        match build_reload_config(&s, None) {
            Request::ReloadConfig { zenzai_enabled, zenzai_weight, .. } => {
                assert!(!zenzai_enabled);
                assert_eq!(zenzai_weight, "");
            }
            _ => panic!("ReloadConfig を組み立てるはず"),
        }
    }
}

#[cfg(test)]
mod a8_tests {
    use super::{is_toggle_repeat, plan_start_session, should_log_slow, Response, MODE_TOGGLE_REPEAT_GUARD, IPC_TIMEOUT_CONVERT, IPC_TIMEOUT_FAST, IPC_TIMEOUT_LIVE};
    use std::time::Duration;

    #[test]
    fn start_session_plan_adopts_session_and_drops_otherwise() {
        // 正常応答: セッション採用。
        assert_eq!(plan_start_session(Ok(Response::Session { session: 7, proto: None })), Some(7));
        // タイムアウト: 遅延 Session フレームが滞留しうるので接続破棄（恒常 1-off desync 防止）。
        assert_eq!(
            plan_start_session(Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "t"))),
            None
        );
        // 切断系エラーも破棄。
        assert_eq!(
            plan_start_session(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "b"))),
            None
        );
        // 予期しない応答型（プロトコル desync の兆候）も破棄。
        assert_eq!(
            plan_start_session(Ok(Response::Error { message: "x".into() })),
            None
        );
    }

    #[test]
    fn handshake_decision_table() {
        use super::{decide_handshake, HandshakeAction};
        // 一致 → 採用。
        assert_eq!(decide_handshake(Some(super::PROTO_VERSION), false), HandshakeAction::Accept);
        // 不一致（None=handshake 以前の旧エンジン）で未試行 → 世代交代。
        assert_eq!(decide_handshake(None, false), HandshakeAction::ShutdownRespawn);
        // 不一致（新しすぎる proto）で未試行 → 世代交代。
        assert_eq!(decide_handshake(Some(999), false), HandshakeAction::ShutdownRespawn);
        // 一度試して尚不一致 → 接続維持（無限 shutdown ループ防止）。
        assert_eq!(decide_handshake(None, true), HandshakeAction::DegradeKeep);
    }

    #[test]
    fn toggle_repeat_suppresses_only_within_guard() {
        // 軽微1: 初回（None）は通す、閾値未満は抑止、閾値以上は通す。
        assert!(!is_toggle_repeat(None, MODE_TOGGLE_REPEAT_GUARD)); // 初回
        assert!(is_toggle_repeat(Some(Duration::from_millis(33)), MODE_TOGGLE_REPEAT_GUARD)); // オートリピート連射 → 抑止
        assert!(is_toggle_repeat(Some(Duration::from_millis(299)), MODE_TOGGLE_REPEAT_GUARD)); // 閾値直前 → 抑止
        assert!(!is_toggle_repeat(Some(MODE_TOGGLE_REPEAT_GUARD), MODE_TOGGLE_REPEAT_GUARD)); // ちょうど閾値 → 通す
        assert!(!is_toggle_repeat(Some(Duration::from_millis(500)), MODE_TOGGLE_REPEAT_GUARD)); // 意図した押し直し → 通す
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
        assert!(!drained_needs_drop(&Response::Reading { reading: "にほんご".into() }));
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
                &ipc::protocol::Request::Insert { session: 1, text: "n".into(), style: None },
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
                &Request::LiveConvert { session: 1, seq: 7, left_context: None, auto_commit: true },
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
        let res = timed_request(&mut c, &Request::StartSession, IPC_TIMEOUT_FAST, "start_session");

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
        assert!(!b.probe_allowed(), "probe must be suppressed after session failure");
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
        assert!(!ts_val.is_empty() && ts_val.bytes().all(|b| b.is_ascii_digit()),
            "ts 値が数字でない: {content}");
        let _ = std::fs::remove_file(&logp);
    }

    #[test]
    fn rotation_renames_oversized_log_to_dot1_once() {
        let dir = std::env::temp_dir().join(format!("nospacekey-rotatetest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let logp = dir.join("nospacekey-tip.log");
        let rotated = dir.join("nospacekey-tip.log.1");
        // 上限以下: ローテーションしない。
        std::fs::write(&logp, "1234").unwrap();
        rotate_log_if_larger_than(&logp, 4);
        assert!(logp.exists() && !rotated.exists(), "上限以下で回してはいけない");
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
    use super::{compartment_flag_is_set, scopes_contain_password};

    #[test]
    fn scopes_contain_password_detects() {
        use windows::Win32::UI::TextServices::{IS_DEFAULT, IS_PASSWORD};
        assert!(scopes_contain_password(&[IS_DEFAULT.0, IS_PASSWORD.0]));
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
}

#[cfg(test)]
mod commit_undo_tests {
    use super::{undo_precheck, UndoSkip};

    #[test]
    fn undo_precheck_gates_all_preconditions() {
        // (armed, has_composition, has_buffer, tlen_utf16) -> Ok / 各 skip reason
        assert!(undo_precheck(true, false, true, 3).is_ok());
        assert_eq!(undo_precheck(false, false, true, 3), Err(UndoSkip::NotArmed));
        assert_eq!(undo_precheck(true, true, true, 3), Err(UndoSkip::CompositionOpen)); // 部分確定直後など
        assert_eq!(undo_precheck(true, false, false, 3), Err(UndoSkip::NoBuffer));
        assert_eq!(undo_precheck(true, false, true, 65), Err(UndoSkip::TooLong)); // 64 UTF-16 単位上限
    }
}
