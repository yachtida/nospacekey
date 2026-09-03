use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{
    channel, sync_channel, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError,
    TrySendError,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_queue::ArrayQueue;
use ipc::client::{EngineClient, EngineIdentityError, VerifiedEngineClient};
use ipc::protocol::{Request, Response, PROTO_VERSION};

use crate::globals::ComObjectGuard;
#[cfg(test)]
use crate::input_module::BackgroundIntent;
use crate::input_module::{
    CompositionSnapshot, InputSegment, RequestId, SnapshotIdentity, SnapshotPurpose, TextStyle,
};

const DIRTY: u64 = 0;
const RESEED_QUEUED: u64 = 1;
const SYNCED: u64 = 2;
const TERMINAL: u64 = 3;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const ENHANCEMENT_POLL_INTERVAL: Duration = Duration::from_millis(15);
// Swift stops GPU work at 250/900ms; the extra 100ms absorbs IPC scheduling without
// allowing an always-pending peer to keep the optional lane alive indefinitely.
const LIVE_ENHANCEMENT_BUDGET: Duration = Duration::from_millis(350);
const EXPLICIT_ENHANCEMENT_BUDGET: Duration = Duration::from_millis(1_000);
const ENHANCEMENT_CONNECT_BUDGET: Duration = Duration::from_millis(500);
const ENHANCEMENT_REQUEST_BUDGET: Duration = Duration::from_millis(100);
const SNAPSHOT_STATUS_CAPACITY: usize = 8;

fn encode(generation: u64, phase: u64) -> u64 {
    (generation << 2) | phase
}
fn generation(state: u64) -> u64 {
    state >> 2
}
fn phase(state: u64) -> u64 {
    state & 3
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WorkerIntent {
    Insert {
        request: RequestId,
        segments: Vec<InputSegment>,
        reseed: bool,
    },
    #[cfg(test)]
    LiveConvert {
        seq: u64,
    },
    CommitAndClose,
    Close,
}

#[derive(Debug)]
pub(crate) struct QueuedIntent {
    generation: u64,
    intent: WorkerIntent,
}

struct SharedState {
    pipeline: AtomicU64,
    close_before_generation: AtomicU64,
    rejected: AtomicU64,
}

#[derive(Clone)]
pub(crate) struct BackgroundMailbox {
    sender: SyncSender<QueuedIntent>,
    shared: Arc<SharedState>,
}

impl BackgroundMailbox {
    #[cfg(test)]
    pub(crate) fn try_push(&self, intent: BackgroundIntent) -> bool {
        match intent {
            BackgroundIntent::Insert { request, segments } => {
                if self.needs_reseed() {
                    self.try_reseed(request, segments)
                } else {
                    self.try_delta(WorkerIntent::Insert {
                        request,
                        segments,
                        reseed: false,
                    })
                }
            }
            BackgroundIntent::Reseed { request, segments } => {
                self.request_close();
                self.try_reseed(request, segments)
            }
            BackgroundIntent::LiveSnapshot { .. }
            | BackgroundIntent::Convert { .. }
            | BackgroundIntent::Commit { .. } => false,
        }
    }
    fn needs_reseed(&self) -> bool {
        phase(self.shared.pipeline.load(Ordering::Acquire)) == DIRTY
    }

    fn begin_composition(&self) {
        self.advance_to_dirty(false);
    }

    fn request_close(&self) {
        let next = self.advance_to_dirty(true);
        let _ = self.sender.try_send(QueuedIntent {
            generation: next,
            intent: WorkerIntent::Close,
        });
    }

    fn advance_to_dirty(&self, close: bool) -> u64 {
        let mut current = self.shared.pipeline.load(Ordering::Acquire);
        loop {
            let next_generation = generation(current).wrapping_add(1);
            let next = encode(next_generation, DIRTY);
            match self.shared.pipeline.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if close {
                        self.shared
                            .close_before_generation
                            .fetch_max(next_generation, Ordering::AcqRel);
                    }
                    return next_generation;
                }
                Err(actual) => current = actual,
            }
        }
    }

    fn try_reseed(&self, request: RequestId, segments: Vec<InputSegment>) -> bool {
        let mut current = self.shared.pipeline.load(Ordering::Acquire);
        loop {
            if phase(current) != DIRTY {
                return false;
            }
            let queued = encode(generation(current), RESEED_QUEUED);
            match self.shared.pipeline.compare_exchange_weak(
                current,
                queued,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return self.enqueue(
                        generation(current),
                        WorkerIntent::Insert {
                            request,
                            segments,
                            reseed: true,
                        },
                    )
                }
                Err(actual) => current = actual,
            }
        }
    }

    fn try_delta(&self, intent: WorkerIntent) -> bool {
        let state = self.shared.pipeline.load(Ordering::Acquire);
        if !matches!(phase(state), RESEED_QUEUED | SYNCED) {
            return false;
        }
        self.enqueue(generation(state), intent)
    }

    fn try_commit_and_close(&self) -> bool {
        let mut current = self.shared.pipeline.load(Ordering::Acquire);
        loop {
            if !matches!(phase(current), RESEED_QUEUED | SYNCED) {
                self.request_close();
                return false;
            }
            let terminal = encode(generation(current), TERMINAL);
            match self.shared.pipeline.compare_exchange_weak(
                current,
                terminal,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return self.enqueue(generation(current), WorkerIntent::CommitAndClose),
                Err(actual) => current = actual,
            }
        }
    }

    fn enqueue(&self, generation: u64, intent: WorkerIntent) -> bool {
        match self.sender.try_send(QueuedIntent { generation, intent }) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.shared.rejected.fetch_add(1, Ordering::Relaxed);
                self.request_close();
                false
            }
        }
    }
}

pub(crate) struct BackgroundInputWorker {
    mailbox: BackgroundMailbox,
    // Snapshot bodies never enter this lane. Live wakeups are coalesced to one command, so a
    // typing burst cannot consume the capacity needed to reconfigure or close the private session.
    snapshot_sender: SyncSender<SnapshotCommand>,
    pending_snapshot: Arc<ArrayQueue<CompositionSnapshot>>,
    snapshot_work_notified: Arc<AtomicBool>,
    snapshot_worker_alive: Arc<AtomicBool>,
    // The capacity-one lock-free slot is authoritative, so control-lane saturation can delay a
    // configuration wakeup but can never discard the newest configuration.
    desired_snapshot_configuration: Arc<ArrayQueue<DesiredSnapshotConfiguration>>,
    desired_snapshot_configuration_generation: Arc<AtomicU64>,
    snapshot_shutdown: Arc<AtomicBool>,
    snapshot_connection_epoch: Arc<AtomicU64>,
    results: Arc<ArrayQueue<LiveSnapshotResult>>,
    enhancement_results: Arc<ArrayQueue<LiveSnapshotResult>>,
    pending_enhancement: Arc<ArrayQueue<SnapshotEnhancementRequest>>,
    // A receipt follows an irreversible TSF write, so it cannot share a bounded lane where Full
    // would silently leave EngineHost free to propose the same prefix again.
    auto_commit_receipt_sender: Sender<Request>,
    snapshot_statuses: Arc<ArrayQueue<SnapshotStatus>>,
}

#[derive(Clone, Copy, Debug)]
struct SnapshotEnhancementRequest {
    serial: u64,
    identity: SnapshotIdentity,
    purpose: SnapshotPurpose,
    baseline: u64,
    deadline: Instant,
}

#[derive(Clone)]
struct SnapshotEnhancementPublisher {
    pending: Arc<ArrayQueue<SnapshotEnhancementRequest>>,
    latest_serial: Arc<AtomicU64>,
}

impl SnapshotEnhancementPublisher {
    fn offer(&self, result: &LiveSnapshotResult) {
        let serial = self.latest_serial.fetch_add(1, Ordering::AcqRel) + 1;
        let budget = match result.purpose {
            SnapshotPurpose::Live => LIVE_ENHANCEMENT_BUDGET,
            SnapshotPurpose::Explicit => EXPLICIT_ENHANCEMENT_BUDGET,
        };
        let _ = self.pending.force_push(SnapshotEnhancementRequest {
            serial,
            identity: result.identity,
            purpose: result.purpose,
            baseline: result.baseline,
            deadline: Instant::now() + budget,
        });
    }
}

enum SnapshotCommand {
    DesiredConfigurationChanged,
    WorkAvailable,
    Close,
}

struct DesiredSnapshotConfiguration {
    generation: u64,
    request: Request,
}

fn offer_live_snapshot(
    sender: &SyncSender<SnapshotCommand>,
    pending: &ArrayQueue<CompositionSnapshot>,
    work_notified: &AtomicBool,
    worker_alive: &AtomicBool,
    snapshot: CompositionSnapshot,
) -> bool {
    if !worker_alive.load(Ordering::Acquire) {
        return false;
    }
    let _ = pending.force_push(snapshot);
    if work_notified.swap(true, Ordering::AcqRel) {
        return true;
    }
    match sender.try_send(SnapshotCommand::WorkAvailable) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            // The queued control command already guarantees a wake. Keep the bit clear so a
            // revision arriving after that control is consumed can publish its own wakeup.
            work_notified.store(false, Ordering::Release);
            true
        }
        Err(TrySendError::Disconnected(_)) => {
            work_notified.store(false, Ordering::Release);
            while pending.pop().is_some() {}
            worker_alive.store(false, Ordering::Release);
            false
        }
    }
}

fn enqueue_auto_commit_receipt(
    sender: &Sender<Request>,
    wake: &SyncSender<SnapshotCommand>,
    receipt: crate::input_module::AutoCommitReceipt,
) -> bool {
    let sent = sender
        .send(Request::AutoCommitReceipt {
            composition: receipt.identity.composition,
            revision: receipt.identity.revision,
            configuration_generation: receipt.identity.configuration_generation,
            connection_generation: receipt.identity.connection_generation,
            proposal: receipt.proposal,
        })
        .is_ok();
    if sent {
        let _ = wake.try_send(SnapshotCommand::WorkAvailable);
    }
    sent
}

struct SnapshotWorkerAlive {
    alive: Arc<AtomicBool>,
    pending: Arc<ArrayQueue<CompositionSnapshot>>,
    work_notified: Arc<AtomicBool>,
}

impl Drop for SnapshotWorkerAlive {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        while self.pending.pop().is_some() {}
        self.work_notified.store(false, Ordering::Release);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotStatus {
    Configured {
        configuration_generation: u64,
        connection_epoch: u64,
    },
    Invalidated {
        configuration_generation: u64,
        connection_epoch: u64,
    },
    VersionMismatch {
        configuration_generation: u64,
        connection_epoch: u64,
        actual: Option<u32>,
        actual_boot: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveSnapshotResult {
    pub(crate) identity: SnapshotIdentity,
    pub(crate) purpose: SnapshotPurpose,
    pub(crate) text: String,
    pub(crate) candidates: Option<Vec<String>>,
    pub(crate) candidate_remaining: Option<Vec<String>>,
    pub(crate) baseline: u64,
    pub(crate) enhancement: bool,
    pub(crate) auto_commit: Option<crate::input_module::AutoCommitProposal>,
}

impl BackgroundInputWorker {
    pub(crate) fn start(pipe: String, capacity: usize) -> Self {
        assert!(capacity > 0, "background worker capacity must be positive");
        let (mailbox, receiver) = bounded_mailbox(capacity);
        let (snapshot_sender, snapshot_receiver) = sync_channel(capacity);
        let pending_snapshot = Arc::new(ArrayQueue::new(1));
        let snapshot_work_notified = Arc::new(AtomicBool::new(false));
        let snapshot_worker_alive = Arc::new(AtomicBool::new(true));
        let results = Arc::new(ArrayQueue::new(1));
        let enhancement_results = Arc::new(ArrayQueue::new(1));
        let pending_enhancement = Arc::new(ArrayQueue::new(1));
        let (auto_commit_receipt_sender, auto_commit_receipt_receiver) = channel();
        let latest_enhancement_serial = Arc::new(AtomicU64::new(0));
        let enhancement_publisher = SnapshotEnhancementPublisher {
            pending: pending_enhancement.clone(),
            latest_serial: latest_enhancement_serial.clone(),
        };
        let snapshot_statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let desired_snapshot_configuration = Arc::new(ArrayQueue::new(1));
        let desired_snapshot_configuration_generation = Arc::new(AtomicU64::new(0));
        let snapshot_shutdown = Arc::new(AtomicBool::new(false));
        let snapshot_connection_epoch = Arc::new(AtomicU64::new(1));
        let shared = mailbox.shared.clone();
        let guard = ComObjectGuard::new();
        let stateful_pipe = pipe.clone();
        if std::thread::Builder::new()
            .name("nospacekey-input".to_string())
            .spawn(move || {
                let _guard = guard;
                run_worker(receiver, shared, EngineInputTransport::new(stateful_pipe));
            })
            .is_err()
        {
            crate::text_service::tip_log("ev=input_worker_spawn_failed");
        }
        let snapshot_guard = ComObjectGuard::new();
        let snapshot_pipe = pipe.clone();
        let published_snapshot_epoch = snapshot_connection_epoch.clone();
        let worker_desired_configuration = desired_snapshot_configuration.clone();
        let worker_desired_generation = desired_snapshot_configuration_generation.clone();
        let worker_snapshot_shutdown = snapshot_shutdown.clone();
        let worker_pending_snapshot = pending_snapshot.clone();
        let worker_snapshot_work_notified = snapshot_work_notified.clone();
        let worker_snapshot_alive = snapshot_worker_alive.clone();
        let worker_results = results.clone();
        let worker_snapshot_statuses = snapshot_statuses.clone();
        if std::thread::Builder::new()
            .name("nospacekey-snapshot".to_string())
            .spawn(move || {
                let _alive = SnapshotWorkerAlive {
                    alive: worker_snapshot_alive,
                    pending: worker_pending_snapshot.clone(),
                    work_notified: worker_snapshot_work_notified.clone(),
                };
                let _guard = snapshot_guard;
                run_snapshot_worker(
                    snapshot_receiver,
                    worker_desired_configuration,
                    worker_desired_generation,
                    worker_snapshot_shutdown,
                    worker_pending_snapshot,
                    worker_snapshot_work_notified,
                    EngineSnapshotTransport::new(
                        snapshot_pipe,
                        published_snapshot_epoch,
                        Some(enhancement_publisher),
                        auto_commit_receipt_receiver,
                    ),
                    worker_results,
                    worker_snapshot_statuses,
                );
            })
            .is_err()
        {
            snapshot_worker_alive.store(false, Ordering::Release);
            crate::text_service::tip_log("ev=snapshot_worker_spawn_failed");
        }
        let enhancement_guard = ComObjectGuard::new();
        let enhancement_shutdown = snapshot_shutdown.clone();
        let worker_pending_enhancement = pending_enhancement.clone();
        let worker_latest_enhancement_serial = latest_enhancement_serial.clone();
        let worker_enhancement_results = enhancement_results.clone();
        if std::thread::Builder::new()
            .name("nospacekey-snapshot-enhancement".to_string())
            .spawn(move || {
                let _guard = enhancement_guard;
                run_snapshot_enhancement_worker(
                    enhancement_shutdown,
                    worker_pending_enhancement,
                    worker_latest_enhancement_serial,
                    EngineSnapshotEnhancementTransport::new(pipe),
                    worker_enhancement_results,
                );
            })
            .is_err()
        {
            crate::text_service::tip_log("ev=snapshot_enhancement_worker_spawn_failed");
        }
        Self {
            mailbox,
            snapshot_sender,
            pending_snapshot,
            snapshot_work_notified,
            snapshot_worker_alive,
            desired_snapshot_configuration,
            desired_snapshot_configuration_generation,
            snapshot_shutdown,
            snapshot_connection_epoch,
            results,
            enhancement_results,
            pending_enhancement,
            auto_commit_receipt_sender,
            snapshot_statuses,
        }
    }

    pub(crate) fn needs_reseed(&self) -> bool {
        self.mailbox.needs_reseed()
    }
    pub(crate) fn begin_composition(&self) {
        self.mailbox.begin_composition();
    }
    pub(crate) fn try_reseed(&self, request: RequestId, segments: Vec<InputSegment>) -> bool {
        self.mailbox.try_reseed(request, segments)
    }
    pub(crate) fn try_insert(&self, request: RequestId, segments: Vec<InputSegment>) -> bool {
        self.mailbox.try_delta(WorkerIntent::Insert {
            request,
            segments,
            reseed: false,
        })
    }
    pub(crate) fn connection_generation(&self) -> u64 {
        self.snapshot_connection_epoch.load(Ordering::Acquire)
    }
    pub(crate) fn try_live_snapshot(&self, snapshot: CompositionSnapshot) -> bool {
        !self.snapshot_shutdown.load(Ordering::Acquire)
            && offer_live_snapshot(
                &self.snapshot_sender,
                &self.pending_snapshot,
                &self.snapshot_work_notified,
                &self.snapshot_worker_alive,
                snapshot,
            )
    }
    pub(crate) fn try_configure_snapshot(&self, generation: u64, request: Request) -> bool {
        let _ = self
            .desired_snapshot_configuration
            .force_push(DesiredSnapshotConfiguration {
                generation,
                request,
            });
        self.desired_snapshot_configuration_generation
            .store(generation, Ordering::Release);
        !matches!(
            self.snapshot_sender
                .try_send(SnapshotCommand::DesiredConfigurationChanged),
            Err(TrySendError::Disconnected(_))
        )
    }
    pub(crate) fn desired_snapshot_configuration_generation(&self) -> u64 {
        self.desired_snapshot_configuration_generation
            .load(Ordering::Acquire)
    }
    pub(crate) fn try_result(&self) -> Option<LiveSnapshotResult> {
        self.results
            .pop()
            .or_else(|| self.enhancement_results.pop())
    }
    pub(crate) fn try_snapshot_status(&self) -> Option<SnapshotStatus> {
        self.snapshot_statuses.pop()
    }
    pub(crate) fn try_auto_commit_receipt(
        &self,
        receipt: crate::input_module::AutoCommitReceipt,
    ) -> bool {
        enqueue_auto_commit_receipt(
            &self.auto_commit_receipt_sender,
            &self.snapshot_sender,
            receipt,
        )
    }
    pub(crate) fn try_commit_and_close(&self) -> bool {
        self.mailbox.try_commit_and_close()
    }
    pub(crate) fn request_close(&self) {
        self.mailbox.request_close();
    }
}

impl Drop for BackgroundInputWorker {
    fn drop(&mut self) {
        self.snapshot_shutdown.store(true, Ordering::Release);
        while self.desired_snapshot_configuration.pop().is_some() {}
        while self.pending_snapshot.pop().is_some() {}
        while self.pending_enhancement.pop().is_some() {}
        while self.enhancement_results.pop().is_some() {}
        self.snapshot_work_notified.store(false, Ordering::Release);
        self.desired_snapshot_configuration_generation
            .store(0, Ordering::Release);
        let _ = self.snapshot_sender.try_send(SnapshotCommand::Close);
        self.mailbox.request_close();
    }
}

pub(crate) fn bounded_mailbox(capacity: usize) -> (BackgroundMailbox, Receiver<QueuedIntent>) {
    let (sender, receiver) = sync_channel(capacity);
    (
        BackgroundMailbox {
            sender,
            shared: Arc::new(SharedState {
                pipeline: AtomicU64::new(encode(1, DIRTY)),
                close_before_generation: AtomicU64::new(0),
                rejected: AtomicU64::new(0),
            }),
        },
        receiver,
    )
}

trait WorkerTransport {
    fn apply(&mut self, intent: &WorkerIntent) -> bool;
    fn close(&mut self);
}

fn run_worker<T: WorkerTransport>(
    receiver: Receiver<QueuedIntent>,
    shared: Arc<SharedState>,
    mut transport: T,
) {
    let mut worker_generation = 0;
    loop {
        if worker_generation != 0
            && worker_generation < shared.close_before_generation.load(Ordering::Acquire)
        {
            transport.close();
            worker_generation = 0;
        }
        let queued = match receiver.recv_timeout(POLL_INTERVAL) {
            Ok(value) => value,
            Err(RecvTimeoutError::Timeout) => {
                flush_rejected(&shared);
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                transport.close();
                flush_rejected(&shared);
                return;
            }
        };
        flush_rejected(&shared);
        let current = shared.pipeline.load(Ordering::Acquire);
        if queued.generation != generation(current) {
            continue;
        }
        if matches!(queued.intent, WorkerIntent::Close) {
            transport.close();
            worker_generation = 0;
            continue;
        }
        if worker_generation != queued.generation {
            transport.close();
            worker_generation = queued.generation;
        }
        let reseed = matches!(queued.intent, WorkerIntent::Insert { reseed: true, .. });
        if !transport.apply(&queued.intent) {
            mark_worker_failed(&shared, queued.generation);
            transport.close();
            worker_generation = 0;
            continue;
        }
        if reseed {
            let _ = shared.pipeline.compare_exchange(
                encode(queued.generation, RESEED_QUEUED),
                encode(queued.generation, SYNCED),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        if matches!(queued.intent, WorkerIntent::CommitAndClose) {
            transport.close();
            worker_generation = 0;
        }
    }
}

fn mark_worker_failed(shared: &SharedState, failed_generation: u64) {
    let mut current = shared.pipeline.load(Ordering::Acquire);
    loop {
        if generation(current) != failed_generation {
            return;
        }
        let next_generation = failed_generation.wrapping_add(1);
        match shared.pipeline.compare_exchange_weak(
            current,
            encode(next_generation, DIRTY),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                shared
                    .close_before_generation
                    .fetch_max(next_generation, Ordering::AcqRel);
                return;
            }
            Err(actual) => current = actual,
        }
    }
}

fn flush_rejected(shared: &SharedState) {
    let rejected = shared.rejected.swap(0, Ordering::AcqRel);
    if rejected != 0 {
        crate::text_service::tip_log(&format!("ev=input_worker_queue_rejected count={rejected}"));
    }
}

struct EngineInputTransport {
    pipe: String,
    client: Option<VerifiedEngineClient>,
    session: i64,
    identity_mismatch: bool,
}

impl EngineInputTransport {
    fn new(pipe: String) -> Self {
        Self {
            pipe,
            client: None,
            session: 0,
            identity_mismatch: false,
        }
    }

    fn ensure_session(&mut self) -> bool {
        if self.identity_mismatch {
            return false;
        }
        if self.client.is_some() && self.session != 0 {
            return true;
        }
        match EngineClient::connect_verified_to(
            &self.pipe,
            Duration::from_millis(50),
            Instant::now() + Duration::from_millis(250),
        ) {
            Ok(client) => {
                let session = client.session();
                self.session = session;
                self.client = Some(client);
                true
            }
            Err(EngineIdentityError::Mismatch {
                actual_proto,
                actual_boot,
            }) => {
                self.identity_mismatch = true;
                crate::text_service::tip_log(&format!(
                    "ev=input_engine_identity_mismatch expected_proto={} expected_boot={} actual_proto={actual_proto:?} actual_boot={actual_boot:?}",
                    PROTO_VERSION,
                    env!("CARGO_PKG_VERSION")
                ));
                false
            }
            Err(_) => false,
        }
    }

    fn request_ready(&mut self, request: &Request) -> Option<Response> {
        self.client
            .as_mut()?
            .request_within(request, Instant::now() + Duration::from_millis(250))
            .ok()
    }
}

impl WorkerTransport for EngineInputTransport {
    fn apply(&mut self, intent: &WorkerIntent) -> bool {
        if matches!(intent, WorkerIntent::Insert { reseed: true, .. }) {
            self.close();
        }
        if !self.ensure_session() {
            return false;
        }
        let session = self.session;
        match intent {
            WorkerIntent::Insert { segments, .. } => segments.iter().all(|segment| {
                matches!(
                    self.request_ready(&Request::Insert {
                        session,
                        text: segment.text.clone(),
                        style: match segment.style {
                            TextStyle::Kana => None,
                            TextStyle::Direct => Some("direct".to_string()),
                        },
                    }),
                    Some(Response::Reading { .. })
                )
            }),
            #[cfg(test)]
            WorkerIntent::LiveConvert { .. } => false,
            WorkerIntent::CommitAndClose => matches!(
                self.request_ready(&Request::Commit { session, index: 0 }),
                Some(Response::Committed { .. }) | Some(Response::Error { .. })
            ),
            WorkerIntent::Close => true,
        }
    }

    fn close(&mut self) {
        if self.session != 0 {
            if let Some(client) = self.client.as_mut() {
                let _ = client.request_within(
                    &Request::EndSession {
                        session: self.session,
                    },
                    Instant::now() + Duration::from_millis(250),
                );
            }
        }
        self.client = None;
        self.session = 0;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SnapshotConfigureOutcome {
    Configured,
    RetryableFailure,
    VersionMismatch {
        actual: Option<u32>,
        actual_boot: Option<String>,
    },
}

trait SnapshotTransport {
    fn configure(&mut self, request: &Request) -> SnapshotConfigureOutcome;
    fn convert(&mut self, snapshot: &CompositionSnapshot) -> Option<LiveSnapshotResult>;
    fn schedule_enhancement(&self, _result: &LiveSnapshotResult) {}
    fn apply_auto_commit_receipt(&mut self, _request: &Request) -> bool {
        false
    }
    fn drain_auto_commit_receipts(&mut self) -> bool {
        true
    }
    fn health_probe(&mut self) -> bool {
        true
    }
    fn connection_epoch(&self) -> u64;
    fn invalidate(&mut self) -> u64;
    fn recover_link(&mut self) {}
}

enum SnapshotEnhancementPoll {
    Pending,
    Ready(LiveSnapshotResult),
    Unavailable,
    LinkFailure,
}

trait SnapshotEnhancementTransport {
    fn poll_enhancement(
        &mut self,
        identity: SnapshotIdentity,
        purpose: SnapshotPurpose,
        baseline: u64,
        deadline: Instant,
    ) -> SnapshotEnhancementPoll;
}

#[derive(Clone, Copy)]
struct RetryPolicy {
    base: Duration,
    cap: Duration,
    health_probe_interval: Duration,
}

impl RetryPolicy {
    fn delay(self, failures: u32) -> Duration {
        let multiplier = 2u32.saturating_pow(failures.saturating_sub(1));
        (self.base * multiplier).min(self.cap)
    }
}

fn recover_snapshot_link<T: SnapshotTransport>(transport: &mut T, shutdown: &AtomicBool) -> bool {
    if shutdown.load(Ordering::Acquire) {
        transport.invalidate();
        return false;
    }
    transport.recover_link();
    if shutdown.load(Ordering::Acquire) {
        transport.invalidate();
        return false;
    }
    true
}

fn invalidate_and_recover_snapshot_stream<T: SnapshotTransport>(
    transport: &mut T,
    shutdown: &AtomicBool,
    statuses: &ArrayQueue<SnapshotStatus>,
    failed_generation: u64,
    retry_delay: Duration,
) -> Option<Instant> {
    let connection_epoch = transport.invalidate();
    let _ = statuses.force_push(SnapshotStatus::Invalidated {
        configuration_generation: failed_generation,
        connection_epoch,
    });
    recover_snapshot_link(transport, shutdown).then(|| Instant::now() + retry_delay)
}

fn wait_for_snapshot_command(
    receiver: &Receiver<SnapshotCommand>,
    retry_at: Option<Instant>,
) -> Result<Option<SnapshotCommand>, ()> {
    match retry_at {
        Some(deadline) => {
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(command) => Ok(Some(command)),
                Err(RecvTimeoutError::Timeout) => Ok(None),
                Err(RecvTimeoutError::Disconnected) => Err(()),
            }
        }
        None => receiver.recv().map(Some).map_err(|_| ()),
    }
}

fn run_snapshot_worker<T: SnapshotTransport>(
    receiver: Receiver<SnapshotCommand>,
    desired_configurations: Arc<ArrayQueue<DesiredSnapshotConfiguration>>,
    desired_configuration_generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    pending_snapshot: Arc<ArrayQueue<CompositionSnapshot>>,
    work_notified: Arc<AtomicBool>,
    transport: T,
    results: Arc<ArrayQueue<LiveSnapshotResult>>,
    statuses: Arc<ArrayQueue<SnapshotStatus>>,
) {
    run_snapshot_worker_with_retry(
        receiver,
        desired_configurations,
        desired_configuration_generation,
        shutdown,
        pending_snapshot,
        work_notified,
        transport,
        results,
        statuses,
        RetryPolicy {
            base: crate::engine_link::BACKOFF_BASE,
            cap: crate::engine_link::BACKOFF_CAP,
            health_probe_interval: Duration::from_secs(5),
        },
    );
}

fn run_snapshot_worker_with_retry<T: SnapshotTransport>(
    receiver: Receiver<SnapshotCommand>,
    desired_configurations: Arc<ArrayQueue<DesiredSnapshotConfiguration>>,
    desired_configuration_generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    pending_snapshot: Arc<ArrayQueue<CompositionSnapshot>>,
    work_notified: Arc<AtomicBool>,
    mut transport: T,
    results: Arc<ArrayQueue<LiveSnapshotResult>>,
    statuses: Arc<ArrayQueue<SnapshotStatus>>,
    retry_policy: RetryPolicy,
) {
    let mut latest_configuration: Option<(u64, Request)> = None;
    let mut configuration_generation = 0;
    let mut configure_attempts = 0;
    let mut retry_at: Option<Instant> = None;
    let mut health_probe_at: Option<Instant> = None;
    let mut latest_snapshot: Option<CompositionSnapshot> = None;
    let mut replay_latest_snapshot = false;
    let mut selected_replay_snapshot: Option<CompositionSnapshot> = None;
    loop {
        if shutdown.load(Ordering::Acquire) {
            transport.invalidate();
            return;
        }
        adopt_desired_snapshot_configuration(
            &desired_configurations,
            &desired_configuration_generation,
            &mut latest_configuration,
            &mut configuration_generation,
            &mut configure_attempts,
            &mut retry_at,
            &mut health_probe_at,
        );
        if shutdown.load(Ordering::Acquire) {
            transport.invalidate();
            return;
        }
        let work_wakeup = match receiver.try_recv() {
            Ok(SnapshotCommand::Close) | Err(TryRecvError::Disconnected) => {
                transport.invalidate();
                return;
            }
            Ok(SnapshotCommand::DesiredConfigurationChanged) => continue,
            Ok(SnapshotCommand::WorkAvailable) => {
                work_notified.store(false, Ordering::Release);
                true
            }
            Err(TryRecvError::Empty) => false,
        };
        if retry_at.is_some_and(|deadline| deadline <= Instant::now()) {
            retry_at = None;
            let Some((generation, request)) = latest_configuration.as_ref() else {
                continue;
            };
            if desired_configuration_generation.load(Ordering::Acquire) != *generation {
                continue;
            }
            if shutdown.load(Ordering::Acquire) {
                transport.invalidate();
                return;
            }
            let configured = transport.configure(request);
            if shutdown.load(Ordering::Acquire) {
                transport.invalidate();
                return;
            }
            match configured {
                SnapshotConfigureOutcome::Configured => {
                    configuration_generation = *generation;
                    configure_attempts = 0;
                    health_probe_at = Some(Instant::now() + retry_policy.health_probe_interval);
                    let _ = statuses.force_push(SnapshotStatus::Configured {
                        configuration_generation,
                        connection_epoch: transport.connection_epoch(),
                    });
                    let rebind = |mut snapshot: CompositionSnapshot| {
                        (snapshot.identity.configuration_generation == configuration_generation)
                            .then(|| {
                                snapshot.identity.connection_generation =
                                    transport.connection_epoch();
                                snapshot
                            })
                    };
                    let pending_replay = pending_snapshot.pop().and_then(&rebind);
                    let selected_replay = selected_replay_snapshot.take().and_then(&rebind);
                    let attempted_replay = replay_latest_snapshot
                        .then(|| latest_snapshot.clone())
                        .flatten()
                        .and_then(&rebind);
                    selected_replay_snapshot =
                        pending_replay.or(selected_replay).or(attempted_replay);
                    replay_latest_snapshot = false;
                }
                SnapshotConfigureOutcome::RetryableFailure => {
                    configuration_generation = 0;
                    configure_attempts = configure_attempts.saturating_add(1);
                    let connection_epoch = transport.invalidate();
                    let _ = statuses.force_push(SnapshotStatus::Invalidated {
                        configuration_generation: *generation,
                        connection_epoch,
                    });
                    if !recover_snapshot_link(&mut transport, &shutdown) {
                        return;
                    }
                    retry_at = Some(Instant::now() + retry_policy.delay(configure_attempts));
                    health_probe_at = None;
                }
                SnapshotConfigureOutcome::VersionMismatch {
                    actual,
                    actual_boot,
                } => {
                    configuration_generation = 0;
                    configure_attempts = 0;
                    let connection_epoch = transport.invalidate();
                    let _ = statuses.force_push(SnapshotStatus::VersionMismatch {
                        configuration_generation: *generation,
                        connection_epoch,
                        actual,
                        actual_boot,
                    });
                    retry_at = None;
                    health_probe_at = None;
                }
            }
            continue;
        }
        if health_probe_at.is_some_and(|deadline| deadline <= Instant::now()) {
            if shutdown.load(Ordering::Acquire) {
                transport.invalidate();
                return;
            }
            let healthy = transport.health_probe();
            if shutdown.load(Ordering::Acquire) {
                transport.invalidate();
                return;
            }
            if healthy {
                health_probe_at = Some(Instant::now() + retry_policy.health_probe_interval);
            } else {
                let failed_generation = configuration_generation;
                configuration_generation = 0;
                configure_attempts = 1;
                let connection_epoch = transport.invalidate();
                let _ = statuses.force_push(SnapshotStatus::Invalidated {
                    configuration_generation: failed_generation,
                    connection_epoch,
                });
                replay_latest_snapshot = latest_snapshot.is_some();
                if !recover_snapshot_link(&mut transport, &shutdown) {
                    return;
                }
                retry_at = Some(Instant::now() + retry_policy.delay(configure_attempts));
                health_probe_at = None;
            }
            continue;
        }
        let next_deadline = [retry_at, health_probe_at].into_iter().flatten().min();
        let runnable_snapshot = configuration_generation != 0
            && (selected_replay_snapshot.is_some() || !pending_snapshot.is_empty());
        if !work_wakeup && !runnable_snapshot {
            match wait_for_snapshot_command(&receiver, next_deadline) {
                Ok(Some(SnapshotCommand::Close)) | Err(()) => {
                    transport.invalidate();
                    return;
                }
                Ok(Some(SnapshotCommand::DesiredConfigurationChanged)) | Ok(None) => continue,
                Ok(Some(SnapshotCommand::WorkAvailable)) => {
                    work_notified.store(false, Ordering::Release);
                }
            }
        }
        if shutdown.load(Ordering::Acquire) {
            transport.invalidate();
            return;
        }
        adopt_desired_snapshot_configuration(
            &desired_configurations,
            &desired_configuration_generation,
            &mut latest_configuration,
            &mut configuration_generation,
            &mut configure_attempts,
            &mut retry_at,
            &mut health_probe_at,
        );
        if shutdown.load(Ordering::Acquire) {
            transport.invalidate();
            return;
        }
        if configuration_generation == 0 {
            continue;
        }
        // Receipts describe already-applied document writes and must reach EngineHost before a
        // newer snapshot can advance that stream's auto-commit history.
        if !transport.drain_auto_commit_receipts() {
            let failed_generation = configuration_generation;
            configuration_generation = 0;
            configure_attempts = 0;
            let Some(next_retry) = invalidate_and_recover_snapshot_stream(
                &mut transport,
                &shutdown,
                &statuses,
                failed_generation,
                retry_policy.delay(1),
            ) else {
                return;
            };
            retry_at = Some(next_retry);
            health_probe_at = None;
            replay_latest_snapshot = latest_snapshot.is_some();
            continue;
        }
        if let Some(snapshot) = selected_replay_snapshot
            .take()
            .or_else(|| pending_snapshot.pop())
        {
            if snapshot.identity.configuration_generation == configuration_generation
                && snapshot.identity.connection_generation == transport.connection_epoch()
            {
                latest_snapshot = Some(snapshot.clone());
                if shutdown.load(Ordering::Acquire) {
                    transport.invalidate();
                    return;
                }
                // TSF publishes a successful receipt before its later keystroke snapshot on the
                // same apartment thread; rechecking after pop preserves that producer order.
                if !transport.drain_auto_commit_receipts() {
                    let failed_generation = configuration_generation;
                    configuration_generation = 0;
                    configure_attempts = 0;
                    let Some(next_retry) = invalidate_and_recover_snapshot_stream(
                        &mut transport,
                        &shutdown,
                        &statuses,
                        failed_generation,
                        retry_policy.delay(1),
                    ) else {
                        return;
                    };
                    retry_at = Some(next_retry);
                    health_probe_at = None;
                    replay_latest_snapshot = true;
                    continue;
                }
                let result = transport.convert(&snapshot);
                if shutdown.load(Ordering::Acquire) {
                    transport.invalidate();
                    return;
                }
                if let Some(result) = result {
                    let _ = results.force_push(result.clone());
                    transport.schedule_enhancement(&result);
                } else {
                    let failed_generation = configuration_generation;
                    configuration_generation = 0;
                    configure_attempts = 0;
                    let Some(next_retry) = invalidate_and_recover_snapshot_stream(
                        &mut transport,
                        &shutdown,
                        &statuses,
                        failed_generation,
                        retry_policy.delay(1),
                    ) else {
                        return;
                    };
                    retry_at = Some(next_retry);
                    health_probe_at = None;
                    replay_latest_snapshot = true;
                }
            }
            continue;
        }
    }
}

fn run_snapshot_enhancement_worker<T: SnapshotEnhancementTransport>(
    shutdown: Arc<AtomicBool>,
    pending: Arc<ArrayQueue<SnapshotEnhancementRequest>>,
    latest_serial: Arc<AtomicU64>,
    mut transport: T,
    results: Arc<ArrayQueue<LiveSnapshotResult>>,
) {
    let mut active = None;
    while !shutdown.load(Ordering::Acquire) {
        if let Some(latest) = pending.pop() {
            active = Some(latest);
        }
        let Some(request) = active else {
            std::thread::park_timeout(POLL_INTERVAL);
            continue;
        };
        if request.serial != latest_serial.load(Ordering::Acquire)
            || Instant::now() >= request.deadline
        {
            active = None;
            continue;
        }
        let outcome = transport.poll_enhancement(
            request.identity,
            request.purpose,
            request.baseline,
            request.deadline,
        );
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        if request.serial != latest_serial.load(Ordering::Acquire)
            || Instant::now() >= request.deadline
        {
            active = None;
            continue;
        }
        match outcome {
            SnapshotEnhancementPoll::Pending => {
                std::thread::park_timeout(
                    ENHANCEMENT_POLL_INTERVAL
                        .min(request.deadline.saturating_duration_since(Instant::now())),
                );
            }
            SnapshotEnhancementPoll::Ready(result) => {
                let _ = results.force_push(result);
                active = None;
            }
            SnapshotEnhancementPoll::Unavailable | SnapshotEnhancementPoll::LinkFailure => {
                active = None;
            }
        }
    }
}

fn adopt_desired_snapshot_configuration(
    desired_configurations: &ArrayQueue<DesiredSnapshotConfiguration>,
    desired_configuration_generation: &AtomicU64,
    latest_configuration: &mut Option<(u64, Request)>,
    configuration_generation: &mut u64,
    configure_attempts: &mut u32,
    retry_at: &mut Option<Instant>,
    health_probe_at: &mut Option<Instant>,
) {
    let Some(desired) = desired_configurations.pop() else {
        return;
    };
    desired_configuration_generation.fetch_max(desired.generation, Ordering::AcqRel);
    *latest_configuration = Some((desired.generation, desired.request));
    *configuration_generation = 0;
    *configure_attempts = 0;
    *retry_at = Some(Instant::now());
    *health_probe_at = None;
}

struct EngineSnapshotTransport {
    pipe: String,
    client: Option<EngineClient>,
    connection_epoch: Arc<AtomicU64>,
    enhancement_publisher: Option<SnapshotEnhancementPublisher>,
    auto_commit_receipt_receiver: Receiver<Request>,
}

struct EngineSnapshotEnhancementTransport {
    pipe: String,
    client: Option<VerifiedEngineClient>,
    identity_mismatch: bool,
}

fn configure_snapshot_protocol(
    request: &Request,
    mut send: impl FnMut(&Request) -> Option<Response>,
) -> SnapshotConfigureOutcome {
    let Some(Response::Session {
        session,
        proto,
        boot,
    }) = send(&Request::StartSession)
    else {
        return SnapshotConfigureOutcome::RetryableFailure;
    };
    if proto != Some(PROTO_VERSION) || boot.as_deref() != Some(env!("CARGO_PKG_VERSION")) {
        return SnapshotConfigureOutcome::VersionMismatch {
            actual: proto,
            actual_boot: boot,
        };
    }
    if !matches!(send(&Request::EndSession { session }), Some(Response::Ok)) {
        return SnapshotConfigureOutcome::RetryableFailure;
    }
    if matches!(send(request), Some(Response::Ok)) {
        SnapshotConfigureOutcome::Configured
    } else {
        SnapshotConfigureOutcome::RetryableFailure
    }
}

impl EngineSnapshotTransport {
    fn new(
        pipe: String,
        connection_epoch: Arc<AtomicU64>,
        enhancement_publisher: Option<SnapshotEnhancementPublisher>,
        auto_commit_receipt_receiver: Receiver<Request>,
    ) -> Self {
        Self {
            pipe,
            client: None,
            connection_epoch,
            enhancement_publisher,
            auto_commit_receipt_receiver,
        }
    }

    fn ensure_client(&mut self) -> bool {
        if self.client.is_some() {
            return true;
        }
        match EngineClient::connect_to(&self.pipe, Duration::from_millis(500)) {
            Ok(client) => {
                self.client = Some(client);
                true
            }
            Err(_) => false,
        }
    }

    fn request(&mut self, request: &Request, timeout: Duration) -> Option<Response> {
        if !self.ensure_client() {
            return None;
        }
        self.client
            .as_mut()?
            .request_within(request, Instant::now() + timeout)
            .ok()
    }
}

impl SnapshotTransport for EngineSnapshotTransport {
    fn configure(&mut self, request: &Request) -> SnapshotConfigureOutcome {
        let outcome = configure_snapshot_protocol(request, |request| {
            self.request(request, Duration::from_millis(250))
        });
        if matches!(outcome, SnapshotConfigureOutcome::VersionMismatch { .. }) {
            self.client = None;
        }
        outcome
    }

    fn convert(&mut self, snapshot: &CompositionSnapshot) -> Option<LiveSnapshotResult> {
        let response = self.request(
            &Request::LiveSnapshot {
                composition: snapshot.identity.composition,
                revision: snapshot.identity.revision,
                configuration_generation: snapshot.identity.configuration_generation,
                connection_generation: snapshot.identity.connection_generation,
                segments: snapshot
                    .segments
                    .iter()
                    .map(|segment| ipc::protocol::SnapshotSegment {
                        text: segment.text.clone(),
                        style: match segment.style {
                            TextStyle::Kana => None,
                            TextStyle::Direct => Some("direct".to_string()),
                        },
                    })
                    .collect(),
                explicit: snapshot.purpose == SnapshotPurpose::Explicit,
                left_context: snapshot.left_context.clone(),
            },
            match snapshot.purpose {
                SnapshotPurpose::Live => Duration::from_millis(400),
                SnapshotPurpose::Explicit => Duration::from_millis(1_200),
            },
        )?;
        match response {
            Response::SnapshotResult {
                composition,
                revision,
                configuration_generation,
                connection_generation,
                text,
                candidates,
                candidate_remaining,
                baseline,
                auto_commit,
            } => {
                let (candidates, candidate_remaining) = match snapshot.purpose {
                    SnapshotPurpose::Live => (None, None),
                    SnapshotPurpose::Explicit => {
                        let candidates = candidates?;
                        let remaining = candidate_remaining?;
                        if candidates.len() != remaining.len() {
                            return None;
                        }
                        (Some(candidates), Some(remaining))
                    }
                };
                Some(LiveSnapshotResult {
                    identity: SnapshotIdentity {
                        composition,
                        revision,
                        configuration_generation,
                        connection_generation,
                    },
                    purpose: snapshot.purpose,
                    text,
                    candidates,
                    candidate_remaining,
                    baseline,
                    enhancement: false,
                    auto_commit: auto_commit.map(|proposal| {
                        crate::input_module::AutoCommitProposal {
                            proposal: proposal.proposal,
                            identity: SnapshotIdentity {
                                composition,
                                revision,
                                configuration_generation,
                                connection_generation,
                            },
                            text: proposal.text,
                            consumed_reading: proposal.consumed_reading,
                            remaining: proposal.remaining,
                        }
                    }),
                })
            }
            _ => None,
        }
    }

    fn schedule_enhancement(&self, result: &LiveSnapshotResult) {
        if let Some(publisher) = &self.enhancement_publisher {
            publisher.offer(result);
        }
    }

    fn apply_auto_commit_receipt(&mut self, request: &Request) -> bool {
        matches!(
            self.request(request, Duration::from_millis(250)),
            Some(Response::Ok)
        )
    }

    fn drain_auto_commit_receipts(&mut self) -> bool {
        while let Ok(request) = self.auto_commit_receipt_receiver.try_recv() {
            let request_epoch = match &request {
                Request::AutoCommitReceipt {
                    connection_generation,
                    ..
                } => *connection_generation,
                _ => continue,
            };
            if request_epoch != self.connection_epoch() {
                continue;
            }
            if !self.apply_auto_commit_receipt(&request) {
                while self.auto_commit_receipt_receiver.try_recv().is_ok() {}
                return false;
            }
        }
        true
    }

    fn health_probe(&mut self) -> bool {
        matches!(
            self.request(&Request::Ping, Duration::from_millis(100)),
            Some(Response::Pong)
        )
    }

    fn connection_epoch(&self) -> u64 {
        self.connection_epoch.load(Ordering::Acquire)
    }

    fn invalidate(&mut self) -> u64 {
        self.client = None;
        // A named-pipe connection cannot survive its EngineHost process. Advancing only after
        // dropping that connection therefore identifies both the connection and the observed
        // engine boot boundary; a second wire-level boot UUID would not reject any extra result.
        self.connection_epoch.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn recover_link(&mut self) {
        let _ = crate::text_service::spawn_engine_only(&self.pipe);
    }
}

impl EngineSnapshotEnhancementTransport {
    fn new(pipe: String) -> Self {
        Self {
            pipe,
            client: None,
            identity_mismatch: false,
        }
    }

    fn request(&mut self, request: &Request, deadline: Instant) -> Option<Response> {
        if self.client.is_none() {
            if self.identity_mismatch {
                return None;
            }
            let timeout =
                bounded_enhancement_timeout(Instant::now(), deadline, ENHANCEMENT_CONNECT_BUDGET)?;
            let start_deadline = Instant::now().checked_add(timeout)?;
            self.client = match EngineClient::connect_verified_to(
                &self.pipe,
                timeout,
                start_deadline,
            ) {
                Ok(client) => Some(client),
                Err(EngineIdentityError::Mismatch {
                    actual_proto,
                    actual_boot,
                }) => {
                    self.identity_mismatch = true;
                    crate::text_service::tip_log(&format!(
                        "ev=enhancement_engine_identity_mismatch expected_proto={} expected_boot={} actual_proto={actual_proto:?} actual_boot={actual_boot:?}",
                        PROTO_VERSION,
                        env!("CARGO_PKG_VERSION")
                    ));
                    None
                }
                Err(_) => None,
            };
        }
        let now = Instant::now();
        let timeout = bounded_enhancement_timeout(now, deadline, ENHANCEMENT_REQUEST_BUDGET)?;
        let response = self
            .client
            .as_mut()?
            .request_within(request, now.checked_add(timeout)?);
        match response {
            Ok(response) => Some(response),
            Err(_) => {
                self.client = None;
                None
            }
        }
    }
}

fn bounded_enhancement_timeout(now: Instant, deadline: Instant, cap: Duration) -> Option<Duration> {
    let remaining = deadline.checked_duration_since(now)?;
    (!remaining.is_zero()).then_some(remaining.min(cap))
}

impl SnapshotEnhancementTransport for EngineSnapshotEnhancementTransport {
    fn poll_enhancement(
        &mut self,
        identity: SnapshotIdentity,
        purpose: SnapshotPurpose,
        baseline: u64,
        deadline: Instant,
    ) -> SnapshotEnhancementPoll {
        let Some(response) = self.request(
            &Request::PollSnapshotEnhancement {
                composition: identity.composition,
                revision: identity.revision,
                configuration_generation: identity.configuration_generation,
                connection_generation: identity.connection_generation,
                baseline,
            },
            deadline,
        ) else {
            return SnapshotEnhancementPoll::LinkFailure;
        };
        decode_snapshot_enhancement(response, identity, purpose, baseline)
    }
}

fn decode_snapshot_enhancement(
    response: Response,
    identity: SnapshotIdentity,
    purpose: SnapshotPurpose,
    baseline: u64,
) -> SnapshotEnhancementPoll {
    match response {
        Response::SnapshotEnhancementPending => SnapshotEnhancementPoll::Pending,
        Response::SnapshotEnhancementUnavailable => SnapshotEnhancementPoll::Unavailable,
        Response::SnapshotEnhancement {
            composition,
            revision,
            configuration_generation,
            connection_generation,
            baseline: response_baseline,
            text,
            candidates,
            candidate_remaining,
        } if identity.composition == composition
            && identity.revision == revision
            && identity.configuration_generation == configuration_generation
            && identity.connection_generation == connection_generation
            && baseline == response_baseline =>
        {
            let (candidates, candidate_remaining) = match purpose {
                SnapshotPurpose::Live => (None, None),
                SnapshotPurpose::Explicit => {
                    let Some(candidates) = candidates else {
                        return SnapshotEnhancementPoll::Unavailable;
                    };
                    let Some(remaining) = candidate_remaining else {
                        return SnapshotEnhancementPoll::Unavailable;
                    };
                    if candidates.len() != remaining.len() {
                        return SnapshotEnhancementPoll::Unavailable;
                    }
                    (Some(candidates), Some(remaining))
                }
            };
            SnapshotEnhancementPoll::Ready(LiveSnapshotResult {
                identity,
                purpose,
                text,
                candidates,
                candidate_remaining,
                baseline,
                enhancement: true,
                auto_commit: None,
            })
        }
        Response::SnapshotEnhancement { .. } => SnapshotEnhancementPoll::Unavailable,
        _ => SnapshotEnhancementPoll::LinkFailure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::channel;
    use std::sync::Mutex;

    trait QueueTestExt<T> {
        fn recv_timeout(&self, timeout: Duration) -> Result<T, ()>;
        fn try_recv(&self) -> Result<T, ()>;
        fn try_iter(&self) -> std::vec::IntoIter<T>;
    }

    impl<T> QueueTestExt<T> for Arc<ArrayQueue<T>> {
        fn recv_timeout(&self, timeout: Duration) -> Result<T, ()> {
            let deadline = Instant::now() + timeout;
            loop {
                if let Some(value) = self.pop() {
                    return Ok(value);
                }
                if Instant::now() >= deadline {
                    return Err(());
                }
                std::thread::yield_now();
            }
        }

        fn try_recv(&self) -> Result<T, ()> {
            self.pop().ok_or(())
        }

        fn try_iter(&self) -> std::vec::IntoIter<T> {
            let mut values = Vec::new();
            while let Some(value) = self.pop() {
                values.push(value);
            }
            values.into_iter()
        }
    }

    fn segment(text: impl Into<String>) -> InputSegment {
        InputSegment {
            text: text.into(),
            style: TextStyle::Kana,
        }
    }

    fn enhancement_mailbox() -> (
        SnapshotEnhancementPublisher,
        Arc<ArrayQueue<SnapshotEnhancementRequest>>,
        Arc<AtomicU64>,
    ) {
        let pending = Arc::new(ArrayQueue::new(1));
        let latest_serial = Arc::new(AtomicU64::new(0));
        (
            SnapshotEnhancementPublisher {
                pending: pending.clone(),
                latest_serial: latest_serial.clone(),
            },
            pending,
            latest_serial,
        )
    }

    fn wait_for_count(value: &AtomicU64, expected: u64) {
        let deadline = Instant::now() + Duration::from_millis(200);
        while value.load(Ordering::Acquire) < expected {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for count {expected}"
            );
            std::thread::yield_now();
        }
    }

    fn test_reload_config() -> Request {
        Request::ReloadConfig {
            llm_enabled: false,
            llm_api_key: String::new(),
            llm_endpoint: String::new(),
            llm_model: String::new(),
            llm_prompt: String::new(),
            llm_timeout_ms: 1,
            zenzai_enabled: false,
            zenzai_weight: String::new(),
            inline_prediction_enabled: false,
            learning_enabled: false,
            typo_learn_enabled: false,
            zenzai_inference_limit: None,
        }
    }

    #[test]
    fn snapshot_protocol_sends_nothing_after_a_boot_identity_mismatch() {
        let requests = Mutex::new(Vec::new());
        let mut responses = [Some(Response::Session {
            session: 41,
            proto: Some(PROTO_VERSION),
            boot: Some("old-build".into()),
        })]
        .into_iter();
        let outcome = configure_snapshot_protocol(&test_reload_config(), |request| {
            requests.lock().unwrap().push(match request {
                Request::StartSession => "start",
                Request::EndSession { session: 41 } => "end",
                Request::ReloadConfig { .. } => "reload",
                Request::LiveSnapshot { .. } => "snapshot",
                _ => "other",
            });
            responses.next().flatten()
        });

        assert_eq!(
            outcome,
            SnapshotConfigureOutcome::VersionMismatch {
                actual: Some(PROTO_VERSION),
                actual_boot: Some("old-build".into()),
            }
        );
        assert_eq!(*requests.lock().unwrap(), vec!["start"]);
    }

    #[test]
    fn snapshot_protocol_configures_only_after_the_full_identity_matches() {
        let requests = Mutex::new(Vec::new());
        let mut responses = [
            Some(Response::Session {
                session: 42,
                proto: Some(PROTO_VERSION),
                boot: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            Some(Response::Ok),
            Some(Response::Ok),
        ]
        .into_iter();
        let outcome = configure_snapshot_protocol(&test_reload_config(), |request| {
            requests.lock().unwrap().push(match request {
                Request::StartSession => "start",
                Request::EndSession { session: 42 } => "end",
                Request::ReloadConfig { .. } => "reload",
                Request::LiveSnapshot { .. } => "snapshot",
                _ => "other",
            });
            responses.next().flatten()
        });

        assert_eq!(outcome, SnapshotConfigureOutcome::Configured);
        assert_eq!(*requests.lock().unwrap(), vec!["start", "end", "reload"]);
    }

    #[derive(Clone, Default)]
    struct Journal(Arc<Mutex<Vec<u64>>>);

    impl WorkerTransport for Journal {
        fn apply(&mut self, intent: &WorkerIntent) -> bool {
            let id = match intent {
                WorkerIntent::Insert { request, .. } => request.0,
                WorkerIntent::LiveConvert { seq } => *seq,
                WorkerIntent::CommitAndClose | WorkerIntent::Close => return true,
            };
            self.0.lock().unwrap().push(id);
            true
        }
        fn close(&mut self) {}
    }

    #[derive(Clone, Default)]
    struct ReplayJournal {
        ids: Arc<Mutex<Vec<u64>>>,
        text: Arc<Mutex<String>>,
    }

    impl WorkerTransport for ReplayJournal {
        fn apply(&mut self, intent: &WorkerIntent) -> bool {
            match intent {
                WorkerIntent::Insert {
                    request,
                    segments,
                    reseed,
                } => {
                    self.ids.lock().unwrap().push(request.0);
                    let mut text = self.text.lock().unwrap();
                    if *reseed {
                        text.clear();
                    }
                    for segment in segments {
                        text.push_str(&segment.text);
                    }
                    true
                }
                WorkerIntent::CommitAndClose => {
                    self.ids.lock().unwrap().push(u64::MAX);
                    true
                }
                WorkerIntent::LiveConvert { .. } | WorkerIntent::Close => true,
            }
        }

        fn close(&mut self) {}
    }

    struct ScheduledReplayJournal(ReplayJournal);

    impl WorkerTransport for ScheduledReplayJournal {
        fn apply(&mut self, intent: &WorkerIntent) -> bool {
            let schedule_key = match intent {
                WorkerIntent::Insert { request, .. } => request.0,
                WorkerIntent::LiveConvert { seq } => *seq,
                WorkerIntent::CommitAndClose | WorkerIntent::Close => 0,
            };
            for _ in 0..(schedule_key.wrapping_mul(17) % 31) {
                std::hint::spin_loop();
            }
            self.0.apply(intent)
        }

        fn close(&mut self) {}
    }

    #[test]
    fn deltas_queued_behind_reseed_are_delivered_without_loss() {
        let (mailbox, receiver) = bounded_mailbox(10_001);
        assert!(mailbox.try_reseed(RequestId(0), vec![segment("seed:")]));
        let mut expected = String::from("seed:");
        for id in 1..=10_000 {
            let text = ["a", "i", "u", "e", "o"][id as usize % 5];
            expected.push_str(text);
            assert!(mailbox.try_delta(WorkerIntent::Insert {
                request: RequestId(id),
                segments: vec![segment(text)],
                reseed: false,
            }));
        }
        let journal = ReplayJournal::default();
        let observed_ids = journal.ids.clone();
        let observed_text = journal.text.clone();
        let shared = mailbox.shared.clone();
        drop(mailbox);
        run_worker(receiver, shared, journal);
        let actual_ids = observed_ids.lock().unwrap();
        assert_eq!(actual_ids.len(), 10_001);
        assert!(actual_ids.iter().copied().eq(0..=10_000));
        assert_eq!(*observed_text.lock().unwrap(), expected);
    }

    #[test]
    fn varied_worker_schedule_preserves_the_final_journal() {
        let (mailbox, receiver) = bounded_mailbox(2_001);
        let journal = ReplayJournal::default();
        let observed_ids = journal.ids.clone();
        let observed_text = journal.text.clone();
        let shared = mailbox.shared.clone();
        let worker = std::thread::spawn(move || {
            run_worker(receiver, shared, ScheduledReplayJournal(journal))
        });
        assert!(mailbox.try_reseed(RequestId(0), vec![segment("seed:")]));
        let mut expected = String::from("seed:");
        for id in 1..=2_000 {
            let text = ["ka", "na", "A", "-", "n"][id as usize % 5];
            expected.push_str(text);
            assert!(mailbox.try_delta(WorkerIntent::Insert {
                request: RequestId(id),
                segments: vec![segment(text)],
                reseed: false,
            }));
        }
        drop(mailbox);
        worker.join().unwrap();
        assert!(observed_ids.lock().unwrap().iter().copied().eq(0..=2_000));
        assert_eq!(*observed_text.lock().unwrap(), expected);
    }

    #[test]
    fn a_late_reseed_success_cannot_overwrite_a_new_dirty_generation() {
        let (mailbox, _receiver) = bounded_mailbox(1);
        assert!(mailbox.try_reseed(RequestId(1), vec![segment("a")]));
        let old = mailbox.shared.pipeline.load(Ordering::Acquire);
        mark_worker_failed(&mailbox.shared, generation(old));
        let late = mailbox.shared.pipeline.compare_exchange(
            encode(generation(old), RESEED_QUEUED),
            encode(generation(old), SYNCED),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        assert!(late.is_err());
        assert_eq!(
            phase(mailbox.shared.pipeline.load(Ordering::Acquire)),
            DIRTY
        );
    }

    #[test]
    fn queue_full_requires_a_new_reseed_and_returns_quickly() {
        let (mailbox, _receiver) = bounded_mailbox(1);
        assert!(mailbox.try_reseed(RequestId(1), vec![segment("a")]));
        let started = Instant::now();
        assert!(!mailbox.try_delta(WorkerIntent::Insert {
            request: RequestId(2),
            segments: vec![segment("b")],
            reseed: false,
        }));
        assert!(started.elapsed() < Duration::from_millis(8));
        assert!(mailbox.needs_reseed());
    }

    struct FailOnceTransport {
        failed: Arc<AtomicBool>,
        journal: Journal,
    }

    impl WorkerTransport for FailOnceTransport {
        fn apply(&mut self, intent: &WorkerIntent) -> bool {
            if !self.failed.swap(true, Ordering::AcqRel) {
                return false;
            }
            self.journal.apply(intent)
        }

        fn close(&mut self) {}
    }

    #[test]
    fn worker_failure_requires_and_accepts_a_new_full_reseed() {
        let (mailbox, receiver) = bounded_mailbox(4);
        let shared = mailbox.shared.clone();
        let failed = Arc::new(AtomicBool::new(false));
        let journal = Journal::default();
        let observed = journal.0.clone();
        let transport = FailOnceTransport {
            failed: failed.clone(),
            journal,
        };
        let worker = std::thread::spawn(move || run_worker(receiver, shared, transport));

        assert!(mailbox.try_reseed(RequestId(1), vec![segment("a")]));
        let deadline = Instant::now() + Duration::from_millis(100);
        while (!failed.load(Ordering::Acquire) || !mailbox.needs_reseed())
            && Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert!(failed.load(Ordering::Acquire));
        assert!(mailbox.needs_reseed());
        assert!(mailbox.try_reseed(RequestId(2), vec![segment("ab")]));
        drop(mailbox);
        worker.join().unwrap();
        assert_eq!(*observed.lock().unwrap(), vec![2]);
    }

    struct CloseCountingTransport {
        applied: Arc<AtomicBool>,
        closes: Arc<AtomicU64>,
    }

    impl WorkerTransport for CloseCountingTransport {
        fn apply(&mut self, _intent: &WorkerIntent) -> bool {
            self.applied.store(true, Ordering::Release);
            true
        }

        fn close(&mut self) {
            self.closes.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[test]
    fn close_request_wakes_an_idle_worker() {
        let (mailbox, receiver) = bounded_mailbox(2);
        let shared = mailbox.shared.clone();
        let applied = Arc::new(AtomicBool::new(false));
        let closes = Arc::new(AtomicU64::new(0));
        let transport = CloseCountingTransport {
            applied: applied.clone(),
            closes: closes.clone(),
        };
        let worker = std::thread::spawn(move || run_worker(receiver, shared, transport));
        assert!(mailbox.try_reseed(RequestId(1), vec![segment("a")]));
        let deadline = Instant::now() + Duration::from_millis(100);
        while !applied.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(applied.load(Ordering::Acquire));
        let before = closes.load(Ordering::Acquire);
        mailbox.request_close();
        let deadline = Instant::now() + Duration::from_millis(100);
        while closes.load(Ordering::Acquire) == before && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(closes.load(Ordering::Acquire) > before);
        drop(mailbox);
        worker.join().unwrap();
    }

    #[test]
    fn terminal_commit_is_fifo_and_rejects_later_deltas() {
        let (mailbox, receiver) = bounded_mailbox(3);
        assert!(mailbox.try_reseed(RequestId(1), vec![segment("a")]));
        assert!(mailbox.try_commit_and_close());
        assert!(!mailbox.try_delta(WorkerIntent::Insert {
            request: RequestId(2),
            segments: vec![segment("b")],
            reseed: false,
        }));
        let journal = ReplayJournal::default();
        let observed_ids = journal.ids.clone();
        let shared = mailbox.shared.clone();
        drop(mailbox);
        run_worker(receiver, shared, journal);
        assert_eq!(*observed_ids.lock().unwrap(), vec![1, u64::MAX]);
    }

    struct FailFirstRecordingTransport {
        attempts: Arc<Mutex<Vec<&'static str>>>,
    }

    impl WorkerTransport for FailFirstRecordingTransport {
        fn apply(&mut self, intent: &WorkerIntent) -> bool {
            let label = match intent {
                WorkerIntent::Insert { reseed: true, .. } => "reseed",
                WorkerIntent::CommitAndClose => "commit",
                _ => "delta",
            };
            let mut attempts = self.attempts.lock().unwrap();
            attempts.push(label);
            attempts.len() != 1
        }

        fn close(&mut self) {}
    }

    #[test]
    fn reseed_failure_invalidates_an_already_queued_terminal_commit() {
        let (mailbox, receiver) = bounded_mailbox(2);
        assert!(mailbox.try_reseed(RequestId(1), vec![segment("a")]));
        assert!(mailbox.try_commit_and_close());
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let shared = mailbox.shared.clone();
        let observed_state = shared.clone();
        drop(mailbox);
        run_worker(
            receiver,
            shared,
            FailFirstRecordingTransport {
                attempts: attempts.clone(),
            },
        );
        assert_eq!(*attempts.lock().unwrap(), vec!["reseed"]);
        assert_eq!(
            phase(observed_state.pipeline.load(Ordering::Acquire)),
            DIRTY
        );
    }

    struct FailSecondRecordingTransport {
        attempts: Arc<Mutex<Vec<&'static str>>>,
    }

    impl WorkerTransport for FailSecondRecordingTransport {
        fn apply(&mut self, intent: &WorkerIntent) -> bool {
            let label = match intent {
                WorkerIntent::Insert { reseed: true, .. } => "reseed",
                WorkerIntent::Insert { .. } => "delta",
                WorkerIntent::CommitAndClose => "commit",
                _ => "other",
            };
            let mut attempts = self.attempts.lock().unwrap();
            attempts.push(label);
            attempts.len() != 2
        }

        fn close(&mut self) {}
    }

    #[test]
    fn delta_failure_invalidates_a_queued_terminal_commit() {
        let (mailbox, receiver) = bounded_mailbox(3);
        assert!(mailbox.try_reseed(RequestId(1), vec![segment("a")]));
        assert!(mailbox.try_delta(WorkerIntent::Insert {
            request: RequestId(2),
            segments: vec![segment("b")],
            reseed: false,
        }));
        assert!(mailbox.try_commit_and_close());
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let shared = mailbox.shared.clone();
        let observed_state = shared.clone();
        drop(mailbox);
        run_worker(
            receiver,
            shared,
            FailSecondRecordingTransport {
                attempts: attempts.clone(),
            },
        );
        assert_eq!(*attempts.lock().unwrap(), vec!["reseed", "delta"]);
        assert_eq!(
            phase(observed_state.pipeline.load(Ordering::Acquire)),
            DIRTY
        );
    }

    struct GatedTransport {
        started: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
        applied: Arc<AtomicU64>,
        closes: Arc<AtomicU64>,
    }

    impl WorkerTransport for GatedTransport {
        fn apply(&mut self, _intent: &WorkerIntent) -> bool {
            let applied = self.applied.fetch_add(1, Ordering::AcqRel);
            if applied == 0 {
                self.started.send(()).unwrap();
                self.release.recv().unwrap();
            }
            true
        }

        fn close(&mut self) {
            self.closes.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct SnapshotJournal {
        configure_outcomes: Arc<Mutex<std::collections::VecDeque<bool>>>,
        convert_outcomes: Arc<Mutex<std::collections::VecDeque<bool>>>,
        conversions: Arc<Mutex<Vec<u64>>>,
        connection_epoch: Arc<AtomicU64>,
    }

    impl SnapshotTransport for SnapshotJournal {
        fn configure(&mut self, _request: &Request) -> SnapshotConfigureOutcome {
            if self
                .configure_outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(true)
            {
                SnapshotConfigureOutcome::Configured
            } else {
                SnapshotConfigureOutcome::RetryableFailure
            }
        }

        fn convert(&mut self, snapshot: &CompositionSnapshot) -> Option<LiveSnapshotResult> {
            self.conversions
                .lock()
                .unwrap()
                .push(snapshot.identity.revision);
            self.convert_outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(true)
                .then(|| LiveSnapshotResult {
                    identity: snapshot.identity,
                    purpose: snapshot.purpose,
                    text: format!("converted:{}", snapshot.identity.revision),
                    candidates: None,
                    candidate_remaining: None,
                    baseline: 1,
                    enhancement: false,
                    auto_commit: None,
                })
        }

        fn connection_epoch(&self) -> u64 {
            self.connection_epoch.load(Ordering::Acquire)
        }

        fn invalidate(&mut self) -> u64 {
            self.connection_epoch.fetch_add(1, Ordering::AcqRel) + 1
        }
    }

    struct FailFirstSnapshotBarrier {
        first_started: std::sync::mpsc::Sender<()>,
        first_release: Option<std::sync::mpsc::Receiver<()>>,
        receipt_failure_started: Option<std::sync::mpsc::Sender<()>>,
        receipt_failure_release: Option<std::sync::mpsc::Receiver<()>>,
        configure_count: u32,
        fail_reconnected_receipt_once: bool,
        conversions: Arc<Mutex<Vec<u64>>>,
        connection_epoch: Arc<AtomicU64>,
    }

    impl SnapshotTransport for FailFirstSnapshotBarrier {
        fn configure(&mut self, _request: &Request) -> SnapshotConfigureOutcome {
            self.configure_count += 1;
            SnapshotConfigureOutcome::Configured
        }

        fn convert(&mut self, snapshot: &CompositionSnapshot) -> Option<LiveSnapshotResult> {
            self.conversions
                .lock()
                .unwrap()
                .push(snapshot.identity.revision);
            if let Some(release) = self.first_release.take() {
                self.first_started.send(()).unwrap();
                release.recv().unwrap();
                return None;
            }
            Some(LiveSnapshotResult {
                identity: snapshot.identity,
                purpose: snapshot.purpose,
                text: format!("converted:{}", snapshot.identity.revision),
                candidates: None,
                candidate_remaining: None,
                baseline: 1,
                enhancement: false,
                auto_commit: None,
            })
        }

        fn connection_epoch(&self) -> u64 {
            self.connection_epoch.load(Ordering::Acquire)
        }

        fn invalidate(&mut self) -> u64 {
            self.connection_epoch.fetch_add(1, Ordering::AcqRel) + 1
        }

        fn drain_auto_commit_receipts(&mut self) -> bool {
            if self.configure_count == 2 && self.fail_reconnected_receipt_once {
                if let Some(started) = self.receipt_failure_started.take() {
                    started.send(()).unwrap();
                }
                if let Some(release) = self.receipt_failure_release.take() {
                    release.recv().unwrap();
                }
                self.fail_reconnected_receipt_once = false;
                return false;
            }
            true
        }
    }

    struct EnhancementSnapshotTransport {
        connection_epoch: Arc<AtomicU64>,
        publisher: SnapshotEnhancementPublisher,
    }

    impl SnapshotTransport for EnhancementSnapshotTransport {
        fn configure(&mut self, _request: &Request) -> SnapshotConfigureOutcome {
            SnapshotConfigureOutcome::Configured
        }

        fn convert(&mut self, snapshot: &CompositionSnapshot) -> Option<LiveSnapshotResult> {
            Some(LiveSnapshotResult {
                identity: snapshot.identity,
                purpose: snapshot.purpose,
                text: "classic".to_string(),
                candidates: None,
                candidate_remaining: None,
                baseline: 42,
                enhancement: false,
                auto_commit: None,
            })
        }

        fn schedule_enhancement(&self, result: &LiveSnapshotResult) {
            self.publisher.offer(result);
        }

        fn connection_epoch(&self) -> u64 {
            self.connection_epoch.load(Ordering::Acquire)
        }

        fn invalidate(&mut self) -> u64 {
            self.connection_epoch.fetch_add(1, Ordering::AcqRel) + 1
        }
    }

    struct ReadyEnhancementTransport;

    impl SnapshotEnhancementTransport for ReadyEnhancementTransport {
        fn poll_enhancement(
            &mut self,
            identity: SnapshotIdentity,
            purpose: SnapshotPurpose,
            baseline: u64,
            _deadline: Instant,
        ) -> SnapshotEnhancementPoll {
            SnapshotEnhancementPoll::Ready(LiveSnapshotResult {
                identity,
                purpose,
                text: "gpu".to_string(),
                candidates: None,
                candidate_remaining: None,
                baseline,
                enhancement: true,
                auto_commit: None,
            })
        }
    }

    struct GatedEnhancementTransport {
        started: std::sync::mpsc::Sender<()>,
        release: Option<std::sync::mpsc::Receiver<()>>,
    }

    impl SnapshotEnhancementTransport for GatedEnhancementTransport {
        fn poll_enhancement(
            &mut self,
            identity: SnapshotIdentity,
            purpose: SnapshotPurpose,
            baseline: u64,
            _deadline: Instant,
        ) -> SnapshotEnhancementPoll {
            if let Some(release) = self.release.take() {
                self.started.send(()).unwrap();
                release.recv().unwrap();
            }
            SnapshotEnhancementPoll::Ready(LiveSnapshotResult {
                identity,
                purpose,
                text: format!("gpu:{}", identity.revision),
                candidates: None,
                candidate_remaining: None,
                baseline,
                enhancement: true,
                auto_commit: None,
            })
        }
    }

    struct FailingEnhancementTransport {
        calls: Arc<AtomicU64>,
    }

    impl SnapshotEnhancementTransport for FailingEnhancementTransport {
        fn poll_enhancement(
            &mut self,
            identity: SnapshotIdentity,
            purpose: SnapshotPurpose,
            baseline: u64,
            _deadline: Instant,
        ) -> SnapshotEnhancementPoll {
            match self.calls.fetch_add(1, Ordering::AcqRel) {
                0 => SnapshotEnhancementPoll::LinkFailure,
                1 => SnapshotEnhancementPoll::Unavailable,
                _ => SnapshotEnhancementPoll::Ready(LiveSnapshotResult {
                    identity,
                    purpose,
                    text: format!("gpu:{}", identity.revision),
                    candidates: None,
                    candidate_remaining: None,
                    baseline,
                    enhancement: true,
                    auto_commit: None,
                }),
            }
        }
    }

    struct BlockingPendingEnhancementTransport {
        started: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
        calls: Arc<AtomicU64>,
    }

    impl SnapshotEnhancementTransport for BlockingPendingEnhancementTransport {
        fn poll_enhancement(
            &mut self,
            _identity: SnapshotIdentity,
            _purpose: SnapshotPurpose,
            _baseline: u64,
            _deadline: Instant,
        ) -> SnapshotEnhancementPoll {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.started.send(()).unwrap();
            self.release.recv().unwrap();
            SnapshotEnhancementPoll::Pending
        }
    }

    struct BlockingSnapshotTransport {
        started: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
        configured_requests: Arc<Mutex<Vec<&'static str>>>,
        connection_epoch: Arc<AtomicU64>,
    }

    impl SnapshotTransport for BlockingSnapshotTransport {
        fn configure(&mut self, request: &Request) -> SnapshotConfigureOutcome {
            self.configured_requests
                .lock()
                .unwrap()
                .push(match request {
                    Request::Ping => "g1",
                    Request::Shutdown => "g2",
                    _ => "other",
                });
            SnapshotConfigureOutcome::Configured
        }

        fn convert(&mut self, snapshot: &CompositionSnapshot) -> Option<LiveSnapshotResult> {
            self.started.send(()).unwrap();
            self.release.recv().unwrap();
            Some(LiveSnapshotResult {
                identity: snapshot.identity,
                purpose: snapshot.purpose,
                text: "old-result".to_string(),
                candidates: None,
                candidate_remaining: None,
                baseline: 1,
                enhancement: false,
                auto_commit: None,
            })
        }

        fn connection_epoch(&self) -> u64 {
            self.connection_epoch.load(Ordering::Acquire)
        }

        fn invalidate(&mut self) -> u64 {
            self.connection_epoch.fetch_add(1, Ordering::AcqRel) + 1
        }
    }

    struct ShutdownSnapshotTransport {
        started: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
        configured_requests: Arc<Mutex<Vec<&'static str>>>,
        conversions: Arc<Mutex<Vec<u64>>>,
        invalidations: Arc<AtomicU64>,
        connection_epoch: Arc<AtomicU64>,
    }

    struct CrashLoopSnapshotTransport {
        configure_attempts: Arc<AtomicU64>,
        recovery_attempts: Arc<AtomicU64>,
        failures_before_recovery: u64,
        connection_epoch: Arc<AtomicU64>,
    }

    struct VersionMismatchSnapshotTransport {
        configure_attempts: Arc<AtomicU64>,
        recovery_attempts: Arc<AtomicU64>,
        connection_epoch: Arc<AtomicU64>,
    }

    struct UnconfiguredCountingTransport {
        outcome: SnapshotConfigureOutcome,
        configure_attempts: Arc<AtomicU64>,
        recovery_attempts: Arc<AtomicU64>,
        receipt_attempts: Arc<AtomicU64>,
        conversion_attempts: Arc<AtomicU64>,
        connection_epoch: Arc<AtomicU64>,
    }

    impl SnapshotTransport for UnconfiguredCountingTransport {
        fn configure(&mut self, _request: &Request) -> SnapshotConfigureOutcome {
            self.configure_attempts.fetch_add(1, Ordering::AcqRel);
            self.outcome.clone()
        }

        fn convert(&mut self, _snapshot: &CompositionSnapshot) -> Option<LiveSnapshotResult> {
            self.conversion_attempts.fetch_add(1, Ordering::AcqRel);
            None
        }

        fn connection_epoch(&self) -> u64 {
            self.connection_epoch.load(Ordering::Acquire)
        }

        fn invalidate(&mut self) -> u64 {
            self.connection_epoch.fetch_add(1, Ordering::AcqRel) + 1
        }

        fn recover_link(&mut self) {
            self.recovery_attempts.fetch_add(1, Ordering::AcqRel);
        }

        fn drain_auto_commit_receipts(&mut self) -> bool {
            self.receipt_attempts.fetch_add(1, Ordering::AcqRel);
            true
        }
    }

    impl SnapshotTransport for VersionMismatchSnapshotTransport {
        fn configure(&mut self, _request: &Request) -> SnapshotConfigureOutcome {
            self.configure_attempts.fetch_add(1, Ordering::AcqRel);
            SnapshotConfigureOutcome::VersionMismatch {
                actual: Some(5),
                actual_boot: Some("old-build".into()),
            }
        }

        fn convert(&mut self, _snapshot: &CompositionSnapshot) -> Option<LiveSnapshotResult> {
            panic!("version-mismatched transport must not receive snapshots")
        }

        fn connection_epoch(&self) -> u64 {
            self.connection_epoch.load(Ordering::Acquire)
        }

        fn invalidate(&mut self) -> u64 {
            self.connection_epoch.fetch_add(1, Ordering::AcqRel) + 1
        }

        fn recover_link(&mut self) {
            self.recovery_attempts.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct HealthProbeSnapshotTransport {
        configure_attempts: Arc<AtomicU64>,
        health_attempts: Arc<AtomicU64>,
        recovery_attempts: Arc<AtomicU64>,
        health_outcomes: Arc<Mutex<std::collections::VecDeque<bool>>>,
        connection_epoch: Arc<AtomicU64>,
    }

    struct ReceiptBarrierSnapshotTransport {
        configure_attempts: Arc<AtomicU64>,
        receipt_attempts: Arc<AtomicU64>,
        pending_receipt: Arc<AtomicBool>,
        first_drain_started: std::sync::mpsc::Sender<()>,
        first_drain_release: Option<std::sync::mpsc::Receiver<()>>,
        receipt_succeeds: bool,
        events: Arc<Mutex<Vec<&'static str>>>,
        conversions: Arc<Mutex<Vec<u64>>>,
        recovery_attempts: Arc<AtomicU64>,
        connection_epoch: Arc<AtomicU64>,
    }

    impl SnapshotTransport for ReceiptBarrierSnapshotTransport {
        fn configure(&mut self, _request: &Request) -> SnapshotConfigureOutcome {
            self.configure_attempts.fetch_add(1, Ordering::AcqRel);
            SnapshotConfigureOutcome::Configured
        }

        fn convert(&mut self, snapshot: &CompositionSnapshot) -> Option<LiveSnapshotResult> {
            self.events.lock().unwrap().push("convert");
            self.conversions
                .lock()
                .unwrap()
                .push(snapshot.identity.revision);
            Some(LiveSnapshotResult {
                identity: snapshot.identity,
                purpose: snapshot.purpose,
                text: "converted".to_string(),
                candidates: None,
                candidate_remaining: None,
                baseline: 1,
                enhancement: false,
                auto_commit: None,
            })
        }

        fn drain_auto_commit_receipts(&mut self) -> bool {
            if let Some(release) = self.first_drain_release.take() {
                self.first_drain_started.send(()).unwrap();
                release.recv().unwrap();
                return true;
            }
            if self.pending_receipt.swap(false, Ordering::AcqRel) {
                self.receipt_attempts.fetch_add(1, Ordering::AcqRel);
                self.events.lock().unwrap().push("receipt");
                return self.receipt_succeeds;
            }
            true
        }

        fn connection_epoch(&self) -> u64 {
            self.connection_epoch.load(Ordering::Acquire)
        }

        fn invalidate(&mut self) -> u64 {
            self.connection_epoch.fetch_add(1, Ordering::AcqRel) + 1
        }

        fn recover_link(&mut self) {
            self.recovery_attempts.fetch_add(1, Ordering::AcqRel);
        }
    }

    impl SnapshotTransport for HealthProbeSnapshotTransport {
        fn configure(&mut self, _request: &Request) -> SnapshotConfigureOutcome {
            self.configure_attempts.fetch_add(1, Ordering::AcqRel);
            SnapshotConfigureOutcome::Configured
        }

        fn convert(&mut self, snapshot: &CompositionSnapshot) -> Option<LiveSnapshotResult> {
            Some(LiveSnapshotResult {
                identity: snapshot.identity,
                purpose: snapshot.purpose,
                text: "healthy".to_string(),
                candidates: None,
                candidate_remaining: None,
                baseline: 1,
                enhancement: false,
                auto_commit: None,
            })
        }

        fn health_probe(&mut self) -> bool {
            self.health_attempts.fetch_add(1, Ordering::AcqRel);
            self.health_outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(true)
        }

        fn connection_epoch(&self) -> u64 {
            self.connection_epoch.load(Ordering::Acquire)
        }

        fn invalidate(&mut self) -> u64 {
            self.connection_epoch.fetch_add(1, Ordering::AcqRel) + 1
        }

        fn recover_link(&mut self) {
            self.recovery_attempts.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct BlockingHealthProbeTransport {
        started: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
        recoveries: Arc<AtomicU64>,
        invalidations: Arc<AtomicU64>,
        connection_epoch: Arc<AtomicU64>,
        health_result: bool,
    }

    struct HealthFailureReplayTransport {
        configure_count: u32,
        health_started: Option<std::sync::mpsc::Sender<()>>,
        health_release: Option<std::sync::mpsc::Receiver<()>>,
        reconfigure_started: Option<std::sync::mpsc::Sender<()>>,
        reconfigure_release: Option<std::sync::mpsc::Receiver<()>>,
        conversions: Arc<Mutex<Vec<SnapshotIdentity>>>,
        connection_epoch: Arc<AtomicU64>,
    }

    impl SnapshotTransport for HealthFailureReplayTransport {
        fn configure(&mut self, _request: &Request) -> SnapshotConfigureOutcome {
            self.configure_count += 1;
            if self.configure_count == 2 {
                if let Some(started) = self.reconfigure_started.take() {
                    started.send(()).unwrap();
                }
                if let Some(release) = self.reconfigure_release.take() {
                    release.recv().unwrap();
                }
            }
            SnapshotConfigureOutcome::Configured
        }

        fn convert(&mut self, snapshot: &CompositionSnapshot) -> Option<LiveSnapshotResult> {
            self.conversions.lock().unwrap().push(snapshot.identity);
            Some(LiveSnapshotResult {
                identity: snapshot.identity,
                purpose: snapshot.purpose,
                text: format!("recovered:{}", snapshot.identity.revision),
                candidates: None,
                candidate_remaining: None,
                baseline: 1,
                enhancement: false,
                auto_commit: None,
            })
        }

        fn health_probe(&mut self) -> bool {
            let Some(started) = self.health_started.take() else {
                return true;
            };
            started.send(()).unwrap();
            self.health_release.take().unwrap().recv().unwrap();
            false
        }

        fn connection_epoch(&self) -> u64 {
            self.connection_epoch.load(Ordering::Acquire)
        }

        fn invalidate(&mut self) -> u64 {
            self.connection_epoch.fetch_add(1, Ordering::AcqRel) + 1
        }

        fn recover_link(&mut self) {}
    }

    impl SnapshotTransport for BlockingHealthProbeTransport {
        fn configure(&mut self, _request: &Request) -> SnapshotConfigureOutcome {
            SnapshotConfigureOutcome::Configured
        }

        fn convert(&mut self, _snapshot: &CompositionSnapshot) -> Option<LiveSnapshotResult> {
            None
        }

        fn health_probe(&mut self) -> bool {
            self.started.send(()).unwrap();
            self.release.recv().unwrap();
            self.health_result
        }

        fn connection_epoch(&self) -> u64 {
            self.connection_epoch.load(Ordering::Acquire)
        }

        fn invalidate(&mut self) -> u64 {
            self.invalidations.fetch_add(1, Ordering::AcqRel);
            self.connection_epoch.fetch_add(1, Ordering::AcqRel) + 1
        }

        fn recover_link(&mut self) {
            self.recoveries.fetch_add(1, Ordering::AcqRel);
        }
    }

    impl SnapshotTransport for CrashLoopSnapshotTransport {
        fn configure(&mut self, _request: &Request) -> SnapshotConfigureOutcome {
            if self.configure_attempts.fetch_add(1, Ordering::AcqRel)
                >= self.failures_before_recovery
            {
                SnapshotConfigureOutcome::Configured
            } else {
                SnapshotConfigureOutcome::RetryableFailure
            }
        }

        fn convert(&mut self, snapshot: &CompositionSnapshot) -> Option<LiveSnapshotResult> {
            Some(LiveSnapshotResult {
                identity: snapshot.identity,
                purpose: snapshot.purpose,
                text: "recovered".to_string(),
                candidates: None,
                candidate_remaining: None,
                baseline: 1,
                enhancement: false,
                auto_commit: None,
            })
        }

        fn connection_epoch(&self) -> u64 {
            self.connection_epoch.load(Ordering::Acquire)
        }

        fn invalidate(&mut self) -> u64 {
            self.connection_epoch.fetch_add(1, Ordering::AcqRel) + 1
        }

        fn recover_link(&mut self) {
            self.recovery_attempts.fetch_add(1, Ordering::AcqRel);
        }
    }

    impl SnapshotTransport for ShutdownSnapshotTransport {
        fn configure(&mut self, request: &Request) -> SnapshotConfigureOutcome {
            self.configured_requests
                .lock()
                .unwrap()
                .push(match request {
                    Request::Ping => "g1",
                    Request::Shutdown => "g2",
                    _ => "other",
                });
            SnapshotConfigureOutcome::Configured
        }

        fn convert(&mut self, snapshot: &CompositionSnapshot) -> Option<LiveSnapshotResult> {
            let mut conversions = self.conversions.lock().unwrap();
            conversions.push(snapshot.identity.revision);
            let first = conversions.len() == 1;
            drop(conversions);
            if first {
                self.started.send(()).unwrap();
                self.release.recv().unwrap();
            }
            Some(LiveSnapshotResult {
                identity: snapshot.identity,
                purpose: snapshot.purpose,
                text: "result".to_string(),
                candidates: None,
                candidate_remaining: None,
                baseline: 1,
                enhancement: false,
                auto_commit: None,
            })
        }

        fn connection_epoch(&self) -> u64 {
            self.connection_epoch.load(Ordering::Acquire)
        }

        fn invalidate(&mut self) -> u64 {
            self.invalidations.fetch_add(1, Ordering::AcqRel);
            self.connection_epoch.fetch_add(1, Ordering::AcqRel) + 1
        }
    }

    fn snapshot(
        configuration_generation: u64,
        connection_generation: u64,
        revision: u64,
    ) -> CompositionSnapshot {
        CompositionSnapshot {
            identity: SnapshotIdentity {
                composition: 1,
                revision,
                configuration_generation,
                connection_generation,
            },
            purpose: SnapshotPurpose::Live,
            segments: vec![segment("nihon")],
            left_context: None,
        }
    }

    fn explicit_snapshot(
        configuration_generation: u64,
        connection_generation: u64,
        revision: u64,
    ) -> CompositionSnapshot {
        CompositionSnapshot {
            purpose: SnapshotPurpose::Explicit,
            ..snapshot(configuration_generation, connection_generation, revision)
        }
    }

    fn offer_test_snapshot(
        sender: &SyncSender<SnapshotCommand>,
        pending: &ArrayQueue<CompositionSnapshot>,
        work_notified: &AtomicBool,
        snapshot: CompositionSnapshot,
    ) -> bool {
        offer_live_snapshot(
            sender,
            pending,
            work_notified,
            &AtomicBool::new(true),
            snapshot,
        )
    }

    fn desired_configuration_slot() -> (
        Arc<ArrayQueue<DesiredSnapshotConfiguration>>,
        Arc<AtomicU64>,
    ) {
        (Arc::new(ArrayQueue::new(1)), Arc::new(AtomicU64::new(0)))
    }

    #[test]
    fn explicit_snapshot_replaces_an_obsolete_pending_live_snapshot() {
        let (sender, receiver) = sync_channel(1);
        let pending = ArrayQueue::new(1);
        let notified = AtomicBool::new(false);
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &notified,
            snapshot(1, 1, 1),
        ));
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &notified,
            explicit_snapshot(1, 1, 1),
        ));

        assert_eq!(
            pending.pop().map(|snapshot| snapshot.purpose),
            Some(SnapshotPurpose::Explicit)
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(SnapshotCommand::WorkAvailable)
        ));
        assert!(receiver.try_recv().is_err());
    }

    fn offer_configuration(
        sender: &SyncSender<SnapshotCommand>,
        configurations: &ArrayQueue<DesiredSnapshotConfiguration>,
        published_generation: &AtomicU64,
        generation: u64,
        request: Request,
    ) {
        let _ = configurations.force_push(DesiredSnapshotConfiguration {
            generation,
            request,
        });
        published_generation.store(generation, Ordering::Release);
        let _ = sender.try_send(SnapshotCommand::DesiredConfigurationChanged);
    }

    #[test]
    fn blocked_stateful_insert_does_not_block_snapshot_conversion() {
        let (mailbox, stateful_receiver) = bounded_mailbox(2);
        let shared = mailbox.shared.clone();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let stateful_worker = std::thread::spawn(move || {
            run_worker(
                stateful_receiver,
                shared,
                GatedTransport {
                    started: started_tx,
                    release: release_rx,
                    applied: Arc::new(AtomicU64::new(0)),
                    closes: Arc::new(AtomicU64::new(0)),
                },
            )
        });
        assert!(mailbox.try_reseed(RequestId(1), vec![segment("n")]));
        started_rx.recv_timeout(Duration::from_millis(100)).unwrap();

        let (snapshot_sender, snapshot_receiver) = sync_channel(2);
        let results = Arc::new(ArrayQueue::new(1));
        let result_sender = results.clone();
        let acks = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let ack_sender = acks.clone();
        let conversions = Arc::new(Mutex::new(Vec::new()));
        let observed = conversions.clone();
        let connection_epoch = Arc::new(AtomicU64::new(1));
        let (desired, desired_generation) = desired_configuration_slot();
        let worker_desired = desired.clone();
        let worker_desired_generation = desired_generation.clone();
        let pending = Arc::new(ArrayQueue::new(1));
        let worker_pending = pending.clone();
        let work_notified = Arc::new(AtomicBool::new(false));
        let worker_notified = work_notified.clone();
        let snapshot_worker = std::thread::spawn(move || {
            run_snapshot_worker(
                snapshot_receiver,
                worker_desired,
                worker_desired_generation,
                Arc::new(AtomicBool::new(false)),
                worker_pending,
                worker_notified,
                SnapshotJournal {
                    configure_outcomes: Arc::new(Mutex::new(Default::default())),
                    convert_outcomes: Arc::new(Mutex::new(Default::default())),
                    conversions,
                    connection_epoch,
                },
                result_sender,
                ack_sender,
            )
        });
        offer_configuration(
            &snapshot_sender,
            &desired,
            &desired_generation,
            7,
            Request::Ping,
        );
        offer_test_snapshot(
            &snapshot_sender,
            &pending,
            &work_notified,
            snapshot(7, 1, 11),
        );

        assert_eq!(
            acks.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 7,
                connection_epoch: 1,
            }
        );
        assert_eq!(
            results
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .text,
            "converted:11"
        );
        assert_eq!(*observed.lock().unwrap(), vec![11]);

        snapshot_sender.send(SnapshotCommand::Close).unwrap();
        snapshot_worker.join().unwrap();
        release_tx.send(()).unwrap();
        drop(mailbox);
        stateful_worker.join().unwrap();
    }

    #[test]
    fn classic_snapshot_is_observable_before_its_late_gpu_enhancement() {
        let (sender, receiver) = sync_channel(2);
        let results = Arc::new(ArrayQueue::new(1));
        let worker_results = results.clone();
        let enhancement_results = Arc::new(ArrayQueue::new(1));
        let worker_enhancement_results = enhancement_results.clone();
        let pending_enhancement = Arc::new(ArrayQueue::new(1));
        let worker_pending_enhancement = pending_enhancement.clone();
        let latest_serial = Arc::new(AtomicU64::new(0));
        let worker_latest_serial = latest_serial.clone();
        let publisher = SnapshotEnhancementPublisher {
            pending: pending_enhancement,
            latest_serial,
        };
        let shutdown = Arc::new(AtomicBool::new(false));
        let enhancement_shutdown = shutdown.clone();
        let enhancement_worker = std::thread::spawn(move || {
            run_snapshot_enhancement_worker(
                enhancement_shutdown,
                worker_pending_enhancement,
                worker_latest_serial,
                ReadyEnhancementTransport,
                worker_enhancement_results,
            )
        });
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let worker_statuses = statuses.clone();
        let (desired, desired_generation) = desired_configuration_slot();
        let worker_desired = desired.clone();
        let worker_desired_generation = desired_generation.clone();
        let pending = Arc::new(ArrayQueue::new(1));
        let worker_pending = pending.clone();
        let notified = Arc::new(AtomicBool::new(false));
        let worker_notified = notified.clone();
        let worker = std::thread::spawn(move || {
            run_snapshot_worker(
                receiver,
                worker_desired,
                worker_desired_generation,
                Arc::new(AtomicBool::new(false)),
                worker_pending,
                worker_notified,
                EnhancementSnapshotTransport {
                    connection_epoch: Arc::new(AtomicU64::new(1)),
                    publisher,
                },
                worker_results,
                worker_statuses,
            )
        });
        offer_configuration(&sender, &desired, &desired_generation, 7, Request::Ping);
        offer_test_snapshot(&sender, &pending, &notified, snapshot(7, 1, 11));
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)),
            Ok(SnapshotStatus::Configured { .. })
        ));
        let classic = results.recv_timeout(Duration::from_millis(100)).unwrap();
        assert_eq!(
            (classic.text.as_str(), classic.enhancement),
            ("classic", false)
        );
        let enhanced = enhancement_results
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        assert_eq!(
            (enhanced.text.as_str(), enhanced.enhancement),
            ("gpu", true)
        );
        assert_eq!(enhanced.baseline, classic.baseline);
        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
        shutdown.store(true, Ordering::Release);
        enhancement_worker.join().unwrap();
    }

    #[test]
    fn blocked_enhancement_poll_does_not_delay_the_next_classic_snapshot() {
        let (publisher, enhancement_pending, latest_serial) = enhancement_mailbox();
        let enhancement_results = Arc::new(ArrayQueue::new(1));
        let worker_enhancement_results = enhancement_results.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let enhancement_shutdown = shutdown.clone();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let enhancement_worker = std::thread::spawn(move || {
            run_snapshot_enhancement_worker(
                enhancement_shutdown,
                enhancement_pending,
                latest_serial,
                GatedEnhancementTransport {
                    started: started_tx,
                    release: Some(release_rx),
                },
                worker_enhancement_results,
            )
        });

        let (sender, receiver) = sync_channel(2);
        let results = Arc::new(ArrayQueue::new(1));
        let worker_results = results.clone();
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let worker_statuses = statuses.clone();
        let (desired, desired_generation) = desired_configuration_slot();
        let worker_desired = desired.clone();
        let worker_desired_generation = desired_generation.clone();
        let pending = Arc::new(ArrayQueue::new(1));
        let worker_pending = pending.clone();
        let notified = Arc::new(AtomicBool::new(false));
        let worker_notified = notified.clone();
        let classic_worker = std::thread::spawn(move || {
            run_snapshot_worker(
                receiver,
                worker_desired,
                worker_desired_generation,
                Arc::new(AtomicBool::new(false)),
                worker_pending,
                worker_notified,
                EnhancementSnapshotTransport {
                    connection_epoch: Arc::new(AtomicU64::new(1)),
                    publisher,
                },
                worker_results,
                worker_statuses,
            )
        });

        offer_configuration(&sender, &desired, &desired_generation, 7, Request::Ping);
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)),
            Ok(SnapshotStatus::Configured { .. })
        ));
        offer_test_snapshot(&sender, &pending, &notified, snapshot(7, 1, 11));
        assert_eq!(
            results
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .identity
                .revision,
            11
        );
        started_rx.recv_timeout(Duration::from_millis(100)).unwrap();

        offer_test_snapshot(&sender, &pending, &notified, snapshot(7, 1, 12));
        let next_classic = results.recv_timeout(Duration::from_millis(100)).unwrap();
        assert_eq!(
            (next_classic.identity.revision, next_classic.enhancement),
            (12, false)
        );
        assert!(statuses.try_recv().is_err());

        release_tx.send(()).unwrap();
        let enhancement = enhancement_results
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        assert_eq!(
            (enhancement.identity.revision, enhancement.baseline),
            (12, 42)
        );
        sender.send(SnapshotCommand::Close).unwrap();
        classic_worker.join().unwrap();
        shutdown.store(true, Ordering::Release);
        enhancement_worker.join().unwrap();
    }

    #[test]
    fn enhancement_failures_do_not_invalidate_the_classic_lane() {
        let (publisher, enhancement_pending, latest_serial) = enhancement_mailbox();
        let enhancement_results = Arc::new(ArrayQueue::new(1));
        let worker_enhancement_results = enhancement_results.clone();
        let calls = Arc::new(AtomicU64::new(0));
        let observed_calls = calls.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let enhancement_shutdown = shutdown.clone();
        let enhancement_worker = std::thread::spawn(move || {
            run_snapshot_enhancement_worker(
                enhancement_shutdown,
                enhancement_pending,
                latest_serial,
                FailingEnhancementTransport { calls },
                worker_enhancement_results,
            )
        });

        let (sender, receiver) = sync_channel(2);
        let results = Arc::new(ArrayQueue::new(1));
        let worker_results = results.clone();
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let worker_statuses = statuses.clone();
        let (desired, desired_generation) = desired_configuration_slot();
        let worker_desired = desired.clone();
        let worker_desired_generation = desired_generation.clone();
        let pending = Arc::new(ArrayQueue::new(1));
        let worker_pending = pending.clone();
        let notified = Arc::new(AtomicBool::new(false));
        let worker_notified = notified.clone();
        let classic_worker = std::thread::spawn(move || {
            run_snapshot_worker(
                receiver,
                worker_desired,
                worker_desired_generation,
                Arc::new(AtomicBool::new(false)),
                worker_pending,
                worker_notified,
                EnhancementSnapshotTransport {
                    connection_epoch: Arc::new(AtomicU64::new(1)),
                    publisher,
                },
                worker_results,
                worker_statuses,
            )
        });

        offer_configuration(&sender, &desired, &desired_generation, 7, Request::Ping);
        assert_eq!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 7,
                connection_epoch: 1,
            }
        );
        for revision in [11, 12, 13] {
            offer_test_snapshot(&sender, &pending, &notified, snapshot(7, 1, revision));
            let classic = results.recv_timeout(Duration::from_millis(100)).unwrap();
            assert_eq!(
                (
                    classic.identity.revision,
                    classic.identity.connection_generation
                ),
                (revision, 1)
            );
            wait_for_count(&observed_calls, revision - 10);
        }
        let enhancement = enhancement_results
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        assert_eq!(
            (enhancement.identity.revision, enhancement.baseline),
            (13, 42)
        );
        assert!(!statuses
            .try_iter()
            .any(|status| matches!(status, SnapshotStatus::Invalidated { .. })));

        sender.send(SnapshotCommand::Close).unwrap();
        classic_worker.join().unwrap();
        shutdown.store(true, Ordering::Release);
        enhancement_worker.join().unwrap();
    }

    #[test]
    fn always_pending_enhancement_stops_polling_at_its_absolute_deadline() {
        let pending = Arc::new(ArrayQueue::new(1));
        let worker_pending = pending.clone();
        let latest_serial = Arc::new(AtomicU64::new(1));
        let worker_latest_serial = latest_serial.clone();
        let results = Arc::new(ArrayQueue::new(1));
        let calls = Arc::new(AtomicU64::new(0));
        let observed_calls = calls.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let deadline = Instant::now() + Duration::from_millis(20);
        pending
            .push(SnapshotEnhancementRequest {
                serial: 1,
                identity: snapshot(7, 1, 11).identity,
                purpose: SnapshotPurpose::Live,
                baseline: 42,
                deadline,
            })
            .unwrap();
        let worker = std::thread::spawn(move || {
            run_snapshot_enhancement_worker(
                worker_shutdown,
                worker_pending,
                worker_latest_serial,
                BlockingPendingEnhancementTransport {
                    started: started_tx,
                    release: release_rx,
                    calls,
                },
                results,
            )
        });
        started_rx.recv_timeout(Duration::from_millis(100)).unwrap();
        while Instant::now() < deadline {
            std::thread::yield_now();
        }
        release_tx.send(()).unwrap();
        wait_for_count(&observed_calls, 1);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(observed_calls.load(Ordering::Acquire), 1);

        latest_serial.store(2, Ordering::Release);
        pending
            .push(SnapshotEnhancementRequest {
                serial: 2,
                identity: snapshot(7, 1, 12).identity,
                purpose: SnapshotPurpose::Live,
                baseline: 43,
                deadline: Instant::now(),
            })
            .unwrap();
        shutdown.store(true, Ordering::Release);
        worker.join().unwrap();
        assert_eq!(observed_calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn malformed_enhancement_identity_baseline_and_candidate_ranges_are_unavailable() {
        let identity = snapshot(7, 1, 11).identity;
        let response = |revision, baseline, candidates, remaining| Response::SnapshotEnhancement {
            composition: identity.composition,
            revision,
            configuration_generation: identity.configuration_generation,
            connection_generation: identity.connection_generation,
            baseline,
            text: "gpu".to_string(),
            candidates,
            candidate_remaining: remaining,
        };
        for malformed in [
            response(12, 42, None, None),
            response(11, 41, None, None),
            response(
                11,
                42,
                Some(vec!["a".to_string(), "b".to_string()]),
                Some(vec![String::new()]),
            ),
        ] {
            assert!(matches!(
                decode_snapshot_enhancement(malformed, identity, SnapshotPurpose::Explicit, 42,),
                SnapshotEnhancementPoll::Unavailable
            ));
        }
    }

    #[test]
    fn enhancement_io_budget_never_outlives_its_absolute_deadline() {
        let now = Instant::now();
        assert_eq!(
            bounded_enhancement_timeout(
                now,
                now + Duration::from_millis(50),
                Duration::from_millis(500),
            ),
            Some(Duration::from_millis(50))
        );
        assert_eq!(
            bounded_enhancement_timeout(
                now,
                now + Duration::from_millis(500),
                Duration::from_millis(100),
            ),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            bounded_enhancement_timeout(now, now, Duration::from_millis(100)),
            None
        );
        assert_eq!(
            bounded_enhancement_timeout(
                now + Duration::from_millis(1),
                now,
                Duration::from_millis(100),
            ),
            None
        );
    }

    #[test]
    fn failed_reload_cannot_ack_or_convert_as_the_requested_generation() {
        let (sender, receiver) = sync_channel(2);
        let results = Arc::new(ArrayQueue::new(1));
        let result_sender = results.clone();
        let acks = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let ack_sender = acks.clone();
        let conversions = Arc::new(Mutex::new(Vec::new()));
        let observed = conversions.clone();
        let configure_outcomes = Arc::new(Mutex::new([false].into_iter().collect()));
        let connection_epoch = Arc::new(AtomicU64::new(1));
        let (desired, desired_generation) = desired_configuration_slot();
        let worker_desired = desired.clone();
        let worker_desired_generation = desired_generation.clone();
        let worker_pending = Arc::new(ArrayQueue::new(1));
        let worker_notified = Arc::new(AtomicBool::new(false));
        let worker = std::thread::spawn(move || {
            run_snapshot_worker(
                receiver,
                worker_desired,
                worker_desired_generation,
                Arc::new(AtomicBool::new(false)),
                worker_pending,
                worker_notified,
                SnapshotJournal {
                    configure_outcomes,
                    convert_outcomes: Arc::new(Mutex::new(Default::default())),
                    conversions,
                    connection_epoch,
                },
                result_sender,
                ack_sender,
            )
        });
        offer_configuration(&sender, &desired, &desired_generation, 9, Request::Ping);
        assert_eq!(
            acks.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Invalidated {
                configuration_generation: 9,
                connection_epoch: 2,
            }
        );
        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();

        assert!(!acks
            .try_iter()
            .any(|status| matches!(status, SnapshotStatus::Configured { .. })));
        assert!(results.try_recv().is_err());
        assert!(observed.lock().unwrap().is_empty());
    }

    #[test]
    fn failed_snapshot_is_replayed_and_applied_after_timer_recovery_without_new_input() {
        let (sender, receiver) = sync_channel(4);
        let results = Arc::new(ArrayQueue::new(1));
        let result_sender = results.clone();
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let status_sender = statuses.clone();
        let conversions = Arc::new(Mutex::new(Vec::new()));
        let observed = conversions.clone();
        let connection_epoch = Arc::new(AtomicU64::new(1));
        let published_epoch = connection_epoch.clone();
        let (first_started_tx, first_started_rx) = channel();
        let (first_release_tx, first_release_rx) = channel();
        let (desired, desired_generation) = desired_configuration_slot();
        let worker_desired = desired.clone();
        let worker_desired_generation = desired_generation.clone();
        let pending = Arc::new(ArrayQueue::new(1));
        let worker_pending = pending.clone();
        let work_notified = Arc::new(AtomicBool::new(false));
        let worker_notified = work_notified.clone();
        let worker = std::thread::spawn(move || {
            run_snapshot_worker(
                receiver,
                worker_desired,
                worker_desired_generation,
                Arc::new(AtomicBool::new(false)),
                worker_pending,
                worker_notified,
                FailFirstSnapshotBarrier {
                    first_started: first_started_tx,
                    first_release: Some(first_release_rx),
                    receipt_failure_started: None,
                    receipt_failure_release: None,
                    configure_count: 0,
                    fail_reconnected_receipt_once: true,
                    conversions,
                    connection_epoch,
                },
                result_sender,
                status_sender,
            )
        });
        offer_configuration(&sender, &desired, &desired_generation, 4, Request::Ping);
        assert_eq!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 4,
                connection_epoch: 1,
            }
        );

        let mut module = crate::input_module::InputModule::default();
        for ch in "nihon".chars() {
            module.handle(crate::input_module::InputEvent::Key(
                crate::input_module::KeyEvent::Text {
                    ch,
                    style: TextStyle::Kana,
                    replay: crate::input_module::ReplayMode::Delta,
                },
            ));
        }
        let crate::input_module::BackgroundIntent::LiveSnapshot { snapshot } = module
            .live_snapshot(4, 1, None)
            .expect("composing input has a snapshot")
        else {
            panic!("unexpected background intent")
        };
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &work_notified,
            snapshot,
        ));
        first_started_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        module.handle(crate::input_module::InputEvent::Key(
            crate::input_module::KeyEvent::Text {
                ch: 'g',
                style: TextStyle::Kana,
                replay: crate::input_module::ReplayMode::Delta,
            },
        ));
        let crate::input_module::BackgroundIntent::LiveSnapshot { snapshot } = module
            .live_snapshot(4, 1, None)
            .expect("newer composing input has a snapshot")
        else {
            panic!("unexpected background intent")
        };
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &work_notified,
            snapshot,
        ));
        first_release_tx.send(()).unwrap();
        assert_eq!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Invalidated {
                configuration_generation: 4,
                connection_epoch: 2,
            }
        );
        assert_eq!(
            statuses.recv_timeout(Duration::from_secs(2)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 4,
                connection_epoch: 2,
            }
        );
        assert_eq!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Invalidated {
                configuration_generation: 4,
                connection_epoch: 3,
            }
        );
        assert_eq!(
            statuses.recv_timeout(Duration::from_secs(2)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 4,
                connection_epoch: 3,
            }
        );
        assert_eq!(published_epoch.load(Ordering::Acquire), 3);
        assert!(module.rebind_expected_snapshot_connection(4, 3));

        let result = results.recv_timeout(Duration::from_millis(100)).unwrap();
        assert_eq!(result.identity.revision, 6);
        assert_eq!(result.identity.connection_generation, 3);
        let applied = module.handle(crate::input_module::InputEvent::Engine(
            crate::input_module::EngineResult::LiveSnapshot {
                identity: result.identity,
                text: result.text,
            },
        ));
        assert!(matches!(
            applied.immediate,
            Some(crate::input_module::ImmediateOperation::SetPreedit { ref text })
                if text == "converted:6"
        ));
        assert_eq!(*observed.lock().unwrap(), vec![5, 6]);

        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn initial_configuration_failure_recovers_without_a_new_configuration_command() {
        let (sender, receiver) = sync_channel(2);
        let result_sender = Arc::new(ArrayQueue::new(1));
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let status_sender = statuses.clone();
        let (desired, desired_generation) = desired_configuration_slot();
        let worker_desired = desired.clone();
        let worker_desired_generation = desired_generation.clone();
        let pending = Arc::new(ArrayQueue::new(1));
        let worker_pending = pending.clone();
        let work_notified = Arc::new(AtomicBool::new(false));
        let worker_notified = work_notified.clone();
        let worker = std::thread::spawn(move || {
            run_snapshot_worker(
                receiver,
                worker_desired,
                worker_desired_generation,
                Arc::new(AtomicBool::new(false)),
                worker_pending,
                worker_notified,
                SnapshotJournal {
                    configure_outcomes: Arc::new(Mutex::new([false, true].into_iter().collect())),
                    convert_outcomes: Arc::new(Mutex::new(Default::default())),
                    conversions: Arc::new(Mutex::new(Vec::new())),
                    connection_epoch: Arc::new(AtomicU64::new(1)),
                },
                result_sender,
                status_sender,
            )
        });
        offer_configuration(&sender, &desired, &desired_generation, 6, Request::Ping);
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Invalidated {
                configuration_generation: 6,
                connection_epoch: 2,
            }
        ));
        let retry_started = Instant::now();
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &work_notified,
            snapshot(6, 2, 1),
        ));
        assert!(statuses.recv_timeout(Duration::from_millis(200)).is_err());
        assert_eq!(
            statuses.recv_timeout(Duration::from_secs(2)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 6,
                connection_epoch: 2,
            }
        );
        assert!(retry_started.elapsed() >= Duration::from_millis(900));
        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn configuration_success_after_two_seconds_is_still_published() {
        let (sender, receiver) = sync_channel(2);
        let result_sender = Arc::new(ArrayQueue::new(1));
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let status_sender = statuses.clone();
        let (desired, desired_generation) = desired_configuration_slot();
        let worker_desired = desired.clone();
        let worker_desired_generation = desired_generation.clone();
        let worker_pending = Arc::new(ArrayQueue::new(1));
        let worker_notified = Arc::new(AtomicBool::new(false));
        let worker = std::thread::spawn(move || {
            run_snapshot_worker(
                receiver,
                worker_desired,
                worker_desired_generation,
                Arc::new(AtomicBool::new(false)),
                worker_pending,
                worker_notified,
                SnapshotJournal {
                    configure_outcomes: Arc::new(Mutex::new(
                        [false, false, false, true].into_iter().collect(),
                    )),
                    convert_outcomes: Arc::new(Mutex::new(Default::default())),
                    conversions: Arc::new(Mutex::new(Vec::new())),
                    connection_epoch: Arc::new(AtomicU64::new(1)),
                },
                result_sender,
                status_sender,
            )
        });
        offer_configuration(&sender, &desired, &desired_generation, 8, Request::Ping);
        let started = Instant::now();
        let configured = loop {
            let status = statuses.recv_timeout(Duration::from_secs(5)).unwrap();
            if matches!(status, SnapshotStatus::Configured { .. }) {
                break status;
            }
        };
        assert!(started.elapsed() >= Duration::from_secs(2));
        assert_eq!(
            configured,
            SnapshotStatus::Configured {
                configuration_generation: 8,
                connection_epoch: 4,
            }
        );
        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn crash_loop_keeps_retrying_past_the_old_limit_without_new_input() {
        let (sender, receiver) = sync_channel(1);
        let results = Arc::new(ArrayQueue::new(1));
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let (desired, desired_generation) = desired_configuration_slot();
        let attempts = Arc::new(AtomicU64::new(0));
        let recoveries = Arc::new(AtomicU64::new(0));
        let worker = {
            let desired = desired.clone();
            let desired_generation = desired_generation.clone();
            let statuses = statuses.clone();
            let attempts = attempts.clone();
            let recoveries = recoveries.clone();
            std::thread::spawn(move || {
                run_snapshot_worker_with_retry(
                    receiver,
                    desired,
                    desired_generation,
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(ArrayQueue::new(1)),
                    Arc::new(AtomicBool::new(false)),
                    CrashLoopSnapshotTransport {
                        configure_attempts: attempts,
                        recovery_attempts: recoveries,
                        failures_before_recovery: 7,
                        connection_epoch: Arc::new(AtomicU64::new(1)),
                    },
                    results,
                    statuses,
                    RetryPolicy {
                        base: Duration::from_millis(1),
                        cap: Duration::from_millis(4),
                        health_probe_interval: Duration::from_secs(1),
                    },
                )
            })
        };

        offer_configuration(&sender, &desired, &desired_generation, 12, Request::Ping);
        let deadline = Instant::now() + Duration::from_secs(1);
        let configured = loop {
            if let Some(status) = statuses.pop() {
                if matches!(status, SnapshotStatus::Configured { .. }) {
                    break status;
                }
            }
            assert!(Instant::now() < deadline, "supervisor stopped retrying");
            std::thread::yield_now();
        };
        assert_eq!(attempts.load(Ordering::Acquire), 8);
        assert_eq!(recoveries.load(Ordering::Acquire), 7);
        assert!(matches!(
            configured,
            SnapshotStatus::Configured {
                configuration_generation: 12,
                connection_epoch: 8,
            }
        ));

        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn version_mismatch_is_latched_without_recovery_or_retry() {
        let (sender, receiver) = sync_channel(1);
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let (desired, desired_generation) = desired_configuration_slot();
        let attempts = Arc::new(AtomicU64::new(0));
        let recoveries = Arc::new(AtomicU64::new(0));
        let worker = {
            let desired = desired.clone();
            let desired_generation = desired_generation.clone();
            let statuses = statuses.clone();
            let attempts = attempts.clone();
            let recoveries = recoveries.clone();
            std::thread::spawn(move || {
                run_snapshot_worker_with_retry(
                    receiver,
                    desired,
                    desired_generation,
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(ArrayQueue::new(1)),
                    Arc::new(AtomicBool::new(false)),
                    VersionMismatchSnapshotTransport {
                        configure_attempts: attempts,
                        recovery_attempts: recoveries,
                        connection_epoch: Arc::new(AtomicU64::new(1)),
                    },
                    Arc::new(ArrayQueue::new(1)),
                    statuses,
                    RetryPolicy {
                        base: Duration::from_millis(1),
                        cap: Duration::from_millis(1),
                        health_probe_interval: Duration::from_millis(1),
                    },
                )
            })
        };

        offer_configuration(&sender, &desired, &desired_generation, 15, Request::Ping);
        let deadline = Instant::now() + Duration::from_secs(1);
        let status = loop {
            if let Some(status) = statuses.pop() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "mismatch status was not published"
            );
            std::thread::yield_now();
        };
        assert_eq!(
            status,
            SnapshotStatus::VersionMismatch {
                configuration_generation: 15,
                connection_epoch: 2,
                actual: Some(5),
                actual_boot: Some("old-build".into()),
            }
        );
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(attempts.load(Ordering::Acquire), 1);
        assert_eq!(recoveries.load(Ordering::Acquire), 0);

        offer_configuration(&sender, &desired, &desired_generation, 16, Request::Ping);
        assert_eq!(
            statuses.recv_timeout(Duration::from_secs(1)).unwrap(),
            SnapshotStatus::VersionMismatch {
                configuration_generation: 16,
                connection_epoch: 3,
                actual: Some(5),
                actual_boot: Some("old-build".into()),
            }
        );
        assert_eq!(attempts.load(Ordering::Acquire), 2);
        assert_eq!(recoveries.load(Ordering::Acquire), 0);

        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn retryable_failure_holds_pending_snapshot_without_work_until_close() {
        let (sender, receiver) = sync_channel(2);
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let (desired, desired_generation) = desired_configuration_slot();
        let pending = Arc::new(ArrayQueue::new(1));
        let notified = Arc::new(AtomicBool::new(false));
        let configure_attempts = Arc::new(AtomicU64::new(0));
        let recovery_attempts = Arc::new(AtomicU64::new(0));
        let receipt_attempts = Arc::new(AtomicU64::new(0));
        let conversion_attempts = Arc::new(AtomicU64::new(0));
        let (done_tx, done_rx) = channel();
        let worker = {
            let desired = desired.clone();
            let desired_generation = desired_generation.clone();
            let pending = pending.clone();
            let notified = notified.clone();
            let statuses = statuses.clone();
            let configure_attempts = configure_attempts.clone();
            let recovery_attempts = recovery_attempts.clone();
            let receipt_attempts = receipt_attempts.clone();
            let conversion_attempts = conversion_attempts.clone();
            std::thread::spawn(move || {
                run_snapshot_worker_with_retry(
                    receiver,
                    desired,
                    desired_generation,
                    Arc::new(AtomicBool::new(false)),
                    pending,
                    notified,
                    UnconfiguredCountingTransport {
                        outcome: SnapshotConfigureOutcome::RetryableFailure,
                        configure_attempts,
                        recovery_attempts,
                        receipt_attempts,
                        conversion_attempts,
                        connection_epoch: Arc::new(AtomicU64::new(1)),
                    },
                    Arc::new(ArrayQueue::new(1)),
                    statuses,
                    RetryPolicy {
                        base: Duration::from_secs(60),
                        cap: Duration::from_secs(60),
                        health_probe_interval: Duration::from_secs(60),
                    },
                );
                done_tx.send(()).unwrap();
            })
        };

        offer_configuration(&sender, &desired, &desired_generation, 17, Request::Ping);
        assert_eq!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Invalidated {
                configuration_generation: 17,
                connection_epoch: 2,
            }
        );
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &notified,
            snapshot(17, 2, 1),
        ));
        sender.send(SnapshotCommand::Close).unwrap();
        done_rx.recv_timeout(Duration::from_millis(100)).unwrap();
        worker.join().unwrap();

        assert_eq!(configure_attempts.load(Ordering::Acquire), 1);
        assert_eq!(recovery_attempts.load(Ordering::Acquire), 1);
        assert_eq!(receipt_attempts.load(Ordering::Acquire), 0);
        assert_eq!(conversion_attempts.load(Ordering::Acquire), 0);
        assert_eq!(pending.pop().unwrap().identity.revision, 1);
    }

    #[test]
    fn version_mismatch_holds_pending_snapshot_without_work_until_close() {
        let (sender, receiver) = sync_channel(2);
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let (desired, desired_generation) = desired_configuration_slot();
        let pending = Arc::new(ArrayQueue::new(1));
        let notified = Arc::new(AtomicBool::new(false));
        let configure_attempts = Arc::new(AtomicU64::new(0));
        let recovery_attempts = Arc::new(AtomicU64::new(0));
        let receipt_attempts = Arc::new(AtomicU64::new(0));
        let conversion_attempts = Arc::new(AtomicU64::new(0));
        let (done_tx, done_rx) = channel();
        let worker = {
            let desired = desired.clone();
            let desired_generation = desired_generation.clone();
            let pending = pending.clone();
            let notified = notified.clone();
            let statuses = statuses.clone();
            let configure_attempts = configure_attempts.clone();
            let recovery_attempts = recovery_attempts.clone();
            let receipt_attempts = receipt_attempts.clone();
            let conversion_attempts = conversion_attempts.clone();
            std::thread::spawn(move || {
                run_snapshot_worker_with_retry(
                    receiver,
                    desired,
                    desired_generation,
                    Arc::new(AtomicBool::new(false)),
                    pending,
                    notified,
                    UnconfiguredCountingTransport {
                        outcome: SnapshotConfigureOutcome::VersionMismatch {
                            actual: Some(5),
                            actual_boot: Some("old-build".into()),
                        },
                        configure_attempts,
                        recovery_attempts,
                        receipt_attempts,
                        conversion_attempts,
                        connection_epoch: Arc::new(AtomicU64::new(1)),
                    },
                    Arc::new(ArrayQueue::new(1)),
                    statuses,
                    RetryPolicy {
                        base: Duration::from_secs(60),
                        cap: Duration::from_secs(60),
                        health_probe_interval: Duration::from_secs(60),
                    },
                );
                done_tx.send(()).unwrap();
            })
        };

        offer_configuration(&sender, &desired, &desired_generation, 18, Request::Ping);
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::VersionMismatch {
                configuration_generation: 18,
                connection_epoch: 2,
                ..
            }
        ));
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &notified,
            snapshot(18, 2, 1),
        ));
        sender.send(SnapshotCommand::Close).unwrap();
        done_rx.recv_timeout(Duration::from_millis(100)).unwrap();
        worker.join().unwrap();

        assert_eq!(configure_attempts.load(Ordering::Acquire), 1);
        assert_eq!(recovery_attempts.load(Ordering::Acquire), 0);
        assert_eq!(receipt_attempts.load(Ordering::Acquire), 0);
        assert_eq!(conversion_attempts.load(Ordering::Acquire), 0);
        assert_eq!(pending.pop().unwrap().identity.revision, 1);
    }

    #[test]
    fn close_preempts_selected_replay_after_receipt_failure() {
        let (sender, receiver) = sync_channel(4);
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let results = Arc::new(ArrayQueue::new(1));
        let (desired, desired_generation) = desired_configuration_slot();
        let pending = Arc::new(ArrayQueue::new(1));
        let notified = Arc::new(AtomicBool::new(false));
        let conversions = Arc::new(Mutex::new(Vec::new()));
        let observed = conversions.clone();
        let (first_started_tx, first_started_rx) = channel();
        let (first_release_tx, first_release_rx) = channel();
        let (receipt_started_tx, receipt_started_rx) = channel();
        let (receipt_release_tx, receipt_release_rx) = channel();
        let (done_tx, done_rx) = channel();
        let worker = {
            let desired = desired.clone();
            let desired_generation = desired_generation.clone();
            let pending = pending.clone();
            let notified = notified.clone();
            let statuses = statuses.clone();
            std::thread::spawn(move || {
                run_snapshot_worker_with_retry(
                    receiver,
                    desired,
                    desired_generation,
                    Arc::new(AtomicBool::new(false)),
                    pending,
                    notified,
                    FailFirstSnapshotBarrier {
                        first_started: first_started_tx,
                        first_release: Some(first_release_rx),
                        receipt_failure_started: Some(receipt_started_tx),
                        receipt_failure_release: Some(receipt_release_rx),
                        configure_count: 0,
                        fail_reconnected_receipt_once: true,
                        conversions,
                        connection_epoch: Arc::new(AtomicU64::new(1)),
                    },
                    results,
                    statuses,
                    RetryPolicy {
                        base: Duration::from_millis(1),
                        cap: Duration::from_millis(1),
                        health_probe_interval: Duration::from_secs(60),
                    },
                );
                done_tx.send(()).unwrap();
            })
        };

        offer_configuration(&sender, &desired, &desired_generation, 19, Request::Ping);
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured { .. }
        ));
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &notified,
            snapshot(19, 1, 5),
        ));
        first_started_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &notified,
            snapshot(19, 1, 6),
        ));
        first_release_tx.send(()).unwrap();
        receipt_started_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        sender.send(SnapshotCommand::Close).unwrap();
        receipt_release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_millis(100)).unwrap();
        worker.join().unwrap();

        assert_eq!(*observed.lock().unwrap(), vec![5]);
    }

    #[test]
    fn reconnect_backoff_doubles_and_stays_capped() {
        let policy = RetryPolicy {
            base: Duration::from_secs(1),
            cap: Duration::from_secs(30),
            health_probe_interval: Duration::from_secs(5),
        };
        assert_eq!(policy.delay(1), Duration::from_secs(1));
        assert_eq!(policy.delay(2), Duration::from_secs(2));
        assert_eq!(policy.delay(5), Duration::from_secs(16));
        assert_eq!(policy.delay(6), Duration::from_secs(30));
        assert_eq!(policy.delay(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn configured_idle_link_recovers_without_a_command_or_new_input() {
        let (sender, receiver) = sync_channel(1);
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let (desired, desired_generation) = desired_configuration_slot();
        let configure_attempts = Arc::new(AtomicU64::new(0));
        let health_attempts = Arc::new(AtomicU64::new(0));
        let recovery_attempts = Arc::new(AtomicU64::new(0));
        let worker = {
            let statuses = statuses.clone();
            let configure_attempts = configure_attempts.clone();
            let health_attempts = health_attempts.clone();
            let recovery_attempts = recovery_attempts.clone();
            let desired = desired.clone();
            let desired_generation = desired_generation.clone();
            std::thread::spawn(move || {
                run_snapshot_worker_with_retry(
                    receiver,
                    desired,
                    desired_generation,
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(ArrayQueue::new(1)),
                    Arc::new(AtomicBool::new(false)),
                    HealthProbeSnapshotTransport {
                        configure_attempts,
                        health_attempts,
                        recovery_attempts,
                        health_outcomes: Arc::new(Mutex::new([false].into_iter().collect())),
                        connection_epoch: Arc::new(AtomicU64::new(1)),
                    },
                    Arc::new(ArrayQueue::new(1)),
                    statuses,
                    RetryPolicy {
                        base: Duration::from_millis(1),
                        cap: Duration::from_millis(4),
                        health_probe_interval: Duration::from_millis(1),
                    },
                )
            })
        };

        offer_configuration(&sender, &desired, &desired_generation, 21, Request::Ping);
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 21,
                connection_epoch: 1,
            }
        ));
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Invalidated {
                configuration_generation: 21,
                connection_epoch: 2,
            }
        ));
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 21,
                connection_epoch: 2,
            }
        ));
        assert_eq!(configure_attempts.load(Ordering::Acquire), 2);
        assert_eq!(recovery_attempts.load(Ordering::Acquire), 1);
        assert!(health_attempts.load(Ordering::Acquire) >= 1);

        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn failed_auto_commit_receipt_invalidates_before_same_epoch_snapshot_conversion() {
        let (sender, receiver) = sync_channel(2);
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let results = Arc::new(ArrayQueue::new(1));
        let (desired, desired_generation) = desired_configuration_slot();
        let pending = Arc::new(ArrayQueue::new(1));
        let notified = Arc::new(AtomicBool::new(false));
        let configure_attempts = Arc::new(AtomicU64::new(0));
        let receipt_attempts = Arc::new(AtomicU64::new(0));
        let pending_receipt = Arc::new(AtomicBool::new(false));
        let (first_drain_started_tx, first_drain_started_rx) = channel();
        let (first_drain_release_tx, first_drain_release_rx) = channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let conversions = Arc::new(Mutex::new(Vec::new()));
        let recovery_attempts = Arc::new(AtomicU64::new(0));
        let connection_epoch = Arc::new(AtomicU64::new(1));
        let worker = {
            let desired = desired.clone();
            let desired_generation = desired_generation.clone();
            let pending = pending.clone();
            let notified = notified.clone();
            let statuses = statuses.clone();
            let results = results.clone();
            let configure_attempts = configure_attempts.clone();
            let receipt_attempts = receipt_attempts.clone();
            let pending_receipt = pending_receipt.clone();
            let events = events.clone();
            let conversions = conversions.clone();
            let recovery_attempts = recovery_attempts.clone();
            let connection_epoch = connection_epoch.clone();
            std::thread::spawn(move || {
                run_snapshot_worker_with_retry(
                    receiver,
                    desired,
                    desired_generation,
                    Arc::new(AtomicBool::new(false)),
                    pending,
                    notified,
                    ReceiptBarrierSnapshotTransport {
                        configure_attempts,
                        receipt_attempts,
                        pending_receipt,
                        first_drain_started: first_drain_started_tx,
                        first_drain_release: Some(first_drain_release_rx),
                        receipt_succeeds: false,
                        events,
                        conversions,
                        recovery_attempts,
                        connection_epoch,
                    },
                    results,
                    statuses,
                    RetryPolicy {
                        base: Duration::from_millis(1),
                        cap: Duration::from_millis(1),
                        health_probe_interval: Duration::from_secs(1),
                    },
                )
            })
        };

        offer_configuration(&sender, &desired, &desired_generation, 31, Request::Ping);
        assert_eq!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 31,
                connection_epoch: 1,
            }
        );
        sender.send(SnapshotCommand::WorkAvailable).unwrap();
        first_drain_started_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        pending_receipt.store(true, Ordering::Release);
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &notified,
            snapshot(31, 1, 1),
        ));
        first_drain_release_tx.send(()).unwrap();
        assert_eq!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Invalidated {
                configuration_generation: 31,
                connection_epoch: 2,
            }
        );
        assert_eq!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 31,
                connection_epoch: 2,
            }
        );
        let replayed = results.recv_timeout(Duration::from_millis(100)).unwrap();
        assert_eq!(replayed.identity.revision, 1);
        assert_eq!(replayed.identity.connection_generation, 2);
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &notified,
            snapshot(31, 2, 2),
        ));
        assert_eq!(
            results
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .identity
                .revision,
            2
        );
        assert_eq!(*conversions.lock().unwrap(), vec![1, 2]);
        assert_eq!(
            *events.lock().unwrap(),
            vec!["receipt", "convert", "convert"]
        );
        assert_eq!(receipt_attempts.load(Ordering::Acquire), 1);
        assert_eq!(configure_attempts.load(Ordering::Acquire), 2);
        assert_eq!(recovery_attempts.load(Ordering::Acquire), 1);

        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn receipt_enqueued_after_first_drain_is_applied_before_snapshot_conversion() {
        let (sender, receiver) = sync_channel(2);
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let results = Arc::new(ArrayQueue::new(1));
        let (desired, desired_generation) = desired_configuration_slot();
        let pending = Arc::new(ArrayQueue::new(1));
        let notified = Arc::new(AtomicBool::new(false));
        let configure_attempts = Arc::new(AtomicU64::new(0));
        let receipt_attempts = Arc::new(AtomicU64::new(0));
        let pending_receipt = Arc::new(AtomicBool::new(false));
        let (first_drain_started_tx, first_drain_started_rx) = channel();
        let (first_drain_release_tx, first_drain_release_rx) = channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let conversions = Arc::new(Mutex::new(Vec::new()));
        let worker = {
            let desired = desired.clone();
            let desired_generation = desired_generation.clone();
            let pending = pending.clone();
            let notified = notified.clone();
            let statuses = statuses.clone();
            let results = results.clone();
            let configure_attempts = configure_attempts.clone();
            let receipt_attempts = receipt_attempts.clone();
            let pending_receipt = pending_receipt.clone();
            let events = events.clone();
            let conversions = conversions.clone();
            std::thread::spawn(move || {
                run_snapshot_worker_with_retry(
                    receiver,
                    desired,
                    desired_generation,
                    Arc::new(AtomicBool::new(false)),
                    pending,
                    notified,
                    ReceiptBarrierSnapshotTransport {
                        configure_attempts,
                        receipt_attempts,
                        pending_receipt,
                        first_drain_started: first_drain_started_tx,
                        first_drain_release: Some(first_drain_release_rx),
                        receipt_succeeds: true,
                        events,
                        conversions,
                        recovery_attempts: Arc::new(AtomicU64::new(0)),
                        connection_epoch: Arc::new(AtomicU64::new(1)),
                    },
                    results,
                    statuses,
                    RetryPolicy {
                        base: Duration::from_millis(1),
                        cap: Duration::from_millis(1),
                        health_probe_interval: Duration::from_secs(1),
                    },
                )
            })
        };

        offer_configuration(&sender, &desired, &desired_generation, 32, Request::Ping);
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 32,
                connection_epoch: 1,
            }
        ));
        sender.send(SnapshotCommand::WorkAvailable).unwrap();
        first_drain_started_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        pending_receipt.store(true, Ordering::Release);
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &notified,
            snapshot(32, 1, 1),
        ));
        first_drain_release_tx.send(()).unwrap();

        assert_eq!(
            results
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .identity
                .revision,
            1
        );
        assert_eq!(*events.lock().unwrap(), vec!["receipt", "convert"]);
        assert_eq!(*conversions.lock().unwrap(), vec![1]);
        assert_eq!(receipt_attempts.load(Ordering::Acquire), 1);
        assert_eq!(configure_attempts.load(Ordering::Acquire), 1);

        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn first_snapshot_offered_during_failed_health_probe_replays_on_new_epoch() {
        let (sender, receiver) = sync_channel(2);
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let results = Arc::new(ArrayQueue::new(1));
        let (desired, desired_generation) = desired_configuration_slot();
        let pending = Arc::new(ArrayQueue::new(1));
        let notified = Arc::new(AtomicBool::new(false));
        let conversions = Arc::new(Mutex::new(Vec::new()));
        let (health_started_tx, health_started_rx) = channel();
        let (health_release_tx, health_release_rx) = channel();
        let worker = {
            let desired = desired.clone();
            let desired_generation = desired_generation.clone();
            let pending = pending.clone();
            let notified = notified.clone();
            let statuses = statuses.clone();
            let results = results.clone();
            let conversions = conversions.clone();
            std::thread::spawn(move || {
                run_snapshot_worker_with_retry(
                    receiver,
                    desired,
                    desired_generation,
                    Arc::new(AtomicBool::new(false)),
                    pending,
                    notified,
                    HealthFailureReplayTransport {
                        configure_count: 0,
                        health_started: Some(health_started_tx),
                        health_release: Some(health_release_rx),
                        reconfigure_started: None,
                        reconfigure_release: None,
                        conversions,
                        connection_epoch: Arc::new(AtomicU64::new(1)),
                    },
                    results,
                    statuses,
                    RetryPolicy {
                        base: Duration::from_millis(1),
                        cap: Duration::from_millis(1),
                        health_probe_interval: Duration::from_millis(1),
                    },
                )
            })
        };

        offer_configuration(&sender, &desired, &desired_generation, 40, Request::Ping);
        assert_eq!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 40,
                connection_epoch: 1,
            }
        );
        health_started_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();

        let mut module = crate::input_module::InputModule::default();
        module.handle(crate::input_module::InputEvent::Key(
            crate::input_module::KeyEvent::Text {
                ch: 'n',
                style: TextStyle::Kana,
                replay: crate::input_module::ReplayMode::Delta,
            },
        ));
        let crate::input_module::BackgroundIntent::LiveSnapshot { snapshot } = module
            .live_snapshot(40, 1, None)
            .expect("first composing input has a snapshot")
        else {
            panic!("unexpected background intent")
        };
        assert!(offer_test_snapshot(&sender, &pending, &notified, snapshot));
        health_release_tx.send(()).unwrap();

        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Invalidated {
                configuration_generation: 40,
                connection_epoch: 2,
            }
        ));
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 40,
                connection_epoch: 2,
            }
        ));
        assert!(module.rebind_expected_snapshot_connection(40, 2));
        let result = results.recv_timeout(Duration::from_millis(100)).unwrap();
        assert_eq!(result.identity.connection_generation, 2);
        let applied = module.handle(crate::input_module::InputEvent::Engine(
            crate::input_module::EngineResult::LiveSnapshot {
                identity: result.identity,
                text: result.text,
            },
        ));
        assert!(matches!(
            applied.immediate,
            Some(crate::input_module::ImmediateOperation::SetPreedit { ref text })
                if text == "recovered:1"
        ));

        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn first_snapshot_offered_after_invalidation_replays_when_reconfigure_completes() {
        let (sender, receiver) = sync_channel(2);
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let results = Arc::new(ArrayQueue::new(1));
        let (desired, desired_generation) = desired_configuration_slot();
        let pending = Arc::new(ArrayQueue::new(1));
        let notified = Arc::new(AtomicBool::new(false));
        let (health_started_tx, health_started_rx) = channel();
        let (health_release_tx, health_release_rx) = channel();
        let (reconfigure_started_tx, reconfigure_started_rx) = channel();
        let (reconfigure_release_tx, reconfigure_release_rx) = channel();
        let worker = {
            let desired = desired.clone();
            let desired_generation = desired_generation.clone();
            let pending = pending.clone();
            let notified = notified.clone();
            let statuses = statuses.clone();
            let results = results.clone();
            std::thread::spawn(move || {
                run_snapshot_worker_with_retry(
                    receiver,
                    desired,
                    desired_generation,
                    Arc::new(AtomicBool::new(false)),
                    pending,
                    notified,
                    HealthFailureReplayTransport {
                        configure_count: 0,
                        health_started: Some(health_started_tx),
                        health_release: Some(health_release_rx),
                        reconfigure_started: Some(reconfigure_started_tx),
                        reconfigure_release: Some(reconfigure_release_rx),
                        conversions: Arc::new(Mutex::new(Vec::new())),
                        connection_epoch: Arc::new(AtomicU64::new(1)),
                    },
                    results,
                    statuses,
                    RetryPolicy {
                        base: Duration::from_millis(1),
                        cap: Duration::from_millis(1),
                        health_probe_interval: Duration::from_millis(1),
                    },
                )
            })
        };

        offer_configuration(&sender, &desired, &desired_generation, 41, Request::Ping);
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured { .. }
        ));
        health_started_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        health_release_tx.send(()).unwrap();
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Invalidated { .. }
        ));
        reconfigure_started_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &notified,
            explicit_snapshot(41, 1, 1),
        ));
        reconfigure_release_tx.send(()).unwrap();
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 41,
                connection_epoch: 2,
            }
        ));
        let result = results.recv_timeout(Duration::from_millis(100)).unwrap();
        assert_eq!(result.identity, explicit_snapshot(41, 2, 1).identity);
        assert_eq!(result.purpose, SnapshotPurpose::Explicit);

        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn reconnect_drops_an_old_configuration_snapshot_without_losing_new_work() {
        let (sender, receiver) = sync_channel(2);
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let results = Arc::new(ArrayQueue::new(1));
        let (desired, desired_generation) = desired_configuration_slot();
        let pending = Arc::new(ArrayQueue::new(1));
        let notified = Arc::new(AtomicBool::new(false));
        let conversions = Arc::new(Mutex::new(Vec::new()));
        let observed = conversions.clone();
        let (health_started_tx, health_started_rx) = channel();
        let (health_release_tx, health_release_rx) = channel();
        let (reconfigure_started_tx, reconfigure_started_rx) = channel();
        let (reconfigure_release_tx, reconfigure_release_rx) = channel();
        let worker = {
            let desired = desired.clone();
            let desired_generation = desired_generation.clone();
            let pending = pending.clone();
            let notified = notified.clone();
            let statuses = statuses.clone();
            let results = results.clone();
            std::thread::spawn(move || {
                run_snapshot_worker_with_retry(
                    receiver,
                    desired,
                    desired_generation,
                    Arc::new(AtomicBool::new(false)),
                    pending,
                    notified,
                    HealthFailureReplayTransport {
                        configure_count: 0,
                        health_started: Some(health_started_tx),
                        health_release: Some(health_release_rx),
                        reconfigure_started: Some(reconfigure_started_tx),
                        reconfigure_release: Some(reconfigure_release_rx),
                        conversions,
                        connection_epoch: Arc::new(AtomicU64::new(1)),
                    },
                    results,
                    statuses,
                    RetryPolicy {
                        base: Duration::from_millis(1),
                        cap: Duration::from_millis(1),
                        health_probe_interval: Duration::from_millis(1),
                    },
                )
            })
        };

        offer_configuration(&sender, &desired, &desired_generation, 42, Request::Ping);
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured { .. }
        ));
        health_started_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        health_release_tx.send(()).unwrap();
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Invalidated { .. }
        ));
        reconfigure_started_rx
            .recv_timeout(Duration::from_millis(100))
            .unwrap();
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &notified,
            snapshot(39, 1, 1),
        ));
        reconfigure_release_tx.send(()).unwrap();
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured { .. }
        ));
        assert!(results.recv_timeout(Duration::from_millis(20)).is_err());
        assert!(observed.lock().unwrap().is_empty());

        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &notified,
            snapshot(42, 2, 2),
        ));
        assert_eq!(
            results
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .identity,
            snapshot(42, 2, 2).identity
        );
        assert_eq!(*observed.lock().unwrap(), vec![snapshot(42, 2, 2).identity]);

        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn healthy_idle_link_probes_at_a_bounded_cadence_and_stops_on_close() {
        let (sender, receiver) = sync_channel(1);
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let (desired, desired_generation) = desired_configuration_slot();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let worker = {
            let desired = desired.clone();
            let desired_generation = desired_generation.clone();
            let statuses = statuses.clone();
            std::thread::spawn(move || {
                run_snapshot_worker_with_retry(
                    receiver,
                    desired,
                    desired_generation,
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(ArrayQueue::new(1)),
                    Arc::new(AtomicBool::new(false)),
                    BlockingHealthProbeTransport {
                        started: started_tx,
                        release: release_rx,
                        recoveries: Arc::new(AtomicU64::new(0)),
                        invalidations: Arc::new(AtomicU64::new(0)),
                        connection_epoch: Arc::new(AtomicU64::new(1)),
                        health_result: true,
                    },
                    Arc::new(ArrayQueue::new(1)),
                    statuses,
                    RetryPolicy {
                        base: Duration::from_millis(1),
                        cap: Duration::from_millis(4),
                        health_probe_interval: Duration::from_millis(20),
                    },
                )
            })
        };

        offer_configuration(&sender, &desired, &desired_generation, 22, Request::Ping);
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured { .. }
        ));
        started_rx.recv_timeout(Duration::from_millis(100)).unwrap();
        assert!(started_rx.recv_timeout(Duration::from_millis(10)).is_err());
        let released_at = Instant::now();
        release_tx.send(()).unwrap();
        started_rx.recv_timeout(Duration::from_millis(100)).unwrap();
        assert!(released_at.elapsed() >= Duration::from_millis(20));
        sender.send(SnapshotCommand::Close).unwrap();
        release_tx.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn shutdown_during_an_idle_health_probe_stops_before_recovery() {
        let (sender, receiver) = sync_channel(1);
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let (desired, desired_generation) = desired_configuration_slot();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let recoveries = Arc::new(AtomicU64::new(0));
        let observed_recoveries = recoveries.clone();
        let invalidations = Arc::new(AtomicU64::new(0));
        let observed_invalidations = invalidations.clone();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let worker = {
            let desired = desired.clone();
            let desired_generation = desired_generation.clone();
            let statuses = statuses.clone();
            std::thread::spawn(move || {
                run_snapshot_worker_with_retry(
                    receiver,
                    desired,
                    desired_generation,
                    worker_shutdown,
                    Arc::new(ArrayQueue::new(1)),
                    Arc::new(AtomicBool::new(false)),
                    BlockingHealthProbeTransport {
                        started: started_tx,
                        release: release_rx,
                        recoveries,
                        invalidations,
                        connection_epoch: Arc::new(AtomicU64::new(1)),
                        health_result: false,
                    },
                    Arc::new(ArrayQueue::new(1)),
                    statuses,
                    RetryPolicy {
                        base: Duration::from_millis(1),
                        cap: Duration::from_millis(4),
                        health_probe_interval: Duration::from_millis(1),
                    },
                )
            })
        };

        offer_configuration(&sender, &desired, &desired_generation, 23, Request::Ping);
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured { .. }
        ));
        started_rx.recv_timeout(Duration::from_millis(100)).unwrap();
        shutdown.store(true, Ordering::Release);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(observed_recoveries.load(Ordering::Acquire), 0);
        assert_eq!(observed_invalidations.load(Ordering::Acquire), 1);
    }

    #[test]
    fn latest_configuration_survives_a_full_queue_when_publication_is_interrupted() {
        let (sender, receiver) = sync_channel(1);
        let result_sender = Arc::new(ArrayQueue::new(1));
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let status_sender = statuses.clone();
        let (desired, desired_generation) = desired_configuration_slot();
        let worker_desired = desired.clone();
        let worker_desired_generation = desired_generation.clone();
        let pending = Arc::new(ArrayQueue::new(1));
        let worker_pending = pending.clone();
        let work_notified = Arc::new(AtomicBool::new(false));
        let worker_notified = work_notified.clone();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let configured_requests = Arc::new(Mutex::new(Vec::new()));
        let observed_requests = configured_requests.clone();
        let worker = std::thread::spawn(move || {
            run_snapshot_worker(
                receiver,
                worker_desired,
                worker_desired_generation,
                Arc::new(AtomicBool::new(false)),
                worker_pending,
                worker_notified,
                BlockingSnapshotTransport {
                    started: started_tx,
                    release: release_rx,
                    configured_requests,
                    connection_epoch: Arc::new(AtomicU64::new(1)),
                },
                result_sender,
                status_sender,
            )
        });
        offer_configuration(&sender, &desired, &desired_generation, 1, Request::Ping);
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 1,
                ..
            }
        ));
        offer_test_snapshot(&sender, &pending, &work_notified, snapshot(1, 1, 1));
        started_rx.recv_timeout(Duration::from_millis(100)).unwrap();
        offer_test_snapshot(&sender, &pending, &work_notified, snapshot(1, 1, 2));

        let _ = desired.force_push(DesiredSnapshotConfiguration {
            generation: 2,
            request: Request::Shutdown,
        });
        assert!(matches!(
            sender.try_send(SnapshotCommand::DesiredConfigurationChanged),
            Err(TrySendError::Full(_))
        ));
        release_tx.send(()).unwrap();

        assert_eq!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured {
                configuration_generation: 2,
                connection_epoch: 1,
            }
        );
        assert_eq!(desired_generation.load(Ordering::Acquire), 2);
        assert_eq!(*observed_requests.lock().unwrap(), vec!["g1", "g2"]);
        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn live_burst_keeps_only_the_running_and_newest_revisions() {
        let (sender, receiver) = sync_channel(1);
        let pending = Arc::new(ArrayQueue::new(1));
        let worker_pending = pending.clone();
        let work_notified = Arc::new(AtomicBool::new(false));
        let worker_notified = work_notified.clone();
        let results = Arc::new(ArrayQueue::new(1));
        let worker_results = results.clone();
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let worker_statuses = statuses.clone();
        let (desired, desired_generation) = desired_configuration_slot();
        let worker_desired = desired.clone();
        let worker_desired_generation = desired_generation.clone();
        let conversions = Arc::new(Mutex::new(Vec::new()));
        let observed = conversions.clone();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let worker = std::thread::spawn(move || {
            run_snapshot_worker(
                receiver,
                worker_desired,
                worker_desired_generation,
                Arc::new(AtomicBool::new(false)),
                worker_pending,
                worker_notified,
                ShutdownSnapshotTransport {
                    started: started_tx,
                    release: release_rx,
                    configured_requests: Arc::new(Mutex::new(Vec::new())),
                    conversions,
                    invalidations: Arc::new(AtomicU64::new(0)),
                    connection_epoch: Arc::new(AtomicU64::new(1)),
                },
                worker_results,
                worker_statuses,
            )
        });
        offer_configuration(&sender, &desired, &desired_generation, 1, Request::Ping);
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)).unwrap(),
            SnapshotStatus::Configured { .. }
        ));
        assert!(offer_test_snapshot(
            &sender,
            &pending,
            &work_notified,
            snapshot(1, 1, 1),
        ));
        started_rx.recv_timeout(Duration::from_millis(100)).unwrap();
        for revision in 2..=1_000 {
            assert!(offer_test_snapshot(
                &sender,
                &pending,
                &work_notified,
                snapshot(1, 1, revision),
            ));
            assert!(pending.len() <= 1);
        }

        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_millis(200);
        let final_result = loop {
            if let Some(result) = results.pop() {
                if result.identity.revision == 1_000 {
                    break result;
                }
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        };
        assert_eq!(*observed.lock().unwrap(), vec![1, 1_000]);
        assert_eq!(final_result.identity.revision, 1_000);
        assert!(pending.is_empty());
        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn disconnected_snapshot_worker_rejects_and_clears_live_work() {
        let (sender, receiver) = sync_channel(1);
        drop(receiver);
        let pending = ArrayQueue::new(1);
        let work_notified = AtomicBool::new(false);
        let worker_alive = AtomicBool::new(true);

        assert!(!offer_live_snapshot(
            &sender,
            &pending,
            &work_notified,
            &worker_alive,
            snapshot(1, 1, 1),
        ));
        assert!(pending.is_empty());
        assert!(!worker_alive.load(Ordering::Acquire));
    }

    #[test]
    fn idle_snapshot_command_wait_blocks_until_work_is_notified() {
        let (sender, receiver) = sync_channel(1);
        let (completed_tx, completed_rx) = channel();
        let waiter = std::thread::spawn(move || {
            completed_tx
                .send(wait_for_snapshot_command(&receiver, None))
                .unwrap();
        });

        assert!(completed_rx
            .recv_timeout(Duration::from_millis(30))
            .is_err());
        sender.send(SnapshotCommand::WorkAvailable).unwrap();
        assert!(matches!(
            completed_rx.recv_timeout(Duration::from_millis(100)),
            Ok(Ok(Some(SnapshotCommand::WorkAvailable)))
        ));
        waiter.join().unwrap();
    }

    #[test]
    fn live_work_notifications_are_coalesced_to_one_command() {
        let (sender, receiver) = sync_channel(64);
        let pending = ArrayQueue::new(1);
        let work_notified = AtomicBool::new(false);
        let worker_alive = AtomicBool::new(true);

        for revision in 1..=1_000 {
            assert!(offer_live_snapshot(
                &sender,
                &pending,
                &work_notified,
                &worker_alive,
                snapshot(1, 1, revision),
            ));
        }

        assert!(matches!(
            receiver.try_recv(),
            Ok(SnapshotCommand::WorkAvailable)
        ));
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(pending.pop().unwrap().identity.revision, 1_000);
    }

    #[test]
    fn full_control_lane_leaves_the_next_live_revision_notifiable() {
        let (sender, receiver) = sync_channel(1);
        sender
            .send(SnapshotCommand::DesiredConfigurationChanged)
            .unwrap();
        let pending = ArrayQueue::new(1);
        let work_notified = AtomicBool::new(false);
        let worker_alive = AtomicBool::new(true);

        assert!(offer_live_snapshot(
            &sender,
            &pending,
            &work_notified,
            &worker_alive,
            snapshot(1, 1, 2),
        ));
        assert!(!work_notified.load(Ordering::Acquire));
        assert!(matches!(
            receiver.try_recv(),
            Ok(SnapshotCommand::DesiredConfigurationChanged)
        ));
        assert_eq!(pending.pop().unwrap().identity.revision, 2);

        assert!(offer_live_snapshot(
            &sender,
            &pending,
            &work_notified,
            &worker_alive,
            snapshot(1, 1, 3),
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(SnapshotCommand::WorkAvailable)
        ));
        assert_eq!(pending.pop().unwrap().identity.revision, 3);
    }

    #[test]
    fn full_control_lane_does_not_discard_a_successful_auto_commit_receipt() {
        let (control_sender, control_receiver) = sync_channel(1);
        control_sender
            .send(SnapshotCommand::DesiredConfigurationChanged)
            .unwrap();
        let (receipt_sender, receipt_receiver) = channel();
        let identity = SnapshotIdentity {
            composition: 8,
            revision: 13,
            configuration_generation: 2,
            connection_generation: 5,
        };

        assert!(enqueue_auto_commit_receipt(
            &receipt_sender,
            &control_sender,
            crate::input_module::AutoCommitReceipt {
                proposal: 17,
                identity,
            },
        ));
        assert!(matches!(
            receipt_receiver.try_recv(),
            Ok(Request::AutoCommitReceipt {
                composition: 8,
                revision: 13,
                proposal: 17,
                ..
            })
        ));
        assert!(matches!(
            control_receiver.try_recv(),
            Ok(SnapshotCommand::DesiredConfigurationChanged)
        ));
    }

    #[test]
    #[should_panic(expected = "background worker capacity must be positive")]
    fn background_worker_rejects_zero_capacity() {
        let _ = BackgroundInputWorker::start("unused".to_string(), 0);
    }

    #[test]
    fn live_notification_is_not_lost_at_the_idle_boundary() {
        let (sender, receiver) = sync_channel(4);
        let pending = Arc::new(ArrayQueue::new(1));
        let worker_pending = pending.clone();
        let work_notified = Arc::new(AtomicBool::new(false));
        let worker_notified = work_notified.clone();
        let results = Arc::new(ArrayQueue::new(1));
        let worker_results = results.clone();
        let statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let worker_statuses = statuses.clone();
        let (desired, desired_generation) = desired_configuration_slot();
        let worker_desired = desired.clone();
        let worker_desired_generation = desired_generation.clone();
        let worker = std::thread::spawn(move || {
            run_snapshot_worker(
                receiver,
                worker_desired,
                worker_desired_generation,
                Arc::new(AtomicBool::new(false)),
                worker_pending,
                worker_notified,
                SnapshotJournal {
                    configure_outcomes: Arc::new(Mutex::new(Default::default())),
                    convert_outcomes: Arc::new(Mutex::new(Default::default())),
                    conversions: Arc::new(Mutex::new(Vec::new())),
                    connection_epoch: Arc::new(AtomicU64::new(1)),
                },
                worker_results,
                worker_statuses,
            )
        });
        offer_configuration(&sender, &desired, &desired_generation, 1, Request::Ping);
        assert!(matches!(
            statuses.recv_timeout(Duration::from_millis(100)),
            Ok(SnapshotStatus::Configured { .. })
        ));

        for revision in 1..=100 {
            assert!(offer_test_snapshot(
                &sender,
                &pending,
                &work_notified,
                snapshot(1, 1, revision),
            ));
            assert_eq!(
                results
                    .recv_timeout(Duration::from_millis(100))
                    .unwrap()
                    .identity
                    .revision,
                revision
            );
        }
        sender.send(SnapshotCommand::Close).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn snapshot_worker_alive_guard_clears_work_on_exit_and_unwind() {
        let alive = Arc::new(AtomicBool::new(true));
        let pending = Arc::new(ArrayQueue::new(1));
        let notified = Arc::new(AtomicBool::new(true));
        pending.push(snapshot(1, 1, 1)).unwrap();
        {
            let _guard = SnapshotWorkerAlive {
                alive: alive.clone(),
                pending: pending.clone(),
                work_notified: notified.clone(),
            };
        }
        assert!(!alive.load(Ordering::Acquire));
        assert!(pending.is_empty());
        assert!(!notified.load(Ordering::Acquire));

        alive.store(true, Ordering::Release);
        notified.store(true, Ordering::Release);
        pending.push(snapshot(1, 1, 2)).unwrap();
        let guarded_alive = alive.clone();
        let guarded_pending = pending.clone();
        let guarded_notified = notified.clone();
        let _ = std::panic::catch_unwind(move || {
            let _guard = SnapshotWorkerAlive {
                alive: guarded_alive,
                pending: guarded_pending,
                work_notified: guarded_notified,
            };
            panic!("worker panic");
        });

        assert!(!alive.load(Ordering::Acquire));
        assert!(pending.is_empty());
        assert!(!notified.load(Ordering::Acquire));
    }

    #[test]
    fn dropping_owner_stops_a_blocked_snapshot_worker_without_draining_saturated_work() {
        let (mailbox, _stateful_receiver) = bounded_mailbox(1);
        let (snapshot_sender, snapshot_receiver) = sync_channel(1);
        let results = Arc::new(ArrayQueue::new(1));
        let result_sender = results.clone();
        let snapshot_statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let status_sender = snapshot_statuses.clone();
        let (desired, desired_generation) = desired_configuration_slot();
        let observed_desired = desired.clone();
        let observed_desired_generation = desired_generation.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let connection_epoch = Arc::new(AtomicU64::new(1));
        let configured_requests = Arc::new(Mutex::new(Vec::new()));
        let observed_configurations = configured_requests.clone();
        let conversions = Arc::new(Mutex::new(Vec::new()));
        let observed_conversions = conversions.clone();
        let invalidations = Arc::new(AtomicU64::new(0));
        let observed_invalidations = invalidations.clone();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let worker_desired = desired.clone();
        let worker_desired_generation = desired_generation.clone();
        let worker_connection_epoch = connection_epoch.clone();
        let pending = Arc::new(ArrayQueue::new(1));
        let worker_pending = pending.clone();
        let work_notified = Arc::new(AtomicBool::new(false));
        let worker_notified = work_notified.clone();
        let worker = std::thread::spawn(move || {
            run_snapshot_worker(
                snapshot_receiver,
                worker_desired,
                worker_desired_generation,
                worker_shutdown,
                worker_pending,
                worker_notified,
                ShutdownSnapshotTransport {
                    started: started_tx,
                    release: release_rx,
                    configured_requests,
                    conversions,
                    invalidations,
                    connection_epoch: worker_connection_epoch,
                },
                result_sender,
                status_sender,
            )
        });
        let owner = BackgroundInputWorker {
            mailbox,
            snapshot_sender,
            pending_snapshot: pending,
            snapshot_work_notified: work_notified,
            snapshot_worker_alive: Arc::new(AtomicBool::new(true)),
            desired_snapshot_configuration: desired,
            desired_snapshot_configuration_generation: desired_generation,
            snapshot_shutdown: shutdown,
            snapshot_connection_epoch: connection_epoch,
            results,
            enhancement_results: Arc::new(ArrayQueue::new(1)),
            pending_enhancement: Arc::new(ArrayQueue::new(1)),
            auto_commit_receipt_sender: channel().0,
            snapshot_statuses,
        };

        assert!(owner.try_configure_snapshot(1, Request::Ping));
        let deadline = Instant::now() + Duration::from_millis(100);
        let configured = loop {
            if let Some(status) = owner.try_snapshot_status() {
                break status;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        };
        assert!(matches!(
            configured,
            SnapshotStatus::Configured {
                configuration_generation: 1,
                ..
            }
        ));
        assert!(owner.try_live_snapshot(snapshot(1, 1, 1)));
        started_rx.recv_timeout(Duration::from_millis(100)).unwrap();
        assert!(owner.try_live_snapshot(snapshot(1, 1, 2)));
        assert!(owner.try_configure_snapshot(2, Request::Shutdown));

        drop(owner);
        assert!(observed_desired.is_empty());
        assert_eq!(observed_desired_generation.load(Ordering::Acquire), 0);
        release_tx.send(()).unwrap();
        worker.join().unwrap();

        assert_eq!(*observed_configurations.lock().unwrap(), vec!["g1"]);
        assert_eq!(*observed_conversions.lock().unwrap(), vec![1]);
        assert_eq!(observed_invalidations.load(Ordering::Acquire), 1);
    }

    #[test]
    fn dropping_the_owner_invalidates_queued_work_and_closes_the_worker() {
        let (mailbox, receiver) = bounded_mailbox(1);
        let shared = mailbox.shared.clone();
        let results = Arc::new(ArrayQueue::new(1));
        let (snapshot_sender, _snapshot_receiver) = sync_channel(1);
        let snapshot_connection_epoch = Arc::new(AtomicU64::new(1));
        let (desired_snapshot_configuration, desired_snapshot_configuration_generation) =
            desired_configuration_slot();
        let snapshot_statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let owner = BackgroundInputWorker {
            mailbox,
            snapshot_sender,
            pending_snapshot: Arc::new(ArrayQueue::new(1)),
            snapshot_work_notified: Arc::new(AtomicBool::new(false)),
            snapshot_worker_alive: Arc::new(AtomicBool::new(true)),
            desired_snapshot_configuration,
            desired_snapshot_configuration_generation,
            snapshot_shutdown: Arc::new(AtomicBool::new(false)),
            snapshot_connection_epoch,
            results,
            enhancement_results: Arc::new(ArrayQueue::new(1)),
            pending_enhancement: Arc::new(ArrayQueue::new(1)),
            auto_commit_receipt_sender: channel().0,
            snapshot_statuses,
        };
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let applied = Arc::new(AtomicU64::new(0));
        let closes = Arc::new(AtomicU64::new(0));
        let transport = GatedTransport {
            started: started_tx,
            release: release_rx,
            applied: applied.clone(),
            closes: closes.clone(),
        };
        let worker = std::thread::spawn(move || run_worker(receiver, shared, transport));
        assert!(owner.try_reseed(RequestId(1), vec![segment("a")]));
        started_rx.recv_timeout(Duration::from_millis(100)).unwrap();
        assert!(owner.try_insert(RequestId(2), vec![segment("b")]));
        drop(owner);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(applied.load(Ordering::Acquire), 1);
        assert!(closes.load(Ordering::Acquire) > 0);
    }

    #[test]
    fn stateful_pipeline_generations_do_not_change_the_snapshot_connection_epoch() {
        let (mailbox, _receiver) = bounded_mailbox(1);
        let (snapshot_sender, _snapshot_receiver) = sync_channel(1);
        let results = Arc::new(ArrayQueue::new(1));
        let snapshot_statuses = Arc::new(ArrayQueue::new(SNAPSHOT_STATUS_CAPACITY));
        let (desired_snapshot_configuration, desired_snapshot_configuration_generation) =
            desired_configuration_slot();
        let owner = BackgroundInputWorker {
            mailbox,
            snapshot_sender,
            pending_snapshot: Arc::new(ArrayQueue::new(1)),
            snapshot_work_notified: Arc::new(AtomicBool::new(false)),
            snapshot_worker_alive: Arc::new(AtomicBool::new(true)),
            desired_snapshot_configuration,
            desired_snapshot_configuration_generation,
            snapshot_shutdown: Arc::new(AtomicBool::new(false)),
            snapshot_connection_epoch: Arc::new(AtomicU64::new(9)),
            results,
            enhancement_results: Arc::new(ArrayQueue::new(1)),
            pending_enhancement: Arc::new(ArrayQueue::new(1)),
            auto_commit_receipt_sender: channel().0,
            snapshot_statuses,
        };

        owner.begin_composition();
        owner.request_close();

        assert_eq!(owner.connection_generation(), 9);
    }

    struct LifecycleJournal(Arc<Mutex<Vec<String>>>);

    impl WorkerTransport for LifecycleJournal {
        fn apply(&mut self, intent: &WorkerIntent) -> bool {
            let event = match intent {
                WorkerIntent::Insert {
                    segments,
                    reseed: true,
                    ..
                } => format!(
                    "full:{}",
                    segments
                        .iter()
                        .map(|segment| format!("{:?}:{}", segment.style, segment.text))
                        .collect::<Vec<_>>()
                        .join("|")
                ),
                WorkerIntent::LiveConvert { .. } => "live".to_string(),
                WorkerIntent::CommitAndClose => "commit".to_string(),
                _ => "delta".to_string(),
            };
            self.0.lock().unwrap().push(event);
            true
        }

        fn close(&mut self) {
            self.0.lock().unwrap().push("close".to_string());
        }
    }

    #[test]
    fn partial_transition_closes_old_generation_then_reseeds_before_live() {
        let (mailbox, receiver) = bounded_mailbox(4);
        assert!(mailbox.try_reseed(RequestId(1), vec![segment("old")]));
        mailbox.request_close();
        assert!(mailbox.try_reseed(
            RequestId(2),
            vec![
                segment("のこり"),
                InputSegment {
                    text: "A".to_string(),
                    style: TextStyle::Direct,
                },
            ],
        ));
        assert!(mailbox.try_delta(WorkerIntent::LiveConvert { seq: 3 }));
        let events = Arc::new(Mutex::new(Vec::new()));
        let shared = mailbox.shared.clone();
        drop(mailbox);
        run_worker(receiver, shared, LifecycleJournal(events.clone()));
        let events = events.lock().unwrap();
        assert!(!events.iter().any(|event| event.contains("old")));
        let full = events
            .iter()
            .position(|event| event == "full:Kana:のこり|Direct:A")
            .unwrap();
        let live = events.iter().position(|event| event == "live").unwrap();
        assert!(events[..full].iter().any(|event| event == "close"));
        assert!(full < live);
    }

    #[test]
    fn surface_edit_reseed_replaces_the_old_worker_state_with_styled_canonical_input() {
        let (mailbox, receiver) = bounded_mailbox(4);
        assert!(mailbox.try_reseed(RequestId(1), vec![segment("ny")]));
        assert!(mailbox.try_push(BackgroundIntent::Reseed {
            request: RequestId(2),
            segments: vec![InputSegment {
                text: "n".to_string(),
                style: TextStyle::Direct,
            }],
        }));
        let events = Arc::new(Mutex::new(Vec::new()));
        let shared = mailbox.shared.clone();
        drop(mailbox);
        run_worker(receiver, shared, LifecycleJournal(events.clone()));

        let events = events.lock().unwrap();
        assert!(!events.iter().any(|event| event.contains("ny")));
        let full = events
            .iter()
            .position(|event| event == "full:Direct:n")
            .unwrap();
        assert!(events[..full].iter().any(|event| event == "close"));
    }

    #[test]
    fn partial_reseed_queue_full_stays_dirty_for_the_local_remaining() {
        let (mailbox, receiver) = bounded_mailbox(1);
        assert!(mailbox.try_reseed(RequestId(1), vec![segment("old")]));
        mailbox.request_close();
        assert!(!mailbox.try_reseed(RequestId(2), vec![segment("remaining")]));
        assert!(mailbox.needs_reseed());
        let stale = receiver.recv().unwrap();
        assert_ne!(
            stale.generation,
            generation(mailbox.shared.pipeline.load(Ordering::Acquire))
        );
    }
}
