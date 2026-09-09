//! Confirmed document writes outlive composition state and their TSF owner.
use crate::candidate_window::CandidateUI;
use ipc::clause::{CommitReceipt, ReceiptRejection, ReceiptStatus};
use ipc::client::{EngineClient, VerifiedEngineClient};
use ipc::protocol::{Request, Response};
use std::collections::{HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const CAPACITY: usize = 128;
const LIFETIME: Duration = Duration::from_secs(30);
const REQUEST_BUDGET: Duration = Duration::from_millis(1200);
const RETRIES: [Duration; 2] = [Duration::from_millis(250), Duration::from_millis(1000)];

fn next_commit_id() -> Option<ipc::clause::CommitId> {
    static INSTANCE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let client_instance = INSTANCE
        .get_or_init(|| unsafe {
            windows::Win32::System::Com::CoCreateGuid()
                .ok()
                .map(|id| format!("{id:?}"))
        })
        .as_ref()?
        .clone();
    let sequence = SEQUENCE
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .ok()?
        + 1;
    Some(ipc::clause::CommitId {
        client_instance,
        sequence,
    })
}

fn freeze(model: &crate::clause_conversion::ClauseConversion, text: &str) -> Option<CommitReceipt> {
    use crate::clause_conversion::SurfaceSource;
    use ipc::clause::{CommitInterval, IntervalLearning, NoLearningReason};
    let identity = model.learning_identity.as_ref()?;
    if model.text() != text {
        return None;
    }
    let receipt = CommitReceipt {
        commit_id: next_commit_id()?,
        engine_epoch: identity.engine_epoch.clone(),
        learning_generation: identity.learning_generation,
        reading: model.reading.clone(),
        text: text.into(),
        intervals: model
            .clauses
            .iter()
            .map(|clause| CommitInterval {
                reading_start: clause.start,
                reading_end: clause.end,
                surface: clause.surface.clone(),
                learning: match &clause.source {
                    SurfaceSource::Candidate {
                        token,
                        explicitly_selected,
                    } => IntervalLearning::Candidate {
                        token: token.clone(),
                        explicitly_selected: *explicitly_selected,
                    },
                    SurfaceSource::Reading => IntervalLearning::None {
                        reason: NoLearningReason::Reading,
                    },
                    SurfaceSource::LocalSurface => IntervalLearning::None {
                        reason: NoLearningReason::Invalidated,
                    },
                },
            })
            .collect(),
        sentence_token: model.sentence_token.clone(),
    };
    receipt.validate(|_, _| true).ok()?;
    Some(receipt)
}

impl crate::text_service::TextService_Impl {
    pub(crate) fn poll_receipt_notice(&self) {
        let invalidated = self.receipt_outbox.borrow().as_ref()
            .is_some_and(ReceiptOutbox::take_learning_invalidation);
        if invalidated {
            self.clause_worker.refresh_learning();
            let changed = {
                let outbox = self.receipt_outbox.borrow();
                let mut model = self.local_clauses.borrow_mut();
                match (outbox.as_ref(), model.as_mut()) {
                    (Some(outbox), Some(model)) => outbox.invalidate_model(model),
                    _ => false,
                }
            };
            if changed {
                self.candidate_ui.borrow_mut().hide();
            }
        }
        let notice = self
            .receipt_outbox
            .borrow()
            .as_ref()
            .and_then(ReceiptOutbox::take_notice);
        if let Some(notice) = notice {
            crate::text_service::tip_log(&format!("ev=receipt_delivery_notice reason={notice:?}"));
            let theme = self.appearance.borrow_mut().current_theme();
            self.mode_hud.borrow_mut().flash_learning_failure(theme);
        }
    }

    pub(crate) fn prepare_commit_receipt(&self, text: &str) -> Option<Box<dyn FnOnce()>> {
        let receipt = freeze(self.local_clauses.borrow().as_ref()?, text)?;
        let mut outbox = self.receipt_outbox.borrow_mut();
        if outbox.is_none() {
            *outbox = ReceiptOutbox::start(crate::engine_link::stable_pipe_name()).ok();
        }
        let outbox = outbox.as_ref()?.clone();
        Some(Box::new(move || {
            outbox.text_applied(receipt, Instant::now());
        }))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeliveryNotice {
    Capacity,
    Expired,
    Failed,
    EpochChanged,
    Rejected(ReceiptRejection),
}

struct Delivery {
    receipt: CommitReceipt,
    expires: Instant,
    next_attempt: Instant,
    attempts: usize,
}

impl Delivery {
    fn new(receipt: CommitReceipt, applied_at: Instant) -> Self {
        Self {
            receipt,
            expires: applied_at + LIFETIME,
            next_attempt: applied_at,
            attempts: 0,
        }
    }

    fn retry(&mut self, now: Instant) -> bool {
        let Some(delay) = self
            .attempts
            .checked_sub(1)
            .and_then(|index| RETRIES.get(index))
        else {
            return false;
        };
        self.next_attempt = now + *delay;
        self.next_attempt < self.expires
    }
}

enum DeliveryResult {
    Acknowledged(ReceiptStatus),
    Retry,
    EpochChanged,
}

trait ReceiptTransport {
    fn deliver(&mut self, receipt: &CommitReceipt, deadline: Instant) -> DeliveryResult;
}

struct EngineReceiptTransport {
    pipe: String,
    client: Option<VerifiedEngineClient>,
}

impl ReceiptTransport for EngineReceiptTransport {
    fn deliver(&mut self, receipt: &CommitReceipt, deadline: Instant) -> DeliveryResult {
        if self.client.is_none() {
            self.client = EngineClient::connect_verified_to(
                &self.pipe,
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(500)),
                deadline,
            )
            .ok();
        }
        let Some(client) = self.client.as_mut() else {
            return DeliveryResult::Retry;
        };
        if client.learning_identity().engine_epoch != receipt.engine_epoch {
            return DeliveryResult::EpochChanged;
        }
        // Send the original generation: the engine must consult its ledger
        // before rejecting an unprocessed receipt from an older generation.
        match client.request_within(&Request::CommitReceipt(receipt.clone()), deadline) {
            Ok(Response::CommitReceiptAck { commit_id, status })
                if commit_id == receipt.commit_id =>
            {
                if matches!(status, ReceiptStatus::Applied | ReceiptStatus::AlreadyProcessed) {
                    crate::text_service::tip_log(&format!("ev=receipt_ack sequence={}", commit_id.sequence));
                }
                DeliveryResult::Acknowledged(status)
            }
            _ => {
                self.client = None;
                DeliveryResult::Retry
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct ReceiptOutbox {
    sender: SyncSender<Delivery>,
    outstanding: Arc<AtomicUsize>,
    notice: Arc<Mutex<Option<DeliveryNotice>>>,
    learning_invalidated: Arc<AtomicBool>,
    rejected_generations: Arc<Mutex<HashMap<String, u64>>>,
}

impl ReceiptOutbox {
    pub fn start(pipe: String) -> std::io::Result<Self> {
        Self::start_with(EngineReceiptTransport { pipe, client: None })
    }

    fn start_with(transport: impl ReceiptTransport + Send + 'static) -> std::io::Result<Self> {
        let (sender, receiver) = sync_channel(CAPACITY);
        let outstanding = Arc::new(AtomicUsize::new(0));
        let notice = Arc::new(Mutex::new(None));
        let learning_invalidated = Arc::new(AtomicBool::new(false));
        let worker_invalidated = learning_invalidated.clone();
        let rejected_generations = Arc::new(Mutex::new(HashMap::new()));
        let worker_rejected = rejected_generations.clone();
        let worker_count = outstanding.clone();
        let worker_notice = notice.clone();
        let guard = crate::globals::ComObjectGuard::new();
        std::thread::Builder::new()
            .name("nospacekey-receipts".into())
            .spawn(move || {
                let _guard = guard;
                run(receiver, worker_count, worker_notice, worker_invalidated, worker_rejected, transport);
            })?;
        Ok(Self {
            sender,
            outstanding,
            notice,
            learning_invalidated,
            rejected_generations,
        })
    }

    /// Invoke only after SetText/InsertTextAtSelection succeeded. Count includes
    /// queued, in-flight and retrying deliveries, so capacity is exactly 128.
    pub fn text_applied(&self, receipt: CommitReceipt, applied_at: Instant) -> bool {
        if self
            .outstanding
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < CAPACITY).then_some(count + 1)
            })
            .is_err()
        {
            *self.notice.lock().unwrap() = Some(DeliveryNotice::Capacity);
            return false;
        }
        if self
            .sender
            .try_send(Delivery::new(receipt, applied_at))
            .is_err()
        {
            *self.notice.lock().unwrap() = Some(DeliveryNotice::Failed);
            self.outstanding.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        true
    }

    pub fn take_notice(&self) -> Option<DeliveryNotice> {
        self.notice.lock().unwrap().take()
    }
    pub fn take_learning_invalidation(&self) -> bool {
        self.learning_invalidated.swap(false, Ordering::AcqRel)
    }
    /// Retain rejection watermarks after the notification is consumed, so a
    /// late initial snapshot cannot reintroduce rejected learning materials.
    pub fn invalidate_model(&self, model: &mut crate::clause_conversion::ClauseConversion) -> bool {
        let rejected = model.learning_identity.as_ref().is_some_and(|identity| self.rejects_identity(identity));
        if rejected { model.invalidate_learning(); }
        rejected
    }

    pub fn configure_model(&self, model: &mut crate::clause_conversion::ClauseConversion,
        identity: Option<ipc::client::EngineLearningIdentity>) {
        if identity.as_ref().is_some_and(|identity| self.rejects_identity(identity)) {
            // A delayed, rejected acknowledgement cannot retire newer candidates.
            self.invalidate_model(model);
        } else {
            model.configured_learning_identity(identity);
        }
    }
    pub fn rejects_identity(&self, identity: &ipc::client::EngineLearningIdentity) -> bool {
        self.rejected_generations.lock().unwrap().get(&identity.engine_epoch)
            .is_some_and(|generation| identity.learning_generation <= *generation)
    }
    pub fn pending(&self) -> bool {
        self.outstanding.load(Ordering::Acquire) != 0
            || self.learning_invalidated.load(Ordering::Acquire)
            || self.notice.lock().unwrap().is_some()
    }
}

fn run(
    receiver: Receiver<Delivery>,
    outstanding: Arc<AtomicUsize>,
    notice: Arc<Mutex<Option<DeliveryNotice>>>,
    learning_invalidated: Arc<AtomicBool>,
    rejected_generations: Arc<Mutex<HashMap<String, u64>>>,
    mut transport: impl ReceiptTransport,
) {
    let mut pending: VecDeque<Delivery> = VecDeque::new();
    let mut disconnected = false;
    loop {
        pending.extend(receiver.try_iter());
        let now = Instant::now();
        if let Some(index) = pending
            .iter()
            .position(|item| item.next_attempt <= now || item.expires <= now)
        {
            let mut item = pending.remove(index).unwrap();
            let terminal = if now >= item.expires {
                Some(Some(DeliveryNotice::Expired))
            } else {
                item.attempts += 1;
                match transport.deliver(&item.receipt, (now + REQUEST_BUDGET).min(item.expires)) {
                    DeliveryResult::Acknowledged(
                        ReceiptStatus::Applied | ReceiptStatus::AlreadyProcessed,
                    ) => Some(None),
                    DeliveryResult::Acknowledged(ReceiptStatus::Rejected { reason }) => {
                        Some(Some(DeliveryNotice::Rejected(reason)))
                    }
                    DeliveryResult::EpochChanged => Some(Some(DeliveryNotice::EpochChanged)),
                    DeliveryResult::Retry if item.retry(Instant::now()) => None,
                    DeliveryResult::Retry => Some(Some(if Instant::now() >= item.expires {
                        DeliveryNotice::Expired
                    } else {
                        DeliveryNotice::Failed
                    })),
                }
            };
            if let Some(message) = terminal {
                if let Some(message) = message {
                    if matches!(message, DeliveryNotice::EpochChanged
                        | DeliveryNotice::Rejected(ReceiptRejection::StaleLearningGeneration)) {
                        // Independent of the coalesced HUD notice: a later
                        // capacity/failure message must not erase invalidation.
                        let generation = if matches!(message, DeliveryNotice::EpochChanged) {
                            u64::MAX
                        } else {
                            item.receipt.learning_generation
                        };
                        rejected_generations.lock().unwrap()
                            .entry(item.receipt.engine_epoch.clone())
                            .and_modify(|old| *old = (*old).max(generation))
                            .or_insert(generation);
                        learning_invalidated.store(true, Ordering::Release);
                    }
                    crate::text_service::tip_log(&format!(
                        "ev=receipt_delivery_failed reason={message:?}"
                    ));
                    *notice.lock().unwrap() = Some(message);
                }
                // Publish a failure before releasing the last outstanding
                // count, so the STA cannot observe an idle outbox in between.
                outstanding.fetch_sub(1, Ordering::AcqRel);
            } else {
                pending.push_back(item);
            }
            continue;
        }
        if disconnected && pending.is_empty() {
            return;
        }
        let wait = pending
            .iter()
            .map(|item| {
                item.next_attempt
                    .min(item.expires)
                    .saturating_duration_since(now)
            })
            .min()
            .unwrap_or(Duration::from_millis(50))
            .min(Duration::from_millis(50));
        if disconnected {
            std::thread::sleep(wait);
        } else {
            match receiver.recv_timeout(wait) {
                Ok(item) => pending.push_back(item),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => disconnected = true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipc::clause::{
        CommitId, CommitInterval, IntervalLearning, NoLearningReason, ReadingPosition,
    };
    use std::sync::mpsc::{channel, Sender};

    fn receipt() -> CommitReceipt {
        CommitReceipt {
            commit_id: CommitId {
                client_instance: "11111111-1111-4111-8111-111111111111".into(),
                sequence: 1,
            },
            engine_epoch: "original-engine".into(),
            learning_generation: 7,
            reading: "あ".into(),
            text: "あ".into(),
            sentence_token: None,
            intervals: vec![CommitInterval {
                reading_start: ReadingPosition(0),
                reading_end: ReadingPosition(1),
                surface: "あ".into(),
                learning: IntervalLearning::None {
                    reason: NoLearningReason::Reading,
                },
            }],
        }
    }

    struct Script {
        results: VecDeque<DeliveryResult>,
        events: Arc<Mutex<Vec<(CommitReceipt, Instant)>>>,
        done: Sender<()>,
        block: Option<(Sender<()>, Receiver<()>)>,
    }
    impl ReceiptTransport for Script {
        fn deliver(&mut self, receipt: &CommitReceipt, deadline: Instant) -> DeliveryResult {
            let now = Instant::now();
            assert!(deadline <= now + REQUEST_BUDGET);
            self.events.lock().unwrap().push((receipt.clone(), now));
            if let Some((entered, release)) = self.block.take() {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(3)).unwrap();
            }
            self.results
                .pop_front()
                .unwrap_or(DeliveryResult::Acknowledged(ReceiptStatus::Applied))
        }
    }
    impl Drop for Script {
        fn drop(&mut self) {
            let _ = self.done.send(());
        }
    }

    #[test]
    fn retry_keeps_id_payload_and_schedule_after_owner_drops() {
        let (done, finished) = channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let outbox = ReceiptOutbox::start_with(Script {
            results: VecDeque::from([
                DeliveryResult::Retry,
                DeliveryResult::Retry,
                DeliveryResult::Acknowledged(ReceiptStatus::AlreadyProcessed),
            ]),
            events: events.clone(),
            done,
            block: None,
        })
        .unwrap();
        let payload = receipt();
        assert!(outbox.text_applied(payload.clone(), Instant::now()));
        drop(outbox);
        finished.recv_timeout(Duration::from_secs(4)).unwrap();
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|(actual, _)| actual == &payload));
        assert!(events[1].1.duration_since(events[0].1) >= RETRIES[0]);
        assert!(events[2].1.duration_since(events[1].1) >= RETRIES[1]);
    }

    #[test]
    fn capacity_includes_in_flight_and_does_not_evict_accepted_receipts() {
        let (done, finished) = channel();
        let (entered, started) = channel();
        let (release, blocked) = channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let outbox = ReceiptOutbox::start_with(Script {
            results: VecDeque::new(),
            events: events.clone(),
            done,
            block: Some((entered, blocked)),
        })
        .unwrap();
        assert!(outbox.text_applied(receipt(), Instant::now()));
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        for sequence in 2..=128 {
            let mut next = receipt();
            next.commit_id.sequence = sequence;
            assert!(outbox.text_applied(next, Instant::now()));
        }
        assert!(!outbox.text_applied(receipt(), Instant::now()));
        assert_eq!(outbox.take_notice(), Some(DeliveryNotice::Capacity));
        release.send(()).unwrap();
        drop(outbox);
        finished.recv_timeout(Duration::from_secs(3)).unwrap();
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 128);
        assert_eq!(events.last().unwrap().0.commit_id.sequence, 128);
    }

    #[test]
    fn expired_and_permanent_failures_are_terminal() {
        for (age, result, expected, attempts) in [
            (LIFETIME, DeliveryResult::Retry, DeliveryNotice::Expired, 0),
            (
                Duration::ZERO,
                DeliveryResult::EpochChanged,
                DeliveryNotice::EpochChanged,
                1,
            ),
            (
                Duration::ZERO,
                DeliveryResult::Acknowledged(ReceiptStatus::Rejected {
                    reason: ReceiptRejection::StaleLearningGeneration,
                }),
                DeliveryNotice::Rejected(ReceiptRejection::StaleLearningGeneration),
                1,
            ),
        ] {
            let (done, finished) = channel();
            let events = Arc::new(Mutex::new(Vec::new()));
            let outbox = ReceiptOutbox::start_with(Script {
                results: VecDeque::from([result]),
                events: events.clone(),
                done,
                block: None,
            })
            .unwrap();
            let notice = outbox.notice.clone();
            assert!(outbox.text_applied(receipt(), Instant::now() - age));
            drop(outbox);
            finished.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(*notice.lock().unwrap(), Some(expected));
            assert_eq!(events.lock().unwrap().len(), attempts);
        }
    }

    #[test]
    fn retry_never_extends_the_original_lifetime_or_exceeds_three_attempts() {
        let applied = Instant::now();
        let mut delivery = Delivery::new(receipt(), applied);
        delivery.attempts = 1;
        assert!(delivery.retry(applied));
        delivery.attempts = 2;
        assert!(delivery.retry(applied + Duration::from_secs(1)));
        delivery.attempts = 3;
        assert!(!delivery.retry(applied + Duration::from_secs(2)));
        delivery.attempts = 1;
        assert!(!delivery.retry(applied + LIFETIME - Duration::from_millis(100)));
        assert_eq!(delivery.expires, applied + LIFETIME);
    }

    #[test]
    fn a_fast_failure_keeps_polling_until_its_notice_is_consumed() {
        let (done, finished) = channel();
        let outbox = ReceiptOutbox::start_with(Script {
            results: VecDeque::from([DeliveryResult::EpochChanged]), events: Arc::new(Mutex::new(Vec::new())),
            done, block: None,
        }).unwrap();
        assert!(outbox.text_applied(receipt(), Instant::now()));
        let deadline = Instant::now() + Duration::from_secs(1);
        while outbox.outstanding.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(outbox.pending());
        assert_eq!(outbox.take_notice(), Some(DeliveryNotice::EpochChanged));
        assert!(outbox.pending());
        *outbox.notice.lock().unwrap() = Some(DeliveryNotice::Capacity);
        assert_eq!(outbox.take_notice(), Some(DeliveryNotice::Capacity));
        assert!(outbox.take_learning_invalidation());
        assert!(!outbox.take_learning_invalidation());
        assert!(!outbox.pending());
        drop(outbox);
        finished.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn rejection_watermark_survives_notification_and_preserves_new_generations() {
        use crate::clause_conversion::{ClauseConversion, SurfaceSource};
        use ipc::clause::{ClauseId, ClauseState, SnapshotClauseData, SnapshotIdentity, WireClause};
        let (sender, _receiver) = sync_channel(CAPACITY);
        let outbox = ReceiptOutbox {
            sender, outstanding: Arc::new(AtomicUsize::new(0)),
            notice: Arc::new(Mutex::new(None)),
            learning_invalidated: Arc::new(AtomicBool::new(true)),
            rejected_generations: Arc::new(Mutex::new(HashMap::from([("old".into(), 7)]))),
        };
        assert!(outbox.take_learning_invalidation()); // Initial still pending.
        let mut model = ClauseConversion::from_snapshot(
            SnapshotIdentity { composition: 1, revision: 1, configuration_generation: 1, connection_generation: 1 },
            1, SnapshotClauseData { reading: "あ".into(), conversion_revision: 0, request_id: 1,
                sentence_token: Some("sentence".into()), clauses: vec![WireClause {
                    id: ClauseId(1), reading_start: ReadingPosition(0), reading_end: ReadingPosition(1),
                    state: ClauseState::Converted, surface: "亜".into(), candidate_token: Some("candidate".into()),
                }] }, "亜").unwrap();
        for (epoch, generation, rejected) in [("old", 8, false), ("new", 1, false), ("old", 7, true)] {
            model.learning_identity = Some(ipc::client::EngineLearningIdentity {
                engine_epoch: epoch.into(), learning_generation: generation,
            });
            let mut configured = model.clone();
            outbox.configure_model(&mut configured, Some(ipc::client::EngineLearningIdentity {
                engine_epoch: "old".into(), learning_generation: 7,
            }));
            assert_eq!(configured.learning_identity.is_none(), rejected);
            assert_eq!(configured.text(), "亜");
            if !rejected {
                assert_eq!(configured.learning_identity, model.learning_identity);
                assert_eq!(configured.clauses, model.clauses);
            }
            assert_eq!(outbox.invalidate_model(&mut model), rejected);
            assert_eq!(model.text(), "亜");
            if !rejected {
                assert!(matches!(model.clauses[0].source, SurfaceSource::Candidate { .. }));
                assert!(model.sentence_token.is_some());
            }
        }
        assert_eq!(model.clauses[0].source, SurfaceSource::LocalSurface);
        assert!(model.learning_identity.is_none() && model.sentence_token.is_none());
    }

    #[test]
    fn frozen_receipt_keeps_non_learning_intervals_and_original_generation() {
        use crate::clause_conversion::{ClauseConversion, SurfaceSource};
        use ipc::clause::{
            ClauseId, ClauseState, SnapshotClauseData, SnapshotIdentity, WireClause,
        };
        let mut model = ClauseConversion::from_snapshot(
            SnapshotIdentity {
                composition: 1,
                revision: 1,
                configuration_generation: 1,
                connection_generation: 1,
            },
            1,
            SnapshotClauseData {
                reading: "あいう".into(),
                conversion_revision: 0,
                request_id: 0,
                sentence_token: None,
                clauses: (0..3)
                    .map(|index| WireClause {
                        id: ClauseId(index + 1),
                        reading_start: ReadingPosition(index as u32),
                        reading_end: ReadingPosition(index as u32 + 1),
                        state: ClauseState::Reading,
                        surface: ["あ", "い", "う"][index as usize].into(),
                        candidate_token: None,
                    })
                    .collect(),
            },
            "あいう",
        )
        .unwrap();
        assert!(freeze(&model, "あいう").is_none());
        model.learning_identity = Some(ipc::client::EngineLearningIdentity {
            engine_epoch: "old".into(),
            learning_generation: 7,
        });
        model.clauses[0].source = SurfaceSource::Candidate {
            token: "first".into(),
            explicitly_selected: true,
        };
        model.clauses[2].source = SurfaceSource::LocalSurface;
        let saved = freeze(&model, "あいう").unwrap();
        model.clauses[0].surface = "亜".into();
        model
            .learning_identity
            .as_mut()
            .unwrap()
            .learning_generation = 8;
        assert_eq!(saved.text, "あいう");
        assert_eq!(saved.learning_generation, 7);
        assert_eq!(saved.intervals.len(), 3);
        assert!(matches!(
            saved.intervals[1].learning,
            IntervalLearning::None {
                reason: NoLearningReason::Reading
            }
        ));
        assert!(matches!(
            saved.intervals[2].learning,
            IntervalLearning::None {
                reason: NoLearningReason::Invalidated
            }
        ));
        assert_ne!(saved.commit_id, freeze(&model, "亜いう").unwrap().commit_id);
        assert!(freeze(&model, "別の本文").is_none());
    }
}
