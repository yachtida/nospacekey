//! Direct Cargo builds must open the settings UI without a Vite server.
//! Use the same generated context as main so this checks embedded assets,
//! rather than merely checking that frontend/dist exists on disk.

#[test]
fn direct_cargo_build_uses_bundled_frontend() {
    assert!(
        !tauri::is_dev(),
        "standalone config builds must enable tauri/custom-protocol; otherwise they open localhost:5173"
    );
}

#[test]
fn generated_context_contains_the_frontend_and_its_entry_assets() {
    let context: tauri::Context<tauri::Wry> = tauri::generate_context!();
    let assets = context.assets();
    let index = assets
        .get(&"index.html".into())
        .expect("settings index.html must be embedded even without a development server");
    let html = std::str::from_utf8(&index).expect("settings HTML must be UTF-8");
    assert!(html.contains("<title>nospacekey 設定</title>"));

    // Vite emits hashed filenames. Check the actual entry references instead
    // of hard-coding a hash that changes with every UI build.
    for (attribute, extension) in [("src=\"", ".js"), ("href=\"", ".css")] {
        let paths: Vec<_> = html
            .split(attribute)
            .skip(1)
            .filter_map(|part| part.split('"').next())
            .filter(|path| path.ends_with(extension))
            .collect();
        assert!(
            !paths.is_empty(),
            "missing {extension} entry in settings HTML"
        );
        for path in paths {
            assert!(
                assets.get(&path.trim_start_matches('/').into()).is_some(),
                "settings entry asset must be embedded: {path}"
            );
        }
    }
}
