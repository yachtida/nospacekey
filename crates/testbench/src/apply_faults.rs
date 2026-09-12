//! Deterministic delivery at the engine-response and RequestEditSession seams.
//! Uses the production COM preedit session and real msctf ranges/property APIs.
use crate::apply_state::{ApplyOutcome, ApplyState};
use crate::preedit_apply::{PreeditApply, PreeditRequest};
use crate::preedit_session::StartOrUpdatePreedit;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use windows::core::{implement, Interface, Ref, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::TextServices::*;

/// Explicit clock: no sleep, no engine process, no dependence on GPU timing.
pub(crate) struct ResponseDelivery<T> {
    entries: Vec<(u64, u64, T)>,
}
impl<T> ResponseDelivery<T> {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
    pub fn hold(&mut self, request_id: u64, ready_at: u64, response: T) {
        self.entries.push((request_id, ready_at, response));
    }
    pub fn release(&mut self, request_id: u64, now: u64) -> Option<T> {
        let index = self
            .entries
            .iter()
            .position(|(id, ready, _)| *id == request_id && *ready <= now)?;
        Some(self.entries.remove(index).2)
    }
}

#[implement(ITfCompositionSink)]
struct Sink {
    apply: Rc<PreeditApply>,
    composition: Rc<RefCell<Option<ITfComposition>>>,
    terminations: Rc<Cell<u32>>,
}
impl ITfCompositionSink_Impl for Sink_Impl {
    fn OnCompositionTerminated(
        &self,
        _ecwrite: u32,
        composition: Ref<'_, ITfComposition>,
    ) -> Result<()> {
        let current = self.composition.borrow().clone();
        if current
            .as_ref()
            .is_some_and(|c| composition.ok().is_ok_and(|p| c == p))
        {
            self.apply.invalidate();
            self.composition.borrow_mut().take();
            self.terminations.set(self.terminations.get() + 1);
        }
        Ok(())
    }
}

struct Harness {
    deliveries: Rc<RefCell<Vec<String>>>,
    commit_report: RefCell<Option<Rc<ApplyState>>>,
    #[cfg(feature = "tsf-test-hooks")]
    fail_attributes: Rc<Cell<bool>>,
    caret: Rc<RefCell<Option<ITfRange>>>,
    end_pending: Rc<Cell<bool>>,
    end_context: Rc<RefCell<Option<ITfContext>>>,
    end_status: Rc<Cell<crate::commit_session::CompositionEndStatus>>,
    end_retry_count: Rc<Cell<u8>>,
    end_epoch: Rc<Cell<u64>>,
    manager: ITfThreadMgr,
    document: ITfDocumentMgr,
    context: ITfContext,
    tid: u32,
    store: Rc<crate::text_store::StoreState>,
    apply: Rc<PreeditApply>,
    composition: Rc<RefCell<Option<ITfComposition>>>,
    sink: ITfCompositionSink,
    terminations: Rc<Cell<u32>>,
    #[cfg(feature = "tsf-test-hooks")]
    faults: Rc<crate::preedit_session::PreeditFaults>,
}
impl Harness {
    fn new() -> Result<Self> {
        unsafe {
            let manager: ITfThreadMgr =
                CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
            let tid = manager.Activate()?;
            let document = manager.CreateDocumentMgr()?;
            let (store_if, store) = crate::text_store::HarnessTextStore::create(HWND::default());
            let mut context = None;
            let mut cookie = 0;
            document.CreateContext(tid, 0, &store_if, &mut context, &mut cookie)?;
            let context = context.unwrap();
            document.Push(&context)?;
            let apply = Rc::new(PreeditApply::default());
            let composition = Rc::new(RefCell::new(None));
            let terminations = Rc::new(Cell::new(0));
            let sink: ITfCompositionSink = Sink {
                apply: apply.clone(),
                composition: composition.clone(),
                terminations: terminations.clone(),
            }
            .into();
            Ok(Self {
                deliveries: Rc::new(RefCell::new(Vec::new())),
                commit_report: RefCell::new(None),
                #[cfg(feature = "tsf-test-hooks")]
                fail_attributes: Rc::new(Cell::new(false)),
                caret: Rc::new(RefCell::new(None)),
                end_pending: Rc::new(Cell::new(false)),
                end_context: Rc::new(RefCell::new(None)),
                end_status: Rc::new(Cell::new(crate::commit_session::CompositionEndStatus::Idle)),
                end_retry_count: Rc::new(Cell::new(0)),
                end_epoch: Rc::new(Cell::new(0)),
                manager,
                document,
                context,
                tid,
                store,
                apply,
                composition,
                sink,
                terminations,
                #[cfg(feature = "tsf-test-hooks")]
                faults: Rc::new(crate::preedit_session::PreeditFaults::default()),
            })
        }
    }
    fn request(
        &self,
        text: &str,
        target: Option<(usize, usize)>,
    ) -> (Rc<PreeditRequest>, ITfEditSession) {
        self.decorated_request(text, target, Vec::new())
    }
    fn decorated_request(&self, text: &str, target: Option<(usize, usize)>, converted: Vec<(usize, usize)>) -> (Rc<PreeditRequest>, ITfEditSession) {
        let request = self.apply.request_display(
            &self.context,
            self.composition.borrow().clone(),
            text,
            target,
            None,
            converted,
        );
        let session = StartOrUpdatePreedit {
            #[cfg(feature = "tsf-test-hooks")]
            faults: self.faults.clone(),
            apply: self.apply.clone(),
            request: request.clone(),
            on_complete: |_| {},
            capture_left_context: |_, _| None,
            sink: self.sink.clone(),
            da_variant: VARIANT::from(1i32),
            da_target_variant: VARIANT::from(2i32),
            da_converted_variant: VARIANT::from(3i32),
            composition: self.composition.clone(),
            started: Rc::new(Cell::new(false)),
            left_context_out: Rc::new(RefCell::new(None)),
            _guard: crate::globals::ComObjectGuard::new(),
        }
        .into();
        (request, session)
    }
    fn commit_request(&self, text: &str) -> (Rc<PreeditRequest>, ITfEditSession) {
        self.commit_request_until(text, None)
    }
    fn commit_request_until(
        &self,
        text: &str,
        deadline: Option<std::time::Instant>,
    ) -> (Rc<PreeditRequest>, ITfEditSession) {
        use crate::commit_session::CommitText;
        self.apply.invalidate();
        self.end_epoch.set(self.end_epoch.get() + 1);
        let composition = self.composition.borrow().clone();
        let request = self.apply.request(&self.context, composition, text, None);
        *self.commit_report.borrow_mut() = Some(request.state.clone());
        let deliveries = self.deliveries.clone();
        let frozen_text = text.to_string();
        let session = CommitText {
            on_text_applied: RefCell::new(Some(Box::new(move || deliveries.borrow_mut().push(frozen_text)))),
            #[cfg(feature = "tsf-test-hooks")]
            fail_attributes: self.fail_attributes.clone(),
            caret: self.caret.clone(),
            apply: self.apply.clone(),
            request: request.clone(),
            executed: Cell::new(false),
            deadline,
            context: self.context.clone(),
            text: text.into(),
            composition: self.composition.clone(),
            end_pending: self.end_pending.clone(),
            end_context: self.end_context.clone(),
            end_status: self.end_status.clone(),
            end_retry_count: self.end_retry_count.clone(),
            _guard: crate::globals::ComObjectGuard::new(),
        }
        .into();
        (request, session)
    }
    fn repair_session(&self) -> (Rc<Cell<bool>>, ITfEditSession) {
        self.repair_session_with_cancel(false)
    }
    fn repair_session_with_cancel(
        &self,
        cancel_selection: bool,
    ) -> (Rc<Cell<bool>>, ITfEditSession) {
        let active = Rc::new(Cell::new(true));
        let session = crate::commit_session::EndCompositionOnly {
            report: self.commit_report.borrow().clone(),
            cancel_selection,
            #[cfg(feature = "tsf-test-hooks")]
            fail_attributes: self.fail_attributes.clone(),
            expected_context: self.context.clone(),
            expected_composition: self.composition.borrow().clone(),
            caret: self.caret.clone(),
            active: active.clone(),
            executed: Cell::new(false),
            epoch: self.end_epoch.clone(),
            expected_epoch: self.end_epoch.get(),
            composition: self.composition.clone(),
            end_pending: self.end_pending.clone(),
            end_context: self.end_context.clone(),
            end_status: self.end_status.clone(),
            end_retry_count: self.end_retry_count.clone(),
            _guard: crate::globals::ComObjectGuard::new(),
        }
        .into();
        (active, session)
    }
    fn execute_repair(&self, session: &ITfEditSession) -> bool {
        unsafe {
            self.context.RequestEditSession(
                self.tid,
                session,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0),
            )
        }
        .is_ok_and(|hr| hr.is_ok())
    }
    fn execute(&self, request: &PreeditRequest, session: &ITfEditSession) -> ApplyOutcome {
        request.state.prepare_request();
        let result = unsafe {
            self.context.RequestEditSession(
                self.tid,
                session,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0),
            )
        };
        match result {
            Ok(hr) => request.state.requested(0, Some(hr.0), TF_S_ASYNC.0),
            Err(error) => request.state.requested(error.code().0, None, TF_S_ASYNC.0),
        }
        request.state.outcome()
    }
    fn terminate(&self) -> Result<()> {
        let composition = self.composition.borrow().clone();
        if let Some(composition) = composition {
            let owner: ITfContextOwnerCompositionServices = self.context.cast()?;
            unsafe {
                owner.TerminateComposition(&composition.cast::<ITfCompositionView>()?)?;
            }
        }
        Ok(())
    }
    fn display_attributes_present(&self) -> Result<bool> {
        self.attribute_values().map(|values| values.iter().any(Option::is_some))
    }
    fn attribute_values(&self) -> Result<Vec<Option<i32>>> {
        let result = Rc::new(RefCell::new(None));
        let session: ITfEditSession = AttributeProbe {
            context: self.context.clone(),
            units: self.store.full().encode_utf16().count() as i32,
            result: result.clone(),
        }
        .into();
        unsafe {
            self.context.RequestEditSession(
                self.tid,
                &session,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READ.0),
            )?
        }
        .ok()?;
        let values = result.borrow_mut().take();
        values
            .ok_or_else(|| windows::Win32::Foundation::E_FAIL.into())
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.apply.invalidate();
        let _ = self.terminate();
        unsafe {
            let _ = self.document.Pop(TF_POPF_ALL);
            let _ = self.manager.Deactivate();
        }
    }
}

// Uses ReconvertStart's production range check with a real, clamped msctf range.
#[implement(ITfEditSession)]
struct RangeWrite {
    context: ITfContext,
    state: Rc<ApplyState>,
}
impl ITfEditSession_Impl for RangeWrite_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        self.state.begin(true);
        let result: Result<()> = (|| unsafe {
            let range = self.context.GetEnd(ec)?;
            crate::edit_range::shift_start_exact(&range, ec, -4)?;
            range.SetText(ec, 0, &[b'x' as u16])?;
            self.state.wrote_text();
            Ok(())
        })();
        self.state
            .complete(result.as_ref().err().map_or(0, |e| e.code().0));
        result
    }
}

#[implement(ITfEditSession)]
struct AttributeProbe {
    context: ITfContext,
    units: i32,
    result: Rc<RefCell<Option<Vec<Option<i32>>>>>,
}
impl ITfEditSession_Impl for AttributeProbe_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        unsafe {
            let property = self.context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
            let mut values = Vec::new();
            for index in 0..self.units {
                let range = self.context.GetStart(ec)?;
                let mut moved = 0;
                range.ShiftEnd(ec, index + 1, &mut moved, core::ptr::null())?;
                if moved != index + 1 {
                    return Err(windows::Win32::Foundation::E_FAIL.into());
                }
                range.ShiftStart(ec, index, &mut moved, core::ptr::null())?;
                if moved != index {
                    return Err(windows::Win32::Foundation::E_FAIL.into());
                }
                let value = property.GetValue(ec, &range)?;
                values.push((value.Anonymous.Anonymous.vt == windows::Win32::System::Variant::VT_I4)
                    .then(|| value.Anonymous.Anonymous.Anonymous.lVal));
            }
            *self.result.borrow_mut() = Some(values);
        }
        Ok(())
    }
}

pub fn run() -> i32 {
    let _com = match crate::tsf_host::ComSta::init() {
        Ok(com) => com,
        Err(error) => {
            println!("apply-faults : ERROR ({error})");
            return 2;
        }
    };
    type Case = (&'static str, fn(&Harness) -> Result<bool>);
    let cases: &[Case] = &[
        ("commit notification survives body repair and fires only once", |h| {
            let (initial, session) = h.request("reading", None);
            h.execute(&initial, &session);
            let (commit, session) = h.commit_request("body");
            commit.state.requested(0, Some(TF_S_ASYNC.0), TF_S_ASYNC.0);
            let deferred_has_no_receipt = h.deliveries.borrow().is_empty();
            h.store.reject_selection.set(true);
            h.execute(&commit, &session);
            let body_delivered = *h.deliveries.borrow() == ["body"];
            h.store.reject_selection.set(false);
            let (_, repair) = h.repair_session();
            let repaired = h.execute_repair(&repair);
            h.execute(&commit, &session);
            Ok(deferred_has_no_receipt && body_delivered && repaired
                && *h.deliveries.borrow() == ["body"] && h.store.full() == "body")
        }),
        #[cfg(feature = "tsf-test-hooks")]
        (
            "explicit cancellation clears attributes without moving caret",
            |h| {
                let (initial, s) = h.request("seed", Some((1, 2)));
                h.execute(&initial, &s);
                h.fail_attributes.set(true);
                let (r, s) = h.commit_request("body");
                h.execute(&r, &s);
                let pending = h.end_pending.get();
                let writes = h.store.text_writes.get();
                h.store.reject_selection.set(true);
                let (_, repair) = h.repair_session_with_cancel(true);
                Ok(pending
                    && h.execute_repair(&repair)
                    && !h.display_attributes_present()?
                    && !h.end_pending.get()
                    && h.store.full() == "body"
                    && h.store.text_writes.get() == writes)
            },
        ),
        #[cfg(feature = "tsf-test-hooks")]
        (
            "explicit cancellation exits even when attribute cleanup is rejected",
            |h| {
                let (initial, s) = h.request("seed", None);
                h.execute(&initial, &s);
                h.fail_attributes.set(true);
                let (r, s) = h.commit_request("body");
                h.execute(&r, &s);
                let pending = h.end_pending.get();
                h.fail_attributes.set(true);
                let (_, repair) = h.repair_session_with_cancel(true);
                Ok(pending
                    && h.execute_repair(&repair)
                    && !h.end_pending.get()
                    && h.composition.borrow().is_none()
                    && h.store.full() == "body"
                    && r.state.report().text_applied
                    && !r.state.report().fully_applied())
            },
        ),
        (
            "commit clears display attributes over the whole body",
            |h| {
                let (initial, s) = h.request("seed", Some((1, 2)));
                h.execute(&initial, &s);
                let present = h.display_attributes_present()?;
                let (r, s) = h.commit_request("body");
                h.execute(&r, &s);
                Ok(present
                    && r.state.take_text_applied()
                    && !h.end_pending.get()
                    && !h.display_attributes_present()?
                    && h.store.full() == "body")
            },
        ),
        #[cfg(feature = "tsf-test-hooks")]
        (
            "attribute clear rejection repairs without rewriting body",
            |h| {
                let (initial, s) = h.request("seed", Some((1, 2)));
                h.execute(&initial, &s);
                let present = h.display_attributes_present()?;
                h.fail_attributes.set(true);
                let (r, s) = h.commit_request("body");
                let outcome = h.execute(&r, &s);
                let writes = h.store.text_writes.get();
                let pending = h.end_pending.get() && h.caret.borrow().is_some();
                let (_, repair) = h.repair_session();
                Ok(present
                    && pending
                    && outcome == ApplyOutcome::TextAppliedRepairPending
                    && r.state.take_text_applied()
                    && h.execute_repair(&repair)
                    && !h.display_attributes_present()?
                    && h.store.text_writes.get() == writes
                    && !h.end_pending.get()
                    && h.store.full() == "body"
                    && r.state.report().fully_applied()
                    && !r.state.take_text_applied())
            },
        ),
        (
            "body completing after deadline retains repair barrier",
            |h| {
                let (initial, s) = h.request("reading", None);
                h.execute(&initial, &s);
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1000);
                let (r, s) = h.commit_request_until("body", Some(deadline));
                *h.store.on_text.borrow_mut() = Some(Box::new(move || {
                    std::thread::sleep(
                        deadline.saturating_duration_since(std::time::Instant::now())
                            + std::time::Duration::from_millis(1),
                    );
                }));
                h.store.reject_selection.set(true);
                h.execute(&r, &s);
                let pending = r.state.take_text_applied()
                    && h.end_pending.get()
                    && h.caret.borrow().is_some();
                let writes = h.store.text_writes.get();
                h.store.reject_selection.set(false);
                let (_, repair) = h.repair_session();
                Ok(pending
                    && h.execute_repair(&repair)
                    && h.store.full() == "body"
                    && h.store.selection() == (4, 4)
                    && h.store.text_writes.get() == writes)
            },
        ),
        (
            "explicit caret abandonment permits close without deleting body",
            |h| {
                let (initial, s) = h.request("reading", None);
                h.execute(&initial, &s);
                let (r, s) = h.commit_request("body");
                h.store.reject_selection.set(true);
                h.execute(&r, &s);
                let writes = h.store.text_writes.get();
                let pending = h.end_pending.get();
                let (_, repair) = h.repair_session_with_cancel(true);
                Ok(pending
                    && h.execute_repair(&repair)
                    && !h.end_pending.get()
                    && h.store.full() == "body"
                    && h.store.text_writes.get() == writes)
            },
        ),
        ("commit caret repair closes without rewriting body", |h| {
            let (initial, s) = h.request("reading", None);
            h.execute(&initial, &s);
            let (r, s) = h.commit_request("body");
            h.store.reject_selection.set(true);
            h.execute(&r, &s);
            let writes = h.store.text_writes.get();
            let pending = h.end_pending.get()
                && h.composition.borrow().is_some()
                && r.state.take_text_applied();
            let (_, retry) = h.repair_session();
            let rejected = !h.execute_repair(&retry) && h.end_pending.get();
            h.store.reject_selection.set(false);
            let (_, retry) = h.repair_session();
            Ok(pending
                && rejected
                && h.execute_repair(&retry)
                && !h.end_pending.get()
                && h.composition.borrow().is_none()
                && h.store.selection() == (4, 4)
                && h.store.full() == "body"
                && h.store.text_writes.get() == writes)
        }),
        ("direct commit caret repair writes body once", |h| {
            let (r, s) = h.commit_request("body");
            h.store.reject_selection.set(true);
            h.execute(&r, &s);
            let pending = h.end_pending.get() && r.state.take_text_applied();
            let writes = h.store.text_writes.get();
            h.store.reject_selection.set(false);
            let (_, retry) = h.repair_session();
            Ok(pending
                && h.execute_repair(&retry)
                && !h.end_pending.get()
                && h.store.selection() == (4, 4)
                && h.store.text_writes.get() == writes)
        }),
        ("revoked close session cannot repair selection", |h| {
            let (r, s) = h.commit_request("body");
            h.store.reject_selection.set(true);
            h.execute(&r, &s);
            let (active, retry) = h.repair_session();
            active.set(false);
            h.store.reject_selection.set(false);
            let selection = h.store.selection();
            Ok(!h.execute_repair(&retry)
                && h.end_pending.get()
                && h.store.selection() == selection
                && h.caret.borrow().is_some())
        }),
        ("old close epoch cannot repair newer commit", |h| {
            let (r, s) = h.commit_request("body");
            h.store.reject_selection.set(true);
            h.execute(&r, &s);
            let (_, retry) = h.repair_session();
            h.end_epoch.set(h.end_epoch.get() + 1);
            h.store.reject_selection.set(false);
            Ok(!h.execute_repair(&retry) && h.end_pending.get() && h.caret.borrow().is_some())
        }),
        (
            "commit deferred body executes before its fixed deadline",
            |h| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1200);
                let (r, s) = h.commit_request_until("body", Some(deadline));
                r.state.requested(0, Some(TF_S_ASYNC.0), TF_S_ASYNC.0);
                let deferred =
                    r.state.outcome() == ApplyOutcome::Deferred && !r.state.text_applied();
                h.execute(&r, &s);
                Ok(deferred
                    && r.state.take_text_applied()
                    && !r.state.take_text_applied()
                    && h.store.full() == "body")
            },
        ),
        (
            "commit session rejects execution after its fixed deadline",
            |h| {
                let deadline = std::time::Instant::now() - std::time::Duration::from_millis(1);
                let (r, s) = h.commit_request_until("old", Some(deadline));
                r.state.requested(0, Some(TF_S_ASYNC.0), TF_S_ASYNC.0);
                h.execute(&r, &s);
                Ok(r.state.stale() && !r.state.text_applied() && h.store.text_writes.get() == 0)
            },
        ),
        ("commit lock rejection does not acknowledge body", |h| {
            let (r, s) = h.commit_request("body");
            h.store.force_lock_rejection(true);
            h.execute(&r, &s);
            let rejected = !r.state.take_text_applied() && h.store.full().is_empty();
            r.state.begin(false);
            h.store.force_lock_rejection(false);
            h.execute(&r, &s);
            Ok(rejected && !r.state.text_applied() && h.store.full().is_empty())
        }),
        ("commit deferred then revoked cannot write", |h| {
            let (r, s) = h.commit_request("old");
            r.state.requested(0, Some(TF_S_ASYNC.0), TF_S_ASYNC.0);
            let deferred =
                r.state.outcome() == ApplyOutcome::Deferred && !r.state.take_text_applied();
            r.state.begin(false);
            h.execute(&r, &s);
            Ok(deferred && r.state.stale() && h.store.text_writes.get() == 0)
        }),
        (
            "commit stale composition cannot overwrite new preedit",
            |h| {
                let (initial, s) = h.request("reading", None);
                h.execute(&initial, &s);
                let (old, delayed) = h.commit_request("old");
                let (new, s) = h.request("new", None);
                h.execute(&new, &s);
                let writes = h.store.text_writes.get();
                h.execute(&old, &delayed);
                Ok(old.state.stale()
                    && !old.state.text_applied()
                    && h.store.full() == "new"
                    && h.store.text_writes.get() == writes)
            },
        ),
        (
            "commit SetText rejection preserves reading for retry",
            |h| {
                let (initial, s) = h.request("reading", None);
                h.execute(&initial, &s);
                let (r, s) = h.commit_request("body");
                h.store.reject_text.set(true);
                h.execute(&r, &s);
                let rejected = !r.state.take_text_applied() && h.store.full() == "reading"
                    && h.deliveries.borrow().is_empty();
                h.store.reject_text.set(false);
                let (retry, s) = h.commit_request("body");
                h.execute(&retry, &s);
                Ok(rejected && retry.state.take_text_applied() && h.store.full() == "body"
                    && *h.deliveries.borrow() == ["body"])
            },
        ),
        (
            "commit body acknowledged once despite selection failure",
            |h| {
                let (initial, s) = h.request("reading", None);
                h.execute(&initial, &s);
                let writes = h.store.text_writes.get();
                let (r, s) = h.commit_request("body");
                h.store.reject_selection.set(true);
                let outcome = h.execute(&r, &s);
                let written = r.state.take_text_applied();
                h.store.reject_selection.set(false);
                h.execute(&r, &s);
                Ok(outcome == ApplyOutcome::TextAppliedRepairPending
                    && written
                    && !r.state.take_text_applied()
                    && h.store.full() == "body"
                    && h.store.text_writes.get() == writes + 1)
            },
        ),
        (
            "commit direct insertion is not repeated by a retained session",
            |h| {
                let (r, s) = h.commit_request("body");
                h.execute(&r, &s);
                let written = r.state.take_text_applied();
                let writes = h.store.text_writes.get();
                h.execute(&r, &s);
                Ok(written
                    && !r.state.take_text_applied()
                    && h.store.full() == "body"
                    && h.store.text_writes.get() == writes
                    && h.store.selection() == (4, 4))
            },
        ),
        (
            "commit direct insertion reports incomplete selection repair",
            |h| {
                let (r, s) = h.commit_request("body");
                h.store.reject_selection.set(true);
                let outcome = h.execute(&r, &s);
                Ok(outcome == ApplyOutcome::TextAppliedRepairPending
                    && r.state.report().body_applied()
                    && !r.state.report().fully_applied()
                    && r.state.take_text_applied()
                    && h.store.full() == "body")
            },
        ),
        #[cfg(feature = "tsf-test-hooks")]
        ("request release can reenter invalidation", |h| {
            let (request, session) = h.request("old", None);
            let apply = h.apply.clone();
            let released = Rc::new(Cell::new(false));
            let callback_released = released.clone();
            *request.on_drop.borrow_mut() = Some(Box::new(move || {
                apply.invalidate();
                callback_released.set(true);
            }));
            drop(session);
            drop(request);
            h.apply.invalidate();
            Ok(released.get())
        }),
        ("short replacement range rejects SetText", |h| {
            let (initial, session) = h.request("abc", None);
            if h.execute(&initial, &session) != ApplyOutcome::Applied {
                return Ok(false);
            }
            let writes = h.store.text_writes.get();
            let state = Rc::new(ApplyState::default());
            let session: ITfEditSession = RangeWrite {
                context: h.context.clone(),
                state: state.clone(),
            }
            .into();
            let result = unsafe {
                h.context.RequestEditSession(
                    h.tid,
                    &session,
                    TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0),
                )
            };
            Ok(!result.is_ok_and(|hr| hr.is_ok())
                && state.outcome() == ApplyOutcome::NotApplied
                && !state.text_applied()
                && h.store.full() == "abc"
                && h.store.text_writes.get() == writes)
        }),
        ("request rejection then retry", |h| {
            let (r, s) = h.request("old", None);
            h.store.force_lock_rejection(true);
            let rejected =
                h.execute(&r, &s) == ApplyOutcome::NotApplied && h.store.full().is_empty();
            h.store.force_lock_rejection(false);
            Ok(rejected && h.execute(&r, &s) == ApplyOutcome::Applied && h.store.full() == "old")
        }),
        ("SetText rejection keeps operation retryable", |h| {
            let (r, s) = h.request("old", None);
            h.store.reject_text.set(true);
            let rejected = h.execute(&r, &s) == ApplyOutcome::NotApplied && !r.state.text_applied();
            h.store.reject_text.set(false);
            Ok(rejected && h.execute(&r, &s) == ApplyOutcome::Applied && h.store.full() == "old")
        }),
        ("post-SetText selection repair writes text once", |h| {
            let (r, s) = h.request("old", None);
            h.store.reject_selection.set(true);
            let pending = h.execute(&r, &s) == ApplyOutcome::TextAppliedRepairPending;
            let written = h.store.text_writes.get();
            let consumed = r.state.take_text_applied();
            h.store.reject_selection.set(false);
            let (retry, s) = h.request("old", None);
            Ok(pending
                && written == 1
                && consumed
                && Rc::ptr_eq(&r, &retry)
                && h.execute(&retry, &s) == ApplyOutcome::Applied
                && h.store.text_writes.get() == written
                && !retry.state.take_text_applied()
                && h.store.selection() == (3, 3))
        }),
        ("equal body changes target and rewrites body for host redraw", |h| {
            let (first, session) = h.request("abcd", Some((0, 2)));
            if h.execute(&first, &session) != ApplyOutcome::Applied { return Ok(false); }
            let writes = h.store.text_writes.get();
            let (next, session) = h.request("abcd", Some((2, 2)));
            h.store.reject_text.set(true);
            let rejected = h.execute(&next, &session) == ApplyOutcome::NotApplied;
            h.store.reject_text.set(false);
            Ok(rejected && h.execute(&next, &session) == ApplyOutcome::Applied
                && h.store.text_writes.get() == writes + 1 && h.store.full() == "abcd")
        }),
        ("equal body moves target and rewrites converted attributes", |h| {
            let converted = vec![(0, 2), (3, 1)];
            let (first, session) = h.decorated_request("あいうえ", Some((0, 2)), converted.clone());
            if h.execute(&first, &session) != ApplyOutcome::Applied
                || h.attribute_values()? != vec![Some(2), Some(2), Some(1), Some(3)] { return Ok(false); }
            let writes = h.store.text_writes.get();
            h.store.reject_text.set(true);
            let (next, session) = h.decorated_request("あいうえ", Some((3, 1)), converted);
            let rejected = h.execute(&next, &session) == ApplyOutcome::NotApplied;
            h.store.reject_text.set(false);
            Ok(rejected && h.execute(&next, &session) == ApplyOutcome::Applied
                && h.store.text_writes.get() == writes + 1
                && h.attribute_values()? == vec![Some(3), Some(3), Some(1), Some(2)])
        }),
        ("retired initial request cannot match restarted display numbering", |h| {
            let (old, stale) = h.request("old", None);
            h.apply.restart_after_end();
            let (new, current) = h.request("new", None);
            Ok(old.identity == new.identity
                && h.execute(&old, &stale) == ApplyOutcome::NotApplied
                && h.store.full().is_empty()
                && h.execute(&new, &current) == ApplyOutcome::Applied
                && h.store.full() == "new")
        }),
        #[cfg(feature = "tsf-test-hooks")]
        ("attribute failure repairs without rewriting", |h| {
            h.faults
                .0
                .set(Some(crate::preedit_session::PreeditFault::Attributes));
            let (r, s) = h.request("abcd", None);
            let pending = h.execute(&r, &s) == ApplyOutcome::TextAppliedRepairPending;
            Ok(pending
                && h.store.full() == "abcd"
                && h.execute(&r, &s) == ApplyOutcome::Applied
                && h.store.text_writes.get() == 1)
        }),
        #[cfg(feature = "tsf-test-hooks")]
        (
            "short ShiftEnd retains text and retries decoration only",
            |h| {
                h.faults
                    .0
                    .set(Some(crate::preedit_session::PreeditFault::ShiftEnd));
                let (r, s) = h.request("abcd", Some((1, 2)));
                let pending = h.execute(&r, &s) == ApplyOutcome::TextAppliedRepairPending;
                Ok(pending
                    && h.execute(&r, &s) == ApplyOutcome::Applied
                    && h.store.text_writes.get() == 1)
            },
        ),
        #[cfg(feature = "tsf-test-hooks")]
        (
            "short ShiftStart retains text and retries decoration only",
            |h| {
                h.faults
                    .0
                    .set(Some(crate::preedit_session::PreeditFault::ShiftStart));
                let (r, s) = h.request("abcd", Some((1, 2)));
                let pending = h.execute(&r, &s) == ApplyOutcome::TextAppliedRepairPending;
                Ok(pending
                    && h.execute(&r, &s) == ApplyOutcome::Applied
                    && h.store.text_writes.get() == 1)
            },
        ),
        (
            "deferred initial execution after termination and new composition",
            |h| {
                let (seed, s) = h.request("old", None);
                if h.execute(&seed, &s) != ApplyOutcome::Applied {
                    return Ok(false);
                }
                let (old, deferred) = h.request("stale", None);
                old.state.requested(0, Some(TF_S_ASYNC.0), TF_S_ASYNC.0);
                if old.state.outcome() != ApplyOutcome::Deferred {
                    return Ok(false);
                }
                h.terminate()?;
                if h.terminations.get() != 1 {
                    return Ok(false);
                }
                let (new, s) = h.request("new", None);
                if h.execute(&new, &s) != ApplyOutcome::Applied {
                    return Ok(false);
                }
                let before = h.store.full();
                let writes = h.store.text_writes.get();
                Ok(h.execute(&old, &deferred) == ApplyOutcome::NotApplied
                    && old.state.stale()
                    && h.execute(&old, &deferred) == ApplyOutcome::NotApplied
                    && h.store.full() == before
                    && h.store.text_writes.get() == writes)
            },
        ),
        (
            "terminated decoration repair cannot touch new composition",
            |h| {
                let (old, s) = h.request("old", None);
                h.store.reject_selection.set(true);
                if h.execute(&old, &s) != ApplyOutcome::TextAppliedRepairPending {
                    return Ok(false);
                }
                h.store.reject_selection.set(false);
                h.terminate()?;
                let (new, next) = h.request("new", None);
                if h.execute(&new, &next) != ApplyOutcome::Applied {
                    return Ok(false);
                }
                let before = (
                    h.store.full(),
                    h.store.selection(),
                    h.store.text_writes.get(),
                );
                h.execute(&old, &s);
                Ok(old.state.stale()
                    && before
                        == (
                            h.store.full(),
                            h.store.selection(),
                            h.store.text_writes.get(),
                        ))
            },
        ),
        ("focus loss invalidates a deferred first composition", |h| {
            let (r, s) = h.request("stale", None);
            r.state.requested(0, Some(TF_S_ASYNC.0), TF_S_ASYNC.0);
            h.apply.retain_context(None);
            Ok(h.execute(&r, &s) == ApplyOutcome::NotApplied
                && h.store.full().is_empty()
                && h.composition.borrow().is_none())
        }),
        ("deferred completion records text exactly once", |h| {
            let (r, s) = h.request("later", None);
            r.state.requested(0, Some(TF_S_ASYNC.0), TF_S_ASYNC.0);
            let waiting =
                r.state.outcome() == ApplyOutcome::Deferred && !r.state.take_text_applied();
            let complete =
                h.execute(&r, &s) == ApplyOutcome::Applied && r.state.take_text_applied();
            Ok(waiting
                && complete
                && h.execute(&r, &s) == ApplyOutcome::Applied
                && !r.state.take_text_applied()
                && h.store.text_writes.get() == 1)
        }),
        ("superseded repair cannot restore old selection", |h| {
            let (old, s) = h.request("old", None);
            h.store.reject_selection.set(true);
            if h.execute(&old, &s) != ApplyOutcome::TextAppliedRepairPending {
                return Ok(false);
            }
            h.store.reject_selection.set(false);
            let (new, next) = h.request("newer", Some((1, 2)));
            if h.execute(&new, &next) != ApplyOutcome::Applied {
                return Ok(false);
            }
            let before = (
                h.store.full(),
                h.store.selection(),
                h.store.text_writes.get(),
            );
            h.execute(&old, &s);
            Ok(old.state.stale()
                && before
                    == (
                        h.store.full(),
                        h.store.selection(),
                        h.store.text_writes.get(),
                    ))
        }),
        ("invalid target rejected before SetText", |h| {
            let (r, s) = h.request("😀", Some((1, 1)));
            Ok(h.execute(&r, &s) == ApplyOutcome::NotApplied && h.store.text_writes.get() == 0)
        }),
        ("engine response delay and reversed edit delivery", |h| {
            let mut replies = ResponseDelivery::new();
            replies.hold(
                1,
                100,
                ipc::protocol::Response::Reading {
                    reading: "old".into(),
                },
            );
            replies.hold(
                2,
                10,
                ipc::protocol::Response::Reading {
                    reading: "new".into(),
                },
            );
            if replies.release(1, 10).is_some() {
                return Ok(false);
            }
            let (old, delayed) = h.request("old", None);
            old.state.requested(0, Some(TF_S_ASYNC.0), TF_S_ASYNC.0);
            let Some(ipc::protocol::Response::Reading { reading }) = replies.release(2, 10) else {
                return Ok(false);
            };
            let (new, s) = h.request(&reading, None);
            if h.execute(&new, &s) != ApplyOutcome::Applied {
                return Ok(false);
            }
            if replies.release(1, 100).is_none() {
                return Ok(false);
            }
            h.execute(&old, &delayed);
            Ok(h.store.full() == "new" && old.state.stale() && h.store.text_writes.get() == 1)
        }),
    ];
    let reentrant = Harness::new().map(|h| {
        let apply = h.apply.clone();
        *h.store.on_selection.borrow_mut() = Some(Box::new(move || apply.invalidate()));
        let (r, s) = h.request("body", None);
        h.execute(&r, &s) == ApplyOutcome::TextAppliedRepairPending
            && r.state.report().text_applied
            && !r.state.report().body_applied()
    });
    let mut failed = !matches!(reentrant, Ok(true));
    println!(
        "apply-faults reentrant selection invalidation : {}",
        if failed { "FAIL" } else { "PASS" }
    );
    for (name, case) in cases {
        match Harness::new().and_then(|h| case(&h)) {
            Ok(true) => println!("apply-faults {name} : PASS"),
            Ok(false) => {
                println!("apply-faults {name} : FAIL");
                failed = true;
            }
            Err(error) => {
                println!("apply-faults {name} : ERROR ({error})");
                failed = true;
            }
        }
    }
    i32::from(failed)
}
