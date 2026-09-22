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
fn type_reading(host: &TsfHost) -> bool {
    crate::scenarios::typed("gazo")
        .into_iter()
        .all(|key| host.feed_key(key.0))
}
fn check_case(dir: &std::path::Path, live: bool, enabled: bool) -> Result<(), String> {
    std::fs::write(dir.join("settings.json"), serde_json::json!({
        "version": 2, "live_conversion": {"enabled": live},
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
    if !type_reading(&host) {
        return Err("reading keys not eaten".into());
    }
    if !enabled {
        let _ = crate::driver::run_keys(&host, &[crate::scenarios::WAIT_CONVERSION]);
        if !host.candidate_strings().is_empty() || host.feed_key(0x09) {
            return Err("disabled preview claimed Tab".into());
        }
        return Ok(());
    }
    if !wait(|| host.candidate_strings().iter().any(|c| c == "画像")) {
        return Err(format!(
            "missing 画像: {:?}, preedit={:?}",
            host.candidate_strings(),
            host.store.preedit()
        ));
    }
    if !host.store.committed().is_empty() || (!live && host.store.preedit() != "がぞ") {
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
    if !type_reading(&host) || !wait(|| host.candidate_strings().iter().any(|c| c == "画像")) {
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
    if !host.feed_key(0x55) || !wait(|| !host.candidate_strings().is_empty()) {
        return Err("reading change did not reopen preview".into());
    }
    let choices = host.candidate_strings();
    let index = choices
        .iter()
        .position(|c| c == "画像")
        .ok_or("updated preview lacks 画像")?;
    if !host.behavior_select_and_finalize(index as u32)
        || !wait(|| !host.store.composing())
        || host.store.committed() != "画像"
    {
        return Err("click/Behavior completion commit".into());
    }
    host.store.reset();
    if !type_reading(&host) || !wait(|| !host.candidate_strings().is_empty()) {
        return Err("numeric preview".into());
    }
    if !host.feed_key(0x31)
        || !host.store.committed().is_empty()
        || !host.feed_key(0x08)
        || !wait(|| host.candidate_strings().iter().any(|c| c == "画像"))
    {
        return Err("unselected digit/Backspace changed or committed the reading".into());
    }
    if !host.feed_key(0x09)
        || !host.feed_key(0x08)
        || host.store.preedit() != "がぞ"
        || !host.store.committed().is_empty()
    {
        return Err("selected Backspace lost reading".into());
    }
    if !host.feed_key(0x55) || !wait(|| !host.candidate_strings().is_empty()) {
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
    if !type_reading(&host) || !wait(|| !host.candidate_strings().is_empty()) {
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
    if !type_reading(&host) || !wait(|| !host.candidate_strings().is_empty()) {
        return Err("cancel preview".into());
    }
    if !host.feed_key(0x09)
        || !host.feed_key(0x1B)
        || host.store.preedit() != "がぞ"
        || !host.store.committed().is_empty()
    {
        return Err("selected Esc lost reading".into());
    }
    if !host.feed_key(0x55) || !wait(|| !host.candidate_strings().is_empty()) {
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
    if dir.join("feedback.jsonl").exists() {
        return Err("feedback file created".into());
    }
    Ok(())
}
pub fn run() -> i32 {
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
        let result = check_case(&dir, live, enabled);
        println!(
            "input-predictions live={live} enabled={enabled} : {} {:?}",
            if result.is_ok() { "PASS" } else { "FAIL" },
            result.as_ref().err()
        );
        passed &= result.is_ok();
    }
    if passed {
        0
    } else {
        1
    }
}
