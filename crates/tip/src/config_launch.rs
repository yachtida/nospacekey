//! 設定アプリ（NospacekeyConfig.exe）の起動。ロード済みの旧 TIP ではなく、HKLM に
//! 現在登録されている TIP と同じディレクトリの Config を選ぶ。
//!
//! なぜ切り出すか: パス組み立て（`config_exe_path`）は COM/OS 非依存の純関数として
//! 単体テストで固定し、CreateProcessW を叩く `launch_config_app` は build-only に分離して、
//! テスト可能な部分の回帰を機械的に守るため。

use std::path::{Path, PathBuf};

/// DLL のフルパスから、同じディレクトリの NospacekeyConfig.exe パスを組み立てる。
/// 親ディレクトリが取れない/空（ベース名のみ等）場合は None
/// （空 parent への join はカレントディレクトリ依存になるため拒否する）。
fn config_exe_path(dll_path: &str) -> Option<PathBuf> {
    let dll = Path::new(dll_path);
    if !dll.is_absolute()
        || !dll
            .file_name()?
            .to_str()?
            .eq_ignore_ascii_case("nospacekey_tip.dll")
    {
        return None;
    }
    let parent = dll.parent()?;
    // なぜ空 parent を弾くか: Path::new("nospacekey_tip.dll").parent() は Some("") を返す仕様で、
    // 空ディレクトリへの join はカレントディレクトリ依存の相対パスになり、意図しない場所の
    // exe を起動しかねない。ディレクトリが確定しないなら起動そのものを諦める。
    if parent.as_os_str().is_empty() {
        return None;
    }
    Some(parent.join("NospacekeyConfig.exe"))
}

fn active_config_exe_path_with(
    read_active_tip: impl FnOnce() -> Option<String>,
    mut file_exists: impl FnMut(&Path) -> bool,
) -> Option<PathBuf> {
    let tip = read_active_tip()?;
    let config = config_exe_path(&tip)?;
    if !file_exists(Path::new(&tip)) {
        return None;
    }
    file_exists(&config).then_some(config)
}

/// 現在登録されている TIP と同じ版の設定アプリを解決する。登録不在・不正パス・
/// Config 欠落では旧 TIP の兄弟へ戻らず、設定スキーマの古い書き手を起動しない。
pub(crate) fn active_config_exe_path() -> Option<PathBuf> {
    active_config_exe_path_with(crate::register::active_tip_path, Path::is_file)
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

    #[test]
    fn settings_open_uses_the_config_beside_the_registered_tip() {
        let registered_tip = std::path::PathBuf::from(
            r"C:\Program Files\nospacekey\versions\2.0.0\nospacekey_tip.dll",
        );
        let expected = std::path::PathBuf::from(
            r"C:\Program Files\nospacekey\versions\2.0.0\NospacekeyConfig.exe",
        );
        let actual = active_config_exe_path_with(
            || Some(registered_tip.to_string_lossy().into_owned()),
            |path| path == registered_tip || path == expected,
        );
        assert_eq!(actual, Some(expected));
    }

    #[test]
    fn settings_open_fails_closed_without_a_valid_registered_tip() {
        assert_eq!(
            active_config_exe_path_with(|| None, |_| true),
            None,
            "the loaded old TIP must not become an implicit fallback"
        );
        for invalid in [
            r"C:\Program Files\nospacekey\versions\2.0.0\other.dll",
            r"versions\2.0.0\nospacekey_tip.dll",
        ] {
            assert_eq!(
                active_config_exe_path_with(|| Some(invalid.to_string()), |_| true),
                None,
                "invalid active registration must not select a Config: {invalid}"
            );
        }
        assert_eq!(
            active_config_exe_path_with(
                || {
                    Some(
                        r"C:\Program Files\nospacekey\versions\2.0.0\nospacekey_tip.dll"
                            .to_string(),
                    )
                },
                |_| false,
            ),
            None,
            "a committed TIP without its Config must fail closed"
        );

        let registered_tip = r"C:\Program Files\nospacekey\versions\2.0.0\nospacekey_tip.dll";
        let config = std::path::PathBuf::from(
            r"C:\Program Files\nospacekey\versions\2.0.0\NospacekeyConfig.exe",
        );
        assert_eq!(
            active_config_exe_path_with(|| Some(registered_tip.to_string()), |path| path == config,),
            None,
            "a missing registered TIP must not be treated as an active installation"
        );
    }
}
