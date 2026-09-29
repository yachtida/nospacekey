//! One latest-request slot; classification and model loading never run on the STA.
use crate::mixed_conversion::{
    spans_from_projection, validate_mixed_result, MixedIdentity, ValidatedMixed,
};
use ipc::{
    client::EngineClient,
    protocol::{Request, Response, SnapshotSegment},
};
use mixed_input::{
    classify::{classify, edge::Dictionary, model::ClassifyModel},
    plan::{InterpretationPlan, SegmentKind},
    projection::Projection,
    source::CompositionSource,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};
use std::time::{Duration, Instant};

/// Check only on the snapshot worker, and only when the engine proposes a commit.
/// Space's lower-ranked alternatives must not disable ordinary Japanese typing.
pub(crate) fn ordinary_auto_commit_is_safe(
    source: &CompositionSource,
    consumed_reading: &str,
    bundle: &mixed_input::assets::Bundle,
) -> bool {
    if consumed_reading.is_empty()
        || !source.reading_text().starts_with(consumed_reading)
        || source.source_len() > mixed_input::classify::MAX_ANALYZE_SCALARS
    {
        return false;
    }
    let reading_end = consumed_reading.chars().count() as u32;
    let Some(source_end) = source.layout().iter()
        .find(|element| element.reading.end.get() == reading_end)
        .map(|element| element.source.end.get()) else { return false; };
    classify(source, &bundle.model, &bundle.dictionary).first().is_some_and(|best|
        !best.plan.spans.iter().any(|span|
            span.kind == SegmentKind::Literal && span.range.start.get() < source_end))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub composition: u64,
    pub revision: u64,
    pub configuration: u64,
    pub connection: u64,
    pub request: u64,
}
pub(crate) struct Work {
    pub identity: Identity,
    pub source: CompositionSource,
    pub segments: Vec<SnapshotSegment>,
    pub left_context: Option<String>,
    pub forced: Option<InterpretationPlan>,
    pub live: Option<mixed_input::live::LiveState>,
    pub deadline: Instant,
}
#[derive(Clone, Debug)]
pub(crate) enum Choice {
    Ordinary {
        text: String,
        remaining: String,
        learning: Option<OrdinaryLearning>,
    },
    Mixed {
        plan: InterpretationPlan,
        projection: Projection,
        display: ValidatedMixed,
    },
}
#[derive(Clone, Debug)]
pub(crate) struct OrdinaryLearning {
    pub identity: ipc::client::EngineLearningIdentity,
    pub reading: String,
    pub token: String,
    pub sentence: bool,
}
impl Choice {
    pub fn text(&self) -> &str {
        match self {
            Self::Ordinary { text, .. } => text,
            Self::Mixed { display, .. } => &display.text,
        }
    }
}
pub(crate) struct Reply {
    pub identity: Identity,
    pub choices: Vec<Choice>,
    pub fence: mixed_input::live::CommitFence,
}
struct Shared {
    work: Mutex<Option<Work>>,
    ready: Condvar,
    reply: Mutex<Option<Reply>>,
    stopped: AtomicBool,
}
pub(crate) struct Worker {
    shared: Arc<Shared>,
}
impl Worker {
    pub fn start(pipe: String) -> Option<Self> {
        let shared = Arc::new(Shared {
            work: Mutex::new(None),
            ready: Condvar::new(),
            reply: Mutex::new(None),
            stopped: AtomicBool::new(false),
        });
        let worker = shared.clone();
        let guard = crate::globals::ComObjectGuard::new();
        std::thread::Builder::new()
            .name("nospacekey-mixed".into())
            .spawn(move || {
                let _guard = guard;
                let started = Instant::now();
                let assets = mixed_input::assets::Bundle::embedded();
                match &assets {
                    Ok(bundle) => crate::text_service::tip_log(&format!(
                        "mixed assets=ready generation={} load_us={}",
                        bundle.generation,
                        started.elapsed().as_micros()
                    )),
                    Err(reason) => crate::text_service::tip_log(&format!(
                        "mixed assets=disabled reason={reason:?}"
                    )),
                }
                loop {
                    let work = {
                        let mut slot = worker.work.lock().unwrap();
                        while slot.is_none() && !worker.stopped.load(Ordering::Acquire) {
                            slot = worker.ready.wait(slot).unwrap();
                        }
                        if worker.stopped.load(Ordering::Acquire) {
                            break;
                        }
                        slot.take().unwrap()
                    };
                    let started = Instant::now();
                    let (choices, fence) = assets
                        .as_ref()
                        .ok()
                        .and_then(|bundle| {
                            calculate(&pipe, &work, &bundle.model, &bundle.dictionary)
                        })
                        .unwrap_or_default();
                    crate::text_service::tip_log(&format!(
                        "mixed worker_ms={} choices={} expired={}",
                        started.elapsed().as_millis(),
                        choices.len(),
                        Instant::now() >= work.deadline
                    ));
                    if !worker.stopped.load(Ordering::Acquire) {
                        *worker.reply.lock().unwrap() = Some(Reply {
                            identity: work.identity,
                            choices,
                            fence,
                        });
                    }
                }
            })
            .ok()?;
        Some(Self { shared })
    }
    pub fn submit(&self, work: Work) -> bool {
        let Ok(mut slot) = self.shared.work.try_lock() else {
            return false;
        };
        *slot = Some(work);
        self.shared.ready.notify_one();
        true
    }
    pub fn reply(&self) -> Option<Reply> {
        self.shared.reply.try_lock().ok()?.take()
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        // The waiter checks the stop flag under this lock, avoiding a lost wakeup.
        let _slot = self.shared.work.lock().unwrap();
        self.shared.stopped.store(true, Ordering::Release);
        self.shared.ready.notify_one();
    }
}
fn calculate(
    pipe: &str,
    work: &Work,
    model: &ClassifyModel,
    dictionary: &Dictionary,
) -> Option<(Vec<Choice>, mixed_input::live::CommitFence)> {
    if Instant::now() >= work.deadline {
        return None;
    }
    let mut fence = mixed_input::live::CommitFence::default();
    let plans: Vec<_> = if let Some(state) = &work.live {
        let scored = classify(&work.source, model, dictionary);
        let Some(plan) = state.proposal(
            &scored,
            model.thresholds.auto_margin,
            model.thresholds.auto_margin + 2.0,
        ) else {
            return Some((vec![], fence));
        };
        if state.accepted.is_none() && !plan.spans.iter().any(|s| s.kind == SegmentKind::Literal) {
            return Some((vec![], fence));
        }
        let projection = Projection::build(1, &work.source, plan).ok()?;
        let competitors: Vec<_> = scored
            .iter()
            .filter(|p| scored[0].score - p.score <= model.thresholds.auto_margin + 2.0)
            .map(|p| p.plan.clone())
            .collect();
        fence =
            mixed_input::live::commit_fence(&work.source, &projection, &competitors, false, None);
        vec![plan.clone()]
    } else if let Some(plan) = &work.forced {
        vec![plan.clone()]
    } else {
        mixed_input::selection::mixed_candidate_plans(&classify(&work.source, model, dictionary))
            .cloned()
            .collect()
    };
    if plans.is_empty() {
        return Some((vec![], fence));
    }
    let mut client = EngineClient::connect_to(
        pipe,
        Duration::from_millis(100).min(work.deadline.saturating_duration_since(Instant::now())),
    )
    .ok()?;
    let session_reply = client
        .request_within(&Request::StartSession, work.deadline)
        .ok()?;
    let capabilities = match &session_reply {
        Response::Session { capabilities, .. } => capabilities,
        _ => return None,
    };
    if !crate::mixed_conversion::engine_supports_mixed(capabilities.as_ref()) {
        return None;
    }
    let (session, learning) = ipc::client::verify_session_metadata(session_reply).ok()?;
    let result = collect_choices(
        |request| client.request_within(request, work.deadline).ok(),
        session,
        &learning,
        work,
        plans,
    );
    // EndSession is bounded by the original deadline; dropping the pipe also retires its session.
    if Instant::now() < work.deadline {
        let _ = client.request_within(&Request::EndSession { session }, work.deadline);
    }
    result.map(|choices| (choices, fence))
}
fn collect_choices(
    mut send: impl FnMut(&Request) -> Option<Response>,
    session: i64,
    learning: &ipc::client::EngineLearningIdentity,
    work: &Work,
    plans: Vec<InterpretationPlan>,
) -> Option<Vec<Choice>> {
    let id = work.identity;
    let mut choices = Vec::new();
    if work.live.is_none() {
        let response = send(&Request::LiveSnapshot {
            include_flat_candidates: true,
            composition: id.composition,
            revision: id.revision,
            configuration_generation: id.configuration,
            connection_generation: id.connection,
            conversion_revision: 0,
            request_id: id.request,
            segments: work.segments.clone(),
            explicit: true,
            live_search_width: None,
            left_context: work.left_context.clone(),
        })?;
        choices = match response {
            Response::SnapshotResult {
                composition,
                revision,
                configuration_generation,
                connection_generation,
                text,
                candidates,
                candidate_remaining,
                clause_data,
                baseline,
                ..
            } if (
                composition,
                revision,
                configuration_generation,
                connection_generation,
                clause_data.request_id,
            ) == (
                id.composition,
                id.revision,
                id.configuration,
                id.connection,
                id.request,
            ) && clause_data.validate(&text).is_ok() =>
            {
                let texts = candidates
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| vec![text]);
                let remaining =
                    candidate_remaining.unwrap_or_else(|| vec![String::new(); texts.len()]);
                if remaining.len() != texts.len() {
                    return None;
                }
                let request = ipc::clause::ClauseCandidatesRequest {
                    key: ipc::clause::ClauseRequestKey {
                        identity: ipc::clause::SnapshotIdentity { composition, revision,
                            configuration_generation, connection_generation },
                        baseline, conversion_revision: clause_data.conversion_revision,
                        clause_id: clause_data.clauses.first()?.id, request_id: id.request,
                    },
                    reading: clause_data.reading.clone(),
                    reading_start: ipc::clause::ReadingPosition(0),
                    reading_end: ipc::clause::ReadingPosition(clause_data.reading.chars().count() as u32),
                    preceding_surfaces: vec![], include_prefix_candidates: true,
                };
                let tokens = if let Some(candidates) = &clause_data.flat_candidates {
                    if request.validate_candidates(candidates).is_ok() { candidates.clone() } else { vec![] }
                } else { match send(&Request::ClauseCandidates(request.clone())) {
                    Some(Response::ClauseCandidatesResult { key, status: ipc::clause::ClauseCandidatesStatus::Ready { candidates } })
                        if key == request.key && request.validate_candidates(&candidates).is_ok() => candidates,
                    _ => vec![],
                } };
                texts
                    .into_iter()
                    .zip(remaining)
                    .map(|(text, remaining)| {
                        let metadata = clause_data.reading.strip_suffix(&remaining).and_then(|consumed| {
                            tokens.iter().find(|candidate| candidate.surface == text
                                && candidate.reading_end.0 == consumed.chars().count() as u32)
                                .map(|candidate| OrdinaryLearning { identity: learning.clone(),
                                    reading: consumed.into(), token: candidate.token.clone(), sentence: remaining.is_empty() })
                        });
                        Choice::Ordinary { text, remaining, learning: metadata }
                    })
                    .collect::<Vec<_>>()
            }
            _ => return None,
        };
    }
    for (index, plan) in plans.into_iter().enumerate() {
        if Instant::now() >= work.deadline {
            break;
        }
        let projection = match Projection::build(index as u64 + 1, &work.source, &plan) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let spans = spans_from_projection(&projection);
        let identity = MixedIdentity {
            composition: id.composition,
            revision: id.revision,
            configuration_generation: id.configuration,
            connection_generation: id.connection,
            request_id: id.request,
            source_revision: work.source.revision(),
            plan_id: projection.plan_id,
        };
        let response = send(&Request::MixedConvert {
            session,
            composition: id.composition,
            revision: id.revision,
            configuration_generation: id.configuration,
            connection_generation: id.connection,
            conversion_revision: 0,
            request_id: id.request,
            source_revision: work.source.revision(),
            plan_id: projection.plan_id,
            spans: spans.clone(),
            left_context: work.left_context.clone(),
        });
        if let Some(Response::MixedResult {
            composition,
            revision,
            configuration_generation,
            connection_generation,
            request_id,
            source_revision,
            plan_id,
            engine_epoch,
            learning_generation,
            text,
            spans: result_spans,
        }) = response
        {
            if let Some(display) = validate_mixed_result(
                &identity,
                &spans,
                composition,
                revision,
                configuration_generation,
                connection_generation,
                request_id,
                source_revision,
                plan_id,
                &engine_epoch,
                learning_generation,
                &text,
                &result_spans,
            ) {
                choices.push(Choice::Mixed {
                    plan,
                    projection,
                    display,
                });
            }
        }
    }
    Some(choices)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_auto_commit_keeps_unselected_english_and_source_units_editable() {
        use mixed_input::classify::tune::source_from_str;
        let bundle = mixed_input::assets::Bundle::embedded().unwrap();
        let mixed = source_from_str("githubnotukaikatawosetsumeisurunodenagaibunshouwoutimasu");
        assert!(!ordinary_auto_commit_is_safe(&mixed, "ぎて", &bundle));
        assert!(!ordinary_auto_commit_is_safe(&mixed, "ぎ", &bundle));
        assert!(!ordinary_auto_commit_is_safe(&mixed, "ぎてゅb", &bundle));
        let japanese = source_from_str("kyouhaiitenkinodenagaibunshouwoutimasu");
        assert!(ordinary_auto_commit_is_safe(&japanese, "きょうは", &bundle));
        assert!(!ordinary_auto_commit_is_safe(&japanese, "き", &bundle));
        let mut composer = crate::local_kana_composer::LocalKanaComposer::default();
        for ch in "kyouhaiitenkinanodenagaibunshouwoutitsuduketeimasu".chars() {
            composer.push(ch, crate::local_kana_composer::InputStyle::Kana);
        }
        let japanese = composer.composition_source(1);
        assert!(ordinary_auto_commit_is_safe(&japanese, "きょうはいい", &bundle),
            "{:?}", classify(&japanese, &bundle.model, &bundle.dictionary).first());
        let too_long = source_from_str(&"a".repeat(257));
        assert!(!ordinary_auto_commit_is_safe(&too_long, "あ", &bundle));
    }

    #[test]
    fn ordinary_japanese_space_keeps_clause_conversion_and_github_still_offers_mixed() {
        let bundle = mixed_input::assets::Bundle::embedded().unwrap();
        for text in ["kyouhaiitenkidesu", "nihongo", "watashihagakkouheikimasu"] {
            let source = mixed_input::classify::tune::source_from_str(text);
            let plans = classify(&source, &bundle.model, &bundle.dictionary);
            assert!(mixed_input::selection::mixed_candidate_plans(&plans).next().is_none(),
                "lower-ranked Literal alternatives must not replace Japanese clauses: {text}");
            let mut request = work();
            request.source = source;
            let (choices, _) = calculate("unused-no-engine-needed", &request, &bundle.model, &bundle.dictionary).unwrap();
            assert!(choices.is_empty(), "the STA must enter its ordinary clause path");
        }
        for text in ["githubnotukaikata", "gazounoyounigithub"] {
            let source = mixed_input::classify::tune::source_from_str(text);
            let plans = classify(&source, &bundle.model, &bundle.dictionary);
            assert!(mixed_input::selection::mixed_candidate_plans(&plans).next().is_some(), "{text}");
        }
    }
    fn work() -> Work {
        Work {
            identity: Identity {
                composition: 7,
                revision: 2,
                configuration: 3,
                connection: 4,
                request: 9,
            },
            source: mixed_input::classify::tune::source_from_str("made"),
            segments: vec![],
            left_context: None,
            forced: None,
            live: None,
            deadline: Instant::now() + Duration::from_secs(1),
        }
    }
    fn normal(work: &Work) -> Response {
        let id = work.identity;
        let reading = work.source.reading_text();
        Response::SnapshotResult {
            clause_data: ipc::clause::SnapshotClauseData::from_reading(
                reading.clone(),
                0,
                id.request,
            ),
            composition: id.composition,
            revision: id.revision,
            configuration_generation: id.configuration,
            connection_generation: id.connection,
            text: reading,
            candidates: Some(vec!["まで".into(), "迄".into()]),
            candidate_remaining: Some(vec![String::new(), String::new()]),
            baseline: 1,
            auto_commit: None,
        }
    }

    #[test]
    fn snapshot_owned_tokens_do_not_requery_or_reorder_candidates() {
        let work = work();
        let identity = ipc::client::EngineLearningIdentity { engine_epoch: "epoch".into(), learning_generation: 1 };
        let choices = collect_choices(|request| {
            assert!(matches!(request, Request::LiveSnapshot { include_flat_candidates: true, .. }), "snapshot token pool needs no second search");
            let mut response = normal(&work);
            if let Response::SnapshotResult { clause_data, .. } = &mut response {
                clause_data.flat_candidates = Some(vec![
                    ipc::clause::ClauseCandidate { surface: "まで".into(), token: "first".into(), reading_start: ipc::clause::ReadingPosition(0), reading_end: ipc::clause::ReadingPosition(2) },
                    ipc::clause::ClauseCandidate { surface: "迄".into(), token: "chosen".into(), reading_start: ipc::clause::ReadingPosition(0), reading_end: ipc::clause::ReadingPosition(2) },
                ]);
            }
            Some(response)
        }, 1, &identity, &work, vec![]).unwrap();
        assert_eq!(choices.iter().map(Choice::text).collect::<Vec<_>>(), ["まで", "迄"]);
        assert!(matches!(&choices[1], Choice::Ordinary { learning: Some(metadata), .. } if metadata.token == "chosen" && metadata.sentence));
    }

    #[test]
    fn ordinary_learning_is_bound_to_selected_surface_consumed_reading_and_reply_key() {
        for stale in [false, true] {
            let mut work = work();
            work.source = mixed_input::classify::tune::source_from_str("toukyouniiku");
            let identity = ipc::client::EngineLearningIdentity { engine_epoch: "epoch".into(), learning_generation: 7 };
            let choices = collect_choices(|request| match request {
                Request::LiveSnapshot { .. } => {
                    let mut response = normal(&work);
                    if let Response::SnapshotResult { candidates, candidate_remaining, .. } = &mut response {
                        *candidates = Some(vec!["東京に行く".into(), "東京".into()]);
                        *candidate_remaining = Some(vec!["".into(), "にいく".into()]);
                    }
                    Some(response)
                }
                Request::ClauseCandidates(request) => {
                    assert!(request.include_prefix_candidates);
                    let mut key = request.key;
                    if stale { key.request_id += 1; }
                    Some(Response::ClauseCandidatesResult { key, status: ipc::clause::ClauseCandidatesStatus::Ready {
                        candidates: vec![
                            ipc::clause::ClauseCandidate { surface: "東京に行く".into(), token: "full".into(), reading_start: ipc::clause::ReadingPosition(0), reading_end: ipc::clause::ReadingPosition(8) },
                            ipc::clause::ClauseCandidate { surface: "東京".into(), token: "prefix".into(), reading_start: ipc::clause::ReadingPosition(0), reading_end: ipc::clause::ReadingPosition(5) },
                        ],
                    }})
                }
                _ => None,
            }, 1, &identity, &work, vec![]).unwrap();
            assert_eq!(choices.iter().map(Choice::text).collect::<Vec<_>>(), ["東京に行く", "東京"]);
            let Choice::Ordinary { remaining, learning, .. } = &choices[1] else { panic!() };
            assert_eq!(remaining, "にいく");
            if stale { assert!(learning.is_none()); }
            else {
                let metadata = learning.as_ref().unwrap();
                assert_eq!(metadata.reading, "とうきょう");
                assert_eq!(metadata.token, "prefix");
                assert_eq!(metadata.identity, identity);
                assert!(matches!(&choices[0], Choice::Ordinary { learning: Some(meta), .. } if meta.token == "full"));
            }
        }
    }

    #[test]
    fn mixed_candidates_follow_normal_order_and_keep_independent_projections() {
        let work = work();
        let plan =
            InterpretationPlan::build("made", &[(SegmentKind::Literal, "made".into())]).unwrap();
        let choices = collect_choices(
            |request| match request {
                Request::LiveSnapshot { .. } => Some(normal(&work)),
                Request::MixedConvert {
                    composition,
                    revision,
                    configuration_generation,
                    connection_generation,
                    request_id,
                    source_revision,
                    plan_id,
                    ..
                } => Some(Response::MixedResult {
                    composition: *composition,
                    revision: *revision,
                    configuration_generation: *configuration_generation,
                    connection_generation: *connection_generation,
                    request_id: *request_id,
                    source_revision: *source_revision,
                    plan_id: *plan_id,
                    engine_epoch: "test".into(),
                    learning_generation: 1,
                    text: "made".into(),
                    spans: vec![ipc::protocol::MixedSpanResult {
                        kind: "literal".into(),
                        reading_start: 0,
                        reading_end: 4,
                        text: "made".into(),
                        candidate_token: None,
                    }],
                }),
                _ => None,
            },
            1,
            &ipc::client::EngineLearningIdentity { engine_epoch: "test".into(), learning_generation: 1 },
            &work,
            vec![plan],
        )
        .unwrap();
        assert_eq!(
            choices.iter().map(Choice::text).collect::<Vec<_>>(),
            ["まで", "迄", "made"]
        );
        assert_eq!(work.source.reading_text(), "まで");
        assert!(
            matches!(&choices[2], Choice::Mixed { projection, .. } if projection.reading() == "made")
        );
    }
    #[test]
    fn stale_normal_response_never_becomes_a_candidate() {
        let work = work();
        let choices = collect_choices(
            |_| {
                let mut response = normal(&work);
                if let Response::SnapshotResult { revision, .. } = &mut response {
                    *revision += 1;
                }
                Some(response)
            },
            1,
            &ipc::client::EngineLearningIdentity { engine_epoch: "test".into(), learning_generation: 1 },
            &work,
            vec![],
        );
        assert!(choices.is_none());
    }
    #[test]
    fn mixed_failure_retains_original_normal_candidates() {
        let work = work();
        let plan =
            InterpretationPlan::build("made", &[(SegmentKind::Literal, "made".into())]).unwrap();
        let choices = collect_choices(
            |request| match request {
                Request::LiveSnapshot { .. } => Some(normal(&work)),
                _ => None,
            },
            1,
            &ipc::client::EngineLearningIdentity { engine_epoch: "test".into(), learning_generation: 1 },
            &work,
            vec![plan],
        )
        .unwrap();
        assert_eq!(
            choices.iter().map(Choice::text).collect::<Vec<_>>(),
            ["まで", "迄"]
        );
    }
}
