//! nospacekey IME 自動検証テスト台（TSF テスト台）のエントリ。
//! 自スレッドに nospacekey TSF プロファイルを適用し、本物の VK を注入して
//! 確定文字列／preedit／候補ログを観測する。

mod doc_state;
mod text_store;
mod tsf_host;
mod uielement_sink;
mod scenarios;
mod driver;
mod log_parse;
mod report;

fn main() {
    // item12 用: TIP が spawn する engine プロセスは testbench の環境を継承するので、
    // ここで echo モードを立てておく（llmConvert だけに効き、他シナリオの挙動は変えない）。
    // 単一スレッド起動直後・他スレッド未起動の時点で 1 度だけ設定する。
    std::env::set_var("NOSPACEKEY_LLM_ECHO", "1");
    // Task 1 で診断ログは既定OFFになったため、ヘッドレス検証では明示的に有効化する。
    // log_parse は %TEMP%\nospacekey-tip.log の ev= 行（text=/list=/latin= 含む）を読むため必須。
    // 単一スレッド起動直後・他スレッド未起動の時点で 1 度だけ設定する。
    std::env::set_var("NOSPACEKEY_LOG", "1");
    // item24(バグ#2 回帰)用: iOS 移植の自動確定(fac6315)が有効だと preedit が正当に縮み、
    // 「セッション喪失による崩壊」と区別できなくなる。TIP が spawn する engine はこの環境を
    // 継承するので、テストは自動確定 off の決定的な世界で行う(NOSPACEKEY_LLM_ECHO と同じ機構)。
    // 注意: 既に他ホストが起こした常駐 engine へ接続した場合はその engine の設定に従う。
    std::env::set_var("NOSPACEKEY_AUTO_COMMIT", "disabled");
    let mut args: Vec<String> = std::env::args().collect();
    // --own-desktop: ゲート専用デスクトップで自分自身を再起動する(ユーザー操作中でも前面を
    // 取れる=foreground-trap 回避)。子プロセスは env マーカーで認識し、前面補助だけ有効化する。
    if std::env::var_os(tsf_host::OWN_DESKTOP_ENV).is_some() {
        tsf_host::mark_own_desktop();
    } else if let Some(i) = args.iter().position(|a| a == "--own-desktop") {
        args.remove(i);
        std::process::exit(tsf_host::respawn_on_gate_desktop(&args));
    }
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("--stage0");
    let json_path = args.iter().position(|a| a == "--json").and_then(|i| args.get(i + 1)).cloned();
    let code = match mode {
        "--stage0" | "" => tsf_host::stage0_spike(),
        "--canonical" => run_canonical(),
        "--scenarios" => run_scenarios_reported(json_path),
        "--item8" => run_item8_mode(),
        "--item9" => run_item9_mode(),
        "--item12" => run_item12_mode(),
        "--item13" => run_item13_mode(),
        "--item14" => run_item14_mode(),
        "--item15" => run_item15_mode(),
        "--item17" => run_item17_mode(),
        "--item18" => run_item18_mode(),
        "--item19" => run_item19_mode(),
        "--item24" => run_item24_mode(),
        "--item29" => run_item29_mode(),
        "--item30" => run_item30_mode(),
        "--item31" => run_item31_mode(),
        "--keymap-smoke" => run_keymap_smoke(),
        "--diag" => tsf_host::diag(),
        other => { eprintln!("unknown mode: {other}"); 2 }
    };
    std::process::exit(code);
}

/// nihongo␣⏎ → committed()==日本語 を 1 本検証する（実エンジン経由）。
fn run_canonical() -> i32 {
    // COM(STA) は host より先に束縛し、後に解放する（Drop 逆順／Task 5 ComSta 修正に整合）。
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("CANONICAL FAIL: ComSta::init {e:?}"); return 2; }
    };
    let host = match tsf_host::TsfHost::start() {
        Ok(h) => h,
        Err(e) => { eprintln!("CANONICAL FAIL: start {e:?}"); return 2; }
    };
    let mut keys = scenarios::typed("nihongo");
    keys.push(scenarios::SPACE);
    keys.push(scenarios::ENTER);
    let obs = driver::run_keys(&host, &keys);
    let committed = host.store.committed();
    for o in &obs {
        println!("  {:>9} vk={:#04x} eaten={} {}ms preedit={:?}", o.label, o.vk, o.eaten, o.elapsed_ms, o.preedit);
    }
    println!("CANONICAL committed={committed:?}");
    if committed == "日本語" { println!("CANONICAL PASS"); 0 } else { eprintln!("CANONICAL FAIL"); 1 }
}

/// item8: エンジン kill 耐性。ComSta ガードを host より先に束縛してから start。
fn run_item8_mode() -> i32 {
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("item8 ComSta::init fail: {e:?}"); return 2; }
    };
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r = driver::run_item8(&host, 5000);
            println!("item8 : {} ({})", if r.passed { "PASS" } else { "FAIL" }, r.detail);
            if r.passed { 0 } else { 1 }
        }
        Err(e) => { eprintln!("item8 start fail: {e:?}"); 2 }
    }
}

/// item9: 解除後 eaten=false。ComSta ガードを host より先に束縛してから start。
fn run_item9_mode() -> i32 {
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("item9 ComSta::init fail: {e:?}"); return 2; }
    };
    match tsf_host::TsfHost::start() {
        Ok(mut host) => {
            let (before, after) = driver::run_item9(&mut host);
            println!("item9 : before_eaten={before} after_eaten={after}");
            if before && !after { println!("item9 PASS"); 0 } else { eprintln!("item9 FAIL"); 1 }
        }
        Err(e) => { eprintln!("item9 start fail: {e:?}"); 2 }
    }
}

/// item12: Tab→外部LLM変換のスレッド配線（echo）。ComSta ガードを host より先に束縛して start。
fn run_item12_mode() -> i32 {
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("item12 ComSta::init fail: {e:?}"); return 2; }
    };
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r = driver::run_item12(&host);
            println!("item12 : {} ({})", if r.passed { "PASS" } else { "FAIL" }, r.detail);
            if r.passed { 0 } else { 1 }
        }
        Err(e) => { eprintln!("item12 start fail: {e:?}"); 2 }
    }
}

/// item13: SP5 ヘッドレス再変換（半角英数モード）。ComSta ガードを host より先に束縛して start。
fn run_item13_mode() -> i32 {
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("item13 ComSta::init fail: {e:?}"); return 2; }
    };
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r = driver::run_item13(&host);
            println!("item13 : {} ({})", if r.passed { "PASS" } else { "FAIL" }, r.detail);
            if r.passed { 0 } else { 1 }
        }
        Err(e) => { eprintln!("item13 start fail: {e:?}"); 2 }
    }
}

/// item14: SP6a 候補 UIElement の advertise/データ/Behavior 観測。
/// requires regsvr32-registered DLL（VM/admin）。ComSta ガードを host より先に束縛して start。
fn run_item14_mode() -> i32 {
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("item14 ComSta::init fail: {e:?}"); return 2; }
    };
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r = driver::run_item14(&host);
            println!("item14 : {} ({})", if r.passed { "PASS" } else { "FAIL" }, r.detail);
            if r.passed { 0 } else { 1 }
        }
        Err(e) => { eprintln!("item14 start fail: {e:?}"); 2 }
    }
}

/// item15: ライブ変換中のキャレット末尾追従。ComSta ガードを host より先に束縛して start。
fn run_item15_mode() -> i32 {
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("item15 ComSta::init fail: {e:?}"); return 2; }
    };
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r = driver::run_item15(&host);
            println!("item15 : {} ({})", if r.passed { "PASS" } else { "FAIL" }, r.detail);
            if r.passed { 0 } else { 1 }
        }
        Err(e) => { eprintln!("item15 start fail: {e:?}"); 2 }
    }
}

/// item17: SP5 step-6 非空かな選択の再変換。ComSta ガードを host より先に束縛して start。
fn run_item17_mode() -> i32 {
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("item17 ComSta::init fail: {e:?}"); return 2; }
    };
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r = driver::run_item17(&host);
            println!("item17 : {} ({})", if r.passed { "PASS" } else { "FAIL" }, r.detail);
            if r.passed { 0 } else { 1 }
        }
        Err(e) => { eprintln!("item17 start fail: {e:?}"); 2 }
    }
}

/// item18: 別ウィンドウへのフォーカス喪失でエンジン読みが居残らない。ComSta ガードを host より先に束縛して start。
fn run_item18_mode() -> i32 {
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("item18 ComSta::init fail: {e:?}"); return 2; }
    };
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r = driver::run_item18(&host);
            println!("item18 : {} ({})", if r.passed { "PASS" } else { "FAIL" }, r.detail);
            if r.passed { 0 } else { 1 }
        }
        Err(e) => { eprintln!("item18 start fail: {e:?}"); 2 }
    }
}

/// item19: SP5 実機バグ回帰（direct + OnKeyDown 直叩きで A–Z パススルー）。ComSta ガードを host より先に束縛して start。
fn run_item19_mode() -> i32 {
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("item19 ComSta::init fail: {e:?}"); return 2; }
    };
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r = driver::run_item19(&host);
            println!("item19 : {} ({})", if r.passed { "PASS" } else { "FAIL" }, r.detail);
            if r.passed { 0 } else { 1 }
        }
        Err(e) => { eprintln!("item19 start fail: {e:?}"); 2 }
    }
}

/// item24: 未確定 25 打鍵の preedit 全保持（22文字目以降ドロップの再現/回帰）。
/// ComSta ガードを host より先に束縛して start。
fn run_item24_mode() -> i32 {
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("item24 ComSta::init fail: {e:?}"); return 2; }
    };
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r = driver::run_item24(&host);
            println!("item24 : {} ({})", if r.passed { "PASS" } else { "FAIL" }, r.detail);
            if r.passed { 0 } else { 1 }
        }
        Err(e) => { eprintln!("item24 start fail: {e:?}"); 2 }
    }
}

/// item29: keyboard-disabled コンテキスト（Edge パスワード欄相当）で全キー素通し。
/// ComSta ガードを host より先に束縛して start。
fn run_item29_mode() -> i32 {
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("item29 ComSta::init fail: {e:?}"); return 2; }
    };
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r = driver::run_item29(&host);
            println!("item29 : {} ({})", if r.passed { "PASS" } else { "FAIL" }, r.detail);
            if r.passed { 0 } else { 1 }
        }
        Err(e) => { eprintln!("item29 start fail: {e:?}"); 2 }
    }
}

/// item30（Task4 確定取消 headless 回帰・往路）: nihongo→Space→Enter→Ctrl+Backspace→Esc の往復。
/// ComSta ガードを host より先に束縛して start。
fn run_item30_mode() -> i32 {
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("item30 ComSta::init fail: {e:?}"); return 2; }
    };
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r = driver::run_item30(&host);
            println!("item30 : {} ({})", if r.passed { "PASS" } else { "FAIL" }, r.detail);
            if r.passed { 0 } else { 1 }
        }
        Err(e) => { eprintln!("item30 start fail: {e:?}"); 2 }
    }
}

/// item31（Task4 確定取消 headless 回帰・disarm）: 確定後の打鍵/settle 経由で武装が解除される。
/// ComSta ガードを host より先に束縛して start。
fn run_item31_mode() -> i32 {
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("item31 ComSta::init fail: {e:?}"); return 2; }
    };
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r = driver::run_item31(&host);
            println!("item31 : {} ({})", if r.passed { "PASS" } else { "FAIL" }, r.detail);
            if r.passed { 0 } else { 1 }
        }
        Err(e) => { eprintln!("item31 start fail: {e:?}"); 2 }
    }
}

/// keymap リマップのヘッドレススモーク。settings は Activate 時 1 回読みなので、
/// TIP ロード前に専用 LOCALAPPDATA へ remap 済み settings.json を書いてから活性化する
/// (実ユーザーの settings.json を汚さない)。
/// シナリオ: to_katakana を F7→F11 へリマップ。
///   typed("nihongo") → F7(解放済み=表記変換しない) → F11(カタカナ表記変換)
/// 自己証明（偽 PASS 防止）: ev=notation vk=0x7a を直接観測する（=F11 が実際に表記変換を
/// 発火したこと）だけでなく、ev=notation vk=0x76 の不在（=F7 が既定バインドから外れて
/// 何もしなかったこと）も見る。preedit 一致だけだと「F7 がそのまま効いて表記変換し、
/// 直後の F11 が何もしなかった」偽 PASS を拾えない。
fn run_keymap_smoke() -> i32 {
    // item24 と同じ理由（run_item24 冒頭コメント参照）: 常駐 engine がユーザー日常使用分の
    // 古い env を握っていると自前 spawn に切り替わらず非決定化する。
    driver::kill_engine_processes();

    let base = std::env::temp_dir().join(format!("nospacekey-keymap-smoke-{}", std::process::id()));
    let dir = base.join("nospacekey");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("keymap-smoke: scratch dir fail: {e:?}");
        return 2;
    }
    if let Err(e) = std::fs::write(
        dir.join("settings.json"),
        r#"{"version":1,"keymap":{"to_katakana":"F11"}}"#,
    ) {
        eprintln!("keymap-smoke: settings fixture fail: {e:?}");
        return 2;
    }
    // TsfHost::start() の ActivateProfile が settings を 1 回読みする。ここで実ユーザーの
    // %LOCALAPPDATA%\nospacekey\settings.json を汚さないよう、host 起動前にだけ差し替える。
    std::env::set_var("LOCALAPPDATA", &base);

    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("keymap-smoke ComSta::init fail: {e:?}"); return 2; }
    };
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let _ = host.set_native_mode();
            host.warm_up();
            host.store.reset();
            let pid = std::process::id();
            let base_evs = log_parse::read_events(pid).len();

            for k in scenarios::typed("nihongo") { let _ = host.feed_key(k.0); }
            let _ = host.feed_key(scenarios::F7.0); // 解放済み: 表記変換しない期待
            let _ = host.feed_key(scenarios::F11.0); // リマップ先: カタカナ表記変換
            let preedit = host.store.preedit();

            let evs: Vec<log_parse::Ev> =
                log_parse::read_events(pid).into_iter().skip(base_evs).collect();
            let saw_f11_notation = evs
                .iter()
                .any(|e| matches!(e, log_parse::Ev::Notation { vk } if *vk == 0x7a));
            let saw_f7_notation = evs
                .iter()
                .any(|e| matches!(e, log_parse::Ev::Notation { vk } if *vk == 0x76));

            let passed = saw_f11_notation && !saw_f7_notation && preedit == "ニホンゴ";
            println!(
                "keymap-smoke : {} (saw_f11_notation={saw_f11_notation} \
                 saw_f7_notation={saw_f7_notation} preedit={preedit:?})",
                if passed { "PASS" } else { "FAIL" }
            );
            if passed { 0 } else { 1 }
        }
        Err(e) => { eprintln!("keymap-smoke start fail: {e:?}"); 2 }
    }
}

fn run_scenarios_reported(json_path: Option<String>) -> i32 {
    use report::{HarnessReport, ItemReport};
    // COM(STA) apartment for the whole run. これが失敗したら実行不能（started=false）。
    let _com = match tsf_host::ComSta::init() {
        Ok(c) => c,
        Err(e) => {
            let rep = HarnessReport {
                all_pass: false, started: false,
                start_error: Some(format!("ComSta::init: {e:?}")), items: vec![],
            };
            rep.print_table();
            if let Some(p) = json_path { let _ = rep.write_json(&p); }
            return rep.exit_code();
        }
    };
    let mut items: Vec<ItemReport> = Vec::new();

    // item1–7,10: シナリオ毎に新しい TsfHost（TIP の composition/engine 状態は store.reset() で消えない）。
    // item8/9 は専用ドライバ（engine kill / deactivate）が要るので下で個別実行する。
    for sc in scenarios::all() {
        if sc.item == 8 || sc.item == 9 { continue; } // item8/9 は下で個別実行
        match tsf_host::TsfHost::start() {
            Ok(host) => {
                let r = driver::run_scenario(&host, &sc);
                items.push(ItemReport {
                    item: r.item, name: r.name.into(),
                    status: if r.passed { "pass" } else { "fail" }.into(),
                    detail: r.detail, max_elapsed_ms: r.max_elapsed_ms,
                });
                // conversion-mode compartment はプロセス共有（host を作り直しても残る）なので、
                // シナリオが direct/NATIVE を変えたまま終わると後続の全 item が偽 FAIL する。
                // 「汚す item だけ列挙して戻す」方式は取らない: item34→36→37/38 と追加のたびに
                // ここへ番号を書き足す運用は記載漏れ＝原因不明の偽 FAIL という footgun だった。
                // native 着地シナリオには no-op なので、全シナリオ後に無条件で揃える。
                if !host.set_native_mode() {
                    eprintln!("warn: item{} 後の set_native_mode 失敗（後続 item が偽 FAIL する恐れ）", sc.item);
                }
            }
            Err(e) => items.push(ItemReport {
                item: sc.item, name: sc.name.into(), status: "error".into(),
                detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
            }),
        }
    }

    // item8: エンジン kill 耐性（新しい host）。
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r8 = driver::run_item8(&host, 5000);
            items.push(ItemReport {
                item: 8, name: "engine kill resilience".into(),
                status: if r8.passed { "pass" } else { "fail" }.into(),
                detail: r8.detail, max_elapsed_ms: 0,
            });
        }
        Err(e) => items.push(ItemReport {
            item: 8, name: "engine kill resilience".into(), status: "error".into(),
            detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
        }),
    }

    // item9: 解除後 eaten=false。必ず最後（解除はスレッド状態を変えるため）。
    match tsf_host::TsfHost::start() {
        Ok(mut host) => {
            let (before, after) = driver::run_item9(&mut host);
            let pass9 = before && !after;
            items.push(ItemReport {
                item: 9, name: "deactivate returns to normal".into(),
                status: if pass9 { "pass" } else { "fail" }.into(),
                detail: format!("before={before} after={after}"), max_elapsed_ms: 0,
            });
        }
        Err(e) => items.push(ItemReport {
            item: 9, name: "deactivate returns to normal".into(), status: "error".into(),
            detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
        }),
    }

    // item12: Tab→外部LLM変換のスレッド配線（worker→ポーリング→preedit 反映）を echo 検証。
    // 専用ドライバ（合成→Tab→settle_llm）が要るので個別実行する。新しい host で。
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r12 = driver::run_item12(&host);
            items.push(ItemReport {
                item: 12, name: "tab->llm convert wiring (echo)".into(),
                status: if r12.passed { "pass" } else { "fail" }.into(),
                detail: r12.detail, max_elapsed_ms: 0,
            });
        }
        Err(e) => items.push(ItemReport {
            item: 12, name: "tab->llm convert wiring (echo)".into(), status: "error".into(),
            detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
        }),
    }

    // item13: SP5 ヘッドレス再変換（半角英数モードで 変換キー0x1C→OnKeyDown→非空 StartComposition→
    // 候補→Esc 復元 / Enter 確定）。専用ドライバ（シード＋モード設定）が要るので個別実行。新 host で。
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r13 = driver::run_item13(&host);
            items.push(ItemReport {
                item: 13, name: "reconvert (direct mode, headless)".into(),
                status: if r13.passed { "pass" } else { "fail" }.into(),
                detail: r13.detail, max_elapsed_ms: 0,
            });
        }
        Err(e) => items.push(ItemReport {
            item: 13, name: "reconvert (direct mode, headless)".into(), status: "error".into(),
            detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
        }),
    }

    // item14: SP6a 候補 UIElement（ITfUIElementSink で BeginUIElement を観測 → GetUIElement で
    // 候補データ読み戻し → Behavior で SetSelection/Finalize）。専用ドライバ（pbShow=FALSE 模擬＋
    // 変換駆動）が要るので個別実行する。新 host で。
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r14 = driver::run_item14(&host);
            items.push(ItemReport {
                item: 14, name: "candidate uielement advertise (headless)".into(),
                status: if r14.passed { "pass" } else { "fail" }.into(),
                detail: r14.detail, max_elapsed_ms: 0,
            });
        }
        Err(e) => items.push(ItemReport {
            item: 14, name: "candidate uielement advertise (headless)".into(), status: "error".into(),
            detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
        }),
    }

    // item15: ライブ変換中のキャレット末尾追従（先頭居座りバグの回帰）。専用ドライバ
    // （"nihongo" を打ってデバウンス後に selection を観測）が要るので個別実行する。新 host で。
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r15 = driver::run_item15(&host);
            items.push(ItemReport {
                item: 15, name: "live-conversion caret follows to end".into(),
                status: if r15.passed { "pass" } else { "fail" }.into(),
                detail: r15.detail, max_elapsed_ms: 0,
            });
        }
        Err(e) => items.push(ItemReport {
            item: 15, name: "live-conversion caret follows to end".into(), status: "error".into(),
            detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
        }),
    }

    // item16: 前方一致候補の部分確定でデータロスしない（"日本"確定→残り読み"ご"が継続）。専用ドライバ
    // （nihongo+Space で候補→日本 を Behavior 確定→committed/preedit/ログ観測）が要るので個別実行。新 host で。
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r16 = driver::run_item16(&host);
            items.push(ItemReport {
                item: 16, name: "prefix-candidate partial commit keeps remainder".into(),
                status: if r16.passed { "pass" } else { "fail" }.into(),
                detail: r16.detail, max_elapsed_ms: 0,
            });
        }
        Err(e) => items.push(ItemReport {
            item: 16, name: "prefix-candidate partial commit keeps remainder".into(), status: "error".into(),
            detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
        }),
    }

    // item17: SP5 step-6 非空かな選択の再変換（"にほんご" 選択→変換キー→kind=surface／
    // "日本語" 選択は do-no-harm skip）。専用ドライバ（シード＋選択＋モード設定）が要るので個別実行。新 host で。
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r17 = driver::run_item17(&host);
            items.push(ItemReport {
                item: 17, name: "reconvert kana selection (headless)".into(),
                status: if r17.passed { "pass" } else { "fail" }.into(),
                detail: r17.detail, max_elapsed_ms: 0,
            });
        }
        Err(e) => items.push(ItemReport {
            item: 17, name: "reconvert kana selection (headless)".into(), status: "error".into(),
            detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
        }),
    }

    // item18: 別ウィンドウへのフォーカス喪失でエンジンの読みが居残らない（フォーカス喪失データ
    // 残留の回帰）。専用ドライバ（nihongo→別docへSetFocus往復→aiueo）が要るので個別実行。新 host で。
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r18 = driver::run_item18(&host);
            items.push(ItemReport {
                item: 18, name: "focus loss resets stale engine session".into(),
                status: if r18.passed { "pass" } else { "fail" }.into(),
                detail: r18.detail, max_elapsed_ms: 0,
            });
        }
        Err(e) => items.push(ItemReport {
            item: 18, name: "focus loss resets stale engine session".into(), status: "error".into(),
            detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
        }),
    }

    // item19: SP5 実機バグ回帰（direct(半角英数)モードで、OnTestKeyDown を経ず OnKeyDown を
    // 直叩きするホスト経路でも A–Z が かな化されず素通しされる）。専用ドライバ
    // （native 対照→direct 設定→feed_key_keydown_only で abc 注入）が要るので個別実行。新 host で。
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r19 = driver::run_item19(&host);
            items.push(ItemReport {
                item: 19, name: "direct mode passes latin via keydown-only host path".into(),
                status: if r19.passed { "pass" } else { "fail" }.into(),
                detail: r19.detail, max_elapsed_ms: 0,
            });
        }
        Err(e) => items.push(ItemReport {
            item: 19, name: "direct mode passes latin via keydown-only host path".into(), status: "error".into(),
            detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
        }),
    }

    // item24: 未確定長文入力が live_convert タイムアウト(drop_engine)を跨いで保全される
    // （バグ#2「22文字目以降ドロップ」+ fac6315 盲点の回帰）。専用ドライバ（engine pre-kill＋
    // 打鍵間 settle_debounce のインターリーブ＋崩壊検知）が要るので個別実行する。新 host で。
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r24 = driver::run_item24(&host);
            items.push(ItemReport {
                item: 24, name: "long uncommitted input survives live-convert timeout".into(),
                status: if r24.passed { "pass" } else { "fail" }.into(),
                detail: r24.detail, max_elapsed_ms: 0,
            });
        }
        Err(e) => items.push(ItemReport {
            item: 24, name: "long uncommitted input survives live-convert timeout".into(), status: "error".into(),
            detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
        }),
    }

    // item29: Edge(Chromium) パスワード欄相当＝GUID_COMPARTMENT_KEYBOARD_DISABLED=1 の
    // コンテキストで全キー素通し（実機発見バグ#1 の回帰）。context compartment の設定と
    // フォーカス遷移が要るので専用ドライバで個別実行する。新 host で。
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r29 = driver::run_item29(&host);
            items.push(ItemReport {
                item: 29, name: "keyboard-disabled context passes keys through (Edge password)".into(),
                status: if r29.passed { "pass" } else { "fail" }.into(),
                detail: r29.detail, max_elapsed_ms: 0,
            });
        }
        Err(e) => items.push(ItemReport {
            item: 29, name: "keyboard-disabled context passes keys through (Edge password)".into(),
            status: "error".into(),
            detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
        }),
    }

    // item30（Task4 確定取消 headless 回帰・往路）: nihongo→Space→Enter→Ctrl+Backspace→Esc の
    // 往復が無傷に成立すること。Ctrl 修飾の注入（feed_key_with_ctrl）が要るので専用ドライバで
    // 個別実行する。新 host で。
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r30 = driver::run_item30(&host);
            items.push(ItemReport {
                item: 30, name: "commit-undo round trip (Ctrl+Backspace then Esc restores)".into(),
                status: if r30.passed { "pass" } else { "fail" }.into(),
                detail: r30.detail, max_elapsed_ms: 0,
            });
        }
        Err(e) => items.push(ItemReport {
            item: 30, name: "commit-undo round trip (Ctrl+Backspace then Esc restores)".into(),
            status: "error".into(),
            detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
        }),
    }

    // item31（Task4 確定取消 headless 回帰・disarm）: 確定後の打鍵/settle 経由で武装が解除され
    // Ctrl+Backspace が素通しになること（C-1 の settle 経路 armed 非残留を含む）。新 host で。
    match tsf_host::TsfHost::start() {
        Ok(host) => {
            let r31 = driver::run_item31(&host);
            items.push(ItemReport {
                item: 31, name: "commit-undo disarms after further keystroke/settle".into(),
                status: if r31.passed { "pass" } else { "fail" }.into(),
                detail: r31.detail, max_elapsed_ms: 0,
            });
        }
        Err(e) => items.push(ItemReport {
            item: 31, name: "commit-undo disarms after further keystroke/settle".into(),
            status: "error".into(),
            detail: format!("start fail: {e:?}"), max_elapsed_ms: 0,
        }),
    }

    // 実行順（…,10,8,9,12,13,14,15,16,17,18,19,24）ではなく item 番号昇順で表示・出力する。
    items.sort_by_key(|i| i.item);
    let all_pass = items.iter().all(|i| i.status == "pass");
    let rep = HarnessReport { all_pass, started: true, start_error: None, items };
    rep.print_table();
    if let Some(p) = json_path { let _ = rep.write_json(&p); }
    rep.exit_code()
}
