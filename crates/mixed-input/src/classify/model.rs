//! 判別モデル: 特徴重み・辺の採点・アーティファクト（計画書 5.2 / 9.3）。
//!
//! 重みは線形特徴の加重和。n-gram 表は train 分割の頻度から学習し、特徴重みと
//! 遷移コストは validation で調整する（`tune`）。モデルはオフラインで作り、
//! 学習依存を製品へ含めない。直列化はテキスト形式＋checksum で、型・形式版・
//! 上限を検証してから読み込む（読込失敗で混在機能だけ無効化する呼出側契約）。

use crate::classify::dp::TransitionCosts;
use crate::classify::edge::{DictLayer, Edge};
use crate::classify::ngram::{NgramModel, TextLabel};
use crate::plan::SegmentKind;

/// 線形特徴重み。全て「大きいほどその解釈が有利」向きの符号で持つ。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FeatureWeights {
    /// n-gram 対数尤度（辺の kind ラベル下）への係数。
    pub ngram: f64,
    /// 一般語辞書一致の加点。
    pub dict_general: f64,
    /// 技術用語辞書一致の加点。
    pub dict_tech: f64,
    /// 日本語辺に ASCII 大文字（CapsLock 原文字）を含むときの調整。
    pub ja_upper: f64,
    /// 末尾単発 `n` を確定規則で完成させる辺の調整（未完を完成済みにしない
    /// 方向へ働くことが多い）。
    pub finalize_n: f64,
    /// Literal 辺に大文字を含むときの加点（Capitalized 語の証拠）。
    pub lit_upper: f64,
    /// Literal 辺に数字を含むときの加点。
    pub lit_digit: f64,
    /// 辞書一致しない短い（5文字未満）Literal 辺への調整（負でペナルティ）。
    /// 日本語の中に浮く短い英字断片（CapsLock の散らばり等）を抑える
    /// （計画書 5.2「不自然な短い区間・言語切替へのペナルティ」。辞書語は
    /// この限りでない）。
    pub lit_short: f64,
    /// 辺1本あたりの固定コスト（正の値でペナルティ。不自然な短い区間の抑制）。
    pub span_cost: f64,
    /// 言語切替1回の基本コスト。
    pub switch_cost: f64,
    /// 日本語→Literal 切替の追加コスト（普通の日本語を英字にする誤りの抑制）。
    pub ja_to_literal: f64,
}

impl Default for FeatureWeights {
    fn default() -> Self {
        FeatureWeights {
            ngram: 1.0,
            dict_general: 4.0,
            dict_tech: 6.0,
            ja_upper: -1.0,
            finalize_n: -0.5,
            lit_upper: 2.0,
            lit_digit: 1.0,
            lit_short: -2.0,
            span_cost: 1.0,
            switch_cost: 2.0,
            ja_to_literal: 1.5,
        }
    }
}

impl FeatureWeights {
    pub fn transition_costs(&self) -> TransitionCosts {
        TransitionCosts {
            switch: self.switch_cost,
            ja_to_literal: self.ja_to_literal,
        }
    }
}

/// 候補提示と自動適用の閾値（計画書 5.3）。生スコア差を確率と呼ばない。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecisionThresholds {
    /// top1 と次位の（意味的に異なる）Plan のスコア差がこれ未満なら保留
    /// （自動適用しない）。
    pub auto_margin: f64,
    /// 候補提示側の閾値。この差を超える代替解釈だけを Mixed 候補へ出す。
    /// 適用は PR5 の候補接続から（オフライン評価では上位 KEEP_INTERPRETATIONS 件
    /// を常に報告するため調整対象にしない）。
    pub candidate_margin: f64,
}

impl Default for DecisionThresholds {
    fn default() -> Self {
        DecisionThresholds {
            auto_margin: 2.0,
            candidate_margin: 0.5,
        }
    }
}

/// 学習済みモデル本体。
#[derive(Clone, Debug)]
pub struct ClassifyModel {
    pub ngram: NgramModel,
    pub weights: FeatureWeights,
    pub thresholds: DecisionThresholds,
}

impl Default for ClassifyModel {
    fn default() -> Self {
        ClassifyModel {
            ngram: NgramModel::default(),
            weights: FeatureWeights::default(),
            thresholds: DecisionThresholds::default(),
        }
    }
}

impl ClassifyModel {
    /// 辺1本のスコア。`source_text` は辺の source 文字列（大小文字・記号を保存）、
    /// `reading_text` はこの区間の読み。日本語らしさは読み（CapsLock 原文字を
    /// 小文字へ戻したローマ字）で測り、原文保持らしさは原文で測る。合同の
    /// 呼出側（テスト等）は reading に source と同じ文字列を渡してよい。
    pub fn score_edge(&self, edge: &Edge, source_text: &str, reading_text: &str) -> f64 {
        let has_upper = source_text.chars().any(|ch| ch.is_ascii_uppercase());
        let has_digit = source_text.chars().any(|ch| ch.is_ascii_digit());
        let base = match edge.kind {
            SegmentKind::Japanese => {
                self.weights.ngram * self.ngram.logp(TextLabel::Japanese, reading_text)
                    + self.weights.ja_upper * f64::from(has_upper)
                    + self.weights.finalize_n * f64::from(edge.finalize_tail_n)
            }
            SegmentKind::Literal => {
                let dict_bonus = match edge.dict {
                    Some(DictLayer::General) => self.weights.dict_general,
                    Some(DictLayer::Tech) => self.weights.dict_tech,
                    None => 0.0,
                };
                self.weights.ngram * self.ngram.logp(TextLabel::Literal, source_text)
                    + dict_bonus
                    + self.weights.lit_upper * f64::from(has_upper)
                    + self.weights.lit_digit * f64::from(has_digit)
                    + self.weights.lit_short
                        * f64::from(edge.dict.is_none() && source_text.chars().count() < 5)
            }
        };
        base - self.weights.span_cost
    }

    /// テキスト形式へ直列化する。
    pub fn to_artifact(&self) -> String {
        let mut body = String::new();
        body.push_str("ngram_alpha=8\n");
        body.push_str(&format!("w_ngram={}\n", self.weights.ngram));
        body.push_str(&format!("w_dict_general={}\n", self.weights.dict_general));
        body.push_str(&format!("w_dict_tech={}\n", self.weights.dict_tech));
        body.push_str(&format!("w_ja_upper={}\n", self.weights.ja_upper));
        body.push_str(&format!("w_finalize_n={}\n", self.weights.finalize_n));
        body.push_str(&format!("w_lit_upper={}\n", self.weights.lit_upper));
        body.push_str(&format!("w_lit_digit={}\n", self.weights.lit_digit));
        body.push_str(&format!("w_lit_short={}\n", self.weights.lit_short));
        body.push_str(&format!("w_span_cost={}\n", self.weights.span_cost));
        body.push_str(&format!("w_switch_cost={}\n", self.weights.switch_cost));
        body.push_str(&format!("w_ja_to_literal={}\n", self.weights.ja_to_literal));
        body.push_str(&format!("t_auto_margin={}\n", self.thresholds.auto_margin));
        body.push_str(&format!(
            "t_candidate_margin={}\n",
            self.thresholds.candidate_margin
        ));
        let entries = self.ngram.entries();
        body.push_str(&format!("entry_count={}\n", entries.len()));
        for (label, n, context, next, count) in &entries {
            // context/next は code point の hex 列で書く。学習データに区切り文字
            // （| 等）や任意の記号が現れても自分の出力を読み戻せるようにする。
            body.push_str(&format!(
                "{label}|{n}|{}|{:x}|{count}\n",
                encode_codepoints(context),
                *next as u32
            ));
        }
        let mut out = String::new();
        out.push_str("format=mixed-input-classify-model\n");
        out.push_str("version=1\n");
        out.push_str("kind=char-ngram-linear\n");
        out.push_str(&format!("checksum={:016x}\n", fnv1a(body.as_bytes())));
        out.push_str(&body);
        out
    }

    /// テキスト形式から読み込む。形式・checksum・上限を検証し、不合は Err。
    pub fn from_artifact(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        if lines.next() != Some("format=mixed-input-classify-model") {
            return Err("形式行が一致しない".to_string());
        }
        if lines.next() != Some("version=1") {
            return Err("対応しない形式版".to_string());
        }
        if lines.next() != Some("kind=char-ngram-linear") {
            return Err("モデル種別が一致しない".to_string());
        }
        let checksum_line = lines.next().ok_or("checksum 行がない")?;
        let expected = checksum_line
            .strip_prefix("checksum=")
            .and_then(|hex| u64::from_str_radix(hex, 16).ok())
            .ok_or("checksum が読めない")?;
        let body_start = text
            .find(checksum_line)
            .map(|at| at + checksum_line.len() + 1)
            .ok_or("checksum 行が見つからない")?;
        let body = text.get(body_start..).ok_or("checksum 行の後に本文がない")?;
        let actual = fnv1a(body.as_bytes());
        if actual != expected {
            return Err(format!("checksum 不一致: {expected:016x} ≠ {actual:016x}"));
        }
        parse_body(body)
    }
}

const MAX_ENTRIES: usize = 500_000;
/// 1件あたりの件数上限。500k 件 × この値が u64 に収まる範囲で割る
/// （不正 artifact の加算オーバーフロー防止）。
const MAX_COUNT: u64 = 1_000_000_000;

fn parse_body(body: &str) -> Result<ClassifyModel, String> {
    let mut weights = FeatureWeights::default();
    let mut thresholds = DecisionThresholds::default();
    let mut entry_count: Option<usize> = None;
    let mut entries: Vec<(TextLabel, u8, String, char, u64)> = Vec::new();
    // NaN / inf を受理すると DP の順序付けが壊れるので有限値だけ通す。
    let finite = |value: f64, name: &str| -> Result<f64, String> {
        value
            .is_finite()
            .then_some(value)
            .ok_or_else(|| format!("{name} が有限値でない"))
    };
    for line in body.lines() {
        if let Some(value) = line.strip_prefix("w_ngram=") {
            weights.ngram = finite(value.parse().map_err(|_| "w_ngram が読めない")?, "w_ngram")?;
        } else if let Some(value) = line.strip_prefix("w_dict_general=") {
            weights.dict_general = finite(
                value.parse().map_err(|_| "w_dict_general が読めない")?,
                "w_dict_general",
            )?;
        } else if let Some(value) = line.strip_prefix("w_dict_tech=") {
            weights.dict_tech = finite(
                value.parse().map_err(|_| "w_dict_tech が読めない")?,
                "w_dict_tech",
            )?;
        } else if let Some(value) = line.strip_prefix("w_ja_upper=") {
            weights.ja_upper = finite(
                value.parse().map_err(|_| "w_ja_upper が読めない")?,
                "w_ja_upper",
            )?;
        } else if let Some(value) = line.strip_prefix("w_finalize_n=") {
            weights.finalize_n = finite(
                value.parse().map_err(|_| "w_finalize_n が読めない")?,
                "w_finalize_n",
            )?;
        } else if let Some(value) = line.strip_prefix("w_lit_upper=") {
            weights.lit_upper = finite(
                value.parse().map_err(|_| "w_lit_upper が読めない")?,
                "w_lit_upper",
            )?;
        } else if let Some(value) = line.strip_prefix("w_lit_digit=") {
            weights.lit_digit = finite(
                value.parse().map_err(|_| "w_lit_digit が読めない")?,
                "w_lit_digit",
            )?;
        } else if let Some(value) = line.strip_prefix("w_lit_short=") {
            weights.lit_short = finite(
                value.parse().map_err(|_| "w_lit_short が読めない")?,
                "w_lit_short",
            )?;
        } else if let Some(value) = line.strip_prefix("w_span_cost=") {
            weights.span_cost = finite(
                value.parse().map_err(|_| "w_span_cost が読めない")?,
                "w_span_cost",
            )?;
        } else if let Some(value) = line.strip_prefix("w_switch_cost=") {
            weights.switch_cost = finite(
                value.parse().map_err(|_| "w_switch_cost が読めない")?,
                "w_switch_cost",
            )?;
        } else if let Some(value) = line.strip_prefix("w_ja_to_literal=") {
            weights.ja_to_literal = finite(
                value.parse().map_err(|_| "w_ja_to_literal が読めない")?,
                "w_ja_to_literal",
            )?;
        } else if let Some(value) = line.strip_prefix("t_auto_margin=") {
            thresholds.auto_margin = finite(
                value.parse().map_err(|_| "t_auto_margin が読めない")?,
                "t_auto_margin",
            )?;
        } else if let Some(value) = line.strip_prefix("t_candidate_margin=") {
            thresholds.candidate_margin = finite(
                value.parse().map_err(|_| "t_candidate_margin が読めない")?,
                "t_candidate_margin",
            )?;
        } else if let Some(value) = line.strip_prefix("entry_count=") {
            entry_count = Some(value.parse().map_err(|_| "entry_count が読めない")?);
        } else if line.starts_with("ngram_alpha=") {
            if line != "ngram_alpha=8" {
                return Err("ngram_alpha は 8 固定".to_string());
            }
        } else {
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() != 5 {
                return Err(format!("不明な行: {line}"));
            }
            let label = TextLabel::parse(parts[0])?;
            let n: u8 = parts[1].parse().map_err(|_| "n が読めない")?;
            if !(1..=5).contains(&n) {
                return Err(format!("n が上限外: {n}"));
            }
            let context = decode_codepoints(parts[2])?;
            if context.chars().count() + 1 != usize::from(n) {
                return Err("n と文脈長が一致しない".to_string());
            }
            let code: u32 = u32::from_str_radix(parts[3], 16).map_err(|_| "next が読めない")?;
            let next_char = char::from_u32(code).ok_or("next が不正な code point")?;
            let count: u64 = parts[4].parse().map_err(|_| "count が読めない")?;
            // 不正 artifact が巨大 count で加算オーバーフローを起こせないように
            // 1件あたりの上限を検査する（MAX_ENTRIES 件 × 上限 < u64::MAX）。
            if count > MAX_COUNT {
                return Err(format!("count が上限超過: {count}"));
            }
            entries.push((label, n, context, next_char, count));
            // 全行積んでからでは遅いので読み込み中に上限を検査する。
            if entries.len() > MAX_ENTRIES {
                return Err(format!("件数上限超過: {}", entries.len()));
            }
        }
    }
    let expected = entry_count.ok_or("entry_count がない")?;
    if entries.len() != expected {
        return Err(format!(
            "entry_count 不一致: 宣言 {expected} / 実際 {}",
            entries.len()
        ));
    }
    Ok(ClassifyModel {
        ngram: NgramModel::from_entries(&entries),
        weights,
        thresholds,
    })
}

/// 文字列を code point の hex（空白区切り）へ。n=1 の空文脈は空文字列。
fn encode_codepoints(text: &str) -> String {
    text.chars()
        .map(|ch| format!("{:x}", ch as u32))
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_codepoints(text: &str) -> Result<String, String> {
    if text.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::new();
    for part in text.split(' ') {
        let code: u32 = u32::from_str_radix(part, 16).map_err(|_| "context が読めない")?;
        out.push(char::from_u32(code).ok_or("context が不正な code point")?);
    }
    Ok(out)
}

/// FNV-1a 64bit。依存なしの checksum（計画書 9.3 の整合検証用）。
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::ngram::NgramModel;
    use crate::position::SourceRange;

    fn sample_model() -> ClassifyModel {
        ClassifyModel {
            ngram: NgramModel::from_labeled_texts(
                [
                    (TextLabel::Japanese, "kyouhanihonnnokyouwotsukau"),
                    (TextLabel::Literal, "github repository typescript"),
                ]
                .into_iter(),
            ),
            weights: FeatureWeights::default(),
            thresholds: DecisionThresholds::default(),
        }
    }

    #[test]
    fn artifact_roundtrip_preserves_model() {
        let model = sample_model();
        let text = model.to_artifact();
        let restored = ClassifyModel::from_artifact(&text).unwrap();
        assert_eq!(restored.weights, model.weights);
        assert_eq!(restored.thresholds, model.thresholds);
        for (label, probe) in [
            (TextLabel::Japanese, "kyouha"),
            (TextLabel::Literal, "repo"),
        ] {
            assert_eq!(
                model.ngram.logp(label, probe),
                restored.ngram.logp(label, probe)
            );
        }
    }

    #[test]
    fn artifact_rejects_truncated_header_and_oversize_context_without_panicking() {
        let header = "format=mixed-input-classify-model\nversion=1\nkind=char-ngram-linear\n";
        assert!(ClassifyModel::from_artifact(&format!("{header}checksum=0")).is_err());
        for length in [255, 256] {
            let context = std::iter::repeat("61").take(length).collect::<Vec<_>>().join(" ");
            let body = format!("entry_count=1\nngram_alpha=8\nlit|1|{context}|61|1\n");
            let artifact = format!("{header}checksum={:016x}\n{body}", fnv1a(body.as_bytes()));
            assert!(ClassifyModel::from_artifact(&artifact).is_err());
        }
    }

    #[test]
    fn artifact_rejects_checksum_mismatch() {
        let model = sample_model();
        let mut text = model.to_artifact();
        // body の一部（件数行の後の最初の行）を書き換える。
        let marker = "entry_count=";
        let at = text.find(marker).unwrap();
        let line_end = text[at..].find('\n').unwrap() + at;
        text.insert(line_end, '#');
        assert!(ClassifyModel::from_artifact(&text).is_err());
    }

    #[test]
    fn artifact_rejects_unknown_lines_and_version() {
        let model = sample_model();
        let text = model.to_artifact().replace("version=1", "version=2");
        assert!(ClassifyModel::from_artifact(&text).is_err());
        let tampered = model.to_artifact().replace("w_ngram=1", "w_ngram=x");
        assert!(ClassifyModel::from_artifact(&tampered).is_err());
    }

    #[test]
    fn artifact_escapes_arbitrary_symbols_and_survives_roundtrip() {
        // 学習データに区切り文字や記号が現れても自分の出力を読み戻せる。
        let mut model = sample_model();
        model.ngram =
            NgramModel::from_labeled_texts([(TextLabel::Literal, "a|b\nc|d")].into_iter());
        let text = model.to_artifact();
        let restored = ClassifyModel::from_artifact(&text).unwrap();
        for probe in ["a|b", "c|d", "|"] {
            assert_eq!(
                model.ngram.logp(TextLabel::Literal, probe),
                restored.ngram.logp(TextLabel::Literal, probe)
            );
        }
    }

    #[test]
    fn artifact_rejects_non_finite_weights_with_valid_checksum() {
        let model = sample_model();
        let text = model.to_artifact();
        let cs_at = text.find("checksum=").expect("checksum 行がある");
        let line_end = text[cs_at..].find('\n').expect("改行がある") + cs_at;
        let head = &text[..cs_at];
        let body = &text[line_end + 1..];
        let tampered = body.replacen("w_ngram=1\n", "w_ngram=NaN\n", 1);
        let rebuilt = format!(
            "{head}checksum={:016x}\n{tampered}",
            fnv1a(tampered.as_bytes())
        );
        let error = ClassifyModel::from_artifact(&rebuilt).expect_err("NaN は拒否");
        assert!(error.contains("有限値"), "{error}");
    }

    #[test]
    fn literal_dictionary_edge_outscores_unknown_literal_of_same_text() {
        let model = sample_model();
        let base = Edge {
            range: SourceRange::new(0, 6),
            kind: SegmentKind::Literal,
            finalize_tail_n: false,
            dict: Some(DictLayer::Tech),
            fixed: false,
        };
        let unknown = Edge {
            dict: None,
            ..base.clone()
        };
        assert!(
            model.score_edge(&base, "github", "github")
                > model.score_edge(&unknown, "github", "github")
        );
    }
}
