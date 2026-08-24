#![cfg_attr(windows, windows_subsystem = "windows")]

//! Scheduled-task entry point for the opt-in update checker.
//!
//! The checker deliberately has no Config/TIP dependency.  It reads the shared
//! settings, takes a per-user state lock, performs at most one fixed-endpoint
//! request, and reports an update through the Toast seam.

use chrono::{DateTime, Utc};
use nospacekey_update::client::{ClientError, ReleaseClient};
use nospacekey_update::notification::{
    toast_payload, NotificationSink, WindowsNotificationSink, TOAST_GROUP, TOAST_TAG,
};
use nospacekey_update::release::{
    format_version, parse_version, select_installable_release, Version,
};
use nospacekey_update::state::{format_time, NotificationTuple, StateStore, UpdateState};
use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;

const EXIT_OK: i32 = 0;
const EXIT_STATE: i32 = 10;
const EXIT_NETWORK: i32 = 20;
const EXIT_TOAST: i32 = 30;
const EXIT_PANIC: i32 = 40;

type FetchResult = Result<
    (
        Vec<nospacekey_update::GithubReleaseJson>,
        Option<DateTime<Utc>>,
    ),
    ClientError,
>;

fn main() {
    let code = std::panic::catch_unwind(run).unwrap_or_else(|_| {
        diagnostic_log(None, "checker panic");
        EXIT_PANIC
    });
    std::process::exit(code);
}

fn run() -> i32 {
    let code = std::panic::catch_unwind(run_inner).unwrap_or_else(|_| {
        diagnostic_log(None, "checker panic");
        EXIT_PANIC
    });
    diagnostic_log(None, &format!("checker exit={code}"));
    code
}

fn run_inner() -> i32 {
    let (initial_settings, initial_outcome) = settings::load_reporting_read_only();
    if !matches!(
        initial_outcome,
        settings::LoadOutcome::Loaded | settings::LoadOutcome::Missing
    ) {
        diagnostic_log(None, &format!("settings load failed: {initial_outcome:?}"));
        return EXIT_STATE;
    }
    if !initial_settings.update.automatic_check {
        diagnostic_log(None, "automatic check disabled");
        return EXIT_OK;
    }

    let Some(state_path) = nospacekey_update::state::update_state_path() else {
        diagnostic_log(None, "state path unavailable");
        return EXIT_STATE;
    };
    let store = StateStore::new(state_path.clone());
    let lock = match store.acquire_lock() {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            diagnostic_log(Some(&state_path), "state lock already held");
            return EXIT_OK;
        }
        Err(error) => {
            diagnostic_log(Some(&state_path), &format!("state lock failed: {error}"));
            return EXIT_STATE;
        }
    };
    let _lock = lock;
    let settings = match settings_after_lock(&initial_settings, settings::load_reporting_read_only)
    {
        Ok(Some(settings)) => settings,
        Ok(None) => {
            diagnostic_log(Some(&state_path), "automatic check disabled after lock");
            return EXIT_OK;
        }
        Err(code) => {
            diagnostic_log(Some(&state_path), "settings reload after lock failed");
            return code;
        }
    };
    let mut state = match store.load() {
        Ok(state) => state,
        Err(error) => {
            diagnostic_log(Some(&state_path), &format!("state load failed: {error}"));
            return EXIT_STATE;
        }
    };
    // Close the final settings-to-network race as far as the short-lived
    // process can: a user turning the option OFF while state is loading must
    // still prevent the request below.
    let settings = match settings_after_lock(&settings, settings::load_reporting_read_only) {
        Ok(Some(settings)) => settings,
        Ok(None) => {
            diagnostic_log(Some(&state_path), "automatic check disabled before request");
            return EXIT_OK;
        }
        Err(code) => {
            diagnostic_log(Some(&state_path), "settings final reload failed");
            return code;
        }
    };
    let current = match parse_version(env!("CARGO_PKG_VERSION")) {
        Some(version) => version,
        None => {
            diagnostic_log(Some(&state_path), "checker version is not valid semver");
            return EXIT_NETWORK;
        }
    };
    let client = match ReleaseClient::production() {
        Ok(client) => client,
        Err(error) => {
            diagnostic_log(Some(&state_path), &format!("client build failed: {error}"));
            return EXIT_NETWORK;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            diagnostic_log(Some(&state_path), &format!("runtime build failed: {error}"));
            return EXIT_NETWORK;
        }
    };
    let now = Utc::now();
    let mut sink = WindowsNotificationSink;
    run_flow(
        &settings,
        &current,
        &mut state,
        now,
        || runtime.block_on(client.fetch()),
        &mut sink,
        |next| store.save(next).map_err(|error| error.to_string()),
        Some(&state_path),
    )
}

fn settings_after_lock<F>(
    initial: &settings::Settings,
    reload: F,
) -> Result<Option<settings::Settings>, i32>
where
    F: FnOnce() -> (settings::Settings, settings::LoadOutcome),
{
    if !initial.update.automatic_check {
        return Ok(None);
    }
    let (settings, outcome) = reload();
    if !matches!(
        outcome,
        settings::LoadOutcome::Loaded | settings::LoadOutcome::Missing
    ) {
        return Err(EXIT_STATE);
    }
    if settings.update.automatic_check {
        Ok(Some(settings))
    } else {
        Ok(None)
    }
}

/// Execute the checker state machine with its network, Toast, and persistence
/// seams injected.  Keeping the fetch closure here makes the OFF/throttle paths
/// provably network-free without introducing a second fetch middleman.
// Keep the seam arguments explicit: each is an independently testable side
// effect, and folding them into a context would obscure the state machine.
#[allow(clippy::too_many_arguments)]
fn run_flow<F, S, Save>(
    settings: &settings::Settings,
    current: &Version,
    state: &mut UpdateState,
    now: DateTime<Utc>,
    fetch: F,
    sink: &mut S,
    mut save: Save,
    log_path: Option<&Path>,
) -> i32
where
    F: FnOnce() -> FetchResult,
    S: NotificationSink,
    Save: FnMut(&UpdateState) -> Result<(), String>,
{
    if !settings.update.automatic_check {
        diagnostic_log(log_path, "automatic check disabled");
        return EXIT_OK;
    }
    if state.should_throttle(now) {
        diagnostic_log(log_path, "check throttled");
        return EXIT_OK;
    }

    let (releases, _) = match fetch() {
        Ok(result) => result,
        Err(ClientError::Http {
            status: 403 | 429,
            retry_not_before,
        }) => {
            state.retry_not_before = retry_not_before.map(format_time);
            if let Err(error) = save(state) {
                diagnostic_log(log_path, &format!("retry state save failed: {error}"));
                return EXIT_STATE;
            }
            diagnostic_log(log_path, "GitHub rate limit response");
            return EXIT_NETWORK;
        }
        Err(error) => {
            diagnostic_log(log_path, &format!("release fetch failed: {error}"));
            return EXIT_NETWORK;
        }
    };

    let candidate = select_installable_release(&releases, current, settings.update.include_beta);
    state.mark_success(now, None);
    let Some(candidate) = candidate else {
        // Removing a stale toast is intentionally best-effort.  The successful
        // release check still advances the throttle state.
        let _ = sink.remove_stale(TOAST_TAG, TOAST_GROUP);
        if let Err(error) = save(state) {
            diagnostic_log(log_path, &format!("state save failed: {error}"));
            return EXIT_STATE;
        }
        diagnostic_log(log_path, "no installable update");
        return EXIT_OK;
    };

    let tuple = NotificationTuple {
        installed_version: format_version(current),
        available_version: format_version(&candidate.version),
        channel: candidate.channel.as_str().to_string(),
    };
    if state.notification_was_sent(&tuple) {
        if let Err(error) = save(state) {
            diagnostic_log(log_path, &format!("state save failed: {error}"));
            return EXIT_STATE;
        }
        diagnostic_log(log_path, "notification already sent");
        return EXIT_OK;
    }

    let payload = toast_payload(&candidate.version);
    if let Err(error) = sink.submit(&payload, TOAST_TAG, TOAST_GROUP) {
        // Do not update last_notification until Toast has accepted the payload;
        // the next eligible run may retry the same candidate.
        if let Err(save_error) = save(state) {
            diagnostic_log(
                log_path,
                &format!("toast failed ({error}); state save failed: {save_error}"),
            );
            return EXIT_STATE;
        }
        diagnostic_log(log_path, &format!("toast failed: {error}"));
        return EXIT_TOAST;
    }

    state.last_notification = Some(tuple);
    if let Err(error) = save(state) {
        diagnostic_log(
            log_path,
            &format!("toast submitted; notification state save failed: {error}"),
        );
        return EXIT_STATE;
    }
    diagnostic_log(log_path, "toast submitted");
    EXIT_OK
}

/// Persistent diagnostics are opt-in and contain only checker phase/status
/// strings.  No settings, response bodies, or user data are written.
fn diagnostic_log(_state_path: Option<&Path>, message: &str) {
    if std::env::var_os("NOSPACEKEY_LOG").as_deref() != Some(OsStr::new("1")) {
        return;
    }
    let path = std::env::temp_dir().join("nospacekey-update.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let timestamp = Utc::now().to_rfc3339();
    let _ = writeln!(file, "{timestamp} {message}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    struct TestSink {
        submits: usize,
        removals: usize,
        fail_submit: bool,
    }

    impl NotificationSink for TestSink {
        fn submit(&mut self, _payload: &str, _tag: &str, _group: &str) -> Result<(), String> {
            self.submits += 1;
            if self.fail_submit {
                Err("test toast failure".into())
            } else {
                Ok(())
            }
        }

        fn remove_stale(&mut self, _tag: &str, _group: &str) -> Result<(), String> {
            self.removals += 1;
            Ok(())
        }
    }

    fn candidate_release() -> nospacekey_update::GithubReleaseJson {
        let version = "9.0.0";
        nospacekey_update::GithubReleaseJson {
            tag_name: "v9.0.0".into(),
            prerelease: false,
            draft: false,
            body: String::new(),
            assets: vec![
                nospacekey_update::GithubAssetJson {
                    name: format!("nospacekey-setup-{version}.exe"),
                    size: 1,
                    browser_download_url: format!(
                        "https://github.com/yachtida/nospacekey/releases/download/v9.0.0/nospacekey-setup-{version}.exe"
                    ),
                    state: Some("uploaded".into()),
                },
                nospacekey_update::GithubAssetJson {
                    name: "SHA256SUMS.txt".into(),
                    size: 1,
                    browser_download_url: "https://github.com/yachtida/nospacekey/releases/download/v9.0.0/SHA256SUMS.txt".into(),
                    state: Some("uploaded".into()),
                },
            ],
        }
    }

    fn test_settings(automatic_check: bool) -> settings::Settings {
        let mut settings = settings::Settings::default();
        settings.update.automatic_check = automatic_check;
        settings
    }

    fn current() -> Version {
        parse_version("8.0.0").unwrap()
    }

    fn no_save(_: &UpdateState) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn automatic_off_never_calls_fetch() {
        let settings = test_settings(false);
        let mut state = UpdateState::default();
        let mut sink = TestSink::default();
        let calls = Cell::new(0);
        let code = run_flow(
            &settings,
            &current(),
            &mut state,
            Utc::now(),
            || {
                calls.set(calls.get() + 1);
                Ok((Vec::new(), None))
            },
            &mut sink,
            no_save,
            None,
        );
        assert_eq!(code, EXIT_OK);
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn settings_are_reread_after_lock_before_a_fetch() {
        let initial = test_settings(true);
        let latest = test_settings(false);
        let reread = settings_after_lock(&initial, || (latest, settings::LoadOutcome::Loaded));
        assert!(matches!(&reread, Ok(None)));
        let calls = Cell::new(0);
        if let Ok(Some(settings)) = reread {
            let mut state = UpdateState::default();
            let mut sink = TestSink::default();
            let _ = run_flow(
                &settings,
                &current(),
                &mut state,
                Utc::now(),
                || {
                    calls.set(calls.get() + 1);
                    Ok((Vec::new(), None))
                },
                &mut sink,
                no_save,
                None,
            );
        }
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn settings_recovery_states_are_state_errors_after_lock() {
        let initial = test_settings(true);
        let latest = test_settings(true);
        assert!(matches!(
            settings_after_lock(&initial, || (latest, settings::LoadOutcome::Corrupt)),
            Err(EXIT_STATE)
        ));
    }

    #[test]
    fn throttle_and_retry_do_not_call_fetch() {
        let settings = test_settings(true);
        let now = Utc::now();
        let mut throttled = UpdateState::default();
        throttled.mark_success(now - Duration::hours(1), None);
        let mut sink = TestSink::default();
        let calls = Cell::new(0);
        assert_eq!(
            run_flow(
                &settings,
                &current(),
                &mut throttled,
                now,
                || {
                    calls.set(calls.get() + 1);
                    Ok((Vec::new(), None))
                },
                &mut sink,
                no_save,
                None,
            ),
            EXIT_OK
        );
        assert_eq!(calls.get(), 0);

        let mut retry = UpdateState::default();
        let saved = Cell::new(0);
        assert_eq!(
            run_flow(
                &settings,
                &current(),
                &mut retry,
                now,
                || {
                    calls.set(calls.get() + 1);
                    Err(ClientError::Http {
                        status: 429,
                        retry_not_before: Some(now + Duration::hours(1)),
                    })
                },
                &mut sink,
                |_| {
                    saved.set(saved.get() + 1);
                    Ok(())
                },
                None,
            ),
            EXIT_NETWORK
        );
        assert_eq!(saved.get(), 1);
        assert_eq!(
            run_flow(
                &settings,
                &current(),
                &mut retry,
                now,
                || {
                    calls.set(calls.get() + 1);
                    Ok((Vec::new(), None))
                },
                &mut sink,
                no_save,
                None,
            ),
            EXIT_OK
        );
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn toast_success_saves_tuple_and_failure_leaves_it_for_retry() {
        let settings = test_settings(true);
        let now = Utc::now();
        let releases = vec![candidate_release()];
        let mut success_state = UpdateState::default();
        let mut success_sink = TestSink::default();
        let saved = RefCell::new(Vec::new());
        assert_eq!(
            run_flow(
                &settings,
                &current(),
                &mut success_state,
                now,
                || Ok((releases.clone(), None)),
                &mut success_sink,
                |state| {
                    saved.borrow_mut().push(state.clone());
                    Ok(())
                },
                None,
            ),
            EXIT_OK
        );
        assert_eq!(success_sink.submits, 1);
        assert!(saved.borrow().last().unwrap().last_notification.is_some());

        let mut failure_state = UpdateState::default();
        let mut failure_sink = TestSink {
            fail_submit: true,
            ..TestSink::default()
        };
        let failed_saved = RefCell::new(Vec::new());
        assert_eq!(
            run_flow(
                &settings,
                &current(),
                &mut failure_state,
                now,
                || Ok((releases.clone(), None)),
                &mut failure_sink,
                |state| {
                    failed_saved.borrow_mut().push(state.clone());
                    Ok(())
                },
                None,
            ),
            EXIT_TOAST
        );
        assert_eq!(failure_sink.submits, 1);
        assert!(failed_saved
            .borrow()
            .last()
            .unwrap()
            .last_notification
            .is_none());

        let mut retry_sink = TestSink::default();
        assert_eq!(
            run_flow(
                &settings,
                &current(),
                &mut failure_state,
                now + Duration::hours(6),
                || Ok((releases, None)),
                &mut retry_sink,
                no_save,
                None,
            ),
            EXIT_OK
        );
        assert_eq!(retry_sink.submits, 1);
    }
}
