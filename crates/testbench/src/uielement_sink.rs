//! testbench: TIP の UIElement advertise を観測する sink。pbShow を制御してイマーシブ/デスクトップを模擬。
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use windows::core::{implement, BOOL, Result};
use windows::Win32::UI::TextServices::{ITfUIElementSink, ITfUIElementSink_Impl};

#[derive(Default)]
pub struct SinkLog {
    pub begun: RefCell<Vec<u32>>,
    pub updated: RefCell<Vec<u32>>,
    pub ended: RefCell<Vec<u32>>,
    /// Some(false)=イマーシブ模擬(ホストが描く) / Some(true)=デスクトップ / None=既定(変更しない)。
    pub force_pbshow: Cell<Option<bool>>,
}

#[implement(ITfUIElementSink)]
pub struct UiElementSink {
    pub log: Rc<SinkLog>,
}

impl ITfUIElementSink_Impl for UiElementSink_Impl {
    fn BeginUIElement(&self, id: u32, pbshow: *mut BOOL) -> Result<()> {
        self.log.begun.borrow_mut().push(id);
        if let Some(v) = self.log.force_pbshow.get() {
            unsafe { if !pbshow.is_null() { *pbshow = BOOL::from(v); } }
        }
        Ok(())
    }
    fn UpdateUIElement(&self, id: u32) -> Result<()> { self.log.updated.borrow_mut().push(id); Ok(()) }
    fn EndUIElement(&self, id: u32) -> Result<()> { self.log.ended.borrow_mut().push(id); Ok(()) }
}
