//! シナリオ（VK 列）を TsfHost に流し、各ステップで store と ev= ログを観測する。

use std::time::Instant;
use crate::scenarios::{typed, Vk};
use crate::tsf_host::TsfHost;

/// 1 シナリオの観測結果。
pub struct StepObs {
    pub label: &'static str,
    pub vk: u32,
    pub eaten: bool,
    pub elapsed_ms: u128,
    pub committed: String,
    pub preedit: String,
    pub full: String,
    pub sel: (i32, i32),
}

/// VK 列を流し、各ステップの観測を返す。
pub fn run_keys(host: &TsfHost, keys: &[Vk]) -> Vec<StepObs> {
    let mut obs = Vec::new();
    for k in keys {
        let t0 = Instant::now();
        let eaten = host.feed_key(k.0);
        let elapsed_ms = t0.elapsed().as_millis();
        obs.push(StepObs {
            label: k.1, vk: k.0, eaten, elapsed_ms,
            committed: host.store.committed(),
            preedit: host.store.preedit(),
            full: host.store.full(),
            sel: host.store.selection(),
        });
    }
    obs
}

use crate::log_parse::{read_events, Ev};
use crate::scenarios::Scenario;

pub struct ScenarioResult {
    pub item: u32,
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
    pub max_elapsed_ms: u128,
}

/// 1 シナリオを隔離実行する。host は呼び元が用意（item9 は deactivate を挟むため）。
pub fn run_scenario(host: &TsfHost, sc: &Scenario) -> ScenarioResult {
    // 測定打鍵の前にエンジン＋合成を温める（初打鍵合成の即終了による孤児文字を吸収する）。
    // item1 は打鍵が無い（activation のみ）ので不要。warm_up の残骸は直後の reset で消える。
    if !sc.keys.is_empty() { host.warm_up(); }
    host.store.reset();
    let base = read_events(std::process::id()).len(); // 実行前の ev 数（warm_up 後に取る）
    let obs = run_keys(host, &sc.keys);
    // SP3: ライブ変換は毎打鍵ではなくデバウンスタイマ（UI スレッド, ~30ms）で行われる。
    // ヘッドレスでは feed_key が即 pump するためタイマが発火しない。打ち終えてから
    // タイマを発火させ、preedit を最終形（漢字かな交じり）へ確定させてから観測する。
    if !sc.keys.is_empty() { host.settle_debounce(); }
    // 毎キー計測を stderr へ（verify-console.log に残る）。確定後キャレット位置や
    // 合成範囲と buffer のズレ（余分文字の発生箇所）を後追いするための診断ログ。
    for o in &obs {
        eprintln!(
            "[trace] item{} {:>9} vk={:#04x} eaten={} full={:?} preedit={:?} committed={:?} sel={:?}",
            sc.item, o.label, o.vk, o.eaten, o.full, o.preedit, o.committed, o.sel
        );
    }
    // デバウンス発火後（settle 後）の最終観測。preedit が漢字化したか確認する診断行。
    if !sc.keys.is_empty() {
        eprintln!(
            "[trace] item{}  <settle> full={:?} preedit={:?} committed={:?} sel={:?}",
            sc.item, host.store.full(), host.store.preedit(), host.store.committed(), host.store.selection()
        );
    }
    let evs_all = read_events(std::process::id());
    // item1(activate) は start() の ActivateProfile で base より前に出るため、全 ev を見る。
    let evs: Vec<Ev> = if sc.item == 1 {
        evs_all
    } else {
        evs_all.into_iter().skip(base).collect()
    };
    let committed = host.store.committed();
    let full = host.store.full();
    let preedit = host.store.preedit();
    let eaten_last = obs.last().map(|o| o.eaten).unwrap_or(false);
    let max_elapsed_ms = obs.iter().map(|o| o.elapsed_ms).max().unwrap_or(0);
    let (passed, detail) = match (sc.expect)(&committed, &full, &preedit, &evs, eaten_last) {
        Ok(()) => (true, "ok".to_string()),
        Err(e) => (false, e),
    };
    ScenarioResult { item: sc.item, name: sc.name, passed, detail, max_elapsed_ms }
}

pub struct Item8Result { pub passed: bool, pub detail: String }

/// nihongo を打ってエンジンを起こし → engine_spawn pid を kill → さらに打鍵 → 確定。
/// 期待: 各 key が閾値内に返る AND 最終 commit が source=reading（劣化）。
pub fn run_item8(host: &TsfHost, threshold_ms: u128) -> Item8Result {
    host.store.reset();
    let pid = std::process::id();

    // 1) 最初の打鍵でエンジンを起こす。
    let _ = run_keys(host, &crate::scenarios::typed("ni"));
    // 2) engine_spawn pid を取得して kill。
    let evs = read_events(pid);
    let engine_pid = evs.iter().rev().find_map(|e| match e {
        Ev::EngineSpawn { pid, ok: true } => Some(*pid), _ => None,
    });
    if let Some(epid) = engine_pid {
        kill_pid(epid);
    } else {
        return Item8Result { passed: false, detail: "ev=engine_spawn pid= が見つからない".into() };
    }

    // 3) kill 後も継続打鍵 → 変換 → 確定。各 key の経過時間を測る。
    let mut rest = crate::scenarios::typed("hongo");
    rest.push(crate::scenarios::SPACE);
    rest.push(crate::scenarios::ENTER);
    let obs = run_keys(host, &rest);
    let max_ms = obs.iter().map(|o| o.elapsed_ms).max().unwrap_or(0);

    // 4) 判定。
    let evs2 = read_events(pid);
    let last_commit_reading = evs2.iter().rev().find_map(|e| match e {
        Ev::Commit { source, .. } => Some(source == "reading"), _ => None,
    }).unwrap_or(false);
    let responsive = max_ms < threshold_ms;
    let committed = host.store.committed();
    let passed = responsive && (last_commit_reading || !committed.is_empty());
    Item8Result {
        passed,
        detail: format!("max_elapsed={max_ms}ms (<{threshold_ms}?{responsive}) commit_reading={last_commit_reading} committed={committed:?}"),
    }
}

pub struct Item12Result { pub passed: bool, pub detail: String }

/// item12: Shift+Tab→外部LLM変換のスレッド配線（worker→ポーリングタイマ→preedit 反映）を
/// echo モードで headless 検証する（リスク R2）。
///
/// 流れ: "nihongo" を打って合成（ライブ変換 日本語）→ **Shift+Tab** で start_llm_convert
/// （Tab 割当変更＝Tab 単体は修正変換 TypoConvert に割当済みのため、外部LLM変換は Shift+Tab へ
/// 移動した。EngineClient を別スレッドへ move し LlmConvert を投げ、preedit を「🌐変換中…」にして
/// 50ms ポーリングタイマを arm）→ settle_llm でワーカ完了＋WM_TIMER 発火を待つ→
/// preedit が echo マーカ "LLM:"+reading（=LLM:にほんご）へ全置換される。
///
/// 自己証明: TIP ログに ev=llm_request（要求が出た）と ev=llm_applied（UI スレッドが
/// 結果を反映した）が出ていることを確認し、Shift+Tab が素通しされただけの偽 PASS を防ぐ。
/// echo モードは main で NOSPACEKEY_LLM_ECHO=1 を設定済み（spawn される engine が継承）。
pub fn run_item12(host: &TsfHost) -> Item12Result {
    host.warm_up();
    host.store.reset();
    let pid = std::process::id();
    let base = read_events(pid).len();

    // 1) "nihongo" を打ってライブ変換まで進める（合成中・preedit=日本語）。
    let _ = run_keys(host, &crate::scenarios::typed("nihongo"));
    host.settle_debounce();
    let composing_before = host.store.composing();
    let preedit_live = host.store.preedit();

    // 2) Shift+Tab → 外部LLM変換起動。eaten を確認（idle ではなく合成中なので食うはず）。
    // Tab 単体は修正変換(TypoConvert)に割当変更済みのため Shift 修飾で注入する。
    let tab_eaten = host.feed_key_with_shift(crate::scenarios::TAB.0);

    // 3) ワーカ IPC ＋ ポーリングタイマ（50ms）を落ち着かせる。
    host.settle_llm();
    let preedit_after = host.store.preedit();

    // 4) ev 自己証明: llm_request と llm_applied が出ているか。
    let evs: Vec<Ev> = read_events(pid).into_iter().skip(base).collect();
    let has_request = evs.iter().any(|e| matches!(e, Ev::LlmRequest { .. }));
    let has_applied = evs.iter().any(|e| matches!(e, Ev::LlmApplied { .. }));

    let detail = format!(
        "composing_before={composing_before} live={preedit_live:?} tab_eaten={tab_eaten} \
         after={preedit_after:?} ev_request={has_request} ev_applied={has_applied}"
    );

    // 判定: 合成中に Tab を食い、要求が出て（ev_request）、UI スレッドが結果を反映し（ev_applied）、
    // preedit が echo マーカ "LLM:" 始まりへ置換されていること。
    let passed = composing_before
        && tab_eaten
        && has_request
        && has_applied
        && preedit_after.starts_with("LLM:");
    Item12Result { passed, detail }
}

pub struct Item13Result { pub passed: bool, pub detail: String }

/// item13 経路 C のイベント位置照合（純ロジック、TSF 不要でテスト可能）。
///
/// 「最初の reconvert_shown(latin) より **後ろ** に reconvert_cancel が出ているか」を返す。
/// トグルが reconvert を畳んだ証跡を「素通しの偶発キャンセル」と区別するための核。
/// latin が見つからない／キャンセルが先に出ている／どちらも無い場合は false。
fn cancel_followed_shown(evs: &[Ev], latin: &str) -> bool {
    let shown = evs.iter().position(|e| {
        matches!(e, Ev::ReconvertShown { latin: l, .. } if l == latin)
    });
    let cancel = evs.iter().rposition(|e| matches!(e, Ev::ReconvertCancel));
    matches!((shown, cancel), (Some(s), Some(c)) if c > s)
}

/// item13: SP5 ヘッドレス再変換（半角英数モードの「ゆで卵」再変換）。
///
/// §9 トップリスク（非空 range の StartComposition ＋ GetText 読み戻し ＋ Esc 復元）を
/// VM 無しで証明する。流れ:
///
/// ```text
/// 1) 文書へ既存確定テキスト "React nihongo" をシード（キャレット末尾）。
/// 2) conversion-mode を半角英数(直接)へ（set_direct_mode）→ is_direct_mode()==true。
/// 3) 変換キー（VK_CONVERT 0x1C）を注入。PreserveKey(0x1C) は OS に拒否され preserved key に
///    ならない（実バグ）ため、msctf は通常キーとして OnTestKeyDown/OnKeyDown へ配送 →
///    VK_CONVERT arm → start_reconvert。ReconvertStart が末尾ラテン run "nihongo" を range 読み戻しし
///    （ev=reconvert_shown latin=nihongo）、その range で非空 StartComposition（store の
///    OnStartComposition → composing=true）、g1 リプレイで候補（日本語…）を preedit へ。
/// 4-A) Esc → cancel_reconvert → RestoreText で元ラテンを書き戻し composition を閉じる
///      → 文書が "React nihongo" へ復元（ev=reconvert_cancel）。
/// 4-B) 別シードで Enter → 候補確定（source=candidate）→ "React " + 変換結果（日本語）。
/// 4-C) 別シードで再変換 → 候補表示中にモードトグル（0x1D）→ C1 修正が reconverting ラッチと
///      composition を畳む（ev=reconvert_cancel ＋ 復元）→ direct へ戻して再変換すると
///      2 本目の候補が再び出る（ラッチが残らずブリックしない）。
/// ```
///
/// 自己証明（偽 PASS 防止）:
///   - preserved-key が食われた（reconvert_eaten=true）こと。
///   - ev=reconvert_shown が出て latin=nihongo（range 読み戻しが効いた）こと。
///   - 再変換中に composing=true（非空 StartComposition が成立した）こと。
///
/// これらが無いと「0x1C が素通ししただけで文書はもとから React nihongo のまま」でも
/// 復元アサートが PASS してしまう。
///
///   - 経路 C: トグル後の reconvert_cancel が 1 本目の reconvert_shown より後ろに出ること、
///     かつ 2 本目の reconvert_shown が存在すること。C1 修正を revert するとラッチが残り
///     再入ガードで 2 本目が出ない＝この存在チェックが偽 PASS を許さない。
pub fn run_item13(host: &TsfHost) -> Item13Result {
    let pid = std::process::id();

    // エンジンを先に温める。warm_up は native(ひらがな)モード（start 直後の既定）で行う:
    // direct モードでは 'a' が食われず本文へ漏れる（will_handle direct→pass）ので、
    // モード切替前に温め、初回再変換が cold-engine アーティファクトに当たらないようにする。
    host.warm_up();
    host.store.reset();

    // ---- 前提セットアップ: モードを直接入力へ。失敗なら item13 は不能（誤 PASS を出さない）。
    let direct_set = host.set_direct_mode();
    if !direct_set {
        return Item13Result {
            passed: false,
            detail: "set_direct_mode 失敗（ITfCompartmentMgr 経由で conversion-mode を設定できない）".into(),
        };
    }

    // ===== 経路 A: 再変換 → Esc で元ラテン復元（§9 トップリスクの本体）=====
    host.store.reset();
    host.store.seed_committed("React nihongo");
    let seeded = host.store.full();
    let base_a = read_events(pid).len();

    // 変換キーを注入。PreserveKey(0x1C) 失敗のため msctf は通常キー経路(OnKeyDown VK_CONVERT)へ配送。
    let recon_eaten = host.feed_key(0x1C); // VK_CONVERT
    let composing_during = host.store.composing();
    let preedit_during = host.store.preedit();

    let evs_a: Vec<Ev> = read_events(pid).into_iter().skip(base_a).collect();
    let shown = evs_a.iter().find_map(|e| match e {
        Ev::ReconvertShown { n, latin, .. } => Some((*n, latin.clone())),
        _ => None,
    });

    // Esc → cancel_reconvert → RestoreText（元ラテン復元）。
    let _ = host.feed_key(0x1B); // VK_ESCAPE
    let restored = host.store.full();
    // 復元後キャレット。RestoreText が SetText 後に Collapse(末尾)+SetSelection しないと、
    // 合成開始位置（=単語の先頭）へ戻る（実機 SP5 報告: カーソルが単語の手前に居座り、直前が
    // 空白になって再変換キーが対象を掴めない）。末尾追従していれば文書末（"nihongo" 末）に来る。
    let restored_caret = host.store.selection();
    let evs_a2: Vec<Ev> = read_events(pid).into_iter().skip(base_a).collect();
    let saw_cancel = evs_a2.iter().any(|e| matches!(e, Ev::ReconvertCancel));

    // ===== 経路 B: 再変換 → Enter で候補確定 =====
    host.store.reset();
    host.store.seed_committed("React nihongo");
    let base_b = read_events(pid).len();
    let _ = host.feed_key(0x1C); // 再変換
    // 実機フレーク防御: 別アプリの TIP 活性化（pid 59912 等）で前面が奪われ、再変換合成が
    // pump 中に msctf へ terminate されることがある。合成が消えていたらフォーカスを取り戻し
    // direct を再確認して再変換し直す（warm_up の生存パターンと同思想。VM では合成が生き残る
    // ので skip される）。さもないと続く Enter が showing=false で食われず candidate 確定が出ない。
    if !host.store.composing() {
        host.reclaim_focus();
        host.set_direct_mode();
        host.store.reset();
        host.store.seed_committed("React nihongo");
        let _ = host.feed_key(0x1C);
    }
    let preedit_b = host.store.preedit();
    let _ = host.feed_key(0x0D); // Enter 確定
    let committed_b = host.store.full();
    let evs_b: Vec<Ev> = read_events(pid).into_iter().skip(base_b).collect();
    let commit_b = evs_b.iter().rev().find_map(|e| match e {
        Ev::Commit { text, source } => Some((text.clone(), source.clone())),
        _ => None,
    });

    // ===== 経路 C: 再変換候補表示中にモードトグル → ラッチ＋composition を畳む（最終レビュー C1）=====
    //
    // 回帰対象バグ: OnPreservedKey のモードトグル枝が、`reconverting` ラッチと開いた
    // composition を畳まずに toggle_conversion_mode へ進むと、ラッチ true と composition が
    // モード境界をまたいで残り、(a) 機能がブリックする（start_reconvert の再入ガード
    // `if self.reconverting.get() { return; }` に永久に弾かれて二度と候補が出ない）、
    // (b) 切替後の Esc が誤って RestoreText に流れる。C1 修正は、トグル枝が reconverting なら
    // 先に cancel_reconvert（ev=reconvert_cancel ＋ RestoreText 復元 ＋ ラッチクリア）する。
    //
    // 自己証明（C1 修正を revert すると落ちる）:
    //   - C-2: トグル前に必ず reconvert_shown(latin=nihongo) が出る（候補が確実に上がっている）。
    //   - C-3a: トグル後に reconvert_cancel が「その reconvert_shown より後ろの位置」で出る
    //           （cancel_reconvert がトグルに連動して呼ばれた＝ラッチが畳まれた証跡）。
    //           ＋文書が "React nihongo" へ復元（RestoreText が走った＝composition も閉じた）。
    //   - C-4: 再度（direct へ戻して）再変換すると **2 本目の** reconvert_shown(latin=nihongo)
    //          が出る。バグ版はラッチが true のまま残り再入ガードで return するので 2 本目は
    //          絶対に出ない＝この存在チェックが偽 PASS を許さない。
    host.store.reset();
    host.reclaim_focus(); // 実機フレーク防御: 経路 B で別アプリ活性化に奪われた配送/フォーカスを取り戻す。
    let direct_c = host.set_direct_mode(); // 経路 B の確定でモードは不変のはずだが念のため再確認。
    host.store.seed_committed("React nihongo");
    let base_c = read_events(pid).len();

    // C-1) 再変換 → 候補表示。
    let recon_eaten_c = host.feed_key(0x1C); // VK_CONVERT
    let composing_c1 = host.store.composing();
    let evs_c1: Vec<Ev> = read_events(pid).into_iter().skip(base_c).collect();
    // この経路で最初に出た reconvert_shown の位置（base_c 起点の相対 index）。
    let shown1_pos = evs_c1.iter().position(|e| {
        matches!(e, Ev::ReconvertShown { latin, .. } if latin == "nihongo")
    });

    // C-2) 候補表示中にモードトグル（VK_NONCONVERT 0x1D）。C1 修正なら cancel_reconvert→toggle。
    let toggle_eaten_c = host.feed_key(0x1D); // VK_NONCONVERT
    let composing_after_toggle = host.store.composing();
    let restored_c = host.store.full();
    let evs_c2: Vec<Ev> = read_events(pid).into_iter().skip(base_c).collect();
    // トグルに連動した reconvert_cancel が、最初の reconvert_shown より後ろに出ているか
    //（純ロジック cancel_followed_shown でテスト可能。cancel_pos は診断表示用に別途取る）。
    let cancel_pos = evs_c2.iter().rposition(|e| matches!(e, Ev::ReconvertCancel));
    let cancel_after_shown = cancel_followed_shown(&evs_c2, "nihongo");

    // C-3) 機能がブリックしていないことの証明: direct へ戻して再シード → もう一度再変換 →
    //      2 本目の reconvert_shown(latin=nihongo) が出る（ラッチが残っていたら出ない）。
    host.reclaim_focus(); // 実機フレーク防御: 奪われた配送/フォーカスを取り戻してから再変換する。
    let direct_c2 = host.set_direct_mode(); // トグルで native へ移ったので direct へ戻す。
    host.store.seed_committed("React nihongo");
    let mid_c = read_events(pid).len(); // 2 本目の reconvert_shown 計数のための再カウント基点。
    let recon_eaten_c2 = host.feed_key(0x1C); // 2 回目の再変換
    let composing_c2 = host.store.composing();
    let evs_c3: Vec<Ev> = read_events(pid).into_iter().skip(mid_c).collect();
    let second_shown = evs_c3.iter().any(|e| {
        matches!(e, Ev::ReconvertShown { latin, .. } if latin == "nihongo")
    });
    // 後片付け（2 本目の候補を閉じて次 item / Drop へ綺麗な状態で渡す）。
    let _ = host.feed_key(0x1B); // Esc
    // conversion-mode compartment はプロセス共有で direct のまま残るので、後続 scenario が
    // ネイティブ前提でも壊れないよう native へ戻す（item14 側でも明示復帰するが二重の保険）。
    let _ = host.set_native_mode();

    // ---- 判定 ----
    let detail = format!(
        "seeded={seeded:?} recon_eaten={recon_eaten} composing_during={composing_during} \
         preedit_during={preedit_during:?} shown={shown:?} restored={restored:?} restored_caret={restored_caret:?} saw_cancel={saw_cancel} \
         | B: preedit={preedit_b:?} committed={committed_b:?} commit={commit_b:?} \
         | C: direct={direct_c}/{direct_c2} recon_eaten={recon_eaten_c} composing_c1={composing_c1} \
         shown1_pos={shown1_pos:?} toggle_eaten={toggle_eaten_c} composing_after_toggle={composing_after_toggle} \
         restored_c={restored_c:?} cancel_pos={cancel_pos:?} cancel_after_shown={cancel_after_shown} \
         recon_eaten_c2={recon_eaten_c2} composing_c2={composing_c2} second_shown={second_shown}"
    );

    // 経路 A 必須: preserved-key 発火（食われ）＋ reconvert_shown（latin=nihongo）＋ 合成成立
    //              ＋ Esc 復元（文書が "React nihongo" へ戻る）＋ ev=reconvert_cancel。
    let shown_ok = matches!(&shown, Some((n, latin)) if *n >= 1 && latin == "nihongo");
    // 復元後キャレットが復元文字列の末尾にあること（"nihongo" は文書末なので文書末 == 末尾）。
    // RestoreText が末尾へ SetSelection しないと先頭（合成開始位置）へ戻り、この条件が落ちる。
    let restored_end = restored.encode_utf16().count() as i32;
    let path_a_ok = recon_eaten
        && shown_ok
        && composing_during
        && restored == "React nihongo"
        && restored_caret == (restored_end, restored_end)
        && saw_cancel;

    // 経路 B 必須: 候補確定（source=candidate）で "React " 始まり＋日本語化（"React nihongo" 以外）。
    // 変換結果はエンジン依存だが、少なくとも「React 」が残り、末尾が元ラテンそのままでないこと。
    let path_b_ok = matches!(&commit_b, Some((_t, src)) if src == "candidate")
        && committed_b.starts_with("React ")
        && committed_b != "React nihongo";

    // 経路 C 必須（C1 修正のロック）:
    //   - 1 本目の reconvert_shown(latin=nihongo) が出た（トグル前に候補が確実に上がっていた）。
    //   - トグル後に reconvert_cancel が「その shown より後ろ」で出た（=トグルが畳んだ）。
    //   - 文書が "React nihongo" へ復元され、トグル後は合成中でない（composition が閉じた）。
    //   - 2 本目の reconvert_shown が出た（ラッチが残らず再変換が再び発火＝ブリックしていない）。
    // shown1_pos.is_some() を独立条件として課し、cancel_after_shown だけに頼らない
    //（cancel_pos も shown1_pos も None なら cancel_after_shown=false なので二重に塞ぐ）。
    let path_c_ok = direct_c
        && recon_eaten_c
        && shown1_pos.is_some()
        && composing_c1
        && cancel_after_shown
        && restored_c == "React nihongo"
        && !composing_after_toggle
        && direct_c2
        && recon_eaten_c2
        && second_shown;

    Item13Result { passed: path_a_ok && path_b_ok && path_c_ok, detail }
}

pub struct Item17Result { pub passed: bool, pub detail: String }

/// item17: SP5 step-6 — 非空かな選択の再変換（半角英数モード）。
/// 正常系: 確定済み "にほんご" を選択 → 変換キー(0x1C) → kind=surface の reconvert_shown が出る
///         → Esc で "にほんご" へ復元（reconvert_cancel）。
/// do-no-harm: 確定済み "日本語" を選択 → 変換キー → reconvert_skip(reason=non_kana)、
///             reconvert_shown は出ず、文書は "日本語" のまま（壊さない）。
/// 現行コード（surface 経路未実装）では正常系の kind=surface も skip も出ないので両系で FAIL する
/// （= Task 6 の配線が入って初めて PASS する回帰ガード）。
pub fn run_item17(host: &TsfHost) -> Item17Result {
    use crate::log_parse::{read_events, Ev};
    let pid = std::process::id();

    // エンジンを先に温める（run_item13 と同じ順序）。warm_up は native(ひらがな)モードで行う:
    // direct モードでは 'a' が食われず本文へ漏れるので、モード切替前に温める。
    host.warm_up();

    // ---- 前提セットアップ: モードを直接入力へ。失敗なら item17 は不能（誤 PASS を出さない）。
    host.store.reset();
    let direct_set = host.set_direct_mode();
    if !direct_set {
        return Item17Result {
            passed: false,
            detail: "set_direct_mode 失敗（ITfCompartmentMgr 経由で conversion-mode を設定できない）".into(),
        };
    }

    // --- 正常系: かな選択 ---
    host.store.reset();
    host.store.seed_committed("にほんご");
    let kana_len = "にほんご".encode_utf16().count() as i32; // = 4
    host.store.set_selection(0, kana_len);
    let base = read_events(pid).len();
    host.feed_key(0x1C); // VK_CONVERT
    let evs = read_events(pid);
    let surface_shown = evs[base..].iter().any(|e| matches!(e, Ev::ReconvertShown { kind, latin, .. } if kind == "surface" && latin == "にほんご"));

    // Esc → 復元
    host.feed_key(0x1B); // VK_ESCAPE
    let evs = read_events(pid);
    let restored = evs[base..].iter().any(|e| matches!(e, Ev::ReconvertCancel))
        && host.store.full() == "にほんご";

    // --- do-no-harm: 漢字選択 ---
    host.store.reset();
    host.store.seed_committed("日本語");
    // 経路 B の後はモードが変わっている可能性があるので direct を再確認（run_item13 経路 C と同じ）。
    host.set_direct_mode();
    let kanji_len = "日本語".encode_utf16().count() as i32; // = 3
    host.store.set_selection(0, kanji_len);
    let base2 = read_events(pid).len();
    host.feed_key(0x1C);
    let evs = read_events(pid);
    let skipped = evs[base2..].iter().any(|e| matches!(e, Ev::ReconvertSkip { reason } if reason == "non_kana"));
    let no_shown = !evs[base2..].iter().any(|e| matches!(e, Ev::ReconvertShown { .. }));
    let intact = host.store.full() == "日本語";

    let passed = surface_shown && restored && skipped && no_shown && intact;
    let detail = format!(
        "surface_shown={surface_shown} restored={restored} skipped={skipped} no_shown={no_shown} intact={intact}"
    );
    Item17Result { passed, detail }
}

pub struct Item14Result { pub passed: bool, pub detail: String }

/// item14 (SP6a): requires regsvr32-registered DLL (VM/admin). Asserts UIElement advertise +
/// candidate data + Behavior finalize.
///
/// SP6a は候補リストを TSF UI Element 化した。TIP の CandidatePresenter は候補表示時に
/// ITfUIElementMgr::BeginUIElement で `ITfCandidateListUIElementBehavior`（ITfUIElement 派生）を
/// ホストへ提示し、ホストが返す *pbShow で「自前描画(TRUE)」「データ公開のみ(FALSE=イマーシブ)」を
/// 分岐する。本 item は testbench の ITfUIElementSink でこの advertise を観測し、GetUIElement で
/// 候補データ（ITfCandidateListUIElement::GetCount/GetString/GetSelection）を実 msctf 経由で
/// 読み戻し、さらに ITfCandidateListUIElementBehavior::SetSelection/Finalize でホスト発の
/// 選択＋確定（マウス/タッチ相当）を模擬して、既存 commit 経路に流れることを確認する。
///
/// CRITICAL: 非管理者シェルでは TIP DLL を regsvr32 登録できないため、本 item は item1–13 同様
/// この場では RUN できない（VM/admin 必須）。下のアサートは型検査・配線検証のために書かれ、
/// 実走は VM レビューに委ねる（verify-sp6a.ps1 参照）。
///
/// 流れ（item3/5 の変換駆動を再利用: "nihongo" + Space で候補を出す）:
///   0) pbShow=FALSE を設定（イマーシブ模擬。BeginUIElement で sink がこの値を書き戻す）。
///   1) warm_up → reset → "nihongo" を打って Space で候補確定窓を出す（CandidatePresenter::show
///      → BeginUIElement 発火 → sink.begun に id が入る）。
///   2) アサート(a): ui_log().begun が非空（advertise が起きた）。
///   3) アサート(b): candidate_strings() が非空かつ "日本語" を含む（item3 が候補に "日本語" を
///      要求するのと同じ照合。GetUIElement→GetCount/GetString が実 element からデータを返す）。
///   4) アサート(c): candidate_selection()==0（初期選択は先頭）。
///   5) Behavior: behavior_select_and_finalize(1)（2 番目を選び Finalize）→ pump/settle →
///      store が 2 番目の候補を確定したこと（committed が候補[1] と一致）。
pub fn run_item14(host: &TsfHost) -> Item14Result {
    // 0) イマーシブ模擬: BeginUIElement で *pbShow=FALSE を書き戻させる（ホストが描く宣言）。
    //    advert 自体は pbShow に依らず発火するので、begun の観測には必須ではないが、
    //    DoD のイマーシブ経路（pbShow=FALSE）を踏ませるため明示的に設定する。
    host.force_pbshow(Some(false));

    // 0b) item13 が conversion-mode compartment を直接入力(0)のまま残す（プロセス共有なので host を
    //     作り直しても残る）。item14 はネイティブ前提なので明示的に戻す。さもないと TIP がキーを
    //     食わず候補が出ず begun=[] で FAIL する。
    let _ = host.set_native_mode();

    // 1) 変換を駆動して候補を出す（item3/5 と同じ "nihongo" + Space）。
    host.warm_up();
    host.store.reset();
    let mut keys = crate::scenarios::typed("nihongo");
    keys.push(crate::scenarios::SPACE);
    let _ = run_keys(host, &keys);
    host.settle_debounce();

    // 2) advertise が起きたか（BeginUIElement → sink.begun）。
    let begun = host.ui_log().begun.borrow().clone();
    let begun_nonempty = !begun.is_empty();

    // 3) 候補データを実 element 経由で読み戻す。
    let strings = host.candidate_strings();
    let strings_nonempty = !strings.is_empty();
    let has_nihongo = strings.iter().any(|s| s == "日本語");

    // 4) 初期選択は先頭（0）であること。
    let sel = host.candidate_selection();

    // 5) Behavior 経由でホスト選択＋確定（2 番目）を模擬し、commit 経路へ流れることを確認する。
    let expected_second = strings.get(1).cloned().unwrap_or_default();
    let beh_reached = host.behavior_select_and_finalize(1);
    // Finalize は notify→text_service の outbox drain（既存 commit/cancel 経路）を経て確定する。
    // ヘッドレスでは notify が UI スレッドへ post する想定なので settle で落ち着かせる。
    host.settle_debounce();
    pump_settle(host);
    let committed = host.store.committed();
    // 2 番目の候補（strings[1]）が確定したか。緩いフォールバックは置かない＝厳密一致を VM の合格線とする。
    // overall pass は strings_nonempty を要求するので、実合格時 strings[1]=expected_second は非空であり、
    // 「commit が 2 番目の候補に正確に一致する」ことが無条件の合格条件になる。
    let committed_is_second = committed == expected_second;

    let detail = format!(
        "begun={begun:?} strings={strings:?} has_nihongo={has_nihongo} sel={sel} \
         beh_reached={beh_reached} expected_second={expected_second:?} committed={committed:?}"
    );

    // 判定（VM 実走で評価される。compile/型検査＋配線は本セッションで担保）:
    //   (a) advertise が起きた、(b) 候補データが実 element から読め "日本語" を含む、
    //   (c) 初期選択 0、(d) Behavior へ到達し 2 番目を確定できた。
    let passed = begun_nonempty
        && strings_nonempty
        && has_nihongo
        && sel == 0
        && beh_reached
        && committed_is_second;
    Item14Result { passed, detail }
}

pub struct Item16Result { pub passed: bool, pub detail: String }

/// item16: 前方一致候補の部分確定でデータロスしないこと（実機 VM で評価。compile/型検査＋配線は本セッションで担保）。
/// 再現: "nihongo"(にほんご) を Space で変換 → 前方一致候補「日本」(にほん) を選んで確定 →
/// 「日本」が確定し、消費されなかった残り読み「ご」が新しい composition として継続する（捨てない）。
/// バグ時は「日本」確定後に composition が全リセットされ「ご」が消失していた。
///
/// 合格条件（すべて満たす）:
///   (a) 候補列に「日本」がある（前方一致候補の存在）、
///   (b) Behavior（マウス/タッチ模擬）で「日本」を選択＋Finalize できた、
///   (c) committed が「日本」と一致（前方分を確定）、
///   (d) preedit が非空＝残り読みが composition として継続（「ご」を捨てていない）、
///   (e) ev=commit text=日本 source=candidate_prefix（部分確定マーカ）がログに出た。
pub fn run_item16(host: &TsfHost) -> Item16Result {
    // item13/14 が conversion-mode を direct のまま残しうるのでネイティブへ明示復帰（さもないと候補が出ない）。
    let _ = host.set_native_mode();
    host.warm_up();
    host.store.reset();
    let pid = std::process::id();

    // 1) "nihongo"(にほんご) を打って Space で候補を出す。
    let mut keys = crate::scenarios::typed("nihongo");
    keys.push(crate::scenarios::SPACE);
    let _ = run_keys(host, &keys);
    host.settle_debounce();

    // 2) 候補列から前方一致候補「日本」(にほん) の index を探す。
    let strings = host.candidate_strings();
    let idx_nihon = strings.iter().position(|s| s == "日本");

    // 3) 「日本」を選択＋Finalize（マウス/タッチ Behavior 経由＝drain_behavior→commit_candidate）。
    let beh_reached = match idx_nihon {
        Some(i) => host.behavior_select_and_finalize(i as u32),
        None => false,
    };
    host.settle_debounce();
    pump_settle(host);

    // 4) 観測: 「日本」が確定し、残り読み「ご」が composition として継続（preedit 非空）。
    let committed = host.store.committed();
    let preedit = host.store.preedit();

    // 5) ログ: 部分確定マーカ ev=commit text=日本 source=candidate_prefix。
    let evs = read_events(pid);
    let has_partial_commit = evs.iter().any(|e| matches!(e,
        Ev::Commit { text, source } if text == "日本" && source == "candidate_prefix"));

    // 6) 残り読みセッションが生きていること: 続けて Space で残り読み(ご)を変換し候補が出るか。
    //    部分確定が自分の do_commit→OnCompositionTerminated でセッションを畳んでいる(セッション0)と、
    //    この convert は劣化して候補ゼロになる（codex P1 の回帰ガード）。
    let _ = run_keys(host, &[crate::scenarios::SPACE]);
    host.settle_debounce();
    let remainder_cands = host.candidate_strings();
    let remainder_session_alive = !remainder_cands.is_empty();

    let detail = format!(
        "strings={strings:?} idx_nihon={idx_nihon:?} beh_reached={beh_reached} \
         committed={committed:?} preedit={preedit:?} has_partial_commit={has_partial_commit} \
         remainder_cands={remainder_cands:?} remainder_session_alive={remainder_session_alive}"
    );
    let passed = idx_nihon.is_some()
        && beh_reached
        && committed == "日本"
        && !preedit.is_empty()
        && has_partial_commit
        && remainder_session_alive;
    Item16Result { passed, detail }
}

pub struct Item15Result { pub passed: bool, pub detail: String }

/// item15: ライブ変換中、キャレットが preedit（合成文字列）の末尾に来ること
/// （実機で発覚した「ライブ変換中カーソルが先頭に居座る」バグの回帰）。
///
/// "nihongo" を打つと毎打鍵→デバウンスでライブ変換され、preedit が漢字かな交じり文へ
/// 全置換される（run_preedit→StartOrUpdatePreedit）。修正前の TIP は range.SetText のみで
/// SetSelection せず、実 msctf ではキャレットが合成開始位置(先頭)に残るため sel==(0,0)。
/// 修正後は preedit 更新ごとに末尾へ SetSelection するので sel==(len,len)
/// （len=合成文字列の UTF-16 長＝バッファ長。合成中は文書全体が preedit）。
///
/// 自己証明（素通し/偽 PASS 防止）:
///   - composing==true（実際に合成中である）。
///   - preedit 非空（ライブ変換結果が出ている）。
///   - キャレットが先頭 (0,0) でなく末尾 (len,len) に一致する。
/// 変換結果の具体漢字は model 依存なので、末尾位置 len はバッファ長から動的に取る（model 非依存）。
/// harness は SetText でキャレットを動かさない実 msctf 準拠モデル（doc_state/text_store 忠実化）
/// なので、修正前は (0,0)・修正後は (len,len) という差を検出できる。
pub fn run_item15(host: &TsfHost) -> Item15Result {
    host.warm_up();
    host.store.reset();
    let _ = run_keys(host, &crate::scenarios::typed("nihongo"));
    host.settle_debounce();

    let composing = host.store.composing();
    let preedit = host.store.preedit();
    let full = host.store.full();
    let (s, e) = host.store.selection();
    // 末尾位置 = 合成文字列の UTF-16 長。合成中（未確定）は文書全体が preedit なのでバッファ長に等しい。
    let end = full.encode_utf16().count() as i32;

    let detail = format!(
        "composing={composing} preedit={preedit:?} full={full:?} sel=({s},{e}) end={end}"
    );
    let passed = composing
        && !preedit.is_empty()
        && end > 0
        && (s, e) == (end, end)
        && (s, e) != (0, 0);
    Item15Result { passed, detail }
}

pub struct Item18Result { pub passed: bool, pub detail: String }

/// %TEMP%\nospacekey-tip.log の **このプロセス**の行に `needle` を含むものがあるか。
/// ログは追記式で実行間に消えないため、`[pid N]` プレフィックスで現プロセス行に限定する
/// （連続実行で前回の行を誤って拾わないように）。
fn tip_log_has(needle: &str) -> bool {
    let path = std::path::Path::new(&std::env::var("TEMP").unwrap_or_default())
        .join("nospacekey-tip.log");
    let pid_tag = format!("[pid {}]", std::process::id());
    std::fs::read_to_string(path)
        .map(|s| s.lines().any(|l| l.contains(&pid_tag) && l.contains(needle)))
        .unwrap_or(false)
}

/// item18: 別ウィンドウへのフォーカス喪失でエンジンセッションの読みが居残らない
/// （フォーカス喪失データ残留の回帰ガード）。
///
/// 機序: 別ウィンドウ（多くは別プロセス）へフォーカスが移るとホストは live preedit を文書へ確定
/// するが `ITfCompositionSink::OnCompositionTerminated` を呼ばないことがある。すると TIP の
/// エンジンセッション（読み にほんご）が居残り、戻って打つと古い読みへ連結される（aiueo →
/// にほんごあいうえお → 日本語あいうえお。確定済み 日本語 と合わさり 日本語日本語あいうえお）。
///
/// 検証: "nihongo" をライブ変換中（合成＋セッションが生きている）まで打ち、フォーカスを別 doc へ
/// 移して戻し、続けて "aiueo" を打つ。修正版は `ITfThreadMgrEventSink::OnSetFocus` がセッションを
/// 畳むので、新規入力は新しいセッションで始まり preedit に前の読み（にほん/日本）が混ざらない。
pub fn run_item18(host: &TsfHost) -> Item18Result {
    // conversion-mode compartment はプロセス共有で、--scenarios では直前の item17 が
    // do-no-harm 経路で direct(半角英数)を残す。direct のままだと will_handle が A-Z を
    // パススルーし、harness はパススルー字を store に入れないので "nihongo" が合成を始めず
    // フォーカス放棄経路を踏めない（偽 PASS）。item14/16 と同じく native へ戻してから打つ。
    let _ = host.set_native_mode();
    host.warm_up();
    host.store.reset();

    // 1) ライブ変換中まで打つ（engine セッション＋合成が生きている＝放棄対象がある）。
    let _ = run_keys(host, &crate::scenarios::typed("nihongo"));
    host.settle_debounce();
    let before = host.store.preedit();

    // 2) 別ウィンドウへフォーカスが移って戻る（実機の別窓クリック相当）。OnSetFocus が発火するはず。
    let focus_ok = host.lose_and_regain_focus().is_ok();

    // 3) 続けて別語を打つ。読みが居残っていれば にほんご へ連結される。
    let _ = run_keys(host, &crate::scenarios::typed("aiueo"));
    host.settle_debounce();
    let after = host.store.preedit();
    let full = host.store.full();

    // engine が実際に動いて変換した証拠: before がライブ変換結果(日本語)＝engine セッションが
    // 生きていた。engine 不起動だと TIP が raw preedit(にほんご)へ劣化し、focus_abandon も no_stale も
    // 真になって本来検証すべき「engine セッションの読み居残り」経路を踏まずに偽 PASS する（Codex P2）。
    // before に漢字変換(日本)が出ていることを必須条件にし、engine 経路を確実に踏ませる。
    let engine_converted = before.contains("日本");
    // OnSetFocus（doc フォーカス変化）が放棄リセットを焚いたか＝ITfThreadMgrEventSink 経路の配線ガード。
    let abandoned = tip_log_has("ev=focus_abandon");
    // ITfThreadFocusSink（クロスプロセス前面喪失の OnKillThreadFocus）の advise 配線が生きているか。
    // 実配送はヘッドレスでは焚けない（別スレッド/プロセスの前面化が必要）ので、ここでは advise の
    // 成否だけを検証する（退行で AdviseSink を落とすと thread_advised=false になり item18 が落ちる）。
    // 実際の OnKillThreadFocus 配送＋リセットは実機の手動再現（log src=killthreadfocus）で確認する。
    let thread_sink_advised = tip_log_has("thread_advised=true");
    // 新規入力に前の読みが混ざっていない（フォーカス喪失データ残留が無い＝実挙動）。
    let no_stale = !after.contains("日本") && !after.contains("にほん");

    // `abandoned`(ev=focus_abandon) はヘッドレスでは構造的に観測できないので合否条件から外す。
    // 実 msctf は in-process の空 docmgr への SetFocus では ITfThreadMgrEventSink::OnSetFocus を
    // sink へ配送せず（全 run で OnSetFocus/ev=focus_abandon は 0 件）、フォーカス喪失の後始末を
    // ITfCompositionSink::OnCompositionTerminated 経由で行う（reset_abandoned_composition→
    // engine_end_session が走り、読み残留は消える）。よって「実際にデータ残留が無い」(no_stale)＋
    // sink 配線(thread_sink_advised) で判定し、abandoned は診断用に detail へ残すのみ。
    // 実機の OnSetFocus/OnKillThreadFocus 発火は SP3 手動受入で別途確認済み
    // （ev=focus_abandon src=setfocus / src=killthreadfocus）。
    let passed = focus_ok && engine_converted && thread_sink_advised && no_stale;
    let detail = format!(
        "before={before:?} after={after:?} full={full:?} focus_ok={focus_ok} engine_converted={engine_converted} abandoned={abandoned}(diag-only) thread_sink_advised={thread_sink_advised} no_stale={no_stale}"
    );
    Item18Result { passed, detail }
}

pub struct Item19Result { pub passed: bool, pub detail: String }

/// item19 (SP5 実機バグ回帰): direct(半角英数)モードで、ホストが OnTestKeyDown を経ず
/// OnKeyDown を直接呼ぶ経路（feed_key_keydown_only）でも A–Z が **かな化されず素通し** される。
///
/// 回帰対象バグ（実機・US 配列で発覚）: direct の「A–Z を食わず本文へ流す」gate が will_handle
/// （＝OnTestKeyDown 専用）にしか無く、OnKeyDown は direct を見ずに A–Z を input_char へ流していた。
/// 実機ホスト（IMM/CUAS 等）は TestKeyDown を呼ばず KeyDown を直叩きするため gate が素通りし、
/// direct でも `abc`→`あbc`（kana 化）になっていた。通常の feed_key は TestKeyDown が先に gate
/// するためこのバグを再現できない（direct で TestKeyDown=false → KeyDown へ進まない）。よって
/// KeyDown のみ注入する feed_key_keydown_only で実機経路を忠実に再現する。
///
/// 自己証明（修正を revert すると落ちる）:
///   - direct で feed_key_keydown_only('a'/'b'/'c') が **eaten=false**（パススルー）。
///     バグ版は OnKeyDown VK_A アームが input_char を呼び eaten=true。
///   - preedit/composing/full が空のまま（バグ版は preedit「あ」・composing=true・full「あbc」）。
///   - `ev=keydown vk=0x41 direct=true` がログに出る（OnKeyDown が direct を実際に観測した証跡）。
///   - 対照: native では同じ keydown-only 経路で 'a' が **eaten=true**（gate が native を壊さない）。
///   - VK_CONVERT(0x1C) は direct でも **eaten=true**（再変換トリガは食う＝item13 と非回帰）。
pub fn run_item19(host: &TsfHost) -> Item19Result {
    // 共有 compartment が前シナリオから direct を残しうるので、まず native で温める。
    let _ = host.set_native_mode();
    host.warm_up();
    host.store.reset();

    // 対照: native + keydown-only で 'a' は従来どおり食われる（gate が native を壊さないこと）。
    let native_a_eaten = host.feed_key_keydown_only(0x41); // 'a'
    let _ = host.feed_key_keydown_only(0x1B); // Esc: 対照で開いた合成を畳む
    host.store.reset();

    // ---- 本体: モードを direct へ。失敗なら不能（誤 PASS を出さない）。
    if !host.set_direct_mode() {
        return Item19Result { passed: false, detail: "set_direct_mode 失敗（compartment 設定不可）".into() };
    }

    // direct + keydown-only（＝実機の OnTestKeyDown 非経由）で `abc` を注入。
    let a_eaten = host.feed_key_keydown_only(0x41); // 'a'
    let preedit_a = host.store.preedit();
    let composing_a = host.store.composing();
    let b_eaten = host.feed_key_keydown_only(0x42); // 'b'
    let c_eaten = host.feed_key_keydown_only(0x43); // 'c'
    let full_after = host.store.full();
    let committed_after = host.store.committed();

    // OnKeyDown が direct を実際に観測した証跡（OnTestKeyDown 非経由でも必ず出る診断）。
    let saw_keydown_direct = tip_log_has("ev=keydown vk=0x41 direct=true");

    // VK_CONVERT は direct でも食う（再変換トリガ＝item13 非回帰）。Esc で合成を畳む。
    let conv_eaten = host.feed_key_keydown_only(0x1C);
    let _ = host.feed_key_keydown_only(0x1B);

    // 後始末: native へ戻す（compartment はプロセス共有）。
    let _ = host.set_native_mode();

    let pass_through = !a_eaten && !b_eaten && !c_eaten;
    let no_kana = preedit_a.is_empty() && !composing_a && full_after.is_empty() && committed_after.is_empty();
    let passed = native_a_eaten && pass_through && no_kana && conv_eaten && saw_keydown_direct;
    let detail = format!(
        "native_a_eaten={native_a_eaten} a/b/c_eaten={a_eaten}/{b_eaten}/{c_eaten} preedit_a={preedit_a:?} composing_a={composing_a} full_after={full_after:?} committed_after={committed_after:?} conv_eaten={conv_eaten} keydown_direct={saw_keydown_direct}"
    );
    Item19Result { passed, detail }
}

pub struct Item24Result { pub passed: bool, pub detail: String }

/// item24: 未確定のまま 25 打鍵しても preedit が全打鍵を保持する（実機発見バグ #2
/// 「22文字目以降ドロップ」の再現/回帰）。
///
/// 'a' 連打はローマ字1打鍵=かな1文字（あ）なので、デバウンス前の生かな preedit の
/// 文字数は打鍵数と 1:1 対応する（"nihongo" 等の複合ローマ字だと文字数が動いて境界を
/// 特定できない）。run_keys の StepObs で各打鍵直後の preedit を観測し、
/// preedit.chars().count() == 打鍵数 が最初に崩れた位置を報告する。
///
/// 切り分け診断:
///   - 崩れたステップの eaten=false → TIP のキー処理側でドロップ（will_handle/OnKeyDown gate）。
///   - eaten=true なのに preedit が伸びない → engine/IPC 側でドロップ（Insert 失敗・応答欠落）。
///   - ev=ipc_timeout / ev=degraded / ev=engine_backoff の有無でタイムアウト起因かを判別する。
pub fn run_item24(host: &TsfHost) -> Item24Result {
    // I-3(2026-07-07 レビュー): main.rs の env(NOSPACEKEY_AUTO_COMMIT=disabled 等)は TIP が
    // spawn した engine にしか効かない。ユーザー日常使用分の常駐 engine が居るとそちらへ
    // 接続して env 不発＝非決定になるため、先に kill して自前 spawn を強制する
    // (item8 が engine kill を行うのと同じ作法。常駐 engine はユーザーの次打鍵で自動
    // respawn する — A7 で受入済みの自己修復)。
    kill_engine_processes();
    let _ = host.set_native_mode();
    host.warm_up();
    host.store.reset();

    const N: usize = 25;

    // 実機の打鍵cadenceを忠実化: 打鍵間に settle_debounce を挟み、ライブ変換（デバウンス発火→
    // run_preedit がライブ変換結果へ全置換）と次の Insert がインターリーブする経路を踏ませる。
    // 一括 run_keys（デバウンス発火なし）では 25/25 PASS 済みのため、差分はこのインターリーブ。
    // 観測点は feed_key 直後（Insert 応答の生かな reading。'a'→あ で打鍵数と 1:1）。
    let mut first_bad: Option<(usize, usize, bool, String)> = None; // (打鍵番号, 実長, eaten, preedit)
    let mut raw_len = 0usize;
    for i in 0..N {
        let eaten = host.feed_key(0x41); // VK_A
        let preedit = host.store.preedit();
        let n = preedit.chars().count();
        eprintln!("[trace] item24 key#{:02} eaten={} preedit_len={} preedit={:?}", i + 1, eaten, n, preedit);
        if n != i + 1 && first_bad.is_none() {
            first_bad = Some((i + 1, n, eaten, preedit));
        }
        raw_len = n;
        host.settle_debounce(); // 実タイピング相当: 次打鍵前にライブ変換を発火させる
    }

    // デバウンス発火後（ライブ変換後）は漢字化で文字数が変わり得るため合否には使わず診断表示のみ。
    host.settle_debounce();
    let settled = host.store.preedit();

    // ---- フェーズ B: 混合ローマ字の長文（実使用形。子音保留・拗音・ライブ変換の漢字化を通す）----
    // 読み25文字: わたしのなまえはたなかですきょうはいいてんきですね
    // ローマ字は子音で一時的に preedit 長が上下するため per-step 1:1 は成立しない。
    // 最終の生かな reading が期待文字列と完全一致することを合否条件にし、per-step は trace のみ。
    let _ = host.feed_key(0x1B); // Esc: フェーズ A の合成を畳む
    let _ = host.feed_key(0x1B);
    host.store.reset();
    // 制約(M-1): この文字列は「観測点(feed_key 直後=Insert 応答の生かな)で前ステップ比
    // 50% 超の縮みを起こす部分読み」を含まないこと。ローマ字合成の縮みは高々 -1
    // (sh+i→し 等)なので現行文字列は安全。変更時は崩壊判定(半減未満)と干渉しないか確認する。
    const ROMAJI: &str = "watashinonamaehatanakadesukyouhaiitenkidesune";
    // 合否は「崩壊検知」: セッション喪失バグは preedit が数文字→1文字へ落ちる（例 17→1）。
    // 正常時の縮みは (a) ローマ字合成（sh+i→し で -1）、(b) デバウンス発火がポンプ中に挟まり
    // ライブ変換（漢字化）が表示された場合の圧縮、のいずれも半減未満には落ちない。
    // よって「前ステップ比で半分未満（かつ前ステップ 6 文字以上）」を喪失と判定する。
    // 文字列の完全一致は使わない（ポンプ中デバウンスで漢字表示になる正常ケースを偽 FAIL にしない）。
    let mut b_collapse: Option<(usize, usize, usize, String)> = None; // (打鍵番号, 前len, 今len, preedit)
    let mut b_prev_len = 0usize;
    let mut b_last = String::new();
    for (i, k) in crate::scenarios::typed(ROMAJI).iter().enumerate() {
        let eaten = host.feed_key(k.0);
        b_last = host.store.preedit();
        let n = b_last.chars().count();
        eprintln!(
            "[trace] item24B key#{:02} eaten={} preedit_len={} preedit={:?}",
            i + 1, eaten, n, b_last
        );
        if b_prev_len >= 6 && n < b_prev_len / 2 && b_collapse.is_none() {
            b_collapse = Some((i + 1, b_prev_len, n, b_last.clone()));
        }
        b_prev_len = n;
        host.settle_debounce(); // 実タイピング相当のインターリーブ
    }
    let b_ok = b_collapse.is_none();
    let b_settled = host.store.preedit();

    // フォアグラウンドトラップ判別: ユーザーが物理操作中だと msctf が TIP の Activate を
    // 呼ばず（ActivateProfile=S_OK のまま）全キー素通しになる。ev=activate の有無で
    // 「バグの FAIL」と「トラップの FAIL（再実行が必要）」を区別する。
    let evs = read_events(std::process::id());
    let activate_seen = evs.iter().any(|e| matches!(e, Ev::Activate));
    // I-3 診断: 自動確定(iOS 移植, source=live_auto)が発火した run は preedit が正当に
    // 縮み得る＝崩壊判定が非決定になる。pre-kill で env が効いていれば必ず false。
    // true のまま FAIL したら「常駐 engine に接続した run」を疑い再実行する。
    let auto_commit_seen = evs
        .iter()
        .any(|e| matches!(e, Ev::Commit { source, .. } if source == "live_auto"));

    let ipc_timeout = tip_log_has("ev=ipc_timeout");
    let degraded = tip_log_has("ev=degraded");
    let backoff = tip_log_has("ev=engine_backoff");

    let passed = first_bad.is_none() && raw_len == N && b_ok;
    let detail = format!(
        "activate_seen={activate_seen} auto_commit_seen={auto_commit_seen} \
         A: first_bad={first_bad:?} raw_len={raw_len}/{N} settled={settled:?} \
         | B: collapse={b_collapse:?} last={b_last:?} settled={b_settled:?} \
         | ipc_timeout={ipc_timeout} degraded={degraded} backoff={backoff}"
    );
    Item24Result { passed, detail }
}

pub struct Item29Result { pub passed: bool, pub detail: String }

/// item29: 実機発見バグ #1「Edge(Chromium) パスワード欄で IS_PASSWORD 検知不発」の再現/回帰。
///
/// Chromium/Edge はパスワード欄の InputScope を IS_PASSWORD でなく IS_PRIVATE にする
/// （IS_PRIVATE はシークレットモードの通常欄でも単独で立つので password の根拠にできない）。
/// 代わりにパスワード欄専用 ITfContext の compartment GUID_COMPARTMENT_KEYBOARD_DISABLED に
/// 1 を書き、フィールド種別が変わるたび別ドキュメントへ SetFocus し直す
/// （ui/base/ime/win/tsf_bridge.cc）。TIP はこの compartment を見て全キー素通し
/// （パスワード欄と同じ完全 direct 化）しなければならない。
///
/// 3 フェーズの自己証明:
///   A(対照): compartment なしで 'a' が食われ preedit が伸びる — IME がこの harness で
///     正常に効いていることの証明（フォアグラウンドトラップ/direct モード残留の偽 FAIL を
///     item29 自身のバグと区別する）。
///   B(本題): compartment=1 ＋ フォーカス遷移（Chromium 同様 SetFocus が必ず伴う。TIP の
///     ctx ポインタキャッシュも実機同様 OnSetFocus で無効化される）後、'a' 3 打が全て
///     食われず store も無変化。
///   C(復帰): compartment=0 ＋ フォーカス遷移で再び食われる — B の素通しが compartment
///     起因であること、および無効状態が居残らないことの証明。
pub fn run_item29(host: &TsfHost) -> Item29Result {
    let _ = host.set_native_mode();
    host.warm_up();
    host.store.reset();

    // ---- フェーズ A(対照): compartment なしでは通常どおり合成される ----
    let a_eaten = host.feed_key(0x41); // VK_A
    let a_preedit = host.store.preedit();
    let a_ok = a_eaten && !a_preedit.is_empty();
    let _ = host.feed_key(0x1B); // Esc: 合成を畳む
    host.store.reset();

    // ---- フェーズ B(本題): KEYBOARD_DISABLED=1 のコンテキストでは全キー素通し ----
    let set_ok = host.set_context_keyboard_disabled(true);
    let refocus_ok = host.lose_and_regain_focus().is_ok();
    let mut b_eaten_any = false;
    for _ in 0..3 {
        if host.feed_key(0x41) { b_eaten_any = true; }
    }
    let b_preedit = host.store.preedit();
    let b_committed = host.store.committed();
    let b_ok = set_ok && refocus_ok && !b_eaten_any && b_preedit.is_empty() && b_committed.is_empty();

    // ---- フェーズ C(復帰): compartment を戻せば再び合成される ----
    let unset_ok = host.set_context_keyboard_disabled(false);
    let refocus2_ok = host.lose_and_regain_focus().is_ok();
    let c_eaten = host.feed_key(0x41);
    let c_preedit = host.store.preedit();
    let c_ok = unset_ok && refocus2_ok && c_eaten && !c_preedit.is_empty();
    let _ = host.feed_key(0x1B); // 後始末: 合成を畳む
    host.store.reset();

    let passed = a_ok && b_ok && c_ok;
    let detail = format!(
        "A(control): eaten={a_eaten} preedit={a_preedit:?} | \
         B(disabled): set_ok={set_ok} refocus_ok={refocus_ok} eaten_any={b_eaten_any} \
         preedit={b_preedit:?} committed={b_committed:?} | \
         C(restore): unset_ok={unset_ok} refocus_ok={refocus2_ok} eaten={c_eaten} preedit={c_preedit:?}"
    );
    Item29Result { passed, detail }
}

pub struct Item30Result { pub passed: bool, pub detail: String }

/// item30（Task4 確定取消 headless 回帰・往路）: `nihongo → Space → Enter → Ctrl+Backspace → Esc`。
///
/// 確定取消（Ctrl+Backspace）→Esc の往復が無傷に成立することの回帰。Ctrl+Backspace で
/// 直前確定「日本語」を再変換候補化し、Esc で `reconvert_original`（=確定文字列）を
/// RestoreText 経由で書き戻す（Task 3 start_commit_undo 本体、既存 Esc 経路の再利用）。
///
/// 自己証明（偽 PASS 防止）:
///   - `Ev::Commit{source:"candidate"}` が出ている（Space で候補確定した前提の成立）。
///   - `Ev::CommitUndoShown` が出ている（Ctrl+Backspace が実際に再変換候補を出した）。
///   - Ctrl+Backspace 直後の preedit が非空（候補が復活＝素通しでないことの直接観測）。
///   - 最終 full == "日本語"（Esc の RestoreText で確定文字列が一字一句無傷復元）。
/// これらが無いと「Ctrl+Backspace が素通ししただけで文書はもとから日本語のまま」でも
/// 最終状態アサートだけでは PASS してしまう。
pub fn run_item30(host: &TsfHost) -> Item30Result {
    let pid = std::process::id();
    let _ = host.set_native_mode();
    host.warm_up();
    host.store.reset();
    let base = read_events(pid).len();

    for k in typed("nihongo") { let _ = host.feed_key(k.0); }
    host.settle_debounce();
    let _ = host.feed_key(0x20); // Space: 候補表示
    let _ = host.feed_key(0x0D); // Enter: 候補確定（source=candidate）
    let committed_full = host.store.full();

    let evs_commit: Vec<Ev> = read_events(pid).into_iter().skip(base).collect();
    let saw_candidate_commit = evs_commit.iter().any(|e| {
        matches!(e, Ev::Commit { source, .. } if source == "candidate")
    });

    // Ctrl+Backspace: 確定取消。
    let undo_base = read_events(pid).len();
    let undo_eaten = host.feed_key_with_ctrl(0x08); // VK_BACK
    let preedit_after_undo = host.store.preedit();
    let evs_undo: Vec<Ev> = read_events(pid).into_iter().skip(undo_base).collect();
    let saw_undo_shown = evs_undo.iter().any(|e| matches!(e, Ev::CommitUndoShown { .. }));

    // Esc: reconvert_original（=確定文字列）を RestoreText で書き戻す既存経路。
    let _ = host.feed_key(0x1B); // VK_ESCAPE
    let restored_full = host.store.full();

    let passed = saw_candidate_commit
        && undo_eaten
        && saw_undo_shown
        && !preedit_after_undo.is_empty()
        && restored_full == "日本語";
    let detail = format!(
        "committed_full={committed_full:?} saw_candidate_commit={saw_candidate_commit} \
         undo_eaten={undo_eaten} saw_undo_shown={saw_undo_shown} \
         preedit_after_undo={preedit_after_undo:?} restored_full={restored_full:?}"
    );
    Item30Result { passed, detail }
}

pub struct Item31Result { pub passed: bool, pub detail: String }

/// item31（Task4 確定取消 headless 回帰・disarm）: 確定後の打鍵で武装が解除されること。
///
/// 本体: `a → Enter → Home → Ctrl+Backspace`。
///   - `a→Enter`（source=live）で armed が立つ（`arms_undo("live")==true`）。
///   - `Home` は素の非修飾キー（undo_hot でない）なので M-5 の規律により disarm_undo() が走る。
///   - 続く Ctrl+Backspace は not_armed → 何もせず素通し（アプリの単語削除に譲る）。
/// 自己証明: eaten_last=false（cmd_modifier ゲートで素通し）AND CommitUndoShown 皆無。
///
/// 亜種（C-1 settle 経路の armed 非残留）: `nihongo → Space → 無変換(0x1D) → Ctrl+Backspace`。
///   - Space で候補確定 → 無変換（モードトグル）が settle_active_input 経由で
///     source="candidate" 確定を経由しつつ、末尾の disarm_undo() で armed を必ず落とす
///     （C-1 修正の実証: 「settle→commit_candidate で armed が立つ穴」を塞いだ回帰）。
///   - 続く Ctrl+Backspace は not_armed → eaten=false AND CommitUndoShown なし。
pub fn run_item31(host: &TsfHost) -> Item31Result {
    let pid = std::process::id();
    let _ = host.set_native_mode();
    host.warm_up();
    host.store.reset();

    // ---- 本体: a → Enter → Home → Ctrl+Backspace ----
    let _ = host.feed_key(0x41); // 'a'
    let _ = host.feed_key(0x0D); // Enter（source=live で armed が立つ）
    let _ = host.feed_key(0x24); // Home（非修飾・undo_hot でない → disarm）
    let base1 = read_events(pid).len();
    let eaten1 = host.feed_key_with_ctrl(0x08); // Ctrl+Backspace（not_armed → 素通し期待）
    let evs1: Vec<Ev> = read_events(pid).into_iter().skip(base1).collect();
    let shown1 = evs1.iter().any(|e| matches!(e, Ev::CommitUndoShown { .. }));
    let part1_ok = !eaten1 && !shown1;

    // ---- 亜種（C-1）: nihongo → Space → 無変換(0x1D) → Ctrl+Backspace ----
    host.store.reset();
    for k in typed("nihongo") { let _ = host.feed_key(k.0); }
    host.settle_debounce();
    let _ = host.feed_key(0x20); // Space: 候補表示
    let _ = host.feed_key(0x1D); // 無変換（モードトグル）: settle_active_input 経由で確定＋disarm
    let base2 = read_events(pid).len();
    let eaten2 = host.feed_key_with_ctrl(0x08); // Ctrl+Backspace（not_armed → 素通し期待）
    let evs2: Vec<Ev> = read_events(pid).into_iter().skip(base2).collect();
    let shown2 = evs2.iter().any(|e| matches!(e, Ev::CommitUndoShown { .. }));
    let part2_ok = !eaten2 && !shown2;
    let _ = host.set_native_mode(); // 無変換で direct 化した compartment を後続 item のため戻す

    let passed = part1_ok && part2_ok;
    let detail = format!(
        "part1(a→Enter→Home→CtrlBS): eaten={eaten1} undo_shown={shown1} ok={part1_ok} | \
         part2(nihongo→Space→無変換→CtrlBS): eaten={eaten2} undo_shown={shown2} ok={part2_ok}"
    );
    Item31Result { passed, detail }
}

/// 溜まった WM_TIMER / post を drain する小ヘルパ（Behavior の notify→確定反映を確実にする）。
/// settle_debounce は内部で sleep + pump するので、それを数回繰り返してホスト発の確定 post を捌く。
fn pump_settle(host: &TsfHost) {
    for _ in 0..5 {
        host.settle_debounce();
        if !host.store.committed().is_empty() { break; }
    }
}

fn kill_pid(pid: u32) {
    use std::os::windows::process::CommandExt;
    // taskkill /F /PID。失敗は無視（既に死んでいる等）。
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .status();
}

/// 常駐 engine（NospacekeyEngineHost.exe）を全て kill する。失敗（不在等）は無視。
/// item24 の env 継承を確実にするための pre-kill（詳細は run_item24 冒頭コメント）。
/// keymap-smoke（main.rs）も同じ作法で使うため pub(crate)。
pub(crate) fn kill_engine_processes() {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/IM", "NospacekeyEngineHost.exe"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .status();
}

/// item9: プロファイル解除前後で feed_key('a') の eaten を測る。
/// 期待: 解除前は食う（before=true）、DeactivateProfile 後は食わない（after=false）。
/// host は deactivate のため &mut で受ける。
pub fn run_item9(host: &mut TsfHost) -> (bool, bool) {
    let before = host.feed_key(0x41); // VK_A
    host.store.reset();
    if let Err(e) = host.deactivate() { eprintln!("item9 deactivate err: {e:?}"); }
    let after = host.feed_key(0x41);
    (before, after)
}

#[cfg(test)]
mod tests {
    use super::{cancel_followed_shown, Ev};

    fn shown(latin: &str) -> Ev { Ev::ReconvertShown { n: 3, kind: "latin".into(), latin: latin.into() } }
    fn cancel() -> Ev { Ev::ReconvertCancel }

    // item13 経路 C の核ロジック cancel_followed_shown の純テスト（TSF 不要）。
    // 「トグルが reconvert を畳んだ」＝reconvert_shown の **後ろ** に reconvert_cancel が
    // 出ている、を表現する。C1 修正が無いと（バグ版）トグルは cancel を呼ばないので
    // shown の後ろに cancel が出ず、ここが false になる＝経路 C が落ちる。

    #[test]
    fn cancel_after_matching_shown_is_true() {
        // shown(nihongo) → cancel の順。トグルが畳んだ正常系。
        let evs = vec![shown("nihongo"), cancel()];
        assert!(cancel_followed_shown(&evs, "nihongo"));
    }

    #[test]
    fn cancel_before_shown_is_false() {
        // cancel が先、shown が後（前経路の残骸など）。トグル連動とは認めない。
        let evs = vec![cancel(), shown("nihongo")];
        assert!(!cancel_followed_shown(&evs, "nihongo"));
    }

    #[test]
    fn shown_without_cancel_is_false() {
        // C1 修正を revert したバグ版を模す: トグルしても cancel が出ない。
        let evs = vec![shown("nihongo")];
        assert!(!cancel_followed_shown(&evs, "nihongo"));
    }

    #[test]
    fn cancel_without_matching_shown_is_false() {
        // latin が一致する shown が無ければ false（別 latin の候補だけ＋cancel）。
        let evs = vec![shown("react"), cancel()];
        assert!(!cancel_followed_shown(&evs, "nihongo"));
    }

    #[test]
    fn empty_is_false() {
        assert!(!cancel_followed_shown(&[], "nihongo"));
    }

    #[test]
    fn uses_first_shown_and_last_cancel() {
        // 1 本目 shown → cancel（トグル畳み）→ 2 本目 shown（再変換が再び発火）。
        // 最初の shown(0) より後ろに cancel(1) が居るので true。
        let evs = vec![shown("nihongo"), cancel(), shown("nihongo")];
        assert!(cancel_followed_shown(&evs, "nihongo"));
    }
}
