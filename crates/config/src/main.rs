#![windows_subsystem = "windows"]
//! NospacekeyConfig.exe — nospacekey の設定 GUI（Tauri v2 / WebView2）。
//!
//! TIP の `ITfFnConfigure::Show`（親 HWND を argv[1] で受けるが parse-and-ignore）と
//! トレイメニューの 2 経路から起動される兄弟プロセス。settings.json の読み書きは
//! `crates/settings` 経由（スキーマはそちらが契約）。

mod commands;
mod download;
mod logic;

use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

/// Tauri（=WebView2）初期化に失敗したときの最後の砦。WebView2 ランタイム不在の
/// Win10 素環境などでは UI を出せないため、Win32 MessageBox で案内して終了する。
fn fatal_dialog(text: &str) {
    let msg: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let title: Vec<u16> = "nospacekey 設定".encode_utf16().chain([0]).collect();
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(msg.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn main() {
    // アンインストーラ/更新からの graceful 停止経路。Tauri/WebView2 を一切起動せず、
    // 常駐エンジンへ Shutdown を送って終了する（新規インストール時も無害＝エンジン不在で code 0）。
    // argv[1] の parse-and-ignore より前に判定する（--stop-engine は HWND として parse されないが明示）。
    if std::env::args().any(|a| a == "--stop-engine") {
        std::process::exit(commands::stop_engine());
    }

    // argv[1] = 親 HWND（isize 文字列）。v1 同様 parse-and-ignore。
    let _parent_hwnd: Option<isize> = std::env::args().nth(1).and_then(|a| a.parse().ok());

    let result = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::apply_settings,
            commands::get_default_settings,
            commands::get_app_info,
            commands::open_settings_dir,
            commands::open_releases_page,
            commands::open_external_url,
            commands::clear_learning_history,
            download::zenzai_model_status,
            download::download_zenzai_model,
            download::cancel_zenzai_download,
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        fatal_dialog(&format!(
            "設定画面を起動できませんでした。\nMicrosoft Edge WebView2 ランタイムが必要です\n（Windows 11 には標準搭載。Windows 10 では Microsoft のサイトから入手できます）。\n\n詳細: {e}"
        ));
    }
}
