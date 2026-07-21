// ⑤ 版整合の自動検証: Cargo(workspace) / tauri.conf.json / installer/version.iss /
// BuildInfo.swift が一致しないと fail。将来の release.ps1 はこのテストと
// scripts/sync-version.ps1 -Check をフェイルファストに使う。
use std::fs;
use std::path::Path;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
} // crates/config

fn read(rel: &str) -> String {
    fs::read_to_string(root().join(rel)).unwrap()
}

/// ルート Cargo.toml の [workspace.package] 節から version を素朴に抜く(toml crate 不要)。
fn workspace_version() -> String {
    let s = read("../../Cargo.toml");
    let sect = s
        .split("[workspace.package]")
        .nth(1)
        .expect("[workspace.package] 節がない");
    sect.lines()
        .take_while(|l| !l.trim_start().starts_with('[')) // 次の節見出しで走査を打ち切る
        .find_map(|l| {
            l.trim()
                .strip_prefix("version")
                .and_then(|r| r.split('"').nth(1))
                .map(str::to_string)
        })
        .expect("workspace.package.version がない")
}

#[test]
fn all_version_declarations_match_workspace_package() {
    let ws = workspace_version();
    // config crate 自身が workspace 継承している証明(= 全 crate の代表)
    assert_eq!(
        env!("CARGO_PKG_VERSION"),
        ws,
        "crate が version.workspace=true を継承していない"
    );
    // tauri.conf.json
    let conf: serde_json::Value = serde_json::from_str(&read("tauri.conf.json")).unwrap();
    assert_eq!(
        conf["version"].as_str().unwrap(),
        ws,
        "tauri.conf.json の version 不一致"
    );
    // installer/version.iss
    assert!(
        read("../../installer/version.iss")
            .contains(&format!("#define MyAppVersion \"{ws}\"")),
        "version.iss 不一致"
    );
    // engine-host BuildInfo.swift
    assert!(
        read("../../engine-host/Sources/NospacekeyEngineCore/BuildInfo.swift")
            .contains(&format!("version = \"{ws}\"")),
        "BuildInfo.swift 不一致"
    );
}

#[test]
fn nospacekey_iss_has_no_hardcoded_version() {
    // .iss 本体に版リテラルが復活しないこと(#include "version.iss" 経由のみ)
    let iss = read("../../installer/nospacekey.iss");
    assert!(iss.contains("#include \"version.iss\""));
    assert!(
        !iss.contains("#define MyAppVersion \""),
        "nospacekey.iss に版の直書きが復活している"
    );
}

#[test]
fn all_crates_inherit_workspace_version() {
    // env!(CARGO_PKG_VERSION) は config 1 crate の継承しか証明しない。
    // 6 crate 全部の Cargo.toml を読み、[package] 節(次の [ 行まで)に
    // version.workspace = true があることを assert — cargo test 単独で全所在が閉じる。
    // (sync-version.ps1 -Check の同種検査は release.ps1 用の冗長系)
    for c in ["tip", "ipc", "ids", "settings", "testbench", "config"] {
        let s = read(&format!("../{c}/Cargo.toml"));
        let pkg: Vec<&str> = s
            .split("[package]")
            .nth(1)
            .unwrap_or_else(|| panic!("{c}: [package] 節がない"))
            .lines()
            .take_while(|l| !l.trim_start().starts_with('[')) // 依存テーブルは対象外
            .collect();
        assert!(
            pkg.iter().any(|l| l.trim() == "version.workspace = true"),
            "{c}/Cargo.toml の [package] が version.workspace = true でない"
        );
        assert!(
            !pkg.iter().any(|l| l.trim_start().starts_with("version = \"")),
            "{c}/Cargo.toml の [package] に version 直書きが残っている"
        );
    }
}
