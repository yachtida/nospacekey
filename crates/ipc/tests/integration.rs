//! EngineHost を一意な一時layoutへstageして起動する結合テスト。CI では `#[ignore]`。
//!
//! 実行: `cargo test -p ipc --test integration tip_like -- --ignored --nocapture`

use ipc::client::EngineClient;
use ipc::protocol::{Request, Response};
use std::time::Duration;

#[test]
#[ignore = "requires a built Swift engine"]
fn clause_prefix_candidates_over_unique_pipe() {
    use ipc::clause::*;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let engine = IsolatedEngine::stage();
    let pipe = isolated_pipe("clause-prefix");
    let _child = start_engine(Command::new(engine.exe()).arg(&pipe)
        .creation_flags(0x08000000)
        .env("NOSPACEKEY_ZENZAI", "off")
        .env("NOSPACEKEY_LEARNING", "0")
        .env("NOSPACEKEY_MEMORY_DIR", engine.root.join("memory"))
        .env("TEMP", &engine.root).env("TMP", &engine.root)
        .stdout(Stdio::null()).stderr(Stdio::null()));
    let mut client = EngineClient::connect_to(&pipe, Duration::from_secs(5)).unwrap();
    let session = ipc::client::verify_session_identity(
        client.request(&Request::StartSession).unwrap()).unwrap();
    let snapshot = client.request(&Request::LiveSnapshot {
        composition: 1, revision: 1, configuration_generation: 1, connection_generation: 1,
        conversion_revision: 0, request_id: 1,
        segments: vec![ipc::protocol::SnapshotSegment { text: "がぞうのように".into(), style: Some("direct".into()) }],
        explicit: true, live_search_width: None, left_context: None,
    }).unwrap();
    let Response::SnapshotResult { baseline, .. } = snapshot else { panic!("expected snapshot"); };
    for include_prefix_candidates in [false, true] {
        let request = ClauseCandidatesRequest {
            key: ClauseRequestKey {
                identity: SnapshotIdentity { composition: 1, revision: 1, configuration_generation: 1, connection_generation: 1 },
                baseline, conversion_revision: 0, clause_id: ClauseId(1),
                request_id: if include_prefix_candidates { 3 } else { 2 },
            },
            reading: "がぞうのように".into(), reading_start: ReadingPosition(0), reading_end: ReadingPosition(7),
            preceding_surfaces: vec![], include_prefix_candidates,
        };
        let response = client.request(&Request::ClauseCandidates(request.clone())).unwrap();
        let Response::ClauseCandidatesResult { key, status: ClauseCandidatesStatus::Ready { candidates } } = response else {
            panic!("expected candidates, got {response:?}");
        };
        assert_eq!(key, request.key);
        request.validate_candidates(&candidates).unwrap();
        assert!(candidates.iter().any(|c| c.surface == "画像のように" && c.reading_end == ReadingPosition(7)));
        assert_eq!(candidates.iter().any(|c| c.surface == "画像の" && c.reading_end == ReadingPosition(4)), include_prefix_candidates);
    }
    client.request(&Request::EndSession { session }).unwrap();
}

#[test]
#[ignore = "requires a built Swift engine"]
fn live_search_width_over_unique_pipe() {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let engine = IsolatedEngine::stage();
    let pipe = isolated_pipe("live-search-width");
    let _child = start_engine(Command::new(engine.exe()).arg(&pipe)
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .env("NOSPACEKEY_ZENZAI", "off")
        .env("NOSPACEKEY_LEARNING", "0")
        .env("NOSPACEKEY_MEMORY_DIR", engine.root.join("memory"))
        .env("TEMP", &engine.root).env("TMP", &engine.root)
        .stdout(Stdio::null()).stderr(Stdio::null()));
    let mut client = EngineClient::connect_to(&pipe, Duration::from_secs(5)).unwrap();
    let session = ipc::client::verify_session_identity(
        client.request(&Request::StartSession).unwrap()).unwrap();
    for explicit in [false, true] {
        for (index, width) in [None, Some(10), Some(1)].into_iter().enumerate() {
            let result = client.request(&Request::LiveSnapshot {
                composition: 1, revision: index as u64 + 1,
                configuration_generation: 1, connection_generation: 1,
                conversion_revision: 0, request_id: index as u64 + 1,
                segments: vec![ipc::protocol::SnapshotSegment {
                    text: "すこーぷがいなので".into(), style: Some("direct".into()),
                }],
                explicit, live_search_width: width, left_context: None,
            }).unwrap();
            let Response::SnapshotResult { text, candidates, .. } = result else {
                panic!("expected snapshot, got {result:?}");
            };
            let expected = if explicit || width == Some(10) { "スコープ外なので" } else { "スコープ買いなので" };
            assert_eq!(text, expected, "explicit={explicit} width={width:?}");
            assert_eq!(candidates.is_some(), explicit);
        }
    }
    client.request(&Request::EndSession { session }).unwrap();
}

/// Run in one client process across an engine replacement. An optional fixture
/// directory lets this exercise two real product builds with the same protocol.
#[test]
#[ignore = "requires a built Swift engine"]
fn compatible_engine_update_reconnects_and_converts() {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let previous = std::env::var_os("NOSPACEKEY_COMPAT_PREVIOUS_ENGINE_DIR");
    let old = match &previous {
        Some(directory) => IsolatedEngine::stage_from(std::path::Path::new(directory)),
        None => IsolatedEngine::stage(),
    };
    let new = IsolatedEngine::stage();
    // A synthetic session keeps the production endpoint out of this test.
    let pipe = ipc::client::pipe_name_for_session(1_000_000 + std::process::id());
    let mut epochs = Vec::new();
    let mut builds = Vec::new();
    for engine in [&old, &new] {
        let child = start_engine(Command::new(engine.exe()).arg(&pipe)
            .env("NOSPACEKEY_MEMORY_DIR", engine.root.join("memory"))
            .env("NOSPACEKEY_LEARNING", "0")
            .env("NOSPACEKEY_ZENZAI", "off")
            .env("TEMP", &engine.root)
            .env("TMP", &engine.root)
            .stdout(Stdio::null()).stderr(Stdio::null()));
        let mut client = EngineClient::connect_to(&pipe, Duration::from_secs(5)).unwrap();
        let response = client.request(&Request::StartSession).unwrap();
        if let Response::Session { ref boot, .. } = response {
            builds.push(boot.clone());
        }
        let (session, metadata) = ipc::client::verify_session_metadata(response).unwrap();
        epochs.push(metadata.engine_epoch);
        for request in [
            Request::Insert { session, text: "nihongo".into(), style: None },
            Request::Convert { session, left_context: None },
            Request::LiveConvert { session, seq: 1, left_context: None, auto_commit: false },
            Request::EndSession { session },
        ] {
            let result = client.request_within(&request, Instant::now() + Duration::from_secs(5)).unwrap();
            match request {
                Request::Insert { .. } => assert!(matches!(result, Response::Reading { reading } if reading == "にほんご")),
                Request::Convert { .. } => assert!(matches!(result, Response::Candidates { candidates } if candidates.iter().any(|value| value == "日本語"))),
                Request::LiveConvert { .. } => assert!(matches!(result, Response::LiveResult { text, .. } if text == "日本語")),
                _ => assert!(matches!(result, Response::Ok)),
            }
        }
        drop(client);
        drop(child);
    }
    assert_ne!(epochs[0], epochs[1], "replacement must have a new engine epoch");
    if previous.is_some() {
        assert_ne!(builds[0], builds[1], "fixture must be a different product version");
    }
}

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
        Self::stage_from(&source)
    }

    fn stage_from(source: &std::path::Path) -> Self {
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
