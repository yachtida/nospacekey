//! EngineHost を一意な一時layoutへstageして起動する結合テスト。CI では `#[ignore]`。
//!
//! 実行: `cargo test -p ipc --test integration tip_like -- --ignored --nocapture`

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
    c.request(&Request::Insert {
        session: sid,
        text: "nihongo".into(),
        style: None,
    })
    .unwrap();
    let cands = match c
        .request(&Request::Convert {
            session: sid,
            left_context: None,
        })
        .unwrap()
    {
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

    let engine = IsolatedEngine::stage();
    let exe = engine.exe();
    let pipe = isolated_pipe("itest");

    let _child = start_engine(Command::new(&exe).arg(&pipe));
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
        c.request(&Request::Insert {
            session: sid,
            text: ch.to_string(),
            style: None,
        })
        .unwrap();
    }
    let cands = match c
        .request(&Request::Convert {
            session: sid,
            left_context: None,
        })
        .unwrap()
    {
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
    let engine = IsolatedEngine::stage();
    let exe = engine.exe();
    let pipe = isolated_pipe("live");
    let _child = start_engine(Command::new(&exe).arg(&pipe));
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
        c.request(&Request::Insert {
            session: sid,
            text: ch.to_string(),
            style: None,
        })
        .unwrap();
        match c
            .request(&Request::LiveConvert {
                session: sid,
                seq: i as u64,
                left_context: None,
                auto_commit: false,
            })
            .unwrap()
        {
            Response::LiveResult {
                seq,
                text,
                reading,
                committed: _,
            } => {
                assert_eq!(seq, i as u64, "seq echoed");
                assert!(!reading.is_empty(), "reading non-empty at len {}", i + 1);
                last = text;
            }
            other => panic!("expected LiveResult, got {:?}", other),
        }
    }
    assert_eq!(
        last, "日本語",
        "final live text should be 日本語, got {last}"
    );
}

/// echo モード: engine が "LLM:"+reading を即返すことを確認（スレッド配線の決定的検証用）。
/// 実行: cargo test -p ipc --test integration llm_convert_echo -- --ignored --nocapture
#[test]
#[ignore]
fn llm_convert_echo_returns_marker() {
    use std::process::Command;
    let engine = IsolatedEngine::stage();
    let exe = engine.exe();
    let pipe = isolated_pipe("llm");
    let _child = start_engine(
        Command::new(&exe)
            .arg(&pipe)
            .env("NOSPACEKEY_LLM_ECHO", "1"),
    );
    let mut c = match EngineClient::connect_to(&pipe, Duration::from_secs(5)) {
        Ok(c) => c,
        Err(e) => panic!("connect_to({pipe}) failed: {e}"),
    };
    let sid = match c.request(&Request::StartSession).unwrap() {
        Response::Session { session, .. } => session,
        other => panic!("expected Session, got {:?}", other),
    };
    for ch in "nihongo".chars() {
        c.request(&Request::Insert {
            session: sid,
            text: ch.to_string(),
            style: None,
        })
        .unwrap();
    }
    let text = match c
        .request(&Request::LlmConvert {
            session: sid,
            seq: 1,
            left_context: None,
        })
        .unwrap()
    {
        Response::LlmResult { seq, text } => {
            assert_eq!(seq, 1);
            text
        }
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

fn start_engine(command: &mut std::process::Command) -> ChildGuard {
    let mut child = command.spawn().expect("spawn engine");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        child.try_wait().expect("query engine"),
        None,
        "engine exited before listening"
    );
    ChildGuard(child)
}

struct IsolatedEngine {
    root: std::path::PathBuf,
    exe: std::path::PathBuf,
}

fn isolated_pipe(label: &str) -> String {
    format!(
        r"\\.\pipe\nospacekey-engine-{label}-{}.s{}",
        std::process::id(),
        ipc::client::current_session_id()
    )
}

impl IsolatedEngine {
    fn stage() -> Self {
        let source = engine_build_dir();
        let source_exe = source.join("NospacekeyEngineHost.exe");
        assert!(
            source_exe.is_file(),
            "engine exe not built: {}",
            source_exe.display()
        );
        let root = std::env::temp_dir().join(format!(
            "nospacekey-ipc-integration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::create_dir(&root).expect("create isolated engine layout");
        for entry in std::fs::read_dir(&source).expect("read engine build layout") {
            let entry = entry.expect("read engine entry");
            let path = entry.path();
            let name = entry.file_name();
            if path.is_file()
                && (path.extension().is_some_and(|extension| extension == "dll")
                    || name == "NospacekeyEngineHost.exe")
            {
                std::fs::copy(&path, root.join(&name)).expect("copy engine artifact");
            } else if path.is_dir() && name.to_string_lossy().ends_with(".resources") {
                copy_tree(&path, &root.join(&name));
            }
        }
        std::fs::write(
            root.join(".nospacekey-lifetime"),
            b"nospacekey version lifetime sentinel\n",
        )
        .expect("write lifetime sentinel");
        let exe = root.join("NospacekeyEngineHost.exe");
        Self { root, exe }
    }

    fn exe(&self) -> &std::path::Path {
        &self.exe
    }
}

impl Drop for IsolatedEngine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn copy_tree(source: &std::path::Path, destination: &std::path::Path) {
    std::fs::create_dir(destination).expect("create resource directory");
    for entry in std::fs::read_dir(source).expect("read resource directory") {
        let entry = entry.expect("read resource entry");
        let target = destination.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy resource file");
        }
    }
}

fn engine_build_dir() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR = <workspace>/crates/ipc
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join(r"engine-host\.build\x86_64-unknown-windows-msvc\debug")
}
