fn main() {
    // エクスポートは lib.rs の `#[no_mangle] extern "system"` 関数（DllGetClassObject /
    // DllCanUnloadNow / DllRegisterServer / DllUnregisterServer）を rustc が cdylib から
    // 自動エクスポートするため、.def ファイルは不要。
    //
    // 以前は `/DEF:nospacekey_tip.def` を渡していたが、(1) リンカの作業ディレクトリは
    // クレートルートではないため相対パスが解決できず LNK1104 になる、(2) rustc が
    // 生成する lib.def と二重指定になる、ため撤去した。

    // 品質ループ①: ビルド識別子（git short hash）を rustc-env 化する。tip_log の
    // ev=log_open build=<ver>-<githash> が「どのビルドのログか」を特定できるように。
    // git コマンド失敗（クリーン VM の tarball ビルド等 .git 不在）は "unknown" —
    // ビルド自体は決して壊さない。
    let git_hash = git_short_hash_or_unknown();
    println!("cargo:rustc-env=GIT_HASH={git_hash}");
    // F-2: rerun-if-changed を一度でも出すと cargo は既定の「常に再実行」をやめるため、
    // watch 対象の選定が GIT_HASH の鮮度を決める。
    // - .git/HEAD は**ブランチ切替**でしか変わらない（同一ブランチ上のコミットでは
    //   refs/heads/<branch> が変わるだけ）ので、これ単独だと通常の「コミット→リビルド」で
    //   GIT_HASH が陳腐化し、log_open が別ビルドの hash を出す。
    // - .git/logs/HEAD（reflog）は commit / checkout / reset のたびに追記されるので、
    //   これを併せて watch して毎コミットで build.rs を再実行させる（best-effort。
    //   .git 不在の tarball ビルドではファイル欠如で毎回 rerun になるが軽量なので許容）。
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");

    // VERSIONINFO(②): エクスプローラのプロパティ/サポート報告で「どのビルドか」を
    // 特定可能にする。FileVersion/ProductName 等は winresource が CARGO_PKG_* から
    // 自動設定する。文字列にブランド名リテラルを焼かないのは命名未決定のため
    // （改名時にここを触らずに済ませる — 表示名は installer 側の MyAppName 1 箇所に集約）。
    #[cfg(windows)]
    {
        let ver = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
        let name = std::env::var("CARGO_PKG_NAME").expect("CARGO_PKG_NAME");
        let mut res = winresource::WindowsResource::new();
        res.set("ProductVersion", &format!("{ver}+{git_hash}"));
        res.set("FileDescription", &format!("{name} (TSF text input processor)"));
        res.compile()
            .expect("VERSIONINFO embed failed (check rc.exe / Windows SDK on PATH)");
    }
}

/// `git rev-parse --short HEAD` の結果。失敗（git 不在 / .git 不在 / 非 UTF-8）は "unknown"。
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
