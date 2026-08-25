//! Inline prediction artifacts are consumed by three processes. Keep their fixed contract in sync.

use std::fs;
use std::path::Path;

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn assert_contains_all(label: &str, source: &str, values: &[&str]) {
    for value in values {
        assert!(source.contains(value), "{label} missing {value}");
    }
}

#[test]
fn fixed_artifact_values_reach_each_independent_consumer() {
    let downloader = read("src/prediction_download.rs");
    let tip = read("../tip/src/prediction_worker.rs");
    let engine = read("../../engine-host/Sources/NospacekeyEngineCore/PredictionService.swift");
    let run_gate = read("../../scripts/run-gate.ps1");
    let stage_dist = read("../../scripts/stage-dist.ps1");
    let receipt_values = [
        "llm-jp-3-150m-q8_0-c060ca9.gguf",
        "191f2fdf41a6f64f00ec6b4fcc39ec6164bb13b41d3609ac8f5b2b6149a23a6d",
        "955dc1fa623fab38cc92a3f4ee172423ae6d73201c4207569bfdf5626bc733f0",
        "schema=1\\n",
    ];
    assert_contains_all("downloader", &downloader, &receipt_values);
    assert_contains_all("TIP", &tip, &receipt_values);
    assert_contains_all("engine host", &engine, &receipt_values);
    assert_contains_all("downloader", &downloader, &["164_257_184", "6_416_433"]);
    assert_contains_all("TIP", &tip, &["164_257_184", "6_416_433"]);
    assert_contains_all(
        "Sandbox gate",
        &run_gate,
        &[
            "llm-jp-3-150m-q8_0-c060ca9.gguf",
            "Length = 164257184",
            "Length = 6416433",
            "191F2FDF41A6F64F00EC6B4FCC39EC6164BB13B41D3609AC8F5B2B6149A23A6D",
            "955DC1FA623FAB38CC92A3F4EE172423AE6D73201C4207569BFDF5626BC733F0",
        ],
    );

    let llama_revision = "c060ca974c773c7c3d17fd1b66dc9d312bc292c0";
    assert_contains_all("engine host", &engine, &[llama_revision]);
    assert_contains_all("Sandbox gate", &run_gate, &[llama_revision]);
    assert_contains_all("distribution staging", &stage_dist, &[llama_revision]);
}

#[test]
fn model_download_uses_the_official_repository_and_pinned_revision() {
    let downloader = read("src/prediction_download.rs");
    assert!(downloader.contains(
        "https://github.com/yachtida/nospacekey/releases/download/inline-prediction-model-v1/"
    ));
    assert!(downloader.contains("b112feef602fff752e4dac4c30af6a2c2fa41c7a/tokenizer.json"));
}

#[test]
fn redistributed_model_has_a_pinned_modification_notice() {
    let notice = read("../../THIRD-PARTY-NOTICES.md");
    for required in [
        "llm-jp-3-150m-q8_0-c060ca9.gguf",
        "b112feef602fff752e4dac4c30af6a2c2fa41c7a",
        "c060ca974c773c7c3d17fd1b66dc9d312bc292c0",
        "converted to GGUF and quantized to Q8_0",
        "No fine-tuning or additional training was performed",
    ] {
        assert!(notice.contains(required), "model notice missing {required}");
    }
}
