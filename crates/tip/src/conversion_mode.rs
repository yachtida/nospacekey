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

/// SP7 改定: apply_default_direct の成否ポリシー。成功 = compartment が取れた、かつ
/// （書込が不要=already-direct、または SetValue 成功）。失敗（false）のとき呼び出し元は
/// default_direct_applied を立てず（次回 Activate で再試行）、direct_mode_owned も
/// 立てない（langbar Cell・打鍵ゲートは live 値を追従させる）。
pub fn default_direct_success(
    compartment_available: bool,
    needs_write: bool,
    write_ok: bool,
) -> bool {
    compartment_available && (!needs_write || write_ok)
}

/// AddItem 直前の langbar Cell に入れる値。
/// 直後の `apply_default_direct` が走れて（`will_apply`）、かつ compartment が取れるときだけ
/// 楽観的に直接入力（A）を出す。取れなければ live 読みのまま（失敗時に表示A・入力あ を残さない）。
pub fn langbar_direct_for_additem(
    will_apply: bool,
    compartment_available: bool,
    live_is_direct: bool,
) -> bool {
    if will_apply && compartment_available {
        true
    } else {
        live_is_direct
    }
}

/// langbar の OnUpdate が必要か。Cell が既に目標と一致していれば再描画しない。
/// 不一致（ロールバックや未プリセット）のときだけ通知する。
pub fn should_notify_langbar(cell_is_direct: bool, target_is_direct: bool) -> bool {
    cell_is_direct != target_is_direct
}

/// 打鍵ゲートが使う「今 direct か」。TIP がモードを所有している（default_direct 適用・
/// トグル・ephemeral）ときは langbar Cell を真実にする。ホストが Activate 後に
/// compartment を NATIVE へ戻しても、表示 A のままひらがな入力にはしない。
pub fn effective_is_direct(owned: bool, langbar_is_direct: bool, live_is_direct: bool) -> bool {
    if owned {
        langbar_is_direct
    } else {
        live_is_direct
    }
}

/// トグルの XOR 対象。owned かつ Cell と live が食い違うとき、live を XOR すると
/// 「A に見えているのに次も direct」になる。Cell を before の NATIVE ビットにする。
pub fn toggle_before_mode(owned: bool, langbar_is_direct: bool, live: u32) -> u32 {
    if !owned {
        return live;
    }
    if langbar_is_direct {
        to_direct(live)
    } else {
        live | CONVMODE_NATIVE
    }
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
    fn default_direct_success_truth_table() {
        // compartment なしは常に失敗（needs_write/write_ok に依らず再試行へ）。
        assert!(!default_direct_success(false, false, false));
        assert!(!default_direct_success(false, false, true));
        assert!(!default_direct_success(false, true, false));
        assert!(!default_direct_success(false, true, true));
        // compartment あり・already-direct（書込不要）は成功（write_ok は無関係）。
        assert!(default_direct_success(true, false, false));
        assert!(default_direct_success(true, false, true));
        // compartment あり・要書込は SetValue の成否に従う。失敗は false ＝
        // owned も applied も立てず、langbar は live 値へ戻す（表示A・入力あ を残さない）。
        assert!(default_direct_success(true, true, true));
        assert!(!default_direct_success(true, true, false));
    }

    #[test]
    fn langbar_direct_for_additem_truth_table() {
        assert!(langbar_direct_for_additem(true, true, false)); // 適用予定かつ compartment あり → A
        assert!(!langbar_direct_for_additem(true, false, false)); // compartment なし → 楽観しない
        assert!(langbar_direct_for_additem(true, true, true));
        assert!(!langbar_direct_for_additem(false, true, false));
        assert!(langbar_direct_for_additem(false, true, true));
    }

    #[test]
    fn should_notify_langbar_truth_table() {
        assert!(!should_notify_langbar(true, true)); // 成功経路で再 OnUpdate しない
        assert!(should_notify_langbar(true, false)); // ロールバックで通知する
        assert!(should_notify_langbar(false, true));
    }

    #[test]
    fn effective_is_direct_trusts_cell_when_owned() {
        assert!(effective_is_direct(true, true, false)); // 所有中: live が NATIVE でも A
        assert!(!effective_is_direct(true, false, true));
        assert!(!effective_is_direct(false, true, false)); // 未所有: live
        assert!(effective_is_direct(false, false, true));
    }

    #[test]
    fn toggle_before_mode_uses_cell_when_owned_and_live_diverges() {
        // 表示 A・live ひらがなでトグル → before は direct、XOR で NATIVE（あへ）
        let before = toggle_before_mode(true, true, CONVMODE_NATIVE);
        assert_eq!(before, 0);
        assert_eq!(toggled(before), CONVMODE_NATIVE);
        // 未所有なら live をそのまま XOR
        assert_eq!(
            toggle_before_mode(false, true, CONVMODE_NATIVE),
            CONVMODE_NATIVE
        );
        assert_eq!(toggled(toggle_before_mode(false, true, CONVMODE_NATIVE)), 0);
    }

    #[test]
    fn empty_or_non_i4_compartment_value_defaults_to_native() {
        // 未設定(VT_EMPTY)の compartment は NATIVE 既定へ落ちる（本バグの本体）。
        assert_eq!(
            mode_from_compartment_value(&VARIANT::default()),
            CONVMODE_NATIVE
        );
        // 明示的に VT_I4 でセットされた値はそのまま返る（direct/native とも保存）。
        assert_eq!(mode_from_compartment_value(&VARIANT::from(0i32)), 0);
        assert_eq!(
            mode_from_compartment_value(&VARIANT::from(1i32)),
            CONVMODE_NATIVE
        );
        // 非 I4 型(VT_BOOL)も NATIVE 既定へ落ちる。
        assert_eq!(
            mode_from_compartment_value(&VARIANT::from(true)),
            CONVMODE_NATIVE
        );
    }
}
