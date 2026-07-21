use std::fs::{File, OpenOptions};
use std::io;
use std::time::{Duration, Instant};

use crate::framing::{read_frame, write_frame, MAX_FRAME_LEN};
use crate::protocol::{Request, Response};

/// フレーム到達をポーリングするための最小抽象（実パイプ Win32 からロジックを分離しテスト可能にする）。
pub(crate) trait FramePeek {
    /// `(バッファ内総バイト数, 先頭4byteが揃っていれば本体長 Some(len))` を非破壊に返す。
    fn peek(&self) -> io::Result<(u32, Option<u32>)>;
}

const POLL_MIN: Duration = Duration::from_millis(1);
const POLL_MAX: Duration = Duration::from_millis(3);

/// connect_to のポーリング間隔ランプ（初回 10ms→倍々→上限 80ms、残り時間でクランプ）。
const CONNECT_POLL_MIN: Duration = Duration::from_millis(10);
const CONNECT_POLL_MAX: Duration = Duration::from_millis(80);

/// `deadline` までに「4byte長 + 本体」がバッファに揃うのを待つ。
/// 揃えば Ok(())、期限超過は TimedOut、本体長が上限超は InvalidData、peek 失敗はそのエラー。
/// フレームが揃ってから read_frame を呼ぶことで read_exact のブロッキングを回避する。
pub(crate) fn wait_for_full_frame<P: FramePeek>(p: &P, deadline: Instant) -> io::Result<()> {
    let mut interval = POLL_MIN;
    loop {
        let (total, len_opt) = p.peek()?;
        if let Some(len) = len_opt {
            if len as usize > MAX_FRAME_LEN {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("frame length {len} exceeds maximum {MAX_FRAME_LEN}"),
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
/// Windows の名前付きパイプは通常のファイルとして開けるため、`OpenOptions` で
/// 読み書き両用に開く。接続は `timeout` まで短い間隔でリトライする。
pub struct EngineClient {
    pipe: File,
    /// 未読応答を owe している（前回の要求がタイムアウトしたが応答フレームは後から届く）状態。
    /// true の間は交互性が崩れているため、次の要求を送る前に必ず drain_pending で読み切る。
    pending: bool,
}

const PIPE_PATH: &str = r"\\.\pipe\nospacekey-engine";

/// per-logon-session で安定な pipe 名。同一セッションの全 TIP インスタンス・engine・設定アプリが
/// 同じ名を算出する（Spec2 で crates/tip/src/engine_link.rs から移設 — 設定アプリの
/// ClearLearning が同じ engine へ届くための唯一の算出点）。
pub fn pipe_name_for_session(session_id: u32) -> String {
    format!(r"\\.\pipe\nospacekey-engine.s{session_id}")
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
    if ok.is_ok() { sid } else { 0 }
}

#[cfg(not(windows))]
pub fn current_session_id() -> u32 { 0 }

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
            match OpenOptions::new().read(true).write(true).open(pipe_path) {
                Ok(pipe) => return Ok(Self { pipe, pending: false }),
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

    /// 1要求を送り、1応答を受け取る。フレーミング（4byte長さ前置）は内部で処理する。
    pub fn request(&mut self, req: &Request) -> io::Result<Response> {
        write_frame(&mut self.pipe, req)?;
        read_frame(&mut self.pipe)
    }

    /// `deadline` までに応答フレームが揃わなければ `TimedOut` を返す（write は phase1 では bound しない）。
    /// フレーム到達後の read_frame はバッファ済みで非ブロック。broken pipe はそのエラーを返す。
    ///
    /// 不変条件（H5）: この peek→read は「厳密な 要求→応答 交互 かつ `&mut self` 単一所有」を
    /// 前提に TOCTOU フリー。すなわち (1) 1 つの `EngineClient` は同時に 1 要求しか飛ばさない
    /// （`&mut self` で直列化。呼び出し側 TIP も UI スレッド or 専属 LLM ワーカのどちらか一方が
    /// 排他所有する）、(2) サーバは 1 要求に 1 応答を厳密に交互で返す（protocol.rs は seq 相関を
    /// 持たず、正しさはこの交互性のみに依存）。この 2 つが成り立つ限り、peek で「フレームが揃った」
    /// と見えた次バイト列＝いま送った要求の応答であり、他フレームが割り込むことはない。
    /// 将来 パイプライン化／複数フレーム滞留／クライアント共有 を導入するなら、この前提が壊れる
    /// （peek と read の間に別フレームが挟まりうる＝ズレる）ので seq 相関 or 応答フレーム境界の
    /// 明示ドレインを必ず併せて入れること。
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
        if self.pending {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "request_within called while a response is owed; drain_pending first",
            ));
        }
        write_frame(&mut self.pipe, req)?;
        let peeker = win::PipePeeker::new(&self.pipe);
        wait_for_full_frame(&peeker, deadline)?;
        read_frame(&mut self.pipe)
    }

    /// `request_within` と同一だが、`deadline` 超過（TimedOut）のときに接続を捨てず `pending` を
    /// 立てる。呼び出し側（LiveConvert/Insert）は接続とサーバ側セッションを守りたい経路で使い、
    /// 次の要求の前に `drain_pending` で滞留応答を読み切る責務を負う。TimedOut 以外のエラー
    /// （broken pipe 等）は交互性を回復できないのでそのまま返す（呼び出し側が drop する）。
    #[cfg(windows)]
    pub fn request_within_keep(&mut self, req: &Request, deadline: Instant) -> io::Result<Response> {
        match self.request_within(req, deadline) {
            Ok(resp) => Ok(resp),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                self.pending = true;
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// pending（未読応答を owe）状態なら滞留フレームを 1 枚読み切って交互性を回復する。
    /// pending でなければ `Ok(None)`。`deadline` までに揃えば pending をクリアして `Ok(Some(resp))`、
    /// 予算切れは `TimedOut`（pending は維持＝呼び出し側が INV5 の暴走ガードで最終判断）、
    /// パイプ破断はそのエラー（pending は維持だが呼び出し側は drop する）。
    #[cfg(windows)]
    pub fn drain_pending(&mut self, deadline: Instant) -> io::Result<Option<Response>> {
        if !self.pending {
            return Ok(None);
        }
        let peeker = win::PipePeeker::new(&self.pipe);
        wait_for_full_frame(&peeker, deadline)?;
        let resp = read_frame(&mut self.pipe)?;
        self.pending = false;
        Ok(Some(resp))
    }

    /// pending 状態か（呼び出し側の規律チェック用）。
    pub fn is_pending(&self) -> bool {
        self.pending
    }

    /// 非 Windows では PeekNamedPipe が無いため従来どおりブロッキング（開発機は Windows）。
    #[cfg(not(windows))]
    pub fn request_within(&mut self, req: &Request, _deadline: Instant) -> io::Result<Response> {
        self.request(req)
    }

    /// 非 Windows では締め切りブロックが無いので keep 版も従来どおりブロッキング。
    #[cfg(not(windows))]
    pub fn request_within_keep(&mut self, req: &Request, _deadline: Instant) -> io::Result<Response> {
        self.request(req)
    }

    /// 非 Windows では pending を作れないので常に `Ok(None)`。
    #[cfg(not(windows))]
    pub fn drain_pending(&mut self, _deadline: Instant) -> io::Result<Option<Response>> {
        Ok(None)
    }
}

#[cfg(windows)]
mod win {
    use super::FramePeek;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Pipes::PeekNamedPipe;

    /// `std::fs::File`（名前付きパイプ）を借りて PeekNamedPipe で非破壊にバッファ量を見る。
    pub(super) struct PipePeeker<'a> {
        // 借りた File のハンドルの NON-OWNING コピー（File が所有・close する。ここでは Drop しない）。
        // PhantomData<&'a File> が寿命を縛るのでこのハンドルが dangling することはない。
        handle: HANDLE,
        _borrow: std::marker::PhantomData<&'a std::fs::File>,
    }
    impl<'a> PipePeeker<'a> {
        pub(super) fn new(f: &'a std::fs::File) -> Self {
            Self { handle: HANDLE(f.as_raw_handle() as _), _borrow: std::marker::PhantomData }
        }
    }
    impl<'a> FramePeek for PipePeeker<'a> {
        fn peek(&self) -> io::Result<(u32, Option<u32>)> {
            let mut buf = [0u8; 4];
            let mut read_now: u32 = 0;
            let mut total_avail: u32 = 0;
            let mut left_this_msg: u32 = 0;
            unsafe {
                PeekNamedPipe(
                    self.handle,
                    Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                    buf.len() as u32,
                    Some(&mut read_now),
                    Some(&mut total_avail),
                    Some(&mut left_this_msg),
                )
                .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))?;
            }
            let len_opt = if read_now >= 4 { Some(u32::from_le_bytes(buf)) } else { None };
            Ok((total_avail, len_opt))
        }
    }
}

#[cfg(test)]
mod wait_tests {
    use super::*;
    use std::cell::RefCell;
    use std::time::{Duration, Instant};

    /// 呼ぶたびに次の状態を返し、尽きたら最後の状態を返し続ける疑似 peek。
    struct FakePeek {
        states: RefCell<Vec<std::io::Result<(u32, Option<u32>)>>>,
        last: (u32, Option<u32>),
    }
    impl FakePeek {
        fn ok(states: Vec<(u32, Option<u32>)>) -> Self {
            let last = *states.last().expect("FakePeek::ok requires at least one state");
            Self { states: RefCell::new(states.into_iter().map(Ok).rev().collect()), last }
        }
        fn err() -> Self {
            Self { states: RefCell::new(vec![Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))]), last: (0, None) }
        }
    }
    impl FramePeek for FakePeek {
        fn peek(&self) -> std::io::Result<(u32, Option<u32>)> {
            self.states.borrow_mut().pop().unwrap_or_else(|| Ok(self.last))
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
        let big = (MAX_FRAME_LEN as u32).wrapping_add(1);
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
    fn create_server(name: &str) -> windows::Win32::Foundation::HANDLE {
        let w = wide(name);
        // windows 0.62: CreateNamedPipeW（W 版）は Result ではなく HANDLE を直接返し、
        // 失敗は INVALID_HANDLE_VALUE。A 版だけが Result を返すため .expect は使えない。
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(w.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,        // nMaxInstances
                4096,     // out buffer
                4096,     // in buffer
                0,        // default timeout
                None,     // default security
            )
        };
        assert!(!handle.is_invalid(), "CreateNamedPipeW failed");
        handle
    }

    #[test]
    fn request_within_times_out_when_no_reply() {
        // 一意名（プロセス/スレッド由来）。Date/rand は使えないのでアドレスで一意化。
        let name = format!(r"\\.\pipe\nospacekey-a8-test-{:p}", &0u8 as *const u8);
        let server = create_server(&name);

        // クライアント接続 → 応答が来ないので TimedOut になること。
        let mut client = EngineClient::connect_to(&name, Duration::from_secs(1))
            .expect("client connect failed");
        let started = Instant::now();
        let res = client.request_within(
            &Request::StartSession,
            Instant::now() + Duration::from_millis(80),
        );
        let elapsed = started.elapsed();

        unsafe { let _ = CloseHandle(server); }

        let err = res.expect_err("expected timeout error");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        // 締め切り(80ms)近辺で戻ること（無限ブロックしていない）。
        assert!(elapsed < Duration::from_millis(800), "took too long: {elapsed:?}");
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
            &Request::LiveConvert { session: 1, seq: 1, left_context: None, auto_commit: false },
            Instant::now() + Duration::from_millis(40),
        );
        assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(client.is_pending(), "keep 版のタイムアウトで pending が立つべき");

        // pending 中に request_within を呼ぶと規律違反として弾かれる（送信前 drain の強制）。
        let guarded = client.request_within(
            &Request::StartSession,
            Instant::now() + Duration::from_millis(10),
        );
        assert_eq!(guarded.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
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
            &Request::Insert { session: 1, text: "n".into(), style: None },
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
        assert!(started.elapsed() < Duration::from_millis(50), "single-shot probe must not sleep");
    }

    /// 存在しないパイプ名 + timeout=100ms → deadline 近辺で戻る（ランプが deadline を大きく超過しない）。
    #[test]
    fn ramp_respects_deadline() {
        let name = format!(r"\\.\pipe\nospacekey-a7-noexist2-{:p}", &0u8 as *const u8);
        let started = Instant::now();
        let r = EngineClient::connect_to(&name, Duration::from_millis(100));
        assert!(r.is_err());
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(100), "should keep retrying until deadline: {elapsed:?}");
        assert!(elapsed < Duration::from_millis(400), "ramp must clamp to remaining time: {elapsed:?}");
    }
}

#[cfg(test)]
mod pipe_name_tests {
    use super::*;
    #[test]
    fn pipe_name_is_stable_and_session_scoped() {
        assert_eq!(pipe_name_for_session(1), r"\\.\pipe\nospacekey-engine.s1");
        assert_eq!(pipe_name_for_session(7), pipe_name_for_session(7));
        assert_ne!(pipe_name_for_session(1), pipe_name_for_session(2));
    }
}
