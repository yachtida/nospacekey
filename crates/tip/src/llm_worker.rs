//! Owned LLM request lane: connect, replay the captured styled reading, then convert.
//! No STA connection or COM pointer crosses the thread boundary.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use ipc::client::EngineClient;
use ipc::protocol::{Request, Response};
use crate::input_module::{InputSegment, TextStyle};

pub struct LlmOutcome {
    pub seq: u64,
    pub result: Result<String, String>,
}
pub type LlmSlot = Arc<Mutex<Option<LlmOutcome>>>;

fn convert_reading(
    session: i64,
    segments: Vec<InputSegment>,
    seq: u64,
    left_context: Option<String>,
    mut request: impl FnMut(&Request) -> Result<Response, String>,
) -> Result<String, String> {
    for segment in segments {
        let response = request(&Request::Insert {
            session,
            text: segment.text,
            style: match segment.style { TextStyle::Kana => None, TextStyle::Direct => Some("direct".into()) },
        })?;
        if !matches!(response, Response::Reading { .. }) {
            return Err("reading replay rejected".into());
        }
    }
    match request(&Request::LlmConvert { session, seq, left_context })? {
        Response::LlmResult { seq: echoed, text } if echoed == seq => Ok(text),
        Response::LlmResult { .. } => Err("LLM sequence mismatch".into()),
        _ => Err("LLM request rejected".into()),
    }
}

/// A single deadline covers the verified handshake, replay, and conversion.
/// The worker closes its own connection even when the STA abandons the result slot.
pub fn spawn_llm_worker(
    pipe: String,
    segments: Vec<InputSegment>,
    seq: u64,
    left_context: Option<String>,
    slot: LlmSlot,
    timeout: Duration,
) -> std::io::Result<()> {
    let guard = crate::globals::ComObjectGuard::new();
    std::thread::Builder::new().name("nospacekey-llm".into()).spawn(move || {
        let _guard = guard;
        let deadline = Instant::now() + timeout;
        let result = EngineClient::connect_verified_to(&pipe, timeout.min(Duration::from_millis(500)), deadline)
            .map_err(|_| "LLM connection failed".to_string())
            .and_then(|mut client| {
                convert_reading(client.session(), segments, seq, left_context, |request| {
                    client.request_within(request, deadline).map_err(|_| "LLM IPC failed".into())
                })
            });
        if let Ok(mut outcome) = slot.lock() {
            *outcome = Some(LlmOutcome { seq, result });
        }
    }).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captured_segments_are_acknowledged_in_order_before_conversion() {
        let segments = vec![
            InputSegment { text: "にほん".into(), style: TextStyle::Kana },
            InputSegment { text: "GPU".into(), style: TextStyle::Direct },
        ];
        let mut calls = 0;
        let result = convert_reading(7, segments, 3, Some("context".into()), |request| {
            match calls {
                0 => assert!(matches!(request, Request::Insert { session: 7, text, style: None } if text == "にほん")),
                1 => assert!(matches!(request, Request::Insert { session: 7, text, style: Some(style) } if text == "GPU" && style == "direct")),
                2 => assert!(matches!(request, Request::LlmConvert { session: 7, seq: 3, left_context: Some(context) } if context == "context")),
                _ => panic!("unexpected extra request"),
            }
            calls += 1;
            Ok(match request {
                Request::Insert { .. } => Response::Reading { reading: "reading".into() },
                _ => Response::LlmResult { seq: 3, text: "日本GPU".into() },
            })
        });
        assert_eq!(result.unwrap(), "日本GPU");
        assert_eq!(calls, 3);
    }

    #[test]
    fn replay_rejection_prevents_conversion_and_wrong_sequence_is_rejected() {
        let mut calls = 0;
        let result = convert_reading(7, vec![InputSegment { text: "に".into(), style: TextStyle::Kana }], 3, None, |_| {
            calls += 1;
            Ok(Response::Error { message: "rejected".into() })
        });
        assert!(result.is_err());
        assert_eq!(calls, 1);
        assert!(convert_reading(7, vec![], 3, None, |_| Ok(Response::LlmResult { seq: 2, text: "stale".into() })).is_err());
    }

    #[test]
    fn slot_holds_outcome() {
        let slot: LlmSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
        *slot.lock().unwrap() = Some(LlmOutcome {
            seq: 1,
            result: Ok("x".into()),
        });
        let taken = slot.lock().unwrap().take();
        assert!(matches!(taken, Some(LlmOutcome { seq: 1, .. })));
    }

    /// A nonresponsive verified handshake is bounded and publishes an error.
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

            let slot: LlmSlot = Arc::new(Mutex::new(None));
            // 巡3 P6: io::Result 返却化に伴う戻り値の明示的破棄（生成失敗はテスト対象外）。
            let _ = spawn_llm_worker(name.clone(), vec![], 3, None, slot.clone(), Duration::from_millis(200));

            // 5秒以内に必ず outcome が書かれること（旧実装ならここで永久に来ない）。
            let deadline = Instant::now() + Duration::from_secs(5);
            let outcome = loop {
                if let Some(o) = slot.lock().unwrap().take() {
                    break o;
                }
                assert!(
                    Instant::now() < deadline,
                    "worker never posted outcome (unbounded block)"
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            unsafe {
                let _ = CloseHandle(server);
            }
            assert_eq!(outcome.seq, 3);
            assert!(
                outcome.result.is_err(),
                "expected error, got {:?}",
                outcome.result
            );
        }
    }
}
