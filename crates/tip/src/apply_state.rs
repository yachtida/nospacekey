//! C6: one display operation survives request rejection and decoration repair.
use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ApplyIdentity {
    pub context_token: u64,
    pub composition_token: u64,
    pub display_revision: u64,
    pub operation_id: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ApplyOutcome {
    #[default]
    NotApplied,
    TextAppliedRepairPending,
    Applied,
    Deferred,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ApplyReport {
    pub outcome: ApplyOutcome,
    pub text_applied: bool,
    pub stale: bool,
}

impl ApplyReport {
    // Legacy state/anchor callers acknowledge the body, not decoration repair.
    pub fn body_applied(self) -> bool {
        self.text_applied && !self.stale
    }
    pub fn fully_applied(self) -> bool {
        self.outcome == ApplyOutcome::Applied && !self.stale
    }
}

#[derive(Default)]
pub(crate) struct ApplyState {
    outcome: Cell<ApplyOutcome>,
    executed: Cell<bool>,
    text_applied: Cell<bool>,
    text_consumed: Cell<bool>,
    stale: Cell<bool>,
    pub request_hresult: Cell<Option<i32>>,
    pub session_hresult: Cell<Option<i32>>,
}

impl ApplyState {
    pub fn report(&self) -> ApplyReport {
        ApplyReport {
            outcome: self.outcome(),
            text_applied: self.text_applied(),
            stale: self.stale(),
        }
    }
    pub fn outcome(&self) -> ApplyOutcome {
        self.outcome.get()
    }
    pub fn text_applied(&self) -> bool {
        self.text_applied.get()
    }
    pub fn stale(&self) -> bool {
        self.stale.get()
    }

    pub fn begin(&self, current: bool) -> bool {
        self.executed.set(true);
        if !current || self.stale.get() {
            self.stale.set(true);
            self.outcome.set(if self.text_applied.get() {
                ApplyOutcome::TextAppliedRepairPending
            } else {
                ApplyOutcome::NotApplied
            });
            return false;
        }
        self.outcome.set(if self.text_applied.get() {
            ApplyOutcome::TextAppliedRepairPending
        } else {
            ApplyOutcome::NotApplied
        });
        true
    }

    pub fn wrote_text(&self) {
        self.text_applied.set(true);
        self.outcome.set(ApplyOutcome::TextAppliedRepairPending);
    }

    pub fn complete(&self, code: i32) {
        self.session_hresult.set(Some(code));
        self.outcome.set(if code >= 0 && !self.stale.get() {
            ApplyOutcome::Applied
        } else if self.text_applied.get() {
            ApplyOutcome::TextAppliedRepairPending
        } else {
            ApplyOutcome::NotApplied
        });
    }

    pub fn prepare_request(&self) {
        self.executed.set(false);
    }

    // The outer HRESULT is distinct from phrSession. A positive async status
    // must never stand in for an executed SetText.
    pub fn requested(&self, request: i32, session: Option<i32>, async_status: i32) {
        self.request_hresult.set(Some(request));
        if !self.executed.get() {
            self.session_hresult.set(session);
            if request >= 0 && session == Some(async_status) {
                self.outcome.set(ApplyOutcome::Deferred);
            }
        }
    }

    pub fn take_text_applied(&self) -> bool {
        self.text_applied.get() && !self.text_consumed.replace(true)
    }
}

pub(crate) fn exact_shift(requested: i32, moved: i32) -> bool {
    requested == moved
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deferred_is_not_success_and_late_completion_is_consumed_once() {
        let s = ApplyState::default();
        s.requested(0, Some(17), 17);
        assert_eq!(s.outcome(), ApplyOutcome::Deferred);
        assert!(!s.take_text_applied());
        assert!(s.begin(true));
        s.wrote_text();
        s.complete(-1);
        assert_eq!(s.outcome(), ApplyOutcome::TextAppliedRepairPending);
        assert!(s.take_text_applied());
        assert!(s.begin(true));
        assert!(s.text_applied());
        s.complete(0);
        assert_eq!(s.outcome(), ApplyOutcome::Applied);
        assert!(!s.take_text_applied());
    }
    #[test]
    fn body_acknowledgement_does_not_wait_for_decoration_or_accept_stale_work() {
        let s = ApplyState::default();
        assert!(!s.report().body_applied());
        s.begin(true);
        s.wrote_text();
        s.complete(-1);
        assert!(s.report().body_applied());
        assert!(!s.report().fully_applied());
        s.begin(false);
        assert!(s.report().text_applied);
        assert!(!s.report().body_applied());
    }
    #[test]
    fn stale_initial_execution_and_repair_never_run() {
        for written in [false, true] {
            let s = ApplyState::default();
            if written {
                s.wrote_text();
            }
            assert!(!s.begin(false));
            assert!(!s.begin(true));
            assert!(s.stale());
            assert_eq!(s.text_applied(), written);
        }
    }
    #[test]
    fn request_rejection_and_short_shift_are_not_success() {
        let s = ApplyState::default();
        s.requested(-1, None, 17);
        assert_eq!(s.outcome(), ApplyOutcome::NotApplied);
        assert!(!exact_shift(-3, -2));
        assert!(!exact_shift(3, 2));
        assert!(exact_shift(3, 3));
    }
}
