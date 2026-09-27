//! ローマ字かな変換の純粋部分（表と規則判定）。
//!
//! `LocalKanaComposer`（crates/tip）と混在入力の読み再合成 `synthesize` が同じ表と
//! 規則を使う（計画書 4.1: 単純な別実装を作って二重管理しない）。composer 側の
//! 状態（pending・journal・カーソル凍結）はここに持たせない。

/// composer の `push_with_resolver` と同じ解決器の型。
pub type RomanResolver = fn(&str) -> Option<(&'static str, &'static str)>;

pub fn lookup_roman(input: &str) -> Option<(&'static str, &'static str)> {
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

pub fn is_roman_prefix(input: &str) -> bool {
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

/// composer の pending 先頭をどう確定するかの純粋判定。`LocalKanaComposer` の
/// `advance_pending` と読み再合成 `synthesize` が共有する。判定だけを行い、
/// composer の stable / journal への反映は呼び出し側の責務。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RomanStep {
    /// 未完ローマ字。これ以上消費しない（composer の pending 保持と同じ）。
    Hold,
    /// pending 先頭 `drain` バイトを、元打鍵 `original` → かな `kana` の1 unit として
    /// 確定する。
    Unit {
        original: String,
        kana: &'static str,
        drain: usize,
    },
    /// 先頭文字をローマ字として解釈せずそのまま確定する。`alphabetic` は ASCII 英字か
    /// （composer は Direct 染色と automatic_literals 簿記に使う）。
    Literal { ch: char, alphabetic: bool },
}

pub fn pending_step(pending: &str, resolve: RomanResolver) -> RomanStep {
    if pending.is_empty() || pending == "ny" {
        return RomanStep::Hold;
    }
    if pending.starts_with("nn") {
        return RomanStep::Unit {
            original: "nn".to_string(),
            kana: "ん",
            drain: 2,
        };
    }
    if let Some((roman, kana)) = resolve(pending) {
        return RomanStep::Unit {
            original: roman.to_string(),
            kana,
            drain: roman.len(),
        };
    }
    let mut chars = pending.chars();
    let first = chars.next().expect("pending is non-empty");
    let second = chars.next();
    if first == 'n' && second.is_some() {
        return RomanStep::Unit {
            original: "n".to_string(),
            kana: "ん",
            drain: 1,
        };
    }
    if first != 'n'
        && first.is_ascii_alphabetic()
        && second == Some(first)
        && !matches!(first, 'a' | 'i' | 'u' | 'e' | 'o')
    {
        return RomanStep::Unit {
            original: first.to_string(),
            kana: "っ",
            drain: 1,
        };
    }
    if is_roman_prefix(pending) {
        return RomanStep::Hold;
    }
    RomanStep::Literal {
        ch: first,
        alphabetic: first.is_ascii_alphabetic(),
    }
}

/// 読み再合成の1 unit（元打鍵とそのかな）。`original` の連結は入力全体と一致する。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RomanUnit {
    pub original: String,
    pub kana: String,
}

/// ローマ字生文字列から読みを再合成する。Plan の Japanese 区間をその区間の原文に
/// 適用するときに使い、composer-unit 境界と独立な区間境界でも成立する（例:
/// `pythonno` の `nn` unit 内部で切って `no` → `の` を得る）。
///
/// `finalize_tail` が true のときだけ、末尾に残った単発 `n` を既存の確定規則
/// （`finalize_pending_n` と同じ `n` → `ん`）で完成させる。false では composer の
/// pending と同様にそのまま返す（未完ローマ字を完成済みとして扱わない、計画書 5.1）。
pub fn synthesize(raw: &str, finalize_tail: bool) -> Vec<RomanUnit> {
    let mut units = Vec::new();
    let mut pending = String::new();
    for ch in raw.chars() {
        pending.push(ch);
        loop {
            if pending.is_empty() {
                break;
            }
            match pending_step(&pending, lookup_roman) {
                RomanStep::Hold => break,
                RomanStep::Unit {
                    original,
                    kana,
                    drain,
                } => {
                    units.push(RomanUnit {
                        original,
                        kana: kana.to_string(),
                    });
                    pending.drain(..drain);
                }
                RomanStep::Literal { ch, .. } => {
                    units.push(RomanUnit {
                        original: ch.to_string(),
                        kana: ch.to_string(),
                    });
                    pending.drain(..ch.len_utf8());
                }
            }
        }
    }
    if !pending.is_empty() {
        if finalize_tail && pending == "n" {
            units.push(RomanUnit {
                original: "n".to_string(),
                kana: "ん".to_string(),
            });
        } else {
            let text = pending.clone();
            units.push(RomanUnit {
                original: text.clone(),
                kana: text,
            });
        }
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(raw: &str, finalize_tail: bool) -> String {
        synthesize(raw, finalize_tail)
            .iter()
            .map(|unit| unit.kana.as_str())
            .collect()
    }

    fn originals(raw: &str, finalize_tail: bool) -> String {
        synthesize(raw, finalize_tail)
            .iter()
            .map(|unit| unit.original.as_str())
            .collect()
    }

    #[test]
    fn matches_the_composer_differential_corpus() {
        // crates/tip の differential_corpus_matches_pinned_azookey_readings と同じ
        // 入力群（AzooKey pin 由来）。再合成が composer と同じ読みを出す最小固定。
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
            ("va", "ゔぁ"),
        ];
        for (input, expected) in cases {
            assert_eq!(reading(input, false), expected, "input={input:?}");
            assert_eq!(
                originals(input, false),
                input,
                "unit 元打鍵の連結は入力と一致: {input:?}"
            );
        }
    }

    #[test]
    fn unit_shapes_pin_nn_geminate_and_passthrough() {
        assert_eq!(
            synthesize("nn", false),
            vec![RomanUnit {
                original: "nn".to_string(),
                kana: "ん".to_string(),
            }]
        );
        assert_eq!(
            synthesize("tta", false),
            vec![
                RomanUnit {
                    original: "t".to_string(),
                    kana: "っ".to_string(),
                },
                RomanUnit {
                    original: "ta".to_string(),
                    kana: "た".to_string(),
                },
            ],
            "促音の元打鍵は先頭1文字だけ。2文字目は次 unit（ta）に属す"
        );
        assert_eq!(
            synthesize("va", false),
            vec![RomanUnit {
                original: "va".to_string(),
                kana: "ゔぁ".to_string(),
            }],
            "va → ゔぁ は同長でも1つの変換 unit"
        );
        assert_eq!(
            synthesize("き1", false),
            vec![
                RomanUnit {
                    original: "き".to_string(),
                    kana: "き".to_string(),
                },
                RomanUnit {
                    original: "1".to_string(),
                    kana: "1".to_string(),
                },
            ],
            "非ローマ字はそのまま通す（composer の可視読みと同じ）"
        );
    }

    #[test]
    fn trailing_pending_n_stays_unfinished_at_the_source_end() {
        assert_eq!(reading("hon", false), "ほn");
        assert_eq!(
            reading("ky", false),
            "ky",
            "n 1文字でない未完末尾は確定しない"
        );
    }

    #[test]
    fn trailing_n_finalizes_only_on_request() {
        assert_eq!(reading("hon", true), "ほん");
        assert_eq!(
            reading("ny", true),
            "ny",
            "finalize_pending_n と同じく n 1文字だけ"
        );
        assert_eq!(
            reading("won", true),
            "をん",
            "日本語→Literal 境界の末尾 n 完成に使う"
        );
    }

    #[test]
    fn n_before_a_consonant_resolves_inside_the_text() {
        assert_eq!(reading("nk", false), "んk");
        assert_eq!(reading("konpyuta", true), "こんぴゅた");
    }
}
