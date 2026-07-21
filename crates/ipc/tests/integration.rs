//! エンジン host (NospacekeyEngineHost.exe) が起動している前提の結合テスト。
//! 事前に `engine-host` をビルドして起動しておくこと。CI では `#[ignore]`。
//!
//! 実行: `cargo test -p ipc --test integration -- --ignored --nocapture`

use ipc::client::EngineClient;
use ipc::protocol::{Request, Response};
use std::time::Duration;

#[test]
#[ignore] // 事前に engine-host を起動しておくこと
fn convert_nihongo_returns_kanji() {
    let mut c = EngineClient::connect(Duration::from_secs(2)).unwrap();
    let sid = match c.request(&Request::StartSession).unwrap() {
        Response::Session { session, .. } => session,
        other => panic!("expected Session, got {:?}", other),
    };
    c.request(&Request::Insert { session: sid, text: "nihongo".into(), style: None }).unwrap();
    let cands = match c.request(&Request::Convert { session: sid, left_context: None }).unwrap() {
        Response::Candidates { candidates } => candidates,
        other => panic!("expected Candidates, got {:?}", other),
    };
    assert!(cands.iter().any(|s| s == "日本語"), "got {:?}", cands);
}

/// TIP の実挙動を IPC 越しに再現する自己完結テスト（実機 IME バグの再現/回帰用）:
///  - エンジンを **一意パイプ名を引数に** 自分で起動する（main.swift の argv 経路）
///  - `connect_to` で **その専用パイプ** に接続する（プロセス毎一意化の経路）
///  - **1文字ずつ** Insert する（key_event_sink.rs OnKeyDown と同じ）→ Convert
///
/// 実行: `cargo test -p ipc --test integration tip_like -- --ignored --nocapture`
#[test]
#[ignore] // engine-host をビルド済みであること（テストが自分で起動する）
fn tip_like_per_char_over_unique_pipe() {
    use std::process::Command;

    let exe = engine_exe_path();
    assert!(exe.exists(), "engine exe not built: {}", exe.display());
    let pipe = format!(r"\\.\pipe\nospacekey-engine-itest-{}", std::process::id());

    let _child = ChildGuard(Command::new(&exe).arg(&pipe).spawn().expect("spawn engine"));
    // 専用パイプへ接続（最大5s）。
    let mut c = match EngineClient::connect_to(&pipe, Duration::from_secs(5)) {
        Ok(c) => c,
        Err(e) => panic!("connect_to({pipe}) failed: {e}"),
    };

    let sid = match c.request(&Request::StartSession).unwrap() {
        Response::Session { session, .. } => session,
        other => panic!("expected Session, got {:?}", other),
    };
    // TIP と同じく1文字ずつ送る。
    for ch in "nihongo".chars() {
        c.request(&Request::Insert { session: sid, text: ch.to_string(), style: None }).unwrap();
    }
    let cands = match c.request(&Request::Convert { session: sid, left_context: None }).unwrap() {
        Response::Candidates { candidates } => candidates,
        other => panic!("expected Candidates, got {:?}", other),
    };
    assert!(cands.iter().any(|s| s == "日本語"), "got {:?}", cands);
}

/// ライブ変換: 1文字ずつ Insert→LiveConvert し、seq エコーと最終 text=日本語 を検証。
/// 実行: cargo test -p ipc --test integration live_convert -- --ignored --nocapture
#[test]
#[ignore]
fn live_convert_returns_kanji_per_char() {
    use std::process::Command;
    let exe = engine_exe_path();
    assert!(exe.exists(), "engine exe not built: {}", exe.display());
    let pipe = format!(r"\\.\pipe\nospacekey-engine-live-{}", std::process::id());
    let _child = ChildGuard(Command::new(&exe).arg(&pipe).spawn().expect("spawn engine"));
    let mut c = match EngineClient::connect_to(&pipe, Duration::from_secs(5)) {
        Ok(c) => c,
        Err(e) => panic!("connect_to({pipe}) failed: {e}"),
    };
    let sid = match c.request(&Request::StartSession).unwrap() {
        Response::Session { session, .. } => session,
        other => panic!("expected Session, got {:?}", other),
    };
    let mut last = String::new();
    for (i, ch) in "nihongo".chars().enumerate() {
        c.request(&Request::Insert { session: sid, text: ch.to_string(), style: None }).unwrap();
        match c.request(&Request::LiveConvert { session: sid, seq: i as u64, left_context: None, auto_commit: false }).unwrap() {
            Response::LiveResult { seq, text, reading, committed: _ } => {
                assert_eq!(seq, i as u64, "seq echoed");
                assert!(!reading.is_empty(), "reading non-empty at len {}", i + 1);
                last = text;
            }
            other => panic!("expected LiveResult, got {:?}", other),
        }
    }
    assert_eq!(last, "日本語", "final live text should be 日本語, got {last}");
}

/// echo モード: engine が "LLM:"+reading を即返すことを確認（スレッド配線の決定的検証用）。
/// 実行: NOSPACEKEY_LLM_ECHO=1 cargo test -p ipc --test integration llm_convert_echo -- --ignored --nocapture
#[test]
#[ignore]
fn llm_convert_echo_returns_marker() {
    use std::process::Command;
    let exe = engine_exe_path();
    assert!(exe.exists(), "engine exe not built: {}", exe.display());
    let pipe = format!(r"\\.\pipe\nospacekey-engine-llm-{}", std::process::id());
    let _child = ChildGuard(Command::new(&exe).arg(&pipe)
        .env("NOSPACEKEY_LLM_ECHO", "1")
        .spawn().expect("spawn engine"));
    let mut c = match EngineClient::connect_to(&pipe, Duration::from_secs(5)) {
        Ok(c) => c,
        Err(e) => panic!("connect_to({pipe}) failed: {e}"),
    };
    let sid = match c.request(&Request::StartSession).unwrap() {
        Response::Session { session, .. } => session,
        other => panic!("expected Session, got {:?}", other),
    };
    for ch in "nihongo".chars() {
        c.request(&Request::Insert { session: sid, text: ch.to_string(), style: None }).unwrap();
    }
    let text = match c.request(&Request::LlmConvert { session: sid, seq: 1, left_context: None }).unwrap() {
        Response::LlmResult { seq, text } => { assert_eq!(seq, 1); text }
        other => panic!("expected LlmResult, got {:?}", other),
    };
    assert!(text.starts_with("LLM:"), "echo marker expected, got {text}");
}

/// 子プロセスを drop 時に kill+wait して zombie 化を防ぐガード。
struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// ワークスペース直下の engine-host のビルド済み exe パス（debug）。
fn engine_exe_path() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR = <workspace>/crates/ipc
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join(r"engine-host\.build\x86_64-unknown-windows-msvc\debug\NospacekeyEngineHost.exe")
}
