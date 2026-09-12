use crate::clause_conversion::{LocalEditOutcome, OperationMode};
use crate::input_module::{InputEvent, KeyEvent};
use crate::text_service::TextService_Impl;
use windows::core::{Result, BOOL};
use windows::Win32::Foundation::TRUE;
use windows::Win32::UI::TextServices::ITfContext;

impl TextService_Impl {
    pub(crate) fn mixed_editing(&self) -> bool {
        self.local_clauses.borrow().as_ref().is_some_and(|model| model.editing_range().is_some())
    }

    pub(crate) fn edit_mixed_reading(&self, context: &ITfContext, key: KeyEvent, original: Option<char>) -> Result<BOOL> {
        let Some(mut model) = self.local_clauses.borrow().clone() else { return Ok(TRUE); };
        let Some((start, end)) = model.editing_range() else { return Ok(TRUE); };
        let mut input = self.state.borrow().clone();
        let cursor = input.reading_cursor();
        if matches!(key, KeyEvent::Backspace) && cursor <= start
            || matches!(key, KeyEvent::Delete) && cursor >= end { return Ok(TRUE); }
        let accepted_key = key.clone();
        if matches!(key, KeyEvent::Text { .. } | KeyEvent::Backspace | KeyEvent::Delete) && input.reading_revision() == u64::MAX {
            return Ok(self.end_mixed_at_capacity(context, accepted_key, original));
        }
        match key {
            KeyEvent::ReadingHome => { input.set_reading_cursor(start); }
            KeyEvent::ReadingEnd => { input.set_reading_cursor(end); }
            _ => { input.handle(InputEvent::Key(key)); }
        }
        if let Some(original) = original { input.preserve_last_literal_original(original); }
        if input.canonical_reading() != model.reading {
            match model.replace_edited_reading(input.canonical_reading(), input.reading_revision()) {
                LocalEditOutcome::Exhausted => return Ok(self.end_mixed_at_capacity(context, accepted_key, original)),
                LocalEditOutcome::Unchanged => return Ok(TRUE),
                LocalEditOutcome::Empty => {
                    if self.do_cancel(context) { self.state.borrow_mut().reset(); self.clear_clause_nav(); }
                    else if self.replaying_conversion_queue.get() { return Ok(BOOL(0)); }
                    else if self.bind_conversion_queue_context(context) {
                        self.conversion_queue.borrow_mut().push(crate::conversion_queue::ConversionAction::Cancel);
                        self.conversion_queue.borrow_mut().commit_failed = true;
                        self.show_conversion_queue_notice(context, "取消を反映できません。Enterで再試行");
                    }
                    return Ok(TRUE);
                }
                _ => {}
            }
        } else {
            input.set_reading_cursor(input.reading_cursor().max(start).min(end));
        }
        input.notation_fixed = None;
        *self.last_reading.borrow_mut() = input.canonical_reading().to_owned();
        *self.state.borrow_mut() = input;
        *self.local_clauses.borrow_mut() = Some(model);
        *self.current_context.borrow_mut() = Some(context.clone());
        self.background_input.request_close();
        self.disarm_debounce();
        self.render_local_edit(context);
        Ok(TRUE)
    }

    fn end_mixed_at_capacity(&self, context: &ITfContext, key: KeyEvent, original: Option<char>) -> BOOL {
        if let Some(model) = self.local_clauses.borrow_mut().as_mut() { model.mode = OperationMode::Converting; }
        if let KeyEvent::Text { ch, style, .. } = key {
            if self.replaying_conversion_queue.get() {
                self.conversion_queue.borrow_mut().prepend_commit_for_insert();
                return BOOL(0);
            }
            if !self.bind_conversion_queue_context(context) { return TRUE; }
            let admitted = self.conversion_queue.borrow_mut().push_commit_then_mapped_insert(ch.to_string(), original.map(|ch| ch.to_string()), style, false);
            if admitted == crate::conversion_queue::QueueAdmission::Accepted { self.drain_conversion_actions(context); }
            else { self.show_conversion_queue_notice(context, "入力を受け付けられません。Enterで再試行、Escで取消"); }
        } else { self.queue_local_clause_commit(context, false); }
        TRUE
    }

    pub(crate) fn convert_edited_interval(&self, context: &ITfContext) {
        let Some(mut model) = self.local_clauses.borrow().clone() else { return; };
        let mut input = self.state.borrow().clone();
        match input.finalize_pending_n() {
            None => { let _ = self.end_mixed_at_capacity(context, KeyEvent::Space, None); return; }
            Some(true) => {
                if model.replace_edited_reading(input.canonical_reading(), input.reading_revision()) == LocalEditOutcome::Exhausted {
                    let _ = self.end_mixed_at_capacity(context, KeyEvent::Space, None);
                    return;
                }
                *self.last_reading.borrow_mut() = input.canonical_reading().to_owned();
                *self.state.borrow_mut() = input;
            }
            Some(false) => {}
        }
        model.mode = OperationMode::Converting;
        model.reset_notation_cycle();
        *self.local_clauses.borrow_mut() = Some(model);
        self.local_clause_space(context);
    }
}
