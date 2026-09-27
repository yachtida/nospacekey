//! 文字 n-gram の対数尤度モデル（計画書 5.2）。
//!
//! 日本語ローマ字らしさと原文保持（Literal）らしさを、それぞれのラベルで学習した
//! 文字 1–5 gram の出現確率で測る。unigram（n=1）も同じ表に載せることで、
//! 直列化した件数表からの復元が学習結果と完全に一致する。平滑化は文脈長の逐次
//! バックオフ（長い文脈へ固定係数で補間し、未知文字は unigram の add-one に
//! 落ちる）。スコアは「文字あたりの対数尤度の区間内総和」とし、区間の平均を
//! 足す設計（短い分割が有利になる）は採らない。

use std::collections::HashMap;

/// 学習・採点のラベル。`Literal` は「英語である」ではなく「原文保持」。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum TextLabel {
    Japanese,
    Literal,
}

impl std::fmt::Display for TextLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TextLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            TextLabel::Japanese => "ja",
            TextLabel::Literal => "lit",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "ja" => Ok(TextLabel::Japanese),
            "lit" => Ok(TextLabel::Literal),
            other => Err(format!("未知の n-gram ラベル: {other}")),
        }
    }
}

/// 1ラベル分の n-gram 表。文脈（n-1 文字、n=1 は空文脈）ごとに次文字の件数を持つ。
#[derive(Clone, Default, Debug)]
struct LabelTable {
    /// context(n-1 文字) -> (next char, count)。
    counts: HashMap<String, HashMap<char, u64>>,
    /// 文脈別の合計件数（context -> total）。空文脈の total が全件数。
    context_totals: HashMap<String, u64>,
    /// 学習時に見た文字の種類数 + 1（未知文字の add-one 用）。
    alphabet: u64,
}

impl LabelTable {
    fn add_text(&mut self, text: &str) {
        let chars: Vec<char> = text.chars().collect();
        for (index, ch) in chars.iter().enumerate() {
            for n in 1..=5usize {
                if index + 1 < n {
                    continue;
                }
                let context: String = chars[index + 1 - n..index].iter().collect();
                let entry = self.counts.entry(context.clone()).or_default();
                *entry.entry(*ch).or_insert(0) += 1;
                *self.context_totals.entry(context).or_insert(0) += 1;
            }
        }
        self.refresh_alphabet();
    }

    fn refresh_alphabet(&mut self) {
        self.alphabet = self
            .counts
            .get("")
            .map(|unigrams| unigrams.len() as u64 + 1)
            .unwrap_or(1);
    }

    fn unigram_probability(&self, next: char) -> f64 {
        let unigrams = self.counts.get("");
        let hit = unigrams.and_then(|m| m.get(&next)).copied().unwrap_or(0);
        let total = self.context_totals.get("").copied().unwrap_or(0);
        (hit as f64 + 1.0) / (total as f64 + self.alphabet as f64)
    }

    /// 1文字あたりの対数尤度。長い文脈から順に固定係数 alpha で短い文脈へ補間する
    /// （件数のない文脈はスキップ）。alpha が大きいほど長い文脈の影響が薄まる。
    fn char_logp(&self, context: &[char], next: char, alpha: f64) -> f64 {
        let mut probability = self.unigram_probability(next);
        for width in 2..=5usize {
            if context.len() + 1 < width {
                continue;
            }
            let ctx: String = context[context.len() + 1 - width..].iter().collect();
            let Some(total) = self.context_totals.get(&ctx) else {
                continue;
            };
            let hit = self
                .counts
                .get(&ctx)
                .and_then(|nexts| nexts.get(&next))
                .copied()
                .unwrap_or(0);
            probability = (hit as f64 + alpha * probability) / (*total as f64 + alpha);
        }
        probability.ln()
    }

    /// 文字ごとの対数尤度列。区間スコアはこれの総和（計画書 5.2）。
    fn per_char_logps(&self, text: &str, alpha: f64) -> Vec<f64> {
        let chars: Vec<char> = text.chars().collect();
        let mut out = Vec::with_capacity(chars.len());
        for index in 0..chars.len() {
            out.push(self.char_logp(&chars[..index], chars[index], alpha));
        }
        out
    }
}

/// 学習済みの両ラベル表。
#[derive(Clone, Default, Debug)]
pub struct NgramModel {
    tables: [LabelTable; 2],
    /// バックオフ補間係数（固定）。再現性のため学習では動かさない。
    alpha: f64,
}

impl NgramModel {
    pub fn from_labeled_texts<'a>(texts: impl Iterator<Item = (TextLabel, &'a str)>) -> Self {
        let mut model = NgramModel {
            tables: [LabelTable::default(), LabelTable::default()],
            alpha: 8.0,
        };
        for (label, text) in texts {
            model.tables[label_index(label)].add_text(text);
        }
        model
    }

    /// テキストのラベル下での対数尤度（文字あたりの総和）。
    pub fn logp(&self, label: TextLabel, text: &str) -> f64 {
        self.tables[label_index(label)]
            .per_char_logps(text, self.alpha)
            .iter()
            .sum()
    }

    /// 直列化用の件数表。`(label, n, context, next, count)`。n=1 は context が空。
    pub fn entries(&self) -> Vec<(TextLabel, u8, String, char, u64)> {
        let mut out = Vec::new();
        for (slot, label) in [TextLabel::Japanese, TextLabel::Literal]
            .into_iter()
            .enumerate()
        {
            for (context, nexts) in &self.tables[slot].counts {
                let n = context.chars().count() as u8 + 1;
                for (next, count) in nexts {
                    out.push((label, n, context.clone(), *next, *count));
                }
            }
        }
        out.sort();
        out
    }

    /// `entries` から復元する。学習と同一の表・スコアが得られる。
    pub fn from_entries(entries: &[(TextLabel, u8, String, char, u64)]) -> Self {
        let mut model = NgramModel {
            tables: [LabelTable::default(), LabelTable::default()],
            alpha: 8.0,
        };
        for (label, n, context, next, count) in entries {
            debug_assert_eq!(*n, context.chars().count() as u8 + 1);
            let table = &mut model.tables[label_index(*label)];
            let entry = table.counts.entry(context.clone()).or_default();
            *entry.entry(*next).or_insert(0) += *count;
            *table.context_totals.entry(context.clone()).or_insert(0) += *count;
        }
        for table in &mut model.tables {
            table.refresh_alphabet();
        }
        model
    }
}

fn label_index(label: TextLabel) -> usize {
    match label {
        TextLabel::Japanese => 0,
        TextLabel::Literal => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trained_text_scores_higher_under_its_label() {
        let ja = "kyouhanihonnnokyouwotsukau";
        let lit = "typescript and rust are languages";
        let model = NgramModel::from_labeled_texts(
            [
                (TextLabel::Japanese, ja),
                (TextLabel::Japanese, ja),
                (TextLabel::Literal, lit),
                (TextLabel::Literal, lit),
            ]
            .into_iter(),
        );
        let ja_score = model.logp(TextLabel::Japanese, "kyouha");
        let lit_score = model.logp(TextLabel::Literal, "kyouha");
        assert!(
            ja_score > lit_score,
            "学習したローマ字は ja 下で高くなる: {ja_score} vs {lit_score}"
        );
        let ja_word = model.logp(TextLabel::Japanese, "language");
        let lit_word = model.logp(TextLabel::Literal, "language");
        assert!(
            lit_word > ja_word,
            "学習した英単語は lit 下で高くなる: {lit_word} vs {ja_word}"
        );
    }

    #[test]
    fn entries_roundtrip_reproduces_scores() {
        let model = NgramModel::from_labeled_texts(
            [
                (TextLabel::Japanese, "konnnichiha"),
                (TextLabel::Literal, "github repository"),
            ]
            .into_iter(),
        );
        let entries = model.entries();
        let restored = NgramModel::from_entries(&entries);
        for text in ["konnnichiha", "github", "repo", "q"] {
            for label in [TextLabel::Japanese, TextLabel::Literal] {
                assert_eq!(model.logp(label, text), restored.logp(label, text));
            }
        }
    }

    #[test]
    fn unseen_characters_fall_back_to_add_one() {
        let model = NgramModel::from_labeled_texts([(TextLabel::Japanese, "aiueo")].into_iter());
        // 未学習文字を含んでも有限値を返す（パニック・NaN にしない）。
        let score = model.logp(TextLabel::Japanese, "zq?");
        assert!(score.is_finite());
    }
}
