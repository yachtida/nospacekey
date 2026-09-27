//! 判別器（PR4）の公開 API 結合試験。学習済み重み（synthetic train + validation
//! 調整）での完了条件を固定する。手動ルールだけの段階を「学習モデル完成」と
//! 呼ばないため、ここでは必ず train_ngram + tune を通す。

use mixed_input::classify::classify;
use mixed_input::classify::model::ClassifyModel;
use mixed_input::classify::synth::{self, SynthConfig};
use mixed_input::classify::tune::tune;
use mixed_input::classify::tune::{train_ngram, LabeledCase};
use mixed_input::plan::SegmentKind;

/// 学習+調整は重いのでプロセス内で1回だけ行う（決定的なのでキャッシュしてよい）。
fn learned_model() -> &'static ClassifyModel {
    static MODEL: std::sync::OnceLock<ClassifyModel> = std::sync::OnceLock::new();
    MODEL.get_or_init(|| {
        let episodes = synth::generate(&SynthConfig::default());
        let train: Vec<LabeledCase> = episodes
            .iter()
            .filter(|e| e.split == synth::Split::Train)
            .map(|e| LabeledCase {
                source: e.source.clone(),
                expected: e.expected.clone(),
            })
            .collect();
        let validation: Vec<LabeledCase> = episodes
            .iter()
            .filter(|e| e.split == synth::Split::Validation)
            .map(|e| LabeledCase {
                source: e.source.clone(),
                expected: e.expected.clone(),
            })
            .collect();
        tune(
            train_ngram(&train),
            &validation,
            &synth::default_dictionary(),
        )
    })
}

#[test]
fn learned_model_splits_githubnotukaikata() {
    // PR4 の完了条件: githubnotukaikata の内部を分割できる。
    let model = learned_model();
    let source = mixed_input::classify::tune::source_from_str("githubnotukaikata");
    let results = classify(&source, model, &synth::default_dictionary());
    let top = results.first().expect("候補がある");
    let spans: Vec<(SegmentKind, &str)> = top
        .plan
        .spans
        .iter()
        .map(|span| (span.kind, span.text.as_str()))
        .collect();
    assert_eq!(
        spans,
        vec![
            (SegmentKind::Literal, "github"),
            (SegmentKind::Japanese, "notukaikata"),
        ]
    );
}

#[test]
fn learned_model_keeps_the_all_japanese_path_as_candidate() {
    // 全部日本語として扱う既存相当の経路が候補に必ず残る（計画書 5.2）。
    // githubnotukaikata はローマ字規則で全体を読めないので、全体1区間の
    // Japanese 経路の合成で担保されるケース。
    let model = learned_model();
    let source = mixed_input::classify::tune::source_from_str("githubnotukaikata");
    let results = classify(&source, model, &synth::default_dictionary());
    assert!(
        results.iter().any(|scored| scored.plan.spans.len() == 1
            && scored.plan.spans[0].kind == SegmentKind::Japanese
            && scored.plan.spans[0].text == "githubnotukaikata"),
        "全日本語経路が候補に残る: {:?}",
        results
            .iter()
            .map(|r| r
                .plan
                .spans
                .iter()
                .map(|s| format!("{}[{}]", s.kind.as_str(), s.text))
                .collect::<Vec<_>>())
            .collect::<Vec<_>>()
    );
}

#[test]
fn learned_model_does_not_latinize_plain_japanese() {
    // 「普通の日本語を英字にしてしまう誤り」を出さない（計画書 5.2）。
    // top1 が全部 Japanese であることを frozen の純日本語で確認する。
    let model = learned_model();
    let episodes = synth::generate(&SynthConfig::default());
    for episode in episodes.iter().filter(|e| {
        e.split == synth::Split::Frozen && e.category == synth::SynthCategory::PureJapanese
    }) {
        let source = mixed_input::classify::tune::source_from_str(&episode.source);
        let results = classify(&source, model, &synth::default_dictionary());
        let top = results.first().expect("候補がある");
        assert!(
            top.plan
                .spans
                .iter()
                .all(|span| span.kind == SegmentKind::Japanese),
            "{} ({}) が英字化された: {:?}",
            episode.id,
            episode.source,
            top.plan
                .spans
                .iter()
                .map(|s| format!("{}[{}]", s.kind.as_str(), s.text))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn explicit_literal_pins_the_span_against_the_learned_model() {
    // 由来の確かな明示 Literal は自動推定より優先される（計画書 2.1 / 5.1）。
    // 未指定側はかな打鍵と同じ単位粒度・読み合成で載せる。
    let model = learned_model();
    let mut composed = mixed_input::source::CompositionSource::empty(0);
    composed
        .push(mixed_input::source::SourceElement {
            provenance: mixed_input::source::Provenance::ExplicitLiteral,
            source_text: "quizzle".to_string(),
            reading: "quizzle".to_string(),
        })
        .unwrap();
    for element in mixed_input::classify::tune::kana_elements("wotukau") {
        composed.push(element).unwrap();
    }
    let results = classify(&composed, model, &synth::default_dictionary());
    let top = results.first().expect("候補がある");
    assert_eq!(top.plan.spans[0].kind, SegmentKind::Literal);
    assert_eq!(top.plan.spans[0].text, "quizzle");
    // 明示 Literal と矛盾する全日本語経路は候補へ出さない。
    assert!(results.iter().all(|scored| scored
        .plan
        .spans
        .first()
        .map(|span| span.kind == SegmentKind::Literal)
        .unwrap_or(false)));
}
