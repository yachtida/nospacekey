//! EngineHost を一意な一時layoutへstageして起動する結合テスト。CI では `#[ignore]`。
//!
//! 実行: `cargo test -p ipc --test integration tip_like -- --ignored --nocapture`

use ipc::client::EngineClient;
use ipc::protocol::{Request, Response};
use std::time::Duration;

#[test]
#[ignore = "requires a built Swift engine"]
fn production_connection_authenticates_a_staged_sibling_engine() {
    if let Some(pipe) = std::env::var_os("NOSPACEKEY_IDENTITY_TEST_PIPE") {
        let mut client = EngineClient::connect_to(pipe.to_str().unwrap(), Duration::from_secs(5)).unwrap();
        let session = ipc::client::verify_session_identity(client.request(&Request::StartSession).unwrap()).unwrap();
        assert!(matches!(client.request(&Request::EndSession { session }).unwrap(), Response::Ok));
        return;
    }
    run_production_identity_test(None);
}

#[test]
#[ignore = "requires a built Swift engine and Python 3; creates disposable AppContainer profiles"]
fn sandboxed_production_connection_authenticates_a_staged_sibling_engine() {
    for lpac in [false, true] {
        run_production_identity_test(Some(lpac));
    }
}

fn run_production_identity_test(sandbox: Option<bool>) {
    use std::process::{Command, Stdio};
    use std::os::windows::process::CommandExt;
    let engine = IsolatedEngine::stage();
    let pipe = ipc::client::pipe_name_for_session(2_000_000 + std::process::id());
    let _child = start_engine(Command::new(engine.exe()).arg(&pipe)
        .creation_flags(0x08000000)
        .env("NOSPACEKEY_ZENZAI", "off").env("NOSPACEKEY_LEARNING", "0")
        .env("NOSPACEKEY_MEMORY_DIR", engine.root.join("memory"))
        .env("TEMP", &engine.root).env("TMP", &engine.root)
        .stdout(Stdio::null()).stderr(Stdio::null()));
    let client_exe = engine.root.join("IdentityTestClient.exe");
    std::fs::copy(std::env::current_exe().unwrap(), &client_exe).unwrap();
    let mut command = if let Some(lpac) = sandbox {
        let mut command = Command::new("python");
        command.arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/run-appcontainer-client.py")).arg(&client_exe);
        if lpac { command.arg("--lpac"); }
        command
    } else {
        let mut command = Command::new(client_exe);
        command.args(["--ignored", "--exact", "production_connection_authenticates_a_staged_sibling_engine", "--nocapture"]);
        command
    };
    let output = command
        .creation_flags(0x08000000)
        .env("NOSPACEKEY_IDENTITY_TEST_PIPE", &pipe).output().unwrap();
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

#[test]
#[ignore = "requires a built Swift engine"]
fn snapshot_deadline_over_unique_pipe() {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let engine = IsolatedEngine::stage();
    let pipe = isolated_pipe("snapshot-deadline");
    let _child = start_engine(Command::new(engine.exe()).arg(&pipe).arg("--persist")
        .creation_flags(0x08000000)
        .env("NOSPACEKEY_ZENZAI", "off").env("NOSPACEKEY_LEARNING", "0")
        .env("NOSPACEKEY_MEMORY_DIR", engine.root.join("memory"))
        .env("TEMP", &engine.root).env("TMP", &engine.root)
        .stdout(Stdio::null()).stderr(Stdio::null()));
    let mut client = EngineClient::connect_to(&pipe, Duration::from_secs(5)).unwrap();
    let snapshot = Request::LiveSnapshot {
        include_flat_candidates: false,
        composition: 1, revision: 1, configuration_generation: 1, connection_generation: 1,
        conversion_revision: 0, request_id: 1,
        segments: vec![ipc::protocol::SnapshotSegment { text: "あ".into(), style: Some("direct".into()) }],
        explicit: true, live_search_width: None, left_context: None,
    };
    assert!(matches!(client.request_within(&snapshot,
        std::time::Instant::now() + Duration::from_millis(1_200)).unwrap(), Response::SnapshotResult { .. }));

    let mut raw = std::fs::OpenOptions::new().read(true).write(true).open(&pipe).unwrap();
    let mut expired = serde_json::to_value(&snapshot).unwrap();
    expired["deadline_tick_ms"] = 0.into();
    ipc::framing::write_request_frame(&mut raw, &expired).unwrap();
    let response: Response = ipc::framing::read_frame(&mut raw).unwrap();
    assert!(matches!(response, Response::Error { message } if message.contains("expired")));
    // Expiry rejects only that calculation; the same connection still handles legacy frames.
    ipc::framing::write_request_frame(&mut raw, &snapshot).unwrap();
    let response: Response = ipc::framing::read_frame(&mut raw).unwrap();
    assert!(matches!(response, Response::SnapshotResult { .. }));
}

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
        include_flat_candidates: false,
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
fn mixed_menu_ordinary_candidate_receipts_survive_worker_session_end() {
    use ipc::clause::*;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let engine = IsolatedEngine::stage();
    let pipe = isolated_pipe("mixed-ordinary-receipt");
    let _child = start_engine(Command::new(engine.exe()).arg(&pipe).arg("--persist")
        .creation_flags(0x08000000).env("NOSPACEKEY_ZENZAI", "off")
        .env("NOSPACEKEY_LEARNING", "1").env("NOSPACEKEY_MEMORY_DIR", engine.root.join("memory"))
        .env("TEMP", &engine.root).env("TMP", &engine.root)
        .stdout(Stdio::null()).stderr(Stdio::null()));
    let mut worker = EngineClient::connect_to(&pipe, Duration::from_secs(5)).unwrap();
    let (session, learning) = ipc::client::verify_session_metadata(worker.request(&Request::StartSession).unwrap()).unwrap();
    let Response::SnapshotResult { baseline, clause_data, candidates: Some(texts), candidate_remaining: Some(remaining), .. } = worker.request(&Request::LiveSnapshot {
        include_flat_candidates: true,
        composition: 1, revision: 1, configuration_generation: 1, connection_generation: 1,
        conversion_revision: 0, request_id: 1,
        segments: vec![ipc::protocol::SnapshotSegment { text: "がぞうのように".into(), style: Some("direct".into()) }],
        explicit: true, live_search_width: None, left_context: None,
    }).unwrap() else { panic!("expected explicit snapshot"); };
    let request = ClauseCandidatesRequest {
        key: ClauseRequestKey { identity: SnapshotIdentity { composition: 1, revision: 1, configuration_generation: 1, connection_generation: 1 },
            baseline, conversion_revision: clause_data.conversion_revision, clause_id: clause_data.clauses[0].id, request_id: 1 },
        reading: clause_data.reading.clone(), reading_start: ReadingPosition(0), reading_end: ReadingPosition(7),
        preceding_surfaces: vec![], include_prefix_candidates: true,
    };
    let candidates = clause_data.flat_candidates.as_ref().expect("snapshot carries its own flat candidate tokens");
    request.validate_candidates(candidates).unwrap();
    let selected = texts.iter().zip(&remaining).filter_map(|(surface, tail)| {
        let consumed = clause_data.reading.strip_suffix(tail)?;
        let candidate = candidates.iter().find(|candidate| candidate.surface == *surface && candidate.reading_end.0 == consumed.chars().count() as u32)?;
        Some((candidate.clone(), consumed.to_string()))
    }).collect::<Vec<_>>();
    assert_eq!(selected.len(), texts.len(), "all ordinary snapshot choices have a matching learning token");
    assert!(selected.iter().any(|(c, _)| c.reading_end.0 == 7));
    assert!(selected.iter().any(|(c, _)| c.reading_end.0 < 7));
    worker.request(&Request::EndSession { session }).unwrap();
    drop(worker);
    let mut outbox = EngineClient::connect_to(&pipe, Duration::from_secs(5)).unwrap();
    for (index, (candidate, consumed)) in selected.iter().enumerate() {
        let receipt = CommitReceipt {
            commit_id: CommitId { client_instance: "550e8400-e29b-41d4-a716-446655440000".into(), sequence: index as u64 + 1 },
            engine_epoch: learning.engine_epoch.clone(), learning_generation: learning.learning_generation,
            reading: consumed.clone(), text: candidate.surface.clone(),
            sentence_token: (candidate.reading_end.0 == 7).then(|| candidate.token.clone()),
            intervals: vec![CommitInterval { reading_start: ReadingPosition(0), reading_end: candidate.reading_end,
                surface: candidate.surface.clone(), learning: IntervalLearning::Candidate { token: candidate.token.clone(), explicitly_selected: true } }],
        };
        receipt.validate(|token, _| token == candidate.token).unwrap();
        let response = outbox.request(&Request::CommitReceipt(receipt)).unwrap();
        assert!(matches!(response, Response::CommitReceiptAck { status: ReceiptStatus::Applied, .. }), "{response:?}");
    }
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
                include_flat_candidates: false,
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
        let mut client = EngineClient::connect_to_engine_at(&pipe, Duration::from_secs(5), engine.exe()).unwrap();
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

/// 混在変換（ADR-0007 / 実装計画 §6.1）: 固定 Plan を MixedConvert へ渡し、
/// Literal の原文保持・日本語区間の変換と token 発行・確定 receipt の学習分離を
/// 実エンジン（古典変換。NOSPACEKEY_ZENZAI は engine 起動時の既定）で検証する。
/// 実行: cargo test -p ipc --test integration mixed_convert -- --ignored --nocapture
#[test]
#[ignore]
fn mixed_convert_keeps_literal_and_converts_japanese_over_unique_pipe() {
    use std::process::Command;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    let engine = IsolatedEngine::stage();
    let exe = engine.exe();
    let pipe = isolated_pipe("mixed");
    let _child = start_engine(Command::new(&exe).arg(&pipe).arg("--persist")
        .creation_flags(0x08000000)
        .env("NOSPACEKEY_ZENZAI", "off").env("NOSPACEKEY_LEARNING", "0")
        .env("NOSPACEKEY_MEMORY_DIR", engine.root.join("memory"))
        .env("TEMP", &engine.root).env("TMP", &engine.root)
        .stdout(Stdio::null()).stderr(Stdio::null()));
    let mut c = match EngineClient::connect_to(&pipe, Duration::from_secs(5)) {
        Ok(c) => c,
        Err(e) => panic!("connect_to({pipe}) failed: {e}"),
    };

    // StartSession: 新エンジンは capability 広告を出す（mixed_input_v1）。
    let (sid, engine_epoch, learning_generation) = match c.request(&Request::StartSession).unwrap() {
        Response::Session { session, engine_epoch, learning_generation, capabilities: Some(list), .. }
            if list.iter().any(|capability| capability == "mixed_input_v1") =>
        {
            (session, engine_epoch, learning_generation)
        }
        other => panic!("expected capable Session, got {:?}", other),
    };

    let spans = vec![
        ipc::protocol::MixedSpan {
            kind: "japanese".into(),
            reading_start: 0,
            reading_end: 4,
            text: "きょうは".into(),
        },
        ipc::protocol::MixedSpan {
            kind: "literal".into(),
            reading_start: 4,
            reading_end: 10,
            text: "Python".into(),
        },
        ipc::protocol::MixedSpan {
            kind: "japanese".into(),
            reading_start: 10,
            reading_end: 14,
            text: "にほんご".into(),
        },
    ];
    let request = Request::MixedConvert {
        session: sid,
        composition: 8,
        revision: 13,
        configuration_generation: 2,
        connection_generation: 5,
        conversion_revision: 0,
        request_id: 4,
        source_revision: 9,
        plan_id: 3,
        spans: spans.clone(),
        left_context: None,
    };
    let (text, result_spans) = match c.request_within(&request,
        std::time::Instant::now() + Duration::from_millis(1_200)).unwrap() {
        Response::MixedResult { text, spans, .. } => (text, spans),
        other => panic!("expected MixedResult, got {:?}", other),
    };
    assert_eq!(result_spans.len(), 3);
    // Literal は原文を一字不動で保持し、学習 token を持たない。
    assert_eq!(result_spans[1].text, "Python");
    assert_eq!(result_spans[1].candidate_token, None);
    // 日本語区間は空でなく token を発行する。
    assert!(!result_spans[0].text.is_empty());
    let first_token = result_spans[0].candidate_token.clone().expect("japanese token");
    assert!(result_spans[2].candidate_token.is_some());
    // 全体テキストは span 表示の連結。
    assert_eq!(text, result_spans.iter().map(|span| span.text.as_str()).collect::<String>());

    // 確定 receipt: 日本語区間だけ Candidate token で学習し、Literal は学習対象外。
    let receipt = ipc::clause::CommitReceipt {
        commit_id: ipc::clause::CommitId {
            // engine は client_instance を UUID として検証する（UUID でなければ
            // InvalidIntervals で拒否）。
            client_instance: "11111111-1111-4111-8111-111111111111".into(),
            sequence: 1,
        },
        engine_epoch,
        learning_generation,
        reading: "きょうはPythonにほんご".into(),
        text: text.clone(),
        intervals: result_spans
            .iter()
            .map(|span| ipc::clause::CommitInterval {
                reading_start: ipc::clause::ReadingPosition(span.reading_start),
                reading_end: ipc::clause::ReadingPosition(span.reading_end),
                surface: span.text.clone(),
                learning: if span.kind == "literal" {
                    ipc::clause::IntervalLearning::None {
                        reason: ipc::clause::NoLearningReason::NotLearningTarget,
                    }
                } else {
                    ipc::clause::IntervalLearning::Candidate {
                        token: span.candidate_token.clone().unwrap(),
                        explicitly_selected: true,
                    }
                },
            })
            .collect(),
        sentence_token: None,
    };
    receipt.validate(|token, interval| {
        spans_are_consistent(token, interval, &result_spans)
    })
    .expect("receipt keeps its invariants");
    let _ = first_token;
    match c.request_within(&Request::CommitReceipt(receipt),
        std::time::Instant::now() + Duration::from_millis(1_200)).unwrap() {
        Response::CommitReceiptAck { status: ipc::clause::ReceiptStatus::Applied, .. } => {}
        other => panic!("expected applied receipt ack, got {:?}", other),
    }
}

fn spans_are_consistent(
    token: &str,
    interval: &ipc::clause::CommitInterval,
    spans: &[ipc::protocol::MixedSpanResult],
) -> bool {
    spans.iter().any(|span| {
        span.kind == "japanese"
            && span.candidate_token.as_deref() == Some(token)
            && span.reading_start == interval.reading_start.0
            && span.reading_end == interval.reading_end.0
    })
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
    // Stage an explicitly chosen release build into the same isolated test layout.
    if let Some(path) = std::env::var_os("NOSPACEKEY_TEST_ENGINE_DIR") {
        return std::path::PathBuf::from(path);
    }
    // CARGO_MANIFEST_DIR = <workspace>/crates/ipc
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join(r"engine-host\.build\x86_64-unknown-windows-msvc\debug")
}
