//! COM と IPC から独立した入力イベント境界。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotIdentity {
    /// Local issuance identity; navigation can change replay without changing reading.
    pub request: u64,
    pub composition: u64,
    pub revision: u64,
    pub configuration_generation: u64,
    pub connection_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CandidateResultIdentity {
    pub composition: u64,
    pub revision: u64,
    pub result: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoCommitReceipt {
    pub proposal: u64,
    pub identity: SnapshotIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoCommitProposal {
    pub proposal: u64,
    pub identity: SnapshotIdentity,
    pub text: String,
    pub consumed_reading: String,
    pub remaining: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompositionSnapshot {
    pub identity: SnapshotIdentity,
    pub purpose: SnapshotPurpose,
    pub segments: Vec<InputSegment>,
    pub left_context: Option<String>,
    pub live_search_width: u32,
    pub mixed_source: Option<mixed_input::source::CompositionSource>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotPurpose {
    Live,
    Explicit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    Text {
        ch: char,
        style: TextStyle,
        replay: ReplayMode,
        /// 正規化前の打鍵原文字。CapsLock 大文字をかな読み用の小文字へ正規化して
        /// 渡すときなどに ch と違い、journal の unit original（元入力）になる。
        /// None は ch がそのまま原文字。
        original: Option<char>,
    },
    Backspace,
    Delete,
    MoveReading(i32),
    ReadingHome,
    ReadingEnd,
    Space,
    MoveCandidate(i32),
    SelectCandidate(usize),
    CommitCandidate(Option<usize>),
    Enter,
    Escape,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayMode {
    Delta,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextStyle {
    Kana,
    Direct,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleEvent {
    Activated,
    Deactivated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineResult {
    Reading {
        request: RequestId,
        text: String,
    },
    Candidates {
        request: RequestId,
        values: Vec<String>,
    },
    Commit {
        request: RequestId,
        candidate: Option<usize>,
        resolved_text: String,
        outcome: EngineCommitOutcome,
    },
    Disconnected {
        request: RequestId,
    },
    LiveSnapshot {
        identity: SnapshotIdentity,
        text: String,
    },
    LiveAutoCommitProposal(AutoCommitProposal),
    ExplicitSnapshot {
        identity: SnapshotIdentity,
        candidates: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineCommitOutcome {
    Applied { text: String, remaining: String },
    Fallback { text: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Key(KeyEvent),
    Lifecycle(LifecycleEvent),
    Engine(EngineResult),
    Candidates(CandidateEvent),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CandidateEvent {
    Replace {
        values: Vec<String>,
        selected: usize,
        reason: CandidateReplacement,
    },
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateReplacement {
    NewResult,
    UserDriven,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImmediateOperation {
    SetPreedit {
        text: String,
    },
    ShowCandidates {
        identity: CandidateResultIdentity,
        values: Vec<String>,
        selected: usize,
    },
    Commit {
        text: String,
        candidate: Option<usize>,
        remaining: Option<String>,
        remaining_latin_from: Option<usize>,
    },
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackgroundIntent {
    Insert {
        request: RequestId,
        segments: Vec<InputSegment>,
    },
    Reseed {
        request: RequestId,
        segments: Vec<InputSegment>,
    },
    Convert {
        request: RequestId,
    },
    Commit {
        request: RequestId,
        candidate_result: CandidateResultIdentity,
        candidate: Option<usize>,
        text: Option<String>,
    },
    LiveSnapshot {
        snapshot: CompositionSnapshot,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputSegment {
    pub text: String,
    pub style: TextStyle,
}

impl From<crate::local_kana_composer::ReplaySegment> for InputSegment {
    fn from(segment: crate::local_kana_composer::ReplaySegment) -> Self {
        Self {
            text: segment.text,
            style: match segment.style {
                crate::local_kana_composer::InputStyle::Kana => TextStyle::Kana,
                crate::local_kana_composer::InputStyle::Direct => TextStyle::Direct,
            },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleOutput {
    pub eaten: bool,
    pub immediate: Option<ImmediateOperation>,
    pub background: Option<BackgroundIntent>,
}

/// A settled live-conversion result kept as the display anchor. While the
/// canonical reading keeps extending `reading`, new keys render as
/// "text + local kana suffix" instead of rewinding the whole preedit to raw
/// kana. Only stable snapshots (no unfinished roman pending) may build an
/// anchor; every non-extension event drops it.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LiveDisplayAnchor {
    reading: String,
    text: String,
}

#[derive(Clone, Default)]
pub struct InputModule {
    state: crate::input_state::InputState,
    local_kana: crate::local_kana_composer::LocalKanaComposer,
    candidates: Vec<String>,
    selected: usize,
    candidate_result: Option<CandidateResultIdentity>,
    candidate_interacted: bool,
    next_candidate_result: u64,
    expected_candidates: Option<(RequestId, u64, u64)>,
    pending_candidate_commit: Option<(RequestId, CandidateResultIdentity)>,
    next_request: u64,
    next_snapshot_request: u64,
    composition: u64,
    revision: u64,
    expected_snapshot: Option<(SnapshotIdentity, SnapshotPurpose)>,
    pending_auto_commit: Option<AutoCommitReceipt>,
    auto_commit_receipt: Option<AutoCommitReceipt>,
    replay_from_canonical: bool,
    cursor_replay_pending: bool,
    live_display_anchor: Option<LiveDisplayAnchor>,
    pending_live_display: Option<(String, Option<LiveDisplayAnchor>)>,
}

impl std::ops::Deref for InputModule {
    type Target = crate::input_state::InputState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl InputModule {
    pub(crate) fn bump_live_seq(&mut self) -> u64 { self.state.bump_live_seq() }

    pub(crate) fn bump_llm_seq(&mut self) -> u64 { self.state.bump_llm_seq() }

    pub(crate) fn can_start_llm(&self) -> bool {
        self.state.composing && !self.state.awaiting_llm()
    }

    pub(crate) fn canonical_segments(&self) -> Vec<InputSegment> {
        self.replay_segments()
    }

    pub(crate) fn live_snapshot(
        &mut self,
        configuration_generation: u64,
        connection_generation: u64,
        left_context: Option<String>,
    ) -> Option<BackgroundIntent> {
        if !self.state.composing || !self.local_kana.cursor_at_end() {
            return None;
        }
        self.next_snapshot_request = self.next_snapshot_request.checked_add(1)?;
        let identity = SnapshotIdentity {
            request: self.next_snapshot_request,
            composition: self.composition,
            revision: self.revision,
            configuration_generation,
            connection_generation,
        };
        self.expected_snapshot = Some((identity, SnapshotPurpose::Live));
        Some(BackgroundIntent::LiveSnapshot {
            snapshot: CompositionSnapshot {
                identity,
                purpose: SnapshotPurpose::Live,
                live_search_width: 1,
                mixed_source: None,
                segments: self.replay_segments(),
                left_context,
            },
        })
    }

    pub(crate) fn explicit_snapshot(
        &mut self,
        configuration_generation: u64,
        connection_generation: u64,
        left_context: Option<String>,
    ) -> Option<BackgroundIntent> {
        if !self.state.composing {
            return None;
        }
        self.next_snapshot_request = self.next_snapshot_request.checked_add(1)?;
        let identity = SnapshotIdentity {
            request: self.next_snapshot_request,
            composition: self.composition,
            revision: self.revision,
            configuration_generation,
            connection_generation,
        };
        self.expected_snapshot = Some((identity, SnapshotPurpose::Explicit));
        Some(BackgroundIntent::LiveSnapshot {
            snapshot: CompositionSnapshot {
                identity,
                purpose: SnapshotPurpose::Explicit,
                live_search_width: 1,
                mixed_source: None,
                segments: self.replay_segments(),
                left_context,
            },
        })
    }

    pub fn handle(&mut self, event: InputEvent) -> ModuleOutput {
        match event {
            InputEvent::Key(key) => self.handle_key(key),
            InputEvent::Lifecycle(LifecycleEvent::Activated) => ModuleOutput::default(),
            InputEvent::Lifecycle(LifecycleEvent::Deactivated) if self.state.composing => {
                let operation = Self::finish_operation(String::new(), true, None, None, None);
                ModuleOutput {
                    eaten: false,
                    immediate: Some(operation),
                    background: None,
                }
            }
            InputEvent::Lifecycle(LifecycleEvent::Deactivated) => ModuleOutput::default(),
            InputEvent::Engine(result) => self.handle_engine(result),
            InputEvent::Candidates(CandidateEvent::Replace {
                values,
                selected,
                reason,
            }) => self.replace_candidates(values, selected, reason),
            InputEvent::Candidates(CandidateEvent::Closed) => {
                self.clear_candidates();
                ModuleOutput::default()
            }
        }
    }

    pub fn complete(&mut self, operation: &ImmediateOperation, applied: bool) {
        if let ImmediateOperation::SetPreedit { text } = operation {
            if let Some((pending_text, anchor)) = self.pending_live_display.take() {
                if applied && *text == pending_text {
                    self.live_display_anchor = anchor;
                }
            }
        }
        if !applied {
            self.pending_auto_commit = None;
            if matches!(operation, ImmediateOperation::Cancel) {
                self.state.resume_composing_after_cancel_reject();
                self.revision = self.revision.wrapping_add(1);
                self.invalidate_live_snapshot();
                self.invalidate_live_display();
            }
            return;
        }
        if matches!(operation, ImmediateOperation::Commit { .. }) {
            self.auto_commit_receipt = self.pending_auto_commit.take();
        }
        match operation {
            ImmediateOperation::Commit {
                remaining: Some(remaining),
                remaining_latin_from,
                ..
            } if !remaining.is_empty() => {
                self.reseed_after_partial_commit_with_latin(remaining, *remaining_latin_from)
            }
            ImmediateOperation::Commit { .. } | ImmediateOperation::Cancel => self.reset(),
            _ => {}
        }
    }

    pub(crate) fn take_auto_commit_receipt(&mut self) -> Option<AutoCommitReceipt> {
        self.auto_commit_receipt.take()
    }

    pub fn reseed_after_partial_commit(&mut self, remaining: &str) {
        self.reseed_after_partial_commit_with_latin(remaining, None);
    }

    fn reseed_after_partial_commit_with_latin(
        &mut self,
        remaining: &str,
        latin_from: Option<usize>,
    ) {
        self.state
            .reseed_after_partial_commit_with_latin(remaining, latin_from);
        if !self.local_kana.retain_suffix(remaining) {
            self.local_kana.reseed_reading(remaining);
        }
        self.replay_from_canonical = true;
        self.clear_candidates();
        self.revision = self.revision.wrapping_add(1);
        self.invalidate_live_snapshot();
        // The remaining reading has no known surface form; anchoring the
        // committed prefix's text against it would splice mismatched halves.
        self.invalidate_live_display();
    }

    pub(crate) fn clear_notation(&mut self) {
        self.state.notation_fixed = None;
        self.invalidate_live_snapshot();
        self.invalidate_live_display();
    }

    pub(crate) fn set_notation(&mut self, notation: crate::keymap::Notation) {
        self.state.notation_fixed = Some(notation);
        self.invalidate_live_snapshot();
        self.invalidate_live_display();
    }

    pub(crate) fn set_awaiting_llm(&mut self, awaiting: bool) {
        if awaiting {
            // A reply calculated before correction must not overwrite the LLM result,
            // even if it arrives after the awaiting flag has been cleared.
            self.invalidate_live_snapshot();
        }
        self.state.set_awaiting_llm(awaiting);
    }

    pub(crate) fn invalidate_live_snapshot(&mut self) {
        self.pending_live_display = None;
        self.expected_snapshot = None;
        self.pending_auto_commit = None;
    }

    /// Drops the live display anchor. Deliberately separate from
    /// invalidate_live_snapshot: every key press retires the pending snapshot,
    /// but the anchor must survive plain typing and die only on non-extension
    /// events (candidates, notation, partial commit, disconnect, ...).
    pub(crate) fn invalidate_live_display(&mut self) {
        self.pending_live_display = None;
        self.live_display_anchor = None;
    }

    pub(crate) fn live_display_anchor_matches(&self, reading: &str, text: &str) -> bool {
        // Let explicit conversion resolve unfinished romaji, including final n.
        self.local_kana.reading_parts().1.is_empty()
            && self.live_display_anchor.as_ref().is_some_and(|anchor|
                anchor.reading == reading && anchor.text == text)
    }

    /// Immediate preedit text for a key press: the anchor text plus the local
    /// kana that extends it, so the converted part never flashes back to kana.
    fn immediate_display(&mut self) -> String {
        if !self.local_kana.cursor_at_end() {
            self.invalidate_live_display();
            return self.canonical_reading().to_owned();
        }
        let (stable, pending) = self.local_kana.reading_parts();
        if let Some(anchor) = &self.live_display_anchor {
            if let Some(suffix) = stable.strip_prefix(&anchor.reading) {
                return format!("{}{}{}", anchor.text, suffix, pending);
            }
        }
        self.invalidate_live_display();
        self.local_kana.reading().to_owned()
    }

    pub(crate) fn rebind_expected_snapshot_connection(
        &mut self,
        configuration_generation: u64,
        connection_generation: u64,
    ) -> bool {
        let Some((mut identity, purpose)) = self.expected_snapshot else {
            return false;
        };
        if identity.configuration_generation != configuration_generation {
            return false;
        }
        identity.connection_generation = connection_generation;
        self.expected_snapshot = Some((identity, purpose));
        // The anchor's text came from the old connection; a rebind means the
        // engine restarted, so the next display must not splice it.
        self.invalidate_live_display();
        true
    }

    pub fn candidate_commit(&mut self, index: Option<usize>) -> ModuleOutput {
        let index = index.unwrap_or(self.selected);
        let Some(candidate_result) = self.candidate_result else {
            return ModuleOutput::default();
        };
        let Some(text) = self.candidates.get(index).cloned() else {
            return ModuleOutput::default();
        };
        self.candidate_interacted = true;
        let request = self.request_id();
        self.pending_candidate_commit = Some((request, candidate_result));
        ModuleOutput {
            eaten: true,
            immediate: None,
            background: Some(BackgroundIntent::Commit {
                request,
                candidate_result,
                candidate: Some(index),
                text: Some(text),
            }),
        }
    }

    pub(crate) fn background_reseed(&mut self) -> BackgroundIntent {
        let request = self.request_id();
        BackgroundIntent::Insert {
            request,
            segments: self.replay_segments(),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> ModuleOutput {
        match key {
            KeyEvent::Text { ch, style, replay, original } => {
                let cursor_edit = self.cursor_replay_pending || !self.local_kana.cursor_at_end();
                if !self.state.composing {
                    self.composition = self.composition.wrapping_add(1);
                    self.revision = 0;
                    self.replay_from_canonical = false;
                }
                self.clear_candidates();
                match style {
                    TextStyle::Kana => {
                        self.state.on_char(ch);
                        self.local_kana.push_with_original(
                            ch,
                            crate::local_kana_composer::InputStyle::Kana,
                            original,
                        );
                    }
                    TextStyle::Direct => {
                        self.state.on_char_latin(ch);
                        self.local_kana.push_with_original(
                            ch,
                            crate::local_kana_composer::InputStyle::Direct,
                            original,
                        );
                        // Direct 挿入で作曲ジャーナルにスタイル付きの境界が生じる。raw と
                        // latin_from からの再生は latin 境界より後のかな入力まで Direct 化して
                        // しまうため、以降の Full 再生は composer 側のセグメントを権威にする
                        // (打鍵ごとの Delta 送信は影響を受けない)。
                        self.replay_from_canonical = true;
                    }
                };
                if cursor_edit { self.reanchor_after_surface_edit(self.state.latin_mode()); }
                self.cursor_replay_pending = false;
                self.revision = self.revision.wrapping_add(1);
                self.invalidate_live_snapshot();
                let request = self.request_id();
                let segments = match replay {
                    ReplayMode::Delta => vec![InputSegment {
                        text: ch.to_string(),
                        style,
                    }],
                    ReplayMode::Full => self.replay_segments(),
                };
                ModuleOutput {
                    eaten: true,
                    immediate: Some(Self::display_operation(self.immediate_display())),
                    background: Some(if cursor_edit {
                        BackgroundIntent::Reseed { request, segments: self.canonical_segments() }
                    } else { BackgroundIntent::Insert { request, segments } }),
                }
            }
            KeyEvent::MoveReading(direction) if self.state.composing => {
                self.navigate_reading(|composer| composer.move_cursor(direction))
            }
            KeyEvent::ReadingHome | KeyEvent::ReadingEnd if self.state.composing => {
                let position = if key == KeyEvent::ReadingHome { 0 } else { u32::MAX };
                self.navigate_reading(|composer| composer.set_cursor(ipc::clause::ReadingPosition(position)))
            }
            KeyEvent::Delete if self.state.composing => {
                if !self.local_kana.delete_forward() { return ModuleOutput { eaten: true, ..ModuleOutput::default() }; }
                self.clear_candidates();
                self.invalidate_live_display();
                self.reanchor_after_surface_edit(self.state.latin_mode());
                self.revision = self.revision.wrapping_add(1);
                self.invalidate_live_snapshot();
                let request = self.request_id();
                ModuleOutput {
                    eaten: true,
                    immediate: Some(if self.local_kana.reading().is_empty() { ImmediateOperation::Cancel }
                        else { Self::display_operation(self.local_kana.reading().to_owned()) }),
                    background: Some(BackgroundIntent::Reseed { request, segments: self.canonical_segments() }),
                }
            }
            KeyEvent::Backspace if self.state.composing => {
                if self.local_kana.cursor().0 == 0 && !self.canonical_reading().is_empty() {
                    return ModuleOutput { eaten: true, ..ModuleOutput::default() };
                }
                self.clear_candidates();
                self.invalidate_live_display();
                let keep_latin_mode = self.state.latin_mode();
                self.state.on_backspace();
                self.local_kana.backspace_at_cursor();
                self.reanchor_after_surface_edit(keep_latin_mode);
                self.revision = self.revision.wrapping_add(1);
                self.invalidate_live_snapshot();
                let request = self.request_id();
                let segments = self.replay_segments();
                ModuleOutput {
                    eaten: true,
                    immediate: Some(if self.local_kana.reading().is_empty() {
                        ImmediateOperation::Cancel
                    } else {
                        Self::display_operation(self.local_kana.reading().to_owned())
                    }),
                    background: Some(BackgroundIntent::Reseed { request, segments }),
                }
            }
            KeyEvent::Space if self.state.composing => {
                let request = self.request_id();
                self.expected_candidates = Some((request, self.composition, self.revision));
                ModuleOutput {
                    eaten: true,
                    immediate: None,
                    background: Some(BackgroundIntent::Convert { request }),
                }
            }
            KeyEvent::MoveCandidate(delta) if !self.candidates.is_empty() => {
                self.candidate_interacted = true;
                self.invalidate_live_display();
                let n = self.candidates.len() as i32;
                self.selected = (self.selected as i32 + delta).rem_euclid(n) as usize;
                let text = self.candidates[self.selected].clone();
                ModuleOutput {
                    eaten: true,
                    immediate: Some(Self::display_operation(text)),
                    background: None,
                }
            }
            KeyEvent::SelectCandidate(index) if !self.candidates.is_empty() => {
                self.candidate_interacted = true;
                self.invalidate_live_display();
                self.selected = index.min(self.candidates.len() - 1);
                let text = self.candidates[self.selected].clone();
                ModuleOutput {
                    eaten: true,
                    immediate: Some(Self::display_operation(text)),
                    background: None,
                }
            }
            KeyEvent::CommitCandidate(index) => self.candidate_commit(index),
            KeyEvent::Enter if !self.candidates.is_empty() => self.candidate_commit(None),
            KeyEvent::Enter if self.state.composing => {
                let text = self.local_kana.reading().to_owned();
                ModuleOutput {
                    eaten: true,
                    immediate: Some(Self::finish_operation(text, false, None, None, None)),
                    background: None,
                }
            }
            KeyEvent::Escape if self.state.composing => {
                let operation = Self::finish_operation(String::new(), true, None, None, None);
                ModuleOutput {
                    eaten: true,
                    immediate: Some(operation),
                    background: None,
                }
            }
            _ => ModuleOutput::default(),
        }
    }

    fn handle_engine(&mut self, result: EngineResult) -> ModuleOutput {
        let operation = match result {
            EngineResult::Reading { text, .. } if text.is_empty() && self.state.raw.is_empty() => {
                ImmediateOperation::Cancel
            }
            EngineResult::Reading { .. } => Self::display_operation(self.immediate_display()),
            EngineResult::Candidates { request, values } => {
                let Some((expected, composition, revision)) = self.expected_candidates else {
                    return ModuleOutput::default();
                };
                if request != expected {
                    return ModuleOutput::default();
                }
                return self.replace_candidates_for_revision(values, 0, composition, revision);
            }
            EngineResult::Commit {
                request,
                candidate,
                resolved_text,
                outcome,
            } => {
                match self.pending_candidate_commit {
                    Some((pending_request, pending_identity)) => {
                        if request != pending_request
                            || candidate.is_none()
                            || self.candidate_result != Some(pending_identity)
                        {
                            return ModuleOutput::default();
                        }
                        self.pending_candidate_commit = None;
                    }
                    None if candidate.is_some() => {
                        return ModuleOutput::default();
                    }
                    None => {}
                }
                match outcome {
                    EngineCommitOutcome::Applied {
                        text: engine_text,
                        remaining,
                    } => {
                        let (text, remaining, remaining_latin_from) = if remaining.is_empty() {
                            (resolved_text, remaining, None)
                        } else {
                            let Some(remaining) = self.validate_partial_reseed(&remaining) else {
                                return ModuleOutput::default();
                            };
                            let Some(remaining_latin_from) =
                                self.validated_remaining_latin_from(&remaining)
                            else {
                                return ModuleOutput::default();
                            };
                            (engine_text, remaining, remaining_latin_from)
                        };
                        Self::finish_operation(
                            text,
                            false,
                            candidate,
                            Some(remaining),
                            remaining_latin_from,
                        )
                    }
                    EngineCommitOutcome::Fallback { .. } => {
                        Self::finish_operation(resolved_text, false, candidate, None, None)
                    }
                }
            }
            EngineResult::Disconnected { .. } => {
                self.invalidate_live_display();
                if self.state.raw.is_empty() {
                    return ModuleOutput {
                        eaten: false,
                        immediate: Some(ImmediateOperation::Cancel),
                        background: None,
                    };
                }
                let text = self.local_kana.reading().to_owned();
                Self::display_operation(text)
            }
            EngineResult::LiveSnapshot { identity, text } => {
                if self.state.awaiting_llm()
                    || self.expected_snapshot != Some((identity, SnapshotPurpose::Live))
                    || !self.state.composing
                {
                    return ModuleOutput::default();
                }
                let (stable, pending) = self.local_kana.reading_parts();
                let anchor = if text.is_empty() {
                    None
                } else if pending.is_empty() {
                    Some(LiveDisplayAnchor {
                        reading: stable.to_owned(),
                        text: text.clone(),
                    })
                } else {
                    self.live_display_anchor.clone()
                };
                self.pending_live_display = Some((text.clone(), anchor));
                Self::display_operation(text)
            }
            EngineResult::LiveAutoCommitProposal(proposal) => {
                if self.state.awaiting_llm()
                    || self.expected_snapshot != Some((proposal.identity, SnapshotPurpose::Live))
                    || !self.state.composing
                    || self.pending_auto_commit.is_some()
                    || proposal.text.is_empty()
                    || self.local_kana.reading()
                        != format!("{}{}", proposal.consumed_reading, proposal.remaining)
                {
                    return ModuleOutput::default();
                }
                if proposal.consumed_reading.is_empty() {
                    return ModuleOutput::default();
                }
                let Some(remaining) = self.validate_partial_reseed(&proposal.remaining) else {
                    return ModuleOutput::default();
                };
                let Some(remaining_latin_from) = self.validated_remaining_latin_from(&remaining)
                else {
                    return ModuleOutput::default();
                };
                self.pending_auto_commit = Some(AutoCommitReceipt {
                    proposal: proposal.proposal,
                    identity: proposal.identity,
                });
                Self::finish_operation(
                    proposal.text,
                    false,
                    None,
                    Some(remaining),
                    remaining_latin_from,
                )
            }
            EngineResult::ExplicitSnapshot {
                identity,
                candidates,
            } => {
                if self.expected_snapshot != Some((identity, SnapshotPurpose::Explicit))
                    || !self.state.composing
                {
                    return ModuleOutput::default();
                }
                return self.replace_candidates_for_revision(
                    candidates,
                    0,
                    identity.composition,
                    identity.revision,
                );
            }
        };
        ModuleOutput {
            eaten: false,
            immediate: Some(operation),
            background: None,
        }
    }

    fn request_id(&mut self) -> RequestId {
        self.next_request += 1;
        RequestId(self.next_request)
    }

    fn replace_candidates(
        &mut self,
        values: Vec<String>,
        selected: usize,
        reason: CandidateReplacement,
    ) -> ModuleOutput {
        match reason {
            CandidateReplacement::NewResult => self.replace_candidates_for_revision(
                values,
                selected,
                self.composition,
                self.revision,
            ),
            CandidateReplacement::UserDriven => {
                let Some(current_result) = self.candidate_result else {
                    return ModuleOutput::default();
                };
                if values.is_empty()
                    || !self.state.composing
                    || current_result.composition != self.composition
                    || current_result.revision != self.revision
                {
                    return ModuleOutput::default();
                }
                self.candidate_interacted = false;
                let output = self.replace_candidates_for_revision(
                    values,
                    selected,
                    self.composition,
                    self.revision,
                );
                self.candidate_interacted = true;
                output
            }
        }
    }

    fn replace_candidates_for_revision(
        &mut self,
        values: Vec<String>,
        selected: usize,
        composition: u64,
        revision: u64,
    ) -> ModuleOutput {
        if composition != self.composition || revision != self.revision || self.candidate_interacted
        {
            return ModuleOutput::default();
        }
        if values.is_empty() {
            self.clear_candidates();
            return ModuleOutput::default();
        }
        self.next_candidate_result = self.next_candidate_result.wrapping_add(1);
        // Candidate preview replaces the preedit with a candidate surface, so
        // the converted-part anchor no longer matches what is on screen.
        self.invalidate_live_display();
        let identity = CandidateResultIdentity {
            composition,
            revision,
            result: self.next_candidate_result,
        };
        self.selected = selected.min(values.len() - 1);
        self.candidates = values.clone();
        self.candidate_result = Some(identity);
        self.pending_candidate_commit = None;
        ModuleOutput {
            eaten: false,
            immediate: Some(ImmediateOperation::ShowCandidates {
                identity,
                values,
                selected: self.selected,
            }),
            background: None,
        }
    }

    fn replay_segments(&self) -> Vec<InputSegment> {
        if self.replay_from_canonical {
            return self
                .local_kana
                .replay_segments()
                .into_iter()
                .map(InputSegment::from)
                .collect();
        }
        // latin_from が立つ経路(Direct 挿入・部分確定・再錨)はすべて同時に
        // replay_from_canonical を立てる。ゆえに latin_from アームは現在到達不能な
        // 防御であり、latin_from を単独で書く経路を将来足すと「境界後のかな入力まで
        // Direct 化する」退行がこの経由で復活する — その検出用の不変条件。
        debug_assert!(
            self.state.latin_from.is_none() || self.replay_from_canonical,
            "latin_from must promote replay to the composer journal"
        );
        match self.state.latin_from {
            Some(index) if index > 0 && index < self.state.raw.len() => vec![
                InputSegment {
                    text: self.state.raw[..index].to_string(),
                    style: TextStyle::Kana,
                },
                InputSegment {
                    text: self.state.raw[index..].to_string(),
                    style: TextStyle::Direct,
                },
            ],
            Some(0) if !self.state.raw.is_empty() => vec![InputSegment {
                text: self.state.raw.clone(),
                style: TextStyle::Direct,
            }],
            _ => vec![InputSegment {
                text: self.state.raw.clone(),
                style: TextStyle::Kana,
            }],
        }
    }

    fn display_operation(text: String) -> ImmediateOperation {
        ImmediateOperation::SetPreedit { text }
    }

    fn finish_operation(
        text: String,
        cancel: bool,
        candidate: Option<usize>,
        remaining: Option<String>,
        remaining_latin_from: Option<usize>,
    ) -> ImmediateOperation {
        if cancel {
            ImmediateOperation::Cancel
        } else {
            ImmediateOperation::Commit {
                text,
                candidate,
                remaining,
                remaining_latin_from,
            }
        }
    }

    pub(crate) fn reset(&mut self) {
        self.state.reset();
        self.local_kana.clear();
        self.replay_from_canonical = false;
        self.cursor_replay_pending = false;
        self.clear_candidates();
        self.expected_candidates = None;
        self.expected_snapshot = None;
        self.pending_auto_commit = None;
        self.auto_commit_receipt = None;
        self.invalidate_live_display();
    }

    fn clear_candidates(&mut self) {
        self.candidates.clear();
        self.selected = 0;
        self.candidate_result = None;
        self.candidate_interacted = false;
        self.expected_candidates = None;
        self.pending_candidate_commit = None;
    }

    pub(crate) fn canonical_reading(&self) -> &str {
        self.local_kana.reading()
    }

    pub(crate) fn clause_identity(&self, configuration_generation: u64, connection_generation: u64) -> ipc::clause::SnapshotIdentity {
        ipc::clause::SnapshotIdentity { composition: self.composition, revision: self.revision,
            configuration_generation, connection_generation }
    }

    pub(crate) fn reading_cursor(&self) -> ipc::clause::ReadingPosition { self.local_kana.cursor() }

    pub(crate) fn set_reading_cursor(&mut self, cursor: ipc::clause::ReadingPosition) -> ModuleOutput {
        self.navigate_reading(|composer| composer.set_cursor(cursor))
    }

    pub(crate) fn reading_revision(&self) -> u64 { self.revision }

    /// 混在表示の採用判定に使う composition 世代（PR3。revision は reading_revision）。
    pub(crate) fn composition_id(&self) -> u64 { self.composition }

    /// 混在入力の混在 Plan を composer（編集状態）へ反映する（PR3）。元打鍵の由来
    /// （Typed/Direct/不明）と再合成の境界は adoption unit が保持し、journal へ
    /// 反映される。末尾の未完ローマ字は pending として保持する（合成継続の意味を
    /// 保存）。読み・unit 列・由来のいずれかが変わるとき（厳密な no-op 以外）
    /// revision を進める — 読みが同じでも source の意味（由来・スタイル・対応単位）
    /// が変われば別世代（PR2 の source_revision 契約）。旧 JP-only snapshot
    /// （expected_snapshot）と自動確定保留は失効させる（§7.2）。
    pub(crate) fn adopt_mixed_projection(
        &mut self,
        projection: &mixed_input::projection::Projection,
    ) -> bool {
        let Some(revision) = self.revision.checked_add(1) else {
            return false;
        };
        let units = projection.adoption_units();
        let reading_before = self.local_kana.reading().to_string();
        let triples_before = self.effective_unit_signatures();
        let saved = self.local_kana.clone();
        self.local_kana.rebuild_units(&units);
        // 再構築の読みは Projection と一字一致していなければならない（採用は
        // 解釈の変更であって文字順の変更ではない）。一致しない再構築は破棄して
        // 採用前の状態へ戻す（表示・確定へ出さない）。
        if self.local_kana.reading() != projection.reading() {
            self.local_kana = saved;
            self.invalidate_live_snapshot();
            self.invalidate_live_display();
            return false;
        }
        let changed = reading_before != self.local_kana.reading()
            || triples_before != self.effective_unit_signatures();
        if changed {
            self.revision = revision;
        }
        self.clear_candidates();
        self.reanchor_after_surface_edit(self.state.latin_mode());
        self.invalidate_live_snapshot();
        self.invalidate_live_display();
        true
    }

    /// composer の完全な複製を返す（混在採用の失敗ロールバック用。PR3）。
    /// journal の unit 列だけでなく、ResolvedKana 相当の読み・pending・suffix・
    /// カーソル凍結まで含む。
    pub(crate) fn snapshot_composer(
        &self,
    ) -> crate::local_kana_composer::LocalKanaComposer {
        self.local_kana.clone()
    }

    /// snapshot_composer の複製へ戻す（混在採用の失敗ロールバック）。復元も編集と
    /// して revision を進める — 古い非同期要求の同一性を復活させない（§7.2）。
    /// 旧 snapshot の失効（invalidate_live_snapshot）は復元しない。
    pub(crate) fn restore_composer(
        &mut self,
        saved: crate::local_kana_composer::LocalKanaComposer,
    ) -> bool {
        let Some(revision) = self.revision.checked_add(1) else {
            return false;
        };
        self.local_kana = saved;
        self.revision = revision;
        self.clear_candidates();
        self.reanchor_after_surface_edit(self.state.latin_mode());
        self.invalidate_live_snapshot();
        self.invalidate_live_display();
        true
    }

    /// Literal の採用印は revision や表示キャッシュから独立に保持する。
    pub(crate) fn widen_unprotected_digits(&self, text: &str) -> String {
        let units = self.local_kana.effective_input_units();
        // Converted surfaces have no scalar mapping to the composer journal.
        // Preserve their width until a validated projection supplies that mapping.
        if text != self.canonical_reading() && units.iter().any(|u| u.literal) {
            return text.to_owned();
        }
        text.chars().enumerate().map(|(at, ch)| {
            let protected = units.iter().any(|u| u.literal && u.start.0 <= at as u32 && (at as u32) < u.end.0);
            if ch.is_ascii_digit() && !protected { char::from_u32(ch as u32 + 0xFEE0).unwrap() } else { ch }
        }).collect()
    }

    /// 現在の実効 unit 列（読み・journal 登録スタイル・元打鍵・Literal 採用印）。採用が source の
    /// 意味を変えたかの判定に使う。
    pub(crate) fn effective_unit_signatures(&self) -> Vec<(String, bool, String, bool)> {
        self.local_kana
            .effective_input_units()
            .into_iter()
            .map(|unit| {
                let literal = unit.style == crate::local_kana_composer::InputStyle::Direct;
                let reading: String = self
                    .local_kana
                    .reading()
                    .chars()
                    .skip(unit.start.0 as usize)
                    .take((unit.end.0 - unit.start.0) as usize)
                    .collect();
                (reading, literal, unit.original, unit.literal)
            })
            .collect()
    }

    pub(crate) fn finalize_pending_n(&mut self) -> Option<bool> {
        let mut composer = self.local_kana.clone();
        if !composer.finalize_pending_n() { return Some(false); }
        let revision = self.revision.checked_add(1)?;
        self.local_kana = composer;
        self.revision = revision;
        self.reanchor_after_surface_edit(self.state.latin_mode());
        self.clear_candidates();
        self.invalidate_live_snapshot();
        self.invalidate_live_display();
        Some(true)
    }

    pub(crate) fn adopt_conversion_reading(&mut self, reading: &str) -> bool {
        // 読みの一致は no-op（composer が早期 return する）なので世代を進めない。
        // それ以外の採用は読み・完成状態・由来スタイルを変えるため、composition
        // source の内容が変わる。plan_id + source_revision で同一性を判定する後続
        // 処理が取り込み前後の対応表を区別できるよう、編集と同じく世代を進める。
        let changes = self.local_kana.reading() != reading;
        if !self.local_kana.adopt_conversion_reading(reading) { return false; }
        if changes {
            self.revision = self.revision.wrapping_add(1);
        }
        self.reanchor_after_surface_edit(self.state.latin_mode());
        self.invalidate_live_snapshot();
        self.invalidate_live_display();
        true
    }

    pub(crate) fn reading_cursor_utf16(&self) -> ipc::clause::DisplayUtf16Position {
        ipc::clause::DisplayUtf16Position(self.canonical_reading().chars().take(self.reading_cursor().0 as usize)
            .map(char::len_utf16).sum::<usize>() as u32)
    }

    pub(crate) fn original_input(&self, start: ipc::clause::ReadingPosition, end: ipc::clause::ReadingPosition) -> Option<String> {
        self.local_kana.original_input(start, end)
    }

    /// 現在の未確定入力の元入力ソース（混在入力 PR2）。元入力の世代には
    /// composition revision を使う。表示・変換への接続は PR3 以降。
    #[allow(dead_code)]
    pub(crate) fn composition_source(&self) -> mixed_input::source::CompositionSource {
        self.local_kana.composition_source(self.revision)
    }

    pub(crate) fn preserve_last_literal_original(&mut self, original: char) {
        self.local_kana.preserve_last_literal_original(original);
    }

    /// 読みナビゲーション共通（MoveReading / ReadingHome / ReadingEnd /
    /// set_reading_cursor）。末尾 pending を凍結する移動は、canonical reading が
    /// 不変でも composition source の由来を実効 Kana → Direct 凍結へ替えるため、
    /// 同じ plan_id + source_revision で異なる Projection が成立しないよう世代を
    /// 進める。unit を凍結しない純粋な移動は世代を進めない（source 内容も不変）。
    fn navigate_reading(
        &mut self,
        move_composer: impl FnOnce(&mut crate::local_kana_composer::LocalKanaComposer) -> bool,
    ) -> ModuleOutput {
        let freezes_tail_pending =
            !self.local_kana.reading_parts().1.is_empty() && self.local_kana.cursor_at_end();
        let moved = move_composer(&mut self.local_kana);
        if moved && freezes_tail_pending {
            self.revision = self.revision.wrapping_add(1);
        }
        self.reading_navigation_output(moved)
    }

    fn reading_navigation_output(&mut self, moved: bool) -> ModuleOutput {
        if !moved { return ModuleOutput { eaten: true, ..ModuleOutput::default() }; }
        self.clear_candidates();
        self.state.notation_fixed = None;
        self.invalidate_live_snapshot();
        self.invalidate_live_display();
        self.replay_from_canonical = true;
        self.cursor_replay_pending = true;
        ModuleOutput { eaten: true, immediate: Some(Self::display_operation(self.canonical_reading().to_owned())), background: None }
    }

    pub(crate) fn retain_clause_reading_prefix(&mut self, prefix: &str) -> Option<u64> {
        let revision = self.revision.checked_add(1)?;
        if !self.local_kana.reading().starts_with(prefix) || prefix.len() >= self.local_kana.reading().len()
            || !ipc::clause::legal_boundaries(self.local_kana.reading()).ok()?
                .contains(&ipc::clause::ReadingPosition(prefix.chars().count() as u32)) { return None; }
        if !self.local_kana.retain_prefix(prefix) { return None; }
        self.reanchor_after_surface_edit(false);
        self.revision = revision;
        self.clear_candidates();
        self.invalidate_live_snapshot();
        self.invalidate_live_display();
        Some(revision)
    }

    fn reanchor_after_surface_edit(&mut self, keep_latin_mode: bool) {
        let reading = self.local_kana.reading();
        // raw を読みで再錨するため、旧 raw ドメインの state.latin_from は無効。作曲セグメントの
        // 末尾 Direct 接尾から境界を再計算する(接尾が無ければ latin 幅 0 = モード維持のみ)。
        let latin_from = keep_latin_mode.then(|| {
            self.local_kana
                .replay_segments()
                .last()
                .filter(|segment| segment.style == crate::local_kana_composer::InputStyle::Direct)
                .map_or(reading.len(), |segment| reading.len() - segment.text.len())
        });
        self.state.raw = reading.to_owned();
        self.state.composing = !reading.is_empty();
        self.state.notation_fixed = None;
        self.state.latin_from = self.state.composing.then_some(latin_from).flatten();
        self.replay_from_canonical = true;
    }

    pub(crate) fn validate_partial_reseed(&self, proposed: &str) -> Option<String> {
        if proposed.is_empty() {
            return None;
        }
        let canonical = self.local_kana.reading();
        let prefix_len = canonical.len().checked_sub(proposed.len())?;
        if prefix_len == 0
            || !canonical.ends_with(proposed)
            || !self.local_kana.can_retain_suffix(proposed)
        {
            return None;
        }
        Some(canonical[prefix_len..].to_owned())
    }

    fn validated_remaining_latin_from(&self, remaining: &str) -> Option<Option<usize>> {
        if self.state.latin_from.is_none() {
            return Some(None);
        }
        // 残りの latin 境界は作曲ジャーナルから計る。raw は Direct 挿入後も生ローマ字の
        // まま(ドメインが読みと混在)なので、raw[latin_from..] を remaining(読みドメイン)と
        // 直接比較すると正当な提案を誤って棄却する。remaining 領域内で最初の Direct
        // セグメントが始まる位置が新しい境界、Direct が無ければ latin 幅 0(末尾)。
        let canonical = self.local_kana.reading();
        let suffix_start = canonical.len().checked_sub(remaining.len())?;
        let mut offset = 0usize;
        let mut latin_start: Option<usize> = None;
        for segment in self.local_kana.replay_segments() {
            let seg_end = offset + segment.text.len();
            if latin_start.is_none()
                && seg_end > suffix_start
                && segment.style == crate::local_kana_composer::InputStyle::Direct
            {
                latin_start = Some(offset.max(suffix_start) - suffix_start);
            }
            offset = seg_end;
        }
        Some(Some(latin_start.unwrap_or(remaining.len())))
    }
}

pub fn resolve_current_flat_candidate(module: &mut InputModule) -> ModuleOutput {
    module.candidate_commit(None)
}

pub fn resolve_absolute_flat_candidate(module: &mut InputModule, index: usize) -> ModuleOutput {
    module.candidate_commit(Some(index))
}

pub(crate) fn apply_presenter_candidate_selection(
    module: &mut InputModule,
    index: usize,
) -> ModuleOutput {
    module.handle(InputEvent::Key(KeyEvent::SelectCandidate(index)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_adoption_advances_revision_even_when_direct_reading_is_unchanged() {
        use mixed_input::{plan::{InterpretationPlan, SegmentKind}, projection::Projection};
        let mut module = InputModule::default();
        module.handle(InputEvent::Key(KeyEvent::Text { ch: '3', style: TextStyle::Direct, replay: ReplayMode::Delta, original: None }));
        let before = module.reading_revision();
        let source = module.composition_source();
        let plan = InterpretationPlan::build("3", &[(SegmentKind::Literal, "3".into())]).unwrap();
        assert!(module.adopt_mixed_projection(&Projection::build(1, &source, &plan).unwrap()));
        assert_eq!(module.reading_revision(), before + 1);
        let source = module.composition_source();
        assert!(module.adopt_mixed_projection(&Projection::build(2, &source, &plan).unwrap()));
        assert_eq!(module.reading_revision(), before + 1);
    }

    #[test]
    fn frozen_nonfirst_candidate_keeps_selection_and_suffix_across_rejection() {
        for remaining in ["", "にいく"] {
            let mut module = InputModule::default();
            for ch in "toukyouniiku".chars() { module.handle(key(ch)); }
            let mut request = prepare_candidate_commit(&mut module, vec!["とうきょう".into(), "東京".into()], 1);
            for applied in [false, true] {
                let output = module.handle(InputEvent::Engine(EngineResult::Commit {
                    request, candidate: Some(1), resolved_text: "東京".into(),
                    outcome: EngineCommitOutcome::Applied { text: "東京".into(), remaining: remaining.into() },
                }));
                let operation = output.immediate.unwrap();
                assert!(matches!(&operation, ImmediateOperation::Commit { text, candidate: Some(1), remaining: Some(tail), .. }
                    if text == "東京" && tail == remaining));
                module.complete(&operation, applied);
                if !applied {
                    assert_eq!(module.canonical_reading(), "とうきょうにいく");
                    let retry = module.candidate_commit(Some(1));
                    request = match retry.background { Some(BackgroundIntent::Commit { request, text: Some(text), .. }) => {
                        assert_eq!(text, "東京"); request
                    }, other => panic!("{other:?}") };
                }
            }
            assert_eq!(module.canonical_reading(), remaining);
        }
    }

    #[test]
    fn literal_kana_can_be_repaired_to_japanese_after_more_typing() {
        use mixed_input::{plan::{InterpretationPlan, SegmentKind}, projection::Projection, position::SourceRange};
        let mut module = InputModule::default();
        for ch in "made".chars() { module.handle(key(ch)); }
        let source = module.composition_source();
        let literal = InterpretationPlan::build("made", &[(SegmentKind::Literal, "made".into())]).unwrap();
        assert!(module.adopt_mixed_projection(&Projection::build(1, &source, &literal).unwrap()));
        for ch in "desu".chars() { module.handle(key(ch)); }
        let source = module.composition_source();
        assert_eq!(source.source_text(), "madedesu");
        let plan = InterpretationPlan::build("madedesu", &[(SegmentKind::Literal, "made".into()), (SegmentKind::Japanese, "desu".into())]).unwrap();
        let repaired = mixed_input::selection::reinterpret(&source, &plan, SourceRange::new(0, 4), SegmentKind::Japanese).unwrap();
        assert!(module.adopt_mixed_projection(&Projection::build(2, &source, &repaired).unwrap()));
        assert_eq!(module.canonical_reading(), "までです");
    }

    #[test]
    fn literal_digits_stay_protected_after_typing_cursor_edits_and_deletion() {
        use mixed_input::{plan::{InterpretationPlan, SegmentKind}, projection::Projection};
        let mut module = InputModule::default();
        for ch in "Python3".chars() { module.handle(key(ch)); }
        let source = module.composition_source();
        let plan = InterpretationPlan::build("Python3", &[(SegmentKind::Literal, "Python3".into())]).unwrap();
        assert!(module.adopt_mixed_projection(&Projection::build(1, &source, &plan).unwrap()));
        for ch in "wo4".chars() { module.handle(key(ch)); }
        assert_eq!(module.widen_unprotected_digits(module.canonical_reading()), "Python3を４");
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(module.widen_unprotected_digits(module.canonical_reading()), "Python3を");
        module.set_reading_cursor(ipc::clause::ReadingPosition(0));
        module.handle(key('5'));
        assert_eq!(module.widen_unprotected_digits(module.canonical_reading()), "５Python3を");
        module.set_reading_cursor(ipc::clause::ReadingPosition(6));
        module.handle(InputEvent::Key(KeyEvent::Delete));
        assert_eq!(module.widen_unprotected_digits(module.canonical_reading()), "５Pytho3を");
    }

    #[test]
    fn mixed_prefix_reconstructs_source_only_after_successful_commit() {
        use mixed_input::{live::{commit_fence, source_after_prefix}, plan::{InterpretationPlan, SegmentKind}, projection::Projection};
        let mut module = InputModule::default();
        for ch in "kyouhaRustnotukaikata".chars() { module.handle(key(ch)); }
        let source = module.composition_source();
        let plan = InterpretationPlan::build(&source.source_text(), &[
            (SegmentKind::Japanese,"kyouha".into()), (SegmentKind::Literal,"Rust".into()),
            (SegmentKind::Japanese,"notukaikata".into())]).unwrap();
        let projection = Projection::build(1,&source,&plan).unwrap();
        let fence = commit_fence(&source,&projection,&[plan],false,None);
        let expected = source_after_prefix(&source,&projection,fence,8,2).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        let operation = ImmediateOperation::Commit { text:"今日はRust".into(), candidate:None,
            remaining:Some("のつかいかた".into()), remaining_latin_from:None };
        let before = module.composition_source();
        module.complete(&operation,false);
        assert_eq!(module.composition_source(),before);
        module.complete(&operation,true);
        assert_eq!(module.canonical_reading(),"のつかいかた");
        assert_eq!(module.composition_source().elements(),expected.elements());
        for ch in "desu".chars() { module.handle(key(ch)); }
        assert_eq!(module.composition_source().source_text(),"notukaikatadesu");
        assert_eq!(module.canonical_reading(),"のつかいかたです");
    }

    #[test]
    fn idle_keys_pass_and_cancel_rejection_preserves_the_actual_composition() {
        let mut module = InputModule::default();
        for event in [KeyEvent::Enter, KeyEvent::Escape] {
            assert!(!module.handle(InputEvent::Key(event)).eaten);
        }
        assert!(module.explicit_snapshot(1, 1, None).is_none());
        module.handle(key('a'));
        assert!(module.explicit_snapshot(1, 1, None).is_some());
        let cancel = module.handle(InputEvent::Key(KeyEvent::Escape)).immediate.unwrap();
        module.complete(&cancel, false);
        let enter = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert!(matches!(enter.immediate, Some(ImmediateOperation::Commit { ref text, .. }) if text == "あ"));
        module.complete(&cancel, true);
        assert!(!module.handle(InputEvent::Key(KeyEvent::Enter)).eaten);
        assert!(module.canonical_reading().is_empty());
    }

    #[test]
    fn llm_admission_uses_the_production_module_state() {
        let mut module = InputModule::default();
        assert!(!module.can_start_llm());
        module.handle(key('a'));
        assert!(module.can_start_llm());
        module.set_awaiting_llm(true);
        assert!(!module.can_start_llm());
        module.set_awaiting_llm(false);
        assert!(module.can_start_llm());
        module.reset();
        assert!(!module.can_start_llm());
    }

    #[test]
    fn folded_literal_keeps_original_keys_in_the_journal() {
        use ipc::clause::ReadingPosition as P;
        let mut module = InputModule::default();
        module.handle(key('a'));
        module.handle(key('。'));
        module.preserve_last_literal_original('.');
        assert_eq!(module.canonical_reading(), "あ。");
        assert_eq!(module.original_input(P(0), P(2)).as_deref(), Some("a."));
        module.handle(InputEvent::Key(KeyEvent::ReadingHome));
        module.handle(key('、'));
        module.preserve_last_literal_original(',');
        assert_eq!(module.original_input(P(0), P(3)).as_deref(), Some(",a."));
    }

    #[test]
    fn composition_source_tracks_typed_provenance_and_revision_across_edits() {
        use mixed_input::position::SourceRange;
        use mixed_input::source::Provenance;
        // 混在入力 PR2: InputModule 経由で元入力ソースを導出し、英字の大小と
        // 由来・世代（revision）が正しく追従することを固定する。
        let mut module = InputModule::default();
        for event in "kyou".chars().map(key).chain([direct_key('P'), direct_key('y')]) {
            module.handle(event);
        }
        assert_eq!(module.canonical_reading(), "きょうPy");

        let source = module.composition_source();
        assert_eq!(source.revision(), module.revision);
        assert_eq!(source.source_text(), "kyouPy");
        assert_eq!(source.reading_text(), "きょうPy");
        assert!(source.is_original_recoverable(SourceRange::new(0, 6)));
        // Direct 打鍵部分も元文字（大小）が分かる Typed。
        assert_eq!(source.elements()[2].provenance, Provenance::Typed { style: mixed_input::source::SourceStyle::Direct });

        // 編集で revision が進み、旧世代のソースは stale と判定できる。
        let stale_revision = source.revision();
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        let source = module.composition_source();
        assert_eq!(module.canonical_reading(), "きょうP");
        assert_eq!(source.source_text(), "kyouP");
        assert_ne!(source.revision(), stale_revision);
        assert_eq!(source.revision(), module.revision);
    }

    #[test]
    fn mixed_projection_adoption_updates_the_editing_state() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // nihongoka（読み にほんごか）へ Japanese(nihongo) + Literal(ka) を採用する。
        // 採用は composer（読み・編集・元打鍵の由来）へ反映され、読みは にほんごka
        // になる。採用後の Backspace は Literal の a を削り、採用前の読みの か を
        // 対象にしない（表示と編集対象の不一致を残さない）。
        let mut module = InputModule::default();
        for ch in "nihongoka".chars() {
            module.handle(key(ch));
        }
        assert_eq!(module.canonical_reading(), "にほんごか");
        let projection = {
            let source = module.composition_source();
            let plan = InterpretationPlan::build(
                &source.source_text(),
                &[
                    (SegmentKind::Japanese, "nihongo".to_string()),
                    (SegmentKind::Literal, "ka".to_string()),
                ],
            )
            .unwrap();
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap()
        };
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "にほんごka");
        // 元打鍵の由来は保存される（Literal は原文 ka、Japanese はローマ字）。
        let source = module.composition_source();
        assert_eq!(source.source_text(), "nihongoka");
        assert_eq!(source.reading_text(), "にほんごka");

        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(
            module.canonical_reading(),
            "にほんごk",
            "Backspace は Literal 末尾の a を削る（採用前の か ではない）"
        );
        let source = module.composition_source();
        assert_eq!(source.source_text(), "nihongok");
    }

    #[test]
    fn mixed_adoption_invalidates_pending_live_snapshots() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // 採用前に発行した旧 JP-only snapshot への応答は、採用後には適用しない
        // （expected_snapshot の失効 — §7.2。旧解釈の表示で Literal を上書きさせない）。
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(1, 1, None).unwrap()
        else { unreachable!() };
        let projection = {
            let source = module.composition_source();
            let plan = InterpretationPlan::build(
                &source.source_text(),
                &[(SegmentKind::Japanese, source.source_text())],
            )
            .unwrap();
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap()
        };
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                identity: snapshot.identity,
                text: "日本語".into(),
            })),
            ModuleOutput::default(),
            "採用後に届いた旧 snapshot 応答は拒否される"
        );
    }

    #[test]
    fn mixed_adoption_rollback_restores_the_previous_units() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // 採用の失敗ロールバック: 採用前の composer の完全な複製（ResolvedKana
        // 相当の読み・pending・suffix を含む）を保存しておき、復元で読み・由来を
        // もとに戻せる（編集として revision は進む）。
        let mut module = InputModule::default();
        for ch in "nihongoka".chars() {
            module.handle(key(ch));
        }
        // journal の対応を失った読み（ResolvedKana 相当）も含めて保存されることの
        // 確認: kyo → Backspace で 元打鍵不明の き を作り、Direct の X を足す。
        let mut edited = InputModule::default();
        for ch in "kyo".chars() {
            edited.handle(key(ch));
        }
        edited.handle(InputEvent::Key(KeyEvent::Backspace));
        edited.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'X',
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        }));
        assert_eq!(edited.canonical_reading(), "きX");
        let saved_edited = edited.snapshot_composer();

        let saved = module.snapshot_composer();
        let projection = {
            let source = module.composition_source();
            let plan = InterpretationPlan::build(
                &source.source_text(),
                &[
                    (SegmentKind::Japanese, "nihongo".to_string()),
                    (SegmentKind::Literal, "ka".to_string()),
                ],
            )
            .unwrap();
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap()
        };
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "にほんごka");
        assert!(module.restore_composer(saved));
        assert_eq!(module.canonical_reading(), "にほんごか");
        let source = module.composition_source();
        assert_eq!(source.source_text(), "nihongoka");

        // ResolvedKana 相当の読み（き）を含む状態の復元でも文字が欠落しない。
        let projection = {
            let source = edited.composition_source();
            let plan = InterpretationPlan::build(
                &source.source_text(),
                &[
                    (SegmentKind::Japanese, "き".to_string()),
                    (SegmentKind::Literal, "X".to_string()),
                ],
            )
            .unwrap();
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap()
        };
        assert!(edited.adopt_mixed_projection(&projection));
        assert_eq!(edited.canonical_reading(), "きX");
        assert!(edited.restore_composer(saved_edited));
        assert_eq!(edited.canonical_reading(), "きX", "不明由来のきが欠落しない");
        let source = edited.composition_source();
        assert_eq!(source.source_text(), "きX");
    }

    #[test]
    fn mixed_adoption_keeps_direct_boundaries_and_unknown_provenance() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // Direct("n"→"n") + Kana("na"→"な") + Direct("X") に Japanese("nna") +
        // Literal("X") を採用する。採用後の再 Projection で n と na が同じ再合成
        // run に混入せず（Direct 境界の保存）、読みが composer と一致する。
        let mut module = InputModule::default();
        module.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'n',
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        }));
        for ch in "na".chars() {
            module.handle(key(ch));
        }
        module.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'X',
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        }));
        assert_eq!(module.canonical_reading(), "nなX");
        let projection = {
            let source = module.composition_source();
            let plan = InterpretationPlan::build(
                &source.source_text(),
                &[
                    (SegmentKind::Japanese, "nna".to_string()),
                    (SegmentKind::Literal, "X".to_string()),
                ],
            )
            .unwrap();
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap()
        };
        assert_eq!(projection.reading(), "nなX", "採用前の Projection は Direct 境界を保存");
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "nなX");
        // 採用後の再 Projection も同じ読み（run 混入で んあX にならない）。
        let reprojected = {
            let source = module.composition_source();
            let plan = InterpretationPlan::build(
                &source.source_text(),
                &[
                    (SegmentKind::Japanese, "nna".to_string()),
                    (SegmentKind::Literal, "X".to_string()),
                ],
            )
            .unwrap();
            mixed_input::projection::Projection::build(2, &source, &plan).unwrap()
        };
        assert_eq!(
            reprojected.reading(),
            module.canonical_reading(),
            "採用後の再 Projection は composer の読みと一致する"
        );
    }

    #[test]
    fn mixed_adoption_keeps_unknown_provenance_unknown() {
        use mixed_input::position::SourceRange;
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // 元打鍵不明の き（ResolvedKana）は、Japanese として採用しても Typed に
        // ならない（original_input は None を維持し、Literal 化は拒否される）。
        let mut module = InputModule::default();
        for ch in "kyo".chars() {
            module.handle(key(ch));
        }
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(module.canonical_reading(), "き");
        let source = module.composition_source();
        assert_eq!(source.original(SourceRange::new(0, 1)), None);

        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "き");
        let source = module.composition_source();
        assert_eq!(
            source.original(SourceRange::new(0, 1)),
            None,
            "採用で元打鍵不明の範囲を既知にしない"
        );
        // Literal への再解釈は原文が復元できないため Projection が拒否する。
        let literal_plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Literal, source.source_text())],
        )
        .unwrap();
        assert!(matches!(
            mixed_input::projection::Projection::build(2, &source, &literal_plan),
            Err(mixed_input::projection::ProjectionError::LiteralOriginalUnknown { .. })
        ));
    }

    #[test]
    fn mixed_adoption_keeps_trailing_pending_composition() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // 未完 Kana の k（pending）を含む Literal("X") + Japanese("k") の採用は、
        // 末尾を pending として保持する。次の a は k と合成して か になる
        // （stable 固定で Xkあ にならない）。
        let mut module = InputModule::default();
        module.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'X',
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        }));
        module.handle(key('k'));
        assert_eq!(module.canonical_reading(), "Xk");
        let source = module.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Literal, "X".to_string()),
                (SegmentKind::Japanese, "k".to_string()),
            ],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "Xk");
        module.handle(key('a'));
        assert_eq!(module.canonical_reading(), "Xか", "未完 k は a と合成を続ける");
    }

    #[test]
    fn mixed_adoption_keeps_a_plan_split_unfinished_tail_pending() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // Plan 境界が未完ローマ字を分割するケース: ky へ Literal("k") + Japanese("y")
        // を採用すると、未完の残り y は pending として保持される。次の a は y と
        // 合成して や になる（kや。stable 固定で kyあ にならない）。
        let mut module = InputModule::default();
        for ch in "ky".chars() {
            module.handle(key(ch));
        }
        assert_eq!(module.canonical_reading(), "ky");
        let source = module.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Literal, "k".to_string()),
                (SegmentKind::Japanese, "y".to_string()),
            ],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert_eq!(projection.reading(), "ky");
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "ky");
        module.handle(key('a'));
        assert_eq!(module.canonical_reading(), "kや", "分割された未完 y は a と合成を続ける");
    }

    #[test]
    fn mixed_adoption_keeps_source_order_when_a_sealed_unit_follows_the_pending_tail() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // 上の対称ケース: ky 全体が pending の状態で Japanese("k") + Literal("y")
        // を採用する。sealed な y を stable へ戻すと再構築順（stable → pending →
        // suffix）で "yk" と逆転するため、y は pending の後ろ（suffix 側）へ置き
        // 読み "ky" を保つ。次の a は k と合成して かy になる（kyあ にならない）。
        let mut module = InputModule::default();
        for ch in "ky".chars() {
            module.handle(key(ch));
        }
        assert_eq!(module.canonical_reading(), "ky");
        let source = module.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Japanese, "k".to_string()),
                (SegmentKind::Literal, "y".to_string()),
            ],
        )
        .unwrap();
        let projection = mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert_eq!(projection.reading(), "ky");
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "ky", "採用で文字順が逆転しない");
        module.handle(key('a'));
        assert_eq!(
            module.canonical_reading(),
            "かy",
            "未完 k は a と合成し、sealed な y は後ろに残る"
        );
    }

    #[test]
    fn mixed_adoption_keeps_frozen_roman_reopenable_but_seals_explicit_direct() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // カーソル移動で凍結したローマ字（ky）は、採用を挟んでも「削除で再開できる」
        // を維持する: 採用 → Backspace（y を削る）→ a で Xか。一方で明示 Direct は
        // 採用後も再開しない（封印のまま）。
        let mut module = InputModule::default();
        module.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'X',
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        }));
        for ch in "ky".chars() {
            module.handle(key(ch));
        }
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(3)).eaten);
        let source = module.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Literal, "X".to_string()),
                (SegmentKind::Japanese, "ky".to_string()),
            ],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "Xky");
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(module.canonical_reading(), "Xk", "凍結末尾の削除で未完 k が再開する");
        module.handle(key('a'));
        assert_eq!(module.canonical_reading(), "Xか", "再開した k は a と合成する");

        // 明示 Direct（採用前に凍結一覧へ無い）は採用後も再開しない: Japanese("X")
        // として採用しても、Backspace は X を削るだけで pending へ戻さない。
        let mut sealed = InputModule::default();
        sealed.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'X',
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        }));
        let source = sealed.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, "X".to_string())],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(sealed.adopt_mixed_projection(&projection));
        sealed.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(sealed.canonical_reading(), "", "明示 Direct は再開せず削除される");
    }

    #[test]
    fn mixed_adoption_keeps_unknown_pending_unfinished() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // 元打鍵不明の未完（凍結 unit の部分削除で reopen した k。pending_originals 空）
        // は、採用で stable 化せず未完を引き継ぐ。次の a は k と合成して か に
        // なり、原文字不明も維持する（捏造しない）。
        let mut module = InputModule::default();
        module.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'X',
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        }));
        for ch in "ky".chars() {
            module.handle(key(ch));
        }
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(3)).eaten);
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(module.canonical_reading(), "Xk");
        let source = module.composition_source();
        assert_eq!(source.original(mixed_input::position::SourceRange::new(1, 2)), None);
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Literal, "X".to_string()),
                (SegmentKind::Japanese, "k".to_string()),
            ],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "Xk");
        module.handle(key('a'));
        assert_eq!(module.canonical_reading(), "Xか", "不明由来の未完 k も合成を続ける");
        let source = module.composition_source();
        assert_eq!(
            source.original(mixed_input::position::SourceRange::new(1, 2)),
            None,
            "再合成後も原文字不明を維持する"
        );
    }

    #[test]
    fn mixed_adoption_reopen_inheritance_is_position_anchored() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // 誤継承の排除: 明示 Direct の k（先頭）と凍結ローマ字の k（末尾）が同文。
        /// 再開可能なのは末尾だけ。Japanese(kak) 採用 → BS2 → a で kあ（先頭 k が
        /// pending へ戻って か にはならない）。
        let mut module = InputModule::default();
        module.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'k',
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        }));
        for ch in "ak".chars() {
            module.handle(key(ch));
        }
        assert_eq!(module.canonical_reading(), "kあk");
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(3)).eaten);
        let source = module.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "kあk");
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(module.canonical_reading(), "k", "先頭の明示 Direct は再開しない");
        module.handle(key('a'));
        assert_eq!(module.canonical_reading(), "kあ");

        // 継承漏れの排除: 別々に凍結した k と y（run 文字列 "ky" と完全一致する
        /// unit がない）でも、source 範囲の包含で両方が再開可能を継承する。
        let mut split = InputModule::default();
        split.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'X',
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        }));
        split.handle(key('k'));
        assert!(split.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        assert!(split.set_reading_cursor(ipc::clause::ReadingPosition(2)).eaten);
        split.handle(key('y'));
        assert!(split.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        assert!(split.set_reading_cursor(ipc::clause::ReadingPosition(3)).eaten);
        let source = split.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Literal, "X".to_string()),
                (SegmentKind::Japanese, "ky".to_string()),
            ],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(split.adopt_mixed_projection(&projection));
        split.handle(InputEvent::Key(KeyEvent::Backspace));
        split.handle(key('a'));
        assert_eq!(split.canonical_reading(), "Xか", "分割凍結でも再開を継承する");
    }

    #[test]
    fn mixed_adoption_inherits_suffix_side_frozen_regions() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // カーソル後方（suffix 側）に置かれた凍結ローマ字も継承する: Home だけ実行
        // して ky を suffix へ凍結した状態で採用 → End → BS → a で Xか。
        let mut module = InputModule::default();
        module.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'X',
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        }));
        for ch in "ky".chars() {
            module.handle(key(ch));
        }
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        let source = module.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Literal, "X".to_string()),
                (SegmentKind::Japanese, "ky".to_string()),
            ],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "Xky");
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(3)).eaten);
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        module.handle(key('a'));
        assert_eq!(
            module.canonical_reading(),
            "Xか",
            "suffix 側の凍結も採用を挟んで再開できる"
        );
    }

    #[test]
    fn mixed_adoption_handles_resolved_kana_offsets_without_false_inheritance() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // 元打鍵不明の「き」のあとの凍結 k: source 座標は composition_source と同じ
        // 歩行（不明範囲も読み長で数える）から求める。ずれていると あ→あ の unit に
        // 再開可能を誤継承し、削除で多バイト文字の内部を切って panic し得る。
        let mut module = InputModule::default();
        for ch in "kya".chars() {
            module.handle(key(ch));
        }
        module.handle(InputEvent::Key(KeyEvent::Backspace)); // き ゚→き（元打鍵不明）
        assert_eq!(module.canonical_reading(), "き");
        module.handle(key('a'));
        module.handle(key('k'));
        assert_eq!(module.canonical_reading(), "きあk");
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(3)).eaten);
        let source = module.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "きあk");
        // BS は末尾 k を削り、誤継承がなく「あ」の内部を切らない。
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(module.canonical_reading(), "きあ");
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(module.canonical_reading(), "き", "あ の unit は再開可能を継承しない");
    }

    #[test]
    fn mixed_adoption_splits_a_straddling_unknown_unit() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // 元打鍵不明の stable（き）と元打鍵不明の pending（k）が 1 つの Unknown に
        // まとまるケース。pending 境界で分割し、未完 k は合成を続ける。
        let mut module = InputModule::default();
        for ch in "kya".chars() {
            module.handle(key(ch));
        }
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        for ch in "ky".chars() {
            module.handle(key(ch));
        }
        assert_eq!(module.canonical_reading(), "きky");
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(4)).eaten);
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(module.canonical_reading(), "きk");
        let source = module.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "きk");
        module.handle(key('a'));
        assert_eq!(module.canonical_reading(), "きか", "Unknown 内の pending 境界も分割して引き継ぐ");
    }

    #[test]
    fn mixed_adoption_inherits_reopen_for_unknown_frozen_regions() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // 元打鍵不明のまま再凍結した k（原文字不明だが削除で再開できる）も、採用で
        // 再開可能性を失わない。BS→a で Xか。
        let mut module = InputModule::default();
        module.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'X',
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        }));
        for ch in "ky".chars() {
            module.handle(key(ch));
        }
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(3)).eaten);
        module.handle(InputEvent::Key(KeyEvent::Backspace)); // 不明の k を reopen
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(2)).eaten);
        module.handle(key('i'));
        assert_eq!(module.canonical_reading(), "Xkい", "凍結 k の後の i は即 い になる");
        let source = module.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Literal, "X".to_string()),
                (SegmentKind::Japanese, "ki".to_string()),
            ],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(module.canonical_reading(), "Xk");
        module.handle(key('a'));
        assert_eq!(module.canonical_reading(), "Xか", "不明凍結 k の再開可能性を継承する");
    }

    #[test]
    fn mixed_adoption_maps_suffix_offsets_with_pending() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // pending（k）がある状態での suffix 側凍結（ky）: 読み座標は stable+pending
        // を加算し、source 座標も歩行から求める。End→BS→a で kか。
        let mut module = InputModule::default();
        for ch in "ky".chars() {
            module.handle(key(ch));
        }
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        module.handle(key('k'));
        assert_eq!(module.canonical_reading(), "kky");
        let source = module.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(3)).eaten);
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        module.handle(key('a'));
        assert_eq!(module.canonical_reading(), "kか", "suffix 凍結の再開も pending 込みの座標で継承する");
    }

    #[test]
    fn mixed_adoption_does_not_split_a_completed_unit_across_the_pending_boundary() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // nnn（読み んn、末尾 n が pending）へ Literal(n)+Japanese(nn) を採用。
        /// nn→ん は完成済みの非1:1 unit なので途中で切らず、pending も空になる
        /// （原文字を残して次打鍵の原文字として消費させない）。次の a で
        /// source は nnna のまま nnn に縮まない。
        let mut module = InputModule::default();
        for ch in "nnn".chars() {
            module.handle(key(ch));
        }
        assert_eq!(module.canonical_reading(), "んn");
        let source = module.composition_source();
        assert_eq!(source.source_text(), "nnn");
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Literal, "n".to_string()),
                (SegmentKind::Japanese, "nn".to_string()),
            ],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "nん");
        module.handle(key('a'));
        assert_eq!(module.canonical_reading(), "nんあ");
        let source = module.composition_source();
        assert_eq!(source.source_text(), "nnna", "元打鍵が欠落しない");
    }

    #[test]
    fn mixed_adoption_does_not_take_suffix_side_unknown_as_pending() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // ky→Home→End→BS（不明の k を suffix 側へ再凍結）→ Home → s（pending）。
        /// pending の source は [0,1) で k は [1,2)（範囲外）。末尾が roman prefix
        /// でも pending へ戻さず、suffix の凍結を維持する。採用→End→BS→a で
        /// skあ（sか にならない）。
        let mut module = InputModule::default();
        for ch in "ky".chars() {
            module.handle(key(ch));
        }
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(2)).eaten);
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        module.handle(key('s'));
        assert_eq!(module.canonical_reading(), "sk");
        let source = module.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(2)).eaten);
        module.handle(key('a'));
        assert_eq!(
            module.canonical_reading(),
            "skあ",
            "suffix 側の k は pending へ戻らず、a は単独で あ になる"
        );
    }

    #[test]
    fn mixed_adoption_keeps_a_middle_pending_with_suffix_after_it() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // 同じセットアップで採用直後にその場で a を打つ: 本物の pending s は
        // pending のまま保持され（suffix k は範囲外として除外）、s+a が再合成
        // される（さk）。End を挟んだ上のテストと対で、middle pending の継承を固定する。
        let mut module = InputModule::default();
        for ch in "ky".chars() {
            module.handle(key(ch));
        }
        assert!(
            module
                .set_reading_cursor(ipc::clause::ReadingPosition(0))
                .eaten
        );
        assert!(
            module
                .set_reading_cursor(ipc::clause::ReadingPosition(2))
                .eaten
        );
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert!(
            module
                .set_reading_cursor(ipc::clause::ReadingPosition(0))
                .eaten
        );
        module.handle(key('s'));
        assert_eq!(module.canonical_reading(), "sk");
        let source = module.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection = mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "sk");
        module.handle(key('a'));
        assert_eq!(
            module.canonical_reading(),
            "さk",
            "pending s は pending のまま残り a と再合成される。suffix k は後ろに残る"
        );
    }

    #[test]
    fn mixed_adoption_inherits_partial_frozen_overlap_of_unknown_units() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // き（不明）+ k（不明のまま再凍結・再開可能）が 1 つの Unknown unit に
        // まとまるケース。凍結範囲との交差部分（k だけ）を再開可能へ登録する。
        /// BS→a で きか（きkあ にならない）。
        let mut module = InputModule::default();
        for ch in "kya".chars() {
            module.handle(key(ch));
        }
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        for ch in "ky".chars() {
            module.handle(key(ch));
        }
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(4)).eaten);
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(0)).eaten);
        assert!(module.set_reading_cursor(ipc::clause::ReadingPosition(2)).eaten);
        module.handle(key('i'));
        assert_eq!(module.canonical_reading(), "きkい");
        let source = module.composition_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection =
            mixed_input::projection::Projection::build(1, &source, &plan).unwrap();
        assert!(module.adopt_mixed_projection(&projection));
        assert_eq!(module.canonical_reading(), "きkい");
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(module.canonical_reading(), "きk");
        module.handle(key('a'));
        assert_eq!(module.canonical_reading(), "きか", "交差部分の k だけ再開可能を継承する");
    }

    #[test]
    fn caps_lock_kana_keeps_the_typed_case_in_the_composition_source() {
        use mixed_input::position::SourceRange;
        // CapsLock 相当のかな打鍵（ToUnicode が大文字を返し、読み合成は小文字へ
        // 正規化する経路）。読みは従来どおり小文字ローマ字のままで、元入力の
        // Typed だけが実打鍵の大文字を保存する。
        let mut module = InputModule::default();
        for (ch, original) in [('a', 'A'), ('p', 'P'), ('i', 'I')] {
            module.handle(InputEvent::Key(KeyEvent::Text {
                ch,
                style: TextStyle::Kana,
                replay: ReplayMode::Delta,
                original: Some(original),
            }));
        }
        assert_eq!(module.canonical_reading(), "あぴ");
        let source = module.composition_source();
        assert_eq!(source.source_text(), "API");
        assert_eq!(source.reading_text(), "あぴ");
        assert_eq!(source.original(SourceRange::new(0, 3)).as_deref(), Some("API"));
    }

    #[test]
    fn reading_adoption_advances_the_source_revision() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // 読みの取り込みはソースの読み・由来を変える。同じ plan_id +
        // source_revision で異なる対応表が存在しないよう、取り込みで世代が進む。
        // 旧世代の Projection は source_revision で stale と判定できる。
        let mut module = InputModule::default();
        module.handle(key('n'));
        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(1, 1, None).unwrap()
        else { unreachable!() };
        let before = module.composition_source();
        assert_eq!(before.source_text(), "n");
        assert_eq!(before.reading_text(), "n");
        assert_eq!(before.revision(), module.revision);

        assert!(module.adopt_conversion_reading("ん"));
        let after = module.composition_source();
        assert_eq!(after.source_text(), "n", "元入力は変わらない");
        assert_eq!(after.reading_text(), "ん", "読みは取り込みで変わる");
        assert_ne!(
            after.revision(),
            before.revision(),
            "取り込み前後のソースは別世代"
        );
        assert_eq!(after.revision(), module.revision);

        let plan_for = |source: &mixed_input::source::CompositionSource| {
            InterpretationPlan::build(
                &source.source_text(),
                &[(SegmentKind::Japanese, source.source_text())],
            )
            .unwrap()
        };
        let before_projection =
            mixed_input::projection::Projection::build(5, &before, &plan_for(&before)).unwrap();
        let after_projection =
            mixed_input::projection::Projection::build(5, &after, &plan_for(&after)).unwrap();
        assert_ne!(
            before_projection.source_revision, after_projection.source_revision,
            "同じ plan_id の Projection でも世代で区別できる"
        );
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                identity: snapshot.identity,
                text: "n".into(),
            })),
            ModuleOutput::default(),
            "取り込み前の世代の snapshot 結果は受け入れ口で拒否される"
        );

        // 同じ読みの再取り込みは no-op なので世代を進めない。
        let revision = module.revision;
        assert!(module.adopt_conversion_reading("ん"));
        assert_eq!(module.revision, revision);
    }

    #[test]
    fn tail_pending_freeze_by_cursor_move_advances_the_source_revision() {
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        // 末尾 pending を残したカーソル移動は、その実効由来を Kana → Direct 凍結へ
        // 替える。canonical reading が不変でも Projection の結果（nny → nんy）が
        // 変わるので、この移動だけ世代を進め、凍結しない純粋な移動は進めない。
        let mut module = InputModule::default();
        for ch in "nny".chars() { module.handle(key(ch)); }
        assert_eq!(module.canonical_reading(), "んy");
        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(1, 1, None).unwrap()
        else { unreachable!() };

        let before = module.composition_source();
        assert_eq!(before.revision(), module.revision);
        let plan_for = |source: &mixed_input::source::CompositionSource| {
            InterpretationPlan::build(
                &source.source_text(),
                &[
                    (SegmentKind::Literal, "n".to_string()),
                    (SegmentKind::Japanese, "ny".to_string()),
                ],
            )
            .unwrap()
        };
        let before_projection =
            mixed_input::projection::Projection::build(3, &before, &plan_for(&before)).unwrap();
        assert_eq!(before_projection.reading(), "nny");

        module.handle(InputEvent::Key(KeyEvent::ReadingHome));
        module.handle(InputEvent::Key(KeyEvent::ReadingEnd));

        let after = module.composition_source();
        assert_eq!(after.reading_text(), before.reading_text(), "canonical reading は不変");
        assert_ne!(
            after.revision(), before.revision(),
            "凍結を伴う移動は source 内容が変わるので世代が進む"
        );
        assert_eq!(after.revision(), module.revision);
        let after_projection =
            mixed_input::projection::Projection::build(3, &after, &plan_for(&after)).unwrap();
        assert_eq!(after_projection.reading(), "nんy", "凍結で再合成の境界が変わる");
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                identity: snapshot.identity,
                text: "んy".into(),
            })),
            ModuleOutput::default(),
            "凍結前の世代の snapshot 結果は受け入れ口で拒否される"
        );

        // pending のない純粋なカーソル移動は source 内容が不変なので世代を進めない。
        let revision = module.revision;
        module.handle(InputEvent::Key(KeyEvent::ReadingHome));
        assert_eq!(module.composition_source().revision(), revision);
    }

    #[test]
    fn interior_pending_input_cannot_combine_with_existing_suffix_on_replay() {
        let mut module = InputModule::default();
        for ch in "au".chars() { module.handle(key(ch)); }
        module.handle(InputEvent::Key(KeyEvent::MoveReading(-1)));
        module.handle(key('n'));
        assert_eq!(module.canonical_reading(), "あnう");
        assert_eq!(replayed_reading(&module.canonical_segments()), "あnう");
        module.handle(key('a'));
        assert_eq!(module.canonical_reading(), "あなう");
        assert_eq!(replayed_reading(&module.canonical_segments()), "あなう");
    }

    #[test]
    fn navigation_reissues_snapshot_without_reusing_its_identity() {
        let mut module = InputModule::default();
        module.handle(key('n'));
        let BackgroundIntent::LiveSnapshot { snapshot: old } = module.live_snapshot(1, 1, None).unwrap() else { unreachable!() };
        module.handle(InputEvent::Key(KeyEvent::ReadingHome));
        module.handle(InputEvent::Key(KeyEvent::ReadingEnd));
        let BackgroundIntent::LiveSnapshot { snapshot: new } = module.live_snapshot(1, 1, None).unwrap() else { unreachable!() };
        // 末尾 pending を凍結する Home→End は source の実効由来を替えるため、世代は
        // 編集と同様に進む（凍結しない純粋な移動は不変。tail_pending_freeze テスト参照）。
        // このテストの契約は request 単位での identity 再利用禁止であること。
        assert_ne!(old.identity.revision, new.identity.revision);
        assert_ne!(old.segments, new.segments);
        assert_ne!(old.identity.request, new.identity.request);
        assert_eq!(module.handle(InputEvent::Engine(EngineResult::LiveSnapshot { identity: old.identity, text: "ん".into() })), ModuleOutput::default());
        assert!(module.handle(InputEvent::Engine(EngineResult::LiveSnapshot { identity: new.identity, text: "n".into() })).immediate.is_some());
        module.next_snapshot_request = u64::MAX;
        assert!(module.live_snapshot(1, 1, None).is_none());
        assert!(module.explicit_snapshot(1, 1, None).is_none());
    }

    #[test]
    fn cursor_edits_reseed_full_reading_and_reject_the_old_live_snapshot() {
        use ipc::clause::ReadingPosition as P;
        let mut module = InputModule::default();
        for ch in "kyou".chars() { module.handle(key(ch)); }
        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(1, 1, None).unwrap() else { unreachable!() };
        let revision = module.revision;
        module.handle(InputEvent::Key(KeyEvent::MoveReading(-1)));
        assert_eq!(module.reading_cursor(), P(2));
        assert_eq!(module.revision, revision);
        assert!(module.live_snapshot(1, 1, None).is_none());
        assert_eq!(module.handle(InputEvent::Engine(EngineResult::LiveSnapshot { identity: snapshot.identity, text: "今日".into() })), ModuleOutput::default());
        for ch in "ka".chars() {
            let output = module.handle(key(ch));
            let Some(BackgroundIntent::Reseed { segments, .. }) = output.background else { panic!("cursor edit must reseed") };
            assert_eq!(replayed_reading(&segments), module.canonical_reading());
        }
        assert_eq!(module.canonical_reading(), "きょかう");
        assert_eq!(module.original_input(P(0), P(4)).as_deref(), Some("kyokau"));
        module.handle(InputEvent::Key(KeyEvent::Delete));
        assert_eq!(module.canonical_reading(), "きょか");
        module.handle(InputEvent::Key(KeyEvent::ReadingHome));
        let before = module.revision;
        let output = module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert!(output.eaten && output.immediate.is_none() && output.background.is_none());
        assert_eq!(module.revision, before);
        module.handle(InputEvent::Key(KeyEvent::ReadingEnd));
        assert_eq!(module.reading_cursor(), P(3));
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(module.canonical_reading(), "きょ");
    }

    #[test]
    fn reading_cursor_converts_scalar_position_to_utf16_without_splitting_surrogates() {
        let mut module = InputModule::default();
        for ch in "a😀b".chars() { module.handle(InputEvent::Key(KeyEvent::Text { ch, style: TextStyle::Direct, replay: ReplayMode::Full, original: None })); }
        assert_eq!(module.reading_cursor_utf16().0, 4);
        module.handle(InputEvent::Key(KeyEvent::MoveReading(-1)));
        assert_eq!(module.reading_cursor_utf16().0, 3);
        module.handle(InputEvent::Key(KeyEvent::MoveReading(-1)));
        assert_eq!(module.reading_cursor_utf16().0, 1);
        module.handle(InputEvent::Key(KeyEvent::Delete));
        assert_eq!(module.canonical_reading(), "ab");
        assert_eq!(module.reading_cursor_utf16().0, 1);
    }

    #[test]
    fn clause_prefix_removal_rejects_stale_snapshot_and_retains_future_input() {
        let mut module = InputModule::default();
        for ch in "kyouhaiitenkidesu".chars() { module.handle(key(ch)); }
        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(3, 7, None).unwrap()
        else { unreachable!() };
        assert!(module.retain_clause_reading_prefix("きゅう").is_none());
        assert!(module.retain_clause_reading_prefix("きょうはいい").is_some());
        assert_eq!(module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
            identity: snapshot.identity, text: "今日はいい天気です".into(),
        })), ModuleOutput::default());
        module.handle(key('a'));
        assert_eq!(module.canonical_reading(), "きょうはいいあ");
        assert_eq!(replayed_reading(&module.canonical_segments()), "きょうはいいあ");
    }

    #[test]
    fn deleted_invalid_romaji_recombines_in_display_and_replay() {
        for (keys, expected) in [
            ("dhyBBa", "だ"),
            ("dqBa", "だ"),
            ("kqBa", "か"),
            ("nyBya", "にゃ"),
            ("adhyBBa", "あだ"),
        ] {
            for replay in [ReplayMode::Delta, ReplayMode::Full] {
                let mut module = InputModule::default();
                for ch in keys.chars() {
                    let event = if ch == 'B' {
                        InputEvent::Key(KeyEvent::Backspace)
                    } else {
                        InputEvent::Key(KeyEvent::Text {
                            ch,
                            style: TextStyle::Kana,
                            replay,
                            original: None,
                        })
                    };
                    let output = module.handle(event);
                    if let Some(ImmediateOperation::SetPreedit { text }) = output.immediate {
                        assert_eq!(text, module.canonical_reading());
                    }
                    assert_eq!(
                        replayed_reading(&module.canonical_segments()),
                        module.canonical_reading()
                    );
                }
                assert_eq!(module.canonical_reading(), expected, "{keys}");
            }
        }
    }

    #[test]
    fn issue9_backspace_reopens_cursor_frozen_romaji_and_reseeds_kana_pending() {
        // Issue #9: カーソル移動で凍結した未確定ローマ字（こんn）へ削除で戻った後の打鍵は
        // n+i=ni で再結合する。再結合は engine reseed が pending を Kana style で送ることに
        // 依存するため、セグメント様式も固定する。
        let mut module = InputModule::default();
        for ch in "konnn".chars() {
            module.handle(key(ch));
        }
        assert_eq!(module.canonical_reading(), "こんn");
        module.handle(InputEvent::Key(KeyEvent::MoveReading(-1)));
        module.handle(InputEvent::Key(KeyEvent::ReadingEnd));
        module.handle(key('i'));
        assert_eq!(module.canonical_reading(), "こんnい", "凍結中の打鍵は結合しない");
        let deletion = module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(module.canonical_reading(), "こんn");
        // reopen した pending "n" は末尾 Kana ランへマージされた 1 セグメントとして
        // engine へ再送される（Direct 凍結のままなら Kana ランに溶け込まない）。
        match deletion.background {
            Some(BackgroundIntent::Reseed { segments, .. }) => assert_eq!(
                segments,
                vec![InputSegment { text: "こんn".into(), style: TextStyle::Kana }],
                "reopen した n は Kana style として engine へ再送される"
            ),
            other => panic!("expected reseed after backspace, got {other:?}"),
        }
        module.handle(key('i'));
        assert_eq!(module.canonical_reading(), "こんに");
        assert_eq!(
            replayed_reading(&module.canonical_segments()),
            module.canonical_reading()
        );
    }

    #[test]
    fn automatic_romaji_origin_survives_partial_commit_and_stale_snapshots() {
        let mut module = InputModule::default();
        for ch in "adhy".chars() {
            module.handle(key(ch));
        }
        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(3, 7, None).unwrap()
        else {
            unreachable!()
        };
        module.reseed_after_partial_commit("dhy");
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        module.handle(key('a'));
        assert_eq!(module.canonical_reading(), "だ");
        assert_eq!(replayed_reading(&module.canonical_segments()), "だ");
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                identity: snapshot.identity,
                text: "あdhy".into(),
            })),
            ModuleOutput::default()
        );
        let deletion = module.handle(InputEvent::Key(KeyEvent::Backspace));
        module.complete(deletion.immediate.as_ref().unwrap(), true);
        assert!(module.canonical_reading().is_empty());
        for ch in "da".chars() {
            module.handle(key(ch));
        }
        assert_eq!(module.canonical_reading(), "だ");
    }

    fn text(value: &str) -> String {
        value.to_string()
    }

    fn key(ch: char) -> InputEvent {
        InputEvent::Key(KeyEvent::Text {
            ch,
            style: TextStyle::Kana,
            replay: ReplayMode::Delta,
            original: None,
        })
    }

    fn direct_key(ch: char) -> InputEvent {
        InputEvent::Key(KeyEvent::Text {
            ch,
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        })
    }

    fn replayed_composer(
        segments: &[InputSegment],
    ) -> crate::local_kana_composer::LocalKanaComposer {
        let mut composer = crate::local_kana_composer::LocalKanaComposer::default();
        for segment in segments {
            let style = match segment.style {
                TextStyle::Kana => crate::local_kana_composer::InputStyle::Kana,
                TextStyle::Direct => crate::local_kana_composer::InputStyle::Direct,
            };
            for ch in segment.text.chars() {
                composer.push(ch, style);
            }
        }
        composer
    }

    fn replayed_reading(segments: &[InputSegment]) -> String {
        replayed_composer(segments).reading().to_owned()
    }

    fn apply_live_snapshot(module: &mut InputModule, text: &str) {
        let BackgroundIntent::LiveSnapshot { snapshot } =
            module.live_snapshot(3, 7, None).expect("snapshot")
        else {
            unreachable!()
        };
        let output = module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
            identity: snapshot.identity,
            text: text.into(),
        }));
        module.complete(output.immediate.as_ref().unwrap(), true);
        assert!(matches!(
            output.immediate,
            Some(ImmediateOperation::SetPreedit { .. })
        ));
    }

    #[test]
    fn single_kana_live_display_commits_hiragana_and_space_still_requests_conversion() {
        for (roman, reading) in [("ma", "ま"), ("ka", "か"), ("ga", "が"), ("la", "ぁ"), ("nn", "ん")] {
            let mut module = InputModule::default();
            for ch in roman.chars() { module.handle(key(ch)); }
            assert_eq!(module.canonical_reading(), reading);
            apply_live_snapshot(&mut module, reading);
            let space = module.handle(InputEvent::Key(KeyEvent::Space));
            assert!(matches!(space.background, Some(BackgroundIntent::Convert { .. })));
            let enter = module.handle(InputEvent::Key(KeyEvent::Enter));
            assert!(matches!(enter.immediate,
                Some(ImmediateOperation::Commit { text, candidate: None, .. }) if text == reading));
            assert!(enter.background.is_none());
        }
    }

    #[test]
    fn deleting_to_single_kana_rejects_delayed_multicharacter_live_results() {
        for publish_new_snapshot in [false, true] {
            let mut module = InputModule::default();
            for ch in "made".chars() { module.handle(key(ch)); }
            assert_eq!(module.canonical_reading(), "まで");
            apply_live_snapshot(&mut module, "間で");
            let BackgroundIntent::LiveSnapshot { snapshot: old } =
                module.live_snapshot(3, 7, None).unwrap() else { unreachable!() };
            let deletion = module.handle(InputEvent::Key(KeyEvent::Backspace));
            assert_eq!(module.canonical_reading(), "ま");
            assert_eq!(deletion.immediate, Some(ImmediateOperation::SetPreedit { text: "ま".into() }));
            if publish_new_snapshot { apply_live_snapshot(&mut module, "ま"); }
            assert_eq!(module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                identity: old.identity, text: "間で".into(),
            })), ModuleOutput::default());
            assert_eq!(module.handle(InputEvent::Engine(EngineResult::LiveAutoCommitProposal(
                AutoCommitProposal {
                    proposal: 1, identity: old.identity, text: "間".into(),
                    consumed_reading: "ま".into(), remaining: "で".into(),
                },
            ))), ModuleOutput::default());
            assert_eq!(module.immediate_display(), "ま");
            // Enter and mode-switch/cursor settle use the displayed text without a fresh conversion.
            assert_eq!(crate::input_state::plan_live_enter(None, &module.immediate_display(), module.canonical_reading()),
                crate::input_state::LiveEnterPlan::DirectCommit { text: "ま".into() });
            let enter = module.handle(InputEvent::Key(KeyEvent::Enter));
            assert!(matches!(enter.immediate,
                Some(ImmediateOperation::Commit { text, candidate: None, .. }) if text == "ま"));
            assert!(enter.background.is_none());
        }
    }

    #[test]
    fn single_kana_space_accepts_explicit_kanji_candidates() {
        let mut module = InputModule::default();
        for ch in "ma".chars() { module.handle(key(ch)); }
        apply_live_snapshot(&mut module, "ま");
        let space = module.handle(InputEvent::Key(KeyEvent::Space));
        let Some(BackgroundIntent::Convert { request }) = space.background else { unreachable!() };
        module.handle(InputEvent::Engine(EngineResult::Candidates {
            request, values: vec!["間".into(), "ま".into()],
        }));
        assert_eq!(module.candidates, vec!["間", "ま"]);
        let enter = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert!(matches!(enter.background, Some(BackgroundIntent::Commit { candidate: Some(0), .. })));
    }

    #[test]
    fn finalized_pending_n_uses_the_same_hiragana_live_commit_path() {
        let mut module = InputModule::default();
        module.handle(key('n'));
        assert_eq!(module.finalize_pending_n(), Some(true));
        apply_live_snapshot(&mut module, "ん");
        let enter = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert!(matches!(enter.immediate,
            Some(ImmediateOperation::Commit { text, candidate: None, .. }) if text == "ん"));
    }

    fn displayed(module: &mut InputModule, event: InputEvent) -> String {
        match module.handle(event).immediate {
            Some(ImmediateOperation::SetPreedit { text }) => text,
            other => panic!("unexpected immediate: {other:?}"),
        }
    }

    fn prepare_candidate_commit(
        module: &mut InputModule,
        values: Vec<String>,
        selected: usize,
    ) -> RequestId {
        module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values,
            selected,
            reason: CandidateReplacement::NewResult,
        }));
        match module.candidate_commit(None).background {
            Some(BackgroundIntent::Commit { request, .. }) => request,
            other => panic!("unexpected commit intent: {other:?}"),
        }
    }

    #[test]
    fn only_the_current_snapshot_identity_can_replace_local_preedit() {
        let mut module = InputModule::default();
        module.handle(key('n'));
        let BackgroundIntent::LiveSnapshot { snapshot } =
            module.live_snapshot(3, 7, None).expect("snapshot")
        else {
            unreachable!()
        };

        for stale in [
            SnapshotIdentity {
                revision: snapshot.identity.revision - 1,
                ..snapshot.identity
            },
            SnapshotIdentity {
                composition: snapshot.identity.composition + 1,
                ..snapshot.identity
            },
            SnapshotIdentity {
                connection_generation: snapshot.identity.connection_generation + 1,
                ..snapshot.identity
            },
            SnapshotIdentity {
                configuration_generation: snapshot.identity.configuration_generation + 1,
                ..snapshot.identity
            },
        ] {
            assert_eq!(
                module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                    identity: stale,
                    text: "古い".into(),
                })),
                ModuleOutput::default()
            );
        }
        assert_eq!(
            module
                .handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                    identity: snapshot.identity,
                    text: "ん".into(),
                }))
                .immediate,
            Some(ImmediateOperation::SetPreedit { text: "ん".into() })
        );
    }

    #[test]
    fn a_new_key_invalidates_a_delayed_snapshot_without_changing_local_kana() {
        let mut module = InputModule::default();
        module.handle(key('n'));
        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(1, 1, None).unwrap()
        else {
            unreachable!()
        };
        let local = module.handle(key('i'));
        assert_eq!(
            local.immediate,
            Some(ImmediateOperation::SetPreedit { text: "に".into() })
        );
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                identity: snapshot.identity,
                text: "二".into(),
            })),
            ModuleOutput::default()
        );
        assert_eq!(module.canonical_reading(), "に");
    }

    #[test]
    fn anchored_display_extends_new_keys_without_rewinding_to_kana() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        apply_live_snapshot(&mut module, "日本語");
        assert_eq!(displayed(&mut module, key('n')), "日本語n");
        // 撥音 n のかな確定は安定部を基準に比較するため、anchor を切らせない。
        assert_eq!(displayed(&mut module, key('a')), "日本語な");
    }

    #[test]
    fn rejected_live_display_does_not_replace_the_visible_anchor() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        for accepted in [false, true] {
            let BackgroundIntent::LiveSnapshot { snapshot } =
                module.live_snapshot(3, 7, None).unwrap()
            else {
                unreachable!()
            };
            let output = module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                identity: snapshot.identity,
                text: "日本語".into(),
            }));
            module.complete(output.immediate.as_ref().unwrap(), accepted);
            assert_eq!(
                module.immediate_display(),
                if accepted {
                    "日本語"
                } else {
                    "にほんご"
                }
            );
        }
        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(3, 7, None).unwrap()
        else {
            unreachable!()
        };
        let output = module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
            identity: snapshot.identity,
            text: "別の表記".into(),
        }));
        module.complete(output.immediate.as_ref().unwrap(), false);
        assert_eq!(displayed(&mut module, key('n')), "日本語n");
    }

    #[test]
    fn anchored_display_keeps_the_anchor_through_sokuon_and_youon() {
        let mut module = InputModule::default();
        for ch in "honn".chars() {
            module.handle(key(ch));
        }
        apply_live_snapshot(&mut module, "本");
        assert_eq!(displayed(&mut module, key('t')), "本t");
        // 促音: pending "tt" の先頭 t が っ へ確定しても anchor は維持される。
        assert_eq!(displayed(&mut module, key('t')), "本っt");
        assert_eq!(displayed(&mut module, key('u')), "本っつ");
        // 拗音: "kya" → きゃ の2文字確定でも anchor は維持される。
        assert_eq!(displayed(&mut module, key('k')), "本っつk");
        assert_eq!(displayed(&mut module, key('y')), "本っつky");
        assert_eq!(displayed(&mut module, key('a')), "本っつきゃ");
    }

    #[test]
    fn pre_llm_snapshot_stays_stale_after_llm_finishes() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        let BackgroundIntent::LiveSnapshot { snapshot } =
            module.live_snapshot(3, 7, None).expect("snapshot")
        else {
            unreachable!()
        };
        module.set_awaiting_llm(true);
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                identity: snapshot.identity,
                text: "日本語".into(),
            })),
            ModuleOutput::default()
        );
        module.set_awaiting_llm(false);
        assert_eq!(module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
            identity: snapshot.identity, text: "古い表層".into(),
        })), ModuleOutput::default());
        let BackgroundIntent::LiveSnapshot { snapshot } =
            module.live_snapshot(3, 7, None).expect("new snapshot")
        else { unreachable!() };
        let output = module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
            identity: snapshot.identity,
            text: "日本語".into(),
        }));
        module.complete(output.immediate.as_ref().unwrap(), true);
        assert!(matches!(
            output.immediate,
            Some(ImmediateOperation::SetPreedit { .. })
        ));
        assert_eq!(displayed(&mut module, key('n')), "日本語n");
    }

    #[test]
    fn an_empty_live_snapshot_drops_the_stale_anchor() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        apply_live_snapshot(&mut module, "日本語");
        apply_live_snapshot(&mut module, "");
        assert_eq!(displayed(&mut module, key('n')), "にほんごn");
    }

    #[test]
    fn a_snapshot_with_roman_pending_never_becomes_an_anchor() {
        let mut module = InputModule::default();
        module.handle(key('n'));
        module.handle(key('i'));
        module.handle(key('k')); // stable=にか, pending=k
        apply_live_snapshot(&mut module, "化");
        // 適用結果は表示されるが anchor にならない: 次打鍵はかな全体へ戻る。
        assert_eq!(displayed(&mut module, key('a')), "にか");
    }

    #[test]
    fn a_stale_snapshot_keeps_the_current_anchor() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        apply_live_snapshot(&mut module, "日本語");
        let stale = module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
            identity: SnapshotIdentity {
                revision: 999,
                request: 0,
                composition: 1,
                configuration_generation: 3,
                connection_generation: 7,
            },
            text: "古い".into(),
        }));
        assert_eq!(stale, ModuleOutput::default());
        assert_eq!(displayed(&mut module, key('n')), "日本語n");
    }

    #[test]
    fn backspace_notation_partial_commit_candidates_and_disconnect_drop_the_anchor() {
        // Backspace: anchor は削除単位の表面を知らないため保持しない。
        {
            let mut module = InputModule::default();
            for ch in "nihongo".chars() {
                module.handle(key(ch));
            }
            apply_live_snapshot(&mut module, "日本語");
            module.handle(InputEvent::Key(KeyEvent::Backspace));
            assert_eq!(displayed(&mut module, key('n')), "にほんn");
        }
        // 表記固定
        {
            let mut module = InputModule::default();
            for ch in "nihongo".chars() {
                module.handle(key(ch));
            }
            apply_live_snapshot(&mut module, "日本語");
            module.set_notation(crate::keymap::Notation::Hiragana);
            assert_eq!(displayed(&mut module, key('n')), "にほんごn");
        }
        // 部分確定の reseed: 残り読みに対応する表面が無い。
        {
            let mut module = InputModule::default();
            for ch in "nihongo".chars() {
                module.handle(key(ch));
            }
            apply_live_snapshot(&mut module, "日本語");
            module.reseed_after_partial_commit("ご");
            assert_eq!(displayed(&mut module, key('n')), "ごn");
        }
        // 候補表示
        {
            let mut module = InputModule::default();
            for ch in "nihongo".chars() {
                module.handle(key(ch));
            }
            apply_live_snapshot(&mut module, "日本語");
            let output = module.handle(InputEvent::Candidates(CandidateEvent::Replace {
                values: vec!["日本語".into(), "ニホンゴ".into()],
                selected: 0,
                reason: CandidateReplacement::NewResult,
            }));
            assert!(matches!(
                output.immediate,
                Some(ImmediateOperation::ShowCandidates { .. })
            ));
            assert_eq!(displayed(&mut module, key('n')), "にほんごn");
        }
        // エンジン切断
        {
            let mut module = InputModule::default();
            for ch in "nihongo".chars() {
                module.handle(key(ch));
            }
            apply_live_snapshot(&mut module, "日本語");
            module.handle(InputEvent::Engine(EngineResult::Disconnected {
                request: RequestId(1),
            }));
            assert_eq!(displayed(&mut module, key('n')), "にほんごn");
        }
    }

    #[test]
    fn a_fresh_stable_snapshot_rebuilds_the_anchor_after_invalidation() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        apply_live_snapshot(&mut module, "日本語");
        module.set_notation(crate::keymap::Notation::Hiragana);
        module.handle(key('n'));
        assert_eq!(displayed(&mut module, key('a')), "にほんごな");
        apply_live_snapshot(&mut module, "日本語な");
        assert_eq!(displayed(&mut module, key('n')), "日本語なn");
    }

    #[test]
    fn direct_ascii_extends_the_anchor_from_stable_not_pending() {
        let mut module = InputModule::default();
        for ch in "niho".chars() {
            module.handle(key(ch));
        }
        apply_live_snapshot(&mut module, "二歩");
        // Direct の ASCII は stable 側へ凍結されるため pending と誤認されない。
        assert_eq!(displayed(&mut module, direct_key('A')), "二歩A");
        assert_eq!(displayed(&mut module, key('n')), "二歩An");
    }

    #[test]
    fn without_an_anchor_new_keys_stay_canonical_kana() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        // snapshot 未適用(エンジン不在と同じ)では従来どおり正規かな。
        assert_eq!(displayed(&mut module, key('n')), "にほんごn");
    }

    #[test]
    fn auto_commit_proposal_requires_the_exact_revision_and_consumed_reading() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(1, 4, None).unwrap()
        else {
            unreachable!()
        };
        let proposal = AutoCommitProposal {
            proposal: 9,
            identity: snapshot.identity,
            text: "日本".into(),
            consumed_reading: "にほん".into(),
            remaining: "ご".into(),
        };

        let mut stale = proposal.clone();
        stale.identity.revision -= 1;
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveAutoCommitProposal(
                stale
            ))),
            ModuleOutput::default()
        );
        let mut wrong_range = proposal.clone();
        wrong_range.consumed_reading = "にほ".into();
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveAutoCommitProposal(
                wrong_range
            ))),
            ModuleOutput::default()
        );
        assert!(matches!(
            module
                .handle(InputEvent::Engine(EngineResult::LiveAutoCommitProposal(proposal)))
                .immediate,
            Some(ImmediateOperation::Commit {
                text,
                remaining: Some(remaining),
                ..
            }) if text == "日本" && remaining == "ご"
        ));
    }

    #[test]
    fn auto_commit_receipt_is_unique_and_only_follows_successful_apply() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(1, 4, None).unwrap()
        else {
            unreachable!()
        };
        let proposal = AutoCommitProposal {
            proposal: 9,
            identity: snapshot.identity,
            text: "日本".into(),
            consumed_reading: "にほん".into(),
            remaining: "ご".into(),
        };
        let rejected = module
            .handle(InputEvent::Engine(EngineResult::LiveAutoCommitProposal(
                proposal.clone(),
            )))
            .immediate
            .unwrap();
        module.complete(&rejected, false);
        assert_eq!(module.canonical_reading(), "にほんご");
        assert_eq!(module.take_auto_commit_receipt(), None);

        let applied = module
            .handle(InputEvent::Engine(EngineResult::LiveAutoCommitProposal(
                proposal.clone(),
            )))
            .immediate
            .unwrap();
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveAutoCommitProposal(
                proposal
            ))),
            ModuleOutput::default(),
            "a proposal already awaiting TSF cannot be applied twice"
        );
        module.complete(&applied, true);
        assert_eq!(module.canonical_reading(), "ご");
        assert_eq!(
            module.take_auto_commit_receipt(),
            Some(AutoCommitReceipt {
                proposal: 9,
                identity: snapshot.identity
            })
        );
        assert_eq!(module.take_auto_commit_receipt(), None);
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveAutoCommitProposal(
                AutoCommitProposal {
                    proposal: 9,
                    identity: snapshot.identity,
                    text: "日本".into(),
                    consumed_reading: "にほん".into(),
                    remaining: "ご".into(),
                }
            ))),
            ModuleOutput::default(),
            "an applied proposal cannot commit again after the journal advances"
        );
    }

    #[test]
    fn auto_commit_preserves_an_unfinished_roman_suffix_for_the_next_key() {
        for (before, consumed, remaining, after, expected) in [
            ("an", "あ", "n", "yuu", "にゅう"),
            ("ak", "あ", "k", "i", "き"),
            ("any", "あ", "ny", "a", "にゃ"),
            ("ash", "あ", "sh", "a", "しゃ"),
            ("at", "あ", "t", "a", "た"),
            ("gakk", "がっ", "k", "ou", "こう"),
        ] {
            let mut module = InputModule::default();
            for ch in before.chars() {
                module.handle(key(ch));
            }
            let BackgroundIntent::LiveSnapshot { snapshot } =
                module.live_snapshot(1, 4, None).unwrap()
            else {
                unreachable!()
            };
            let operation = module
                .handle(InputEvent::Engine(EngineResult::LiveAutoCommitProposal(
                    AutoCommitProposal {
                        proposal: 9,
                        identity: snapshot.identity,
                        text: "確定".into(),
                        consumed_reading: consumed.into(),
                        remaining: remaining.into(),
                    },
                )))
                .immediate
                .expect("the stable prefix is eligible for auto-commit");
            module.complete(&operation, true);
            for ch in after.chars() {
                module.handle(key(ch));
            }

            assert_eq!(module.canonical_reading(), expected, "before={before}");
            assert_eq!(
                replayed_reading(&module.canonical_segments()),
                expected,
                "before={before}"
            );
        }
    }

    #[test]
    fn auto_commit_reseed_preserves_frozen_ascii_styles() {
        let mut module = InputModule::default();
        for ch in "akq".chars() {
            module.handle(key(ch));
        }
        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(1, 4, None).unwrap()
        else {
            unreachable!()
        };
        let operation = module
            .handle(InputEvent::Engine(EngineResult::LiveAutoCommitProposal(
                AutoCommitProposal {
                    proposal: 9,
                    identity: snapshot.identity,
                    text: "亜".into(),
                    consumed_reading: "あ".into(),
                    remaining: "kq".into(),
                },
            )))
            .immediate
            .expect("the stable prefix is eligible for auto-commit");

        module.complete(&operation, true);

        assert_eq!(
            module.canonical_segments(),
            vec![
                InputSegment {
                    text: "k".into(),
                    style: TextStyle::Direct,
                },
                InputSegment {
                    text: "q".into(),
                    style: TextStyle::Kana,
                },
            ]
        );
    }

    #[test]
    fn fresh_direct_boundary_also_replays_without_recombination() {
        let mut module = InputModule::default();
        module.handle(key('n'));
        module.handle(direct_key('A'));
        for ch in "yu".chars() {
            module.handle(key(ch));
        }
        assert_eq!(module.canonical_reading(), "nAゆ");
        assert_eq!(
            replayed_reading(&module.canonical_segments()),
            module.canonical_reading(),
            "full replay of a fresh direct boundary must not recombine the frozen n"
        );
    }

    #[test]
    fn partial_commit_after_a_fresh_direct_boundary_keeps_the_latin_region() {
        let mut module = InputModule::default();
        module.handle(key('n'));
        module.handle(direct_key('A'));
        for ch in "yu".chars() {
            module.handle(key(ch));
        }
        apply_partial(&mut module, "Aゆ");
        assert!(module.latin_mode());
        // 残り "Aゆ" の latin 部は先頭の "A" のみ。境界は生 raw ドメインではなく
        // 作曲ジャーナルから計られる。
        assert_eq!(module.latin_from, Some(0));
        assert_eq!(module.canonical_reading(), "Aゆ");
    }

    #[test]
    fn direct_boundary_freezes_flushed_pending_for_replay() {
        let mut module = InputModule::default();
        for ch in "ny".chars() {
            module.handle(key(ch));
        }
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        module.handle(direct_key('A'));
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        for ch in "yu".chars() {
            module.handle(key(ch));
        }
        assert_eq!(module.canonical_reading(), "nゆ");
        assert_eq!(
            replayed_reading(&module.canonical_segments()),
            module.canonical_reading(),
            "engine replay must not recombine the n frozen at the direct boundary"
        );
    }

    #[test]
    fn backspace_reanchors_background_to_visible_units_and_future_keys() {
        for (before, after_backspace, next, after_next) in [
            ("ata", "あ", 'i', "あい"),
            ("sha", "し", 'a', "しあ"),
            ("kaki", "か", 'o', "かお"),
            ("ny", "n", 'a', "な"),
            ("kq", "k", 'a', "か"),
        ] {
            let mut module = InputModule::default();
            for ch in before.chars() {
                module.handle(key(ch));
            }
            let output = module.handle(InputEvent::Key(KeyEvent::Backspace));
            assert!(matches!(
                output.immediate,
                Some(ImmediateOperation::SetPreedit { ref text }) if text == after_backspace
            ));
            assert!(matches!(
                output.background,
                Some(BackgroundIntent::Reseed { .. })
            ));
            let BackgroundIntent::Insert { segments, .. } = module.background_reseed() else {
                unreachable!()
            };
            let mut replayed = replayed_composer(&segments);
            assert_eq!(replayed.reading(), after_backspace, "before={before}");

            module.handle(key(next));
            replayed.push(next, crate::local_kana_composer::InputStyle::Kana);
            assert_eq!(module.canonical_reading(), after_next, "before={before}");
            assert_eq!(replayed.reading(), after_next, "before={before}");
            assert_eq!(
                replayed_reading(&module.canonical_segments()),
                after_next,
                "before={before}"
            );
        }

        let mut direct = InputModule::default();
        for ch in ['x', 'e', '\u{301}'] {
            direct.handle(InputEvent::Key(KeyEvent::Text {
                ch,
                style: TextStyle::Direct,
                replay: ReplayMode::Delta,
                original: None,
            }));
        }
        let output = direct.handle(InputEvent::Key(KeyEvent::Backspace));
        assert!(matches!(
            output.background,
            Some(BackgroundIntent::Reseed { .. })
        ));
        let BackgroundIntent::Insert { segments, .. } = direct.background_reseed() else {
            unreachable!()
        };
        // P6/C2: U+0301 is a separate legal reading position; only kana
        // dakuten/handakuten attach to the preceding scalar in reading edits.
        assert_eq!(replayed_reading(&segments), "xe");
    }

    #[test]
    fn deleting_the_final_visible_unit_leaves_no_replay_material() {
        let mut module = InputModule::default();
        for ch in "ta".chars() {
            module.handle(key(ch));
        }

        let output = module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(output.immediate, Some(ImmediateOperation::Cancel));
        assert!(matches!(
            output.background,
            Some(BackgroundIntent::Reseed { .. })
        ));
        let BackgroundIntent::Insert { segments, .. } = module.background_reseed() else {
            unreachable!()
        };
        assert!(segments.is_empty());
    }

    #[test]
    fn partial_commit_then_backspace_preserves_the_visible_continuation() {
        let mut module = InputModule::default();
        for ch in "any".chars() {
            module.handle(key(ch));
        }
        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(1, 4, None).unwrap()
        else {
            unreachable!()
        };
        let operation = module
            .handle(InputEvent::Engine(EngineResult::LiveAutoCommitProposal(
                AutoCommitProposal {
                    proposal: 9,
                    identity: snapshot.identity,
                    text: "亜".into(),
                    consumed_reading: "あ".into(),
                    remaining: "ny".into(),
                },
            )))
            .immediate
            .unwrap();
        module.complete(&operation, true);

        let output = module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(
            output.immediate,
            Some(ImmediateOperation::SetPreedit { text: "n".into() })
        );
        let BackgroundIntent::Insert { segments, .. } = module.background_reseed() else {
            unreachable!()
        };
        assert_eq!(replayed_reading(&segments), "n");

        module.handle(key('a'));
        assert_eq!(module.canonical_reading(), "な");
        assert_eq!(
            replayed_reading(&module.canonical_segments()),
            module.canonical_reading()
        );
    }

    #[test]
    fn deleting_a_direct_suffix_after_a_canonical_replay_keeps_the_mode_boundary_at_the_end() {
        let mut module = InputModule::default();
        for ch in "ny".chars() {
            module.handle(key(ch));
        }
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        module.handle(direct('A'));

        module.handle(InputEvent::Key(KeyEvent::Backspace));

        assert!(module.latin_mode());
        // d2ea29f 以降、Direct 境界で追い出された pending n 自体も Direct リテラルとして
        // 凍結される。よって読み全体が latin 領域 = 境界は先頭に来る。
        assert_eq!(module.latin_from, Some(0));
        assert_eq!(module.canonical_reading(), "n");
    }

    #[test]
    fn rejected_cancel_invalidates_the_snapshot_even_after_reissuing_it() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(1, 4, None).unwrap()
        else {
            unreachable!()
        };
        let operation = module
            .handle(InputEvent::Key(KeyEvent::Escape))
            .immediate
            .unwrap();
        module.complete(&operation, false);
        let BackgroundIntent::LiveSnapshot {
            snapshot: replacement,
        } = module.live_snapshot(1, 4, None).unwrap()
        else {
            unreachable!()
        };

        assert_ne!(replacement.identity, snapshot.identity);

        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveAutoCommitProposal(
                AutoCommitProposal {
                    proposal: 9,
                    identity: snapshot.identity,
                    text: "日本".into(),
                    consumed_reading: "にほん".into(),
                    remaining: "ご".into(),
                }
            ))),
            ModuleOutput::default()
        );
    }

    #[test]
    fn space_waits_on_local_kana_and_accepts_only_explicit_candidates_for_that_revision() {
        let mut module = InputModule::default();
        for ch in "nihon".chars() {
            module.handle(key(ch));
        }
        let BackgroundIntent::LiveSnapshot { snapshot: live } =
            module.live_snapshot(1, 4, None).unwrap()
        else {
            unreachable!()
        };
        let BackgroundIntent::LiveSnapshot { snapshot: explicit } =
            module.explicit_snapshot(1, 4, None).unwrap()
        else {
            unreachable!()
        };
        assert_eq!(module.canonical_reading(), "にほn");
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                identity: live.identity,
                text: "日本".into(),
            })),
            ModuleOutput::default()
        );
        assert!(matches!(
            module
                .handle(InputEvent::Engine(EngineResult::ExplicitSnapshot {
                    identity: explicit.identity,
                    candidates: vec!["日本".into(), "二本".into()],
                }))
                .immediate,
            Some(ImmediateOperation::ShowCandidates {
                identity,
                values,
                selected: 0,
            }) if identity.composition == explicit.identity.composition
                && identity.revision == explicit.identity.revision
                && values == vec!["日本".to_string(), "二本".to_string()]
        ));
    }

    #[test]
    fn enter_commits_local_kana_without_waiting_for_pending_explicit_candidates() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        let BackgroundIntent::LiveSnapshot { snapshot } =
            module.explicit_snapshot(1, 4, None).unwrap()
        else {
            unreachable!()
        };

        let enter = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert_eq!(
            enter.immediate,
            Some(ImmediateOperation::Commit {
                text: "にほんご".into(),
                candidate: None,
                remaining: None,
                remaining_latin_from: None,
            })
        );
        module.complete(enter.immediate.as_ref().unwrap(), true);
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::ExplicitSnapshot {
                identity: snapshot.identity,
                candidates: vec!["日本語".into()],
            })),
            ModuleOutput::default()
        );
    }

    #[test]
    fn reversed_results_apply_only_the_newest_revision() {
        let mut module = InputModule::default();
        module.handle(key('n'));
        let BackgroundIntent::LiveSnapshot { snapshot: older } =
            module.live_snapshot(1, 4, None).unwrap()
        else {
            unreachable!()
        };
        module.handle(key('i'));
        let BackgroundIntent::LiveSnapshot { snapshot: newer } =
            module.live_snapshot(1, 4, None).unwrap()
        else {
            unreachable!()
        };
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                identity: older.identity,
                text: "二".into(),
            })),
            ModuleOutput::default()
        );
        assert_eq!(
            module
                .handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                    identity: newer.identity,
                    text: "荷".into(),
                }))
                .immediate,
            Some(ImmediateOperation::SetPreedit { text: "荷".into() })
        );
    }

    #[test]
    fn partial_reseed_notation_and_reset_invalidate_live_results() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(1, 1, None).unwrap()
        else {
            unreachable!()
        };
        module.reseed_after_partial_commit("ご");
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                identity: snapshot.identity,
                text: "日本語".into(),
            })),
            ModuleOutput::default()
        );

        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(1, 1, None).unwrap()
        else {
            unreachable!()
        };
        module.set_notation(crate::keymap::Notation::Katakana);
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                identity: snapshot.identity,
                text: "語".into(),
            })),
            ModuleOutput::default()
        );

        let BackgroundIntent::LiveSnapshot { snapshot } = module.live_snapshot(1, 1, None).unwrap()
        else {
            unreachable!()
        };
        module.reset();
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
                identity: snapshot.identity,
                text: "語".into(),
            })),
            ModuleOutput::default()
        );
    }

    fn direct(ch: char) -> InputEvent {
        InputEvent::Key(KeyEvent::Text {
            ch,
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        })
    }

    fn apply_partial(module: &mut InputModule, remaining: &str) {
        let request = prepare_candidate_commit(module, vec![text("日本")], 0);
        let output = module.handle(InputEvent::Engine(EngineResult::Commit {
            request,
            candidate: Some(0),
            resolved_text: text("日本"),
            outcome: EngineCommitOutcome::Applied {
                text: text("日本"),
                remaining: text(remaining),
            },
        }));
        let operation = output.immediate.expect("valid partial commit");
        module.complete(&operation, true);
    }

    #[test]
    fn partial_commit_preserves_kana_and_direct_remaining_segments() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        module.handle(direct('a'));
        module.handle(direct('i'));
        apply_partial(&mut module, "ごai");
        assert!(matches!(
            module.background_reseed(),
            BackgroundIntent::Insert { segments, .. }
                if segments == vec![
                    InputSegment { text: text("ご"), style: TextStyle::Kana },
                    InputSegment { text: text("ai"), style: TextStyle::Direct },
                ]
        ));
    }

    #[test]
    fn partial_commit_inside_direct_suffix_keeps_remaining_direct() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        module.handle(direct('a'));
        module.handle(direct('i'));
        apply_partial(&mut module, "i");
        assert!(matches!(
            module.background_reseed(),
            BackgroundIntent::Insert { segments, .. }
                if segments == vec![InputSegment { text: text("i"), style: TextStyle::Direct }]
        ));
    }

    #[test]
    fn partial_commit_with_empty_direct_suffix_preserves_latin_mode() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        module.handle(direct('a'));
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        apply_partial(&mut module, "ご");
        assert!(module.latin_mode());
        assert_eq!(module.latin_from, Some(module.raw.len()));
    }

    #[test]
    fn local_kana_is_immediate_and_survives_engine_disconnect() {
        let mut module = InputModule::default();
        let mut request = None;

        for ch in "nihongo".chars() {
            let output = module.handle(key(ch));
            assert!(output.eaten);
            assert!(matches!(
                output.background,
                Some(BackgroundIntent::Insert { .. })
            ));
            assert!(matches!(
                output.immediate,
                Some(ImmediateOperation::SetPreedit { .. })
            ));
            request = match output.background {
                Some(BackgroundIntent::Insert { request, .. }) => Some(request),
                _ => None,
            };
        }

        assert_eq!(module.canonical_reading(), "にほんご");
        let disconnected = module.handle(InputEvent::Engine(EngineResult::Disconnected {
            request: request.unwrap(),
        }));
        assert_eq!(
            disconnected.immediate,
            Some(ImmediateOperation::SetPreedit {
                text: "にほんご".into()
            })
        );
    }

    #[test]
    fn engine_reading_does_not_replace_canonical_local_kana() {
        let mut module = InputModule::default();
        let output = module.handle(key('a'));
        let request = match output.background {
            Some(BackgroundIntent::Insert { request, .. }) => request,
            other => panic!("unexpected insert output: {other:?}"),
        };

        let engine = module.handle(InputEvent::Engine(EngineResult::Reading {
            request,
            text: "亜".into(),
        }));

        assert_eq!(
            engine.immediate,
            Some(ImmediateOperation::SetPreedit { text: "あ".into() })
        );
        assert_eq!(module.canonical_reading(), "あ");
    }

    #[test]
    fn unfinished_suffix_direct_text_and_symbols_share_the_local_entrypoint() {
        let mut module = InputModule::default();
        module.handle(key('k'));
        let unfinished = module.handle(key('y'));
        assert!(unfinished.eaten);
        assert_eq!(
            unfinished.immediate,
            Some(ImmediateOperation::SetPreedit { text: "ky".into() })
        );

        let direct = module.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'A',
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        }));
        assert_eq!(
            direct.immediate,
            Some(ImmediateOperation::SetPreedit { text: "kyA".into() })
        );

        let symbol = module.handle(key('。'));
        assert_eq!(
            symbol.immediate,
            Some(ImmediateOperation::SetPreedit {
                text: "kyA。".into()
            })
        );
    }

    #[test]
    fn backspace_and_enter_keep_working_without_engine_results() {
        let mut module = InputModule::default();
        for ch in "nihong".chars() {
            module.handle(key(ch));
        }

        let backspace = module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert_eq!(
            backspace.immediate,
            Some(ImmediateOperation::SetPreedit {
                text: "にほん".into()
            })
        );
        assert!(matches!(
            backspace.background,
            Some(BackgroundIntent::Reseed { .. })
        ));

        let enter = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert_eq!(
            enter.immediate,
            Some(ImmediateOperation::Commit {
                text: "にほん".into(),
                candidate: None,
                remaining: None,
                remaining_latin_from: None,
            })
        );
        assert!(enter.background.is_none());
    }

    #[test]
    fn existing_typing_space_and_enter_are_observable_through_one_interface() {
        let mut module = InputModule::default();

        let key = module.handle(key('n'));
        assert!(key.eaten);
        let request = match key.background {
            Some(BackgroundIntent::Insert { request, segments }) => {
                assert_eq!(
                    segments,
                    vec![InputSegment {
                        text: text("n"),
                        style: TextStyle::Kana
                    }]
                );
                request
            }
            other => panic!("unexpected intent: {other:?}"),
        };

        let reading = module.handle(InputEvent::Engine(EngineResult::Reading {
            request,
            text: text("ん"),
        }));
        assert!(matches!(
            reading.immediate,
            Some(ImmediateOperation::SetPreedit { text, .. }) if text == "n"
        ));

        let convert = module.handle(InputEvent::Key(KeyEvent::Space));
        assert!(convert.eaten);
        assert!(matches!(
            convert.background,
            Some(BackgroundIntent::Convert { .. })
        ));

        let commit = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert!(commit.eaten);
        assert!(matches!(
            commit.immediate,
            Some(ImmediateOperation::Commit { text, .. }) if text == "n"
        ));
    }

    #[test]
    fn rejected_preedit_application_keeps_composition_for_a_retry() {
        let mut module = InputModule::default();
        let key = module.handle(key('a'));
        let request = match key.background.unwrap() {
            BackgroundIntent::Insert { request, .. } => request,
            other => panic!("unexpected intent: {other:?}"),
        };
        let result = module.handle(InputEvent::Engine(EngineResult::Reading {
            request,
            text: text("あ"),
        }));
        let operation = result.immediate.unwrap();
        module.complete(&operation, false);

        let enter = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert!(matches!(
            enter.immediate,
            Some(ImmediateOperation::Commit { text, .. }) if text == "あ"
        ));
    }

    #[test]
    fn reversed_engine_readings_cannot_rewind_the_canonical_preedit() {
        let mut module = InputModule::default();
        let first = module.handle(key('a'));
        let first_request = match first.background.unwrap() {
            BackgroundIntent::Insert { request, .. } => request,
            other => panic!("unexpected intent: {other:?}"),
        };
        let second = module.handle(key('i'));
        let second_request = match second.background.unwrap() {
            BackgroundIntent::Insert { request, .. } => request,
            other => panic!("unexpected intent: {other:?}"),
        };
        let mut engine = ScriptedEngine::default();
        engine.push(
            first_request,
            EngineResult::Reading {
                request: first_request,
                text: text("あ"),
            },
        );
        engine.push(
            second_request,
            EngineResult::Reading {
                request: second_request,
                text: text("あい"),
            },
        );

        let newest = module.handle(InputEvent::Engine(engine.take(second_request).unwrap()));
        assert!(
            matches!(newest.immediate, Some(ImmediateOperation::SetPreedit { text, .. }) if text == "あい")
        );
        let oldest = module.handle(InputEvent::Engine(engine.take(first_request).unwrap()));
        assert!(
            matches!(oldest.immediate, Some(ImmediateOperation::SetPreedit { text, .. }) if text == "あい")
        );
        assert_eq!(engine.take(RequestId(3)), None);
    }

    #[test]
    fn backspace_escape_and_lifecycle_keep_the_existing_eaten_contract() {
        let mut module = InputModule::default();
        assert!(!module.handle(InputEvent::Key(KeyEvent::Other)).eaten);
        assert!(!module.handle(InputEvent::Key(KeyEvent::Backspace)).eaten);

        module.handle(InputEvent::Lifecycle(LifecycleEvent::Activated));
        module.handle(key('a'));
        let backspace = module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert!(backspace.eaten);
        assert!(matches!(
            backspace.background,
            Some(BackgroundIntent::Reseed { .. })
        ));

        module.handle(key('i'));
        let escape = module.handle(InputEvent::Key(KeyEvent::Escape));
        let operation = match escape.immediate {
            Some(operation @ ImmediateOperation::Cancel) => operation,
            other => panic!("unexpected operation: {other:?}"),
        };
        module.complete(&operation, true);
        assert!(!module.handle(InputEvent::Key(KeyEvent::Enter)).eaten);

        module.handle(key('u'));
        let deactivated = module.handle(InputEvent::Lifecycle(LifecycleEvent::Deactivated));
        let operation = match deactivated.immediate {
            Some(operation @ ImmediateOperation::Cancel) => operation,
            other => panic!("unexpected operation: {other:?}"),
        };
        assert!(module.handle(InputEvent::Key(KeyEvent::Enter)).eaten);
        module.complete(&operation, true);
        assert!(!module.handle(InputEvent::Key(KeyEvent::Enter)).eaten);
    }

    #[test]
    fn final_backspace_empty_reading_requests_cancel_and_rejection_remains_retryable() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        let backspace = module.handle(InputEvent::Key(KeyEvent::Backspace));
        let request = match backspace.background {
            Some(BackgroundIntent::Reseed { request, .. }) => request,
            other => panic!("unexpected reseed intent: {other:?}"),
        };
        let result = module.handle(InputEvent::Engine(EngineResult::Reading {
            request,
            text: String::new(),
        }));
        let operation = match result.immediate {
            Some(operation @ ImmediateOperation::Cancel) => operation,
            other => panic!("unexpected empty-reading operation: {other:?}"),
        };

        module.complete(&operation, false);

        let retry = module.handle(InputEvent::Key(KeyEvent::Backspace));
        assert!(retry.eaten);
        assert!(matches!(
            retry.background,
            Some(BackgroundIntent::Reseed { .. })
        ));
    }

    #[test]
    fn final_backspace_disconnect_cancel_success_resets_composition() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        let backspace = module.handle(InputEvent::Key(KeyEvent::Backspace));
        let request = match backspace.background {
            Some(BackgroundIntent::Reseed { request, .. }) => request,
            other => panic!("unexpected reseed intent: {other:?}"),
        };
        let result = module.handle(InputEvent::Engine(EngineResult::Disconnected { request }));
        let operation = match result.immediate {
            Some(operation @ ImmediateOperation::Cancel) => operation,
            other => panic!("unexpected disconnected operation: {other:?}"),
        };

        module.complete(&operation, true);

        assert!(!module.handle(InputEvent::Key(KeyEvent::Enter)).eaten);
    }

    #[test]
    fn non_final_backspace_keeps_local_reading_when_engine_disagrees() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        module.handle(key('i'));
        let backspace = module.handle(InputEvent::Key(KeyEvent::Backspace));
        let request = match backspace.background {
            Some(BackgroundIntent::Reseed { request, .. }) => request,
            other => panic!("unexpected reseed intent: {other:?}"),
        };

        let result = module.handle(InputEvent::Engine(EngineResult::Reading {
            request,
            text: text("foreign"),
        }));

        assert!(matches!(
            result.immediate,
            Some(ImmediateOperation::SetPreedit { text }) if text == "あ"
        ));
    }

    #[test]
    fn partial_reseed_accepts_only_a_proper_suffix_copied_from_canonical_reading() {
        let mut module = InputModule::default();
        for ch in "kyouhaame".chars() {
            module.handle(key(ch));
        }

        assert_eq!(module.validate_partial_reseed("あめ"), Some(text("あめ")));
        assert_eq!(module.validate_partial_reseed("foreign"), None);
        assert_eq!(module.validate_partial_reseed("きょうはあめ"), None);
        assert_eq!(module.validate_partial_reseed(""), None);
        assert_eq!(module.canonical_reading(), "きょうはあめ");

        let mut unfinished = InputModule::default();
        for ch in "any".chars() {
            unfinished.handle(key(ch));
        }
        assert_eq!(unfinished.validate_partial_reseed("ny"), Some(text("ny")));
        assert_eq!(unfinished.validate_partial_reseed("y"), None);
        assert_eq!(unfinished.canonical_reading(), "あny");
    }

    #[test]
    fn invalid_partial_engine_result_emits_no_commit_and_keeps_canonical_composition() {
        let mut module = InputModule::default();
        for ch in "kyouhaame".chars() {
            module.handle(key(ch));
        }
        let request = prepare_candidate_commit(&mut module, vec![text("今日は")], 0);
        let output = module.handle(InputEvent::Engine(EngineResult::Commit {
            request,
            candidate: Some(0),
            resolved_text: text("今日は"),
            outcome: EngineCommitOutcome::Applied {
                text: text("今日は"),
                remaining: text("foreign"),
            },
        }));

        assert_eq!(output, ModuleOutput::default());
        assert_eq!(module.canonical_reading(), "きょうはあめ");
        module.handle(InputEvent::Candidates(CandidateEvent::Closed));
        let enter = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert!(matches!(
            enter.immediate,
            Some(ImmediateOperation::Commit { text, .. }) if text == "きょうはあめ"
        ));
    }

    #[test]
    fn candidate_preview_and_successful_commit_are_reported_as_display_operations() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        let convert = module.handle(InputEvent::Key(KeyEvent::Space));
        let request = match convert.background {
            Some(BackgroundIntent::Convert { request }) => request,
            other => panic!("unexpected conversion intent: {other:?}"),
        };
        let preview = module.handle(InputEvent::Engine(EngineResult::Candidates {
            request,
            values: vec![text("亜"), text("阿")],
        }));
        assert!(matches!(
            preview.immediate,
            Some(ImmediateOperation::ShowCandidates { values, selected: 0, .. }) if values == vec![text("亜"), text("阿")]
        ));

        let moved = module.handle(InputEvent::Key(KeyEvent::MoveCandidate(1)));
        assert!(
            matches!(moved.immediate, Some(ImmediateOperation::SetPreedit { text, .. }) if text == "阿")
        );
        let selected = module.handle(InputEvent::Key(KeyEvent::SelectCandidate(1)));
        assert!(
            matches!(selected.immediate, Some(ImmediateOperation::SetPreedit { text, .. }) if text == "阿")
        );
        let committed = module.handle(InputEvent::Key(KeyEvent::Enter));
        let (request, candidate) = match committed.background {
            Some(BackgroundIntent::Commit {
                request,
                candidate,
                text: Some(surface),
                ..
            }) => {
                assert_eq!(surface, "阿");
                (request, candidate)
            }
            other => panic!("unexpected intent: {other:?}"),
        };
        let committed = module.handle(InputEvent::Engine(EngineResult::Commit {
            request,
            candidate,
            resolved_text: text("阿"),
            outcome: EngineCommitOutcome::Applied {
                text: text("阿"),
                remaining: String::new(),
            },
        }));
        let operation = match committed.immediate {
            Some(operation @ ImmediateOperation::Commit { .. }) => operation,
            other => panic!("unexpected operation: {other:?}"),
        };
        assert!(matches!(
            &operation,
            ImmediateOperation::Commit {
                text,
                candidate: Some(1),
                remaining: Some(remaining),
                ..
            } if text == "阿" && remaining.is_empty()
        ));
        module.complete(&operation, true);
        assert!(!module.handle(InputEvent::Key(KeyEvent::Enter)).eaten);
    }

    #[test]
    fn closing_candidates_makes_enter_commit_the_composition_not_a_stale_selection() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("亜"), text("阿")],
            selected: 0,
            reason: CandidateReplacement::NewResult,
        }));
        module.handle(InputEvent::Candidates(CandidateEvent::Closed));

        let enter = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert!(matches!(
            enter.immediate,
            Some(ImmediateOperation::Commit { text, .. }) if text == "あ"
        ));
    }

    #[test]
    fn partial_commit_reseed_clears_the_previous_candidate_selection() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        module.handle(key('i'));
        module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("亜"), text("阿")],
            selected: 0,
            reason: CandidateReplacement::NewResult,
        }));
        module.reseed_after_partial_commit("い");

        let enter = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert!(matches!(
            enter.immediate,
            Some(ImmediateOperation::Commit { text, .. }) if text == "い"
        ));
    }

    #[test]
    fn full_replay_intent_contains_the_exact_styled_segments_sent_to_the_engine() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        module.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'B',
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
            original: None,
        }));
        let output = module.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'C',
            style: TextStyle::Direct,
            replay: ReplayMode::Full,
            original: None,
        }));
        assert!(matches!(
            output.background,
            Some(BackgroundIntent::Insert { segments, .. })
                if segments == vec![
                    InputSegment { text: text("あ"), style: TextStyle::Kana },
                    InputSegment { text: text("BC"), style: TextStyle::Direct },
                ]
        ));
    }

    fn candidate_commit_signature(output: ModuleOutput) -> Option<(bool, usize, String)> {
        match output {
            ModuleOutput {
                eaten,
                background:
                    Some(BackgroundIntent::Commit {
                        candidate: Some(index),
                        text: Some(text),
                        ..
                    }),
                ..
            } => Some((eaten, index, text)),
            _ => None,
        }
    }

    fn module_with_second_candidate_selected() -> InputModule {
        let mut module = InputModule::default();
        module.handle(key('a'));
        module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("亜"), text("阿")],
            selected: 1,
            reason: CandidateReplacement::NewResult,
        }));
        module
    }

    #[test]
    fn enter_settle_and_behavior_finalize_share_the_current_candidate_commit() {
        let enter =
            module_with_second_candidate_selected().handle(InputEvent::Key(KeyEvent::Enter));
        let settle = resolve_current_flat_candidate(&mut module_with_second_candidate_selected());
        let behavior = resolve_current_flat_candidate(&mut module_with_second_candidate_selected());

        assert_eq!(
            candidate_commit_signature(enter),
            Some((true, 1, text("阿")))
        );
        assert_eq!(
            candidate_commit_signature(settle),
            Some((true, 1, text("阿")))
        );
        assert_eq!(
            candidate_commit_signature(behavior),
            Some((true, 1, text("阿")))
        );
    }

    #[test]
    fn absolute_candidate_commit_uses_that_exact_index_and_rejects_invalid_indices() {
        let absolute =
            resolve_absolute_flat_candidate(&mut module_with_second_candidate_selected(), 0);
        assert_eq!(
            candidate_commit_signature(absolute),
            Some((true, 0, text("亜")))
        );

        let mut populated = module_with_second_candidate_selected();
        assert_eq!(populated.candidate_commit(Some(9)), ModuleOutput::default());
        assert_eq!(
            InputModule::default().candidate_commit(None),
            ModuleOutput::default()
        );
    }

    #[test]
    fn actual_engine_commit_outcomes_expose_full_partial_and_fallback_material() {
        let mut module = InputModule::default();
        let request = prepare_candidate_commit(&mut module, vec![text("日本語")], 0);
        let full = module.handle(InputEvent::Engine(EngineResult::Commit {
            request,
            candidate: Some(0),
            resolved_text: text("日本語"),
            outcome: EngineCommitOutcome::Applied {
                text: text("日本語"),
                remaining: String::new(),
            },
        }));
        assert!(matches!(
            full.immediate,
            Some(ImmediateOperation::Commit {
                text,
                candidate: Some(0),
                remaining: Some(remaining),
                ..
            }) if text == "日本語" && remaining.is_empty()
        ));

        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }

        let request = prepare_candidate_commit(&mut module, vec![text("日本語"), text("日本")], 1);
        let partial = module.handle(InputEvent::Engine(EngineResult::Commit {
            request,
            candidate: Some(1),
            resolved_text: text("日本語"),
            outcome: EngineCommitOutcome::Applied {
                text: text("日本"),
                remaining: text("ご"),
            },
        }));
        assert!(matches!(
            partial.immediate,
            Some(ImmediateOperation::Commit {
                text,
                candidate: Some(1),
                remaining: Some(remaining),
                ..
            }) if text == "日本" && remaining == "ご"
        ));

        let fallback = module.handle(InputEvent::Engine(EngineResult::Commit {
            request: RequestId(3),
            candidate: None,
            resolved_text: text("にほんご"),
            outcome: EngineCommitOutcome::Fallback {
                text: text("にほんご"),
            },
        }));
        assert!(matches!(
            fallback.immediate,
            Some(ImmediateOperation::Commit {
                text,
                candidate: None,
                remaining: None,
                ..
            }) if text == "にほんご"
        ));
    }

    #[test]
    fn resolved_text_wins_for_full_and_fallback_but_partial_uses_engine_prefix() {
        let mut module = InputModule::default();
        let request = prepare_candidate_commit(&mut module, vec![text("表示候補")], 0);
        let candidate_full = module.handle(InputEvent::Engine(EngineResult::Commit {
            request,
            candidate: Some(0),
            resolved_text: text("表示候補"),
            outcome: EngineCommitOutcome::Applied {
                text: text("エンジン結果"),
                remaining: String::new(),
            },
        }));
        assert!(matches!(
            candidate_full.immediate,
            Some(ImmediateOperation::Commit { text, .. }) if text == "表示候補"
        ));

        let live_full = module.handle(InputEvent::Engine(EngineResult::Commit {
            request: RequestId(2),
            candidate: None,
            resolved_text: text("表示候補"),
            outcome: EngineCommitOutcome::Applied {
                text: text("エンジン結果"),
                remaining: String::new(),
            },
        }));
        assert!(matches!(
            live_full.immediate,
            Some(ImmediateOperation::Commit { text, .. }) if text == "表示候補"
        ));

        for ch in "anokori".chars() {
            module.handle(key(ch));
        }

        let request = prepare_candidate_commit(&mut module, vec![text("表示候補")], 0);
        let candidate_partial = module.handle(InputEvent::Engine(EngineResult::Commit {
            request,
            candidate: Some(0),
            resolved_text: text("表示候補"),
            outcome: EngineCommitOutcome::Applied {
                text: text("実確定"),
                remaining: text("のこり"),
            },
        }));
        assert!(matches!(
            candidate_partial.immediate,
            Some(ImmediateOperation::Commit { text, remaining: Some(remaining), .. })
                if text == "実確定" && remaining == "のこり"
        ));

        let live_partial = module.handle(InputEvent::Engine(EngineResult::Commit {
            request: RequestId(4),
            candidate: None,
            resolved_text: text("表示候補"),
            outcome: EngineCommitOutcome::Applied {
                text: text("実確定"),
                remaining: text("のこり"),
            },
        }));
        assert!(matches!(
            live_partial.immediate,
            Some(ImmediateOperation::Commit { text, remaining: Some(remaining), .. })
                if text == "実確定" && remaining == "のこり"
        ));

        let request = prepare_candidate_commit(&mut module, vec![text("表示候補")], 0);
        let fallback = module.handle(InputEvent::Engine(EngineResult::Commit {
            request,
            candidate: Some(0),
            resolved_text: text("表示候補"),
            outcome: EngineCommitOutcome::Fallback {
                text: text("fallback payload"),
            },
        }));
        assert!(matches!(
            fallback.immediate,
            Some(ImmediateOperation::Commit { text, .. }) if text == "表示候補"
        ));
    }

    #[test]
    fn disconnect_uses_canonical_kana_instead_of_engine_text_plus_raw_suffix() {
        let mut module = InputModule::default();
        let first = module.handle(key('n'));
        let first_request = match first.background.unwrap() {
            BackgroundIntent::Insert { request, .. } => request,
            other => panic!("unexpected intent: {other:?}"),
        };
        module.handle(InputEvent::Engine(EngineResult::Reading {
            request: first_request,
            text: text("ん"),
        }));
        let second = module.handle(key('a'));
        let second_request = match second.background.unwrap() {
            BackgroundIntent::Insert { request, .. } => request,
            other => panic!("unexpected intent: {other:?}"),
        };

        let degraded = module.handle(InputEvent::Engine(EngineResult::Disconnected {
            request: second_request,
        }));
        assert!(matches!(
            degraded.immediate,
            Some(ImmediateOperation::SetPreedit { text, .. }) if text == "な"
        ));
    }

    #[test]
    fn disconnect_drops_an_incompatible_multibyte_snapshot_without_panicking() {
        let mut module = InputModule::default();
        let first = module.handle(key('é'));
        let request = match first.background.unwrap() {
            BackgroundIntent::Insert { request, .. } => request,
            other => panic!("unexpected intent: {other:?}"),
        };
        module.handle(InputEvent::Engine(EngineResult::Reading {
            request,
            text: text("え"),
        }));
        module.handle(InputEvent::Key(KeyEvent::Backspace));
        let next = module.handle(key('x'));
        let request = match next.background.unwrap() {
            BackgroundIntent::Insert { request, .. } => request,
            other => panic!("unexpected intent: {other:?}"),
        };
        let degraded = module.handle(InputEvent::Engine(EngineResult::Disconnected { request }));
        assert!(
            matches!(degraded.immediate, Some(ImmediateOperation::SetPreedit { text, .. }) if text == "x")
        );
    }

    #[test]
    fn repeated_backspace_keeps_canonical_kana_as_enter_fallback_material() {
        let mut module = InputModule::default();
        for ch in "nihongo".chars() {
            module.handle(key(ch));
        }
        module.handle(InputEvent::Engine(EngineResult::Reading {
            request: RequestId(7),
            text: text("日本語"),
        }));
        for expected in ["にほん", "にほ", "に"] {
            module.handle(InputEvent::Key(KeyEvent::Backspace));
            let degraded = module.handle(InputEvent::Engine(EngineResult::Disconnected {
                request: RequestId(8),
            }));
            assert!(matches!(
                degraded.immediate,
                Some(ImmediateOperation::SetPreedit { text }) if text == expected
            ));
        }

        module.handle(key('d'));
        let degraded = module.handle(InputEvent::Engine(EngineResult::Disconnected {
            request: RequestId(9),
        }));
        let _resolved_text = match degraded.immediate {
            Some(ImmediateOperation::SetPreedit { text }) => text,
            other => panic!("unexpected degraded display: {other:?}"),
        };
        let enter = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert!(matches!(
            enter.immediate,
            Some(ImmediateOperation::Commit { text, .. }) if text == "にd"
        ));
    }

    #[test]
    fn synchronous_cancel_result_controls_deactivation_reset() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        let finish = module.handle(InputEvent::Lifecycle(LifecycleEvent::Deactivated));
        let operation = match finish.immediate.unwrap() {
            operation @ ImmediateOperation::Cancel => operation,
            other => panic!("unexpected operation: {other:?}"),
        };
        module.complete(&operation, false);
        assert!(module.handle(InputEvent::Key(KeyEvent::Enter)).eaten);
        module.complete(&operation, true);
        assert!(!module.handle(InputEvent::Key(KeyEvent::Enter)).eaten);
    }

    #[test]
    fn synchronous_partial_commit_reseeds_only_after_application_succeeds() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        module.handle(key('i'));
        module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("亜"), text("阿")],
            selected: 1,
            reason: CandidateReplacement::NewResult,
        }));
        let request = match module.candidate_commit(None).background {
            Some(BackgroundIntent::Commit { request, .. }) => request,
            other => panic!("unexpected commit intent: {other:?}"),
        };
        let operation = module
            .handle(InputEvent::Engine(EngineResult::Commit {
                request,
                candidate: Some(1),
                resolved_text: text("阿"),
                outcome: EngineCommitOutcome::Applied {
                    text: text("阿"),
                    remaining: text("い"),
                },
            }))
            .immediate
            .unwrap();

        module.complete(&operation, false);
        assert_eq!(
            candidate_commit_signature(module.handle(InputEvent::Key(KeyEvent::Enter))),
            Some((true, 1, text("阿")))
        );

        module.complete(&operation, true);
        let retry = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert!(matches!(
            retry.immediate,
            Some(ImmediateOperation::Commit { text, .. }) if text == "い"
        ));
    }

    #[test]
    fn synchronous_full_commit_resets_only_after_application_succeeds() {
        let mut module = module_with_second_candidate_selected();
        let request = match module.candidate_commit(None).background {
            Some(BackgroundIntent::Commit { request, .. }) => request,
            other => panic!("unexpected commit intent: {other:?}"),
        };
        let operation = module
            .handle(InputEvent::Engine(EngineResult::Commit {
                request,
                candidate: Some(1),
                resolved_text: text("阿"),
                outcome: EngineCommitOutcome::Applied {
                    text: text("別のエンジン表層"),
                    remaining: String::new(),
                },
            }))
            .immediate
            .unwrap();

        module.complete(&operation, false);
        assert_eq!(
            candidate_commit_signature(module.handle(InputEvent::Key(KeyEvent::Enter))),
            Some((true, 1, text("阿")))
        );

        module.complete(&operation, true);
        assert!(!module.handle(InputEvent::Key(KeyEvent::Enter)).eaten);
    }

    #[test]
    fn synchronous_behavior_abort_rejection_retains_state_and_success_resets_it() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        let operation = module
            .handle(InputEvent::Key(KeyEvent::Escape))
            .immediate
            .unwrap();
        assert_eq!(operation, ImmediateOperation::Cancel);

        module.complete(&operation, false);
        assert!(module.handle(InputEvent::Key(KeyEvent::Enter)).eaten);
        module.complete(&operation, true);
        assert!(!module.handle(InputEvent::Key(KeyEvent::Enter)).eaten);
    }

    #[test]
    fn saturated_background_mailbox_does_not_stop_local_kana_or_enter() {
        let (mailbox, _receiver) = crate::background_input::bounded_mailbox(1);
        let mut module = InputModule::default();

        for ch in "nihongo".chars() {
            let output = module.handle(key(ch));
            assert!(matches!(
                output.immediate,
                Some(ImmediateOperation::SetPreedit { .. })
            ));
            let _ = mailbox.try_push(output.background.unwrap());
        }

        let enter = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert_eq!(
            enter.immediate,
            Some(ImmediateOperation::Commit {
                text: text("にほんご"),
                candidate: None,
                remaining: None,
                remaining_latin_from: None,
            })
        );
    }

    #[test]
    fn local_key_processing_preserves_order_under_sustained_load() {
        let mut module = InputModule::default();
        let (mailbox, _receiver) = crate::background_input::bounded_mailbox(1);
        let mut latencies = Vec::with_capacity(10_000);
        let expected = "にほんごあいう".repeat(1_000);

        for ch in "nihongoaiu".repeat(1_000).chars() {
            let started = std::time::Instant::now();
            let output = module.handle(key(ch));
            let _ = mailbox.try_push(output.background.clone().unwrap());
            latencies.push(started.elapsed());
            assert!(matches!(
                output.immediate,
                Some(ImmediateOperation::SetPreedit { .. })
            ));
        }

        let enter = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert!(matches!(
            enter.immediate,
            Some(ImmediateOperation::Commit { text, .. }) if text == expected
        ));
        latencies.sort_unstable();
        assert!(latencies[9_899] < std::time::Duration::from_millis(8));
    }

    #[test]
    fn candidate_results_require_the_exact_conversion_request() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        let convert = module.handle(InputEvent::Key(KeyEvent::Space));
        let request = match convert.background {
            Some(BackgroundIntent::Convert { request }) => request,
            other => panic!("unexpected conversion intent: {other:?}"),
        };

        let stale = module.handle(InputEvent::Engine(EngineResult::Candidates {
            request: RequestId(request.0 - 1),
            values: vec![text("古い候補")],
        }));
        assert_eq!(stale, ModuleOutput::default());

        let current = module.handle(InputEvent::Engine(EngineResult::Candidates {
            request,
            values: vec![text("亜"), text("阿")],
        }));
        assert!(matches!(
            current.immediate,
            Some(ImmediateOperation::ShowCandidates { values, .. })
                if values == vec![text("亜"), text("阿")]
        ));
    }

    #[test]
    fn exact_revision_results_can_replace_candidates_before_interaction() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        let snapshot = match module.explicit_snapshot(1, 1, None).unwrap() {
            BackgroundIntent::LiveSnapshot { snapshot } => snapshot,
            other => panic!("unexpected snapshot intent: {other:?}"),
        };
        let classic = module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("亜"), text("阿")],
            selected: 0,
            reason: CandidateReplacement::NewResult,
        }));
        let classic_identity = match classic.immediate {
            Some(ImmediateOperation::ShowCandidates { identity, .. }) => identity,
            other => panic!("unexpected candidate display: {other:?}"),
        };

        let enhanced = module.handle(InputEvent::Engine(EngineResult::ExplicitSnapshot {
            identity: snapshot.identity,
            candidates: vec![text("あ"), text("亜")],
        }));
        assert!(matches!(
            enhanced.immediate,
            Some(ImmediateOperation::ShowCandidates {
                identity,
                values,
                selected: 0,
            }) if identity != classic_identity && values == vec![text("あ"), text("亜")]
        ));
    }

    #[test]
    fn interaction_rejects_a_duplicate_result_for_the_same_classic_request() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        let request = match module.handle(InputEvent::Key(KeyEvent::Space)).background {
            Some(BackgroundIntent::Convert { request }) => request,
            other => panic!("unexpected conversion intent: {other:?}"),
        };
        module.handle(InputEvent::Engine(EngineResult::Candidates {
            request,
            values: vec![text("亜"), text("阿")],
        }));
        module.handle(InputEvent::Key(KeyEvent::MoveCandidate(1)));

        let duplicate = module.handle(InputEvent::Engine(EngineResult::Candidates {
            request,
            values: vec![text("あ"), text("亜")],
        }));
        assert_eq!(duplicate, ModuleOutput::default());
        assert_eq!(
            candidate_commit_signature(module.handle(InputEvent::Key(KeyEvent::Enter))),
            Some((true, 1, text("阿")))
        );
    }

    #[test]
    fn keyboard_interaction_freezes_the_visible_candidates_against_late_results() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        let snapshot = match module.explicit_snapshot(1, 1, None).unwrap() {
            BackgroundIntent::LiveSnapshot { snapshot } => snapshot,
            other => panic!("unexpected snapshot intent: {other:?}"),
        };
        module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("亜"), text("阿")],
            selected: 0,
            reason: CandidateReplacement::NewResult,
        }));

        let moved = module.handle(InputEvent::Key(KeyEvent::MoveCandidate(1)));
        assert!(matches!(
            moved.immediate,
            Some(ImmediateOperation::SetPreedit { text }) if text == "阿"
        ));
        let late = module.handle(InputEvent::Engine(EngineResult::ExplicitSnapshot {
            identity: snapshot.identity,
            candidates: vec![text("あ"), text("亜")],
        }));
        assert_eq!(late, ModuleOutput::default());
        assert_eq!(
            candidate_commit_signature(module.handle(InputEvent::Key(KeyEvent::Enter))),
            Some((true, 1, text("阿")))
        );
    }

    #[test]
    fn pointer_selection_freezes_the_visible_candidates_against_late_results() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        let snapshot = match module.explicit_snapshot(1, 1, None).unwrap() {
            BackgroundIntent::LiveSnapshot { snapshot } => snapshot,
            other => panic!("unexpected snapshot intent: {other:?}"),
        };
        module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("亜"), text("阿")],
            selected: 0,
            reason: CandidateReplacement::NewResult,
        }));

        module.handle(InputEvent::Key(KeyEvent::SelectCandidate(1)));
        let late = module.handle(InputEvent::Engine(EngineResult::ExplicitSnapshot {
            identity: snapshot.identity,
            candidates: vec![text("あ"), text("亜")],
        }));
        assert_eq!(late, ModuleOutput::default());
        assert_eq!(
            candidate_commit_signature(module.handle(InputEvent::Key(KeyEvent::Enter))),
            Some((true, 1, text("阿")))
        );
    }

    #[test]
    fn enter_commit_is_bound_to_the_visible_candidate_result_identity() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        let shown = module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("亜"), text("阿")],
            selected: 1,
            reason: CandidateReplacement::NewResult,
        }));
        let shown_identity = match shown.immediate {
            Some(ImmediateOperation::ShowCandidates { identity, .. }) => identity,
            other => panic!("unexpected candidate display: {other:?}"),
        };

        let committed = module.handle(InputEvent::Key(KeyEvent::Enter));
        assert!(matches!(
            committed.background,
            Some(BackgroundIntent::Commit {
                candidate_result,
                candidate: Some(1),
                text: Some(text),
                ..
            }) if candidate_result == shown_identity && text == "阿"
        ));
    }

    #[test]
    fn a_new_input_revision_accepts_a_new_candidate_result_set() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("亜"), text("阿")],
            selected: 0,
            reason: CandidateReplacement::NewResult,
        }));
        module.handle(InputEvent::Key(KeyEvent::MoveCandidate(1)));

        module.handle(key('i'));
        let snapshot = match module.explicit_snapshot(1, 1, None).unwrap() {
            BackgroundIntent::LiveSnapshot { snapshot } => snapshot,
            other => panic!("unexpected snapshot intent: {other:?}"),
        };
        let replacement = module.handle(InputEvent::Engine(EngineResult::ExplicitSnapshot {
            identity: snapshot.identity,
            candidates: vec![text("愛"), text("藍")],
        }));
        assert!(matches!(
            replacement.immediate,
            Some(ImmediateOperation::ShowCandidates { values, selected: 0, .. })
                if values == vec![text("愛"), text("藍")]
        ));
    }

    #[test]
    fn user_driven_candidate_view_replacement_preserves_freeze_and_commit_identity() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        let classic_request = match module.handle(InputEvent::Key(KeyEvent::Space)).background {
            Some(BackgroundIntent::Convert { request }) => request,
            other => panic!("unexpected conversion intent: {other:?}"),
        };
        let snapshot = match module.explicit_snapshot(1, 1, None).unwrap() {
            BackgroundIntent::LiveSnapshot { snapshot } => snapshot,
            other => panic!("unexpected snapshot intent: {other:?}"),
        };
        module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("亜"), text("阿")],
            selected: 0,
            reason: CandidateReplacement::NewResult,
        }));
        module.handle(InputEvent::Key(KeyEvent::MoveCandidate(1)));

        let user_view = module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("移"), text("異")],
            selected: 1,
            reason: CandidateReplacement::UserDriven,
        }));
        let visible_identity = match user_view.immediate {
            Some(ImmediateOperation::ShowCandidates { identity, .. }) => identity,
            other => panic!("unexpected user-driven view: {other:?}"),
        };
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::Candidates {
                request: classic_request,
                values: vec![text("あ"), text("亜")],
            })),
            ModuleOutput::default()
        );
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::ExplicitSnapshot {
                identity: snapshot.identity,
                candidates: vec![text("あ"), text("亜")],
            })),
            ModuleOutput::default()
        );
        assert!(matches!(
            module.handle(InputEvent::Key(KeyEvent::Enter)).background,
            Some(BackgroundIntent::Commit {
                candidate_result,
                candidate: Some(1),
                text: Some(text),
                ..
            }) if candidate_result == visible_identity && text == "異"
        ));
    }

    #[test]
    fn candidate_commit_response_requires_the_pending_visible_result_and_is_single_use() {
        let mut module = module_with_second_candidate_selected();
        let commit = module.candidate_commit(None);
        let request = match commit.background {
            Some(BackgroundIntent::Commit { request, .. }) => request,
            other => panic!("unexpected commit intent: {other:?}"),
        };
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::Commit {
                request,
                candidate: None,
                resolved_text: text("偽装"),
                outcome: EngineCommitOutcome::Fallback {
                    text: text("偽装")
                },
            })),
            ModuleOutput::default()
        );
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::Commit {
                request: RequestId(request.0 + 1),
                candidate: Some(1),
                resolved_text: text("偽装"),
                outcome: EngineCommitOutcome::Fallback {
                    text: text("偽装")
                },
            })),
            ModuleOutput::default()
        );
        let response = EngineResult::Commit {
            request,
            candidate: Some(1),
            resolved_text: text("阿"),
            outcome: EngineCommitOutcome::Applied {
                text: text("阿"),
                remaining: String::new(),
            },
        };

        let accepted = module.handle(InputEvent::Engine(response.clone()));
        assert!(matches!(
            accepted.immediate,
            Some(ImmediateOperation::Commit { text, .. }) if text == "阿"
        ));
        assert_eq!(
            module.handle(InputEvent::Engine(response)),
            ModuleOutput::default()
        );
    }

    #[test]
    fn commit_without_pending_accepts_only_the_legacy_non_candidate_result() {
        let mut module = InputModule::default();
        let live = module.handle(InputEvent::Engine(EngineResult::Commit {
            request: RequestId(1),
            candidate: None,
            resolved_text: text("表示済み"),
            outcome: EngineCommitOutcome::Fallback {
                text: text("表示済み"),
            },
        }));
        assert!(matches!(
            live.immediate,
            Some(ImmediateOperation::Commit { text, .. }) if text == "表示済み"
        ));
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::Commit {
                request: RequestId(2),
                candidate: Some(0),
                resolved_text: text("候補"),
                outcome: EngineCommitOutcome::Fallback {
                    text: text("候補")
                },
            })),
            ModuleOutput::default()
        );
    }

    #[test]
    fn new_result_event_cannot_thaw_an_interacted_candidate_set() {
        let mut module = module_with_second_candidate_selected();
        let visible_identity = match module
            .handle(InputEvent::Candidates(CandidateEvent::Replace {
                values: vec![text("亜"), text("阿")],
                selected: 1,
                reason: CandidateReplacement::NewResult,
            }))
            .immediate
        {
            Some(ImmediateOperation::ShowCandidates { identity, .. }) => identity,
            other => panic!("unexpected initial result: {other:?}"),
        };
        module.handle(InputEvent::Key(KeyEvent::MoveCandidate(-1)));

        assert_eq!(
            module.handle(InputEvent::Candidates(CandidateEvent::Replace {
                values: vec![text("あ"), text("亜")],
                selected: 1,
                reason: CandidateReplacement::NewResult,
            })),
            ModuleOutput::default()
        );
        assert!(matches!(
            module.handle(InputEvent::Key(KeyEvent::Enter)).background,
            Some(BackgroundIntent::Commit {
                candidate_result,
                candidate: Some(0),
                text: Some(text),
                ..
            }) if candidate_result == visible_identity && text == "亜"
        ));
    }

    #[test]
    fn first_user_driven_view_starts_freeze_and_empty_view_preserves_it() {
        let mut module = InputModule::default();
        module.handle(key('a'));
        let snapshot = match module.explicit_snapshot(1, 1, None).unwrap() {
            BackgroundIntent::LiveSnapshot { snapshot } => snapshot,
            other => panic!("unexpected snapshot intent: {other:?}"),
        };
        module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("亜"), text("阿")],
            selected: 0,
            reason: CandidateReplacement::NewResult,
        }));
        let user_view = module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("移"), text("異")],
            selected: 1,
            reason: CandidateReplacement::UserDriven,
        }));
        let visible_identity = match user_view.immediate {
            Some(ImmediateOperation::ShowCandidates { identity, .. }) => identity,
            other => panic!("unexpected user-driven view: {other:?}"),
        };

        assert_eq!(
            module.handle(InputEvent::Candidates(CandidateEvent::Replace {
                values: Vec::new(),
                selected: 0,
                reason: CandidateReplacement::UserDriven,
            })),
            ModuleOutput::default()
        );
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::ExplicitSnapshot {
                identity: snapshot.identity,
                candidates: vec![text("あ"), text("亜")],
            })),
            ModuleOutput::default()
        );
        assert!(matches!(
            module.handle(InputEvent::Key(KeyEvent::Enter)).background,
            Some(BackgroundIntent::Commit {
                candidate_result,
                candidate: Some(1),
                text: Some(text),
                ..
            }) if candidate_result == visible_identity && text == "異"
        ));
    }

    #[test]
    fn replacing_candidates_or_advancing_revision_invalidates_an_old_commit_response() {
        fn response(request: RequestId) -> EngineResult {
            EngineResult::Commit {
                request,
                candidate: Some(1),
                resolved_text: text("阿"),
                outcome: EngineCommitOutcome::Applied {
                    text: text("阿"),
                    remaining: String::new(),
                },
            }
        }

        let mut replaced = module_with_second_candidate_selected();
        let request = match replaced.candidate_commit(None).background {
            Some(BackgroundIntent::Commit { request, .. }) => request,
            other => panic!("unexpected commit intent: {other:?}"),
        };
        replaced.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("あ"), text("亜")],
            selected: 0,
            reason: CandidateReplacement::UserDriven,
        }));
        assert_eq!(
            replaced.handle(InputEvent::Engine(response(request))),
            ModuleOutput::default()
        );

        let mut advanced = module_with_second_candidate_selected();
        let request = match advanced.candidate_commit(None).background {
            Some(BackgroundIntent::Commit { request, .. }) => request,
            other => panic!("unexpected commit intent: {other:?}"),
        };
        advanced.handle(key('i'));
        assert_eq!(
            advanced.handle(InputEvent::Engine(response(request))),
            ModuleOutput::default()
        );
    }

    #[test]
    fn pointer_selection_request_reaches_freeze_without_com_or_hwnd() {
        use std::cell::{Cell, RefCell};

        let shared = RefCell::new(crate::candidate_state::CandidateState::new());
        shared.borrow_mut().set(vec![text("亜"), text("阿")], 0);
        let dirty = Cell::new(false);
        let mut module = InputModule::default();
        module.handle(key('a'));
        let snapshot = match module.explicit_snapshot(1, 1, None).unwrap() {
            BackgroundIntent::LiveSnapshot { snapshot } => snapshot,
            other => panic!("unexpected snapshot intent: {other:?}"),
        };
        module.handle(InputEvent::Candidates(CandidateEvent::Replace {
            values: vec![text("亜"), text("阿")],
            selected: 0,
            reason: CandidateReplacement::NewResult,
        }));

        assert!(crate::candidate_state::request_selection(
            &shared, &dirty, 0
        ));
        if dirty.replace(false) {
            apply_presenter_candidate_selection(&mut module, shared.borrow().selected());
        }
        assert_eq!(
            module.handle(InputEvent::Engine(EngineResult::ExplicitSnapshot {
                identity: snapshot.identity,
                candidates: vec![text("あ"), text("亜")],
            })),
            ModuleOutput::default()
        );
        assert_eq!(
            candidate_commit_signature(module.handle(InputEvent::Key(KeyEvent::Enter))),
            Some((true, 0, text("亜")))
        );
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct ScriptedEngine {
    ready: std::collections::BTreeMap<u64, EngineResult>,
}

#[cfg(test)]
impl ScriptedEngine {
    pub(crate) fn push(&mut self, request: RequestId, result: EngineResult) {
        self.ready.insert(request.0, result);
    }

    pub(crate) fn take(&mut self, request: RequestId) -> Option<EngineResult> {
        self.ready.remove(&request.0)
    }
}
