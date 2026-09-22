//! Non-modal reading completions; selection joins the existing clause/receipt path.
use crate::candidate_window::CandidateUI;
use crate::text_service::com_identity_eq;
use crate::text_service::TextService_Impl;
use ipc::clause::{
    ClauseCandidate, ClauseCandidatesRequest, ClauseId, ClauseRequestKey, ReadingPosition,
    SnapshotIdentity,
};
use ipc::client::EngineLearningIdentity;
use ipc::protocol::{Request, Response};
use std::time::{Duration, Instant};
use windows::Win32::UI::TextServices::ITfContext;

#[derive(Default)]
pub(crate) struct InputPredictions {
    pending: Option<(ClauseCandidatesRequest, Instant)>,
    owner: Option<ITfContext>,
    requested: Option<SnapshotIdentity>,
    dismissed: Option<SnapshotIdentity>,
    ready: Option<Ready>,
    pub selecting: bool,
}
struct Ready {
    request: ClauseCandidatesRequest,
    candidates: Vec<ClauseCandidate>,
    learning: Option<EngineLearningIdentity>,
}
impl InputPredictions {
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn visible(&self) -> bool {
        self.ready.is_some()
    }
    fn expire(&mut self, now: Instant, current: SnapshotIdentity) -> bool {
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, deadline)| now >= *deadline)
        {
            return self
                .pending
                .take()
                .is_some_and(|(request, _)| request.key.identity != current);
        }
        false
    }
    fn accepts(
        &self,
        request: &ClauseCandidatesRequest,
        identity: SnapshotIdentity,
        reading: &str,
    ) -> bool {
        !self.selecting
            && self.dismissed != Some(identity)
            && request.key.identity == identity
            && request.reading == reading
    }
}

impl TextService_Impl {
    pub(crate) fn retain_input_prediction_context(&self, context: Option<&ITfContext>) {
        let owner = self.input_predictions.borrow().owner.clone();
        let changed = owner
            .as_ref()
            .is_some_and(|owner| context.is_none_or(|current| !com_identity_eq(current, owner)));
        if changed {
            if self.input_predictions.borrow().selecting {
                self.reset_abandoned_composition();
                return;
            }
            self.hide_input_prediction_preview(true);
            self.clear_input_predictions();
        }
    }
    fn prediction_owner_matches(&self, context: &ITfContext) -> bool {
        let owner = self.input_predictions.borrow().owner.clone();
        owner
            .as_ref()
            .is_some_and(|owner| com_identity_eq(owner, context))
    }
    fn prediction_context_is_focused(&self, context: &ITfContext) -> bool {
        let manager = self.thread_mgr.borrow().clone();
        manager
            .and_then(|manager| unsafe { manager.GetFocus().ok() })
            .and_then(|document| unsafe { document.GetTop().ok() })
            .is_some_and(|top| com_identity_eq(&top, context))
    }

    fn prediction_identity(&self) -> SnapshotIdentity {
        self.state.borrow().clause_identity(
            self.configuration_generation.get(),
            self.background_input.connection_generation(),
        )
    }
    pub(crate) fn clear_input_predictions(&self) {
        let visible = self.input_predictions.borrow().visible();
        let dismissed = self.input_predictions.borrow().dismissed;
        *self.input_predictions.borrow_mut() = InputPredictions {
            dismissed,
            ..Default::default()
        };
        if visible {
            self.candidate_ui.borrow_mut().hide();
        }
    }
    pub(crate) fn hide_input_prediction_preview(&self, dismiss: bool) {
        let identity = self.prediction_identity();
        let visible = {
            let mut state = self.input_predictions.borrow_mut();
            if dismiss {
                state.dismissed = Some(identity);
            }
            state.ready.take().is_some()
        };
        if visible {
            self.candidate_ui.borrow_mut().hide();
        }
    }
    pub(crate) fn submit_input_predictions(&self, context: &ITfContext) {
        if !self.input_prediction_enabled.get()
            || self.local_clauses.borrow().is_some()
            || self.explicit_snapshot_pending.get()
            || self.showing.get()
            || self.reconverting.get()
            || (self.is_direct_mode() && !self.ephemeral_kana.get())
            || self.is_password_context(context)
        {
            return;
        }
        if !self.prediction_context_is_focused(context) {
            return;
        }
        let identity = self.prediction_identity();
        if self.acknowledged_configuration_generation.get() != identity.configuration_generation
            || self.acknowledged_snapshot_connection_generation.get()
                != identity.connection_generation
        {
            return;
        }
        let reading = {
            let input = self.state.borrow();
            if !input.composing || input.notation_fixed.is_some() {
                return;
            }
            ipc::clause::normalize_reading(input.canonical_reading())
        };
        if reading.is_empty() {
            self.hide_input_prediction_preview(false);
            return;
        }
        let mut state = self.input_predictions.borrow_mut();
        if state.pending.is_some()
            || state.selecting
            || state.requested == Some(identity)
            || state.dismissed == Some(identity)
        {
            return;
        }
        let Some(id) = self.background_input.next_clause_request_id() else {
            return;
        };
        let request = ClauseCandidatesRequest {
            key: ClauseRequestKey {
                identity,
                baseline: 0,
                conversion_revision: 0,
                clause_id: ClauseId(1),
                request_id: id,
            },
            reading: reading.clone(),
            reading_start: ReadingPosition(0),
            reading_end: ReadingPosition(reading.chars().count() as u32),
            preceding_surfaces: vec![],
            include_prefix_candidates: false,
        };
        let deadline = Instant::now() + Duration::from_millis(400);
        state.requested = Some(identity);
        if self
            .clause_worker
            .submit(Request::InputPredictions(request.clone()), deadline)
        {
            state.owner = Some(context.clone());
            state.pending = Some((request, deadline));
            drop(state);
            self.arm_clause_poll();
        }
    }
    pub(crate) fn expire_input_predictions(&self) {
        let identity = self.prediction_identity();
        let latest_needs_request = self
            .input_predictions
            .borrow_mut()
            .expire(Instant::now(), identity);
        if latest_needs_request {
            self.arm_debounce();
        }
    }
    pub(crate) fn accept_input_predictions(&self, reply: crate::clause_worker::ClauseReply) {
        let Response::InputPredictionsResult { key, candidates } = reply.response else {
            return;
        };
        let request = {
            let mut state = self.input_predictions.borrow_mut();
            if !state
                .pending
                .as_ref()
                .is_some_and(|(request, _)| request.key == key && key == reply.key)
            {
                return;
            }
            state.pending.take().map(|(request, _)| request).unwrap()
        };
        let identity = self.prediction_identity();
        let reading = ipc::clause::normalize_reading(self.state.borrow().canonical_reading());
        if !self
            .input_predictions
            .borrow()
            .accepts(&request, identity, &reading)
        {
            self.arm_debounce();
            return;
        }
        if !self.input_prediction_enabled.get()
            || !self.state.borrow().composing
            || self.local_clauses.borrow().is_some()
            || self.explicit_snapshot_pending.get()
            || self.showing.get()
            || candidates.is_empty()
            || request.validate_candidates(&candidates).is_err()
        {
            return;
        }
        let Some(context) = self.current_context.borrow().clone() else {
            return;
        };
        let owner_matches = self.prediction_owner_matches(&context);
        if !owner_matches || !self.prediction_context_is_focused(&context) {
            return;
        }
        if self.is_password_context(&context)
            || (self.is_direct_mode() && !self.ephemeral_kana.get())
        {
            return;
        }
        let values = candidates
            .iter()
            .map(|c| c.surface.clone())
            .collect::<Vec<_>>();
        self.input_predictions.borrow_mut().ready = Some(Ready {
            request,
            candidates,
            learning: reply.learning_identity.filter(|identity| {
                !self
                    .receipt_outbox
                    .borrow()
                    .as_ref()
                    .is_some_and(|outbox| outbox.rejects_identity(identity))
            }),
        });
        self.reading_monitor.borrow_mut().hide();
        let anchor = self.caret_point(&context);
        let theme = self.appearance.borrow_mut().current_theme();
        self.candidate_ui
            .borrow_mut()
            .show_preview(&values, anchor, theme);
        crate::text_service::tip_log("ev=input_predictions shown=true");
    }
    pub(crate) fn input_prediction_claims(
        &self,
        vk: u32,
        action: crate::keymap::KeyAction,
        modified: bool,
    ) -> bool {
        !modified
            && action == crate::keymap::KeyAction::None
            && (self.input_predictions.borrow().visible()
                || self.input_predictions.borrow().selecting)
            && matches!(vk, 0x09 | 0x1B)
    }
    pub(crate) fn enter_input_prediction_selection(
        &self,
        context: &ITfContext,
        index: usize,
    ) -> bool {
        let owner_matches = self.prediction_owner_matches(context);
        if !owner_matches || !self.prediction_context_is_focused(context) {
            self.clear_input_predictions();
            return false;
        }
        let identity = self.prediction_identity();
        let reading = ipc::clause::normalize_reading(self.state.borrow().canonical_reading());
        let ready = self.input_predictions.borrow_mut().ready.take();
        let Some(ready) = ready else {
            return false;
        };
        if !self
            .input_predictions
            .borrow()
            .accepts(&ready.request, identity, &reading)
        {
            self.candidate_ui.borrow_mut().hide();
            return false;
        }
        let Some(mut model) = crate::clause_conversion::ClauseConversion::from_predictions(
            identity,
            reading,
            ready.candidates,
            index,
        ) else {
            return false;
        };
        model.learning_identity = ready.learning;
        *self.local_clauses.borrow_mut() = Some(model);
        self.input_predictions.borrow_mut().selecting = true;
        self.state.borrow_mut().invalidate_live_snapshot();
        self.state.borrow_mut().invalidate_live_display();
        self.disarm_debounce();
        self.render_local_edit(context);
        true
    }
    pub(crate) fn handle_input_prediction_key(
        &self,
        context: &ITfContext,
        vk: u32,
        action: crate::keymap::KeyAction,
        modified: bool,
    ) -> bool {
        let selecting = self.input_predictions.borrow().selecting;
        if !modified && action == crate::keymap::KeyAction::None {
            if vk == 0x09 && selecting {
                if let Some(model) = self.local_clauses.borrow_mut().as_mut() {
                    model.advance_candidate(1);
                }
                self.render_local_edit(context);
                return true;
            }
            if vk == 0x09 && self.enter_input_prediction_selection(context, 0) {
                return true;
            }
            if selecting && matches!(vk, 0x1B | 0x08) {
                let reading = self.state.borrow().canonical_reading().to_owned();
                let Some(mut model) = crate::clause_conversion::ClauseConversion::from_reading(
                    self.prediction_identity(),
                    reading,
                ) else {
                    return true;
                };
                model.mode = crate::clause_conversion::OperationMode::Editing;
                *self.local_clauses.borrow_mut() = Some(model);
                self.input_predictions.borrow_mut().selecting = false;
                self.hide_input_prediction_preview(true);
                self.candidate_ui.borrow_mut().hide();
                self.render_local_edit(context);
                return true;
            } else if vk == 0x1B && self.input_predictions.borrow().visible() {
                self.hide_input_prediction_preview(true);
                return true;
            }
        }
        if !selecting {
            self.hide_input_prediction_preview(false);
        }
        false
    }
    pub(crate) fn input_prediction_mouse(
        &self,
        context: &ITfContext,
        action: Option<crate::candidate_uielement::BehaviorAction>,
        sync: bool,
    ) -> bool {
        if !self.input_predictions.borrow().visible() {
            return false;
        }
        match action {
            Some(crate::candidate_uielement::BehaviorAction::Abort) => {
                self.hide_input_prediction_preview(true)
            }
            Some(crate::candidate_uielement::BehaviorAction::Finalize) => {
                let index = self.cand_state.borrow().selected();
                if self.enter_input_prediction_selection(context, index) {
                    self.queue_local_clause_commit(context);
                }
            }
            _ if sync => { /* A hover/selection notification never changes the composition. */ }
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_prediction_retries_cannot_rearm_live_conversion_during_space_wait() {
        use crate::input_module::{InputEvent, KeyEvent, ReplayMode, TextStyle};
        for explicit_pending in [false, true] {
            for expired in [false, true] {
                let service = windows::core::ComObject::new(crate::text_service::TextService::new());
                service.state.borrow_mut().handle(InputEvent::Key(KeyEvent::Text {
                    ch: 'が', style: TextStyle::Kana, replay: ReplayMode::Full,
                }));
                let request = ClauseCandidatesRequest {
                    key: ClauseRequestKey {
                        identity: service.prediction_identity(), baseline: 0,
                        conversion_revision: 0, clause_id: ClauseId(1), request_id: 1,
                    },
                    reading: "が".into(), reading_start: ReadingPosition(0),
                    reading_end: ReadingPosition(1), preceding_surfaces: vec![],
                    include_prefix_candidates: false,
                };
                service.state.borrow_mut().handle(InputEvent::Key(KeyEvent::Text {
                    ch: 'ぞ', style: TextStyle::Kana, replay: ReplayMode::Full,
                }));
                service.input_predictions.borrow_mut().pending = Some((request.clone(), Instant::now()));
                service.explicit_snapshot_pending.set(explicit_pending);
                if expired {
                    service.expire_input_predictions();
                } else {
                    service.accept_input_predictions(crate::clause_worker::ClauseReply {
                        key: request.key,
                        response: Response::InputPredictionsResult { key: request.key, candidates: vec![] },
                        learning_identity: None, rebaseline: None,
                    });
                }
                let rearmed = service.debounce_timer.get() != 0;
                service.disarm_debounce();
                assert!(!service.input_predictions.borrow().pending());
                assert_eq!(rearmed, !explicit_pending, "expired={expired}, explicit={explicit_pending}");
            }
        }
    }

    #[test]
    fn preview_rejects_old_reading_focus_connection_config_and_selection() {
        let identity = SnapshotIdentity {
            composition: 1,
            revision: 2,
            configuration_generation: 3,
            connection_generation: 4,
        };
        let request = ClauseCandidatesRequest {
            key: ClauseRequestKey {
                identity,
                baseline: 0,
                conversion_revision: 0,
                clause_id: ClauseId(1),
                request_id: 1,
            },
            reading: "がぞ".into(),
            reading_start: ReadingPosition(0),
            reading_end: ReadingPosition(2),
            preceding_surfaces: vec![],
            include_prefix_candidates: false,
        };
        let mut state = InputPredictions::default();
        assert!(state.accepts(&request, identity, "がぞ"));
        assert!(!state.accepts(&request, identity, "がぞう"));
        for changed in [
            SnapshotIdentity {
                composition: 2,
                ..identity
            },
            SnapshotIdentity {
                revision: 3,
                ..identity
            },
            SnapshotIdentity {
                configuration_generation: 4,
                ..identity
            },
            SnapshotIdentity {
                connection_generation: 5,
                ..identity
            },
        ] {
            assert!(!state.accepts(&request, changed, "がぞ"));
        }
        let now = Instant::now();
        state.pending = Some((request.clone(), now));
        assert!(state.expire(
            now,
            SnapshotIdentity {
                revision: 3,
                ..identity
            }
        ));
        assert!(!state.pending());
        state.pending = Some((request.clone(), now));
        assert!(
            !state.expire(now, identity),
            "unchanged reading must not retry indefinitely"
        );
        assert!(!state.pending());
        state.dismissed = Some(identity);
        assert!(!state.accepts(&request, identity, "がぞ"));
        state.dismissed = None;
        state.selecting = true;
        assert!(!state.accepts(&request, identity, "がぞ"));
    }
}
