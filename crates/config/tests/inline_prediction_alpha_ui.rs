use std::fs;
use std::path::Path;

fn read(rel: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)).unwrap()
}

#[test]
fn settings_identifies_inline_prediction_as_alpha() {
    let html = read("ui/index.html");

    assert!(
        html.contains("<h2>インライン予測（アルファ版）</h2>"),
        "インライン予測の見出しにアルファ版表示が必要"
    );
    assert!(
        html.contains("アルファ版機能です。動作や仕様は今後変更される可能性があります。"),
        "アルファ版であることの利用者向け説明が必要"
    );
}
