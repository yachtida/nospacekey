//! 解決済み keymap(Activate 時に settings から構築)と、キーイベント毎の役割解決(resolve_action)。
//! ハードコード VK 比較(undo_hot_now / ephemeral_trigger_hot / Tab / F6-F10 / Space・変換 の henkan)の後継。

use settings::keymap::{default_chords, resolve_binding, Binding, KeyChord, KeymapFunc};
use windows::core::GUID;

/// F6-F10 表記変換の種別(vk 直参照の後継 — リマップ後は vk と種別が独立)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notation { Hiragana, Katakana, HankakuKana, ZenkakuEisu, HankakuEisu }

/// 半角/全角キーの VK 揺れ(0x19=VK_KANJI / 0xF4=VK_OEM_ENTER)を正準 0xF3 へ畳む。
/// 適用は生 vk の入口(on_test_key_down_impl / on_key_down_impl)で一度だけ(spec §3/§7.2)。
pub fn normalize_vk(vk: u32) -> u32 {
    match vk {
        0x19 | 0xF4 => 0xF3,
        _ => vk,
    }
}

/// NotationRotate の遷移(spec §4.1)。現在表記から「次」を導出する。
/// Why not 独立カウンタ: F6-F10 との併用で表示と巡回位置がズレる。表示中の表記
/// (notation_fixed)を唯一の状態にすれば、どの経路で表記が変わっても次の一手が表示と整合する。
pub fn next_notation(current: Option<Notation>) -> Notation {
    match current {
        Some(Notation::Katakana) => Notation::HankakuKana,
        Some(Notation::HankakuKana) => Notation::Hiragana,
        // None(ライブ変換=ひらがな起点) / Hiragana / 英数系 → カタカナで巡回に入る。
        _ => Notation::Katakana,
    }
}

const NOTATIONS: [(KeymapFunc, Notation); 5] = [
    (KeymapFunc::ToHiragana, Notation::Hiragana),
    (KeymapFunc::ToKatakana, Notation::Katakana),
    (KeymapFunc::ToHankakuKana, Notation::HankakuKana),
    (KeymapFunc::ToZenkakuEisu, Notation::ZenkakuEisu),
    (KeymapFunc::ToHankakuEisu, Notation::HankakuEisu),
];

/// Activate 時に settings から解決した keymap。キーシンク経路の 11 機能は実効チョード
/// (None=無効)、Preserved 系 3 機能は Binding のまま持つ(既定=JIS/US 二重登録と明示
/// バインドを registration 側で区別する必要があるため)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Keymap {
    pub mode_toggle: Binding,
    pub reconvert: Binding,
    pub feedback: Binding,
    ephemeral: Option<KeyChord>,
    commit_undo: Option<KeyChord>,
    typo: Option<KeyChord>,
    llm: Option<KeyChord>,
    notations: [Option<KeyChord>; 5], // NOTATIONS と同順
    notation_rotate: Option<KeyChord>,
    convert: [Option<KeyChord>; 2],
}

fn sink_chord(v: &Option<String>, f: KeymapFunc, legacy: &str) -> Option<KeyChord> {
    match resolve_binding(v) {
        Binding::Default => Some(default_chords(f, legacy)[0]),
        Binding::Disabled => None,
        Binding::Chord(c) => Some(c),
    }
}

fn convert_chords(v: &Option<String>) -> [Option<KeyChord>; 2] {
    match resolve_binding(v) {
        Binding::Default => {
            let d = default_chords(KeymapFunc::Convert, "");
            [Some(d[0]), Some(d[1])]
        }
        Binding::Disabled => [None, None],
        Binding::Chord(c) => [Some(c), None],
    }
}

impl Keymap {
    pub fn from_settings(s: &settings::Settings) -> Self {
        use KeymapFunc::*;
        let km = &s.keymap;
        let legacy = s.ephemeral.trigger.as_str();
        let mut notations = [None; 5];
        for (i, (f, _)) in NOTATIONS.iter().enumerate() {
            notations[i] = sink_chord(km.get(*f), *f, legacy);
        }
        Keymap {
            mode_toggle: resolve_binding(&km.mode_toggle),
            reconvert: resolve_binding(&km.reconvert),
            feedback: resolve_binding(&km.feedback),
            ephemeral: sink_chord(&km.ephemeral, Ephemeral, legacy),
            commit_undo: sink_chord(&km.commit_undo, CommitUndo, legacy),
            typo: sink_chord(&km.typo_correct, TypoCorrect, legacy),
            llm: sink_chord(&km.llm_convert, LlmConvert, legacy),
            notations,
            notation_rotate: sink_chord(&km.notation_rotate, NotationRotate, legacy),
            convert: convert_chords(&km.convert),
        }
    }
}

impl Default for Keymap {
    fn default() -> Self { Keymap::from_settings(&settings::Settings::default()) }
}

/// チョードは修飾の**完全一致**で照合する(旧 undo_hot_now の「ctrl && !alt」より厳格 —
/// Shift 併用を別バインドとして空けるための意図的な仕様変更。golden テストの許容差分)。
fn chord_hits(c: Option<KeyChord>, vk: u32, ctrl: bool, shift: bool, alt: bool) -> bool {
    matches!(c, Some(c) if c.vk == vk && c.ctrl == ctrl && c.shift == shift && c.alt == alt)
}

/// キー役割の解決結果(1 キーイベントにつき高々 1 個の相互排他アクション)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    None, Convert, Reconvert, ModeToggle, Ephemeral, CommitUndo, Typo, Llm, Notation(Notation),
    NotationRotate,
}

pub struct ActionInput {
    pub vk: u32, pub ctrl: bool, pub shift: bool, pub alt: bool,
    pub composing: bool, pub showing: bool, pub direct: bool,
    pub undo_armed: bool, pub ephemeral_enabled: bool, pub typo_enabled: bool, pub llm_enabled: bool,
}

/// Global 機能(binding)の実効チョードに bare_special な当該キーが含まれるか。
fn global_bare_special_hit(b: Binding, f: KeymapFunc, vk: u32, ctrl: bool, shift: bool, alt: bool) -> bool {
    if !settings::keymap::bare_special(vk, ctrl, shift, alt) { return false; }
    match b {
        Binding::Default => default_chords(f, "").iter()
            .any(|c| c.vk == vk && !c.ctrl && !c.shift && !c.alt),
        Binding::Chord(c) => c.vk == vk && !c.ctrl && !c.shift && !c.alt,
        Binding::Disabled => false,
    }
}

/// キーイベント 1 回分の役割解決。OnTestKeyDown / OnKeyDown の両入口が同じ値を計算し、
/// 「食うか」と実処理の一致(この repo の設計不変条件)を保つ(旧 KeyHots/compute_hots の相互排他版)。
pub fn resolve_action(km: &Keymap, i: &ActionInput) -> KeyAction {
    let idle = !i.composing && !i.showing;
    let hit = |c: Option<KeyChord>| chord_hits(c, i.vk, i.ctrl, i.shift, i.alt);

    // CommitUndo: armed のみ(idle ゲート無し=旧 KeyHots.undo と同一。golden 等価)。
    if i.undo_armed && hit(km.commit_undo) { return KeyAction::CommitUndo; }
    // Ephemeral: direct+idle。
    if i.ephemeral_enabled && i.direct && idle && hit(km.ephemeral) { return KeyAction::Ephemeral; }
    // Composing 系(native composing)。
    if i.composing && !i.direct {
        if i.typo_enabled && hit(km.typo) { return KeyAction::Typo; }
        if i.llm_enabled && hit(km.llm) { return KeyAction::Llm; }
        if let Some((_, (_, n))) = NOTATIONS.iter().enumerate().find(|(idx, _)| hit(km.notations[*idx])) {
            return KeyAction::Notation(*n);
        }
        if hit(km.notation_rotate) { return KeyAction::NotationRotate; }
    }
    // Convert(henkan): composing || showing、Convert の 2 チョードいずれか。
    // Global 救済より前に置く(spec §6.1: composing/showing では Composing 群の束縛が
    // Global 機能へのフォールバックより優先 — Convert も Composing 群)。既定チョードに
    // Global との重複は無いため既定挙動は不変。
    if (i.composing || i.showing)
        && km.convert.iter().any(|c| chord_hits(*c, i.vk, i.ctrl, i.shift, i.alt)) {
        return KeyAction::Convert;
    }
    // Global 系の bare_special 救済(ModeToggle=全文脈 / Reconvert=direct+idle)。
    if global_bare_special_hit(km.mode_toggle, KeymapFunc::ModeToggle, i.vk, i.ctrl, i.shift, i.alt) {
        return KeyAction::ModeToggle;
    }
    if i.direct && idle
        && global_bare_special_hit(km.reconvert, KeymapFunc::Reconvert, i.vk, i.ctrl, i.shift, i.alt) {
        return KeyAction::Reconvert;
    }
    KeyAction::None
}

/// Activate で OS に登録する PreservedKey の一覧(純関数 — 登録/解除の対称性はこの
/// 戻り値を保存して両方に使うことで保証する)。
pub struct PreservedReg {
    pub guid: GUID,
    pub vk: u32,
    pub modifiers: u32,
    pub desc: &'static str,
}

pub fn build_preserved_regs(km: &Keymap, feedback_enabled: bool) -> Vec<PreservedReg> {
    use crate::globals::{
        GUID_PRESERVEDKEY_FEEDBACK, GUID_PRESERVEDKEY_FEEDBACK_US,
        GUID_PRESERVEDKEY_MODE_TOGGLE, GUID_PRESERVEDKEY_MODE_TOGGLE_HZ,
        GUID_PRESERVEDKEY_MODE_TOGGLE_US,
        GUID_PRESERVEDKEY_RECONVERT, GUID_PRESERVEDKEY_RECONVERT_US,
    };
    use windows::Win32::UI::TextServices::{TF_MOD_ALT, TF_MOD_CONTROL, TF_MOD_SHIFT};
    fn mods(c: KeyChord) -> u32 {
        (if c.ctrl { TF_MOD_CONTROL } else { 0 })
            | (if c.shift { TF_MOD_SHIFT } else { 0 })
            | (if c.alt { TF_MOD_ALT } else { 0 })
    }
    let mut out = Vec::new();
    match km.mode_toggle {
        // 既定: JIS(無変換)+US(Alt+;) の現行二重登録。明示バインドは単一登録
        // (US GUID は使わない — classify_preserved_key は主 GUID だけで束ねられる)。
        Binding::Default => {
            out.push(PreservedReg { guid: GUID_PRESERVEDKEY_MODE_TOGGLE, vk: 0x1D, modifiers: 0, desc: "nospacekey mode toggle" });
            out.push(PreservedReg { guid: GUID_PRESERVEDKEY_MODE_TOGGLE_US, vk: 0xBA, modifiers: TF_MOD_ALT, desc: "nospacekey mode toggle (US)" });
            out.push(PreservedReg { guid: GUID_PRESERVEDKEY_MODE_TOGGLE_HZ, vk: 0xF3, modifiers: 0, desc: "nospacekey mode toggle (hankaku/zenkaku)" });
        }
        Binding::Chord(c) => out.push(PreservedReg { guid: GUID_PRESERVEDKEY_MODE_TOGGLE, vk: c.vk, modifiers: mods(c), desc: "nospacekey mode toggle" }),
        Binding::Disabled => {}
    }
    match km.reconvert {
        Binding::Default => {
            out.push(PreservedReg { guid: GUID_PRESERVEDKEY_RECONVERT, vk: 0x1C, modifiers: 0, desc: "nospacekey reconvert" });
            out.push(PreservedReg { guid: GUID_PRESERVEDKEY_RECONVERT_US, vk: 0xBF, modifiers: TF_MOD_ALT, desc: "nospacekey reconvert (US)" });
        }
        Binding::Chord(c) => out.push(PreservedReg { guid: GUID_PRESERVEDKEY_RECONVERT, vk: c.vk, modifiers: mods(c), desc: "nospacekey reconvert" }),
        Binding::Disabled => {}
    }
    if feedback_enabled {
        match km.feedback {
            Binding::Default => {
                out.push(PreservedReg { guid: GUID_PRESERVEDKEY_FEEDBACK, vk: 0x1C, modifiers: TF_MOD_CONTROL, desc: "nospacekey feedback" });
                out.push(PreservedReg { guid: GUID_PRESERVEDKEY_FEEDBACK_US, vk: 0xBF, modifiers: TF_MOD_CONTROL, desc: "nospacekey feedback (US)" });
            }
            Binding::Chord(c) => out.push(PreservedReg { guid: GUID_PRESERVEDKEY_FEEDBACK, vk: c.vk, modifiers: mods(c), desc: "nospacekey feedback" }),
            Binding::Disabled => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keymap_matches_current_hardcoded_behavior() {
        let km = Keymap::default();
        // 確定取消: armed + Ctrl+Backspace。
        assert_eq!(resolve_action(&km, &ActionInput { vk: 0x08, ctrl: true, undo_armed: true, ..ainput(0x08) }), KeyAction::CommitUndo);
        // shift 併用は不発(チョードは修飾の完全一致 — 旧実装より厳格化は意図的)。
        assert_eq!(resolve_action(&km, &ActionInput { vk: 0x08, ctrl: true, shift: true, undo_armed: true, ..ainput(0x08) }), KeyAction::None);
        // 一時かな: direct+idle の F8。
        assert_eq!(resolve_action(&km, &ActionInput { direct: true, ..ainput(0x77) }), KeyAction::Ephemeral);
        // composing 中の F8 は一時かなでなく半角カナ表記変換。
        assert_eq!(resolve_action(&km, &ActionInput { composing: true, ..ainput(0x77) }), KeyAction::Notation(Notation::HankakuKana));
        // Tab 二毛作: 無 Shift=修正変換 / Shift=LLM(composing のみ)。
        assert_eq!(resolve_action(&km, &ActionInput { composing: true, ..ainput(0x09) }), KeyAction::Typo);
        assert_eq!(resolve_action(&km, &ActionInput { composing: true, shift: true, ..ainput(0x09) }), KeyAction::Llm);
        // feature off なら発火しない(素通し — 旧 will_handle_gated の veto と同じ)。
        assert_eq!(resolve_action(&km, &ActionInput { composing: true, typo_enabled: false, ..ainput(0x09) }), KeyAction::None);
    }

    #[test]
    fn remapped_keymap_moves_action_to_new_chord_and_frees_old_key() {
        let mut s = settings::Settings::default();
        s.keymap.to_katakana = Some("F11".into());
        s.keymap.commit_undo = Some("Ctrl+Shift+KeyZ".into());
        let km = Keymap::from_settings(&s);
        // 旧キーは不発、新キーで発火。
        assert_eq!(resolve_action(&km, &ActionInput { composing: true, ..ainput(0x76) }), KeyAction::None, "F7 は解放済み");
        assert_eq!(resolve_action(&km, &ActionInput { composing: true, ..ainput(0x7A) }), KeyAction::Notation(Notation::Katakana));
        assert_eq!(resolve_action(&km, &ActionInput { vk: 0x5A, ctrl: true, shift: true, undo_armed: true, ..ainput(0x5A) }), KeyAction::CommitUndo);
        assert_eq!(resolve_action(&km, &ActionInput { vk: 0x08, ctrl: true, undo_armed: true, ..ainput(0x08) }), KeyAction::None, "Ctrl+Backspace は解放済み");
    }

    #[test]
    fn disabled_and_legacy_trigger_resolve() {
        let mut s = settings::Settings::default();
        s.keymap.typo_correct = Some("none".into());
        s.ephemeral.trigger = "f9".into(); // 旧設定のフォールバック(keymap.ephemeral は None)
        let km = Keymap::from_settings(&s);
        assert_eq!(resolve_action(&km, &ActionInput { composing: true, ..ainput(0x09) }), KeyAction::None, "無効化した機能は発火しない");
        assert_eq!(resolve_action(&km, &ActionInput { direct: true, ..ainput(0x78) }), KeyAction::Ephemeral, "旧 ephemeral.trigger=f9 が既定として生きる");
        // keymap.ephemeral が明示されれば旧設定より優先。
        let mut s2 = s.clone();
        s2.keymap.ephemeral = Some("F10".into());
        let km2 = Keymap::from_settings(&s2);
        assert_ne!(resolve_action(&km2, &ActionInput { direct: true, ..ainput(0x78) }), KeyAction::Ephemeral);
        assert_eq!(resolve_action(&km2, &ActionInput { direct: true, ..ainput(0x79) }), KeyAction::Ephemeral);
    }

    fn ainput(vk: u32) -> ActionInput {
        ActionInput {
            vk, ctrl: false, shift: false, alt: false,
            composing: false, showing: false, direct: false,
            undo_armed: false, ephemeral_enabled: true, typo_enabled: true, llm_enabled: true,
        }
    }

    #[test]
    fn normalize_vk_folds_hankaku_zenkaku_variants() {
        assert_eq!(normalize_vk(0x19), 0xF3, "VK_KANJI → 正準");
        assert_eq!(normalize_vk(0xF4), 0xF3, "VK_OEM_ENTER → 正準");
        assert_eq!(normalize_vk(0xF3), 0xF3);
        assert_eq!(normalize_vk(0x41), 0x41, "他 VK は不変");
    }

    #[test]
    fn next_notation_cycles_kana_three_states() {
        use Notation::*;
        assert_eq!(next_notation(None), Katakana, "新規合成はひらがな起点 → 次はカタカナ");
        assert_eq!(next_notation(Some(Hiragana)), Katakana);
        assert_eq!(next_notation(Some(Katakana)), HankakuKana);
        assert_eq!(next_notation(Some(HankakuKana)), Hiragana);
        assert_eq!(next_notation(Some(ZenkakuEisu)), Katakana, "英数からはカタカナで巡回に入る");
        assert_eq!(next_notation(Some(HankakuEisu)), Katakana);
    }

    #[test]
    fn notation_rotate_resolution_matrix() {
        let km = Keymap::default();
        // composing && !direct → NotationRotate(Composing ブロックが Global 救済より先)。
        assert_eq!(resolve_action(&km, &ActionInput { composing: true, ..ainput(0x1D) }),
                   KeyAction::NotationRotate);
        // idle → ModeToggle(従来どおり)。direct+composing → ModeToggle(!direct 述語で Rotate 不発)。
        assert_eq!(resolve_action(&km, &ainput(0x1D)), KeyAction::ModeToggle);
        assert_eq!(resolve_action(&km, &ActionInput { composing: true, direct: true, ..ainput(0x1D) }),
                   KeyAction::ModeToggle);
        // 半角/全角(正準 0xF3)は全文脈 ModeToggle(Composing 束縛が無いので Global フォールバック)。
        for st in [ainput(0xF3),
                   ActionInput { composing: true, ..ainput(0xF3) },
                   ActionInput { direct: true, ..ainput(0xF3) },
                   ActionInput { showing: true, ..ainput(0xF3) }] {
            assert_eq!(resolve_action(&km, &st), KeyAction::ModeToggle);
        }
        // Alt 併用の 0xF3 は bare でない → 不発(素通し。Alt+半角/全角は独立チョード扱い)。
        assert_eq!(resolve_action(&km, &ActionInput { alt: true, ..ainput(0xF3) }), KeyAction::None);
        // rotate="none" → composing 0x1D は ModeToggle へフォールバック(従来挙動へ戻す逃げ道)。
        let mut s = settings::Settings::default();
        s.keymap.notation_rotate = Some("none".into());
        let km2 = Keymap::from_settings(&s);
        assert_eq!(resolve_action(&km2, &ActionInput { composing: true, ..ainput(0x1D) }),
                   KeyAction::ModeToggle);
        // rotate=Ctrl+K リバインド → 0x1D composing は ModeToggle、Ctrl+K composing は Rotate。
        let mut s3 = settings::Settings::default();
        s3.keymap.notation_rotate = Some("Ctrl+KeyK".into());
        let km3 = Keymap::from_settings(&s3);
        assert_eq!(resolve_action(&km3, &ActionInput { composing: true, ..ainput(0x1D) }),
                   KeyAction::ModeToggle);
        assert_eq!(resolve_action(&km3, &ActionInput { composing: true, ctrl: true, ..ainput(0x4B) }),
                   KeyAction::NotationRotate);
        // mode_toggle="none" → 0x1D idle も 0xF3 も不発。
        let mut s4 = settings::Settings::default();
        s4.keymap.mode_toggle = Some("none".into());
        let km4 = Keymap::from_settings(&s4);
        assert_eq!(resolve_action(&km4, &ainput(0x1D)), KeyAction::None);
        assert_eq!(resolve_action(&km4, &ainput(0xF3)), KeyAction::None);
    }

    #[test]
    fn resolve_action_default_matrix() {
        let km = Keymap::default();
        // henkan: Space/変換 の native composing / showing。
        assert_eq!(resolve_action(&km, &ActionInput { composing: true, ..ainput(0x20) }), KeyAction::Convert);
        assert_eq!(resolve_action(&km, &ActionInput { showing: true, ..ainput(0x20) }), KeyAction::Convert);
        assert_eq!(resolve_action(&km, &ActionInput { composing: true, ..ainput(0x1C) }), KeyAction::Convert);
        assert_eq!(resolve_action(&km, &ActionInput { showing: true, ..ainput(0x1C) }), KeyAction::Convert);
        // 変換 direct+idle = Reconvert(ephemeral には落ちない)。
        assert_eq!(resolve_action(&km, &ActionInput { direct: true, ..ainput(0x1C) }), KeyAction::Reconvert);
        // native+idle の 変換/Space = None(素通し)。
        assert_eq!(resolve_action(&km, &ainput(0x1C)), KeyAction::None);
        assert_eq!(resolve_action(&km, &ainput(0x20)), KeyAction::None);
        // 無変換 bare: idle=ModeToggle / composing=NotationRotate(spec §6.1 — Composing 優先)。
        assert_eq!(resolve_action(&km, &ainput(0x1D)), KeyAction::ModeToggle);
        assert_eq!(resolve_action(&km, &ActionInput { composing: true, ..ainput(0x1D) }), KeyAction::NotationRotate);
        // 一時かなは F8(direct+idle)でのみ。変換で ephemeral には決してならない。
        assert_eq!(resolve_action(&km, &ActionInput { direct: true, ..ainput(0x77) }), KeyAction::Ephemeral);
        for st in [ainput(0x1C), ActionInput { direct: true, ..ainput(0x1C) }, ActionInput { composing: true, ..ainput(0x1C) }] {
            assert_ne!(resolve_action(&km, &st), KeyAction::Ephemeral);
        }
        // CommitUndo は idle ゲート無し(旧 KeyHots.undo と同じ)。
        assert_eq!(resolve_action(&km, &ActionInput { vk: 0x08, ctrl: true, undo_armed: true, composing: true, ..ainput(0x08) }), KeyAction::CommitUndo);
    }

    #[test]
    fn resolve_action_respects_rebinds() {
        let mut s = settings::Settings::default();
        s.keymap.convert = Some("none".into());
        let km = Keymap::from_settings(&s);
        assert_eq!(resolve_action(&km, &ActionInput { composing: true, ..ainput(0x20) }), KeyAction::None);
        assert_eq!(resolve_action(&km, &ActionInput { composing: true, ..ainput(0x1C) }), KeyAction::None);

        let mut s2 = settings::Settings::default();
        s2.keymap.reconvert = Some("Ctrl+KeyR".into());
        let km2 = Keymap::from_settings(&s2);
        assert_eq!(resolve_action(&km2, &ActionInput { direct: true, ..ainput(0x1C) }), KeyAction::None);

        let mut s3 = settings::Settings::default();
        s3.keymap.mode_toggle = Some("Ctrl+KeyM".into());
        let km3 = Keymap::from_settings(&s3);
        assert_eq!(resolve_action(&km3, &ainput(0x1D)), KeyAction::None);
    }

    #[test]
    fn preserved_regs_reflect_bindings() {
        use windows::Win32::UI::TextServices::{TF_MOD_ALT, TF_MOD_CONTROL};
        // 既定: JIS/US/半角全角 の 3 登録(toggle)+ JIS/US(reconvert)+ feedback は enabled 時のみ。
        let km = Keymap::default();
        let regs = build_preserved_regs(&km, false);
        assert_eq!(regs.len(), 5);
        assert!(regs.iter().any(|r| r.vk == 0x1D && r.modifiers == 0));
        assert!(regs.iter().any(|r| r.vk == 0xBA && r.modifiers == TF_MOD_ALT));
        assert!(regs.iter().any(|r| r.vk == 0xF3 && r.modifiers == 0), "半角/全角の第3登録");
        let regs = build_preserved_regs(&km, true);
        assert_eq!(regs.len(), 7);
        assert!(regs.iter().any(|r| r.vk == 0x1C && r.modifiers == TF_MOD_CONTROL));
        // 明示バインド: 単一登録(JIS/US 区別は既定専用の概念)。無効: 登録なし。
        let mut s = settings::Settings::default();
        s.keymap.mode_toggle = Some("Ctrl+KeyJ".into());
        s.keymap.reconvert = Some("none".into());
        let km = Keymap::from_settings(&s);
        let regs = build_preserved_regs(&km, false);
        assert_eq!(regs.len(), 1);
        assert_eq!((regs[0].vk, regs[0].modifiers), (0x4A, TF_MOD_CONTROL));
    }
}
