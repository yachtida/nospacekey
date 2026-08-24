//! tauri コマンド層。logic（純関数）と settings crate を繋ぐだけの薄い層に保つ。

use crate::logic::{
    self, DictCmdError, DictLock, EngineStatus, ExportReportDto, FieldError, ImportReportDto,
    ListReport, MutationReport, SettingsDto,
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::{Emitter, Manager};

/// The startup worker can wait for bounded checker state-lock retries and
/// may then perform several bounded Task Scheduler commands.  Config only
/// waits long enough to get a coherent first DTO; a timeout still falls back
/// to the disk truth and never forces the switch OFF.
pub const RECONCILE_COMPLETION_WAIT: Duration = Duration::from_secs(45);
pub const STARTUP_RECONCILE_COMPLETE_EVENT: &str = "startup-reconcile-complete";
const STARTUP_LEASE_RETRY_ATTEMPTS: usize = 2;
const STARTUP_LEASE_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ReconcileCompletion {
    #[default]
    Pending,
    Success,
    Error,
    Timeout,
}

#[derive(serde::Serialize)]
pub struct LoadResult {
    pub dto: SettingsDto,
    /// settings.json が存在し JSON として壊れていた（→ load() が隔離して既定に劣化した）。
    pub corrupt_recovered: bool,
    /// 起動時の per-user task reconciliation が失敗した場合の inline 状態。
    pub update_task_error: Option<String>,
}

/// 起動時に automatic_check を安全側へ倒した理由を UI へ渡す短命 state。
/// OFF fallback の保存に成功した場合だけ Config の表示を forced-off にし、保存失敗時は
/// 永続 ON を正直に表示して、ユーザーが次に Apply できるよう inline warning を残す。
#[derive(Default)]
pub struct AutomaticCheckReconcileState {
    error: std::sync::Mutex<Option<String>>,
    needs_repair: std::sync::Mutex<bool>,
    /// Whether `error` describes a startup/reconcile path that forced the
    /// persisted ON setting to be presented as OFF.  A post-save apply
    /// warning is different: if the OFF fallback save failed, the disk still
    /// contains ON and get_settings must report that truth to the UI.
    force_automatic_off: std::sync::Mutex<bool>,
    corrupt_recovered_notice: std::sync::Mutex<bool>,
    completion: std::sync::Mutex<ReconcileCompletion>,
    completion_changed: std::sync::Condvar,
}

impl AutomaticCheckReconcileState {
    fn set_error(&self, error: String) {
        self.set_error_with_force(error, true);
    }

    fn set_persisted_on_warning(&self, error: String) {
        self.set_error_with_force(error, false);
    }

    fn set_error_with_force(&self, error: String, force_automatic_off: bool) {
        if let Ok(mut current) = self.error.lock() {
            *current = Some(error);
        }
        if let Ok(mut repair) = self.needs_repair.lock() {
            *repair = true;
        }
        if let Ok(mut force_off) = self.force_automatic_off.lock() {
            *force_off = force_automatic_off;
        }
    }

    fn clear(&self) {
        if let Ok(mut current) = self.error.lock() {
            *current = None;
        }
        if let Ok(mut repair) = self.needs_repair.lock() {
            *repair = false;
        }
        if let Ok(mut force_off) = self.force_automatic_off.lock() {
            *force_off = false;
        }
    }

    /// Clear a warning only after an Apply transaction completed without a
    /// warning.  The worker's completion state is intentionally left alone
    /// while it is Pending or Timeout because that worker may still publish a
    /// late result.  A terminal worker Error is the one state an Apply can
    /// safely supersede; transition it to Success and wake get_settings so a
    /// previously stale generic warning cannot survive the successful Apply.
    fn clear_after_successful_apply(&self) {
        self.clear();
        let (mut current, poisoned) = match self.completion.lock() {
            Ok(current) => (current, false),
            Err(poisoned) => (poisoned.into_inner(), true),
        };
        if poisoned {
            *current = ReconcileCompletion::Error;
            self.completion_changed.notify_all();
        } else if *current == ReconcileCompletion::Error {
            *current = ReconcileCompletion::Success;
            self.completion_changed.notify_all();
        }
    }

    fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|current| current.clone())
    }

    fn needs_repair(&self) -> bool {
        self.needs_repair
            .lock()
            .map(|repair| *repair)
            .unwrap_or(true)
    }

    fn forces_automatic_off(&self) -> bool {
        self.force_automatic_off
            .lock()
            .map(|force_off| *force_off)
            .unwrap_or(true)
    }

    fn note_corrupt_recovered(&self) {
        if let Ok(mut notice) = self.corrupt_recovered_notice.lock() {
            *notice = true;
        }
    }

    fn take_corrupt_recovered_notice(&self) -> bool {
        self.corrupt_recovered_notice
            .lock()
            .map(|mut notice| std::mem::replace(&mut *notice, false))
            .unwrap_or(false)
    }

    fn finish(&self) {
        let status = if self.error().is_some() {
            ReconcileCompletion::Error
        } else {
            ReconcileCompletion::Success
        };
        self.complete(status);
    }

    fn complete(&self, status: ReconcileCompletion) {
        let (mut current, poisoned) = match self.completion.lock() {
            Ok(current) => (current, false),
            Err(poisoned) => (poisoned.into_inner(), true),
        };
        // A poisoned completion mutex must not strand get_settings forever.
        // Recover the guard, publish Error as the conservative status, and
        // notify waiters even when the worker itself is unwinding.
        *current = if poisoned {
            ReconcileCompletion::Error
        } else {
            status
        };
        self.completion_changed.notify_all();
    }

    #[cfg(test)]
    fn completion(&self) -> ReconcileCompletion {
        match self.completion.lock() {
            Ok(current) => *current,
            Err(poisoned) => {
                let mut current = poisoned.into_inner();
                *current = ReconcileCompletion::Error;
                self.completion_changed.notify_all();
                ReconcileCompletion::Error
            }
        }
    }

    fn wait_for_completion(&self, timeout: Duration) -> ReconcileCompletion {
        let deadline = std::time::Instant::now() + timeout;
        let (mut current, poisoned) = match self.completion.lock() {
            Ok(current) => (current, false),
            Err(poisoned) => (poisoned.into_inner(), true),
        };
        if poisoned {
            *current = ReconcileCompletion::Error;
            self.completion_changed.notify_all();
            return ReconcileCompletion::Error;
        }
        loop {
            match *current {
                ReconcileCompletion::Pending => {
                    let Some(remaining) =
                        deadline.checked_duration_since(std::time::Instant::now())
                    else {
                        *current = ReconcileCompletion::Timeout;
                        return ReconcileCompletion::Timeout;
                    };
                    let (next, result) =
                        match self.completion_changed.wait_timeout(current, remaining) {
                            Ok(value) => value,
                            Err(poisoned) => {
                                let (mut current, _) = poisoned.into_inner();
                                *current = ReconcileCompletion::Error;
                                self.completion_changed.notify_all();
                                return ReconcileCompletion::Error;
                            }
                        };
                    current = next;
                    if result.timed_out() && *current == ReconcileCompletion::Pending {
                        *current = ReconcileCompletion::Timeout;
                        return ReconcileCompletion::Timeout;
                    }
                }
                status => return status,
            }
        }
    }
}

/// Start startup reconciliation after setup has returned.  The worker obtains
/// SettingsLock before entering reconciliation, and the reconciliation core
/// obtains the checker StateLock only after that, preserving the existing lock
/// order without making Tauri's setup callback wait on external commands.
pub fn spawn_reconcile_worker<R: tauri::Runtime>(app: tauri::AppHandle<R>) {
    std::mem::drop(tauri::async_runtime::spawn_blocking(move || {
        let reconcile = app.state::<AutomaticCheckReconcileState>();
        run_reconcile_worker_with(&reconcile, || {
            let settings_lock = app.state::<crate::logic::SettingsLock>();
            let guard = settings_lock.0.lock().map_err(|_| {
                "設定ロックを取得できないため、起動時の reconcile を完了できませんでした"
                    .to_string()
            })?;
            reconcile_automatic_check_task(&reconcile);
            drop(guard);
            Ok::<(), String>(())
        });
        // The worker is the sole startup generation.  Emit after every
        // terminal path (success, explicit error, or panic recovery) so a UI
        // that timed out can refresh the disk truth without another worker.
        let _ = app.emit(STARTUP_RECONCILE_COMPLETE_EVENT, ());
    }));
}

fn run_reconcile_worker_with<F>(reconcile: &AutomaticCheckReconcileState, action: F)
where
    F: FnOnce() -> Result<(), String>,
{
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(action));
    match result {
        Ok(Ok(())) => reconcile.finish(),
        Ok(Err(error)) => {
            // No reconciliation save happened on this pre-load/lock failure,
            // so a persisted ON must remain visible as ON.
            reconcile.set_persisted_on_warning(error);
            reconcile.finish();
        }
        Err(_) => {
            // A panic must not leave get_settings waiting forever.  The disk
            // is re-read by get_settings and this error is only used as an
            // inline diagnostic/repair hint.
            reconcile.set_persisted_on_warning(
                "起動時の自動更新確認 reconcile が異常終了しました。次回起動時に再試行します"
                    .to_string(),
            );
            reconcile.finish();
        }
    }
}

fn settings_mutation_error(outcome: settings::LoadOutcome) -> String {
    match outcome {
        settings::LoadOutcome::PermissionDenied => {
            "設定ファイルを読み取れないため、変更を保存できません（アクセス拒否）。".into()
        }
        settings::LoadOutcome::IoError => {
            "設定ファイルの読み取りに失敗したため、変更を保存できません。".into()
        }
        settings::LoadOutcome::NoPath => {
            "設定ファイルの保存先を解決できないため、変更を保存できません。".into()
        }
        settings::LoadOutcome::CorruptQuarantineFailed => {
            "設定ファイルが壊れており、原本を退避できないため変更を保存できません。".into()
        }
        outcome => format!(
            "設定ファイルの読み込み状態が mutation に適さないため、変更を保存できません（{outcome:?}）。"
        ),
    }
}

pub(crate) fn settings_mutation_error_for_download(outcome: settings::LoadOutcome) -> String {
    settings_mutation_error(outcome)
}

fn corrupt_recovered(outcome: settings::LoadOutcome, handoff: bool) -> bool {
    outcome == settings::LoadOutcome::Corrupt || handoff
}

fn load_settings_for_mutation() -> Result<settings::Settings, Vec<FieldError>> {
    settings::load_for_mutation().map_err(|outcome| {
        vec![FieldError {
            field: "_io".into(),
            message: settings_mutation_error(outcome),
        }]
    })
}

// (async) 必須: 同期 command は Tauri v2 でメインスレッド実行のため、settings.json の
// ファイル I/O が UNC/AV 介入等で遅延すると WebView ごと固まる（clear_learning_history
// の I-1 注記と同一の規律。get_settings/apply_settings/zenzai_model_status の3つが
// この規律から漏れていた — UIバグ調査 2026-08-16 #9）。
#[tauri::command(async)]
pub fn get_settings(reconcile: tauri::State<'_, AutomaticCheckReconcileState>) -> LoadResult {
    get_settings_with_timeout(&reconcile, RECONCILE_COMPLETION_WAIT)
}

fn get_settings_with_timeout(
    reconcile: &AutomaticCheckReconcileState,
    timeout: Duration,
) -> LoadResult {
    let completion = reconcile.wait_for_completion(timeout);
    let timed_out = completion == ReconcileCompletion::Timeout;
    let completion_failed_without_detail =
        completion == ReconcileCompletion::Error && reconcile.error().is_none();
    // load_reporting は JSON 構文だけでなく Settings 型の不正も同じ一回の read で報告し、
    // 破損原本の退避にも成功した場合だけ Corrupt を返す。
    let (s, outcome) = settings::load_reporting();
    let corrupt_recovered = corrupt_recovered(
        outcome,
        reconcile.take_corrupt_recovered_notice()
            || settings::has_pending_corrupt_recovery_notice(),
    );
    let update_task_error = reconcile
        .error()
        .or_else(|| {
            timed_out.then(|| {
                "起動時の自動更新確認が制限時間内に完了しませんでした。現在の保存済み設定を表示しています。"
                    .to_string()
            })
        })
        .or_else(|| {
            completion_failed_without_detail.then(|| {
                "起動時の自動更新確認状態を確認できませんでした。現在の保存済み設定を表示しています。"
                    .to_string()
            })
        });
    let mut dto = logic::to_dto(&s);
    if !timed_out
        && !completion_failed_without_detail
        && update_task_error.is_some()
        && reconcile.forces_automatic_off()
    {
        // A startup failure whose OFF fallback was persisted must not leave the
        // UI displaying a functional ON state. The next Apply persists this
        // forced-off DTO.
        dto.update_automatic_check = false;
    }
    LoadResult {
        dto,
        corrupt_recovered,
        update_task_error,
    }
}

/// Mark the corrupt-recovery notices whose toast has already been placed in
/// the DOM.  The settings ledger is append-only: acknowledgement creates
/// matching `.ack` files and never removes or overwrites pending entries.
#[tauri::command(async)]
pub fn acknowledge_corrupt_recovery_notices() {
    settings::acknowledge_corrupt_recovery_notices();
}

#[tauri::command(async)]
pub fn apply_settings(
    lock: tauri::State<'_, crate::logic::SettingsLock>,
    reconcile: tauri::State<'_, AutomaticCheckReconcileState>,
    dto: SettingsDto,
) -> Result<(), Vec<FieldError>> {
    // read-modify-save を他の settings.json 書き込み（モデルDL終端の weight_path 反映等）
    // と直列化する — async 化で並行に走れるようになった分の last-writer-wins 防止。
    let _guard = lock.0.lock().map_err(|_| {
        vec![FieldError {
            field: "_io".into(),
            message: "設定ロックを取得できないため、変更を保存できません。".into(),
        }]
    })?;
    // prev は適用時点のディスク上の値を読む（起動後に TIP 側で version 等が変わる可能性に備える）。
    let prev = load_settings_for_mutation()?;
    let s = logic::apply_dto(dto, &prev, settings::dpapi::encrypt)?;
    let warning = apply_automatic_check_transaction_with_lease(
        &reconcile,
        &prev,
        &s,
        || nospacekey_update::scheduler::register_or_update(&checker_path()),
        nospacekey_update::scheduler::run_now,
        |identity| match identity {
            Some(identity) => nospacekey_update::scheduler::delete(identity),
            None => current_task_identity()
                .and_then(|identity| nospacekey_update::scheduler::delete(&identity)),
        },
        |settings| settings::save(settings).map_err(|error| error.to_string()),
        acquire_update_state_lock,
    )?;
    if let Some(warning) = warning {
        // A warning means the transaction was not fully successful. Preserve
        // an existing repair/error state and report the persisted disk truth;
        // a later clean Apply is responsible for clearing it.
        reconcile.set_persisted_on_warning(warning);
    } else {
        // This includes a successful stale-OFF task deletion retry.
        reconcile.clear_after_successful_apply();
    }
    Ok(())
}

/// Apply the automatic-check task/settings transaction with the OS operations
/// injected at its boundary. The production closures below call Task Scheduler;
/// tests use the same control flow with counters, without mocking schtasks.exe.
#[cfg(test)]
struct NoopUpdateStateLease;

#[cfg(test)]
fn apply_automatic_check_transaction<Register, RunNow, Delete, Save>(
    reconcile: &AutomaticCheckReconcileState,
    prev: &settings::Settings,
    next: &settings::Settings,
    register: Register,
    run_now: RunNow,
    delete: Delete,
    save: Save,
) -> Result<Option<String>, Vec<FieldError>>
where
    Register: FnMut() -> Result<nospacekey_update::scheduler::TaskIdentity, String>,
    RunNow: FnMut(&nospacekey_update::scheduler::TaskIdentity) -> Result<(), String>,
    Delete: FnMut(Option<&nospacekey_update::scheduler::TaskIdentity>) -> Result<(), String>,
    Save: FnMut(&settings::Settings) -> Result<(), String>,
{
    apply_automatic_check_transaction_with_lease(
        reconcile,
        prev,
        next,
        register,
        run_now,
        delete,
        save,
        || Ok(Some(NoopUpdateStateLease)),
    )
}

/// Transaction core with the existing OS seams plus the per-user checker lease.
/// `Lease` is generic so unit tests can model lock ownership without making a
/// second mutex; production supplies the real `StateLock` from the update crate.
#[allow(clippy::too_many_arguments)]
fn apply_automatic_check_transaction_with_lease<Acquire, Lease, Register, RunNow, Delete, Save>(
    reconcile: &AutomaticCheckReconcileState,
    prev: &settings::Settings,
    next: &settings::Settings,
    mut register: Register,
    mut run_now: RunNow,
    mut delete: Delete,
    mut save: Save,
    mut acquire_lease: Acquire,
) -> Result<Option<String>, Vec<FieldError>>
where
    Acquire: FnMut() -> Result<Option<Lease>, String>,
    Register: FnMut() -> Result<nospacekey_update::scheduler::TaskIdentity, String>,
    RunNow: FnMut(&nospacekey_update::scheduler::TaskIdentity) -> Result<(), String>,
    Delete: FnMut(Option<&nospacekey_update::scheduler::TaskIdentity>) -> Result<(), String>,
    Save: FnMut(&settings::Settings) -> Result<(), String>,
{
    let repair_off = !next.update.automatic_check && reconcile.needs_repair();
    // OFF transitions are only reported as successfully persisted after the
    // same StateStore lease used by the checker is held. If the checker won the
    // race, this waits for its bounded HTTP interval; timeout/error leaves
    // settings ON. The ON->run failure rollback acquires the same lease below,
    // after the ON save and before any task delete/OFF save.
    let _state_lease = if (prev.update.automatic_check && !next.update.automatic_check)
        || repair_off
    {
        match acquire_lease() {
            Ok(Some(lease)) => Some(lease),
            Ok(None) => {
                return Err(vec![FieldError {
                    field: "update_automatic_check".into(),
                    message: "自動更新確認の実行中で、OFF を安全に保存できませんでした。少し待って再試行してください。".into(),
                }]);
            }
            Err(error) => {
                return Err(vec![FieldError {
                    field: "update_automatic_check".into(),
                    message: format!(
                        "自動更新確認の排他状態を確認できないため、OFF を保存できませんでした: {error}"
                    ),
                }]);
            }
        }
    } else {
        None
    };

    if !should_register_task(
        prev.update.automatic_check,
        next.update.automatic_check,
        reconcile.needs_repair(),
    ) {
        return save_settings_for_transaction(next, save).map(|_| {
            if prev.update.automatic_check && !next.update.automatic_check {
                let warning = delete_warning(&mut delete);
                if repair_off && warning.is_none() {
                    reconcile.clear_after_successful_apply();
                }
                warning
            } else if repair_off {
                match delete(None) {
                    Ok(()) => {
                        // The stale OFF task was removed while holding the
                        // checker lease. Only this successful retry repairs
                        // the startup warning; an unrelated Apply must not
                        // clear it merely because settings were saved.
                        reconcile.clear_after_successful_apply();
                        None
                    }
                    Err(error) => {
                        let warning = format!(
                            "自動更新確認タスクを削除できませんでした（設定は OFF です。次回起動時に再試行します）: {error}"
                        );
                        reconcile.set_persisted_on_warning(warning.clone());
                        Some(warning)
                    }
                }
            } else {
                None
            }
        });
    }

    let identity = register().map_err(|error| {
        let message = format!("自動更新確認タスクを登録できませんでした: {error}");
        reconcile.set_error(message.clone());
        vec![FieldError {
            field: "update_automatic_check".into(),
            message,
        }]
    })?;
    if let Err(error) = save(next) {
        let _ = delete(Some(&identity));
        return Err(vec![FieldError {
            field: "_io".into(),
            message: format!("設定を保存できませんでした: {error}"),
        }]);
    }

    // The checker reads settings before doing any I/O. Register first, persist
    // ON, and only then request the immediate run so it cannot observe OFF.
    if let Err(error) = run_now(&identity) {
        return Ok(Some(rollback_apply_after_run_failure(
            reconcile,
            next,
            &identity,
            error,
            &mut acquire_lease,
            &mut delete,
            &mut save,
        )));
    }
    Ok(None)
}

/// Roll back an ON Apply only while holding the same checker StateLock used by
/// the startup and OFF transitions.  The ON save already happened before
/// `run_now`; when the lock cannot be acquired, leave that persisted ON alone
/// and do not touch the task or write a speculative OFF snapshot.
fn rollback_apply_after_run_failure<Acquire, Lease, Delete, Save>(
    reconcile: &AutomaticCheckReconcileState,
    next: &settings::Settings,
    identity: &nospacekey_update::scheduler::TaskIdentity,
    run_error: String,
    acquire_lease: &mut Acquire,
    delete: &mut Delete,
    save: &mut Save,
) -> String
where
    Acquire: FnMut() -> Result<Option<Lease>, String>,
    Delete: FnMut(Option<&nospacekey_update::scheduler::TaskIdentity>) -> Result<(), String>,
    Save: FnMut(&settings::Settings) -> Result<(), String>,
{
    let warning = format!("自動更新確認タスクを直ちに実行できませんでした: {run_error}");
    let _state_lease = match acquire_lease() {
        Ok(Some(lease)) => lease,
        Ok(None) => {
            let message = format!(
                "{warning}。自動更新確認の排他状態を取得できないため、タスク削除と OFF 保存を行いませんでした（設定は ON のままです）。"
            );
            reconcile.set_persisted_on_warning(message.clone());
            return message;
        }
        Err(error) => {
            let message = format!(
                "{warning}。自動更新確認の排他状態を確認できないため、タスク削除と OFF 保存を行いませんでした（設定は ON のままです）: {error}"
            );
            reconcile.set_persisted_on_warning(message.clone());
            return message;
        }
    };

    let mut reason = warning;
    if let Err(delete_error) = delete(Some(identity)) {
        reason = format!("{reason}。自動更新確認タスクを削除できませんでした: {delete_error}");
    }
    let (message, off_saved) =
        persist_reconcile_off_with_status(next.clone(), reason, |settings| save(settings));
    if off_saved {
        message
    } else {
        format!("{message}（設定は ON のままです）")
    }
}

fn acquire_update_state_lock() -> Result<Option<nospacekey_update::state::StateLock>, String> {
    let path = nospacekey_update::state::update_state_path().ok_or_else(|| {
        "LOCALAPPDATA が解決できないため、checker の排他を確認できません".to_string()
    })?;
    nospacekey_update::StateStore::new(path)
        .acquire_lock_with_timeout(nospacekey_update::state::STATE_LOCK_WAIT)
        .map_err(|error| format!("checker の state lock を取得できません: {error}"))
}

fn save_settings_for_transaction<Save>(
    settings: &settings::Settings,
    mut save: Save,
) -> Result<(), Vec<FieldError>>
where
    Save: FnMut(&settings::Settings) -> Result<(), String>,
{
    save(settings).map_err(|error| {
        vec![FieldError {
            field: "_io".into(),
            message: format!("設定を保存できませんでした: {error}"),
        }]
    })
}

fn delete_warning<Delete>(delete: &mut Delete) -> Option<String>
where
    Delete: FnMut(Option<&nospacekey_update::scheduler::TaskIdentity>) -> Result<(), String>,
{
    match delete(None) {
        Ok(()) => None,
        Err(error) => Some(format!(
            "自動更新確認タスクを削除できませんでした（設定は OFF です。次回起動時に再試行します）: {error}"
        )),
    }
}

fn should_register_task(prev_enabled: bool, next_enabled: bool, needs_repair: bool) -> bool {
    next_enabled && (!prev_enabled || needs_repair)
}

#[cfg(test)]
fn run_now_succeeded(result: &Result<(), String>) -> bool {
    result.is_ok()
}

fn checker_path() -> std::path::PathBuf {
    let config = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from("NospacekeyConfig.exe"));
    nospacekey_update::scheduler::checker_path_from_config(&config)
}

fn current_task_identity() -> Result<nospacekey_update::scheduler::TaskIdentity, String> {
    nospacekey_update::scheduler::current_user_sid()
        .map(nospacekey_update::scheduler::task_identity)
}

#[tauri::command(async)]
pub fn dismiss_automatic_check_prompt(
    lock: tauri::State<'_, crate::logic::SettingsLock>,
) -> Result<(), String> {
    let _guard = lock
        .0
        .lock()
        .map_err(|_| "設定ロックを取得できませんでした".to_string())?;
    let mut current = settings::load_for_mutation().map_err(settings_mutation_error)?;
    if current.update.automatic_check_prompt_dismissed {
        return Ok(());
    }
    current.update.automatic_check_prompt_dismissed = true;
    settings::save(&current)
        .map_err(|error| format!("案内を閉じた状態を保存できませんでした: {error}"))
}

#[tauri::command]
pub fn consume_update_intent(intent: tauri::State<'_, crate::activation::PendingIntent>) -> bool {
    intent.consume()
}

fn startup_reconcile_load_is_usable(outcome: settings::LoadOutcome) -> bool {
    matches!(
        outcome,
        settings::LoadOutcome::Loaded
            | settings::LoadOutcome::Missing
            | settings::LoadOutcome::Empty
            | settings::LoadOutcome::Corrupt
    )
}

fn startup_reconcile_load_warning(outcome: settings::LoadOutcome) -> String {
    format!(
        "起動時に設定を安全に読み取れないため、自動更新確認タスクは変更しません（{outcome:?}）。"
    )
}

/// Config 起動時の task/settings reconciliation。ON の自己修復に失敗した場合は、
/// OFF の保存に成功したときだけ OFF へ収束する。保存できなかった場合は、永続 ON を
/// 正直に表示したまま inline warning を返す。OFF の残存 task は削除する。
pub fn reconcile_automatic_check_task(reconcile: &AutomaticCheckReconcileState) {
    let (settings, outcome) = settings::load_reporting();
    if !startup_reconcile_load_is_usable(outcome) {
        // PermissionDenied/IoError/NoPath and a failed quarantine are not a
        // trustworthy Settings snapshot.  Do not turn the default DTO into a
        // task delete/save decision; keep the persisted state authoritative.
        reconcile.set_persisted_on_warning(startup_reconcile_load_warning(outcome));
        return;
    }
    if outcome == settings::LoadOutcome::Corrupt {
        // setup runs before the first get_settings call. Preserve the in-process
        // handoff; the cross-process pending ledger remains until the UI acks it.
        reconcile.note_corrupt_recovered();
    }
    // The background worker holds SettingsLock before entering this function.
    // Acquire the checker lease only after that lock, and keep it through every
    // startup save/delete path.
    // The generic seam below keeps this ordering and ownership visible in tests.
    reconcile_automatic_check_task_with_lease(
        reconcile,
        settings,
        current_task_identity,
        || nospacekey_update::scheduler::register_or_update(&checker_path()),
        nospacekey_update::scheduler::run_now,
        nospacekey_update::scheduler::delete,
        move |settings| {
            if matches!(
                outcome,
                settings::LoadOutcome::Loaded
                    | settings::LoadOutcome::Missing
                    | settings::LoadOutcome::Empty
                    | settings::LoadOutcome::Corrupt
            ) {
                settings::save(settings).map_err(|error| error.to_string())
            } else {
                Err(settings_mutation_error(outcome))
            }
        },
        acquire_update_state_lock,
    );
}

/// Startup reconciliation core with all external effects injected. The state
/// lease is acquired for the persisted-OFF task deletion path, or immediately
/// before an ON failure's OFF fallback. It is held until any related settings
/// save and task deletion have completed.
#[allow(clippy::too_many_arguments)]
fn reconcile_automatic_check_task_with_lease<
    Acquire,
    Lease,
    CurrentIdentity,
    Register,
    RunNow,
    Delete,
    Save,
>(
    reconcile: &AutomaticCheckReconcileState,
    settings: settings::Settings,
    mut current_identity: CurrentIdentity,
    mut register: Register,
    mut run_now: RunNow,
    mut delete: Delete,
    mut save: Save,
    mut acquire_lease: Acquire,
) where
    Acquire: FnMut() -> Result<Option<Lease>, String>,
    CurrentIdentity: FnMut() -> Result<nospacekey_update::scheduler::TaskIdentity, String>,
    Register: FnMut() -> Result<nospacekey_update::scheduler::TaskIdentity, String>,
    RunNow: FnMut(&nospacekey_update::scheduler::TaskIdentity) -> Result<(), String>,
    Delete: FnMut(&nospacekey_update::scheduler::TaskIdentity) -> Result<(), String>,
    Save: FnMut(&settings::Settings) -> Result<(), String>,
{
    if !settings.update.automatic_check {
        // Even an already-persisted OFF setting must wait for a checker before
        // deleting its task. A failure is surfaced instead of being swallowed.
        let Some(_state_lease) = acquire_startup_state_lease(reconcile, &mut acquire_lease, true)
        else {
            return;
        };
        match current_identity().and_then(|identity| delete(&identity)) {
            Ok(()) => reconcile.clear(),
            Err(error) => reconcile.set_error(format!(
                "自動更新確認タスクを削除できませんでした（設定は OFF です。次回起動時に再試行します）: {error}"
            )),
        }
        return;
    }

    let identity = match current_identity() {
        Ok(identity) => identity,
        Err(error) => {
            if let Some(_state_lease) =
                acquire_startup_state_lease_with_retry(reconcile, &mut acquire_lease)
            {
                save_reconcile_off_with_task_cleanup(
                    reconcile,
                    settings,
                    format!("自動更新確認のユーザー SID を取得できませんでした: {error}"),
                    None,
                    &mut delete,
                    &mut save,
                );
            }
            return;
        }
    };
    match register() {
        Ok(identity) => {
            // Startup reconciliation has loaded settings as ON, so the
            // immediate run is safe. A lease is needed only if it fails and
            // the task/settings must converge to OFF.
            if let Err(error) = run_now(&identity) {
                if let Some(_state_lease) =
                    acquire_startup_state_lease(reconcile, &mut acquire_lease, false)
                {
                    save_reconcile_off_with_task_cleanup(
                        reconcile,
                        settings,
                        format!("自動更新確認タスクを直ちに実行できませんでした: {error}"),
                        Some(&identity),
                        &mut delete,
                        &mut save,
                    );
                }
            } else {
                reconcile.clear();
            }
        }
        Err(error) => {
            if let Some(_state_lease) =
                acquire_startup_state_lease_with_retry(reconcile, &mut acquire_lease)
            {
                save_reconcile_off_with_task_cleanup(
                    reconcile,
                    settings,
                    format!("自動更新確認タスクを登録できませんでした: {error}"),
                    Some(&identity),
                    &mut delete,
                    &mut save,
                );
            }
        }
    }
}

/// A checker may be finishing its bounded run just as startup reconciliation
/// reaches the fallback path. Give that run one short, bounded chance to
/// release the state lease before publishing the persisted ON warning.
/// Crucially, no OFF snapshot is written while the lease is unavailable.
fn acquire_startup_state_lease_with_retry<Acquire, Lease>(
    reconcile: &AutomaticCheckReconcileState,
    acquire_lease: &mut Acquire,
) -> Option<Lease>
where
    Acquire: FnMut() -> Result<Option<Lease>, String>,
{
    for attempt in 0..STARTUP_LEASE_RETRY_ATTEMPTS {
        if let Some(lease) = acquire_startup_state_lease(reconcile, acquire_lease, false) {
            return Some(lease);
        }
        if attempt + 1 < STARTUP_LEASE_RETRY_ATTEMPTS {
            std::thread::sleep(STARTUP_LEASE_RETRY_DELAY);
        }
    }
    None
}

fn acquire_startup_state_lease<Acquire, Lease>(
    reconcile: &AutomaticCheckReconcileState,
    acquire_lease: &mut Acquire,
    force_off_on_failure: bool,
) -> Option<Lease>
where
    Acquire: FnMut() -> Result<Option<Lease>, String>,
{
    match acquire_lease() {
        Ok(Some(lease)) => Some(lease),
        Ok(None) => {
            let message =
                "自動更新確認の実行中で、起動時の reconcile を安全に完了できませんでした。少し待って再試行してください。".into();
            if force_off_on_failure {
                reconcile.set_error(message);
            } else {
                reconcile.set_persisted_on_warning(message);
            }
            None
        }
        Err(error) => {
            let message = format!(
                "自動更新確認の排他状態を確認できないため、起動時の reconcile を完了できませんでした: {error}"
            );
            if force_off_on_failure {
                reconcile.set_error(message);
            } else {
                reconcile.set_persisted_on_warning(message);
            }
            None
        }
    }
}

fn save_reconcile_off_with_task_cleanup<Delete, Save>(
    reconcile: &AutomaticCheckReconcileState,
    settings: settings::Settings,
    reason: String,
    identity: Option<&nospacekey_update::scheduler::TaskIdentity>,
    delete: &mut Delete,
    save: &mut Save,
) where
    Delete: FnMut(&nospacekey_update::scheduler::TaskIdentity) -> Result<(), String>,
    Save: FnMut(&settings::Settings) -> Result<(), String>,
{
    let (message, off_saved) =
        persist_reconcile_off_with_status(settings, reason, |settings| save(settings));
    if off_saved {
        let message = if let Some(identity) = identity {
            match delete(identity) {
                Ok(()) => message,
                Err(error) => {
                    format!("{message}。自動更新確認タスクを削除できませんでした: {error}")
                }
            }
        } else {
            message
        };
        reconcile.set_error(message);
    } else {
        // The first startup load was ON. If its OFF fallback could not be
        // persisted, the disk truth is still ON and must remain visible.
        reconcile.set_persisted_on_warning(message);
    }
}

#[cfg(test)]
fn persist_reconcile_off<F>(settings: settings::Settings, reason: String, save: F) -> String
where
    F: FnOnce(&settings::Settings) -> Result<(), String>,
{
    persist_reconcile_off_with_status(settings, reason, save).0
}

fn persist_reconcile_off_with_status<F>(
    mut settings: settings::Settings,
    reason: String,
    save: F,
) -> (String, bool)
where
    F: FnOnce(&settings::Settings) -> Result<(), String>,
{
    settings.update.automatic_check = false;
    settings.update.automatic_check_prompt_dismissed = true;
    match save(&settings) {
        Ok(()) => (reason, true),
        Err(error) => (
            format!("{reason}。自動確認を OFF に保存できませんでした: {error}"),
            false,
        ),
    }
}

#[tauri::command]
pub fn get_default_settings() -> SettingsDto {
    logic::to_dto(&settings::Settings::default())
}

/// 記号個別選択グリッドの1項目(半角/全角プレビュー)。JS に写像表を持たせないための供給源
/// （2026-08-02 spec §3）。
#[derive(serde::Serialize)]
pub struct SymbolCatalogEntry {
    pub half: char,
    pub full: char,
}

/// 記号個別選択の対象29件カタログ（read-only、`get_default_settings` と同列）。
#[tauri::command]
pub fn get_symbol_catalog() -> Vec<SymbolCatalogEntry> {
    settings::symbol::symbol_targets()
        .map(|(half, full)| SymbolCatalogEntry { half, full })
        .collect()
}

#[derive(serde::Serialize)]
pub struct AppInfo {
    pub version: String,
    pub build_hash: String,
    pub settings_path: String,
}

/// The engine creates this mutex before loading dictionaries/models and keeps
/// its handle alive until process exit.  Holding a newly-created object with
/// the same name therefore reserves the entire "engine is absent" interval:
/// an engine that races with file deletion observes `ERROR_ALREADY_EXISTS`
/// and exits before touching learning memory.
fn engine_singleton_mutex_name(pipe: &str) -> String {
    format!(
        "Local\\nospacekey-engine-singleton-{}",
        pipe.replace('\\', "_")
    )
}

/// Swift `LearningSettings.coordinationScope` と同じ user-profile scope。
/// Global object 名をユーザーごとに分離しつつ、別 logon session では同じ名前にする。
fn learning_coordination_scope(raw: &str) -> String {
    let normalized = raw.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let mut hash = 14_695_981_039_346_656_037u64;
    for byte in normalized.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    format!("{hash:016x}")
}

fn learning_memory_dir_from_env() -> Result<PathBuf, String> {
    match std::env::var_os("NOSPACEKEY_MEMORY_DIR").filter(|v| !v.is_empty()) {
        Some(path) => Ok(PathBuf::from(path)),
        None => {
            let local = std::env::var_os("LOCALAPPDATA")
                .filter(|v| !v.is_empty())
                .ok_or_else(|| "LOCALAPPDATA が解決できません。".to_string())?;
            Ok(PathBuf::from(local).join("nospacekey").join("memory"))
        }
    }
}

fn learning_coordination_scope_from_env() -> Result<String, String> {
    let path = learning_memory_dir_from_env()?;
    Ok(learning_coordination_scope(&path.to_string_lossy()))
}

fn learning_lifecycle_mutex_name(scope: &str) -> String {
    format!(r"Global\nospacekey-learning-lifecycle-{scope}")
}

fn learning_presence_mutex_name(scope: &str, session_id: u32) -> String {
    format!(r"Global\nospacekey-learning-presence-{scope}-s{session_id}")
}

/// EngineHost が process lifetime 中保持する presence object の存在を調べる。
/// lifecycle gate の ownership 中にだけ呼ぶため、「不在確認直後に別 session Engine が起動」する
/// TOCTOU は EngineHost 起動側の同じ gate 待ちで閉じる。
fn learning_presence_exists(scope: &str, session_id: u32) -> Result<bool, String> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    let name = HSTRING::from(learning_presence_mutex_name(scope, session_id));
    unsafe {
        let created = CreateMutexW(None, false, &name);
        let last_error = GetLastError();
        match created {
            Ok(handle) => {
                let exists = last_error == ERROR_ALREADY_EXISTS;
                let _ = CloseHandle(handle);
                Ok(exists)
            }
            Err(e) => Err(format!(
                "session {session_id} のエンジン状態を確認できませんでした: {e}"
            )),
        }
    }
}

fn logon_session_ids() -> Result<Vec<u32>, String> {
    use windows::Win32::System::RemoteDesktop::{
        WTSEnumerateSessionsW, WTSFreeMemory, WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW,
    };

    let mut sessions: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        WTSEnumerateSessionsW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            0,
            1,
            &mut sessions,
            &mut count,
        )
        .map_err(|e| format!("Windows session 一覧を取得できませんでした: {e}"))?;
        let ids = if sessions.is_null() || count == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(sessions, count as usize)
                .iter()
                .map(|session| session.SessionId)
                .collect()
        };
        if !sessions.is_null() {
            WTSFreeMemory(sessions.cast());
        }
        Ok(ids)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WindowsSessionUser {
    domain: String,
    user: String,
}

fn same_windows_session_user(a: &WindowsSessionUser, b: &WindowsSessionUser) -> bool {
    a.domain.eq_ignore_ascii_case(&b.domain) && a.user.eq_ignore_ascii_case(&b.user)
}

/// WTS が確保した UTF-16 buffer を、返却 byte 長の内側だけで検証して String 化する。
/// API/形式エラーは「別ユーザー」とみなさず fail-closed にする。
fn wts_session_text(
    session_id: u32,
    info_class: windows::Win32::System::RemoteDesktop::WTS_INFO_CLASS,
    label: &str,
) -> Result<String, String> {
    use windows::core::PWSTR;
    use windows::Win32::System::RemoteDesktop::{
        WTSFreeMemory, WTSQuerySessionInformationW, WTS_CURRENT_SERVER_HANDLE,
    };

    let mut buffer = PWSTR::null();
    let mut bytes = 0u32;
    let queried = unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            session_id,
            info_class,
            &mut buffer,
            &mut bytes,
        )
    };
    if let Err(e) = queried {
        if !buffer.is_null() {
            unsafe { WTSFreeMemory(buffer.as_ptr().cast()) };
        }
        return Err(format!(
            "session {session_id} の {label} を確認できませんでした: {e}"
        ));
    }

    let decoded = if buffer.is_null() {
        if bytes == 0 {
            Ok(String::new())
        } else {
            Err(format!(
                "session {session_id} の {label} が不正な null buffer を返しました。"
            ))
        }
    } else if !bytes.is_multiple_of(2) {
        Err(format!(
            "session {session_id} の {label} が不正な UTF-16 長を返しました。"
        ))
    } else {
        let wide = unsafe { std::slice::from_raw_parts(buffer.as_ptr(), (bytes / 2) as usize) };
        let len = wide
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(wide.len());
        String::from_utf16(&wide[..len])
            .map_err(|_| format!("session {session_id} の {label} が不正な UTF-16 です。"))
    };
    if !buffer.is_null() {
        unsafe { WTSFreeMemory(buffer.as_ptr().cast()) };
    }
    decoded
}

fn wts_session_user(session_id: u32) -> Result<Option<WindowsSessionUser>, String> {
    use windows::Win32::System::RemoteDesktop::{WTSDomainName, WTSUserName};

    let user = wts_session_text(session_id, WTSUserName, "ユーザー名")?;
    if user.is_empty() {
        // Services/session 0 等の、対話ユーザーを持たない session。
        return Ok(None);
    }
    let domain = wts_session_text(session_id, WTSDomainName, "ドメイン名")?;
    Ok(Some(WindowsSessionUser { domain, user }))
}

fn has_other_same_user_session(
    current: u32,
    current_user: &WindowsSessionUser,
    sessions: &[(u32, Option<WindowsSessionUser>)],
) -> bool {
    sessions.iter().any(|(session_id, user)| {
        *session_id != current
            && user
                .as_ref()
                .is_some_and(|user| same_windows_session_user(current_user, user))
    })
}

fn ensure_no_other_same_user_session(current: u32, sessions: &[u32]) -> Result<(), String> {
    let session_users = sessions
        .iter()
        .map(|session_id| Ok((*session_id, wts_session_user(*session_id)?)))
        .collect::<Result<Vec<_>, String>>()?;
    let current_user = session_users
        .iter()
        .find_map(|(session_id, user)| (*session_id == current).then_some(user.as_ref()).flatten())
        .ok_or_else(|| {
            "現在の Windows session のユーザーを確認できないため、安全に消去できません。"
                .to_string()
        })?;
    if has_other_same_user_session(current, current_user, &session_users) {
        return Err(
            "同じユーザーの別 Windows session がログオン中のため、学習履歴を消去できません。そちらの session をサインアウトしてから再試行してください。"
                .into(),
        );
    }
    Ok(())
}

/// ClearLearning の全区間で保持する user-wide gate。別 session Engine が既に生存するなら
/// fail-closed、新規 Engine 起動は gate 解放まで待たせる。これにより current-session IPC だけで
/// clear しても、別 Engine の古い RAM が後から共有 memory へ flush される経路を成立させない。
struct LearningClearLease {
    handle: windows::Win32::Foundation::HANDLE,
    scope: String,
    current: u32,
}

impl LearningClearLease {
    fn acquire() -> Result<(Self, bool), String> {
        use windows::core::HSTRING;
        use windows::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};

        let scope = learning_coordination_scope_from_env()?;
        let name = HSTRING::from(learning_lifecycle_mutex_name(&scope));
        let handle = unsafe { CreateMutexW(None, false, &name) }
            .map_err(|e| format!("学習消去 gate を作成できませんでした: {e}"))?;
        let wait = unsafe { WaitForSingleObject(handle, 4_000) };
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err("別のエンジンが起動処理中です。少し待って再試行してください。".into());
        }
        let current = ipc::client::current_session_id();
        let lease = Self {
            handle,
            scope,
            current,
        };
        // The current session may have no EngineHost at all.  We still
        // perform the full session/presence audit here, but expose that one
        // fact to the caller so only the proven-absent path can use the
        // destructive file fallback.
        let current_present = lease.revalidate_allow_absent()?;
        Ok((lease, current_present))
    }

    /// gate 保持中の session/presence 再検証。Clear 応答後にも呼び、新しい同一ユーザー
    /// session や旧 EngineHost への入れ替わりを成功扱いにしない。
    fn revalidate(&self) -> Result<(), String> {
        self.revalidate_with_current_presence(true).map(|_| ())
    }

    fn revalidate_allow_absent(&self) -> Result<bool, String> {
        self.revalidate_with_current_presence(false)
    }

    fn revalidate_with_current_presence(&self, require_current: bool) -> Result<bool, String> {
        let sessions = logon_session_ids()?;
        let current = self.current;
        if !sessions.contains(&current) {
            return Err("現在の Windows session を確認できないため、安全に消去できません。".into());
        }
        ensure_no_other_same_user_session(current, &sessions)?;
        let current_present = learning_presence_exists(&self.scope, current)?;
        if require_current && !current_present {
            return Err(
                "現在のエンジンが学習消去の session 協調に対応していません。IME を再起動して再試行してください。"
                    .into(),
            );
        }
        for session_id in sessions.into_iter().filter(|id| *id != current) {
            if learning_presence_exists(&self.scope, session_id)? {
                return Err(
                    "別の Windows session で nospacekey エンジンが動作中のため、学習履歴を消去できません。そちらの session をサインアウトしてから再試行してください。"
                        .into(),
                );
            }
        }
        Ok(current_present)
    }
}

impl Drop for LearningClearLease {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::ReleaseMutex;
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

pub(crate) struct EngineAbsenceLease(windows::Win32::Foundation::HANDLE);

impl EngineAbsenceLease {
    pub(crate) fn acquire(pipe: &str) -> Result<Self, String> {
        use windows::core::HSTRING;
        use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
        use windows::Win32::System::Threading::CreateMutexW;

        let name = HSTRING::from(engine_singleton_mutex_name(pipe));
        unsafe {
            // GetLastError must be sampled immediately after CreateMutexW:
            // successful opens report ERROR_ALREADY_EXISTS only this way.
            let created = CreateMutexW(None, false, &name);
            let last_error = GetLastError();
            match created {
                Ok(handle) if last_error == ERROR_ALREADY_EXISTS => {
                    let _ = CloseHandle(handle);
                    Err("エンジンが起動中または実行中です。少し待って再試行してください".into())
                }
                Ok(handle) => Ok(Self(handle)),
                Err(e) => Err(format!(
                    "エンジン停止状態を確認できないため、学習履歴を削除しませんでした: {e}"
                )),
            }
        }
    }
}

impl Drop for EngineAbsenceLease {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LearningEntryKind {
    Regular,
    Reparse,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LearningEntry {
    path: PathBuf,
    name: std::ffi::OsString,
    kind: LearningEntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LearningScanError {
    NotFound,
    Failed(String),
}

fn metadata_is_reparse(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn scan_learning_directory(path: &Path) -> Result<Vec<LearningEntry>, LearningScanError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            LearningScanError::NotFound
        } else {
            LearningScanError::Failed(format!("学習履歴フォルダを確認できません: {error}"))
        }
    })?;
    if metadata_is_reparse(&metadata) || !metadata.is_dir() {
        return Err(LearningScanError::Failed(
            "学習履歴フォルダが通常の directory ではありません。".into(),
        ));
    }

    let entries = std::fs::read_dir(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            LearningScanError::NotFound
        } else {
            LearningScanError::Failed(format!("学習履歴フォルダを列挙できません: {error}"))
        }
    })?;
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            LearningScanError::Failed(format!("学習履歴ファイルを列挙できません: {error}"))
        })?;
        let path = entry.path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(LearningScanError::Failed(format!(
                    "学習履歴ファイルを確認できません: {error}"
                )))
            }
        };
        let kind = if metadata_is_reparse(&metadata) {
            LearningEntryKind::Reparse
        } else if metadata.is_file() {
            LearningEntryKind::Regular
        } else {
            LearningEntryKind::Other
        };
        result.push(LearningEntry {
            name: entry.file_name(),
            path,
            kind,
        });
    }
    Ok(result)
}

fn is_learning_file_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name == ".pause"
        || name == "corrections.json"
        || name == "learningMemory.txt"
        || name.starts_with("memory")
}

fn scan_error_message(error: LearningScanError) -> String {
    match error {
        LearningScanError::NotFound => "学習履歴フォルダは既に存在しません。".into(),
        LearningScanError::Failed(message) => message,
    }
}

/// Delete only the allowlisted regular files, with the scanner/remover
/// injected so the safety sequence is testable without touching real user
/// data.  The second scan is a verification barrier; a newly appearing
/// allowlisted file is an error rather than an optimistic success.
fn clear_learning_files_with<Scan, Remove>(
    path: &Path,
    mut scan: Scan,
    mut remove: Remove,
) -> Result<(), String>
where
    Scan: FnMut(&Path) -> Result<Vec<LearningEntry>, LearningScanError>,
    Remove: FnMut(&Path) -> std::io::Result<()>,
{
    let entries = match scan(path) {
        Ok(entries) => entries,
        Err(LearningScanError::NotFound) => return Ok(()),
        Err(error) => return Err(scan_error_message(error)),
    };
    for entry in entries
        .iter()
        .filter(|entry| is_learning_file_name(&entry.name))
    {
        match entry.kind {
            LearningEntryKind::Regular => match remove(&entry.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("学習履歴ファイルを削除できません: {error}"));
                }
            },
            LearningEntryKind::Reparse => {
                return Err("学習履歴の対象が reparse point のため削除しません。".into())
            }
            LearningEntryKind::Other => {
                return Err("学習履歴の対象が regular file ではないため削除しません。".into())
            }
        }
    }

    let remaining = match scan(path) {
        Ok(entries) => entries,
        Err(LearningScanError::NotFound) => return Ok(()),
        Err(error) => return Err(scan_error_message(error)),
    };
    if remaining
        .iter()
        .any(|entry| is_learning_file_name(&entry.name))
    {
        return Err("学習履歴ファイルの削除後も対象が残っています。".into());
    }
    Ok(())
}

fn clear_learning_files(path: &Path) -> Result<(), String> {
    clear_learning_files_with(path, scan_learning_directory, |entry| {
        std::fs::remove_file(entry)
    })
}

#[tauri::command]
pub fn get_app_info() -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_hash: env!("GIT_HASH").to_string(),
        settings_path: settings::settings_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    }
}

/// Spec2: 学習履歴を ClearLearning IPC で消去する（RAM+ディスクをエンジン自身が
/// serviceLock 下で処理するため、mmap や遅延 flush と競合しない）。パイプ名は TIP と同じ
/// per-logon-session 名（ipc::client::stable_pipe_name）。現 session の Engine が存在する
/// 場合は接続失敗時も直接ファイル削除へ fallback しない。current presence が gate 下で
/// 不在と確認できた場合だけ、同じ pipe の EngineAbsenceLease を取ってから allowlist の
/// regular files を個別に消去する（foreign entry、directory、reparse point は触らない）。
///
/// `(async)` 必須（I-1）: Tauri v2 の同期 command は main スレッド実行のため、blocking な
/// pipe I/O で UI がフリーズする。さらに素の request()（deadline 無し）はエンジンが
/// warm-up（converterLock 数秒保持）やハング中だと無期限ブロックするので、A8 の
/// `request_within` で 2 秒の deadline を切る。
#[tauri::command(async)]
pub fn clear_learning_history() -> Result<String, String> {
    use std::time::Duration;
    // current session の Engine へだけ届く IPC を送る前に、同一ユーザーの全 session を
    // fail-closed で監査し、新規 Engine 起動も応答完了まで止める。
    let (cross_session_lease, current_engine_present) = LearningClearLease::acquire()?;
    let pipe = ipc::client::stable_pipe_name();
    if current_engine_present {
        return match ipc::client::EngineClient::connect_to(&pipe, Duration::from_millis(250)) {
            Ok(mut c) => {
                request_clear_learning_once(&mut c)?;
                // 最初の消去中に同一ユーザー session が現れた場合は成功を返さない。検証までに
                // 一時 session が終了して旧 RAM を flush 済みでも、二度目の冪等 Clear がディスクを
                // 最終的に空へ戻す。検証後に現れる Engine は一度目の空ディスクからしか読めない。
                cross_session_lease.revalidate()?;
                request_clear_learning_once(&mut c)?;
                Ok("engine".into())
            }
            Err(e) => Err(format!(
                "エンジンに接続できないため、学習履歴を安全に消去できませんでした。IME を有効にして少し待ってから再試行してください: {e}"
            )),
        };
    }

    // No current presence was observed while the user-wide lifecycle gate was
    // held.  Reserve the same per-pipe singleton before rechecking, so a
    // half-started Engine cannot race the file deletion path.
    let _absence_lease = EngineAbsenceLease::acquire(&pipe)?;
    if cross_session_lease.revalidate_allow_absent()? {
        return Err("エンジンが起動したため、学習履歴を安全に削除できませんでした。".into());
    }
    let memory_dir = learning_memory_dir_from_env()?;
    clear_learning_files(&memory_dir)?;
    Ok("files".into())
}

#[tauri::command]
pub fn open_settings_dir() {
    // explorer /select,<path> でファイルを選択状態でフォルダを開く（ファイル不在でもフォルダは開く）。
    let Some(p) = settings::settings_path() else {
        return;
    };
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", p.display()))
        .spawn();
}

/// `--stop-engine` の終了コード判定（純関数）。sent=接続できて Shutdown を送った、
/// gone=その後 pipe が消えた（＝停止確認）。exit code は診断用（.iss は code に依らず taskkill へ
/// 進む）: 0 = 停止完了 or 元々不在 / 1 = 送ったが 3s 以内に消えない。
fn stop_engine_exit_code(sent: bool, gone: bool) -> i32 {
    if !sent || gone {
        0
    } else {
        1
    }
}

/// アンインストーラ/更新から `NospacekeyConfig.exe --stop-engine` で呼ばれる graceful 停止。
/// 常駐エンジンへ Request::Shutdown を送り（エンジンは学習 flush→応答後 exit）、pipe の消滅を
/// 最大 3s ポーリングして停止を確認する。GUI は出さない（main は Tauri init 前にこれを呼ぶ）。
/// パイプ名は TIP と同じ per-logon-session 名（stable_pipe_name）＝現セッション分のみ止まる。
/// 他ユーザセッションのエンジンや graceful 失敗分は .iss の elevated taskkill が掃討する。
pub fn stop_engine() -> i32 {
    use std::time::{Duration, Instant};
    let pipe = ipc::client::stable_pipe_name();
    let Ok(mut c) = ipc::client::EngineClient::connect_to(&pipe, Duration::from_millis(250)) else {
        // エンジン不在（接続失敗）＝止めるものが無い＝成功。
        return stop_engine_exit_code(false, false);
    };
    // engine は応答を書いてから exit するので、応答（Ok / 読取り時 broken pipe）は問わない —
    // 真の停止判定は下の pipe 消滅ポーリング。deadline は 1s（flush 込みでも余裕）。
    let deadline = Instant::now() + Duration::from_millis(1000);
    let _ = c.request_within(&ipc::protocol::Request::Shutdown, deadline);
    drop(c); // 送信済み接続は用済み。停止判定は新規 connect で行うので c は保持しない。
             // pipe 消滅を最大 3s ポーリング（connect(0ms) が Err になるまで 100ms 間隔）。
    let poll_until = Instant::now() + Duration::from_millis(3000);
    let mut gone = false;
    while Instant::now() < poll_until {
        if ipc::client::EngineClient::connect_to(&pipe, Duration::ZERO).is_err() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    stop_engine_exit_code(true, gone)
}

/// Releases ページ URL を repository から組み立てる純関数。末尾 `.git`／`/` を落として
/// `/releases/latest` を連結する。
fn releases_url(repo: &str) -> String {
    let base = repo
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_end_matches('/');
    format!("{base}/releases/latest")
}

/// 情報画面の「更新を確認」。既定ブラウザで Releases ページを開く（explorer に URL を
/// 渡して委譲＝ open_settings_dir と同型。新規依存・新規 capability 不要）。
#[tauri::command]
pub fn open_releases_page() {
    let _ = std::process::Command::new("explorer.exe")
        .arg(releases_url(env!("CARGO_PKG_REPOSITORY")))
        .spawn();
}

/// Zenzai モデルの帰属表示（作者ページ / ライセンス）を既定ブラウザで開く。
/// URL は allowlist に固定する: フロントから任意 URL を shell へ渡せると、汚染された
/// UI 文字列で任意サイト（ローカル UNC 含む）を開かせる余地が生まれるため。
#[tauri::command]
pub fn open_external_url(url: String) {
    if is_allowed_external_url(&url) {
        let _ = std::process::Command::new("explorer.exe").arg(&url).spawn();
    }
}

/// 開いてよい外部 URL（帰属表示リンク）の allowlist 判定（純関数）。
fn is_allowed_external_url(url: &str) -> bool {
    const ALLOW: &[&str] = &[
        "https://huggingface.co/Miwa-Keita/zenz-v3.1-small-gguf",
        "https://creativecommons.org/licenses/by-sa/4.0/",
    ];
    ALLOW.contains(&url)
}

// ============================================================================
// カスタム辞書 CRUD(Issue #3 spec §5.3)。logic::dict_*_logic を State から借りて
// 呼ぶだけの薄層に保つ(mutation 本体・直列化は logic.rs 側で単体テスト済み)。
// ============================================================================

/// 辞書系 IPC の接続 timeout(spec §4.2)。`clear_learning_history` の 250ms より短縮する —
/// mutation ごとにエンジン不在で待たされると連続登録の体感に響くため、ローカル pipe の
/// 生存確認としては 100ms で十分。
const DICT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);
const DICT_REQUEST_DEADLINE: std::time::Duration = std::time::Duration::from_millis(2000);

/// ReloadDictionary を実際にパイプへ送る唯一の実装。enabled は呼び出し側
/// (`reload_payload` でゲート済み)から渡す — ここで settings を読み直さない
/// (logic 層が enabled をでっち上げない不変条件と対の規律)。
fn send_reload_over_pipe(enabled: bool) -> EngineStatus {
    use std::time::Instant;
    let pipe = ipc::client::stable_pipe_name();
    let Ok(mut c) = ipc::client::EngineClient::connect_to(&pipe, DICT_CONNECT_TIMEOUT) else {
        return EngineStatus::Absent;
    };
    let deadline = Instant::now() + DICT_REQUEST_DEADLINE;
    match c.request_within(
        &ipc::protocol::Request::ReloadDictionary { enabled },
        deadline,
    ) {
        Ok(ipc::protocol::Response::Ok) => EngineStatus::Applied,
        Ok(ipc::protocol::Response::Error { .. }) => EngineStatus::Declined,
        Ok(_) => EngineStatus::Declined, // 未知応答は旧エンジンの拒否と同型に畳む
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => EngineStatus::Timeout,
        Err(_) => EngineStatus::Absent, // 送信後の切断等も「反映されなかった」で無害に畳む
    }
}

/// ReloadDictionary を送る全コマンド共通のゲート(spec §4.2)。送る enabled は常にディスクの
/// settings.json から読む(UI の未適用トグル値は送らない)。読み取りが `Loaded`/`Missing`
/// 以外なら `reload_payload` が送信を止める。
/// 巡3 Z6 + 巡4 B3: **呼び出し時に**ディスクを読み、かつ**読み〜パイプ送信まで SettingsLock を
/// 保持する**（生成時の eager 捕捉は stale-capture の源、非保持だと「辞書 mutation が読んだ値→
/// 適用が保存→mutation の送信が後着で旧値を送り返す」ms 級の逆転が残る。エンジン側は接続別
/// スレッド並行・desired 最終書き込み勝ちで自動回復なし）。ロック順は Dict→Settings の一方向のみ。
fn reload_sender(slock: &crate::logic::SettingsLock) -> impl Fn() -> EngineStatus + '_ {
    move || {
        let _guard = slock.0.lock().unwrap();
        let (s, outcome) = settings::load_reporting();
        match logic::reload_payload(outcome, s.user_dictionary.enabled) {
            Some(enabled) => send_reload_over_pipe(enabled),
            None => EngineStatus::Absent,
        }
    }
}

fn dict_path_or_err() -> Result<std::path::PathBuf, DictCmdError> {
    settings::user_dictionary::dict_path().ok_or_else(|| DictCmdError::Io {
        message: "LOCALAPPDATA が解決できません".into(),
    })
}

#[tauri::command(async)]
pub fn dict_list(lock: tauri::State<DictLock>) -> Result<ListReport, DictCmdError> {
    logic::dict_list_logic(&lock, &dict_path_or_err()?)
}

#[tauri::command(async)]
pub fn dict_add(
    lock: tauri::State<DictLock>,
    slock: tauri::State<'_, crate::logic::SettingsLock>,
    ruby: String,
    word: String,
    pos: String,
) -> Result<MutationReport, DictCmdError> {
    let path = dict_path_or_err()?;
    // 巡4 B3: State は Deref — &*slock でローカル参照を作り sender へ渡す
    // （読み〜送信の SettingsLock 保持は reload_sender 内で行う）。
    let slock = &*slock;
    logic::dict_add_logic(&lock, &path, &reload_sender(slock), &ruby, &word, &pos)
}

#[tauri::command(async)]
pub fn dict_update(
    lock: tauri::State<DictLock>,
    slock: tauri::State<'_, crate::logic::SettingsLock>,
    old_ruby: String,
    old_word: String,
    ruby: String,
    word: String,
    pos: String,
) -> Result<MutationReport, DictCmdError> {
    let path = dict_path_or_err()?;
    let slock = &*slock;
    logic::dict_update_logic(
        &lock,
        &path,
        &reload_sender(slock),
        &old_ruby,
        &old_word,
        &ruby,
        &word,
        &pos,
    )
}

#[tauri::command(async)]
pub fn dict_delete(
    lock: tauri::State<DictLock>,
    slock: tauri::State<'_, crate::logic::SettingsLock>,
    ruby: String,
    word: String,
) -> Result<MutationReport, DictCmdError> {
    let path = dict_path_or_err()?;
    let slock = &*slock;
    logic::dict_delete_logic(&lock, &path, &reload_sender(slock), &ruby, &word)
}

/// TSV ファイルを選んでインポートする。ダイアログ表示・ファイル読み込みは mutex の外
/// (spec §5.3)。キャンセルは `Ok(None)`。
/// 巡3 Q6: ピッカーは設定ウィンドウをオーナーにするモーダル — Rust 直呼びの
/// `app.dialog().file()` は既定で親を持たず、表示中も設定UI全体が操作可能になる
/// （プラグインの JS API 側は常に set_parent するのと対照的）。
#[tauri::command(async)]
pub fn dict_import(
    app: tauri::AppHandle,
    window: tauri::Window,
    lock: tauri::State<DictLock>,
    slock: tauri::State<'_, crate::logic::SettingsLock>,
) -> Result<Option<ImportReportDto>, DictCmdError> {
    use tauri_plugin_dialog::DialogExt;
    let Some(picked) = app
        .dialog()
        .file()
        .set_parent(&window)
        .add_filter("TSV", &["tsv", "txt"])
        .blocking_pick_file()
    else {
        return Ok(None);
    };
    let file_path = picked.into_path().map_err(|e| DictCmdError::Io {
        message: e.to_string(),
    })?;
    let bytes = std::fs::read(&file_path).map_err(|e| DictCmdError::Io {
        message: e.to_string(),
    })?;
    let path = dict_path_or_err()?;
    let slock = &*slock;
    logic::dict_import_logic(&lock, &path, &reload_sender(slock), &bytes).map(Some)
}

/// TSV ファイルへエクスポートする。保存先ダイアログは mutex の外(spec §5.3)。
/// キャンセルは `Ok(None)`。ピッカーのモーダル化は dict_import と同じ（巡3 Q6）。
#[tauri::command(async)]
pub fn dict_export(
    app: tauri::AppHandle,
    window: tauri::Window,
    lock: tauri::State<DictLock>,
) -> Result<Option<ExportReportDto>, DictCmdError> {
    use tauri_plugin_dialog::DialogExt;
    let Some(picked) = app
        .dialog()
        .file()
        .set_parent(&window)
        .add_filter("TSV", &["tsv"])
        .set_file_name("user_dictionary.tsv")
        .blocking_save_file()
    else {
        return Ok(None);
    };
    let file_path = picked.into_path().map_err(|e| DictCmdError::Io {
        message: e.to_string(),
    })?;
    let path = dict_path_or_err()?;
    let (tsv, report) = logic::dict_export_logic(&lock, &path)?;
    std::fs::write(&file_path, tsv.as_bytes()).map_err(|e| DictCmdError::Io {
        message: e.to_string(),
    })?;
    Ok(Some(report))
}

/// settings 適用成功後にフロントが fire-and-forget で呼ぶトグル反映(spec §4.2)。
/// エントリ mutation(§4.1 の起動時 enqueue で救済される)と異なり、トグル適用は engine
/// init〜pipe 作成の短い窓に落ちると次回まで無言で効かないため、接続失敗時のみ
/// 300ms×3 リトライして窓を実質閉塞する。
/// 巡3 Z5: 「送らない」(Corrupt 等の読み取り抑止)と「届かなかった」(接続失敗)を区別する —
/// load_reporting は Corrupt を退避するため、抑止後にリトライすると 2 回目が Missing で
/// 既定値(enabled=true)を送ってしまい「破損時は送らない」不変条件を迂回する。抑止は即返し、
/// 接続失敗だけが再試行対象。
/// 巡3 Z6: 読み込み〜パイプ送信まで SettingsLock を保持(apply_settings と直列化 —
/// sleep 中に別適用が保存した値を古い要求が追い越すのを防ぐ。ロック順はこの関数のみが
/// Settings→(送信)で逆順を取る者はいない)。
#[tauri::command(async)]
pub fn dict_sync_engine(lock: tauri::State<'_, crate::logic::SettingsLock>) -> EngineStatus {
    for attempt in 0..3u32 {
        let status = {
            let _guard = lock.inner().0.lock().unwrap();
            let (s, outcome) = settings::load_reporting();
            match logic::reload_payload(outcome, s.user_dictionary.enabled) {
                Some(enabled) => send_reload_over_pipe(enabled),
                // 抑止（送るべきではない）— リトライせず即抜け。
                None => return EngineStatus::Absent,
            }
        };
        match status {
            EngineStatus::Absent if attempt + 1 < 3 => {
                // _guard は上のブロック抜けで解放済み（sleep をロック保持外で寝る）。
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            status => return status,
        }
    }
    EngineStatus::Absent
}

fn request_clear_learning_once(c: &mut ipc::client::EngineClient) -> Result<(), String> {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_millis(2000);
    match c.request_within(&ipc::protocol::Request::ClearLearning, deadline) {
        Ok(ipc::protocol::Response::Ok) => Ok(()),
        Ok(ipc::protocol::Response::Error { message }) => {
            Err(format!("エンジンが消去を拒否しました: {message}"))
        }
        Ok(other) => Err(format!("予期しない応答: {other:?}")),
        Err(e) => Err(format!(
            "エンジンが応答しません（変換中の可能性）。少し待って再試行してください: {e}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_automatic_check_transaction, apply_automatic_check_transaction_with_lease,
        clear_learning_files_with, corrupt_recovered, engine_singleton_mutex_name,
        get_settings_with_timeout, get_symbol_catalog, has_other_same_user_session,
        is_allowed_external_url, learning_coordination_scope, learning_lifecycle_mutex_name,
        learning_presence_mutex_name, persist_reconcile_off, persist_reconcile_off_with_status,
        reconcile_automatic_check_task_with_lease, releases_url, run_now_succeeded,
        run_reconcile_worker_with, should_register_task, startup_reconcile_load_is_usable,
        startup_reconcile_load_warning, stop_engine_exit_code, AutomaticCheckReconcileState,
        EngineAbsenceLease, LearningEntry, LearningEntryKind, LearningScanError,
        ReconcileCompletion, WindowsSessionUser,
    };
    use std::cell::{Cell, RefCell};
    use std::ffi::OsString;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
    use std::thread;
    use std::time::Duration;

    static LOCALAPPDATA_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn localappdata_test_lock() -> MutexGuard<'static, ()> {
        LOCALAPPDATA_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct LocalAppDataGuard {
        previous: Option<OsString>,
    }

    impl LocalAppDataGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("LOCALAPPDATA");
            std::env::set_var("LOCALAPPDATA", path);
            Self { previous }
        }
    }

    impl Drop for LocalAppDataGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("LOCALAPPDATA", value),
                None => std::env::remove_var("LOCALAPPDATA"),
            }
        }
    }

    #[test]
    fn reconcile_completion_seam_covers_pending_success_error_and_timeout() {
        let pending = AutomaticCheckReconcileState::default();
        assert_eq!(pending.completion(), ReconcileCompletion::Pending);

        let success = AutomaticCheckReconcileState::default();
        success.complete(ReconcileCompletion::Success);
        assert_eq!(
            success.wait_for_completion(Duration::ZERO),
            ReconcileCompletion::Success
        );

        let error = AutomaticCheckReconcileState::default();
        error.complete(ReconcileCompletion::Error);
        assert_eq!(
            error.wait_for_completion(Duration::ZERO),
            ReconcileCompletion::Error
        );

        let timeout = AutomaticCheckReconcileState::default();
        assert_eq!(
            timeout.wait_for_completion(Duration::from_millis(1)),
            ReconcileCompletion::Timeout
        );
        assert_eq!(timeout.completion(), ReconcileCompletion::Timeout);
    }

    #[test]
    fn successful_apply_clears_error_and_only_error_completion_becomes_success() {
        let error = AutomaticCheckReconcileState::default();
        error.set_error("worker failed".into());
        error.complete(ReconcileCompletion::Error);
        error.clear_after_successful_apply();
        assert_eq!(error.completion(), ReconcileCompletion::Success);
        assert!(error.error().is_none());
        assert!(!error.needs_repair());
        assert!(!error.forces_automatic_off());

        for status in [ReconcileCompletion::Pending, ReconcileCompletion::Timeout] {
            let state = AutomaticCheckReconcileState::default();
            state.set_persisted_on_warning("late worker state".into());
            if status == ReconcileCompletion::Timeout {
                state.complete(status);
            }
            state.clear_after_successful_apply();
            assert_eq!(state.completion(), status);
            assert!(state.error().is_none());
            assert!(!state.needs_repair());
            assert!(!state.forces_automatic_off());
        }
    }

    #[test]
    fn successful_apply_removes_terminal_worker_error_from_get_settings() {
        let _lock = localappdata_test_lock();
        let base = std::env::temp_dir().join(format!(
            "nospacekey-config-apply-error-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let _env = LocalAppDataGuard::set(&base);
        let path = settings::settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut persisted = settings::Settings::default();
        persisted.update.automatic_check = true;
        std::fs::write(&path, persisted.to_json()).unwrap();

        let state = AutomaticCheckReconcileState::default();
        state.set_error("worker error".into());
        state.complete(ReconcileCompletion::Error);
        assert!(get_settings_with_timeout(&state, Duration::ZERO)
            .update_task_error
            .is_some());
        state.clear_after_successful_apply();
        let after = get_settings_with_timeout(&state, Duration::ZERO);
        assert!(after.update_task_error.is_none());
        assert!(after.dto.update_automatic_check);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn unusable_startup_load_outcomes_keep_persisted_truth_and_skip_reconcile() {
        for outcome in [
            settings::LoadOutcome::PermissionDenied,
            settings::LoadOutcome::IoError,
            settings::LoadOutcome::NoPath,
            settings::LoadOutcome::CorruptQuarantineFailed,
        ] {
            assert!(!startup_reconcile_load_is_usable(outcome));
            let state = AutomaticCheckReconcileState::default();
            state.set_persisted_on_warning(startup_reconcile_load_warning(outcome));
            assert!(state.needs_repair());
            assert!(state.error().is_some());
            assert!(!state.forces_automatic_off());
        }
        for outcome in [
            settings::LoadOutcome::Loaded,
            settings::LoadOutcome::Missing,
            settings::LoadOutcome::Empty,
            settings::LoadOutcome::Corrupt,
        ] {
            assert!(startup_reconcile_load_is_usable(outcome));
        }
    }

    #[test]
    fn worker_early_failure_and_panic_complete_without_forcing_persisted_on_off() {
        let lock_error = AutomaticCheckReconcileState::default();
        run_reconcile_worker_with(&lock_error, || Err("settings lock failed".into()));
        assert_eq!(lock_error.completion(), ReconcileCompletion::Error);
        assert!(lock_error.error().is_some());
        assert!(!lock_error.forces_automatic_off());

        let panic = AutomaticCheckReconcileState::default();
        run_reconcile_worker_with(&panic, || -> Result<(), String> {
            panic!("simulated worker panic")
        });
        assert_eq!(panic.completion(), ReconcileCompletion::Error);
        assert!(panic.error().is_some());
        assert!(!panic.forces_automatic_off());
    }

    #[test]
    fn poisoned_completion_is_recovered_as_error_and_keeps_disk_on_truth() {
        let state = Arc::new(AutomaticCheckReconcileState::default());
        let poisoned = Arc::clone(&state);
        let join = thread::spawn(move || {
            let _guard = poisoned.completion.lock().unwrap();
            panic!("simulated completion mutex poison");
        });
        assert!(join.join().is_err());
        assert_eq!(
            state.wait_for_completion(Duration::ZERO),
            ReconcileCompletion::Error
        );
        assert_eq!(state.completion(), ReconcileCompletion::Error);
        assert!(!state.forces_automatic_off());

        let _lock = localappdata_test_lock();
        let base = std::env::temp_dir().join(format!(
            "nospacekey-config-reconcile-poison-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let _env = LocalAppDataGuard::set(&base);
        let path = settings::settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut persisted = settings::Settings::default();
        persisted.update.automatic_check = true;
        std::fs::write(&path, persisted.to_json()).unwrap();

        let result = get_settings_with_timeout(&state, Duration::ZERO);
        assert!(result.dto.update_automatic_check);
        assert!(result
            .update_task_error
            .expect("poison must have inline diagnostic")
            .contains("状態を確認できません"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn timeout_get_settings_keeps_persisted_on_truth_and_reports_inline_error() {
        let _lock = localappdata_test_lock();
        let base = std::env::temp_dir().join(format!(
            "nospacekey-config-reconcile-timeout-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let _env = LocalAppDataGuard::set(&base);
        let path = settings::settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut persisted = settings::Settings::default();
        persisted.update.automatic_check = true;
        persisted.update.automatic_check_prompt_dismissed = true;
        std::fs::write(&path, persisted.to_json()).unwrap();

        let state = AutomaticCheckReconcileState::default();
        let result = get_settings_with_timeout(&state, Duration::from_millis(1));
        assert!(result.dto.update_automatic_check);
        assert!(result
            .update_task_error
            .expect("timeout must be inline")
            .contains("制限時間"));
        assert!(!result.corrupt_recovered);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tip_first_pending_ledger_handoff_is_one_shot_and_missing_alone_is_silent() {
        let _lock = localappdata_test_lock();
        let base = std::env::temp_dir().join(format!(
            "nospacekey-config-corrupt-handoff-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let _env = LocalAppDataGuard::set(&base);
        let path = settings::settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        // TIP's production loader quarantines the original and leaves the
        // durable pending ledger entry for Config's process.
        std::fs::write(&path, "{ broken").unwrap();
        let (_, outcome) = settings::load_reporting();
        assert_eq!(outcome, settings::LoadOutcome::Corrupt);
        assert!(!path.exists());
        assert!(settings::has_pending_corrupt_recovery_notice());
        assert!(corrupt_recovered(
            settings::LoadOutcome::Missing,
            settings::has_pending_corrupt_recovery_notice()
        ));
        // A current quarantine failure does not discard a prior successful
        // pending notice; Config only acknowledges after the toast is in DOM.
        assert!(corrupt_recovered(
            settings::LoadOutcome::CorruptQuarantineFailed,
            settings::has_pending_corrupt_recovery_notice()
        ));
        settings::acknowledge_corrupt_recovery_notices();
        assert!(!settings::has_pending_corrupt_recovery_notice());
        assert!(!corrupt_recovered(
            settings::LoadOutcome::Missing,
            settings::has_pending_corrupt_recovery_notice()
        ));
        // A normal first run has no pending ledger entry and must not show the toast.
        assert!(!corrupt_recovered(
            settings::LoadOutcome::Missing,
            settings::has_pending_corrupt_recovery_notice()
        ));

        let _ = std::fs::remove_dir_all(&base);
    }

    struct StartupLease(Rc<RefCell<Vec<&'static str>>>);

    impl Drop for StartupLease {
        fn drop(&mut self) {
            self.0.borrow_mut().push("release");
        }
    }

    #[test]
    fn corrupt_recovery_notice_is_handed_off_once_after_reconcile() {
        let state = AutomaticCheckReconcileState::default();
        // Both JSON syntax corruption and a typed field corruption are reported
        // by the loader as a successful quarantine, then reconciled get_settings
        // sees Missing and consumes the in-process handoff exactly once.
        assert!(corrupt_recovered(settings::LoadOutcome::Corrupt, false));
        assert!(corrupt_recovered(settings::LoadOutcome::Corrupt, false));
        state.note_corrupt_recovered();
        assert!(corrupt_recovered(
            settings::LoadOutcome::Missing,
            state.take_corrupt_recovered_notice()
        ));
        assert!(!corrupt_recovered(
            settings::LoadOutcome::Missing,
            state.take_corrupt_recovered_notice()
        ));
        // A failed current quarantine must not erase a prior successful
        // cross-process handoff; the pending ledger still owns that notice.
        assert!(!corrupt_recovered(
            settings::LoadOutcome::CorruptQuarantineFailed,
            false
        ));
        assert!(corrupt_recovered(
            settings::LoadOutcome::CorruptQuarantineFailed,
            true
        ));
    }

    #[test]
    fn symbol_catalog_returns_29_entries_excluding_dash_comma_period() {
        let catalog = get_symbol_catalog();
        assert_eq!(catalog.len(), 29);
        assert!(catalog.iter().all(|e| !matches!(e.half, '-' | ',' | '.')));
    }

    #[test]
    fn external_url_allowlist_admits_only_attribution_links() {
        assert!(is_allowed_external_url(
            "https://huggingface.co/Miwa-Keita/zenz-v3.1-small-gguf"
        ));
        assert!(is_allowed_external_url(
            "https://creativecommons.org/licenses/by-sa/4.0/"
        ));
        // 近いが別物・任意 URL・UNC は弾く（前方一致ではなく完全一致）。
        assert!(!is_allowed_external_url(
            "https://huggingface.co/Miwa-Keita/zenz-v3.1-small-gguf/evil"
        ));
        assert!(!is_allowed_external_url("https://example.com"));
        assert!(!is_allowed_external_url(r"\\attacker\share"));
        assert!(!is_allowed_external_url(""));
    }

    #[test]
    fn releases_url_appends_latest_and_trims_dot_git() {
        assert_eq!(
            releases_url("https://github.com/o/r"),
            "https://github.com/o/r/releases/latest"
        );
        assert_eq!(
            releases_url("https://github.com/o/r.git"),
            "https://github.com/o/r/releases/latest"
        );
        assert_eq!(
            releases_url("https://github.com/o/r/"),
            "https://github.com/o/r/releases/latest"
        );
    }

    #[test]
    fn stop_engine_exit_code_maps_sent_and_gone() {
        // 不在（接続できず）＝止めるものが無い＝成功（gone は無意味）。
        assert_eq!(stop_engine_exit_code(false, false), 0);
        assert_eq!(stop_engine_exit_code(false, true), 0);
        // 送って pipe が消えた＝停止確認＝成功。
        assert_eq!(stop_engine_exit_code(true, true), 0);
        // 送ったが 3s 以内に pipe が消えない＝診断用の失敗（呼び出し元 .iss は taskkill へ進む）。
        assert_eq!(stop_engine_exit_code(true, false), 1);
    }

    #[test]
    fn engine_absence_lease_matches_host_name_and_excludes_a_second_lease() {
        let pipe = format!(
            r"\\.\pipe\nospacekey-test-clear.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        assert_eq!(
            engine_singleton_mutex_name(r"\\.\pipe\nospacekey-engine.s42"),
            r"Local\nospacekey-engine-singleton-__._pipe_nospacekey-engine.s42"
        );

        let first = EngineAbsenceLease::acquire(&pipe).expect("first lease");
        assert!(EngineAbsenceLease::acquire(&pipe).is_err());
        drop(first);
        assert!(EngineAbsenceLease::acquire(&pipe).is_ok());
    }

    fn learning_entry(name: &str, kind: LearningEntryKind) -> LearningEntry {
        LearningEntry {
            path: std::path::PathBuf::from(name),
            name: OsString::from(name),
            kind,
        }
    }

    #[test]
    fn stopped_learning_clear_seam_deletes_only_allowlisted_regular_files() {
        let scans = RefCell::new(vec![
            Ok(vec![
                learning_entry("memory.bin", LearningEntryKind::Regular),
                learning_entry(".pause", LearningEntryKind::Regular),
                learning_entry("corrections.json", LearningEntryKind::Regular),
                learning_entry("learningMemory.txt", LearningEntryKind::Regular),
                learning_entry("foreign.json", LearningEntryKind::Regular),
            ]),
            Ok(vec![learning_entry(
                "foreign.json",
                LearningEntryKind::Regular,
            )]),
        ]);
        let removed = RefCell::new(Vec::new());
        let result = clear_learning_files_with(
            std::path::Path::new("memory"),
            |_| scans.borrow_mut().remove(0),
            |path| {
                removed.borrow_mut().push(path.to_path_buf());
                Ok(())
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(
            &*removed.borrow(),
            &[
                std::path::PathBuf::from("memory.bin"),
                std::path::PathBuf::from(".pause"),
                std::path::PathBuf::from("corrections.json"),
                std::path::PathBuf::from("learningMemory.txt"),
            ]
        );
    }

    #[test]
    fn stopped_learning_clear_seam_rejects_reparse_and_delete_failures() {
        let reparse = clear_learning_files_with(
            std::path::Path::new("memory"),
            |_| {
                Ok(vec![learning_entry(
                    "memory.bin",
                    LearningEntryKind::Reparse,
                )])
            },
            |_| panic!("reparse target must never be passed to remove_file"),
        );
        assert!(reparse
            .expect_err("reparse target must fail")
            .contains("reparse"));

        let delete_error = clear_learning_files_with(
            std::path::Path::new("memory"),
            |_| {
                Ok(vec![learning_entry(
                    "memory.bin",
                    LearningEntryKind::Regular,
                )])
            },
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied",
                ))
            },
        );
        assert!(delete_error
            .expect_err("delete error must propagate")
            .contains("denied"));
    }

    #[test]
    fn stopped_learning_clear_seam_rejects_non_directory_scan_and_verification_residue() {
        let non_directory = clear_learning_files_with(
            std::path::Path::new("memory"),
            |_| Err(LearningScanError::Failed("reparse directory".into())),
            |_| panic!("scan failure must not delete"),
        );
        assert!(non_directory
            .expect_err("directory safety failure must propagate")
            .contains("reparse directory"));

        let residue = clear_learning_files_with(
            std::path::Path::new("memory"),
            {
                let calls = Cell::new(0);
                move |_| {
                    calls.set(calls.get() + 1);
                    Ok(vec![learning_entry(
                        "memory.bin",
                        LearningEntryKind::Regular,
                    )])
                }
            },
            |_| Ok(()),
        );
        assert!(residue
            .expect_err("verification residue must fail")
            .contains("残っています"));

        let legacy_residue = clear_learning_files_with(
            std::path::Path::new("memory"),
            {
                let calls = Cell::new(0);
                move |_| {
                    calls.set(calls.get() + 1);
                    Ok(vec![learning_entry(
                        "learningMemory.txt",
                        LearningEntryKind::Regular,
                    )])
                }
            },
            |_| Ok(()),
        );
        assert!(legacy_residue
            .expect_err("legacy verification residue must fail")
            .contains("残っています"));
    }

    #[test]
    fn stopped_learning_clear_seam_treats_missing_directory_and_file_as_success() {
        let missing_dir = clear_learning_files_with(
            std::path::Path::new("memory"),
            |_| Err(LearningScanError::NotFound),
            |_| panic!("missing directory must not delete"),
        );
        assert_eq!(missing_dir, Ok(()));

        let scans = RefCell::new(vec![
            Ok(vec![learning_entry(
                "memory.bin",
                LearningEntryKind::Regular,
            )]),
            Ok(Vec::new()),
        ]);
        let removed = Cell::new(false);
        let missing_file = clear_learning_files_with(
            std::path::Path::new("memory"),
            |_| scans.borrow_mut().remove(0),
            |_| {
                removed.set(true);
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"))
            },
        );
        assert_eq!(missing_file, Ok(()));
        assert!(removed.get());
    }

    #[test]
    fn learning_coordination_names_match_swift_engine() {
        let scope =
            learning_coordination_scope(r"C:\Users\Example\AppData\Local\nospacekey\memory");
        assert_eq!(scope, "540cb8761d8b7d7f");
        assert_eq!(
            learning_lifecycle_mutex_name(&scope),
            r"Global\nospacekey-learning-lifecycle-540cb8761d8b7d7f"
        );
        assert_eq!(
            learning_presence_mutex_name(&scope, 42),
            r"Global\nospacekey-learning-presence-540cb8761d8b7d7f-s42"
        );
    }

    #[test]
    fn same_user_session_detection_is_case_insensitive_and_ignores_empty_sessions() {
        let current = WindowsSessionUser {
            domain: "WORKSTATION".into(),
            user: "Example".into(),
        };
        let other_case = WindowsSessionUser {
            domain: "workstation".into(),
            user: "example".into(),
        };
        let different = WindowsSessionUser {
            domain: "WORKSTATION".into(),
            user: "Other".into(),
        };

        assert!(!has_other_same_user_session(
            7,
            &current,
            &[(0, None), (7, Some(current.clone())), (8, Some(different))],
        ));
        assert!(has_other_same_user_session(
            7,
            &current,
            &[(7, Some(current.clone())), (8, Some(other_case))],
        ));
    }

    #[test]
    fn automatic_task_repair_retries_registration_after_a_failed_reconcile_save() {
        assert!(should_register_task(false, true, false));
        assert!(should_register_task(true, true, true));
        assert!(!should_register_task(true, true, false));
        assert!(!should_register_task(true, false, true));
        assert!(run_now_succeeded(&Ok(())));
        assert!(!run_now_succeeded(&Err("run failed".into())));
    }

    #[test]
    fn startup_off_lock_timeout_does_not_delete_or_clear() {
        let reconcile = AutomaticCheckReconcileState::default();
        let settings = settings::Settings::default();
        let deletes = Cell::new(0);

        reconcile_automatic_check_task_with_lease(
            &reconcile,
            settings,
            || panic!("startup OFF timeout must not inspect the task"),
            || panic!("startup OFF timeout must not register a task"),
            |_| panic!("startup OFF timeout must not run a task"),
            |_| {
                deletes.set(deletes.get() + 1);
                Ok(())
            },
            |_| panic!("startup OFF timeout must not save settings"),
            || Ok::<Option<()>, String>(None),
        );

        assert_eq!(deletes.get(), 0);
        assert!(reconcile.error().is_some());
        assert!(reconcile.needs_repair());
        assert!(reconcile.forces_automatic_off());
    }

    #[test]
    fn startup_off_lock_error_does_not_delete_or_save() {
        let reconcile = AutomaticCheckReconcileState::default();
        let settings = settings::Settings::default();
        let deletes = Cell::new(0);
        let saves = Cell::new(0);

        reconcile_automatic_check_task_with_lease(
            &reconcile,
            settings,
            || panic!("startup OFF lock error must not inspect the task"),
            || panic!("startup OFF lock error must not register a task"),
            |_| panic!("startup OFF lock error must not run a task"),
            |_| {
                deletes.set(deletes.get() + 1);
                Ok(())
            },
            |_| {
                saves.set(saves.get() + 1);
                Ok(())
            },
            || Err::<Option<()>, _>("state lock I/O error".into()),
        );

        assert_eq!(deletes.get(), 0);
        assert_eq!(saves.get(), 0);
        assert!(reconcile
            .error()
            .expect("lock error is surfaced")
            .contains("state lock I/O error"));
        assert!(reconcile.forces_automatic_off());
    }

    #[test]
    fn startup_off_delete_holds_lease_until_delete_and_reports_success() {
        let reconcile = AutomaticCheckReconcileState::default();
        let settings = settings::Settings::default();
        let events = Rc::new(RefCell::new(Vec::new()));
        let lease_events = events.clone();

        reconcile_automatic_check_task_with_lease(
            &reconcile,
            settings,
            {
                let events = events.clone();
                move || {
                    events.borrow_mut().push("identity");
                    Ok(nospacekey_update::scheduler::task_identity("S-1-5-21"))
                }
            },
            || panic!("startup OFF must not register a task"),
            |_| panic!("startup OFF must not run a task"),
            {
                let events = events.clone();
                move |_| {
                    events.borrow_mut().push("delete");
                    Ok(())
                }
            },
            |_| panic!("startup OFF must not save settings"),
            move || {
                lease_events.borrow_mut().push("acquire");
                Ok(Some(StartupLease(lease_events.clone())))
            },
        );

        assert_eq!(
            &*events.borrow(),
            &["acquire", "identity", "delete", "release"]
        );
        assert!(reconcile.error().is_none());
        assert!(!reconcile.needs_repair());
    }

    #[test]
    fn startup_run_failure_saves_off_then_deletes_before_releasing_lease() {
        let reconcile = AutomaticCheckReconcileState::default();
        let mut settings = settings::Settings::default();
        settings.update.automatic_check = true;
        settings.update.automatic_check_prompt_dismissed = true;
        let events = Rc::new(RefCell::new(Vec::new()));
        let lease_events = events.clone();
        let saved = Rc::new(RefCell::new(Vec::new()));
        let save_results = saved.clone();
        let identity = nospacekey_update::scheduler::task_identity("S-1-5-21");

        reconcile_automatic_check_task_with_lease(
            &reconcile,
            settings,
            {
                let events = events.clone();
                let identity = identity.clone();
                move || {
                    events.borrow_mut().push("identity");
                    Ok(identity.clone())
                }
            },
            {
                let events = events.clone();
                let identity = identity.clone();
                move || {
                    events.borrow_mut().push("register");
                    Ok(identity.clone())
                }
            },
            {
                let events = events.clone();
                move |_| {
                    events.borrow_mut().push("run");
                    Err("run failed".into())
                }
            },
            {
                let events = events.clone();
                move |_| {
                    events.borrow_mut().push("delete");
                    Ok(())
                }
            },
            {
                let events = events.clone();
                move |settings| {
                    events.borrow_mut().push("save");
                    save_results.borrow_mut().push(settings.clone());
                    Ok(())
                }
            },
            move || {
                lease_events.borrow_mut().push("acquire");
                Ok(Some(StartupLease(lease_events.clone())))
            },
        );

        assert_eq!(
            &*events.borrow(),
            &["identity", "register", "run", "acquire", "save", "delete", "release"]
        );
        let saved = saved.borrow();
        assert_eq!(saved.len(), 1);
        assert!(!saved[0].update.automatic_check);
        assert!(saved[0].update.automatic_check_prompt_dismissed);
        assert!(reconcile
            .error()
            .expect("run failure is surfaced")
            .contains("run failed"));
    }

    #[test]
    fn startup_run_failure_lock_timeout_does_not_delete_or_save() {
        let reconcile = AutomaticCheckReconcileState::default();
        let mut settings = settings::Settings::default();
        settings.update.automatic_check = true;
        let deletes = Cell::new(0);
        let saves = Cell::new(0);
        let identity = nospacekey_update::scheduler::task_identity("S-1-5-21");

        reconcile_automatic_check_task_with_lease(
            &reconcile,
            settings,
            {
                let identity = identity.clone();
                move || Ok(identity.clone())
            },
            {
                let identity = identity.clone();
                move || Ok(identity.clone())
            },
            |_| Err("run failed".into()),
            |_| {
                deletes.set(deletes.get() + 1);
                Ok(())
            },
            |_| {
                saves.set(saves.get() + 1);
                Ok(())
            },
            || Ok::<Option<()>, String>(None),
        );

        assert_eq!(deletes.get(), 0);
        assert_eq!(saves.get(), 0);
        assert!(reconcile.error().is_some());
        assert!(!reconcile.forces_automatic_off());
    }

    #[test]
    fn startup_register_failure_retries_after_initial_lock_timeout() {
        let reconcile = AutomaticCheckReconcileState::default();
        let mut settings = settings::Settings::default();
        settings.update.automatic_check = true;
        let deletes = Cell::new(0);
        let saves = Cell::new(0);
        let identity = nospacekey_update::scheduler::task_identity("S-1-5-21");

        reconcile_automatic_check_task_with_lease(
            &reconcile,
            settings,
            {
                let identity = identity.clone();
                move || Ok(identity.clone())
            },
            || Err("register failed".into()),
            |_| panic!("register failure must not run a task"),
            |_| {
                deletes.set(deletes.get() + 1);
                Ok(())
            },
            |_| {
                saves.set(saves.get() + 1);
                Ok(())
            },
            {
                let attempts = Cell::new(0);
                move || {
                    attempts.set(attempts.get() + 1);
                    if attempts.get() == 1 {
                        Ok(None)
                    } else {
                        Ok(Some(()))
                    }
                }
            },
        );

        assert_eq!(deletes.get(), 1);
        assert_eq!(saves.get(), 1);
        assert!(reconcile.error().is_some());
        assert!(reconcile.forces_automatic_off());
    }

    #[test]
    fn startup_register_failure_save_failure_keeps_task_and_persisted_on() {
        let reconcile = AutomaticCheckReconcileState::default();
        let mut settings = settings::Settings::default();
        settings.update.automatic_check = true;
        let deletes = Cell::new(0);
        let saves = Cell::new(0);
        let identity = nospacekey_update::scheduler::task_identity("S-1-5-21");

        reconcile_automatic_check_task_with_lease(
            &reconcile,
            settings,
            {
                let identity = identity.clone();
                move || Ok(identity.clone())
            },
            || Err("register failed".into()),
            |_| panic!("register failure must not run a task"),
            |_| {
                deletes.set(deletes.get() + 1);
                Ok(())
            },
            |_| {
                saves.set(saves.get() + 1);
                Err("disk full".into())
            },
            || Ok::<Option<()>, String>(Some(())),
        );

        assert_eq!(saves.get(), 1);
        assert_eq!(deletes.get(), 0);
        assert!(reconcile
            .error()
            .expect("save failure is surfaced")
            .contains("disk full"));
        assert!(reconcile.needs_repair());
        assert!(!reconcile.forces_automatic_off());
    }

    #[test]
    fn startup_sid_failure_lock_error_does_not_delete_or_save() {
        let reconcile = AutomaticCheckReconcileState::default();
        let mut settings = settings::Settings::default();
        settings.update.automatic_check = true;
        let deletes = Cell::new(0);
        let saves = Cell::new(0);

        reconcile_automatic_check_task_with_lease(
            &reconcile,
            settings,
            || Err::<nospacekey_update::scheduler::TaskIdentity, _>("SID failed".into()),
            || panic!("SID failure must not register a task"),
            |_| panic!("SID failure must not run a task"),
            |_| {
                deletes.set(deletes.get() + 1);
                Ok(())
            },
            |_| {
                saves.set(saves.get() + 1);
                Ok(())
            },
            || Err::<Option<()>, _>("state lock I/O error".into()),
        );

        assert_eq!(deletes.get(), 0);
        assert_eq!(saves.get(), 0);
        assert!(reconcile
            .error()
            .expect("lock error is surfaced")
            .contains("state lock I/O error"));
        assert!(!reconcile.forces_automatic_off());
    }

    #[test]
    fn startup_register_failure_deletes_and_saves_off_before_releasing_lease() {
        let reconcile = AutomaticCheckReconcileState::default();
        let mut settings = settings::Settings::default();
        settings.update.automatic_check = true;
        let events = Rc::new(RefCell::new(Vec::new()));
        let lease_events = events.clone();
        let saved = Rc::new(Cell::new(0));
        let save_count = saved.clone();
        let identity = nospacekey_update::scheduler::task_identity("S-1-5-21");

        reconcile_automatic_check_task_with_lease(
            &reconcile,
            settings,
            {
                let events = events.clone();
                let identity = identity.clone();
                move || {
                    events.borrow_mut().push("identity");
                    Ok(identity.clone())
                }
            },
            {
                let events = events.clone();
                move || {
                    events.borrow_mut().push("register");
                    Err("register failed".into())
                }
            },
            |_| panic!("startup register failure must not run a task"),
            {
                let events = events.clone();
                move |_| {
                    events.borrow_mut().push("delete");
                    Ok(())
                }
            },
            {
                let events = events.clone();
                move |settings| {
                    events.borrow_mut().push("save");
                    assert!(!settings.update.automatic_check);
                    save_count.set(save_count.get() + 1);
                    Ok(())
                }
            },
            move || {
                lease_events.borrow_mut().push("acquire");
                Ok(Some(StartupLease(lease_events.clone())))
            },
        );

        assert_eq!(
            &*events.borrow(),
            &["identity", "register", "acquire", "save", "delete", "release"]
        );
        assert_eq!(saved.get(), 1);
        assert!(reconcile
            .error()
            .expect("register failure is surfaced")
            .contains("register failed"));
    }

    #[test]
    fn startup_sid_failure_save_failure_keeps_persisted_on_truthful() {
        let reconcile = AutomaticCheckReconcileState::default();
        let mut settings = settings::Settings::default();
        settings.update.automatic_check = true;
        let saves = Cell::new(0);
        let deletes = Cell::new(0);

        reconcile_automatic_check_task_with_lease(
            &reconcile,
            settings,
            || Err::<nospacekey_update::scheduler::TaskIdentity, _>("SID failed".into()),
            || panic!("SID failure must not register a task"),
            |_| panic!("SID failure must not run a task"),
            |_| {
                deletes.set(deletes.get() + 1);
                Ok(())
            },
            |_| {
                saves.set(saves.get() + 1);
                Err("disk full".into())
            },
            || Ok::<Option<()>, String>(Some(())),
        );

        assert_eq!(saves.get(), 1);
        assert_eq!(deletes.get(), 0);
        assert!(reconcile
            .error()
            .expect("SID/save failure is surfaced")
            .contains("disk full"));
        assert!(reconcile.needs_repair());
        assert!(!reconcile.forces_automatic_off());
    }

    #[test]
    fn startup_success_keeps_lease_until_run_and_clears_reconcile_error() {
        let reconcile = AutomaticCheckReconcileState::default();
        let mut settings = settings::Settings::default();
        settings.update.automatic_check = true;
        let events = Rc::new(RefCell::new(Vec::new()));
        let lease_events = events.clone();
        let saves = Cell::new(0);
        let deletes = Cell::new(0);
        let identity = nospacekey_update::scheduler::task_identity("S-1-5-21");

        reconcile_automatic_check_task_with_lease(
            &reconcile,
            settings,
            {
                let events = events.clone();
                let identity = identity.clone();
                move || {
                    events.borrow_mut().push("identity");
                    Ok(identity.clone())
                }
            },
            {
                let events = events.clone();
                let identity = identity.clone();
                move || {
                    events.borrow_mut().push("register");
                    Ok(identity.clone())
                }
            },
            {
                let events = events.clone();
                move |_| {
                    events.borrow_mut().push("run");
                    Ok(())
                }
            },
            |_| {
                deletes.set(deletes.get() + 1);
                Ok(())
            },
            |_| {
                saves.set(saves.get() + 1);
                Ok(())
            },
            move || {
                lease_events.borrow_mut().push("acquire");
                Ok(Some(StartupLease(lease_events.clone())))
            },
        );

        assert_eq!(&*events.borrow(), &["identity", "register", "run"]);
        assert_eq!(saves.get(), 0);
        assert_eq!(deletes.get(), 0);
        assert!(reconcile.error().is_none());
    }

    #[test]
    fn run_failure_off_save_uses_the_already_loaded_settings() {
        let mut existing = settings::Settings::default();
        existing.update.automatic_check = true;
        existing.llm.api_key_dpapi = "preserve-me".into();
        let saved = std::cell::RefCell::new(None);

        let message = persist_reconcile_off(existing, "task failed".into(), |settings| {
            saved.replace(Some(settings.clone()));
            Ok(())
        });

        let saved = saved.into_inner().expect("save seam called");
        assert_eq!(message, "task failed");
        assert!(!saved.update.automatic_check);
        assert!(saved.update.automatic_check_prompt_dismissed);
        assert_eq!(saved.llm.api_key_dpapi, "preserve-me");
    }

    #[test]
    fn post_save_reconcile_reports_whether_off_was_persisted() {
        let settings = settings::Settings::default();
        let (_, saved) =
            persist_reconcile_off_with_status(settings.clone(), "task failed".into(), |_| Ok(()));
        assert!(saved);

        let (_, saved) = persist_reconcile_off_with_status(settings, "task failed".into(), |_| {
            Err("disk full".into())
        });
        assert!(!saved);
    }

    #[test]
    fn off_apply_retries_stale_task_and_clears_repair_only_after_delete_success() {
        let reconcile = AutomaticCheckReconcileState::default();
        reconcile.set_persisted_on_warning("startup stale task".into());
        let prev = settings::Settings::default();
        let mut next = prev.clone();
        next.update.include_beta = true;
        let events = Rc::new(RefCell::new(Vec::new()));
        let lease_events = events.clone();
        let result = apply_automatic_check_transaction_with_lease(
            &reconcile,
            &prev,
            &next,
            || panic!("OFF repair must not register"),
            |_| panic!("OFF repair must not run"),
            {
                let events = events.clone();
                move |_| {
                    events.borrow_mut().push("delete");
                    Ok(())
                }
            },
            {
                let events = events.clone();
                move |_| {
                    events.borrow_mut().push("save");
                    Ok(())
                }
            },
            move || {
                lease_events.borrow_mut().push("lease");
                Ok(Some(StartupLease(lease_events.clone())))
            },
        )
        .expect("unrelated OFF apply should save after acquiring repair lease");

        assert!(result.is_none());
        assert_eq!(&*events.borrow(), &["lease", "save", "delete", "release"]);
        assert!(!reconcile.needs_repair());
        assert!(reconcile.error().is_none());
    }

    #[test]
    fn off_apply_keeps_repair_warning_when_stale_task_delete_still_fails() {
        let reconcile = AutomaticCheckReconcileState::default();
        reconcile.set_persisted_on_warning("startup stale task".into());
        let prev = settings::Settings::default();
        let next = prev.clone();
        let deletes = Cell::new(0);
        let result = apply_automatic_check_transaction_with_lease(
            &reconcile,
            &prev,
            &next,
            || panic!("OFF repair must not register"),
            |_| panic!("OFF repair must not run"),
            |_| {
                deletes.set(deletes.get() + 1);
                Err("task still exists".into())
            },
            |_| Ok(()),
            || Ok::<Option<()>, String>(Some(())),
        )
        .expect("OFF remains persisted even when repair delete fails");

        assert!(result
            .expect("delete warning")
            .contains("task still exists"));
        assert_eq!(deletes.get(), 1);
        assert!(reconcile.needs_repair());
        assert!(reconcile
            .error()
            .expect("repair warning is retained")
            .contains("task still exists"));
        assert!(!reconcile.forces_automatic_off());
    }

    #[test]
    fn automatic_register_failure_does_not_save_settings() {
        let reconcile = AutomaticCheckReconcileState::default();
        let prev = settings::Settings::default();
        let mut next = prev.clone();
        next.update.automatic_check = true;
        next.update.automatic_check_prompt_dismissed = true;
        let saves = Cell::new(0);

        let result = apply_automatic_check_transaction(
            &reconcile,
            &prev,
            &next,
            || Err("register failed".into()),
            |_| Ok(()),
            |_| Ok(()),
            |_| {
                saves.set(saves.get() + 1);
                Ok(())
            },
        );

        let errors = result.expect_err("registration failure must reject the apply");
        assert_eq!(errors[0].field, "update_automatic_check");
        assert_eq!(saves.get(), 0);
    }

    #[test]
    fn automatic_off_lock_timeout_does_not_save_or_delete() {
        let reconcile = AutomaticCheckReconcileState::default();
        let mut prev = settings::Settings::default();
        prev.update.automatic_check = true;
        let mut next = prev.clone();
        next.update.automatic_check = false;
        let saves = Cell::new(0);
        let deletes = Cell::new(0);

        let result = apply_automatic_check_transaction_with_lease(
            &reconcile,
            &prev,
            &next,
            || panic!("OFF timeout must not register a task"),
            |_| panic!("OFF timeout must not run a task"),
            |_| {
                deletes.set(deletes.get() + 1);
                Ok(())
            },
            |_| {
                saves.set(saves.get() + 1);
                Ok(())
            },
            || Ok::<Option<()>, String>(None),
        );

        let errors = result.expect_err("lease timeout must reject OFF");
        assert_eq!(errors[0].field, "update_automatic_check");
        assert_eq!(saves.get(), 0);
        assert_eq!(deletes.get(), 0);
    }

    #[test]
    fn run_failure_saves_on_then_off_with_prompt_and_returns_warning() {
        let reconcile = AutomaticCheckReconcileState::default();
        let prev = settings::Settings::default();
        let mut next = prev.clone();
        next.update.automatic_check = true;
        next.update.automatic_check_prompt_dismissed = true;
        let saved = RefCell::new(Vec::new());
        let deleted = Cell::new(0);

        let result = apply_automatic_check_transaction(
            &reconcile,
            &prev,
            &next,
            || Ok(nospacekey_update::scheduler::TaskIdentity { sid: "test".into() }),
            |_| Err("run failed".into()),
            |_| {
                deleted.set(deleted.get() + 1);
                Ok(())
            },
            |settings| {
                saved.borrow_mut().push(settings.clone());
                Ok(())
            },
        )
        .expect("OFF fallback save makes the transaction successful");

        assert!(result.expect("run failure warning").contains("run failed"));
        assert_eq!(deleted.get(), 1);
        let saved = saved.into_inner();
        assert_eq!(saved.len(), 2);
        assert!(saved[0].update.automatic_check);
        assert!(saved[0].update.automatic_check_prompt_dismissed);
        assert!(!saved[1].update.automatic_check);
        assert!(saved[1].update.automatic_check_prompt_dismissed);
    }

    #[test]
    fn apply_run_failure_holds_state_lease_through_delete_and_off_save() {
        let reconcile = AutomaticCheckReconcileState::default();
        let prev = settings::Settings::default();
        let mut next = prev.clone();
        next.update.automatic_check = true;
        let events = Rc::new(RefCell::new(Vec::new()));
        let identity = nospacekey_update::scheduler::task_identity("S-1-5-21");
        let lease_events = events.clone();
        let saved = Rc::new(RefCell::new(Vec::new()));
        let save_on_events = events.clone();
        let save_off_events = events.clone();
        let result = apply_automatic_check_transaction_with_lease(
            &reconcile,
            &prev,
            &next,
            {
                let identity = identity.clone();
                move || Ok(identity.clone())
            },
            |_| Err("run failed".into()),
            {
                let events = events.clone();
                move |_| {
                    events.borrow_mut().push("delete");
                    Ok(())
                }
            },
            {
                let saved = saved.clone();
                move |settings| {
                    if settings.update.automatic_check {
                        save_on_events.borrow_mut().push("save-on");
                    } else {
                        save_off_events.borrow_mut().push("save-off");
                    }
                    saved.borrow_mut().push(settings.clone());
                    Ok(())
                }
            },
            move || {
                lease_events.borrow_mut().push("acquire");
                Ok(Some(StartupLease(lease_events.clone())))
            },
        )
        .expect("run failure rollback is an acknowledged warning");

        assert!(result.expect("run failure warning").contains("run failed"));
        assert_eq!(
            &*events.borrow(),
            &["save-on", "acquire", "delete", "save-off", "release"]
        );
        assert_eq!(saved.borrow().len(), 2);
        assert!(!saved.borrow()[1].update.automatic_check);
    }

    #[test]
    fn apply_run_failure_without_state_lease_keeps_on_and_skips_rollback_side_effects() {
        for acquire_result in [
            Ok::<Option<()>, String>(None),
            Err::<Option<()>, _>("state lock error".into()),
        ] {
            let reconcile = AutomaticCheckReconcileState::default();
            let prev = settings::Settings::default();
            let mut next = prev.clone();
            next.update.automatic_check = true;
            let deletes = Cell::new(0);
            let saved = RefCell::new(Vec::new());
            let result = apply_automatic_check_transaction_with_lease(
                &reconcile,
                &prev,
                &next,
                || Ok(nospacekey_update::scheduler::task_identity("S-1-5-21")),
                |_| Err("run failed".into()),
                |_| {
                    deletes.set(deletes.get() + 1);
                    Ok(())
                },
                |settings| {
                    saved.borrow_mut().push(settings.clone());
                    Ok(())
                },
                {
                    let mut acquire_result = Some(acquire_result);
                    move || acquire_result.take().unwrap()
                },
            )
            .expect("lock failure remains an inline warning");

            let warning = result.expect("run failure warning");
            assert!(warning.contains("ON のまま"));
            assert_eq!(deletes.get(), 0);
            assert_eq!(saved.borrow().len(), 1);
            assert!(saved.borrow()[0].update.automatic_check);
            assert!(reconcile.error().is_some());
            assert!(!reconcile.forces_automatic_off());
        }
    }

    #[test]
    fn run_failure_off_fallback_save_failure_keeps_persisted_on_after_delete_success() {
        let reconcile = AutomaticCheckReconcileState::default();
        let prev = settings::Settings::default();
        let mut next = prev.clone();
        next.update.automatic_check = true;
        next.update.automatic_check_prompt_dismissed = true;
        let saved = RefCell::new(Vec::new());

        let result = apply_automatic_check_transaction(
            &reconcile,
            &prev,
            &next,
            || Ok(nospacekey_update::scheduler::TaskIdentity { sid: "test".into() }),
            |_| Err("run failed".into()),
            |_| Ok(()),
            |settings| {
                let mut saved = saved.borrow_mut();
                saved.push(settings.clone());
                if saved.len() == 1 {
                    Ok(())
                } else {
                    Err("disk full".into())
                }
            },
        )
        .expect("fallback failure is an acknowledged ON result");

        assert!(result.expect("warning").contains("ON のまま"));
        assert!(saved.borrow()[0].update.automatic_check);
    }

    #[test]
    fn run_failure_off_fallback_save_failure_keeps_persisted_on_after_delete_failure() {
        let reconcile = AutomaticCheckReconcileState::default();
        let prev = settings::Settings::default();
        let mut next = prev.clone();
        next.update.automatic_check = true;
        next.update.automatic_check_prompt_dismissed = true;
        let saved = RefCell::new(Vec::new());

        let result = apply_automatic_check_transaction(
            &reconcile,
            &prev,
            &next,
            || Ok(nospacekey_update::scheduler::TaskIdentity { sid: "test".into() }),
            |_| Err("run failed".into()),
            |_| Err("delete failed".into()),
            |settings| {
                let mut saved = saved.borrow_mut();
                saved.push(settings.clone());
                if saved.len() == 1 {
                    Ok(())
                } else {
                    Err("disk full".into())
                }
            },
        )
        .expect("fallback failure is an acknowledged ON result");

        assert!(result.expect("warning").contains("ON のまま"));
        assert!(saved.borrow()[0].update.automatic_check);
    }

    #[test]
    fn persisted_on_warning_does_not_force_get_settings_off() {
        let reconcile = AutomaticCheckReconcileState::default();
        reconcile.set_persisted_on_warning("task failed; setting remains ON".into());
        assert!(!reconcile.forces_automatic_off());
        assert!(reconcile.needs_repair());
    }

    #[test]
    fn off_save_precedes_delete_and_delete_failure_is_a_warning() {
        let reconcile = AutomaticCheckReconcileState::default();
        let mut prev = settings::Settings::default();
        prev.update.automatic_check = true;
        let mut next = prev.clone();
        next.update.automatic_check = false;
        next.update.automatic_check_prompt_dismissed = true;
        let saved = RefCell::new(Vec::new());

        let warning = apply_automatic_check_transaction(
            &reconcile,
            &prev,
            &next,
            || panic!("OFF transition must not register a task"),
            |_| panic!("OFF transition must not run a task"),
            |identity| {
                assert!(identity.is_none());
                Err("delete failed".into())
            },
            |settings| {
                saved.borrow_mut().push(settings.clone());
                Ok(())
            },
        )
        .expect("delete failure does not reject a persisted OFF setting");

        assert!(warning.expect("delete warning").contains("delete failed"));
        let saved = saved.into_inner();
        assert_eq!(saved.len(), 1);
        assert!(!saved[0].update.automatic_check);
        assert!(saved[0].update.automatic_check_prompt_dismissed);
    }
}
