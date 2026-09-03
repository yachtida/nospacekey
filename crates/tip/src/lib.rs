//! nospacekey_tip.dll — TSF Text Input Processor の COM エントリポイント。
//! PART 1: COM スケルトン + TSF 登録 + 純粋な入力状態機械。

mod background_input;
mod candidate_presenter;
mod candidate_state;
mod candidate_uielement;
mod candidate_window;
mod class_factory;
mod config_launch;
mod conversion_mode;
mod display_attribute;
mod edit_session;
pub(crate) mod engine_link;
mod focus;
mod globals;
#[allow(dead_code)]
pub(crate) mod input_module;
mod input_state;
// Task 4 wires the model-free state/worker to the runtime IPC.
mod key_event_sink;
mod keymap;
mod langbar;
mod langbar_icon;
mod llm_worker;
pub mod local_kana_composer;
mod mode_hud;
mod popup;
mod power;
#[allow(dead_code)]
mod prediction_state;
#[allow(dead_code)]
mod prediction_worker;
mod reading_monitor;
mod register;
mod render;
mod text_service;
mod theme;

use class_factory::ClassFactory;
use globals::{set_hinst, CLSID_NOSPACEKEY, DLL_REF};
use std::ffi::c_void;
use std::sync::atomic::Ordering;
use windows::core::{IUnknown, Interface, BOOL, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, E_FAIL, HMODULE, S_FALSE, S_OK, TRUE};
use windows::Win32::System::LibraryLoader::DisableThreadLibraryCalls;

#[no_mangle]
extern "system" fn DllMain(inst: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == 1 {
        if !unsafe { nospacekey_lifetime::dll_process_attach(inst) } {
            return BOOL(0);
        }
        // DLL_PROCESS_ATTACH: モジュールハンドルを AtomicPtr 経由で保存する（static mut は使わない）。
        set_hinst(inst);
        // 以後 DllMain がスレッド毎（ATTACH/DETACH）に再入するのを止める。現状の分岐では無害だが、
        // 不要な再入を断って脆弱性を減らす。失敗は致命的でないので結果は捨てる。
        unsafe {
            let _ = DisableThreadLibraryCalls(inst);
        }
    } else if reason == 0 {
        unsafe { nospacekey_lifetime::dll_process_detach() };
    }
    TRUE
}

#[no_mangle]
extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    // 登録済み CLSID 以外への要求は提供しない（COM 規約: CLASS_E_CLASSNOTAVAILABLE）。
    // 実運用ではレジストリ経由で自分の CLSID しか来ないが、規約どおり防御する。
    if rclsid.is_null() || unsafe { *rclsid } != CLSID_NOSPACEKEY {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IUnknown = ClassFactory::new().into();
    unsafe { factory.query(riid, ppv) }
}

#[no_mangle]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    if DLL_REF.load(Ordering::SeqCst) <= 0 {
        S_OK
    } else {
        S_FALSE
    }
}

fn registration_hresult<E>(
    operation: impl FnOnce() -> Result<(), E>,
    report_failure: impl FnOnce(&E),
) -> HRESULT {
    match operation() {
        Ok(()) => S_OK,
        Err(error) => {
            report_failure(&error);
            E_FAIL
        }
    }
}

#[no_mangle]
extern "system" fn DllRegisterServer() -> HRESULT {
    // The installer owns rollback. This wrapper deliberately has no cleanup capability because
    // a shared CLSID may still point to a retained working version.
    registration_hresult(register::register, |error| {
        text_service::tip_log(&format!("ev=register_failed err={error:?}"));
    })
}

#[no_mangle]
extern "system" fn DllInstall(install: BOOL, command_line: PCWSTR) -> HRESULT {
    if !install.as_bool() || command_line.is_null() {
        return E_FAIL;
    }
    let command = match unsafe { command_line.to_string() } {
        Ok(command) => command,
        Err(_) => return E_FAIL,
    };
    let target = match register::validated_restore_target(&command) {
        Ok(target) => target,
        Err(error) => {
            text_service::tip_log(&format!("ev=restore_validation_failed err={error:?}"));
            return E_FAIL;
        }
    };
    registration_hresult(
        || register::register_for_target(&target),
        |error| {
            text_service::tip_log(&format!("ev=restore_register_failed err={error:?}"));
        },
    )
}

#[no_mangle]
extern "system" fn DllUnregisterServer() -> HRESULT {
    match register::unregister() {
        Ok(()) => S_OK,
        Err(_) => E_FAIL,
    }
}

#[cfg(test)]
mod registration_entrypoint_tests {
    use super::registration_hresult;
    use std::cell::RefCell;
    use windows::Win32::Foundation::{E_FAIL, S_OK};

    #[test]
    fn registration_hresult_runs_only_operation_on_success() {
        let events = RefCell::new(Vec::new());
        let result = registration_hresult::<()>(
            || {
                events.borrow_mut().push("operation");
                Ok(())
            },
            |_| events.borrow_mut().push("report"),
        );
        assert_eq!(result, S_OK);
        assert_eq!(events.into_inner(), ["operation"]);
    }

    #[test]
    fn registration_hresult_reports_once_after_failure_without_cleanup_capability() {
        let events = RefCell::new(Vec::new());
        let result = registration_hresult(
            || {
                events.borrow_mut().push("operation");
                Err("injected failure")
            },
            |error| {
                assert_eq!(*error, "injected failure");
                events.borrow_mut().push("report");
            },
        );
        assert_eq!(result, E_FAIL);
        assert_eq!(events.into_inner(), ["operation", "report"]);
    }
}
