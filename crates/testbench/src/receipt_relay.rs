//! Test-only relay: the native engine processes a receipt, but its first ACK is lost.
use ipc::{clause::{CommitId, CommitReceipt, ReceiptStatus}, client::EngineClient, protocol::{Request, Response}};
use std::{fs::File, io::{self, Read, Write}, os::windows::{io::{AsRawHandle, FromRawHandle}, process::CommandExt},
    process::{Child, Command, Stdio}, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
    thread::{self, JoinHandle}, time::{Duration, Instant}};
use windows::{core::PCWSTR, Win32::{Foundation::HANDLE, Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX},
    System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PeekNamedPipe, PIPE_NOWAIT, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE}}};

type Observation = (CommitReceipt, Option<(CommitId, ReceiptStatus)>);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RelayFault { PassThrough, LoseFirstAck, ExpiredBaseline, RestartEngine, DeliveryExpiry, ObserveConfiguration }

#[derive(Clone)]
pub struct TokenTiming {
    pub before: Instant,
    pub after: Instant,
    pub tokens: Vec<String>,
}

pub struct ReceiptRelay {
    stop: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
    engine: Child,
    observations: Arc<Mutex<Vec<Observation>>>,
    retry_allowed: Arc<AtomicBool>,
    clause_requests: Arc<Mutex<Vec<Vec<u8>>>>,
    conversion_allowed: Arc<AtomicBool>,
    backend: String,
    token_timing: Arc<Mutex<Option<TokenTiming>>>,
    delivery_times: Arc<Mutex<Vec<(Instant, Instant)>>>,
    clear_before_receipt: Arc<AtomicBool>,
    preclear_receipt: Arc<Mutex<Option<CommitReceipt>>>,
}

struct Pipe {
    file: File,
    stop: Arc<AtomicBool>,
    deadline: Instant,
}

impl Pipe {
    fn create(name: &str, first: bool, stop: Arc<AtomicBool>) -> io::Result<Self> {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let flags = if first { PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE } else { PIPE_ACCESS_DUPLEX };
        let handle = unsafe { CreateNamedPipeW(PCWSTR(wide.as_ptr()), flags,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT, 255, 65536, 65536, 0, None) };
        if handle.is_invalid() { return Err(io::Error::last_os_error()); }
        Ok(Self { file: unsafe { File::from_raw_handle(handle.0) }, stop,
            deadline: Instant::now() + Duration::from_secs(30) })
    }

    fn connected(&self) -> io::Result<bool> {
        match unsafe { ConnectNamedPipe(HANDLE(self.file.as_raw_handle()), None) } {
            Ok(()) => Ok(false), // PIPE_NOWAIT success means listening, not connected.
            Err(error) if error.code().0 as u32 == 0x80070217 => Ok(true), // ERROR_PIPE_CONNECTED
            // TIP's presence probe may disconnect before accept. Retire this instance
            // just like a connected client; it must not stop the listener.
            Err(error) if error.code().0 as u32 == 0x800700e8 => Ok(true), // ERROR_NO_DATA
            Err(error) if error.code().0 as u32 == 0x80070218 => Ok(false), // ERROR_PIPE_LISTENING
            Err(error) => Err(io::Error::other(error)),
        }
    }

    fn wait(&self) -> io::Result<()> {
        if self.stop.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        thread::sleep(Duration::from_millis(2));
        Ok(())
    }

    fn request(&mut self) -> io::Result<Request> {
        self.deadline = Instant::now() + Duration::from_secs(30);
        let mut header = [0; 4];
        self.read_exact(&mut header)?;
        let len = u32::from_le_bytes(header) as usize;
        if len > ipc::framing::MAX_REQUEST_FRAME_LEN { return Err(io::ErrorKind::InvalidData.into()); }
        self.deadline = Instant::now() + Duration::from_secs(3);
        let mut body = vec![0; len];
        self.read_exact(&mut body)?;
        serde_json::from_slice(&body).map_err(io::Error::other)
    }
}

impl Read for Pipe {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() { return Ok(0); }
        loop {
            if self.stop.load(Ordering::Acquire) { return Err(io::ErrorKind::BrokenPipe.into()); }
            let mut available = 0;
            unsafe { PeekNamedPipe(HANDLE(self.file.as_raw_handle()), None, 0, None, Some(&mut available), None) }
                .map_err(io::Error::other)?;
            if available == 0 { self.wait()?; continue; }
            match self.file.read(buffer) {
                Err(error) if error.raw_os_error() == Some(232) => self.wait()?, // ERROR_NO_DATA
                result => return result,
            }
        }
    }
}

impl Write for Pipe {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        loop {
            if self.stop.load(Ordering::Acquire) { return Err(io::ErrorKind::BrokenPipe.into()); }
            match self.file.write(buffer) {
                Ok(0) if !buffer.is_empty() => self.wait()?,
                Err(error) if error.raw_os_error() == Some(232) => self.wait()?,
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

impl ReceiptRelay {
    fn spawn_engine(back: &str) -> io::Result<Child> {
        let executable = std::env::current_exe()?.with_file_name("NospacekeyEngineHost.exe");
        let log = std::fs::OpenOptions::new().create(true).append(true)
            .open(std::env::temp_dir().join("nospacekey-engine.log"))?;
        let stderr = log.try_clone()?;
        let mut engine = Command::new(executable).args([back, "--persist"])
            .env("NOSPACEKEY_ZENZAI", "off")
            .creation_flags(0x08000000).stdin(Stdio::null()).stdout(Stdio::from(log)).stderr(Stdio::from(stderr)).spawn()?;
        if let Err(error) = EngineClient::connect_verified_to(back, Duration::from_secs(10),
            Instant::now() + Duration::from_secs(12)) {
            let _ = engine.kill();
            let _ = engine.wait();
            return Err(io::Error::other(format!("{error:?}")));
        }
        Ok(engine)
    }

    pub fn start(fault: RelayFault) -> io::Result<Self> {
        let drop_first_ack = fault == RelayFault::LoseFirstAck;
        let front = ipc::client::stable_pipe_name();
        // Native learning coordination requires the real numeric .s<session> suffix.
        let back = front.replacen("nospacekey-engine.", &format!("nospacekey-receipt-gate-{}.", std::process::id()), 1);
        let stop = Arc::new(AtomicBool::new(false));
        // Refuse an occupied normal pipe; never attach the fault to somebody else's engine.
        let first = Pipe::create(&front, true, stop.clone())?;
        let engine = Self::spawn_engine(&back)?;
        let backend = back.clone();
        let observations = Arc::new(Mutex::new(Vec::new()));
        let captured = observations.clone();
        let stopping = stop.clone();
        let retry_allowed = Arc::new(AtomicBool::new(!drop_first_ack));
        let release_retry = retry_allowed.clone();
        let clause_requests = Arc::new(Mutex::new(Vec::new()));
        let captured_clauses = clause_requests.clone();
        let expired_once = Arc::new(AtomicBool::new(false));
        let conversion_allowed = Arc::new(AtomicBool::new(!matches!(fault, RelayFault::ExpiredBaseline | RelayFault::RestartEngine)));
        let release_conversion = conversion_allowed.clone();
        let token_timing = Arc::new(Mutex::new(None::<TokenTiming>));
        let issued = token_timing.clone();
        let delivery_times = Arc::new(Mutex::new(Vec::new()));
        let delivered = delivery_times.clone();
        let clear_before_receipt = Arc::new(AtomicBool::new(false));
        let clearing = clear_before_receipt.clone();
        let preclear_receipt = Arc::new(Mutex::new(None));
        let frozen_before_clear = preclear_receipt.clone();
        let listener = thread::spawn(move || {
            let mut pending = first;
            let mut clients = Vec::new();
            while !stopping.load(Ordering::Acquire) && clients.len() < 64 {
                match pending.connected() {
                    Ok(false) => { thread::sleep(Duration::from_millis(2)); continue; }
                    Err(error) => { eprintln!("receipt relay accept: {error}"); break; }
                    Ok(true) => {}
                }
                let next = match Pipe::create(&front, false, stopping.clone()) {
                    Ok(pipe) => pipe,
                    Err(error) => { eprintln!("receipt relay listen: {error}"); break; }
                };
                let mut pipe = std::mem::replace(&mut pending, next);
                let mut client_pid = 0;
                if unsafe { GetNamedPipeClientProcessId(HANDLE(pipe.file.as_raw_handle()), &mut client_pid) }.is_err()
                    || client_pid != std::process::id() { continue; }
                let backend = back.clone();
                let captured = captured.clone();
                let release_retry = release_retry.clone();
                let captured_clauses = captured_clauses.clone();
                let expired_once = expired_once.clone();
                let release_conversion = release_conversion.clone();
                let issued = issued.clone();
                let delivered = delivered.clone();
                let clearing = clearing.clone();
                let frozen_before_clear = frozen_before_clear.clone();
                clients.push(thread::spawn(move || {
                    let Ok(mut engine) = EngineClient::connect_to(&backend, Duration::from_secs(1)) else { return; };
                    while let Ok(request) = pipe.request() {
                        let received_at = Instant::now();
                        if matches!(fault, RelayFault::ExpiredBaseline | RelayFault::RestartEngine | RelayFault::ObserveConfiguration) {
                            if matches!(request, Request::LiveSnapshot { .. } | Request::ClauseCandidates(_) | Request::ConvertClauses(_)) {
                                let mut requests = captured_clauses.lock().unwrap();
                                if requests.len() >= 64 { break; }
                                requests.push(serde_json::to_vec(&request).expect("captured request serializes"));
                            }
                            if let Request::ClauseCandidates(candidate) = &request {
                                if fault == RelayFault::ExpiredBaseline && !expired_once.swap(true, Ordering::AcqRel) {
                                    let response = Response::ClauseCandidatesResult { key: candidate.key,
                                        status: ipc::clause::ClauseCandidatesStatus::Unavailable { reason: ipc::clause::ClauseUnavailableReason::Expired } };
                                    if ipc::framing::write_frame(&mut pipe, &response).is_err() { break; }
                                    continue;
                                }
                            }
                            if matches!(request, Request::ConvertClauses(_)) {
                                while !release_conversion.load(Ordering::Acquire) {
                                    if pipe.wait().is_err() { return; }
                                }
                            }
                        }
                        if matches!(request, Request::CommitReceipt(_)) && !captured.lock().unwrap().is_empty() {
                            while !release_retry.load(Ordering::Acquire) {
                                if pipe.wait().is_err() { return; }
                            }
                        }
                        if fault == RelayFault::DeliveryExpiry && matches!(request, Request::CommitReceipt(_)) {
                            let Some(timing) = issued.lock().unwrap().clone() else { break; };
                            while Instant::now() < timing.after + Duration::from_millis(60_100) {
                                if pipe.wait().is_err() { return; }
                            }
                        }
                        if let Request::CommitReceipt(receipt) = &request {
                            if clearing.swap(false, Ordering::AcqRel) {
                                *frozen_before_clear.lock().unwrap() = Some(receipt.clone());
                                if !matches!(engine.request_within(&Request::ClearLearning,
                                    Instant::now() + Duration::from_secs(2)), Ok(Response::Ok)) { break; }
                            }
                        }
                        let sent_at = Instant::now();
                        let Ok(response) = engine.request_within(&request, sent_at + Duration::from_secs(2)) else { break; };
                        if fault == RelayFault::DeliveryExpiry {
                            if let Response::ClauseCandidatesResult { status: ipc::clause::ClauseCandidatesStatus::Ready { candidates }, .. } = &response {
                                let mut timing = issued.lock().unwrap();
                                if timing.is_none() {
                                    *timing = Some(TokenTiming { before: sent_at, after: Instant::now(),
                                        tokens: candidates.iter().map(|candidate| candidate.token.clone()).collect() });
                                }
                            }
                            if matches!(request, Request::CommitReceipt(_)) {
                                let mut times = delivered.lock().unwrap();
                                if times.len() >= 4 { break; }
                                times.push((received_at, sent_at));
                            }
                        }
                        if let Request::CommitReceipt(receipt) = &request {
                            let mut observations = captured.lock().unwrap();
                            let drop_ack = drop_first_ack && observations.is_empty();
                            if observations.len() >= 4 { break; }
                            let ack = match &response {
                                Response::CommitReceiptAck { commit_id, status } => Some((commit_id.clone(), status.clone())),
                                _ => None,
                            };
                            observations.push((receipt.clone(), ack));
                            if drop_ack { break; }
                        }
                        pipe.deadline = Instant::now() + Duration::from_secs(2);
                        if ipc::framing::write_frame(&mut pipe, &response).is_err() { break; }
                    }
                }));
            }
            stopping.store(true, Ordering::Release);
            for client in clients { let _ = client.join(); }
        });
        Ok(Self { stop, listener: Some(listener), engine, observations, retry_allowed, clause_requests, conversion_allowed, backend, token_timing, delivery_times, clear_before_receipt, preclear_receipt })
    }

    pub fn observations(&self) -> Vec<Observation> {
        self.observations.lock().unwrap().clone()
    }
    pub fn token_timing(&self) -> Option<TokenTiming> { self.token_timing.lock().unwrap().clone() }
    pub fn delivery_times(&self) -> Vec<(Instant, Instant)> { self.delivery_times.lock().unwrap().clone() }
    pub fn arm_clear_before_receipt(&self) { self.clear_before_receipt.store(true, Ordering::Release); }
    pub fn preclear_receipt(&self) -> Option<CommitReceipt> { self.preclear_receipt.lock().unwrap().clone() }

    pub fn allow_retry_after_next_composition(&self) {
        self.retry_allowed.store(true, Ordering::Release);
    }
    pub fn clause_requests(&self) -> Vec<Request> {
        self.clause_requests.lock().unwrap().iter().map(|bytes| serde_json::from_slice(bytes).expect("captured request decodes")).collect()
    }
    pub fn allow_conversion(&self) { self.conversion_allowed.store(true, Ordering::Release); }
    pub fn clear_learning(&self) -> io::Result<(ipc::client::EngineLearningIdentity, ipc::client::EngineLearningIdentity)> {
        let connect = || EngineClient::connect_verified_to(&self.backend, Duration::from_secs(2),
            Instant::now() + Duration::from_secs(3)).map_err(io::Error::other);
        let mut client = connect()?;
        let before = client.learning_identity().clone();
        let response = client.request_within(&Request::ClearLearning, Instant::now() + Duration::from_secs(3))
            .map_err(io::Error::other)?;
        if !matches!(response, Response::Ok) { return Err(io::Error::other(format!("clear learning: {response:?}"))); }
        let after = connect()?.learning_identity().clone();
        Ok((before, after))
    }
    pub fn restart_engine(&mut self) -> io::Result<(u32, u32, bool)> {
        let epoch = |pipe: &str| EngineClient::connect_verified_to(pipe, Duration::from_secs(2), Instant::now() + Duration::from_secs(3))
            .map(|client| client.learning_identity().engine_epoch.clone()).map_err(io::Error::other);
        let old_epoch = epoch(&self.backend)?;
        let old_pid = self.engine.id();
        self.engine.kill()?;
        self.engine.wait()?;
        self.engine = Self::spawn_engine(&self.backend)?;
        let new_epoch = epoch(&self.backend)?;
        Ok((old_pid, self.engine.id(), old_epoch != new_epoch))
    }
}

impl Drop for ReceiptRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(listener) = self.listener.take() { let _ = listener.join(); }
        let _ = self.engine.kill();
        let _ = self.engine.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonblocking_pipe_forwards_a_complete_protocol_exchange() {
        let name = format!(r"\\.\pipe\nospacekey-receipt-exchange-test-{}", std::process::id());
        let stop = Arc::new(AtomicBool::new(false));
        let mut pipe = Pipe::create(&name, true, stop.clone()).unwrap();
        let server = thread::spawn(move || {
            while !pipe.connected().unwrap() { pipe.wait().unwrap(); }
            assert_eq!(pipe.request().unwrap(), Request::Ping);
            ipc::framing::write_frame(&mut pipe, &Response::Pong).unwrap();
            while !pipe.stop.load(Ordering::Acquire) { thread::sleep(Duration::from_millis(2)); }
        });
        let mut client = EngineClient::connect_to(&name, Duration::from_secs(1)).unwrap();
        let response = client.request_within(&Request::Ping, Instant::now() + Duration::from_secs(1));
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        assert!(matches!(response.unwrap(), Response::Pong));
    }

    #[test]
    fn disconnected_presence_probe_can_be_retired_without_stopping_accept() {
        let name = format!(r"\\.\pipe\nospacekey-receipt-probe-test-{}", std::process::id());
        let stop = Arc::new(AtomicBool::new(false));
        let probe = Pipe::create(&name, true, stop.clone()).unwrap();
        assert!(!probe.connected().unwrap(), "opening a listener is not accepting a client");
        assert!(!probe.connected().unwrap(), "an idle listener must remain available");
        let client = EngineClient::connect_to(&name, Duration::from_secs(1)).unwrap();
        drop(client);
        assert!(probe.connected().unwrap());
        let next = Pipe::create(&name, false, stop).unwrap();
        drop(probe);
        let _client = EngineClient::connect_to(&name, Duration::from_secs(1)).unwrap();
        assert!(next.connected().unwrap());
    }
}
