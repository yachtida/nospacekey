//! COM サーバ登録（HKLM\SOFTWARE\Classes\CLSID\...\InprocServer32）と
//! TSF プロファイル/カテゴリ登録。
//! DllRegisterServer / DllUnregisterServer から呼ばれる。

use crate::globals::{CLSID_NOSPACEKEY, LANGID_JA, PROFILE_NOSPACEKEY};
use crate::text_service::tip_log;
use windows::core::{Error, Result, GUID};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, E_FAIL};
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

pub fn register() -> Result<()> {
    // --- InprocServer32: 既定値に DLL のフルパス、ThreadingModel=Apartment ---
    // パス取得は切り詰め検出つきヘルパを使う（固定 260 だと長いパスで不正な値を登録し、
    // COM がサーバをロードできない壊れた IME になる）。取得不能なら登録を中止する。
    let dll_path = crate::globals::module_file_path().ok_or(E_FAIL)?;
    let clsid = guid_braced(&CLSID_NOSPACEKEY);
    let user_key_path = format!("Software\\Classes\\CLSID\\{}", clsid);
    // HKCR is a merged HKLM/HKCU view and routes value writes to a pre-existing
    // per-user key.  This TIP is machine-wide, so refuse a legacy/current-user
    // overlay before mutating machine state rather than registering a DLL in an
    // unexpected hive.
    if !registry_tree_absent(windows_registry::CURRENT_USER, &user_key_path, "user_clsid") {
        return Err(E_FAIL.into());
    }
    let key_path = format!("SOFTWARE\\Classes\\CLSID\\{}\\InprocServer32", clsid);
    let k = windows_registry::LOCAL_MACHINE.create(&key_path)?;
    k.set_string("", &dll_path)?;
    k.set_string("ThreadingModel", "Apartment")?;

    unsafe {
        // --- TSF プロファイル登録 ---
        let profiles: ITfInputProcessorProfileMgr =
            CoCreateInstance(&CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER)?;
        let desc: Vec<u16> = "nospacekey".encode_utf16().collect();
        // プロファイル固定アイコン (Gapless N 案) は DLL 内 RT_GROUP_ICON リソース ID=7。
        // iconfile に DLL 自身のパスを渡すと iconIndex はリソースIDとして解釈される
        // （MS-IME 等の DLL 内蔵アイコンと同じ形式。配布物に .ico を同梱せず済む）。
        let iconfile: Vec<u16> = dll_path.encode_utf16().collect();
        profiles.RegisterProfile(
            &CLSID_NOSPACEKEY,
            LANGID_JA,
            &PROFILE_NOSPACEKEY,
            &desc,
            &iconfile,
            crate::langbar_icon::RES_PROFILE_N as u32,
            HKL::default(),
            0,
            true,
            0,
        )?;

        // --- カテゴリ登録（キーボード TIP + イマーシブ対応 + UIElement 対応）---
        let cat: ITfCategoryMgr =
            CoCreateInstance(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)?;
        for c in [
            GUID_TFCAT_TIP_KEYBOARD,
            GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
            // SP6a: UIElement 対応を宣言（実体は CandidateListUIElement）
            GUID_TFCAT_TIPCAP_UIELEMENTENABLED,
            // 表示属性プロバイダ（ITfDisplayAttributeProvider）として自分を宣言する。
            // 実行時に RegisterGUID(GUID_DISPLAY_ATTRIBUTE) する一方でこのカテゴリを
            // 登録していないと、ホストによっては preedit の下線属性が適用されない。
            GUID_TFCAT_DISPLAYATTRIBUTEPROVIDER,
            // --- シェル検索面（Start/タスクバー検索 = TextInputHost の AppContainer/LPAC）対応 ---
            // COM-less 活性化を宣言。これが無いと CTF は AppContainer ホスト内で
            // 通常の COM(InprocServer32) 活性化ができず TIP をインスタンス化できない
            // → 既定 MS-IME へフォールバック固定（実機で確認された症状の主因候補）。
            // Mozc/Google日本語入力・MS SampleIME はいずれも COMLESS を登録している。
            GUID_TFCAT_TIPCAP_COMLESS,
            // システムトレイ(Input Indicator)互換／入力モードコンパートメント対応の宣言。
            // シェル側が hiragana/英数 をクエリ・トグルできるようにし、第二級IME扱いを避ける。
            GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
            GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT,
        ] {
            cat.RegisterCategory(&CLSID_NOSPACEKEY, &c, &CLSID_NOSPACEKEY)?;
        }
    }
    Ok(())
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
