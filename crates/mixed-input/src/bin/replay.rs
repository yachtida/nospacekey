//! 契約 fixture の検証・再生 CLI（計画書 10 の PR1 成果物）。
//!
//! ```text
//! mixed-input-replay --all crates/mixed-input/fixtures
//! mixed-input-replay path/to/a.toml path/to/b.toml
//! ```
//!
//! 終了コード: 0 = 全件有効 / 1 = 違反あり / 2 = 使い方誤り・読込失敗。

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use mixed_input::episode::{self, Episode};
use mixed_input::validation;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--all" => {
                let Some(dir) = args.get(index + 1) else {
                    return usage();
                };
                let Ok(entries) = fs::read_dir(dir) else {
                    eprintln!("ディレクトリを読めない: {dir}");
                    return ExitCode::from(2);
                };
                let mut paths: Vec<PathBuf> = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext| ext == "toml")
                            .unwrap_or(false)
                    })
                    .collect();
                paths.sort();
                files.extend(paths);
                index += 1;
            }
            path => files.push(PathBuf::from(path)),
        }
        index += 1;
    }
    if files.is_empty() {
        return usage();
    }

    let mut episodes: Vec<Episode> = Vec::new();
    for file in &files {
        let text = match fs::read_to_string(file) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("{} を読めない: {error}", file.display());
                return ExitCode::from(2);
            }
        };
        match episode::parse_episodes(&text) {
            Ok(mut parsed) => episodes.append(&mut parsed),
            Err(error) => {
                eprintln!("{} の解析に失敗: {error}", file.display());
                return ExitCode::from(2);
            }
        }
    }

    let mut counts: Vec<(String, usize)> = Vec::new();
    for ep in &episodes {
        let name = ep.category.as_str().to_string();
        match counts
            .iter_mut()
            .find(|(entry_name, _)| *entry_name == name)
        {
            Some((_, count)) => *count += 1,
            None => counts.push((name, 1)),
        }
    }
    counts.sort();
    println!("エピソード合計: {}", episodes.len());
    for (name, count) in &counts {
        println!("  {name}: {count}");
    }

    let violations = validation::validate_corpus(&episodes);
    if violations.is_empty() {
        println!("違反なし。");
        ExitCode::SUCCESS
    } else {
        for violation in &violations {
            println!(
                "[{}] {}: {}",
                violation.rule, violation.episode_id, violation.message
            );
        }
        println!("違反 {} 件。", violations.len());
        ExitCode::FAILURE
    }
}

fn usage() -> ExitCode {
    eprintln!("使い方: mixed-input-replay --all <fixtures-dir> | <file.toml>...");
    ExitCode::from(2)
}
