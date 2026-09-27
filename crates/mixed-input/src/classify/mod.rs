//! 混在入力の区間判別（計画書 5 節。PR4）。
//!
//! 入力は `CompositionSource`（元入力と由来）。出力は解釈候補の k-best
//! （`InterpretationPlan` + スコア + 保留判定）。全部日本語として扱う既存相当の
//! 経路は、明示 Literal 制約と矛盾しない限り候補に必ず残す（計画書 5.2）。
//! このモジュールは同期・非同期の入力経路に依存しない純粋関数で、TIP 側ワーカー
//! （PR5 以降）とオフライン評価 CLI が同じ実装を呼ぶ。

pub mod dp;
pub mod edge;
pub mod model;
pub mod ngram;
pub mod synth;
pub mod tune;

use crate::classify::dp::Path;
use crate::classify::edge::{generate_edges, Dictionary};
use crate::classify::model::ClassifyModel;
use crate::plan::{InterpretationPlan, SegmentKind};
use crate::source::CompositionSource;

/// 保持する解釈の上限（計画書 5.2 の初期探索パラメータ案）。
pub const KEEP_INTERPRETATIONS: usize = 4;

/// 判別結果の1解釈。`plan.spans` は隣接する同 kind を結合済み。
#[derive(Clone, Debug)]
pub struct ScoredPlan {
    pub plan: InterpretationPlan,
    pub score: f64,
    /// top1 のときだけ Some: 意味的に異なる次位 Plan とのスコア差。次位が無い
    /// （一意）なら `f64::INFINITY`。辺の組み合わせ違いで同じ区間列になる経路は
    /// 正規化済みなので、この差は常に別解釈との差。
    pub margin: Option<f64>,
    /// 保留（自動適用しない。現在の解釈を維持し候補へ出す。計画書 5.3）。
    pub retained: bool,
}

/// 1回の解析対象上限（計画書 5.2）。超える場合は英単語の途中を切らず、
/// その回の自動判別を見送って既存相当（全部日本語）だけを返す。
pub const MAX_ANALYZE_SCALARS: u32 = 256;

/// 未確定入力ソースを判別する。戻り値はスコア降順、最大
/// `KEEP_INTERPRETATIONS` 件。辺の組み合わせ違いで同じ区間列になる経路は
/// 1件に正規化する（候補枠と margin を本当の別解釈のために空けておく）。
pub fn classify(
    source: &CompositionSource,
    model: &ClassifyModel,
    dictionary: &Dictionary,
) -> Vec<ScoredPlan> {
    let text = source.source_text();
    let chars: Vec<char> = text.chars().collect();
    // 上限超過: 判別を見送り、既存相当の全体日本語経路だけを返す
    // （長文貼り付け等で TIP の CPU/メモリを膨らませない。計画書 5.2）。
    if source.source_len() > MAX_ANALYZE_SCALARS {
        return whole_span_japanese_path(source, model)
            .and_then(|path| build_plan(&chars, &path))
            .map(|plan| {
                vec![ScoredPlan {
                    plan,
                    score: 0.0,
                    margin: Some(f64::INFINITY),
                    retained: true,
                }]
            })
            .unwrap_or_default();
    }
    let edges = generate_edges(source, dictionary);
    // source 位置 → 読み位置の写像。要素の両端は常に対応し、内部は要素の
    // source と読みの scalar 長が一致するときだけ線形に対応する。日本語辺は
    // 合法境界でしか切らないため、両端は常にこの写像に載る。読みで測る理由は
    // CapsLock 原文字（大文字）が日本語らしさの評価を歪めないため。
    let reading_chars: Vec<char> = source.reading_text().chars().collect();
    let mut reading_at: Vec<Option<u32>> = vec![None; chars.len() + 1];
    for element in source.layout() {
        let (s0, s1) = (element.source.start.get(), element.source.end.get());
        let (r0, r1) = (element.reading.start.get(), element.reading.end.get());
        reading_at[s0 as usize] = Some(r0);
        reading_at[s1 as usize] = Some(r1);
        if s1 - s0 == r1 - r0 {
            for d in 0..(s1 - s0) {
                reading_at[(s0 + d) as usize] = Some(r0 + d);
            }
        }
    }
    reading_at[chars.len()] = reading_at[chars.len()].or(Some(reading_chars.len() as u32));
    let scored: Vec<(crate::classify::edge::Edge, f64)> = edges
        .iter()
        .map(|edge| {
            let start = edge.range.start.get() as usize;
            let end = edge.range.end.get() as usize;
            let slice: String = chars[start..end].iter().collect();
            let reading_slice: String = match (reading_at[start], reading_at[end]) {
                (Some(from), Some(to)) if to > from => {
                    reading_chars[from as usize..to as usize].iter().collect()
                }
                _ => slice.clone(),
            };
            (edge.clone(), model.score_edge(edge, &slice, &reading_slice))
        })
        .collect();
    let costs = model.weights.transition_costs();
    // 全部日本語として扱う既存相当の経路（明示 Literal 制約と矛盾しない限り必ず
    // 候補に残す。計画書 5.2）。ローマ字規則で完全に読める場合は JA 辺だけの
    // k-best、読めない文字を含む場合は source 全体を1つの Japanese 区間とみなす
    // 経路（既存のかな入力と同じ解釈）。プールのスコア順位に関係なく正規化後に
    // 必ず入れる。
    let ja_only: Vec<(crate::classify::edge::Edge, f64)> = scored
        .iter()
        .filter(|(edge, _)| edge.kind == SegmentKind::Japanese)
        .cloned()
        .collect();
    let best_ja = dp::k_best(source.source_len(), &ja_only, 1, &costs)
        .into_iter()
        .next()
        .or_else(|| whole_span_japanese_path(source, model));

    // DP は同 kind の辺で連続拡張しないため、各パスはそのまま1つの Plan
    // （=区間列）に対応し、k-best のパス同士はすべて意味的に異なる。これで
    // 上位 KEEP_INTERPRETATIONS 件の解釈と次位との margin が近似なしに確定する。
    let paths = dp::k_best(
        source.source_len(),
        &scored,
        KEEP_INTERPRETATIONS + 1,
        &costs,
    );
    let mut canonical = canonicalize(&chars, &paths);
    if let Some(best_ja) = &best_ja {
        let present = canonical
            .iter()
            .any(|(plan, _)| plan.spans.len() == 1 && plan.spans[0].kind == SegmentKind::Japanese);
        if !present {
            if let Some(plan) = build_plan(&chars, best_ja) {
                canonical.push((plan, best_ja.score));
            }
        }
    }
    canonical.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    canonical.truncate(KEEP_INTERPRETATIONS);
    // 上位 k から溢れても既存相当は最下位枠で保持する。
    if canonical.len() == KEEP_INTERPRETATIONS {
        let has_all_ja = canonical
            .iter()
            .any(|(plan, _)| plan.spans.len() == 1 && plan.spans[0].kind == SegmentKind::Japanese);
        if !has_all_ja {
            if let Some(path) = best_ja {
                if let Some(plan) = build_plan(&chars, &path) {
                    canonical.pop();
                    canonical.push((plan, path.score));
                }
            }
        }
    }

    let mut out: Vec<ScoredPlan> = Vec::with_capacity(canonical.len());
    for (rank, (plan, score)) in canonical.iter().enumerate() {
        // margin は意味的に異なる次位 Plan との差で測る（正規化済みなので
        // canonical[1] は必ず別解釈）。スコア差を確率と呼ばない（計画書 5.3）。
        let margin = if rank == 0 {
            Some(
                canonical
                    .get(1)
                    .map(|(_, next)| score - next)
                    .unwrap_or(f64::INFINITY),
            )
        } else {
            None
        };
        let retained = rank == 0 && margin.is_some_and(|m| m < model.thresholds.auto_margin);
        out.push(ScoredPlan {
            plan: plan.clone(),
            score: *score,
            margin,
            retained,
        });
    }
    out
}

/// 辺経路列を Plan へ正規化する。隣接する同 kind を結合した区間列が同じ経路は
/// 代表1件（最良スコア）にまとめる。
fn canonicalize(chars: &[char], paths: &[dp::Path]) -> Vec<(crate::plan::InterpretationPlan, f64)> {
    let mut canonical: Vec<(crate::plan::InterpretationPlan, f64)> = Vec::new();
    for path in paths {
        let Some(plan) = build_plan(chars, path) else {
            continue;
        };
        match canonical.iter_mut().find(|(existing, _)| {
            existing
                .spans
                .iter()
                .map(|span| (span.kind, span.text.clone()))
                .eq(plan.spans.iter().map(|span| (span.kind, span.text.clone())))
        }) {
            Some((_, best)) => {
                if path.score > *best {
                    *best = path.score;
                }
            }
            None => canonical.push((plan, path.score)),
        }
    }
    canonical
}

/// source 全体を1つの Japanese 区間とみなす経路（既存のかな入力と同じ解釈）。
/// 明示 Literal を含む、または空のときは作らない。
fn whole_span_japanese_path(source: &CompositionSource, model: &ClassifyModel) -> Option<Path> {
    if source.is_empty() || source.source_len() == 0 {
        return None;
    }
    let has_explicit = source.elements().iter().any(|element| {
        matches!(
            element.provenance,
            crate::source::Provenance::ExplicitLiteral
        )
    });
    if has_explicit {
        return None;
    }
    let text = source.source_text();
    let reading = source.reading_text();
    let edge = crate::classify::edge::Edge {
        range: crate::position::SourceRange::new(0, source.source_len()),
        kind: SegmentKind::Japanese,
        finalize_tail_n: false,
        dict: None,
        fixed: false,
    };
    let score = model.score_edge(&edge, &text, &reading);
    Some(Path {
        score,
        steps: vec![(edge, score)],
    })
}

/// パスの辺列から Plan を組む。隣接する同 kind の辺は1つの span へ結合する
/// （辺は切れ得る位置の列で、Plan の区間は言語区間）。
fn build_plan(chars: &[char], path: &Path) -> Option<InterpretationPlan> {
    let mut specs: Vec<(SegmentKind, String)> = Vec::new();
    for (edge, _) in &path.steps {
        let slice: String = chars[edge.range.start.get() as usize..edge.range.end.get() as usize]
            .iter()
            .collect();
        match specs.last_mut() {
            Some((kind, text)) if *kind == edge.kind => text.push_str(&slice),
            _ => specs.push((edge.kind, slice)),
        }
    }
    let source: String = chars.iter().collect();
    InterpretationPlan::build(&source, &specs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::edge::{DictLayer, Dictionary};
    use crate::classify::ngram::{NgramModel, TextLabel};
    use crate::source::{Provenance, SourceElement, SourceStyle};

    fn training_model() -> ClassifyModel {
        let ja = [
            "kyouhanihonnnokyouwotsukau",
            "konnnichiha",
            "rainennnomatsuriniikimasu",
            "kyounosekainijuunijissai",
            "watasinomachinohokutoside",
        ];
        let lit = [
            "github repository",
            "typescript and rust",
            "docker container image",
            "visual studio code editor",
            "nospacekey input method",
        ];
        let mut texts: Vec<(TextLabel, &str)> = Vec::new();
        for text in ja.iter() {
            texts.push((TextLabel::Japanese, text));
        }
        for text in lit.iter() {
            texts.push((TextLabel::Literal, text));
        }
        ClassifyModel {
            ngram: NgramModel::from_labeled_texts(texts.into_iter()),
            weights: crate::classify::model::FeatureWeights::default(),
            thresholds: crate::classify::model::DecisionThresholds::default(),
        }
    }

    fn typed_source(source: &str) -> CompositionSource {
        let mut composed = CompositionSource::empty(0);
        composed
            .push(SourceElement {
                provenance: Provenance::Typed {
                    style: SourceStyle::Kana,
                },
                source_text: source.to_string(),
                reading: source.to_string(),
            })
            .unwrap();
        composed
    }

    #[test]
    fn splits_githubnotukaikata() {
        let model = training_model();
        let mut dictionary = Dictionary::new();
        dictionary.insert("github", DictLayer::Tech);
        let source = typed_source("githubnotukaikata");
        let results = classify(&source, &model, &dictionary);
        assert!(!results.is_empty());
        let top = &results[0];
        let kinds: Vec<&str> = top
            .plan
            .spans
            .iter()
            .map(|span| span.kind.as_str())
            .collect();
        assert_eq!(
            top.plan
                .spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<Vec<_>>(),
            vec!["github", "notukaikata"],
            "PR4 の完了条件: githubnotukaikata の内部を分割する"
        );
        assert_eq!(kinds, vec!["literal", "japanese"]);
    }

    #[test]
    fn keeps_all_japanese_for_plain_japanese() {
        let model = training_model();
        let source = typed_source("kyouhanihonnwotsukau");
        let results = classify(&source, &model, &Dictionary::new());
        let top = &results[0];
        assert_eq!(top.plan.spans.len(), 1);
        assert_eq!(top.plan.spans[0].kind, SegmentKind::Japanese);
    }

    #[test]
    fn all_japanese_path_stays_in_candidates_for_mixed() {
        let model = training_model();
        let mut dictionary = Dictionary::new();
        dictionary.insert("github", DictLayer::Tech);
        let source = typed_source("githubnotukaikata");
        let results = classify(&source, &model, &dictionary);
        assert!(
            results.iter().any(|scored| {
                scored.plan.spans.len() == 1 && scored.plan.spans[0].kind == SegmentKind::Japanese
            }),
            "全部日本語の経路が候補に残る: {:?}",
            results
                .iter()
                .map(|r| r
                    .plan
                    .spans
                    .iter()
                    .map(|s| s.text.clone())
                    .collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn explicit_literal_pins_the_span() {
        let model = training_model();
        let mut composed = CompositionSource::empty(0);
        composed
            .push(SourceElement {
                provenance: Provenance::ExplicitLiteral,
                source_text: "github".to_string(),
                reading: "github".to_string(),
            })
            .unwrap();
        composed
            .push(SourceElement {
                provenance: Provenance::Typed {
                    style: SourceStyle::Kana,
                },
                source_text: "wotukau".to_string(),
                reading: "wotukau".to_string(),
            })
            .unwrap();
        let results = classify(&composed, &model, &Dictionary::new());
        let top = &results[0];
        assert_eq!(top.plan.spans[0].kind, SegmentKind::Literal);
        assert_eq!(top.plan.spans[0].text, "github");
        assert_eq!(top.plan.spans[1].kind, SegmentKind::Japanese);
    }
}
