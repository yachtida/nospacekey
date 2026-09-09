//! Owns preedit requests across COM callouts; no RefCell borrow crosses TSF.
use crate::apply_state::{ApplyIdentity, ApplyOutcome, ApplyState};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use windows::core::{IUnknown, Interface, HSTRING};
use windows::Win32::UI::TextServices::{ITfComposition, ITfContext};

fn same<T: Interface>(a: &T, b: &T) -> bool {
    match (a.cast::<IUnknown>(), b.cast::<IUnknown>()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub(crate) struct PreeditRequest {
    pub identity: ApplyIdentity,
    pub context: ITfContext,
    pub text: HSTRING,
    pub target: Option<(usize, usize)>,
    pub caret: Option<ipc::clause::DisplayUtf16Position>,
    pub converted: Vec<(usize, usize)>,
    pub composition: RefCell<Option<ITfComposition>>,
    pub state: Rc<ApplyState>,
    #[cfg(feature = "tsf-test-hooks")]
    pub on_drop: RefCell<Option<Box<dyn FnOnce()>>>,
}

#[cfg(feature = "tsf-test-hooks")]
impl Drop for PreeditRequest {
    fn drop(&mut self) {
        if let Some(callback) = self.on_drop.get_mut().take() {
            callback();
        }
    }
}

#[derive(Default)]
pub(crate) struct PreeditApply {
    serial: Cell<u64>,
    current: RefCell<Option<Rc<PreeditRequest>>>,
}

impl PreeditApply {
    fn next(&self) -> u64 {
        // MAX is reserved for ending the composition. Retrying that frozen
        // commit may reuse its revision; ordinary display requests stop earlier.
        let next = self.serial.get().saturating_add(1);
        self.serial.set(next);
        next
    }

    pub fn invalidate(&self) {
        let retired = self.current.borrow_mut().take();
        // Releasing the last request can call back through its COM references.
        drop(retired);
    }

    pub fn needs_end(&self) -> bool { self.serial.get() >= u64::MAX - 1 }

    /// Called only after the owned composition and its pending close have ended.
    pub fn restart_after_end(&self) {
        let retired = self.current.borrow_mut().take();
        self.serial.set(0);
        // Publish the restarted counter before releasing COM references that
        // may synchronously issue another display request.
        drop(retired);
    }

    #[cfg(feature = "tsf-test-hooks")]
    pub fn force_near_capacity(&self) { self.serial.set(u64::MAX - 1); }

    pub fn retain_context(&self, context: Option<&ITfContext>) {
        let current = self.current.borrow().clone();
        if current
            .as_ref()
            .is_some_and(|r| !context.is_some_and(|c| same(&r.context, c)))
        {
            self.invalidate();
        }
    }

    pub fn request(
        &self,
        context: &ITfContext,
        composition: Option<ITfComposition>,
        text: &str,
        target: Option<(usize, usize)>,
    ) -> Rc<PreeditRequest> {
        self.request_with_caret(context, composition, text, target, None)
    }

    pub fn request_with_caret(
        &self,
        context: &ITfContext,
        composition: Option<ITfComposition>,
        text: &str,
        target: Option<(usize, usize)>,
        caret: Option<ipc::clause::DisplayUtf16Position>,
    ) -> Rc<PreeditRequest> {
        self.request_display(context, composition, text, target, caret, Vec::new())
    }

    pub fn request_display(
        &self,
        context: &ITfContext,
        composition: Option<ITfComposition>,
        text: &str,
        target: Option<(usize, usize)>,
        caret: Option<ipc::clause::DisplayUtf16Position>,
        converted: Vec<(usize, usize)>,
    ) -> Rc<PreeditRequest> {
        let old = self.current.borrow().clone();
        let context_matches = old.as_ref().is_some_and(|old| same(&old.context, context));
        let composition_matches = old.as_ref().is_some_and(|old| {
            let expected = old.composition.borrow().clone();
            match (expected.as_ref(), composition.as_ref()) {
                (Some(a), Some(b)) => same(a, b),
                (None, None) => true,
                _ => false,
            }
        });
        if let Some(old) = old.as_ref() {
            if context_matches
                && composition_matches
                && old.text == text
                && old.target == target
                && old.caret == caret
                && old.converted == converted
                && !old.state.stale()
                && old.state.outcome() != ApplyOutcome::Applied
                && self.is_current(old, &composition)
            {
                return Rc::clone(old);
            }
        }
        let revision = self.next();
        let request = Rc::new(PreeditRequest {
            identity: ApplyIdentity {
                context_token: if context_matches {
                    old.as_ref().unwrap().identity.context_token
                } else {
                    revision
                },
                composition_token: if context_matches && composition_matches {
                    old.as_ref().unwrap().identity.composition_token
                } else {
                    revision
                },
                display_revision: revision,
                operation_id: revision,
            },
            context: context.clone(),
            text: HSTRING::from(text),
            target,
            caret,
            converted,
            composition: RefCell::new(composition),
            state: Rc::new(ApplyState::default()),
            #[cfg(feature = "tsf-test-hooks")]
            on_drop: RefCell::new(None),
        });
        let retired = self.current.borrow_mut().replace(Rc::clone(&request));
        drop(retired);
        request
    }

    pub fn is_current(
        &self,
        request: &PreeditRequest,
        composition: &Option<ITfComposition>,
    ) -> bool {
        let expected = request.composition.borrow().clone();
        let composition_matches = match (expected.as_ref(), composition.as_ref()) {
            (Some(a), Some(b)) => same(a, b),
            (None, None) => true,
            _ => false,
        };
        // QueryInterface is a COM callout too. Check the generation after it,
        // with no shared borrow held across the identity comparison.
        composition_matches
            && self
                .current
                .borrow()
                .as_ref()
                .is_some_and(|current| std::ptr::eq(current.as_ref(), request) && current.identity == request.identity)
    }
}
