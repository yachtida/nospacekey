//! Frozen candidate objects own their source, Projection, and validated conversion.
use crate::{
    candidate_window::CandidateUI,
    local_kana_composer::LocalKanaComposer,
    mixed_worker::{Choice, Identity, Reply, Work, Worker},
    text_service::TextService_Impl,
};
use mixed_input::{
    plan::{InterpretationPlan, SegmentKind},
    position::SourceRange,
    source::CompositionSource,
};
use std::time::{Duration, Instant};
use windows::Win32::UI::TextServices::ITfContext;

pub(crate) struct Pending {
    identity: Identity,
    source: CompositionSource,
    saved: LocalKanaComposer,
    deadline: Instant,
    forced: bool,
    live: bool,
}
pub(crate) struct Menu {
    identity: Identity,
    revision: u64,
    source: CompositionSource,
    saved: LocalKanaComposer,
    choices: Vec<Choice>,
    selected: Option<usize>,
    repair: Option<(u32, u32)>,
    repair_plan: InterpretationPlan,
}
#[derive(Default)]
pub(crate) struct Controller {
    pub configured: settings::MixedInputMode,
    composition: Option<u64>,
    mode: settings::MixedInputMode,
    locked: bool,
    live: mixed_input::live::LiveState,
    live_source: Option<CompositionSource>,
    fence: mixed_input::live::CommitFence,
    worker: Option<Worker>,
    pending: Option<Pending>,
    menu: Option<Menu>,
}
impl Controller {
    pub fn clear(&mut self) {
        self.pending = None;
        self.menu = None;
    }
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
    fn blocks_ordinary_auto_commit(&mut self, composition: u64) -> bool {
        // Merely offering mixed candidates does not own the ordinary Japanese
        // composition. Dropping a prefix proposal also drops its live display.
        self.mode(composition) == settings::MixedInputMode::Auto
            || self.locked || self.pending.is_some() || self.menu.is_some()
    }
    fn mode(&mut self, composition: u64) -> settings::MixedInputMode {
        if self.composition != Some(composition) {
            self.composition = Some(composition);
            self.mode = if cfg!(feature = "mixed-input-live-trial")
                && self.configured == settings::MixedInputMode::Auto
            {
                settings::MixedInputMode::Auto
            } else {
                self.configured.effective()
            };
            self.live = Default::default();
            self.live_source = None;
            self.fence = Default::default();
            self.locked = false;
            self.pending = None;
            self.menu = None;
        }
        self.mode
    }
}
impl TextService_Impl {
    pub(crate) fn mixed_repair_available(&self) -> bool {
        self.mixed_mode_active() && !self.is_direct_mode() && !self.state.borrow().latin_mode()
            && !self.state.borrow().awaiting_llm() && !self.reconverting.get()
            && self.pending_commit.borrow().is_none() && !self.composition_end_pending.get()
            && self.conversion_queue.borrow().front().is_none()
            && !self.conversion_queue.borrow().owner_lost && !self.conversion_queue.borrow().commit_failed
    }

    pub(crate) fn begin_manual_mixed_repair(&self, ctx: &ITfContext) {
        if !self.mixed_repair_available() { return; }
        if self.mixed_menu_current() {
            self.begin_mixed_repair(ctx);
            return;
        }
        let source = self.state.borrow().composition_source();
        let saved = self.state.borrow().snapshot_composer();
        let Some(menu) = manual_repair_menu(self.mixed_identity(0), source, saved) else { return; };
        self.disarm_debounce();
        self.cancel_explicit_snapshot_wait();
        self.conversion_queue.borrow_mut().resolved();
        self.state.borrow_mut().invalidate_live_snapshot();
        self.clear_clause_nav();
        let mut controller = self.mixed_candidates.borrow_mut();
        controller.live.lock();
        controller.menu = Some(menu);
        drop(controller);
        self.show_mixed_menu(ctx, 0);
    }

    pub(crate) fn mixed_mode_active(&self) -> bool {
        let state = self.state.borrow();
        state.composing
            && self
                .mixed_candidates
                .borrow_mut()
                .mode(state.composition_id())
                != settings::MixedInputMode::Off
    }
    pub(crate) fn mixed_holds_interpretation(&self) -> bool {
        let state = self.state.borrow();
        let controller = self.mixed_candidates.borrow();
        state.composing
            && controller.composition == Some(state.composition_id())
            && controller.locked
    }
    pub(crate) fn mixed_blocks_ordinary_auto_commit(&self) -> bool {
        let state = self.state.borrow();
        state.composing && self.mixed_candidates.borrow_mut()
            .blocks_ordinary_auto_commit(state.composition_id())
    }
    fn mixed_identity(&self, request: u64) -> Identity {
        let state = self.state.borrow();
        Identity {
            composition: state.composition_id(),
            revision: state.reading_revision(),
            configuration: self.configuration_generation.get(),
            connection: self.background_input.connection_generation(),
            request,
        }
    }
    pub(crate) fn begin_mixed_candidates(&self) -> bool {
        if !self.mixed_mode_active() || self.state.borrow().latin_mode() || self.reconverting.get()
        {
            return false;
        }
        if self.acknowledged_configuration_generation.get() != self.configuration_generation.get()
            || self.acknowledged_snapshot_connection_generation.get()
                != self.background_input.connection_generation()
        {
            return false;
        }
        let source = self.state.borrow().composition_source();
        let saved = self.state.borrow().snapshot_composer();
        self.mixed_candidates.borrow_mut().live.lock();
        self.submit_mixed_candidates(source, saved, None, false)
    }
    fn submit_mixed_candidates(
        &self,
        source: CompositionSource,
        saved: LocalKanaComposer,
        forced: Option<InterpretationPlan>,
        live: bool,
    ) -> bool {
        if source.is_empty() {
            return false;
        }
        let pipe = self.engine_pipe_name();
        let request = {
            let mut controller = self.mixed_candidates.borrow_mut();
            let Some(request) = self.background_input.next_clause_request_id() else {
                return false;
            };
            if controller.worker.is_none() {
                controller.worker = Worker::start(pipe);
            }
            request
        };
        let identity = self.mixed_identity(request);
        let segments = self
            .state
            .borrow()
            .canonical_segments()
            .into_iter()
            .map(|s| ipc::protocol::SnapshotSegment {
                text: s.text,
                style: (s.style == crate::input_module::TextStyle::Direct).then(|| "direct".into()),
            })
            .collect();
        let deadline = Instant::now() + Duration::from_millis(1200);
        let is_forced = forced.is_some();
        let work = Work {
            identity,
            source: source.clone(),
            segments,
            left_context: self.left_context.borrow().clone(),
            forced,
            live: live.then(|| self.mixed_candidates.borrow().live.clone()),
            deadline,
        };
        let submitted = self
            .mixed_candidates
            .borrow()
            .worker
            .as_ref()
            .is_some_and(|worker| worker.submit(work));
        if !submitted {
            return false;
        }
        self.mixed_candidates.borrow_mut().pending = Some(Pending {
            identity,
            source,
            saved,
            deadline,
            forced: is_forced,
            live,
        });
        self.disarm_debounce();
        self.clear_input_predictions();
        self.state.borrow_mut().invalidate_live_snapshot();
        self.arm_clause_poll();
        true
    }

    /// The trial uses the same latest slot, without opening or replacing a candidate menu.
    pub(crate) fn begin_mixed_live(&self) -> bool {
        if !self.live_enabled.get() {
            return false;
        }
        let composition = self.state.borrow().composition_id();
        if self.mixed_candidates.borrow_mut().mode(composition) != settings::MixedInputMode::Auto {
            return false;
        }
        let eligible = {
            let state = self.state.borrow();
            let controller = self.mixed_candidates.borrow();
            controller.live.status != mixed_input::live::Status::UserLocked
                && controller.menu.is_none()
                && !self.showing.get()
                && !state.latin_mode()
                && !self.reconverting.get()
                && state.reading_cursor().0 == state.canonical_reading().chars().count() as u32
                && self.acknowledged_configuration_generation.get()
                    == self.configuration_generation.get()
                && self.acknowledged_snapshot_connection_generation.get()
                    == self.background_input.connection_generation()
        };
        if !eligible {
            return !ordinary_live_fallback(&self.mixed_candidates.borrow());
        }
        let current = self.state.borrow().composition_source();
        let source = trial_source(
            &current,
            self.mixed_candidates.borrow().live_source.as_ref(),
        );
        let Some(source) = source else {
            return !ordinary_live_fallback(&self.mixed_candidates.borrow());
        };
        let saved = self.state.borrow().snapshot_composer();
        let submitted = self.submit_mixed_candidates(source, saved, None, true);
        submitted || !ordinary_live_fallback(&self.mixed_candidates.borrow())
    }
    fn apply_mixed_live(
        &self,
        ctx: &ITfContext,
        pending: &Pending,
        choice: &Choice,
        fence: mixed_input::live::CommitFence,
    ) {
        if self.mixed_candidates.borrow().live.status == mixed_input::live::Status::UserLocked
            || self.showing.get()
            || self.state.borrow().reading_cursor().0
                != self.state.borrow().canonical_reading().chars().count() as u32
        {
            return;
        }
        let Choice::Mixed {
            plan,
            projection,
            display,
        } = choice
        else {
            return;
        };
        let saved = self.state.borrow().snapshot_composer();
        let old_display = self.mixed_display.borrow().clone();
        let old_text = self.live_text.borrow().clone();
        let old_reading = self.last_reading.borrow().clone();
        if !self.state.borrow_mut().adopt_mixed_projection(projection) {
            return;
        }
        let id = self.mixed_identity(0);
        let mut projection = projection.clone();
        projection.source_revision = id.revision;
        *self.mixed_display.borrow_mut() = Some(crate::mixed_conversion::MixedDisplay {
            identity: crate::mixed_conversion::MixedIdentity {
                composition: id.composition,
                revision: id.revision,
                configuration_generation: id.configuration,
                connection_generation: id.connection,
                request_id: 0,
                source_revision: id.revision,
                plan_id: projection.plan_id,
            },
            projection,
            display: display.clone(),
        });
        *self.live_text.borrow_mut() = display.text.clone();
        *self.last_reading.borrow_mut() = self.state.borrow().canonical_reading().to_string();
        self.background_input.request_close();
        let applied = self.run_preedit(ctx, &display.text);
        if self.mixed_identity(0) != id {
            return;
        }
        if !applied {
            self.state.borrow_mut().restore_composer(saved);
            let revision = self.state.borrow().reading_revision();
            let mut old_display = old_display;
            if let Some(display) = old_display.as_mut() {
                display.identity.revision = revision;
                display.identity.source_revision = revision;
                display.projection.source_revision = revision;
            }
            *self.mixed_display.borrow_mut() = old_display;
            *self.live_text.borrow_mut() = old_text;
            *self.last_reading.borrow_mut() = old_reading;
            return;
        }
        let mut controller = self.mixed_candidates.borrow_mut();
        controller.live.accept(plan.clone());
        controller.live_source = Some(pending.source.clone());
        controller.fence = fence;
    }
    pub(crate) fn poll_mixed_candidates(&self) {
        let reply = self
            .mixed_candidates
            .borrow()
            .worker
            .as_ref()
            .and_then(Worker::reply);
        let expired = self
            .mixed_candidates
            .borrow()
            .pending
            .as_ref()
            .is_some_and(|p| Instant::now() >= p.deadline);
        if reply.is_none() && !expired {
            return;
        }
        let pending = self.mixed_candidates.borrow_mut().pending.take();
        let Some(pending) = pending else {
            return;
        };
        if pending.identity != self.mixed_identity(pending.identity.request) {
            return;
        }
        let Some(ctx) = self.current_context.borrow().clone() else {
            return;
        };
        let (choices, fence) = match reply {
            Some(Reply {
                identity,
                choices,
                fence,
            }) if identity == pending.identity && !expired => (choices, fence),
            Some(_) if !expired => {
                self.mixed_candidates.borrow_mut().pending = Some(pending);
                return;
            }
            _ => (vec![], Default::default()),
        };
        if pending.live {
            if let Some(choice) = choices.first() {
                self.apply_mixed_live(&ctx, &pending, choice, fence);
            } else if ordinary_live_fallback(&self.mixed_candidates.borrow()) && !self.showing.get() {
                // Current identity was checked above. Keep the existing Japanese path
                // when classification abstains or the optional Mixed service is unavailable.
                self.submit_ordinary_live();
            }
            return;
        }
        if !choices.iter().any(|choice| matches!(choice, Choice::Mixed { .. })) {
            self.begin_explicit_snapshot_wait();
            if self.local_converting() {
                self.render_local_edit(&ctx);
            }
            return;
        }
        let source_text = pending.source.source_text();
        let Ok(plan) = InterpretationPlan::build(
            &source_text,
            &[(SegmentKind::Japanese, source_text.clone())],
        ) else {
            return;
        };
        let selected = if pending.forced {
            choices
                .iter()
                .position(|c| matches!(c, Choice::Mixed { .. }))
                .unwrap_or(0)
        } else {
            0
        };
        self.clear_clause_nav();
        self.mixed_candidates.borrow_mut().menu = Some(Menu {
            identity: pending.identity,
            revision: pending.identity.revision,
            source: pending.source,
            saved: pending.saved,
            choices,
            selected: None,
            repair: None,
            repair_plan: plan,
        });
        self.show_mixed_menu(&ctx, selected);
    }
    pub(crate) fn prepare_mixed_ordinary_receipt(&self, text: &str) -> Option<Box<dyn FnOnce()>> {
        if !self.mixed_menu_current() { return None; }
        let receipt = {
            let controller = self.mixed_candidates.borrow();
            let menu = controller.menu.as_ref()?;
            let Choice::Ordinary { text: selected, learning: Some(learning), .. } = menu.choices.get(menu.selected?)? else { return None; };
            if selected != text { return None; }
            ordinary_receipt(learning, text, crate::receipt_outbox::next_commit_id()?)?
        };
        let mut outbox = self.receipt_outbox.borrow_mut();
        if outbox.is_none() {
            *outbox = crate::receipt_outbox::ReceiptOutbox::start(crate::engine_link::stable_pipe_name()).ok();
        }
        let outbox = outbox.as_ref()?.clone();
        Some(Box::new(move || { outbox.text_applied(receipt, Instant::now()); }))
    }

    fn mixed_menu_current(&self) -> bool {
        let current = self.mixed_identity(0);
        self.mixed_candidates
            .borrow()
            .menu
            .as_ref()
            .is_some_and(|m| {
                m.identity.composition == current.composition
                    && m.revision == current.revision
                    && m.identity.configuration == current.configuration
                    && m.identity.connection == current.connection
            })
    }
    fn mixed_labels(&self) -> Vec<String> {
        self.mixed_candidates
            .borrow()
            .menu
            .as_ref()
            .map(|m| {
                if m.repair.is_some() {
                    vec!["英字のまま".into(), "日本語として解釈".into()]
                } else {
                    let mut labels: Vec<_> =
                        m.choices.iter().map(|c| c.text().to_string()).collect();
                    labels.push("区間を修正（左右で移動・Shift＋左右で範囲選択）".into());
                    labels
                }
            })
            .unwrap_or_default()
    }
    fn show_mixed_menu(&self, ctx: &ITfContext, selected: usize) {
        let labels = self.mixed_labels();
        if labels.is_empty() {
            return;
        }
        self.replace_module_candidates(&labels, selected);
        self.showing.set(true);
        let anchor = self.caret_point(ctx);
        let theme = self.appearance.borrow_mut().current_theme();
        self.candidate_ui
            .borrow_mut()
            .show(&labels, selected, anchor, theme);
        self.preview_mixed_selection(ctx);
    }
    pub(crate) fn preview_mixed_selection(&self, ctx: &ITfContext) -> bool {
        if self.mixed_candidates.borrow().menu.is_none() {
            return false;
        }
        if !self.mixed_menu_current() {
            self.cancel_mixed_menu(ctx, false);
            return true;
        }
        let index = self.cand_state.borrow().selected();
        let material = {
            let controller = self.mixed_candidates.borrow();
            let menu = controller.menu.as_ref().unwrap();
            if menu.repair.is_some() {
                drop(controller);
                self.render_mixed_repair(ctx);
                return true;
            }
            if index >= menu.choices.len() {
                return true;
            }
            if menu.selected == Some(index) {
                return true;
            }
            (menu.saved.clone(), menu.choices[index].clone())
        };
        let previous_composer = self.state.borrow().snapshot_composer();
        let previous_display = self.mixed_display.borrow().clone();
        let previous_text = self.live_text.borrow().clone();
        let previous_reading = self.last_reading.borrow().clone();
        let previous_locked = self.mixed_candidates.borrow().locked;
        let previous_plan = self
            .mixed_candidates
            .borrow()
            .menu
            .as_ref()
            .unwrap()
            .repair_plan
            .clone();
        let previous_selection = self
            .mixed_candidates
            .borrow()
            .menu
            .as_ref()
            .and_then(|m| m.selected);
        if !self.state.borrow_mut().restore_composer(material.0) {
            return true;
        }
        self.mixed_display.borrow_mut().take();
        let text = match &material.1 {
            Choice::Ordinary { text, .. } => text.clone(),
            Choice::Mixed {
                projection,
                display,
                ..
            } => {
                if !self.state.borrow_mut().adopt_mixed_projection(projection) {
                    return true;
                }
                let id = self.mixed_identity(0);
                let mut projection = projection.clone();
                projection.source_revision = id.revision;
                *self.mixed_display.borrow_mut() = Some(crate::mixed_conversion::MixedDisplay {
                    identity: crate::mixed_conversion::MixedIdentity {
                        composition: id.composition,
                        revision: id.revision,
                        configuration_generation: id.configuration,
                        connection_generation: id.connection,
                        request_id: 0,
                        source_revision: id.revision,
                        plan_id: projection.plan_id,
                    },
                    projection,
                    display: display.clone(),
                });
                display.text.clone()
            }
        };
        self.mixed_candidates.borrow_mut().locked = matches!(&material.1, Choice::Mixed { .. });
        let revision = self.state.borrow().reading_revision();
        if let Some(menu) = self.mixed_candidates.borrow_mut().menu.as_mut() {
            menu.revision = revision;
            menu.selected = Some(index);
            if let Choice::Mixed { plan, .. } = material.1 {
                menu.repair_plan = plan;
            } else {
                let source = menu.source.source_text();
                menu.repair_plan =
                    InterpretationPlan::build(&source, &[(SegmentKind::Japanese, source.clone())])
                        .unwrap();
            }
        }
        self.replace_module_candidates(&self.mixed_labels(), index);
        self.background_input.request_close();
        *self.live_text.borrow_mut() = text.clone();
        *self.last_reading.borrow_mut() = self.state.borrow().canonical_reading().to_string();
        if !self.run_preedit(ctx, &text) && self.mixed_menu_current() {
            self.state.borrow_mut().restore_composer(previous_composer);
            let revision = self.state.borrow().reading_revision();
            let mut previous_display = previous_display;
            if let Some(display) = previous_display.as_mut() {
                display.identity.revision = revision;
                display.identity.source_revision = revision;
                display.projection.source_revision = revision;
            }
            *self.mixed_display.borrow_mut() = previous_display;
            *self.live_text.borrow_mut() = previous_text;
            *self.last_reading.borrow_mut() = previous_reading;
            self.mixed_candidates.borrow_mut().locked = previous_locked;
            if let Some(menu) = self.mixed_candidates.borrow_mut().menu.as_mut() {
                menu.revision = revision;
                menu.selected = previous_selection;
                menu.repair_plan = previous_plan;
            }
            let selected = previous_selection.unwrap_or(0);
            self.cand_state.borrow_mut().set_selection(selected);
            self.replace_module_candidates(&self.mixed_labels(), selected);
        }
        true
    }
    pub(crate) fn mixed_menu_open(&self) -> bool {
        self.mixed_candidates.borrow().menu.is_some()
    }

    pub(crate) fn commit_mixed_selection(&self, ctx: &ITfContext, index: usize) -> bool {
        if self.mixed_candidates.borrow().menu.is_none() {
            return false;
        }
        if !self.mixed_menu_current() {
            self.cancel_mixed_menu(ctx, false);
            return true;
        }
        let repairing = self
            .mixed_candidates
            .borrow()
            .menu
            .as_ref()
            .unwrap()
            .repair
            .is_some();
        let choice_count = self
            .mixed_candidates
            .borrow()
            .menu
            .as_ref()
            .unwrap()
            .choices
            .len();
        if repairing {
            self.apply_mixed_repair(ctx, index);
            return true;
        }
        if index >= choice_count {
            self.begin_mixed_repair(ctx);
            return true;
        }
        self.cand_state.borrow_mut().set_selection(index);
        self.preview_mixed_selection(ctx);
        let choice = self
            .mixed_candidates
            .borrow()
            .menu
            .as_ref()
            .filter(|m| m.selected == Some(index))
            .and_then(|m| m.choices.get(index))
            .cloned();
        match choice {
            Some(Choice::Mixed { display, .. }) => {
                if self.commit_and_reset(ctx, &display.text, "candidate", Some(index)) {
                    self.mixed_candidates.borrow_mut().menu = None;
                }
            }
            Some(Choice::Ordinary { text, remaining, .. }) => {
                let before = self.mixed_identity(0);
                if let Some((request, _, _)) = self.module_candidate_commit(Some(index)) {
                    self.commit_frozen_candidate(ctx, request, index, &text, &remaining);
                }
                let current = self.mixed_identity(0);
                if before.composition == current.composition && before.revision == current.revision {
                    self.show_mixed_menu(ctx, index);
                } else {
                    self.mixed_candidates.borrow_mut().menu = None;
                }
            }
            None => {}
        }
        true
    }
    fn cancel_mixed_menu(&self, ctx: &ITfContext, restore: bool) -> bool {
        let menu = self.mixed_candidates.borrow_mut().menu.take();
        self.mixed_candidates.borrow_mut().pending = None;
        let Some(mut menu) = menu else {
            return true;
        };
        let previous_composer = self.state.borrow().snapshot_composer();
        let previous_display = self.mixed_display.borrow().clone();
        let previous_locked = self.mixed_candidates.borrow().locked;
        if restore && !self.state.borrow_mut().restore_composer(menu.saved.clone()) {
            self.mixed_candidates.borrow_mut().menu = Some(menu);
            return false;
        }
        let requested_revision = self.state.borrow().reading_revision();
        let reading = self.state.borrow().canonical_reading().to_string();
        self.showing.set(false);
        if !self.run_preedit(ctx, &reading) {
            if self.state.borrow().composition_id() == menu.identity.composition
                && self.state.borrow().reading_revision() == requested_revision
            {
                if restore {
                    self.state.borrow_mut().restore_composer(previous_composer);
                }
                let revision = self.state.borrow().reading_revision();
                let mut previous_display = previous_display;
                if let Some(display) = previous_display.as_mut() {
                    display.identity.revision = revision;
                    display.identity.source_revision = revision;
                    display.projection.source_revision = revision;
                }
                *self.mixed_display.borrow_mut() = previous_display;
                menu.revision = revision;
                let selected = self.cand_state.borrow().selected();
                self.mixed_candidates.borrow_mut().menu = Some(menu);
                self.mixed_candidates.borrow_mut().locked = previous_locked;
                self.replace_module_candidates(&self.mixed_labels(), selected);
                self.showing.set(true);
            }
            return false;
        }
        if restore {
            self.mixed_display.borrow_mut().take();
            self.mixed_candidates.borrow_mut().locked = false;
        }
        self.candidate_ui.borrow_mut().hide();
        self.state
            .borrow_mut()
            .handle(crate::input_module::InputEvent::Candidates(
                crate::input_module::CandidateEvent::Closed,
            ));
        *self.live_text.borrow_mut() = reading.clone();
        *self.last_reading.borrow_mut() = reading;
        true
    }
    pub(crate) fn handle_mixed_key(
        &self,
        ctx: &ITfContext,
        vk: u32,
        shift: bool,
        command: bool,
    ) -> bool {
        if self.state.borrow().composing {
            self.mixed_candidates
                .borrow_mut()
                .mode(self.state.borrow().composition_id());
        }
        if command || (!matches!(vk, 0x10 | 0x11 | 0x12 | 0x41..=0x5A)) {
            // Cursor movement, deletion, notation commands and explicit selection win.
            self.mixed_candidates.borrow_mut().live.lock();
        }
        if command {
            if !matches!(vk, 0x10 | 0x11 | 0x12) {
                if !self.cancel_mixed_menu(ctx, true) {
                    return true;
                }
            }
            return false;
        }
        if self.mixed_candidates.borrow().pending.is_some() {
            if !matches!(vk, 0x10 | 0x11 | 0x12) {
                self.mixed_candidates.borrow_mut().pending = None;
            }
            if vk == 0x1B {
                return true;
            }
        }
        if self.mixed_candidates.borrow().menu.is_none() {
            return false;
        }
        if vk == 0x1B {
            self.cancel_mixed_menu(ctx, true);
            return true;
        }
        let repairing = self
            .mixed_candidates
            .borrow()
            .menu
            .as_ref()
            .unwrap()
            .repair
            .is_some();
        if repairing && matches!(vk, 0x25 | 0x27 | 0x24 | 0x23) {
            let mut controller = self.mixed_candidates.borrow_mut();
            let menu = controller.menu.as_mut().unwrap();
            let (anchor, cursor) = menu.repair.unwrap();
            let next = match vk {
                0x24 => 0,
                0x23 => menu.source.source_len(),
                0x25 => cursor.saturating_sub(1),
                _ => (cursor + 1).min(menu.source.source_len()),
            };
            menu.repair = Some((if shift { anchor } else { next }, next));
            drop(controller);
            self.render_mixed_repair(ctx);
            return true;
        }
        if matches!(vk, 0x0D) {
            let index = self.cand_state.borrow().selected();
            return self.commit_mixed_selection(ctx, index);
        }
        if matches!(vk, 0x25 | 0x27 | 0x24 | 0x23) {
            self.begin_mixed_repair(ctx);
            return true;
        }
        if vk == 0x08 || vk == 0x2E {
            return !self.cancel_mixed_menu(ctx, true);
        }
        if keeps_menu_for_direct_settle(vk, shift, self.shift_latin_compose.get()) {
            return false;
        }
        if !matches!(
            vk,
            0x10 | 0x11 | 0x12 | 0x20 | 0x26 | 0x28 | 0x21 | 0x22 | 0x30..=0x39
        ) {
            // Accept the next logical input before attempting any body write. A rejected
            // intermediate redraw must not eat the key; input_char owns retryable redraw.
            self.mixed_candidates.borrow_mut().menu = None;
            self.candidate_ui.borrow_mut().hide();
            self.showing.set(false);
            self.state
                .borrow_mut()
                .handle(crate::input_module::InputEvent::Candidates(
                    crate::input_module::CandidateEvent::Closed,
                ));
        }
        false
    }
    fn begin_mixed_repair(&self, ctx: &ITfContext) {
        if let Some(menu) = self.mixed_candidates.borrow_mut().menu.as_mut() {
            menu.repair = Some((0, menu.source.source_len()));
        }
        self.show_mixed_menu(ctx, 0);
    }
    fn render_mixed_repair(&self, ctx: &ITfContext) {
        let material = self
            .mixed_candidates
            .borrow()
            .menu
            .as_ref()
            .map(|m| (m.source.source_text(), m.repair));
        let Some((text, Some((anchor, cursor)))) = material else {
            return;
        };
        // The correction editor explicitly displays source characters; labels never enter the body.
        let start = text
            .chars()
            .take(anchor.min(cursor) as usize)
            .map(char::len_utf16)
            .sum::<usize>();
        let end = text
            .chars()
            .take(anchor.max(cursor) as usize)
            .map(char::len_utf16)
            .sum::<usize>();
        self.run_preedit_with_target(ctx, &text, Some((start, end - start)));
        self.reading_monitor.borrow_mut().hide();
    }
    fn apply_mixed_repair(&self, ctx: &ITfContext, index: usize) {
        let material = {
            let controller = self.mixed_candidates.borrow();
            let menu = controller.menu.as_ref().unwrap();
            let Some((anchor, cursor)) = menu.repair else {
                return;
            };
            let range = SourceRange::new(anchor.min(cursor), anchor.max(cursor));
            let kind = if index == 0 {
                SegmentKind::Literal
            } else {
                SegmentKind::Japanese
            };
            let Some(plan) =
                mixed_input::selection::reinterpret(&menu.source, &menu.repair_plan, range, kind)
            else {
                return;
            };
            (menu.source.clone(), menu.saved.clone(), plan)
        };
        if !self.cancel_mixed_menu(ctx, true) {
            return;
        }
        let revision = self.state.borrow().reading_revision();
        let source = CompositionSource::try_new(material.0.elements().to_vec(), revision).unwrap();
        self.submit_mixed_candidates(source, material.1, Some(material.2), false);
    }
}

fn manual_repair_menu(identity: Identity, source: CompositionSource, saved: LocalKanaComposer) -> Option<Menu> {
    let text = source.source_text();
    let repair_plan = InterpretationPlan::build(&text, &[(SegmentKind::Japanese, text.clone())]).ok()?;
    Some(Menu {
        identity, revision: identity.revision, repair: Some((0, source.source_len())),
        source, saved, choices: vec![], selected: None, repair_plan,
    })
}

fn ordinary_live_fallback(controller: &Controller) -> bool {
    controller.live.status == mixed_input::live::Status::Unresolved
        && controller.live.accepted.is_none()
        && !controller.locked
        && controller.menu.is_none()
}

/// Only the provisional prefix may have become Direct through adoption. User Direct,
/// unknown source, punctuation and non-append edits stay outside the initial live trial.
fn trial_source(
    current: &CompositionSource,
    original: Option<&CompositionSource>,
) -> Option<CompositionSource> {
    use mixed_input::source::{Provenance, SourceStyle};
    let text = current.source_text();
    if text.is_empty() || !text.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return None;
    }
    let prefix = original.map_or(0, CompositionSource::source_len);
    if let Some(original) = original {
        if !text.starts_with(&original.source_text()) {
            return None;
        }
    }
    if current.layout().iter().any(|e| match e.provenance {
        Provenance::Typed {
            style: SourceStyle::Kana,
        } => false,
        Provenance::Typed {
            style: SourceStyle::Direct | SourceStyle::LiteralKana,
        } => e.source.end.get() > prefix,
        _ => true,
    }) {
        return None;
    }
    if original.is_none() {
        return Some(current.clone());
    }
    CompositionSource::try_new(
        mixed_input::classify::tune::kana_elements(&text),
        current.revision(),
    )
    .ok()
}

#[cfg(test)]
mod live_tests {
    use super::*;
    #[test]
    fn manual_mixed_repair_is_available_when_japanese_wins_classification() {
        let mut composer = LocalKanaComposer::default();
        for ch in "madewotukatteimasu".chars() {
            composer.push(ch, crate::local_kana_composer::InputStyle::Kana);
        }
        let source = composer.composition_source(1);
        let bundle = mixed_input::assets::Bundle::embedded().unwrap();
        let scored = mixed_input::classify::classify(&source, &bundle.model, &bundle.dictionary);
        assert!(mixed_input::selection::mixed_candidate_plans(&scored).next().is_none());
        let menu = manual_repair_menu(Identity {
            composition: 1, revision: 1, configuration: 1, connection: 1, request: 0,
        }, source, composer).unwrap();
        assert_eq!(menu.repair, Some((0, 18)));
        let repaired = mixed_input::selection::reinterpret(&menu.source, &menu.repair_plan,
            SourceRange::new(0, 4), SegmentKind::Literal).unwrap();
        let projection = mixed_input::projection::Projection::build(1, &menu.source, &repaired).unwrap();
        assert_eq!(projection.reading(), "madeをつかっています");
        assert_eq!(menu.saved.reading(), "までをつかっています");
    }

    #[test]
    fn candidate_setting_does_not_block_ordinary_auto_commit_but_mixed_ownership_does() {
        let mut c = Controller::default();
        c.configured = settings::MixedInputMode::Candidates;
        assert!(!c.blocks_ordinary_auto_commit(1));
        c.locked = true;
        assert!(c.blocks_ordinary_auto_commit(1), "adopted Literal must not be consumed");
        assert!(!c.blocks_ordinary_auto_commit(2), "the next composition is ordinary again");
        c.pending = Some(Pending {
            identity: Identity { composition: 2, revision: 1, configuration: 1, connection: 1, request: 1 },
            source: mixed_input::classify::tune::source_from_str("made"),
            saved: LocalKanaComposer::default(), deadline: Instant::now(), forced: false, live: false,
        });
        assert!(c.blocks_ordinary_auto_commit(2), "Space owns the pending interpretation");
        c.clear();
        assert!(!c.blocks_ordinary_auto_commit(2));
        c.configured = settings::MixedInputMode::Auto;
        assert_eq!(c.blocks_ordinary_auto_commit(3), cfg!(feature = "mixed-input-live-trial"),
            "the experimental auto mode keeps its prefix-commit guard");
    }
    #[test]
    fn abstention_uses_normal_live_but_never_reinterprets_an_adopted_or_locked_plan() {
        let mut c = Controller::default();
        assert!(ordinary_live_fallback(&c));
        c.live.accept(
            InterpretationPlan::build("made", &[(SegmentKind::Literal, "made".into())]).unwrap(),
        );
        assert!(!ordinary_live_fallback(&c));
        c.live = Default::default();
        c.live.lock();
        assert!(!ordinary_live_fallback(&c));
    }
    #[test]
    fn live_trial_restores_only_its_own_provisional_literal_prefix() {
        let original = mixed_input::classify::tune::source_from_str("made");
        let mut elements = original.elements().to_vec();
        for e in &mut elements {
            e.provenance = mixed_input::source::Provenance::Typed {
                style: mixed_input::source::SourceStyle::Direct,
            };
            e.reading = e.source_text.clone();
        }
        elements.extend(mixed_input::classify::tune::kana_elements("desu"));
        let adopted = CompositionSource::try_new(elements, 2).unwrap();
        assert!(trial_source(&adopted, None).is_none());
        let restored = trial_source(&adopted, Some(&original)).unwrap();
        assert_eq!(restored.source_text(), "madedesu");
        assert_eq!(restored.reading_text(), "までです");
        assert_eq!(restored.revision(), 2);
    }
    #[test]
    fn trial_is_composition_scoped_and_cursor_lock_blocks_reapplication() {
        let mut c = Controller::default();
        c.configured = settings::MixedInputMode::Auto;
        assert_eq!(
            c.mode(1),
            if cfg!(feature = "mixed-input-live-trial") {
                settings::MixedInputMode::Auto
            } else {
                settings::MixedInputMode::Candidates
            }
        );
        c.live.lock();
        assert_eq!(c.live.status, mixed_input::live::Status::UserLocked);
        c.configured = settings::MixedInputMode::Off;
        assert_ne!(c.mode(1), settings::MixedInputMode::Off);
        assert_eq!(c.mode(2), settings::MixedInputMode::Off);
        assert_eq!(c.live.status, mixed_input::live::Status::Unresolved);
    }
}

fn ordinary_receipt(learning: &crate::mixed_worker::OrdinaryLearning, text: &str,
    commit_id: ipc::clause::CommitId) -> Option<ipc::clause::CommitReceipt> {
    let receipt = ipc::clause::CommitReceipt {
        commit_id, engine_epoch: learning.identity.engine_epoch.clone(),
        learning_generation: learning.identity.learning_generation,
        reading: learning.reading.clone(), text: text.into(),
        sentence_token: learning.sentence.then(|| learning.token.clone()),
        intervals: vec![ipc::clause::CommitInterval {
            reading_start: ipc::clause::ReadingPosition(0),
            reading_end: ipc::clause::ReadingPosition(learning.reading.chars().count() as u32),
            surface: text.into(), learning: ipc::clause::IntervalLearning::Candidate {
                token: learning.token.clone(), explicitly_selected: true,
            },
        }],
    };
    receipt.validate(|token, _| token == learning.token).ok()?;
    Some(receipt)
}

fn keeps_menu_for_direct_settle(vk: u32, shift: bool, compose: bool) -> bool {
    shift && !compose && matches!(vk, 0x41..=0x5A)
}

#[cfg(test)]
mod commit_tests {
    use super::*;
    #[test]
    fn shift_direct_settle_retains_the_partial_candidate_menu() {
        assert!(keeps_menu_for_direct_settle(0x41, true, false));
        assert!(!keeps_menu_for_direct_settle(0x41, true, true));
        assert!(!keeps_menu_for_direct_settle(0x41, false, false));
        assert!(!keeps_menu_for_direct_settle(0x08, true, false));
    }
    #[test]
    fn ordinary_prefix_receipt_learns_only_the_consumed_reading() {
        let metadata = crate::mixed_worker::OrdinaryLearning {
            identity: ipc::client::EngineLearningIdentity { engine_epoch: "epoch".into(), learning_generation: 2 },
            reading: "とうきょう".into(), token: "tokyo".into(), sentence: false,
        };
        let receipt = ordinary_receipt(&metadata, "東京", ipc::clause::CommitId { client_instance: "test".into(), sequence: 1 }).unwrap();
        assert_eq!(receipt.reading, "とうきょう");
        assert_eq!(receipt.intervals[0].reading_end.0, 5);
        assert!(matches!(&receipt.intervals[0].learning, ipc::clause::IntervalLearning::Candidate { token, explicitly_selected: true } if token == "tokyo"));
    }
}
