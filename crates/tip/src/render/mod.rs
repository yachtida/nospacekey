//! A 段の描画基盤。`SurfaceRenderer`（DComp+D2D）と、GDI/DComp 両方が使う DWM chrome ヘルパ。
//!
//! DWM chrome（角丸・システムバックドロップ）はウィンドウ生成直後に一度適用する。Win10 や
//! 未対応環境では `DwmSetWindowAttribute` が失敗するが、すべて握り潰して不透明・角なしへ
//! degrade する（TIP パスでは決して panic しない）。

mod renderer; // Task 4 以降で中身を実装。
pub use renderer::{is_device_lost, SurfaceRenderer};

use std::ffi::c_void;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMSBT_NONE, DWMSBT_TRANSIENTWINDOW, DWMWCP_DEFAULT, DWMWCP_ROUND,
};

/// 角丸・アクリルバックドロップを適用する。失敗は握り潰す（Win10=no-op）。
/// `rounded=false` は明示的に角なし、`acrylic=false` はバックドロップ無効（不透明）。
pub fn apply_dwm_chrome(hwnd: HWND, rounded: bool, acrylic: bool) {
    unsafe {
        let corner = if rounded { DWMWCP_ROUND } else { DWMWCP_DEFAULT };
        let corner_val = corner.0 as u32;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner_val as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
        let backdrop = if acrylic { DWMSBT_TRANSIENTWINDOW } else { DWMSBT_NONE };
        let backdrop_val = backdrop.0 as u32;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop_val as *const _ as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
    }
}
