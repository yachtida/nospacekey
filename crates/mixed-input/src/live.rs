//! Conservative live interpretation and commit boundaries. Scores are margins, not probabilities.
use crate::{
    classify::ScoredPlan,
    plan::{InterpretationPlan, SegmentKind},
    position::{SourcePosition, SourceRange},
    projection::Projection,
    source::CompositionSource,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Unresolved,
    Provisional,
    UserLocked,
}
#[derive(Clone, Debug, Default)]
pub struct LiveState {
    pub status: Status,
    pub accepted: Option<InterpretationPlan>,
}
impl LiveState {
    pub fn lock(&mut self) {
        self.status = Status::UserLocked;
    }
    pub fn accept(&mut self, plan: InterpretationPlan) {
        self.accepted = Some(plan);
        self.status = Status::Provisional;
    }
    /// The state changes only after the caller has obtained a body ACK.
    pub fn proposal<'a>(
        &self,
        results: &'a [ScoredPlan],
        adopt: f64,
        change: f64,
    ) -> Option<&'a InterpretationPlan> {
        if self.status == Status::UserLocked
            || !adopt.is_finite()
            || !change.is_finite()
            || adopt < 0.0
            || change < adopt
        {
            return None;
        }
        let best = results.first()?;
        let margin = best.margin?;
        if margin.is_nan() || best.retained {
            return None;
        }
        let changes_prefix = self.accepted.as_ref().is_some_and(|old| {
            let old_text: String = old.spans.iter().map(|s| s.text.as_str()).collect();
            let new_text: String = best.plan.spans.iter().map(|s| s.text.as_str()).collect();
            !new_text.starts_with(&old_text)
                || labels(old)
                    .iter()
                    .zip(labels(&best.plan))
                    .any(|(a, b)| *a != b)
        });
        (margin >= if changes_prefix { change } else { adopt }).then_some(&best.plan)
    }
}
fn labels(plan: &InterpretationPlan) -> Vec<SegmentKind> {
    plan.spans
        .iter()
        .flat_map(|s| std::iter::repeat(s.kind).take(s.range.len() as usize))
        .collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitFence {
    pub source_end: u32,
    pub reading_end: u32,
}
impl CommitFence {
    pub fn permits(self, reading_end: u32) -> bool {
        reading_end > 0 && reading_end <= self.reading_end
    }
}
/// The unfinished final span is excluded even when every current alternative agrees.
/// Positions inside a source unit or Literal span are never exposed as commit boundaries.
pub fn commit_fence(
    source: &CompositionSource,
    projection: &Projection,
    alternatives: &[InterpretationPlan],
    pending: bool,
    editing: Option<SourceRange>,
) -> CommitFence {
    if pending || alternatives.is_empty() || projection.source_revision != source.revision() {
        return CommitFence::default();
    }
    let text = source.source_text();
    let Some(first) = alternatives.first() else {
        return CommitFence::default();
    };
    if alternatives
        .iter()
        .any(|p| Projection::build(0, source, p).is_err())
    {
        return CommitFence::default();
    }
    let expected: Vec<_> = projection
        .spans
        .iter()
        .flat_map(|s| std::iter::repeat(s.kind).take(s.source.len() as usize))
        .collect();
    if expected != labels(first)
        || projection
            .spans
            .iter()
            .map(|s| s.source_text.as_str())
            .collect::<String>()
            != text
    {
        return CommitFence::default();
    }
    let mut limit = first.spans.last().map_or(0, |s| s.range.start.get());
    for alternative in alternatives.iter().skip(1) {
        if let Some(position) = expected
            .iter()
            .zip(labels(alternative))
            .position(|(a, b)| *a != b)
        {
            limit = limit.min(position as u32);
        }
    }
    if let Some(range) = editing {
        limit = limit.min(range.start.get());
    }
    let end = source
        .layout()
        .iter()
        .map(|e| e.source.end.get())
        .filter(|&end| end <= limit)
        .filter(|&end| {
            !projection.spans.iter().any(|s| {
                s.kind == SegmentKind::Literal
                    && s.source.start.get() < end
                    && end < s.source.end.get()
            })
        })
        .filter_map(|end| {
            projection
                .source_to_reading(SourcePosition::new(end))
                .map(|r| CommitFence {
                    source_end: end,
                    reading_end: r.get(),
                })
        })
        .last();
    end.unwrap_or_default()
}
/// Prepare the suffix without consuming source. The caller installs it only after commit ACK.
pub fn source_after_prefix(
    source: &CompositionSource,
    projection: &Projection,
    fence: CommitFence,
    reading_end: u32,
    revision: u64,
) -> Option<CompositionSource> {
    if !fence.permits(reading_end) || source.revision() != projection.source_revision {
        return None;
    }
    let cut = projection
        .reading_to_source(crate::position::ReadingPosition::new(reading_end))?
        .get();
    if cut > fence.source_end
        || projection.spans.iter().any(|s| {
            s.kind == SegmentKind::Literal && s.source.start.get() < cut && cut < s.source.end.get()
        })
    {
        return None;
    }
    let layout = source.layout();
    let count = layout.iter().position(|e| e.source.end.get() == cut)? + 1;
    CompositionSource::try_new(source.elements()[count..].to_vec(), revision).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::tune::source_from_str;
    fn plan(text: &str, parts: &[(SegmentKind, &str)]) -> InterpretationPlan {
        InterpretationPlan::build(
            text,
            &parts
                .iter()
                .map(|(k, t)| (*k, t.to_string()))
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }
    fn result(plan: InterpretationPlan, margin: f64) -> Vec<ScoredPlan> {
        vec![ScoredPlan {
            plan,
            score: margin,
            margin: Some(margin),
            retained: false,
        }]
    }
    #[test]
    fn hysteresis_and_user_lock_override_repeated_high_scores() {
        let ja = plan("made", &[(SegmentKind::Japanese, "made")]);
        let literal = plan("made", &[(SegmentKind::Literal, "made")]);
        let mut state = LiveState::default();
        assert!(state
            .proposal(&result(literal.clone(), 1.9), 2., 4.)
            .is_none());
        assert!(state
            .proposal(&result(literal.clone(), 2.), 2., 4.)
            .is_some());
        state.accept(literal.clone());
        assert!(state.proposal(&result(ja.clone(), 3.9), 2., 4.).is_none());
        assert!(state.proposal(&result(ja.clone(), 4.), 2., 4.).is_some());
        state.lock();
        assert!(state.proposal(&result(ja, f64::INFINITY), 2., 4.).is_none());
        assert_eq!(state.accepted, Some(literal));
    }
    #[test]
    fn fence_excludes_disagreement_tail_and_edited_literal() {
        let source = source_from_str("kyouhaRustnotukaikata");
        let chosen = plan(
            &source.source_text(),
            &[
                (SegmentKind::Japanese, "kyouha"),
                (SegmentKind::Literal, "Rust"),
                (SegmentKind::Japanese, "notukaikata"),
            ],
        );
        let projection = Projection::build(1, &source, &chosen).unwrap();
        let safe = commit_fence(&source, &projection, &[chosen.clone()], false, None);
        assert_eq!((safe.source_end, safe.reading_end), (10, 8));
        let suffix = source_after_prefix(&source, &projection, safe, 8, 77).unwrap();
        assert_eq!(suffix.source_text(), "notukaikata");
        assert_eq!(suffix.revision(), 77);
        assert_eq!(source.source_text(), "kyouhaRustnotukaikata");
        assert!(source_after_prefix(&source, &projection, safe, 7, 77).is_none());
        let edited = commit_fence(
            &source,
            &projection,
            &[chosen.clone()],
            false,
            Some(SourceRange::new(8, 9)),
        );
        assert_eq!(edited.source_end, 6);
        assert_eq!(
            commit_fence(&source, &projection, &[chosen.clone()], true, None),
            CommitFence::default()
        );
        let ja = plan(
            &source.source_text(),
            &[(SegmentKind::Japanese, "kyouhaRustnotukaikata")],
        );
        assert_eq!(
            commit_fence(&source, &projection, &[chosen, ja], false, None).source_end,
            6
        );
    }
    #[test]
    fn fence_never_splits_a_recomposed_source_unit() {
        let source = source_from_str("pythonno");
        let chosen = plan(
            "pythonno",
            &[
                (SegmentKind::Literal, "python"),
                (SegmentKind::Japanese, "no"),
            ],
        );
        let projection = Projection::build(1, &source, &chosen).unwrap();
        // nn straddles the interpretation boundary: whole source-unit safety wins.
        assert_eq!(
            commit_fence(&source, &projection, &[chosen], false, None),
            CommitFence::default()
        );
    }
}
