//! Validate the complete replacement range before any text mutation.
use crate::apply_state::exact_shift;
use windows::core::Result;
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::UI::TextServices::ITfRange;

pub(crate) unsafe fn shift_start_exact(range: &ITfRange, ec: u32, count: i32) -> Result<()> {
    let mut moved = 0;
    range.ShiftStart(ec, count, &mut moved, core::ptr::null())?;
    if !exact_shift(count, moved) {
        return Err(E_FAIL.into());
    }
    Ok(())
}
