//! The STA owns clause selection and surfaces; engine replies only supply calculations.
use ipc::clause::{
    ClauseCandidate, ClauseCandidatesRequest, ClauseCandidatesStatus, ClauseId, ClauseRequestKey,
    ClauseState, ClauseValidationError, PrecedingSurface, ReadingPosition, SnapshotClauseData,
    SnapshotIdentity, ClauseRange, ConvertClausesRequest,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationMode {
    Editing,
    Converting,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SurfaceSource {
    Reading,
    Candidate {
        token: String,
        explicitly_selected: bool,
    },
    LocalSurface,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalClause {
    pub id: ClauseId,
    pub start: ReadingPosition,
    pub end: ReadingPosition,
    pub surface: String,
    pub source: SurfaceSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CandidateWindow {
    Closed,
    Loading(ClauseRequestKey),
    Ready {
        clause: ClauseId,
        candidates: Vec<ClauseCandidate>,
        selected: usize,
    },
}

#[derive(Clone, Debug)]
struct IssuedCandidates {
    request: ClauseCandidatesRequest,
    deadline: Instant,
    conversion: Option<ConvertClausesRequest>,
    rebaseline_used: bool,
    advance: i32,
}

#[derive(Clone, Debug)]
struct CachedCandidates {
    start: ReadingPosition,
    end: ReadingPosition,
    preceding: Vec<PrecedingSurface>,
    candidates: Vec<ClauseCandidate>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalEditOutcome { Unchanged, Changed, ReadingChanged, Empty, Editing, Exhausted }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CancelStage { None, SelectedReading }

#[derive(Clone, Debug)]
pub(crate) struct ClauseRebaseline {
    original: IssuedCandidates,
    boundary: bool,
    identity: SnapshotIdentity,
    snapshot_request_id: u64,
    convert_request_id: u64,
}

impl ClauseRebaseline {
    pub fn original_key(&self) -> ClauseRequestKey { self.original.request.key }
    pub fn snapshot(&self, left_context: Option<String>) -> ipc::protocol::Request {
        ipc::protocol::Request::LiveSnapshot {
            composition: self.identity.composition,
            revision: self.identity.revision,
            configuration_generation: self.identity.configuration_generation,
            connection_generation: self.identity.connection_generation,
            conversion_revision: self.original.request.key.conversion_revision,
            request_id: self.snapshot_request_id,
            segments: vec![ipc::protocol::SnapshotSegment {
                text: self.original.request.reading.clone(), style: Some("direct".into()),
            }],
            explicit: true,
            live_search_width: None,
            left_context,
        }
    }

    pub fn deadline(&self) -> Instant { self.original.deadline }
}

#[derive(Clone, Debug)]
pub(crate) struct ClauseConversion {
    pub learning_identity: Option<ipc::client::EngineLearningIdentity>,
    pub mode: OperationMode,
    pub reading: String,
    pub identity: SnapshotIdentity,
    pub baseline: u64,
    pub revision: u64,
    pub clauses: Vec<LocalClause>,
    pub selected: usize,
    pub window: CandidateWindow,
    pub sentence_token: Option<String>,
    issued: HashMap<u64, IssuedCandidates>,
    cache: HashMap<ClauseId, CachedCandidates>,
    last_request_id: u64,
    next_clause_id: Option<u64>,
    boundary_request: Option<IssuedCandidates>,
    cancel_stage: CancelStage,
    pub user_driven: bool,
    pub editing_clause: Option<ClauseId>,
    notation_cycle: Option<(crate::keymap::Notation, ReadingPosition, ReadingPosition, String, u8)>,
}

impl ClauseConversion {
    pub fn from_snapshot(
        identity: SnapshotIdentity,
        baseline: u64,
        data: SnapshotClauseData,
        text: &str,
    ) -> Result<Self, ClauseValidationError> {
        data.validate(text)?;
        let clauses: Vec<LocalClause> = data
            .clauses
            .into_iter()
            .map(|c| LocalClause {
                id: c.id,
                start: c.reading_start,
                end: c.reading_end,
                surface: c.surface,
                source: match c.state {
                    ClauseState::Reading => SurfaceSource::Reading,
                    ClauseState::Converted => SurfaceSource::Candidate {
                        token: c.candidate_token.expect("validated converted token"),
                        explicitly_selected: false,
                    },
                },
            })
            .collect();
        let next_clause_id = clauses.iter().map(|clause| clause.id.0).max().unwrap_or(0).checked_add(1);
        Ok(Self {
            learning_identity: None,
            mode: OperationMode::Converting,
            reading: data.reading,
            identity,
            baseline,
            revision: data.conversion_revision,
            clauses,
            selected: 0,
            window: CandidateWindow::Closed,
            sentence_token: data.sentence_token,
            issued: HashMap::new(),
            cache: HashMap::new(),
            last_request_id: data.request_id,
            next_clause_id,
            boundary_request: None,
            cancel_stage: CancelStage::None,
            user_driven: false,
            editing_clause: None,
            notation_cycle: None,
        })
    }

    pub fn from_reading(identity: SnapshotIdentity, reading: String) -> Option<Self> {
        let end = ReadingPosition(u32::try_from(reading.chars().count()).ok()?);
        if end.0 == 0 { return None; }
        Some(Self {
            learning_identity: None, mode: OperationMode::Editing,
            clauses: vec![LocalClause { id: ClauseId(1), start: ReadingPosition(0), end,
                surface: reading.clone(), source: SurfaceSource::Reading }],
            reading, identity, baseline: 0, revision: 0, selected: 0,
            window: CandidateWindow::Closed, sentence_token: None,
            issued: HashMap::new(), cache: HashMap::new(), last_request_id: 0,
            next_clause_id: Some(2), boundary_request: None,
            cancel_stage: CancelStage::None, user_driven: true, notation_cycle: None, editing_clause: None,
        })
    }

    pub fn reset_notation_cycle(&mut self) { self.notation_cycle = None; }

    /// Freeze the applied live clauses on the first Space. A locally typed
    /// suffix remains a reading clause until the user converts that interval.
    pub fn promote_live_display(&self, identity: SnapshotIdentity, reading: &str, text: &str) -> Option<Self> {
        if self.identity.composition != identity.composition
            || self.identity.configuration_generation != identity.configuration_generation
            || self.identity.connection_generation != identity.connection_generation
            || self.identity.revision > identity.revision
            || self.clauses.iter().all(|clause| clause.source == SurfaceSource::Reading) { return None; }
        let suffix = reading.strip_prefix(&self.reading)?;
        if text != format!("{}{suffix}", self.text()) { return None; }
        let mut model = self.clone();
        if !suffix.is_empty() {
            let start = ReadingPosition(u32::try_from(self.reading.chars().count()).ok()?);
            let end = ReadingPosition(u32::try_from(reading.chars().count()).ok()?);
            if !ipc::clause::legal_boundaries(reading).ok()?.contains(&start) { return None; }
            let id = model.next_clause_id?;
            model.next_clause_id = Some(id.checked_add(1)?);
            model.clauses.push(LocalClause { id: ClauseId(id), start, end,
                surface: suffix.into(), source: SurfaceSource::Reading });
            model.reading = reading.into();
            model.sentence_token = None;
            model.revision = model.revision.checked_add(1)?;
        }
        model.identity = identity;
        model.mode = OperationMode::Converting;
        model.user_driven = true;
        Some(model)
    }

    pub fn begin_reading_edit(&mut self, index: usize) -> LocalEditOutcome {
        let Some(clause) = self.clauses.get(index) else { return LocalEditOutcome::Unchanged; };
        let Some(revision) = self.revision.checked_add(1) else { return LocalEditOutcome::Exhausted; };
        let surface = self.reading.chars().skip(clause.start.0 as usize).take((clause.end.0 - clause.start.0) as usize).collect();
        let clause = &mut self.clauses[index];
        clause.surface = surface;
        clause.source = SurfaceSource::Reading;
        self.editing_clause = Some(clause.id);
        self.selected = index;
        self.mode = OperationMode::Editing;
        self.revision = revision;
        self.cancel_stage = CancelStage::None;
        self.notation_cycle = None;
        self.user_driven = true;
        self.close_window();
        self.cache.clear();
        self.issued.clear();
        self.sentence_token = None;
        LocalEditOutcome::Changed
    }

    pub fn editing_range(&self) -> Option<(ReadingPosition, ReadingPosition)> {
        if self.mode != OperationMode::Editing { return None; }
        self.clauses.iter().find(|clause| Some(clause.id) == self.editing_clause).map(|clause| (clause.start, clause.end))
    }

    /// The input module changes only the active reading interval. All other
    /// surfaces stay local; absolute-offset changes retire their old tokens.
    pub fn replace_edited_reading(&mut self, reading: &str, reading_revision: u64) -> LocalEditOutcome {
        let Some((start, end)) = self.editing_range() else { return LocalEditOutcome::Unchanged; };
        let prefix: String = self.reading.chars().take(start.0 as usize).collect();
        let suffix: String = self.reading.chars().skip(end.0 as usize).collect();
        let Some(new_len) = u32::try_from(reading.chars().count()).ok() else { return LocalEditOutcome::Exhausted; };
        let old_len = self.reading.chars().count() as u32;
        let unchanged = start.0 + old_len - end.0;
        if new_len < unchanged || !reading.starts_with(&prefix) || !reading.ends_with(&suffix) { return LocalEditOutcome::Unchanged; }
        let new_end = ReadingPosition(start.0 + new_len - unchanged);
        let Ok(boundaries) = ipc::clause::legal_boundaries(reading) else { return LocalEditOutcome::Unchanged; };
        if !boundaries.contains(&start) || !boundaries.contains(&new_end) {
            // A combining dakuten newly attached across an interval edge makes
            // that neighbouring clause part of the edited reading as well.
            let index = self.clauses.iter().position(|clause| Some(clause.id) == self.editing_clause).unwrap();
            let first = if !boundaries.contains(&start) { index.saturating_sub(1) } else { index };
            let last = if !boundaries.contains(&new_end) { (index + 1).min(self.clauses.len() - 1) } else { index };
            if first == last { return LocalEditOutcome::Unchanged; }
            let mut draft = self.clone();
            let from = draft.clauses[first].start;
            let to = draft.clauses[last].end;
            let id = draft.clauses[first].id;
            let surface = draft.reading.chars().skip(from.0 as usize).take((to.0 - from.0) as usize).collect();
            draft.clauses.splice(first..=last, [LocalClause { id, start: from, end: to, surface, source: SurfaceSource::Reading }]);
            draft.selected = first;
            draft.editing_clause = Some(id);
            let result = draft.replace_edited_reading(reading, reading_revision);
            if matches!(result, LocalEditOutcome::ReadingChanged | LocalEditOutcome::Empty) { *self = draft; }
            return result;
        }
        let Some(revision) = self.revision.checked_add(1) else { return LocalEditOutcome::Exhausted; };
        if reading_revision <= self.identity.revision { return LocalEditOutcome::Unchanged; }
        let index = self.clauses.iter().position(|clause| Some(clause.id) == self.editing_clause).unwrap();
        let changed_ids = 1 + if new_len != old_len { self.clauses.len() - index - 1 } else { 0 };
        let Some(first_id) = self.next_clause_id else { return LocalEditOutcome::Exhausted; };
        let Some(next_id) = first_id.checked_add(changed_ids as u64) else { return LocalEditOutcome::Exhausted; };
        let delta = i64::from(new_len) - i64::from(old_len);
        let surface = reading.chars().skip(start.0 as usize).take((new_end.0 - start.0) as usize).collect();
        self.clauses[index] = LocalClause { id: ClauseId(first_id), start, end: new_end, surface, source: SurfaceSource::Reading };
        if delta != 0 {
            for (offset, clause) in self.clauses[index + 1..].iter_mut().enumerate() {
                clause.id = ClauseId(first_id + 1 + offset as u64);
                clause.start = ReadingPosition((i64::from(clause.start.0) + delta) as u32);
                clause.end = ReadingPosition((i64::from(clause.end.0) + delta) as u32);
                if matches!(clause.source, SurfaceSource::Candidate { .. }) { clause.source = SurfaceSource::LocalSurface; }
            }
        }
        self.editing_clause = Some(ClauseId(first_id));
        if new_end == start {
            self.clauses.remove(index);
            self.editing_clause = None;
            self.mode = OperationMode::Converting;
            self.selected = index.min(self.clauses.len().saturating_sub(1));
        }
        self.reading = reading.to_owned();
        self.identity.revision = reading_revision;
        self.revision = revision;
        self.next_clause_id = Some(next_id);
        self.cancel_stage = CancelStage::None;
        self.notation_cycle = None;
        self.close_window();
        self.cache.clear();
        self.issued.clear();
        self.sentence_token = None;
        if self.clauses.is_empty() { LocalEditOutcome::Empty } else { LocalEditOutcome::ReadingChanged }
    }

    pub fn text(&self) -> String {
        self.clauses.iter().map(|c| c.surface.as_str()).collect()
    }

    pub fn delete_display_tail(&mut self) -> LocalEditOutcome {
        let Some(last) = self.clauses.last() else { return LocalEditOutcome::Unchanged; };
        let Some(retained_bytes) = crate::grapheme::last_grapheme_start(&last.surface) else { return LocalEditOutcome::Unchanged; };
        let Some(revision) = self.revision.checked_add(1) else { return LocalEditOutcome::Exhausted; };
        let reading_revision = if retained_bytes == 0 {
            let Some(next) = self.identity.revision.checked_add(1) else { return LocalEditOutcome::Exhausted; };
            next
        } else { self.identity.revision };
        if retained_bytes == 0 {
            self.reading = self.reading.chars().take(last.start.0 as usize).collect();
            self.clauses.pop();
            self.selected = self.selected.min(self.clauses.len().saturating_sub(1));
        } else {
            let last = self.clauses.last_mut().unwrap();
            last.surface.truncate(retained_bytes);
            last.source = SurfaceSource::LocalSurface;
        }
        self.identity.revision = reading_revision;
        self.revision = revision;
        self.cancel_stage = CancelStage::None;
        self.user_driven = true;
        self.close_window();
        self.issued.clear();
        self.cache.clear();
        self.sentence_token = None;
        if self.clauses.is_empty() { LocalEditOutcome::Empty }
        else if retained_bytes == 0 { LocalEditOutcome::ReadingChanged }
        else { LocalEditOutcome::Changed }
    }

    pub fn escape(&mut self) -> LocalEditOutcome {
        if self.window != CandidateWindow::Closed { self.close_window(); return LocalEditOutcome::Changed; }
        if self.clauses.is_empty() { return LocalEditOutcome::Unchanged; }
        let Some(revision) = self.revision.checked_add(1) else { return LocalEditOutcome::Exhausted; };
        let all = self.cancel_stage == CancelStage::SelectedReading;
        for (index, clause) in self.clauses.iter_mut().enumerate() {
            if all || index == self.selected {
                clause.surface = self.reading.chars().skip(clause.start.0 as usize).take((clause.end.0 - clause.start.0) as usize).collect();
                clause.source = SurfaceSource::Reading;
            }
        }
        self.revision = revision;
        self.user_driven = true;
        self.issued.clear();
        self.cache.clear();
        self.sentence_token = None;
        self.cancel_stage = if all { CancelStage::None } else { CancelStage::SelectedReading };
        if all { self.editing_clause = None; self.mode = OperationMode::Editing; LocalEditOutcome::Editing } else { LocalEditOutcome::Changed }
    }

    /// Local boundary changes do not issue work or consume any reading.
    pub fn resize_selected(&mut self, direction: i32) -> LocalEditOutcome {
        if direction == 0 || self.mode != OperationMode::Converting { return LocalEditOutcome::Unchanged; }
        let Some(selected) = self.clauses.get(self.selected) else { return LocalEditOutcome::Unchanged; };
        let Ok(boundaries) = ipc::clause::legal_boundaries(&self.reading) else { return LocalEditOutcome::Unchanged; };
        let next_end = if direction > 0 {
            boundaries.into_iter().find(|position| *position > selected.end)
        } else {
            boundaries.into_iter().rev().find(|position| *position < selected.end && *position > selected.start)
        };
        let Some(next_end) = next_end else { return LocalEditOutcome::Unchanged; };
        let right = self.clauses.get(self.selected + 1);
        let tail_end = right.map_or(selected.end, |clause| clause.end);
        let suffix = next_end < tail_end;
        let Some(revision) = self.revision.checked_add(1) else { return LocalEditOutcome::Exhausted; };
        let Some(first_id) = self.next_clause_id else { return LocalEditOutcome::Exhausted; };
        let Some(next_id) = first_id.checked_add(1 + u64::from(suffix)) else { return LocalEditOutcome::Exhausted; };
        let make_reading = |id, start: ReadingPosition, end: ReadingPosition| LocalClause {
            id: ClauseId(id), start, end,
            surface: self.reading.chars().skip(start.0 as usize).take((end.0 - start.0) as usize).collect(),
            source: SurfaceSource::Reading,
        };
        let mut replacements = vec![make_reading(first_id, selected.start, next_end)];
        if suffix { replacements.push(make_reading(first_id + 1, next_end, tail_end)); }
        let end = self.selected + 1 + usize::from(right.is_some());
        self.clauses.splice(self.selected..end, replacements);
        self.next_clause_id = Some(next_id);
        self.cancel_stage = CancelStage::None;
        self.revision = revision;
        self.user_driven = true;
        self.window = CandidateWindow::Closed;
        self.boundary_request = None;
        self.issued.clear();
        self.cache.clear();
        self.sentence_token = None;
        LocalEditOutcome::Changed
    }

    pub fn boundary_loading(&self) -> bool { self.boundary_request.is_some() }

    pub fn open_boundary_conversion(&mut self, request_id: u64, now: Instant) -> Option<ConvertClausesRequest> {
        if self.mode != OperationMode::Converting || self.window != CandidateWindow::Closed
            || request_id <= self.last_request_id { return None; }
        let selected = self.clauses.get(self.selected)?;
        if selected.source != SurfaceSource::Reading { return None; }
        let ranges = self.clauses[self.selected..].iter().take_while(|clause| clause.source == SurfaceSource::Reading)
            .take(if self.editing_clause == Some(selected.id) { 1 } else { usize::MAX })
            .map(|clause| ClauseRange { id: clause.id, reading_start: clause.start, reading_end: clause.end }).collect();
        let request = ConvertClausesRequest {
            key: ClauseRequestKey { identity: self.identity, baseline: self.baseline, conversion_revision: self.revision,
                clause_id: selected.id, request_id },
            reading: self.reading.clone(), clauses: ranges, preceding_surfaces: self.preceding(self.selected),
        };
        request.validate().ok()?;
        self.last_request_id = request_id;
        self.window = CandidateWindow::Loading(request.key);
        self.cancel_stage = CancelStage::None;
        self.boundary_request = Some(IssuedCandidates {
            request: ClauseCandidatesRequest { key: request.key, reading: request.reading.clone(),
                reading_start: selected.start, reading_end: selected.end, preceding_surfaces: request.preceding_surfaces.clone(),
                include_prefix_candidates: true },
            conversion: Some(request.clone()), deadline: now + Duration::from_millis(1200), rebaseline_used: false, advance: 0,
        });
        self.user_driven = true;
        Some(request)
    }

    pub fn accept_boundary_conversion(&mut self, key: ClauseRequestKey, status: ipc::clause::ConvertClausesStatus,
        learning: Option<ipc::client::EngineLearningIdentity>, now: Instant) -> bool {
        let Some(pending) = self.boundary_request.as_ref() else { return false; };
        let Some(request) = pending.conversion.as_ref() else { return false; };
        if request.key != key || key.identity != self.identity || key.baseline != self.baseline
            || key.conversion_revision != self.revision || self.window != CandidateWindow::Loading(key) { return false; }
        if now >= pending.deadline { self.close_window(); return false; }
        let ipc::clause::ConvertClausesStatus::Ready { clauses } = status else {
            if !matches!(status, ipc::clause::ConvertClausesStatus::Pending) { self.close_window(); }
            return false;
        };
        let valid = request.validate_result(&clauses).is_ok() && learning.as_ref().is_some_and(|identity| {
            !identity.engine_epoch.is_empty() && self.learning_identity.as_ref().is_none_or(|old|
                old.engine_epoch == identity.engine_epoch && old.learning_generation <= identity.learning_generation)
        });
        let Some(start) = self.clauses.iter().position(|clause| clause.id == key.clause_id) else { self.close_window(); return false; };
        let current_ranges = self.clauses[start..].iter().take(request.clauses.len()).map(|clause|
            ClauseRange { id: clause.id, reading_start: clause.start, reading_end: clause.end }).collect::<Vec<_>>();
        let Some(revision) = self.revision.checked_add(1).filter(|_| valid && current_ranges == request.clauses) else {
            self.close_window(); return false;
        };
        if self.learning_identity != learning {
            self.invalidate_learning();
            self.learning_identity = learning;
        }
        for (target, clause) in self.clauses[start..].iter_mut().zip(clauses) {
            *target = LocalClause { id: clause.id, start: clause.reading_start, end: clause.reading_end,
                surface: clause.surface, source: match clause.state {
                    ClauseState::Reading => SurfaceSource::Reading,
                    ClauseState::Converted => SurfaceSource::Candidate { token: clause.candidate_token.expect("validated token"), explicitly_selected: false },
                } };
        }
        self.revision = revision;
        self.issued.clear();
        self.cache.clear();
        self.close_window();
        true
    }

    /// Reserve IDs from the same counter as every snapshot/candidate request.
    /// The old model remains authoritative until a validated initial response arrives.
    pub fn prepare_rebaseline(&mut self, key: ClauseRequestKey, snapshot_request_id: u64,
        convert_request_id: u64, now: Instant) -> Option<ClauseRebaseline> {
        let boundary = self.boundary_request.as_ref().is_some_and(|pending| pending.request.key == key);
        let original = if boundary { self.boundary_request.as_ref()? } else { self.issued.get(&key.request_id)? };
        if original.request.key != key || self.identity != key.identity || self.baseline != key.baseline
            || self.revision != key.conversion_revision || self.window != CandidateWindow::Loading(key)
            || original.rebaseline_used || now >= original.deadline || snapshot_request_id <= self.last_request_id
            || convert_request_id <= snapshot_request_id {
            return None;
        }
        let identity = SnapshotIdentity {
            connection_generation: self.identity.connection_generation.checked_add(1)?,
            ..self.identity
        };
        let attempt = ClauseRebaseline { original: original.clone(), boundary, identity,
            snapshot_request_id, convert_request_id };
        self.last_request_id = convert_request_id;
        Some(attempt)
    }

    /// Only the new baseline/identity and learning provenance change here.
    /// The caller must calculate the returned exact interval within the original deadline.
    pub fn accept_rebaseline(&mut self, attempt: &ClauseRebaseline, response: &ipc::protocol::Response,
        learning: ipc::client::EngineLearningIdentity, now: Instant) -> Option<ConvertClausesRequest> {
        let original = &attempt.original.request;
        let key = original.key;
        let issued = if attempt.boundary { self.boundary_request.as_ref()? } else { self.issued.get(&key.request_id)? };
        if issued.request != *original || issued.deadline != attempt.original.deadline
            || issued.conversion != attempt.original.conversion
            || now >= issued.deadline || self.identity != key.identity || self.baseline != key.baseline
            || self.revision != key.conversion_revision || self.reading != original.reading
            || self.window != CandidateWindow::Loading(key) || self.last_request_id != attempt.convert_request_id
            || !self.clauses.get(self.selected).is_some_and(|clause| clause.id == key.clause_id)
            || learning.engine_epoch.is_empty()
            || self.learning_identity.as_ref().is_some_and(|old| old.engine_epoch == learning.engine_epoch
                && old.learning_generation > learning.learning_generation) {
            return None;
        }
        let ipc::protocol::Response::SnapshotResult { composition, revision, configuration_generation,
            connection_generation, baseline, clause_data, text, auto_commit, .. } = response else { return None; };
        if (SnapshotIdentity { composition: *composition, revision: *revision,
            configuration_generation: *configuration_generation, connection_generation: *connection_generation }) != attempt.identity
            || *baseline == 0 || clause_data.request_id != attempt.snapshot_request_id
            || clause_data.conversion_revision != self.revision || clause_data.reading != self.reading
            || auto_commit.is_some() || clause_data.validate(text).is_err() {
            return None;
        }
        let request = ConvertClausesRequest {
            key: ClauseRequestKey { identity: attempt.identity, baseline: *baseline,
                request_id: attempt.convert_request_id, ..key },
            reading: self.reading.clone(),
            clauses: if attempt.boundary { attempt.original.conversion.as_ref()?.clauses.clone() }
                else { vec![ClauseRange { id: key.clause_id, reading_start: original.reading_start,
                    reading_end: original.reading_end }] },
            preceding_surfaces: original.preceding_surfaces.clone(),
        };
        request.validate().ok()?;
        self.invalidate_learning();
        self.identity = attempt.identity;
        self.baseline = *baseline;
        self.learning_identity = Some(learning);
        let mut pending = attempt.original.clone();
        pending.request.key = request.key;
        pending.conversion = Some(request.clone());
        pending.rebaseline_used = true;
        if attempt.boundary { self.boundary_request = Some(pending); }
        else { self.issued.insert(request.key.request_id, pending); }
        self.window = CandidateWindow::Loading(request.key);
        Some(request)
    }

    pub fn request_deadline(&self, key: ClauseRequestKey) -> Option<Instant> {
        if let Some(pending) = self.boundary_request.as_ref().filter(|pending| pending.request.key == key) { return Some(pending.deadline); }
        self.issued.get(&key.request_id).filter(|issued| issued.request.key == key).map(|issued| issued.deadline)
    }

    pub fn accept_rebased_conversion(&mut self, key: ClauseRequestKey,
        status: ipc::clause::ConvertClausesStatus, learning: Option<ipc::client::EngineLearningIdentity>,
        candidate_request_id: u64, now: Instant) -> Option<ClauseCandidatesRequest> {
        let pending = self.issued.get(&key.request_id)?.clone();
        let conversion = pending.conversion.as_ref()?;
        if conversion.key != key || self.identity != key.identity || self.baseline != key.baseline
            || self.revision != key.conversion_revision || self.window != CandidateWindow::Loading(key)
            || !self.clauses.get(self.selected).is_some_and(|clause| clause.id == key.clause_id) {
            return None;
        }
        let valid = now < pending.deadline && candidate_request_id > self.last_request_id
            && learning.is_some() && self.learning_identity == learning;
        let ipc::clause::ConvertClausesStatus::Ready { clauses } = status else {
            if !matches!(status, ipc::clause::ConvertClausesStatus::Pending) { self.end_request(key); }
            return None;
        };
        let Some(revision) = self.revision.checked_add(1).filter(|_| valid && conversion.validate_result(&clauses).is_ok()) else {
            self.end_request(key);
            return None;
        };
        // This calculation restores an expired candidate request. Keep the
        // visible surface as candidate 1; only selecting a candidate changes it.
        self.revision = revision;
        self.last_request_id = candidate_request_id;
        let mut next = pending;
        next.conversion = None;
        next.request.key = ClauseRequestKey { conversion_revision: revision, request_id: candidate_request_id, ..key };
        let request = next.request.clone();
        self.issued.remove(&key.request_id);
        self.issued.insert(candidate_request_id, next);
        self.window = CandidateWindow::Loading(request.key);
        Some(request)
    }

    /// Whole-reading correction has no engine candidate provenance.
    pub fn replace_whole_surface(&mut self, surface: String) -> bool {
        self.cancel_stage = CancelStage::None;
        let Some(revision) = self.revision.checked_add(1) else { return false; };
        let Some(first) = self.clauses.first() else { return false; };
        let id = first.id;
        self.clauses = vec![LocalClause {
            id, start: ReadingPosition(0), end: ReadingPosition(self.reading.chars().count() as u32),
            surface, source: SurfaceSource::LocalSurface,
        }];
        self.selected = 0;
        self.revision = revision;
        self.mode = OperationMode::Converting;
        self.user_driven = true;
        self.window = CandidateWindow::Closed;
        self.boundary_request = None;
        self.issued.clear();
        self.cache.clear();
        self.sentence_token = None;
        true
    }

    pub fn transform_kana(&mut self, start: ReadingPosition, end: ReadingPosition,
        kind: crate::keymap::Notation) -> bool {
        self.transform_notation(start, end, kind, None)
    }

    pub fn transform_notation(&mut self, start: ReadingPosition, end: ReadingPosition,
        kind: crate::keymap::Notation, original: Option<&str>) -> bool {
        self.cancel_stage = CancelStage::None;
        use crate::keymap::Notation;
        let Some(first) = self.clauses.iter().position(|c| c.start == start) else { return false; };
        let Some(last) = self.clauses.iter().position(|c| c.end == end) else { return false; };
        if first > last { return false; }
        let reading: String = self.reading.chars().skip(start.0 as usize)
            .take((end.0 - start.0) as usize).collect();
        let mut cycle = None;
        let (surface, source) = match kind {
            Notation::Hiragana => (reading, SurfaceSource::Reading),
            Notation::Katakana => (crate::input_state::to_katakana(&reading), SurfaceSource::LocalSurface),
            Notation::HankakuKana => (crate::input_state::to_hankaku_kana(&reading), SurfaceSource::LocalSurface),
            Notation::ZenkakuEisu | Notation::HankakuEisu => {
                let material = original.unwrap_or(&reading).to_owned();
                let step = self.notation_cycle.as_ref().filter(|(key, from, to, text, _)|
                    *key == kind && *from == start && *to == end && *text == material)
                    .map_or(0, |(_, _, _, _, step)| (step + 1) % 3);
                // Only ASCII width/case changes are valid without a complete journal.
                let mut first = true;
                let ascii: String = material.chars().map(|ch| {
                    let ch = match ch as u32 {
                        0x3000 => ' ',
                        0xFF01..=0xFF5E => char::from_u32(ch as u32 - 0xFEE0).unwrap(),
                        _ => ch,
                    };
                    if !ch.is_ascii_alphabetic() { return ch; }
                    let uppercase = step == 1 || (step == 2 && first);
                    first = false;
                    if uppercase { ch.to_ascii_uppercase() } else { ch.to_ascii_lowercase() }
                }).collect();
                let surface = if kind == Notation::ZenkakuEisu { crate::input_state::to_zenkaku_ascii(&ascii) } else { ascii };
                cycle = Some((kind, start, end, material, step));
                (surface, SurfaceSource::LocalSurface)
            }
        };
        let Some(revision) = self.revision.checked_add(1) else { return false; };
        let id = self.clauses[first].id;
        self.clauses.splice(first..=last, [LocalClause { id, start, end, surface, source }]);
        self.selected = first;
        self.revision = revision;
        self.notation_cycle = cycle;
        self.user_driven = true;
        if kind != Notation::Hiragana { self.mode = OperationMode::Converting; }
        self.window = CandidateWindow::Closed;
        self.boundary_request = None;
        self.issued.clear();
        // A changed preceding surface invalidates every following candidate list.
        let retained: Vec<_> = self.clauses[..first].iter().map(|c| c.id).collect();
        self.cache.retain(|id, _| retained.contains(id));
        true
    }

    /// Learning identity is independent of the visible composition. Never
    /// reconstruct a user's surfaces merely because their tokens expired.
    pub fn invalidate_learning(&mut self) {
        self.learning_identity = None;
        self.sentence_token = None;
        for clause in &mut self.clauses {
            if matches!(clause.source, SurfaceSource::Candidate { .. }) {
                clause.source = SurfaceSource::LocalSurface;
            }
        }
        self.issued.clear();
        self.cache.clear();
        self.window = CandidateWindow::Closed;
        self.boundary_request = None;
        self.user_driven = true;
    }

    pub fn refresh_learning_identity(&mut self, identity: ipc::client::EngineLearningIdentity) {
        if self.learning_identity.as_ref() == Some(&identity) { return; }
        if self.learning_identity.as_ref().is_some_and(|old| {
            old.engine_epoch != identity.engine_epoch || old.learning_generation > identity.learning_generation
        }) { return; }
        // Requests issued after invalidation still have their own deadlines and
        // verified response identity. Supplying metadata must not cancel Space.
        let issued = std::mem::take(&mut self.issued);
        let loading = match self.window { CandidateWindow::Loading(key) => Some(key), _ => None };
        self.invalidate_learning();
        self.issued = issued;
        if let Some(key) = loading { self.window = CandidateWindow::Loading(key); }
        self.learning_identity = Some(identity);
    }

    /// A generation-checked configuration acknowledgement is authoritative for
    /// this connection, including a new engine epoch after reconnecting.
    pub fn configured_learning_identity(&mut self, identity: Option<ipc::client::EngineLearningIdentity>) {
        let Some(identity) = identity else { self.invalidate_learning(); return; };
        if self.learning_identity.as_ref().is_some_and(|old| old.engine_epoch != identity.engine_epoch) {
            self.invalidate_learning();
        }
        self.refresh_learning_identity(identity);
    }

    pub fn close_window(&mut self) {
        self.boundary_request = None;
        if let CandidateWindow::Loading(key) = self.window {
            self.issued.remove(&key.request_id);
        }
        self.window = CandidateWindow::Closed;
        self.boundary_request = None;
    }

    pub fn move_clause(&mut self, offset: i32) {
        // Movement may retain an in-flight reply for cache, but never for the next window.
        self.window = CandidateWindow::Closed;
        self.boundary_request = None;
        self.user_driven = true;
        if !self.clauses.is_empty() {
            self.selected = (self.selected as i64 + i64::from(offset))
                .clamp(0, self.clauses.len() as i64 - 1) as usize;
        }
    }

    fn preceding(&self, index: usize) -> Vec<PrecedingSurface> {
        self.clauses[..index]
            .iter()
            .map(|c| PrecedingSurface {
                clause_id: c.id,
                reading_start: c.start,
                reading_end: c.end,
                surface: c.surface.clone(),
            })
            .collect()
    }

    fn open_ready(&mut self, candidates: Vec<ClauseCandidate>) {
        let clause = &self.clauses[self.selected];
        let selected = candidates
            .iter()
            .position(|c| c.surface == clause.surface && c.reading_end == clause.end)
            .unwrap_or(0);
        self.window = CandidateWindow::Ready {
            clause: clause.id,
            candidates,
            selected,
        };
    }

    /// The surface already shown is the first choice of a fresh list. Cache
    /// this order so closing/reopening the window does not reorder user choices.
    fn anchor_candidates(&self, id: ClauseId, candidates: &mut Vec<ClauseCandidate>) {
        let Some(clause) = self.clauses.iter().find(|c| c.id == id) else { return; };
        if clause.source == SurfaceSource::Reading { return; }
        let anchor = candidates.iter().position(|c| c.surface == clause.surface && c.reading_end == clause.end)
            .map(|index| candidates.remove(index))
            .unwrap_or_else(|| ClauseCandidate {
                surface: clause.surface.clone(),
                token: match &clause.source {
                    SurfaceSource::Candidate { token, .. } => token.clone(),
                    _ => String::new(), // Local-only entry; never sent as an engine token.
                },
                reading_start: clause.start,
                reading_end: clause.end,
            });
        candidates.retain(|c| c.surface != anchor.surface || c.reading_end != anchor.reading_end);
        candidates.insert(0, anchor);
    }

    /// A Space that opens the list also moves once. Preserve that intent across
    /// asynchronous replies (including a rebaseline), without replaying the key.
    pub fn cycle_candidate(&mut self, request_id: u64, direction: i32, now: Instant)
        -> Option<ClauseCandidatesRequest> {
        if matches!(self.window, CandidateWindow::Ready { .. }) {
            self.advance_candidate(direction);
            return None;
        }
        let request = self.open_candidates(request_id, now);
        if let Some(request) = &request {
            self.issued.get_mut(&request.key.request_id).unwrap().advance = direction;
        } else if matches!(self.window, CandidateWindow::Ready { .. }) {
            self.advance_candidate(direction);
        }
        request
    }

    /// Caller allocates IDs from the connection owner's counter shared with initial snapshots.
    pub fn open_candidates(
        &mut self,
        request_id: u64,
        now: Instant,
    ) -> Option<ClauseCandidatesRequest> {
        if self.mode != OperationMode::Converting
            || self.clauses.is_empty()
            || !matches!(self.window, CandidateWindow::Closed)
        {
            return None;
        }
        self.user_driven = true;
        let c = &self.clauses[self.selected];
        self.cancel_stage = CancelStage::None;
        let preceding = self.preceding(self.selected);
        if let Some(cached) = self.cache.get(&c.id) {
            if cached.start == c.start && cached.end == c.end && cached.preceding == preceding {
                self.open_ready(cached.candidates.clone());
                return None;
            }
        }
        if request_id <= self.last_request_id {
            return None;
        }
        self.last_request_id = request_id;
        let key = ClauseRequestKey {
            identity: self.identity,
            baseline: self.baseline,
            conversion_revision: self.revision,
            clause_id: c.id,
            request_id,
        };
        let request = ClauseCandidatesRequest {
            key,
            reading: self.reading.clone(),
            reading_start: c.start,
            reading_end: c.end,
            preceding_surfaces: preceding,
            include_prefix_candidates: true,
        };
        self.issued.insert(
            request_id,
            IssuedCandidates {
                request: request.clone(),
                deadline: now + Duration::from_millis(1200),
                conversion: None,
                rebaseline_used: false,
                advance: 0,
            },
        );
        self.window = CandidateWindow::Loading(key);
        Some(request)
    }

    pub fn accept_candidates_with_identity(
        &mut self,
        key: ClauseRequestKey,
        status: ClauseCandidatesStatus,
        identity: Option<ipc::client::EngineLearningIdentity>,
        now: Instant,
    ) -> bool {
        if let ClauseCandidatesStatus::Ready { candidates } = &status {
            let Some(issued) = self.issued.get(&key.request_id).cloned() else { return false; };
            if issued.conversion.is_some() { self.end_request(key); return false; }
            if issued.request.key != key || self.identity != key.identity
                || self.baseline != key.baseline || self.revision != key.conversion_revision {
                return false;
            }
            if now >= issued.deadline || issued.request.validate_candidates(candidates).is_err() {
                self.end_request(key);
                return false;
            }
            let Some(identity) = identity else {
                self.end_request(key);
                return false;
            };
            if self.learning_identity.as_ref().is_some_and(|old| {
                old.engine_epoch != identity.engine_epoch
                    || old.learning_generation > identity.learning_generation
            }) {
                self.end_request(key);
                return false;
            }
            if self.learning_identity.as_ref() != Some(&identity) {
                let loading = self.window == CandidateWindow::Loading(key);
                self.invalidate_learning();
                self.learning_identity = Some(identity);
                self.issued.insert(key.request_id, issued);
                if loading { self.window = CandidateWindow::Loading(key); }
            }
        }
        self.accept_candidates(key, status, now)
    }

    pub fn accept_candidates(
        &mut self,
        key: ClauseRequestKey,
        status: ClauseCandidatesStatus,
        now: Instant,
    ) -> bool {
        let Some(issued) = self.issued.get(&key.request_id) else {
            return false;
        };
        if issued.request.key != key
            || self.identity != key.identity
            || self.baseline != key.baseline
            || self.revision != key.conversion_revision
        {
            return false;
        }
        if now >= issued.deadline {
            self.end_request(key);
            return false;
        }
        if issued.conversion.is_some() && !matches!(status, ClauseCandidatesStatus::Unavailable { .. }) {
            self.end_request(key);
            return false;
        }
        match status {
            ClauseCandidatesStatus::Pending => false,
            ClauseCandidatesStatus::Unavailable { .. } => {
                self.end_request(key);
                false
            }
            ClauseCandidatesStatus::Ready { mut candidates } => {
                if issued.request.validate_candidates(&candidates).is_err() {
                    self.end_request(key);
                    return false;
                }
                let advance = issued.advance;
                let start = issued.request.reading_start;
                let end = issued.request.reading_end;
                let preceding = issued.request.preceding_surfaces.clone();
                self.anchor_candidates(key.clause_id, &mut candidates);
                self.cache.insert(
                    key.clause_id,
                    CachedCandidates {
                        start,
                        end,
                        preceding,
                        candidates: candidates.clone(),
                    },
                );
                self.issued.remove(&key.request_id);
                if self
                    .clauses
                    .get(self.selected)
                    .is_some_and(|c| c.id == key.clause_id)
                    && self.window == CandidateWindow::Loading(key)
                {
                    self.open_ready(candidates);
                    if advance != 0 { self.advance_candidate(advance); }
                    true
                } else {
                    false
                }
            }
        }
    }

    pub fn end_request(&mut self, key: ClauseRequestKey) {
        self.issued.remove(&key.request_id);
        if self.window == CandidateWindow::Loading(key) {
            self.window = CandidateWindow::Closed;
            self.boundary_request = None;
        }
    }

    pub fn expire(&mut self, now: Instant) {
        if self.boundary_request.as_ref().is_some_and(|pending| now >= pending.deadline) { self.close_window(); }
        let expired: Vec<_> = self
            .issued
            .values()
            .filter(|r| now >= r.deadline)
            .map(|r| r.request.key)
            .collect();
        for key in expired {
            self.end_request(key);
        }
    }

    pub fn select_candidate(&mut self, index: usize) -> bool {
        self.cancel_stage = CancelStage::None;
        let CandidateWindow::Ready {
            clause,
            candidates,
            ..
        } = &self.window
        else {
            return false;
        };
        let Some(candidate) = candidates.get(index) else {
            return false;
        };
        let Some(c) = self
            .clauses
            .get(self.selected)
            .filter(|c| c.id == *clause)
        else {
            return false;
        };
        let Some(revision) = self.revision.checked_add(1) else {
            return false;
        };
        // The list keeps its original interval while cycling. A shorter choice
        // splits off Reading; choosing a longer one replaces only that suffix.
        let Some(cached) = self.cache.get(clause) else { return false; };
        let end = cached.end;
        let Some(last) = self.clauses.iter().enumerate().skip(self.selected)
            .find_map(|(i, c)| (c.end == end).then_some(i)) else { return false; };
        let changed_boundary = c.end != candidate.reading_end;
        let mut replacement = c.clone();
        replacement.end = candidate.reading_end;
        replacement.surface = candidate.surface.clone();
        replacement.source = if candidate.token.is_empty() { SurfaceSource::LocalSurface }
            else { SurfaceSource::Candidate {
                token: candidate.token.clone(),
                explicitly_selected: true,
            } };
        let mut replacements = vec![replacement];
        let mut next_id = self.next_clause_id;
        if changed_boundary && candidate.reading_end < end {
            let Some(id) = next_id else { return false; };
            let Some(next) = id.checked_add(1) else { return false; };
            next_id = Some(next);
            let Some(surface) = ipc::clause::reading_slice(&self.reading, candidate.reading_end, end) else { return false; };
            replacements.push(LocalClause { id: ClauseId(id), start: candidate.reading_end, end,
                surface, source: SurfaceSource::Reading });
        }
        if changed_boundary { self.sentence_token = None; }
        if c.start == ReadingPosition(0) && candidate.reading_end.0 as usize == self.reading.chars().count() {
            self.sentence_token = (!candidate.token.is_empty()).then(|| candidate.token.clone());
        }
        let replace_end = if changed_boundary { last + 1 } else { self.selected + 1 };
        self.clauses.splice(self.selected..replace_end, replacements);
        self.next_clause_id = next_id;
        if let CandidateWindow::Ready { selected, .. } = &mut self.window { *selected = index; }
        self.revision = revision;
        self.user_driven = true;
        self.issued.clear();
        let retained: Vec<_> = self.clauses[..=self.selected]
            .iter()
            .map(|c| c.id)
            .collect();
        self.cache.retain(|id, _| retained.contains(id));
        true
    }

    pub fn advance_candidate(&mut self, direction: i32) -> bool {
        let CandidateWindow::Ready {
            candidates,
            selected,
            ..
        } = &self.window
        else {
            return false;
        };
        let index =
            (*selected as i64 + i64::from(direction)).rem_euclid(candidates.len() as i64) as usize;
        self.select_candidate(index)
    }

    pub fn page(&self) -> Option<(&[ClauseCandidate], usize)> {
        let CandidateWindow::Ready {
            candidates,
            selected,
            ..
        } = &self.window
        else {
            return None;
        };
        let start = selected / 9 * 9;
        Some((
            &candidates[start..(start + 9).min(candidates.len())],
            selected - start,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipc::clause::{ClauseUnavailableReason, WireClause};
    fn rebaseline_response(attempt: &ClauseRebaseline) -> ipc::protocol::Response {
        ipc::protocol::Response::SnapshotResult {
            composition: attempt.identity.composition,
            revision: attempt.identity.revision,
            configuration_generation: attempt.identity.configuration_generation,
            connection_generation: attempt.identity.connection_generation,
            baseline: 90,
            text: attempt.original.request.reading.clone(),
            clause_data: SnapshotClauseData::from_reading(attempt.original.request.reading.clone(),
                attempt.original.request.key.conversion_revision, attempt.snapshot_request_id),
            candidates: None, candidate_remaining: None, auto_commit: None,
        }
    }

    fn learning(epoch: &str, generation: u64) -> ipc::client::EngineLearningIdentity {
        ipc::client::EngineLearningIdentity { engine_epoch: epoch.into(), learning_generation: generation }
    }

    #[test]
    fn rebaseline_conversion_and_candidates_share_original_deadline_and_preserve_other_clauses() {
        let mut m = model();
        m.transform_kana(ReadingPosition(0), ReadingPosition(1), crate::keymap::Notation::Katakana);
        m.move_clause(1);
        let now = Instant::now();
        let old = m.cycle_candidate(10, 1, now).unwrap();
        let attempt = m.prepare_rebaseline(old.key, 11, 12, now).unwrap();
        let convert = m.accept_rebaseline(&attempt, &rebaseline_response(&attempt), learning("new", 1), now).unwrap();
        assert_eq!(m.text(), "ニ本");
        let converted = WireClause { id: ClauseId(2), reading_start: ReadingPosition(1), reading_end: ReadingPosition(2),
            surface: "歩".into(), state: ClauseState::Converted, candidate_token: Some("new-token".into()) };
        let next = m.accept_rebased_conversion(convert.key, ipc::clause::ConvertClausesStatus::Ready { clauses: vec![converted] },
            Some(learning("new", 1)), 13, now + Duration::from_millis(500)).unwrap();
        assert_eq!(m.text(), "ニ本", "rebaseline must retain the surface that anchors candidate 1");
        assert_eq!(m.clauses[0].source, SurfaceSource::LocalSurface);
        assert_eq!(next.preceding_surfaces[0].surface, "ニ");
        assert_eq!(next.key.conversion_revision, convert.key.conversion_revision + 1);
        assert_eq!(m.request_deadline(next.key), Some(attempt.deadline()));
        assert!(m.prepare_rebaseline(next.key, 14, 15, now).is_none());
        assert!(!m.accept_candidates(old.key, ready(&old, &["古い"]), now));
        assert!(m.accept_candidates_with_identity(next.key, ready(&next, &["歩", "補"]), Some(learning("new", 1)),
            now + Duration::from_millis(1199)));
        assert!(matches!(m.window, CandidateWindow::Ready { selected: 1, .. }));
        assert_eq!(m.page().unwrap().0[0].surface, "本");
        assert_eq!(m.text(), "ニ歩");
    }

    #[test]
    fn invalid_or_cancelled_rebaseline_conversion_does_not_replace_surface() {
        for defect in 0..5 {
            let mut m = model();
            let now = Instant::now();
            let old = m.open_candidates(10, now).unwrap();
            let attempt = m.prepare_rebaseline(old.key, 11, 12, now).unwrap();
            let convert = m.accept_rebaseline(&attempt, &rebaseline_response(&attempt), learning("new", 1), now).unwrap();
            let mut clause = WireClause { id: ClauseId(1), reading_start: ReadingPosition(0), reading_end: ReadingPosition(1),
                surface: "違".into(), state: ClauseState::Converted, candidate_token: Some("new-token".into()) };
            let mut received = now;
            let mut identity = Some(learning("new", 1));
            match defect {
                0 => m.close_window(),
                1 => m.move_clause(1),
                2 => received = attempt.deadline(),
                3 => clause.reading_end = ReadingPosition(2),
                _ => identity = Some(learning("other-engine", 1)),
            }
            assert!(m.accept_rebased_conversion(convert.key, ipc::clause::ConvertClausesStatus::Ready { clauses: vec![clause] },
                identity, 13, received).is_none());
            assert_eq!(m.text(), "日本");
        }
    }

    #[test]
    fn failed_rebaseline_chain_can_start_a_fresh_space_request() {
        let mut m = model();
        let now = Instant::now();
        let old = m.open_candidates(10, now).unwrap();
        let attempt = m.prepare_rebaseline(old.key, 11, 12, now).unwrap();
        let convert = m.accept_rebaseline(&attempt, &rebaseline_response(&attempt), learning("new", 1), now).unwrap();
        assert!(m.accept_rebased_conversion(convert.key, ipc::clause::ConvertClausesStatus::Unavailable {
            reason: ClauseUnavailableReason::Disconnected }, None, 13, now).is_none());
        assert_eq!(m.window, CandidateWindow::Closed);
        let retry_time = now + Duration::from_secs(2);
        let retry = m.open_candidates(14, retry_time).unwrap();
        let second = m.prepare_rebaseline(retry.key, 15, 16, retry_time).unwrap();
        assert_eq!(second.deadline(), retry_time + Duration::from_millis(1200));
        assert_eq!(m.text(), "日本");
    }

    #[test]
    fn rebaseline_preserves_local_partition_and_surfaces_instead_of_initial_snapshot() {
        for kind in [crate::keymap::Notation::Hiragana, crate::keymap::Notation::Katakana] {
            let mut m = model();
            m.learning_identity = Some(learning("old", 3));
            m.sentence_token = Some("old-sentence".into());
            assert!(m.transform_kana(ReadingPosition(0), ReadingPosition(1), kind));
            m.move_clause(1);
            let before = m.clone();
            let now = Instant::now();
            let original = m.open_candidates(10, now).unwrap();
            let attempt = m.prepare_rebaseline(original.key, 11, 12, now).unwrap();
            assert_eq!(attempt.deadline(), now + Duration::from_millis(1200));
            assert_eq!(attempt.snapshot(Some("前文".into())), ipc::protocol::Request::LiveSnapshot {
                composition: 1, revision: 2, configuration_generation: 3, connection_generation: 5,
                conversion_revision: before.revision, request_id: 11,
                segments: vec![ipc::protocol::SnapshotSegment { text: "にほ".into(), style: Some("direct".into()) }],
                explicit: true, live_search_width: None, left_context: Some("前文".into()),
            });
            let request = m.accept_rebaseline(&attempt, &rebaseline_response(&attempt), learning("new", 1), now).unwrap();
            assert_eq!(m.text(), before.text());
            assert_eq!(m.reading, before.reading);
            assert_eq!(m.clauses.len(), 2); // Initial response has one Reading clause.
            assert_eq!(m.selected, 1);
            assert_eq!(m.revision, before.revision);
            assert_eq!(m.mode, before.mode);
            for (actual, old) in m.clauses.iter().zip(&before.clauses) {
                assert_eq!((actual.id, actual.start, actual.end, &actual.surface), (old.id, old.start, old.end, &old.surface));
            }
            assert_eq!(m.clauses[0].source, before.clauses[0].source);
            assert_eq!(m.clauses[1].source, SurfaceSource::LocalSurface);
            assert_eq!(m.sentence_token, None);
            assert_eq!(m.baseline, 90);
            assert_eq!(m.identity.connection_generation, 5);
            assert_eq!(m.learning_identity, Some(learning("new", 1)));
            assert_eq!(request.clauses, vec![ClauseRange { id: ClauseId(2), reading_start: ReadingPosition(1), reading_end: ReadingPosition(2) }]);
            assert_eq!(request.preceding_surfaces, original.preceding_surfaces);
            assert_eq!(request.key.request_id, 12);
            assert!(request.validate().is_ok());
            assert_eq!(m.window, CandidateWindow::Loading(request.key));
        }
    }

    #[test]
    fn rebaseline_rejects_stale_or_malformed_snapshot_without_mutating_local_model() {
        for defect in 0..8 {
            let mut m = model();
            let now = Instant::now();
            let request = m.open_candidates(10, now).unwrap();
            let attempt = m.prepare_rebaseline(request.key, 11, 12, now).unwrap();
            let mut response = rebaseline_response(&attempt);
            let ipc::protocol::Response::SnapshotResult { composition, connection_generation, baseline,
                clause_data, text, auto_commit, .. } = &mut response else { unreachable!() };
            match defect {
                0 => *composition += 1,
                1 => *connection_generation -= 1,
                2 => *baseline = 0,
                3 => clause_data.request_id += 1,
                4 => clause_data.conversion_revision += 1,
                5 => { clause_data.reading = "かほ".into(); *text = "かほ".into(); },
                6 => *text = "不一致".into(),
                _ => *auto_commit = Some(ipc::protocol::AutoCommitProposal {
                    proposal: 1, text: "日".into(), consumed_reading: "に".into(), remaining: "ほ".into(),
                }),
            }
            let before = format!("{m:?}");
            assert!(m.accept_rebaseline(&attempt, &response, learning("new", 1), now).is_none(), "defect={defect}");
            assert_eq!(format!("{m:?}"), before);
        }
    }

    #[test]
    fn rebaseline_cannot_restore_cancelled_moved_or_timed_out_request() {
        for action in 0..4 {
            let mut m = model();
            let now = Instant::now();
            let original = m.open_candidates(10, now).unwrap();
            let attempt = m.prepare_rebaseline(original.key, 11, 12, now).unwrap();
            let received = match action {
                0 => { m.close_window(); now },
                1 => { m.move_clause(1); now },
                2 => { m.prepare_rebaseline(original.key, 13, 14, now).unwrap(); now },
                _ => attempt.deadline(),
            };
            let before = format!("{m:?}");
            assert!(m.accept_rebaseline(&attempt, &rebaseline_response(&attempt), learning("new", 1), received).is_none());
            assert_eq!(format!("{m:?}"), before);
        }
    }

    #[test]
    fn rebaseline_requires_fresh_ids_and_checked_connection_generation() {
        let mut m = model();
        let now = Instant::now();
        let original = m.open_candidates(10, now).unwrap();
        assert!(m.prepare_rebaseline(original.key, 10, 11, now).is_none());
        assert!(m.prepare_rebaseline(original.key, 11, 11, now).is_none());
        assert!(m.prepare_rebaseline(original.key, 11, 12, now + Duration::from_millis(1200)).is_none());
        m.close_window();
        m.identity.connection_generation = u64::MAX;
        let original = m.open_candidates(11, now).unwrap();
        assert!(m.prepare_rebaseline(original.key, 12, 13, now).is_none());
    }
    #[test]
    fn whole_correction_preserves_reading_but_removes_candidate_provenance() {
        let mut m = model();
        let reading = m.reading.clone();
        let revision = m.revision;
        assert!(m.replace_whole_surface("補正文".into()));
        assert_eq!(m.reading, reading);
        assert_eq!(m.text(), "補正文");
        assert_eq!(m.clauses.len(), 1);
        assert_eq!(m.clauses[0].start, ReadingPosition(0));
        assert_eq!(m.clauses[0].end.0 as usize, reading.chars().count());
        assert_eq!(m.clauses[0].source, SurfaceSource::LocalSurface);
        assert_eq!(m.revision, revision + 1);
        assert!(m.sentence_token.is_none());
        assert!(matches!(m.window, CandidateWindow::Closed));
    }

    #[test]
    fn ascii_notation_cycles_original_units_and_preserves_reading_mode_and_range() {
        use crate::keymap::Notation as N;
        let mut m = ClauseConversion::from_reading(model().identity, "にほんご".into()).unwrap();
        let revision = m.identity.revision;
        let (start, end) = (ReadingPosition(0), ReadingPosition(4));
        for expected in ["nihongo", "NIHONGO", "Nihongo", "nihongo"] {
            assert!(m.transform_notation(start, end, N::HankakuEisu, Some("nihongo")));
            assert_eq!(m.text(), expected);
            assert_eq!(m.mode, OperationMode::Converting);
            assert_eq!(m.identity.revision, revision);
            assert_eq!((m.clauses[0].start, m.clauses[0].end), (start, end));
        }
        assert!(m.transform_notation(start, end, N::ZenkakuEisu, Some("nihongo")));
        assert_eq!(m.text(), "ｎｉｈｏｎｇｏ");
        assert!(m.transform_notation(start, end, N::HankakuEisu, Some("nihongo")));
        assert_eq!(m.text(), "nihongo");
        m.reset_notation_cycle();
        assert!(m.transform_notation(start, end, N::HankakuEisu, Some("nihongo")));
        assert_eq!(m.text(), "nihongo");
        assert!(m.transform_kana(start, end, N::Hiragana));
        assert_eq!(m.mode, OperationMode::Converting);
        assert_eq!(m.text(), "にほんご");
    }

    #[test]
    fn ascii_notation_fallback_keeps_kana_and_non_ascii_case() {
        use crate::keymap::Notation as N;
        let mut m = ClauseConversion::from_reading(model().identity, "きょー、。Ａé".into()).unwrap();
        let end = m.clauses[0].end;
        for expected in ["きょー、。aé", "きょー、。Aé", "きょー、。Aé"] {
            assert!(m.transform_notation(ReadingPosition(0), end, N::HankakuEisu, None));
            assert_eq!(m.text(), expected);
        }
        assert!(m.transform_notation(ReadingPosition(0), end, N::ZenkakuEisu, None));
        assert_eq!(m.text(), "きょー、。ａé");
    }

    fn model() -> ClauseConversion {
        let clauses = [(1, 0, 1, "日"), (2, 1, 2, "本")]
            .map(|(id, start, end, surface)| WireClause {
                id: ClauseId(id),
                reading_start: ReadingPosition(start),
                reading_end: ReadingPosition(end),
                state: ClauseState::Converted,
                surface: surface.into(),
                candidate_token: Some(format!("token{id}")),
            })
            .to_vec();
        ClauseConversion::from_snapshot(
            SnapshotIdentity {
                composition: 1,
                revision: 2,
                configuration_generation: 3,
                connection_generation: 4,
            },
            5,
            SnapshotClauseData {
                reading: "にほ".into(),
                conversion_revision: 0,
                request_id: 1,
                clauses,
                sentence_token: None,
            },
            "日本",
        )
        .unwrap()
    }
    fn ready(request: &ClauseCandidatesRequest, surfaces: &[&str]) -> ClauseCandidatesStatus {
        ClauseCandidatesStatus::Ready {
            candidates: surfaces
                .iter()
                .enumerate()
                .map(|(i, surface)| ClauseCandidate {
                    surface: (*surface).into(),
                    token: format!("{}-{i}", request.key.request_id),
                    reading_start: request.reading_start,
                    reading_end: request.reading_end,
                })
                .collect(),
        }
    }

    #[test]
    fn shorter_candidates_preserve_suffix_and_cycle_back_to_full_range() {
        let mut m = model();
        m.reading = "がぞうのようにする".into();
        m.clauses[0].end = ReadingPosition(7);
        m.clauses[0].surface = "画像のように".into();
        m.clauses[1].start = ReadingPosition(7);
        m.clauses[1].end = ReadingPosition(9);
        m.clauses[1].surface = "する".into();
        m.sentence_token = Some("whole-sentence".into());
        let following = m.clauses[1].clone();
        let now = Instant::now();
        let request = m.open_candidates(2, now).unwrap();
        let ClauseCandidatesStatus::Ready { mut candidates } = ready(&request, &["画像のように", "画像の", "画像"]) else { unreachable!() };
        candidates[1].reading_end = ReadingPosition(4);
        candidates[2].reading_end = ReadingPosition(3);
        assert!(m.accept_candidates(request.key, ClauseCandidatesStatus::Ready { candidates }, now));
        assert!(m.advance_candidate(1));
        assert_eq!(m.text(), "画像のようにする");
        assert_eq!(m.clauses[0].end, ReadingPosition(4));
        assert_eq!(m.clauses[1].surface, "ように");
        assert_eq!(m.clauses[1].source, SurfaceSource::Reading);
        assert_eq!(m.clauses[2], following);
        assert_eq!(m.sentence_token, None);
        let suffix_id = m.clauses[1].id;
        assert!(m.advance_candidate(1));
        assert_eq!(m.clauses[1].surface, "のように");
        assert_ne!(m.clauses[1].id, suffix_id);
        assert!(m.advance_candidate(1));
        assert_eq!(m.clauses.len(), 2);
        assert_eq!(m.clauses[0].end, ReadingPosition(7));
        assert_eq!(m.clauses[1], following);
        assert!(m.select_candidate(1));
        m.close_window();
        let shorter = m.open_candidates(3, now).unwrap();
        assert_eq!(shorter.reading_end, ReadingPosition(4), "closed prefix selection must not reuse the old full-range cache");
        m.move_clause(1);
        let suffix = m.open_boundary_conversion(4, now).unwrap();
        assert_eq!(suffix.clauses, vec![ClauseRange { id: m.clauses[1].id,
            reading_start: ReadingPosition(4), reading_end: ReadingPosition(7) }]);
        assert_eq!(m.reading, "がぞうのようにする");
    }

    #[test]
    fn same_surface_prefix_keeps_its_range_and_exhausted_split_is_atomic() {
        let mut m = model();
        m.clauses.truncate(1);
        m.reading = "ああ".into();
        m.clauses[0].end = ReadingPosition(2);
        m.clauses[0].surface = "亜".into();
        let now = Instant::now();
        let request = m.open_candidates(2, now).unwrap();
        let ClauseCandidatesStatus::Ready { mut candidates } = ready(&request, &["亜", "亜"]) else { unreachable!() };
        candidates[0].reading_end = ReadingPosition(1);
        assert!(m.accept_candidates(request.key, ClauseCandidatesStatus::Ready { candidates }, now));
        let (page, selected) = m.page().unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(selected, 0);
        assert_eq!(page[0].reading_end, ReadingPosition(2));
        let before = m.clauses.clone();
        let revision = m.revision;
        m.next_clause_id = Some(u64::MAX);
        assert!(!m.select_candidate(1));
        assert_eq!(m.clauses, before);
        assert_eq!(m.revision, revision);
        assert_eq!(m.page().unwrap().1, 0);
        m.next_clause_id = Some(3);
        assert!(m.select_candidate(1));
        assert_eq!(m.text(), "亜あ");
        assert_eq!(m.clauses[0].end, ReadingPosition(1));
        assert_eq!(m.escape(), LocalEditOutcome::Changed);
        assert_eq!(m.escape(), LocalEditOutcome::Changed);
        assert_eq!(m.text(), "ああ");
    }

    #[test]
    fn space_candidates_start_with_displayed_surface_then_visit_second_choice() {
        let mut m = model();
        m.clauses.truncate(1);
        m.reading = "おしたら".into();
        m.clauses[0].end = ReadingPosition(4);
        m.clauses[0].surface = "推したら".into();
        let now = Instant::now();
        let request = m.open_candidates(2, now).unwrap();
        assert!(m.accept_candidates(request.key,
            ready(&request, &["押したら", "推したら", "おしたら"]), now));
        let (page, selected) = m.page().unwrap();
        assert_eq!(selected, 0, "the displayed conversion is always candidate 1");
        assert_eq!(page.iter().map(|c| c.surface.as_str()).collect::<Vec<_>>(),
            ["推したら", "押したら", "おしたら"]);
        assert_eq!(m.text(), "推したら");
        assert!(m.advance_candidate(1));
        assert_eq!(m.page().unwrap().1, 1);
        assert_eq!(m.text(), "押したら", "Space must not skip to おしたら");
    }

    #[test]
    fn opening_space_moves_once_after_reply_and_reopening_keeps_the_order() {
        let mut m = model();
        let now = Instant::now();
        let request = m.cycle_candidate(2, 1, now).unwrap();
        assert_eq!(m.text(), "日本", "no change while candidates are loading");
        let reply = ready(&request, &["二", "日", "に"]);
        assert!(m.accept_candidates(request.key, reply.clone(), now));
        assert_eq!((m.text(), m.page().unwrap().1), ("二本".into(), 1));
        assert!(matches!(&m.clauses[0].source, SurfaceSource::Candidate { token, explicitly_selected: true } if token == "2-0"));
        assert!(!m.accept_candidates(request.key, reply, now), "duplicate reply cannot move twice");
        m.close_window();
        assert!(m.cycle_candidate(3, 1, now).is_none(), "reopen uses the cached order");
        assert_eq!((m.text(), m.page().unwrap().1), ("に本".into(), 2));
        assert!(m.cycle_candidate(4, 1, now).is_none());
        assert_eq!((m.text(), m.page().unwrap().1), ("日本".into(), 0));
    }

    #[test]
    fn absent_live_surface_stays_selectable_without_inventing_a_learning_token() {
        let mut m = model();
        m.invalidate_learning();
        let now = Instant::now();
        let request = m.cycle_candidate(2, 1, now).unwrap();
        assert!(m.accept_candidates(request.key, ready(&request, &["二", "に"]), now));
        assert_eq!((m.text(), m.page().unwrap().1), ("二本".into(), 1));
        m.advance_candidate(-1);
        assert_eq!(m.text(), "日本");
        assert_eq!(m.clauses[0].source, SurfaceSource::LocalSurface);
        assert!(m.sentence_token.is_none());
    }

    #[test]
    fn cancelled_opening_space_does_not_advance_a_later_window() {
        let mut m = model();
        let now = Instant::now();
        let old = m.cycle_candidate(2, 1, now).unwrap();
        m.close_window();
        let new = m.open_candidates(3, now).unwrap();
        assert!(!m.accept_candidates(old.key, ready(&old, &["二", "日"]), now));
        assert!(m.accept_candidates(new.key, ready(&new, &["二", "日"]), now));
        assert_eq!((m.text(), m.page().unwrap().1), ("日本".into(), 0));
    }

    #[test]
    fn live_promotion_keeps_clause_boundaries_and_a_locally_typed_suffix() {
        let m = model();
        let identity = SnapshotIdentity { revision: m.identity.revision + 1, ..m.identity };
        let promoted = m.promote_live_display(identity, "にほです", "日本です").unwrap();
        assert_eq!(promoted.text(), "日本です");
        assert_eq!(promoted.clauses[..2], m.clauses);
        assert_eq!(promoted.clauses[2].source, SurfaceSource::Reading);
        assert_eq!((promoted.clauses[2].start, promoted.clauses[2].end), (ReadingPosition(2), ReadingPosition(4)));
        assert!(promoted.sentence_token.is_none());
        assert!(m.promote_live_display(identity, "にほです", "二歩です").is_none());
        assert!(m.promote_live_display(identity, "にです", "日です").is_none());
        assert!(m.promote_live_display(SnapshotIdentity { composition: 2, ..identity }, "にほ", "日本").is_none());
        assert!(m.promote_live_display(SnapshotIdentity { connection_generation: 5, ..identity }, "にほ", "日本").is_none());
        let reading = ClauseConversion::from_reading(identity, "にほ".into()).unwrap();
        assert!(reading.promote_live_display(identity, "にほ", "にほ").is_none(),
            "a reading-only fallback still needs explicit conversion on first Space");
    }

    #[test]
    fn display_deletion_matches_unicode_15_1_grapheme_fixture() {
        assert_eq!(unicode_segmentation::UNICODE_VERSION, (15, 1, 0));
        let fixture = include_str!("../../../docs/design/clause-navigation-p5/GraphemeBreakTest-15.1.0.txt");
        let mut cases = 0;
        for line in fixture.lines() {
            let data = line.split('#').next().unwrap().trim();
            if data.is_empty() { continue; }
            let mut clusters = Vec::<String>::new();
            let mut current = String::new();
            for token in data.split_whitespace() {
                match token {
                    "÷" => { if !current.is_empty() { clusters.push(std::mem::take(&mut current)); } }
                    "×" => {}
                    scalar => current.push(char::from_u32(u32::from_str_radix(scalar, 16).unwrap()).unwrap()),
                }
            }
            assert!(current.is_empty(), "trailing boundary: {line}");
            let text = clusters.concat();
            let mut m = model();
            m.clauses[1].surface = text;
            m.selected = 0; // deletion always targets the last surface
            while clusters.len() > 1 {
                clusters.pop();
                assert_eq!(m.delete_display_tail(), LocalEditOutcome::Changed, "{line}");
                assert_eq!(m.clauses[1].surface, clusters.concat(), "{line}");
                assert_eq!(m.reading, "にほ");
                assert_eq!(m.clauses[1].source, SurfaceSource::LocalSurface);
            }
            assert_eq!(m.delete_display_tail(), LocalEditOutcome::ReadingChanged, "{line}");
            assert_eq!(m.reading, "に");
            assert_eq!(m.text(), "日");
            cases += 1;
        }
        assert!(cases > 1000);
    }

    #[test]
    fn display_deletion_matches_p2_examples() {
        let fixtures: serde_json::Value = serde_json::from_str(include_str!(
            "../../../docs/design/clause-navigation-p2/state-fixtures.json"
        )).unwrap();
        let cases = fixtures["display_backspace"].as_array().unwrap();
        assert_eq!(cases.len(), 4);
        for case in cases {
            let mut m = model();
            m.clauses[1].surface = case["surface"].as_str().unwrap().into();
            m.selected = 0;
            let expected = case["after_bs"].as_str().unwrap();
            let outcome = m.delete_display_tail();
            assert_eq!(m.text(), format!("日{expected}"), "{case}");
            assert_eq!(m.reading, if expected.is_empty() { "に" } else { "にほ" }, "{case}");
            assert_eq!(outcome, if expected.is_empty() {
                LocalEditOutcome::ReadingChanged
            } else { LocalEditOutcome::Changed }, "{case}");
        }
    }

    #[test]
    fn deleting_surface_keeps_original_reading_until_clause_becomes_empty() {
        let mut m = boundary_model();
        m.clauses[2].surface = "天気".into();
        m.selected = 0;
        let reading = m.reading.clone();
        let identity = m.identity;
        m.sentence_token = Some("seed".into());
        assert_eq!(m.delete_display_tail(), LocalEditOutcome::Changed);
        assert_eq!(m.clauses[2].surface, "天");
        assert_eq!(m.reading, reading);
        assert_eq!(m.identity, identity);
        assert!(m.sentence_token.is_none());
        m.selected = 2;
        assert_eq!(m.delete_display_tail(), LocalEditOutcome::ReadingChanged);
        assert_eq!(m.reading, "か\u{3099}きくけ");
        assert_eq!(m.selected, 1);
        assert_eq!(m.identity.revision, identity.revision + 1);
        assert_eq!(m.clauses.len(), 2);
    }

    #[test]
    fn escape_closes_then_reverts_selected_then_all_reading_even_after_movement() {
        let mut m = model();
        let now = Instant::now();
        let request = m.open_candidates(2, now).unwrap();
        m.accept_candidates(request.key, ready(&request, &["日", "二"]), now);
        assert_eq!(m.escape(), LocalEditOutcome::Changed);
        assert_eq!(m.text(), "日本");
        assert_eq!(m.escape(), LocalEditOutcome::Changed);
        assert_eq!(m.text(), "に本");
        assert_eq!(m.mode, OperationMode::Converting);
        m.move_clause(1);
        assert_eq!(m.escape(), LocalEditOutcome::Editing);
        assert_eq!(m.text(), "にほ");
        assert_eq!(m.mode, OperationMode::Editing);
        assert_eq!(m.cancel_stage, CancelStage::None);
    }

    #[test]
    fn notation_and_boundary_changes_reset_selected_reading_cancel_stage() {
        for resize in [true, false] {
            let mut m = model();
            assert_eq!(m.escape(), LocalEditOutcome::Changed);
            if resize { assert_eq!(m.resize_selected(1), LocalEditOutcome::Changed); }
            else { assert!(m.transform_kana(ReadingPosition(0), ReadingPosition(1), crate::keymap::Notation::Katakana)); }
            assert_eq!(m.escape(), LocalEditOutcome::Changed);
            assert_eq!(m.mode, OperationMode::Converting);
            assert_eq!(m.escape(), LocalEditOutcome::Editing);
        }
    }

    fn boundary_model() -> ClauseConversion {
        let mut model = model();
        model.reading = "か\u{3099}きくけこ".into();
        model.clauses = [(1, 0, 3, "柿"), (2, 3, 5, "区家"), (3, 5, 6, "個")].into_iter()
            .map(|(id, start, end, surface)| LocalClause { id: ClauseId(id), start: ReadingPosition(start), end: ReadingPosition(end),
                surface: surface.into(), source: SurfaceSource::Candidate { token: format!("token{id}"), explicitly_selected: true } }).collect();
        model.next_clause_id = Some(4);
        model.learning_identity = Some(ipc::client::EngineLearningIdentity { engine_epoch: "epoch".into(), learning_generation: 1 });
        model
    }

    #[test]
    fn mixed_reading_edit_preserves_other_surfaces_and_retires_shifted_tokens() {
        let mut m = boundary_model();
        let first = m.clauses[0].clone();
        let last = m.clauses[2].clone();
        assert_eq!(m.begin_reading_edit(1), LocalEditOutcome::Changed);
        assert_eq!(m.text(), "柿くけ個");
        let reading_revision = m.identity.revision + 1;
        assert_eq!(m.replace_edited_reading("か\u{3099}きくあけこ", reading_revision), LocalEditOutcome::ReadingChanged);
        assert_eq!(m.text(), "柿くあけ個");
        assert_eq!(m.clauses[0], first);
        assert_eq!(m.clauses[2].surface, last.surface);
        assert_eq!(m.clauses[2].source, SurfaceSource::LocalSurface);
        assert_ne!(m.clauses[2].id, last.id);
        assert_eq!(m.clauses[2].start, ReadingPosition(6));
        assert_eq!(m.clauses[2].end, ReadingPosition(7));
        assert_eq!(m.identity.revision, reading_revision);
        m.mode = OperationMode::Converting;
        let request = m.open_boundary_conversion(20, Instant::now()).unwrap();
        assert_eq!(request.clauses, vec![ClauseRange { id: m.clauses[1].id, reading_start: ReadingPosition(3), reading_end: ReadingPosition(6) }]);
        assert_eq!(request.reading, "か\u{3099}きくあけこ");
        assert_eq!(request.preceding_surfaces[0].surface, first.surface);
    }

    #[test]
    fn deleting_mixed_interval_removes_it_and_keeps_adjacent_surfaces() {
        let mut m = boundary_model();
        m.begin_reading_edit(1);
        assert_eq!(m.replace_edited_reading("か\u{3099}きこ", m.identity.revision + 1), LocalEditOutcome::ReadingChanged);
        assert_eq!(m.text(), "柿個");
        assert_eq!(m.clauses.len(), 2);
        assert_eq!(m.clauses[0].end, m.clauses[1].start);
        assert_eq!(m.clauses[1].end, ReadingPosition(4));
        assert_eq!(m.mode, OperationMode::Converting);
    }

    #[test]
    fn mixed_edit_exhaustion_is_atomic_and_dakuten_can_absorb_affected_neighbour() {
        for ids in [false, true] {
            let mut m = boundary_model();
            m.begin_reading_edit(1);
            if ids { m.next_clause_id = Some(u64::MAX); } else { m.revision = u64::MAX; }
            let before = m.clauses.clone();
            let reading = m.reading.clone();
            assert_eq!(m.replace_edited_reading("か\u{3099}きくあけこ", m.identity.revision + 1), LocalEditOutcome::Exhausted);
            assert_eq!(m.clauses, before);
            assert_eq!(m.reading, reading);
        }
        let mut m = boundary_model();
        m.begin_reading_edit(1);
        assert_eq!(m.replace_edited_reading("か\u{3099}き\u{3099}くけこ", m.identity.revision + 1), LocalEditOutcome::ReadingChanged);
        assert_eq!(m.clauses.len(), 2);
        assert_eq!(m.clauses[0].start, ReadingPosition(0));
        assert_eq!(m.clauses[0].source, SurfaceSource::Reading);
        assert_eq!(m.clauses[1].surface, "個");
        assert_eq!(m.text(), "か\u{3099}き\u{3099}くけ個");
    }

    #[test]
    fn boundary_edits_preserve_reading_and_unaffected_surfaces_without_issuing_work() {
        let mut m = boundary_model();
        let identity = m.identity;
        let reading = m.reading.clone();
        let last = m.clauses[2].clone();
        let old = m.open_candidates(2, Instant::now()).unwrap();
        m.sentence_token = Some("seed".into());
        assert_eq!(m.resize_selected(-1), LocalEditOutcome::Changed);
        assert_eq!(m.clauses[0].end, ReadingPosition(2)); // combining dakuten stays with its base
        assert_eq!(m.clauses[1].start, ReadingPosition(2));
        assert_eq!(m.text(), "か\u{3099}きくけ個");
        assert_eq!(m.clauses[2], last);
        assert_eq!(m.reading, reading);
        assert_eq!(m.identity, identity);
        assert_eq!(m.revision, 1);
        assert!(m.sentence_token.is_none());
        assert!(m.issued.is_empty() && !m.boundary_loading());
        assert_eq!(m.window, CandidateWindow::Closed);
        assert!(!m.accept_candidates(old.key, ready(&old, &["旧"]), Instant::now()));
        let ids = [m.clauses[0].id, m.clauses[1].id];
        assert_eq!(m.resize_selected(1), LocalEditOutcome::Changed);
        assert!(m.clauses[..2].iter().all(|clause| !ids.contains(&clause.id)));
        assert_eq!(m.resize_selected(1), LocalEditOutcome::Changed);
        assert_eq!(m.resize_selected(1), LocalEditOutcome::Changed); // absorb the entire right clause
        assert_eq!(m.clauses.len(), 2);
        assert_eq!(m.clauses[1], last);
        assert_eq!(m.reading, reading);
    }

    #[test]
    fn boundary_edges_suffix_creation_and_exhaustion_are_atomic() {
        let mut m = boundary_model();
        assert_eq!(m.resize_selected(-1), LocalEditOutcome::Changed);
        let before = m.clauses.clone();
        assert_eq!(m.resize_selected(-1), LocalEditOutcome::Unchanged);
        assert_eq!(m.clauses, before);
        m.move_clause(i32::MAX);
        assert_eq!(m.resize_selected(1), LocalEditOutcome::Unchanged);
        m.clauses = vec![LocalClause { id: ClauseId(9), start: ReadingPosition(0), end: ReadingPosition(6),
            surface: "全体".into(), source: SurfaceSource::LocalSurface }];
        m.selected = 0;
        m.next_clause_id = Some(10);
        assert_eq!(m.resize_selected(-1), LocalEditOutcome::Changed);
        assert_eq!(m.clauses.len(), 2);
        assert_eq!(m.clauses[1].surface, "こ");
        let before = m.clauses.clone();
        m.next_clause_id = Some(u64::MAX);
        assert_eq!(m.resize_selected(1), LocalEditOutcome::Exhausted);
        assert_eq!(m.clauses, before);
        m.next_clause_id = Some(100);
        m.revision = u64::MAX;
        assert_eq!(m.resize_selected(1), LocalEditOutcome::Exhausted);
        assert_eq!(m.clauses, before);
    }

    #[test]
    fn boundary_response_requires_exact_ranges_and_applies_changed_intervals_together() {
        let mut m = boundary_model();
        let now = Instant::now();
        assert_eq!(m.resize_selected(-1), LocalEditOutcome::Changed);
        let last = m.clauses[2].clone();
        let request = m.open_boundary_conversion(3, now).unwrap();
        assert_eq!(request.clauses.len(), 2);
        let response = request.clauses.iter().map(|range| WireClause { id: range.id, reading_start: range.reading_start,
            reading_end: range.reading_end, surface: "新".into(), state: ClauseState::Converted,
            candidate_token: Some(format!("new{}", range.id.0)) }).collect::<Vec<_>>();
        let mut invalid = response.clone();
        invalid[1].reading_end = ReadingPosition(6);
        let reading_surfaces = m.text();
        assert!(!m.accept_boundary_conversion(request.key, ipc::clause::ConvertClausesStatus::Ready { clauses: invalid }, m.learning_identity.clone(), now));
        assert_eq!(m.text(), reading_surfaces);
        assert!(!m.boundary_loading());
        let request = m.open_boundary_conversion(4, now).unwrap();
        assert!(m.accept_boundary_conversion(request.key, ipc::clause::ConvertClausesStatus::Ready { clauses: response }, m.learning_identity.clone(), now));
        assert_eq!(m.text(), "新新個");
        assert_eq!(m.clauses[2], last);
        assert_eq!(m.selected, 0);
        assert_eq!(m.window, CandidateWindow::Closed);
    }

    #[test]
    fn cancelled_or_expired_boundary_response_never_restores_old_surfaces() {
        for cancel in [true, false] {
            let mut m = boundary_model();
            let now = Instant::now();
            assert_eq!(m.resize_selected(1), LocalEditOutcome::Changed);
            let request = m.open_boundary_conversion(3, now).unwrap();
            let before = m.clauses.clone();
            if cancel { m.close_window(); } else { m.expire(now + Duration::from_millis(1200)); }
            assert!(!m.boundary_loading());
            assert_eq!(m.clauses, before);
            assert!(!m.accept_boundary_conversion(request.key, ipc::clause::ConvertClausesStatus::Ready { clauses: vec![] }, m.learning_identity.clone(), now));
            assert_eq!(m.clauses, before);
        }
    }

    #[test]
    fn boundary_rebaseline_keeps_all_resized_ranges_and_the_original_deadline() {
        let mut m = boundary_model();
        let now = Instant::now();
        assert_eq!(m.resize_selected(-1), LocalEditOutcome::Changed);
        let original = m.open_boundary_conversion(3, now).unwrap();
        let before = m.clauses.clone();
        let attempt = m.prepare_rebaseline(original.key, 4, 5, now).unwrap();
        let convert = m.accept_rebaseline(&attempt, &rebaseline_response(&attempt), learning("new", 1), now).unwrap();
        assert_eq!(convert.clauses, original.clauses);
        assert_eq!(convert.reading, original.reading);
        assert_eq!(m.clauses.iter().map(|c| (&c.id, &c.start, &c.end, &c.surface)).collect::<Vec<_>>(),
            before.iter().map(|c| (&c.id, &c.start, &c.end, &c.surface)).collect::<Vec<_>>());
        assert_eq!(m.request_deadline(convert.key), Some(now + Duration::from_millis(1200)));
        assert!(m.prepare_rebaseline(convert.key, 6, 7, now).is_none());
        assert_ne!(convert.key.identity.connection_generation, original.key.identity.connection_generation);
        let clauses = convert.clauses.iter().map(|range| WireClause {
            id: range.id, reading_start: range.reading_start, reading_end: range.reading_end,
            surface: "新".into(), state: ClauseState::Converted, candidate_token: Some("fresh".into()),
        }).collect();
        assert!(m.accept_boundary_conversion(convert.key, ipc::clause::ConvertClausesStatus::Ready { clauses }, Some(learning("new", 1)), now));
        assert_eq!(m.text(), "新新個");
        assert_eq!(m.clauses[2].source, SurfaceSource::LocalSurface);
    }

    #[test]
    fn cancelled_and_expired_boundary_rebaseline_cannot_reinstall_a_wait() {
        for cancel in [true, false] {
            let mut m = boundary_model();
            let now = Instant::now();
            assert_eq!(m.resize_selected(-1), LocalEditOutcome::Changed);
            let original = m.open_boundary_conversion(3, now).unwrap();
            let before = m.clauses.clone();
            let attempt = m.prepare_rebaseline(original.key, 4, 5, now).unwrap();
            if cancel { m.close_window(); } else { m.expire(now + Duration::from_millis(1200)); }
            assert!(m.accept_rebaseline(&attempt, &rebaseline_response(&attempt), learning("new", 1), now).is_none());
            assert_eq!(m.clauses, before);
            assert!(!m.boundary_loading());
        }
    }
    #[test]
    fn configuration_ack_refreshes_learning_without_changing_the_visible_composition() {
        let mut m = model();
        let old = ipc::client::EngineLearningIdentity { engine_epoch: "engine".into(), learning_generation: 1 };
        m.learning_identity = Some(old.clone());
        let identity = m.identity;
        let now = Instant::now();
        let request = m.open_candidates(2, now).unwrap();
        assert!(m.accept_candidates(request.key, ready(&request, &["二"]), now));
        assert!(m.select_candidate(0));
        let visible = m.text();
        let revision = m.revision;
        let updated = ipc::client::EngineLearningIdentity { learning_generation: 2, ..old };
        m.configured_learning_identity(Some(updated.clone()));
        assert_eq!(m.text(), visible);
        assert_eq!(m.identity, identity);
        assert_eq!(m.revision, revision);
        assert_eq!(m.window, CandidateWindow::Closed);
        assert!(m.clauses.iter().all(|c| c.source == SurfaceSource::LocalSurface));
        assert_eq!(m.learning_identity, Some(updated));
        assert!(!m.accept_candidates(request.key, ready(&request, &["日"]), now));
    }

    #[test]
    fn authoritative_configuration_can_adopt_a_new_epoch_but_unavailable_metadata_never_restores_tokens() {
        let mut m = model();
        m.learning_identity = Some(ipc::client::EngineLearningIdentity { engine_epoch: "old".into(), learning_generation: 9 });
        let updated = ipc::client::EngineLearningIdentity { engine_epoch: "new".into(), learning_generation: 1 };
        m.configured_learning_identity(Some(updated.clone()));
        assert_eq!(m.learning_identity, Some(updated.clone()));
        assert_eq!(m.text(), "日本");
        m.configured_learning_identity(None);
        assert!(m.learning_identity.is_none());
        assert_eq!(m.text(), "日本");
        m.configured_learning_identity(Some(updated.clone()));
        assert_eq!(m.learning_identity, Some(updated));
        assert!(m.clauses.iter().all(|c| c.source == SurfaceSource::LocalSurface));
    }

    #[test]
    fn kana_transform_changes_only_target_and_rejects_old_candidates() {
        use crate::keymap::Notation;
        let mut m = model();
        let now = Instant::now();
        let request = m.open_candidates(2, now).unwrap();
        let other = m.clauses[1].clone();
        let identity = m.identity;
        m.sentence_token = Some("seed".into());
        assert!(m.transform_kana(ReadingPosition(0), ReadingPosition(1), Notation::Katakana));
        assert_eq!(m.text(), "ニ本");
        assert_eq!(m.clauses[0].source, SurfaceSource::LocalSurface);
        assert_eq!(m.clauses[1], other);
        assert_eq!(m.identity, identity);
        assert_eq!(m.reading, "にほ");
        assert_eq!(m.sentence_token.as_deref(), Some("seed"));
        assert_eq!(m.window, CandidateWindow::Closed);
        assert!(!m.accept_candidates(request.key, ready(&request, &["二"]), now));
        assert!(m.transform_kana(ReadingPosition(0), ReadingPosition(1), Notation::Katakana));
        assert_eq!(m.text(), "ニ本");
        assert!(m.transform_kana(ReadingPosition(0), ReadingPosition(1), Notation::Hiragana));
        assert_eq!(m.text(), "に本");
        assert_eq!(m.clauses[0].source, SurfaceSource::Reading);
        assert_eq!(m.mode, OperationMode::Converting);
    }

    #[test]
    fn queued_whole_reading_kana_transform_preserves_original_target_across_snapshot_segmentation() {
        use crate::keymap::Notation;
        let mut m = model();
        let id = m.clauses[0].id;
        assert!(m.transform_kana(ReadingPosition(0), ReadingPosition(2), Notation::HankakuKana));
        assert_eq!(m.text(), "ﾆﾎ");
        assert_eq!(m.clauses.len(), 1);
        assert_eq!(m.clauses[0].id, id);
        assert_eq!(m.clauses[0].end, ReadingPosition(2));
        assert_eq!(m.clauses[0].source, SurfaceSource::LocalSurface);
        let before = m.text();
        assert!(!m.transform_kana(ReadingPosition(1), ReadingPosition(2), Notation::Katakana));
        assert_eq!(m.text(), before);
    }

    #[test]
    fn only_explicit_whole_reading_selection_replaces_the_sentence_seed() {
        let mut m = model();
        let now = Instant::now();
        m.sentence_token = Some("original seed".into());
        let part = m.open_candidates(2, now).unwrap();
        assert!(m.accept_candidates(part.key, ready(&part, &["二"]), now));
        assert!(m.select_candidate(0));
        assert_eq!(m.sentence_token.as_deref(), Some("original seed"));
        m.close_window();
        m.clauses.truncate(1);
        m.clauses[0].end = ReadingPosition(2);
        m.clauses[0].surface = "日本".into();
        let whole = m.open_candidates(3, now).unwrap();
        assert!(m.accept_candidates(whole.key, ready(&whole, &["日本", "二穂"]), now));
        assert_eq!(m.sentence_token.as_deref(), Some("original seed"));
        assert!(m.select_candidate(1));
        assert_eq!(m.sentence_token.as_deref(), Some("3-1"));
        m.move_clause(1);
        assert_eq!(m.sentence_token.as_deref(), Some("3-1"));
    }

    #[test]
    fn new_candidate_generation_preserves_surfaces_and_replaces_learning_materials() {
        let mut m = model();
        let identity = |generation| ipc::client::EngineLearningIdentity {
            engine_epoch: "engine".into(), learning_generation: generation,
        };
        m.learning_identity = Some(identity(1));
        m.sentence_token = Some("old sentence".into());
        let now = Instant::now();
        let request = m.open_candidates(2, now).unwrap();
        assert!(m.accept_candidates_with_identity(request.key, ready(&request, &["二", "日"]), Some(identity(2)), now));
        assert_eq!(m.text(), "日本");
        assert_eq!(m.learning_identity, Some(identity(2)));
        assert!(m.sentence_token.is_none());
        assert!(m.clauses.iter().all(|clause| clause.source == SurfaceSource::LocalSurface));
        assert!(m.select_candidate(1));
        assert_eq!(m.text(), "二本");
        assert!(matches!(m.clauses[0].source, SurfaceSource::Candidate { .. }));
        m.move_clause(1);
        let old = m.open_candidates(3, now).unwrap();
        assert!(!m.accept_candidates_with_identity(old.key, ready(&old, &["穂"]), Some(identity(1)), now));
        assert_eq!(m.learning_identity, Some(identity(2)));
        assert_eq!(m.text(), "二本");
    }

    #[test]
    fn refresh_preserves_space_issued_after_invalidation_and_cannot_roll_back_epoch() {
        let mut m = model();
        m.invalidate_learning();
        let now = Instant::now();
        let request = m.open_candidates(2, now).unwrap();
        let fresh = ipc::client::EngineLearningIdentity { engine_epoch: "fresh".into(), learning_generation: 2 };
        m.refresh_learning_identity(fresh.clone());
        assert_eq!(m.window, CandidateWindow::Loading(request.key));
        assert!(m.accept_candidates_with_identity(request.key, ready(&request, &["二", "日"]), Some(fresh.clone()), now));
        let window = m.window.clone();
        m.refresh_learning_identity(ipc::client::EngineLearningIdentity { engine_epoch: "old".into(), learning_generation: 9 });
        assert_eq!(m.learning_identity, Some(fresh));
        assert_eq!(m.window, window);
        assert!(m.select_candidate(1));
        assert_eq!(m.text(), "二本");
    }

    #[test]
    fn identity_refresh_does_not_restore_invalidated_tokens_or_accept_stale_responses() {
        let mut m = model();
        let now = Instant::now();
        let request = m.open_candidates(2, now).unwrap();
        m.invalidate_learning();
        let identity = ipc::client::EngineLearningIdentity { engine_epoch: "engine".into(), learning_generation: 2 };
        m.refresh_learning_identity(identity.clone());
        assert_eq!(m.text(), "日本");
        assert_eq!(m.learning_identity, Some(identity.clone()));
        assert!(m.clauses.iter().all(|clause| clause.source == SurfaceSource::LocalSurface));
        assert!(!m.accept_candidates_with_identity(request.key, ready(&request, &["二"]), Some(identity), now));
    }

    #[test]
    fn learning_invalidation_preserves_composition_and_rejects_late_candidates() {
        let mut m = model();
        let now = Instant::now();
        m.learning_identity = Some(ipc::client::EngineLearningIdentity {
            engine_epoch: "old".into(), learning_generation: 1,
        });
        m.sentence_token = Some("sentence".into());
        let cached = m.open_candidates(2, now).unwrap();
        assert!(m.accept_candidates(cached.key, ready(&cached, &["日", "二"]), now));
        m.move_clause(1);
        m.clauses[1].source = SurfaceSource::Reading;
        m.clauses[1].surface = "ほ".into();
        let late = m.open_candidates(3, now).unwrap();
        let before = (m.text(), m.reading.clone(), m.identity, m.baseline, m.revision,
            m.selected, m.mode, m.clauses.iter().map(|c| (c.id, c.start, c.end)).collect::<Vec<_>>());
        m.invalidate_learning();
        assert_eq!(before, (m.text(), m.reading.clone(), m.identity, m.baseline, m.revision,
            m.selected, m.mode, m.clauses.iter().map(|c| (c.id, c.start, c.end)).collect::<Vec<_>>()));
        assert_eq!(m.clauses[0].source, SurfaceSource::LocalSurface);
        assert_eq!(m.clauses[1].source, SurfaceSource::Reading);
        assert!(m.learning_identity.is_none() && m.sentence_token.is_none());
        assert!(m.cache.is_empty() && m.issued.is_empty() && m.user_driven);
        assert_eq!(m.window, CandidateWindow::Closed);
        assert!(!m.accept_candidates(late.key, ready(&late, &["本"]), now));
        m.move_clause(-1);
        assert!(m.open_candidates(4, now).is_some(), "invalidated cache must be recalculated");
    }

    #[test]
    fn movement_is_sequential_clamped_and_does_not_request_candidates() {
        let mut m = model();
        m.move_clause(-1);
        m.move_clause(1);
        assert_eq!(m.selected, 1);
        assert_eq!(m.revision, 0);
        assert_eq!(m.text(), "日本");
        assert!(m.issued.is_empty());
        assert_eq!(m.window, CandidateWindow::Closed);
    }
    #[test]
    fn reversed_clause_replies_cannot_replace_the_selected_window() {
        let mut m = model();
        let now = Instant::now();
        let a = m.open_candidates(2, now).unwrap();
        m.move_clause(1);
        let b = m.open_candidates(3, now).unwrap();
        assert!(m.accept_candidates(b.key, ready(&b, &["穂", "本"]), now));
        assert!(!m.accept_candidates(a.key, ready(&a, &["二", "日"]), now));
        assert_eq!(m.page().unwrap().0[0].surface, "本");
        assert_eq!(
            m.text(),
            "日本",
            "opening a window must preserve the current surface"
        );
        m.move_clause(-1);
        assert!(m.open_candidates(4, now).is_none());
        assert_eq!(m.page().unwrap().0[0].surface, "日");
    }
    #[test]
    fn cancel_reopen_and_terminal_failure_never_accept_old_ready() {
        let mut m = model();
        let now = Instant::now();
        let old = m.open_candidates(2, now).unwrap();
        m.close_window();
        let current = m.open_candidates(3, now).unwrap();
        assert!(!m.accept_candidates(old.key, ready(&old, &["二"]), now));
        assert_eq!(m.window, CandidateWindow::Loading(current.key));
        assert!(!m.accept_candidates(
            current.key,
            ClauseCandidatesStatus::Unavailable {
                reason: ClauseUnavailableReason::Expired
            },
            now
        ));
        assert!(!m.accept_candidates(current.key, ready(&current, &["二"]), now));
        assert_eq!(m.window, CandidateWindow::Closed);
    }
    #[test]
    fn pending_does_not_extend_deadline_and_late_reply_cannot_revive_window() {
        let mut m = model();
        let now = Instant::now();
        let r = m.open_candidates(2, now).unwrap();
        m.accept_candidates(
            r.key,
            ClauseCandidatesStatus::Pending,
            now + Duration::from_millis(1199),
        );
        m.expire(now + Duration::from_millis(1200));
        assert!(!m.accept_candidates(r.key, ready(&r, &["二"]), now + Duration::from_millis(1201)));
        assert_eq!(m.text(), "日本");
        assert_eq!(m.window, CandidateWindow::Closed);
    }
    #[test]
    fn selecting_preceding_surface_invalidates_downstream_but_keeps_other_surfaces() {
        let mut m = model();
        let now = Instant::now();
        m.move_clause(1);
        let b = m.open_candidates(2, now).unwrap();
        m.accept_candidates(b.key, ready(&b, &["穂"]), now);
        m.move_clause(-1);
        let a = m.open_candidates(3, now).unwrap();
        m.accept_candidates(a.key, ready(&a, &["二"]), now);
        assert!(m.select_candidate(1));
        assert_eq!(m.text(), "二本");
        assert_eq!(m.revision, 1);
        m.move_clause(1);
        let new = m.open_candidates(4, now).unwrap();
        assert_eq!(new.preceding_surfaces[0].surface, "二");
        assert!(!m.accept_candidates(b.key, ready(&b, &["古"]), now));
    }
    #[test]
    fn pages_have_nine_entries_without_truncating_candidate_list() {
        let mut m = model();
        let now = Instant::now();
        let r = m.open_candidates(2, now).unwrap();
        m.accept_candidates(
            r.key,
            ready(
                &r,
                &["日", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10"],
            ),
            now,
        );
        assert_eq!(m.page().unwrap().0.len(), 9);
        m.select_candidate(8);
        m.advance_candidate(1);
        assert_eq!(m.page().unwrap().0.len(), 2);
        assert_eq!(m.page().unwrap().1, 0);
        m.select_candidate(10);
        m.advance_candidate(1);
        assert_eq!(m.text(), "日本");
    }
}
