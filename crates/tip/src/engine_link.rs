//! engine への接続先 pipe 名（算出は ipc::client へ移設 — 設定アプリと共有）と spawn 直列化。
//! pipe_name_for_session/current_session_id は Task 7 の設定アプリが ipc::client から直接
//! 使う想定のためここでは再 export しない（cdylib の本クレートは自身が使わない re-export に
//! unused_imports が立つ）。TIP 内で実際に使うのは stable_pipe_name のみ。
pub use ipc::client::stable_pipe_name;

#[derive(Debug, PartialEq, Eq)]
pub enum EngineAction { UseExisting, SpawnThenConnect, DegradeNoSpawn }

/// connect 試行結果と「このプロセスで spawn 済みか」から次アクションを決める純粋関数。
pub fn decide(connected: bool, already_spawn_attempted: bool) -> EngineAction {
    if connected { EngineAction::UseExisting }
    else if already_spawn_attempted { EngineAction::DegradeNoSpawn }
    else { EngineAction::SpawnThenConnect }
}

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

use std::time::{Duration, Instant};

/// 再接続フルコース失敗の初回バックオフ間隔。
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);
/// 再接続バックオフ間隔の上限（指数増加の頭打ち）。
pub const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// engine 再接続フルコース失敗の間隔制御（キースレッド専用・純粋構造体）。
/// n 回目の失敗で `BACKOFF_BASE * 2^(n-1)`、`BACKOFF_CAP` で頭打ちの遅延を課す。
/// `Instant::now()` を内部で呼ばず必ず引数で受け取ることでテスト可能にしている。
/// フルコース（spawn+connect+session 確立まで）の失敗は `on_connect_failure`／
/// `on_session_failure` で通知する。両者は同じ遅延スケジュールに従うが、
/// session 確立後の失敗（＝engine は生きていた可能性が高い）は不要な spawn を
/// 避けるため probe も止める。probe_suppressed は「直近の失敗種別」を表す
/// フラグで、connect 失敗を挟むと再び probe を許可する。
pub struct ReconnectBackoff {
    failures: u32,
    next_allowed: Option<Instant>,
    probe_suppressed: bool,
}

impl ReconnectBackoff {
    pub fn new() -> Self {
        Self { failures: 0, next_allowed: None, probe_suppressed: false }
    }

    /// 現在ログ用に保持している失敗回数。
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// フルコース（spawn+connect+session確立）の再試行を許可するか。
    pub fn full_attempt_allowed(&self, now: Instant) -> bool {
        match self.next_allowed {
            Some(t) => now >= t,
            None => true,
        }
    }

    /// 軽量な生存確認（probe）を許可するか。session 失敗直後のみ抑止される。
    pub fn probe_allowed(&self) -> bool {
        !self.probe_suppressed
    }

    /// 失敗回数を進め、次の許可時刻を `now` からのスケジュールで更新する。
    fn record_failure(&mut self, now: Instant) {
        self.failures += 1;
        let delay = (BACKOFF_BASE * 2u32.saturating_pow(self.failures - 1)).min(BACKOFF_CAP);
        self.next_allowed = Some(now + delay);
    }

    /// connect 試行自体の失敗を記録する。probe は止めない
    /// （engine プロセスが存在しない可能性が高く、probe で早期に生存確認したい）。
    pub fn on_connect_failure(&mut self, now: Instant) {
        self.record_failure(now);
        self.probe_suppressed = false;
    }

    /// session 確立後の失敗を記録する。同じ遅延スケジュールに加え、
    /// engine が生きていた可能性が高いため probe も抑止する。
    pub fn on_session_failure(&mut self, now: Instant) {
        self.record_failure(now);
        self.probe_suppressed = true;
    }

    /// 全状態をリセットする（再接続成功時に呼ぶ想定）。
    pub fn reset(&mut self) {
        self.failures = 0;
        self.next_allowed = None;
        self.probe_suppressed = false;
    }
}

/// プロセス跨ぎで「engine singleton 起動」を直列化する best-effort RAII ガード。
/// acquire 中に他ホストが既に起こしていれば、呼び出し側は再接続で拾える。
/// 取得できなくても起動は進める（最悪 engine が一時的に2個＝既知の軽微な限界）。
pub struct SpawnGuard { handle: Option<HANDLE>, owned: bool }
impl SpawnGuard {
    pub fn acquire(pipe: &str) -> Self {
        let name = format!("Global\\nospacekey-spawn-{}", pipe.replace('\\', "_"));
        unsafe {
            match CreateMutexW(None, false, &HSTRING::from(name)) {
                Ok(h) => {
                    // Cap at 500ms: if the guard can't be acquired in time, spawning proceeds
                    // unserialized — at worst a transient second engine, which persistent
                    // singletons resolve on the next keystroke's connect. Bounding the
                    // key-thread block matters more than perfect serialization.
                    let r = WaitForSingleObject(h, 500);
                    // WAIT_ABANDONED means the previous owner died holding the mutex; we still
                    // own it and must release it in Drop, otherwise the lock leaks permanently.
                    let owned = r == WAIT_OBJECT_0 || r == WAIT_ABANDONED;
                    Self { handle: Some(h), owned }
                }
                Err(_) => Self { handle: None, owned: false },
            }
        }
    }
}
impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            unsafe {
                if self.owned { let _ = ReleaseMutex(h); }
                let _ = CloseHandle(h);
            }
        }
    }
}

#[cfg(test)]
mod decide_tests {
    use super::*;
    #[test]
    fn decide_table() {
        assert_eq!(decide(true,  false), EngineAction::UseExisting);
        assert_eq!(decide(true,  true ), EngineAction::UseExisting);
        assert_eq!(decide(false, false), EngineAction::SpawnThenConnect);
        assert_eq!(decide(false, true ), EngineAction::DegradeNoSpawn);
    }
}

#[cfg(test)]
mod backoff_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn initial_state_allows_attempt_and_probe() {
        let t0 = Instant::now();
        let b = ReconnectBackoff::new();
        assert!(b.full_attempt_allowed(t0));
        assert!(b.probe_allowed());
    }

    #[test]
    fn connect_failure_delays_full_attempt_but_not_probe() {
        let t0 = Instant::now();
        let mut b = ReconnectBackoff::new();
        b.on_connect_failure(t0);
        assert!(!b.full_attempt_allowed(t0 + Duration::from_millis(999)));
        assert!(b.full_attempt_allowed(t0 + Duration::from_secs(1)));
        assert!(b.probe_allowed());
    }

    #[test]
    fn repeated_connect_failures_double_then_cap() {
        let t0 = Instant::now();
        let mut b = ReconnectBackoff::new();
        b.on_connect_failure(t0); // 1回目: 1s
        b.on_connect_failure(t0); // 2回目: 2s
        assert!(!b.full_attempt_allowed(t0 + Duration::from_millis(1999)));
        assert!(b.full_attempt_allowed(t0 + Duration::from_secs(2)));

        b.on_connect_failure(t0); // 3回目: 4s
        assert!(!b.full_attempt_allowed(t0 + Duration::from_millis(3999)));
        assert!(b.full_attempt_allowed(t0 + Duration::from_secs(4)));

        b.on_connect_failure(t0); // 4回目: 8s
        b.on_connect_failure(t0); // 5回目: 16s
        b.on_connect_failure(t0); // 6回目: cap 30s
        assert!(!b.full_attempt_allowed(t0 + Duration::from_secs(29)));
        assert!(b.full_attempt_allowed(t0 + Duration::from_secs(30)));
    }

    #[test]
    fn session_failure_same_schedule_and_suppresses_probe() {
        let t0 = Instant::now();
        let mut b = ReconnectBackoff::new();
        b.on_session_failure(t0);
        assert!(!b.full_attempt_allowed(t0 + Duration::from_millis(999)));
        assert!(b.full_attempt_allowed(t0 + Duration::from_secs(1)));
        assert!(!b.probe_allowed());
    }

    #[test]
    fn reset_clears_everything() {
        let t0 = Instant::now();
        let mut b = ReconnectBackoff::new();
        b.on_session_failure(t0);
        b.on_session_failure(t0);
        b.reset();
        assert!(b.full_attempt_allowed(t0));
        assert!(b.probe_allowed());
        assert_eq!(b.failures(), 0);

        // reset 後の次の失敗は 1s から再スタート
        b.on_connect_failure(t0);
        assert!(!b.full_attempt_allowed(t0 + Duration::from_millis(999)));
        assert!(b.full_attempt_allowed(t0 + Duration::from_secs(1)));
    }

    #[test]
    fn connect_failure_after_session_failure_restores_probe() {
        let t0 = Instant::now();
        let mut b = ReconnectBackoff::new();
        b.on_session_failure(t0);
        assert!(!b.probe_allowed());
        b.on_connect_failure(t0);
        assert!(b.probe_allowed());
    }

    #[test]
    fn delay_is_anchored_to_instant_passed_to_failure_call() {
        let t0 = Instant::now();
        let mut b = ReconnectBackoff::new();
        // フルコース終了後に「新しい now」を渡す契約:
        // 遅延は呼び出し時に渡した now からの相対時刻で起算される。
        let later = t0 + Duration::from_secs(100);
        b.on_connect_failure(later);
        assert!(!b.full_attempt_allowed(later + Duration::from_millis(999)));
        assert!(b.full_attempt_allowed(later + Duration::from_secs(1)));
        // t0 起点ではまだ許可されないはず(101s < later+1s=101s だが t0+1s は当然 later より前)
        assert!(!b.full_attempt_allowed(t0 + Duration::from_secs(1)));
    }
}
