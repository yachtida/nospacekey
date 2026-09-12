//! checker 専用 state の原子保存、破損隔離、throttle、ユーザー単位 lock。

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const UPDATE_STATE_SCHEMA_VERSION: u32 = 1;
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Config の OFF 適用が、既に起動済み checker の bounded network 区間を待つ上限。
/// checker の HTTP timeout(15s)を上回り、無限待機にはならない。
pub const STATE_LOCK_WAIT: Duration = Duration::from_secs(20);
const STATE_LOCK_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotificationTuple {
    pub installed_version: String,
    pub available_version: String,
    pub channel: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateState {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_successful_check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_not_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_notification: Option<NotificationTuple>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            schema_version: UPDATE_STATE_SCHEMA_VERSION,
            last_successful_check: None,
            retry_not_before: None,
            last_notification: None,
        }
    }
}

impl UpdateState {
    pub fn should_throttle(&self, now: DateTime<Utc>) -> bool {
        if let Some(retry) = parse_time(self.retry_not_before.as_deref()) {
            if retry > now {
                return true;
            }
        }
        let Some(last) = parse_time(self.last_successful_check.as_deref()) else {
            return false;
        };
        // 時計が過去へ戻ったら throttle を無効化する。
        if last > now {
            return false;
        }
        now.signed_duration_since(last).to_std().unwrap_or_default() < CHECK_INTERVAL
    }

    pub fn mark_success(&mut self, now: DateTime<Utc>, retry_not_before: Option<DateTime<Utc>>) {
        self.last_successful_check = Some(format_time(now));
        self.retry_not_before = retry_not_before.map(format_time);
    }

    pub fn notification_was_sent(&self, tuple: &NotificationTuple) -> bool {
        self.last_notification.as_ref() == Some(tuple)
    }
}

fn parse_time(value: Option<&str>) -> Option<DateTime<Utc>> {
    value?.parse::<DateTime<Utc>>().ok()
}

pub fn format_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn update_state_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(|value| {
            PathBuf::from(value)
                .join("nospacekey")
                .join("update-state.json")
        })
}

pub struct StateStore {
    pub path: PathBuf,
}

impl StateStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> std::io::Result<UpdateState> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(UpdateState::default())
            }
            Err(error) => return Err(error),
        };
        let state = serde_json::from_str::<UpdateState>(&text);
        match state {
            Ok(state) if state.schema_version == UPDATE_STATE_SCHEMA_VERSION => Ok(state),
            _ => {
                quarantine(&self.path)?;
                Ok(UpdateState::default())
            }
        }
    }

    pub fn save(&self, state: &UpdateState) -> std::io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("state path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let json = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
        let mut temp = self.path.clone();
        temp.set_file_name(format!(".update-state.{}.tmp", std::process::id()));
        write_temp_file(&temp, &json, |path, bytes| std::fs::write(path, bytes))?;
        if let Err(error) = atomic_replace(&temp, &self.path) {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
        Ok(())
    }

    pub fn acquire_lock(&self) -> std::io::Result<Option<StateLock>> {
        let path = self.path.with_file_name("update-state.lock");
        StateLock::acquire(path)
    }

    /// 既存 checker が保持している間だけ短く再試行し、deadline を過ぎたら
    /// `Ok(None)` を返す。ロック取得エラーは待機せず呼び出し元へ伝える。
    pub fn acquire_lock_with_timeout(
        &self,
        timeout: Duration,
    ) -> std::io::Result<Option<StateLock>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.acquire_lock()? {
                Some(lock) => return Ok(Some(lock)),
                None => {
                    let now = std::time::Instant::now();
                    if now >= deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(STATE_LOCK_POLL.min(deadline - now));
                }
            }
        }
    }
}

/// 一時ファイルへの書き込み途中で失敗しても、次回保存を壊れた残骸に依存させない。
fn write_temp_file<F>(path: &Path, bytes: &[u8], write: F) -> std::io::Result<()>
where
    F: FnOnce(&Path, &[u8]) -> std::io::Result<()>,
{
    if let Err(error) = write(path, bytes) {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

fn atomic_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let source: Vec<u16> = from.as_os_str().encode_wide().chain([0]).collect();
        let target: Vec<u16> = to.as_os_str().encode_wide().chain([0]).collect();
        unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(target.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(std::io::Error::other)
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

fn quarantine(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let stamp = Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let base = path.with_file_name(format!(
        "{}.corrupt.{stamp}",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let mut candidate = base.clone();
    let mut suffix = 0u32;
    while candidate.exists() {
        suffix += 1;
        candidate = PathBuf::from(format!("{}.{}", base.display(), suffix));
    }
    std::fs::rename(path, candidate)
}

pub struct StateLock {
    file: std::fs::File,
    #[cfg(windows)]
    overlapped: windows::Win32::System::IO::OVERLAPPED,
}

impl StateLock {
    fn acquire(path: PathBuf) -> std::io::Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(windows)]
        {
            use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
            use windows::Win32::Foundation::HANDLE;
            use windows::Win32::Storage::FileSystem::{
                LockFileEx, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
                LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
            };
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                // Let the second process open the handle; LockFileEx then
                // reports a clean lock miss instead of OpenFile's sharing
                // violation, which is normalized to Ok(None) below.
                .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
                .open(path)?;
            let mut overlapped = windows::Win32::System::IO::OVERLAPPED::default();
            let result = unsafe {
                LockFileEx(
                    HANDLE(file.as_raw_handle() as _),
                    LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                    None,
                    1,
                    0,
                    &mut overlapped,
                )
            };
            if let Err(error) = result {
                if error.code().0 as u32 == 0x80070021 {
                    return Ok(None);
                }
                return Err(std::io::Error::other(error.to_string()));
            }
            Ok(Some(Self { file, overlapped }))
        }
        #[cfg(not(windows))]
        {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(path)?;
            Ok(Some(Self { file }))
        }
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows::Win32::Foundation::HANDLE;
            use windows::Win32::Storage::FileSystem::UnlockFileEx;
            unsafe {
                let _ = UnlockFileEx(
                    HANDLE(self.file.as_raw_handle() as _),
                    None,
                    1,
                    0,
                    &mut self.overlapped,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn throttle_boundary_and_future_clock() {
        let now = Utc.with_ymd_and_hms(2026, 8, 22, 12, 0, 0).unwrap();
        let mut state = UpdateState::default();
        state.mark_success(now - chrono::Duration::hours(6), None);
        assert!(!state.should_throttle(now));
        state.mark_success(now - chrono::Duration::hours(5), None);
        assert!(state.should_throttle(now));
        state.mark_success(now + chrono::Duration::hours(1), None);
        assert!(!state.should_throttle(now));
    }

    #[test]
    fn corrupt_and_unknown_state_are_quarantined() {
        let dir = tempfile_dir();
        let path = dir.join("update-state.json");
        std::fs::write(&path, "not-json").unwrap();
        let store = StateStore::new(path.clone());
        assert_eq!(store.load().unwrap(), UpdateState::default());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn temp_write_error_removes_partial_temp_file() {
        let dir = tempfile_dir();
        let path = dir.join(".update-state.tmp");
        let result = write_temp_file(&path, b"partial", |path, bytes| {
            std::fs::write(path, bytes)?;
            Err(std::io::Error::other("injected write failure"))
        });
        assert!(result.is_err());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn second_state_lock_is_a_clean_miss_instead_of_open_sharing_error() {
        let path = std::env::temp_dir().join(format!(
            "nospacekey-update-lock-{}-{}.lock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = StateLock::acquire(path.clone())
            .unwrap()
            .expect("first lock");
        assert!(StateLock::acquire(path.clone()).unwrap().is_none());
        assert!(std::fs::remove_file(&path).is_err());
        drop(first);
        assert!(StateLock::acquire(path.clone()).unwrap().is_some());
        let _ = std::fs::remove_file(path);
    }

    #[cfg(windows)]
    #[test]
    fn state_lock_waits_for_checker_and_times_out_bounded() {
        let path = std::env::temp_dir().join(format!(
            "nospacekey-update-lock-wait-{}-{}.lock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = StateStore::new(path.clone());
        let (ready, started) = std::sync::mpsc::channel();
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            let worker_store = StateStore::new(worker_path);
            let held = worker_store.acquire_lock().unwrap().expect("checker lock");
            ready.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            drop(held);
        });
        started.recv().unwrap();
        assert!(store
            .acquire_lock_with_timeout(Duration::from_millis(5))
            .unwrap()
            .is_none());
        assert!(store
            .acquire_lock_with_timeout(Duration::from_secs(1))
            .unwrap()
            .is_some());
        let _ = worker.join();
        let _ = std::fs::remove_file(path);
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let sequence = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "nospacekey-update-state-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
