use std::fs;
use std::path::Path;

fn read(rel: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)).unwrap()
}

#[test]
fn settings_identifies_inline_prediction_as_alpha() {
    let page = read("frontend/src/pages/EnginePage.tsx");

    assert!(
        page.contains("アルファ版") && page.contains("<h2>インライン予測</h2>"),
        "インライン予測の見出しにアルファ版表示が必要"
    );
    assert!(
        page.contains("Zenzaiとは独立した専用モデル"),
        "アルファ版の独立したモデル状態を説明する必要がある"
    );
}
