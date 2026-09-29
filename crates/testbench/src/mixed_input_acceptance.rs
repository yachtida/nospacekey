//! Mixed candidate selection against the installed TIP, only inside the disposable gate.
use crate::{
    scenarios,
    tsf_host::{self, TsfHost},
};
use std::time::{Duration, Instant};
fn wait(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        tsf_host::pump();
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}
fn open_menu(host: &TsfHost, input: &str) -> Result<String, String> {
    for key in scenarios::typed(input) {
        if !host.feed_key(key.0) {
            return Err("typing not eaten".into());
        }
    }
    let reading = host.store.preedit();
    if !host.feed_key(scenarios::SPACE.0)
        || !wait(|| {
            host.candidate_strings()
                .iter()
                .any(|s| s.starts_with("区間を修正"))
        })
    {
        return Err(format!(
            "no mixed menu: {:?}; preedit={:?}",
            host.candidate_strings(),
            host.store.preedit()
        ));
    }
    Ok(reading)
}
fn open(host: &TsfHost) -> Result<(u32, String), String> {
    let reading = open_menu(host, "githubnotukaikata")?;
    let index = host
        .candidate_strings()
        .iter()
        .position(|s| s.starts_with("github"))
        .ok_or_else(|| format!("no github candidate: {:?}", host.candidate_strings()))?
        as u32;
    Ok((index, reading))
}
fn check_manual_repair(host: &TsfHost) -> Result<(), String> {
    for converted in [false, true] {
        host.store.reset();
        for key in scenarios::typed("madewotukatteimasu") {
            if !host.feed_key(key.0) { return Err("repair typing not eaten".into()); }
        }
        let reading = host.store.preedit();
        if converted && (!host.feed_key(scenarios::SPACE.0)
            || !wait(|| host.store.preedit() != reading)) {
            return Err("ordinary clause conversion did not start".into());
        }
        if !host.feed_key_with_ctrl_shift(scenarios::SPACE.0)
            || !wait(|| host.candidate_strings() == ["英字のまま", "日本語として解釈"]) {
            return Err(format!("manual repair unavailable: converted={converted}"));
        }
        host.feed_key(scenarios::ESC.0);
        if host.store.preedit() != reading || !host.store.committed().is_empty() {
            return Err("repair cancel changed the input".into());
        }
        host.feed_key_with_ctrl_shift(scenarios::SPACE.0);
        host.feed_key(0x24); // Home; select only 'made', retaining the Japanese suffix.
        for _ in 0..4 { host.feed_key_with_shift(0x27); }
        host.feed_key(scenarios::ENTER.0);
        if !wait(|| host.store.preedit().starts_with("made")
            && host.candidate_strings().iter().any(|s| s.starts_with("made"))) {
            return Err(format!("manual Literal was not adopted: {:?}", host.store.preedit()));
        }
        let expected = host.store.preedit();
        if expected == "madewotukatteimasu" || !expected.ends_with("います") {
            return Err(format!("repair lost its Japanese suffix: {expected:?}"));
        }
        host.feed_key(scenarios::ENTER.0);
        if !wait(|| !host.store.composing() && host.store.committed() == expected) {
            return Err("repair commit mismatch".into());
        }
    }
    Ok(())
}

fn check_ordinary(host: &TsfHost) -> Result<(), String> {
    for number_key in [false, true] {
        host.store.reset();
        let (mixed_index, _) = open(host)?;
        if mixed_index <= 1 { return Err("alternate ordinary choice missing before mixed alternatives".into()); }
        let index = 1;
        let expected = host.candidate_strings()[index as usize].clone();
        if number_key {
            host.feed_key(0x31 + index);
        } else {
            host.behavior_select(index);
            let before = host.store.preedit();
            host.store.reject_text.set(true);
            host.behavior_select_and_finalize(index);
            tsf_host::pump();
            if !host.store.committed().is_empty() || host.store.preedit() != before
                || host.candidate_strings().is_empty() {
                return Err("rejected ordinary commit lost document or menu state".into());
            }
            host.store.reject_text.set(false);
            host.behavior_select_and_finalize(index);
        }
        if !wait(|| !host.store.composing() && host.store.committed() == expected) {
            return Err(format!("ordinary choice was replaced: number={number_key} full={:?}", host.store.full()));
        }
    }
    host.store.reset();
    let reading = open_menu(host, "gazounoyounigithub")?;
    let remaining = reading.strip_prefix("がぞう")
        .ok_or_else(|| format!("unexpected partial candidate reading: {reading:?}"))?;
    let expected = format!("画像{remaining}A");
    let index = host.candidate_strings().iter().position(|s| s == "画像")
        .ok_or_else(|| format!("no partial ordinary candidate: {:?}", host.candidate_strings()))? as u32;
    host.behavior_select(index);
    host.feed_key_with_shift(0x41);
    if !wait(|| !host.store.composing() && host.store.committed() == expected) {
        return Err(format!("Shift settle lost the partial candidate suffix: {:?}", host.store.full()));
    }
    Ok(())
}
fn check(host: &TsfHost) -> Result<(), String> {
    let (index, reading) = open(host)?;
    if !host.behavior_select(index) || !wait(|| host.store.preedit().starts_with("github")) {
        return Err("host selection did not adopt".into());
    }
    let selected = host.store.preedit();
    host.store.reject_text.set(true);
    host.behavior_select(0);
    tsf_host::pump();
    if host.store.preedit() != selected || host.candidate_selection() != index {
        return Err("rejected preview changed selection".into());
    }
    host.feed_key(scenarios::ESC.0);
    tsf_host::pump();
    if host.store.preedit() != selected || host.candidate_strings().is_empty() {
        return Err("rejected Escape lost candidate state".into());
    }
    host.store.reject_text.set(false);
    if !host.feed_key(scenarios::ESC.0)
        || !wait(|| host.store.preedit() == reading && host.candidate_strings().is_empty())
    {
        return Err("Escape did not restore source".into());
    }
    host.feed_key(scenarios::ESC.0);
    if !wait(|| !host.store.composing()) {
        return Err("cancel did not close composition".into());
    }
    host.store.reset();
    let (index, _) = open(host)?;
    let expected = host.candidate_strings()[index as usize].clone();
    if !host.behavior_select_and_finalize(index)
        || !wait(|| !host.store.composing() && host.store.committed() == expected)
    {
        return Err(format!(
            "host finalize mismatch: {:?}; wanted={expected:?}",
            host.store.full()
        ));
    }
    host.store.reset();
    let (index, _) = open(host)?;
    host.behavior_select(index);
    if !wait(|| host.store.preedit().starts_with("github")) {
        return Err("preview missing".into());
    }
    let before = host.store.preedit();
    host.store.reject_text.set(true);
    if !host.feed_key(scenarios::typed("d")[0].0) {
        return Err("continuation key was not accepted".into());
    }
    if host.store.preedit() != before || !host.store.committed().is_empty() {
        return Err("rejected continuation changed the body".into());
    }
    host.store.reject_text.set(false);
    if !wait(|| host.store.preedit().starts_with("github") && host.store.preedit().ends_with('d')) {
        return Err("continuation redraw did not recover without another key".into());
    }
    for key in scenarios::typed("esu") {
        host.feed_key(key.0);
    }
    if !host.store.preedit().starts_with("github")
        || !host.store.preedit().ends_with("です")
        || !host.store.committed().is_empty()
    {
        return Err(format!(
            "continuation changed literal: {:?}",
            host.store.full()
        ));
    }
    let expected = host.store.preedit();
    host.feed_key(scenarios::ENTER.0);
    if !wait(|| !host.store.composing() && host.store.committed() == expected) {
        return Err("continued input commit mismatch".into());
    }
    check_ordinary(host)?;
    check_manual_repair(host)
}
pub fn run() -> i32 {
    if std::env::var("NOSPACEKEY_TEST_SANDBOX").as_deref() != Ok("1") {
        println!("mixed-input : ERROR (requires run-gate.ps1 -Sandbox)");
        return 2;
    }
    let Some(path) = settings::settings_path() else {
        return 2;
    };
    if path.exists() {
        println!("mixed-input : ERROR (existing settings)");
        return 2;
    }
    if std::fs::create_dir_all(path.parent().unwrap()).is_err() {
        return 2;
    }
    let mut settings = settings::Settings::default();
    settings.mixed_input = settings::MixedInputMode::Candidates;
    settings.live_conversion.enabled = false;
    settings.input_prediction_enabled = false;
    settings.zenzai.enabled = false;
    settings.learning.enabled = false;
    settings.shift_latin.mode = "commit".into();
    if std::fs::write(path, settings.to_json()).is_err() {
        return 2;
    }
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(_) => return 2,
    };
    let host = match TsfHost::start() {
        Ok(host) => host,
        Err(_) => return 2,
    };
    if !host.normalize_native_mode() || !host.force_native_conversion_mode() {
        return 2;
    }
    host.force_pbshow(Some(false));
    host.warm_up();
    host.store.reset();
    let result = check(&host);
    println!(
        "mixed-input : {} {:?}",
        if result.is_ok() { "PASS" } else { "FAIL" },
        result.as_ref().err()
    );
    if result.is_ok() {
        0
    } else {
        1
    }
}
