//! ライブ変換ON下の Space 文節表示属性ゲート（2026-09 live太下線調査）。
//! 実機報告: live ON で Space を押すと文節区切りの下線は出るが、選択文節の
//! 太下線（TF_ATTR_TARGET_CONVERTED / fBoldLine）だけが出ない。live OFF では
//! 同一アプリで出る。ホストは属性のみの更新でも OnEndEdit で再読み込みする
//! （Chromium TSFTextStore は無条件に GUID_PROP_ATTRIBUTE を読む）ため、
//! 疑いは TIP が TARGET atom を範囲へ SetValue していないことに絞られる。
//! このゲートは実登録 TIP に対し live ON の scratch settings で
//! 「タイプ→live snapshot 適用→Space→ClausePresented→属性 probe」を走らせ、
//! preedit 上の per-unit atom→GUID を検証する。
use crate::{log_parse::{read_events, Ev}, scenarios, tsf_host::{self, ComSta, TsfHost}};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use windows::core::{implement, Result};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::TextServices::*;

fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        tsf_host::pump();
        if ready() { return true; }
        if Instant::now() >= deadline { return false; }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn check(name: &str, passed: bool, detail: impl std::fmt::Debug) -> bool {
    println!("live-clause-display {name} : {} ({detail:?})", if passed { "PASS" } else { "FAIL" });
    passed
}

#[implement(ITfEditSession)]
struct AttributeProbe {
    context: ITfContext,
    units: i32,
    result: Rc<RefCell<Option<Vec<Option<i32>>>>>,
}
impl ITfEditSession_Impl for AttributeProbe_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        unsafe {
            let property = self.context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
            let mut values = Vec::new();
            for index in 0..self.units {
                let range = self.context.GetStart(ec)?;
                let mut moved = 0;
                range.ShiftEnd(ec, index + 1, &mut moved, core::ptr::null())?;
                if moved != index + 1 {
                    return Err(windows::Win32::Foundation::E_FAIL.into());
                }
                range.ShiftStart(ec, index, &mut moved, core::ptr::null())?;
                if moved != index {
                    return Err(windows::Win32::Foundation::E_FAIL.into());
                }
                let value = property.GetValue(ec, &range)?;
                values.push((value.Anonymous.Anonymous.vt == windows::Win32::System::Variant::VT_I4)
                    .then(|| value.Anonymous.Anonymous.Anonymous.lVal));
            }
            *self.result.borrow_mut() = Some(values);
        }
        Ok(())
    }
}

/// preedit 全 unit の表示属性 atom を読み、GUID へ解決したラベル列を返す。
/// 不明 atom はその数値のまま文字列化する（診断用）。
fn probe_preedit_attributes(host: &TsfHost) -> Vec<String> {
    let context = host.context().clone();
    let units = host.store.full().encode_utf16().count() as i32;
    let result = Rc::new(RefCell::new(None));
    let session: ITfEditSession = AttributeProbe {
        context: context.clone(),
        units,
        result: result.clone(),
    }
    .into();
    let hr = unsafe {
        context.RequestEditSession(
            host.client_id(),
            &session,
            TF_CONTEXT_EDIT_CONTEXT_FLAGS(TF_ES_SYNC.0 | TF_ES_READ.0),
        )
    };
    let atoms = match (hr.is_ok(), result.borrow_mut().take()) {
        (true, Some(values)) => values,
        _ => return Vec::new(),
    };
    let tail_units = host.store.preedit().encode_utf16().count();
    let tail: Vec<Option<i32>> = atoms[atoms.len().saturating_sub(tail_units)..].to_vec();
    let category: ITfCategoryMgr = match unsafe {
        CoCreateInstance(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)
    } {
        Ok(cat) => cat,
        Err(_) => return tail.iter().map(|a| format!("{a:?}")).collect(),
    };
    tail.iter()
        .map(|atom| match atom {
            None => "none".to_string(),
            Some(atom) => {
                let guid = match unsafe { category.GetGUID(*atom as u32) } {
                    Ok(guid) => guid,
                    Err(_) => return format!("atom({atom})"),
                };
                if guid == ids::GUID_DISPLAY_ATTRIBUTE_TARGET { "target".into() }
                else if guid == ids::GUID_DISPLAY_ATTRIBUTE_CONVERTED { "converted".into() }
                else if guid == ids::GUID_DISPLAY_ATTRIBUTE { "input".into() }
                else if guid == ids::GUID_DISPLAY_ATTRIBUTE_PREDICTION { "prediction".into() }
                else { format!("guid({guid:?})") }
            }
        })
        .collect()
}

/// target ラベルの連続 run の位置（最初の run の (start, end)）。無ければ None。
fn first_target_run(labels: &[String]) -> Option<(usize, usize)> {
    let start = labels.iter().position(|l| l == "target")?;
    let end = start + labels.iter().skip(start).take_while(|l| *l == "target").count();
    Some((start, end))
}

pub fn run() -> i32 {
    crate::driver::kill_engine_processes();
    let base = std::env::temp_dir().join(format!("nospacekey-live-clause-display-{}", std::process::id()));
    let dir = base.join("nospacekey");
    if let Err(e) = std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(dir.join("settings.json"),
            r#"{"version":2,"zenzai":{"enabled":false},"learning":{"enabled":false},"default_direct":false,"live_conversion":{"enabled":true}}"#))
    {
        eprintln!("live-clause-display settings fixture fail: {e:?}");
        return 2;
    }
    std::env::set_var("LOCALAPPDATA", &base);

    let _com = match ComSta::init() {
        Ok(com) => com,
        Err(e) => {
            println!("live-clause-display : ERROR ({e})");
            let _ = std::fs::remove_dir_all(&base);
            return 2;
        }
    };
    let host = match TsfHost::start() {
        Ok(host) => host,
        Err(e) => {
            println!("live-clause-display : ERROR ({e})");
            let _ = std::fs::remove_dir_all(&base);
            return 2;
        }
    };
    if !host.normalize_native_mode() && !host.force_native_conversion_mode() {
        println!("live-clause-display : ERROR (conversion mode stuck in direct; toggle IME to hiragana and retry)");
        let _ = std::fs::remove_dir_all(&base);
        return 2;
    }
    host.warm_up();
    host.store.reset();

    let mut eaten = true;
    for key in scenarios::typed("kyouhaiitenkidesu") { eaten &= host.feed_key(key.0); }
    let kana = "きょうはいいてんきです";
    let mut settled = String::new();
    for _ in 0..5 {
        host.settle_debounce();
        settled = host.store.preedit();
        if !settled.is_empty() && settled != kana { break; }
    }
    if settled.is_empty() || settled == kana {
        println!("live-clause-display : ERROR (live snapshot did not apply; settled={settled:?})");
        let _ = std::fs::remove_dir_all(&base);
        return 2;
    }

    let pid = std::process::id();
    let events = read_events(pid).len();
    eaten &= host.feed_key(scenarios::SPACE.0);
    let presented = wait_until(|| read_events(pid).iter().skip(events)
        .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. })));
    let passed = check("space-presents-clause-view", eaten && presented, host.store.preedit());
    let passed = check("first-space-preserves-live-text", host.store.preedit() == settled,
        (&settled, &host.store.preedit())) && passed;

    let labels = probe_preedit_attributes(&host);
    let target_run = first_target_run(&labels);
    let mut passed = check("first-space-targets-first-clause",
        labels.first().is_some_and(|l| l == "target") && target_run.is_some_and(|(_, end)| end < labels.len()),
        (&host.store.preedit(), &labels)) && passed;
    passed = check("later-clauses-are-converted",
        labels.last().is_some_and(|l| l == "converted"), (&host.store.preedit(), &labels)) && passed;

    let events = read_events(pid).len();
    eaten &= host.feed_key(scenarios::SPACE.0);
    let ready = wait_until(|| read_events(pid).iter().skip(events)
        .any(|event| matches!(event, Ev::ClausePresented { ready: true, .. })));
    let candidates = host.candidate_strings();
    let selected = host.candidate_selection();
    let selected_end = target_run.map(|(_, end)| end).unwrap_or(0);
    let original_units = settled.encode_utf16().collect::<Vec<_>>();
    let first_clause = String::from_utf16_lossy(&original_units[..selected_end]);
    let suffix = String::from_utf16_lossy(&original_units[selected_end..]);
    let expected = candidates.get(1).map(|candidate| format!("{candidate}{suffix}"));
    passed = check("second-space-selects-second-candidate", eaten && ready
        && candidates.first() == Some(&first_clause) && selected == 1
        && expected.as_deref() == Some(host.store.preedit().as_str()),
        (&candidates, selected, &host.store.preedit())) && passed;
    // Third Space advances once again, then Esc retains that selected surface.
    eaten &= host.feed_key(scenarios::SPACE.0);
    let next = if candidates.len() > 2 { 2 } else { 0 };
    let expected = candidates.get(next).map(|candidate| format!("{candidate}{suffix}"));
    passed = check("third-space-advances-once", eaten
        && host.candidate_selection() == next as u32
        && expected.as_deref() == Some(host.store.preedit().as_str()),
        (host.candidate_selection(), &host.store.preedit())) && passed;
    eaten &= host.feed_key(scenarios::ESC.0);
    passed = check("escape-keeps-selected-surface", eaten
        && expected.as_deref() == Some(host.store.preedit().as_str()), host.store.preedit()) && passed;

    let events = read_events(pid).len();
    let writes_before_nav = host.store.text_writes.get();
    let body_before_nav = host.store.preedit();
    eaten &= host.feed_key(scenarios::RIGHT.0);
    let moved = wait_until(|| read_events(pid).iter().skip(events)
        .any(|event| matches!(event, Ev::ClausePresented { .. })));
    let labels_after = probe_preedit_attributes(&host);
    let moved_run = first_target_run(&labels_after);
    passed = check("clause-nav-moves-target",
        eaten && moved
            && labels_after.first().is_some_and(|l| l != "target")
            && moved_run.is_some_and(|(start, _)| start > 0),
        (&host.store.preedit(), &labels_after)) && passed;
    // 属性のみの更新(テキスト不変・装飾変化)では Chromium 系ホストが下線スタイルを
    // 再描画しない(2026-09 live太下線凍結問題)。装飾付き適用は本文を書き直して
    // ホストに再描画を強制する契約。本文内容は不変のままでも書き込みが起きること。
    passed = check("clause-nav-rewrites-body-for-host-redraw",
        host.store.text_writes.get() > writes_before_nav
            && host.store.preedit() == body_before_nav,
        (writes_before_nav, host.store.text_writes.get(), &body_before_nav)) && passed;

    let _ = host.feed_key(scenarios::ESC.0);
    let _ = std::fs::remove_dir_all(&base);
    if passed { 0 } else { 1 }
}
