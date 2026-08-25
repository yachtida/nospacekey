//! nospacekey_tip.dll — TSF Text Input Processor の COM エントリポイント。
//! PART 1: COM スケルトン + TSF 登録 + 純粋な入力状態機械。

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
mod input_state;
// Task 4 wires the model-free state/worker to the runtime IPC.
mod key_event_sink;
mod keymap;
mod langbar;
mod langbar_icon;
mod llm_worker;
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
use windows::core::{IUnknown, Interface, BOOL, GUID, HRESULT};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, E_FAIL, HMODULE, S_FALSE, S_OK, TRUE};
use windows::Win32::System::LibraryLoader::DisableThreadLibraryCalls;

#[no_mangle]
extern "system" fn DllMain(inst: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == 1 {
        // DLL_PROCESS_ATTACH: モジュールハンドルを AtomicPtr 経由で保存する（static mut は使わない）。
        set_hinst(inst);
        // 以後 DllMain がスレッド毎（ATTACH/DETACH）に再入するのを止める。現状の分岐では無害だが、
        // 不要な再入を断って脆弱性を減らす。失敗は致命的でないので結果は捨てる。
        unsafe {
            let _ = DisableThreadLibraryCalls(inst);
        }
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

#[no_mangle]
extern "system" fn DllRegisterServer() -> HRESULT {
    match register::register() {
        Ok(()) => S_OK,
        Err(e) => {
            // 途中失敗で InprocServer32 / プロファイルが半端に残ると nospacekey が「壊れた IME」
            // として一覧に居座る。unregister() で逆順ロールバックし、両者の結果をログに残して
            // 実機での「出るが使えない」状態を診断可能にする。
            text_service::tip_log(&format!("ev=register_failed err={e:?}"));
            match register::unregister() {
                Ok(()) => text_service::tip_log("ev=register_rolled_back"),
                Err(ue) => {
                    text_service::tip_log(&format!("ev=register_rollback_failed err={ue:?}"))
                }
            }
            E_FAIL
        }
    }
}

#[no_mangle]
extern "system" fn DllUnregisterServer() -> HRESULT {
    match register::unregister() {
        Ok(()) => S_OK,
        Err(_) => E_FAIL,
    }
}
