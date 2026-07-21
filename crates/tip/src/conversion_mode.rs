//! TSF conversion-mode（ひらがな/半角英数）の読み書き。
//! 判定の純ロジックはここで単体テストし、COM 取得は TextService 側から呼ぶ。

use windows::Win32::System::Variant::{VARIANT, VT_I4};

/// TF_CONVERSIONMODE_NATIVE ビット（ひらがな等の「ネイティブ入力」）。
pub const CONVMODE_NATIVE: u32 = 0x0001;
/// TF_CONVERSIONMODE_FULLSHAPE ビット（全角）。落とすと半角。
pub const CONVMODE_FULLSHAPE: u32 = 0x0008;

/// compartment から読んだ VARIANT を conversion-mode 値へ変換する純関数。
/// conversion-mode は本来 VT_I4 だが、未設定の compartment は VT_EMPTY を返す
/// （`GetValue` は Err にならず Ok(VT_EMPTY)）。windows 0.62 の
/// `i32::try_from(&VARIANT)`（VariantToInt32）は VT_EMPTY を Ok(0) に強制変換して
/// しまうため、値を採用する前に vt が VT_I4 であることを明示的に確認する。
/// VT_I4 以外（VT_EMPTY 含む）は NATIVE 既定へ。
pub fn mode_from_compartment_value(v: &VARIANT) -> u32 {
    if v.vt() != VT_I4 {
        return CONVMODE_NATIVE;
    }
    i32::try_from(v).unwrap_or(CONVMODE_NATIVE as i32) as u32
}

/// conversion-mode 値から「半角英数(直接入力)か」を判定する純関数。
/// NATIVE ビットが立っていなければ直接入力（boiled-egg）。
pub fn is_direct(mode: u32) -> bool {
    (mode & CONVMODE_NATIVE) == 0
}

/// トグル後の conversion-mode 値（NATIVE ビットを反転）。
pub fn toggled(mode: u32) -> u32 {
    mode ^ CONVMODE_NATIVE
}

/// SP7: 起動時の「半角英数(直接入力)」へ初期化した conversion-mode 値。
/// ユーザの当初ニーズは明確に**半角**（全角だとターミナル/Vim でショートカットが不発）なので、
/// NATIVE（ネイティブ入力）と FULLSHAPE（全角）の両ビットを落として半角を保証する。
/// ROMAN 等その他のビットは保存する。
pub fn to_direct(mode: u32) -> u32 {
    mode & !(CONVMODE_NATIVE | CONVMODE_FULLSHAPE)
}

/// SP7: 起動時に default_direct を適用すべきか（ワンショット）。
/// 設定が有効で、かつこのインスタンスでまだ適用していないときだけ true。
/// これにより IME 切替の往復で再 Activate されても、ユーザの手動トグルを巻き戻さない。
pub fn should_apply_default_direct(enabled: bool, already_applied: bool) -> bool {
    enabled && !already_applied
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn direct_when_native_bit_clear() {
        assert!(is_direct(0)); // 半角英数
        assert!(!is_direct(CONVMODE_NATIVE)); // ひらがな
    }
    #[test]
    fn toggle_flips_native_bit() {
        assert_eq!(toggled(0), CONVMODE_NATIVE);
        assert_eq!(toggled(CONVMODE_NATIVE), 0);
    }
    #[test]
    fn to_direct_clears_native_and_fullshape() {
        const ROMAN: u32 = 0x0010;
        assert_eq!(to_direct(CONVMODE_NATIVE), 0); // ひらがな → 半角英数
        assert_eq!(to_direct(0), 0); // 既に半角英数 → そのまま
        // 全角ひらがな(NATIVE|FULLSHAPE) からは FULLSHAPE も落として半角を保証する。
        assert_eq!(to_direct(CONVMODE_NATIVE | CONVMODE_FULLSHAPE), 0);
        // ROMAN 等その他のビットは保存する。
        assert_eq!(to_direct(CONVMODE_NATIVE | ROMAN), ROMAN);
        assert!(is_direct(to_direct(CONVMODE_NATIVE | CONVMODE_FULLSHAPE)));
    }

    #[test]
    fn should_apply_default_direct_is_one_shot() {
        assert!(should_apply_default_direct(true, false)); // 有効 & 未適用 → 適用する
        assert!(!should_apply_default_direct(true, true)); // 有効 & 適用済み → しない（手動トグル尊重）
        assert!(!should_apply_default_direct(false, false)); // 無効 → しない
        assert!(!should_apply_default_direct(false, true));
    }

    #[test]
    fn empty_or_non_i4_compartment_value_defaults_to_native() {
        // 未設定(VT_EMPTY)の compartment は NATIVE 既定へ落ちる（本バグの本体）。
        assert_eq!(mode_from_compartment_value(&VARIANT::default()), CONVMODE_NATIVE);
        // 明示的に VT_I4 でセットされた値はそのまま返る（direct/native とも保存）。
        assert_eq!(mode_from_compartment_value(&VARIANT::from(0i32)), 0);
        assert_eq!(mode_from_compartment_value(&VARIANT::from(1i32)), CONVMODE_NATIVE);
        // 非 I4 型(VT_BOOL)も NATIVE 既定へ落ちる。
        assert_eq!(mode_from_compartment_value(&VARIANT::from(true)), CONVMODE_NATIVE);
    }
}
