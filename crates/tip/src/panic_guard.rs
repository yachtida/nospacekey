//! A Rust panic must be caught before returning through a COM vtable thunk.
use windows::core::Result;
use windows::Win32::Foundation::E_FAIL;

pub(crate) fn com(site: &str, body: impl FnOnce() -> Result<()>) -> Result<()> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(payload) => {
            // An arbitrary panic payload may itself have a panicking destructor.
            std::mem::forget(payload);
            crate::text_service::tip_log(&format!("ev=panic site={site}"));
            Err(E_FAIL.into())
        }
    }
}
