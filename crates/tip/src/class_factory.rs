//! IClassFactory 実装。COM ランタイムが DllGetClassObject 経由で取得し、
//! CreateInstance で TextService を生成して要求 IID にキャストして返す。

use std::ffi::c_void;
use windows::core::{implement, IUnknown, IUnknownImpl, Interface, Ref, Result, GUID, BOOL};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl, CoLockObjectExternal};
use crate::text_service::TextService;
use crate::globals::ComObjectGuard;

#[implement(IClassFactory)]
pub struct ClassFactory {
    // C-1: 全 #[implement] COM オブジェクトの生存数を DLL_REF で数える。ファクトリが
    // 生きている間は DllCanUnloadNow が S_OK を返さず、DLL のアンロードを防ぐ。
    _guard: ComObjectGuard,
}

impl ClassFactory {
    pub fn new() -> Self {
        Self { _guard: ComObjectGuard::new() }
    }
}

impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(&self, _outer: Ref<'_, IUnknown>, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        // TextService を作り IUnknown として保持してから要求 IID へ query する。
        let unknown: IUnknown = TextService::new().into();
        unsafe { unknown.query(riid, ppv).ok() }
    }

    fn LockServer(&self, flock: BOOL) -> Result<()> {
        // ファクトリ自身（IUnknown）の外部ロック数を増減させ、DLL の常駐を保つ。
        let unknown: IUnknown = self.to_interface();
        unsafe { CoLockObjectExternal(&unknown, flock.as_bool(), true) }
    }
}
