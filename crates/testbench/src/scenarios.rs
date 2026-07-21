//! ヘッドレス・シナリオの Rust データ構造（テキスト DSL は YAGNI）。

/// 1 ステップ＝注入する VK（と人間可読ラベル）。
#[derive(Clone, Copy)]
pub struct Vk(pub u32, pub &'static str);

// VK 定数（key_event_sink.rs と同値）。
pub const BACK: Vk = Vk(0x08, "Backspace");
pub const TAB: Vk = Vk(0x09, "Tab");
pub const ENTER: Vk = Vk(0x0D, "Enter");
pub const ESC: Vk = Vk(0x1B, "Esc");
pub const SPACE: Vk = Vk(0x20, "Space");
pub const UP: Vk = Vk(0x26, "Up");
pub const DOWN: Vk = Vk(0x28, "Down");
pub const NONCONVERT: Vk = Vk(0x1D, "Muhenkan"); // モードトグル（OnPreservedKey 経由）
pub const CONVERT: Vk = Vk(0x1C, "Convert"); // 再変換/henkan（Task4: 対象なし→ephemeral フォールバック）
pub const HOME: Vk = Vk(0x24, "Home");
pub const OEM_PERIOD: Vk = Vk(0xBE, "OemPeriod"); // '.'（打鍵作法 Task3: idle 全角直接確定）
pub const OEM_MINUS: Vk = Vk(0xBD, "OemMinus"); // '-'（かな合成中 → 長音符 ー）
pub const OEM_SLASH: Vk = Vk(0xBF, "Oem2Slash"); // '/'（かな合成中 → 全角中点 ・）
pub const F7: Vk = Vk(0x76, "F7"); // 表記変換: カタカナ（打鍵作法 Task4）
pub const F8: Vk = Vk(0x77, "F8"); // ephemeral かなトリガ（既定）
pub const F11: Vk = Vk(0x7A, "F11"); // keymap リマップ検証用（--keymap-smoke: to_katakana の付け替え先）
pub fn ch(c: char) -> Vk { Vk(0x41 + (c as u32 - 'a' as u32), "Char") } // a..z
/// 数字キー VK（メイン行 0-9）。`d` は 0..=9。
pub fn digit(d: u32) -> Vk { Vk(0x30 + d, "Digit") }

/// "nihongo" を VK 列に展開する。
pub fn typed(s: &str) -> Vec<Vk> { s.chars().map(ch).collect() }

use crate::log_parse::Ev;

/// シナリオ実行後の判定述語: (committed, full, preedit, 観測 ev 列, 最終キーの eaten) → Ok/Err。
pub type Expect = fn(committed: &str, full: &str, preedit: &str, evs: &[Ev], eaten_last: bool) -> Result<(), String>;

/// 各シナリオの観測源と期待。判定は driver::judge が行う。
pub struct Scenario {
    pub item: u32,
    pub name: &'static str,
    pub keys: Vec<Vk>,
    /// 実行後に満たすべき述語（store 終端状態＋観測した ev 列を受ける）。
    pub expect: Expect,
}

fn has_activate(evs: &[Ev]) -> bool { evs.iter().any(|e| matches!(e, Ev::Activate)) }
fn candidates_contains(evs: &[Ev], want: &str) -> bool {
    evs.iter().any(|e| matches!(e, Ev::CandidatesShown { list, .. } if list.iter().any(|x| x == want)))
}
fn any_candidate_move(evs: &[Ev]) -> bool { evs.iter().any(|e| matches!(e, Ev::CandidateMove { .. })) }
fn has_candidates_shown(evs: &[Ev]) -> bool { evs.iter().any(|e| matches!(e, Ev::CandidatesShown { .. })) }

/// 9 シナリオ。item8/9 は Stage 3 で expect を厳密化する。
pub fn all() -> Vec<Scenario> {
    vec![
        Scenario { item: 1, name: "activate", keys: vec![],
            expect: |_c, _f, _p, evs, _e| if has_activate(evs) { Ok(()) } else { Err("ev=activate 未受信".into()) } },
        // SP3: 毎打鍵ライブ変換のため preedit は変換後の漢字かな交じり文になる
        // （旧 SP1/2 の素のかな "にほんご" ではない）。"nihongo" のライブ変換結果 "日本語" を期待する。
        Scenario { item: 2, name: "romaji->live-converted preedit", keys: typed("nihongo"),
            expect: |_c, _f, p, _e, _l| if p == "日本語" { Ok(()) } else { Err(format!("preedit={p:?} != 日本語（ライブ変換結果）")) } },
        Scenario { item: 3, name: "space shows 日本語", keys: { let mut k = typed("nihongo"); k.push(SPACE); k },
            expect: |_c, _f, _p, evs, _l| if candidates_contains(evs, "日本語") { Ok(()) } else { Err("候補に 日本語 が無い".into()) } },
        Scenario { item: 4, name: "candidate move", keys: { let mut k = typed("nihongo"); k.push(SPACE); k.push(SPACE); k },
            expect: |_c, _f, _p, evs, _l| if any_candidate_move(evs) { Ok(()) } else { Err("ev=candidate_move 未受信".into()) } },
        Scenario { item: 5, name: "commit kanji", keys: { let mut k = typed("nihongo"); k.push(SPACE); k.push(ENTER); k },
            expect: |c, _f, _p, _e, _l| if c == "日本語" { Ok(()) } else { Err(format!("committed={c:?} != 日本語")) } },
        Scenario { item: 6, name: "esc cancels", keys: { let mut k = typed("nihongo"); k.push(SPACE); k.push(ESC); k.push(ESC); k },
            // 自己証明: SPACE で候補が出た（ev=candidates_shown）ことを確認した上で ESC×2 後に文書が空、を要求する。
            // これで「TIP が打鍵を素通ししただけ（実は何も処理していない）でも full 空＝PASS」になる偽 PASS を防ぐ。
            expect: |_c, f, _p, evs, _l| {
                if !has_candidates_shown(evs) { return Err("ev=candidates_shown 未受信（候補が出ておらず Esc 取消の前提が崩れる）".into()); }
                if f.is_empty() { Ok(()) } else { Err(format!("full={f:?} != 空（Esc で取消されていない）")) }
            } },
        // SP3: backspace で読みを1つ削り、デバウンスでライブ再変換する。"nihongo"（読み にほんご→
        // ライブ変換 日本語）から BACK で読みが にほん になり、そのライブ変換結果は "日本"（日本＝にほん）。
        // settle（デバウンス発火）後に観測する。旧 SP1/2 の素のかな "にほん" は SP3 では成立しない。
        Scenario { item: 7, name: "backspace shrinks (live re-convert)", keys: { let mut k = typed("nihongo"); k.push(BACK); k },
            // 本 item の本質: BACK で読みが にほんご→にほん に縮み、ライブ再変換が走って にほん の
            // 漢字表記になること。top-1 の具体漢字は model 依存（classic では 2本/二本、Zenzai では 日本）
            // なので固定しない。自己証明: (a) BACK 前のライブ "日本語" のままでない（縮んだ）、
            // (b) 語/ご を含まない（ご が確かに削れた）、(c) 本 を含む（にほん→…本… へ変換された）。
            expect: |_c, _f, p, _e, _l| {
                if p == "日本語" { return Err(format!("preedit={p:?} が BACK 前のライブ結果のまま（読みが縮んでいない）")); }
                if p.contains('語') || p.contains('ご') { return Err(format!("preedit={p:?} に 語/ご が残る（BACK で ご が削れていない）")); }
                if !p.contains('本') { return Err(format!("preedit={p:?} が にほん の変換結果(…本…)でない")); }
                Ok(())
            } },
        // item8/9 は Stage 3 で厳密化（ここは骨格・常に Ok で false-green を避けるため Stage 3 まで除外実行）。
        Scenario { item: 8, name: "engine kill resilience", keys: vec![],
            expect: |_c, _f, _p, _e, _l| Err("Stage 3 未実装".into()) },
        Scenario { item: 9, name: "deactivate returns to normal", keys: vec![],
            expect: |_c, _f, _p, _e, l| if !l { Ok(()) } else { Err("eaten_last=true（解除後も食っている）".into()) } },
        // item10: 上下矢印で候補選択が動くこと（ユーザ報告のバグ回帰）。
        // 自己証明: 候補が出た（candidates_shown）うえで ↓ が選択を 0→1 に動かし
        // （ev=candidate_move sel=1）、最後の ↑ をちゃんと食う（eaten_last=true）。
        // ↓/↑ が素通しされていた旧実装では sel=1 の candidate_move が出ず FAIL する。
        Scenario { item: 10, name: "arrow keys move candidate selection",
            keys: { let mut k = typed("nihongo"); k.push(SPACE); k.push(DOWN); k.push(UP); k },
            expect: |_c, _f, _p, evs, eaten_last| {
                // 前提: 候補が 2 件以上出ていること。1 件だと move_selection が循環して
                // ↓ でも sel=0 のままになり、下の sel=1 アサートが「矢印が壊れている」と
                // 誤報する偽 FAIL になる。候補数 n>=2 を先に自己証明しておく（item6 と同じ作法）。
                if !evs.iter().any(|e| matches!(e, Ev::CandidatesShown { n, .. } if *n >= 2)) {
                    return Err("候補が 2 件以上出ていない（↓ で sel 0→1 に動ける前提が崩れる）".into());
                }
                if !evs.iter().any(|e| matches!(e, Ev::CandidateMove { sel: 1 })) {
                    return Err("↓ で選択が 0→1 に動いていない（ev=candidate_move sel=1 が無い）".into());
                }
                if !eaten_last { return Err("最後の ↑ が食われていない（eaten_last=false＝素通し）".into()); }
                Ok(())
            } },
        // item11: 確定後にキャレットが確定文字列の末尾へ来ること（実機で発覚したカーソルバグの回帰）。
        // 「a→Enter（あ確定）, i→Enter（い確定）」と 2 語を続けて確定し、2 語目が 1 語目の後ろに
        // 入る（committed=="あい"）ことを要求する。確定時に SetSelection で末尾へキャレットを動かさ
        // ない旧実装では、2 語目が打ち始めの先頭に挿入され committed=="いあ"（逆順）になって FAIL する。
        // harness は SetText でキャレットを動かさない実 TSF 準拠モデル（doc_state/text_store の忠実化と
        // 対）なので、この差を検出できる。
        // 自己証明: SP3 では Space を押さず Enter するとライブ変換結果が確定する（source=live）。
        // その確定が「あ」「い」の順で 2 回出ていることを先に確認し、TIP が打鍵を素通ししただけの
        // 偽 PASS を防ぐ（旧 SP1/2 の source=reading から SP3 で source=live に変わった）。
        Scenario { item: 11, name: "caret after commit lands at end (two words stay ordered)",
            keys: vec![ch('a'), ENTER, ch('i'), ENTER],
            expect: |committed, _f, _p, evs, _l| {
                let commits: Vec<&str> = evs.iter().filter_map(|e| match e {
                    Ev::Commit { text, source } if source == "live" => Some(text.as_str()),
                    _ => None,
                }).collect();
                if commits.len() != 2 {
                    return Err(format!("ライブ確定が 2 回出ていない（commits={commits:?}）— 2 語確定の前提が崩れる"));
                }
                // 'a' は model 有無に依らず あ。1 語目が あ であることを自己証明（素通し検出）。
                if commits[0] != "あ" {
                    return Err(format!("1 語目のライブ確定が あ でない（commits={commits:?}）— 素通しの疑い"));
                }
                // 本 item の本質は確定後キャレットが末尾に来て 2 語が「順序通り」連結されること
                // （逆順 いあ になる旧バグの回帰）。2 語目の漢字は model 依存（classic では い→居）なので
                // 固定せず、committed == commits[0]+commits[1]（feed 順の連結）で順序を検証する。
                // キャレット不変条件そのものは doc_state の単体テストで別途担保。
                let expected = format!("{}{}", commits[0], commits[1]);
                if committed != expected {
                    return Err(format!("committed={committed:?} != {expected:?}（確定後キャレットが末尾に無く 2 語が順序通り連結されていない）"));
                }
                Ok(())
            } },
        // item20: 合成中にモードトグル（無変換 0x1D）→ 開いていた合成が確定されて畳まれる（UU-3 回帰）。
        // 旧実装ではモードだけ切り替わり composition が孤立（direct 側で Enter/Esc/BS が素通しになり
        // preedit を閉じる手段がなくなる）。conversion-mode compartment は同一の実 ITfThreadMgr 上＝
        // プロセス共有なので、シナリオ毎に新しい TsfHost を作っても direct のまま残り他 item へ波及する。
        // 復元はランナー側（main.rs）が全シナリオ後に無条件 host.set_native_mode() で行う。
        // 自己証明: (a) ev=commit source=mode_toggle が出る（settle 経路が走った）、
        // (b) preedit が空（composition が畳まれた）、(c) committed=="日本語"
        // （nihongo のライブ変換確定。item2 が同じ値を preedit で固定済みなので model 差異は無い）。
        Scenario { item: 20, name: "mode toggle mid-composition commits preedit",
            keys: { let mut k = typed("nihongo"); k.push(NONCONVERT); k },
            expect: |c, _f, p, evs, _l| {
                if !evs.iter().any(|e| matches!(e, Ev::Commit { source, .. } if source == "mode_toggle")) {
                    return Err("ev=commit source=mode_toggle が出ていない（トグル前の settle が走っていない）".into());
                }
                if !p.is_empty() {
                    return Err(format!("preedit={p:?} != 空（composition が畳まれていない）"));
                }
                if c != "日本語" {
                    return Err(format!("committed={c:?} != 日本語（ライブ変換結果が確定されていない）"));
                }
                Ok(())
            } },
        // item21: 合成中に Home を押す → 開いていた合成が確定されて畳まれる（UU-6 回帰）。
        // 旧実装では Home が match の catch-all に落ちて素通し（Ok(FALSE)）→ アプリのキャレット
        // だけ移動し preedit が別位置に取り残される。修正後は will_handle が composition 中の
        // Home を食い、settle で確定して畳む。
        // 自己証明: (a) Home をちゃんと食う（eaten_last=true。旧実装は素通しで false）、
        // (b) ev=commit source=navigate が出る（settle 経路が走った）、
        // (c) preedit が空（composition が畳まれた）、(d) committed=="日本語"
        // （nihongo のライブ変換確定。item2/item20 が同値を固定済みで model 差異は無い）。
        Scenario { item: 21, name: "home mid-composition commits preedit",
            keys: { let mut k = typed("nihongo"); k.push(HOME); k },
            expect: |c, _f, p, evs, eaten_last| {
                if !eaten_last {
                    return Err("最後の Home が食われていない（eaten_last=false＝素通しで preedit 取り残し）".into());
                }
                if !evs.iter().any(|e| matches!(e, Ev::Commit { source, .. } if source == "navigate")) {
                    return Err("ev=commit source=navigate が出ていない（Home 前の settle が走っていない）".into());
                }
                if !p.is_empty() {
                    return Err(format!("preedit={p:?} != 空（composition が畳まれていない）"));
                }
                if c != "日本語" {
                    return Err(format!("committed={c:?} != 日本語（ライブ変換結果が確定されていない）"));
                }
                Ok(())
            } },
        // item22: ライブ確定が engine Commit(0) 経由になっても（Spec2 学習合流）、多かな 2 語の
        // 連続ライブ確定が壊れない（セッション desync・確定文字列の欠落が無い）ことの配線回帰。
        // 前方一致候補で部分確定が走った場合は source=live_prefix が出るので、live と live_prefix の
        // 両方を集めて「確定文字列の連結 == committed」を検証する（どちらの経路でも合計は不変）。
        Scenario { item: 22, name: "consecutive live enters stay ordered via engine commit (Spec2)",
            keys: { let mut k = typed("kyou"); k.push(ENTER); k.extend(typed("ha")); k.push(ENTER); k },
            expect: |committed, _f, _p, evs, _eaten_last| {
                let commits: Vec<&str> = evs.iter().filter_map(|e| match e {
                    Ev::Commit { text, source } if source == "live" || source == "live_prefix" =>
                        Some(text.as_str()),
                    _ => None,
                }).collect();
                if commits.is_empty() {
                    return Err("ライブ確定（source=live/live_prefix）が 1 回も出ていない".into());
                }
                // 注意: 「preedit 空」は assert しない（I-2）。最終 Enter の top-1 が前方一致候補だと
                // live_prefix 部分確定で composition（残り読み）が正しく残る — それはバグではなく
                // 部分確定継続の仕様。モデル依存で発火が読めないため、本質の不変条件
                // 「確定文字列の連結 == committed」だけを検証する（ランナーはシナリオ毎に
                // 新しい TsfHost を作るので、preedit が残ったまま終えても他 item へ波及しない）。
                let expected: String = commits.concat();
                if committed != expected {
                    return Err(format!(
                        "committed={committed:?} != ライブ確定の連結 {expected:?}（Commit(0) 経路で desync/欠落の疑い）"));
                }
                Ok(())
            } },
        // item23: U9 左文脈注入の配線回帰。「にほんご」を確定（Space→Enter）した後に「たべる」を
        // 入力して Space（変換要求）まで進めると、2 回目の composition 開始時に捕捉される左文脈は
        // 直前の確定文字列「日本語」が文書に残っているため非空になる（1 回目は文書が空なので
        // ev=left_context len=0）。自己証明: (a) Commit イベントが最低 1 回出ている（確定の前提）、
        // (b) その Commit より**後**に len>0 の ev=left_context が出ている（前文書の確定を跨いで
        // 左文脈が正しく再捕捉されている＝stale 残留でも欠落でもない）。
        Scenario { item: 23, name: "left context captured after a prior commit (U9 wiring regression)",
            keys: {
                let mut k = typed("nihongo");
                k.push(SPACE);
                k.push(ENTER);
                k.extend(typed("taberu"));
                k.push(SPACE);
                k
            },
            expect: |_c, _f, _p, evs, _eaten_last| {
                let first_commit = evs.iter().position(|e| matches!(e, Ev::Commit { .. }));
                let Some(first_commit) = first_commit else {
                    return Err("ev=commit が 1 回も出ていない（左文脈テストの前提となる確定が無い）".into());
                };
                let post_commit_nonempty_left_context = evs.iter().enumerate().any(|(i, e)| {
                    i > first_commit && matches!(e, Ev::LeftContext { len } if *len > 0)
                });
                if !post_commit_nonempty_left_context {
                    return Err(
                        "確定後に len>0 の ev=left_context が出ていない（前文書確定後の左文脈再捕捉が壊れている）"
                            .into(),
                    );
                }
                Ok(())
            } },
        // item25: 打鍵作法 Task3 — idle（composition なし）で OEM_PERIOD を打つと全角句点「。」が
        // 直接確定される（native モード）。composition を張らない do_commit の composition 無し枝
        // （InsertTextAtSelection＋末尾 SetSelection — レビュー M-3）で 1 発挿入する経路の回帰。
        // **2 連打**で committed=="。。" を要求する: 挿入後にキャレットが末尾へ追従しないと
        // 2 打目が 1 打目の**前**へ入り "。。" にならない（連打順序＝キャレット後置の検証）。
        // 自己証明: (a) 最後の OEM_PERIOD をちゃんと食う（旧実装は idle 素通しで eaten=false）、
        // (b) ev=commit source=idle_symbol が 2 回出る（新経路が 2 打とも走った）、
        // (c) committed=="。。"（順序保証込み）。
        Scenario { item: 25, name: "idle oem-period commits fullwidth kuten directly (twice, ordered)",
            keys: vec![OEM_PERIOD, OEM_PERIOD],
            expect: |c, _f, p, evs, eaten_last| {
                if !eaten_last {
                    return Err("OEM_PERIOD が食われていない（eaten_last=false＝idle 素通しの旧挙動）".into());
                }
                let n = evs.iter().filter(|e| matches!(e, Ev::Commit { text, source }
                    if text == "。" && source == "idle_symbol")).count();
                if n != 2 {
                    return Err(format!("ev=commit text=。 source=idle_symbol が 2 回出ていない（n={n}）"));
                }
                if !p.is_empty() {
                    return Err(format!("preedit={p:?} != 空（idle 直接確定で composition を張らないはず）"));
                }
                if c != "。。" {
                    return Err(format!("committed={c:?} != 。。（2 打目がキャレット末尾に入っていない）"));
                }
                Ok(())
            } },
        // item26: 打鍵作法 Task4 — F7 で読みがカタカナ表記へ置換され、デバウンス（ライブ変換）に
        // 上書きされない（disarm_debounce の回帰）。run_scenario は打鍵後に settle_debounce を
        // 挟むので、disarm が漏れているとライブ変換が「日本語」へ上書きして FAIL する。
        Scenario { item: 26, name: "f7 converts reading to katakana (survives debounce)",
            keys: { let mut k = typed("nihongo"); k.push(F7); k },
            expect: |_c, _f, p, _e, eaten_last| {
                if !eaten_last {
                    return Err("F7 が食われていない（eaten_last=false＝素通し）".into());
                }
                if p != "ニホンゴ" {
                    return Err(format!("preedit={p:?} != ニホンゴ（F7 表記変換が効いていない/ライブ変換に上書きされた）"));
                }
                Ok(())
            } },
        // item27: 打鍵作法 Task4 — F7 の後の Enter は表示中のカタカナをそのまま確定する
        // （notation_fixed ラッチ: engine のライブ変換結果 日本語 で上書き確定しない）。
        Scenario { item: 27, name: "enter after f7 commits katakana as shown",
            keys: { let mut k = typed("nihongo"); k.push(F7); k.push(ENTER); k },
            expect: |c, _f, p, evs, _l| {
                if !evs.iter().any(|e| matches!(e, Ev::Commit { text, source }
                    if text == "ニホンゴ" && source == "live")) {
                    return Err("ev=commit text=ニホンゴ source=live が出ていない（engine live 結果で上書きされた疑い）".into());
                }
                if !p.is_empty() {
                    return Err(format!("preedit={p:?} != 空（composition が畳まれていない）"));
                }
                if c != "ニホンゴ" {
                    return Err(format!("committed={c:?} != ニホンゴ"));
                }
                Ok(())
            } },
        // item28: レビュー I-1 — 候補ウィンドウ表示中の F7 は窓を**閉じて**表記変換する。
        // 閉じないと直後の Enter が showing 枝で stale 候補（変換時の「日本語」等）を
        // commit_candidate し、画面表示（ニホンゴ）と違う文字列が確定する。
        // 自己証明: (a) Space で候補が出た（candidates_shown — 窓が開いた前提の成立）、
        // (b) committed=="ニホンゴ"（stale 候補でなく表示中の表記が確定）、
        // (c) ev=commit source=live（candidate 経路でない）、(d) preedit 空。
        Scenario { item: 28, name: "f7 while candidates shown closes window; enter commits katakana",
            keys: { let mut k = typed("nihongo"); k.push(SPACE); k.push(F7); k.push(ENTER); k },
            expect: |c, _f, p, evs, _l| {
                if !has_candidates_shown(evs) {
                    return Err("ev=candidates_shown 未受信（候補窓が開いておらず I-1 の前提が崩れる）".into());
                }
                if !evs.iter().any(|e| matches!(e, Ev::Commit { text, source }
                    if text == "ニホンゴ" && source == "live")) {
                    return Err("ev=commit text=ニホンゴ source=live が出ていない（stale 候補の candidate 確定の疑い）".into());
                }
                if evs.iter().any(|e| matches!(e, Ev::Commit { source, .. }
                    if source == "candidate" || source == "candidate_prefix")) {
                    return Err("ev=commit source=candidate(_prefix) が出ている（F7 が候補窓を閉じていない）".into());
                }
                if !p.is_empty() {
                    return Err(format!("preedit={p:?} != 空（composition が畳まれていない）"));
                }
                if c != "ニホンゴ" {
                    return Err(format!("committed={c:?} != ニホンゴ（画面表示と違う文字列が確定）"));
                }
                Ok(())
            } },
        // item32: 伸ばし棒 — かな合成中の `-` が長音符 `ー` になり読み/変換結果に残る（Task1）。
        // "ko-hi-" → こーひー → ライブ変換 コーヒー（いずれも ー を含む）。旧実装は半角 `-` のまま FAIL。
        Scenario { item: 32, name: "prolonged sound mark ー mid-word",
            keys: vec![ch('k'), ch('o'), OEM_MINUS, ch('h'), ch('i'), OEM_MINUS],
            expect: |_c, _f, p, _e, _l| if p.contains('ー') { Ok(()) } else { Err(format!("preedit={p:?} に ー が無い（伸ばし棒が半角のまま）")) } },
        // item33: 数字 composition — かなモード idle の数字が composition を開始する（Task6）。
        // preedit が非空＝読みに入っている（旧実装は素通しで preedit 空）。digit(0)=0x30 は
        // is_text_vk アーム、digit(1..3)=VK_1..9 アーム — 両経路の idle 数字を1本で駆動する。
        Scenario { item: 33, name: "native idle digits start composition",
            keys: vec![digit(0), digit(1), digit(2), digit(3)],
            expect: |_c, _f, p, _e, _l| if !p.is_empty() { Ok(()) } else { Err(format!("preedit={p:?} 空（数字が composition に入っていない）")) } },
        // item34: ephemeral 開始→かな入力→Enter 確定→direct へ自動復帰（Task3: 復帰配線の回帰）。
        Scenario { item: 34, name: "ephemeral enter, type, commit returns to direct",
            keys: { let mut k = vec![F8]; k.extend(typed("nihongo")); k.push(ENTER); k },
            expect: |_c, _f, p, evs, _l| {
                // 確定後: preedit 空・ev に EphemeralEnter と EphemeralExit の両方が出る。
                let entered = evs.iter().any(|e| matches!(e, Ev::EphemeralEnter));
                let exited = evs.iter().any(|e| matches!(e, Ev::EphemeralExit));
                if entered && exited && p.is_empty() { Ok(()) }
                else { Err(format!("entered={entered} exited={exited} preedit={p:?}")) }
            } },
        // item35: ephemeral 中 Esc で composition 破棄→direct へ復帰（Task3: 復帰配線の回帰）。
        Scenario { item: 35, name: "ephemeral esc discards and returns to direct",
            keys: { let mut k = vec![F8]; k.extend(typed("nihongo")); k.push(ESC); k },
            expect: |_c, f, _p, evs, _l| {
                let exited = evs.iter().any(|e| matches!(e, Ev::EphemeralExit));
                if exited && f.is_empty() { Ok(()) }
                else { Err(format!("exited={exited} full={f:?}（Esc 破棄＋direct 復帰が不成立）")) }
            } },
        // item36: direct idle で VK_CONVERT（再変換対象なし。headless doc は空）→ ephemeral 開始へ
        // フォールバック（Task4）。実機 Terminal では読み戻し失敗で同じく reconverting=false になる。
        Scenario { item: 36, name: "convert with no reconvert target falls back to ephemeral",
            keys: vec![CONVERT],
            expect: |_c, _f, _p, evs, _l| {
                if evs.iter().any(|e| matches!(e, Ev::EphemeralEnter)) { Ok(()) }
                else { Err("no EphemeralEnter after empty convert".into()) }
            } },
        // item37: Task8(a/空素通し) — ephemeral 開始直後、何も打たず Enter だけ押すと
        // ephemeral_idle_abort が idle（composing=false, showing=false）で「かなモードが素通しする
        // キー」判定を通し（native idle の Enter は will_handle が false）、Enter を消費する**前**に
        // direct へ復帰する。Enter 自体は素通し（eaten_last=false — will_handle_gated も
        // ephemeral_hot/undo_hot 無しでは同じ false を返す）ので、Enter 由来の副作用（改行等）は
        // 出さずアプリへそのまま渡る。
        // 自己証明: (a) EphemeralEnter/EphemeralExit の両方が出る（開始→idle-abort 経由の復帰が
        // 実際に走った）、(b) preedit 空（何も打っていないので当然）、(c) committed 空
        // （ephemeral 側が何かを確定していない＝素通しのみ）、(d) eaten_last=false（Enter は食われず
        // アプリへ渡る）。
        Scenario { item: 37, name: "ephemeral empty then enter passes through and returns to direct",
            keys: vec![F8, ENTER],
            expect: |c, _f, p, evs, eaten_last| {
                let entered = evs.iter().any(|e| matches!(e, Ev::EphemeralEnter));
                let exited = evs.iter().any(|e| matches!(e, Ev::EphemeralExit));
                if !entered { return Err("ev=ephemeral_enter が出ていない（F8 が開始トリガとして食われていない）".into()); }
                if !exited { return Err("ev=ephemeral_exit が出ていない（idle の Enter で direct へ復帰していない）".into()); }
                if !p.is_empty() { return Err(format!("preedit={p:?} != 空")); }
                if !c.is_empty() { return Err(format!("committed={c:?} != 空（何も確定していないはず）")); }
                if eaten_last { return Err("最後の Enter が食われている（eaten_last=true＝素通しでない）".into()); }
                Ok(())
            } },
        // item38: Task8(b/トグル昇格) — ephemeral 中にモードトグル（無変換）を押すと「ephemeral かなの
        // 打ちかけ入力」が settle_before_mode_toggle → commit_and_reset で確定・畳まれ、その
        // commit_and_reset が（トグル分岐が ephemeral_kana フラグを落とすより前に）
        // exit_ephemeral_to_direct を呼んで compartment を direct へ戻す（ev=ephemeral_exit が出る —
        // 「EphemeralExit が出ない」という素朴な assert は誤り）。続けて toggle_conversion_mode が
        // その時点の compartment（direct）を読んでトグルするため、**最終的に compartment は
        // native（かな）へ反転**し、ephemeral_kana フラグは false のまま＝「一時的ではない永続かな」
        // に昇格する。この昇格の観測可能な唯一の真実は ev=mode_toggle の direct= 値（トグル後の
        // 実際値）であって EphemeralExit の有無ではない。
        // 自己証明: (a) EphemeralEnter が出る（F8 開始）、(b) EphemeralExit も出る（settle 経由の
        // 復帰が実際に走った証拠。無ければ ephemeral_kana が消費されないまま==別の壊れ方）、
        // (c) 最後の ModeToggle イベントの direct が false（トグル後に native＝かなへ着地）、
        // (d) committed が非空（トグル前の settle でライブ確定が実際に走った）。確定文字列の
        // 具体値は「ni」のライブ変換 top-1 が model 依存（item2/7 と同じ注意）なので固定しない
        // （本 item の本質は昇格＝mode_toggle の着地であって確定文字列の内容ではない）。
        Scenario { item: 38, name: "toggle mid-ephemeral promotes to persistent kana",
            keys: { let mut k = vec![F8]; k.extend(typed("ni")); k.push(NONCONVERT); k },
            expect: |c, _f, _p, evs, _l| {
                let entered = evs.iter().any(|e| matches!(e, Ev::EphemeralEnter));
                let exited = evs.iter().any(|e| matches!(e, Ev::EphemeralExit));
                if !entered { return Err("ev=ephemeral_enter が出ていない".into()); }
                if !exited {
                    return Err("ev=ephemeral_exit が出ていない（settle_before_mode_toggle 経由の commit_and_reset が走っていない）".into());
                }
                let last_toggle_direct = evs.iter().rev().find_map(|e| match e {
                    Ev::ModeToggle { direct } => Some(*direct),
                    _ => None,
                });
                match last_toggle_direct {
                    Some(false) => {}
                    Some(true) => return Err("ev=mode_toggle direct=true（トグル後も direct のまま＝かなへ昇格していない）".into()),
                    None => return Err("ev=mode_toggle が出ていない".into()),
                }
                if c.is_empty() {
                    return Err("committed が空（トグル前の settle でライブ確定されていない）".into());
                }
                Ok(())
            } },
        // item39: 変換中の句読点 — かな合成中の `.` が全角句点「。」になり読み/変換に残る
        // （`-` と同仕様＝設計 §C）。"ni" → に（合成中）→ `.` で「。」を読みへ畳み込む。旧実装は
        // 半角 `.` のまま読みへ入り FAIL。auto-commit で確定側に出ることもあるので
        // preedit ∪ committed のどちらかに 。 があれば合格。
        Scenario { item: 39, name: "fullwidth kuten folds into reading mid-composition",
            keys: { let mut k = typed("ni"); k.push(OEM_PERIOD); k },
            expect: |c, _f, p, _e, eaten_last| {
                if !eaten_last {
                    return Err("OEM_PERIOD が食われていない（合成中 素通しの旧挙動）".into());
                }
                if !(p.contains('。') || c.contains('。')) {
                    return Err(format!("preedit={p:?} committed={c:?} に 。 が無い（合成中の句読点が半角のまま）"));
                }
                Ok(())
            } },
        // item40: 記号トグル既定 OFF — かな合成中の `/` は半角のまま読みへ畳み込まれる
        //（2026-07-16 spec で既定を全角→半角へ変更。設定注入機構が testbench に無いため
        // ON 側の全角化(/→・)は unit で担保し、実機受入で補完する）。
        Scenario { item: 40, name: "symbol stays halfwidth mid-composition by default",
            keys: { let mut k = typed("ni"); k.push(OEM_SLASH); k },
            expect: |c, _f, p, _e, eaten_last| {
                if !eaten_last {
                    return Err("OEM_SLASH が食われていない（合成中 素通し）".into());
                }
                if p.contains('・') || c.contains('・') {
                    return Err(format!("preedit={p:?} committed={c:?} に ・ がある（既定 OFF なのに全角化）"));
                }
                if !(p.contains('/') || c.contains('/')) {
                    return Err(format!("preedit={p:?} committed={c:?} に / が無い（打鍵が消えた）"));
                }
                Ok(())
            } },
        // item41: 修正変換(Tab) — 打ち間違い読み(s 2連打)を Tab 一発で修復し確定する。
        // "shitekudassai"(読み してくだっさい。literal 変換は「してく獺祭」等に崩壊する実測済みケース)
        // → Tab で ev=typo_candidates_shown(修復ブロック先頭=してください)→ Enter で確定。
        // 自己証明: typo_candidates_shown の list 先頭が してください であること(候補が出ずに
        // Enter がライブ確定しただけの偽 PASS を防ぐ)+ committed==してください。
        Scenario { item: 41, name: "tab typo-convert repairs double-s and commits",
            keys: { let mut k = typed("shitekudassai"); k.push(TAB); k.push(ENTER); k },
            expect: |c, _f, _p, evs, _l| {
                let shown = evs.iter().find_map(|e| match e {
                    Ev::TypoCandidatesShown { list, .. } => Some(list),
                    _ => None,
                });
                let Some(list) = shown else { return Err("ev=typo_candidates_shown 未受信(Tab が修正変換を起動していない)".into()) };
                if list.first().map(String::as_str) != Some("してください") {
                    return Err(format!("修復候補の先頭が してください でない: {list:?}"));
                }
                if c != "してください" { return Err(format!("committed={c:?} != してください")); }
                Ok(())
            } },
        // item42: 読みモニタ（spec 2026-07-21）。ライブ変換中("nihongo"→preedit=日本語)に
        // ev=reading_monitor action=show が出て、Enter 確定後に action=hide が出ること。
        // 自己証明: (a) show が確定(ev=commit)より前に出る（合成中に表示された）、
        // (b) 確定後に hide が出る（窓が残留しない）。show/hide の実描画有無ではなく
        // ログ契約を検証する（ヘッドレスの限界 — 実描画・位置・排他は実機受入）。
        Scenario { item: 42, name: "reading monitor shows during live conversion, hides on commit",
            keys: { let mut k = typed("nihongo"); k.push(ENTER); k },
            expect: |_c, _f, _p, evs, _l| {
                let commit_at = evs.iter().position(|e| matches!(e, Ev::Commit { .. }));
                let show_at = evs.iter().position(
                    |e| matches!(e, Ev::ReadingMonitor { action } if action == "show"));
                let hide_after_commit = match commit_at {
                    Some(ci) => evs.iter().skip(ci).any(
                        |e| matches!(e, Ev::ReadingMonitor { action } if action == "hide")),
                    None => false,
                };
                match (show_at, commit_at) {
                    (None, _) => Err("ev=reading_monitor action=show が出ていない（ライブ変換中に読みモニタが表示されていない）".into()),
                    (_, None) => Err("ev=commit が出ていない（Enter 確定の前提が崩れる）".into()),
                    (Some(s), Some(c)) if s > c => Err("show が確定より後（合成中に表示されていない）".into()),
                    _ if !hide_after_commit => Err("確定後に ev=reading_monitor action=hide が出ていない（窓が残留）".into()),
                    _ => Ok(()),
                }
            } },
    ]
}
