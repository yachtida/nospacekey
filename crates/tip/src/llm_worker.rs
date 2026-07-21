//! 外部LLM変換のワーカスレッドと結果スロット。
//!
//! TIP 本体（`TextService`）は STA・`!Send`（Rc/Cell/RefCell・COM）だが、`EngineClient` は
//! `std::fs::File` を包むだけで `Send`。Tab 押下中は入力ロックで UI スレッドが IPC を出さない
//! ので、接続を1本このワーカへ move して秒オーダのブロッキング呼び出しをさせ、結果だけを
//! 共有スロット（`Arc<Mutex>`）へ書き戻す。UI スレッドは別途ポーリングタイマでスロットを見る。
//! **スレッド境界を越えるのは owned 値（`EngineClient`/`String`）のみ。**

use std::sync::{Arc, Mutex};

use ipc::client::EngineClient;
use ipc::protocol::{Request, Response};

/// ワーカ→UIスレッドへ返す結果。借りた `EngineClient` を同梱して返却する。
pub struct LlmOutcome {
    pub seq: u64,
    /// Ok(補正文) / Err(メッセージ)。
    pub result: Result<String, String>,
    /// 借りた接続。UI スレッドが Ok 時は再格納、Err 時は drop（破棄）する。
    pub client: Option<EngineClient>,
}

/// ワーカ→UIスレッド受け渡しスロット。Tab ごとに新規生成する。
pub type LlmSlot = Arc<Mutex<Option<LlmOutcome>>>;

/// ワーカスレッドを起動する。`client` を move し `LlmConvert` を実行、結果を slot へ。
/// `timeout` はワーカ自身の待ち上限（B10: 旧実装は無期限 `request` で、応答しないエンジンが
/// 生きている限りワーカスレッドとエンジン側接続スレッドを永久占有した）。UI 側の LLM_TIMEOUT(8s)
/// より長く取り、通常のエンジン側タイムアウト応答（llm_timeout_ms 既定 15s）は従来どおり受け取る。
/// 既知の限界: `request_within` の期限は read 待ちにのみ効き、write は非有界のまま
/// （client.rs の phase1 制約）。LlmConvert は小フレームで実際には write ブロックしないが、
/// 期限が write 詰まりまで覆う保証は無い（監査 B3 と同根・レビュー M-1）。
pub fn spawn_llm_worker(
    mut client: EngineClient,
    session: i64,
    seq: u64,
    left_context: Option<String>,
    slot: LlmSlot,
    timeout: std::time::Duration,
) {
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + timeout;
        let result: Result<String, String> = match client
            .request_within(&Request::LlmConvert { session, seq, left_context }, deadline)
        {
            // エコーされた seq が一致する応答だけ採用する。接続は Tab 毎に再利用されるため、
            // 前要求の未読フレームを読んでしまった場合（ストリーム desync）はここで弾き、
            // Err にして接続を破棄＝次操作で貼り直す（誤った結果を確定させない）。
            Ok(Response::LlmResult { seq: echoed, text }) if echoed == seq => Ok(text),
            Ok(Response::LlmResult { seq: echoed, .. }) =>
                Err(format!("seq mismatch: got {echoed}, want {seq}")),
            Ok(Response::Error { message }) => Err(message),
            Ok(other) => Err(format!("unexpected response: {other:?}")),
            Err(e) => Err(format!("ipc error: {e}")),
        };
        // IPC 成功なら接続を返す。エラー（接続破損の可能性）なら返さない＝UIで drop_engine。
        let client_back = if result.is_ok() { Some(client) } else { None };
        if let Ok(mut g) = slot.lock() {
            *g = Some(LlmOutcome { seq, result, client: client_back });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slot_holds_outcome() {
        let slot: LlmSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
        *slot.lock().unwrap() = Some(LlmOutcome { seq: 1, result: Ok("x".into()), client: None });
        let taken = slot.lock().unwrap().take();
        assert!(matches!(taken, Some(LlmOutcome { seq: 1, .. })));
    }

    /// Windows 限定: 応答を返さない dead-reply pipe を相手に、ワーカが `timeout` で諦めて
    /// Err を slot へ書き、接続を返さない（=UI 側 drop_engine 合流）ことを証明する。
    /// 旧実装（無期限 `request`）はエンジンが生きているが応答しない間ワーカスレッドが永久ブロックし、
    /// エンジン側の接続スレッドも1本占有し続けた（2026-07-10 跨プロセスブロッキング監査 B10）。
    #[cfg(windows)]
    mod win {
        use super::super::*;
        use std::time::{Duration, Instant};
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::CloseHandle;
        // windows 0.62: PIPE_ACCESS_DUPLEX は FILE_FLAGS_AND_ATTRIBUTES 型で Storage::FileSystem に在る。
        use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
        use windows::Win32::System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
        };

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }

        /// サーバ端の pipe インスタンスを1個だけ作って握ったまま返す（応答は返さない）。
        fn create_server(name: &str) -> windows::Win32::Foundation::HANDLE {
            let w = wide(name);
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(w.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1,    // nMaxInstances
                    4096, // out buffer
                    4096, // in buffer
                    0,    // default timeout
                    None, // default security
                )
            };
            assert!(!handle.is_invalid(), "CreateNamedPipeW failed");
            handle
        }

        #[test]
        fn worker_posts_error_within_timeout_when_engine_never_replies() {
            // 一意名（スタックアドレス由来）。Date/rand は使えないのでアドレスで一意化。
            let name = format!(r"\\.\pipe\nospacekey-llmw-test-{:p}", &0u8 as *const u8);
            let server = create_server(&name);
            let client = EngineClient::connect_to(&name, Duration::from_secs(1)).expect("connect");

            let slot: LlmSlot = Arc::new(Mutex::new(None));
            spawn_llm_worker(client, 7, 3, None, slot.clone(), Duration::from_millis(200));

            // 5秒以内に必ず outcome が書かれること（旧実装ならここで永久に来ない）。
            let deadline = Instant::now() + Duration::from_secs(5);
            let outcome = loop {
                if let Some(o) = slot.lock().unwrap().take() {
                    break o;
                }
                assert!(Instant::now() < deadline, "worker never posted outcome (unbounded block)");
                std::thread::sleep(Duration::from_millis(10));
            };
            unsafe {
                let _ = CloseHandle(server);
            }
            assert_eq!(outcome.seq, 3);
            assert!(outcome.result.is_err(), "expected error, got {:?}", outcome.result);
            assert!(outcome.client.is_none(), "timed-out connection must not be returned");
        }
    }
}
