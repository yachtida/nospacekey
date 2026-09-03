//! インライン予測バックエンドの境界。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use crate::prediction_state::{PredictionRequest, Timestamp};
use ipc::client::{verify_start_session, EngineClient, EngineIdentityError};
use ipc::protocol::{Request, Response};

pub(crate) trait Predictor {
    fn predict(&self, context_before: &str) -> Option<String>;
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PredictionOutcome {
    pub(crate) seq: u64,
    pub(crate) text: Option<String>,
    pub(crate) finished_at: Timestamp,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum IpcPredictionResult {
    Prediction(String),
    Unavailable(String),
    Failed,
    VersionMismatch {
        actual_proto: Option<u32>,
        actual_boot: Option<String>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct IpcPredictionOutcome {
    pub(crate) seq: u64,
    pub(crate) result: IpcPredictionResult,
    pub(crate) duration_ms: u128,
}

pub(crate) struct PredictionSlot {
    outcome: Mutex<Option<IpcPredictionOutcome>>,
    cancelled: AtomicBool,
}

impl PredictionSlot {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            outcome: Mutex::new(None),
            cancelled: AtomicBool::new(false),
        })
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub(crate) fn take(&self) -> Option<IpcPredictionOutcome> {
        self.outcome
            .lock()
            .ok()
            .and_then(|mut outcome| outcome.take())
    }
    fn complete(&self, outcome: IpcPredictionOutcome) {
        if self.is_cancelled() {
            return;
        }
        if let Ok(mut guard) = self.outcome.lock() {
            *guard = Some(outcome);
        }
    }
}

/// Acquire the DLL-lifetime owner before spawning and move it into the worker.
/// Dropping the returned JoinHandle may detach the thread, but cannot make the
/// in-process TIP unloadable while the worker is still executing its code.
fn spawn_owned_prediction_thread<Owner, Work>(
    name: &str,
    owner: Owner,
    work: Work,
) -> std::io::Result<JoinHandle<()>>
where
    Owner: Send + 'static,
    Work: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _owner = owner;
            work();
        })
}

struct TokenizerCache {
    path: PathBuf,
    tokenizer: Arc<tokenizers::Tokenizer>,
}

fn model_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NOSPACEKEY_PREDICTION_MODEL_DIR") {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("LOCALAPPDATA").map(|local| {
        PathBuf::from(local)
            .join("Nospacekey")
            .join("models")
            .join("inline-prediction")
    })
}

const MODEL_FILENAME: &str = "llm-jp-3-150m-q8_0-c060ca9.gguf";
const MODEL_LEN: u64 = 164_257_184;
const TOKENIZER_LEN: u64 = 6_416_433;
const VERIFIED_FILENAME: &str = "VERIFIED";
const VERIFIED_CONTENT: &str = concat!(
    "schema=1\n",
    "model_sha256=191f2fdf41a6f64f00ec6b4fcc39ec6164bb13b41d3609ac8f5b2b6149a23a6d\n",
    "tokenizer_sha256=955dc1fa623fab38cc92a3f4ee172423ae6d73201c4207569bfdf5626bc733f0\n",
);

#[derive(Clone, PartialEq, Eq)]
struct ArtifactFingerprint {
    model_path: PathBuf,
    model_len: u64,
    tokenizer_path: PathBuf,
    tokenizer_len: u64,
    marker_modified: Option<std::time::SystemTime>,
}

fn verify_artifacts(dir: &Path) -> Result<PathBuf, &'static str> {
    static CACHE: OnceLock<Mutex<Option<(ArtifactFingerprint, Result<(), &'static str>)>>> =
        OnceLock::new();
    let model_path = dir.join(MODEL_FILENAME);
    let tokenizer_path = dir.join("tokenizer.json");
    let marker_path = dir.join(VERIFIED_FILENAME);
    let model_meta = std::fs::metadata(&model_path).map_err(|_| "missing_model")?;
    let tokenizer_meta = std::fs::metadata(&tokenizer_path).map_err(|_| "missing_tokenizer")?;
    let marker_meta = std::fs::metadata(&marker_path).map_err(|_| "unverified_model")?;
    let fingerprint = ArtifactFingerprint {
        model_path: model_path.clone(),
        model_len: model_meta.len(),
        tokenizer_path: tokenizer_path.clone(),
        tokenizer_len: tokenizer_meta.len(),
        marker_modified: marker_meta.modified().ok(),
    };
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().map_err(|_| "artifact_cache_failed")?;
    if let Some((cached, result)) = guard.as_ref() {
        if cached == &fingerprint {
            return result.clone().map(|_| tokenizer_path);
        }
    }
    let result = if model_meta.len() != MODEL_LEN {
        Err("invalid_model")
    } else if tokenizer_meta.len() != TOKENIZER_LEN {
        Err("invalid_tokenizer")
    } else if !std::fs::read_to_string(marker_path).is_ok_and(|receipt| receipt == VERIFIED_CONTENT)
    {
        Err("unverified_model")
    } else {
        Ok(())
    };
    *guard = Some((fingerprint, result));
    result.map(|_| tokenizer_path)
}

fn set_current_thread_below_normal() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// Artifact verification is recorded transactionally by the downloader. At Activate each host
/// only checks the fixed receipt and warms its tokenizer, avoiding a 170 MB hash storm when several
/// applications load the TIP together.
pub(crate) fn warm_prediction_artifacts() -> std::io::Result<()> {
    let guard = crate::globals::ComObjectGuard::new();
    spawn_owned_prediction_thread("nospacekey-prediction-artifacts", guard, move || {
        set_current_thread_below_normal();
        if let Some(dir) = model_dir() {
            if let Ok(tokenizer) = verify_artifacts(&dir) {
                let _ = tokenize_with_path(&tokenizer, "予測準備");
            }
        }
    })
    .map(drop)
}

fn tokenize_with_path(path: &Path, context_before: &str) -> Result<Vec<u32>, String> {
    static CACHE: OnceLock<Mutex<Option<TokenizerCache>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache
        .lock()
        .map_err(|_| "tokenizer cache poisoned".to_owned())?;
    if guard.as_ref().is_none_or(|cached| cached.path != path) {
        let tokenizer =
            tokenizers::Tokenizer::from_file(path).map_err(|_| "tokenizer unavailable")?;
        *guard = Some(TokenizerCache {
            path: path.to_owned(),
            tokenizer: Arc::new(tokenizer),
        });
    }
    let tokenizer = Arc::clone(&guard.as_ref().expect("installed above").tokenizer);
    drop(guard);
    let encoding = tokenizer
        .encode(context_before, true)
        .map_err(|_| "tokenization failed")?;
    let ids = encoding.get_ids();
    if ids.is_empty() {
        return Err("tokenization produced an empty prompt".into());
    }
    const MAX_PROMPT_TOKENS: usize = 480;
    if ids.len() <= MAX_PROMPT_TOKENS {
        return Ok(ids.to_vec());
    }
    let mut truncated = Vec::with_capacity(MAX_PROMPT_TOKENS);
    truncated.push(ids[0]);
    truncated.extend_from_slice(&ids[(ids.len() - (MAX_PROMPT_TOKENS - 1))..]);
    Ok(truncated)
}

fn interpret_response(seq: u64, response: Response, duration_ms: u128) -> IpcPredictionOutcome {
    let result = match response {
        Response::Prediction { seq: echoed, text } if echoed == seq => {
            IpcPredictionResult::Prediction(text)
        }
        Response::PredictionUnavailable { seq: echoed, state } if echoed == seq => {
            IpcPredictionResult::Unavailable(state)
        }
        _ => IpcPredictionResult::Failed,
    };
    IpcPredictionOutcome {
        seq,
        result,
        duration_ms,
    }
}

fn prediction_session(
    mut send: impl FnMut(&Request) -> std::io::Result<Response>,
) -> Result<i64, EngineIdentityError> {
    verify_start_session(|request| send(request).map_err(EngineIdentityError::Io))
}

/// 通常変換と別の名前付きパイプ接続を開き、1要求だけを低優先度スレッドで実行する。
/// connection drop は engine-host の onDisconnect を通じて予測用 session を必ず掃除する。
pub(crate) fn spawn_ipc_prediction_worker(
    pipe_name: String,
    request: PredictionRequest,
    slot: Arc<PredictionSlot>,
    timeout: std::time::Duration,
) -> std::io::Result<()> {
    let guard = crate::globals::ComObjectGuard::new();
    spawn_owned_prediction_thread("nospacekey-prediction", guard, move || {
        set_current_thread_below_normal();
        let started = std::time::Instant::now();
        let seq = request.seq;
        let cancelled = || slot.is_cancelled();
        if cancelled() {
            return;
        }
        let tokenizer = match model_dir()
            .ok_or("missing_model")
            .and_then(|dir| verify_artifacts(&dir))
        {
            Ok(path) => path,
            Err(state) => {
                slot.complete(IpcPredictionOutcome {
                    seq,
                    result: IpcPredictionResult::Unavailable(state.into()),
                    duration_ms: started.elapsed().as_millis(),
                });
                return;
            }
        };
        if cancelled() {
            return;
        }
        let token_ids = match tokenize_with_path(&tokenizer, &request.context_before) {
            Ok(ids) if !cancelled() => ids,
            Ok(_) => return,
            Err(_) => {
                slot.complete(IpcPredictionOutcome {
                    seq,
                    result: IpcPredictionResult::Unavailable("invalid_tokenizer".into()),
                    duration_ms: started.elapsed().as_millis(),
                });
                return;
            }
        };
        drop(request.context_before);
        if cancelled() {
            return;
        }
        // Integrity checking and tokenizer initialization are readiness work. The transport
        // receives a fresh hard budget; PredictionState still rejects any result that missed
        // the user-visible 400 ms deadline measured from dispatch.
        let deadline = std::time::Instant::now() + timeout;
        let outcome = (|| -> Result<IpcPredictionOutcome, IpcPredictionResult> {
            let connect_budget = timeout.min(std::time::Duration::from_millis(75));
            let mut client = EngineClient::connect_to(&pipe_name, connect_budget)
                .map_err(|_| IpcPredictionResult::Failed)?;
            let session = prediction_session(|request| client.request_within(request, deadline))
                .map_err(|error| match error {
                    EngineIdentityError::Mismatch {
                        actual_proto,
                        actual_boot,
                    } => IpcPredictionResult::VersionMismatch {
                        actual_proto,
                        actual_boot,
                    },
                    _ => IpcPredictionResult::Failed,
                })?;
            let response = client
                .request_within(
                    &Request::Predict {
                        session,
                        seq,
                        token_ids,
                    },
                    deadline,
                )
                .map_err(|_| IpcPredictionResult::Failed)?;
            Ok(interpret_response(
                seq,
                response,
                started.elapsed().as_millis(),
            ))
        })()
        .unwrap_or_else(|result| IpcPredictionOutcome {
            seq,
            result,
            duration_ms: started.elapsed().as_millis(),
        });
        slot.complete(outcome);
    })
    .map(drop)
}

pub(crate) fn run_prediction(
    predictor: &impl Predictor,
    request: &PredictionRequest,
    finished_at: Timestamp,
) -> PredictionOutcome {
    let text = predictor.predict(&request.context_before);
    PredictionOutcome {
        seq: request.seq,
        text,
        finished_at,
    }
}

#[cfg(test)]
struct DeterministicPredictor {
    text: String,
}

#[cfg(test)]
impl DeterministicPredictor {
    fn new(text: &str) -> Self {
        Self { text: text.into() }
    }
}

#[cfg(test)]
impl Predictor for DeterministicPredictor {
    fn predict(&self, _context_before: &str) -> Option<String> {
        Some(self.text.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prediction_state::{
        CommitSource, PredictionAnchor, PredictionRequest, PredictionState, Timestamp,
    };

    #[test]
    fn detached_worker_owns_its_dll_lifetime_guard_until_completion() {
        struct DropNotice(Arc<AtomicBool>);
        impl Drop for DropNotice {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = spawn_owned_prediction_thread(
            "prediction-lifetime-test",
            DropNotice(Arc::clone(&dropped)),
            move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            },
        )
        .unwrap();
        drop(worker);

        started_rx.recv().unwrap();
        assert!(!dropped.load(Ordering::Acquire));
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !dropped.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn deterministic_predictor_returns_a_seq_correlated_outcome() {
        let predictor = DeterministicPredictor::new("確認しておきます。");
        let request = PredictionRequest {
            seq: 42,
            context_before: "明日の予定を事前に".into(),
            anchor: PredictionAnchor::new(5),
            deadline_at: Timestamp::from_millis(500),
        };

        let outcome = run_prediction(&predictor, &request, Timestamp::from_millis(200));

        assert_eq!(outcome.seq, 42);
        assert_eq!(outcome.text.as_deref(), Some("確認しておきます。"));
        assert_eq!(outcome.finished_at, Timestamp::from_millis(200));
    }

    #[test]
    fn state_machine_and_test_predictor_complete_the_model_free_flow() {
        let mut state = PredictionState::default();
        state.on_commit(
            CommitSource::Enter,
            "明日の予定を事前に確認して",
            PredictionAnchor::new(8),
            Timestamp::from_millis(0),
        );
        let request = state.poll(Timestamp::from_millis(300)).unwrap();
        let outcome = run_prediction(
            &DeterministicPredictor::new("おきます。"),
            &request,
            Timestamp::from_millis(340),
        );

        let ghost = state
            .on_result(
                outcome.seq,
                outcome.text.as_deref().unwrap(),
                outcome.finished_at,
            )
            .unwrap();

        assert_eq!(ghost.text, "おきます。");
        assert_eq!(ghost.anchor, PredictionAnchor::new(8));
    }

    #[test]
    fn ipc_response_requires_matching_sequence() {
        assert_eq!(
            interpret_response(
                4,
                Response::Prediction {
                    seq: 3,
                    text: "古い".into()
                },
                1
            )
            .result,
            IpcPredictionResult::Failed
        );
        assert_eq!(
            interpret_response(
                4,
                Response::Prediction {
                    seq: 4,
                    text: "続き".into()
                },
                1
            )
            .result,
            IpcPredictionResult::Prediction("続き".into())
        );
    }

    #[test]
    fn explicit_unavailable_state_is_not_a_prediction() {
        assert_eq!(
            interpret_response(
                4,
                Response::PredictionUnavailable {
                    seq: 4,
                    state: "loading".into()
                },
                1,
            )
            .result,
            IpcPredictionResult::Unavailable("loading".into())
        );
    }

    #[test]
    fn prediction_identity_mismatch_sends_no_predict_request() {
        let mut requests = Vec::new();
        let result = prediction_session(|request| {
            requests.push(matches!(request, Request::StartSession));
            Ok(Response::Session {
                session: 7,
                proto: Some(ipc::protocol::PROTO_VERSION),
                boot: Some("loaded-old-build".into()),
            })
        });
        assert!(matches!(result, Err(EngineIdentityError::Mismatch { .. })));
        assert_eq!(requests, vec![true]);
    }

    #[test]
    #[ignore = "requires the pinned local LLM-jp tokenizer artifact"]
    fn product_tokenizer_matches_the_evaluated_hugging_face_ids() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../experiments/inline-prediction/.models/llm-jp-3-150m/tokenizer.json");
        assert_eq!(
            tokenize_with_path(&path, "明日の予定は").unwrap(),
            [1, 50_014, 28_998, 65_484, 29_282]
        );
        assert_eq!(
            tokenize_with_path(&path, "今日は朝から雨が降っているので、出かける前に").unwrap(),
            [1, 46_275, 30_751, 55_574, 31_120, 29_314, 30_857, 78_564, 78_466, 66_700, 99_248]
        );
    }

    #[test]
    #[ignore = "requires the complete pinned product model artifact pair"]
    fn product_artifact_pair_matches_the_pinned_release_contract() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../experiments/inline-prediction/.models/product-test");
        assert_eq!(verify_artifacts(&dir).unwrap(), dir.join("tokenizer.json"));
    }
}
