//! Explicit range repair uses the same legal source boundaries as Projection.
use crate::{
    plan::{InterpretationPlan, SegmentKind},
    position::SourceRange,
    projection::Projection,
    source::CompositionSource,
};

pub fn reinterpret(
    source: &CompositionSource,
    plan: &InterpretationPlan,
    range: SourceRange,
    kind: SegmentKind,
) -> Option<InterpretationPlan> {
    if range.start >= range.end || range.end.get() > source.source_len() {
        return None;
    }
    let current = Projection::build(0, source, plan).ok()?;
    current.source_to_reading(range.start)?;
    current.source_to_reading(range.end)?;
    if kind == SegmentKind::Literal {
        source.original(range)?;
    }
    let chars: Vec<_> = source.source_text().chars().collect();
    let mut labels = vec![SegmentKind::Japanese; chars.len()];
    for span in &plan.spans {
        labels[span.range.start.get() as usize..span.range.end.get() as usize].fill(span.kind);
    }
    labels[range.start.get() as usize..range.end.get() as usize].fill(kind);
    let mut specs: Vec<(SegmentKind, String)> = vec![];
    for (ch, label) in chars.into_iter().zip(labels) {
        if specs.last().is_some_and(|(previous, _)| *previous == label) {
            specs.last_mut()?.1.push(ch);
        } else {
            specs.push((label, ch.to_string()));
        }
    }
    let repaired = InterpretationPlan::build(&source.source_text(), &specs).ok()?;
    Projection::build(0, source, &repaired).ok()?;
    Some(repaired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{Provenance, SourceElement, SourceStyle};
    fn source(text: &str) -> CompositionSource {
        CompositionSource::try_new(crate::classify::tune::kana_elements(text), 3).unwrap()
    }
    #[test]
    fn repair_preserves_other_ranges_and_can_reverse_literal() {
        let source = source("madekite");
        let original =
            InterpretationPlan::build("madekite", &[(SegmentKind::Japanese, "madekite".into())])
                .unwrap();
        let repaired = reinterpret(
            &source,
            &original,
            SourceRange::new(0, 4),
            SegmentKind::Literal,
        )
        .unwrap();
        assert_eq!(
            Projection::build(1, &source, &repaired).unwrap().reading(),
            "madeきて"
        );
        assert_eq!(
            reinterpret(
                &source,
                &repaired,
                SourceRange::new(0, 4),
                SegmentKind::Japanese
            )
            .unwrap(),
            original
        );
    }
    #[test]
    fn unknown_original_and_illegal_boundary_cannot_be_restored() {
        let source = CompositionSource::try_new(
            vec![
                SourceElement {
                    provenance: Provenance::ResolvedKana,
                    source_text: "き".into(),
                    reading: "き".into(),
                },
                SourceElement {
                    provenance: Provenance::Typed {
                        style: SourceStyle::Kana,
                    },
                    source_text: "te".into(),
                    reading: "て".into(),
                },
            ],
            1,
        )
        .unwrap();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        assert!(
            reinterpret(&source, &plan, SourceRange::new(0, 1), SegmentKind::Literal).is_none()
        );
        let source = source_for_kyo();
        let plan =
            InterpretationPlan::build("kyo", &[(SegmentKind::Japanese, "kyo".into())]).unwrap();
        assert!(
            reinterpret(&source, &plan, SourceRange::new(1, 2), SegmentKind::Literal).is_none()
        );
    }
    fn source_for_kyo() -> CompositionSource {
        source("kyo")
    }
}

/// The candidate UI retains the normal interpretation and appends at most two Mixed plans.
pub fn mixed_candidate_plans(
    results: &[crate::classify::ScoredPlan],
) -> impl Iterator<Item = &InterpretationPlan> {
    results
        .iter()
        .filter(|p| p.plan.spans.iter().any(|s| s.kind == SegmentKind::Literal))
        .take(2)
        .map(|p| &p.plan)
}
