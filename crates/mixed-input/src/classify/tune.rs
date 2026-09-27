//! 特徴重みと閾値の調整・評価指標（計画書 5.2 / 5.3 / 9.2）。
//!
//! n-gram 表は train 分割の頻度から学習済みの前提で、特徴重み・遷移コスト・
//! 保留閾値を validation に対する座標探索（coordinate ascent）で調整する。
//! グリッドと走査順序は固定で、目的関数は (exact match 数, 境界F の総和) の
//! 辞書式比較。生スコア差を確率と呼ばないため、閾値も同じ探索で選ぶ。
//! 決定的であることが前提（同じ入力 → 同じ重み）。frozen 分割での再評価は
//! 呼出側（CLI）が行い、調整には使わない。

use crate::classify::edge::Dictionary;
use crate::classify::model::{ClassifyModel, DecisionThresholds, FeatureWeights};
use crate::classify::ngram::{NgramModel, TextLabel};
use crate::classify::{classify, ScoredPlan};
use crate::plan::SegmentKind;
use crate::source::{CompositionSource, Provenance, SourceElement, SourceStyle};

/// 分類対象1件（source と期待区間。synthetic・fixture を同じ形へ載せる）。
#[derive(Clone, Debug)]
pub struct LabeledCase {
    pub source: String,
    pub expected: Vec<(SegmentKind, String)>,
}

/// 1件の評価結果。
#[derive(Clone, Debug)]
pub struct CaseOutcome {
    /// top1 の区間列が期待と完全一致したか。
    pub exact_match: bool,
    /// 上位 KEEP_INTERPRETATIONS 内に期待と一致する解釈があったか。
    pub top_k_hit: bool,
    /// top1 が保留（自動適用しない）と判定されたか。
    pub retained: bool,
    /// 境界位置（言語が変わる scalar 位置）の F値。
    pub boundary_f: f64,
    /// 期待が全部日本語の入力で、top1 に Literal 区間が混ざったか（普通の
    /// 日本語を英字にしてしまう誤り。計画書 5.2 で最優先で抑える）。
    pub latinized_pure: bool,
}

/// 生のローマ字列を実 composer と同じ単位粒度の Typed Kana 要素へ分解する
/// （オフライン評価用）。読み合成は ASCII 小文字化した入力で行い、原文字側に
/// 大小文字を保存する（CapsLock。unit の original と元文字列は ASCII で同長）。
pub fn kana_elements(source: &str) -> Vec<SourceElement> {
    let lowered: String = source
        .chars()
        .map(|ch| {
            ch.is_ascii_uppercase()
                .then(|| ch.to_ascii_lowercase())
                .unwrap_or(ch)
        })
        .collect();
    let units = crate::roman::synthesize(&lowered, false);
    let mut originals = source.chars();
    let mut out = Vec::new();
    for unit in units {
        let take = unit.original.chars().count();
        let original: String = (0..take).filter_map(|_| originals.next()).collect();
        out.push(SourceElement {
            provenance: Provenance::Typed {
                style: SourceStyle::Kana,
            },
            source_text: original,
            reading: unit.kana,
        });
    }
    out
}

/// 生のローマ字列を Typed Kana の CompositionSource へ載せる（オフライン評価用）。
pub fn source_from_str(source: &str) -> CompositionSource {
    let mut composed = CompositionSource::empty(0);
    for element in kana_elements(source) {
        let _ = composed.push(element);
    }
    composed
}

fn expected_boundaries(expected: &[(SegmentKind, String)]) -> Vec<u32> {
    // 言語切替位置だけを見る。末尾の確定境界（source 終端）はどの解釈にも
    // 共通なので含めない（含めると境界Fが恒常的に水増しされる）。
    let mut out = Vec::new();
    let mut at = 0u32;
    for (_kind, text) in expected.iter().take(expected.len().saturating_sub(1)) {
        at += text.chars().count() as u32;
        out.push(at);
    }
    out
}

fn predicted_boundaries(plan: &crate::plan::InterpretationPlan) -> Vec<u32> {
    plan.spans
        .iter()
        .take(plan.spans.len().saturating_sub(1))
        .map(|span| span.range.end.get())
        .collect()
}

/// 1件を分類して指標を出す。
pub fn evaluate_case(
    case: &LabeledCase,
    model: &ClassifyModel,
    dictionary: &Dictionary,
) -> CaseOutcome {
    evaluate_prepared(
        &source_from_str(&case.source),
        &case.expected,
        model,
        dictionary,
    )
}

/// 由来つきの CompositionSource に対する評価（fixture の明示指定などを反映した
/// source で classifier 単体を評価するときに使う）。
pub fn evaluate_prepared(
    source: &CompositionSource,
    expected: &[(SegmentKind, String)],
    model: &ClassifyModel,
    dictionary: &Dictionary,
) -> CaseOutcome {
    let results = classify(source, model, dictionary);
    let matches = |scored: &ScoredPlan| -> bool {
        scored
            .plan
            .spans
            .iter()
            .map(|span| (span.kind, span.text.clone()))
            .eq(expected.iter().cloned())
    };
    let exact_match = results.first().map(matches).unwrap_or(false);
    let top_k_hit = results.iter().any(matches);
    let retained = results.first().map(|r| r.retained).unwrap_or(false);
    // 純日本語の誤英字化は「正解側」だけで判定する（予測側に Japanese を含む等の
    // 条件を入れると、全体 Literal への誤分類がこの最優先ペナルティから抜ける）。
    let expected_is_pure_japanese = expected.len() == 1 && expected[0].0 == SegmentKind::Japanese;
    let latinized_pure = expected_is_pure_japanese
        && results
            .first()
            .is_some_and(|r| r.plan.spans.iter().any(|s| s.kind == SegmentKind::Literal));

    let expected = expected_boundaries(expected);
    let predicted = results
        .first()
        .map(|r| predicted_boundaries(&r.plan))
        .unwrap_or_default();
    let expected_set: std::collections::HashSet<u32> = expected.iter().copied().collect();
    let predicted_set: std::collections::HashSet<u32> = predicted.iter().copied().collect();
    let hits = expected_set.intersection(&predicted_set).count() as f64;
    let precision = if predicted_set.is_empty() {
        1.0
    } else {
        hits / predicted_set.len() as f64
    };
    let recall = if expected_set.is_empty() {
        1.0
    } else {
        hits / expected_set.len() as f64
    };
    let boundary_f = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    CaseOutcome {
        exact_match,
        top_k_hit,
        retained,
        boundary_f,
        latinized_pure,
    }
}

/// 目的関数。辞書式: 純日本語の誤英字化件数（少ないほどよい）→ exact match 数 →
/// 自動適用安全性 → 境界F 総和。第1項を最優先にするのは「普通の日本語を英字に
/// してしまう誤り」のコストを高めるため（計画書 5.2。混在の利得と交換しない）。
/// 自動適用安全性は「正解を自動適用できる件数 − 誤りを自動適用してしまう件数の
/// 3倍」。保留（retained）は閾値だけで変わるため、この項が auto_margin の
/// 調整を担う（計画書 5.3: 候補提示と自動適用の閾値は別々に選ぶ）。
type Objective = (std::cmp::Reverse<usize>, usize, f64, f64);

fn objective(cases: &[LabeledCase], model: &ClassifyModel, dictionary: &Dictionary) -> Objective {
    let mut latinized = 0;
    let mut exact = 0;
    let mut auto_ok = 0.0f64;
    let mut f_sum = 0.0;
    for case in cases {
        let outcome = evaluate_case(case, model, dictionary);
        if outcome.latinized_pure {
            latinized += 1;
        }
        if outcome.exact_match {
            exact += 1;
            if !outcome.retained {
                auto_ok += 1.0;
            }
        } else if !outcome.retained {
            auto_ok -= 3.0;
        }
        f_sum += outcome.boundary_f;
    }
    (std::cmp::Reverse(latinized), exact, auto_ok, f_sum)
}

/// train 分割から n-gram 表を学習したモデルを作る。JA ラベルには日本語区間の
/// **読み**（かな。ローマ字表記の揺れがここで潰れる）を、Literal ラベルには
/// 原文保持区間を積む。採点側も読みで測る（`classify` の reading 写像）ため、
/// 学習と推論の表現が一致する。
pub fn train_ngram(cases: &[LabeledCase]) -> NgramModel {
    let mut ja_texts: Vec<String> = Vec::new();
    let mut lit_texts: Vec<String> = Vec::new();
    let reading_of = |roman: &str| -> String {
        kana_elements(roman)
            .iter()
            .map(|element| element.reading.as_str())
            .collect()
    };
    for case in cases {
        // 隣接する同 kind の日本語区間は連結して文脈を長くする。
        let mut ja_run = String::new();
        for (kind, text) in &case.expected {
            match kind {
                SegmentKind::Japanese => ja_run.push_str(&reading_of(text)),
                SegmentKind::Literal => {
                    if !ja_run.is_empty() {
                        ja_texts.push(std::mem::take(&mut ja_run));
                    }
                    lit_texts.push(text.clone());
                }
            }
        }
        if !ja_run.is_empty() {
            ja_texts.push(ja_run);
        }
    }
    let texts = ja_texts
        .iter()
        .map(|t| (TextLabel::Japanese, t.as_str()))
        .chain(lit_texts.iter().map(|t| (TextLabel::Literal, t.as_str())));
    NgramModel::from_labeled_texts(texts)
}

/// 座標探索で重みと閾値を調整する。探索幅は現在値の倍率で固定。各次元は
/// 全候補を評価してから最良値を一度だけコミットする（評価中の改善値を次候補の
/// 比較基準に混ぜない）。重みは負のまま伸縮する（ja_upper 等は負が正当）。
pub fn tune(
    ngram: NgramModel,
    validation: &[LabeledCase],
    dictionary: &Dictionary,
) -> ClassifyModel {
    let mut model = ClassifyModel {
        ngram,
        weights: FeatureWeights::default(),
        thresholds: DecisionThresholds::default(),
    };
    let scales = [0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0];
    const DIMENSIONS: usize = 12;
    for _pass in 0..2 {
        let mut improved = false;
        for dimension in 0..DIMENSIONS {
            let base = weight_at(&model, dimension);
            let mut best_value = base;
            let mut best_score = objective(validation, &model, dictionary);
            for &scale in scales.iter() {
                let candidate = base * scale;
                set_weight_at(&mut model, dimension, candidate);
                let score = objective(validation, &model, dictionary);
                if score > best_score {
                    best_score = score;
                    best_value = candidate;
                }
            }
            set_weight_at(&mut model, dimension, best_value);
            if best_value != base {
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
    model
}

fn weight_at(model: &ClassifyModel, dimension: usize) -> f64 {
    let w = &model.weights;
    match dimension {
        0 => w.ngram,
        1 => w.dict_general,
        2 => w.dict_tech,
        3 => w.ja_upper,
        4 => w.finalize_n,
        5 => w.lit_upper,
        6 => w.lit_digit,
        7 => w.lit_short,
        8 => w.span_cost,
        9 => w.switch_cost,
        10 => w.ja_to_literal,
        _ => model.thresholds.auto_margin,
    }
}

fn set_weight_at(model: &mut ClassifyModel, dimension: usize, value: f64) {
    match dimension {
        0 => model.weights.ngram = value,
        1 => model.weights.dict_general = value,
        2 => model.weights.dict_tech = value,
        3 => model.weights.ja_upper = value,
        4 => model.weights.finalize_n = value,
        5 => model.weights.lit_upper = value,
        6 => model.weights.lit_digit = value,
        7 => model.weights.lit_short = value,
        8 => model.weights.span_cost = value,
        9 => model.weights.switch_cost = value,
        10 => model.weights.ja_to_literal = value,
        _ => model.thresholds.auto_margin = value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::synth::{self, SynthConfig};

    fn labeled<'a>(
        episodes: impl IntoIterator<Item = &'a synth::SynthEpisode>,
    ) -> Vec<LabeledCase> {
        episodes
            .into_iter()
            .map(|e| LabeledCase {
                source: e.source.clone(),
                expected: e.expected.clone(),
            })
            .collect()
    }

    #[test]
    fn tuned_model_keeps_pure_japanese_as_japanese_on_frozen() {
        // 「普通の日本語を英字にしてしまう誤り」を許さない（計画書 5.2）。
        let episodes = synth::generate(&SynthConfig::default());
        let train = labeled(episodes.iter().filter(|e| e.split == synth::Split::Train));
        let validation = labeled(
            episodes
                .iter()
                .filter(|e| e.split == synth::Split::Validation),
        );
        let dictionary = synth::default_dictionary();
        let model = tune(train_ngram(&train), &validation, &dictionary);
        for episode in episodes.iter().filter(|e| e.split == synth::Split::Frozen) {
            if episode.category != synth::SynthCategory::PureJapanese {
                continue;
            }
            let outcome = evaluate_case(
                &LabeledCase {
                    source: episode.source.clone(),
                    expected: episode.expected.clone(),
                },
                &model,
                &dictionary,
            );
            assert!(
                outcome.exact_match,
                "{} ({}) が全部日本語のまま分類される: source={}",
                episode.id,
                episode.category.as_str(),
                episode.source
            );
        }
    }
}
