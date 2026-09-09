//! Accepted operations survive engine waits; the first Commit separates compositions.
use crate::input_module::TextStyle;
use ipc::clause::{ClauseId, ReadingPosition};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OperationTarget {
    pub clause: Option<ClauseId>,
    pub start: ReadingPosition,
    pub end: ReadingPosition,
    pub candidate_window: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConversionAction {
    MoveClause(i32),
    MoveReading(i32),
    ReadingHome,
    ReadingEnd,
    ResizeClause(i32),
    Candidate(i32),
    Commit,
    Cancel,
    Insert {
        text: String,
        original: Option<String>,
        style: TextStyle,
        direct_commit: bool,
    },
    Backspace,
    Delete,
    Home(OperationTarget),
    End(OperationTarget),
    Transform {
        kind: crate::keymap::Notation,
        target: OperationTarget,
    },
    RotateNotation(OperationTarget),
}
impl ConversionAction {
    fn payload_units(&self) -> usize {
        match self {
            Self::Insert { text, original, .. } => text.encode_utf16().count() + original.as_ref().map_or(0, |s| s.encode_utf16().count()),
            _ => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueAdmission {
    Accepted,
    Full,
    TooLarge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CandidateWaitDisposition { Defer, Immediate, Commit, CommitThenInsert, Close, Noop }

pub(crate) fn candidate_wait_disposition(action: &ConversionAction, barrier: bool) -> CandidateWaitDisposition {
    use CandidateWaitDisposition as D;
    if barrier { return D::Defer; }
    match action {
        ConversionAction::Candidate(_) | ConversionAction::Home(_) | ConversionAction::End(_) => D::Defer,
        ConversionAction::Delete => D::Noop,
        ConversionAction::Commit => D::Commit,
        ConversionAction::Insert { .. } => D::CommitThenInsert,
        ConversionAction::Cancel => D::Close,
        _ => D::Immediate,
    }
}

#[derive(Default)]
pub(crate) struct ConversionQueue {
    actions: VecDeque<ConversionAction>,
    units: usize,
    deadline: Option<Instant>,
    pub waiting: bool,
    pub commit_failed: bool,
    pub initial_reading: Option<String>,
    pub owner_lost: bool,
    pub unwritten_commit: Option<String>,
    // An implicit mode-settle is admitted only to an empty queue, so this
    // intent belongs to its first Commit and must survive failed/deferred attempts.
    pub suppress_commit_prediction: bool,
}
impl ConversionQueue {
    /// Owner loss never grants permission to replay accepted input in another document.
    pub fn detach(&mut self, commit_body: Option<String>, body_applied: bool) {
        if self.owner_lost { return; }
        self.waiting = false;
        if !self.actions.iter().any(|action| matches!(action,
            ConversionAction::Commit | ConversionAction::Insert { .. }
                | ConversionAction::Backspace | ConversionAction::Delete)) {
            self.actions.clear();
            self.units = 0;
        }
        if body_applied && matches!(self.front(), Some(ConversionAction::Commit)) {
            self.complete_front();
        }
        self.unwritten_commit = if body_applied { None } else { commit_body };
        if self.actions.is_empty() && self.unwritten_commit.is_none() {
            self.clear();
        } else {
            self.owner_lost = true;
            self.waiting = false;
            self.deadline = None;
            self.commit_failed = true;
        }
    }
    /// Accept the barrier and its triggering text atomically.
    pub fn push_commit_then_insert(&mut self, text: String, style: TextStyle, direct_commit: bool) -> QueueAdmission {
        self.push_commit_then_mapped_insert(text, None, style, direct_commit)
    }

    pub fn push_commit_then_mapped_insert(&mut self, text: String, original: Option<String>, style: TextStyle, direct_commit: bool) -> QueueAdmission {
        if self.owner_lost { return QueueAdmission::Full; }
        let insert = ConversionAction::Insert { text, original, style, direct_commit };
        let units = insert.payload_units();
        if units > 4096 { return QueueAdmission::TooLarge; }
        if self.actions.len() + 2 > 64 || self.units + units > 4096 { return QueueAdmission::Full; }
        self.actions.push_back(ConversionAction::Commit);
        self.actions.push_back(insert);
        self.units += units;
        QueueAdmission::Accepted
    }
    pub fn begin(&mut self, now: Instant, initial_reading: Option<String>) {
        if self.owner_lost { return; }
        self.deadline
            .get_or_insert(now + Duration::from_millis(1200));
        if self.initial_reading.is_none() {
            self.initial_reading = initial_reading;
        }
        self.waiting = true;
    }
    pub fn push(&mut self, action: ConversionAction) -> QueueAdmission {
        if self.owner_lost { return QueueAdmission::Full; }
        let units = action.payload_units();
        if units > 4096 {
            return QueueAdmission::TooLarge;
        }
        if self.actions.len() >= 64 || self.units + units > 4096 {
            return QueueAdmission::Full;
        }
        self.units += units;
        self.actions.push_back(action);
        QueueAdmission::Accepted
    }
    /// Mixed editing can exhaust its counters on the very first queued edit.
    /// Reserve the slot for the Commit that must then precede that edit.
    pub fn push_reserving_commit(&mut self, action: ConversionAction) -> QueueAdmission {
        if self.actions.len() >= 63 { return QueueAdmission::Full; }
        self.push(action)
    }
    pub fn has_commit(&self) -> bool {
        self.actions
            .iter()
            .any(|a| matches!(a, ConversionAction::Commit))
    }
    pub fn front(&self) -> Option<&ConversionAction> {
        self.actions.front()
    }

    /// A preceding queued Space/F-key may have changed Editing to Converting.
    /// Give its following text the same real Commit barrier as immediate input.
    pub fn prepend_commit_for_insert(&mut self) -> bool {
        if self.actions.len() >= 64 || !matches!(self.actions.front(), Some(ConversionAction::Insert { .. })) { return false; }
        self.actions.push_front(ConversionAction::Commit);
        true
    }
    pub fn prepend_capacity_commit(&mut self) -> bool {
        if matches!(self.front(), Some(ConversionAction::Commit)) { return true; }
        if self.actions.len() < 64 {
            self.actions.push_front(ConversionAction::Commit);
            return true;
        }
        // A full queue of navigation operations can end at its first operation.
        // Text is never replaced; mixed text admission reserves a barrier slot.
        if let Some(front) = self.actions.front_mut() {
            if !matches!(front, ConversionAction::Insert { .. }) {
                *front = ConversionAction::Commit;
                return true;
            }
        }
        false
    }
    /// Counter exhaustion ends the current composition at this exact position.
    /// Preserve the suffix and use the ordinary receipt/deferred Commit path.
    pub fn commit_exhausted_action(&mut self) -> bool {
        if let Some(action @ (ConversionAction::ResizeClause(_) | ConversionAction::Backspace
            | ConversionAction::Delete | ConversionAction::Transform { .. } | ConversionAction::RotateNotation(_)
            | ConversionAction::Cancel | ConversionAction::Candidate(_))) = self.actions.front_mut() {
            *action = ConversionAction::Commit;
            return true;
        }
        false
    }
    /// Remove only after the action's effect has succeeded, including the TSF commit body.
    pub fn complete_front(&mut self) {
        if self.owner_lost { return; }
        if let Some(action) = self.actions.pop_front() {
            if matches!(action, ConversionAction::Commit) {
                self.initial_reading = None;
                self.suppress_commit_prediction = false;
            }
            self.units -= action.payload_units();
        }
        if self.actions.is_empty() && !self.waiting {
            self.deadline = None;
            self.initial_reading = None;
        }
    }
    pub fn consume_front_insert_prefix(&mut self, chars: usize) {
        if let Some(ConversionAction::Insert { text, original, .. }) = self.actions.front_mut() {
            let remaining: String = text.chars().skip(chars).collect();
            self.units -= text.encode_utf16().count() - remaining.encode_utf16().count();
            *text = remaining;
            if let Some(original) = original {
                let remaining: String = original.chars().skip(chars).collect();
                self.units -= original.encode_utf16().count() - remaining.encode_utf16().count();
                *original = remaining;
            }
        }
    }
    pub fn resolved(&mut self) {
        if self.owner_lost { return; }
        self.waiting = false;
        self.initial_reading = None;
        if self.actions.is_empty() {
            self.deadline = None;
        }
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
    pub fn expired(&self, now: Instant) -> bool {
        self.waiting && self.deadline.is_some_and(|deadline| now >= deadline)
    }
    pub fn logical_window_open(&self) -> bool {
        let mut open = false;
        for action in &self.actions {
            match action {
                ConversionAction::Candidate(_) => open = true,
                ConversionAction::MoveClause(_)
                | ConversionAction::ResizeClause(_)
                | ConversionAction::Transform { .. }
                | ConversionAction::RotateNotation(_)
                | ConversionAction::Cancel => open = false,
                ConversionAction::Commit => break,
                _ => {}
            }
        }
        open
    }
    /// A failed calculation cannot discard a commit or any input accepted after it.
    pub fn fail_wait(&mut self) {
        if self.owner_lost { return; }
        self.waiting = false;
        let barrier = self
            .actions
            .iter()
            .position(|a| matches!(a, ConversionAction::Commit));
        let retained = if let Some(barrier) = barrier {
            let mut retained: VecDeque<_> = self.actions.drain(..barrier).filter(|action| matches!(action,
                ConversionAction::Insert { .. } | ConversionAction::Backspace
                    | ConversionAction::Delete | ConversionAction::Cancel)).collect();
            retained.append(&mut self.actions);
            retained
        } else {
            self.actions
                .drain(..)
                .filter(|a| {
                    matches!(
                        a,
                        ConversionAction::Insert { .. }
                            | ConversionAction::Backspace
                            | ConversionAction::Delete
                            | ConversionAction::Cancel
                    )
                })
                .collect()
        };
        self.actions = retained;
        self.units = self
            .actions
            .iter()
            .map(ConversionAction::payload_units)
            .sum();
        if self.actions.is_empty() { self.deadline = None; self.initial_reading = None; }
    }
    /// A whole-reading correction supersedes navigation/calculation intent, but
    /// never an accepted document edit or a detached owner's work.
    pub fn supersede_calculation(&mut self) -> bool {
        if self.owner_lost || self.commit_failed || self.actions.iter().any(|action| matches!(action,
            ConversionAction::Commit | ConversionAction::Insert { .. }
            | ConversionAction::Backspace | ConversionAction::Delete | ConversionAction::Cancel)) {
            return false;
        }
        self.clear();
        true
    }

    pub fn cancel_before_commit(&mut self) -> bool {
        if self.owner_lost || self.has_commit() {
            return false;
        }
        self.clear();
        true
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn exhausted_resize_becomes_commit_before_accepted_suffix() {
        use super::*;
        let mut queue = ConversionQueue::default();
        queue.push(ConversionAction::ResizeClause(1));
        queue.push(ConversionAction::Insert { text: "あ".into(), original: None, style: TextStyle::Kana, direct_commit: false });
        assert!(queue.commit_exhausted_action());
        assert_eq!(queue.front(), Some(&ConversionAction::Commit));
        assert!(queue.has_commit());
        queue.complete_front();
        assert!(matches!(queue.front(), Some(ConversionAction::Insert { text, .. }) if text == "あ"));
        queue.complete_front();
        assert!(queue.front().is_none());
        assert_eq!(queue.units, 0);
    }

    use super::*;
    #[test]
    fn mapped_input_keeps_original_in_step_through_partial_application() {
        let mut queue = ConversionQueue::default();
        assert_eq!(queue.push_commit_then_mapped_insert("。、".into(), Some(".,".into()), TextStyle::Kana, false), QueueAdmission::Accepted);
        assert_eq!(queue.units, 4);
        queue.complete_front();
        queue.consume_front_insert_prefix(1);
        assert!(matches!(queue.front(), Some(ConversionAction::Insert { text, original: Some(original), .. }) if text == "、" && original == ","));
        assert_eq!(queue.units, 2);
        queue.consume_front_insert_prefix(1);
        queue.complete_front();
        assert_eq!(queue.units, 0);
    }

    #[test]
    fn queued_mode_change_adds_commit_without_losing_the_triggering_input() {
        let mut queue = ConversionQueue::default();
        queue.push(ConversionAction::Transform { kind: crate::keymap::Notation::Katakana,
            target: OperationTarget { clause: Some(ClauseId(1)), start: ReadingPosition(0), end: ReadingPosition(1), candidate_window: false } });
        for _ in 0..63 { assert_eq!(queue.push(insert("a")), QueueAdmission::Accepted); }
        queue.complete_front();
        assert!(queue.prepend_commit_for_insert());
        assert_eq!(queue.actions.len(), 64);
        assert_eq!(queue.front(), Some(&ConversionAction::Commit));
        queue.complete_front();
        assert_eq!(queue.actions.len(), 63);
        assert_eq!(queue.front(), Some(&insert("a")));
        assert_eq!(queue.units, 63);
    }

    fn insert(text: &str) -> ConversionAction {
        ConversionAction::Insert {
            text: text.into(),
            original: None,
            style: TextStyle::Kana,
            direct_commit: false,
        }
    }
    #[test]
    fn mixed_queue_keeps_emergency_commit_capacity() {
        let mut queue = ConversionQueue::default();
        for _ in 0..63 { assert_eq!(queue.push_reserving_commit(insert("a")), QueueAdmission::Accepted); }
        assert_eq!(queue.push_reserving_commit(insert("b")), QueueAdmission::Full);
        assert_eq!(queue.push_reserving_commit(ConversionAction::ReadingEnd), QueueAdmission::Full);
        assert!(queue.prepend_commit_for_insert());
        assert_eq!(queue.actions.len(), 64);
        queue.complete_front();
        for _ in 0..63 {
            assert_eq!(queue.front(), Some(&insert("a")));
            queue.complete_front();
        }
        assert!(queue.front().is_none());
    }
    #[test]
    fn implicit_commit_keeps_prediction_intent_through_wait_and_retry_only() {
        let mut queue = ConversionQueue::default();
        queue.push(ConversionAction::Commit);
        queue.suppress_commit_prediction = true;
        queue.push(insert("a"));
        queue.push(ConversionAction::Commit);
        queue.begin(Instant::now(), None);
        queue.fail_wait();
        queue.commit_failed = true;
        assert!(queue.suppress_commit_prediction);
        queue.commit_failed = false;
        queue.complete_front();
        assert!(!queue.suppress_commit_prediction);
        queue.complete_front();
        assert_eq!(queue.front(), Some(&ConversionAction::Commit));
        assert!(!queue.suppress_commit_prediction);
        queue.suppress_commit_prediction = true;
        queue.clear();
        assert!(!queue.suppress_commit_prediction);
    }
    #[test]
    fn correction_supersedes_only_calculation_actions() {
        let mut queue = ConversionQueue::default();
        queue.begin(Instant::now(), Some("にほんご".into()));
        queue.push(ConversionAction::Candidate(1));
        queue.push(ConversionAction::MoveClause(1));
        assert!(queue.supersede_calculation());
        assert!(!queue.waiting);
        assert!(queue.front().is_none());
        assert!(queue.initial_reading.is_none());
        for action in [ConversionAction::Commit, ConversionAction::Backspace,
            ConversionAction::Delete, ConversionAction::Cancel,
            ConversionAction::Insert { text: "残す".into(), original: None, style: TextStyle::Kana, direct_commit: false }] {
            let mut queue = ConversionQueue::default();
            queue.begin(Instant::now(), Some("に".into()));
            queue.push(ConversionAction::Candidate(1));
            queue.push(action.clone());
            assert!(!queue.supersede_calculation());
            assert!(queue.waiting);
            queue.complete_front();
            assert_eq!(queue.front(), Some(&action));
        }
    }

    #[test]
    fn commit_and_triggering_text_are_admitted_together() {
        let mut queue = ConversionQueue::default();
        for _ in 0..63 { queue.push(ConversionAction::MoveClause(1)); }
        assert_eq!(queue.push_commit_then_insert("a".into(), TextStyle::Kana, false), QueueAdmission::Full);
        assert!(!queue.has_commit());
        assert_eq!(queue.actions.len(), 63);
        queue.complete_front();
        assert_eq!(queue.push_commit_then_insert("A".into(), TextStyle::Direct, true), QueueAdmission::Accepted);
        assert!(matches!(queue.actions.back(), Some(ConversionAction::Insert { text, style: TextStyle::Direct, direct_commit: true, .. }) if text == "A"));
        assert_eq!(queue.actions.len(), 64);
        assert_eq!(queue.units, 1);
        queue.clear();
        assert_eq!(queue.push_commit_then_insert("a".repeat(4097), TextStyle::Kana, false), QueueAdmission::TooLarge);
        assert!(queue.front().is_none());
    }
    #[test]
    fn candidate_wait_preserves_commit_barrier_and_local_operations() {
        use CandidateWaitDisposition as D;
        assert_eq!(candidate_wait_disposition(&ConversionAction::Candidate(1), false), D::Defer);
        assert_eq!(candidate_wait_disposition(&ConversionAction::MoveClause(1), false), D::Immediate);
        assert_eq!(candidate_wait_disposition(&ConversionAction::Cancel, false), D::Close);
        assert_eq!(candidate_wait_disposition(&ConversionAction::Commit, false), D::Commit);
        assert_eq!(candidate_wait_disposition(&insert("a"), false), D::CommitThenInsert);
        for action in [ConversionAction::Cancel, ConversionAction::MoveClause(1), insert("a")] {
            assert_eq!(candidate_wait_disposition(&action, true), D::Defer);
        }
    }
    #[test]
    fn failed_candidate_wait_discards_advances_but_keeps_commit_and_later_input() {
        let mut queue = ConversionQueue::default();
        queue.begin(Instant::now(), None);
        queue.push(ConversionAction::Candidate(1)); queue.push(ConversionAction::Candidate(-1));
        queue.fail_wait();
        assert!(queue.front().is_none());
        assert!(queue.deadline().is_none());
        queue.begin(Instant::now(), None);
        queue.push(ConversionAction::Candidate(1)); queue.push(ConversionAction::Commit); queue.push(insert("a"));
        queue.fail_wait();
        assert_eq!(queue.front(), Some(&ConversionAction::Commit));
        queue.complete_front();
        assert_eq!(queue.front(), Some(&insert("a")));
    }
    #[test]
    fn commit_barrier_survives_cancel_and_timeout_with_later_input_in_order() {
        let mut queue = ConversionQueue::default();
        queue.begin(Instant::now(), Some("にほん".into()));
        for action in [
            ConversionAction::MoveClause(1),
            ConversionAction::Commit,
            insert("a"),
            ConversionAction::Cancel,
            insert("b"),
        ] {
            assert_eq!(queue.push(action), QueueAdmission::Accepted);
        }
        assert!(!queue.cancel_before_commit());
        queue.fail_wait();
        assert_eq!(queue.front(), Some(&ConversionAction::Commit));
        assert_eq!(queue.initial_reading.as_deref(), Some("にほん"));
        queue.complete_front();
        assert_eq!(queue.front(), Some(&insert("a")));
        queue.complete_front();
        assert_eq!(queue.front(), Some(&ConversionAction::Cancel));
        queue.complete_front();
        assert_eq!(queue.front(), Some(&insert("b")));
    }
    #[test]
    fn overflow_does_not_mutate_accepted_commit_or_text() {
        let mut queue = ConversionQueue::default();
        queue.push(ConversionAction::Commit);
        for _ in 0..63 {
            queue.push(insert("a"));
        }
        assert_eq!(queue.push(insert("lost")), QueueAdmission::Full);
        queue.fail_wait();
        assert_eq!(queue.actions.len(), 64);
        assert_eq!(queue.front(), Some(&ConversionAction::Commit));
        assert_eq!(queue.units, 63);
    }
    #[test]
    fn insert_capacity_counts_utf16_and_never_accepts_partial_payload() {
        let mut queue = ConversionQueue::default();
        assert_eq!(
            queue.push(insert(&"𠮷".repeat(2048))),
            QueueAdmission::Accepted
        );
        assert_eq!(queue.push(insert("a")), QueueAdmission::Full);
        assert_eq!(
            queue.push(insert(&"𠮷".repeat(2049))),
            QueueAdmission::TooLarge
        );
        assert_eq!(queue.actions.len(), 1);
    }
    #[test]
    fn transfer_to_candidate_wait_does_not_restart_deadline() {
        let mut queue = ConversionQueue::default();
        let now = Instant::now();
        queue.begin(now, Some("にほん".into()));
        queue.push(ConversionAction::Candidate(1));
        queue.push(ConversionAction::Commit);
        queue.resolved();
        queue.complete_front();
        queue.begin(now + Duration::from_millis(1100), None);
        assert!(queue.expired(now + Duration::from_millis(1200)));
    }
    #[test]
    fn logical_window_and_clamped_movement_are_not_aggregated() {
        let mut queue = ConversionQueue::default();
        queue.push(ConversionAction::MoveClause(-1));
        queue.push(ConversionAction::MoveClause(1));
        assert_eq!(queue.actions.len(), 2);
        assert!(!queue.logical_window_open());
        queue.push(ConversionAction::Candidate(1));
        assert!(queue.logical_window_open());
        queue.push(ConversionAction::MoveClause(1));
        assert!(!queue.logical_window_open());
    }

    #[test]
    fn notation_operations_close_the_logical_window_and_rotation_stays_ordered() {
        let target = OperationTarget { clause: None, start: ReadingPosition(0), end: ReadingPosition(4), candidate_window: true };
        let mut queue = ConversionQueue::default();
        queue.push(ConversionAction::Candidate(1));
        queue.push(ConversionAction::Transform { kind: crate::keymap::Notation::Katakana, target: target.clone() });
        assert!(!queue.logical_window_open());
        queue.push(ConversionAction::Candidate(1));
        let rotate = ConversionAction::RotateNotation(target);
        queue.push(rotate.clone());
        queue.push(rotate.clone());
        assert!(!queue.logical_window_open());
        assert_eq!(queue.actions.len(), 5);
        assert_eq!(candidate_wait_disposition(&rotate, false), CandidateWaitDisposition::Immediate);
        assert_eq!(candidate_wait_disposition(&rotate, true), CandidateWaitDisposition::Defer);
    }
    #[test]
    fn lost_owner_preserves_unwritten_body_and_input_until_explicit_cancel() {
        let mut queue = ConversionQueue::default();
        queue.begin(Instant::now(), Some("にほんご".into()));
        queue.push(ConversionAction::Commit);
        queue.push(insert("a"));
        queue.detach(Some("日本語".into()), false);
        assert!(queue.owner_lost && queue.commit_failed && !queue.waiting);
        assert_eq!(queue.unwritten_commit.as_deref(), Some("日本語"));
        assert_eq!(queue.actions, VecDeque::from([ConversionAction::Commit, insert("a")]));
        assert!(queue.deadline().is_none());
        assert_eq!(queue.push(insert("b")), QueueAdmission::Full);
        queue.detach(None, true);
        assert_eq!(queue.unwritten_commit.as_deref(), Some("日本語"));
        queue.fail_wait();
        queue.resolved();
        queue.complete_front();
        assert!(!queue.cancel_before_commit());
        assert_eq!(queue.actions, VecDeque::from([ConversionAction::Commit, insert("a")]));
        assert!(queue.owner_lost && queue.commit_failed);
        queue.clear();
        assert!(!queue.owner_lost);
        assert_eq!(queue.push(insert("c")), QueueAdmission::Accepted);
    }
    #[test]
    fn lost_owner_consumes_applied_commit_once_but_never_its_following_input() {
        let mut queue = ConversionQueue::default();
        queue.push(ConversionAction::Commit);
        queue.push(insert("a"));
        queue.push(ConversionAction::Commit);
        queue.detach(Some("日本語".into()), true);
        queue.detach(None, true);
        assert!(queue.owner_lost);
        assert!(queue.unwritten_commit.is_none());
        assert_eq!(queue.actions, VecDeque::from([insert("a"), ConversionAction::Commit]));
    }
    #[test]
    fn lost_owner_keeps_input_before_a_later_commit_and_partial_insert_suffix() {
        let mut queue = ConversionQueue::default();
        queue.push(insert("abc"));
        queue.push(ConversionAction::Commit);
        queue.consume_front_insert_prefix(1);
        queue.detach(None, false);
        assert_eq!(queue.actions, VecDeque::from([insert("bc"), ConversionAction::Commit]));
        assert_eq!(queue.units, 2);
        assert!(queue.owner_lost);
    }
    #[test]
    fn lost_owner_releases_empty_candidate_wait_without_blocking_new_input() {
        let mut queue = ConversionQueue::default();
        queue.begin(Instant::now(), None);
        queue.push(ConversionAction::Candidate(1));
        queue.detach(None, false);
        assert!(!queue.owner_lost && !queue.waiting && !queue.commit_failed);
        assert_eq!(queue.push(insert("a")), QueueAdmission::Accepted);
    }

    #[test]
    fn second_commit_after_timeout_owns_only_the_new_composition() {
        let mut queue = ConversionQueue::default();
        queue.begin(Instant::now(), Some("にほん".into()));
        queue.push(ConversionAction::Commit); queue.push(insert("a")); queue.push(ConversionAction::Commit);
        queue.fail_wait();
        assert_eq!(queue.initial_reading.as_deref(), Some("にほん"));
        queue.complete_front();
        assert_eq!(queue.initial_reading, None);
        queue.complete_front();
        assert_eq!(queue.front(), Some(&ConversionAction::Commit));
        assert_eq!(queue.initial_reading, None);
    }

    #[test]
    fn failed_insert_keeps_only_unapplied_suffix_and_its_capacity() {
        let mut queue = ConversionQueue::default();
        queue.push(insert("a𠮷b")); queue.push(ConversionAction::Commit);
        queue.consume_front_insert_prefix(1);
        assert_eq!(queue.front(), Some(&insert("𠮷b")));
        assert_eq!(queue.units, 3);
        queue.consume_front_insert_prefix(0);
        assert_eq!(queue.front(), Some(&insert("𠮷b")));
        queue.consume_front_insert_prefix(2); queue.complete_front();
        assert_eq!(queue.front(), Some(&ConversionAction::Commit));
        assert_eq!(queue.units, 0);
    }

    #[test]
    fn full_queue_failure_keeps_input_before_a_later_commit() {
        let mut queue = ConversionQueue::default();
        queue.push(insert("a𠮷b"));
        queue.consume_front_insert_prefix(1);
        queue.push(ConversionAction::Commit);
        queue.push(insert("c"));
        for _ in 0..61 { queue.push(ConversionAction::Candidate(1)); }
        queue.commit_failed = true;
        assert_eq!(queue.push(insert("rejected")), QueueAdmission::Full);
        queue.fail_wait();
        assert_eq!(queue.front(), Some(&insert("𠮷b")));
        assert_eq!(queue.actions.get(1), Some(&ConversionAction::Commit));
        assert_eq!(queue.actions.get(2), Some(&insert("c")));
        assert_eq!(queue.units, 4);
        assert!(queue.commit_failed);
    }
}
