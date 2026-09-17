fn main() {
    verify_frontend_assets();
    // 品質ループ①: config 側にも git short hash を rustc-env 化し、情報画面に
    // `<version> (<hash>)` を出せるようにする（crates/tip/build.rs:14,23-24 と同一方式・出典）。
    // .git/HEAD 単独では同一ブランチのコミットで陳腐化するため reflog も watch する（tip の F-2）。
    println!("cargo:rustc-env=GIT_HASH={}", git_short_hash_or_unknown());
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    tauri_build::build();
}

fn verify_frontend_assets() {
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    let frontend = PathBuf::from("frontend");
    let stamp = frontend.join("dist/.source-hash");
    let index = frontend.join("dist/index.html");
    if !index.is_file() || !stamp.is_file() {
        panic!("config UI assets are missing; run scripts/build-config-ui.ps1 before cargo build -p config");
    }
    let expected: BTreeMap<String, String> = std::fs::read_to_string(&stamp)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once('|'))
        .map(|(path, hash)| (path.to_owned(), hash.to_owned()))
        .collect();
    let mut inputs = Vec::new();
    collect_files(&frontend, &frontend, &mut inputs);
    inputs.sort();
    let actual: BTreeMap<String, String> = inputs
        .into_iter()
        .map(|path| {
            println!("cargo:rerun-if-changed={}", path.display());
            let relative = path
                .strip_prefix(&frontend)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(&path).unwrap_or_default();
            (relative, format!("{:x}", Sha256::digest(bytes)))
        })
        .collect();
    if actual != expected {
        panic!("config UI assets are stale; run scripts/build-config-ui.ps1 before cargo build -p config");
    }

    fn collect_files(root: &Path, directory: &Path, output: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(&path);
            let first = relative
                .components()
                .next()
                .and_then(|item| item.as_os_str().to_str());
            if matches!(first, Some("node_modules" | "dist")) {
                continue;
            }
            let generated = path
                .file_name()
                .and_then(|item| item.to_str())
                .is_some_and(|name| {
                    name.ends_with(".tsbuildinfo")
                        || matches!(name, "vite.config.js" | "vite.config.d.ts")
                });
            if generated {
                continue;
            }
            if path.is_dir() {
                collect_files(root, &path, output);
            } else if path.is_file() {
                output.push(path);
            }
        }
    }
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
