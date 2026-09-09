use crate::clause_conversion::LocalEditOutcome;
use crate::text_service::TextService_Impl;
use windows::core::{implement, IUnknownImpl, Interface, Result, BOOL};
use std::cell::Cell;
use std::rc::Rc;
use windows::Win32::UI::TextServices::{ITfContext, ITfMouseSink, ITfMouseSink_Impl, ITfMouseTracker};

#[implement(ITfMouseSink)]
struct ClauseMouseSink { owner: ITfMouseSink, valid: Rc<Cell<bool>> }

impl ITfMouseSink_Impl for ClauseMouseSink_Impl {
    fn OnMouseEvent(&self, edge: u32, quadrant: u32, buttons: u32) -> Result<BOOL> {
        if !self.valid.get() { return Ok(BOOL(0)); }
        unsafe { self.owner.OnMouseEvent(edge, quadrant, buttons) }
    }
}

impl TextService_Impl {
    pub(crate) fn clear_clause_mouse(&self) {
        let previous = self.clause_mouse.borrow_mut().take();
        if let Some((tracker, cookie, _, valid)) = previous {
            valid.set(false);
            let _ = unsafe { tracker.UnadviseMouseSink(cookie) };
        }
    }

    pub(crate) fn advise_clause_mouse(&self, context: &ITfContext) {
        let Some(composition) = self.composition.borrow().clone() else { return; };
        if self.clause_mouse.borrow().as_ref().is_some_and(|(_, _, old, _)| *old == composition) { return; }
        self.clear_clause_mouse();
        let Ok(tracker) = context.cast::<ITfMouseTracker>() else { return; };
        let Ok(range) = (unsafe { composition.GetRange() }) else { return; };
        let valid = Rc::new(Cell::new(false));
        let sink: ITfMouseSink = ClauseMouseSink { owner: self.to_interface(), valid: valid.clone() }.into();
        if let Ok(cookie) = unsafe { tracker.AdviseMouseSink(&range, &sink) } {
            if self.composition.borrow().as_ref() == Some(&composition) {
                valid.set(true);
                *self.clause_mouse.borrow_mut() = Some((tracker, cookie, composition, valid));
            } else { let _ = unsafe { tracker.UnadviseMouseSink(cookie) }; }
        }
    }

    fn click_clause_reading(&self, edge: u32, quadrant: u32, buttons: u32) -> BOOL {
        if buttons & 1 == 0 || quadrant > 3 || self.pending_commit.borrow().is_some()
            || self.composition_end_pending.get() || self.explicit_snapshot_pending.get()
            || self.conversion_queue.borrow().front().is_some()
            || self.conversion_queue.borrow().owner_lost { return BOOL(0); }
        if self.local_clause_redraw_pending.get() { return BOOL(1); }
        let Some(context) = self.current_context.borrow().clone() else { return BOOL(0); };
        let Some(mut model) = self.local_clauses.borrow().clone() else { return BOOL(0); };
        // Quadrants 0/1 lie before the nearest edge; 2/3 lie after it.
        let position = if quadrant < 2 { edge.saturating_sub(1) } else { edge } as usize;
        let mut offset = 0;
        let Some(index) = model.clauses.iter().position(|clause| {
            offset += clause.surface.encode_utf16().count(); position < offset
        }) else { return BOOL(0); };
        // Selecting a converted surface selects its reading interval, never an
        // inferred character correspondence inside e.g. 今日 / きょう.
        let cursor = model.clauses[index].start;
        let mut input = self.state.borrow().clone();
        if !input.adopt_conversion_reading(&model.reading) { return BOOL(0); }
        match model.begin_reading_edit(index) {
            LocalEditOutcome::Exhausted => { self.queue_local_clause_commit(&context, false); return BOOL(1); }
            LocalEditOutcome::Changed => {}
            _ => return BOOL(0),
        }
        input.set_reading_cursor(cursor);
        input.notation_fixed = None;
        input.invalidate_live_snapshot();
        *self.state.borrow_mut() = input;
        self.clear_conversion_queue();
        *self.local_clauses.borrow_mut() = Some(model);
        self.disarm_debounce();
        self.background_input.request_close();
        self.render_local_edit(&context);
        BOOL(1)
    }
}

impl ITfMouseSink_Impl for TextService_Impl {
    fn OnMouseEvent(&self, edge: u32, quadrant: u32, buttons: u32) -> Result<BOOL> {
        Ok(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.guarded(|| self.click_clause_reading(edge, quadrant, buttons))))
            .unwrap_or(BOOL(0)))
    }
}
