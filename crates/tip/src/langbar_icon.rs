//! タスクバー入力インジケータ（langbar `GUID_LBI_INPUTMODE`）に出す HICON の生成。
//! B段までは Yu Gothic UI のテキスト描画（DrawTextW）で A/あ/あ˙ をラスタライズして
//! いたが、2026-08 アイコン改稿で **Pure Glyph 案**（design/icons/ 採択素材）の
//! 事前ラスタライズ ICO へ切り替えた。ICO は scripts/gen-ime-icons.ps1 が素材 PNG から
//! 生成し、build.rs が DLL リソース（RT_GROUP_ICON ID 1..7）として埋め込む。
//! 実行時はテーマとモードからリソースIDを選び `LoadImageW` で HICON を得るだけ —
//! 染色（Light=#202020 / Dark=#F0F0F0）と一時かなの青点は ICO 生成時に焼き済み。
//!
//! 旧テキスト描画の知見（なぜ事前ラスタライズか）: フォントのサイドベアリング差の
//! 吸収にピクセル実測の中央寄せ（ink_centering_shift）が必要だったが、ICO 化で
//! グリフ形状自体に余白が設計時に含まれるため実行時補正が不要になった。

// --- DLL アイコンリソースID（build.rs の ICONS とセットで管理。変更時は両方触ること）---

pub(crate) const RES_MODE_DIRECT_LIGHT: usize = 1;
pub(crate) const RES_MODE_DIRECT_DARK: usize = 2;
pub(crate) const RES_MODE_KANA_LIGHT: usize = 3;
pub(crate) const RES_MODE_KANA_DARK: usize = 4;
pub(crate) const RES_MODE_EPHEMERAL_LIGHT: usize = 5;
pub(crate) const RES_MODE_EPHEMERAL_DARK: usize = 6;
/// プロファイル固定アイコン（Gapless N 案）。RegisterProfile の iconIndex が参照する。
pub(crate) const RES_PROFILE_N: usize = 7;

/// モードとテーマからタスクバー表示に使うリソースIDを選ぶ純関数。
/// direct のときは ephemeral を無視する（`mode_label_ephemeral` と同じ規則 —
/// direct 中は一時かな状態自体が存在しない）。
pub(crate) fn mode_icon_res_id(is_direct: bool, ephemeral: bool, theme: IconTheme) -> usize {
    match (is_direct, theme) {
        (true, IconTheme::Light) => RES_MODE_DIRECT_LIGHT,
        (true, IconTheme::Dark) => RES_MODE_DIRECT_DARK,
        (false, IconTheme::Light) if !ephemeral => RES_MODE_KANA_LIGHT,
        (false, IconTheme::Light) => RES_MODE_EPHEMERAL_LIGHT,
        (false, IconTheme::Dark) if !ephemeral => RES_MODE_KANA_DARK,
        (false, IconTheme::Dark) => RES_MODE_EPHEMERAL_DARK,
    }
}

/// 96 DPI 基準ピクセル数を実行時 DPI へ整数丸めでスケールする。
/// `candidate_window.rs` の DPI スケール規則（四捨五入相当）と揃える。
pub fn icon_size_px(base_at_96dpi: i32, dpi: i32) -> i32 {
    (base_at_96dpi * dpi + 48) / 96
}

/// タスクバーのライト/ダーク判定結果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconTheme {
    Light,
    Dark,
}

/// `HKCU\...\Personalize\SystemUsesLightTheme` の DWORD 値からタスクバーの明暗を判定する。
/// 値が読めない場合は Light を既定にする（黒文字アイコンの方が誤爆時に見えやすい）。
pub fn icon_theme_from_registry_value(system_uses_light_theme: Option<u32>) -> IconTheme {
    match system_uses_light_theme {
        Some(0) => IconTheme::Dark,
        _ => IconTheme::Light,
    }
}

/// タスクバー/システムの明暗設定を読む。キー/値が無い（Win10 一部ビルド等）場合は None。
/// なぜ Option か: レジストリキー自体が無い環境があり得るため、呼び出し側
/// （`icon_theme_from_registry_value`）で Light 既定にフォールバックする設計。
pub fn read_system_uses_light_theme() -> Option<u32> {
    let key = windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .ok()?;
    key.get_u32("SystemUsesLightTheme").ok()
}

/// モードに応じたタスクバーアイコンを DLL リソースからロードする。
/// ICO は 16/20/24/32/40/48/64px を内包するため、要求サイズ（DPI スケール済み）に
/// 最も近いエントリが OS 側で選ばれる。
///
/// SAFETY: TSF から STA スレッドで呼ばれる。`globals::hinst()` は DllMain
/// (PROCESS_ATTACH) で保存済みの自 DLL ハンドル。LR_SHARED を付けないため
/// 返却された HICON はコピーであり、GetIcon 契約（呼び出し側システムが DestroyIcon
/// する）と整合する。
///
/// 失敗時は None（呼び出し側 GetIcon は E_NOTIMPL にフォールバックし、システムは
/// 既定ロゴを出す）。
pub(crate) unsafe fn render_mode_icon(
    is_direct: bool,
    ephemeral: bool,
    dpi: i32,
) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{LoadImageW, HICON, IMAGE_ICON, LR_DEFAULTCOLOR};

    let size = icon_size_px(16, dpi);
    if size <= 0 {
        return None;
    }
    let theme = icon_theme_from_registry_value(read_system_uses_light_theme());
    let res = mode_icon_res_id(is_direct, ephemeral, theme);
    // MAKEINTRESOURCE 相当: リソースID をそのままポインタ値へ入れて渡す
    // （RT_GROUP_ICON の数値ID 参照。文字列名との衝突は ID < 65536 で判別される）。
    let name = PCWSTR(res as *const u16);
    let handle = LoadImageW(
        Some(crate::globals::hinst().into()),
        name,
        IMAGE_ICON,
        size,
        size,
        LR_DEFAULTCOLOR,
    )
    .ok()?;
    Some(HICON(handle.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_size_scales_with_dpi() {
        // 96 DPI: そのまま。192 DPI: 倍。
        assert_eq!(icon_size_px(16, 96), 16);
        assert_eq!(icon_size_px(16, 192), 32);
        // 144 DPI (150%): 24px 相当。
        assert_eq!(icon_size_px(16, 144), 24);
    }

    #[test]
    fn theme_from_registry_value_light_and_dark() {
        assert_eq!(icon_theme_from_registry_value(Some(1)), IconTheme::Light);
        assert_eq!(icon_theme_from_registry_value(Some(0)), IconTheme::Dark);
    }

    #[test]
    fn theme_from_registry_value_missing_defaults_light() {
        assert_eq!(icon_theme_from_registry_value(None), IconTheme::Light);
    }

    #[test]
    fn res_ids_are_distinct_and_match_build_order() {
        // build.rs の ICONS 並び（direct/kana/ephemeral × light/dark、profile 末尾）と
        // 番号がずれると実行時に別アイコンが出る。定数側の一意性だけでも機械検査する。
        let ids = [
            RES_MODE_DIRECT_LIGHT,
            RES_MODE_DIRECT_DARK,
            RES_MODE_KANA_LIGHT,
            RES_MODE_KANA_DARK,
            RES_MODE_EPHEMERAL_LIGHT,
            RES_MODE_EPHEMERAL_DARK,
            RES_PROFILE_N,
        ];
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(*id, i + 1, "resource ids must stay 1..=7 in declared order");
        }
    }

    #[test]
    fn mode_icon_res_id_selects_direct_by_theme() {
        // direct 中は ephemeral を無視（mode_label_ephemeral と同じ規則）。
        assert_eq!(
            mode_icon_res_id(true, false, IconTheme::Light),
            RES_MODE_DIRECT_LIGHT
        );
        assert_eq!(
            mode_icon_res_id(true, true, IconTheme::Dark),
            RES_MODE_DIRECT_DARK
        );
    }

    #[test]
    fn mode_icon_res_id_selects_kana_and_ephemeral() {
        assert_eq!(
            mode_icon_res_id(false, false, IconTheme::Light),
            RES_MODE_KANA_LIGHT
        );
        assert_eq!(
            mode_icon_res_id(false, true, IconTheme::Light),
            RES_MODE_EPHEMERAL_LIGHT
        );
        assert_eq!(
            mode_icon_res_id(false, false, IconTheme::Dark),
            RES_MODE_KANA_DARK
        );
        assert_eq!(
            mode_icon_res_id(false, true, IconTheme::Dark),
            RES_MODE_EPHEMERAL_DARK
        );
    }
}
