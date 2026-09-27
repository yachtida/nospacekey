//! エピソード契約の検証。計画書 11 節の必須試験と property 試験を
//! fixture 上の不変条件として表現する。ルール記号 `E_*` はテスト失敗文面で
//! そのまま使う。

use crate::episode::{Category, Episode, EpisodeEvent, ExpectedPlanSpec, FeatureMode, Intent};
use crate::plan::{InterpretationPlan, SegmentKind};
use crate::replay;

#[derive(Debug)]
pub struct Violation {
    pub episode_id: String,
    pub rule: &'static str,
    pub message: String,
}

impl Violation {
    fn new(episode_id: &str, rule: &'static str, message: impl std::fmt::Display) -> Self {
        Violation {
            episode_id: episode_id.to_string(),
            rule,
            message: message.to_string(),
        }
    }
}

/// corpus 全体の検証。id の一意性は区分ごとのテストではなくここで守る。
pub fn validate_corpus(episodes: &[Episode]) -> Vec<Violation> {
    let mut violations = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for episode in episodes {
        if !seen.insert(episode.id.clone()) {
            violations.push(Violation::new(
                &episode.id,
                "E_ID_UNIQUE",
                "id が重複している",
            ));
        }
        violations.extend(validate_episode(episode));
    }
    violations
}

pub fn validate_episode(episode: &Episode) -> Vec<Violation> {
    let mut violations = Vec::new();
    let id = episode.id.clone();

    if episode.source.is_empty() {
        violations.push(Violation::new(&id, "E_SOURCE_NONEMPTY", "ソースが空"));
    }

    // 打鍵列が宣言ソースを再現できること。fixture と再生器の契約の軸。
    let trace = match replay::replay(&episode.events) {
        Ok(trace) => trace,
        Err(error) => {
            violations.push(Violation::new(&id, "E_EVENT_BOUNDS", error.0));
            return violations;
        }
    };
    // 打鍵列が宣言ソースを説明できること。部分確定がある場合も、確定済み
    // 接頭辞の内容まで含めて一致を要求する。総量だけの比較は書き間違いや
    // 確定前後の文字置き換わりを見逃す。
    let replayed = format!("{}{}", trace.committed_source, trace.final_pending);
    if replayed != episode.source {
        violations.push(Violation::new(
            &id,
            "E_REPLAY_CONSISTENCY",
            format!(
                "打鍵列の再生結果 {:?}（確定済み {:?}）が宣言ソース {:?} と一致しない",
                trace.final_pending, trace.committed_source, episode.source
            ),
        ));
    }

    let specs: Vec<(SegmentKind, String)> = match &episode.expected {
        ExpectedPlanSpec::AllJapanese => vec![(SegmentKind::Japanese, episode.source.clone())],
        ExpectedPlanSpec::Spans(spans) => spans.clone(),
    };
    let plan = match InterpretationPlan::build(&episode.source, &specs) {
        Ok(plan) => plan,
        Err(error) => {
            violations.push(Violation::new(&id, "E_PLAN_CONCAT", error));
            return violations;
        }
    };

    // 隣接する同種 span は1つにまとめるべきもの。分割として不正。
    for window in plan.spans.windows(2) {
        if window[0].kind == window[1].kind {
            violations.push(Violation::new(
                &id,
                "E_SPAN_ADJACENT_SAME_KIND",
                format!("隣接する {} span を分けている", window[0].kind.as_str()),
            ));
        }
    }
    // Literal は原文と一字不動。結合一致から導かれるが、保証を名前で残す。
    for span in &plan.spans {
        if span.kind == SegmentKind::Literal && span.text != span.range.slice(&episode.source) {
            violations.push(Violation::new(
                &id,
                "E_LITERAL_EXACT",
                format!("{:?} が原文と一致しない", span.text),
            ));
        }
    }

    // モード規則。OFF では既存動作どおり全体日本語（計画書 8.1）。
    if episode.mode == FeatureMode::Off
        && !matches!(episode.expected, ExpectedPlanSpec::AllJapanese)
    {
        violations.push(Violation::new(
            &id,
            "E_MODE_OFF_ALL_JAPANESE",
            "OFF なのに混在解釈を期待している",
        ));
    }

    match episode.category {
        Category::PureJapanese => {
            if !matches!(episode.expected, ExpectedPlanSpec::AllJapanese) {
                violations.push(Violation::new(
                    &id,
                    "E_PURE_JP_NO_LITERAL",
                    "純日本語の契約例で英字区間を期待している",
                ));
            }
        }
        Category::Ambiguous => {
            let intent_ok = episode.intent == Intent::Ambiguous;
            let alternative_ok = episode
                .alternative_literal
                .as_ref()
                .map(|a| episode.source.contains(a.as_str()))
                .unwrap_or(false);
            let mode_ok = episode.mode == FeatureMode::CandidatesOnly;
            let expected_ok = matches!(episode.expected, ExpectedPlanSpec::AllJapanese);
            if !(intent_ok && alternative_ok && mode_ok && expected_ok) {
                violations.push(Violation::new(
                    &id,
                    "E_AMBIGUOUS_RULES",
                    format!(
                        "曖昧例は既定日本語＋候補提示のみ。intent_ambiguous={intent_ok} \
                         alternative_literal_ok={alternative_ok} candidates_mode={mode_ok} \
                         default_japanese={expected_ok}"
                    ),
                ));
            }
        }
        Category::LowercaseMixed
        | Category::CapsSymbol
        | Category::UnknownWords
        | Category::MultiMixed
            if episode.mode != FeatureMode::Off =>
        {
            // 複数混在は「区間が複数」を意味し Literal の個数は問わない
            // （kyouhaPythonwotukau は Literal 1個で正しい）。
            let literals = plan
                .spans
                .iter()
                .filter(|span| span.kind == SegmentKind::Literal)
                .count();
            let (need_literals, need_spans) = if episode.category == Category::MultiMixed {
                (1, 2)
            } else {
                (1, 1)
            };
            if literals < need_literals || plan.spans.len() < need_spans {
                violations.push(Violation::new(
                    &id,
                    "E_MIXED_HAS_LITERAL",
                    format!(
                        "混在区分なのに Literal {literals} 個・区間 {} 個（必要 Literal {need_literals}・区間 {need_spans}）",
                        plan.spans.len()
                    ),
                ));
            }
        }
        _ => {}
    }

    if let Err(message) = check_category_expression(episode) {
        violations.push(Violation::new(&id, "E_CATEGORY_EXPRESSION", message));
    }

    // commit fence: 確定境界は Literal 区間の途中を割らない（計画書 8.4）。
    // expected plan は宣言ソース全体を被覆するので、範囲はこのまま絶対位置。
    for span in &plan.spans {
        if span.kind != SegmentKind::Literal {
            continue;
        }
        for boundary in &trace.commit_boundaries {
            if span.range.start.get() < *boundary && *boundary < span.range.end.get() {
                violations.push(Violation::new(
                    &id,
                    "E_COMMIT_FENCE",
                    format!(
                        "確定境界 {boundary} が Literal 区間 [{}, {}) の途中",
                        span.range.start.get(),
                        span.range.end.get()
                    ),
                ));
            }
        }
    }

    // 利用者の明示変更は自動推定より強い（計画書 2.1）。Escape で解除された
    // 未確定側の指定を除き、確定済み側を含む効力を持つ全件が期待解釈に反映
    // されていることを要求する。範囲は再生器が編集へ追従させているので、
    // 編集後の絶対位置で照合する。
    for (range, kind) in trace
        .reinterpretations
        .iter()
        .chain(&trace.committed_reinterpretations)
    {
        let honored = plan.spans.iter().any(|span| {
            span.kind == *kind
                && span.range.start.get() <= range.start.get()
                && range.end.get() <= span.range.end.get()
        });
        if !honored {
            violations.push(Violation::new(
                &id,
                "E_EXPLICIT_HONORED",
                format!(
                    "明示変更 {} {:?} が期待解釈に反映されていない",
                    kind.as_str(),
                    range
                ),
            ));
        }
    }

    violations
}

/// 区分ごとに「PR1 の完了条件が fixture で表現されている」ことを要求する。
/// 障害注入や同一性の効果は各 PR で実装されるが、例の存在はここで固定する。
fn check_category_expression(episode: &Episode) -> Result<(), String> {
    let tags: Vec<&str> = episode
        .events
        .iter()
        .filter_map(|event| event.annotation_tag())
        .collect();
    let has_tag = |tag: &str| tags.contains(&tag);
    let has_event = |predicate: &dyn Fn(&EpisodeEvent) -> bool| episode.events.iter().any(predicate);
    match episode.category {
        Category::EditEvents => {
            if !has_event(&|event| matches!(event, EpisodeEvent::DeleteBackward { .. })) {
                return Err("削除イベントを含まない".to_string());
            }
        }
        Category::Position => {
            if !has_event(&|event| matches!(event, EpisodeEvent::MoveTo { .. })) {
                return Err("caret 移動イベントを含まない".to_string());
            }
        }
        Category::ExplicitInput => {
            let explicit = has_event(&|event| matches!(event, EpisodeEvent::Reinterpret { .. }))
                || has_tag("temporary_alfanumeric");
            if !explicit {
                return Err("明示入力（一時英数・解釈変更）を含まない".to_string());
            }
        }
        Category::AsyncIdentity => {
            let identity_tag = tags.iter().any(|t| {
                t.starts_with("stale") || t.starts_with("response") || t.starts_with("request")
            });
            if !identity_tag {
                return Err("同一性・古い応答の注記を含まない".to_string());
            }
        }
        Category::PartialCommit => {
            if !has_event(&|event| {
                matches!(
                    event,
                    EpisodeEvent::CommitPrefix { .. } | EpisodeEvent::CommitAll
                )
            }) {
                return Err("部分確定イベントを含まない".to_string());
            }
        }
        Category::Persistence => {
            const REQUIRED: &[&str] = &[
                "tsf_apply_rejected",
                "receipt_resent",
                "learning_generation_changed",
                "double_insertion_guarded",
                "commit_receipt_mismatch",
            ];
            if !REQUIRED.iter().any(|tag| has_tag(tag)) {
                return Err("保存・学習契約の注記を含まない".to_string());
            }
        }
        Category::Compat => {
            const REQUIRED: &[&str] = &[
                "old_tip_new_engine",
                "new_tip_old_engine",
                "capability_missing_fallback",
                "literal_constraint_unsupported_fallback",
            ];
            if !REQUIRED.iter().any(|tag| has_tag(tag)) {
                return Err("新旧組合せの注記を含まない".to_string());
            }
        }
        Category::Failure => {
            const REQUIRED: &[&str] = &[
                "zenzai_stopped",
                "engine_disconnected",
                "model_corrupt",
                "gpu_worker_stopped",
                "engine_reconnect_mid_input",
                "model_load_failed",
            ];
            if !REQUIRED.iter().any(|tag| has_tag(tag)) {
                return Err("障害注入の注記を含まない".to_string());
            }
        }
        _ => {}
    }
    Ok(())
}
