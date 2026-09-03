#![windows_subsystem = "windows"]
//! NospacekeyConfig.exe — nospacekey の設定 GUI（Tauri v2 / WebView2）。
//!
//! TIP の `ITfFnConfigure::Show`（親 HWND を argv[1] で受けるが parse-and-ignore）と
//! トレイメニューの 2 経路から起動される兄弟プロセス。settings.json の読み書きは
//! `crates/settings` 経由（スキーマはそちらが契約）。

mod activation;
mod commands;
mod download;
mod logic;
mod prediction_download;
mod update;

use tauri::{Emitter, Manager};
use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

const CONFIG_INSTANCE_LOCK_ERROR: &str = "NOSPACEKEY_CONFIG_INSTANCE_LOCK";

/// `tauri-plugin-single-instance` の Windows mutex は既定 namespace（logon session 単位）。
/// 同じユーザーが console/RDP 等の別 session で Config を開くと、プロセス内の
/// SettingsLock/DictLock/DOWNLOADING を迂回して共有 `%LOCALAPPDATA%` を競合更新できる。
/// user profile 内の lock file をプロセス寿命中 byte-range lock し、全 session で1個にする。
struct ConfigInstanceLease {
    file: Option<std::fs::File>,
}

impl ConfigInstanceLease {
    fn acquire() -> std::io::Result<Self> {
        let path = settings::settings_path()
            .map(|p| p.with_file_name("config-instance.lock"))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{CONFIG_INSTANCE_LOCK_ERROR}: LOCALAPPDATA が解決できません"),
                )
            })?;
        Self::acquire_at(&path)
    }

    fn acquire_at(path: &std::path::Path) -> std::io::Result<Self> {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{
            LockFileEx, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
            LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
        };
        use windows::Win32::System::IO::OVERLAPPED;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // OPEN_REPARSE_POINT で既存 link 自体を開き、user-controlled link を別実体の
        // 長期ロックへ化けさせない。通常ファイルは残置してよく、process crash でも OS が
        // byte-range lock を自動解放するため stale marker 判定は不要。
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // Rust/Windows の既定は FILE_SHARE_DELETE を含み、保有中でも rename →
            // 同名再作成で別 file object の byte lock を取れてしまう。lock file を外部共有
            // する用途はないため全共有を拒否し、path と保持中 object の結合も維持する。
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)?;
        if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(std::io::Error::other(format!(
                "{CONFIG_INSTANCE_LOCK_ERROR}: 排他ファイルが再解析ポイントです"
            )));
        }

        let mut overlapped = OVERLAPPED::default();
        unsafe {
            LockFileEx(
                HANDLE(file.as_raw_handle() as _),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                None,
                1,
                0,
                &mut overlapped,
            )
        }
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("{CONFIG_INSTANCE_LOCK_ERROR}: {e}"),
            )
        })?;
        Ok(Self { file: Some(file) })
    }
}

impl Drop for ConfigInstanceLease {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::UnlockFileEx;
        use windows::Win32::System::IO::OVERLAPPED;

        let Some(file) = self.file.as_ref() else {
            return;
        };
        let mut overlapped = OVERLAPPED::default();
        unsafe {
            let _ = UnlockFileEx(
                HANDLE(file.as_raw_handle() as _),
                None,
                1,
                0,
                &mut overlapped,
            );
        }
    }
}

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
    let _version_lease = match nospacekey_lifetime::VersionLease::acquire() {
        Ok(lease) => lease,
        Err(_) => std::process::exit(70),
    };
    let launch_intent = activation::parse_args(std::env::args().skip(1));
    // アンインストーラ/更新からの graceful 停止経路。Tauri/WebView2 を一切起動せず、
    // 常駐エンジンへ Shutdown を送って終了する（新規インストール時も無害＝エンジン不在で code 0）。
    // argv[1] の parse-and-ignore より前に判定する（--stop-engine は HWND として parse されないが明示）。
    if launch_intent == activation::LaunchIntent::StopEngine {
        std::process::exit(commands::stop_engine());
    }
    if launch_intent == activation::LaunchIntent::RepairUpdateTask {
        std::process::exit(commands::repair_update_task_after_install());
    }

    // argv[1] = 親 HWND（isize 文字列）。v1 同様 parse-and-ignore。
    let _parent_hwnd: Option<isize> = std::env::args().nth(1).and_then(|a| a.parse().ok());

    let result = tauri::Builder::default()
        .manage(activation::PendingIntent::new(
            launch_intent == activation::LaunchIntent::OpenUpdate,
        ))
        // 巡3 Q2: 最初の plugin として登録（初期化の早い段階で既存インスタンスへ差し替える）。
        // 2 起動目以降は新規ウィンドウを作らず既存へフォーカスを寄せて終わる — 複数プロセスが
        // プロセス内排他（SettingsLock/DictLock/UI ビジーフラグ）を回避して settings.json・
        // 辞書を相互上書きするのを構造的に防ぐ。ウィンドウ label は tauri.conf.json 既定の "main"。
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if activation::parse_args(args).eq(&activation::LaunchIntent::OpenUpdate) {
                app.state::<activation::PendingIntent>().set();
                let _ = app.emit("open-update", ());
            }
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::AutomaticCheckReconcileState::default())
        // Startup reconciliation may persist a safe OFF fallback. Manage the
        // same settings lock before setup so the background worker cannot race
        // a later command or another in-process settings mutation.
        .manage(logic::SettingsLock(std::sync::Mutex::new(())))
        // plugin の session-local 重複転送が先に初期化された後で取得する。通常の同一 session
        // 二重起動は既存窓へフォーカスされ、別 session（または同時起動 race）だけがここで止まる。
        .setup(|app| {
            let lease = ConfigInstanceLease::acquire()?;
            app.manage(lease);
            // 設定と per-user task を起動時に照合する。外部コマンドと state
            // lock を含むため、setup/main event loop は待たず worker へ渡す。
            // ON 失敗時は OFF 保存に成功したときだけ OFF へ収束し、保存失敗時は
            // persisted ON を正直に表示する。
            commands::spawn_reconcile_worker(app.handle().clone());
            Ok(())
        })
        .manage(logic::DictLock(std::sync::Mutex::new(())))
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::acknowledge_corrupt_recovery_notices,
            commands::apply_settings,
            commands::dismiss_automatic_check_prompt,
            commands::consume_update_intent,
            commands::get_default_settings,
            commands::get_symbol_catalog,
            commands::get_app_info,
            commands::open_settings_dir,
            commands::open_releases_page,
            commands::open_external_url,
            commands::clear_learning_history,
            commands::zenzai_runtime_status,
            commands::retry_zenzai,
            download::zenzai_model_status,
            download::download_zenzai_model,
            download::cancel_zenzai_download,
            prediction_download::prediction_model_status,
            prediction_download::download_prediction_model,
            prediction_download::cancel_prediction_model_download,
            update::check_for_update,
            update::download_and_install_update,
            update::cancel_update_download,
            commands::dict_list,
            commands::dict_add,
            commands::dict_update,
            commands::dict_delete,
            commands::dict_import,
            commands::dict_export,
            commands::dict_sync_engine,
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        let details = e.to_string();
        if details.contains(CONFIG_INSTANCE_LOCK_ERROR) {
            fatal_dialog(&format!(
                "設定画面は別のログオンセッションですでに開かれているか、安全な排他状態を確認できません。\n開いている設定画面を閉じてから再試行してください。\n\n詳細: {details}"
            ));
        } else {
            fatal_dialog(&format!(
                "設定画面を起動できませんでした。\nMicrosoft Edge WebView2 ランタイムが必要です\n（Windows 11 には標準搭載。Windows 10 では Microsoft のサイトから入手できます）。\n\n詳細: {details}"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ConfigInstanceLease;

    #[test]
    fn config_instance_lease_excludes_other_handles_and_releases_on_drop() {
        let path = std::env::temp_dir().join(format!(
            "nospacekey-config-instance-{}-{}.lock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = ConfigInstanceLease::acquire_at(&path).expect("first lease");
        assert!(ConfigInstanceLease::acquire_at(&path).is_err());
        let moved = path.with_extension("moved");
        assert!(std::fs::rename(&path, &moved).is_err());
        assert!(path.exists());
        assert!(!moved.exists());
        drop(first);
        // lease 解放後は通常の file lifecycle に戻り、rename も再取得も可能。
        std::fs::rename(&path, &moved).expect("rename after lease drop");
        std::fs::rename(&moved, &path).expect("restore lock path");
        let second = ConfigInstanceLease::acquire_at(&path).expect("lease after restore");
        drop(second);
        let _ = std::fs::remove_file(path);
    }
}
