//! Shared reading coordinates and immutable clause payloads (C1/C2/C4).
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct ReadingPosition(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DisplayUtf16Position(pub u32);

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct ClauseId(pub u64);

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SnapshotIdentity {
    pub composition: u64,
    pub revision: u64,
    pub configuration_generation: u64,
    pub connection_generation: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SnapshotClauseData {
    pub reading: String,
    pub conversion_revision: u64,
    pub request_id: u64,
    pub clauses: Vec<WireClause>,
    #[serde(deserialize_with = "required_option")]
    pub sentence_token: Option<String>,
}

impl SnapshotClauseData {
    pub fn from_reading(reading: String, conversion_revision: u64, request_id: u64) -> Self {
        let clauses = if reading.is_empty() {
            vec![]
        } else {
            vec![WireClause {
                id: ClauseId(1),
                reading_start: ReadingPosition(0),
                reading_end: ReadingPosition(
                    u32::try_from(reading.chars().count()).expect("reading exceeds wire size"),
                ),
                state: ClauseState::Reading,
                surface: reading.clone(),
                candidate_token: None,
            }]
        };
        Self {
            reading,
            conversion_revision,
            request_id,
            clauses,
            sentence_token: None,
        }
    }
    pub fn validate(&self, text: &str) -> Result<(), ClauseValidationError> {
        let end = u32::try_from(self.reading.chars().count())
            .map_err(|_| ClauseValidationError::Range)?;
        validate_clauses(
            &self.reading,
            &self.clauses,
            ReadingPosition(0),
            ReadingPosition(end),
            text,
        )
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotResponseKey {
    #[serde(flatten)]
    pub identity: SnapshotIdentity,
    pub baseline: u64,
    pub conversion_revision: u64,
    pub request_id: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClauseRequestKey {
    pub identity: SnapshotIdentity,
    pub baseline: u64,
    pub conversion_revision: u64,
    pub clause_id: ClauseId,
    pub request_id: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClauseState {
    Reading,
    Converted,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WireClause {
    pub id: ClauseId,
    pub reading_start: ReadingPosition,
    pub reading_end: ReadingPosition,
    pub state: ClauseState,
    pub surface: String,
    #[serde(deserialize_with = "required_option")]
    pub candidate_token: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ClauseRange {
    pub id: ClauseId,
    pub reading_start: ReadingPosition,
    pub reading_end: ReadingPosition,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PrecedingSurface {
    pub clause_id: ClauseId,
    pub reading_start: ReadingPosition,
    pub reading_end: ReadingPosition,
    pub surface: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ClauseCandidate {
    pub surface: String,
    pub token: String,
    pub reading_start: ReadingPosition,
    pub reading_end: ReadingPosition,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ClauseCandidatesRequest {
    pub key: ClauseRequestKey,
    pub reading: String,
    pub reading_start: ReadingPosition,
    pub reading_end: ReadingPosition,
    pub preceding_surfaces: Vec<PrecedingSurface>,
}

impl ClauseCandidatesRequest {
    pub fn validate(&self) -> Result<(), ClauseValidationError> {
        validate_preceding(&self.reading, &self.preceding_surfaces, self.reading_start)?;
        if self.reading_start >= self.reading_end
            || !legal_boundaries(&self.reading)?.contains(&self.reading_end)
            || self
                .preceding_surfaces
                .iter()
                .any(|p| p.clause_id == self.key.clause_id)
        {
            return Err(ClauseValidationError::Range);
        }
        Ok(())
    }

    pub fn validate_candidates(
        &self,
        candidates: &[ClauseCandidate],
    ) -> Result<(), ClauseValidationError> {
        self.validate()?;
        if candidates.is_empty() {
            return Err(ClauseValidationError::Surface);
        }
        for candidate in candidates {
            if candidate.reading_start != self.reading_start
                || candidate.reading_end != self.reading_end
            {
                return Err(ClauseValidationError::Range);
            }
            if candidate.surface.is_empty() {
                return Err(ClauseValidationError::Surface);
            }
            if candidate.token.is_empty() {
                return Err(ClauseValidationError::Token);
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ConvertClausesRequest {
    pub key: ClauseRequestKey,
    pub reading: String,
    pub clauses: Vec<ClauseRange>,
    pub preceding_surfaces: Vec<PrecedingSurface>,
}

impl ConvertClausesRequest {
    pub fn validate(&self) -> Result<(), ClauseValidationError> {
        let first = self.clauses.first().ok_or(ClauseValidationError::Range)?;
        validate_preceding(&self.reading, &self.preceding_surfaces, first.reading_start)?;
        if first.id != self.key.clause_id {
            return Err(ClauseValidationError::Range);
        }
        let boundaries = legal_boundaries(&self.reading)?;
        let mut cursor = first.reading_start;
        let mut ids: HashSet<_> = self
            .preceding_surfaces
            .iter()
            .map(|p| p.clause_id)
            .collect();
        for range in &self.clauses {
            if !ids.insert(range.id) {
                return Err(ClauseValidationError::DuplicateId);
            }
            if range.reading_start != cursor
                || range.reading_end <= cursor
                || !boundaries.contains(&range.reading_end)
            {
                return Err(ClauseValidationError::Range);
            }
            cursor = range.reading_end;
        }
        Ok(())
    }

    pub fn validate_result(&self, clauses: &[WireClause]) -> Result<(), ClauseValidationError> {
        self.validate()?;
        if self.clauses.len() != clauses.len()
            || !self.clauses.iter().zip(clauses).all(|(range, clause)| {
                range.id == clause.id
                    && range.reading_start == clause.reading_start
                    && range.reading_end == clause.reading_end
            })
        {
            return Err(ClauseValidationError::Range);
        }
        validate_clauses(
            &self.reading,
            clauses,
            self.clauses[0].reading_start,
            self.clauses.last().unwrap().reading_end,
            &clauses
                .iter()
                .map(|c| c.surface.as_str())
                .collect::<String>(),
        )
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClauseUnavailableReason {
    InvalidRequest,
    Expired,
    Disconnected,
    NoCandidates,
    Busy,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "status")]
pub enum ClauseCandidatesStatus {
    Pending,
    Ready { candidates: Vec<ClauseCandidate> },
    Unavailable { reason: ClauseUnavailableReason },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "status")]
pub enum ConvertClausesStatus {
    Pending,
    Ready { clauses: Vec<WireClause> },
    Unavailable { reason: ClauseUnavailableReason },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct CommitId {
    pub client_instance: String,
    pub sequence: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoLearningReason {
    Reading,
    Invalidated,
    NotLearningTarget,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum IntervalLearning {
    Candidate {
        token: String,
        explicitly_selected: bool,
    },
    None {
        reason: NoLearningReason,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CommitInterval {
    pub reading_start: ReadingPosition,
    pub reading_end: ReadingPosition,
    pub surface: String,
    pub learning: IntervalLearning,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CommitReceipt {
    pub commit_id: CommitId,
    pub engine_epoch: String,
    pub learning_generation: u64,
    pub reading: String,
    pub text: String,
    pub intervals: Vec<CommitInterval>,
    #[serde(deserialize_with = "required_option")]
    pub sentence_token: Option<String>,
}

fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

impl CommitReceipt {
    /// Token material is engine-owned; callers validate it before performing any learning.
    pub fn validate(
        &self,
        mut token_matches: impl FnMut(&str, &CommitInterval) -> bool,
    ) -> Result<(), ClauseValidationError> {
        if normalize_reading(&self.reading) != self.reading {
            return Err(ClauseValidationError::Reading);
        }
        let boundaries = legal_boundaries(&self.reading)?;
        let mut cursor = ReadingPosition(0);
        let mut surface = String::new();
        for interval in &self.intervals {
            if interval.reading_start != cursor
                || interval.reading_end <= cursor
                || !boundaries.contains(&interval.reading_end)
            {
                return Err(ClauseValidationError::Range);
            }
            if interval.surface.is_empty() {
                return Err(ClauseValidationError::Surface);
            }
            match &interval.learning {
                IntervalLearning::Candidate { token, .. } => {
                    if token.is_empty() || !token_matches(token, interval) {
                        return Err(ClauseValidationError::Token);
                    }
                }
                IntervalLearning::None {
                    reason: NoLearningReason::Reading,
                } => {
                    if reading_slice(&self.reading, cursor, interval.reading_end).as_deref()
                        != Some(&interval.surface)
                    {
                        return Err(ClauseValidationError::Surface);
                    }
                }
                IntervalLearning::None { .. } => {}
            }
            cursor = interval.reading_end;
            surface.push_str(&interval.surface);
        }
        if Some(&cursor) != boundaries.last() {
            return Err(ClauseValidationError::Range);
        }
        if surface != self.text {
            return Err(ClauseValidationError::Surface);
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptRejection {
    Conflict,
    Expired,
    StaleLearningGeneration,
    InvalidToken,
    InvalidIntervals,
    Capacity,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "status")]
pub enum ReceiptStatus {
    Applied,
    AlreadyProcessed,
    Rejected { reason: ReceiptRejection },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClauseValidationError {
    Reading,
    Range,
    DuplicateId,
    Surface,
    Token,
}

pub fn normalize_reading(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if ('\u{30a1}'..='\u{30f6}').contains(&c) {
                char::from_u32(c as u32 - 0x60).unwrap()
            } else {
                c
            }
        })
        .collect()
}

/// Coordinates are scalar offsets, even when one grapheme contains several scalars.
pub fn legal_boundaries(reading: &str) -> Result<Vec<ReadingPosition>, ClauseValidationError> {
    let len = u32::try_from(reading.chars().count()).map_err(|_| ClauseValidationError::Range)?;
    let mut result = vec![ReadingPosition(0)];
    for (index, scalar) in reading.chars().enumerate().skip(1) {
        if !matches!(scalar, '\u{3099}' | '\u{309a}') {
            result.push(ReadingPosition(index as u32));
        }
    }
    if len != 0 {
        result.push(ReadingPosition(len));
    }
    Ok(result)
}

pub fn reading_slice(
    reading: &str,
    start: ReadingPosition,
    end: ReadingPosition,
) -> Option<String> {
    let chars: Vec<_> = reading.chars().collect();
    if start > end {
        return None;
    }
    Some(
        chars
            .get(start.0 as usize..end.0 as usize)?
            .iter()
            .collect(),
    )
}

pub fn display_end(surface: &str) -> Result<DisplayUtf16Position, ClauseValidationError> {
    u32::try_from(surface.encode_utf16().count())
        .map(DisplayUtf16Position)
        .map_err(|_| ClauseValidationError::Range)
}

/// Full snapshots use 0..reading_len; interval conversion uses its exact requested range.
pub fn validate_clauses(
    reading: &str,
    clauses: &[WireClause],
    start: ReadingPosition,
    end: ReadingPosition,
    text: &str,
) -> Result<(), ClauseValidationError> {
    if normalize_reading(reading) != reading {
        return Err(ClauseValidationError::Reading);
    }
    let boundaries = legal_boundaries(reading)?;
    if start > end
        || boundaries.binary_search(&start).is_err()
        || boundaries.binary_search(&end).is_err()
    {
        return Err(ClauseValidationError::Range);
    }
    let mut cursor = start;
    let mut ids = HashSet::new();
    let mut surface = String::new();
    for clause in clauses {
        if !ids.insert(clause.id) {
            return Err(ClauseValidationError::DuplicateId);
        }
        if clause.reading_start != cursor
            || clause.reading_end <= cursor
            || clause.reading_end > end
            || boundaries.binary_search(&clause.reading_end).is_err()
        {
            return Err(ClauseValidationError::Range);
        }
        match clause.state {
            ClauseState::Reading => {
                if clause.candidate_token.is_some() {
                    return Err(ClauseValidationError::Token);
                }
                if reading_slice(reading, cursor, clause.reading_end).as_deref()
                    != Some(&clause.surface)
                {
                    return Err(ClauseValidationError::Surface);
                }
            }
            ClauseState::Converted => {
                if clause.candidate_token.as_deref().is_none_or(str::is_empty) {
                    return Err(ClauseValidationError::Token);
                }
                if clause.surface.is_empty() {
                    return Err(ClauseValidationError::Surface);
                }
            }
        }
        surface.push_str(&clause.surface);
        cursor = clause.reading_end;
    }
    if cursor != end {
        return Err(ClauseValidationError::Range);
    }
    if surface != text {
        return Err(ClauseValidationError::Surface);
    }
    Ok(())
}

pub fn validate_preceding(
    reading: &str,
    preceding: &[PrecedingSurface],
    target: ReadingPosition,
) -> Result<(), ClauseValidationError> {
    if normalize_reading(reading) != reading {
        return Err(ClauseValidationError::Reading);
    }
    let boundaries = legal_boundaries(reading)?;
    let mut cursor = ReadingPosition(0);
    let mut ids = HashSet::new();
    for item in preceding {
        if !ids.insert(item.clause_id) {
            return Err(ClauseValidationError::DuplicateId);
        }
        if item.reading_start != cursor
            || item.reading_end <= cursor
            || boundaries.binary_search(&item.reading_end).is_err()
        {
            return Err(ClauseValidationError::Range);
        }
        if item.surface.is_empty() {
            return Err(ClauseValidationError::Surface);
        }
        cursor = item.reading_end;
    }
    if cursor != target || boundaries.binary_search(&target).is_err() {
        return Err(ClauseValidationError::Range);
    }
    Ok(())
}
