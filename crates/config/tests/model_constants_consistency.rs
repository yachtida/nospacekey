// インストーラ(.iss)とアプリ(download.rs)のモデル定数(URL/SHA256/ファイル名)の
// 機械照合。単一ソースは download.rs — どちらか片方だけ更新するドリフトを RED にする
// (version_consistency.rs と同型のテキスト照合。bin crate のため API 参照はできない)。
use std::fs;
use std::path::Path;

fn read(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel); // crates/config 起点
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// `marker` 以降で最初に現れる二重引用符文字列を返す(改行を跨いで良い —
/// download.rs の MODEL_URL は rustfmt で値が次行に折られている)。
fn quoted_after<'a>(source: &'a str, marker: &str) -> &'a str {
    let at = source
        .find(marker)
        .unwrap_or_else(|| panic!("marker not found: {marker}"));
    let rest = &source[at + marker.len()..];
    let open = rest
        .find('"')
        .unwrap_or_else(|| panic!("no opening quote after {marker}"));
    let body = &rest[open + 1..];
    let close = body
        .find('"')
        .unwrap_or_else(|| panic!("no closing quote after {marker}"));
    &body[..close]
}

#[test]
fn installer_model_constants_match_download_rs() {
    let rs = read("src/download.rs");
    let iss = read("../../installer/nospacekey.iss");
    assert_eq!(
        quoted_after(&iss, "#define ModelFileName"),
        quoted_after(&rs, "const MODEL_FILENAME"),
        "モデルファイル名が .iss と download.rs で不一致"
    );
    assert_eq!(
        quoted_after(&iss, "#define ModelDownloadURL"),
        quoted_after(&rs, "const MODEL_URL"),
        "モデル URL が .iss と download.rs で不一致"
    );
    assert_eq!(
        quoted_after(&iss, "#define ModelSHA256"),
        quoted_after(&rs, "const MODEL_SHA256"),
        "モデル SHA256 が .iss と download.rs で不一致"
    );
}
