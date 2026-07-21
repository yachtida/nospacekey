//! %TEMP%\nospacekey-tip.log を自 PID で絞り、ev= 行を構造化する。

#[derive(Debug, Clone)]
pub enum Ev {
    Activate,
    CandidatesShown { n: usize, #[allow(dead_code)] sel: usize, list: Vec<String> }, // 診断用: sel= を保持（現状未読）
    CandidateMove { sel: usize },
    CandidatesHidden,
    Commit { text: String, source: String },
    EngineSpawn { pid: u32, ok: bool },
    Degraded { #[allow(dead_code)] reason: String }, // 診断用: 劣化理由を保持（現状未読）
    /// 外部LLM変換要求が出た（Tab→start_llm_convert）。seq は世代。
    LlmRequest { #[allow(dead_code)] seq: u64 }, // 診断用: 世代 seq を保持（現状未読）
    /// 外部LLM結果が preedit へ適用された（on_llm_outcome の成功枝）。
    LlmApplied { #[allow(dead_code)] seq: u64 }, // 診断用: 世代 seq を保持（現状未読）
    /// 修正変換（Tab→trigger_typo_convert）が読みのタイポ修復候補を候補窓に出した。
    /// n=候補数, sel=選択位置（常に 0 で開始）, list=候補列（先頭が修復第一候補）。
    /// item41 は list のみ判定に使うため n/sel は診断用に保持（現状未読）。
    TypoCandidatesShown {
        #[allow(dead_code)] n: usize,
        #[allow(dead_code)] sel: usize,
        list: Vec<String>,
    },
    /// 読みモニタの表示状態遷移（action = show|update|hide|destroy）。item42 が読む。
    ReadingMonitor { action: String },
    /// SP5 再変換が候補を出した。n=候補数, kind=latin|surface（経路）, latin=対象になった元文字列。
    ReconvertShown { n: usize, kind: String, latin: String },
    /// SP5 再変換が取消され元文字列を復元した（Esc→cancel_reconvert）。
    ReconvertCancel,
    /// SP5 再変換の対象が再変換不能（漢字/混在等）で何もしなかった（do-no-harm, SP5 step-6）。
    ReconvertSkip { reason: String },
    /// U9: composition 開始時に捕捉した左文脈の長さ（内容はログに出さない — len のみ）。
    LeftContext { len: usize },
    /// 確定取消（Ctrl+Backspace）が候補を出した。n=候補数, rlen=読み長, tlen=確定文字列長
    /// （本文はログに出さない — I-3）。item30 は存在チェック（`matches!(.., CommitUndoShown{..})`）
    /// のみ使うので、フィールドは診断用に保持する（現状未読）。
    CommitUndoShown {
        #[allow(dead_code)] n: usize,
        #[allow(dead_code)] rlen: usize,
        #[allow(dead_code)] tlen: usize,
    },
    /// 確定取消が何もしなかった（do-no-harm）。reason=not_armed|composition_open|no_buffer|
    /// too_long|text_mismatch。too_long のみ tlen= が付く（他 reason は None）。診断用に保持
    /// する（現状未読、単体テストでの reason 検証は除く）。
    CommitUndoSkip {
        #[allow(dead_code)] reason: String,
        #[allow(dead_code)] tlen: Option<usize>,
    },
    /// ephemeral かな開始（enter_ephemeral_kana）。フィールド無し（存在チェックのみ）。
    EphemeralEnter,
    /// ephemeral かな→direct 復帰（exit_ephemeral_to_direct）。
    EphemeralExit,
    /// モードトグル（無変換キー／toggle_conversion_mode）。`direct`=トグル後の実際値
    /// （is_direct(next)）。item37以降: ephemeral 中トグルは settle が先に exit_ephemeral_to_direct
    /// で compartment を direct へ戻すため、この行の direct はトグル**前**が direct であることを
    /// 前提に反転した値になる＝ephemeral 起点のトグルは direct=false（かなへ昇格）で観測できる。
    ModeToggle { direct: bool },
    /// 読みの表記変換が発火した（apply_notation）。vk=実際に押されたキー（--keymap-smoke が
    /// 「リマップ先だけ発火/元キーは解放済み」を vk 値で区別するのに使う）。
    Notation { vk: u32 },
}

fn kv<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    // "ev=commit text=日本語 source=candidate" から key= の値を取る（次の " " まで、
    // ただし最後のキーは行末まで）。list= は '|' を含むので最後に置く前提。
    let pat = format!("{key}=");
    let i = body.find(&pat)? + pat.len();
    let rest = &body[i..];
    Some(rest.split(' ').next().unwrap_or(rest))
}

/// 空白を含みうる値（commit の text=）を取る。値は次の既知キー " <k>=" の
/// 直前まで（無ければ行末まで）とし、前後の空白を除去して返す。
/// follow には text= の後ろに来うるキー名を渡す（commit 行では ["source"]）。
/// 前提: 確定テキストはリテラル " source=" を含まない（含めば早期に切れる）。
fn kv_spanned<'a>(body: &'a str, key: &str, follow: &[&str]) -> Option<&'a str> {
    let pat = format!("{key}=");
    let i = body.find(&pat)? + pat.len();
    let rest = &body[i..];
    // 後続キーのうち " <k>=" が最も早く現れる位置を終端とする。
    let end = follow
        .iter()
        .filter_map(|k| rest.find(&format!(" {k}=")))
        .min()
        .unwrap_or(rest.len());
    Some(rest[..end].trim())
}

/// `s` の中で最初に現れる ` <ident>=` 境界（空白＋ASCII 識別子 [A-Za-z_][A-Za-z0-9_]* ＋ '='）の
/// 空白位置を返す。ログのフィールドは ` key=value` 形式なので、ここで切れば末尾フィールドを
/// 値へ吸収しない。バイト走査だが ' '/'='/ASCII 英数字は UTF-8 多バイト列に現れないため、
/// 返す index は常に文字境界（空白）で str スライス可能。
fn next_field_boundary(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b' ' {
            let mut j = i + 1;
            if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') { j += 1; }
                if j < b.len() && b[j] == b'=' {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// `list=` の値（'|' 区切り候補列）を取り出す。`list=` は行末までの規約だが、将来その後ろに
/// ` key=value` フィールドが付いても最終候補へ吸収しないよう、次の ` <ident>=` 境界で切る。
/// 注意: 候補が稀に ` 英字=` の並びを含むと誤って切れる（現実の IME 候補では非現実的）。
fn list_value(body: &str) -> Option<&str> {
    let i = body.find("list=")? + "list=".len();
    let rest = &body[i..];
    let end = next_field_boundary(rest).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// 1 行から ev を取り出す（自 PID 前提で呼ぶ）。
fn parse_one(body: &str) -> Option<Ev> {
    let body = body.trim();
    // ev=activate は SP6b/SP7 で末尾フィールド (live_conversion=…/default_direct=…) が付く。
    // 他イベント同様に接頭辞許容にする（exact 一致だと activate を取りこぼし item1 が FAIL する）。
    // 末尾の空白ガードで ev=activateX 等の誤マッチを防ぐ。
    if body == "ev=activate" || body.starts_with("ev=activate ") { return Some(Ev::Activate); }
    if body == "ev=candidates_hidden" || body.starts_with("ev=candidates_hidden ") { return Some(Ev::CandidatesHidden); }
    if body.starts_with("ev=candidates_shown") {
        let n = kv(body, "n").and_then(|s| s.parse().ok()).unwrap_or(0);
        let sel = kv(body, "sel").and_then(|s| s.parse().ok()).unwrap_or(0);
        // list= は '|' 区切りの候補列。末尾フィールドが将来付いても吸収しないよう境界で切る（L-8a）。
        let list = list_value(body)
            .map(|v| v.split('|').map(|s| s.trim().to_string()).collect())
            .unwrap_or_default();
        return Some(Ev::CandidatesShown { n, sel, list });
    }
    if body.starts_with("ev=candidate_move") {
        let sel = kv(body, "sel").and_then(|s| s.parse().ok()).unwrap_or(0);
        return Some(Ev::CandidateMove { sel });
    }
    // Task4 I-4: ev=commit_undo_shown / ev=commit_undo_skip は接頭辞 "ev=commit" と衝突するため
    // 下の汎用 ev=commit 分岐より前に判定する（:128-129 の reconvert_cancel/skip 前例と同じ理由。
    // 先に判定しないと ev=commit_undo_* が素の Commit{text:"", source:""} として誤って食われる）。
    if body.starts_with("ev=commit_undo_shown") {
        let n = kv(body, "n").and_then(|s| s.parse().ok()).unwrap_or(0);
        let rlen = kv(body, "rlen").and_then(|s| s.parse().ok()).unwrap_or(0);
        let tlen = kv(body, "tlen").and_then(|s| s.parse().ok()).unwrap_or(0);
        return Some(Ev::CommitUndoShown { n, rlen, tlen });
    }
    if body.starts_with("ev=commit_undo_skip") {
        let reason = kv(body, "reason").unwrap_or("").to_string();
        // too_long のみ tlen= が付く（text_service.rs :1729）。他 reason は None。
        let tlen = kv(body, "tlen").and_then(|s| s.parse().ok());
        return Some(Ev::CommitUndoSkip { reason, tlen });
    }
    if body.starts_with("ev=commit") {
        // text= は空白を含みうる（複数語の確定文）。次の既知キー " source=" まで取る。
        let text = kv_spanned(body, "text", &["source"]).unwrap_or("").to_string();
        let source = kv(body, "source").unwrap_or("").to_string();
        return Some(Ev::Commit { text, source });
    }
    if body.starts_with("ev=engine_spawn") {
        let pid = kv(body, "pid").and_then(|s| s.parse().ok()).unwrap_or(0);
        let ok = kv(body, "ok").map(|s| s == "true").unwrap_or(false);
        return Some(Ev::EngineSpawn { pid, ok });
    }
    if body.starts_with("ev=degraded") {
        let reason = kv(body, "reason").unwrap_or("").to_string();
        return Some(Ev::Degraded { reason });
    }
    if body.starts_with("ev=llm_request") {
        let seq = kv(body, "seq").and_then(|s| s.parse().ok()).unwrap_or(0);
        return Some(Ev::LlmRequest { seq });
    }
    if body.starts_with("ev=llm_applied") {
        let seq = kv(body, "seq").and_then(|s| s.parse().ok()).unwrap_or(0);
        return Some(Ev::LlmApplied { seq });
    }
    // ev=typo_candidates_shown は接頭辞 "ev=typo" が他分岐と衝突しないため単独判定でよい。
    // list= の扱いは ev=candidates_shown と同一（'|' 区切り、次の既知境界で切る）。
    if body.starts_with("ev=typo_candidates_shown") {
        let n = kv(body, "n").and_then(|s| s.parse().ok()).unwrap_or(0);
        let sel = kv(body, "sel").and_then(|s| s.parse().ok()).unwrap_or(0);
        let list = list_value(body)
            .map(|v| v.split('|').map(|s| s.trim().to_string()).collect())
            .unwrap_or_default();
        return Some(Ev::TypoCandidatesShown { n, sel, list });
    }
    if body.starts_with("ev=reading_monitor") {
        let action = kv(body, "action").unwrap_or("").to_string();
        return Some(Ev::ReadingMonitor { action });
    }
    // SP5: ev=reconvert_cancel / ev=reconvert_skip は接頭辞 ev=reconvert_shown と区別するため先に判定する。
    if body == "ev=reconvert_cancel" || body.starts_with("ev=reconvert_cancel ") { return Some(Ev::ReconvertCancel); }
    if body.starts_with("ev=reconvert_skip") {
        let reason = kv(body, "reason").unwrap_or("").to_string();
        return Some(Ev::ReconvertSkip { reason });
    }
    if body.starts_with("ev=reconvert_shown") {
        let n = kv(body, "n").and_then(|s| s.parse().ok()).unwrap_or(0);
        let kind = kv(body, "kind").unwrap_or("latin").to_string();
        let latin = kv(body, "latin").unwrap_or("").to_string();
        return Some(Ev::ReconvertShown { n, kind, latin });
    }
    if body.starts_with("ev=left_context") {
        let len = kv(body, "len").and_then(|s| s.parse().ok()).unwrap_or(0);
        return Some(Ev::LeftContext { len });
    }
    if body == "ev=ephemeral_enter" || body.starts_with("ev=ephemeral_enter ") { return Some(Ev::EphemeralEnter); }
    if body == "ev=ephemeral_exit" || body.starts_with("ev=ephemeral_exit ") { return Some(Ev::EphemeralExit); }
    if body.starts_with("ev=notation") {
        // vk= は "0x7a" 形式（key_event_sink.rs の {vk:#04x}）。
        let vk = kv(body, "vk")
            .and_then(|s| s.strip_prefix("0x"))
            .and_then(|s| u32::from_str_radix(s, 16).ok())
            .unwrap_or(0);
        return Some(Ev::Notation { vk });
    }
    if body.starts_with("ev=mode_toggle") {
        // skip=repeat / skip=no_compartment は direct= を持たない実トグル不発（早期 return）
        // なので、ModeToggle{direct:false} に化けさせず None へ落とす。
        if let Some(s) = kv(body, "direct") {
            return Some(Ev::ModeToggle { direct: s == "true" });
        }
    }
    None
}

/// pid prefix 除去後の行頭 `ts=<digits> ` を読み飛ばす（品質ループ①: TIP は
/// `[pid N] ts=<epoch_ms> ev=...` 形式で書く）。ts= が無い旧形式・harness 行はそのまま返す。
fn strip_ts_prefix(body: &str) -> &str {
    if let Some(rest) = body.strip_prefix("ts=") {
        let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digits > 0 {
            if let Some(after) = rest[digits..].strip_prefix(' ') {
                return after;
            }
        }
    }
    body
}

/// `[pid N] ts=<epoch_ms> ev=...` の行群から、自 PID の ev だけを順に返す。
pub fn parse_lines(lines: &[String], pid: u32) -> Vec<Ev> {
    let prefix = format!("[pid {pid}] ");
    lines.iter().filter_map(|l| {
        let body = l.strip_prefix(&prefix)?;
        parse_one(strip_ts_prefix(body))
    }).collect()
}

/// %TEMP%\nospacekey-tip.log を読み、自 PID の ev を返す。
pub fn read_events(pid: u32) -> Vec<Ev> {
    let dir = match std::env::var_os("TEMP") { Some(d) => d, None => return Vec::new() };
    let path = std::path::Path::new(&dir).join("nospacekey-tip.log");
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    parse_lines(&lines, pid)
}

#[cfg(test)]
mod tests {
    use super::{parse_lines, Ev};

    #[test]
    fn parses_own_pid_events() {
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ev=activate"),
            format!("[pid {pid}] ev=candidates_shown n=3 sel=0 list=日本語|二本語|二本"),
            format!("[pid {}] ev=activate", pid + 1), // 他 PID は無視
            format!("[pid {pid}] ev=commit text=日本語 source=candidate"),
            format!("[pid {pid}] reconnected to foo"), // ev= 以外は無視
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 3);
        assert!(matches!(&evs[0], Ev::Activate));
        match &evs[1] {
            Ev::CandidatesShown { n, list, .. } => {
                assert_eq!(*n, 3);
                assert!(list.contains(&"日本語".to_string()));
            }
            _ => panic!("expected CandidatesShown"),
        }
        match &evs[2] {
            Ev::Commit { text, source } => { assert_eq!(text, "日本語"); assert_eq!(source, "candidate"); }
            _ => panic!("expected Commit"),
        }
    }

    #[test]
    fn parses_reading_monitor_events() {
        // 読みモニタの show/hide が構造化される（item42 の判定材料）。
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ev=reading_monitor action=show len=4"),
            format!("[pid {pid}] ev=reading_monitor action=hide"),
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 2, "evs={evs:?}");
        match &evs[0] {
            Ev::ReadingMonitor { action } => assert_eq!(action, "show"),
            other => panic!("show が解析できない: {other:?}"),
        }
        match &evs[1] {
            Ev::ReadingMonitor { action } => assert_eq!(action, "hide"),
            other => panic!("hide が解析できない: {other:?}"),
        }
    }

    #[test]
    fn parses_events_with_ts_field_after_pid_prefix() {
        // 品質ループ①の新形式: [pid N] ts=1720281600123 ev=commit text=日本語 source=live
        // ts= トークンは読み飛ばし、既存の ev= パースが従来どおり成立する。
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ts=1720281600123 ev=commit text=日本語 source=live"),
            format!("[pid {pid}] ts=1720281600124 ev=activate live_conversion=true"),
            // ev=log_open は既知 Ev に無い＝黙って読み飛ばす（既存 item を壊さない）。
            format!("[pid {pid}] ts=1720281600122 ev=log_open build=0.0.0-abc1234"),
            // ts= 無しの旧形式も引き続き読める（後方互換）。
            format!("[pid {pid}] ev=candidates_hidden"),
            // ts= の値が数字でない行は strip しない（誤検知しない — ev= 行でないので None）。
            format!("[pid {pid}] ts=abc ev=commit text=x source=live"),
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 3, "evs={evs:?}");
        match &evs[0] {
            Ev::Commit { text, source } => { assert_eq!(text, "日本語"); assert_eq!(source, "live"); }
            _ => panic!("expected Commit"),
        }
        assert!(matches!(&evs[1], Ev::Activate));
        assert!(matches!(&evs[2], Ev::CandidatesHidden));
    }

    #[test]
    fn parses_reconvert_events() {
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ev=reconvert_shown n=3 latin=nihongo"),
            format!("[pid {pid}] ev=reconvert_cancel"),
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 2);
        match &evs[0] {
            Ev::ReconvertShown { n, latin, .. } => { assert_eq!(*n, 3); assert_eq!(latin, "nihongo"); }
            _ => panic!("expected ReconvertShown"),
        }
        // 接頭辞衝突（reconvert_cancel が reconvert_shown へ誤マッチしない）。
        assert!(matches!(&evs[1], Ev::ReconvertCancel));
    }

    #[test]
    fn commit_text_with_spaces_is_not_truncated() {
        // 複数語の確定文（空白含む）が next-known-key まで正しく取れること（M-2 回帰ガード）。
        // text= の値は " source=" の直前まで。"React" だけに切り詰めない。
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ev=commit text=React nihongo source=candidate"),
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            Ev::Commit { text, source } => {
                assert_eq!(text, "React nihongo");
                assert_eq!(source, "candidate");
            }
            _ => panic!("expected Commit"),
        }
    }

    #[test]
    fn commit_tolerates_structured_fields() {
        // 品質ループ②: source= の後ろに sel/cand_n/rlen/tlen/mode（＋部分確定の remaining）が
        // 付いても text/source の抽出は従来どおり（text は " source=" 直前まで、source は次の
        // 空白まで）。既存 item のゲート前提を壊さない回帰ガード。
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ts=1720281600123 ev=commit text=React 日本語 source=candidate sel=2 cand_n=9 rlen=4 tlen=3 mode=native"),
            format!("[pid {pid}] ev=commit text=日本 source=candidate_prefix remaining=ご sel=0 cand_n=5 rlen=4 tlen=2 mode=native"),
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 2);
        match &evs[0] {
            Ev::Commit { text, source } => {
                assert_eq!(text, "React 日本語");
                assert_eq!(source, "candidate");
            }
            _ => panic!("expected Commit"),
        }
        match &evs[1] {
            Ev::Commit { text, source } => {
                assert_eq!(text, "日本");
                assert_eq!(source, "candidate_prefix");
            }
            _ => panic!("expected Commit"),
        }
    }

    #[test]
    fn activate_tolerates_trailing_fields() {
        // SP6b/SP7 以降、TIP は ev=activate に live_conversion=…/default_direct=… を付ける。
        // 末尾フィールド付きでも Activate として取れること（item1 回帰ガード）。
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ev=activate live_conversion=true default_direct=false"),
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], Ev::Activate));
    }

    #[test]
    fn parses_reconvert_kind_and_skip() {
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ev=reconvert_shown n=5 kind=surface latin=にほんご"),
            format!("[pid {pid}] ev=reconvert_skip reason=non_kana"),
            format!("[pid {pid}] ev=reconvert_shown n=3 latin=nihongo"), // kind 無し → latin 既定
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 3);
        match &evs[0] {
            Ev::ReconvertShown { n, kind, latin } => {
                assert_eq!(*n, 5); assert_eq!(kind, "surface"); assert_eq!(latin, "にほんご");
            }
            _ => panic!("expected ReconvertShown"),
        }
        match &evs[1] {
            Ev::ReconvertSkip { reason } => assert_eq!(reason, "non_kana"),
            _ => panic!("expected ReconvertSkip"),
        }
        match &evs[2] {
            Ev::ReconvertShown { kind, latin, .. } => { assert_eq!(kind, "latin"); assert_eq!(latin, "nihongo"); }
            _ => panic!("expected ReconvertShown"),
        }
    }

    #[test]
    fn list_tolerates_trailing_future_field() {
        // list= の後ろに将来フィールドが付いても最終候補へ吸収しない（L-8a 回帰ガード）。
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ev=candidates_shown n=3 sel=0 list=日本語|二本語|二本 future=x"),
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            Ev::CandidatesShown { n, list, .. } => {
                assert_eq!(*n, 3);
                assert_eq!(
                    list,
                    &vec!["日本語".to_string(), "二本語".to_string(), "二本".to_string()]
                );
            }
            _ => panic!("expected CandidatesShown"),
        }
    }

    #[test]
    fn parses_left_context_len() {
        // U9: ev=left_context len=N を Ev::LeftContext { len: N } として取れること。
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ev=left_context len=12"),
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            Ev::LeftContext { len } => assert_eq!(*len, 12),
            _ => panic!("expected LeftContext"),
        }
    }

    #[test]
    fn list_preserves_candidates_with_spaces() {
        // 候補が空白を含んでも（` 英字=` でなければ）切らずに保持する。
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ev=candidates_shown n=2 sel=0 list=React 日本語|hello world"),
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            Ev::CandidatesShown { list, .. } => {
                assert_eq!(list, &vec!["React 日本語".to_string(), "hello world".to_string()]);
            }
            _ => panic!("expected CandidatesShown"),
        }
    }

    #[test]
    fn parses_commit_undo_shown_and_skip() {
        // Task4: ev=commit_undo_shown / ev=commit_undo_skip（text_service.rs start_commit_undo）。
        // I-4 回帰: "ev=commit_undo_shown ..." は接頭辞が "ev=commit" と衝突するので、
        // 汎用 ev=commit 分岐に先取りされて Commit{text:"", source:""} に化けないこと
        // （下の CommitUndoShown/CommitUndoSkip の型一致で自動的に検証される）。
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ev=commit_undo_shown n=3 rlen=5 tlen=3"),
            format!("[pid {pid}] ev=commit_undo_skip reason=not_armed"),
            format!("[pid {pid}] ev=commit_undo_skip reason=too_long tlen=41"),
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 3, "evs={evs:?}");
        match &evs[0] {
            Ev::CommitUndoShown { n, rlen, tlen } => {
                assert_eq!(*n, 3);
                assert_eq!(*rlen, 5);
                assert_eq!(*tlen, 3);
            }
            _ => panic!("expected CommitUndoShown"),
        }
        match &evs[1] {
            Ev::CommitUndoSkip { reason, tlen } => {
                assert_eq!(reason, "not_armed");
                assert_eq!(*tlen, None);
            }
            _ => panic!("expected CommitUndoSkip"),
        }
        match &evs[2] {
            Ev::CommitUndoSkip { reason, tlen } => {
                assert_eq!(reason, "too_long");
                assert_eq!(*tlen, Some(41));
            }
            _ => panic!("expected CommitUndoSkip"),
        }
    }

    #[test]
    fn parses_ephemeral_events() {
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ev=ephemeral_enter set_ok=true next=0x0001"),
            format!("[pid {pid}] ev=ephemeral_exit set_ok=true next=0x0000"),
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[0], Ev::EphemeralEnter));
        assert!(matches!(&evs[1], Ev::EphemeralExit));
    }

    #[test]
    fn parses_typo_candidates_shown() {
        // item41: ev=typo_candidates_shown を Ev::TypoCandidatesShown として取れること
        // （修復候補の先頭がしてください、であることを述語で検証するための前提パース）。
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ev=typo_candidates_shown n=3 sel=0 list=してください|して下さい|してく獺祭"),
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 1, "evs={evs:?}");
        match &evs[0] {
            Ev::TypoCandidatesShown { n, list, .. } => {
                assert_eq!(*n, 3);
                assert_eq!(
                    list,
                    &vec!["してください".to_string(), "して下さい".to_string(), "してく獺祭".to_string()]
                );
            }
            _ => panic!("expected TypoCandidatesShown"),
        }
    }

    #[test]
    fn parses_notation_vk() {
        // --keymap-smoke: ev=notation vk=0x7a text=… を Ev::Notation{vk} として取れること
        // （リマップ先キーの発火/元キーの解放済みを vk 値で区別する前提）。
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ev=notation vk=0x7a text=ニホンゴ"),
            format!("[pid {pid}] ev=notation vk=0x76 text=にほんご"),
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 2, "evs={evs:?}");
        assert!(matches!(&evs[0], Ev::Notation { vk } if *vk == 0x7a));
        assert!(matches!(&evs[1], Ev::Notation { vk } if *vk == 0x76));
    }

    #[test]
    // item38: トグル後の実際値（direct=）を読み取れることを確認する（text_service.rs の
    // ev=mode_toggle direct={} ... の書式と一致させる）。
    fn parses_mode_toggle_direct_value() {
        let pid = std::process::id();
        let lines = vec![
            format!("[pid {pid}] ev=mode_toggle direct=false set_ok=true before=0x0000 next=0x0001 after=0x0001 tid=1"),
            format!("[pid {pid}] ev=mode_toggle direct=true set_ok=true before=0x0001 next=0x0000 after=0x0000 tid=1"),
        ];
        let evs = parse_lines(&lines, pid);
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[0], Ev::ModeToggle { direct: false }));
        assert!(matches!(&evs[1], Ev::ModeToggle { direct: true }));
        // I-1 バンドル: skip=repeat/no_compartment は direct= を持たない不発トグルなので、
        // ModeToggle{direct:false} に誤変換されず None（=evs に現れない）であること。
        let skip_lines = vec![format!("[pid {pid}] ev=mode_toggle skip=repeat")];
        assert!(parse_lines(&skip_lines, pid).is_empty());
    }
}
