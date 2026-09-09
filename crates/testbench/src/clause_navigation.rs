//! Staged key acceptance against the installed TIP and real TSF candidate UI.
use crate::{log_parse::{read_events, Ev}, scenarios, tsf_host::{self, ComSta, TsfHost}};
use std::time::{Duration, Instant};

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
    println!("clause-navigation {name} : {} ({detail:?})", if passed { "PASS" } else { "FAIL" });
    passed
}

fn check_kana_commit(host: &TsfHost, key: scenarios::Vk, expected: &str, during_initial: bool, rotations: usize) -> bool {
    let mut eaten = host.feed_key(scenarios::ESC.0);
    if !wait_until(|| !host.store.composing()) {
        return check("kana-reset", false, host.store.full());
    }
    host.store.reset();
    for key in scenarios::typed("nihongo") { eaten &= host.feed_key(key.0); }
    if during_initial {
        eaten &= host.feed_key_no_pump(scenarios::SPACE.0);
        eaten &= host.feed_key_no_pump(key.0);
        for _ in 0..rotations { eaten &= host.feed_key_no_pump(scenarios::NONCONVERT.0); }
    } else {
        let pid = std::process::id();
        let base = read_events(pid).len();
        eaten &= host.feed_key(scenarios::SPACE.0);
        // Earlier cases learn the reading candidate, so its rank may now be first.
        if !wait_until(|| read_events(pid).iter().skip(base)
            .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))
            && !host.store.preedit().is_empty() && host.store.committed().is_empty()) {
            return check("kana-initial", false, host.store.preedit());
        }
        eaten &= host.feed_key(scenarios::SPACE.0);
        if !wait_until(|| host.candidate_strings().len() >= 2) {
            return check("kana-candidates", false, host.candidate_strings());
        }
        if host.candidate_strings().get(host.candidate_selection() as usize) != Some(&host.store.preedit()) {
            return check("kana-whole-clause", false, host.store.preedit());
        }
        eaten &= host.feed_key(key.0);
        for _ in 0..rotations { eaten &= host.feed_key(scenarios::NONCONVERT.0); }
        if !check("kana-visible", eaten && host.store.preedit() == expected
            && host.store.committed().is_empty() && host.candidate_strings().is_empty(),
            (key.1, host.store.preedit())) { return false; }
    }
    eaten &= host.feed_key_no_pump(scenarios::ENTER.0);
    eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
    let complete = wait_until(|| host.store.committed() == expected && host.store.preedit() == "あ");
    check("kana-commit-and-next-input", eaten && complete && host.store.full() == format!("{expected}あ"),
        (key.1, during_initial, rotations, eaten, host.store.committed(), host.store.preedit()))
}

fn check_rejected_commit(host: &TsfHost) -> bool {
    let mut eaten = host.feed_key(scenarios::ESC.0);
    if !wait_until(|| !host.store.composing()) {
        return check("reject-commit-reset", false, host.store.full());
    }
    host.store.reset();
    for key in scenarios::typed("nihongo") { eaten &= host.feed_key(key.0); }
    let event_start = read_events(std::process::id()).len();
    eaten &= host.feed_key(scenarios::SPACE.0);
    if !wait_until(|| read_events(std::process::id()).iter().skip(event_start)
        .any(|event| matches!(event, Ev::ClausePresented { .. }))) {
        return check("reject-commit-conversion", false, host.store.preedit());
    }
    // Earlier cases may have learned the reading itself as the top candidate.
    // Select a real alternative so this fault case also covers candidate surfaces.
    eaten &= host.feed_key(scenarios::SPACE.0);
    if !wait_until(|| host.candidate_strings().len() >= 2) {
        return check("reject-commit-candidates", false, host.candidate_strings());
    }
    let candidate_count = host.candidate_strings().len();
    for _ in 0..candidate_count {
        if host.store.preedit() != "にほんご" { break; }
        eaten &= host.feed_key(scenarios::SPACE.0);
    }
    let body = host.store.preedit();
    if body.is_empty() || body == "にほんご"
        || host.candidate_strings().get(host.candidate_selection() as usize) != Some(&body) {
        return check("reject-commit-candidate-surface", false, body);
    }
    let writes = host.store.text_writes.get();
    host.store.reject_text.set(true);
    eaten &= host.feed_key(scenarios::ENTER.0);
    eaten &= host.feed_key(scenarios::ch('a').0);
    // Keep the fault armed across message delivery, including any deferred edit session.
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        tsf_host::pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    let retained = check("rejected-commit-retains-body-and-input", eaten && !body.is_empty()
        && host.store.composing() && host.store.preedit() == body
        && host.store.committed().is_empty() && host.store.full() == body
        && host.store.text_writes.get() == writes,
        (eaten, &body, host.store.full(), host.store.text_writes.get() - writes));
    host.store.reject_text.set(false);
    eaten &= host.feed_key(scenarios::ENTER.0);
    let recovered = wait_until(|| host.store.committed() == body && host.store.preedit() == "あ");
    let retry = check("rejected-commit-retry-preserves-order", eaten && recovered
        && host.store.full() == format!("{body}あ"),
        (eaten, host.store.committed(), host.store.preedit(), host.store.full()));
    retained && retry
}

fn check_rejected_commit_caret(host: &TsfHost) -> bool {
    let mut eaten = host.feed_key(scenarios::ESC.0);
    if !wait_until(|| !host.store.composing()) {
        return check("commit-caret-reset", false, host.store.full());
    }
    host.store.reset();
    for key in scenarios::typed("nihongo") { eaten &= host.feed_key(key.0); }
    let event_start = read_events(std::process::id()).len();
    eaten &= host.feed_key(scenarios::SPACE.0);
    if !wait_until(|| read_events(std::process::id()).iter().skip(event_start)
        .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) {
        return check("commit-caret-conversion", false, host.store.preedit());
    }
    let body = host.store.preedit();
    let writes = host.store.text_writes.get();
    let rejected = host.store.rejected_selections.get();
    host.store.reject_selection.set(true);
    eaten &= host.feed_key(scenarios::ENTER.0);
    eaten &= host.feed_key(scenarios::ch('a').0);
    let failed = check("commit-caret-failure-writes-body-once", eaten && !body.is_empty()
        && host.store.rejected_selections.get() > rejected
        && host.store.text_writes.get() == writes + 1 && host.store.full() == body,
        (eaten, &body, host.store.full(), host.store.text_writes.get() - writes,
            host.store.rejected_selections.get() - rejected));
    host.store.reject_selection.set(false);
    eaten &= host.feed_key(scenarios::ENTER.0);
    let recovered = wait_until(|| host.store.committed() == body && host.store.preedit() == "あ");
    // Exactly one body write and one new preedit write: repair must not rewrite the body.
    let repaired = check("commit-caret-repair-preserves-input-without-rewrite", eaten && recovered
        && host.store.full() == format!("{body}あ") && host.store.text_writes.get() == writes + 2,
        (eaten, host.store.committed(), host.store.preedit(), host.store.text_writes.get() - writes));
    failed && repaired
}

fn check_terminated_failed_commit(host: &TsfHost) -> bool {
    let mut eaten = host.feed_key(scenarios::ESC.0);
    if !wait_until(|| !host.store.composing()) {
        return check("terminated-commit-reset", false, host.store.full());
    }
    host.store.reset();
    for key in scenarios::typed("nihongo") { eaten &= host.feed_key(key.0); }
    let event_start = read_events(std::process::id()).len();
    eaten &= host.feed_key(scenarios::SPACE.0);
    if !wait_until(|| read_events(std::process::id()).iter().skip(event_start)
        .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) {
        return check("terminated-commit-conversion", false, host.store.preedit());
    }
    let body = host.store.preedit();
    let writes = host.store.text_writes.get();
    host.store.reject_text.set(true);
    eaten &= host.feed_key(scenarios::ENTER.0);
    eaten &= host.feed_key(scenarios::ch('a').0);
    let pending = check("terminated-commit-rejected-with-following-input", eaten && !body.is_empty()
        && host.store.composing() && host.store.full() == body
        && host.store.text_writes.get() == writes, (eaten, host.store.full()));
    let terminated = host.terminate_compositions().is_ok();
    host.store.reject_text.set(false);
    let detached = wait_until(|| !host.store.composing() && read_events(std::process::id()).iter()
        .skip(event_start).any(|event| matches!(event, Ev::ConversionOwnerLost)));
    let ended = check("terminated-commit-retires-owner", terminated && detached
        && host.store.committed() == body && host.store.full() == body,
        (terminated, detached, host.store.committed()));
    eaten &= host.feed_key(scenarios::ENTER.0);
    eaten &= host.feed_key(scenarios::ch('b').0);
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        tsf_host::pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    let held = check("terminated-commit-keeps-detached-input-unwritten", eaten
        && !host.store.composing() && host.store.full() == body
        && host.store.text_writes.get() == writes,
        (eaten, host.store.full(), host.store.text_writes.get() - writes));
    eaten &= host.feed_key(scenarios::ESC.0);
    eaten &= host.feed_key(scenarios::ch('a').0);
    let restarted = wait_until(|| host.store.preedit() == "あ" && host.store.committed() == body);
    let canceled = check("terminated-commit-explicit-discard-allows-new-input", eaten && restarted
        && host.store.full() == format!("{body}あ") && host.store.text_writes.get() == writes + 1,
        (eaten, host.store.committed(), host.store.preedit(), host.store.text_writes.get() - writes));
    pending && ended && held && canceled
}

fn check_reconvert_correction_delivery(host: &TsfHost) -> bool {
    let mut eaten = host.feed_key(scenarios::ESC.0);
    if !wait_until(|| !host.store.composing()) || !host.enter_direct_mode() {
        return check("reconvert-correction-reset", false, host.store.full());
    }
    host.store.reset();
    host.store.seed_committed("React nihongo");
    eaten &= host.feed_key(scenarios::CONVERT.0);
    if !wait_until(|| host.store.composing() && host.candidate_strings().len() >= 2) {
        return check("reconvert-correction-candidates", false, host.candidate_strings());
    }
    let candidates = host.candidate_strings();
    eaten &= host.feed_key(scenarios::DOWN.0);
    let selected = host.candidate_selection() as usize;
    let body = host.store.preedit();
    if selected == 0 || body.is_empty() || candidates.get(selected) != Some(&body) {
        return check("reconvert-correction-selection", false, (selected, body));
    }
    let expected = format!("React {body}");
    let writes = host.store.text_writes.get();
    let event_start = read_events(std::process::id()).len();
    host.store.reject_text.set(true);
    eaten &= host.feed_key(scenarios::ENTER.0);
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        tsf_host::pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    let acknowledged = || read_events(std::process::id()).iter().skip(event_start)
        .any(|event| matches!(event, Ev::ReconvertCorrectionAcknowledged));
    let rejected = check("reconvert-correction-not-sent-before-body", eaten && !acknowledged()
        && host.store.composing() && host.store.preedit() == body
        && host.store.full() == expected && host.store.text_writes.get() == writes,
        (eaten, host.store.full(), host.store.text_writes.get() - writes));
    host.store.reject_text.set(false);
    eaten &= host.feed_key(scenarios::ENTER.0);
    let delivered = wait_until(|| acknowledged() && !host.store.composing());
    let committed = check("reconvert-correction-delivered-after-body", eaten && delivered
        && host.store.preedit().is_empty() && host.store.committed() == expected
        && host.store.full() == expected && host.store.text_writes.get() == writes + 1,
        (eaten, acknowledged(), host.store.committed(), host.store.text_writes.get() - writes));
    rejected && committed
}

fn check_replaced_context(host: &mut TsfHost, after_body: bool) -> bool {
    let mut eaten = host.feed_key(scenarios::ESC.0);
    if !wait_until(|| !host.store.composing()) {
        return check("replaced-context-reset", false, host.store.full());
    }
    host.store.reset();
    for key in scenarios::typed("nihongo") { eaten &= host.feed_key(key.0); }
    let event_start = read_events(std::process::id()).len();
    eaten &= host.feed_key(scenarios::SPACE.0);
    if !wait_until(|| read_events(std::process::id()).iter().skip(event_start)
        .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) {
        return check("replaced-context-conversion", false, host.store.preedit());
    }
    let previous = host.store.clone();
    let body = previous.preedit();
    let writes = previous.text_writes.get();
    let selection_rejections = previous.rejected_selections.get();
    let expected_writes = writes + u32::from(after_body);
    previous.reject_text.set(!after_body);
    previous.reject_selection.set(after_body);
    eaten &= host.feed_key(scenarios::ENTER.0);
    eaten &= host.feed_key(scenarios::ch('a').0);
    let pending = check("replaced-context-rejected-with-following-input", eaten && !body.is_empty()
        && previous.composing() && previous.full() == body && previous.text_writes.get() == expected_writes
        && (!after_body || previous.rejected_selections.get() > selection_rejections),
        (after_body, eaten, previous.full(), previous.text_writes.get() - writes));
    let replacement = "別文書:";
    let replaced = host.replace_document(replacement).is_ok();
    previous.reject_text.set(false);
    previous.reject_selection.set(false);
    let detached = wait_until(|| read_events(std::process::id()).iter().skip(event_start)
        .any(|event| matches!(event, Ev::ConversionOwnerLost)));
    let retired = check("replaced-context-retires-old-owner", replaced && detached
        && !std::rc::Rc::ptr_eq(&previous, &host.store)
        && previous.full() == body && host.store.full() == replacement,
        (after_body, replaced, detached, previous.full(), host.store.full()));
    eaten &= host.feed_key(scenarios::ENTER.0);
    eaten &= host.feed_key(scenarios::ch('b').0);
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        tsf_host::pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    let held = check("replaced-context-never-replays-old-input", eaten
        && previous.full() == body && previous.text_writes.get() == expected_writes
        && host.store.full() == replacement && host.store.text_writes.get() == 0
        && !host.store.composing(),
        (after_body, eaten, previous.full(), host.store.full(), host.store.text_writes.get()));
    eaten &= host.feed_key(scenarios::ESC.0);
    eaten &= host.feed_key(scenarios::ch('a').0);
    let resumed = wait_until(|| host.store.preedit() == "あ" && host.store.committed() == replacement);
    let fresh = check("replaced-context-discard-then-new-input", eaten && resumed
        && host.store.full() == format!("{replacement}あ") && host.store.text_writes.get() == 1
        && previous.full() == body && previous.text_writes.get() == expected_writes,
        (after_body, eaten, previous.full(), host.store.full(), host.store.text_writes.get()));
    pending && retired && held && fresh
}

fn check_reactivated_failed_commit(host: &mut TsfHost) -> bool {
    let mut eaten = host.feed_key(scenarios::ESC.0);
    if !wait_until(|| !host.store.composing()) {
        return check("reactivated-commit-reset", false, host.store.full());
    }
    host.store.reset();
    for key in scenarios::typed("nihongo") { eaten &= host.feed_key(key.0); }
    let event_start = read_events(std::process::id()).len();
    eaten &= host.feed_key(scenarios::SPACE.0);
    if !wait_until(|| read_events(std::process::id()).iter().skip(event_start)
        .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) {
        return check("reactivated-commit-conversion", false, host.store.preedit());
    }
    let body = host.store.preedit();
    let writes = host.store.text_writes.get();
    host.store.reject_text.set(true);
    eaten &= host.feed_key(scenarios::ENTER.0);
    eaten &= host.feed_key(scenarios::ch('a').0);
    let pending = check("reactivated-commit-rejected-with-following-input", eaten && !body.is_empty()
        && host.store.composing() && host.store.full() == body && host.store.text_writes.get() == writes,
        (eaten, host.store.full()));
    let deactivated = host.deactivate().is_ok();
    let inactive_key_passed = !host.feed_key(scenarios::ch('b').0);
    host.store.reject_text.set(false);
    let reactivated = host.reactivate().is_ok();
    let lifecycle = wait_until(|| {
        let events = read_events(std::process::id());
        let events = &events[event_start.min(events.len())..];
        events.iter().any(|event| matches!(event, Ev::ConversionOwnerLost))
            && events.iter().any(|event| matches!(event, Ev::Activate))
    });
    let restarted = check("reactivated-commit-retires-old-owner", deactivated && inactive_key_passed
        && reactivated && lifecycle && host.store.full() == body,
        (deactivated, inactive_key_passed, reactivated, lifecycle, host.store.full()));
    eaten &= host.feed_key(scenarios::ENTER.0);
    eaten &= host.feed_key(scenarios::ch('b').0);
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        tsf_host::pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    let held = check("reactivated-commit-does-not-replay-old-input", eaten
        && host.store.full() == body && host.store.text_writes.get() == writes,
        (eaten, host.store.full(), host.store.text_writes.get() - writes));
    eaten &= host.feed_key(scenarios::ESC.0);
    eaten &= host.feed_key(scenarios::ch('a').0);
    let resumed = wait_until(|| host.store.preedit() == "あ" && host.store.committed() == body);
    let fresh = check("reactivated-commit-discard-then-new-input", eaten && resumed
        && host.store.full() == format!("{body}あ") && host.store.text_writes.get() == writes + 1,
        (eaten, host.store.full(), host.store.text_writes.get() - writes));
    pending && restarted && held && fresh
}

fn check_mode_toggle_commit(host: &TsfHost, relay: &crate::receipt_relay::ReceiptRelay) -> bool {
    use ipc::clause::ReceiptStatus;
    let hook = match DeferredCommitHook::load() {
        Ok(hook) => hook,
        Err(error) => return check("mode-toggle-hook", false, error),
    };
    for case in 0..4 {
        if !host.normalize_native_mode() { return check("mode-toggle-native-setup", false, case); }
        host.feed_key(scenarios::ESC.0);
        if !wait_until(|| !host.store.composing()) { return check("mode-toggle-reset", false, host.store.full()); }
        host.store.reset();
        for key in scenarios::typed("nihongo") { host.feed_key(key.0); }
        let start = read_events(std::process::id()).len();
        host.feed_key(scenarios::SPACE.0);
        if !wait_until(|| read_events(std::process::id()).iter().skip(start)
            .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) { return check("mode-toggle-initial", false, host.store.preedit()); }
        host.feed_key(scenarios::SPACE.0);
        if !wait_until(|| host.candidate_strings().len() >= 2) { return check("mode-toggle-candidates", false, host.store.preedit()); }
        host.feed_key(scenarios::SPACE.0);
        let body = host.store.preedit();
        let writes = host.store.text_writes.get();
        let receipt_count = relay.observations().len();
        let event_start = read_events(std::process::id()).len();
        let rejected_selections = host.store.rejected_selections.get();
        if case == 1 { host.store.reject_text.set(true); }
        if case == 2 && hook.command(1) != 0 { return check("mode-toggle-defer-arm", false, case); }
        if case == 3 { host.store.reject_selection.set(true); }
        let invoked = host.langbar_toggle_mode();
        if invoked.is_err() { return check("mode-toggle-langbar-command", false, invoked); }
        if case != 0 {
            let no_toggle = !read_events(std::process::id()).iter().skip(event_start)
                .any(|event| matches!(event, Ev::ModeToggle { direct: true, .. }));
            let write_count = host.store.text_writes.get() - writes;
            let blocked = check("mode-toggle-waits-for-commit-and-repair", no_toggle
                && write_count == u32::from(case == 3) && host.store.full() == body
                && (case != 2 || hook.command(2) == 1)
                && (case != 3 || host.store.rejected_selections.get() > rejected_selections), (case, no_toggle, write_count, host.store.full()));
            host.store.reject_text.set(false);
            host.store.reject_selection.set(false);
            if !blocked { return false; }
            let inserted = host.feed_key_no_pump(scenarios::ch('a').0);
            if case == 2 {
                if hook.command(3) < 0 || hook.command(4) != 1 { return check("mode-toggle-deferred-release", false, hook.command(4)); }
            } else { host.feed_key(scenarios::ENTER.0); }
            let restored = wait_until(|| host.store.committed() == body && host.store.preedit() == "あ");
            if !check("mode-toggle-retry-retains-native-mode-and-next-input", inserted && restored
                && host.store.text_writes.get() == writes + 2
                && host.effective_mode_is_direct() == Some(false), (case, host.store.full())) { return false; }
        } else if !check("mode-toggle-switches-only-after-body", host.store.committed() == body
            && !host.store.composing() && host.effective_mode_is_direct() == Some(true), host.store.full()) { return false; }
        let delivered = wait_until(|| relay.observations().len() == receipt_count + 1);
        let receipts = relay.observations();
        let receipt_ok = receipts.get(receipt_count).is_some_and(|(receipt, ack)| receipt.reading == "にほんご"
            && receipt.text == body && receipt.intervals.len() == 1
            && receipt.intervals[0].reading_start.0 == 0 && receipt.intervals[0].reading_end.0 == 4
            && matches!(ack, Some((id, ReceiptStatus::Applied)) if *id == receipt.commit_id));
        if !check("mode-toggle-commits-through-receipt-path", delivered && receipt_ok,
            (case, receipts.len(), receipt_ok)) { return false; }
    }
    true
}

fn check_learning_clear_receipt(host: &TsfHost, relay: &crate::receipt_relay::ReceiptRelay, root: &std::path::Path) -> bool {
    use ipc::clause::{IntervalLearning, ReceiptRejection, ReceiptStatus};
    let mut passed = true;
    for stage in 0..3 {
        host.feed_key(scenarios::ESC.0);
        if !wait_until(|| !host.store.composing()) { return check("clear-receipt-reset", false, host.store.full()); }
        host.store.reset();
        let mut eaten = true;
        for key in scenarios::typed("nihongo") { eaten &= host.feed_key(key.0); }
        let start = read_events(std::process::id()).len();
        eaten &= host.feed_key(scenarios::SPACE.0);
        if !wait_until(|| read_events(std::process::id()).iter().skip(start)
            .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) {
            return check("clear-receipt-initial", false, host.store.preedit());
        }
        eaten &= host.feed_key(scenarios::SPACE.0);
        if !wait_until(|| host.candidate_strings().len() >= 2) { return check("clear-receipt-candidates", false, host.store.preedit()); }
        // Prefer a kanji alternative so the successful control has material to persist.
        let limit = host.candidate_strings().len();
        for _ in 0..limit {
            eaten &= host.feed_key(scenarios::SPACE.0);
            if host.store.preedit().chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)) { break; }
        }
        let body = host.store.preedit();
        eaten &= host.feed_key(scenarios::RIGHT.0);
        if stage == 1 { relay.arm_clear_before_receipt(); }
        let event_start = read_events(std::process::id()).len();
        eaten &= host.feed_key_no_pump(scenarios::ENTER.0);
        eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
        let complete = wait_until(|| relay.observations().len() == stage + 1
            && host.store.committed() == body && host.store.preedit() == "あ");
        let observations = relay.observations();
        if !check("clear-receipt-commit-keeps-next-input", eaten && complete && !body.is_empty()
            && host.store.full() == format!("{body}あ"), (stage, observations.len(), host.store.full())) { return false; }
        let (receipt, ack) = &observations[stage];
        let payload = receipt.reading == "にほんご" && receipt.text == body && receipt.intervals.len() == 1
            && receipt.intervals[0].reading_start.0 == 0 && receipt.intervals[0].reading_end.0 == 4
            && matches!(receipt.intervals[0].learning, IntervalLearning::Candidate { explicitly_selected: true, .. });
        if stage == 1 {
            let notice = wait_until(|| read_events(std::process::id()).iter().skip(event_start)
                .any(|event| matches!(event, Ev::ReceiptStaleLearningGeneration)));
            passed &= check("clear-receipt-old-outbox-rejected-without-rewriting", notice && payload
                && relay.preclear_receipt().as_ref() == Some(receipt)
                && receipt.engine_epoch == observations[0].0.engine_epoch
                && receipt.learning_generation == observations[0].0.learning_generation
                && matches!(ack, Some((id, ReceiptStatus::Rejected { reason: ReceiptRejection::StaleLearningGeneration })) if *id == receipt.commit_id),
                (notice, receipt.learning_generation, ack));
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline { tsf_host::pump(); std::thread::sleep(Duration::from_millis(10)); }
            let cleared = learning_files(root);
            passed &= check("clear-receipt-removes-history-without-partial-retry", relay.observations().len() == 2
                && cleared.as_ref().is_ok_and(|files| files.is_empty()) && host.store.committed() == body
                && host.store.preedit() == "あ", (cleared.as_ref().map(|files| files.len()), host.store.full()));
        } else {
            let fresh = stage == 0 || (receipt.engine_epoch == observations[1].0.engine_epoch
                && receipt.learning_generation > observations[1].0.learning_generation
                && receipt.commit_id.client_instance == observations[1].0.commit_id.client_instance
                && receipt.commit_id.sequence > observations[1].0.commit_id.sequence);
            passed &= check("clear-receipt-live-generation-applied", payload && fresh
                && matches!(ack, Some((id, ReceiptStatus::Applied)) if *id == receipt.commit_id),
                (stage, receipt.learning_generation, ack));
            let persisted = wait_until(|| learning_files(root).is_ok_and(|files| !files.is_empty()));
            passed &= check("clear-receipt-learning-persists-before-and-after-clear", persisted
                && host.store.committed() == body && host.store.preedit() == "あ",
                (stage, learning_files(root).as_ref().map(|files| files.len())));
        }
        if !passed { return false; }
    }
    passed
}

fn prepare_local_edit(host: &TsfHost) -> Option<String> {
    for _ in 0..4 {
        if !host.store.composing() { break; }
        host.feed_key(scenarios::ESC.0);
    }
    if !wait_until(|| !host.store.composing()) { return None; }
    host.store.reset();
    for key in scenarios::typed("kyouhaiitenkidesu") { host.feed_key(key.0); }
    let start = read_events(std::process::id()).len();
    host.feed_key(scenarios::SPACE.0);
    if !wait_until(|| read_events(std::process::id()).iter().skip(start)
        .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) { return None; }
    let body = host.store.preedit();
    body.strip_suffix("天気です").map(str::to_owned)
}

fn check_backspace_escape(host: &TsfHost, relay: &crate::receipt_relay::ReceiptRelay) -> bool {
    use ipc::{clause::{IntervalLearning, NoLearningReason}, protocol::Request};
    let Some(prefix) = prepare_local_edit(host) else { return check("local-edit-initial", false, host.store.full()); };
    let requests = relay.clause_requests().len();
    let mut eaten = true;
    let original = host.store.preedit();
    let selection = host.store.selection();
    eaten &= host.feed_key(0x2E);
    if !check("converting-delete-keeps-surface-without-receipt", eaten
        && host.store.preedit() == original && host.store.committed().is_empty()
        && host.store.composing() && host.store.selection() == selection
        && relay.clause_requests().len() == requests && relay.observations().is_empty(),
        host.store.full()) { return false; }
    for _ in 0..3 { eaten &= host.feed_key(scenarios::BACK.0); }
    if !check("backspace-last-surface-keeps-selected-prefix", eaten && host.store.preedit() == format!("{prefix}天")
        && relay.clause_requests().len() == requests && relay.observations().is_empty(), host.store.full()) { return false; }
    host.feed_key(scenarios::RIGHT.0);
    host.feed_key(scenarios::SPACE.0);
    let ready = wait_until(|| host.candidate_strings().len() >= 2);
    let original_reading = relay.clause_requests().iter().rev().find_map(|request| {
        if let Request::ClauseCandidates(request) = request { Some(request.reading == "きょうはいいてんきです"
            && request.reading_start.0 == 6 && request.reading_end.0 == 11) } else { None }
    }).unwrap_or(false);
    host.feed_key(scenarios::HOME.0);
    if !check("backspace-space-restores-original-reading-candidate", ready && original_reading
        && host.store.preedit() == format!("{prefix}天気です"), host.store.full()) { return false; }
    for _ in 0..3 { host.feed_key(scenarios::BACK.0); }
    host.feed_key_no_pump(scenarios::ENTER.0);
    host.feed_key_no_pump(scenarios::ch('a').0);
    let committed = wait_until(|| host.store.committed() == format!("{prefix}天") && host.store.preedit() == "あ"
        && relay.observations().len() == 1);
    let receipt_ok = relay.observations().first().is_some_and(|(receipt, _)| receipt.reading == "きょうはいいてんきです"
        && receipt.intervals.last().is_some_and(|interval| matches!(interval.learning, IntervalLearning::None { reason: NoLearningReason::Invalidated })));
    if !check("backspace-commit-keeps-reading-invalidates-tail-learning", committed && receipt_ok, (receipt_ok, host.store.full())) { return false; }

    let Some(_) = prepare_local_edit(host) else { return check("empty-tail-initial", false, host.store.full()); };
    for _ in 0..4 { host.feed_key(scenarios::BACK.0); }
    host.feed_key(scenarios::ESC.0);
    host.feed_key(scenarios::ESC.0);
    host.feed_key(scenarios::ch('a').0);
    if !check("empty-tail-removes-reading-without-resurrection", host.store.preedit() == "きょうはいいあ"
        && host.store.committed().is_empty(), host.store.full()) { return false; }

    let Some(prefix) = prepare_local_edit(host) else { return check("escape-initial", false, host.store.full()); };
    let original = host.store.preedit();
    host.feed_key(scenarios::SPACE.0);
    if !wait_until(|| !host.candidate_strings().is_empty()) { return check("escape-window", false, host.store.full()); }
    host.feed_key(scenarios::ESC.0);
    if !check("escape-first-closes-window-only", host.store.preedit() == original && host.candidate_strings().is_empty(), host.store.full()) { return false; }
    host.feed_key(scenarios::ESC.0);
    if !check("escape-second-restores-selected-reading", host.store.preedit() == "きょうはいい天気です", host.store.full()) { return false; }
    host.feed_key(scenarios::RIGHT.0);
    host.feed_key(scenarios::ESC.0);
    if !check("escape-movement-retains-stage-then-editing", host.store.preedit() == "きょうはいいてんきです"
        && host.store.composing() && host.store.committed().is_empty(), host.store.full()) { return false; }
    host.feed_key(scenarios::ESC.0);
    if !check("escape-fourth-cancels", !host.store.composing() && host.store.full().is_empty(), host.store.full()) { return false; }

    let Some(_) = prepare_local_edit(host) else { return check("redraw-initial", false, host.store.full()); };
    host.store.reject_text.set(true);
    eaten = host.feed_key(scenarios::BACK.0);
    eaten &= host.feed_key_no_pump(scenarios::ENTER.0);
    eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
    let retained = host.store.preedit() == original && host.store.committed().is_empty();
    host.store.reject_text.set(false);
    let repaired = wait_until(|| host.store.committed() == format!("{prefix}天気で") && host.store.preedit() == "あ");
    if !check("backspace-refused-display-retries-once-before-commit", eaten && retained && repaired, host.store.full()) { return false; }

    let Some(_) = prepare_local_edit(host) else { return check("escape-redraw-initial", false, host.store.full()); };
    host.feed_key(scenarios::ESC.0); // selected reading
    host.store.reject_text.set(true);
    host.feed_key(scenarios::ESC.0); // all reading: retained for display retry
    host.feed_key_no_pump(scenarios::F7.0);
    host.feed_key_no_pump(scenarios::ENTER.0);
    host.feed_key_no_pump(scenarios::ch('a').0);
    host.store.reject_text.set(false);
    let katakana = wait_until(|| host.store.committed() == "キョウハイイテンキデス" && host.store.preedit() == "あ");
    if !check("escape-refused-display-preserves-next-notation-and-commit", katakana && host.store.composing(), host.store.full()) { return false; }

    let Some(_) = prepare_local_edit(host) else { return check("last-grapheme-initial", false, host.store.full()); };
    let count = host.store.preedit().chars().count();
    for _ in 1..count { host.feed_key(scenarios::BACK.0); }
    let last = host.store.preedit();
    let receipts = relay.observations().len();
    host.store.reject_text.set(true);
    eaten = host.feed_key(scenarios::BACK.0);
    let retained = host.store.preedit() == last && last.chars().count() == 1;
    host.store.reject_text.set(false);
    eaten &= host.feed_key(scenarios::ENTER.0); // retry the retained BS, not a commit
    let cancelled = wait_until(|| !host.store.composing());
    check("last-grapheme-refused-cancel-retains-backspace", eaten && retained && cancelled
        && host.store.full().is_empty() && relay.observations().len() == receipts, host.store.full())
}

fn check_boundary_resize(host: &TsfHost, relay: &mut crate::receipt_relay::ReceiptRelay, restart: bool) -> bool {
    let reading = "きょうはいいてんきです";
    for key in scenarios::typed("kyouhaiitenkidesu") { host.feed_key(key.0); }
    let start = read_events(std::process::id()).len();
    host.feed_key(scenarios::SPACE.0);
    if !wait_until(|| read_events(std::process::id()).iter().skip(start)
        .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) { return check("boundary-initial", false, host.store.preedit()); }
    let requests = relay.clause_requests().len();
    let shifted = host.feed_key_with_shift(0x25);
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline { tsf_host::pump(); std::thread::sleep(Duration::from_millis(10)); }
    if !check("boundary-resize-keeps-reading-without-request", shifted && host.store.preedit() == reading
        && host.store.committed().is_empty() && relay.clause_requests().len() == requests,
        (host.store.full(), requests, relay.clause_requests().len())) { return false; }
    if restart {
        let restarted = relay.restart_engine();
        if !check("boundary-restarts-owned-engine", restarted.as_ref().is_ok_and(|(old, new, epoch)| old != new && *epoch)
            && host.store.preedit() == reading && host.store.committed().is_empty(), restarted) { return false; }
    }
    host.feed_key_no_pump(scenarios::SPACE.0);
    host.feed_key_no_pump(0x23); // End
    host.feed_key_no_pump(scenarios::HOME.0);
    let digit = host.feed_key_no_pump(scenarios::digit(1).0);
    if !check("boundary-loading-rejects-unavailable-digit", digit && host.store.preedit() == reading && host.store.committed().is_empty(), host.store.full()) { return false; }
    let completed = wait_until(|| relay.clause_requests().iter().any(|request| matches!(request, ipc::protocol::Request::ConvertClauses(_)))
        && host.store.preedit() != reading);
    let requests = relay.clause_requests();
    let conversion = requests.iter().find_map(|request| if let ipc::protocol::Request::ConvertClauses(request) = request { Some(request) } else { None });
    let ranges_ok = conversion.is_some_and(|request| request.reading == reading && request.clauses.len() == 2
        && request.clauses[0].reading_start.0 == 0 && request.clauses[0].reading_end.0 == 5
        && request.clauses[1].reading_start.0 == 5 && request.clauses[1].reading_end.0 == 11);
    if !check("boundary-space-converts-exact-new-partition", completed && ranges_ok && host.store.committed().is_empty(),
        (ranges_ok, host.store.full())) { return false; }
    if restart {
        let conversions = requests.iter().filter_map(|request| if let ipc::protocol::Request::ConvertClauses(request) = request { Some(request) } else { None }).collect::<Vec<_>>();
        let rebased = conversions.len() == 2 && conversions[0].reading == conversions[1].reading
            && conversions[0].clauses == conversions[1].clauses
            && conversions[0].key.identity.connection_generation != conversions[1].key.identity.connection_generation;
        if !check("boundary-rebaseline-preserves-new-partition", rebased, conversions.len()) { return false; }
    }
    host.feed_key(scenarios::SPACE.0);
    let ready = wait_until(|| !host.candidate_strings().is_empty());
    let first_selected = relay.clause_requests().iter().rev().find_map(|request| {
        if let ipc::protocol::Request::ClauseCandidates(request) = request {
            Some(request.reading_start.0 == 0 && request.reading_end.0 == 5)
        } else { None }
    }).unwrap_or(false);
    if !check("boundary-queued-end-home-keeps-sequential-movement", ready && first_selected, host.store.full()) { return false; }
    host.feed_key(scenarios::ESC.0);
    let body = host.store.preedit();
    host.feed_key_no_pump(scenarios::ENTER.0);
    host.feed_key_no_pump(scenarios::ch('a').0);
    let committed = wait_until(|| host.store.committed() == body && host.store.preedit() == "あ" && relay.observations().len() == 1);
    let payload = relay.observations().first().is_some_and(|(receipt, ack)| receipt.reading == reading && receipt.text == body
        && receipt.intervals.len() == 2 && receipt.intervals[0].reading_start.0 == 0 && receipt.intervals[0].reading_end.0 == 5
        && receipt.intervals[1].reading_start.0 == 5 && receipt.intervals[1].reading_end.0 == 11
        && matches!(ack, Some((id, ipc::clause::ReceiptStatus::Applied)) if *id == receipt.commit_id));
    check("boundary-commit-keeps-full-reading-and-next-input", committed && payload, (payload, host.store.full()))
}

fn check_learning_toggle(host: &TsfHost, relay: &crate::receipt_relay::ReceiptRelay, root: &std::path::Path, settings: &std::path::Path) -> bool {
    use ipc::clause::{IntervalLearning, NoLearningReason, ReceiptStatus};
    use windows::{core::{s, w}, Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress}};
    let function = unsafe { GetModuleHandleW(w!("nospacekey_tip.dll")) }.ok()
        .and_then(|module| unsafe { GetProcAddress(module, s!("NospacekeyTestReloadConfiguration")) });
    let Some(function) = function else { return check("learning-toggle-hook", false, "missing export"); };
    let configure: unsafe extern "system" fn(u32) -> u64 = unsafe { std::mem::transmute(function) };
    let mut generation = 0;
    for stage in 0..2 {
        host.feed_key(scenarios::ESC.0);
        if !wait_until(|| !host.store.composing()) { return check("learning-toggle-reset", false, host.store.full()); }
        host.store.reset();
        for key in scenarios::typed("nihongo") { host.feed_key(key.0); }
        let start = read_events(std::process::id()).len();
        host.feed_key(scenarios::SPACE.0);
        if !wait_until(|| read_events(std::process::id()).iter().skip(start)
            .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) { return check("learning-toggle-initial", false, host.store.preedit()); }
        host.feed_key(scenarios::SPACE.0);
        if !wait_until(|| host.candidate_strings().len() >= 2) { return check("learning-toggle-candidates", false, host.store.preedit()); }
        for _ in 0..host.candidate_strings().len() {
            host.feed_key(scenarios::SPACE.0);
            if host.store.preedit().chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)) { break; }
        }
        let body = host.store.preedit();
        host.feed_key(scenarios::RIGHT.0);
        if stage == 0 {
            generation = unsafe { configure(1) };
            if generation == 0 || generation == u64::MAX { return check("learning-toggle-original-generation", false, generation); }
            let requests = relay.clause_requests().len();
            let writes = host.store.text_writes.get();
            for enabled in [false, true] {
                let config = format!(r#"{{"version":2,"zenzai":{{"enabled":false}},"learning":{{"enabled":{enabled}}},"default_direct":false,"live_conversion":{{"enabled":false}}}}"#);
                if std::fs::write(settings.join("settings.json"), config).is_err() || unsafe { configure(0) } != 0 {
                    return check("learning-toggle-reload", false, enabled);
                }
                let configured = wait_until(|| { let next = unsafe { configure(1) }; next > generation && next != u64::MAX });
                let next = unsafe { configure(1) };
                if !check("learning-toggle-configured-without-conversion-keeps-preedit", configured
                    && relay.clause_requests().len() == requests && host.store.text_writes.get() == writes
                    && host.store.preedit() == body && host.store.committed().is_empty(),
                    (enabled, generation, next, host.store.full())) { return false; }
                generation = next;
            }
        }
        let mut eaten = host.feed_key_no_pump(scenarios::ENTER.0);
        eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
        let delivered = wait_until(|| relay.observations().len() == stage + 1 && host.store.committed() == body && host.store.preedit() == "あ");
        let observations = relay.observations();
        let payload = observations.get(stage).is_some_and(|(receipt, ack)| {
            receipt.learning_generation == generation && receipt.reading == "にほんご" && receipt.text == body
                && receipt.intervals.len() == 1 && receipt.intervals[0].reading_start.0 == 0
                && receipt.intervals[0].reading_end.0 == 4
                && if stage == 0 {
                    receipt.sentence_token.is_none() && matches!(receipt.intervals[0].learning,
                        IntervalLearning::None { reason: NoLearningReason::Invalidated })
                } else { matches!(receipt.intervals[0].learning, IntervalLearning::Candidate { explicitly_selected: true, .. }) }
                && matches!(ack, Some((id, ReceiptStatus::Applied)) if *id == receipt.commit_id)
        });
        if !check("learning-toggle-old-material-stays-invalid-new-material-applied", eaten && delivered && payload,
            (stage, payload, host.store.full())) { return false; }
        if stage == 0 {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline { crate::tsf_host::pump(); std::thread::sleep(Duration::from_millis(10)); }
            if !check("learning-toggle-old-material-does-not-persist", learning_files(root).is_ok_and(|files| files.is_empty()), host.store.full()) { return false; }
        } else {
            if !check("learning-toggle-new-learning-persists", wait_until(|| learning_files(root).is_ok_and(|files| !files.is_empty()))
                && host.store.committed() == body && host.store.preedit() == "あ", host.store.full()) { return false; }
        }
    }
    true
}

fn check_learning_clear(host: &TsfHost, relay: &crate::receipt_relay::ReceiptRelay, root: &std::path::Path) -> bool {
    use ipc::clause::{IntervalLearning, NoLearningReason, ReceiptStatus};
    let mut eaten = true;
    for key in scenarios::typed("kyouhaiitenkidesu") { eaten &= host.feed_key(key.0); }
    let start = read_events(std::process::id()).len();
    eaten &= host.feed_key(scenarios::SPACE.0);
    if !wait_until(|| read_events(std::process::id()).iter().skip(start)
        .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) {
        return check("learning-clear-initial", false, host.store.preedit());
    }
    eaten &= host.feed_key(scenarios::SPACE.0);
    if !wait_until(|| host.candidate_strings().len() >= 2) { return check("learning-clear-first-candidates", false, host.store.preedit()); }
    eaten &= host.feed_key(scenarios::SPACE.0);
    let prefix = host.candidate_strings().get(host.candidate_selection() as usize).cloned().unwrap_or_default();
    let body = host.store.preedit();
    eaten &= host.feed_key(scenarios::RIGHT.0);
    if !check("learning-clear-old-explicit-candidate-retained", eaten && !prefix.is_empty()
        && body == format!("{prefix}天気です") && host.store.preedit() == body
        && host.store.committed().is_empty() && host.candidate_strings().is_empty(), (&prefix, &body)) { return false; }
    let (before, after) = match relay.clear_learning() {
        Ok(identities) => identities,
        Err(error) => return check("learning-clear-native-command", false, error),
    };
    let mut passed = check("learning-clear-advances-generation-preserves-preedit", before.engine_epoch == after.engine_epoch
        && after.learning_generation > before.learning_generation && host.store.preedit() == body
        && host.store.committed().is_empty(), (before.learning_generation, after.learning_generation, host.store.preedit()));
    let cleared = learning_files(root);
    passed &= check("learning-clear-storage-empty-before-new-commit", cleared.as_ref().is_ok_and(|files| files.is_empty()),
        cleared.as_ref().map(|files| files.len()));
    eaten &= host.feed_key(scenarios::SPACE.0);
    let ready = wait_until(|| host.candidate_strings().len() >= 2);
    passed &= check("learning-clear-new-generation-candidates-preserve-other-clause", eaten && ready
        && host.store.preedit() == body && host.store.committed().is_empty(), (host.store.preedit(), host.candidate_strings()));
    if !passed { return false; }
    eaten &= host.feed_key(scenarios::SPACE.0);
    let selected = host.candidate_strings().get(host.candidate_selection() as usize).cloned().unwrap_or_default();
    let committed_body = format!("{prefix}{selected}");
    eaten &= host.feed_key(scenarios::RIGHT.0);
    eaten &= host.feed_key_no_pump(scenarios::ENTER.0);
    eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
    let committed = wait_until(|| host.store.committed() == committed_body && host.store.preedit() == "あ"
        && relay.observations().len() == 1);
    let observations = relay.observations();
    let payload = observations.first().is_some_and(|(receipt, ack)| {
        receipt.engine_epoch == after.engine_epoch && receipt.learning_generation == after.learning_generation
            && receipt.reading == "きょうはいいてんきです" && receipt.text == committed_body
            && receipt.sentence_token.is_none() && receipt.intervals.len() == 2
            && receipt.intervals[0].reading_start.0 == 0 && receipt.intervals[0].reading_end.0 > 0
            && receipt.intervals[0].reading_end == receipt.intervals[1].reading_start
            && receipt.intervals[1].reading_end.0 as usize == receipt.reading.chars().count()
            && receipt.intervals[0].surface == prefix && receipt.intervals[1].surface == selected
            && matches!(receipt.intervals[0].learning, IntervalLearning::None { reason: NoLearningReason::Invalidated })
            && matches!(receipt.intervals[1].learning, IntervalLearning::Candidate { explicitly_selected: true, .. })
            && matches!(ack, Some((id, ReceiptStatus::Applied)) if *id == receipt.commit_id)
    });
    passed &= check("learning-clear-mixed-receipt-uses-only-new-material", eaten && committed && observations.len() == 1 && payload,
        (observations.len(), payload, host.store.full()));
    let persisted = wait_until(|| learning_files(root).is_ok_and(|files| !files.is_empty()));
    passed &= check("learning-clear-new-learning-persists-without-changing-input", persisted
        && host.store.committed() == committed_body && host.store.preedit() == "あ"
        && host.store.full() == format!("{committed_body}あ"), host.store.full());
    passed
}

struct DeferredCommitHook(unsafe extern "system" fn(u32) -> i32);
impl DeferredCommitHook {
    fn load() -> windows::core::Result<Self> {
        use windows::{core::{s, w}, Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress}};
        let module = unsafe { GetModuleHandleW(w!("nospacekey_tip.dll")) }?;
        let function = unsafe { GetProcAddress(module, s!("NospacekeyTestDeferredCommit")) }
            .ok_or_else(windows::core::Error::from_thread)?;
        Ok(Self(unsafe { std::mem::transmute(function) }))
    }
    fn command(&self, command: u32) -> i32 { unsafe { (self.0)(command) } }
}
impl Drop for DeferredCommitHook {
    fn drop(&mut self) { self.command(0); }
}

fn check_deferred_commit(host: &TsfHost, relay: &crate::receipt_relay::ReceiptRelay) -> bool {
    let hook = match DeferredCommitHook::load() {
        Ok(hook) => hook,
        Err(error) => return check("deferred-hook-loaded", false, error),
    };
    let mut passed = true;
    for case in 0..3 {
        let expire = case != 0;
        host.feed_key(scenarios::ESC.0);
        if !wait_until(|| !host.store.composing()) { return check("deferred-reset", false, host.store.full()); }
        host.store.reset();
        let mut eaten = true;
        for key in scenarios::typed("nihongo") { eaten &= host.feed_key(key.0); }
        let start = read_events(std::process::id()).len();
        eaten &= host.feed_key(scenarios::SPACE.0);
        if !wait_until(|| read_events(std::process::id()).iter().skip(start)
            .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) {
            return check("deferred-conversion", false, host.store.preedit());
        }
        let body = host.store.preedit();
        let writes = host.store.text_writes.get();
        let receipts = relay.observations().len();
        if hook.command(1) != 0 { return check("deferred-arm", false, expire); }
        eaten &= host.feed_key_no_pump(scenarios::ENTER.0);
        eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
        if !check("deferred-body-and-next-input-held", eaten && hook.command(2) == 1
            && !body.is_empty() && host.store.preedit() == body && host.store.committed().is_empty()
            && host.store.text_writes.get() == writes && relay.observations().len() == receipts,
            (expire, eaten, &body, host.store.full(), hook.command(2))) { return false; }
        if !expire {
            let released = hook.command(3);
            // The COM call writes the body before the STA consumer replays the queued a.
            passed &= check("deferred-release-writes-body-once", released >= 0 && hook.command(4) == 1
                && host.store.text_writes.get() == writes + 1 && host.store.committed() == body,
                (released, host.store.full(), host.store.text_writes.get() - writes));
            let complete = wait_until(|| host.store.committed() == body && host.store.preedit() == "あ"
                && relay.observations().len() == receipts + 1);
            passed &= check("deferred-consumer-replays-next-composition", complete
                && host.store.full() == format!("{body}あ") && hook.command(2) == 0,
                (host.store.full(), relay.observations().len()));
        } else {
            let deadline = Instant::now() + Duration::from_millis(1300);
            while Instant::now() < deadline { tsf_host::pump(); std::thread::sleep(Duration::from_millis(10)); }
            passed &= check("deferred-expiry-preserves-unwritten-body", host.store.preedit() == body
                && host.store.committed().is_empty() && host.store.text_writes.get() == writes
                && relay.observations().len() == receipts && hook.command(2) == 1, host.store.full());
            if case == 1 {
                eaten &= host.feed_key(scenarios::ENTER.0);
                let retried = wait_until(|| host.store.committed() == body && host.store.preedit() == "あ"
                    && relay.observations().len() == receipts + 1);
                passed &= check("deferred-timeout-retry-preserves-accepted-input", eaten && retried
                    && host.store.full() == format!("{body}あ"), host.store.full());
                let retry_writes = host.store.text_writes.get();
                let released = hook.command(3);
                tsf_host::pump();
                passed &= check("deferred-old-execution-after-retry-cannot-duplicate-body", released < 0 && hook.command(4) == 1
                    && host.store.full() == format!("{body}あ") && host.store.preedit() == "あ"
                    && host.store.text_writes.get() == retry_writes && relay.observations().len() == receipts + 1,
                    (released, host.store.full(), host.store.text_writes.get() - retry_writes));
                if !passed { return false; }
                continue;
            }
            eaten &= host.feed_key(scenarios::ESC.0);
            if !wait_until(|| !host.store.composing()) { return check("deferred-old-composition-ended", false, host.store.full()); }
            for key in scenarios::typed("ka") { eaten &= host.feed_key(key.0); }
            if !wait_until(|| host.store.preedit() == "か") { return check("deferred-new-composition", false, host.store.full()); }
            let new_body = host.store.full();
            let new_writes = host.store.text_writes.get();
            let released = hook.command(3);
            tsf_host::pump();
            passed &= check("deferred-old-first-execution-cannot-touch-new-composition", eaten && released < 0 && hook.command(4) == 1
                && host.store.full() == new_body && host.store.preedit() == "か"
                && host.store.text_writes.get() == new_writes && relay.observations().len() == receipts,
                (released, host.store.full(), host.store.text_writes.get() - new_writes));
        }
        if !passed { return false; }
    }
    passed
}

fn check_rebaseline(host: &TsfHost, relay: &mut crate::receipt_relay::ReceiptRelay, restart: bool) -> bool {
    use ipc::protocol::Request;
    let mut eaten = true;
    let event_start = read_events(std::process::id()).len();
    for key in scenarios::typed("kyouhaiitenkidesu") { eaten &= host.feed_key(key.0); }
    eaten &= host.feed_key(scenarios::SPACE.0);
    if !wait_until(|| read_events(std::process::id()).iter().skip(event_start)
        .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) {
        return check("rebaseline-initial", false, host.store.full());
    }
    eaten &= host.feed_key(scenarios::F7.0);
    eaten &= host.feed_key(scenarios::RIGHT.0);
    let before = host.store.preedit();
    let prefix = "キョウハイイ";
    if !check("rebaseline-local-prefix-setup", eaten && before == format!("{prefix}天気です")
        && host.store.committed().is_empty(), &before) { return false; }
    if restart {
        let restarted = relay.restart_engine();
        if !check("rebaseline-native-engine-restarted", restarted.as_ref().is_ok_and(|(old, new, epoch_changed)| old != new && *epoch_changed),
            format!("{restarted:?}")) { return false; }
    }
    eaten &= host.feed_key(scenarios::SPACE.0);
    let awaiting_interval = wait_until(|| relay.clause_requests().iter().any(|request| matches!(request, Request::ConvertClauses(_))));
    let preserved = check("rebaseline-initial-response-keeps-local-surface", awaiting_interval
        && host.store.preedit() == before && host.store.committed().is_empty(), host.store.full());
    relay.allow_conversion();
    let ready = wait_until(|| host.candidate_strings().len() >= 2);
    let requests = relay.clause_requests();
    let candidates: Vec<_> = requests.iter().filter_map(|request| if let Request::ClauseCandidates(r) = request { Some(r) } else { None }).collect();
    let conversion = requests.iter().find_map(|request| if let Request::ConvertClauses(r) = request { Some(r) } else { None });
    let snapshot = requests.iter().rev().find(|request| matches!(request, Request::LiveSnapshot { .. }));
    let consistent = candidates.len() == 2 && conversion.is_some_and(|conversion| {
        let old = candidates[0];
        let new = candidates[1];
        (restart || conversion.key.baseline != old.key.baseline) && conversion.key.identity.connection_generation > old.key.identity.connection_generation
            && conversion.reading == "きょうはいいてんきです"
            && conversion.clauses.len() == 1 && conversion.clauses[0].id == old.key.clause_id
            && conversion.clauses[0].reading_start == old.reading_start && conversion.clauses[0].reading_end == old.reading_end
            && conversion.preceding_surfaces.len() == 1 && conversion.preceding_surfaces[0].surface == prefix
            && new.key.identity == conversion.key.identity && new.key.baseline == conversion.key.baseline
            && new.key.conversion_revision == conversion.key.conversion_revision + 1
            && new.key.request_id > conversion.key.request_id && new.preceding_surfaces == conversion.preceding_surfaces
            && matches!(snapshot, Some(Request::LiveSnapshot { connection_generation, segments, explicit: true, .. })
                if *connection_generation == conversion.key.identity.connection_generation
                    && segments.len() == 1 && segments[0].text == conversion.reading && segments[0].style.as_deref() == Some("direct"))
    });
    let wire = check("rebaseline-full-reading-then-exact-interval", consistent, (requests.len(), candidates.len()));
    let values = host.candidate_strings();
    let selected = host.candidate_selection() as usize;
    let surface = host.store.preedit();
    let window = check("rebaseline-ready-preserves-other-clause", eaten && ready
        && surface.strip_prefix(prefix).is_some_and(|suffix| values.get(selected).is_some_and(|candidate| candidate == suffix))
        && host.store.committed().is_empty(), (&surface, &values, selected));
    eaten &= host.feed_key_no_pump(scenarios::ENTER.0);
    eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
    let committed = wait_until(|| host.store.committed() == surface && host.store.preedit() == "あ");
    let ordered = check("rebaseline-commit-retains-full-reading-and-next-input", eaten && committed
        && host.store.full() == format!("{surface}あ"), host.store.full());
    preserved && wire && window && ordered
}

pub fn run() -> i32 {
    run_inner(None)
}

pub fn run_receipt_retry() -> i32 {
    run_inner(Some(ReceiptGate::Retry))
}

pub fn run_receipt_expired() -> i32 {
    run_inner(Some(ReceiptGate::ExpiredToken))
}

pub fn run_rebaseline() -> i32 { run_inner(Some(ReceiptGate::Rebaseline)) }
pub fn run_rebaseline_restart() -> i32 { run_inner(Some(ReceiptGate::RebaselineRestart)) }
pub fn run_deferred_commit() -> i32 { run_inner(Some(ReceiptGate::DeferredCommit)) }
pub fn run_receipt_delivery_expiry() -> i32 { run_inner(Some(ReceiptGate::DeliveryExpiry)) }
pub fn run_learning_clear() -> i32 { run_inner(Some(ReceiptGate::LearningClear)) }
pub fn run_learning_clear_receipt() -> i32 { run_inner(Some(ReceiptGate::LearningClearReceipt)) }
pub fn run_mode_toggle_commit() -> i32 { run_inner(Some(ReceiptGate::ModeToggleCommit)) }
pub fn run_learning_toggle() -> i32 { run_inner(Some(ReceiptGate::LearningToggle)) }
pub fn run_boundary_resize() -> i32 { run_inner(Some(ReceiptGate::BoundaryResize)) }
pub fn run_boundary_restart() -> i32 { run_inner(Some(ReceiptGate::BoundaryRestart)) }
pub fn run_backspace_escape() -> i32 { run_inner(Some(ReceiptGate::BackspaceEscape)) }
pub fn run_reading_cursor() -> i32 { run_inner(Some(ReceiptGate::ReadingCursor)) }
pub fn run_notation_cycle() -> i32 { run_inner(Some(ReceiptGate::NotationCycle)) }
pub fn run_mixed_edit() -> i32 { run_inner(Some(ReceiptGate::MixedEdit)) }
pub fn run_display() -> i32 { run_inner(Some(ReceiptGate::Display)) }

fn check_display(host: &TsfHost) -> bool {
    let mut eaten = true;
    let events = read_events(std::process::id()).len();
    for key in scenarios::typed("kyouhaiitenkidesu") { eaten &= host.feed_key(key.0); }
    let rejected_before = host.store.rejected_selections.get();
    let repaired = std::rc::Rc::new(std::cell::Cell::new(false));
    let flag = repaired.clone();
    *host.store.on_selection.borrow_mut() = Some(Box::new(move || flag.set(true)));
    host.store.reject_selection.set(true);
    eaten &= host.feed_key(scenarios::SPACE.0);
    let rejected = wait_until(|| host.store.rejected_selections.get() > rejected_before);
    host.store.reject_selection.set(false);
    let mut passed = check("display-initial-snapshot-repairs-without-key", rejected && wait_until(|| repaired.get()), host.store.full());
    if !wait_until(|| read_events(std::process::id()).iter().skip(events)
        .any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) { return false; }
    eaten &= host.feed_key(scenarios::F7.0);
    let body = host.store.preedit();
    let writes = host.store.text_writes.get();
    eaten &= host.feed_key(scenarios::RIGHT.0);
    passed &= check("display-move-skips-body-write", eaten && host.store.preedit() == body
        && host.store.text_writes.get() == writes, host.store.full());
    eaten &= host.feed_key(scenarios::F7.0);
    let body = host.store.preedit();
    let Some(prefix) = body.strip_suffix("テンキデス") else { return check("display-two-clauses", false, body); };
    let start = prefix.encode_utf16().count() as i32;
    host.store.text_ext_requests.borrow_mut().clear();
    eaten &= host.feed_key(scenarios::SPACE.0);
    let opened = wait_until(|| host.candidate_strings().len() >= 2);
    passed &= check("display-anchor-is-selected-clause-start", eaten && opened
        && host.store.text_ext_requests.borrow().contains(&(start, start)), host.store.text_ext_requests.borrow().clone());
    let values = host.candidate_strings();
    let Some(index) = values.iter().position(|value| value != "テンキデス") else { return false; };
    let expected = format!("{prefix}{}", values[index]);
    let rejected_before = host.store.rejected_selections.get();
    let repaired = std::rc::Rc::new(std::cell::Cell::new(false));
    let flag = repaired.clone();
    *host.store.on_selection.borrow_mut() = Some(Box::new(move || flag.set(true)));
    host.store.reject_selection.set(true);
    let selected = host.behavior_select(index as u32);
    let rejected = wait_until(|| host.store.rejected_selections.get() > rejected_before);
    host.store.reject_selection.set(false);
    passed &= check("display-host-selection-repairs-without-key", selected && rejected
        && wait_until(|| repaired.get() && host.store.preedit() == expected), host.store.full());
    let body = host.store.preedit();
    let writes = host.store.text_writes.get();
    host.store.reject_selection.set(true);
    eaten &= host.feed_key_no_pump(0x25);
    eaten &= host.feed_key_no_pump(scenarios::RIGHT.0);
    host.store.reject_selection.set(false);
    let repaired = wait_until(|| host.store.selection().0 == body.encode_utf16().count() as i32);
    host.store.text_ext_requests.borrow_mut().clear();
    eaten &= host.feed_key(scenarios::SPACE.0);
    let reopened = wait_until(|| host.candidate_strings().len() >= 2);
    passed &= check("display-failed-move-repairs-before-next-move", eaten && repaired && reopened
        && host.store.preedit() == body && host.store.text_writes.get() == writes
        && host.store.text_ext_requests.borrow().contains(&(start, start)), host.store.full());
    let hook = match DeferredCommitHook::load() { Ok(hook) => hook, Err(error) => return check("display-capacity-hook", false, error) };
    let armed = hook.command(6) == 0;
    host.store.reject_text.set(true);
    eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
    eaten &= host.feed_key_no_pump(scenarios::ch('b').0);
    let retained = host.store.preedit() == body && host.store.committed().is_empty();
    host.store.reject_text.set(false);
    eaten &= host.feed_key(scenarios::ENTER.0);
    let recovered = wait_until(|| host.store.committed() == body && host.store.preedit() == "あb");
    passed &= check("display-counter-end-preserves-next-input", eaten && armed && retained && recovered,
        (host.store.committed(), host.store.preedit()));
    let expected = host.store.full();
    let armed = hook.command(6) == 0;
    host.store.reject_selection.set(true);
    eaten &= host.feed_key_no_pump(scenarios::ch('c').0);
    host.store.reject_selection.set(false);
    eaten &= host.feed_key(scenarios::ENTER.0);
    let recovered = wait_until(|| host.store.committed() == expected && host.store.preedit() == "c");
    passed &= check("display-counter-close-repair-retains-triggering-input", eaten && armed && recovered,
        (host.store.committed(), host.store.preedit()));
    passed
}

fn check_mixed_edit(host: &TsfHost, relay: &crate::receipt_relay::ReceiptRelay) -> bool {
    use ipc::protocol::Request;
    let mut eaten = true;
    let events = read_events(std::process::id()).len();
    for key in scenarios::typed("kyouhaiitenkidesu") { eaten &= host.feed_key(key.0); }
    eaten &= host.feed_key(scenarios::SPACE.0);
    if !wait_until(|| read_events(std::process::id()).iter().skip(events).any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) {
        return check("mixed-initial", false, host.store.full());
    }
    eaten &= host.feed_key(scenarios::RIGHT.0);
    eaten &= host.feed_key_with_shift(0x25);
    eaten &= host.feed_key(scenarios::F7.0);
    eaten &= host.feed_key(scenarios::RIGHT.0);
    eaten &= host.feed_key(scenarios::F7.0);
    let before = host.store.preedit();
    let Some(prefix) = before.strip_suffix("テンキデス").map(str::to_owned) else {
        return check("mixed-three-clauses", false, before);
    };
    let edge = prefix.encode_utf16().count() as u32 + 1;
    eaten &= host.store.click_preedit(edge, 2);
    let editing = wait_until(|| host.store.preedit() == format!("{prefix}てんきでス"));
    let mut passed = check("mixed-click-keeps-other-surfaces", eaten && editing
        && host.store.selection() == (prefix.encode_utf16().count() as i32, prefix.encode_utf16().count() as i32),
        (host.store.full(), host.store.selection()));
    eaten &= host.feed_key(scenarios::ch('a').0);
    passed &= check("mixed-insert-keeps-other-surfaces", eaten && host.store.preedit() == format!("{prefix}あてんきでス")
        && host.store.committed().is_empty(), host.store.full());
    eaten &= host.feed_key(scenarios::ch('n').0);
    passed &= check("mixed-pending-n-keeps-other-surfaces", eaten && host.store.preedit() == format!("{prefix}あnてんきでス"), host.store.full());
    let request_count = relay.clause_requests().len();
    eaten &= host.feed_key(scenarios::SPACE.0);
    let requested = wait_until(|| relay.clause_requests().iter().skip(request_count).any(|request| matches!(request, Request::ConvertClauses(_))));
    let requests = relay.clause_requests();
    let exact = requests.iter().skip(request_count).filter_map(|request| if let Request::ConvertClauses(request) = request { Some(request) } else { None })
        .all(|request| request.reading == "きょうはいいあんてんきです" && request.clauses.len() == 1
            && request.clauses[0].reading_start.0 == 6 && request.clauses[0].reading_end.0 == 12
            && request.preceding_surfaces.len() == 1 && request.preceding_surfaces[0].surface == prefix);
    passed &= check("mixed-space-requests-only-edited-interval", eaten && requested && exact, requests.len() - request_count);
    relay.allow_conversion();
    let converted = wait_until(|| host.store.preedit().starts_with(&prefix) && host.store.preedit().ends_with('ス')
        && host.store.preedit() != format!("{prefix}あんてんきでス"));
    passed &= check("mixed-conversion-keeps-prefix-and-suffix", converted && host.store.committed().is_empty(), host.store.full());
    let text = host.store.preedit();
    eaten &= host.feed_key_no_pump(scenarios::ENTER.0);
    eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
    let committed = wait_until(|| host.store.committed() == text && host.store.preedit() == "あ");
    passed &= check("mixed-commit-and-next-input", eaten && committed, (host.store.committed(), host.store.preedit()));
    eaten &= host.feed_key(scenarios::ESC.0);
    if !wait_until(|| !host.store.composing()) { return false; }
    host.store.reset();
    let events = read_events(std::process::id()).len();
    for key in scenarios::typed("kyouhaiitenkidesu") { eaten &= host.feed_key(key.0); }
    eaten &= host.feed_key(scenarios::SPACE.0);
    if !wait_until(|| read_events(std::process::id()).iter().skip(events).any(|event| matches!(event, Ev::ClausePresented { ready: false, .. }))) { return false; }
    eaten &= host.feed_key(scenarios::RIGHT.0);
    eaten &= host.feed_key_with_shift(0x25);
    eaten &= host.feed_key(scenarios::F7.0);
    eaten &= host.feed_key(scenarios::RIGHT.0);
    eaten &= host.feed_key(scenarios::F7.0);
    let second_before = host.store.preedit();
    let Some(second_prefix) = second_before.strip_suffix("テンキデス").map(str::to_owned) else { return false; };
    eaten &= host.store.click_preedit(second_prefix.encode_utf16().count() as u32 + 1, 2);
    let old_sinks = host.store.mouse_sinks_snapshot();
    host.store.reject_text.set(true);
    eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
    eaten &= host.feed_key_no_pump(scenarios::F7.0);
    eaten &= host.feed_key_no_pump(scenarios::RIGHT.0);
    host.store.reject_text.set(false);
    let repaired = wait_until(|| host.store.preedit() == format!("{second_prefix}アテンキデス"));
    passed &= check("mixed-redraw-queued-notation-navigation", eaten && repaired && host.store.committed().is_empty(), host.store.full());
    eaten &= host.feed_key(scenarios::F10.0);
    eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
    let expected = format!("{second_prefix}アテンキデsu");
    let ordered = wait_until(|| host.store.committed() == expected && host.store.preedit() == "あ");
    passed &= check("mixed-redraw-queued-notation-navigation-input", eaten && ordered,
        (host.store.committed(), host.store.preedit()));
    eaten &= host.feed_key(scenarios::F7.0);
    let unchanged = host.store.preedit();
    let stale_ignored = !old_sinks.is_empty() && old_sinks.iter().all(|sink| unsafe { sink.OnMouseEvent(0, 2, 1).is_ok_and(|eaten| !eaten.as_bool()) });
    passed &= check("mixed-retired-mouse-cannot-edit-new-composition", eaten && stale_ignored && host.store.preedit() == unchanged, host.store.full());
    host.store.reject_text.set(true);
    eaten &= host.feed_key_no_pump(scenarios::F10.0);
    for _ in 0..63 { eaten &= host.feed_key_no_pump(scenarios::ch('a').0); }
    host.store.reject_text.set(false);
    let recovered = wait_until(|| host.store.committed() == format!("{expected}a") && host.store.preedit() == "あ".repeat(63));
    passed &= check("mixed-full-queue-reserves-commit-and-recovers", eaten && recovered, (host.store.committed(), host.store.preedit().chars().count()));
    let base = host.store.committed();
    eaten &= host.feed_key(scenarios::F7.0);
    let body = host.store.preedit();
    host.store.reject_text.set(true);
    eaten &= host.store.click_preedit(0, 2);
    eaten &= host.feed_key_no_pump(scenarios::F7.0);
    for _ in 0..60 { eaten &= host.feed_key_no_pump(scenarios::ch('a').0); }
    let second_fault = std::rc::Rc::new(std::cell::Cell::new(false));
    let flag = second_fault.clone();
    let store = host.store.clone();
    *host.store.on_text.borrow_mut() = Some(Box::new(move || { store.reject_text.set(true); flag.set(true); }));
    host.store.reject_text.set(false);
    let first_repaired = wait_until(|| second_fault.get());
    for _ in 0..3 { eaten &= host.feed_key_no_pump(scenarios::ch('a').0); }
    host.store.reject_text.set(false);
    let refilled = wait_until(|| host.store.committed() == format!("{base}{body}") && host.store.preedit() == "あ".repeat(63));
    passed &= check("mixed-refill-between-reading-and-notation-repairs", eaten && first_repaired && refilled,
        (host.store.committed(), host.store.preedit().chars().count()));
    let hook = match DeferredCommitHook::load() { Ok(hook) => hook, Err(error) => return check("mixed-capacity-hook", false, error) };
    eaten &= host.feed_key(scenarios::F7.0);
    eaten &= host.store.click_preedit(0, 2);
    let before = host.store.full();
    host.store.reject_text.set(true);
    let armed = hook.command(5) == 0;
    eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
    eaten &= host.feed_key_no_pump(scenarios::ch('b').0);
    host.store.reject_text.set(false);
    eaten &= host.feed_key(scenarios::ENTER.0);
    let retained = wait_until(|| host.store.committed() == before && host.store.preedit() == "あb");
    passed &= check("mixed-counter-end-preserves-triggering-text", eaten && armed && retained,
        (host.store.committed(), host.store.preedit()));
    let before = host.store.full();
    eaten &= host.feed_key(scenarios::F7.0);
    host.store.reject_text.set(true);
    eaten &= host.store.click_preedit(0, 2);
    let armed = hook.command(5) == 0;
    for _ in 0..63 { eaten &= host.feed_key_no_pump(scenarios::ch('a').0); }
    host.store.reject_text.set(false);
    let retained = wait_until(|| host.store.committed() == before && host.store.preedit() == "あ".repeat(63));
    passed &= check("mixed-full-queue-counter-end-preserves-all-input", eaten && armed && retained,
        (host.store.committed(), host.store.preedit().chars().count()));
    passed
}

fn check_notation_cycle(host: &TsfHost) -> bool {
    let mut eaten = true;
    for key in scenarios::typed("nihongo") { eaten &= host.feed_key_no_pump(key.0); }
    eaten &= host.feed_key_no_pump(0x25);
    let mut passed = true;
    for expected in ["nihongo", "NIHONGO", "Nihongo", "nihongo"] {
        eaten &= host.feed_key(scenarios::F10.0);
        passed &= check("notation-cycle", eaten && host.store.preedit() == expected
            && host.store.committed().is_empty(), host.store.full());
    }
    eaten &= host.feed_key(0x78);
    passed &= check("notation-key-change-resets", eaten && host.store.preedit() == "ｎｉｈｏｎｇｏ", host.store.full());
    eaten &= host.feed_key(scenarios::F6.0);
    eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
    let next = wait_until(|| host.store.committed() == "にほんご" && host.store.preedit() == "あ");
    passed &= check("notation-f6-retains-converting", eaten && next, (host.store.committed(), host.store.preedit()));
    eaten &= host.feed_key(scenarios::ESC.0);
    if !wait_until(|| !host.store.composing()) { return false; }
    host.store.reset();
    for key in scenarios::typed("kyou") { eaten &= host.feed_key_no_pump(key.0); }
    eaten &= host.feed_key_no_pump(0x25);
    eaten &= host.feed_key_no_pump(0x25);
    eaten &= host.feed_key(scenarios::F7.0);
    eaten &= host.feed_key(scenarios::ESC.0);
    eaten &= host.feed_key(scenarios::ESC.0);
    passed &= check("notation-escape-restores-reading-cursor", eaten && host.store.preedit() == "きょう"
        && host.store.selection() == (1, 1), (host.store.full(), host.store.selection()));
    eaten &= host.feed_key(scenarios::F10.0);
    eaten &= host.feed_key(scenarios::SPACE.0);
    let candidates = wait_until(|| host.candidate_strings().len() >= 2);
    passed &= check("notation-space-can-acquire-baseline", eaten && candidates && host.store.committed().is_empty(), host.candidate_strings());
    for _ in 0..4 { eaten &= host.feed_key(scenarios::ESC.0); }
    if !wait_until(|| !host.store.composing()) { return false; }
    host.store.reset();
    eaten &= host.feed_key(0xBE); // Period; punctuation_full_width folds it to 。.
    eaten &= host.feed_key(scenarios::F10.0);
    passed &= check("notation-original-punctuation", eaten && host.store.preedit() == ".", host.store.full());
    eaten &= host.feed_key(0x78);
    passed &= check("notation-fullwidth-original-punctuation", eaten && host.store.preedit() == "．", host.store.full());
    for _ in 0..3 { eaten &= host.feed_key(scenarios::ESC.0); }
    if !wait_until(|| !host.store.composing()) { return false; }
    host.store.reset();
    eaten &= host.feed_key(scenarios::ch('a').0);
    eaten &= host.feed_key(0x6D); // Numpad subtract is folded to the long-vowel mark.
    eaten &= host.feed_key(scenarios::F10.0);
    passed &= check("notation-original-numpad-subtract", eaten && host.store.preedit() == "a-", host.store.full());
    passed
}

fn check_reading_cursor(host: &TsfHost) -> bool {
    let mut eaten = true;
    for key in scenarios::typed("kyou") { eaten &= host.feed_key_no_pump(key.0); }
    eaten &= host.feed_key_no_pump(0x25);
    let mut passed = check("reading-left-keeps-composition", eaten && host.store.preedit() == "きょう"
        && host.store.committed().is_empty() && host.store.selection() == (2, 2), (host.store.full(), host.store.selection()));
    for key in scenarios::typed("ka") { eaten &= host.feed_key_no_pump(key.0); }
    passed &= check("reading-interior-insert", eaten && host.store.preedit() == "きょかう"
        && host.store.selection() == (3, 3), (host.store.full(), host.store.selection()));
    eaten &= host.feed_key_no_pump(0x2E);
    passed &= check("reading-delete-after-cursor", eaten && host.store.preedit() == "きょか"
        && host.store.selection() == (3, 3), (host.store.full(), host.store.selection()));
    eaten &= host.feed_key_no_pump(0x2E);
    passed &= check("reading-delete-at-end-noop", eaten && host.store.preedit() == "きょか"
        && host.store.committed().is_empty() && host.store.selection() == (3, 3),
        (host.store.full(), host.store.selection()));
    eaten &= host.feed_key_no_pump(0x24);
    eaten &= host.feed_key_no_pump(scenarios::BACK.0);
    passed &= check("reading-home-backspace-noop", eaten && host.store.preedit() == "きょか"
        && host.store.selection() == (0, 0), (host.store.full(), host.store.selection()));
    eaten &= host.feed_key_no_pump(0x23);
    eaten &= host.feed_key_no_pump(scenarios::BACK.0);
    passed &= check("reading-end-backspace", eaten && host.store.preedit() == "きょ"
        && host.store.selection() == (2, 2), (host.store.full(), host.store.selection()));
    host.store.reject_selection.set(true);
    eaten &= host.feed_key(0x25);
    host.store.reject_selection.set(false);
    let repaired = wait_until(|| host.store.selection() == (1, 1));
    passed &= check("reading-caret-retry", eaten && repaired && host.store.preedit() == "きょ"
        && host.store.committed().is_empty(), (host.store.full(), host.store.selection()));
    eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
    eaten &= host.feed_key_no_pump(scenarios::ENTER.0);
    eaten &= host.feed_key_no_pump(scenarios::ch('a').0);
    let ordered = wait_until(|| host.store.committed() == "きあょ" && host.store.preedit() == "あ");
    passed &= check("reading-commit-and-next-input", eaten && ordered, (host.store.committed(), host.store.preedit()));
    passed
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReceiptGate { Retry, ExpiredToken, Rebaseline, RebaselineRestart, DeferredCommit, DeliveryExpiry, LearningClear, LearningClearReceipt, ModeToggleCommit, LearningToggle, BoundaryResize, BoundaryRestart, BackspaceEscape, ReadingCursor, NotationCycle, MixedEdit, Display }

fn learning_files(root: &std::path::Path) -> std::io::Result<std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>> {
    let mut directories = vec![root.to_owned()];
    let mut files = std::collections::BTreeMap::new();
    let mut bytes = 0usize;
    let mut entries = 0usize;
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            entries += 1;
            let metadata = entry.metadata()?;
            if entries > 256 || entry.file_type()?.is_symlink() { return Err(std::io::Error::other("unexpected learning tree")); }
            if metadata.is_dir() { directories.push(entry.path()); }
            else if metadata.is_file() {
                bytes = bytes.saturating_add(metadata.len() as usize);
                if bytes > 8 * 1024 * 1024 { return Err(std::io::Error::other("learning snapshot too large")); }
                files.insert(entry.path().strip_prefix(root).unwrap().to_owned(), std::fs::read(entry.path())?);
            }
        }
    }
    Ok(files)
}

fn run_inner(receipt_gate: Option<ReceiptGate>) -> i32 {
    let scratch = std::env::temp_dir().join(format!("nospacekey-clause-gate-{}", std::process::id()));
    let settings = scratch.join("nospacekey");
    if std::fs::create_dir_all(&settings).and_then(|_| std::fs::write(settings.join("settings.json"),
        r#"{"version":2,"zenzai":{"enabled":false},"learning":{"enabled":true},"default_direct":false,"live_conversion":{"enabled":false}}"#)).is_err() {
        println!("clause-navigation setup : ERROR (scratch settings)");
        return 2;
    }
    std::env::set_var("LOCALAPPDATA", &scratch);
    let learning_root = scratch.join("delivery-expiry-memory");
    if matches!(receipt_gate, Some(ReceiptGate::DeliveryExpiry | ReceiptGate::LearningClear | ReceiptGate::LearningClearReceipt | ReceiptGate::LearningToggle)) {
        std::env::set_var("NOSPACEKEY_LEARNING", "1");
        std::env::set_var("NOSPACEKEY_MEMORY_DIR", &learning_root);
    }
    if matches!(receipt_gate, Some(ReceiptGate::Rebaseline | ReceiptGate::RebaselineRestart | ReceiptGate::BoundaryResize | ReceiptGate::BoundaryRestart | ReceiptGate::BackspaceEscape | ReceiptGate::MixedEdit)) {
        if std::fs::write(settings.join("settings.json"),
            r#"{"version":2,"zenzai":{"enabled":false},"learning":{"enabled":false},"default_direct":false,"live_conversion":{"enabled":false}}"#).is_err() { return 2; }
    }
    let mut relay = if receipt_gate.is_some() {
        let fault = match receipt_gate {
            Some(ReceiptGate::Retry) => crate::receipt_relay::RelayFault::LoseFirstAck,
            Some(ReceiptGate::Rebaseline) => crate::receipt_relay::RelayFault::ExpiredBaseline,
            Some(ReceiptGate::RebaselineRestart) => crate::receipt_relay::RelayFault::RestartEngine,
            Some(ReceiptGate::DeliveryExpiry) => crate::receipt_relay::RelayFault::DeliveryExpiry,
            Some(ReceiptGate::LearningToggle | ReceiptGate::BoundaryResize | ReceiptGate::BoundaryRestart | ReceiptGate::BackspaceEscape | ReceiptGate::MixedEdit) => crate::receipt_relay::RelayFault::ObserveConfiguration,
            _ => crate::receipt_relay::RelayFault::PassThrough,
        };
        match crate::receipt_relay::ReceiptRelay::start(fault) {
            Ok(relay) => Some(relay),
            Err(error) => { println!("clause-navigation receipt-relay : ERROR ({error})"); return 2; }
        }
    } else { None };
    let _com = match ComSta::init() {
        Ok(com) => com,
        Err(error) => { println!("clause-navigation setup : ERROR ({error:?})"); return 2; }
    };
    let mut host = match TsfHost::start() {
        Ok(host) => host,
        Err(error) => { println!("clause-navigation setup : ERROR ({error:?})"); return 2; }
    };
    if !host.normalize_native_mode() || !host.force_native_conversion_mode() {
        println!("clause-navigation setup : ERROR (native mode)");
        return 2;
    }
    host.force_pbshow(Some(false));
    host.warm_up();
    host.store.reset();
    if receipt_gate == Some(ReceiptGate::Display) {
        return if check_display(&host) { 0 } else { 1 };
    }
    if receipt_gate == Some(ReceiptGate::ReadingCursor) {
        return if check_reading_cursor(&host) { 0 } else { 1 };
    }
    if receipt_gate == Some(ReceiptGate::MixedEdit) {
        return if check_mixed_edit(&host, relay.as_ref().unwrap()) { 0 } else { 1 };
    }
    if receipt_gate == Some(ReceiptGate::NotationCycle) {
        return if check_notation_cycle(&host) { 0 } else { 1 };
    }
    if receipt_gate == Some(ReceiptGate::BackspaceEscape) {
        return if check_backspace_escape(&host, relay.as_ref().unwrap()) { 0 } else { 1 };
    }
    if receipt_gate == Some(ReceiptGate::LearningToggle) {
        return if check_learning_toggle(&host, relay.as_ref().unwrap(), &learning_root, &settings) { 0 } else { 1 };
    }
    if matches!(receipt_gate, Some(ReceiptGate::BoundaryResize | ReceiptGate::BoundaryRestart)) {
        return if check_boundary_resize(&host, relay.as_mut().unwrap(), receipt_gate == Some(ReceiptGate::BoundaryRestart)) { 0 } else { 1 };
    }
    if receipt_gate == Some(ReceiptGate::ModeToggleCommit) {
        return if check_mode_toggle_commit(&host, relay.as_ref().unwrap()) { 0 } else { 1 };
    }
    if receipt_gate == Some(ReceiptGate::LearningClearReceipt) {
        return if check_learning_clear_receipt(&host, relay.as_ref().unwrap(), &learning_root) { 0 } else { 1 };
    }
    if receipt_gate == Some(ReceiptGate::LearningClear) {
        return if check_learning_clear(&host, relay.as_ref().unwrap(), &learning_root) { 0 } else { 1 };
    }
    if receipt_gate == Some(ReceiptGate::DeferredCommit) {
        return if check_deferred_commit(&host, relay.as_ref().unwrap()) { 0 } else { 1 };
    }
    if matches!(receipt_gate, Some(ReceiptGate::Rebaseline | ReceiptGate::RebaselineRestart)) {
        return if check_rebaseline(&host, relay.as_mut().unwrap(), receipt_gate == Some(ReceiptGate::RebaselineRestart)) { 0 } else { 1 };
    }
    let mut passed = true;
    for key in scenarios::typed("nihongo") { passed &= host.feed_key(key.0); }
    host.feed_key(scenarios::SPACE.0);
    let initial = wait_until(|| host.store.preedit() == "日本語");
    passed &= check("first-space-closed", initial && host.store.committed().is_empty()
        && host.candidate_strings().is_empty(), (host.store.full(), host.candidate_strings()));
    host.feed_key(scenarios::SPACE.0);
    let opened = wait_until(|| host.candidate_strings().len() >= 2);
    let candidates = host.candidate_strings();
    passed &= check("second-space-opens", opened && candidates.len() <= 9, &candidates);
    if !opened { return 1; }
    let before = host.store.preedit();
    let selected = host.candidate_selection() as usize;
    if !check("whole-clause-selection", candidates.get(selected) == Some(&before), (&before, selected)) {
        return 1;
    }
    host.feed_key(scenarios::SPACE.0);
    let next = (selected + 1) % candidates.len();
    passed &= check("selection-updates-document", host.candidate_selection() as usize == next
        && host.store.preedit() == candidates[next] && host.store.preedit() != before,
        (host.candidate_selection(), host.store.preedit()));
    let visible = host.store.preedit();
    let moved = host.feed_key(scenarios::RIGHT.0);
    passed &= check("arrow-closes-without-commit", moved && host.candidate_strings().is_empty()
        && host.store.preedit() == visible && host.store.committed().is_empty(), host.store.full());
    if receipt_gate == Some(ReceiptGate::DeliveryExpiry) {
        let Some(timing) = relay.as_ref().unwrap().token_timing() else { return 1; };
        if !check("delivery-expiry-token-issuance-bracket", timing.after.duration_since(timing.before) < Duration::from_millis(500)
            && timing.tokens.len() >= 2, timing.after.duration_since(timing.before)) { return 1; }
        println!("clause-navigation: holding candidate until token age is about 59 seconds before commit");
        while Instant::now() < timing.after + Duration::from_millis(59_200) {
            tsf_host::pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if receipt_gate == Some(ReceiptGate::ExpiredToken) {
        // Exercise the native monotonic 60-second token lifetime without a product clock override.
        println!("clause-navigation: holding selected candidate for 61 seconds to expire its token");
        let deadline = Instant::now() + Duration::from_secs(61);
        while Instant::now() < deadline {
            tsf_host::pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        passed &= check("expired-token-keeps-uncommitted-surface", host.store.preedit() == visible
            && host.store.committed().is_empty() && host.store.composing(), host.store.full());
    }
    let pid = std::process::id();
    let event_start = read_events(pid).len();
    let learning_before = if receipt_gate == Some(ReceiptGate::DeliveryExpiry) {
        match learning_files(&learning_root) {
            Ok(files) => Some(files),
            Err(error) => { check("delivery-expiry-learning-snapshot", false, error); return 1; }
        }
    } else { None };
    let enter = host.feed_key_no_pump(scenarios::ENTER.0);
    if let Some(relay) = relay.as_ref().filter(|_| receipt_gate == Some(ReceiptGate::Retry)) {
        let first = wait_until(|| relay.observations().len() == 1);
        passed &= check("receipt-first-attempt-before-next-input", first
            && host.store.committed() == visible && host.store.preedit().is_empty(), host.store.full());
    }
    let insert = host.feed_key_no_pump(scenarios::ch('a').0);
    let committed = wait_until(|| host.store.committed() == visible && host.store.preedit() == "あ");
    passed &= check("enter-then-input-order", enter && insert && committed
        && host.store.full() == format!("{visible}あ"), (host.store.committed(), host.store.preedit()));
    if let Some(relay) = relay.as_ref() {
        if !committed { return 1; }
        relay.allow_retry_after_next_composition();
    }
    if matches!(receipt_gate, Some(ReceiptGate::ExpiredToken | ReceiptGate::DeliveryExpiry)) {
        use ipc::clause::{IntervalLearning, ReceiptRejection, ReceiptStatus};
        let relay = relay.as_ref().unwrap();
        let rejected = wait_until(|| read_events(pid).iter().skip(event_start)
            .any(|event| matches!(event, Ev::ReceiptTokenExpired)));
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            tsf_host::pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        let observations = relay.observations();
        if receipt_gate == Some(ReceiptGate::DeliveryExpiry) {
            let timing = relay.token_timing().unwrap();
            let times = relay.delivery_times();
            let ordered = times.len() == 1 && times[0].0.duration_since(timing.after) >= Duration::from_secs(59)
                && times[0].0.duration_since(timing.before) < Duration::from_secs(60)
                && times[0].1.duration_since(timing.after) >= Duration::from_secs(60);
            let same_token = observations.len() == 1 && observations[0].0.intervals.iter().all(|interval|
                matches!(&interval.learning, IntervalLearning::Candidate { token, .. } if timing.tokens.contains(token)));
            passed &= check("delivery-expiry-valid-at-commit-expired-before-forwarding", ordered && same_token,
                times.iter().map(|(received, sent)| (received.duration_since(timing.before).as_millis(),
                    received.duration_since(timing.after).as_millis(), sent.duration_since(timing.after).as_millis())).collect::<Vec<_>>());
            let learning_after = learning_files(&learning_root);
            passed &= check("delivery-expiry-keeps-persisted-learning-unchanged", learning_after.as_ref().is_ok_and(|after|
                learning_before.as_ref() == Some(after)), learning_after.as_ref().map(|files| files.len()));
        }
        let expired = observations.len() == 1 && observations.first().is_some_and(|(receipt, ack)| {
            receipt.reading == "にほんご" && receipt.text == visible
                && receipt.intervals.len() == 1
                && receipt.intervals[0].reading_start.0 == 0 && receipt.intervals[0].reading_end.0 == 4
                && receipt.intervals[0].surface == visible
                && matches!(receipt.intervals[0].learning, IntervalLearning::Candidate { explicitly_selected: true, .. })
                && matches!(ack, Some((id, ReceiptStatus::Rejected { reason: ReceiptRejection::Expired })) if *id == receipt.commit_id)
        });
        passed &= check("expired-token-receipt-rejected-once", rejected && expired,
            (observations.len(), rejected, expired));
        passed &= check("expired-token-preserves-commit-and-next-input", host.store.committed() == visible
            && host.store.preedit() == "あ" && host.store.full() == format!("{visible}あ")
            && !read_events(pid).iter().skip(event_start).any(|event| matches!(event, Ev::ReceiptAcknowledged)),
            host.store.full());
        return if passed { 0 } else { 1 };
    }
    let acknowledged = wait_until(|| read_events(pid).iter().skip(event_start)
        .any(|event| matches!(event, Ev::ReceiptAcknowledged)));
    passed &= check("receipt-after-document-commit", acknowledged && host.store.committed() == visible
        && host.store.preedit() == "あ" && host.store.full() == format!("{visible}あ"),
        host.store.committed());
    if let Some(relay) = relay.as_ref() {
        use ipc::clause::{IntervalLearning, ReceiptStatus};
        let retried = wait_until(|| relay.observations().len() >= 2);
        let observations = relay.observations();
        let exact = observations.len() == 2 && observations[0].0 == observations[1].0;
        let payload = observations.first().is_some_and(|(receipt, _)| {
            receipt.reading == "にほんご" && receipt.text == visible
                && receipt.intervals.len() == 1
                && receipt.intervals[0].reading_start.0 == 0
                && receipt.intervals[0].reading_end.0 == 4
                && receipt.intervals[0].surface == visible
                && matches!(receipt.intervals[0].learning, IntervalLearning::Candidate { explicitly_selected: true, .. })
                && !receipt.commit_id.client_instance.is_empty() && receipt.commit_id.sequence > 0
        });
        let statuses = observations.len() == 2 && observations.iter().zip([
            ReceiptStatus::Applied, ReceiptStatus::AlreadyProcessed,
        ]).all(|((receipt, response), expected)| matches!(response,
            Some((commit_id, status)) if *commit_id == receipt.commit_id && *status == expected));
        passed &= check("receipt-lost-ack-retries-exact-payload", retried && exact && payload && statuses,
            (observations.len(), exact, payload, statuses));
        passed &= check("receipt-retry-keeps-next-composition", acknowledged
            && host.store.committed() == visible && host.store.preedit() == "あ"
            && host.store.full() == format!("{visible}あ"), host.store.full());
        return if passed { 0 } else { 1 };
    }
    for (key, expected) in [(scenarios::F6, "にほんご"), (scenarios::F7, "ニホンゴ"),
        (scenarios::Vk(0x77, "F8"), "ﾆﾎﾝｺﾞ")] {
        if !check_kana_commit(&host, key, expected, false, 0) { return 1; }
    }
    passed &= check_kana_commit(&host, scenarios::F7, "ニホンゴ", true, 0);
    passed &= check_kana_commit(&host, scenarios::F7, "ﾆﾎﾝｺﾞ", false, 1);
    for (rotations, expected) in [(1, "ﾆﾎﾝｺﾞ"), (2, "にほんご"), (3, "ニホンゴ")] {
        if !check_kana_commit(&host, scenarios::F7, expected, true, rotations) { return 1; }
    }
    let mut eaten = host.feed_key(scenarios::ESC.0);
    if !wait_until(|| !host.store.composing()) { return 1; }
    host.store.reset();
    for key in scenarios::typed("nihongo") { eaten &= host.feed_key(key.0); }
    for key in [scenarios::SPACE, scenarios::F7, scenarios::ENTER, scenarios::ENTER, scenarios::ch('a')] {
        eaten &= host.feed_key_no_pump(key.0);
    }
    let complete = wait_until(|| host.store.committed() == "ニホンゴ" && host.store.preedit() == "あ");
    passed &= check("repeated-enter-keeps-next-input", eaten && complete && host.store.full() == "ニホンゴあ",
        (eaten, host.store.committed(), host.store.preedit(), host.store.full()));
    passed &= check_rejected_commit(&host);
    passed &= check_rejected_commit_caret(&host);
    passed &= check_terminated_failed_commit(&host);
    passed &= check_replaced_context(&mut host, false);
    passed &= check_replaced_context(&mut host, true);
    passed &= check_reactivated_failed_commit(&mut host);
    passed &= check_reconvert_correction_delivery(&host);
    if passed { 0 } else { 1 }
}
