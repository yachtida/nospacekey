//! Process-local source of truth for long-running model operations.
//!
//! Downloads cannot survive the Config process, so persistence would add a false recovery
//! promise. Keeping the latest terminal record per model is enough for navigation, missed
//! progress events, cancellation, and orderly window close within this process lifetime.

use serde::Serialize;
use std::sync::{LazyLock, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelKind {
    Zenzai,
    Prediction,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Downloading,
    Verifying,
    PlacementWaiting,
    Placing,
    Activating,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOperationStatus {
    pub operation_id: u64,
    pub model_kind: ModelKind,
    pub phase: OperationPhase,
    pub progress: Option<u8>,
    pub cancelable: bool,
    pub activation_intent: bool,
    pub result: Option<String>,
}

static OPERATIONS: LazyLock<Mutex<Vec<ModelOperationStatus>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

fn with_record(kind: ModelKind, operation_id: u64, update: impl FnOnce(&mut ModelOperationStatus)) {
    if let Ok(mut operations) = OPERATIONS.lock() {
        if let Some(operation) = operations
            .iter_mut()
            .find(|operation| operation.model_kind == kind)
        {
            if operation.operation_id == operation_id {
                update(operation);
            }
        }
    }
}

pub struct OperationGuard {
    kind: ModelKind,
    operation_id: u64,
    finished: bool,
}

impl OperationGuard {
    pub fn begin(kind: ModelKind, operation_id: u64, activation_intent: bool) -> Self {
        if let Ok(mut operations) = OPERATIONS.lock() {
            operations.retain(|operation| operation.model_kind != kind);
            operations.push(ModelOperationStatus {
                operation_id,
                model_kind: kind,
                phase: OperationPhase::Downloading,
                progress: Some(0),
                cancelable: true,
                activation_intent,
                result: None,
            });
        }
        Self {
            kind,
            operation_id,
            finished: false,
        }
    }

    pub fn progress(&self, progress: Option<u8>) {
        with_record(self.kind, self.operation_id, |operation| {
            operation.progress = progress;
        });
    }

    pub fn phase(&self, phase: OperationPhase, cancelable: bool) {
        with_record(self.kind, self.operation_id, |operation| {
            operation.phase = phase;
            operation.cancelable = cancelable;
        });
    }

    pub fn succeed(mut self, result: String) {
        with_record(self.kind, self.operation_id, |operation| {
            operation.phase = OperationPhase::Succeeded;
            operation.progress = Some(100);
            operation.cancelable = false;
            operation.result = Some(result);
        });
        self.finished = true;
    }

    pub fn fail(mut self, result: String) {
        with_record(self.kind, self.operation_id, |operation| {
            let cancelled =
                operation.phase == OperationPhase::Cancelling || result.contains("キャンセル");
            operation.phase = if cancelled {
                OperationPhase::Cancelled
            } else {
                OperationPhase::Failed
            };
            operation.cancelable = false;
            operation.result = Some(result);
        });
        self.finished = true;
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        with_record(self.kind, self.operation_id, |operation| {
            let cancelled = operation.phase == OperationPhase::Cancelling;
            operation.phase = if cancelled {
                OperationPhase::Cancelled
            } else {
                OperationPhase::Failed
            };
            operation.cancelable = false;
            operation.result = Some(
                if cancelled {
                    "キャンセルしました。"
                } else {
                    "処理を完了できませんでした。詳細は画面のエラーを確認してください。"
                }
                .into(),
            );
        });
    }
}

pub fn request_cancel(kind: ModelKind, operation_id: u64) {
    with_record(kind, operation_id, |operation| {
        if operation.cancelable {
            operation.phase = OperationPhase::Cancelling;
            operation.cancelable = false;
        }
    });
}

pub fn progress(kind: ModelKind, operation_id: u64, value: Option<u8>) {
    with_record(kind, operation_id, |operation| operation.progress = value);
}

pub fn statuses() -> Vec<ModelOperationStatus> {
    OPERATIONS
        .lock()
        .map(|operations| operations.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_updates_cannot_mutate_the_latest_attempt() {
        let old = OperationGuard::begin(ModelKind::Zenzai, 10, true);
        let current = OperationGuard::begin(ModelKind::Zenzai, 11, false);
        old.progress(Some(90));
        let status = statuses()
            .into_iter()
            .find(|status| status.model_kind == ModelKind::Zenzai)
            .unwrap();
        assert_eq!(status.operation_id, 11);
        assert_eq!(status.progress, Some(0));
        current.succeed("done".into());
    }
}
