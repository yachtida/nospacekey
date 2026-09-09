use crate::candidate_window::CandidateUI;
use crate::clause_conversion::{CandidateWindow, OperationMode};
use crate::text_service::TextService_Impl;
use ipc::clause::{ClauseCandidatesStatus, ClauseUnavailableReason};
use ipc::protocol::{Request, Response};
use std::time::{Duration, Instant};
use windows::Win32::UI::TextServices::ITfContext;

impl TextService_Impl {
    pub(crate) fn end_display_at_capacity(&self, context: &ITfContext) {
        self.display_end_context.borrow_mut().take();
        if !self.preedit_apply.needs_end() || self.composition.borrow().is_none()
            || self.pending_commit.borrow().is_some() || self.composition_end_pending.get() { return; }
        if !self.bind_conversion_queue_context(context) { return; }
        self.cancel_explicit_snapshot_wait();
        self.state.borrow_mut().invalidate_live_snapshot();
        self.disarm_debounce();
        self.local_clause_redraw_pending.set(false);
        self.local_clause_redraw_deadline.set(None);
        self.conversion_queue.borrow_mut().resolved();
        if !self.conversion_queue.borrow_mut().prepend_capacity_commit() {
            self.conversion_queue.borrow_mut().commit_failed = true;
            self.show_conversion_queue_notice(context, "表示上限に達しました。Enterで確定を再試行、Escで取消");
            return;
        }
        if let Some(model) = self.local_clauses.borrow_mut().as_mut() { model.close_window(); }
        self.drain_conversion_actions(context);
    }
    pub(crate) fn local_clause_backspace(&self, context: &ITfContext) -> bool {
        use crate::clause_conversion::LocalEditOutcome;
        let Some(mut draft) = self.local_clauses.borrow().clone() else { return true; };
        let outcome = draft.delete_display_tail();
        match outcome {
            LocalEditOutcome::Unchanged => return true,
            LocalEditOutcome::Exhausted => { self.queue_local_clause_commit(context, false); return true; }
            LocalEditOutcome::Empty => {
                if !self.do_cancel(context) { return false; }
                self.state.borrow_mut().reset();
                self.clear_clause_nav();
                return true;
            }
            LocalEditOutcome::ReadingChanged => {
                let revision = self.state.borrow_mut().retain_clause_reading_prefix(&draft.reading);
                let Some(revision) = revision else { self.queue_local_clause_commit(context, false); return true; };
                draft.identity.revision = revision;
                *self.last_reading.borrow_mut() = draft.reading.clone();
                // The stateful engine still owns the old suffix. Force the next
                // input to replay the retained canonical prefix, never a delta.
                self.background_input.request_close();
            }
            _ => {}
        }
        self.disarm_debounce();
        self.state.borrow_mut().notation_fixed = None;
        *self.local_clauses.borrow_mut() = Some(draft);
        self.render_local_edit(context);
        true
    }

    pub(crate) fn local_clause_escape(&self, context: &ITfContext) {
        use crate::clause_conversion::LocalEditOutcome;
        let outcome = self.local_clauses.borrow_mut().as_mut().map(|model| model.escape());
        match outcome {
            Some(LocalEditOutcome::Editing) => {
                self.state.borrow_mut().notation_fixed = None;
                self.state.borrow_mut().invalidate_live_snapshot();
                self.candidate_ui.borrow_mut().hide();
                // Keep the Editing model until its all-reading display is applied.
                self.render_local_edit(context);
            }
            Some(LocalEditOutcome::Exhausted) => self.queue_local_clause_commit(context, false),
            _ => { self.render_local_edit(context); }
        }
    }

    pub(crate) fn render_local_edit(&self, context: &ITfContext) {
        self.local_clause_redraw_pending.set(true);
        self.local_clause_redraw_deadline.set(Some(Instant::now() + Duration::from_millis(1200)));
        if !self.retry_local_clause_redraw(context) { self.arm_clause_poll(); }
    }

    /// Retry only display application. The BS/Esc model transition has already
    /// happened, so replaying the original operation would delete/escape twice.
    pub(crate) fn retry_local_clause_redraw(&self, context: &ITfContext) -> bool {
        if !self.local_clause_redraw_pending.get() { return true; }
        if !self.render_local_clauses(context) { return false; }
        self.local_clause_redraw_pending.set(false);
        self.local_clause_redraw_deadline.set(None);
        let editing = self.local_clauses.borrow().as_ref().is_some_and(|m| m.mode == OperationMode::Editing && m.editing_clause.is_none());
        if editing { self.clear_clause_nav(); }
        true
    }

    pub(crate) fn poll_local_clause_redraw(&self) {
        let Some(deadline) = self.local_clause_redraw_deadline.get() else { return; };
        if Instant::now() >= deadline {
            // Stop automatic retries, retain state and accepted input. The next
            // key retries the display before any further semantic operation.
            self.local_clause_redraw_deadline.set(None);
            return;
        }
        let context = self.current_context.borrow().clone();
        if let Some(context) = context {
            if self.retry_local_clause_redraw(&context) { self.drain_conversion_actions(&context); }
        }
    }

    pub(crate) fn apply_configured_learning_identity(&self, identity: Option<ipc::client::EngineLearningIdentity>) {
        let changed_window = {
            let mut state = self.local_clauses.borrow_mut();
            state.as_mut().is_some_and(|model| {
                let before = model.window.clone();
                if let Some(outbox) = self.receipt_outbox.borrow().as_ref() {
                    outbox.configure_model(model, identity);
                } else {
                    model.configured_learning_identity(identity);
                }
                before != model.window
            })
        };
        if changed_window { self.candidate_ui.borrow_mut().hide(); }
    }

    pub(crate) fn local_converting(&self) -> bool {
        self.local_clauses
            .borrow()
            .as_ref()
            .is_some_and(|m| m.mode == OperationMode::Converting)
    }
    pub(crate) fn local_clause_loading(&self) -> bool {
        self.local_clauses
            .borrow()
            .as_ref()
            .is_some_and(|m| matches!(m.window, CandidateWindow::Loading(_)))
    }
    pub(crate) fn render_local_clauses(&self, context: &ITfContext) -> bool {
        let composition_generation = self.composition_generation.get();
        let material = self.local_clauses.borrow().as_ref().map(|m| {
            let segments: Vec<_> = m.clauses.iter().map(|c| c.surface.clone()).collect();
            let target = (m.mode == OperationMode::Converting)
                .then(|| crate::input_state::clause_target_utf16(&segments, m.selected));
            let page = m
                .page()
                .map(|(c, i)| (c.iter().map(|c| c.surface.clone()).collect::<Vec<_>>(), i));
            (
                m.text(),
                target,
                page,
                matches!(m.window, CandidateWindow::Loading(_)),
                m.identity,
                m.revision,
                m.selected,
            )
        });
        let Some((text, target, page, loading, identity, revision, selected_clause)) = material
        else {
            return false;
        };
        self.showing.set(false);
        let applied = self.apply_preedit_with_target(context, &text, target);
        let current = self.composition_generation.get() == composition_generation
            && !self.composition_end_pending.get()
            && self.local_clauses.borrow().as_ref().is_some_and(|m| {
                m.identity == identity && m.revision == revision && m.selected == selected_clause
            });
        if !current {
            return false;
        }
        if !applied.body_applied() {
            self.selection_dirty.set(false);
            self.behavior_outbox.borrow_mut().take();
            self.candidate_ui.borrow_mut().hide();
            return false;
        }
        *self.live_text.borrow_mut() = text.clone();
        self.advise_clause_mouse(context);
        if page.is_some() || loading {
            let (values, selected_candidate) =
                page.unwrap_or_else(|| (vec!["候補を取得中…".into()], 0));
            let anchor = self.caret_point(context);
            if !self.local_clauses.borrow().as_ref().is_some_and(|m| {
                m.identity == identity && m.revision == revision && m.selected == selected_clause
            }) {
                return false;
            }
            let theme = self.appearance.borrow_mut().current_theme();
            self.candidate_ui
                .borrow_mut()
                .show(&values, selected_candidate, anchor, theme);
            if !loading {
                let list = if self.prediction_enabled.get() { String::new() }
                    else { format!(" list={}", values.join("|")) };
                crate::text_service::tip_log(&format!("ev=candidates_shown n={} sel={selected_candidate}{list}", values.len()));
                crate::text_service::tip_log(&format!("ev=clause_presented window=ready sel={selected_candidate}"));
            }
        } else {
            self.candidate_ui.borrow_mut().hide();
            crate::text_service::tip_log("ev=clause_presented window=closed sel=0");
            crate::text_service::tip_log("ev=candidates_hidden");
        }
        applied.fully_applied()
    }
    pub(crate) fn local_clause_space(&self, context: &ITfContext) {
        let now = Instant::now();
        let request_id = self.background_input.next_clause_request_id();
        if request_id.is_none() || self.local_clauses.borrow().as_ref().is_some_and(|model| model.revision == u64::MAX) {
            self.queue_local_clause_commit(context, false);
            return;
        }
        let request = {
            let mut state = self.local_clauses.borrow_mut();
            let Some(model) = state.as_mut() else {
                return;
            };
            if matches!(model.window, CandidateWindow::Ready { .. }) {
                model.advance_candidate(1);
                None
            } else {
                request_id.and_then(|id| model.open_boundary_conversion(id, now).map(Request::ConvertClauses)
                    .or_else(|| model.open_candidates(id, now).map(Request::ClauseCandidates)))
            }
        };
        if let Some(request) = request {
            let key = match &request { Request::ConvertClauses(r) => r.key, Request::ClauseCandidates(r) => r.key, _ => unreachable!() };
            if !self.clause_worker.submit(
                request,
                now + Duration::from_millis(1200),
            ) {
                if let Some(model) = self.local_clauses.borrow_mut().as_mut() {
                    if model.boundary_loading() { model.close_window(); }
                    else { model.accept_candidates(
                        key,
                        ClauseCandidatesStatus::Unavailable {
                            reason: ClauseUnavailableReason::Busy,
                        },
                        now,
                    ); }
                }
            }
        }
        self.render_local_edit(context);
        if self.local_clause_loading() { self.conversion_queue.borrow_mut().begin(now, None); }
        self.arm_clause_poll();
    }
    pub(crate) fn poll_local_clause_results(&self) {
        if let Some(Some(identity)) = self.clause_worker.take_learning_refresh() {
            let rejected = self.receipt_outbox.borrow().as_ref()
                .is_some_and(|outbox| outbox.rejects_identity(&identity));
            if rejected {
                self.clause_worker.refresh_learning();
            } else {
                let changed = {
                    let mut model = self.local_clauses.borrow_mut();
                    model.as_mut().is_some_and(|model| {
                        let before = model.window.clone();
                        model.refresh_learning_identity(identity);
                        before != model.window
                    })
                };
                if changed { self.candidate_ui.borrow_mut().hide(); }
            }
        }
        let context = self.current_context.borrow().clone();
        if let Some(context) = context {
            if self.poll_pending_commit(&context) { return; }
        } else if self.pending_commit.borrow().is_some() {
            self.retire_conversion_owner();
            return;
        }
        let mut changed = false;
        let mut boundary_applied = false;
        while let Some(reply) = self.clause_worker.try_reply() {
            let identity = reply.learning_identity.filter(|identity| {
                !self.receipt_outbox.borrow().as_ref().is_some_and(|outbox| outbox.rejects_identity(identity))
            });
            let mut next = None;
            let mut rebaseline = None;
            if let Some(model) = self.local_clauses.borrow_mut().as_mut() {
                let before = model.window.clone();
                let now = Instant::now();
                if let Some(attempt) = reply.rebaseline {
                    let request = identity.and_then(|identity| model.accept_rebaseline(&attempt, &reply.response, identity, now));
                    if let Some(request) = request {
                        next = Some((Request::ConvertClauses(request), attempt.deadline()));
                    } else {
                        model.end_request(reply.key);
                    }
                } else { match reply.response {
                    Response::ClauseCandidatesResult { key, status } if key == reply.key => {
                        let new_epoch = identity.as_ref().zip(model.learning_identity.as_ref())
                            .is_some_and(|(new, old)| new.engine_epoch != old.engine_epoch);
                        if new_epoch || matches!(status, ClauseCandidatesStatus::Unavailable {
                            reason: ClauseUnavailableReason::Expired | ClauseUnavailableReason::Disconnected }) {
                            if let (Some(snapshot_id), Some(convert_id)) = (self.background_input.next_clause_request_id(),
                                self.background_input.next_clause_request_id()) {
                                rebaseline = model.prepare_rebaseline(key, snapshot_id, convert_id, now);
                            }
                        }
                        if rebaseline.is_none() { model.accept_candidates_with_identity(key, status, identity, now); }
                    }
                    Response::ConvertClausesResult { key, status } if key == reply.key => {
                        if model.boundary_loading() {
                            let new_epoch = identity.as_ref().zip(model.learning_identity.as_ref())
                                .is_some_and(|(new, old)| new.engine_epoch != old.engine_epoch);
                            if new_epoch || matches!(status, ipc::clause::ConvertClausesStatus::Unavailable {
                                reason: ClauseUnavailableReason::Expired | ClauseUnavailableReason::Disconnected }) {
                                if let (Some(snapshot_id), Some(convert_id)) = (self.background_input.next_clause_request_id(),
                                    self.background_input.next_clause_request_id()) {
                                    rebaseline = model.prepare_rebaseline(key, snapshot_id, convert_id, now);
                                }
                            }
                            if rebaseline.is_none() { boundary_applied |= model.accept_boundary_conversion(key, status, identity, now); }
                        } else {
                        let deadline = model.request_deadline(key);
                        let request_id = self.background_input.next_clause_request_id().unwrap_or(0);
                        if let (Some(request), Some(deadline)) = (model.accept_rebased_conversion(key, status, identity, request_id, now), deadline) {
                            next = Some((Request::ClauseCandidates(request), deadline));
                        }
                        }
                    }
                    _ => {
                        model.accept_candidates(
                            reply.key,
                            ClauseCandidatesStatus::Unavailable {
                                reason: ClauseUnavailableReason::InvalidRequest,
                            },
                            now,
                        );
                    }
                } }
                changed |= before != model.window;
            }
            if let Some(attempt) = rebaseline {
                let key = attempt.original_key();
                if !self.clause_worker.submit_rebaseline(attempt, self.left_context.borrow().clone()) {
                    if let Some(model) = self.local_clauses.borrow_mut().as_mut() {
                        model.end_request(key);
                        changed = true;
                    }
                }
            }
            if let Some((request, deadline)) = next {
                let key = match &request { Request::ConvertClauses(r) => r.key, Request::ClauseCandidates(r) => r.key, _ => unreachable!() };
                if !self.clause_worker.submit(request, deadline) {
                    if let Some(model) = self.local_clauses.borrow_mut().as_mut() {
                        model.end_request(key);
                        changed = true;
                    }
                }
            }
        }
        if let Some(model) = self.local_clauses.borrow_mut().as_mut() {
            let before = model.window.clone();
            model.expire(Instant::now());
            changed |= before != model.window;
        }
        if changed {
            let context = self.current_context.borrow().clone();
            if let Some(context) = context {
                self.render_local_edit(&context);
            }
        }
        let context = self.current_context.borrow().clone();
        if let Some(context) = context {
            if self.conversion_queue.borrow().expired(Instant::now()) {
                self.fail_conversion_wait(&context);
            } else if self.conversion_queue.borrow().waiting
                && !self.explicit_snapshot_pending.get()
                && !self.local_clause_loading()
            {
                let ready = self.local_clauses.borrow().as_ref().is_some_and(|m| matches!(m.window, CandidateWindow::Ready { .. }));
                if ready || boundary_applied { self.conversion_queue.borrow_mut().resolved(); }
                else { self.conversion_queue.borrow_mut().fail_wait(); }
                self.drain_conversion_actions(&context);
            }
        }
    }
    pub(crate) fn commit_local_clauses(&self, context: &ITfContext) -> bool {
        if !self.retry_local_clause_redraw(context) { return false; }
        // CommitText writes the complete model itself. A preedit write here
        // duplicates the body and can fail caret repair before commit starts.
        let text = self.local_clauses.borrow().as_ref().map(|m| m.text());
        text.is_some_and(|text| self.commit_and_reset(context, &text, "clause", None))
    }

    pub(crate) fn queue_local_clause_commit(&self, context: &ITfContext, suppress_prediction: bool) {
        use crate::conversion_queue::{ConversionAction, QueueAdmission};
        if self.replaying_conversion_queue.get() && self.conversion_queue.borrow_mut().commit_exhausted_action() {
            return;
        }
        if !self.bind_conversion_queue_context(context) { return; }
        let admitted = {
            let mut queue = self.conversion_queue.borrow_mut();
            let admitted = queue.push(ConversionAction::Commit);
            if admitted == QueueAdmission::Accepted && suppress_prediction {
                queue.suppress_commit_prediction = true;
            }
            admitted
        };
        if admitted != QueueAdmission::Accepted {
            self.show_conversion_queue_notice(context, "入力を受け付けられません。Enterで再試行、Escで取消");
            return;
        }
        if let Some(model) = self.local_clauses.borrow_mut().as_mut() { model.close_window(); }
        self.drain_conversion_actions(context);
    }
}
