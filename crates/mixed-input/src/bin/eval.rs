//! mixed-input-eval — 判別器のオフライン学習・評価 CLI（PR4）。
//!
//! 使い方:
//!   mixed-input-eval train --out <model.txt>     synthetic で学習+調整してモデルを書く
//!   mixed-input-eval eval --model <model.txt>    frozen 分割 + 契約 fixture を評価
//!   mixed-input-eval demo <source>               1入力の解釈候補を表示
//!
//! 評価は報告用で、ここでは品質ゲート（合格/不合格の終了コード）を出さない。
//! 品質ゲートは PR7 の validation が担う（計画書 PR4: 手動ルール段階を
//! 「学習モデル完成」と呼ばない。学習済み重みの再現手順はこの CLI が正本）。

use std::collections::BTreeMap;
use std::path::PathBuf;

use mixed_input::classify::classify;
use mixed_input::classify::model::ClassifyModel;
use mixed_input::classify::synth::{self, SynthConfig};
use mixed_input::classify::tune::{
    evaluate_case, evaluate_prepared, source_from_str, train_ngram, tune, LabeledCase,
};
use mixed_input::episode::{parse_episodes, Category, ExpectedPlanSpec};
use mixed_input::plan::SegmentKind;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("train") => run_train(&args[2..]),
        Some("eval") => run_eval(&args[2..]),
        Some("demo") => run_demo(&args[2..]),
        _ => {
            eprintln!(
                "使い方: mixed-input-eval train --out <model> | eval --model <model> | demo <source>"
            );
            std::process::exit(2);
        }
    }
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|at| args.get(at + 1))
        .cloned()
}

fn run_train(args: &[String]) {
    let Some(out) = flag_value(args, "--out") else {
        eprintln!("train には --out <model> が必要");
        std::process::exit(2);
    };
    let seed: u64 = flag_value(args, "--seed")
        .and_then(|s| s.parse().ok())
        .unwrap_or(42);
    let episodes = synth::generate(&SynthConfig { seed });
    let dictionary = synth::default_dictionary();
    let to_cases = |split: synth::Split| -> Vec<LabeledCase> {
        episodes
            .iter()
            .filter(|e| e.split == split)
            .map(|e| LabeledCase {
                source: e.source.clone(),
                expected: e.expected.clone(),
            })
            .collect()
    };
    let train = to_cases(synth::Split::Train);
    let validation = to_cases(synth::Split::Validation);
    let model = tune(train_ngram(&train), &validation, &dictionary);
    let artifact = model.to_artifact();
    std::fs::write(&out, &artifact).unwrap_or_else(|e| {
        eprintln!("モデルの書き込みに失敗: {out}: {e}");
        std::process::exit(1);
    });
    println!(
        "train={} validation={} → {out} ({} bytes)",
        train.len(),
        validation.len(),
        artifact.len()
    );
}

fn run_eval(args: &[String]) {
    let Some(model_path) = flag_value(args, "--model") else {
        eprintln!("eval には --model <model> が必要");
        std::process::exit(2);
    };
    let text = std::fs::read_to_string(&model_path).unwrap_or_else(|e| {
        eprintln!("モデルの読み込みに失敗: {model_path}: {e}");
        std::process::exit(1);
    });
    let model = ClassifyModel::from_artifact(&text).unwrap_or_else(|e| {
        eprintln!("モデルが不正: {e}");
        std::process::exit(1);
    });
    let dictionary = synth::default_dictionary();
    let seed: u64 = flag_value(args, "--seed")
        .and_then(|s| s.parse().ok())
        .unwrap_or(42);

    // frozen 分割の synthetic（カテゴリ別）。
    let episodes = synth::generate(&SynthConfig { seed });
    let mut by_category: BTreeMap<String, Vec<FixtureCheck>> = BTreeMap::new();
    for episode in episodes.iter().filter(|e| e.split == synth::Split::Frozen) {
        by_category
            .entry(episode.category.as_str().to_string())
            .or_default()
            .push(FixtureCheck::Plain(LabeledCase {
                source: episode.source.clone(),
                expected: episode.expected.clone(),
            }));
    }
    // 契約 fixture。編集・非同期系（混在判定の対象でないカテゴリ）は除外し、
    // 読込・解釈エラーは評価用途として fail-fast にする。
    let fixture_dir =
        flag_value(args, "--fixtures").unwrap_or_else(|| "crates/mixed-input/fixtures".to_string());
    let mut fixture_count = 0;
    let mut skipped_pins_japanese = 0;
    for entry in fixture_files(&fixture_dir) {
        let text = std::fs::read_to_string(&entry).unwrap_or_else(|e| {
            eprintln!("fixture の読み込みに失敗: {}: {e}", entry.display());
            std::process::exit(1);
        });
        let parsed = parse_episodes(&text).unwrap_or_else(|e| {
            eprintln!("fixture の解釈に失敗: {}: {e}", entry.display());
            std::process::exit(1);
        });
        for episode in parsed {
            if matches!(
                episode.category,
                Category::EditEvents
                    | Category::Position
                    | Category::AsyncIdentity
                    | Category::PartialCommit
                    | Category::Persistence
                    | Category::Compat
                    | Category::Failure
                    | Category::Unicode
            ) {
                continue;
            }
            let check = match episode.category {
                // 曖昧例の契約: 日本語 top1・自動適用しない・英字解釈が上位候補にいる。
                Category::Ambiguous => FixtureCheck::Ambiguous {
                    source: episode.source,
                    alternative: episode.alternative_literal,
                },
                // 明示指定（一時英数・再解釈）は由来つき source で評価する。
                Category::ExplicitInput => {
                    // reinterpret:japanese は領域を Explicit Japanese 相当に固定する
                    // 契約だが、PR2 の由来契約（Typed / ResolvedKana / ExplicitLiteral）
                    // にその表現がない。Typed Kana 扱いで評価すると契約と異なる
                    // 評価になるため、**イベントを最後まで再生した時点で** Japanese
                    // pin がまだ残っている fixture だけ PR4 のレポートから除外し、
                    // PR5 の候補オブジェクトで扱う（Escape で解除されたものは通常
                    // 評価する。除外件数は下に出力する）。
                    let (composed, pins_japanese) =
                        replay_reinterprets(&episode.source, &episode.events);
                    if pins_japanese {
                        skipped_pins_japanese += 1;
                        continue;
                    }
                    FixtureCheck::Explicit {
                        source: composed,
                        expected: expected_spans(&episode.expected, &episode.source.clone()),
                    }
                }
                _ => FixtureCheck::Plain(LabeledCase {
                    expected: expected_spans(&episode.expected, &episode.source),
                    source: episode.source,
                }),
            };
            by_category
                .entry(format!("fixture:{}", episode.category.as_str()))
                .or_default()
                .push(check);
            fixture_count += 1;
        }
    }

    println!("model={}", PathBuf::from(&model_path).display());
    println!("category            n   exact  top4  retained  boundaryF");
    for (category, cases) in &by_category {
        let mut exact = 0;
        let mut top4 = 0;
        let mut retained = 0;
        let mut f_sum = 0.0;
        for check in cases {
            let outcome = match check {
                FixtureCheck::Plain(case) => evaluate_case(case, &model, &dictionary),
                FixtureCheck::Explicit { source, expected } => {
                    evaluate_prepared(source, expected, &model, &dictionary)
                }
                FixtureCheck::Ambiguous {
                    source,
                    alternative,
                } => ambiguous_outcome(source, alternative.as_deref(), &model, &dictionary),
            };
            exact += outcome.exact_match as usize;
            top4 += outcome.top_k_hit as usize;
            retained += outcome.retained as usize;
            f_sum += outcome.boundary_f;
        }
        println!(
            "{:<18} {:>3}  {:>5}  {:>4}  {:>8}  {:>9.3}",
            category,
            cases.len(),
            exact,
            top4,
            retained,
            f_sum / cases.len() as f64
        );
    }
    println!("fixture cases: {fixture_count} (reinterpret:japanese の Explicit Japanese 固定は PR5 まで対象外: {skipped_pins_japanese} 件除外)");
}

fn run_demo(args: &[String]) {
    let Some(source) = args.first() else {
        eprintln!("demo には source が必要");
        std::process::exit(2);
    };
    // モデル指定がなければ seed 42 でその場で学習する（動作確認用）。
    let model = match flag_value(args, "--model") {
        Some(path) => {
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                eprintln!("モデルの読み込みに失敗: {path}: {e}");
                std::process::exit(1);
            });
            ClassifyModel::from_artifact(&text).unwrap_or_else(|e| {
                eprintln!("モデルが不正: {e}");
                std::process::exit(1);
            })
        }
        None => {
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
        }
    };
    let results = classify(
        &source_from_str(source),
        &model,
        &synth::default_dictionary(),
    );
    for (rank, scored) in results.iter().enumerate() {
        let spans: Vec<String> = scored
            .plan
            .spans
            .iter()
            .map(|s| format!("{}[{}]", s.kind.as_str(), s.text))
            .collect();
        println!(
            "#{} score={:.3} margin={:?} retained={} {}",
            rank + 1,
            scored.score,
            scored.margin,
            scored.retained,
            spans.join(" ")
        );
    }
}

fn expected_spans(spec: &ExpectedPlanSpec, source: &str) -> Vec<(SegmentKind, String)> {
    match spec {
        ExpectedPlanSpec::Spans(spans) => spans.clone(),
        ExpectedPlanSpec::AllJapanese => {
            vec![(SegmentKind::Japanese, source.to_string())]
        }
    }
}

/// fixture の評価方法。曖昧例と明示指定は契約に沿った別の検査をする。
enum FixtureCheck {
    Plain(LabeledCase),
    Explicit {
        source: mixed_input::source::CompositionSource,
        expected: Vec<(SegmentKind, String)>,
    },
    Ambiguous {
        source: String,
        alternative: Option<String>,
    },
}

/// 曖昧例の契約検査（計画書 5.3 / fixture:ambiguous）。日本語 top1・保留（自動
/// 適用しない）・英字解釈が上位候補にいる、の3条件を exact と数える。
fn ambiguous_outcome(
    source: &str,
    alternative: Option<&str>,
    model: &ClassifyModel,
    dictionary: &mixed_input::classify::edge::Dictionary,
) -> mixed_input::classify::tune::CaseOutcome {
    let results = classify(&source_from_str(source), model, dictionary);
    let top1 = results.first();
    let all_japanese_top1 = top1
        .map(|r| {
            r.plan.spans.len() == 1
                && r.plan.spans[0].kind == SegmentKind::Japanese
                && r.plan.spans[0].text == source
        })
        .unwrap_or(false);
    let retained = top1.map(|r| r.retained).unwrap_or(false);
    let alternative_present = alternative.is_some_and(|alt| {
        results.iter().any(|scored| {
            scored
                .plan
                .spans
                .iter()
                .any(|span| span.kind == SegmentKind::Literal && span.text.contains(alt))
        })
    });
    let boundary_f = top1
        .map(|_| {
            let expected = vec![(SegmentKind::Japanese, source.to_string())];
            evaluate_prepared(&source_from_str(source), &expected, model, dictionary).boundary_f
        })
        .unwrap_or(0.0);
    mixed_input::classify::tune::CaseOutcome {
        exact_match: all_japanese_top1 && retained && alternative_present,
        top_k_hit: alternative_present,
        retained,
        boundary_f,
        latinized_pure: false,
    }
}

/// fixture の明示指定（再解釈イベント）をイベント列の最後まで再生して由来つき
/// CompositionSource へ載せる。位置ごとに後の指定を優先し、Literal 指定の範囲を
/// ExplicitLiteral にする。一時英数（annotate:temporary_alfanumeric）は「自動推定に
/// 渡さない」契約なので全体を ExplicitLiteral に、Escape は明示変更を全て解除する。
/// 戻り値の bool は再生後も Japanese pin（Explicit Japanese 相当の固定）が残って
/// いるか。PR2 の由来契約にその表現が無いため、残っている場合は評価対象外にする。
fn replay_reinterprets(
    source: &str,
    events: &[mixed_input::episode::EpisodeEvent],
) -> (mixed_input::source::CompositionSource, bool) {
    use mixed_input::episode::EpisodeEvent;
    use mixed_input::plan::SegmentKind;
    use mixed_input::source::{CompositionSource, Provenance, SourceElement};
    let chars: Vec<char> = source.chars().collect();
    let mut kinds: Vec<Option<SegmentKind>> = vec![None; chars.len()];
    let mut whole_alphanumeric = false;
    for event in events {
        match event {
            EpisodeEvent::Reinterpret { range, as_kind } => {
                for at in range.start.get()..range.end.get() {
                    if (at as usize) < kinds.len() {
                        kinds[at as usize] = Some(*as_kind);
                    }
                }
            }
            EpisodeEvent::Escape => {
                kinds.fill(None);
            }
            EpisodeEvent::Annotate { tag } if tag == "temporary_alfanumeric" => {
                whole_alphanumeric = true;
            }
            _ => {}
        }
    }
    if whole_alphanumeric {
        kinds.fill(Some(SegmentKind::Literal));
    }
    let pins_japanese = kinds
        .iter()
        .any(|kind| *kind == Some(SegmentKind::Japanese));
    let mut composed = CompositionSource::empty(0);
    let mut index = 0;
    while index < chars.len() {
        let kind = kinds[index];
        let start = index;
        while index < chars.len() && kinds[index] == kind {
            index += 1;
        }
        let text: String = chars[start..index].iter().collect();
        if kind == Some(SegmentKind::Literal) {
            let _ = composed.push(SourceElement {
                provenance: Provenance::ExplicitLiteral,
                source_text: text.clone(),
                reading: text,
            });
        } else {
            // 未指定の範囲はかな打鍵として単位粒度に分解する（大小文字の保存と
            // 読み合成の実態に合わせる）。
            for element in source_from_str(&text).elements().to_vec() {
                let _ = composed.push(element.clone());
            }
        }
    }
    (composed, pins_japanese)
}

fn fixture_files(dir: &str) -> Vec<PathBuf> {
    // 評価用途では typo やアクセス失敗を黙って空にしない（fail-fast）。
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| {
        eprintln!("fixture ディレクトリが読めない: {dir}: {e}");
        std::process::exit(1);
    });
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| {
            eprintln!("fixture ディレクトリの走査に失敗: {dir}: {e}");
            std::process::exit(1);
        });
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            out.push(path);
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mixed_input::episode::EpisodeEvent;
    use mixed_input::source::Provenance;

    fn events(specs: &[&str]) -> Vec<EpisodeEvent> {
        specs
            .iter()
            .map(|s| EpisodeEvent::parse(s).unwrap())
            .collect()
    }

    #[test]
    fn japanese_pin_survives_until_escape_releases_it() {
        // Japanese pin はイベント列の最終状態で判定する。履歴に一度でも
        // reinterpret:japanese があれば無条件除外すると、Escape で解除された
        // fixture（例: type:made → reinterpret:0:4:japanese → escape）まで
        // 評価から漏れる。
        let (composed, pinned) =
            replay_reinterprets("made", &events(&["reinterpret:0:4:japanese"]));
        assert!(pinned, "Escape 前は Japanese pin が残る");

        let (composed, pinned) =
            replay_reinterprets("made", &events(&["reinterpret:0:4:japanese", "escape"]));
        assert!(!pinned, "Escape で Japanese pin は解除される");
        // 解除後は通常のかな打鍵（Typed Kana）として載る。
        assert!(composed
            .elements()
            .iter()
            .all(|element| matches!(element.provenance, Provenance::Typed { .. })));

        // Literal pin も Escape で解除される（ex-009 と対になる方向）。
        let (_, pinned) =
            replay_reinterprets("madekite", &events(&["reinterpret:0:4:literal", "escape"]));
        assert!(!pinned);
    }
}
