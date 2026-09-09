//! Bounded calculation lane. All presentation acceptance remains on the STA.
use ipc::clause::{
    ClauseCandidatesStatus, ClauseRequestKey, ClauseUnavailableReason, ConvertClausesStatus,
};
use ipc::client::{EngineClient, EngineLearningIdentity, VerifiedEngineClient};
use ipc::protocol::{Request, Response};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{sync_channel, Receiver, SyncSender},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

pub(crate) struct ClauseReply {
    pub key: ClauseRequestKey,
    pub response: Response,
    pub learning_identity: Option<EngineLearningIdentity>,
    pub rebaseline: Option<crate::clause_conversion::ClauseRebaseline>,
}
struct Work {
    request: Request,
    key: ClauseRequestKey,
    deadline: Instant,
    rebaseline: Option<crate::clause_conversion::ClauseRebaseline>,
}
pub(crate) struct ClauseWorker {
    sender: SyncSender<Work>,
    replies: Receiver<ClauseReply>,
    shutdown: Arc<AtomicBool>,
    refresh_requested: Arc<AtomicBool>,
    refresh_pending: Arc<AtomicBool>,
    refresh_reply: Arc<Mutex<Option<Option<EngineLearningIdentity>>>>,
}

fn failure(request: &Request, key: ClauseRequestKey, reason: ClauseUnavailableReason) -> Response {
    match request {
        Request::ConvertClauses(_) => Response::ConvertClausesResult {
            key,
            status: ConvertClausesStatus::Unavailable { reason },
        },
        _ => Response::ClauseCandidatesResult {
            key,
            status: ClauseCandidatesStatus::Unavailable { reason },
        },
    }
}

impl ClauseWorker {
    pub fn start(pipe: String) -> Self {
        let (sender, receiver) = sync_channel::<Work>(16);
        let (reply_sender, replies) = sync_channel(16);
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = shutdown.clone();
        let refresh_requested = Arc::new(AtomicBool::new(false));
        let refresh_pending = Arc::new(AtomicBool::new(false));
        let refresh_reply = Arc::new(Mutex::new(None));
        let worker_refresh = refresh_requested.clone();
        let worker_refresh_reply = refresh_reply.clone();
        let guard = crate::globals::ComObjectGuard::new();
        if std::thread::Builder::new().name("nospacekey-clauses".into()).spawn(move || {
            let _guard = guard;
            while !stop.load(Ordering::Acquire) {
                if worker_refresh.swap(false, Ordering::AcqRel) {
                    let deadline = Instant::now() + Duration::from_millis(1200);
                    let identity = EngineClient::connect_verified_to(&pipe, Duration::from_millis(500), deadline)
                        .ok().map(|client| client.learning_identity().clone());
                    *worker_refresh_reply.lock().unwrap() = Some(identity);
                }
                let work = match receiver.recv_timeout(Duration::from_millis(10)) {
                    Ok(work) => work,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(_) => break,
                };
                // Each calculation obtains current session metadata. Keeping a
                // connection across calculations would retain pre-clear metadata.
                let mut client: Option<VerifiedEngineClient> = None;
                let mut learning_identity = None;
                let mut response = failure(&work.request, work.key, ClauseUnavailableReason::Expired);
                while !stop.load(Ordering::Acquire) && Instant::now() < work.deadline {
                    if client.is_none() {
                        client = EngineClient::connect_verified_to(&pipe, work.deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(500)), work.deadline).ok();
                    }
                    let Some(connection) = client.as_mut() else {
                        response = failure(&work.request, work.key, ClauseUnavailableReason::Disconnected);
                        break;
                    };
                    learning_identity = Some(connection.learning_identity().clone());
                    match connection.request_within(&work.request, work.deadline) {
                        Ok(reply) => {
                            let pending = matches!(&reply,
                                Response::ClauseCandidatesResult { key, status: ClauseCandidatesStatus::Pending }
                                | Response::ConvertClausesResult { key, status: ConvertClausesStatus::Pending }
                                if *key == work.key);
                            if pending { std::thread::sleep(Duration::from_millis(10)); continue; }
                            response = reply;
                            break;
                        }
                        Err(_) => {
                            response = failure(&work.request, work.key, ClauseUnavailableReason::Disconnected);
                            break;
                        }
                    }
                }
                // A saturated UI lane resolves through the same fixed STA deadline.
                let _ = reply_sender.try_send(ClauseReply { key: work.key, response, learning_identity, rebaseline: work.rebaseline });
            }
        }).is_err() {
            shutdown.store(true, Ordering::Release);
            crate::text_service::tip_log("ev=clause_worker_spawn_failed");
        }
        Self {
            sender,
            replies,
            shutdown,
            refresh_requested,
            refresh_pending,
            refresh_reply,
        }
    }

    pub fn submit(&self, request: Request, deadline: Instant) -> bool {
        let key = match &request {
            Request::ClauseCandidates(r) => r.key,
            Request::ConvertClauses(r) => r.key,
            _ => return false,
        };
        self.sender
            .try_send(Work {
                request,
                key,
                deadline,
                rebaseline: None,
            })
            .is_ok()
    }
    pub fn try_reply(&self) -> Option<ClauseReply> {
        self.replies.try_recv().ok()
    }
    pub fn submit_rebaseline(&self, attempt: crate::clause_conversion::ClauseRebaseline, left_context: Option<String>) -> bool {
        self.sender.try_send(Work { request: attempt.snapshot(left_context), key: attempt.original_key(),
            deadline: attempt.deadline(), rebaseline: Some(attempt) }).is_ok()
    }
    pub fn refresh_learning(&self) {
        if self.shutdown.load(Ordering::Acquire) { return; }
        if !self.refresh_pending.swap(true, Ordering::AcqRel) {
            self.refresh_requested.store(true, Ordering::Release);
        }
    }
    pub fn learning_pending(&self) -> bool {
        self.refresh_pending.load(Ordering::Acquire)
    }
    pub fn take_learning_refresh(&self) -> Option<Option<EngineLearningIdentity>> {
        let reply = self.refresh_reply.lock().unwrap().take();
        if reply.is_some() { self.refresh_pending.store(false, Ordering::Release); }
        reply
    }
}
impl Drop for ClauseWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_refresh_is_coalesced_and_remains_pending_until_consumed() {
        let worker = ClauseWorker::start(format!(r"\\.\pipe\nospacekey-absent-refresh-test-{}", std::process::id()));
        worker.refresh_learning();
        worker.refresh_learning();
        assert!(worker.learning_pending());
        let deadline = Instant::now() + Duration::from_secs(3);
        while worker.refresh_reply.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(worker.learning_pending());
        assert_eq!(worker.take_learning_refresh(), Some(None));
        assert!(!worker.learning_pending());
        assert_eq!(worker.take_learning_refresh(), None);
    }
}
