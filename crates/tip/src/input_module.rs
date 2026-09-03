//! COM と IPC から独立した入力イベント境界。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotIdentity {
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
    },
    Backspace,
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

#[derive(Default)]
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
    composition: u64,
    revision: u64,
    expected_snapshot: Option<(SnapshotIdentity, SnapshotPurpose)>,
    pending_auto_commit: Option<AutoCommitReceipt>,
    auto_commit_receipt: Option<AutoCommitReceipt>,
    replay_from_canonical: bool,
    live_display_anchor: Option<LiveDisplayAnchor>,
}

impl std::ops::Deref for InputModule {
    type Target = crate::input_state::InputState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl std::ops::DerefMut for InputModule {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

impl InputModule {
    pub(crate) fn canonical_segments(&self) -> Vec<InputSegment> {
        self.replay_segments()
    }

    pub(crate) fn live_snapshot(
        &mut self,
        configuration_generation: u64,
        connection_generation: u64,
        left_context: Option<String>,
    ) -> Option<BackgroundIntent> {
        if !self.state.composing {
            return None;
        }
        let identity = SnapshotIdentity {
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
        let identity = SnapshotIdentity {
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

    pub(crate) fn set_notation(&mut self, notation: crate::keymap::Notation) {
        self.state.notation_fixed = Some(notation);
        self.invalidate_live_snapshot();
        self.invalidate_live_display();
    }

    pub(crate) fn invalidate_live_snapshot(&mut self) {
        self.expected_snapshot = None;
        self.pending_auto_commit = None;
    }

    /// Drops the live display anchor. Deliberately separate from
    /// invalidate_live_snapshot: every key press retires the pending snapshot,
    /// but the anchor must survive plain typing and die only on non-extension
    /// events (candidates, notation, partial commit, disconnect, ...).
    pub(crate) fn invalidate_live_display(&mut self) {
        self.live_display_anchor = None;
    }

    /// Immediate preedit text for a key press: the anchor text plus the local
    /// kana that extends it, so the converted part never flashes back to kana.
    fn immediate_display(&mut self) -> String {
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
            KeyEvent::Text { ch, style, replay } => {
                if !self.state.composing {
                    self.composition = self.composition.wrapping_add(1);
                    self.revision = 0;
                    self.replay_from_canonical = false;
                }
                self.clear_candidates();
                match style {
                    TextStyle::Kana => {
                        self.state.on_char(ch);
                        self.local_kana
                            .push(ch, crate::local_kana_composer::InputStyle::Kana);
                    }
                    TextStyle::Direct => {
                        self.state.on_char_latin(ch);
                        self.local_kana
                            .push(ch, crate::local_kana_composer::InputStyle::Direct);
                        // Direct 挿入で作曲ジャーナルにスタイル付きの境界が生じる。raw と
                        // latin_from からの再生は latin 境界より後のかな入力まで Direct 化して
                        // しまうため、以降の Full 再生は composer 側のセグメントを権威にする
                        // (打鍵ごとの Delta 送信は影響を受けない)。
                        self.replay_from_canonical = true;
                    }
                };
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
                    background: Some(BackgroundIntent::Insert { request, segments }),
                }
            }
            KeyEvent::Backspace if self.state.composing => {
                self.clear_candidates();
                self.invalidate_live_display();
                let keep_latin_mode = self.state.latin_mode();
                self.state.on_backspace();
                self.local_kana.backspace();
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
                if text.is_empty() {
                    // 空結果は読みフォールバック契約: 旧アンカーを残すと次打鍵が
                    // 空表示前の旧変換結果を継ぎ足してしまう。
                    self.invalidate_live_display();
                } else if pending.is_empty() {
                    self.live_display_anchor = Some(LiveDisplayAnchor {
                        reading: stable.to_owned(),
                        text: text.clone(),
                    });
                }
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

    fn text(value: &str) -> String {
        value.to_string()
    }

    fn key(ch: char) -> InputEvent {
        InputEvent::Key(KeyEvent::Text {
            ch,
            style: TextStyle::Kana,
            replay: ReplayMode::Delta,
        })
    }

    fn direct_key(ch: char) -> InputEvent {
        InputEvent::Key(KeyEvent::Text {
            ch,
            style: TextStyle::Direct,
            replay: ReplayMode::Delta,
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
        assert!(matches!(
            output.immediate,
            Some(ImmediateOperation::SetPreedit { .. })
        ));
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
    fn an_awaiting_llm_snapshot_is_rejected_until_llm_finishes() {
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
        // LLM 完了後は同一 identity でも受理され、anchor が構築される。
        module.set_awaiting_llm(false);
        let output = module.handle(InputEvent::Engine(EngineResult::LiveSnapshot {
            identity: snapshot.identity,
            text: "日本語".into(),
        }));
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
            ("kq", "k", 'a', "kあ"),
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
        assert_eq!(replayed_reading(&segments), "x");
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
        }));
        let output = module.handle(InputEvent::Key(KeyEvent::Text {
            ch: 'C',
            style: TextStyle::Direct,
            replay: ReplayMode::Full,
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
