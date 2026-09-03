//! Local roman-to-kana composer that owns the TIP's canonical reading.

use unicode_segmentation::UnicodeSegmentation;

/// How a typed character participates in a composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputStyle {
    Kana,
    Direct,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplaySegment {
    pub text: String,
    pub style: InputStyle,
}

/// Incrementally composes the reading that local kana conversion would produce.
#[derive(Default)]
pub struct LocalKanaComposer {
    stable: String,
    stable_segments: Vec<ReplaySegment>,
    pending: String,
    reading: String,
}

impl LocalKanaComposer {
    /// Adds one character using the supplied input style.
    pub fn push(&mut self, ch: char, style: InputStyle) {
        self.push_with_resolver(ch, style, lookup_roman);
    }

    fn push_with_resolver(
        &mut self,
        ch: char,
        style: InputStyle,
        resolve: fn(&str) -> Option<(&'static str, &'static str)>,
    ) {
        let stable_len = self.stable.len();
        let pending_len = pending_display_len(&self.pending);
        self.reading.truncate(self.reading.len() - pending_len);
        match style {
            InputStyle::Kana => {
                self.pending.push(ch);
                self.advance_pending(resolve);
            }
            InputStyle::Direct => {
                // pending を Kana のまま stable へ送ると、状態保持エンジンが roman2kana で
                // この文字を後続入力と再結合し、ローカル読みと分岐する。ローカル側はこの境界で
                // 結合を終えているため、Direct リテラルとして凍結して送る。
                let pending = std::mem::take(&mut self.pending);
                self.append_stable(&pending, InputStyle::Direct);
                self.append_stable(&ch.to_string(), InputStyle::Direct);
            }
        }
        self.reading.push_str(&self.stable[stable_len..]);
        append_pending(&mut self.reading, &self.pending);
    }

    /// Removes the most recently typed character, if present.
    pub fn backspace(&mut self) {
        if self.pending.pop().is_some() {
            self.refresh_reading();
            return;
        }
        let retained_len = self
            .reading
            .graphemes(true)
            .next_back()
            .map_or(0, |cluster| self.reading.len() - cluster.len());
        self.truncate_stable(retained_len);
        self.refresh_reading();
    }

    /// Clears the current composition.
    pub fn clear(&mut self) {
        self.stable.clear();
        self.stable_segments.clear();
        self.pending.clear();
        self.reading.clear();
    }

    /// Replaces the input after the engine has partially committed a prefix.
    pub fn reseed_reading(&mut self, reading: &str) {
        self.stable.clear();
        self.stable_segments.clear();
        self.append_stable(reading, InputStyle::Kana);
        self.pending.clear();
        self.refresh_reading();
    }

    /// Retains a visible suffix after a prefix commit without resolving an unfinished roman tail.
    pub fn retain_suffix(&mut self, suffix: &str) -> bool {
        let Some((stable_suffix, stable_segments)) = self.retained_stable_suffix(suffix) else {
            return false;
        };
        self.stable = stable_suffix.to_owned();
        self.stable_segments = stable_segments;
        self.refresh_reading();
        true
    }

    /// Reports whether a suffix can be retained without changing the composition.
    pub fn can_retain_suffix(&self, suffix: &str) -> bool {
        self.retained_stable_suffix(suffix).is_some()
    }

    /// Returns styled segments that replay the current visible reading.
    pub fn replay_segments(&self) -> Vec<ReplaySegment> {
        let mut segments = self.stable_segments.clone();
        append_segment(&mut segments, &self.pending, InputStyle::Kana);
        segments
    }

    /// Returns the current canonical reading.
    pub fn reading(&self) -> &str {
        &self.reading
    }

    /// Returns the reading split at the unfinished-roman boundary. Direct and
    /// already-frozen ASCII live in `stable`, so only true roman pending is
    /// reported as pending — callers must not guess this from ASCII suffixes.
    pub(crate) fn reading_parts(&self) -> (&str, &str) {
        (&self.stable, &self.pending)
    }

    fn advance_pending(&mut self, resolve: fn(&str) -> Option<(&'static str, &'static str)>) {
        loop {
            if self.pending.is_empty() {
                return;
            }
            if self.pending == "ny" {
                return;
            }
            if self.pending.starts_with("nn") {
                self.append_stable("ん", InputStyle::Kana);
                self.pending.drain(..2);
                continue;
            }
            if let Some((roman, kana)) = resolve(&self.pending) {
                self.append_stable(kana, InputStyle::Kana);
                self.pending.drain(..roman.len());
                continue;
            }

            let mut chars = self.pending.chars();
            let first = chars.next().expect("pending is non-empty");
            let second = chars.next();
            if first == 'n' && second.is_some() {
                self.append_stable("ん", InputStyle::Kana);
                self.pending.remove(0);
                continue;
            }
            if first != 'n'
                && first.is_ascii_alphabetic()
                && second == Some(first)
                && !matches!(first, 'a' | 'i' | 'u' | 'e' | 'o')
            {
                self.append_stable("っ", InputStyle::Kana);
                self.pending.remove(0);
                continue;
            }
            if is_roman_prefix(&self.pending) {
                return;
            }
            // Replaying a rejected ASCII letter as Kana would let a later key combine with it
            // again, even though this composer has already frozen it as a literal.
            let literal_style = if first.is_ascii_alphabetic() {
                InputStyle::Direct
            } else {
                InputStyle::Kana
            };
            self.append_stable(&first.to_string(), literal_style);
            self.pending.drain(..first.len_utf8());
        }
    }

    fn refresh_reading(&mut self) {
        self.reading.clone_from(&self.stable);
        append_pending(&mut self.reading, &self.pending);
    }

    fn append_stable(&mut self, text: &str, style: InputStyle) {
        self.stable.push_str(text);
        append_segment(&mut self.stable_segments, text, style);
    }

    fn truncate_stable(&mut self, len: usize) {
        self.stable.truncate(len);
        let mut remaining = len;
        for segment in &mut self.stable_segments {
            if remaining == 0 {
                segment.text.clear();
            } else if segment.text.len() > remaining {
                segment.text.truncate(remaining);
                remaining = 0;
            } else {
                remaining -= segment.text.len();
            }
        }
        self.stable_segments
            .retain(|segment| !segment.text.is_empty());
    }

    fn stable_suffix_segments(&self, suffix_len: usize) -> Option<Vec<ReplaySegment>> {
        if suffix_len == 0 {
            return Some(Vec::new());
        }
        let start = self.stable.len().checked_sub(suffix_len)?;
        if !self.stable.is_char_boundary(start) || !is_grapheme_boundary(&self.stable, start) {
            return None;
        }

        let mut offset = 0;
        let mut retained = Vec::new();
        for segment in &self.stable_segments {
            let end = offset + segment.text.len();
            if end > start {
                let segment_start = start.saturating_sub(offset);
                if !segment.text.is_char_boundary(segment_start) {
                    return None;
                }
                append_segment(&mut retained, &segment.text[segment_start..], segment.style);
            }
            offset = end;
        }
        (offset == self.stable.len()).then_some(retained)
    }

    fn retained_stable_suffix<'a>(&self, suffix: &'a str) -> Option<(&'a str, Vec<ReplaySegment>)> {
        if suffix.is_empty() || !self.reading.ends_with(suffix) {
            return None;
        }
        let stable_suffix = if self.pending.is_empty() {
            suffix
        } else {
            suffix.strip_suffix(&self.pending)?
        };
        if !self.stable.ends_with(stable_suffix) {
            return None;
        }
        let stable_segments = self.stable_suffix_segments(stable_suffix.len())?;
        Some((stable_suffix, stable_segments))
    }
}

fn is_grapheme_boundary(text: &str, index: usize) -> bool {
    index == 0
        || index == text.len()
        || text.grapheme_indices(true).any(|(start, _)| start == index)
}

fn append_segment(segments: &mut Vec<ReplaySegment>, text: &str, style: InputStyle) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = segments.last_mut().filter(|last| last.style == style) {
        last.text.push_str(text);
    } else {
        segments.push(ReplaySegment {
            text: text.to_owned(),
            style,
        });
    }
}

fn pending_display_len(pending: &str) -> usize {
    pending.len()
}

fn append_pending(output: &mut String, pending: &str) {
    output.push_str(pending);
}

fn lookup_roman(input: &str) -> Option<(&'static str, &'static str)> {
    let fast = match input {
        "a" => Some(("a", "あ")),
        "i" => Some(("i", "い")),
        "u" => Some(("u", "う")),
        "e" => Some(("e", "え")),
        "o" => Some(("o", "お")),
        "ka" => Some(("ka", "か")),
        "ki" => Some(("ki", "き")),
        "ku" => Some(("ku", "く")),
        "ke" => Some(("ke", "け")),
        "ko" => Some(("ko", "こ")),
        "sa" => Some(("sa", "さ")),
        "si" => Some(("si", "し")),
        "su" => Some(("su", "す")),
        "se" => Some(("se", "せ")),
        "so" => Some(("so", "そ")),
        "ta" => Some(("ta", "た")),
        "ti" => Some(("ti", "ち")),
        "tu" => Some(("tu", "つ")),
        "te" => Some(("te", "て")),
        "to" => Some(("to", "と")),
        "na" => Some(("na", "な")),
        "ni" => Some(("ni", "に")),
        "nu" => Some(("nu", "ぬ")),
        "ne" => Some(("ne", "ね")),
        "no" => Some(("no", "の")),
        _ => None,
    };
    fast.or_else(|| {
        ROMAN_KANA
            .iter()
            .filter(|(roman, _)| input.starts_with(*roman))
            .max_by_key(|(roman, _)| roman.len())
            .copied()
    })
}

fn is_roman_prefix(input: &str) -> bool {
    (input.len() == 1 && input.as_bytes()[0].is_ascii_alphabetic())
        || ROMAN_KANA.iter().any(|(roman, _)| roman.starts_with(input))
}

const ROMAN_KANA: &[(&str, &str)] = &[
    ("ltsu", "っ"),
    ("xtsu", "っ"),
    ("ltu", "っ"),
    ("xtu", "っ"),
    ("lya", "ゃ"),
    ("lyu", "ゅ"),
    ("lyo", "ょ"),
    ("xya", "ゃ"),
    ("xyu", "ゅ"),
    ("xyo", "ょ"),
    ("xn", "ん"),
    ("xwa", "ゎ"),
    ("lwa", "ゎ"),
    ("xka", "ゕ"),
    ("lka", "ゕ"),
    ("xke", "ゖ"),
    ("lke", "ゖ"),
    ("wyi", "ゐ"),
    ("wye", "ゑ"),
    ("ye", "いぇ"),
    ("va", "ゔぁ"),
    ("vi", "ゔぃ"),
    ("vu", "ゔ"),
    ("ve", "ゔぇ"),
    ("vo", "ゔぉ"),
    ("kye", "きぇ"),
    ("gye", "ぎぇ"),
    ("qa", "くぁ"),
    ("qwa", "くぁ"),
    ("qi", "くぃ"),
    ("qwi", "くぃ"),
    ("qu", "くぅ"),
    ("kwu", "くぅ"),
    ("qwu", "くぅ"),
    ("qe", "くぇ"),
    ("qwe", "くぇ"),
    ("qo", "くぉ"),
    ("qwo", "くぉ"),
    ("kya", "きゃ"),
    ("kyu", "きゅ"),
    ("kyo", "きょ"),
    ("gya", "ぎゃ"),
    ("gyu", "ぎゅ"),
    ("gyo", "ぎょ"),
    ("sha", "しゃ"),
    ("shu", "しゅ"),
    ("sho", "しょ"),
    ("she", "しぇ"),
    ("sye", "しぇ"),
    ("sya", "しゃ"),
    ("syu", "しゅ"),
    ("syo", "しょ"),
    ("zya", "じゃ"),
    ("zyu", "じゅ"),
    ("zyo", "じょ"),
    ("jya", "じゃ"),
    ("jyu", "じゅ"),
    ("jyo", "じょ"),
    ("ja", "じゃ"),
    ("ju", "じゅ"),
    ("je", "じぇ"),
    ("jo", "じょ"),
    ("jyi", "じぃ"),
    ("zye", "じぇ"),
    ("jye", "じぇ"),
    ("swa", "すぁ"),
    ("swi", "すぃ"),
    ("swu", "すぅ"),
    ("swe", "すぇ"),
    ("swo", "すぉ"),
    ("cha", "ちゃ"),
    ("chu", "ちゅ"),
    ("cho", "ちょ"),
    ("cya", "ちゃ"),
    ("cyu", "ちゅ"),
    ("cyo", "ちょ"),
    ("tya", "ちゃ"),
    ("tyu", "ちゅ"),
    ("tyo", "ちょ"),
    ("tyi", "ちぃ"),
    ("cyi", "ちぃ"),
    ("che", "ちぇ"),
    ("cye", "ちぇ"),
    ("tye", "ちぇ"),
    ("dya", "ぢゃ"),
    ("dyu", "ぢゅ"),
    ("dyo", "ぢょ"),
    ("dyi", "ぢぃ"),
    ("dye", "ぢぇ"),
    ("tha", "てゃ"),
    ("thu", "てゅ"),
    ("the", "てぇ"),
    ("tho", "てょ"),
    ("twa", "とぁ"),
    ("twi", "とぃ"),
    ("twe", "とぇ"),
    ("two", "とぉ"),
    ("dha", "でゃ"),
    ("dhu", "でゅ"),
    ("dhe", "でぇ"),
    ("dho", "でょ"),
    ("dwa", "どぁ"),
    ("dwi", "どぃ"),
    ("dwe", "どぇ"),
    ("dwo", "どぉ"),
    ("nya", "にゃ"),
    ("nyu", "にゅ"),
    ("nyo", "にょ"),
    ("nyi", "にぃ"),
    ("nye", "にぇ"),
    ("hya", "ひゃ"),
    ("hyu", "ひゅ"),
    ("hyo", "ひょ"),
    ("hyi", "ひぃ"),
    ("hye", "ひぇ"),
    ("bya", "びゃ"),
    ("byu", "びゅ"),
    ("byo", "びょ"),
    ("byi", "びぃ"),
    ("bye", "びぇ"),
    ("pya", "ぴゃ"),
    ("pyu", "ぴゅ"),
    ("pyo", "ぴょ"),
    ("pyi", "ぴぃ"),
    ("pye", "ぴぇ"),
    ("mya", "みゃ"),
    ("myu", "みゅ"),
    ("myo", "みょ"),
    ("myi", "みぃ"),
    ("mye", "みぇ"),
    ("rya", "りゃ"),
    ("ryu", "りゅ"),
    ("ryo", "りょ"),
    ("ryi", "りぃ"),
    ("rye", "りぇ"),
    ("fya", "ふゃ"),
    ("fyu", "ふゅ"),
    ("fyo", "ふょ"),
    ("fa", "ふぁ"),
    ("fi", "ふぃ"),
    ("fe", "ふぇ"),
    ("fo", "ふぉ"),
    ("hwa", "ふぁ"),
    ("hwi", "ふぃ"),
    ("hwe", "ふぇ"),
    ("hwo", "ふぉ"),
    ("tsa", "つぁ"),
    ("tsi", "つぃ"),
    ("tse", "つぇ"),
    ("tso", "つぉ"),
    ("kwa", "くぁ"),
    ("kwi", "くぃ"),
    ("kwe", "くぇ"),
    ("kwo", "くぉ"),
    ("gwa", "ぐぁ"),
    ("gwi", "ぐぃ"),
    ("gwe", "ぐぇ"),
    ("gwo", "ぐぉ"),
    ("gwu", "ぐぅ"),
    ("shi", "し"),
    ("chi", "ち"),
    ("tsu", "つ"),
    ("thi", "てぃ"),
    ("dhi", "でぃ"),
    ("twu", "とぅ"),
    ("dwu", "どぅ"),
    ("fwu", "ふぅ"),
    ("fwa", "ふぁ"),
    ("fwi", "ふぃ"),
    ("fwe", "ふぇ"),
    ("fwo", "ふぉ"),
    ("whi", "うぃ"),
    ("whu", "う"),
    ("wha", "うぁ"),
    ("whe", "うぇ"),
    ("who", "うぉ"),
    ("ca", "か"),
    ("ka", "か"),
    ("ki", "き"),
    ("cu", "く"),
    ("ku", "く"),
    ("ce", "せ"),
    ("ke", "け"),
    ("co", "こ"),
    ("ko", "こ"),
    ("ga", "が"),
    ("gi", "ぎ"),
    ("gu", "ぐ"),
    ("ge", "げ"),
    ("go", "ご"),
    ("ci", "し"),
    ("sa", "さ"),
    ("si", "し"),
    ("su", "す"),
    ("se", "せ"),
    ("so", "そ"),
    ("za", "ざ"),
    ("zi", "じ"),
    ("ji", "じ"),
    ("zu", "ず"),
    ("ze", "ぜ"),
    ("zo", "ぞ"),
    ("ta", "た"),
    ("ti", "ち"),
    ("tu", "つ"),
    ("te", "て"),
    ("to", "と"),
    ("da", "だ"),
    ("di", "ぢ"),
    ("du", "づ"),
    ("de", "で"),
    ("do", "ど"),
    ("na", "な"),
    ("ni", "に"),
    ("nu", "ぬ"),
    ("ne", "ね"),
    ("no", "の"),
    ("ha", "は"),
    ("hi", "ひ"),
    ("hu", "ふ"),
    ("fu", "ふ"),
    ("he", "へ"),
    ("ho", "ほ"),
    ("ba", "ば"),
    ("bi", "び"),
    ("bu", "ぶ"),
    ("be", "べ"),
    ("bo", "ぼ"),
    ("pa", "ぱ"),
    ("pi", "ぴ"),
    ("pu", "ぷ"),
    ("pe", "ぺ"),
    ("po", "ぽ"),
    ("ma", "ま"),
    ("mi", "み"),
    ("mu", "む"),
    ("me", "め"),
    ("mo", "も"),
    ("ya", "や"),
    ("yu", "ゆ"),
    ("yo", "よ"),
    ("ra", "ら"),
    ("ri", "り"),
    ("ru", "る"),
    ("re", "れ"),
    ("ro", "ろ"),
    ("wa", "わ"),
    ("wi", "うぃ"),
    ("we", "うぇ"),
    ("wo", "を"),
    ("la", "ぁ"),
    ("li", "ぃ"),
    ("lu", "ぅ"),
    ("le", "ぇ"),
    ("lo", "ぉ"),
    ("xa", "ぁ"),
    ("xi", "ぃ"),
    ("xu", "ぅ"),
    ("xe", "ぇ"),
    ("xo", "ぉ"),
    ("zh", "←"),
    ("zj", "↓"),
    ("zk", "↑"),
    ("zl", "→"),
    ("a", "あ"),
    ("i", "い"),
    ("u", "う"),
    ("wu", "う"),
    ("e", "え"),
    ("o", "お"),
];

#[cfg(test)]
pub(crate) fn mismatch_diagnostic(local: &str, azookey: &str) -> Option<String> {
    (local != azookey).then(|| {
        format!(
            "ev=local_kana_mismatch local_utf16={} azookey_utf16={}",
            local.encode_utf16().count(),
            azookey.encode_utf16().count()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{lookup_roman, mismatch_diagnostic, InputStyle, LocalKanaComposer, ReplaySegment};
    use crate::text_service::{
        arm_deferred_work_timer, observe_shadow_compare, ShadowMismatchAggregate,
    };
    use std::cell::RefCell;
    use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};

    #[test]
    fn completed_roman_sequence_has_the_pinned_azookey_reading() {
        let mut composer = LocalKanaComposer::default();
        for ch in "nihongo".chars() {
            composer.push(ch, InputStyle::Kana);
        }

        assert_eq!(composer.reading(), "にほんご");
    }

    #[test]
    fn legal_unfinished_roman_suffixes_remain_visible() {
        let mut consonant = LocalKanaComposer::default();
        consonant.push('k', InputStyle::Kana);
        assert_eq!(consonant.reading(), "k");

        let mut digraph = LocalKanaComposer::default();
        for ch in "ky".chars() {
            digraph.push(ch, InputStyle::Kana);
        }
        assert_eq!(digraph.reading(), "ky");

        let mut nasal = LocalKanaComposer::default();
        nasal.push('n', InputStyle::Kana);
        assert_eq!(nasal.reading(), "n");
    }

    #[test]
    fn differential_corpus_matches_pinned_azookey_readings() {
        // These literals were recorded from the pinned AzooKeyKanaKanjiConverter revision.
        let cases = [
            ("konnichiha", "こんいちは"),
            ("gakkou", "がっこう"),
            ("ryokou", "りょこう"),
            ("xya", "ゃ"),
            ("watashi。", "わたし。"),
            ("caceco", "かせこ"),
            ("qwe", "くぇ"),
            ("sye", "しぇ"),
            ("wyi", "ゐ"),
            ("xn", "ん"),
            ("zl", "→"),
            ("n。", "ん。"),
            ("nn", "ん"),
        ];

        for (input, expected) in cases {
            let mut composer = LocalKanaComposer::default();
            for ch in input.chars() {
                composer.push(ch, InputStyle::Kana);
            }
            assert_eq!(composer.reading(), expected, "input={input:?}");
        }
    }

    #[test]
    fn direct_input_and_backspace_match_pinned_azookey_readings() {
        let mut composer = LocalKanaComposer::default();
        for ch in "kyou".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.push('A', InputStyle::Direct);
        assert_eq!(composer.reading(), "きょうA");

        composer.backspace();
        assert_eq!(composer.reading(), "きょう");

        for ch in "sha".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.backspace();
        assert_eq!(composer.reading(), "きょうし");

        for ch in "kaki".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.backspace();
        assert_eq!(composer.reading(), "きょうしか");
    }

    #[test]
    fn pinned_n_rules_prefer_concrete_mappings() {
        let mut nn = LocalKanaComposer::default();
        for ch in "nn".chars() {
            nn.push(ch, InputStyle::Kana);
        }
        assert_eq!(nn.reading(), "ん");

        let mut ny = LocalKanaComposer::default();
        for ch in "ny".chars() {
            ny.push(ch, InputStyle::Kana);
        }
        assert_eq!(ny.reading(), "ny");
    }

    #[test]
    fn replay_segments_preserve_kana_pending_and_direct_styles() {
        let mut composer = LocalKanaComposer::default();
        for ch in "ka".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.push('A', InputStyle::Direct);
        composer.push('k', InputStyle::Kana);

        assert_eq!(
            composer.replay_segments(),
            vec![
                ReplaySegment {
                    text: "か".into(),
                    style: InputStyle::Kana,
                },
                ReplaySegment {
                    text: "A".into(),
                    style: InputStyle::Direct,
                },
                ReplaySegment {
                    text: "k".into(),
                    style: InputStyle::Kana,
                },
            ]
        );
    }

    #[test]
    fn backspace_keeps_nasal_pending_for_replay_and_recombination() {
        let mut composer = LocalKanaComposer::default();
        for ch in "ny".chars() {
            composer.push(ch, InputStyle::Kana);
        }

        composer.backspace();
        assert_eq!(composer.reading(), "n");
        assert_eq!(
            composer.replay_segments(),
            vec![ReplaySegment {
                text: "n".into(),
                style: InputStyle::Kana,
            }]
        );

        composer.push('a', InputStyle::Kana);
        assert_eq!(composer.reading(), "な");
        assert_eq!(
            composer.replay_segments(),
            vec![ReplaySegment {
                text: "な".into(),
                style: InputStyle::Kana,
            }]
        );
    }

    #[test]
    fn backspaced_n_recombines_with_y_and_resolves_before_a_consonant() {
        let mut recombiner = LocalKanaComposer::default();
        for ch in "ny".chars() {
            recombiner.push(ch, InputStyle::Kana);
        }
        recombiner.backspace();
        for ch in "yu".chars() {
            recombiner.push(ch, InputStyle::Kana);
        }
        assert_eq!(recombiner.reading(), "にゅ");

        let mut resolver = LocalKanaComposer::default();
        for ch in "ny".chars() {
            resolver.push(ch, InputStyle::Kana);
        }
        resolver.backspace();
        resolver.push('k', InputStyle::Kana);
        assert_eq!(resolver.reading(), "んk");
    }

    #[test]
    fn replay_keeps_a_frozen_ascii_literal_separate_from_the_next_kana_key() {
        let mut composer = LocalKanaComposer::default();
        for ch in "kq".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.backspace();

        let mut replayed = LocalKanaComposer::default();
        for segment in composer.replay_segments() {
            for ch in segment.text.chars() {
                replayed.push(ch, segment.style);
            }
        }
        composer.push('a', InputStyle::Kana);
        replayed.push('a', InputStyle::Kana);

        assert_eq!(composer.reading(), "kあ");
        assert_eq!(replayed.reading(), composer.reading());
    }

    #[test]
    fn direct_combining_grapheme_backspace_removes_the_whole_grapheme_from_replay() {
        let mut composer = LocalKanaComposer::default();
        for ch in ['x', 'e', '\u{301}'] {
            composer.push(ch, InputStyle::Direct);
        }

        composer.backspace();
        assert_eq!(composer.reading(), "x");
        assert_eq!(
            composer.replay_segments(),
            vec![ReplaySegment {
                text: "x".into(),
                style: InputStyle::Direct,
            }]
        );
    }

    #[test]
    fn retaining_suffix_does_not_split_a_direct_grapheme() {
        let mut composer = LocalKanaComposer::default();
        for ch in ['x', 'e', '\u{301}'] {
            composer.push(ch, InputStyle::Direct);
        }
        let before_segments = composer.replay_segments();

        assert!(!composer.can_retain_suffix("\u{301}"));
        assert!(!composer.retain_suffix("\u{301}"));
        assert_eq!(composer.replay_segments(), before_segments);
        assert_eq!(composer.reading(), "xe\u{301}");
    }

    #[test]
    fn retaining_a_direct_suffix_keeps_its_style() {
        let mut composer = LocalKanaComposer::default();
        composer.push('a', InputStyle::Kana);
        for ch in ['x', 'e', '\u{301}'] {
            composer.push(ch, InputStyle::Direct);
        }

        assert!(composer.retain_suffix("xe\u{301}"));
        assert_eq!(
            composer.replay_segments(),
            vec![ReplaySegment {
                text: "xe\u{301}".into(),
                style: InputStyle::Direct,
            }]
        );
    }

    #[test]
    fn retaining_a_suffix_that_cuts_pending_leaves_replay_state_unchanged() {
        let mut composer = LocalKanaComposer::default();
        for ch in "ky".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        let before_reading = composer.reading().to_owned();
        let before_segments = composer.replay_segments();

        assert!(!composer.can_retain_suffix("y"));
        assert!(!composer.retain_suffix("y"));
        assert_eq!(composer.reading(), before_reading);
        assert_eq!(composer.replay_segments(), before_segments);
    }

    #[test]
    fn retaining_pending_suffix_keeps_pending_for_the_next_roman_key() {
        let mut composer = LocalKanaComposer::default();
        for ch in "an".chars() {
            composer.push(ch, InputStyle::Kana);
        }

        assert!(composer.retain_suffix("n"));
        assert_eq!(composer.reading(), "n");
        composer.push('y', InputStyle::Kana);
        composer.push('u', InputStyle::Kana);
        assert_eq!(composer.reading(), "にゅ");
    }

    #[derive(Debug)]
    struct FixtureCase {
        name: String,
        operations: Vec<FixtureOperation>,
        trajectory: Vec<String>,
    }

    #[derive(Debug)]
    enum FixtureOperation {
        Kana(String),
        Direct(String),
        Backspace,
    }

    fn parse_fixture(fixture: &str) -> Result<Vec<FixtureCase>, String> {
        let mut cases = Vec::new();
        for (line_number, line) in fixture.lines().enumerate() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 3 || fields.iter().any(|field| field.is_empty()) {
                return Err(format!(
                    "fixture line {} must have three non-empty columns",
                    line_number + 1
                ));
            }
            let operations = fields[1]
                .split(',')
                .map(|operation| match operation {
                    "B" => Ok(FixtureOperation::Backspace),
                    operation
                        if operation
                            .strip_prefix("K:")
                            .is_some_and(|payload| !payload.is_empty()) =>
                    {
                        Ok(FixtureOperation::Kana(operation[2..].to_owned()))
                    }
                    operation
                        if operation
                            .strip_prefix("D:")
                            .is_some_and(|payload| !payload.is_empty()) =>
                    {
                        Ok(FixtureOperation::Direct(operation[2..].to_owned()))
                    }
                    _ => Err(format!(
                        "fixture line {} has unknown operation {operation:?}",
                        line_number + 1
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let trajectory: Vec<_> = fields[2].split('|').map(str::to_owned).collect();
            if trajectory.iter().any(String::is_empty) {
                return Err(format!(
                    "fixture line {} has an empty trajectory reading",
                    line_number + 1
                ));
            }
            let event_count: usize = operations
                .iter()
                .map(|operation| match operation {
                    FixtureOperation::Kana(payload) | FixtureOperation::Direct(payload) => {
                        payload.chars().count()
                    }
                    FixtureOperation::Backspace => 1,
                })
                .sum();
            if trajectory.len() != event_count {
                return Err(format!(
                    "fixture line {} has {} events but {} readings",
                    line_number + 1,
                    event_count,
                    trajectory.len()
                ));
            }
            cases.push(FixtureCase {
                name: fields[0].to_owned(),
                operations,
                trajectory,
            });
        }
        if cases.is_empty() {
            return Err("fixture has no cases".to_owned());
        }
        Ok(cases)
    }

    fn run_fixture(
        fixture: &str,
        test_resolver: Option<fn(&str) -> Option<(&'static str, &'static str)>>,
    ) -> Result<(), String> {
        for case in parse_fixture(fixture)? {
            let mut composer = LocalKanaComposer::default();
            let mut event = 0;
            for operation in &case.operations {
                match operation {
                    FixtureOperation::Kana(payload) => {
                        for ch in payload.chars() {
                            if let Some(resolve) = test_resolver {
                                composer.push_with_resolver(ch, InputStyle::Kana, resolve);
                            } else {
                                composer.push(ch, InputStyle::Kana);
                            }
                            assert_trajectory_reading(&case, event, &composer)?;
                            event += 1;
                        }
                    }
                    FixtureOperation::Direct(payload) => {
                        for ch in payload.chars() {
                            if let Some(resolve) = test_resolver {
                                composer.push_with_resolver(ch, InputStyle::Direct, resolve);
                            } else {
                                composer.push(ch, InputStyle::Direct);
                            }
                            assert_trajectory_reading(&case, event, &composer)?;
                            event += 1;
                        }
                    }
                    FixtureOperation::Backspace => {
                        composer.backspace();
                        assert_trajectory_reading(&case, event, &composer)?;
                        event += 1;
                    }
                }
            }
        }
        Ok(())
    }

    fn assert_trajectory_reading(
        case: &FixtureCase,
        event: usize,
        composer: &LocalKanaComposer,
    ) -> Result<(), String> {
        let actual = composer.reading();
        let expected = &case.trajectory[event];
        if actual != expected {
            return Err(format!(
                "fixture={} event={} actual={actual:?} expected={expected:?}",
                case.name,
                event + 1
            ));
        }
        let replayed = replay_segments_reading(composer);
        (replayed == actual).then_some(()).ok_or_else(|| {
            format!(
                "fixture={} event={} replayed={replayed:?} actual={actual:?}",
                case.name,
                event + 1
            )
        })
    }

    fn replay_segments_reading(composer: &LocalKanaComposer) -> String {
        let mut replayed = LocalKanaComposer::default();
        for segment in composer.replay_segments() {
            for ch in segment.text.chars() {
                replayed.push(ch, segment.style);
            }
        }
        replayed.reading().to_owned()
    }

    #[test]
    fn shared_pinned_azookey_fixture_matches_local_composer_trajectory() {
        run_fixture(
            include_str!("../../../fixtures/local-kana-parity.tsv"),
            None,
        )
        .unwrap();
    }

    #[test]
    fn fixture_rejects_malformed_non_comment_rows() {
        for malformed in [
            "missing\tK:a\n",
            "empty\tK:a\t\n",
            "unknown\tQ:a\ta\n",
            "# metadata\n",
        ] {
            assert!(parse_fixture(malformed).is_err(), "fixture={malformed:?}");
        }
    }

    #[test]
    fn shared_fixture_gate_rejects_an_injected_go_to_bo_rule() {
        assert!(run_fixture(
            include_str!("../../../fixtures/local-kana-parity.tsv"),
            Some(|input| {
                lookup_roman(input).map(|(roman, kana)| {
                    if roman == "go" {
                        (roman, "ぼ")
                    } else {
                        (roman, kana)
                    }
                })
            }),
        )
        .is_err());
    }

    #[test]
    #[ignore = "run with: cargo test --release -p nospacekey_tip local_kana_composer::tests::release_ten_thousand_key_performance_gate -- --ignored"]
    fn release_ten_thousand_key_performance_gate() {
        assert!(
            !cfg!(debug_assertions),
            "performance gate requires --release"
        );

        warm_observed_key_path();
        warm_timer_arm_path();
        let samples = run_ten_thousand_observed_keys();
        assert_p99_under_one_millisecond(&samples[..100], "short");
        assert_p99_under_one_millisecond(&samples[4_950..5_050], "medium");
        assert_p99_under_one_millisecond(&samples[9_900..], "long");
        assert_p99_under_one_millisecond(&run_timer_arm_samples(), "timer arm");
    }

    fn warm_observed_key_path() {
        let mut composer = LocalKanaComposer::default();
        let matches = RefCell::new(ShadowMismatchAggregate::default());
        let mismatches = RefCell::new(ShadowMismatchAggregate::default());
        for _ in 0..50 {
            composer.push('k', InputStyle::Kana);
            let _ = observe_shadow_compare(&matches, composer.reading(), composer.reading());
            composer.push('a', InputStyle::Kana);
            let _ = observe_shadow_compare(&mismatches, composer.reading(), "");
        }
    }

    fn warm_timer_arm_path() {
        for _ in 0..10 {
            let aggregate = RefCell::new(ShadowMismatchAggregate::default());
            let timer = std::cell::Cell::new(0);
            assert!(observe_shadow_compare(&aggregate, "local", "azookey"));
            assert!(arm_deferred_work_timer(&timer, true, || unsafe {
                SetTimer(None, 0, 1, None)
            }));
            unsafe { KillTimer(None, timer.get()) }.unwrap();
        }
    }

    fn run_ten_thousand_observed_keys() -> Vec<std::time::Duration> {
        let mut composer = LocalKanaComposer::default();
        let matches = RefCell::new(ShadowMismatchAggregate::default());
        let mismatches = RefCell::new(ShadowMismatchAggregate::default());
        let mut samples = Vec::with_capacity(10_000);
        for event in 0..10_000 {
            let started = std::time::Instant::now();
            composer.push(if event % 2 == 0 { 'k' } else { 'a' }, InputStyle::Kana);
            if event % 2 == 0 {
                assert!(!observe_shadow_compare(
                    &matches,
                    composer.reading(),
                    composer.reading()
                ));
            } else {
                let arm = observe_shadow_compare(&mismatches, composer.reading(), "");
                assert!(arm);
            }
            samples.push(started.elapsed());
        }
        assert_eq!(composer.reading().chars().count(), 5_000);
        assert!(mismatches.borrow().has_pending());
        samples
    }

    fn run_timer_arm_samples() -> Vec<std::time::Duration> {
        let mut samples = Vec::with_capacity(100);
        for _ in 0..100 {
            let aggregate = RefCell::new(ShadowMismatchAggregate::default());
            let timer = std::cell::Cell::new(0);
            let started = std::time::Instant::now();
            let arm = observe_shadow_compare(&aggregate, "local", "azookey");
            assert!(arm);
            assert!(arm_deferred_work_timer(&timer, arm, || unsafe {
                SetTimer(None, 0, 1, None)
            }));
            unsafe { KillTimer(None, timer.get()) }.unwrap();
            samples.push(started.elapsed());
        }
        samples
    }

    fn assert_p99_under_one_millisecond(samples: &[std::time::Duration], bucket: &str) {
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let p99 = sorted[98];
        assert!(
            p99 < std::time::Duration::from_millis(1),
            "{bucket} reading key-path p99 {p99:?} exceeded the 1ms Issue #30 gate"
        );
    }

    #[test]
    fn mismatch_diagnostic_excludes_input_bodies() {
        let event =
            mismatch_diagnostic("にほんご", "にぼんご").expect("different readings log once");

        assert_eq!(
            event,
            "ev=local_kana_mismatch local_utf16=4 azookey_utf16=4"
        );
        assert!(!event.contains("にほんご"));
        assert!(!event.contains("にぼんご"));
    }
}
