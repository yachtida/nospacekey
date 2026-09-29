//! Real TSF/UIElement acceptance for reading predictions, run only through run-gate.ps1.
use crate::tsf_host::{self, TsfHost};
use std::time::{Duration, Instant};
fn wait(mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        tsf_host::pump();
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}
fn wait_preview(host: &TsfHost, completion: &str) -> bool {
    // Stale candidates remain visible while the next revision is in flight.
    wait(|| host.candidate_strings().iter().any(|c| c == completion) && host.claims_key(0x09))
}
fn type_reading(host: &TsfHost, engine: &str) -> bool {
    crate::scenarios::typed(if engine != "azookey" { "sankou" } else { "gazo" })
        .into_iter()
        .all(|key| host.feed_key(key.0))
}
fn resume_reading(host: &TsfHost, engine: &str) -> bool {
    // A fresh Sandbox supplies homophones through the prediction API, without the
    // host profile's extended completions. This checks UI interaction; extended
    // predictions are separately required by the real-host IPC integration test.
    // Replace the final roman key to exercise a fresh revision after dismissal.
    (engine == "azookey" || host.feed_key(0x08)) && host.feed_key(0x55)
}
fn check_case(dir: &std::path::Path, live: bool, enabled: bool, engine: &str) -> Result<(), String> {
    let reading = if engine != "azookey" { "さんこう" } else { "がぞ" };
    let completion = if engine != "azookey" { "参考" } else { "画像" };
    // After gazo -> gazou, also require an extended candidate from the new revision.
    let resumed_completion = if engine != "azookey" { "参考" } else { "画像を" };
    std::fs::write(dir.join("settings.json"), serde_json::json!({
        "version": 2, "conversion_engine": engine, "live_conversion": {"enabled": live},
        "input_prediction_enabled": enabled, "zenzai": {"enabled": false},
        "learning": {"enabled": false}, "feedback": {"enabled":true}, "keymap":{"feedback":"Ctrl+KeyJ"}
    }).to_string()).map_err(|e| e.to_string())?;
    eprintln!("prediction acceptance: start live={live} enabled={enabled}");
    let host = TsfHost::start().map_err(|e| format!("start: {e}"))?;
    if !host.normalize_native_mode() {
        return Err("native mode".into());
    }
    eprintln!("prediction acceptance: warmup");
    host.warm_up();
    host.store.reset();
    for vk in [0x1C, 0xBF, 0x4A] {
        if host.feed_key_with_ctrl(vk) {
            return Err("removed feedback key was claimed".into());
        }
    }
    eprintln!("prediction acceptance: typing");
    if !type_reading(&host, engine) {
        return Err("reading keys not eaten".into());
    }
    if !enabled {
        let _ = crate::driver::run_keys(&host, &[crate::scenarios::WAIT_CONVERSION]);
        if !host.candidate_strings().is_empty() || host.feed_key(0x09) {
            return Err("disabled preview claimed Tab".into());
        }
        return Ok(());
    }
    if !wait_preview(&host, completion) {
        return Err(format!(
            "missing {completion}: {:?}, preedit={:?}",
            host.candidate_strings(),
            host.store.preedit()
        ));
    }
    if !host.store.committed().is_empty() || (!live && host.store.preedit() != reading) {
        return Err("preview changed reading/committed text".into());
    }
    let before = host.store.preedit();
    if !host.behavior_select(1) || host.store.preedit() != before {
        return Err("preview selection notification changed text".into());
    }
    let choices = host.candidate_strings();
    if !host.feed_key(0x09) || host.store.preedit() != choices[0] {
        return Err("Tab did not enter selection".into());
    }
    if choices.len() > 1 && (!host.feed_key(0x28) || host.store.preedit() != choices[1]) {
        return Err("Down did not select second completion".into());
    }
    let selected = host.store.preedit();
    if !host.feed_key(0x0D)
        || !wait(|| !host.store.composing())
        || host.store.committed() != selected
    {
        return Err(format!(
            "completion commit: {:?} wanted {selected}",
            host.store.full()
        ));
    }
    host.store.reset();
    if !type_reading(&host, engine) || !wait_preview(&host, completion) {
        return Err("second preview".into());
    }
    let before = host.store.preedit();
    if !host.feed_key(0x1B)
        || host.store.preedit() != before
        || !host.store.committed().is_empty()
        || !host.candidate_strings().is_empty()
    {
        return Err("Esc must close preview and retain composition".into());
    }
    if host.feed_key(0x09) {
        return Err("dismissed preview claimed Tab".into());
    }
    if !resume_reading(&host, engine) || !wait_preview(&host, resumed_completion) {
        return Err("reading change did not reopen preview".into());
    }
    let choices = host.candidate_strings();
    let index = choices
        .iter()
        .position(|c| c == resumed_completion)
        .ok_or("updated preview lacks completion")?;
    if !host.behavior_select_and_finalize(index as u32)
        || !wait(|| !host.store.composing())
        || host.store.committed() != resumed_completion
    {
        return Err("click/Behavior completion commit".into());
    }
    host.store.reset();
    if !type_reading(&host, engine) || !wait_preview(&host, completion) {
        return Err("numeric preview".into());
    }
    if !host.feed_key(0x31)
        || !host.store.committed().is_empty()
        || !host.feed_key(0x08)
        || !wait_preview(&host, completion)
    {
        return Err("unselected digit/Backspace changed or committed the reading".into());
    }
    // Retained stale rows can already contain 画像 after Backspace. Wait for Tab
    // acceptance (fresh identity), rather than mistaking those rows for a reply.
    if !wait(|| host.feed_key(0x09))
        || !host.feed_key(0x08)
        || host.store.preedit() != reading
        || !host.store.committed().is_empty()
    {
        return Err("selected Backspace lost reading".into());
    }
    if !resume_reading(&host, engine) || !wait_preview(&host, resumed_completion) {
        return Err("resume after selected Backspace".into());
    }
    let selected = host.candidate_strings()[0].clone();
    if !host.feed_key(0x09)
        || !host.feed_key(0x31)
        || !wait(|| !host.store.composing())
        || host.store.committed() != selected
    {
        return Err("selected digit did not commit exactly one candidate".into());
    }
    host.store.reset();
    if !type_reading(&host, engine) || !wait_preview(&host, completion) {
        return Err("typing-resume preview".into());
    }
    let selected = host.candidate_strings()[0].clone();
    if !host.feed_key(0x09)
        || !host.feed_key(0x4B)
        || !host.feed_key(0x41)
        || !wait(|| host.store.committed() == selected && !host.store.preedit().is_empty())
        || !host.feed_key(0x75)
        || !host.feed_key(0x0D)
        || !wait(|| !host.store.composing())
        || host.store.committed() != format!("{selected}か")
    {
        return Err(format!(
            "typing after selection lost input: {:?}",
            host.store.full()
        ));
    }
    host.store.reset();
    if !type_reading(&host, engine) || !wait_preview(&host, completion) {
        return Err("cancel preview".into());
    }
    if !host.feed_key(0x09)
        || !host.feed_key(0x1B)
        || host.store.preedit() != reading
        || !host.store.committed().is_empty()
    {
        return Err("selected Esc lost reading".into());
    }
    if !resume_reading(&host, engine) || !wait_preview(&host, resumed_completion) {
        return Err("resume after selected Esc".into());
    }
    if live && !host.feed_key(0x09) {
        return Err("selection before context switch".into());
    }
    if !host
        .push_and_pop_empty_context()
        .map_err(|e| e.to_string())?
    {
        return Err("top-context change retained preview".into());
    }
    let _ = crate::driver::run_keys(&host, &[crate::scenarios::WAIT_CONVERSION]);
    if !host.candidate_strings().is_empty() {
        return Err("stale context reopened preview".into());
    }
    // AzooKey/hybrid must expose dictionary pages; native Windows vocabulary varies.
    // In every mode, traverse all returned candidates, including a partial last page.
    let _ = host.feed_key(0x1B);
    host.store.reset();
    if !crate::scenarios::typed("sankou").into_iter().all(|key| host.feed_key(key.0)) {
        return Err("page reading keys not eaten".into());
    }
    let mut choices = Vec::new();
    if !wait(|| {
        let offered = host.candidate_strings();
        if offered.is_empty() || (engine != "microsoft" && offered.len() <= 9)
            || !host.feed_key(0x09) { return false; }
        choices = offered;
        true
    }) {
        return Err(format!("missing fresh prediction pages: {:?}", host.candidate_strings()));
    }
    for (index, expected) in choices.iter().enumerate() {
        if (index > 0 && !host.feed_key(0x09)) || host.store.preedit() != *expected {
            return Err(format!("Tab prediction {index}: {:?}, expected {expected}", host.store.preedit()));
        }
        let page_start = index / 9 * 9;
        if host.candidate_strings() != choices[page_start..(page_start + 9).min(choices.len())] {
            return Err(format!("wrong prediction page at {index}"));
        }
    }
    if !host.feed_key(0x09) || host.store.preedit() != choices[0] {
        return Err("Tab did not wrap after the final prediction".into());
    }
    // Commit the first candidate on page two when available, otherwise the last one.
    let commit_index = if choices.len() > 9 { 9 } else { choices.len() - 1 };
    for _ in 0..commit_index {
        if !host.feed_key(0x09) { return Err("Tab stopped before the commit candidate".into()); }
    }
    if !host.feed_key(0x0D) || !wait(|| !host.store.composing()) || host.store.committed() != choices[commit_index] {
        return Err(format!("prediction commit at {commit_index}: {:?}", host.store.full()));
    }
    if dir.join("feedback.jsonl").exists() {
        return Err("feedback file created".into());
    }
    Ok(())
}
pub fn run() -> i32 { run_engine("azookey") }

pub fn run_microsoft() -> i32 { run_engine("microsoft") }
pub fn run_hybrid() -> i32 {
    // main disables auto-commit for deterministic candidate fixtures. Set the
    // production defaults before the first host starts the shared guest engine.
    std::env::set_var("NOSPACEKEY_AUTO_COMMIT", "weak");
    std::env::set_var("NOSPACEKEY_AUTO_COMMIT_MAX_READING", "25");
    run_engine("hybrid")
}

fn check_mixed_candidate_live_commits(dir: &std::path::Path) -> Result<(), String> {
    std::fs::write(dir.join("settings.json"), serde_json::json!({
        "version": 2, "conversion_engine": "hybrid", "mixed_input": "candidates",
        "live_conversion": {"enabled": true, "search_width": 10},
        "input_prediction_enabled": true, "zenzai": {"enabled": false},
        "learning": {"enabled": false}
    }).to_string()).map_err(|e| e.to_string())?;
    let host = TsfHost::start().map_err(|e| format!("start: {e}"))?;
    if !host.normalize_native_mode() { return Err("native mode".into()); }
    host.warm_up();
    host.store.reset();
    for round in 0..2 {
        let before = host.store.committed();
        // No Space/Enter: the length backstop must commit while Japanese typing
        // continues, even when the optional mixed candidate menu is enabled.
        for key in crate::scenarios::typed("kyouhaiitenkinanodenagaibunshouwoutitsuduketeimasu") {
            if !host.feed_key(key.0) { return Err("long reading key not eaten".into()); }
            tsf_host::pump();
        }
        if !wait(|| host.store.committed().len() > before.len()) {
            return Err(format!("round {round}: no automatic prefix commit, preedit={:?}",
                host.store.preedit()));
        }
        if !host.store.committed().starts_with(&before) || !host.store.composing() {
            return Err("prefix commit lost earlier text or remaining composition".into());
        }
    }
    let before = host.store.full();
    if !host.feed_key(0x0D) || !wait(|| !host.store.composing()) || host.store.committed() != before {
        return Err("Enter failed to retain the auto-committed prefix and visible remainder".into());
    }
    host.store.reset();
    let keys = crate::scenarios::typed("githubnotukaikatawosetsumeisurunodenagaibunshouwoutimasu");
    let mut last_events = 0;
    for (index, key) in keys.iter().enumerate() {
        if index + 1 == keys.len() { last_events = crate::log_parse::read_events(std::process::id()).len(); }
        if !host.feed_key(key.0) { return Err("mixed long reading key not eaten".into()); }
    }
    if !wait(|| crate::log_parse::read_events(std::process::id()).iter().skip(last_events)
        .any(|event| matches!(event, crate::log_parse::Ev::LiveSnapshotApplied))) {
        return Err("protected mixed reading lost its live display".into());
    }
    if !host.store.committed().is_empty() {
        return Err(format!("unselected github was auto-committed: {:?}", host.store.committed()));
    }
    host.feed_key(0x20);
    if !wait(|| host.candidate_strings().iter().any(|s| s.starts_with("github"))) {
        return Err("auto-commit protection lost the original github source".into());
    }
    let index = host.candidate_strings().iter().position(|s| s.starts_with("github")).unwrap();
    let expected = host.candidate_strings()[index].clone();
    host.behavior_select_and_finalize(index as u32);
    if !wait(|| !host.store.composing() && host.store.committed() == expected) {
        return Err("protected github candidate could not be committed".into());
    }
    Ok(())
}

fn check_hybrid_clauses(dir: &std::path::Path, live: bool) -> Result<(), String> {
    std::fs::write(dir.join("settings.json"), serde_json::json!({
        "version": 2, "conversion_engine": "hybrid", "mixed_input": "candidates",
        "live_conversion": {"enabled": live}, "input_prediction_enabled": true,
        "zenzai": {"enabled": false}, "learning": {"enabled": false}
    }).to_string()).map_err(|e| e.to_string())?;
    let host = TsfHost::start().map_err(|e| format!("start: {e}"))?;
    if !host.normalize_native_mode() { return Err("native mode".into()); }
    host.warm_up();
    host.store.reset();
    crate::live_clause_display::check_host(&host, live)
        .then_some(()).ok_or_else(|| "Space clause display/navigation failed".into())
}

fn run_engine(engine: &str) -> i32 {
    eprintln!("prediction acceptance: init");
    // Use only the disposable Sandbox's initially empty settings so the host's
    // configuration and its persist engine cannot affect this acceptance run.
    if std::env::var("NOSPACEKEY_TEST_SANDBOX").as_deref() != Ok("1") {
        println!("input-predictions : ERROR (requires run-gate.ps1 -Sandbox)");
        return 2;
    }
    let Some(settings_path) = settings::settings_path() else {
        return 2;
    };
    if settings_path.exists() {
        println!("input-predictions : ERROR (refusing to replace existing settings)");
        return 2;
    }
    let dir = settings_path.parent().unwrap().to_owned();
    if std::fs::create_dir_all(&dir).is_err() {
        return 2;
    }
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(_) => return 2,
    };
    let mut passed = true;
    for (live, enabled) in [(false, true), (true, true), (false, false)] {
        let result = check_case(&dir, live, enabled, engine);
        println!(
            "input-predictions engine={engine} live={live} enabled={enabled} : {} {:?}",
            if result.is_ok() { "PASS" } else { "FAIL" },
            result.as_ref().err()
        );
        passed &= result.is_ok();
    }
    if engine == "hybrid" {
        for live in [false, true] {
            let result = check_hybrid_clauses(&dir, live);
            println!("input-predictions engine=hybrid mixed=candidates clauses live={live} : {} {:?}",
                if result.is_ok() { "PASS" } else { "FAIL" }, result.as_ref().err());
            passed &= result.is_ok();
        }
        let result = check_mixed_candidate_live_commits(&dir);
        println!("input-predictions engine=hybrid mixed=candidates auto-commit : {} {:?}",
            if result.is_ok() { "PASS" } else { "FAIL" }, result.as_ref().err());
        passed &= result.is_ok();
    }
    if passed {
        0
    } else {
        1
    }
}
