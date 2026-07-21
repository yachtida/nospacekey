fn main() {
    // 品質ループ①: config 側にも git short hash を rustc-env 化し、情報画面に
    // `<version> (<hash>)` を出せるようにする（crates/tip/build.rs:14,23-24 と同一方式・出典）。
    // .git/HEAD 単独では同一ブランチのコミットで陳腐化するため reflog も watch する（tip の F-2）。
    println!("cargo:rustc-env=GIT_HASH={}", git_short_hash_or_unknown());
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    tauri_build::build();
}

/// `git rev-parse --short HEAD`。失敗（git/.git 不在・非 UTF-8）は "unknown"（tip/build.rs と同じ）。
fn git_short_hash_or_unknown() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
