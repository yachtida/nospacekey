//! A7: 電源復帰（サスペンド→レジューム）通知とエンジンのプリウォーム。
//!
//! ノート PC のスリープ復帰直後は、常駐エンジン（別プロセス）が OS に落とされている
//! ことが多い。従来は「復帰後の最初の打鍵」で ensure_engine が spawn し直すため、その 1 打鍵が
//! モデルロード（数百 ms〜秒）を待たされる。ここでは `PowerRegisterSuspendResumeNotification`
//! （powrprof）でレジュームを購読し、ユーザが不在のうちに裏でエンジンの生存確認＋不在なら
//! 再起動（モデルロード）を済ませておく。
//!
//! 設計要点:
//! - コールバックは OS のシステムスレッドから呼ばれる。長い処理は禁止 — atomic 更新と
//!   ワーカ thread の spawn だけを行い即 return する。
//! - プリウォームワーカは `ComObjectGuard`（DLL_REF +1）を最初に構築して move する。
//!   これにより裏ワーカ稼働中は `DllCanUnloadNow` が S_OK を返さず、ホストが DLL を
//!   アンロードしてワーカコードが宙に浮く AV を防ぐ（spec C-3）。
//! - `register`/`PowerNotifyHandle` の配線は Task 5（DllMain/activation 経路）で行う。
//!   本タスク時点では未使用のため cdylib ビルドに dead_code 警告が出るが許容する。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ipc::client::EngineClient;
use ipc::protocol::{Request, Response};

use crate::text_service::tip_log;

/// プリウォームの分岐だけを切り出した純粋関数（Win32 非依存でテスト可能）。
/// probe（生存確認）の結果から次アクションを決める。
#[derive(Debug, PartialEq)]
pub(crate) enum PrewarmAction {
    None,
    Respawn,
}

/// probe の結果 → 次アクション。
/// - `Ok(true)`  : Ping に Pong → エンジン生存。何もしない。
/// - `Ok(false)` : connect は成功したが Pong 以外／無応答 ＝ 半死。pipe を占有中なので spawn しても
///   衝突するだけで無意味（spec 非目的）。何もしない。
/// - `Err(_)`    : connect 失敗 ＝ 不在。ユーザ不在中にモデルロードを済ませる（Respawn）。
pub(crate) fn prewarm_action(probe: &std::io::Result<bool /* pong received */>) -> PrewarmAction {
    match probe {
        Ok(true) => PrewarmAction::None,
        Ok(false) => PrewarmAction::None,
        Err(_) => PrewarmAction::Respawn,
    }
}

/// 電源イベント購読中に共有する状態。`Arc` で包み、register 時に `Arc::into_raw` して
/// コールバックの context（生ポインタ）として OS に預ける。
pub(crate) struct PowerEvents {
    /// レジューム世代。コールバックが `PBT_APMRESUMEAUTOMATIC` を受けるたび +1 する。
    /// Task 5 が「前回観測した世代」と比較して、打鍵時の追加自己修復に使う。
    resume_gen: AtomicU32,
    /// このプロセスが接続/起動すべき安定 pipe 名（session scoped）。
    pipe_name: String,
    /// プリウォームワーカが 1 本だけ走るための排他フラグ（CAS で獲得）。
    prewarm_running: AtomicBool,
}

impl PowerEvents {
    fn new(pipe_name: String) -> Self {
        Self {
            resume_gen: AtomicU32::new(0),
            pipe_name,
            prewarm_running: AtomicBool::new(false),
        }
    }

    /// 現在のレジューム世代。
    pub(crate) fn resume_gen(&self) -> u32 {
        self.resume_gen.load(Ordering::SeqCst)
    }
}

/// `prewarm_running` を Drop で必ず false に戻す RAII リセッタ。
/// ワーカが panic で巻き戻っても排他フラグが立ちっぱなしにならないようにする
/// （立ちっぱなしだと以降のレジュームでプリウォームが二度と走らなくなる）。
struct RunningResetter<'a>(&'a AtomicBool);
impl Drop for RunningResetter<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// エンジンの生存確認。connect(500ms)→Ping(request_within, +1s) を行い、
/// `Ok(true)`=生存 / `Ok(false)`=半死 / `Err(_)`=不在 を返す（prewarm_action の入力形）。
fn probe_engine(pipe: &str) -> std::io::Result<bool> {
    let mut client = EngineClient::connect_to(pipe, Duration::from_millis(500))?;
    let deadline = Instant::now() + Duration::from_secs(1);
    match client.request_within(&Request::Ping, deadline) {
        Ok(Response::Pong) => Ok(true),
        // connect は成功しているので接続自体は生きている＝半死（Pong 以外/エラー応答）。
        Ok(_) => Ok(false),
        // 無応答（TimedOut）や切断。connect が通った以上「不在」ではないので半死扱い。
        Err(_) => Ok(false),
    }
}

/// 不在（Respawn）と判定されたときにエンジンを再起動する。spawn 本体（SpawnGuard＋再確認＋
/// env 同型解決＋spawn_engine_hidden）は text_service::spawn_engine_only に部品化されており
/// （cold start ② の Activate プリスポーンと共用）、ここはログの向き先だけを持つ。
/// engine プロセスは絶対に kill しない（不変条件）。Child は pid ログ後即 drop（detached で生き続ける）。
fn respawn_engine(pipe: &str) {
    match crate::text_service::spawn_engine_only(pipe) {
        // Some(0) は「既に listening（誰かが起こした）→ spawn 不要」— 従来と同じ pid=0 ok=true。
        Some(pid) => tip_log(&format!("ev=resume_respawn pid={pid} ok=true")),
        None => tip_log("ev=resume_respawn pid=0 ok=false"),
    }
}

/// プリウォームワーカ本体（専属 thread で実行）。
/// spec 4.5: connect(500ms)→Ping(+1s)→prewarm_action。Respawn なら env 同型で spawn。
fn prewarm(events: Arc<PowerEvents>) {
    // 先頭で RAII リセッタを作る。以降どの経路（panic 含む）で抜けても running が false に戻る。
    let _resetter = RunningResetter(&events.prewarm_running);

    let probe = probe_engine(&events.pipe_name);
    let ok = matches!(probe, Ok(true));
    tip_log(&format!("ev=resume_probe ok={ok}"));

    match prewarm_action(&probe) {
        PrewarmAction::None => {}
        PrewarmAction::Respawn => respawn_engine(&events.pipe_name),
    }
}

// ── ここから Win32 依存（電源通知の登録/コールバック）──

use windows::Win32::Foundation::{HANDLE, NO_ERROR};
use windows::Win32::System::Power::{
    PowerRegisterSuspendResumeNotification, PowerUnregisterSuspendResumeNotification,
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY,
};
use windows::Win32::UI::WindowsAndMessaging::{DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC};

/// レジューム通知のコールバック（OS のシステムスレッドから呼ばれる）。
/// `PBT_APMRESUMEAUTOMATIC`（自動レジューム）のみ反応。ここでは atomic 更新と
/// `thread::Builder::spawn` だけを行い即 return する（OS 推奨。長い処理はワーカ側へ）。
///
/// SAFETY: `context` は register で `Arc::into_raw(Arc<PowerEvents>)` して OS に預けたポインタ。
/// unregister（`PowerUnregisterSuspendResumeNotification`）はコールバックの完了と同期し、
/// その後で初めて context 分の `Arc` を解放するため、コールバック実行中は `*PowerEvents` が生存している。
unsafe extern "system" fn power_callback(
    context: *const c_void,
    event_type: u32,
    _setting: *const c_void,
) -> u32 {
    if event_type == PBT_APMRESUMEAUTOMATIC {
        // SAFETY: 上記のとおり context は生存中の PowerEvents を指す。参照は本関数内でのみ使う。
        let events = unsafe { &*(context as *const PowerEvents) };
        events.resume_gen.fetch_add(1, Ordering::SeqCst);
        // ワーカは 1 本だけ。CAS が取れたときのみ spawn（レジューム連発でも多重起動しない）。
        if events
            .prewarm_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            // context ポインタから ManuallyDrop で一時 Arc 化して clone のみ行う。
            // ManuallyDrop なので from_raw で増やした「見かけの所有権」は drop されず、
            // context 分の参照カウント（register の into_raw ＋1）は減らない — 会計を崩さない。
            // SAFETY: context は into_raw 由来の有効な Arc<PowerEvents> ポインタ。
            let arc = std::mem::ManuallyDrop::new(unsafe {
                Arc::from_raw(context as *const PowerEvents)
            });
            let events2 = Arc::clone(&arc);
            // ワーカ寿命の間 DLL_REF を +1 保持（DLL アンロード AV 防止。spec C-3）。
            // ComObjectGuard は unit struct なので自動的に Send（thread へ move 可）。
            let guard = crate::globals::ComObjectGuard::new();
            // extern "system" 境界を panic で越えるとホストプロセスごと abort するため、
            // spawn は Builder 経由で失敗を捕捉する（OS のスレッド生成失敗など）。
            // Err 時は guard/events2 が Builder のクロージャごと drop され（DLL_REF が戻る）、
            // CAS 済みの prewarm_running を明示的に false へ戻して次回レジュームで再挑戦できるようにする。
            if std::thread::Builder::new()
                .spawn(move || {
                    let _guard = guard;
                    prewarm(events2);
                })
                .is_err()
            {
                events.prewarm_running.store(false, Ordering::SeqCst);
                tip_log("ev=resume_prewarm_spawn_failed");
            }
        }
    }
    0
}

/// 電源通知の登録ハンドル。Drop で unregister し、context 分の `Arc` を解放する。
pub(crate) struct PowerNotifyHandle {
    hpower: HPOWERNOTIFY,
    events: Arc<PowerEvents>,
}

impl PowerNotifyHandle {
    /// 共有状態（世代カウンタ等）への参照。Task 5 が打鍵時の追加自己修復判定に使う。
    pub(crate) fn events(&self) -> &Arc<PowerEvents> {
        &self.events
    }
}

impl Drop for PowerNotifyHandle {
    fn drop(&mut self) {
        // SAFETY: hpower は register が受け取った有効な登録ハンドル。unregister は進行中の
        // コールバック完了を待って戻る（OS 契約）ので、この後 context 分の Arc を安全に解放できる。
        unsafe {
            let _ = PowerUnregisterSuspendResumeNotification(self.hpower);
            // register の `Arc::into_raw`（context 分 +1）に対応する `from_raw`（-1）。
            // into_raw/from_raw の回数がちょうど 1:1 で釣り合い、ここで context 分が解放される。
            let ptr = Arc::as_ptr(&self.events);
            drop(Arc::from_raw(ptr));
        }
    }
}

/// レジューム通知を購読する。成功で `PowerNotifyHandle` を返す。
/// 失敗（古い OS 等）は `None` — 呼び出し側（Task 5）は None を許容し「従来どおり次打鍵で自己修復」へ
/// 劣化する設計（spec 4.4）。
pub(crate) fn register(pipe_name: String) -> Option<PowerNotifyHandle> {
    let events = Arc::new(PowerEvents::new(pipe_name));

    // context 分の参照 +1。OS に生ポインタとして預ける（Drop の from_raw と 1:1 で釣り合う）。
    let context = Arc::into_raw(Arc::clone(&events)) as *mut c_void;

    let mut params = DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(power_callback),
        Context: context,
    };
    // 登録ハンドルは *mut c_void で受ける（powrprof の out param 型）。成功後 HPOWERNOTIFY に包む。
    let mut hpower_raw: *mut c_void = std::ptr::null_mut();

    // SAFETY: params はスタック上に生存し、Callback/Context とも有効。recipient は
    // DEVICE_NOTIFY_CALLBACK なので params 構造体へのポインタを HANDLE に詰めて渡す（Win32 契約）。
    // registrationhandle は out param。
    let err = unsafe {
        PowerRegisterSuspendResumeNotification(
            DEVICE_NOTIFY_CALLBACK,
            HANDLE(&mut params as *mut _ as *mut c_void),
            &mut hpower_raw,
        )
    };

    if err != NO_ERROR {
        // 失敗: OS は context を保持していない。into_raw で増やした分をここで回収して None。
        // SAFETY: context は直前の into_raw 由来で、まだ誰にも渡っていない有効ポインタ。
        unsafe { drop(Arc::from_raw(context as *const PowerEvents)) };
        tip_log(&format!("ev=power_register ok=false err={}", err.0));
        return None;
    }

    tip_log("ev=power_register ok=true");
    Some(PowerNotifyHandle {
        hpower: HPOWERNOTIFY(hpower_raw as isize),
        events,
    })
}

#[cfg(test)]
mod prewarm_tests {
    use super::*;

    #[test]
    fn alive_pong_is_none() {
        // Ping→Pong → 生存 → 何もしない。
        let probe: std::io::Result<bool> = Ok(true);
        assert_eq!(prewarm_action(&probe), PrewarmAction::None);
    }

    #[test]
    fn half_dead_is_none() {
        // 半死（connect Ok・Pong 以外/無応答）→ pipe 占有中なので spawn せず None。
        let probe: std::io::Result<bool> = Ok(false);
        assert_eq!(prewarm_action(&probe), PrewarmAction::None);
    }

    #[test]
    fn absent_is_respawn() {
        // connect 失敗（不在）→ ユーザ不在中にモデルロードを済ませる → Respawn。
        let probe: std::io::Result<bool> =
            Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert_eq!(prewarm_action(&probe), PrewarmAction::Respawn);
    }

    #[test]
    fn resume_gen_advances_once_per_bump() {
        // resume_gen の世代遷移: bump（fetch_add）ごとに +1、poll は同じ値を返す（1 回だけ反応）。
        let ev = PowerEvents::new(String::from("test-pipe"));
        assert_eq!(ev.resume_gen(), 0);

        // コールバックが 1 回発火した相当。
        ev.resume_gen.fetch_add(1, Ordering::SeqCst);
        assert_eq!(ev.resume_gen(), 1);
        // 追加の poll では増えない（世代は 1 のまま — 二重反応しない）。
        assert_eq!(ev.resume_gen(), 1);

        // 次のレジュームでさらに +1。
        ev.resume_gen.fetch_add(1, Ordering::SeqCst);
        assert_eq!(ev.resume_gen(), 2);
    }

    #[test]
    fn running_resetter_clears_flag_on_drop() {
        // RAII リセッタは Drop で必ず running を false に戻す（panic 経路含む排他解放）。
        let flag = AtomicBool::new(true);
        {
            let _r = RunningResetter(&flag);
            assert!(flag.load(Ordering::SeqCst));
        }
        assert!(!flag.load(Ordering::SeqCst), "Drop で running が false に戻る");
    }
}
