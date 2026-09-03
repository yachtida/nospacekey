//! COM サーバ登録（HKLM\SOFTWARE\Classes\CLSID\...\InprocServer32）と
//! TSF プロファイル/カテゴリ登録。
//! DllRegisterServer / DllInstall / DllUnregisterServer から呼ばれる。

use crate::globals::{CLSID_NOSPACEKEY, LANGID_JA, PROFILE_NOSPACEKEY};
use crate::text_service::tip_log;
use windows::core::{Error, Result, GUID};
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, E_FAIL, E_INVALIDARG,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Input::KeyboardAndMouse::HKL;
use windows::Win32::UI::TextServices::{
    CLSID_TF_CategoryMgr,
    CLSID_TF_InputProcessorProfiles,
    ITfCategoryMgr,
    ITfInputProcessorProfileMgr,
    GUID_TFCAT_DISPLAYATTRIBUTEPROVIDER,
    // COM-less 活性化＋シェル統合系（Start/タスクバー検索 = AppContainer/LPAC ホスト対応）。
    GUID_TFCAT_TIPCAP_COMLESS,
    GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
    GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT,
    GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
    GUID_TFCAT_TIPCAP_UIELEMENTENABLED,
    GUID_TFCAT_TIP_KEYBOARD,
};

// この DLL の RT_GROUP_ICON ID は 1 始まりだが、TSF の uIconIndex はファイル内の 0 始まり位置。
// 0 始まり位置の解釈は MS ドキュメントに明記がなく、Win10/11 入力インジケーターの実測と
// register.rs の抽出テスト(profile_icon_index_is_bound_to_the_last_icon_group)で固定している。
const PROFILE_ICON_INDEX: u32 = (crate::langbar_icon::RES_PROFILE_N - 1) as u32;

/// GUID をレジストリ正規形 `{8-4-4-4-12}`（大文字）にして返す。
/// windows-rs の Debug 表記に依存せず、登録キーのパスを安定させる。
fn guid_braced(g: &GUID) -> String {
    let d4 = g.data4;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1, g.data2, g.data3, d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7],
    )
}

fn registry_not_found(error: &Error) -> bool {
    error.code() == ERROR_FILE_NOT_FOUND.to_hresult()
        || error.code() == ERROR_PATH_NOT_FOUND.to_hresult()
}

fn remove_registry_tree(root: &windows_registry::Key, path: &str, label: &str) {
    if let Err(error) = root.remove_tree(path) {
        if !registry_not_found(&error) {
            tip_log(&format!("ev=unregister_remove label={label} err={error:?}"));
        }
    }
}

fn registry_tree_absent(root: &windows_registry::Key, path: &str, label: &str) -> bool {
    match root.open(path) {
        Ok(_) => {
            tip_log(&format!("ev=unregister_residual label={label} path={path}"));
            false
        }
        Err(error) if registry_not_found(&error) => true,
        Err(error) => {
            tip_log(&format!("ev=unregister_verify label={label} err={error:?}"));
            false
        }
    }
}

fn registration_categories() -> [GUID; 7] {
    [
        GUID_TFCAT_TIP_KEYBOARD,
        GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
        GUID_TFCAT_TIPCAP_UIELEMENTENABLED,
        GUID_TFCAT_DISPLAYATTRIBUTEPROVIDER,
        GUID_TFCAT_TIPCAP_COMLESS,
        GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
        GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT,
    ]
}

fn registration_transaction_with<E>(
    target: &str,
    mut register_profile: impl FnMut(&str) -> std::result::Result<(), E>,
    mut register_category: impl FnMut(&GUID) -> std::result::Result<(), E>,
    mut commit_inproc: impl FnMut(&str) -> std::result::Result<(), E>,
) -> std::result::Result<(), E> {
    register_profile(target)?;
    for category in registration_categories() {
        register_category(&category)?;
    }
    commit_inproc(target)
}

fn valid_install_root(root: &str) -> bool {
    let bytes = root.as_bytes();
    bytes.len() >= 4
        && bytes.len() <= 1024
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2] == b'\\'
        && !root.ends_with('\\')
        && !root.contains('/')
        && !root[2..].contains(':')
        && root.split('\\').skip(1).all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.ends_with('.')
                && !part.ends_with(' ')
        })
}

fn restore_target_for_root_with(
    target: &str,
    install_root: &str,
    file_exists: impl FnOnce(&std::path::Path) -> bool,
) -> Option<String> {
    if !valid_install_root(install_root)
        || !valid_install_root(target.rsplit_once('\\')?.0)
        || target.contains('/')
        || target[2..].contains(':')
    {
        return None;
    }
    let legacy = format!(r"{install_root}\nospacekey_tip.dll");
    let versioned_prefix = format!(r"{install_root}\versions\");
    let suffix = r"\nospacekey_tip.dll";
    let version_end = target.len().checked_sub(suffix.len());
    let versioned_match = version_end.is_some_and(|end| {
        target
            .get(..versioned_prefix.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&versioned_prefix))
            && target
                .get(end..)
                .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
            && target
                .get(versioned_prefix.len()..end)
                .is_some_and(|version| semver::Version::parse(version).is_ok())
    });
    let structural_match = target.eq_ignore_ascii_case(&legacy) || versioned_match;
    (structural_match && file_exists(std::path::Path::new(target))).then(|| target.to_string())
}

fn decode_restore_command(command: &str) -> Option<String> {
    let hex = command.strip_prefix("restore-utf16hex=")?;
    if hex.is_empty() || hex.len() % 4 != 0 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let units = hex
        .as_bytes()
        .chunks_exact(4)
        .map(|chunk| {
            std::str::from_utf8(chunk)
                .ok()
                .and_then(|digits| u16::from_str_radix(digits, 16).ok())
        })
        .collect::<Option<Vec<_>>>()?;
    if units.contains(&0) {
        return None;
    }
    String::from_utf16(&units).ok()
}

pub(crate) fn validated_restore_target(command: &str) -> Result<String> {
    let target = decode_restore_command(command).ok_or(E_INVALIDARG)?;
    let marker = windows_registry::LOCAL_MACHINE
        .open("SOFTWARE\\nospacekey\\InstallerTransaction")?
        .get_string("PriorInstallRoot")?;
    restore_target_for_root_with(&target, &marker, std::path::Path::is_file)
        .ok_or_else(|| E_INVALIDARG.into())
}

pub(crate) fn register_for_target(dll_path: &str) -> Result<()> {
    let clsid = guid_braced(&CLSID_NOSPACEKEY);
    let user_key_path = format!("Software\\Classes\\CLSID\\{}", clsid);
    // HKCR is a merged HKLM/HKCU view and routes value writes to a pre-existing
    // per-user key.  This TIP is machine-wide, so refuse a legacy/current-user
    // overlay before mutating machine state rather than registering a DLL in an
    // unexpected hive.
    if !registry_tree_absent(windows_registry::CURRENT_USER, &user_key_path, "user_clsid") {
        return Err(E_FAIL.into());
    }
    unsafe {
        // --- TSF プロファイル登録 ---
        let profiles: ITfInputProcessorProfileMgr =
            CoCreateInstance(&CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER)?;
        let desc: Vec<u16> = "nospacekey".encode_utf16().collect();
        // プロファイル固定アイコンは DLL に埋め込み、配布物へ .ico を別添しない。
        let iconfile: Vec<u16> = dll_path.encode_utf16().collect();
        let cat: ITfCategoryMgr =
            CoCreateInstance(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)?;
        let key_path = format!("SOFTWARE\\Classes\\CLSID\\{}\\InprocServer32", clsid);
        registration_transaction_with(
            dll_path,
            |_| {
                profiles.RegisterProfile(
                    &CLSID_NOSPACEKEY,
                    LANGID_JA,
                    &PROFILE_NOSPACEKEY,
                    &desc,
                    &iconfile,
                    PROFILE_ICON_INDEX,
                    HKL::default(),
                    0,
                    true,
                    0,
                )
            },
            |category| cat.RegisterCategory(&CLSID_NOSPACEKEY, category, &CLSID_NOSPACEKEY),
            |target| {
                let key = windows_registry::LOCAL_MACHINE.create(&key_path)?;
                key.set_string("ThreadingModel", "Apartment")?;
                // The active COM target is the transaction commit marker and must be last.
                key.set_string("", target)
            },
        )?;
    }
    Ok(())
}

pub fn register() -> Result<()> {
    // A truncated module path would commit a COM target Windows cannot load.
    let dll_path = crate::globals::module_file_path().ok_or(E_FAIL)?;
    register_for_target(&dll_path)
}

pub fn unregister() -> Result<()> {
    unsafe {
        // プロファイル解除（失敗しても続行してレジストリは掃除する）。
        match CoCreateInstance::<_, ITfInputProcessorProfileMgr>(
            &CLSID_TF_InputProcessorProfiles,
            None,
            CLSCTX_INPROC_SERVER,
        ) {
            Ok(profiles) => {
                if let Err(error) =
                    profiles.UnregisterProfile(&CLSID_NOSPACEKEY, LANGID_JA, &PROFILE_NOSPACEKEY, 0)
                {
                    tip_log(&format!("ev=unregister_profile err={error:?}"));
                }
            }
            Err(error) => tip_log(&format!("ev=unregister_profile_mgr err={error:?}")),
        }
        // カテゴリ解除（register() の RegisterCategory と対）。これを怠ると
        // HKLM\SOFTWARE\Microsoft\CTF\TIP\{CLSID}\Category が残り、TIP 登録が生き続ける。
        match CoCreateInstance::<_, ITfCategoryMgr>(
            &CLSID_TF_CategoryMgr,
            None,
            CLSCTX_INPROC_SERVER,
        ) {
            Ok(cat) => {
                for c in [
                    GUID_TFCAT_TIP_KEYBOARD,
                    GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
                    GUID_TFCAT_TIPCAP_UIELEMENTENABLED,
                    GUID_TFCAT_DISPLAYATTRIBUTEPROVIDER, // register() と対称に解除する
                    GUID_TFCAT_TIPCAP_COMLESS,
                    GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
                    GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT,
                ] {
                    if let Err(error) =
                        cat.UnregisterCategory(&CLSID_NOSPACEKEY, &c, &CLSID_NOSPACEKEY)
                    {
                        tip_log(&format!("ev=unregister_category cat={c:?} err={error:?}"));
                    }
                }
            }
            Err(error) => tip_log(&format!("ev=unregister_category_mgr err={error:?}")),
        }
    }
    // CLSID ツリーごと削除。失敗（NotFound 以外）は壊れた半登録が残る兆候なのでログに残す。
    let clsid = guid_braced(&CLSID_NOSPACEKEY);
    let key_path = format!("SOFTWARE\\Classes\\CLSID\\{}", clsid);
    remove_registry_tree(windows_registry::LOCAL_MACHINE, &key_path, "clsid");
    // Clean the exact per-user tree that older HKCR-based versions may have
    // created.  Leaving it would override the new machine-wide COM mapping.
    let user_key_path = format!("Software\\Classes\\CLSID\\{}", clsid);
    remove_registry_tree(windows_registry::CURRENT_USER, &user_key_path, "user_clsid");
    // フォールバック: TSF API の解除（UnregisterProfile/UnregisterCategory）が HRESULT 失敗を
    // 返してもキーが残らないよう、TSF TIP 登録ツリーを直接削除する（LanguageProfile/Category 込み）。
    // 64bit ビューと WOW6432Node ビューの両方を掃除する（regsvr32 /u 後に nospacekey が
    // 壊れた IME として一覧へ残り、ウィンドウに居座る不具合の根本対策）。
    let tip = clsid;
    let tip_path = format!("SOFTWARE\\Microsoft\\CTF\\TIP\\{}", tip);
    let wow_tip_path = format!("SOFTWARE\\WOW6432Node\\Microsoft\\CTF\\TIP\\{}", tip);
    remove_registry_tree(windows_registry::LOCAL_MACHINE, &tip_path, "tip64");
    remove_registry_tree(windows_registry::LOCAL_MACHINE, &wow_tip_path, "tip32");

    // TSF APIs can report an error for a partially registered profile even when
    // the direct fallback removed every durable key. The postcondition, rather
    // than an ignored HRESULT, decides success. Any residual or unverifiable
    // tree makes DllUnregisterServer fail so the supervisor retains its marker.
    if registry_tree_absent(windows_registry::LOCAL_MACHINE, &key_path, "clsid")
        && registry_tree_absent(windows_registry::CURRENT_USER, &user_key_path, "user_clsid")
        && registry_tree_absent(windows_registry::LOCAL_MACHINE, &tip_path, "tip64")
        && registry_tree_absent(windows_registry::LOCAL_MACHINE, &wow_tip_path, "tip32")
    {
        Ok(())
    } else {
        Err(E_FAIL.into())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode_restore_command, registration_transaction_with, restore_target_for_root_with,
        PROFILE_ICON_INDEX,
    };
    use std::cell::{Cell, RefCell};

    #[cfg(windows)]
    #[test]
    fn registered_profile_icon_index_resolves_in_built_module() {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::S_OK;
        use windows::Win32::UI::Shell::SHDefExtractIconW;
        use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

        // 検証対象はテスト exe 自身。cargo の契約上、build.rs の winresource は
        // cdylib とテスト exe に同一 .res を埋めるため現行ビルドのリソースを代表する。
        // cdylib を優先探索すると cargo test がビルドしない前提で target に残った
        // **古い** cdylib を掴む危険があり、実配布 DLL(Program Files)の検証は
        // リリース工程の職域なので単体テストでは束縛しない。
        let icon_file = std::env::current_exe().unwrap();
        let icon_file = icon_file
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut large_icon = HICON::default();
        let mut small_icon = HICON::default();
        let result = unsafe {
            SHDefExtractIconW(
                PCWSTR(icon_file.as_ptr()),
                PROFILE_ICON_INDEX as i32,
                0,
                Some(&mut large_icon),
                Some(&mut small_icon),
                (16 << 16) | 32,
            )
        };
        let extracted = !large_icon.is_invalid() && !small_icon.is_invalid();
        unsafe {
            if !large_icon.is_invalid() {
                DestroyIcon(large_icon).unwrap();
            }
            if !small_icon.is_invalid() {
                DestroyIcon(small_icon).unwrap();
            }
        }

        assert_eq!(result, S_OK, "registered profile icon must exist");
        assert!(extracted, "registered profile icon must return HICONs");
    }

    #[cfg(windows)]
    fn module_icon_extractable(index: i32) -> bool {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::S_OK;
        use windows::Win32::UI::Shell::SHDefExtractIconW;
        use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

        let icon_file = std::env::current_exe().unwrap();
        let icon_file = icon_file
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut large_icon = HICON::default();
        let mut small_icon = HICON::default();
        // nIconSize は LOWORD=large・HIWORD=small(MSDN)。(16 << 16) | 32 は large=32px・small=16px。
        let result = unsafe {
            SHDefExtractIconW(
                PCWSTR(icon_file.as_ptr()),
                index,
                0,
                Some(&mut large_icon),
                Some(&mut small_icon),
                (16 << 16) | 32,
            )
        };
        let extracted = result == S_OK && !large_icon.is_invalid() && !small_icon.is_invalid();
        unsafe {
            if !large_icon.is_invalid() {
                let _ = DestroyIcon(large_icon);
            }
            if !small_icon.is_invalid() {
                let _ = DestroyIcon(small_icon);
            }
        }
        extracted
    }

    #[cfg(windows)]
    #[test]
    fn profile_icon_index_is_bound_to_the_last_icon_group() {
        // 位置 = ID - 1 の不変条件の網: 末尾の1つ先の位置は解決不可(=最終位置は N-1)、
        // 負数インデックス(リソースID参照)で ID N が存在する。リソースの**追加・欠落**は
        // 検出する。同一個数のまま ID と .ico の対応を入れ替えても位置には別のアイコンが
        // 入るだけで全て緑のまま(内容同一性は検査しない)。register_for_target が定数を
        // 使うこと自体もこのテストの守備範囲外。
        assert!(module_icon_extractable(PROFILE_ICON_INDEX as i32));
        assert!(
            !module_icon_extractable(crate::langbar_icon::RES_PROFILE_N as i32),
            "one past the last icon must not resolve"
        );
        assert!(
            module_icon_extractable(-(crate::langbar_icon::RES_PROFILE_N as i32)),
            "resource ID must exist; negative index resolves by resource ID"
        );
    }

    #[test]
    fn active_com_target_is_committed_after_profile_and_all_categories() {
        let events = RefCell::new(Vec::new());
        registration_transaction_with::<()>(
            "trusted-target",
            |target| {
                assert_eq!(target, "trusted-target");
                events.borrow_mut().push("profile");
                Ok(())
            },
            |_| {
                events.borrow_mut().push("category");
                Ok(())
            },
            |target| {
                assert_eq!(target, "trusted-target");
                events.borrow_mut().push("inproc");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            events.into_inner(),
            [
                "profile", "category", "category", "category", "category", "category", "category",
                "category", "inproc"
            ]
        );
    }

    #[test]
    fn active_com_target_is_not_changed_when_any_precommit_step_fails() {
        for fail_at in 0..=7 {
            let step = Cell::new(0);
            let active_target = RefCell::new("old-target");
            let result = registration_transaction_with(
                "trusted-target",
                |_| {
                    if fail_at == 0 {
                        Err("injected registration failure")
                    } else {
                        step.set(1);
                        Ok(())
                    }
                },
                |_| {
                    let current = step.get();
                    if current == fail_at {
                        Err("injected registration failure")
                    } else {
                        step.set(current + 1);
                        Ok(())
                    }
                },
                |_| {
                    *active_target.borrow_mut() = "trusted-target";
                    Ok(())
                },
            );

            assert!(result.is_err(), "step {fail_at} must fail");
            assert_eq!(*active_target.borrow(), "old-target");
        }
    }

    #[test]
    fn commit_failure_is_returned_after_every_registration_step() {
        let steps = Cell::new(0);
        let active_target = RefCell::new("old-target");
        let result = registration_transaction_with(
            "trusted-target",
            |_| {
                steps.set(1);
                Ok(())
            },
            |_| {
                steps.set(steps.get() + 1);
                Ok(())
            },
            |_| Err("injected commit failure"),
        );

        assert_eq!(result, Err("injected commit failure"));
        assert_eq!(steps.get(), 8);
        assert_eq!(*active_target.borrow(), "old-target");
    }

    #[test]
    fn restore_command_decodes_utf16_hex_without_command_line_metacharacters() {
        assert_eq!(
            decode_restore_command(
                "restore-utf16hex=0043003A005C006F006C0064005C006E006F00730070006100630065006B00650079005F007400690070002E0064006C006C"
            ),
            Some(r"C:\old\nospacekey_tip.dll".to_string())
        );
        for invalid in [
            "restore-utf16hex=",
            "restore-utf16hex=004",
            "restore-utf16hex=0022&calc",
            "other=0043",
            "restore-utf16hex=D800",
        ] {
            assert_eq!(decode_restore_command(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn restore_target_accepts_only_exact_legacy_or_versioned_product_dll() {
        let root = r"D:\custom nospacekey";
        for accepted in [
            r"D:\custom nospacekey\nospacekey_tip.dll",
            r"D:\custom nospacekey\versions\1.2.3\nospacekey_tip.dll",
            r"D:\custom nospacekey\versions\1.2.3-beta.4+sha.abc\nospacekey_tip.dll",
        ] {
            assert_eq!(
                restore_target_for_root_with(accepted, root, |_| true),
                Some(accepted.to_string()),
                "{accepted}"
            );
        }
        for rejected in [
            r"D:\custom nospacekey\other.dll",
            r"D:\custom nospacekey\backup\nospacekey_tip.dll",
            r"D:\custom nospacekey\versions\v1.2.3\nospacekey_tip.dll",
            r"D:\custom nospacekey\versions\1.2.3\sub\nospacekey_tip.dll",
            r"D:\custom nospacekey\versions\1.2.3\nospacekey_tip.dll:evil",
            r"D:\foreign\nospacekey_tip.dll",
            r"D:\custom nospacekey\nospacekey_tip.dll\child",
        ] {
            assert_eq!(
                restore_target_for_root_with(rejected, root, |_| true),
                None,
                "{rejected}"
            );
        }
        assert_eq!(
            restore_target_for_root_with(r"D:\custom nospacekey\nospacekey_tip.dll", root, |_| {
                false
            }),
            None
        );
        for unsafe_root in [
            r"relative\nospacekey",
            r"D:\custom nospacekey:stream",
            r"D:\custom nospacekey\..\other",
            r"D:\custom nospacekey. ",
        ] {
            assert_eq!(
                restore_target_for_root_with(
                    r"D:\custom nospacekey\nospacekey_tip.dll",
                    unsafe_root,
                    |_| true,
                ),
                None,
                "{unsafe_root}"
            );
        }
    }
}
