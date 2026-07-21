//! configurable keymap: 機能キーのバインド語彙・パーサ・検証・衝突判定。
//! パーサとキー名→VK 表をここ(settings crate)に一元化する: TIP と設定アプリ(Tauri)の
//! 両方が同じコードで解釈するため「UI では通るが TIP で解釈不能」という不整合を構造的に防ぐ。
//! キー名は独自語彙でなくブラウザ KeyboardEvent.code の語彙を正規形とする: 設定アプリの
//! レコーダー(JS keydown)が変換なしでそのまま保存でき、独自名との写像ずれが起きない。

use serde::{Deserialize, Serialize};

/// 修飾キー＋主キーの組(解決済みバインド)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyChord {
    pub vk: u32,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

/// KeyboardEvent.code 語彙 → Windows VK。列挙制: ここに無い名前は割り当て不可
/// (Enter/Esc/矢印/nav の「予約」は語彙からの除外で表現する — 別途の拒否リストは持たない)。
/// Space は語彙に載せる(修飾必須は validate_binding が担う — 単独 Space=変換は固定のまま)。
pub fn key_name_to_vk(name: &str) -> Option<u32> {
    match name {
        "Backspace" => return Some(0x08),
        "Tab" => return Some(0x09),
        "Space" => return Some(0x20),
        "Convert" => return Some(0x1C),    // 変換 VK_CONVERT
        "NonConvert" => return Some(0x1D), // 無変換 VK_NONCONVERT
        "Semicolon" => return Some(0xBA),
        "Equal" => return Some(0xBB),
        "Comma" => return Some(0xBC),
        "Minus" => return Some(0xBD),
        "Period" => return Some(0xBE),
        "Slash" => return Some(0xBF),
        "Backquote" => return Some(0xC0),
        "BracketLeft" => return Some(0xDB),
        "Backslash" => return Some(0xDC),
        "BracketRight" => return Some(0xDD),
        "Quote" => return Some(0xDE),
        _ => {}
    }
    if let Some(c) = name.strip_prefix("Key") {
        let b = c.as_bytes();
        if b.len() == 1 && b[0].is_ascii_uppercase() {
            return Some(b[0] as u32); // 'A'..'Z' == 0x41..0x5A
        }
        return None;
    }
    if let Some(d) = name.strip_prefix("Digit") {
        let b = d.as_bytes();
        if b.len() == 1 && b[0].is_ascii_digit() {
            return Some(0x30 + (b[0] - b'0') as u32);
        }
        return None;
    }
    if let Some(n) = name.strip_prefix('F') {
        if let Ok(i) = n.parse::<u32>() {
            if (1..=24).contains(&i) && n == i.to_string() {
                return Some(0x70 + i - 1); // VK_F1=0x70
            }
        }
        return None;
    }
    None
}

/// VK → KeyboardEvent.code 語彙(key_name_to_vk の逆)。語彙外 VK は None。
pub fn vk_to_key_name(vk: u32) -> Option<String> {
    match vk {
        0x08 => Some("Backspace".into()),
        0x09 => Some("Tab".into()),
        0x1C => Some("Convert".into()),
        0x1D => Some("NonConvert".into()),
        0x20 => Some("Space".into()),
        0x30..=0x39 => Some(format!("Digit{}", vk - 0x30)),
        0x41..=0x5A => Some(format!("Key{}", (vk as u8) as char)),
        0x70..=0x87 => Some(format!("F{}", vk - 0x70 + 1)),
        0xBA => Some("Semicolon".into()),
        0xBB => Some("Equal".into()),
        0xBC => Some("Comma".into()),
        0xBD => Some("Minus".into()),
        0xBE => Some("Period".into()),
        0xBF => Some("Slash".into()),
        0xC0 => Some("Backquote".into()),
        0xDB => Some("BracketLeft".into()),
        0xDC => Some("Backslash".into()),
        0xDD => Some("BracketRight".into()),
        0xDE => Some("Quote".into()),
        _ => None,
    }
}

/// `[Ctrl+][Shift+][Alt+]<KeyName>` をパースする(修飾は順不同・重複は拒否)。
pub fn parse_chord(s: &str) -> Result<KeyChord, String> {
    let (mut ctrl, mut shift, mut alt) = (false, false, false);
    let mut key: Option<&str> = None;
    for part in s.split('+') {
        match part {
            "Ctrl" if !ctrl => ctrl = true,
            "Shift" if !shift => shift = true,
            "Alt" if !alt => alt = true,
            "Ctrl" | "Shift" | "Alt" => return Err(format!("修飾キーが重複しています: {s:?}")),
            other => {
                if key.is_some() {
                    return Err(format!("主キーが複数あります: {s:?}"));
                }
                key = Some(other);
            }
        }
    }
    let name = key.ok_or_else(|| format!("主キーがありません: {s:?}"))?;
    let vk = key_name_to_vk(name)
        .ok_or_else(|| format!("割り当てできないキーです: {name:?}"))?;
    Ok(KeyChord { vk, ctrl, shift, alt })
}

/// KeyChord → 正規形文字列(Ctrl→Shift→Alt の順)。語彙外 VK を含む KeyChord は
/// parse_chord からは生まれないため unwrap 相当だが、防御的に "?" を返す。
pub fn format_chord(c: &KeyChord) -> String {
    let mut out = String::new();
    if c.ctrl { out.push_str("Ctrl+"); }
    if c.shift { out.push_str("Shift+"); }
    if c.alt { out.push_str("Alt+"); }
    out.push_str(&vk_to_key_name(c.vk).unwrap_or_else(|| "?".into()));
    out
}

/// カスタマイズ対象のコマンド系 12 機能(spec §1)。宣言順が「手編集 JSON で同一文脈に
/// 重複バインドがあったときの決定的優先順」を兼ねる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeymapFunc {
    ModeToggle,
    Reconvert,
    Feedback,
    Ephemeral,
    CommitUndo,
    TypoCorrect,
    LlmConvert,
    ToHiragana,
    ToKatakana,
    ToHankakuKana,
    ToZenkakuEisu,
    ToHankakuEisu,
}

pub const ALL_FUNCS: [KeymapFunc; 12] = [
    KeymapFunc::ModeToggle, KeymapFunc::Reconvert, KeymapFunc::Feedback,
    KeymapFunc::Ephemeral, KeymapFunc::CommitUndo,
    KeymapFunc::TypoCorrect, KeymapFunc::LlmConvert,
    KeymapFunc::ToHiragana, KeymapFunc::ToKatakana, KeymapFunc::ToHankakuKana,
    KeymapFunc::ToZenkakuEisu, KeymapFunc::ToHankakuEisu,
];

/// 衝突判定の文脈グループ(spec §6)。Global(PreservedKey)は OS が先取りするため全グループと衝突扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuncGroup { Global, Idle, Composing }

impl KeymapFunc {
    pub fn group(self) -> FuncGroup {
        use KeymapFunc::*;
        match self {
            ModeToggle | Reconvert | Feedback => FuncGroup::Global,
            Ephemeral | CommitUndo => FuncGroup::Idle,
            _ => FuncGroup::Composing,
        }
    }
    pub fn is_preserved(self) -> bool { self.group() == FuncGroup::Global }
    /// Alt はキーシンク経路(OnKeyDown)に WM_SYSKEYDOWN が届かない疑いがあるため
    /// PreservedKey 経路(TF_MOD_ALT を正式サポート)のみ許可する(spec §4)。
    pub fn alt_allowed(self) -> bool { self.is_preserved() }
    /// KeymapSettings のフィールド名(検証エラーの field / UI の data-error-for と一致させる)。
    pub fn settings_field(self) -> &'static str {
        use KeymapFunc::*;
        match self {
            ModeToggle => "mode_toggle", Reconvert => "reconvert", Feedback => "feedback",
            Ephemeral => "ephemeral", CommitUndo => "commit_undo",
            TypoCorrect => "typo_correct", LlmConvert => "llm_convert",
            ToHiragana => "to_hiragana", ToKatakana => "to_katakana",
            ToHankakuKana => "to_hankaku_kana", ToZenkakuEisu => "to_zenkaku_eisu",
            ToHankakuEisu => "to_hankaku_eisu",
        }
    }
    pub fn label_ja(self) -> &'static str {
        use KeymapFunc::*;
        match self {
            ModeToggle => "モードトグル(あ⇔A)", Reconvert => "再変換",
            Feedback => "誤変換フィードバック記録", Ephemeral => "一時かなモード開始",
            CommitUndo => "確定取り消し", TypoCorrect => "修正変換",
            LlmConvert => "外部LLM変換", ToHiragana => "表記変換: ひらがな",
            ToKatakana => "表記変換: カタカナ", ToHankakuKana => "表記変換: 半角カナ",
            ToZenkakuEisu => "表記変換: 全角英数", ToHankakuEisu => "表記変換: 半角英数",
        }
    }
}

/// 旧 ephemeral.trigger("f8"/"f9"/"f10") → チョード。未知は F8(旧 trigger_name_to_vk の既定と同じ)。
pub fn legacy_ephemeral_chord(name: &str) -> KeyChord {
    let vk = match name { "f9" => 0x78, "f10" => 0x79, _ => 0x77 };
    KeyChord { vk, ctrl: false, shift: false, alt: false }
}

fn bare(vk: u32) -> KeyChord { KeyChord { vk, ctrl: false, shift: false, alt: false } }

/// 機能の既定チョード列。Preserved 系は JIS/US の 2 本(text_service.rs の現行登録と同値)、
/// Ephemeral は旧 ephemeral.trigger を継承する(後方互換 — spec §3 移行)。
pub fn default_chords(f: KeymapFunc, legacy_ephemeral_trigger: &str) -> Vec<KeyChord> {
    use KeymapFunc::*;
    match f {
        ModeToggle => vec![bare(0x1D), KeyChord { alt: true, ..bare(0xBA) }],
        Reconvert => vec![bare(0x1C), KeyChord { alt: true, ..bare(0xBF) }],
        Feedback => vec![KeyChord { ctrl: true, ..bare(0x1C) }, KeyChord { ctrl: true, ..bare(0xBF) }],
        Ephemeral => vec![legacy_ephemeral_chord(legacy_ephemeral_trigger)],
        CommitUndo => vec![KeyChord { ctrl: true, ..bare(0x08) }],
        TypoCorrect => vec![bare(0x09)],
        LlmConvert => vec![KeyChord { shift: true, ..bare(0x09) }],
        ToHiragana => vec![bare(0x75)],
        ToKatakana => vec![bare(0x76)],
        ToHankakuKana => vec![bare(0x77)],
        ToZenkakuEisu => vec![bare(0x78)],
        ToHankakuEisu => vec![bare(0x79)],
    }
}

/// 単独で割り当ててよいキーか(F1-F24/変換/無変換/Tab/Backspace)。それ以外の語彙
/// (英字/数字/記号)は Ctrl 必須(spec §6)。
fn standalone_ok(vk: u32) -> bool {
    matches!(vk, 0x70..=0x87 | 0x1C | 0x1D | 0x08 | 0x09)
}

/// 設定アプリ用の厳格検証。"none" は常に妥当。TIP 側は resolve_binding(劣化型)を使い、
/// こちらは保存前ゲート(不正値をそもそも書かせない)。
pub fn validate_binding(f: KeymapFunc, value: &str) -> Result<(), String> {
    if value == "none" {
        return Ok(());
    }
    let c = parse_chord(value)?;
    if c.alt && !f.alt_allowed() {
        return Err("この機能に Alt は割り当てできません(キー入力経路に Alt 併用キーが届かないため)".into());
    }
    if c.vk == 0x20 {
        // Space は修飾必須(単独 Space=変換/空白入力は固定)。Shift+Space は IME トグルの
        // 定番なので、英字と違い Shift 単独修飾も許す(Shift+Space が奪うのは空白 1 種のみで、
        // Shift+英字のように直接入力の一群を影で奪わない)。
        if !c.ctrl && !c.shift && !c.alt {
            return Err("Space 単独は割り当てできません。修飾キー(Ctrl/Shift)を組み合わせてください".into());
        }
    } else if !standalone_ok(c.vk) && !c.ctrl && !c.alt {
        return Err("文字・数字・記号キーには Ctrl を組み合わせてください".into());
    }
    Ok(())
}

/// バインドの3状態。パース不能な文字列は既定へ劣化する(壊れた settings.json でも入力を
/// 止めない — Settings::from_json_str が全体を既定へ落とすのと同じ方針)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding { Default, Disabled, Chord(KeyChord) }

pub fn resolve_binding(v: &Option<String>) -> Binding {
    match v.as_deref() {
        None => Binding::Default,
        Some("none") => Binding::Disabled,
        Some(s) => parse_chord(s).map(Binding::Chord).unwrap_or(Binding::Default),
    }
}

/// 機能→バインド設定値。各フィールドは None=既定 / Some("none")=無効 / Some(チョード)=明示。
/// None も JSON に null として書く(skip しない): 設定アプリの dirty 判定(JSON 文字列比較)が
/// null と欠落を区別できないため、常に全 12 キーを出して表現を一意にする。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KeymapSettings {
    #[serde(default)] pub mode_toggle: Option<String>,
    #[serde(default)] pub reconvert: Option<String>,
    #[serde(default)] pub feedback: Option<String>,
    #[serde(default)] pub ephemeral: Option<String>,
    #[serde(default)] pub commit_undo: Option<String>,
    #[serde(default)] pub typo_correct: Option<String>,
    #[serde(default)] pub llm_convert: Option<String>,
    #[serde(default)] pub to_hiragana: Option<String>,
    #[serde(default)] pub to_katakana: Option<String>,
    #[serde(default)] pub to_hankaku_kana: Option<String>,
    #[serde(default)] pub to_zenkaku_eisu: Option<String>,
    #[serde(default)] pub to_hankaku_eisu: Option<String>,
}

impl KeymapSettings {
    pub fn get(&self, f: KeymapFunc) -> &Option<String> {
        use KeymapFunc::*;
        match f {
            ModeToggle => &self.mode_toggle, Reconvert => &self.reconvert,
            Feedback => &self.feedback, Ephemeral => &self.ephemeral,
            CommitUndo => &self.commit_undo, TypoCorrect => &self.typo_correct,
            LlmConvert => &self.llm_convert, ToHiragana => &self.to_hiragana,
            ToKatakana => &self.to_katakana, ToHankakuKana => &self.to_hankaku_kana,
            ToZenkakuEisu => &self.to_zenkaku_eisu, ToHankakuEisu => &self.to_hankaku_eisu,
        }
    }
}

pub fn find_conflicts(
    km: &KeymapSettings,
    legacy_ephemeral_trigger: &str,
    ephemeral_enabled: bool,
    feedback_enabled: bool,
    typo_enabled: bool,
    llm_enabled: bool,
) -> Vec<Conflict> {
    let entries: Vec<(KeymapFunc, Option<String>)> =
        ALL_FUNCS.map(|f| (f, km.get(f).clone())).to_vec();
    find_conflicts_in(&entries, legacy_ephemeral_trigger,
        ephemeral_enabled, feedback_enabled, typo_enabled, llm_enabled)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict {
    pub a: KeymapFunc,
    pub b: KeymapFunc,
    pub chord: KeyChord,
}

/// 実効チョードの重複を検出する。entries は (機能, 設定値) の全 12 件。
/// feature off の機能(ephemeral/feedback/typo/llm)はチョードを持たない=衝突に参加しない。
pub fn find_conflicts_in(
    entries: &[(KeymapFunc, Option<String>)],
    legacy_ephemeral_trigger: &str,
    ephemeral_enabled: bool,
    feedback_enabled: bool,
    typo_enabled: bool,
    llm_enabled: bool,
) -> Vec<Conflict> {
    use KeymapFunc::*;
    let mut chords: Vec<(KeymapFunc, KeyChord)> = Vec::new();
    for (f, v) in entries {
        let enabled = match f {
            Ephemeral => ephemeral_enabled,
            Feedback => feedback_enabled,
            TypoCorrect => typo_enabled,
            LlmConvert => llm_enabled,
            _ => true,
        };
        if !enabled {
            continue;
        }
        match resolve_binding(v) {
            Binding::Disabled => {}
            Binding::Chord(c) => chords.push((*f, c)),
            Binding::Default => {
                for c in default_chords(*f, legacy_ephemeral_trigger) {
                    chords.push((*f, c));
                }
            }
        }
    }
    let mut out = Vec::new();
    for i in 0..chords.len() {
        for j in i + 1..chords.len() {
            let ((fa, ca), (fb, cb)) = (chords[i], chords[j]);
            let cross_group_ok = fa.group() != fb.group()
                && fa.group() != FuncGroup::Global
                && fb.group() != FuncGroup::Global;
            if ca == cb && !cross_group_ok {
                out.push(Conflict { a: fa, b: fb, chord: ca });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_map_to_vk_and_back() {
        assert_eq!(key_name_to_vk("KeyA"), Some(0x41));
        assert_eq!(key_name_to_vk("KeyZ"), Some(0x5A));
        assert_eq!(key_name_to_vk("Digit0"), Some(0x30));
        assert_eq!(key_name_to_vk("Digit9"), Some(0x39));
        assert_eq!(key_name_to_vk("F1"), Some(0x70));
        assert_eq!(key_name_to_vk("F8"), Some(0x77));
        assert_eq!(key_name_to_vk("F24"), Some(0x87));
        assert_eq!(key_name_to_vk("Backspace"), Some(0x08));
        assert_eq!(key_name_to_vk("Tab"), Some(0x09));
        assert_eq!(key_name_to_vk("Convert"), Some(0x1C));
        assert_eq!(key_name_to_vk("NonConvert"), Some(0x1D));
        assert_eq!(key_name_to_vk("Semicolon"), Some(0xBA));
        assert_eq!(key_name_to_vk("Slash"), Some(0xBF));
        assert_eq!(key_name_to_vk("Space"), Some(0x20));
        // 往復。
        for name in ["KeyQ", "Digit5", "F11", "Backspace", "Tab", "Space", "Convert", "NonConvert", "Comma"] {
            let vk = key_name_to_vk(name).unwrap();
            assert_eq!(vk_to_key_name(vk).as_deref(), Some(name));
        }
    }

    #[test]
    fn unbindable_keys_are_not_in_vocabulary() {
        // 予約キー(Enter/Esc/矢印/nav)は語彙に無い=パースの時点で割り当て不可(spec §6)。
        // Space は語彙に載る(修飾必須の制約は validate_binding 側 — 2026-07-18 修正)。
        for name in ["Enter", "Escape", "ArrowUp", "ArrowDown", "ArrowLeft",
                     "ArrowRight", "Home", "End", "PageUp", "PageDown", "Delete", ""] {
            assert_eq!(key_name_to_vk(name), None, "{name} は割り当て不可のはず");
        }
        assert_eq!(key_name_to_vk("F0"), None);
        assert_eq!(key_name_to_vk("F25"), None);
        assert_eq!(key_name_to_vk("Fabc"), None);
        assert_eq!(key_name_to_vk("Key1"), None); // Key の後は英大文字 1 文字のみ
        assert_eq!(key_name_to_vk("Keya"), None);
    }

    #[test]
    fn parse_chord_accepts_modifiers_in_any_order_and_formats_canonically() {
        let c = parse_chord("Ctrl+Shift+KeyK").unwrap();
        assert_eq!(c, KeyChord { vk: 0x4B, ctrl: true, shift: true, alt: false });
        // 順不同でも受理し、format は Ctrl→Shift→Alt の正規順で返す。
        assert_eq!(format_chord(&parse_chord("Shift+Ctrl+KeyK").unwrap()), "Ctrl+Shift+KeyK");
        assert_eq!(format_chord(&parse_chord("F8").unwrap()), "F8");
        assert_eq!(format_chord(&parse_chord("Alt+Semicolon").unwrap()), "Alt+Semicolon");
        assert_eq!(format_chord(&parse_chord("Ctrl+Backspace").unwrap()), "Ctrl+Backspace");
    }

    #[test]
    fn parse_chord_rejects_malformed_inputs() {
        assert!(parse_chord("").is_err());
        assert!(parse_chord("Ctrl+").is_err());          // 主キーなし
        assert!(parse_chord("Ctrl+Shift").is_err());     // 主キーなし(Shift は修飾)
        assert!(parse_chord("KeyA+KeyB").is_err());      // 主キー2つ
        assert!(parse_chord("Ctrl+Ctrl+KeyA").is_err()); // 修飾重複
        assert!(parse_chord("Meta+KeyA").is_err());      // 未知の修飾
        assert!(parse_chord("Ctrl+Enter").is_err());     // 予約キーは語彙外
        // Space は語彙に載る(単独許可の可否は validate_binding が判定)。
        assert_eq!(format_chord(&parse_chord("Shift+Space").unwrap()), "Shift+Space");
    }

    #[test]
    fn func_groups_match_spec() {
        use KeymapFunc::*;
        // spec §6: global(PreservedKey)/idle/composing の3グループ。
        for f in [ModeToggle, Reconvert, Feedback] {
            assert_eq!(f.group(), FuncGroup::Global);
            assert!(f.is_preserved());
            assert!(f.alt_allowed(), "PreservedKey 経路は Alt 可");
        }
        for f in [Ephemeral, CommitUndo] {
            assert_eq!(f.group(), FuncGroup::Idle);
            assert!(!f.alt_allowed(), "キーシンク経路は Alt 不可(WM_SYSKEYDOWN 不達疑い)");
        }
        for f in [TypoCorrect, LlmConvert, ToHiragana, ToKatakana, ToHankakuKana, ToZenkakuEisu, ToHankakuEisu] {
            assert_eq!(f.group(), FuncGroup::Composing);
            assert!(!f.alt_allowed());
        }
    }

    #[test]
    fn default_chords_mirror_current_hardcoded_keys() {
        use KeymapFunc::*;
        let f8 = KeyChord { vk: 0x77, ctrl: false, shift: false, alt: false };
        assert_eq!(default_chords(Ephemeral, "f8"), vec![f8]);
        assert_eq!(default_chords(Ephemeral, "f9"), vec![KeyChord { vk: 0x78, ..f8 }]);
        assert_eq!(default_chords(Ephemeral, "unknown"), vec![f8], "未知の旧 trigger は F8 へ");
        assert_eq!(default_chords(CommitUndo, "f8"),
                   vec![KeyChord { vk: 0x08, ctrl: true, shift: false, alt: false }]);
        assert_eq!(default_chords(TypoCorrect, "f8"),
                   vec![KeyChord { vk: 0x09, ctrl: false, shift: false, alt: false }]);
        assert_eq!(default_chords(LlmConvert, "f8"),
                   vec![KeyChord { vk: 0x09, ctrl: false, shift: true, alt: false }]);
        assert_eq!(default_chords(ToHiragana, "f8"), vec![KeyChord { vk: 0x75, ..f8 }]);
        assert_eq!(default_chords(ToHankakuEisu, "f8"), vec![KeyChord { vk: 0x79, ..f8 }]);
        // Preserved 系は JIS/US の 2 チョード(text_service.rs の現行登録と同値)。
        assert_eq!(default_chords(ModeToggle, "f8"), vec![
            KeyChord { vk: 0x1D, ctrl: false, shift: false, alt: false },
            KeyChord { vk: 0xBA, ctrl: false, shift: false, alt: true },
        ]);
        assert_eq!(default_chords(Reconvert, "f8"), vec![
            KeyChord { vk: 0x1C, ctrl: false, shift: false, alt: false },
            KeyChord { vk: 0xBF, ctrl: false, shift: false, alt: true },
        ]);
        assert_eq!(default_chords(Feedback, "f8"), vec![
            KeyChord { vk: 0x1C, ctrl: true, shift: false, alt: false },
            KeyChord { vk: 0xBF, ctrl: true, shift: false, alt: false },
        ]);
    }

    #[test]
    fn validate_binding_enforces_alt_and_ctrl_rules() {
        use KeymapFunc::*;
        assert!(validate_binding(CommitUndo, "none").is_ok());
        assert!(validate_binding(CommitUndo, "Ctrl+KeyZ").is_ok());
        assert!(validate_binding(CommitUndo, "F5").is_ok());       // F キーは単独可
        assert!(validate_binding(ModeToggle, "Alt+KeyJ").is_ok()); // Preserved は Alt 可
        // キーシンク経路への Alt は拒否(spec §4)。
        assert!(validate_binding(CommitUndo, "Alt+KeyZ").is_err());
        assert!(validate_binding(ToKatakana, "Alt+F7").is_err());
        // 文字/数字/記号は Ctrl 必須(Shift のみ不可 — Shift+英字=直接入力を影で奪うため)。
        assert!(validate_binding(CommitUndo, "KeyZ").is_err());
        assert!(validate_binding(CommitUndo, "Shift+KeyZ").is_err());
        assert!(validate_binding(CommitUndo, "Digit3").is_err());
        assert!(validate_binding(CommitUndo, "Semicolon").is_err());
        assert!(validate_binding(CommitUndo, "Ctrl+Shift+KeyZ").is_ok());
        // Tab/Backspace は単独可(現行既定が単独 Tab / Ctrl+Back のため)。
        assert!(validate_binding(TypoCorrect, "Tab").is_ok());
        assert!(validate_binding(CommitUndo, "Backspace").is_ok());
        // Space: 単独不可・修飾付きなら可。英字と違い Shift 単独修飾も可
        // (Shift+Space=IME トグルの定番。一時かな/モードトグルへの要望 — 2026-07-18)。
        assert!(validate_binding(Ephemeral, "Space").is_err());
        assert!(validate_binding(Ephemeral, "Shift+Space").is_ok());
        assert!(validate_binding(Ephemeral, "Ctrl+Space").is_ok());
        assert!(validate_binding(ModeToggle, "Shift+Space").is_ok());
        assert!(validate_binding(ModeToggle, "Alt+Space").is_ok()); // Preserved は Alt 可
        assert!(validate_binding(Ephemeral, "Alt+Space").is_err()); // キーシンク経路は Alt 不可
        // 語彙外はパース時点で拒否。
        assert!(validate_binding(CommitUndo, "Ctrl+Enter").is_err());
    }

    #[test]
    fn conflicts_forbid_same_chord_within_group_and_global_vs_all() {
        use KeymapFunc::*;
        let none: Option<String> = None;
        let all_default = || ALL_FUNCS.map(|f| (f, none.clone())).to_vec();
        // 既定同士は無衝突(F8 の idle/composing 二毛作は cross-group で許容 — spec §6)。
        assert!(find_conflicts_in(&all_default(), "f8", true, false, true, true).is_empty());
        // 同一グループ(composing)内の重複=衝突: to_hiragana を F7 にすると to_katakana(既定 F7) と衝突。
        let mut e = all_default();
        e.iter_mut().find(|(f, _)| *f == ToHiragana).unwrap().1 = Some("F7".into());
        let c = find_conflicts_in(&e, "f8", true, false, true, true);
        assert_eq!(c.len(), 1);
        assert!(matches!((c[0].a, c[0].b), (ToHiragana, ToKatakana) | (ToKatakana, ToHiragana)));
        // グループ跨ぎは許容: commit_undo(idle) を F7 にしても to_katakana(composing) と衝突しない。
        let mut e = all_default();
        e.iter_mut().find(|(f, _)| *f == CommitUndo).unwrap().1 = Some("F7".into());
        assert!(find_conflicts_in(&e, "f8", true, false, true, true).is_empty());
        // Global は全グループと衝突: mode_toggle を F7 にすると to_katakana と衝突。
        let mut e = all_default();
        e.iter_mut().find(|(f, _)| *f == ModeToggle).unwrap().1 = Some("F7".into());
        assert_eq!(find_conflicts_in(&e, "f8", true, false, true, true).len(), 1);
        // 無効化した機能・feature off の機能はチョードを持たない=衝突しない。
        let mut e = all_default();
        e.iter_mut().find(|(f, _)| *f == ToKatakana).unwrap().1 = Some("none".into());
        e.iter_mut().find(|(f, _)| *f == ToHiragana).unwrap().1 = Some("F7".into());
        assert!(find_conflicts_in(&e, "f8", true, false, true, true).is_empty());
        // feedback は既定 Ctrl+Slash を持つが feedback_enabled=false なら不参加。
        let mut e = all_default();
        e.iter_mut().find(|(f, _)| *f == TypoCorrect).unwrap().1 = Some("Ctrl+Slash".into());
        assert!(find_conflicts_in(&e, "f8", true, false, true, true).is_empty(), "feedback off なら Ctrl+Slash は空いている");
        assert_eq!(find_conflicts_in(&e, "f8", true, true, true, true).len(), 1, "feedback on なら Global 衝突");
    }

    #[test]
    fn keymap_settings_get_maps_every_func_to_its_field() {
        // clippy::field_reassign_with_default 回避のため struct update 構文で組み立てる
        // (ブリーフの reassign 形と等価)。
        let km = KeymapSettings { llm_convert: Some("F2".into()), ..Default::default() };
        assert_eq!(km.get(KeymapFunc::LlmConvert).as_deref(), Some("F2"));
        assert_eq!(*km.get(KeymapFunc::ModeToggle), None);
        // 公開ラッパ find_conflicts は find_conflicts_in と同じ結果を返す。
        let km = KeymapSettings { to_hiragana: Some("F7".into()), ..Default::default() };
        assert_eq!(find_conflicts(&km, "f8", true, false, true, true).len(), 1);
    }
}
