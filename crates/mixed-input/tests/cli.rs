//! 公開 CLI 名での起動確認。bin 名は manifest で mixed-input-replay に固定されて
//! いることを、CARGO_BIN_EXE の参照自体が保証する（名前が替わるとコンパイルが
//! 失敗する）。ライブラリの検証テストでは捉えられない CLI と fixture の接続を
//! ここで見る。

use std::path::Path;
use std::process::Command;

#[test]
fn cli_validates_all_fixtures_by_its_published_name() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let output = Command::new(env!("CARGO_BIN_EXE_mixed-input-replay"))
        .arg("--all")
        .arg(&fixtures)
        .output()
        .expect("mixed-input-replay を起動できる");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "CLI が失敗: status={:?} stdout={stdout}",
        output.status.code()
    );
    assert!(
        stdout.contains("違反なし"),
        "検証結果が出ていない: {stdout}"
    );
}
