//! インライン予測のCOM非依存状態機械。

const DEBOUNCE_MS: u64 = 300;
const DEADLINE_MS: u64 = 400;
const MIN_CONTEXT_CHARS: usize = 8;
const MAX_CONTEXT_CHARS: usize = 256;
const MAX_PREDICTION_CHARS: usize = 16;
const MIN_PREDICTION_CHARS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Timestamp(u64);

impl Timestamp {
    pub(crate) const fn from_millis(value: u64) -> Self {
        Self(value)
    }

    const fn add_millis(self, value: u64) -> Self {
        Self(self.0.saturating_add(value))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PredictionAnchor(u64);

impl PredictionAnchor {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitSource {
    Enter,
    Candidate,
    Clause,
    AcceptedPrediction,
    LiveAuto,
    Partial,
    ImplicitSettle,
}

impl CommitSource {
    pub(crate) const fn starts_prediction(self) -> bool {
        matches!(
            self,
            Self::Enter | Self::Candidate | Self::Clause | Self::AcceptedPrediction
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PredictionRequest {
    pub(crate) seq: u64,
    pub(crate) context_before: String,
    pub(crate) anchor: PredictionAnchor,
    pub(crate) deadline_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PredictionGhost {
    pub(crate) seq: u64,
    pub(crate) text: String,
    pub(crate) anchor: PredictionAnchor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Invalidation {
    Input,
    CaretMoved,
    SelectionChanged,
    FocusChanged,
    ModeChanged,
    Disabled,
}

impl Invalidation {
    const fn clears_context(self) -> bool {
        !matches!(self, Self::Input)
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingRequest {
    seq: u64,
    anchor: PredictionAnchor,
    deadline_at: Timestamp,
}

#[derive(Debug, Default)]
pub(crate) struct PredictionState {
    context_before: String,
    next_seq: u64,
    debounce: Option<(Timestamp, PredictionAnchor)>,
    pending: Option<PendingRequest>,
    ghost: Option<PredictionGhost>,
    suppressed_context: Option<String>,
}

impl PredictionState {
    pub(crate) fn on_commit(
        &mut self,
        source: CommitSource,
        text: &str,
        anchor: PredictionAnchor,
        now: Timestamp,
    ) {
        if !source.starts_prediction() {
            self.invalidate(Invalidation::CaretMoved);
            return;
        }
        if self.debounce.is_some() || self.pending.is_some() || self.ghost.is_some() {
            self.next_seq = self.next_seq.saturating_add(1);
            self.debounce = None;
            self.pending = None;
            self.ghost = None;
        }
        self.suppressed_context = None;
        self.context_before.push_str(text);
        let context_chars = self.context_before.chars().count();
        if context_chars > MAX_CONTEXT_CHARS {
            let keep_from = self
                .context_before
                .char_indices()
                .nth(context_chars - MAX_CONTEXT_CHARS)
                .map(|(index, _)| index)
                .unwrap_or(0);
            self.context_before.drain(..keep_from);
        }
        if self.context_before.chars().count() >= MIN_CONTEXT_CHARS {
            self.debounce = Some((now.add_millis(DEBOUNCE_MS), anchor));
        }
    }

    pub(crate) fn poll(&mut self, now: Timestamp) -> Option<PredictionRequest> {
        if self.is_current_context_suppressed() {
            self.debounce = None;
            return None;
        }
        let (due_at, anchor) = self.debounce?;
        if now < due_at {
            return None;
        }
        self.debounce = None;
        self.next_seq = self.next_seq.saturating_add(1);
        let request = PredictionRequest {
            seq: self.next_seq,
            context_before: self.context_before.clone(),
            anchor,
            deadline_at: now.add_millis(DEADLINE_MS),
        };
        self.pending = Some(PendingRequest {
            seq: request.seq,
            anchor: request.anchor,
            deadline_at: request.deadline_at,
        });
        Some(request)
    }

    pub(crate) fn invalidate(&mut self, reason: Invalidation) {
        self.next_seq = self.next_seq.saturating_add(1);
        self.debounce = None;
        self.pending = None;
        self.ghost = None;
        if reason.clears_context() {
            self.context_before.clear();
            self.suppressed_context = None;
        }
    }

    pub(crate) fn on_result(
        &mut self,
        seq: u64,
        text: &str,
        now: Timestamp,
    ) -> Option<PredictionGhost> {
        let pending = self.pending?;
        if seq != pending.seq {
            return None;
        }
        self.pending = None;
        if now > pending.deadline_at {
            return None;
        }
        let text: String = text.chars().take(MAX_PREDICTION_CHARS).collect();
        if text.chars().count() < MIN_PREDICTION_CHARS {
            return None;
        }
        let ghost = PredictionGhost {
            seq,
            text,
            anchor: pending.anchor,
        };
        self.ghost = Some(ghost.clone());
        Some(ghost)
    }

    /// 結果が届かない要求を期限で閉じる。遅着結果は pending 不在として破棄される。
    pub(crate) fn expire_pending(&mut self, now: Timestamp) -> bool {
        let Some(pending) = self.pending else {
            return false;
        };
        if now <= pending.deadline_at {
            return false;
        }
        self.pending = None;
        true
    }

    pub(crate) fn ghost(&self) -> Option<&PredictionGhost> {
        self.ghost.as_ref()
    }

    pub(crate) fn has_activity(&self) -> bool {
        self.debounce.is_some() || self.pending.is_some() || self.ghost.is_some()
    }

    /// 文書／入力欄境界で必ず消去すべき私有状態。候補が無くても
    /// 確定文脈と同一文脈抑止は次の予測に影響するため、境界を越えない。
    pub(crate) fn has_private_state(&self) -> bool {
        self.has_activity() || !self.context_before.is_empty() || self.suppressed_context.is_some()
    }

    pub(crate) fn accept_ghost(
        &mut self,
        anchor: PredictionAnchor,
        now: Timestamp,
    ) -> Option<String> {
        let text = self.ghost.take()?.text;
        self.on_commit(CommitSource::AcceptedPrediction, &text, anchor, now);
        Some(text)
    }

    pub(crate) fn dismiss_ghost(&mut self) -> bool {
        if self.ghost.take().is_none() {
            return false;
        }
        self.suppressed_context = Some(self.context_before.clone());
        true
    }

    pub(crate) fn is_current_context_suppressed(&self) -> bool {
        self.suppressed_context.as_ref() == Some(&self.context_before)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_full_commit_requests_after_300ms_debounce() {
        let mut state = PredictionState::default();
        let anchor = PredictionAnchor::new(7);

        state.on_commit(
            CommitSource::Enter,
            "今日はとても晴れです",
            anchor,
            Timestamp::from_millis(100),
        );

        assert_eq!(state.poll(Timestamp::from_millis(399)), None);
        assert_eq!(
            state.poll(Timestamp::from_millis(400)),
            Some(PredictionRequest {
                seq: 1,
                context_before: "今日はとても晴れです".into(),
                anchor,
                deadline_at: Timestamp::from_millis(800),
            })
        );
    }

    #[test]
    fn only_user_initiated_full_commits_start_prediction() {
        assert!(CommitSource::Enter.starts_prediction());
        assert!(CommitSource::Candidate.starts_prediction());
        assert!(CommitSource::Clause.starts_prediction());
        assert!(CommitSource::AcceptedPrediction.starts_prediction());
        assert!(!CommitSource::LiveAuto.starts_prediction());
        assert!(!CommitSource::Partial.starts_prediction());
        assert!(!CommitSource::ImplicitSettle.starts_prediction());
    }

    #[test]
    fn context_shorter_than_eight_characters_does_not_request() {
        let mut state = PredictionState::default();
        state.on_commit(
            CommitSource::Enter,
            "短い文章です",
            PredictionAnchor::new(1),
            Timestamp::from_millis(0),
        );

        assert_eq!(state.poll(Timestamp::from_millis(300)), None);
    }

    #[test]
    fn request_keeps_only_the_last_256_unicode_characters() {
        let mut state = PredictionState::default();
        let committed = format!("{}{}", "前".repeat(10), "後".repeat(256));
        state.on_commit(
            CommitSource::Enter,
            &committed,
            PredictionAnchor::new(1),
            Timestamp::from_millis(0),
        );

        let request = state.poll(Timestamp::from_millis(300)).unwrap();
        assert_eq!(request.context_before, "後".repeat(256));
    }

    #[test]
    fn typing_after_request_makes_the_result_stale() {
        let mut state = PredictionState::default();
        state.on_commit(
            CommitSource::Enter,
            "十分に長い確定済み文脈です",
            PredictionAnchor::new(2),
            Timestamp::from_millis(0),
        );
        let request = state.poll(Timestamp::from_millis(300)).unwrap();

        state.invalidate(Invalidation::Input);

        assert_eq!(
            state.on_result(
                request.seq,
                "続きを入力します。",
                Timestamp::from_millis(350)
            ),
            None
        );
        assert_eq!(state.ghost(), None);
    }

    #[test]
    fn fresh_result_is_anchored_and_limited_to_16_characters() {
        let mut state = PredictionState::default();
        let anchor = PredictionAnchor::new(9);
        state.on_commit(
            CommitSource::Enter,
            "十分に長い確定済み文脈です",
            anchor,
            Timestamp::from_millis(0),
        );
        let request = state.poll(Timestamp::from_millis(300)).unwrap();

        let ghost = state
            .on_result(
                request.seq,
                "1234567890123456後続",
                Timestamp::from_millis(700),
            )
            .unwrap();

        assert_eq!(ghost.text, "1234567890123456");
        assert_eq!(ghost.anchor, anchor);
        assert_eq!(state.ghost(), Some(&ghost));
    }

    #[test]
    fn every_editor_transition_clears_a_visible_ghost() {
        for reason in [
            Invalidation::Input,
            Invalidation::CaretMoved,
            Invalidation::SelectionChanged,
            Invalidation::FocusChanged,
            Invalidation::ModeChanged,
            Invalidation::Disabled,
        ] {
            let mut state = PredictionState::default();
            state.on_commit(
                CommitSource::Enter,
                "十分に長い確定済み文脈です",
                PredictionAnchor::new(3),
                Timestamp::from_millis(0),
            );
            let request = state.poll(Timestamp::from_millis(300)).unwrap();
            state
                .on_result(
                    request.seq,
                    "表示する候補です。",
                    Timestamp::from_millis(350),
                )
                .unwrap();

            state.invalidate(reason);

            assert_eq!(state.ghost(), None, "reason={reason:?}");
        }
    }

    #[test]
    fn accepting_a_ghost_appends_it_and_rearms_prediction() {
        let mut state = PredictionState::default();
        let anchor = PredictionAnchor::new(4);
        state.on_commit(
            CommitSource::Enter,
            "十分に長い確定済み文脈です",
            anchor,
            Timestamp::from_millis(0),
        );
        let first = state.poll(Timestamp::from_millis(300)).unwrap();
        state
            .on_result(first.seq, "確認します。", Timestamp::from_millis(350))
            .unwrap();

        let accepted = state.accept_ghost(anchor, Timestamp::from_millis(400));

        assert_eq!(accepted.as_deref(), Some("確認します。"));
        assert_eq!(state.ghost(), None);
        assert_eq!(state.poll(Timestamp::from_millis(699)), None);
        let second = state.poll(Timestamp::from_millis(700)).unwrap();
        assert_eq!(second.seq, 2);
        assert!(second.context_before.ends_with("確認します。"));
    }

    #[test]
    fn location_or_mode_changes_clear_the_private_context_ring() {
        for reason in [
            Invalidation::CaretMoved,
            Invalidation::SelectionChanged,
            Invalidation::FocusChanged,
            Invalidation::ModeChanged,
            Invalidation::Disabled,
        ] {
            let mut state = PredictionState::default();
            state.on_commit(
                CommitSource::Enter,
                "十分に長い確定済み文脈です",
                PredictionAnchor::new(1),
                Timestamp::from_millis(0),
            );
            state.invalidate(reason);
            state.on_commit(
                CommitSource::Enter,
                "短い文章です",
                PredictionAnchor::new(2),
                Timestamp::from_millis(100),
            );

            assert_eq!(
                state.poll(Timestamp::from_millis(400)),
                None,
                "reason={reason:?}"
            );
        }
    }

    #[test]
    fn a_new_explicit_commit_invalidates_the_previous_request() {
        let mut state = PredictionState::default();
        let anchor = PredictionAnchor::new(1);
        state.on_commit(
            CommitSource::Enter,
            "最初に確定した十分長い文章です",
            anchor,
            Timestamp::from_millis(0),
        );
        let old = state.poll(Timestamp::from_millis(300)).unwrap();

        state.on_commit(
            CommitSource::Enter,
            "次に確定した文章です",
            PredictionAnchor::new(2),
            Timestamp::from_millis(350),
        );

        assert_eq!(
            state.on_result(old.seq, "古い候補です。", Timestamp::from_millis(360)),
            None
        );
        assert_eq!(state.ghost(), None);
    }

    #[test]
    fn automatic_or_partial_commits_only_invalidate_and_never_rearm() {
        for source in [
            CommitSource::LiveAuto,
            CommitSource::Partial,
            CommitSource::ImplicitSettle,
        ] {
            let mut state = PredictionState::default();
            state.on_commit(
                CommitSource::Enter,
                "最初に確定した十分長い文章です",
                PredictionAnchor::new(1),
                Timestamp::from_millis(0),
            );
            let old = state.poll(Timestamp::from_millis(300)).unwrap();

            state.on_commit(
                source,
                "自動的に確定された文章です",
                PredictionAnchor::new(2),
                Timestamp::from_millis(350),
            );

            assert_eq!(
                state.on_result(old.seq, "古い候補です。", Timestamp::from_millis(360)),
                None,
                "source={source:?}"
            );
            assert_eq!(state.poll(Timestamp::from_millis(650)), None);
        }
    }

    #[test]
    fn result_shorter_than_two_characters_is_not_shown() {
        let mut state = PredictionState::default();
        state.on_commit(
            CommitSource::Enter,
            "十分に長い確定済み文脈です",
            PredictionAnchor::new(1),
            Timestamp::from_millis(0),
        );
        let request = state.poll(Timestamp::from_millis(300)).unwrap();

        assert_eq!(
            state.on_result(request.seq, "。", Timestamp::from_millis(350)),
            None
        );
        assert_eq!(state.ghost(), None);
    }

    #[test]
    fn result_after_the_400ms_deadline_is_not_shown() {
        let mut state = PredictionState::default();
        state.on_commit(
            CommitSource::Enter,
            "十分に長い確定済み文脈です",
            PredictionAnchor::new(1),
            Timestamp::from_millis(0),
        );
        let request = state.poll(Timestamp::from_millis(300)).unwrap();

        assert_eq!(
            state.on_result(
                request.seq,
                "期限切れ候補です。",
                Timestamp::from_millis(701)
            ),
            None
        );
        assert_eq!(state.ghost(), None);
    }

    #[test]
    fn dismiss_suppresses_only_the_same_context_until_it_changes() {
        let mut state = PredictionState::default();
        state.on_commit(
            CommitSource::Enter,
            "十分に長い確定済み文脈です",
            PredictionAnchor::new(1),
            Timestamp::from_millis(0),
        );
        let request = state.poll(Timestamp::from_millis(300)).unwrap();
        state
            .on_result(
                request.seq,
                "表示中の候補です。",
                Timestamp::from_millis(350),
            )
            .unwrap();

        assert!(state.dismiss_ghost());
        assert!(state.is_current_context_suppressed());
        assert_eq!(state.ghost(), None);

        state.on_commit(
            CommitSource::Enter,
            "新しく確定した文章です",
            PredictionAnchor::new(2),
            Timestamp::from_millis(400),
        );
        assert!(!state.is_current_context_suppressed());
        assert!(state.poll(Timestamp::from_millis(700)).is_some());
    }

    #[test]
    fn activity_covers_debounce_pending_and_visible_ghost() {
        let mut state = PredictionState::default();
        assert!(!state.has_activity());
        state.on_commit(
            CommitSource::Enter,
            "十分に長い確定済み文脈です",
            PredictionAnchor::new(1),
            Timestamp::from_millis(0),
        );
        assert!(state.has_activity());
        let request = state.poll(Timestamp::from_millis(300)).unwrap();
        assert!(state.has_activity());
        state
            .on_result(
                request.seq,
                "表示する候補です。",
                Timestamp::from_millis(350),
            )
            .unwrap();
        assert!(state.has_activity());
        state.invalidate(Invalidation::SelectionChanged);
        assert!(!state.has_activity());
    }

    #[test]
    fn expired_matching_result_clears_pending_activity() {
        let mut state = PredictionState::default();
        state.on_commit(
            CommitSource::Enter,
            "十分に長い確定済み文脈です",
            PredictionAnchor::new(1),
            Timestamp::from_millis(0),
        );
        let request = state.poll(Timestamp::from_millis(300)).unwrap();
        assert!(state
            .on_result(
                request.seq,
                "期限切れの候補です。",
                Timestamp::from_millis(701)
            )
            .is_none());
        assert!(!state.has_activity());
        assert!(state.has_private_state());
        state.invalidate(Invalidation::FocusChanged);
        assert!(!state.has_private_state());
    }

    #[test]
    fn missing_result_expires_and_late_reply_stays_stale() {
        let mut state = PredictionState::default();
        state.on_commit(
            CommitSource::Enter,
            "明日の予定を確認します",
            PredictionAnchor::new(1),
            Timestamp::from_millis(0),
        );
        let request = state.poll(Timestamp::from_millis(300)).unwrap();
        assert!(!state.expire_pending(Timestamp::from_millis(700)));
        assert!(state.expire_pending(Timestamp::from_millis(701)));
        assert_eq!(
            state.on_result(request.seq, "遅い応答です", Timestamp::from_millis(702)),
            None
        );
    }
}
