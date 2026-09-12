//! A queued Commit owns its immutable body until the STA consumes completion.
use crate::preedit_apply::PreeditRequest;
use crate::text_service::TextService_Impl;
use std::rc::Rc;
use std::time::Instant;
use windows::core::{IUnknown, Interface};
use windows::Win32::UI::TextServices::ITfContext;

pub(crate) struct PendingCommit {
    pub request: Rc<PreeditRequest>,
    pub deadline: Instant,
    pub generation: u64,
}

impl PendingCommit {
    pub fn waiting(&self, now: Instant) -> bool {
        !self.request.state.text_applied() && !self.request.state.stale() && now < self.deadline
    }

    pub fn owns(&self, context: &ITfContext, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        match (
            self.request.context.cast::<IUnknown>(),
            context.cast::<IUnknown>(),
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    }
}

impl TextService_Impl {
    pub(crate) fn conversion_queue_has_work(&self) -> bool {
        let queue = self.conversion_queue.borrow();
        queue.front().is_some() || queue.waiting || queue.commit_failed || queue.owner_lost
    }

    pub(crate) fn conversion_queue_context_changed(&self, context: Option<&ITfContext>) -> bool {
        if !self.conversion_queue_has_work() { return false; }
        let owner = self.conversion_queue_context.borrow().clone();
        let Some(owner) = owner else { return false; };
        match (owner.cast::<IUnknown>(), context.and_then(|c| c.cast::<IUnknown>().ok())) {
            (Ok(owner), Some(current)) => owner != current,
            _ => true,
        }
    }

    pub(crate) fn bind_conversion_queue_context(&self, context: &ITfContext) -> bool {
        if self.conversion_queue.borrow().owner_lost { return false; }
        if self.conversion_queue_context_changed(Some(context)) {
            self.retire_conversion_owner();
            return false;
        }
        let owner = context.clone();
        let retired = self.conversion_queue_context.borrow_mut().replace(owner);
        drop(retired);
        true
    }

    pub(crate) fn release_idle_conversion_queue_context(&self) {
        if !self.conversion_queue_has_work() {
            let retired = self.conversion_queue_context.borrow_mut().take();
            drop(retired);
        }
    }

    pub(crate) fn clear_conversion_queue(&self) {
        self.conversion_queue.borrow_mut().clear();
        self.release_idle_conversion_queue_context();
    }

    pub(crate) fn retire_conversion_owner(&self) {
        let context = self.conversion_queue_context.borrow_mut().take();
        let pending = self.pending_commit.borrow_mut().take();
        let applied = pending.as_ref().is_some_and(|p| p.request.state.take_text_applied());
        let body = pending.as_ref().map(|p| p.request.text.to_string_lossy())
            .or_else(|| self.conversion_queue.borrow().has_commit().then(|| {
                self.local_clauses.borrow().as_ref().map(|m| m.text())
                    .unwrap_or_else(|| self.live_text.borrow().clone())
            }));
        if let Some(pending) = pending.as_ref() {
            if !pending.request.state.text_applied() { pending.request.state.begin(false); }
        }
        self.conversion_queue.borrow_mut().detach(body, applied);
        self.explicit_snapshot_pending.set(false);
        self.explicit_status_deadline.set(None);
        if self.conversion_queue.borrow().owner_lost {
            crate::text_service::tip_log("ev=conversion_owner_lost pending_input=true cancel=escape");
        }
        drop(pending);
        drop(context);
    }
    pub(crate) fn pending_commit_is(&self, pending: &Rc<PendingCommit>) -> bool {
        self.pending_commit
            .borrow()
            .as_ref()
            .is_some_and(|current| Rc::ptr_eq(current, pending))
    }
    pub(crate) fn pending_commit_waiting(&self) -> bool {
        self.pending_commit
            .borrow()
            .as_ref()
            .is_some_and(|p| p.waiting(Instant::now()))
    }

    pub(crate) fn revoke_pending_commit(&self) {
        let pending = self.pending_commit.borrow_mut().take();
        if let Some(pending) = pending {
            if !pending.request.state.text_applied() {
                pending.request.state.begin(false);
            }
        }
    }

    /// Return true while this consumer owns the queue's wait resolution.
    pub(crate) fn poll_pending_commit(&self, context: &ITfContext) -> bool {
        let pending = self.pending_commit.borrow().clone();
        let Some(pending) = pending else {
            return false;
        };
        if !pending.owns(context, self.composition_generation.get()) {
            self.retire_conversion_owner();
            self.show_conversion_queue_notice(context, "元の入力先が終了しました。未処理入力を保持しています。Escで取消");
            return true;
        }
        if self.conversion_queue.borrow().commit_failed {
            return true;
        }
        let owner = pending.owns(context, self.composition_generation.get())
            && self.composition_generation.get() == pending.generation;
        if !self.pending_commit_is(&pending) {
            return true;
        }
        if owner && pending.waiting(Instant::now()) {
            return true;
        }
        self.conversion_queue.borrow_mut().waiting = false;
        if owner && pending.request.state.text_applied() {
            self.drain_conversion_actions(context);
        } else {
            pending.request.state.begin(false);
            self.conversion_queue.borrow_mut().commit_failed = true;
            self.show_conversion_queue_notice(context, "確定できません。Enterで再試行、Escで取消");
        }
        true
    }
}
