//! nospacekey 固有の永続ID（GUID/LANGID）の唯一の真実源。
//! tip(RegisterProfile) と testbench(ActivateProfile) が同一値を参照するために共有する。
//! 生成後は二度と変えない（変えると再登録が必要）。

use windows_core::GUID;

/// このIME固有の永続ID（COM CLSID）。
pub const CLSID_NOSPACEKEY: GUID = GUID::from_u128(0xb4b39227_eff2_41da_b357_0c3170a57875);
/// 入力プロファイルの永続ID。
pub const PROFILE_NOSPACEKEY: GUID = GUID::from_u128(0xffca79b0_79b3_4e21_b240_39e5da32da39);
/// 下線表示用の表示属性GUID。
pub const GUID_DISPLAY_ATTRIBUTE: GUID = GUID::from_u128(0xb8045359_1149_414d_84cd_d8891f69d477);
/// 変換対象文節（文節ナビゲーションの選択文節）の太下線表示用の表示属性GUID。
pub const GUID_DISPLAY_ATTRIBUTE_TARGET: GUID =
    GUID::from_u128(0x5b1a6f0e_9d24_4c31_8a75_36c4de00b917);
/// 日本語（ja-JP）の LANGID。
pub const LANGID_JA: u16 = 0x0411;
