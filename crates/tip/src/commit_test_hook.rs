//! One-shot RequestEditSession deferral for the installed-TIP acceptance gate.
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use windows::core::{implement, Result};
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG};
use windows::Win32::UI::TextServices::{ITfContext, ITfEditSession, ITfEditSession_Impl, TF_CONTEXT_EDIT_CONTEXT_FLAGS, TF_ES_READWRITE, TF_ES_SYNC};

#[derive(Default)]
struct DeferredCommit {
    armed: bool,
    exhaust_clause: bool,
    exhaust_display: bool,
    held: Option<(ITfContext, u32, ITfEditSession)>,
    executions: Rc<Cell<u32>>,
}

#[implement(ITfEditSession)]
struct ObservedSession {
    inner: ITfEditSession,
    executions: Rc<Cell<u32>>,
}
impl ITfEditSession_Impl for ObservedSession_Impl {
    fn DoEditSession(&self, cookie: u32) -> Result<()> {
        self.executions.set(self.executions.get().saturating_add(1));
        unsafe { self.inner.DoEditSession(cookie) }
    }
}
thread_local! {
    static COMMIT: RefCell<DeferredCommit> = RefCell::new(DeferredCommit::default());
}

pub(crate) fn hold(context: &ITfContext, tid: u32, session: &ITfEditSession) -> bool {
    COMMIT.with(|slot| {
        let mut state = slot.borrow_mut();
        if !state.armed || state.held.is_some() { return false; }
        state.armed = false;
        state.held = Some((context.clone(), tid, session.clone()));
        true
    })
}

pub(crate) fn take_clause_exhaustion() -> bool {
    COMMIT.with(|slot| std::mem::take(&mut slot.borrow_mut().exhaust_clause))
}
pub(crate) fn take_display_exhaustion() -> bool {
    COMMIT.with(|slot| std::mem::take(&mut slot.borrow_mut().exhaust_display))
}

// Testbench calls on the same STA: reset, arm, inspect, release, execution count.
#[no_mangle]
extern "system" fn NospacekeyTestDeferredCommit(command: u32) -> i32 {
    std::panic::catch_unwind(|| match command {
        0 => {
            let old = COMMIT.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
            drop(old);
            0
        }
        1 => COMMIT.with(|slot| {
            let mut state = slot.borrow_mut();
            if state.armed || state.held.is_some() { return E_FAIL.0; }
            state.armed = true;
            state.executions = Rc::new(Cell::new(0));
            0
        }),
        2 => COMMIT.with(|slot| i32::from(slot.borrow().held.is_some())),
        3 => {
            let (held, executions) = COMMIT.with(|slot| {
                let mut state = slot.borrow_mut();
                (state.held.take(), state.executions.clone())
            });
            let Some((context, tid, session)) = held else { return E_FAIL.0; };
            let observed: ITfEditSession = ObservedSession { inner: session, executions }.into();
            // No TLS borrow survives the TSF callout or release of COM references.
            unsafe { context.RequestEditSession(tid, &observed,
                TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READWRITE.0)) }
                .unwrap_or_else(|error| error.code()).0
        }
        4 => COMMIT.with(|slot| slot.borrow().executions.get().min(i32::MAX as u32) as i32),
        5 => COMMIT.with(|slot| { slot.borrow_mut().exhaust_clause = true; 0 }),
        6 => COMMIT.with(|slot| { slot.borrow_mut().exhaust_display = true; 0 }),
        _ => E_INVALIDARG.0,
    }).unwrap_or(E_FAIL.0)
}
