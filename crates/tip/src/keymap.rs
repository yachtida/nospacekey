//! 解決済み keymap(Activate 時に settings から構築)と、キーイベント毎の hot 判定。
//! ハードコード VK 比較(undo_hot_now / ephemeral_trigger_hot / Tab / F6-F10)の後継。

use settings::keymap::{default_chords, resolve_binding, Binding, KeyChord, KeymapFunc};
use windows::core::GUID;

/// F6-F10 表記変換の種別(vk 直参照の後継 — リマップ後は vk と種別が独立)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notation { Hiragana, Katakana, HankakuKana, ZenkakuEisu, HankakuEisu }

const NOTATIONS: [(KeymapFunc, Notation); 5] = [
    (KeymapFunc::ToHiragana, Notation::Hiragana),
    (KeymapFunc::ToKatakana, Notation::Katakana),
    (KeymapFunc::ToHankakuKana, Notation::HankakuKana),
    (KeymapFunc::ToZenkakuEisu, Notation::ZenkakuEisu),
    (KeymapFunc::ToHankakuEisu, Notation::HankakuEisu),
];

/// Activate 時に settings から解決した keymap。キーシンク経路の 9 機能は実効チョード
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
}

fn sink_chord(v: &Option<String>, f: KeymapFunc, legacy: &str) -> Option<KeyChord> {
    match resolve_binding(v) {
        Binding::Default => Some(default_chords(f, legacy)[0]),
        Binding::Disabled => None,
        Binding::Chord(c) => Some(c),
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

/// キーイベント 1 回分の keymap 判定。OnTestKeyDown / OnKeyDown の両入口が同じ値を計算し、
/// 「食うか」と実処理の一致(この repo の設計不変条件)を keymap 化後も保つ。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct KeyHots {
    pub undo: bool,
    pub ephemeral: bool,
    pub typo: bool,
    pub llm: bool,
    /// direct 無修飾 VK_CONVERT の再変換フォールバック(PreserveKey(0x1C) が OS に拒否される
    /// ための OnKeyDown 経路)。reconvert が既定のときだけ生かす — 明示リバインド時は
    /// PreservedKey 側が新チョードで受けるので組込キーは解放する。
    pub reconvert_fallback: bool,
    pub notation: Option<Notation>,
}

impl KeyHots {
    pub fn any(&self) -> bool {
        self.undo || self.ephemeral || self.typo || self.llm
            || self.reconvert_fallback || self.notation.is_some()
    }
}

pub struct HotsInput {
    pub vk: u32,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub composing: bool,
    pub showing: bool,
    pub direct: bool,
    pub undo_armed: bool,
    pub ephemeral_enabled: bool,
    pub typo_enabled: bool,
    pub llm_enabled: bool,
}

pub fn compute_hots(km: &Keymap, i: &HotsInput) -> KeyHots {
    let hit = |c: Option<KeyChord>| chord_hits(c, i.vk, i.ctrl, i.shift, i.alt);
    let idle = !i.composing && !i.showing;
    let notation = if i.composing && !i.direct {
        NOTATIONS.iter().enumerate()
            .find(|(idx, _)| hit(km.notations[*idx]))
            .map(|(_, (_, n))| *n)
    } else {
        None
    };
    KeyHots {
        undo: i.undo_armed && hit(km.commit_undo),
        ephemeral: i.ephemeral_enabled && i.direct && idle && hit(km.ephemeral),
        typo: i.typo_enabled && i.composing && !i.direct && hit(km.typo),
        llm: i.llm_enabled && i.composing && !i.direct && hit(km.llm),
        reconvert_fallback: i.direct && i.vk == 0x1C && !i.ctrl && !i.shift && !i.alt
            && km.reconvert == Binding::Default,
        notation,
    }
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
        GUID_PRESERVEDKEY_MODE_TOGGLE, GUID_PRESERVEDKEY_MODE_TOGGLE_US,
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

    fn input(vk: u32) -> HotsInput {
        HotsInput {
            vk, ctrl: false, shift: false, alt: false,
            composing: false, showing: false, direct: false,
            undo_armed: false, ephemeral_enabled: true, typo_enabled: true, llm_enabled: true,
        }
    }

    #[test]
    fn default_keymap_matches_current_hardcoded_behavior() {
        let km = Keymap::default();
        // 確定取消: armed + Ctrl+Backspace(idle)。
        let h = compute_hots(&km, &HotsInput { vk: 0x08, ctrl: true, undo_armed: true, ..input(0x08) });
        assert!(h.undo);
        // shift 併用は不発(チョードは修飾の完全一致 — 旧実装より厳格化は意図的)。
        let h = compute_hots(&km, &HotsInput { vk: 0x08, ctrl: true, shift: true, undo_armed: true, ..input(0x08) });
        assert!(!h.undo);
        // 一時かな: direct+idle の F8。
        let h = compute_hots(&km, &HotsInput { direct: true, ..input(0x77) });
        assert!(h.ephemeral);
        // composing 中の F8 は一時かなでなく半角カナ表記変換。
        let h = compute_hots(&km, &HotsInput { composing: true, ..input(0x77) });
        assert!(!h.ephemeral);
        assert_eq!(h.notation, Some(Notation::HankakuKana));
        // Tab 二毛作: 無 Shift=修正変換 / Shift=LLM(composing のみ)。
        let h = compute_hots(&km, &HotsInput { composing: true, ..input(0x09) });
        assert!(h.typo && !h.llm);
        let h = compute_hots(&km, &HotsInput { composing: true, shift: true, ..input(0x09) });
        assert!(h.llm && !h.typo);
        // feature off なら hot にならない(素通し — 旧 will_handle_gated の veto と同じ)。
        let h = compute_hots(&km, &HotsInput { composing: true, typo_enabled: false, ..input(0x09) });
        assert!(!h.typo);
        // 再変換フォールバック: direct 無修飾 VK_CONVERT、reconvert が既定のときだけ。
        let h = compute_hots(&km, &HotsInput { direct: true, ..input(0x1C) });
        assert!(h.reconvert_fallback);
        let h = compute_hots(&km, &HotsInput { direct: false, ..input(0x1C) });
        assert!(!h.reconvert_fallback);
    }

    #[test]
    fn remapped_keymap_moves_hot_to_new_chord_and_frees_old_key() {
        let mut s = settings::Settings::default();
        s.keymap.to_katakana = Some("F11".into());
        s.keymap.commit_undo = Some("Ctrl+Shift+KeyZ".into());
        let km = Keymap::from_settings(&s);
        // 旧キーは不発、新キーで発火。
        let h = compute_hots(&km, &HotsInput { composing: true, ..input(0x76) });
        assert_eq!(h.notation, None, "F7 は解放済み");
        let h = compute_hots(&km, &HotsInput { composing: true, ..input(0x7A) });
        assert_eq!(h.notation, Some(Notation::Katakana));
        let h = compute_hots(&km, &HotsInput { vk: 0x5A, ctrl: true, shift: true, undo_armed: true, ..input(0x5A) });
        assert!(h.undo);
        let h = compute_hots(&km, &HotsInput { vk: 0x08, ctrl: true, undo_armed: true, ..input(0x08) });
        assert!(!h.undo, "Ctrl+Backspace は解放済み");
    }

    #[test]
    fn disabled_and_legacy_trigger_resolve() {
        let mut s = settings::Settings::default();
        s.keymap.typo_correct = Some("none".into());
        s.ephemeral.trigger = "f9".into(); // 旧設定のフォールバック(keymap.ephemeral は None)
        let km = Keymap::from_settings(&s);
        let h = compute_hots(&km, &HotsInput { composing: true, ..input(0x09) });
        assert!(!h.typo, "無効化した機能は発火しない");
        let h = compute_hots(&km, &HotsInput { direct: true, ..input(0x78) });
        assert!(h.ephemeral, "旧 ephemeral.trigger=f9 が既定として生きる");
        // keymap.ephemeral が明示されれば旧設定より優先。
        let mut s2 = s.clone();
        s2.keymap.ephemeral = Some("F10".into());
        let km2 = Keymap::from_settings(&s2);
        assert!(!compute_hots(&km2, &HotsInput { direct: true, ..input(0x78) }).ephemeral);
        assert!(compute_hots(&km2, &HotsInput { direct: true, ..input(0x79) }).ephemeral);
    }

    #[test]
    fn preserved_regs_reflect_bindings() {
        use windows::Win32::UI::TextServices::{TF_MOD_ALT, TF_MOD_CONTROL};
        // 既定: JIS/US 二重登録 ×(toggle+reconvert)+ feedback は enabled 時のみ。
        let km = Keymap::default();
        let regs = build_preserved_regs(&km, false);
        assert_eq!(regs.len(), 4);
        assert!(regs.iter().any(|r| r.vk == 0x1D && r.modifiers == 0));
        assert!(regs.iter().any(|r| r.vk == 0xBA && r.modifiers == TF_MOD_ALT));
        let regs = build_preserved_regs(&km, true);
        assert_eq!(regs.len(), 6);
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
