//! COM サーバ登録（HKCR\CLSID\...\InprocServer32）と TSF プロファイル/カテゴリ登録。
//! DllRegisterServer / DllUnregisterServer から呼ばれる。

use windows::core::{Result, GUID};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::UI::Input::KeyboardAndMouse::HKL;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::TextServices::{
    ITfInputProcessorProfileMgr, ITfCategoryMgr,
    CLSID_TF_InputProcessorProfiles, CLSID_TF_CategoryMgr,
    GUID_TFCAT_DISPLAYATTRIBUTEPROVIDER,
    GUID_TFCAT_TIP_KEYBOARD, GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
    GUID_TFCAT_TIPCAP_UIELEMENTENABLED,
    // COM-less 活性化＋シェル統合系（Start/タスクバー検索 = AppContainer/LPAC ホスト対応）。
    GUID_TFCAT_TIPCAP_COMLESS, GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
    GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT,
};
use crate::globals::{CLSID_NOSPACEKEY, PROFILE_NOSPACEKEY, LANGID_JA};
use crate::text_service::tip_log;

/// GUID をレジストリ正規形 `{8-4-4-4-12}`（大文字）にして返す。
/// windows-rs の Debug 表記に依存せず、登録キーのパスを安定させる。
fn guid_braced(g: &GUID) -> String {
    let d4 = g.data4;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1, g.data2, g.data3,
        d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7],
    )
}

pub fn register() -> Result<()> {
    // --- InprocServer32: 既定値に DLL のフルパス、ThreadingModel=Apartment ---
    // パス取得は切り詰め検出つきヘルパを使う（固定 260 だと長いパスで不正な値を登録し、
    // COM がサーバをロードできない壊れた IME になる）。取得不能なら登録を中止する。
    let dll_path = crate::globals::module_file_path().ok_or(E_FAIL)?;
    let key_path = format!("CLSID\\{}\\InprocServer32", guid_braced(&CLSID_NOSPACEKEY));
    let k = windows_registry::CLASSES_ROOT.create(&key_path)?;
    k.set_string("", &dll_path)?;
    k.set_string("ThreadingModel", "Apartment")?;

    unsafe {
        // --- TSF プロファイル登録 ---
        let profiles: ITfInputProcessorProfileMgr =
            CoCreateInstance(&CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER)?;
        let desc: Vec<u16> = "nospacekey".encode_utf16().collect();
        let iconfile: Vec<u16> = Vec::new();
        profiles.RegisterProfile(
            &CLSID_NOSPACEKEY, LANGID_JA, &PROFILE_NOSPACEKEY,
            &desc, &iconfile, 0, HKL::default(), 0, true, 0,
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
        if let Ok(profiles) = CoCreateInstance::<_, ITfInputProcessorProfileMgr>(
            &CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER,
        ) {
            let _ = profiles.UnregisterProfile(&CLSID_NOSPACEKEY, LANGID_JA, &PROFILE_NOSPACEKEY, 0);
        }
        // カテゴリ解除（register() の RegisterCategory と対）。これを怠ると
        // HKLM\SOFTWARE\Microsoft\CTF\TIP\{CLSID}\Category が残り、TIP 登録が生き続ける。
        if let Ok(cat) = CoCreateInstance::<_, ITfCategoryMgr>(
            &CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER,
        ) {
            for c in [
                GUID_TFCAT_TIP_KEYBOARD,
                GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
                GUID_TFCAT_TIPCAP_UIELEMENTENABLED,
                GUID_TFCAT_DISPLAYATTRIBUTEPROVIDER, // register() と対称に解除する
                GUID_TFCAT_TIPCAP_COMLESS,
                GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
                GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT,
            ] {
                let _ = cat.UnregisterCategory(&CLSID_NOSPACEKEY, &c, &CLSID_NOSPACEKEY);
            }
        }
    }
    // CLSID ツリーごと削除。失敗（NotFound 以外）は壊れた半登録が残る兆候なのでログに残す。
    let key_path = format!("CLSID\\{}", guid_braced(&CLSID_NOSPACEKEY));
    if let Err(e) = windows_registry::CLASSES_ROOT.remove_tree(&key_path) {
        tip_log(&format!("ev=unregister_clsid_remove err={e:?}"));
    }
    // フォールバック: TSF API の解除（UnregisterProfile/UnregisterCategory）が HRESULT 失敗を
    // 返してもキーが残らないよう、TSF TIP 登録ツリーを直接削除する（LanguageProfile/Category 込み）。
    // 64bit ビューと WOW6432Node ビューの両方を掃除する（regsvr32 /u 後に nospacekey が
    // 壊れた IME として一覧へ残り、ウィンドウに居座る不具合の根本対策）。
    let tip = guid_braced(&CLSID_NOSPACEKEY);
    let _ = windows_registry::LOCAL_MACHINE
        .remove_tree(&format!("SOFTWARE\\Microsoft\\CTF\\TIP\\{}", tip));
    let _ = windows_registry::LOCAL_MACHINE
        .remove_tree(&format!("SOFTWARE\\WOW6432Node\\Microsoft\\CTF\\TIP\\{}", tip));
    Ok(())
}
