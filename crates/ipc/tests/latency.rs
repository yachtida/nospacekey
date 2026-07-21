//! 毎打鍵 convert のレイテンシ実測（Phase 0 計測スパイク）。
//! エンジン host を一意パイプに自分で起動し、1文字ずつ Insert→Convert して往復時間を測る。
//! 往復(roundtrip)＝IPC往復＋推論。推論分はエンジンの `ev=infer ms=` ログ（%TEMP%\nospacekey-engine.log
//! か stderr）と突き合わせて IPC オーバーヘッドを切り分ける。
//!
//! 実行: cargo test -p ipc --test latency -- --ignored --nocapture
//!   古典: そのまま実行（重み無し）
//!   Zenzai: $env:NOSPACEKEY_ZENZAI_WEIGHT に gguf パスを設定してから実行

use ipc::client::EngineClient;
use ipc::protocol::{Request, Response};
use std::time::{Duration, Instant};

#[test]
#[ignore]
fn per_keystroke_convert_latency() {
    let exe = engine_exe_path();
    assert!(exe.exists(), "engine exe not built: {}", exe.display());
    let pipe = format!(r"\\.\pipe\nospacekey-engine-bench-{}", std::process::id());
    // drop 時に kill+wait してエンジンを確実に回収する（zombie 化防止）。
    let _child = ChildGuard(std::process::Command::new(&exe).arg(&pipe).spawn().expect("spawn engine"));
    let mut c = match EngineClient::connect_to(&pipe, Duration::from_secs(10)) {
        Ok(c) => c,
        Err(e) => panic!("connect_to({pipe}) failed: {e}"),
    };

    let sid = match c.request(&Request::StartSession).unwrap() {
        Response::Session { session, .. } => session,
        other => panic!("expected Session, got {:?}", other),
    };

    // 代表文（ローマ字）。TIP と同じく1文字ずつ Insert し、各打鍵後に Convert する。
    let roman = "nihongowonyuuryokusuru";
    let mut samples: Vec<u128> = Vec::new();
    for ch in roman.chars() {
        c.request(&Request::Insert { session: sid, text: ch.to_string(), style: None }).unwrap();
        let t0 = Instant::now();
        let _ = c.request(&Request::Convert { session: sid, left_context: None }).unwrap();
        let us = t0.elapsed().as_micros();
        samples.push(us);
        println!("ev=bench len={} roundtrip_us={}", samples.len(), us);
    }
    let _ = c.request(&Request::EndSession { session: sid });

    samples.sort_unstable();
    let pct = |p: f64| samples[((samples.len() as f64 - 1.0) * p).round() as usize];
    println!(
        "ev=bench_summary n={} p50_us={} p95_us={} max_us={}",
        samples.len(), pct(0.50), pct(0.95), *samples.last().unwrap()
    );
}

/// 子プロセスを drop 時に kill+wait して zombie 化を防ぐガード。
struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// ワークスペース直下の engine-host ビルド済み exe（debug）。
fn engine_exe_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors().nth(2).expect("workspace root")
        .join(r"engine-host\.build\x86_64-unknown-windows-msvc\debug\NospacekeyEngineHost.exe")
}
