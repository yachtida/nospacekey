//! 学習・評価用の synthetic 入力生成（計画書 9.1）。
//!
//! 実IMEの受理規則に沿うローマ字表記（`shi/si`、`tsu/tu`、促音、拗音、末尾 n、
//! 大小文字、記号、未知語、typo）を含むエピソードを決定的に生成する。かな→
//! 一種類のローマ字変換で済ませない。単純な空白を境界ヒントとして入れない。
//! 同一テンプレート（文・語の由来）の派生は同じ split へ置く（計画書 9.2。
//! 全 prefix・編集派生を別 split に散らさない）。

use crate::classify::edge::{DictLayer, Dictionary};
use crate::plan::SegmentKind;

/// 生成器の乱数（決定的。xorshift64*）。再現性が評価の前提なので seed を
/// 呼出側で固定する。`romanize` が公開 API のため実装型も pub にする。
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

/// データ分割（計画書 9.2）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Split {
    Train,
    Validation,
    Frozen,
}

impl Split {
    pub fn as_str(self) -> &'static str {
        match self {
            Split::Train => "train",
            Split::Validation => "validation",
            Split::Frozen => "frozen",
        }
    }
}

/// 生成した1エピソード。fixture と同じ「source と期待解釈」の形。
#[derive(Clone, Debug)]
pub struct SynthEpisode {
    pub id: String,
    /// 由来テンプレート番号。派生（表記揺れ・混在・typo）は全て同じ番号を持ち、
    /// 同じ split へ置く（計画書 9.2）。
    pub template: usize,
    pub split: Split,
    pub category: SynthCategory,
    pub source: String,
    /// 隣接する同 kind は結合済みの期待区間。
    pub expected: Vec<(SegmentKind, String)>,
}

/// 評価区分（計画書 PR4 の完了条件: 一般語・短語・純日本語・未知語を別々に）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SynthCategory {
    PureJapanese,
    MixedGeneral,
    MixedTech,
    MixedUnknown,
    ShortPhrase,
    Typo,
}

impl SynthCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            SynthCategory::PureJapanese => "pure_japanese",
            SynthCategory::MixedGeneral => "mixed_general",
            SynthCategory::MixedTech => "mixed_tech",
            SynthCategory::MixedUnknown => "mixed_unknown",
            SynthCategory::ShortPhrase => "short_phrase",
            SynthCategory::Typo => "typo",
        }
    }
}

/// 学習対象の日本語文テンプレート（自作。出典: 本リポジトリ作成）。
const JA_TEMPLATES: &[&str] = &[
    "きょうはいいてんきです",
    "らいねんのまつりにいきます",
    "わたしのまちのほくとさいど",
    "にほんごをべんきょうしています",
    "こんばんはともだちとあそびます",
    "きてんしゃではこぼうをおきます",
    "しんぶんをよみながらこうちゃをのむ",
    "でんしゃにのっておおさかへいきます",
    "しょくじのあとさんぽをします",
    "かいぎしつでぷれぜんをします",
    "としょかんでほんをかります",
    "あしたはあめがふるそうです",
    "こうえんでこどもたちがあそんでいます",
    "えいがをみてからばんごはんをたべます",
    "よるおそくまでしごとをしました",
    "しんきぶんをかんがえています",
    "うんどうのあとみずをのみます",
    "かいとうらんぷをきりかえます",
    "せんたくものをとりこみます",
    "でんわでよやくをとりました",
    "かいぎのしりょうをさくせいします",
    "こうつうじこのちゅういをよみます",
    "まいあさしちじにおきます",
    "しょくじのまえにてをあらいます",
    "でんきをけしてねます",
    "ろくがつにけっこんしきをします",
    "くるまのうんてんをならいます",
    "きょうとはりょうあんじへいきます",
    "ぎんこうでこうざをひらきます",
    "しゅうかんしんぶんをこうにゅうします",
];

/// 短語（2–3 モーラ）。境界情報が乏しい分割の評価用。
const JA_SHORT: &[&str] = &[
    "かれ", "みず", "ねこ", "とり", "うみ", "やま", "はな", "ほし", "ゆき", "はこ",
];

/// 一般語辞書（データ资产。出典・ライセンスはファイルヘッダが正本）。
const GENERAL_DICT_TEXT: &str = include_str!("../../data/dictionary/general.txt");
/// 技術用語辞書（一般語と別レイヤー）。
const TECH_DICT_TEXT: &str = include_str!("../../data/dictionary/tech.txt");

fn dict_words(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
}

/// 未知語（辞書に入れない holdout。Literal 語彙単位の未知語評価用）。
const UNKNOWN_WORDS: &[&str] = &[
    "flarnby", "quizzle", "vronkite", "plimware", "zanthor", "bleenox", "travok", "mimble",
    "gorlane", "snevik",
];

/// 既定の辞書（一般語 + 技術用語）。未知語は入れない。
pub fn default_dictionary() -> Dictionary {
    let mut dictionary = Dictionary::new();
    for word in dict_words(GENERAL_DICT_TEXT) {
        dictionary.insert(word, DictLayer::General);
    }
    for word in dict_words(TECH_DICT_TEXT) {
        dictionary.insert(word, DictLayer::Tech);
    }
    dictionary
}

/// 辞書の一般語・技術用語（synthetic 生成の語選択と同じ語彙を使う）。
pub fn dictionary_words(layer: DictLayer) -> Vec<String> {
    let text = match layer {
        DictLayer::General => GENERAL_DICT_TEXT,
        DictLayer::Tech => TECH_DICT_TEXT,
    };
    dict_words(text).map(str::to_string).collect()
}

/// 基本かな表。複数表記は実IMEが受理するもの（訓令・ヘボン混在）。
const KANA_TABLE: &[(char, &[&str])] = &[
    ('あ', &["a"]),
    ('い', &["i"]),
    ('う', &["u", "wu"]),
    ('え', &["e"]),
    ('お', &["o"]),
    ('か', &["ka", "ca"]),
    ('き', &["ki"]),
    ('く', &["ku", "cu"]),
    ('け', &["ke"]),
    ('こ', &["ko", "co"]),
    ('さ', &["sa"]),
    ('し', &["shi", "si"]),
    ('す', &["su"]),
    ('せ', &["se", "ce"]),
    ('そ', &["so"]),
    ('た', &["ta"]),
    ('ち', &["chi", "ti"]),
    ('つ', &["tsu", "tu"]),
    ('て', &["te"]),
    ('と', &["to"]),
    ('な', &["na"]),
    ('に', &["ni"]),
    ('ぬ', &["nu"]),
    ('ね', &["ne"]),
    ('の', &["no"]),
    ('は', &["ha"]),
    ('ひ', &["hi"]),
    ('ふ', &["fu", "hu"]),
    ('へ', &["he"]),
    ('ほ', &["ho"]),
    ('ま', &["ma"]),
    ('み', &["mi"]),
    ('む', &["mu"]),
    ('め', &["me"]),
    ('も', &["mo"]),
    ('や', &["ya"]),
    ('ゆ', &["yu"]),
    ('よ', &["yo"]),
    ('ら', &["ra"]),
    ('り', &["ri"]),
    ('る', &["ru"]),
    ('れ', &["re"]),
    ('ろ', &["ro"]),
    ('わ', &["wa"]),
    ('を', &["wo"]),
    ('ん', &["nn", "xn"]),
    ('が', &["ga"]),
    ('ぎ', &["gi"]),
    ('ぐ', &["gu"]),
    ('げ', &["ge"]),
    ('ご', &["go"]),
    ('ざ', &["za"]),
    ('じ', &["ji", "zi"]),
    ('ず', &["zu"]),
    ('ぜ', &["ze"]),
    ('ぞ', &["zo"]),
    ('だ', &["da"]),
    ('ぢ', &["di"]),
    ('づ', &["du"]),
    ('で', &["de"]),
    ('ど', &["do"]),
    ('ば', &["ba"]),
    ('び', &["bi"]),
    ('ぶ', &["bu"]),
    ('べ', &["be"]),
    ('ぼ', &["bo"]),
    ('ぱ', &["pa"]),
    ('ぴ', &["pi"]),
    ('ぷ', &["pu"]),
    ('ぺ', &["pe"]),
    ('ぽ', &["po"]),
];

fn romans_of(kana: char) -> Option<&'static [&'static str]> {
    KANA_TABLE
        .iter()
        .find(|(ch, _)| *ch == kana)
        .map(|(_, r)| *r)
}

/// カナ → ローマ字。実IMEの受理規則に沿う複数表記から選ぶ。
/// 拗音・促音は2文字のかたまりとして先に消費する。
fn kana_to_roman(kana: char, next: Option<char>, rng: &mut Rng) -> (String, usize) {
    if kana == 'っ' {
        // 促音: 次のかなの先頭子音を重ねる（母音・y・n・w の前では xtu）。
        if let Some(next) = next.and_then(romans_of) {
            let first = next[0].chars().next().unwrap();
            if !"ayuiwon".contains(first) {
                return (first.to_string(), 1);
            }
        }
        return ("xtu".to_string(), 1);
    }
    let pick = |variants: &[&str], rng: &mut Rng| variants[rng.below(variants.len())].to_string();
    match (kana, next) {
        (
            base @ ('き' | 'し' | 'ち' | 'に' | 'ひ' | 'み' | 'り'),
            Some(small @ ('ゃ' | 'ゅ' | 'ょ')),
        ) => {
            let variants: &[&str] = match (base, small) {
                ('き', 'ゃ') => &["kya"],
                ('き', 'ゅ') => &["kyu"],
                ('き', 'ょ') => &["kyo"],
                ('し', 'ゃ') => &["sha", "sya"],
                ('し', 'ゅ') => &["shu", "syu"],
                ('し', 'ょ') => &["sho", "syo"],
                ('ち', 'ゃ') => &["cha", "tya"],
                ('ち', 'ゅ') => &["chu", "tyu"],
                ('ち', 'ょ') => &["cho", "tyo"],
                ('に', 'ゃ') => &["nya"],
                ('に', 'ゅ') => &["nyu"],
                ('に', 'ょ') => &["nyo"],
                ('ひ', 'ゃ') => &["hya"],
                ('ひ', 'ゅ') => &["hyu"],
                ('ひ', 'ょ') => &["hyo"],
                ('み', 'ゃ') => &["mya"],
                ('み', 'ゅ') => &["myu"],
                ('み', 'ょ') => &["myo"],
                ('り', 'ゃ') => &["rya"],
                ('り', 'ゅ') => &["ryu"],
                _ => &["ryo"],
            };
            (pick(variants, rng), 2)
        }
        _ => match romans_of(kana) {
            Some(variants) => (pick(variants, rng), 1),
            None => (kana.to_string(), 1),
        },
    }
}

/// かな文をローマ字へ変換する（表記の揺れは rng で決定的に選ぶ）。
pub fn romanize(kana: &str, rng: &mut Rng) -> String {
    let chars: Vec<char> = kana.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    while index < chars.len() {
        let (roman, consumed) = kana_to_roman(chars[index], chars.get(index + 1).copied(), rng);
        out.push_str(&roman);
        index += consumed;
    }
    out
}

/// テンプレート番号 → split（固定。派生が別 split に散らない）。3 で割って
/// train/validation/frozen の各テンプレート数が揃うようにする。混在カテゴリは
/// (template / 3) % 3 で決めるため、全カテゴリが全 split に現れる
/// （frozen で一般語・短語・純日本語・未知語を別々に評価できる）。
fn split_of(template: usize) -> Split {
    match template % 3 {
        0 => Split::Train,
        1 => Split::Validation,
        _ => Split::Frozen,
    }
}

/// 未知語（辞書に入れない holdout）を split ごとに切り分ける（計画書 9.2:
/// Literal 語彙単位の holdout。train の未知語が frozen に漏れない）。
fn unknown_words_for(split: Split) -> &'static [&'static str] {
    match split {
        Split::Train => &UNKNOWN_WORDS[0..4],
        Split::Validation => &UNKNOWN_WORDS[4..7],
        Split::Frozen => &UNKNOWN_WORDS[7..],
    }
}

/// 生成設定。seed を変えると別の表記揺れ・語選択になる。
#[derive(Clone, Debug)]
pub struct SynthConfig {
    pub seed: u64,
}

impl Default for SynthConfig {
    fn default() -> Self {
        SynthConfig { seed: 42 }
    }
}

/// synthetic エピソード群を生成する。
pub fn generate(config: &SynthConfig) -> Vec<SynthEpisode> {
    let mut rng = Rng::new(config.seed);
    let mut episodes = Vec::new();

    for (template, kana) in JA_TEMPLATES.iter().enumerate() {
        let split = split_of(template);
        // 純日本語（表記揺れ2種）。
        for variant in 0..2 {
            let source = romanize(kana, &mut rng);
            episodes.push(SynthEpisode {
                id: format!("ja-{template}-{variant}"),
                template,
                split,
                category: SynthCategory::PureJapanese,
                expected: vec![(SegmentKind::Japanese, source.clone())],
                source,
            });
        }
        // 混在（一般語・技術用語・未知語を1語、語境界に挿入）。カテゴリは
        // (template / 3) % 3 で決め、全 split に全カテゴリが現れるようにする。
        let general = dictionary_words(DictLayer::General);
        let tech = dictionary_words(DictLayer::Tech);
        let (word, category) = match (template / 3) % 3 {
            0 => (
                general[rng.below(general.len())].clone(),
                SynthCategory::MixedGeneral,
            ),
            1 => (
                tech[rng.below(tech.len())].clone(),
                SynthCategory::MixedTech,
            ),
            _ => {
                let holdout = unknown_words_for(split);
                (
                    holdout[rng.below(holdout.len())].to_string(),
                    SynthCategory::MixedUnknown,
                )
            }
        };
        let front_kana: String = kana.chars().take(5).collect();
        let back_kana: String = kana.chars().skip(5).collect();
        let front = romanize(&front_kana, &mut rng);
        let back = romanize(&back_kana, &mut rng);
        let source = format!("{front}{word}{back}");
        let mut expected = Vec::new();
        if !front.is_empty() {
            expected.push((SegmentKind::Japanese, front.clone()));
        }
        expected.push((SegmentKind::Literal, word.to_string()));
        if !back.is_empty() {
            expected.push((SegmentKind::Japanese, back.clone()));
        }
        episodes.push(SynthEpisode {
            id: format!("mixed-{template}"),
            template,
            split,
            category,
            source,
            expected,
        });

        // 大小文字・数字のバリエーション（ja_upper / lit_upper / lit_digit 特徴の
        // 検証用）。語頭を大文字化するか、数字接尾辞を付ける。
        {
            let styled = if word.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && template % 2 == 0
            {
                let mut chars = word.chars();
                let head = chars.next().unwrap().to_ascii_uppercase();
                format!("{head}{}", chars.as_str())
            } else {
                format!("{word}2")
            };
            let source = format!("{front}{styled}{back}");
            let mut expected = Vec::new();
            if !front.is_empty() {
                expected.push((SegmentKind::Japanese, front.clone()));
            }
            expected.push((SegmentKind::Literal, styled.clone()));
            if !back.is_empty() {
                expected.push((SegmentKind::Japanese, back.clone()));
            }
            episodes.push(SynthEpisode {
                id: format!("mixed-style-{template}"),
                template,
                split,
                category,
                source,
                expected,
            });
        }

        // 短語 + 辞書語（2モーラ程度の日本語区間の評価）。
        if template % 2 == 0 {
            let short = JA_SHORT[rng.below(JA_SHORT.len())];
            let word = tech[rng.below(tech.len())].clone();
            let front = romanize(short, &mut rng);
            episodes.push(SynthEpisode {
                id: format!("short-{template}"),
                template,
                split,
                category: SynthCategory::ShortPhrase,
                source: format!("{front}{word}"),
                expected: vec![(SegmentKind::Japanese, front), (SegmentKind::Literal, word)],
            });
        }

        // 末尾の未完 n（finalize_n 特徴の検証用）。読みの末尾に単発 n が残る
        // 入力で、解釈は全体日本語のまま（未完は Plan の終端状態に残る）。
        if template % 4 == 3 {
            let head_kana: String = kana.chars().take(kana.chars().count() - 1).collect();
            let mut source = romanize(&head_kana, &mut rng);
            source.push('n');
            episodes.push(SynthEpisode {
                id: format!("tailn-{template}"),
                template,
                split,
                category: SynthCategory::PureJapanese,
                source: source.clone(),
                expected: vec![(SegmentKind::Japanese, source)],
            });
        }

        // CapsLock 由来の大文字ローマ字（ja_upper 特徴の検証用）。大文字を含んでも
        // 日本語辺は小文字へ戻して読み合成するので、解釈は全体日本語のまま。
        if template % 4 == 1 {
            let base = romanize(kana, &mut rng);
            let source: String = base
                .chars()
                .enumerate()
                .map(|(at, ch)| {
                    (at % 5 == 2 && ch.is_ascii_lowercase())
                        .then(|| ch.to_ascii_uppercase())
                        .unwrap_or(ch)
                })
                .collect();
            episodes.push(SynthEpisode {
                id: format!("caps-{template}"),
                template,
                split,
                category: SynthCategory::PureJapanese,
                source: source.clone(),
                expected: vec![(SegmentKind::Japanese, source)],
            });
        }

        // typo（1文字欠落の純日本語。解釈は全部日本語のまま）。
        if template % 5 == 0 {
            let mut roman = romanize(kana, &mut rng);
            let drop_at = rng.below(roman.chars().count());
            roman = roman
                .chars()
                .enumerate()
                .filter(|(i, _)| *i != drop_at)
                .map(|(_, c)| c)
                .collect();
            episodes.push(SynthEpisode {
                id: format!("typo-{template}"),
                template,
                split,
                category: SynthCategory::Typo,
                source: roman.clone(),
                expected: vec![(SegmentKind::Japanese, roman)],
            });
        }
    }
    episodes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_deterministic_per_seed() {
        let a = generate(&SynthConfig { seed: 7 });
        let b = generate(&SynthConfig { seed: 7 });
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.source, y.source);
            assert_eq!(x.expected, y.expected);
            assert_eq!(x.split, y.split);
        }
    }

    #[test]
    fn romanization_covers_ime_accepted_variants() {
        let mut rng = Rng::new(1);
        let mut seen_shi = false;
        let mut seen_si = false;
        for _ in 0..200 {
            let roman = romanize("し", &mut rng);
            seen_shi |= roman == "shi";
            seen_si |= roman == "si";
        }
        assert!(seen_shi && seen_si, "し は shi/si 両方の表記が出る");
        let mut rng = Rng::new(2);
        let romans: Vec<String> = (0..50).map(|_| romanize("しって", &mut rng)).collect();
        assert!(
            romans
                .iter()
                .all(|r| r.starts_with("shit") || r.starts_with("sit")),
            "促音が次子音を重ねる: {romans:?}"
        );
        let mut rng = Rng::new(3);
        let kyou = romanize("きょう", &mut rng);
        assert!(
            kyou == "kyou" || kyou == "kyowu",
            "きょう は きょ+う の合成: {kyou}"
        );
    }

    #[test]
    fn same_template_derivations_share_a_split() {
        let episodes = generate(&SynthConfig::default());
        for template in 0..JA_TEMPLATES.len() {
            let splits: Vec<Split> = episodes
                .iter()
                .filter(|e| e.template == template)
                .map(|e| e.split)
                .collect();
            assert!(!splits.is_empty());
            assert!(
                splits.iter().all(|s| *s == splits[0]),
                "template {template} の派生が別 split に散った: {splits:?}"
            );
        }
    }

    #[test]
    fn expected_spans_concatenate_to_the_source() {
        for episode in generate(&SynthConfig { seed: 3 }) {
            let joined: String = episode.expected.iter().map(|(_, t)| t.as_str()).collect();
            assert_eq!(
                joined, episode.source,
                "{}: 期待区間の結合が source と一致",
                episode.id
            );
        }
    }

    #[test]
    fn unknown_words_stay_out_of_the_default_dictionary() {
        let dictionary = default_dictionary();
        for word in UNKNOWN_WORDS {
            assert!(
                dictionary.layer_of(word).is_none(),
                "{word} は holdout のまま"
            );
        }
        assert!(dictionary.layer_of("github").is_some());
    }

    #[test]
    fn every_category_appears_in_every_split() {
        // 各 split で一般語・短語・純日本語・未知語・typo を別々に評価できる
        // （PR4 の完了条件。frozen にだけ揃っても不十分）。
        let episodes = generate(&SynthConfig::default());
        for split in [Split::Train, Split::Validation, Split::Frozen] {
            for category in [
                SynthCategory::PureJapanese,
                SynthCategory::MixedGeneral,
                SynthCategory::MixedTech,
                SynthCategory::MixedUnknown,
                SynthCategory::ShortPhrase,
                SynthCategory::Typo,
            ] {
                assert!(
                    episodes
                        .iter()
                        .any(|e| e.split == split && e.category == category),
                    "{split:?} に {} がない",
                    category.as_str()
                );
            }
        }
    }

    #[test]
    fn unknown_holdout_words_do_not_cross_splits() {
        // Literal 語彙単位の holdout: split 間で未知語が共有されない。
        let episodes = generate(&SynthConfig::default());
        let unknown_words = |split: Split| -> Vec<String> {
            episodes
                .iter()
                .filter(|e| e.split == split && e.category == SynthCategory::MixedUnknown)
                .filter_map(|e| {
                    e.expected
                        .iter()
                        .find(|(kind, _)| *kind == SegmentKind::Literal)
                        .map(|(_, text)| text.clone())
                })
                .collect()
        };
        let train = unknown_words(Split::Train);
        let validation = unknown_words(Split::Validation);
        let frozen = unknown_words(Split::Frozen);
        assert!(!train.is_empty() && !validation.is_empty() && !frozen.is_empty());
        for word in train {
            assert!(
                !validation.contains(&word) && !frozen.contains(&word),
                "{word} が split をまたいでいる"
            );
        }
    }

    #[test]
    fn generated_sources_cover_uppercase_digits_and_trailing_n() {
        let episodes = generate(&SynthConfig::default());
        assert!(
            episodes
                .iter()
                .any(|e| e.source.chars().any(|c| c.is_ascii_uppercase())),
            "大文字を含む source がある"
        );
        // CapsLock 由来: 大文字を含みながら期待が全部日本語の episode が各 split にある。
        for split in [Split::Train, Split::Validation, Split::Frozen] {
            assert!(
                episodes.iter().any(|e| e.split == split
                    && e.category == SynthCategory::PureJapanese
                    && e.source.chars().any(|c| c.is_ascii_uppercase())),
                "{split:?} に CapsLock 純日本語がない"
            );
        }
        assert!(
            episodes
                .iter()
                .any(|e| e.source.chars().any(|c| c.is_ascii_digit())),
            "数字を含む source がある"
        );
        assert!(
            episodes.iter().any(|e| e.source.ends_with('n')
                && e.expected.iter().all(|(k, _)| *k == SegmentKind::Japanese)),
            "末尾未完 n の source がある"
        );
    }
}
