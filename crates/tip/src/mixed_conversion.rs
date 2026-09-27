//! 混在変換の TIP 側アダプター（実装計画 §7）。
//! StartSession 応答の capability 判定、採用 Projection からの MixedConvert span 列の
//! 構築、MixedResult の同一性・内容検証を持つ。判別器（PR4）・表示・確定への接続は
//! ここに含めない。

use ipc::protocol::{MixedSpan, MixedSpanResult};
use mixed_input::projection::Projection;

/// StartSession 応答の capability 広告に混在変換（mixed_input_v1）があるか。
/// 旧エンジンは capabilities を返さない（None＝非対応）ので false。
pub(crate) fn engine_supports_mixed(capabilities: Option<&Vec<String>>) -> bool {
    capabilities.is_some_and(|list| list.iter().any(|capability| capability == "mixed_input_v1"))
}

/// 採用中の Projection から MixedConvert の span 列を作る。Japanese は区間の読み
/// （reading_text）、Literal は原文（reading_text = 原文）を載せる。span の読み範囲は
/// Projection の読み座標（Unicode scalar 半開区間）をそのまま使う。
pub(crate) fn spans_from_projection(projection: &Projection) -> Vec<MixedSpan> {
    projection
        .spans
        .iter()
        .map(|span| MixedSpan {
            kind: span.kind.as_str().to_string(),
            reading_start: span.reading.start.get(),
            reading_end: span.reading.end.get(),
            text: span.reading_text.clone(),
        })
        .collect()
}

/// 要求の同一性キー。MixedConvert に渡した値をそのまま保持し、応答のエコーと照合する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MixedIdentity {
    pub composition: u64,
    pub revision: u64,
    pub configuration_generation: u64,
    pub connection_generation: u64,
    pub request_id: u64,
    pub source_revision: u64,
    pub plan_id: u64,
}

/// 検証済みの混在変換結果。`engine_epoch`/`learning_generation` は日本語区間の
/// 学習 token を発行したエンジンの同一性（確定 receipt の学習先と合せる）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidatedMixed {
    pub text: String,
    pub engine_epoch: String,
    pub learning_generation: u64,
    pub spans: Vec<ValidatedMixedSpan>,
}

/// 検証済みの1区間。`candidate_token` は Japanese のみ（Literal は学習対象外で None）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidatedMixedSpan {
    pub is_literal: bool,
    pub reading_start: u32,
    pub reading_end: u32,
    pub text: String,
    pub candidate_token: Option<String>,
}

/// MixedResult の検証。すべて成り立つときだけ Some:
/// - 同一性キーは要求の値と一致（stale 応答の破棄。§7.2）
/// - span 列は要求と同じ数・順序・種類・読み範囲
/// - Literal span の表示は要求の原文と一字不動、token なし
/// - Japanese span の表示は空でなく token がある
/// - 全体テキストは span 表示の連結
pub(crate) fn validate_mixed_result(
    identity: &MixedIdentity,
    request_spans: &[MixedSpan],
    composition: u64,
    revision: u64,
    configuration_generation: u64,
    connection_generation: u64,
    request_id: u64,
    source_revision: u64,
    plan_id: u64,
    engine_epoch: &str,
    learning_generation: u64,
    text: &str,
    spans: &[MixedSpanResult],
) -> Option<ValidatedMixed> {
    if identity.composition != composition
        || identity.revision != revision
        || identity.configuration_generation != configuration_generation
        || identity.connection_generation != connection_generation
        || identity.request_id != request_id
        || identity.source_revision != source_revision
        || identity.plan_id != plan_id
        || engine_epoch.is_empty()
    {
        return None;
    }
    if spans.len() != request_spans.len() {
        return None;
    }
    let mut concatenated = String::new();
    let mut validated = Vec::with_capacity(spans.len());
    for (request, result) in request_spans.iter().zip(spans) {
        if result.kind != request.kind
            || result.reading_start != request.reading_start
            || result.reading_end != request.reading_end
        {
            return None;
        }
        let is_literal = request.kind == "literal";
        if is_literal {
            if result.text != request.text || result.candidate_token.is_some() {
                return None;
            }
        } else if result.text.is_empty() || result.candidate_token.is_none() {
            return None;
        }
        concatenated.push_str(&result.text);
        validated.push(ValidatedMixedSpan {
            is_literal,
            reading_start: result.reading_start,
            reading_end: result.reading_end,
            text: result.text.clone(),
            candidate_token: result.candidate_token.clone(),
        });
    }
    if concatenated != text {
        return None;
    }
    Some(ValidatedMixed {
        text: text.to_string(),
        engine_epoch: engine_epoch.to_string(),
        learning_generation,
        spans: validated,
    })
}

/// 採用中の混在表示。表示テキスト（検証済み）と確定 receipt の材料を持つ。
/// PR3 は固定 Plan の試験経路から作り、PR4 は判別器・PR5 は候補選択から作る。
#[derive(Clone, Debug)]
pub(crate) struct MixedDisplay {
    pub projection: Projection,
    pub identity: MixedIdentity,
    pub display: ValidatedMixed,
}

impl MixedDisplay {
    /// 表示がまだ現在の composition/revision に対応するか。1打鍵でも編集が
    /// 入れば revision が進むので不成立になり、通常の読み表示・確定へ戻る。
    pub fn is_current(&self, composition: u64, revision: u64) -> bool {
        self.identity.composition == composition && self.identity.revision == revision
    }
}

/// 混在確定の receipt（実装計画 §6.3）。日本語区間だけ Candidate token で学習し、
/// Literal 区間は NotLearningTarget で学習対象外にする。混在全文の sentence token は
/// 出さない（初版の方針）。読みは採用 Projection の読み（Literal は原文が読みに
/// 載る — PR2 の契約）。確定本文が検証済み結果と一致しない場合は None。
pub(crate) fn commit_receipt(
    commit_id: ipc::clause::CommitId,
    projection: &Projection,
    validated: &ValidatedMixed,
    committed_text: &str,
) -> Option<ipc::clause::CommitReceipt> {
    if committed_text != validated.text {
        return None;
    }
    let reading = projection.reading();
    if projection.spans.len() != validated.spans.len() {
        return None;
    }
    let mut intervals = Vec::with_capacity(projection.spans.len());
    for (span, validated) in projection.spans.iter().zip(&validated.spans) {
        let learning = if validated.is_literal {
            ipc::clause::IntervalLearning::None {
                reason: ipc::clause::NoLearningReason::NotLearningTarget,
            }
        } else {
            ipc::clause::IntervalLearning::Candidate {
                token: validated.candidate_token.clone()?,
                explicitly_selected: true,
            }
        };
        intervals.push(ipc::clause::CommitInterval {
            reading_start: ipc::clause::ReadingPosition(span.reading.start.get()),
            reading_end: ipc::clause::ReadingPosition(span.reading.end.get()),
            surface: validated.text.clone(),
            learning,
        });
    }
    let receipt = ipc::clause::CommitReceipt {
        commit_id,
        engine_epoch: validated.engine_epoch.clone(),
        learning_generation: validated.learning_generation,
        reading,
        text: validated.text.clone(),
        intervals,
        sentence_token: None,
    };
    // 範囲・連結の不変条件を確定前にもう一度検証する（receipt 側の契約検証）。
    receipt
        .validate(|token, interval| {
            validated.spans.iter().any(|span| {
                !span.is_literal
                    && span.candidate_token.as_deref() == Some(token)
                    && span.reading_start == interval.reading_start.0
                    && span.reading_end == interval.reading_end.0
            })
        })
        .ok()?;
    Some(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mixed_input::plan::{InterpretationPlan, SegmentKind};
    use mixed_input::source::{CompositionSource, Provenance, SourceElement, SourceStyle};

    fn projection_fixture() -> Projection {
        // kyouha(きょうは) + Python(原文) + wotukau(をつかう) の固定 Plan。
        let source = CompositionSource::try_new(
            vec![
                SourceElement {
                    provenance: Provenance::Typed { style: SourceStyle::Kana },
                    source_text: "kyouha".to_string(),
                    reading: "きょうは".to_string(),
                },
                SourceElement {
                    provenance: Provenance::Typed { style: SourceStyle::Kana },
                    source_text: "Python".to_string(),
                    reading: "Python".to_string(),
                },
                SourceElement {
                    provenance: Provenance::Typed { style: SourceStyle::Kana },
                    source_text: "wotukau".to_string(),
                    reading: "をつかう".to_string(),
                },
            ],
            5,
        )
        .unwrap();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Japanese, "kyouha".to_string()),
                (SegmentKind::Literal, "Python".to_string()),
                (SegmentKind::Japanese, "wotukau".to_string()),
            ],
        )
        .unwrap();
        Projection::build(2, &source, &plan).unwrap()
    }

    fn identity() -> MixedIdentity {
        MixedIdentity {
            composition: 8,
            revision: 13,
            configuration_generation: 2,
            connection_generation: 5,
            request_id: 4,
            source_revision: 9,
            plan_id: 3,
        }
    }

    #[test]
    fn capability_gate_matches_only_mixed_input_v1() {
        assert!(engine_supports_mixed(Some(&vec!["mixed_input_v1".to_string()])));
        assert!(engine_supports_mixed(Some(&vec![
            "other".to_string(),
            "mixed_input_v1".to_string()
        ])));
        assert!(!engine_supports_mixed(Some(&vec!["other".to_string()])));
        assert!(!engine_supports_mixed(None), "旧エンジンは capabilities を返さない");
    }

    #[test]
    fn spans_follow_the_projection_reading_coordinates() {
        let spans = spans_from_projection(&projection_fixture());
        assert_eq!(
            spans,
            vec![
                MixedSpan {
                    kind: "japanese".to_string(),
                    reading_start: 0,
                    reading_end: 4,
                    text: "きょうは".to_string(),
                },
                MixedSpan {
                    kind: "literal".to_string(),
                    reading_start: 4,
                    reading_end: 10,
                    text: "Python".to_string(),
                },
                MixedSpan {
                    kind: "japanese".to_string(),
                    reading_start: 10,
                    reading_end: 14,
                    text: "をつかう".to_string(),
                },
            ]
        );
    }

    fn result_spans() -> Vec<MixedSpanResult> {
        vec![
            MixedSpanResult {
                kind: "japanese".to_string(),
                reading_start: 0,
                reading_end: 4,
                text: "今日は".to_string(),
                candidate_token: Some("epoch:1".to_string()),
            },
            MixedSpanResult {
                kind: "literal".to_string(),
                reading_start: 4,
                reading_end: 10,
                text: "Python".to_string(),
                candidate_token: None,
            },
            MixedSpanResult {
                kind: "japanese".to_string(),
                reading_start: 10,
                reading_end: 14,
                text: "を使う".to_string(),
                candidate_token: Some("epoch:2".to_string()),
            },
        ]
    }

    #[test]
    fn valid_result_passes_and_carries_tokens() {
        let request = spans_from_projection(&projection_fixture());
        let identity = identity();
        let validated = validate_mixed_result(
            &identity, &request, 8, 13, 2, 5, 4, 9, 3, "engine-epoch", 6,
            "今日はPythonを使う", &result_spans(),
        )
        .expect("整合する応答は検証を通る");
        assert_eq!(validated.text, "今日はPythonを使う");
        assert_eq!(validated.engine_epoch, "engine-epoch");
        assert_eq!(validated.learning_generation, 6);
        assert_eq!(validated.spans.len(), 3);
        assert!(!validated.spans[0].is_literal);
        assert_eq!(validated.spans[0].candidate_token.as_deref(), Some("epoch:1"));
        assert!(validated.spans[1].is_literal);
        assert_eq!(validated.spans[1].text, "Python");
        assert_eq!(validated.spans[1].candidate_token, None);
        // epoch が空の応答は受けない（学習先が特定できない）。
        assert!(validate_mixed_result(
            &identity, &request, 8, 13, 2, 5, 4, 9, 3, "", 6,
            "今日はPythonを使う", &result_spans(),
        )
        .is_none());
    }

    #[test]
    fn stale_identity_is_rejected() {
        let request = spans_from_projection(&projection_fixture());
        let identity = identity();
        // 要求と1つでもキーが違う応答は適用しない（古い応答の破棄）。
        assert!(validate_mixed_result(
            &identity, &request, 8, 14, 2, 5, 4, 9, 3, "engine-epoch", 6,
            "今日はPythonを使う", &result_spans(),
        )
        .is_none());
        assert!(validate_mixed_result(
            &identity, &request, 8, 13, 2, 5, 4, 9, 4, "engine-epoch", 6,
            "今日はPythonを使う", &result_spans(),
        )
        .is_none());
        assert!(validate_mixed_result(
            &identity, &request, 8, 13, 2, 5, 5, 9, 3, "engine-epoch", 6,
            "今日はPythonを使う", &result_spans(),
        )
        .is_none());
        assert!(validate_mixed_result(
            &identity, &request, 8, 13, 2, 5, 4, 8, 3, "engine-epoch", 6,
            "今日はPythonを使う", &result_spans(),
        )
        .is_none());
    }

    #[test]
    fn literal_mutation_or_japanese_token_loss_is_rejected() {
        let request = spans_from_projection(&projection_fixture());
        let identity = identity();
        // Literal が書き換えられている（大小変更）
        let mut spans = result_spans();
        spans[1].text = "python".to_string();
        assert!(validate_mixed_result(
            &identity, &request, 8, 13, 2, 5, 4, 9, 3, "engine-epoch", 6,
            "今日はpythonを使う", &spans,
        )
        .is_none());
        let mut spans = result_spans();
        spans[1].candidate_token = Some("epoch:3".to_string());
        assert!(validate_mixed_result(
            &identity, &request, 8, 13, 2, 5, 4, 9, 3, "engine-epoch", 6,
            "今日はPythonを使う", &spans,
        )
        .is_none());
        // Japanese の token 欠落
        let mut spans = result_spans();
        spans[0].candidate_token = None;
        assert!(validate_mixed_result(
            &identity, &request, 8, 13, 2, 5, 4, 9, 3, "engine-epoch", 6,
            "今日はPythonを使う", &spans,
        )
        .is_none());
        // 全体テキストが span 連結と一致しない
        assert!(validate_mixed_result(
            &identity, &request, 8, 13, 2, 5, 4, 9, 3, "engine-epoch", 6,
            "今日はPython", &result_spans(),
        )
        .is_none());
        // span 数・範囲のずれ
        let short = &result_spans()[..2];
        assert!(validate_mixed_result(
            &identity, &request, 8, 13, 2, 5, 4, 9, 3, "engine-epoch", 6,
            "今日はPython", short,
        )
        .is_none());
    }

    #[test]
    fn mixed_display_goes_stale_after_any_edit() {
        let projection = projection_fixture();
        let request = spans_from_projection(&projection);
        let identity = identity();
        let validated = validate_mixed_result(
            &identity, &request, 8, 13, 2, 5, 4, 9, 3, "engine-epoch", 6,
            "今日はPythonを使う", &result_spans(),
        )
        .unwrap();
        let display = MixedDisplay { projection, identity, display: validated };
        assert!(display.is_current(8, 13), "採用直後は現在のcompositionに一致");
        assert!(!display.is_current(8, 14), "1打鍵でも編集が入れば不成立");
        assert!(!display.is_current(9, 13), "composition が変われば不成立");
    }

    #[test]
    fn commit_receipt_learns_only_japanese_intervals() {
        use ipc::clause::{CommitId, IntervalLearning, NoLearningReason};
        let projection = projection_fixture();
        let request = spans_from_projection(&projection);
        let identity = identity();
        let validated = validate_mixed_result(
            &identity, &request, 8, 13, 2, 5, 4, 9, 3, "engine-epoch", 6,
            "今日はPythonを使う", &result_spans(),
        )
        .unwrap();
        let receipt = commit_receipt(
            CommitId {
                client_instance: "tip".to_string(),
                sequence: 1,
            },
            &projection,
            &validated,
            "今日はPythonを使う",
        )
        .expect("確定本文が一致すれば receipt を組める");
        // 読みは採用 Projection の読み（Literal は原文が読みに載る）。
        assert_eq!(receipt.reading, "きょうはPythonをつかう");
        assert_eq!(receipt.text, "今日はPythonを使う");
        assert_eq!(receipt.engine_epoch, "engine-epoch");
        assert_eq!(receipt.learning_generation, 6);
        assert_eq!(receipt.intervals.len(), 3);
        assert!(matches!(
            receipt.intervals[0].learning,
            IntervalLearning::Candidate { ref token, explicitly_selected: true } if token == "epoch:1"
        ));
        // Literal 区間は学習対象外。
        assert!(matches!(
            receipt.intervals[1].learning,
            IntervalLearning::None { reason: NoLearningReason::NotLearningTarget }
        ));
        assert_eq!(receipt.sentence_token, None, "混在全文の sentence token は出さない");
    }

    #[test]
    fn commit_receipt_rejects_a_text_that_differs_from_the_validated_result() {
        let projection = projection_fixture();
        let request = spans_from_projection(&projection);
        let identity = identity();
        let validated = validate_mixed_result(
            &identity, &request, 8, 13, 2, 5, 4, 9, 3, "engine-epoch", 6,
            "今日はPythonを使う", &result_spans(),
        )
        .unwrap();
        assert!(commit_receipt(
            ipc::clause::CommitId {
                client_instance: "tip".to_string(),
                sequence: 1,
            },
            &projection,
            &validated,
            "今日はPython", // 確定本文が検証済み結果と違う
        )
        .is_none());
    }
}
