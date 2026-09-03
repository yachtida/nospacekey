use std::fs::File;
use std::io;
use std::time::{Duration, Instant};

use crate::framing::MAX_RESPONSE_FRAME_LEN;
#[cfg(not(windows))]
use crate::framing::{read_frame, write_request_frame};
use crate::protocol::{Request, Response};

#[derive(Debug)]
pub enum EngineIdentityError {
    Io(io::Error),
    Mismatch {
        actual_proto: Option<u32>,
        actual_boot: Option<String>,
    },
    UnexpectedResponse(Response),
}

impl std::fmt::Display for EngineIdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "engine connection failed: {error}"),
            Self::Mismatch {
                actual_proto,
                actual_boot,
            } => write!(
                f,
                "engine identity mismatch: expected proto={} boot={}, actual proto={actual_proto:?} boot={actual_boot:?}",
                crate::protocol::PROTO_VERSION,
                env!("CARGO_PKG_VERSION")
            ),
            Self::UnexpectedResponse(response) => {
                write!(f, "unexpected StartSession response: {response:?}")
            }
        }
    }
}

impl std::error::Error for EngineIdentityError {}

impl From<io::Error> for EngineIdentityError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn verify_session_identity(response: Response) -> Result<i64, EngineIdentityError> {
    match response {
        Response::Session {
            session,
            proto,
            boot,
        } if proto == Some(crate::protocol::PROTO_VERSION)
            && boot.as_deref() == Some(env!("CARGO_PKG_VERSION")) =>
        {
            Ok(session)
        }
        Response::Session { proto, boot, .. } => Err(EngineIdentityError::Mismatch {
            actual_proto: proto,
            actual_boot: boot,
        }),
        response => Err(EngineIdentityError::UnexpectedResponse(response)),
    }
}

pub fn verify_start_session(
    mut send: impl FnMut(&Request) -> Result<Response, EngineIdentityError>,
) -> Result<i64, EngineIdentityError> {
    verify_session_identity(send(&Request::StartSession)?)
}

pub struct VerifiedEngineClient {
    client: EngineClient,
    session: i64,
}

impl VerifiedEngineClient {
    pub fn session(&self) -> i64 {
        self.session
    }

    pub fn request_within(&mut self, request: &Request, deadline: Instant) -> io::Result<Response> {
        self.client.request_within(request, deadline)
    }

    #[cfg(windows)]
    pub fn request_within_keep(
        &mut self,
        request: &Request,
        deadline: Instant,
    ) -> io::Result<Response> {
        self.client.request_within_keep(request, deadline)
    }

    #[cfg(windows)]
    pub fn drain_pending(&mut self, deadline: Instant) -> io::Result<Option<Response>> {
        self.client.drain_pending(deadline)
    }
}

/// フレーム到達をポーリングするための最小抽象（実パイプ Win32 からロジックを分離しテスト可能にする）。
#[allow(dead_code)]
pub(crate) trait FramePeek {
    /// `(バッファ内総バイト数, 先頭4byteが揃っていれば本体長 Some(len))` を非破壊に返す。
    fn peek(&self) -> io::Result<(u32, Option<u32>)>;
}

#[allow(dead_code)]
const POLL_MIN: Duration = Duration::from_millis(1);
#[allow(dead_code)]
const POLL_MAX: Duration = Duration::from_millis(3);

/// connect_to のポーリング間隔ランプ（初回 10ms→倍々→上限 80ms、残り時間でクランプ）。
const CONNECT_POLL_MIN: Duration = Duration::from_millis(10);
const CONNECT_POLL_MAX: Duration = Duration::from_millis(80);

/// `deadline` までに「4byte長 + 本体」がバッファに揃うのを待つ。
/// 揃えば Ok(())、期限超過は TimedOut、本体長が上限超は InvalidData、peek 失敗はそのエラー。
/// フレームが揃ってから read_frame を呼ぶことで read_exact のブロッキングを回避する。
#[allow(dead_code)]
pub(crate) fn wait_for_full_frame<P: FramePeek>(p: &P, deadline: Instant) -> io::Result<()> {
    let mut interval = POLL_MIN;
    loop {
        let (total, len_opt) = p.peek()?;
        if let Some(len) = len_opt {
            if len as usize > MAX_RESPONSE_FRAME_LEN {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("frame length {len} exceeds maximum {MAX_RESPONSE_FRAME_LEN}"),
                ));
            }
            if total as u64 >= 4 + len as u64 {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        // sleep が次の deadline 判定より先に走るため、本 fn は deadline を最大 POLL_MAX(~3ms)
        // 超過して戻り得る（deadline は hard でなく soft）。ms 級の tier に対しては無視できる。
        std::thread::sleep(interval);
        interval = (interval * 2).min(POLL_MAX);
    }
}

/// 名前付きパイプ `\\.\pipe\nospacekey-engine` 経由でエンジンに要求を送るクライアント。
///
/// Windows の名前付きパイプは通常のファイルとして開けるが、通常の generic access は
/// `FILE_CREATE_PIPE_INSTANCE` まで含み得る。published DACL と同じ明示的な 0x12019b
/// の read/write/attributes/synchronize だけを要求して接続する。
pub struct EngineClient {
    pipe: File,
    /// 未読応答を owe している状態。Windows ではフレームの部分読取り位置も保持する。
    /// true/Some の間は交互性が崩れているため、次の要求を送る前に必ず drain_pending で
    /// 読み切る。
    #[cfg(windows)]
    pending: Option<win_io::PendingResponseFrame>,
    #[cfg(windows)]
    /// 要求の write/read 途中で接続の再利用が安全でなくなった状態。
    poisoned: bool,
    #[cfg(not(windows))]
    pending: bool,
}

const PIPE_PATH: &str = r"\\.\pipe\nospacekey-engine";

pub const PIPE_CLIENT_ACCESS_MASK: u32 = 0x0012_019b;

#[cfg(windows)]
fn open_named_pipe(pipe_path: &str) -> io::Result<File> {
    use std::os::windows::io::FromRawHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = pipe_path.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            PIPE_CLIENT_ACCESS_MASK,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
            None,
        )
    }
    .map_err(|error| io::Error::other(error.to_string()))?;
    if handle.is_invalid() {
        // CreateFileW normally returns Err for INVALID_HANDLE_VALUE, but keep the
        // ownership boundary explicit if a future windows crate changes that mapping.
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle.0 as _) })
}

#[cfg(not(windows))]
fn open_named_pipe(pipe_path: &str) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_path)
}

/// per-logon-session で安定な pipe 名。同一セッションの全 TIP インスタンス・engine・設定アプリが
/// 同じ名を算出する（Spec2 で crates/tip/src/engine_link.rs から移設 — 設定アプリの
/// ClearLearning が同じ engine へ届くための唯一の算出点）。
pub fn pipe_name_for_session(session_id: u32) -> String {
    format!(
        r"\\.\pipe\nospacekey-engine.v{}.b{}.s{session_id}",
        crate::protocol::PROTO_VERSION,
        env!("CARGO_PKG_VERSION")
    )
}

/// 現プロセスの logon session id。取得失敗時は 0。
/// （ipc crate の windows 依存は cfg(windows) 限定（Cargo.toml）なので cfg ゲートを添える —
///   client.rs の既存 #[cfg(not(windows))] フォールバックと同じ流儀。M-3）
#[cfg(windows)]
pub fn current_session_id() -> u32 {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let mut sid: u32 = 0;
    // SAFETY: out param へ書くだけ。失敗時は sid=0 のまま。
    let ok = unsafe { ProcessIdToSessionId(std::process::id(), &mut sid) };
    if ok.is_ok() {
        sid
    } else {
        0
    }
}

#[cfg(not(windows))]
pub fn current_session_id() -> u32 {
    0
}

/// このプロセスが接続/起動すべき安定 pipe 名。
pub fn stable_pipe_name() -> String {
    pipe_name_for_session(current_session_id())
}

impl EngineClient {
    /// 既定パイプ `\\.\pipe\nospacekey-engine` へ接続（最大 `timeout` までリトライ）。
    pub fn connect(timeout: Duration) -> io::Result<Self> {
        Self::connect_to(PIPE_PATH, timeout)
    }

    /// 指定したパイプ名へ接続（最大 `timeout` までリトライ）。サーバ未起動なら待って失敗を返す。
    /// TIP はプロセス毎に一意のパイプ名で自分専用エンジンへ接続するためこちらを使う。
    pub fn connect_to(pipe_path: &str, timeout: Duration) -> io::Result<Self> {
        let deadline = Instant::now() + timeout;
        let mut interval = CONNECT_POLL_MIN;
        loop {
            match open_named_pipe(pipe_path) {
                Ok(pipe) => {
                    return Ok(Self {
                        pipe,
                        #[cfg(windows)]
                        pending: None,
                        #[cfg(windows)]
                        poisoned: false,
                        #[cfg(not(windows))]
                        pending: false,
                    })
                }
                Err(e) if Instant::now() < deadline => {
                    let _ = e;
                    // 残り時間を超えて眠らない（短い timeout 窓で soft-deadline を悪化させない）
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    std::thread::sleep(interval.min(remaining.max(Duration::from_millis(1))));
                    interval = (interval * 2).min(CONNECT_POLL_MAX);
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn connect_verified_to(
        pipe_path: &str,
        connect_timeout: Duration,
        start_deadline: Instant,
    ) -> Result<VerifiedEngineClient, EngineIdentityError> {
        let mut client = Self::connect_to(pipe_path, connect_timeout)?;
        let session = verify_start_session(|request| {
            client
                .request_within(request, start_deadline)
                .map_err(EngineIdentityError::Io)
        })?;
        Ok(VerifiedEngineClient { client, session })
    }

    /// 1要求を送り、1応答を受け取る。フレーミング（4byte長さ前置）は内部で処理する。
    pub fn request(&mut self, req: &Request) -> io::Result<Response> {
        #[cfg(windows)]
        {
            self.ensure_ready()?;
            if let Err(error) = win_io::write_request(&self.pipe, req, None) {
                // The explicit InvalidInput path is the preflight request-size rejection and
                // performs zero writes, so it does not poison an otherwise healthy connection.
                if error.kind() == io::ErrorKind::InvalidInput {
                    return Err(error);
                }
                self.poisoned = true;
                return Err(error);
            }
            let mut pending = win_io::PendingResponseFrame::default();
            match win_io::read_response_progress(&self.pipe, None, &mut pending) {
                Ok(response) => Ok(response),
                Err(error) => {
                    self.poisoned = true;
                    Err(error)
                }
            }
        }
        #[cfg(not(windows))]
        {
            write_request_frame(&mut self.pipe, req)?;
            read_frame(&mut self.pipe)
        }
    }

    /// `deadline` までに応答フレームを読み切れなければ `TimedOut` を返す。
    /// Windows では header/body を event-backed overlapped I/O で同じ絶対期限まで待つ。
    ///
    /// 不変条件（H5）: この write→read は「厳密な 要求→応答 交互 かつ `&mut self` 単一所有」を
    /// 前提に安全である。すなわち (1) 1 つの `EngineClient` は同時に 1 要求しか飛ばさない
    /// （`&mut self` で直列化。呼び出し側 TIP も UI スレッド or 専属 LLM ワーカのどちらか一方が
    /// 排他所有する）、(2) サーバは 1 要求に 1 応答を厳密に交互で返す（protocol.rs は seq 相関を
    /// 持たず、正しさはこの交互性のみに依存）。この 2 つが成り立つ限り、read_response が読む
    /// 次のフレーム＝いま送った要求の応答であり、他フレームが割り込むことはない。
    /// 将来 パイプライン化／複数フレーム滞留／クライアント共有を導入するなら、この前提が壊れる
    /// ので seq 相関 or 応答フレーム境界の明示ドレインを必ず併せて入れること。
    ///
    /// 唯一の例外（pending+drain）: 交互性を回復する手段は「破棄」だけではない。要求がタイムアウト
    /// しても応答フレームは後から到着するので、接続を捨てずに `pending` を立て、次の要求を送る前に
    /// `drain_pending` でその滞留フレームを 1 枚読み切れば交互性は保たれる。この drain 方式なら
    /// 接続（＝サーバ側セッション）を破棄せずに済む。よって不変条件は次の 2 択に一般化される:
    /// タイムアウト後は **(a) 接続破棄**（従来。EndSession 失敗など安全側で捨てる TIP 側 Bug 1）
    /// または **(b) pending を立てて次送信前に drain**（LiveConvert/Insert が接続維持のため選ぶ）。
    /// pending を owe したまま request_within を呼ぶのは規律違反なので `InvalidInput` で弾く
    /// （呼び出し側は必ず drain_pending してから request_within/request_within_keep を呼ぶこと）。
    #[cfg(windows)]
    pub fn request_within(&mut self, req: &Request, deadline: Instant) -> io::Result<Response> {
        self.ensure_ready()?;
        if let Err(error) = win_io::write_request(&self.pipe, req, Some(deadline)) {
            if error.kind() == io::ErrorKind::InvalidInput {
                return Err(error);
            }
            self.poisoned = true;
            return Err(error);
        }
        // The non-keep API deliberately does not retain a partial response.  Its callers
        // discard the connection on any read failure, so retaining a frame here would only
        // make an unsafe accidental reuse look recoverable.
        let mut pending = win_io::PendingResponseFrame::default();
        match win_io::read_response_progress(&self.pipe, Some(deadline), &mut pending) {
            Ok(response) => Ok(response),
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }

    /// `request_within` と同一だが、応答の deadline 超過（TimedOut）のときに接続を捨てず
    /// `pending` と部分フレームの読取り位置を保持する。呼び出し側（LiveConvert/Insert）は
    /// 接続とサーバ側セッションを守りたい経路で使い、次の要求の前に `drain_pending` で同じ
    /// 滞留応答を読み切る責務を負う。要求の write deadline 超過は要求自体が部分送信の可能性
    /// があり、`ConnectionAborted` に変換して接続を再利用不能にする（caller が drop する）。
    /// TimedOut 以外の read エラーも交互性を回復できないので接続を再利用不能にする。
    #[cfg(windows)]
    pub fn request_within_keep(
        &mut self,
        req: &Request,
        deadline: Instant,
    ) -> io::Result<Response> {
        self.ensure_ready()?;
        // A write timeout may leave a partial request in the server's byte stream.  It can
        // never be recovered by draining a response, so poison the connection and expose a
        // non-TimedOut error.  This is intentional: TIP's existing non-timeout branch drops
        // the client, while a TimedOut result would incorrectly make it attempt another frame.
        if let Err(error) = win_io::write_request(&self.pipe, req, Some(deadline)) {
            if error.kind() == io::ErrorKind::InvalidInput {
                return Err(error);
            }
            self.poisoned = true;
            if error.kind() == io::ErrorKind::TimedOut {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "request write timed out; named-pipe connection is not reusable",
                ));
            }
            return Err(error);
        }
        let mut pending = win_io::PendingResponseFrame::default();
        match win_io::read_response_progress(&self.pipe, Some(deadline), &mut pending) {
            Ok(resp) => Ok(resp),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                // Preserve both header and body offsets.  A later drain resumes this exact
                // frame instead of interpreting a partially consumed body as a new header.
                self.pending = Some(pending);
                Err(e)
            }
            Err(e) => {
                self.poisoned = true;
                Err(e)
            }
        }
    }

    /// pending（未読応答を owe）状態なら、保存した header/body offset から滞留フレームを
    /// 1 枚読み切って交互性を回復する。pending でなければ `Ok(None)`。`deadline` までに
    /// 揃えば pending をクリアして `Ok(Some(resp))`、予算切れは `TimedOut`（pending は維持＝
    /// 呼び出し側が INV5 の暴走ガードで最終判断）、パイプ破断や不正フレームは接続を再利用不能
    /// にする（caller が drop する）。
    #[cfg(windows)]
    pub fn drain_pending(&mut self, deadline: Instant) -> io::Result<Option<Response>> {
        if self.poisoned {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "named-pipe connection is not reusable",
            ));
        }
        let Some(mut pending) = self.pending.take() else {
            return Ok(None);
        };
        match win_io::read_response_progress(&self.pipe, Some(deadline), &mut pending) {
            Ok(response) => Ok(Some(response)),
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                // Keep the exact offsets so a later drain can continue without desynchronizing
                // the byte-mode pipe.
                self.pending = Some(pending);
                Err(error)
            }
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }

    /// pending 状態か（呼び出し側の規律チェック用）。
    pub fn is_pending(&self) -> bool {
        #[cfg(windows)]
        {
            self.pending.is_some()
        }
        #[cfg(not(windows))]
        {
            self.pending
        }
    }

    #[cfg(windows)]
    fn ensure_ready(&self) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "named-pipe connection is not reusable",
            ));
        }
        if self.pending.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "request called while a response is owed; drain_pending first",
            ));
        }
        Ok(())
    }

    /// 非 Windows では Win32 の hard-deadline I/O が無いため従来どおりブロッキング。
    #[cfg(not(windows))]
    pub fn request_within(&mut self, req: &Request, _deadline: Instant) -> io::Result<Response> {
        self.request(req)
    }

    /// 非 Windows では締め切りブロックが無いので keep 版も従来どおりブロッキング。
    #[cfg(not(windows))]
    pub fn request_within_keep(
        &mut self,
        req: &Request,
        _deadline: Instant,
    ) -> io::Result<Response> {
        self.request(req)
    }

    /// 非 Windows では pending を作れないので常に `Ok(None)`。
    #[cfg(not(windows))]
    pub fn drain_pending(&mut self, _deadline: Instant) -> io::Result<Option<Response>> {
        Ok(None)
    }
}

#[cfg(windows)]
mod win_io {
    use crate::protocol::{Request, Response};
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::time::Instant;
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_IO_PENDING, ERROR_NOT_FOUND, ERROR_OPERATION_ABORTED, HANDLE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    fn handle(file: &File) -> HANDLE {
        HANDLE(file.as_raw_handle() as _)
    }

    fn windows_error(error: windows::core::Error) -> io::Error {
        io::Error::other(error.to_string())
    }

    fn is_win32_error(error: &windows::core::Error, code: u32) -> bool {
        // windows-rs represents Win32 failures as HRESULT_FROM_WIN32(code).
        // The low word remains the original Win32 error number.
        (error.code().0 as u32 & 0xffff) == code
    }

    fn remaining(deadline: Option<Instant>) -> u32 {
        deadline
            .map(|d| {
                d.saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(u32::MAX as u128) as u32
            })
            .unwrap_or(INFINITE)
    }

    pub(super) fn completed_bytes<F>(completion: F) -> windows::core::Result<u32>
    where
        F: FnOnce(&mut u32) -> windows::core::Result<()>,
    {
        let mut transferred = 0u32;
        completion(&mut transferred).map(|()| transferred)
    }

    fn get_completed_bytes(h: HANDLE, overlapped: *const OVERLAPPED) -> io::Result<u32> {
        completed_bytes(|transferred| unsafe {
            GetOverlappedResult(h, overlapped, transferred, false)
        })
        .map_err(windows_error)
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum CancelledCompletion {
        Completed(u32),
        TimedOut,
        Failed(u32),
    }

    /// Classify the result after the deadline observer requested cancellation. A completion
    /// that won the CancelIoEx race is still usable progress and must be returned to the caller;
    /// only a confirmed ERROR_OPERATION_ABORTED on a real timeout is a timeout result.
    pub(super) fn classify_cancelled_completion(
        wait_timed_out: bool,
        cancel_error: Option<u32>,
        reap: Result<u32, u32>,
    ) -> CancelledCompletion {
        match reap {
            Ok(bytes) => CancelledCompletion::Completed(bytes),
            Err(_) if cancel_error.is_some() => {
                CancelledCompletion::Failed(cancel_error.expect("checked above"))
            }
            Err(reap_error) if wait_timed_out && reap_error == ERROR_OPERATION_ABORTED.0 => {
                CancelledCompletion::TimedOut
            }
            Err(error) => CancelledCompletion::Failed(error),
        }
    }

    /// A response frame can outlive one deadline when the keep API is used.  Keep the exact
    /// header/body offsets so the next drain resumes the same frame on the byte-mode pipe.
    #[derive(Default)]
    pub(super) struct PendingResponseFrame {
        header: [u8; 4],
        header_read: usize,
        body: Option<Vec<u8>>,
        body_read: usize,
    }

    fn overlapped<F>(file: &File, deadline: Option<Instant>, start: F) -> io::Result<u32>
    where
        F: FnOnce(HANDLE, *mut OVERLAPPED) -> windows::core::Result<()>,
    {
        if deadline
            .map(|deadline| Instant::now() >= deadline)
            .unwrap_or(false)
        {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        let event = unsafe { CreateEventW(None, true, false, None) }.map_err(windows_error)?;
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        let h = handle(file);
        let started = start(h, &mut overlapped);
        match started {
            Ok(()) => {
                let result = get_completed_bytes(h, &overlapped);
                unsafe {
                    let _ = CloseHandle(event);
                }
                return result;
            }
            Err(error) if is_win32_error(&error, ERROR_IO_PENDING.0) => {}
            Err(error) => {
                unsafe {
                    let _ = CloseHandle(event);
                }
                return Err(windows_error(error));
            }
        }

        let wait = unsafe { WaitForSingleObject(event, remaining(deadline)) };
        if wait == WAIT_OBJECT_0 {
            let result = get_completed_bytes(h, &overlapped);
            unsafe {
                let _ = CloseHandle(event);
            }
            return result;
        }

        // Deadline or wait failure: cancel, then always reap completion before the OVERLAPPED
        // and its buffer leave scope. ERROR_NOT_FOUND means completion won the cancel race;
        // a successful reap is returned as real progress even though the deadline was observed.
        let cancel_failure = match unsafe { CancelIoEx(h, Some(&overlapped as *const _)) } {
            Ok(()) => None,
            Err(error) if is_win32_error(&error, ERROR_NOT_FOUND.0) => None,
            Err(error) => Some(error),
        };
        // Even an unexpected CancelIoEx failure must be followed by a completion reap before
        // the stack OVERLAPPED and caller-owned buffer leave scope.
        let reap = unsafe {
            // Normally the event is signalled by completion.  If the reap wait itself fails,
            // use GetOverlappedResult's blocking mode as the final lifetime barrier so the
            // OVERLAPPED and caller-owned buffer cannot leave scope while the kernel references
            // them.
            let reap_wait = WaitForSingleObject(event, INFINITE);
            if reap_wait == WAIT_OBJECT_0 {
                completed_bytes(|transferred| {
                    GetOverlappedResult(h, &overlapped, transferred, false)
                })
            } else {
                completed_bytes(|transferred| {
                    GetOverlappedResult(h, &overlapped, transferred, true)
                })
            }
        };
        let classification = classify_cancelled_completion(
            wait == WAIT_TIMEOUT,
            cancel_failure
                .as_ref()
                .map(|error| error.code().0 as u32 & 0xffff),
            reap.as_ref()
                .map(|bytes| *bytes)
                .map_err(|error| error.code().0 as u32 & 0xffff),
        );
        unsafe {
            let _ = CloseHandle(event);
        }
        match classification {
            CancelledCompletion::Completed(bytes) => Ok(bytes),
            CancelledCompletion::TimedOut => Err(io::Error::from(io::ErrorKind::TimedOut)),
            CancelledCompletion::Failed(_) => {
                if let Some(error) = cancel_failure {
                    Err(windows_error(error))
                } else {
                    Err(windows_error(
                        reap.expect_err("failed classification requires reap error"),
                    ))
                }
            }
        }
    }

    fn read_exact_progress(
        file: &File,
        buffer: &mut [u8],
        offset: &mut usize,
        deadline: Option<Instant>,
    ) -> io::Result<()> {
        while *offset < buffer.len() {
            let start = *offset;
            let read = overlapped(file, deadline, |h, overlapped| unsafe {
                ReadFile(h, Some(&mut buffer[start..]), None, Some(overlapped))
            })?;
            if read == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            *offset += read as usize;
        }
        Ok(())
    }

    fn write_all(file: &File, buffer: &[u8], deadline: Option<Instant>) -> io::Result<()> {
        let mut offset = 0;
        while offset < buffer.len() {
            let wrote = overlapped(file, deadline, |h, overlapped| unsafe {
                WriteFile(h, Some(&buffer[offset..]), None, Some(overlapped))
            })?;
            if wrote == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            offset += wrote as usize;
        }
        Ok(())
    }

    pub(super) fn write_request(
        file: &File,
        req: &Request,
        deadline: Option<Instant>,
    ) -> io::Result<()> {
        let body = serde_json::to_vec(req)?;
        if body.len() > crate::framing::MAX_REQUEST_FRAME_LEN || u32::try_from(body.len()).is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "request frame length {} exceeds maximum {}",
                    body.len(),
                    crate::framing::MAX_REQUEST_FRAME_LEN
                ),
            ));
        }
        let header = (body.len() as u32).to_le_bytes();
        write_all(file, &header, deadline)?;
        write_all(file, &body, deadline)
    }

    pub(super) fn read_response_progress(
        file: &File,
        deadline: Option<Instant>,
        pending: &mut PendingResponseFrame,
    ) -> io::Result<Response> {
        read_exact_progress(
            file,
            &mut pending.header,
            &mut pending.header_read,
            deadline,
        )?;
        let len = u32::from_le_bytes(pending.header) as usize;
        if len > crate::framing::MAX_RESPONSE_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "frame length {len} exceeds maximum {}",
                    crate::framing::MAX_RESPONSE_FRAME_LEN
                ),
            ));
        }
        if pending.body.is_none() {
            pending.body = Some(vec![0u8; len]);
        }
        let body = pending
            .body
            .as_mut()
            .expect("response body is allocated after header validation");
        read_exact_progress(file, body, &mut pending.body_read, deadline)?;
        let body = pending
            .body
            .take()
            .expect("response body remains present until it is complete");
        serde_json::from_slice(&body)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

#[cfg(test)]
mod wait_tests {
    use super::*;
    use std::cell::RefCell;
    use std::time::{Duration, Instant};

    type PeekState = (u32, Option<u32>);
    type PeekResult = std::io::Result<PeekState>;

    /// 呼ぶたびに次の状態を返し、尽きたら最後の状態を返し続ける疑似 peek。
    struct FakePeek {
        states: RefCell<Vec<PeekResult>>,
        last: PeekState,
    }
    impl FakePeek {
        fn ok(states: Vec<PeekState>) -> Self {
            let last = *states
                .last()
                .expect("FakePeek::ok requires at least one state");
            Self {
                states: RefCell::new(states.into_iter().map(Ok).rev().collect()),
                last,
            }
        }
        fn err() -> Self {
            Self {
                states: RefCell::new(vec![Err(std::io::Error::from(
                    std::io::ErrorKind::BrokenPipe,
                ))]),
                last: (0, None),
            }
        }
    }
    impl FramePeek for FakePeek {
        fn peek(&self) -> std::io::Result<(u32, Option<u32>)> {
            self.states.borrow_mut().pop().unwrap_or(Ok(self.last))
        }
    }

    #[test]
    fn ready_single_shot_returns_ok() {
        // total 14 >= 4 + len(10) → 即 Ok
        let p = FakePeek::ok(vec![(14, Some(10))]);
        assert!(wait_for_full_frame(&p, Instant::now() + Duration::from_secs(1)).is_ok());
    }

    #[test]
    fn split_arrival_waits_then_ok() {
        // len は揃うが total 不足 → 数回後に十分量 → Ok
        let p = FakePeek::ok(vec![(4, Some(10)), (4, Some(10)), (14, Some(10))]);
        assert!(wait_for_full_frame(&p, Instant::now() + Duration::from_secs(1)).is_ok());
    }

    #[test]
    fn never_enough_times_out() {
        let p = FakePeek::ok(vec![(0, None)]);
        let err = wait_for_full_frame(&p, Instant::now() + Duration::from_millis(30)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn oversized_len_is_invalid_data() {
        let big = (MAX_RESPONSE_FRAME_LEN as u32).wrapping_add(1);
        let p = FakePeek::ok(vec![(8, Some(big))]);
        let err = wait_for_full_frame(&p, Instant::now() + Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn peek_error_propagates() {
        let p = FakePeek::err();
        let err = wait_for_full_frame(&p, Instant::now() + Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }
}

#[cfg(all(test, windows))]
mod win_pipe_tests {
    use super::*;
    use crate::framing::{read_frame, write_frame};
    use std::io::Write;
    use std::time::{Duration, Instant};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    // windows 0.62: PIPE_ACCESS_DUPLEX は FILE_FLAGS_AND_ATTRIBUTES 型で Storage::FileSystem に在る
    // （CreateNamedPipeW の dwopenmode 引数の型）。Pipes モジュールには無いので import 元を分ける。
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// サーバ端の pipe インスタンスを1個作って握ったまま返す（応答は返さない）。
    /// クライアントが接続でき、かつ何も返ってこない状況を作る。
    fn create_server_with_input(
        name: &str,
        input_buffer_size: u32,
    ) -> windows::Win32::Foundation::HANDLE {
        let w = wide(name);
        // windows 0.62: CreateNamedPipeW（W 版）は Result ではなく HANDLE を直接返し、
        // 失敗は INVALID_HANDLE_VALUE。A 版だけが Result を返すため .expect は使えない。
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(w.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,    // nMaxInstances
                4096, // out buffer
                input_buffer_size,
                0,    // default timeout
                None, // default security
            )
        };
        assert!(!handle.is_invalid(), "CreateNamedPipeW failed");
        handle
    }

    fn create_server(name: &str) -> windows::Win32::Foundation::HANDLE {
        create_server_with_input(name, 4096)
    }

    #[test]
    fn synchronous_completion_uses_kernel_completion_count() {
        let mut callback_called = false;
        let count = super::win_io::completed_bytes(|transferred| {
            callback_called = true;
            *transferred = 17;
            Ok(())
        })
        .expect("completion callback should provide the actual count");
        assert!(callback_called);
        assert_eq!(count, 17);
    }

    #[test]
    fn cancel_completion_race_keeps_kernel_reported_progress() {
        assert_eq!(
            super::win_io::classify_cancelled_completion(true, None, Ok(17)),
            super::win_io::CancelledCompletion::Completed(17)
        );
        assert_eq!(
            super::win_io::classify_cancelled_completion(
                true,
                None,
                Err(windows::Win32::Foundation::ERROR_OPERATION_ABORTED.0)
            ),
            super::win_io::CancelledCompletion::TimedOut
        );
        // If CancelIoEx itself failed, an aborted reap is not proof that this caller's cancel
        // won; preserve a non-timeout failure for the connection poison path.
        assert_eq!(
            super::win_io::classify_cancelled_completion(
                true,
                Some(5),
                Err(windows::Win32::Foundation::ERROR_OPERATION_ABORTED.0)
            ),
            super::win_io::CancelledCompletion::Failed(5)
        );
    }

    #[test]
    fn request_within_times_out_when_no_reply() {
        // 一意名（プロセス/スレッド由来）。Date/rand は使えないのでアドレスで一意化。
        let name = format!(r"\\.\pipe\nospacekey-a8-test-{:p}", &0u8 as *const u8);
        let server = create_server(&name);

        // クライアント接続 → 応答が来ないので TimedOut になること。
        let mut client =
            EngineClient::connect_to(&name, Duration::from_secs(1)).expect("client connect failed");
        let started = Instant::now();
        let res = client.request_within(
            &Request::StartSession,
            Instant::now() + Duration::from_millis(80),
        );
        let elapsed = started.elapsed();

        unsafe {
            let _ = CloseHandle(server);
        }

        let err = res.expect_err("expected timeout error");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        // 締め切り(80ms)近辺で戻ること（無限ブロックしていない）。
        assert!(
            elapsed < Duration::from_millis(800),
            "took too long: {elapsed:?}"
        );
    }

    /// 要求を受信してから `delay` 後に `resp` を書く応答サーバをスレッドで動かす。
    /// クライアントが接続でき、締め切りより遅れて応答が到着する状況を作る（ドレイン検証用）。
    fn spawn_delayed_reply_server(
        name: String,
        delay: Duration,
        resp: Response,
    ) -> std::thread::JoinHandle<()> {
        use std::os::windows::io::FromRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Pipes::ConnectNamedPipe;
        let server = create_server(&name);
        // HANDLE(*mut c_void) は Send でないのでスレッド境界は usize で渡す。
        let server_addr = server.0 as usize;
        std::thread::spawn(move || {
            let server = HANDLE(server_addr as *mut core::ffi::c_void);
            // クライアント接続を待つ。既に接続済みなら ERROR_PIPE_CONNECTED（無視してよい）。
            unsafe {
                let _ = ConnectNamedPipe(server, None);
            }
            // 生ハンドルを File に載せて既存フレーミングを再利用（drop で CloseHandle される）。
            let mut f = unsafe { std::fs::File::from_raw_handle(server.0 as _) };
            // 要求を 1 枚読み切る（読めなくてもテストは応答書き込みまで進める）。
            let _: io::Result<Request> = read_frame(&mut f);
            std::thread::sleep(delay);
            let _ = write_frame(&mut f, &resp);
            // f の drop でサーバ端を閉じる。
        })
    }

    /// 応答フレームを意図的に分割して書くサーバ。最初の write の後に待つことで、
    /// keep API が header/body の途中で期限切れになり、後続 drain が同じフレームを
    /// 継続することを実パイプ上で固定する。
    fn spawn_split_reply_server(
        name: String,
        first_bytes: usize,
        delay: Duration,
        resp: Response,
    ) -> std::thread::JoinHandle<()> {
        use std::os::windows::io::FromRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Pipes::ConnectNamedPipe;
        let server = create_server(&name);
        let server_addr = server.0 as usize;
        std::thread::spawn(move || {
            let server = HANDLE(server_addr as *mut core::ffi::c_void);
            unsafe {
                let _ = ConnectNamedPipe(server, None);
            }
            let mut f = unsafe { std::fs::File::from_raw_handle(server.0 as _) };
            let _: io::Result<Request> = read_frame(&mut f);

            let body = serde_json::to_vec(&resp).expect("response serialization");
            let mut frame = Vec::with_capacity(4 + body.len());
            frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
            frame.extend_from_slice(&body);
            assert!(first_bytes > 0 && first_bytes < frame.len());
            f.write_all(&frame[..first_bytes])
                .expect("first response fragment");
            std::thread::sleep(delay);
            f.write_all(&frame[first_bytes..])
                .expect("remaining response fragment");
        })
    }

    /// live 経路の keep 版がタイムアウトで pending を立て、サーバ応答到着後に drain_pending が
    /// その滞留フレームを回収して交互性を回復し、次の要求が正しい応答を受けることを検証する。
    /// （1-off desync が起きていれば「1つ前の応答」を読むので、応答内容の照合で検出できる。）
    #[test]
    fn keep_then_drain_recovers_alternation() {
        let name = format!(r"\\.\pipe\nospacekey-drain-test-{:p}", &0u8 as *const u8);
        // 1 回目の要求（LiveConvert 相当）へ ~120ms 遅れで応答。締め切り 40ms は超過する。
        let server = spawn_delayed_reply_server(
            name.clone(),
            Duration::from_millis(120),
            Response::LiveResult {
                seq: 1,
                text: "日本語".into(),
                reading: "にほんご".into(),
                committed: None,
            },
        );

        let mut client =
            EngineClient::connect_to(&name, Duration::from_secs(1)).expect("client connect failed");

        // keep 版: 締め切り 40ms を超過 → TimedOut かつ pending が立つ。
        let r = client.request_within_keep(
            &Request::LiveConvert {
                session: 1,
                seq: 1,
                left_context: None,
                auto_commit: false,
            },
            Instant::now() + Duration::from_millis(40),
        );
        assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(
            client.is_pending(),
            "keep 版のタイムアウトで pending が立つべき"
        );

        // pending 中に request_within を呼ぶと規律違反として弾かれる（送信前 drain の強制）。
        let guarded = client.request_within(
            &Request::StartSession,
            Instant::now() + Duration::from_millis(10),
        );
        assert_eq!(
            guarded.unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(client.is_pending(), "ガードで弾かれても pending は維持");

        // サーバ応答が到着するまで余裕を見て drain。回収した応答が 1 回目のものであること。
        let drained = client
            .drain_pending(Instant::now() + Duration::from_millis(500))
            .expect("drain must not error")
            .expect("drain must recover the owed response");
        match drained {
            Response::LiveResult { seq, .. } => assert_eq!(seq, 1),
            other => panic!("unexpected drained response: {other:?}"),
        }
        assert!(!client.is_pending(), "drain 成功で pending はクリアされる");

        server.join().ok();
    }

    #[test]
    fn partial_header_timeout_then_drain_resumes_same_frame() {
        let name = format!(
            r"\\.\pipe\nospacekey-partial-header-{:p}",
            &0u8 as *const u8
        );
        let server = spawn_split_reply_server(
            name.clone(),
            2,
            Duration::from_millis(120),
            Response::Session {
                session: 42,
                proto: None,
                boot: None,
            },
        );
        let mut client =
            EngineClient::connect_to(&name, Duration::from_secs(1)).expect("client connect failed");

        let error = client
            .request_within_keep(
                &Request::StartSession,
                Instant::now() + Duration::from_millis(40),
            )
            .expect_err("partial header must hit the first deadline");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(client.is_pending());

        // No second request may be written while the partial response is owed.
        let guarded = client
            .request_within_keep(&Request::Ping, Instant::now() + Duration::from_millis(10))
            .expect_err("pending response must gate a new request");
        assert_eq!(guarded.kind(), io::ErrorKind::InvalidInput);

        let response = client
            .drain_pending(Instant::now() + Duration::from_millis(500))
            .expect("drain must resume the frame")
            .expect("pending response must be present");
        assert_eq!(
            response,
            Response::Session {
                session: 42,
                proto: None,
                boot: None,
            }
        );
        assert!(!client.is_pending());
        server.join().expect("split response server");
    }

    #[test]
    fn partial_body_timeout_then_drain_resumes_same_frame() {
        let name = format!(r"\\.\pipe\nospacekey-partial-body-{:p}", &0u8 as *const u8);
        let response = Response::Reading {
            reading: "にほんご".into(),
        };
        let body_len = serde_json::to_vec(&response)
            .expect("response serialization")
            .len();
        assert!(body_len > 2);
        let server = spawn_split_reply_server(
            name.clone(),
            4 + 2,
            Duration::from_millis(120),
            Response::Reading {
                reading: "にほんご".into(),
            },
        );
        let mut client =
            EngineClient::connect_to(&name, Duration::from_secs(1)).expect("client connect failed");

        let error = client
            .request_within_keep(
                &Request::StartSession,
                Instant::now() + Duration::from_millis(40),
            )
            .expect_err("partial body must hit the first deadline");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(client.is_pending());

        let drained = client
            .drain_pending(Instant::now() + Duration::from_millis(500))
            .expect("drain must resume the body")
            .expect("pending response must be present");
        assert_eq!(drained, response);
        assert!(!client.is_pending());
        server.join().expect("split response server");
    }

    #[test]
    fn write_timeout_poison_is_non_timed_out_and_blocks_next_send() {
        let name = format!(r"\\.\pipe\nospacekey-write-timeout-{:p}", &0u8 as *const u8);
        // The server never reads.  A tiny input buffer makes the large request's body write
        // remain pending until the absolute deadline, exercising the partial-write boundary.
        let server = create_server_with_input(&name, 1);
        let mut client =
            EngineClient::connect_to(&name, Duration::from_secs(1)).expect("client connect failed");
        let result = client.request_within_keep(
            &Request::Insert {
                session: 1,
                text: "x".repeat(240_000),
                style: None,
            },
            Instant::now() + Duration::from_millis(60),
        );
        let error = result.expect_err("write must time out against a non-reading server");
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert!(
            !client.is_pending(),
            "partial request must not become drainable"
        );

        let next = client
            .request_within_keep(&Request::Ping, Instant::now() + Duration::from_millis(10))
            .expect_err("poisoned connection must reject the next send");
        assert_eq!(next.kind(), io::ErrorKind::ConnectionAborted);
        unsafe {
            let _ = CloseHandle(server);
        }
    }

    /// pending でないときの drain は Ok(None)（no-op）で、続く request_within が通常どおり動く。
    #[test]
    fn drain_when_not_pending_is_noop() {
        let name = format!(r"\\.\pipe\nospacekey-drain-noop-{:p}", &0u8 as *const u8);
        let server = create_server(&name);
        let mut client =
            EngineClient::connect_to(&name, Duration::from_secs(1)).expect("client connect failed");
        assert!(!client.is_pending());
        let drained = client
            .drain_pending(Instant::now() + Duration::from_millis(10))
            .expect("no-op drain must be Ok");
        assert!(drained.is_none());

        unsafe {
            let _ = CloseHandle(server);
        }
    }

    /// keep 版でタイムアウト → pending 中に無応答のまま drain 予算が尽きると drain は TimedOut を
    /// 返し、pending は維持される（呼び出し側 TIP が INV5 の暴走ガードで最終判断する）。
    #[test]
    fn drain_budget_exhausted_keeps_pending() {
        let name = format!(r"\\.\pipe\nospacekey-drain-exhaust-{:p}", &0u8 as *const u8);
        let server = create_server(&name);
        let mut client =
            EngineClient::connect_to(&name, Duration::from_secs(1)).expect("client connect failed");

        let r = client.request_within_keep(
            &Request::Insert {
                session: 1,
                text: "n".into(),
                style: None,
            },
            Instant::now() + Duration::from_millis(30),
        );
        assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(client.is_pending());

        // 無応答なので drain も締め切りで TimedOut。pending はそのまま。
        let d = client.drain_pending(Instant::now() + Duration::from_millis(30));
        assert_eq!(d.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(client.is_pending(), "drain 予算切れでも pending は維持");

        unsafe {
            let _ = CloseHandle(server);
        }
    }
}

#[cfg(all(test, windows))]
mod connect_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// 存在しないパイプ名 + timeout=0 → リトライせず即 Err（一発プローブ意味論の固定）。
    #[test]
    fn zero_timeout_is_single_shot() {
        let name = format!(r"\\.\pipe\nospacekey-a7-noexist-{:p}", &0u8 as *const u8);
        let started = Instant::now();
        let r = EngineClient::connect_to(&name, Duration::ZERO);
        assert!(r.is_err());
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "single-shot probe must not sleep"
        );
    }

    /// 存在しないパイプ名 + timeout=100ms → deadline 近辺で戻る（ランプが deadline を大きく超過しない）。
    #[test]
    fn ramp_respects_deadline() {
        let name = format!(r"\\.\pipe\nospacekey-a7-noexist2-{:p}", &0u8 as *const u8);
        let started = Instant::now();
        let r = EngineClient::connect_to(&name, Duration::from_millis(100));
        assert!(r.is_err());
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(100),
            "should keep retrying until deadline: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(400),
            "ramp must clamp to remaining time: {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod pipe_name_tests {
    use super::*;

    #[test]
    fn client_access_mask_is_published_read_write_without_create_instance() {
        assert_eq!(PIPE_CLIENT_ACCESS_MASK, 0x0012_019b);
        assert_eq!(PIPE_CLIENT_ACCESS_MASK & 0x4, 0);
    }

    #[test]
    fn pipe_name_is_stable_and_session_scoped() {
        assert_eq!(
            pipe_name_for_session(1),
            concat!(
                r"\\.\pipe\nospacekey-engine.v8.b",
                env!("CARGO_PKG_VERSION"),
                ".s1"
            )
        );
        assert_eq!(pipe_name_for_session(7), pipe_name_for_session(7));
        assert_ne!(pipe_name_for_session(1), pipe_name_for_session(2));
    }

    #[test]
    fn session_identity_requires_exact_wire_and_boot_match() {
        let matching = Response::Session {
            session: 41,
            proto: Some(crate::protocol::PROTO_VERSION),
            boot: Some(env!("CARGO_PKG_VERSION").into()),
        };
        assert_eq!(verify_session_identity(matching).unwrap(), 41);

        for response in [
            Response::Session {
                session: 41,
                proto: Some(crate::protocol::PROTO_VERSION + 1),
                boot: Some(env!("CARGO_PKG_VERSION").into()),
            },
            Response::Session {
                session: 41,
                proto: Some(crate::protocol::PROTO_VERSION),
                boot: Some("different-build".into()),
            },
            Response::Session {
                session: 41,
                proto: None,
                boot: None,
            },
        ] {
            assert!(matches!(
                verify_session_identity(response),
                Err(EngineIdentityError::Mismatch { .. })
            ));
        }
    }

    #[test]
    fn mismatch_handshake_sends_only_start_session() {
        let mut sent = Vec::new();
        let result = verify_start_session(|request| {
            sent.push(matches!(request, Request::StartSession));
            Ok(Response::Session {
                session: 9,
                proto: Some(crate::protocol::PROTO_VERSION),
                boot: Some("loaded-old-build".into()),
            })
        });
        assert!(matches!(result, Err(EngineIdentityError::Mismatch { .. })));
        assert_eq!(sent, vec![true]);
    }
}
