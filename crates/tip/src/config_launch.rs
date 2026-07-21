//! 設定アプリ（NospacekeyConfig.exe）の起動。DLL と同じディレクトリに配置される前提
//! （インストーラ配置構成に依存。crates/config の [[bin]] name と一致させる）。
//!
//! なぜ切り出すか: パス組み立て（`config_exe_path`）は COM/OS 非依存の純関数として
//! 単体テストで固定し、CreateProcessW を叩く `launch_config_app` は build-only に分離して、
//! テスト可能な部分の回帰を機械的に守るため。

use std::path::{Path, PathBuf};

/// DLL のフルパスから、同じディレクトリの NospacekeyConfig.exe パスを組み立てる。
/// 親ディレクトリが取れない/空（ベース名のみ等）場合は None
/// （空 parent への join はカレントディレクトリ依存になるため拒否する）。
pub(crate) fn config_exe_path(dll_path: &str) -> Option<PathBuf> {
    let parent = Path::new(dll_path).parent()?;
    // なぜ空 parent を弾くか: Path::new("nospacekey_tip.dll").parent() は Some("") を返す仕様で、
    // 空ディレクトリへの join はカレントディレクトリ依存の相対パスになり、意図しない場所の
    // exe を起動しかねない。ディレクトリが確定しないなら起動そのものを諦める。
    if parent.as_os_str().is_empty() {
        return None;
    }
    Some(parent.join("NospacekeyConfig.exe"))
}

/// 設定アプリを起動する。COM/OS 依存につき単体テスト不可（Task 5 の実機確認で検証）。
/// 失敗（exe が見つからない等）は false を返すのみで panic しない。
///
/// # Safety
/// CreateProcessW を呼ぶ FFI。`exe_path` は呼び出し側が用意した有効なパスであること。
/// cmdline バッファは NUL 終端付きで CreateProcessW に可変ポインタとして渡す（API 仕様）。
pub(crate) unsafe fn launch_config_app(exe_path: &Path) -> bool {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        CreateProcessW, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
    };

    // なぜ引用符で囲むか: パスに空白（"C:\Program Files\..."）が含まれても1引数として
    // 解釈させるため。末尾 NUL は CreateProcessW の lpCommandLine が要求する。
    let mut cmdline: Vec<u16> = format!("\"{}\"", exe_path.display())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let si = STARTUPINFOW {
        cb: core::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    let ok = unsafe {
        CreateProcessW(
            None,
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            PROCESS_CREATION_FLAGS(0),
            None,
            None,
            &si,
            &mut pi,
        )
    }
    .is_ok();
    if ok {
        // 起動できたら子プロセスのハンドルは不要（起動しっぱなしで待たない）。
        // なぜ閉じるか: 閉じないとハンドルリークになるため。失敗は無視（既に無効なら害なし）。
        unsafe {
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(pi.hThread);
        }
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_exe_path_joins_sibling_exe() {
        let p = config_exe_path(r"C:\Program Files\nospacekey\nospacekey_tip.dll").unwrap();
        assert_eq!(
            p,
            std::path::PathBuf::from(r"C:\Program Files\nospacekey\NospacekeyConfig.exe")
        );
    }

    #[test]
    fn config_exe_path_none_when_no_directory() {
        // Path::new("nospacekey_tip.dll").parent() は Some("") を返す（Rust の仕様）。
        // 空ディレクトリへの join はカレントディレクトリ依存の相対パスになり危険なので、
        // 実装側で空 parent を None 扱いにする。
        assert!(config_exe_path("nospacekey_tip.dll").is_none());
    }
}
