//! COM非依存の入力状態機械。ここをTDDする。
/// 入力フェーズ。Composing=通常ライブ / AwaitingLlm=LLM変換中（入力ロック）。
#[derive(Default, Debug, PartialEq)]
pub enum Phase { #[default] Composing, AwaitingLlm }

#[derive(Default, Debug, PartialEq)]
pub struct InputState {
    pub raw: String,          // 打鍵で貯めたローマ字（エンジンに送る生入力）
    pub composing: bool,      // composition中か
    pub live_seq: u64,        // ライブ変換要求のシーケンス番号（A2 の古い応答破棄用）
    pub llm_seq: u64,
    pub phase: Phase,
    /// 打鍵作法 Task4: F6-F10 で表記を固定したか。true の間、Enter/settle は engine の
    /// ライブ変換結果を参照せず表示中の live_text を直確定する（F7 のカタカナが engine の
    /// 漢字結果で上書き確定されるのを防ぐ）。新たな打鍵/Backspace/確定/取消で解除。
    pub notation_fixed: bool,
    /// Shift英語モード(shift_latin=compose): raw 内で direct 挿入部分が始まるバイト位置。
    /// Some=英語モード中。bool でなく位置を持つのはセッション喪失リプレイ(split_replay)が
    /// かな部/英語部の style 分割に必要なため。合成終息(reset/reseed/raw 枯渇)で None。
    pub latin_from: Option<usize>,
    /// 最後にエンジン応答(または TIP 確定的表示)で表示を更新したときの文字列。
    /// 劣化(エンジン応答 None)時のフォールバック素材。空 = まだ一度も成功していない。
    pub last_good_text: String,
    /// ↑を記録した時点の raw のバイト長。劣化時は raw[この位置..] を追記分として扱う。
    pub last_good_raw_len: usize,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    StartOrUpdatePreedit(String), // preeditに表示すべき文字列
    #[allow(dead_code)] // テスト専用: prod は OnKeyDown 内に同等処理をインライン化（テストモデル）
    RequestConvert,               // 候補要求（Space）
    #[allow(dead_code)] // テスト専用: prod は OnKeyDown 内に同等処理をインライン化（テストモデル）
    Commit,                       // 確定（Enter）
    Cancel,                       // 取消（Esc）
    #[allow(dead_code)] // テスト専用: prod は start_llm_convert を直接呼ぶ（テストモデル）
    RequestLlmConvert,            // 外部LLM変換要求（Tab）
    Pass,                         // IMEは関与しない
}

impl InputState {
    pub fn on_char(&mut self, ch: char) -> Action {
        self.raw.push(ch);
        self.composing = true;
        self.notation_fixed = false; // 新たな打鍵でライブ変換が再開する＝表記固定は解除
        Action::StartOrUpdatePreedit(self.raw.clone())
    }
    /// Shift英語モードの打鍵。最初の1打でモードを立て(raw の現在長=英語部分の開始位置)、
    /// 以降の蓄積は on_char と同一(打鍵でライブ変換再開も同じ)。
    pub fn on_char_latin(&mut self, ch: char) -> Action {
        if self.latin_from.is_none() {
            self.latin_from = Some(self.raw.len());
        }
        self.on_char(ch)
    }
    /// Shift英語モード中か。`composing` を AND するのは不変条件「latin_mode ⇒ composing」を
    /// 構造的に保証するため — この不変条件には eaten 整合（will_handle_gated は latin_mode を
    /// 知らないが composing 中は必ず「食う」と宣言する）と symbol_keydown の InsertStyle::Kana
    /// 固定が依存しており、将来 composing=false を書く経路が latin_from クリアを忘れても
    /// 破れないようにする。
    pub fn latin_mode(&self) -> bool {
        self.composing && self.latin_from.is_some()
    }
    #[allow(dead_code)] // テスト専用: prod の Space 処理は OnKeyDown にインライン化（テストモデル）
    pub fn on_space(&self) -> Action {
        if self.composing { Action::RequestConvert } else { Action::Pass }
    }
    #[allow(dead_code)] // テスト専用: prod の Enter 処理は OnKeyDown にインライン化（テストモデル）
    pub fn on_enter(&self) -> Action {
        if self.composing { Action::Commit } else { Action::Pass }
    }
    pub fn on_escape(&mut self) -> Action {
        if self.composing { self.reset(); Action::Cancel } else { Action::Pass }
    }
    pub fn on_backspace(&mut self) -> Action {
        if self.composing {
            self.raw.pop();
            if self.raw.is_empty() {
                self.composing = false;
                // 合成終息=英語モード終了。クランプで Some(0) を残すと次の新規合成へ漏れる。
                self.latin_from = None;
            } else if let Some(lf) = self.latin_from {
                if lf > self.raw.len() {
                    // 英語部分を消し切ってかな部へ食い込んだ: 開始位置だけ追随し、モードは
                    // 確定まで維持(MS-IME 同様)。次の英字打鍵はここから direct 部になる。
                    self.latin_from = Some(self.raw.len());
                }
            }
            self.notation_fixed = false; // 読みが変わりライブ変換が再開する＝表記固定は解除
            Action::StartOrUpdatePreedit(self.raw.clone())
        } else { Action::Pass }
    }
    pub fn reset(&mut self) {
        self.raw.clear();
        self.composing = false;
        self.phase = Phase::Composing;
        self.notation_fixed = false;
        self.latin_from = None;
        self.last_good_text.clear();
        self.last_good_raw_len = 0;
    }

    /// 前方一致候補の部分確定後、残り読み `remaining` で composition を継続する状態に整える。
    /// `raw` を残り読み（かな）で満たすのは on_backspace の composing 判定をエンジン側の残り読みと
    /// **1:1 で同期**させるため（raw が空のままだと最初の Backspace で composing を取りこぼし、
    /// 2かな以上の残り読みが途中で打ち切られる＝データロス再発。defect#1）。raw が前面に出るのは
    /// エンジン応答失敗時の劣化フォールバックのみで、その場合も正しい残り読みを表示できる。
    pub fn reseed_after_partial_commit(&mut self, remaining: &str) {
        self.raw = remaining.to_string();
        self.composing = true;
        self.phase = Phase::Composing;
        self.notation_fixed = false; // 残り読みのライブ変換が再開する（arm_debounce と対）
        self.latin_from = None; // 残り読みはかな＝英語モードは部分確定で終わる
        // 部分確定前(全読み時代)の last_good が残ると、直後の劣化で確定済み
        // テキストが二重に前置される。残り読みで記録し直す。
        self.last_good_text = remaining.to_string();
        self.last_good_raw_len = self.raw.len();
    }

    /// エンジン応答または TIP 確定的表示で表示文字列を更新したときに呼ぶ。
    /// 劣化時フォールバック(degraded_reading)の素材を最新化する。
    pub fn mark_good(&mut self, text: &str) {
        self.last_good_text = text.to_string();
        self.last_good_raw_len = self.raw.len();
    }

    /// エンジン劣化(応答 None)時の表示/確定文字列。契約: 劣化キーイベント 1 回につき
    /// 1 回呼び、結果を live_text/last_reading へ保存して再利用する(同期後の再呼び出しは
    /// 冪等だが契約としては依存しない)。raw 全体へ巻き戻さないのは、直前まで成功していた
    /// 変換結果が last_good に残っており、捨てると Enter 確定が生ローマ字になるため
    /// (spec 2026-07-21-engine-crash-degraded-fallback-design.md)。
    pub fn degraded_reading(&mut self) -> String {
        if self.raw.len() < self.last_good_raw_len {
            // 劣化中の Backspace が追記分を食い尽くして変換済み部へ食い込んだ。
            // 表示 1 文字と raw 1 char は 1:1 でない(日本語↔nihongo)ため厳密対応は
            // 不可能 — 表示 1 文字 pop の best-effort に留める。
            self.last_good_text.pop();
            self.last_good_raw_len = self.raw.len();
        }
        // last_good_raw_len は常に「過去のある時点の raw.len()」。raw の変更経路は
        // 末尾 push / char 単位 pop / 全置換(reset・reseed は記録も同時更新)のみ
        // なので、記録値 ≤ 現長なら char 境界にあり、このスライスは panic しない。
        let at = self.last_good_raw_len.min(self.raw.len());
        let result = format!("{}{}", self.last_good_text, &self.raw[at..]);
        if result.is_empty() && !self.raw.is_empty() {
            // 表示を削り尽くしても raw が残る密度差ケース。空を返すと呼び出し側の
            // reading.is_empty() → do_cancel が composition ごと raw を破棄する
            // (spec レビュー I-1)ため、生ローマ字表示へ縮退して継続する。
            self.last_good_raw_len = 0;
            return self.raw.clone();
        }
        result
    }
    /// ライブ変換要求ごとに seq を1つ進めて返す（TIP 採番）。
    pub fn bump_live_seq(&mut self) -> u64 {
        self.live_seq += 1;
        self.live_seq
    }
    /// Tab: composition 中かつ Composing フェーズのときだけ LLM 変換を要求する。
    #[allow(dead_code)] // テスト専用: prod は VK_TAB→start_llm_convert を直接呼ぶ（テストモデル）
    pub fn on_tab(&self) -> Action {
        if self.composing && self.phase == Phase::Composing { Action::RequestLlmConvert } else { Action::Pass }
    }
    /// LLM 変換要求ごとに seq を1つ進める（世代ガード用・TIP 採番）。
    pub fn bump_llm_seq(&mut self) -> u64 { self.llm_seq += 1; self.llm_seq }
    pub fn awaiting_llm(&self) -> bool { self.phase == Phase::AwaitingLlm }
    pub fn set_awaiting_llm(&mut self, on: bool) {
        self.phase = if on { Phase::AwaitingLlm } else { Phase::Composing };
    }
}

/// 合成途中にエンジンセッションが失われたか（バグ#2: live_convert タイムアウト等で
/// drop_engine された後、次打鍵の ensure_session が**空の新セッション**を張るケース）の純判定。
/// `session==0` かつ `raw` に蓄積があるのは「commit/cancel/放棄/Deactivate の後」では
/// あり得ない（それらは必ず raw を clear する。Deactivate の reset は 2026-07-07 レビュー
/// I-1 で追加 — 怠ると取消済みテキストが再活性化後の初打鍵で復活する偽陽性リプレイになる）。
/// よってこの組合せ＝合成途中の喪失と同値。
/// true なら新セッションへ raw を一括リプレイしないと、それまでの未確定入力が全損する。
pub fn needs_session_reseed(session: i64, raw: &str) -> bool {
    session == 0 && !raw.is_empty()
}

/// 候補確定の分岐。`FullReset`=従来どおり全確定（composition/セッションを片付ける）、
/// `PartialReseed`=前方一致候補の部分確定（prefix を確定し remaining でセッションを継続）。
#[derive(Debug, PartialEq)]
pub enum CommitPlan {
    FullReset { text: String },
    PartialReseed { prefix: String, remaining: String },
}

/// 候補確定の分岐を決める純関数。`outcome` はエンジンの commit 応答
/// （成功なら `Some((確定text, 残り読み))`、失敗/未知セッションなら `None`）、
/// `resolved_text` は TIP 側 cand_state で解決済みの確定文字列。
/// 残り読みが非空のときだけ部分確定。空（全消費）・失敗はいずれも従来どおりの全確定（バイト等価）。
pub fn plan_commit(outcome: Option<(String, String)>, resolved_text: &str) -> CommitPlan {
    match outcome {
        Some((prefix, remaining)) if !remaining.is_empty() => {
            CommitPlan::PartialReseed { prefix, remaining }
        }
        _ => CommitPlan::FullReset { text: resolved_text.to_string() },
    }
}

/// 候補窓なし Enter（ライブ確定）の分岐を決める純関数（Spec2）。
#[derive(Debug, PartialEq)]
pub enum LiveEnterPlan {
    /// ライブ変換結果あり → engine Commit(0) 経由で確定（学習に乗せる）。
    EngineCommit { text: String },
    /// エンジン劣化 → TIP 手持ちの文字列で直確定（従来挙動・学習なし・確定は必ず成功）。
    DirectCommit { text: String },
}

/// `live` はエンジンのライブ変換応答（None/空 = 劣化）。劣化時は live_text（表示中の文字列）、
/// それも空なら last_reading で直確定する（従来の unwrap 連鎖 engine→live_text→reading と同値）。
pub fn plan_live_enter(live: Option<String>, live_text: &str, last_reading: &str) -> LiveEnterPlan {
    match live.filter(|t| !t.is_empty()) {
        Some(text) => LiveEnterPlan::EngineCommit { text },
        None => {
            let text = if !live_text.is_empty() { live_text.to_string() } else { last_reading.to_string() };
            LiveEnterPlan::DirectCommit { text }
        }
    }
}

/// commit対象文字列を決める純関数（テスト可能）。エンジン失敗時は読みのまま確定する劣化動作。
#[allow(dead_code)] // テスト専用: prod は OnKeyDown に同等の select-or-fallback をインライン化（参照モデル）
pub fn commit_text(convert_result: Result<Vec<String>, ()>, selected: usize, fallback_reading: &str) -> String {
    match convert_result {
        Ok(cands) if !cands.is_empty() => cands.get(selected).cloned().unwrap_or_else(|| cands[0].clone()),
        _ => fallback_reading.to_string(),
    }
}

/// ライブ変換応答が「最新」か（A2 で複数 in-flight のとき古い応答を捨てるための純判定）。
/// A1（同期・1要求1応答）では常に真。
pub fn is_fresh_live(resp_seq: u64, current_seq: u64) -> bool {
    resp_seq == current_seq
}

/// 文字列末尾から連続する ASCII 英字 `[A-Za-z]` ＋ ハイフン `-` の長さ（バイト数＝文字数）を
/// 返す純関数。スペース/他の句読点/数字/非ASCII で停止する。SP5 再変換の「直前ラテン列」境界
/// 決定に使う（D5）。`-` を含めるのはローマ字の長音（`wa-rudo`→ワールド）を1列として掴むため
/// — engine へ渡す直前に `latin_reconvert_reading` が `-`→`ー` へ写す。`-`/`[A-Za-z]` は全て
/// 1バイトASCIIなのでバイト数＝文字数＝UTF-16単位数の不変条件は保たれる（呼び出し側のスライス
/// / ShiftStart が char 境界安全）。
pub fn latin_run_span(text: &str) -> usize {
    text.bytes()
        .rev()
        .take_while(|b| b.is_ascii_alphabetic() || *b == b'-')
        .count()
}

/// キャレット左の周辺テキストを Zenzai/LLM 向け左文脈へ整形する純関数（U9）。
/// 「区切り」（制御文字全般・U+FFFC=TS_CHAR_EMBEDDED・U+2028/U+2029=行/段落区切り）より
/// **後ろだけ**を残す — 区切りを「除去」すると `foo\tbar`→`foobar` の偽文脈を作るため、
/// 除去でなくカットにする。先頭の U+FFFD は 64 UTF-16 単位読みの先頭でサロゲート対が
/// 割れた痕跡（from_utf16_lossy の置換文字）なので strip する。最後に末尾 40 文字
/// （char 単位 = Zenzai 既定 maxLeftSideContextLength と一致）へクランプし、空なら None。
pub fn sanitize_left_context(raw: &str) -> Option<String> {
    let is_separator =
        |c: char| c.is_control() || matches!(c, '\u{FFFC}' | '\u{2028}' | '\u{2029}');
    let tail = match raw.rfind(is_separator) {
        Some(i) => &raw[i + raw[i..].chars().next().map_or(1, char::len_utf8)..],
        None => raw,
    };
    let tail = tail.trim_start_matches('\u{FFFD}');
    let n = tail.chars().count();
    let clamped: String = tail.chars().skip(n.saturating_sub(40)).collect();
    if clamped.is_empty() { None } else { Some(clamped) }
}

// ---- 打鍵作法バンドル: 表記変換の純関数（Task 1）----

/// かな入力打鍵の記号の既定幅（分類順に畳む）: 長音符=無条件 → 句読点=punct トグル →
/// 記号=symbol トグル(既定 OFF) → 置換3件(/[]→・「」 = Mozc symbol_method 相当) →
/// 残りは is_ascii_punctuation 全域を式で全角形へ。英数字は構造的に対象外
///（roman2kana に委ねる）。VK でなく ToUnicode 結果の文字で引く — 記号 VK は
/// レイアウト依存（JIS/US）のため VK 固定マップは禁止（設計ロック 2026-07-07）。
/// 個別表を全記号に持たないのは、表の穴（旧 !/@ の US 到達不能・~→U+301C 混入）を
/// 再生産しないため（2026-07-16 spec §2）。
/// idle 直接確定と composition 畳み込みの両方が呼ぶ単一マップ（`-` と全記号を同仕様に）。
pub fn zenkaku_symbol(c: char, punct_full_width: bool, symbol_full_width: bool) -> Option<char> {
    Some(match c {
        '-' => 'ー',
        ',' => if punct_full_width { '、' } else { return None },
        '.' => if punct_full_width { '。' } else { return None },
        _ if !symbol_full_width => return None,
        '/' => '・', '[' => '「', ']' => '」',
        c if c.is_ascii_punctuation() => zenkaku_of(c),
        _ => return None,
    })
}

/// ASCII 印字可能域の機械写像（0x21..=0x7E → U+FF01..=U+FF5E）。`~`→～(U+FF5E) はここから
/// 出る＝Windows 正準。Mozc/iOS 版の U+301C（波ダッシュ）は CP932 で ? に化けるため採らない。
fn zenkaku_of(c: char) -> char {
    char::from_u32(c as u32 - 0x21 + 0xFF01).unwrap_or(c)
}

/// かな入力中の物理キー文字を「読みに積む文字」へ写す（物理キーボードの打鍵作法）。
/// nospacekey の roman2kana は iOS 前提で `-`→`ー` を持たないため、ここで長音符を補う。
/// 対象は現状 `-`→`ー` のみ。他はそのまま（記号/英字は engine の roman2kana に委ねる）。
pub fn to_kana_reading_char(ch: char) -> char {
    match ch {
        '-' => 'ー',
        _ => ch,
    }
}

/// direct モード再変換で掴んだ生ラテン列（例 `wa-rudo`）を engine へ渡す「読み」へ整形する
/// 純関数。nospacekey の roman2kana は `-`→`ー` を持たない（iOS 前提）ため、`to_kana_reading_char`
/// と同じ写像（`-`→`ー`, 他は不変）を列全体へ適用して長音を復元する
/// （`wa-rudo`→`waーrudo`→roman2kana→`わーるど`→ワールド）。元テキスト（Esc 復元用）は
/// 呼び出し側が別に保持するので、これは engine 入力専用の変換。
pub fn latin_reconvert_reading(text: &str) -> String {
    text.chars().map(to_kana_reading_char).collect()
}

/// ひらがな（U+3041-3096）→カタカナ（+0x60）。他は素通し（長音符 U+30FC は既に共通）。
pub fn to_katakana(s: &str) -> String {
    s.chars().map(|c| match c as u32 {
        0x3041..=0x3096 => char::from_u32(c as u32 + 0x60).unwrap_or(c),
        _ => c,
    }).collect()
}

/// ひらがな→半角カナ（U+FF61-FF9F）。濁点/半濁点は「基底半角カナ+ﾞ/ﾟ」の2単位へ分解する固定表。
/// 表に無い文字（漢字/英数/記号）は素通し。
pub fn to_hankaku_kana(s: &str) -> String {
    // 五十音全段＋濁音＋半濁音＋小書き＋ん/長音符＋IME 句読点。ゐ/ゑ は半角カナに無いので素通し。
    const TABLE: &[(char, &str)] = &[
        ('あ', "ｱ"), ('い', "ｲ"), ('う', "ｳ"), ('え', "ｴ"), ('お', "ｵ"),
        ('か', "ｶ"), ('き', "ｷ"), ('く', "ｸ"), ('け', "ｹ"), ('こ', "ｺ"),
        ('さ', "ｻ"), ('し', "ｼ"), ('す', "ｽ"), ('せ', "ｾ"), ('そ', "ｿ"),
        ('た', "ﾀ"), ('ち', "ﾁ"), ('つ', "ﾂ"), ('て', "ﾃ"), ('と', "ﾄ"),
        ('な', "ﾅ"), ('に', "ﾆ"), ('ぬ', "ﾇ"), ('ね', "ﾈ"), ('の', "ﾉ"),
        ('は', "ﾊ"), ('ひ', "ﾋ"), ('ふ', "ﾌ"), ('へ', "ﾍ"), ('ほ', "ﾎ"),
        ('ま', "ﾏ"), ('み', "ﾐ"), ('む', "ﾑ"), ('め', "ﾒ"), ('も', "ﾓ"),
        ('や', "ﾔ"), ('ゆ', "ﾕ"), ('よ', "ﾖ"),
        ('ら', "ﾗ"), ('り', "ﾘ"), ('る', "ﾙ"), ('れ', "ﾚ"), ('ろ', "ﾛ"),
        ('わ', "ﾜ"), ('を', "ｦ"), ('ん', "ﾝ"),
        ('が', "ｶﾞ"), ('ぎ', "ｷﾞ"), ('ぐ', "ｸﾞ"), ('げ', "ｹﾞ"), ('ご', "ｺﾞ"),
        ('ざ', "ｻﾞ"), ('じ', "ｼﾞ"), ('ず', "ｽﾞ"), ('ぜ', "ｾﾞ"), ('ぞ', "ｿﾞ"),
        ('だ', "ﾀﾞ"), ('ぢ', "ﾁﾞ"), ('づ', "ﾂﾞ"), ('で', "ﾃﾞ"), ('ど', "ﾄﾞ"),
        ('ば', "ﾊﾞ"), ('び', "ﾋﾞ"), ('ぶ', "ﾌﾞ"), ('べ', "ﾍﾞ"), ('ぼ', "ﾎﾞ"),
        ('ぱ', "ﾊﾟ"), ('ぴ', "ﾋﾟ"), ('ぷ', "ﾌﾟ"), ('ぺ', "ﾍﾟ"), ('ぽ', "ﾎﾟ"),
        ('ゔ', "ｳﾞ"),
        ('ぁ', "ｧ"), ('ぃ', "ｨ"), ('ぅ', "ｩ"), ('ぇ', "ｪ"), ('ぉ', "ｫ"),
        ('ゃ', "ｬ"), ('ゅ', "ｭ"), ('ょ', "ｮ"), ('っ', "ｯ"), ('ー', "ｰ"),
        ('。', "｡"), ('、', "､"), ('「', "｢"), ('」', "｣"), ('・', "･"),
    ];
    s.chars().map(|c| TABLE.iter().find(|(k, _)| *k == c).map(|(_, v)| (*v).to_string())
        .unwrap_or_else(|| c.to_string())).collect()
}

/// ASCII 印字可能文字（0x21-0x7E）→全角（U+FF01-FF5E）。空白は U+3000。非 ASCII は素通し。
pub fn to_zenkaku_ascii(s: &str) -> String {
    s.chars().map(|c| match c as u32 {
        0x20 => '\u{3000}',
        0x21..=0x7E => char::from_u32(c as u32 - 0x21 + 0xFF01).unwrap_or(c),
        _ => c,
    }).collect()
}

/// 数字だけを全角へ写す（`0-9`→`０-９`）。他文字は素通し。数字全角設定の既定確定用。
/// to_zenkaku_ascii（英字/記号も全角化）と違い、読みに紛れた英字/記号を誤変換しないよう数字限定。
pub fn to_zenkaku_digits(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '0'..='9' => char::from_u32(c as u32 - '0' as u32 + 0xFF10).unwrap_or(c),
            _ => c,
        })
        .collect()
}

/// 既定確定で数字を全角化するかの純判定。全角設定 ON かつ かなモード（!direct）かつ
/// 候補の明示選択でない（source が "candidate"/"candidate_prefix" でない）とき true。
pub fn should_widen_digits(number_full_width: bool, direct: bool, source: &str) -> bool {
    number_full_width && !direct && !matches!(source, "candidate" | "candidate_prefix")
}

/// エンジン Insert の挿入解釈(IPC style フィールドの TIP 内表現)。
/// Kana=roman2kana(従来・ワイヤ上は style 省略) / Direct=リテラル(Shift英語モード)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InsertStyle { Kana, Direct }

/// セッション喪失リプレイ(バグ#2)の分割純関数。raw をかな部(roman2kana)と英語部(direct)へ
/// 分け、送信順の (text, style) 列を返す。2区間で済むのは latin_from が合成中に一度しか
/// 立たない(英語モードは合成終息まで解除されない)ため — 任意インターリーブを表せる
/// Vec<(String, Style)> を raw 側に持たせる案は不要な一般化。境界/範囲外/None は
/// 1 区間へ縮退(従来ワイヤ等価)。
pub fn split_replay(raw: &str, latin_from: Option<usize>) -> Vec<(String, InsertStyle)> {
    match latin_from {
        Some(i) if i > 0 && i < raw.len() => vec![
            (raw[..i].to_string(), InsertStyle::Kana),
            (raw[i..].to_string(), InsertStyle::Direct),
        ],
        Some(0) if !raw.is_empty() => vec![(raw.to_string(), InsertStyle::Direct)],
        _ => vec![(raw.to_string(), InsertStyle::Kana)],
    }
}

/// 再変換の対象種別。`None`=対象なし / `Latin`=ローマ字リプレイ / `Surface`=かな表層を
/// エンジンへ .direct で渡す / `NonKana`=漢字・混在等（再変換せず無害に離脱）。
#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum ReconvertKind { #[default] None, Latin, Surface, NonKana }

/// 1 文字がかな（ひらがな/カタカナ ブロック）か。長音符 ー(U+30FC)・濁点/反復記号を含む。
fn is_kana(c: char) -> bool {
    matches!(c as u32, 0x3041..=0x309F | 0x30A0..=0x30FF)
}

/// 非空選択の文字列を再変換経路へ分類する純関数（SP5 step-6）。
/// - すべて ASCII 英字 ＋ ハイフン `-` -> Latin （ローマ字リプレイ。`-` は長音の一部）
/// - すべてかな               -> Surface （エンジン .direct 変換）
/// - 空                       -> None
/// - それ以外（漢字/混在/数字/記号/空白）-> NonKana（合成せず何もしない）
///
/// `-` を Latin に含めるのは空選択経路（`latin_run_span`）と境界規律を揃えるため。
pub fn classify_reconvert_selection(s: &str) -> ReconvertKind {
    if s.is_empty() { return ReconvertKind::None; }
    if s.chars().all(|c| c.is_ascii_alphabetic() || c == '-') { return ReconvertKind::Latin; }
    if s.chars().all(is_kana) { return ReconvertKind::Surface; }
    ReconvertKind::NonKana
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typing_builds_preedit_and_space_requests_convert() {
        let mut s = InputState::default();
        assert_eq!(s.on_char('n'), Action::StartOrUpdatePreedit("n".into()));
        assert_eq!(s.on_char('i'), Action::StartOrUpdatePreedit("ni".into()));
        assert_eq!(s.on_space(), Action::RequestConvert);
    }
    #[test]
    fn space_without_composition_passes() {
        let s = InputState::default();
        assert_eq!(s.on_space(), Action::Pass);
    }
    #[test]
    fn escape_resets() {
        let mut s = InputState::default();
        s.on_char('a');
        assert_eq!(s.on_escape(), Action::Cancel);
        assert_eq!(s.raw, "");
        assert!(!s.composing);
    }
    #[test]
    fn backspace_shrinks_then_passes() {
        let mut s = InputState::default();
        s.on_char('a');
        assert_eq!(s.on_backspace(), Action::StartOrUpdatePreedit("".into()));
        assert_eq!(s.on_backspace(), Action::Pass);
    }
    #[test]
    fn commit_falls_back_to_reading_on_engine_error() {
        assert_eq!(commit_text(Err(()), 0, "にほんご"), "にほんご");
    }
    #[test]
    fn commit_uses_selected_candidate() {
        assert_eq!(commit_text(Ok(vec!["日本語".into(), "にほんご".into()]), 0, "にほんご"), "日本語");
    }
    #[test]
    fn live_seq_is_monotonic() {
        let mut s = InputState::default();
        assert_eq!(s.bump_live_seq(), 1);
        assert_eq!(s.bump_live_seq(), 2);
    }
    #[test]
    fn fresh_live_only_for_matching_seq() {
        assert!(is_fresh_live(5, 5));        // 最新 seq の応答だけ採用
        assert!(!is_fresh_live(4, 5));       // 古い応答は捨てる（A2 用）
    }

    #[test]
    fn latin_run_span_basic() {
        assert_eq!(latin_run_span("nihongo"), 7);       // 全部ラテン
        assert_eq!(latin_run_span("React nihongo"), 7); // 直前スペースで停止 → "nihongo"
        assert_eq!(latin_run_span(""), 0);              // 空
        assert_eq!(latin_run_span("abc "), 0);          // 末尾が空白 → 0
        assert_eq!(latin_run_span("abc123"), 0);        // 末尾が数字 → 0
        assert_eq!(latin_run_span("日本語go"), 2);       // 非ASCIIで停止 → "go"
        assert_eq!(latin_run_span("a.b"), 1);           // 句読点で停止 → "b"
    }

    #[test]
    fn latin_run_span_spans_hyphen() {
        // ローマ字の長音を表す半角ハイフン `-` は境界にしない（`wa-rudo`→ワールド 用）。
        // これが無いと後方スキャンが `-` で止まり `rudo` だけを掴む（本バグ）。
        assert_eq!(latin_run_span("wa-rudo"), 7);        // ハイフンを跨いで全体を掴む
        assert_eq!(latin_run_span("React wa-rudo"), 7); // 直前スペースで停止 → "wa-rudo"
        // 他の境界は不変（`-` だけを許可、他記号・空白・数字・非ASCIIは依然境界）。
        assert_eq!(latin_run_span("a.b"), 1);           // 句読点は依然境界
        assert_eq!(latin_run_span("abc "), 0);          // 末尾空白は依然 0
    }
    #[test]
    fn tab_requests_llm_only_when_composing_and_idle_phase() {
        let mut s = InputState::default();
        assert_eq!(s.on_tab(), Action::Pass); // 非 composition
        s.on_char('a');
        assert_eq!(s.on_tab(), Action::RequestLlmConvert);
        s.set_awaiting_llm(true);
        assert_eq!(s.on_tab(), Action::Pass); // 待機中は再要求しない
    }

    // on_space=>RequestConvert は上でテスト済み。その対の on_enter=>Commit を補い、
    // 入力状態機械の Space/Enter 対称性を保つ（これで on_enter/Commit も cfg(test) で被覆）。
    #[test]
    fn enter_commits_only_when_composing() {
        let mut s = InputState::default();
        assert_eq!(s.on_enter(), Action::Pass); // 非 composition は素通し
        s.on_char('a');
        assert_eq!(s.on_enter(), Action::Commit); // composition 中は確定
    }

    #[test]
    fn llm_seq_is_monotonic_and_awaiting_toggles() {
        let mut s = InputState::default();
        assert_eq!(s.bump_llm_seq(), 1);
        assert_eq!(s.bump_llm_seq(), 2);
        assert!(!s.awaiting_llm());
        s.set_awaiting_llm(true);
        assert!(s.awaiting_llm());
        s.set_awaiting_llm(false);
        assert!(!s.awaiting_llm());
    }

    #[test]
    fn classify_reconvert_selection_routes_by_script() {
        use ReconvertKind::*;
        assert_eq!(classify_reconvert_selection("nihongo"), Latin);   // 純ASCII英字
        assert_eq!(classify_reconvert_selection("にほんご"), Surface); // ひらがな
        assert_eq!(classify_reconvert_selection("ニホンゴ"), Surface); // カタカナ
        assert_eq!(classify_reconvert_selection("ラーメン"), Surface); // 長音符込みカタカナ
        assert_eq!(classify_reconvert_selection("日本語"), NonKana);   // 漢字
        assert_eq!(classify_reconvert_selection("日本ご"), NonKana);   // 漢字+かな混在
        assert_eq!(classify_reconvert_selection("にほん go"), NonKana); // かな+ラテン混在(空白含む)
        assert_eq!(classify_reconvert_selection("abc123"), NonKana);   // 英字+数字
        assert_eq!(classify_reconvert_selection(""), None);            // 空
    }

    #[test]
    fn classify_reconvert_selection_allows_hyphen_in_latin() {
        use ReconvertKind::*;
        // 選択したローマ字にハイフン（長音）が含まれても Latin として再変換する
        // （`latin_run_span` と同じ境界規律 — 空選択経路と選択経路で挙動を揃える）。
        assert_eq!(classify_reconvert_selection("wa-rudo"), Latin);
        assert_eq!(classify_reconvert_selection("e-mail"), Latin);
    }

    #[test]
    fn latin_reconvert_reading_maps_hyphen_to_prolonged() {
        // direct 再変換で掴んだ生ラテン列は engine へ渡す前に `-`→`ー` へ写す
        // （nospacekey roman2kana は `-`→`ー` を欠くため。`waーrudo`→roman2kana→わーるど→ワールド）。
        assert_eq!(latin_reconvert_reading("wa-rudo"), "waーrudo");
        assert_eq!(latin_reconvert_reading("nihongo"), "nihongo"); // ハイフン無しは不変
        // 設計判断（意図的）: direct 再変換はラテン列を「ローマ字」として解釈するので、
        // 列中の `-` は一律 長音 `ー` とみなす。`e-mail`/`Wi-Fi` のような英単語の `-` も
        // `ー` になる（`eーmail`）が、これは許容する — 文字だけでは `wa-rudo`(長音)と
        // `e-mail`(英ハイフン)は判別不能で、そもそも英単語を再変換(Alt+/)する動線は無い
        // （再変換＝ローマ字→日本語の明示要求）。誤爆時はユーザが Esc で生テキストへ復元できる。
        assert_eq!(latin_reconvert_reading("e-mail"), "eーmail");
    }

    // ---- 前方一致候補の部分確定（データロス対策） ----

    #[test]
    fn plan_commit_partial_when_reading_remains() {
        // エンジンが (確定text, 残り読み) を返し残り読みが非空 → 部分確定（残りを継続）。
        let plan = plan_commit(Some(("日本".into(), "ご".into())), "日本");
        assert_eq!(plan, CommitPlan::PartialReseed { prefix: "日本".into(), remaining: "ご".into() });
    }

    #[test]
    fn plan_commit_full_when_no_remaining() {
        // 残り読みが空（全消費）→ 従来どおりの全確定（resolved_text を確定）。
        let plan = plan_commit(Some(("日本語".into(), "".into())), "日本語");
        assert_eq!(plan, CommitPlan::FullReset { text: "日本語".into() });
    }

    #[test]
    fn plan_commit_full_on_engine_failure_uses_resolved_text() {
        // エンジン失敗(None)→ TIP 解決済み文字列で全確定（劣化＝従来挙動・バイト等価）。
        let plan = plan_commit(None, "にほんご");
        assert_eq!(plan, CommitPlan::FullReset { text: "にほんご".into() });
    }

    // ---- ライブ確定（候補窓なし Enter）の engine Commit(0) 合流（Spec2） ----

    #[test]
    fn plan_live_enter_prefers_engine_result() {
        // エンジンのライブ変換が生きていれば Commit(0) 経由（学習に乗せる）。
        let p = plan_live_enter(Some("日本語".into()), "にほんご", "nihongo");
        assert_eq!(p, LiveEnterPlan::EngineCommit { text: "日本語".into() });
    }
    #[test]
    fn plan_live_enter_degrades_to_live_text() {
        // エンジン劣化(None): 表示中のライブ文字列で直確定（従来挙動・学習なし）。
        let p = plan_live_enter(None, "にほんご", "nihongo");
        assert_eq!(p, LiveEnterPlan::DirectCommit { text: "にほんご".into() });
    }
    #[test]
    fn plan_live_enter_falls_back_to_reading() {
        // ライブ文字列も空: 読みで直確定（従来の unwrap 連鎖の最終段と同値）。
        let p = plan_live_enter(None, "", "nihongo");
        assert_eq!(p, LiveEnterPlan::DirectCommit { text: "nihongo".into() });
    }
    #[test]
    fn plan_live_enter_empty_engine_result_degrades() {
        // エンジンが空文字を返したら劣化扱い（従来の .filter(!empty) と同値）。
        let p = plan_live_enter(Some(String::new()), "あ", "a");
        assert_eq!(p, LiveEnterPlan::DirectCommit { text: "あ".into() });
    }

    #[test]
    fn reseed_keeps_composing_until_remaining_exhausted() {
        // 部分確定後、残り読み(2かな)で reseed。on_backspace は残り読みと 1:1 で縮み、
        // 最後の1かなを消すまで composing を維持する（defect#1 回帰: composing 早期 false を防ぐ）。
        let mut s = InputState::default();
        s.reseed_after_partial_commit("ほご"); // 2 かな
        assert!(s.composing);
        assert_eq!(s.raw, "ほご");
        s.on_backspace();                  // 1かな消す
        assert!(s.composing, "残り読みが残る間は composing を維持");
        s.on_backspace();                  // 最後の1かな
        assert!(!s.composing, "残り読み枯渇で composing 解除");
    }

    #[test]
    fn reseed_single_kana_drops_composing_on_one_backspace() {
        let mut s = InputState::default();
        s.reseed_after_partial_commit("ご"); // 1 かな
        assert!(s.composing);
        s.on_backspace();
        assert!(!s.composing);
    }

    // ---- エンジン劣化時フォールバック: last_good + raw 追記 ----
    // spec: docs/superpowers/specs/2026-07-21-engine-crash-degraded-fallback-design.md

    #[test]
    fn degraded_reading_appends_raw_typed_after_last_good() {
        let mut s = InputState::default();
        for c in "nihongo".chars() { s.on_char(c); }
        s.mark_good("日本語");
        s.on_char('d');
        assert_eq!(s.degraded_reading(), "日本語d");
    }
    #[test]
    fn degraded_reading_without_last_good_falls_back_to_raw() {
        let mut s = InputState::default();
        for c in "abc".chars() { s.on_char(c); }
        assert_eq!(s.degraded_reading(), "abc");
    }
    #[test]
    fn degraded_backspace_consumes_appended_suffix_first() {
        let mut s = InputState::default();
        for c in "nihongo".chars() { s.on_char(c); }
        s.mark_good("日本語");
        s.on_char('d');
        s.on_backspace();
        assert_eq!(s.degraded_reading(), "日本語");
    }
    #[test]
    fn degraded_backspace_after_suffix_exhausted_trims_last_good_display() {
        let mut s = InputState::default();
        for c in "nihongo".chars() { s.on_char(c); }
        s.mark_good("日本語");
        s.on_backspace(); // raw="nihong"(6) < 記録7 → 表示1文字pop
        assert_eq!(s.degraded_reading(), "日本");
    }
    #[test]
    fn degraded_reading_never_returns_empty_while_raw_remains() {
        // spec レビュー I-1: 表示を削り尽くしても raw が残る限り空を返さない
        // (空を返すと呼び出し側の cancel 経路が raw ごと破棄する)。
        let mut s = InputState::default();
        for c in "nihongo".chars() { s.on_char(c); }
        s.mark_good("日本語");
        s.on_backspace();
        assert_eq!(s.degraded_reading(), "日本");
        s.on_backspace();
        assert_eq!(s.degraded_reading(), "日");
        s.on_backspace(); // 表示枯渇 → raw フォールバック
        assert_eq!(s.degraded_reading(), "niho");
        assert!(s.composing);
        s.on_backspace(); // 以後は raw 縮退で継続
        assert_eq!(s.degraded_reading(), "nih");
    }
    #[test]
    fn degraded_reading_is_idempotent_after_sync() {
        // 契約は1イベント1回だが、二重呼び出しが破壊的でないこと(pop は shrink 検知時のみ)
        let mut s = InputState::default();
        for c in "nihongo".chars() { s.on_char(c); }
        s.mark_good("日本語");
        s.on_backspace();
        assert_eq!(s.degraded_reading(), "日本");
        assert_eq!(s.degraded_reading(), "日本");
    }
    #[test]
    fn reset_clears_last_good() {
        let mut s = InputState::default();
        s.on_char('a');
        s.mark_good("あ");
        s.reset();
        assert_eq!(s.last_good_text, "");
        assert_eq!(s.last_good_raw_len, 0);
    }
    #[test]
    fn reseed_after_partial_commit_records_remaining_as_last_good() {
        let mut s = InputState::default();
        for c in "kyouhaame".chars() { s.on_char(c); }
        s.mark_good("今日は雨");
        s.reseed_after_partial_commit("あめ");
        assert_eq!(s.last_good_text, "あめ");
        assert_eq!(s.last_good_raw_len, "あめ".len());
        s.on_char('d');
        assert_eq!(s.degraded_reading(), "あめd");
    }
    #[test]
    fn degraded_reading_survives_multibyte_raw_from_partial_commit() {
        // 部分確定後の raw はかな(マルチバイト)。pop→スライスで境界 panic しないこと。
        let mut s = InputState::default();
        s.reseed_after_partial_commit("かな");
        s.on_backspace(); // raw="か"(3B) < 記録6B
        assert_eq!(s.degraded_reading(), "か");
    }
    #[test]
    fn degraded_reading_survives_multibyte_append_like_long_vowel_mark() {
        // spec レビュー M-5: 追記側のマルチバイト('-'→'ー' は raw へ 3 バイト push)も試験。
        let mut s = InputState::default();
        for c in "ra".chars() { s.on_char(c); }
        s.mark_good("ら");
        s.on_char('ー');
        assert_eq!(s.degraded_reading(), "らー");
        s.on_backspace();
        assert_eq!(s.degraded_reading(), "ら");
    }

    // ---- バグ#2: 合成途中のセッション喪失 → raw 一括リプレイの純判定 ----

    #[test]
    fn reseed_needed_only_when_session_lost_mid_composition() {
        // session==0 かつ raw 蓄積あり = live_convert タイムアウト等の drop 後（バグ#2 の窓）。
        assert!(needs_session_reseed(0, "watashinonamaeha"));
        // 生きたセッションでは何もしない（通常打鍵・部分確定の継続セッション）。
        assert!(!needs_session_reseed(7, "watashinonamaeha"));
        // raw が空なら新規合成の開始（commit/cancel/放棄の直後）— リプレイ対象なし。
        assert!(!needs_session_reseed(0, ""));
        assert!(!needs_session_reseed(7, ""));
    }

    // ---- U9: sanitize_left_context ----

    #[test]
    fn left_context_keeps_text_after_last_newline() {
        assert_eq!(sanitize_left_context("a\nbc"), Some("bc".into()));
        assert_eq!(sanitize_left_context("a\r\nbc"), Some("bc".into()));
        assert_eq!(sanitize_left_context("一行目\n二行目\n私の名前は"), Some("私の名前は".into()));
    }

    #[test]
    fn left_context_cuts_at_embedded_object_and_line_separators() {
        // U+FFFC(TS_CHAR_EMBEDDED)・U+2028/U+2029(Zl/Zp — Cc でも \r\n でもない)は区切り。
        assert_eq!(sanitize_left_context("画像\u{FFFC}のあと"), Some("のあと".into()));
        assert_eq!(sanitize_left_context("前\u{2028}後"), Some("後".into()));
        assert_eq!(sanitize_left_context("前\u{2029}後"), Some("後".into()));
    }

    #[test]
    fn left_context_cuts_at_control_not_removes() {
        // 除去だと "foobar" の偽文脈になる。区切り扱いで後ろだけ残す。
        assert_eq!(sanitize_left_context("foo\tbar"), Some("bar".into()));
    }

    #[test]
    fn left_context_strips_leading_replacement_char() {
        // 64 UTF-16 単位読みの先頭でサロゲート対が割れると from_utf16_lossy が U+FFFD を残す。
        assert_eq!(sanitize_left_context("\u{FFFD}こんにちは"), Some("こんにちは".into()));
    }

    #[test]
    fn left_context_clamps_to_last_40_chars() {
        let long: String = "あ".repeat(41);
        assert_eq!(sanitize_left_context(&long), Some("あ".repeat(40)));
        let exact: String = "い".repeat(40);
        assert_eq!(sanitize_left_context(&exact), Some(exact.clone()));
    }

    #[test]
    fn left_context_empty_results_are_none() {
        assert_eq!(sanitize_left_context(""), None);
        assert_eq!(sanitize_left_context("本文\n"), None); // 区切りが末尾 = 後ろは空
        assert_eq!(sanitize_left_context("\u{FFFD}"), None); // strip で空
    }

    #[test]
    fn left_context_plain_text_passes_through() {
        assert_eq!(sanitize_left_context("私の名前は"), Some("私の名前は".into()));
    }

    // ---- 打鍵作法バンドル: 表記変換の純関数 ----

    #[test]
    fn zenkaku_symbol_maps_ime_punctuation() {
        // 長音符: 両トグル無関係（かな。切ると「コーヒー」が打てない）
        assert_eq!(zenkaku_symbol('-', false, false), Some('ー'));
        assert_eq!(zenkaku_symbol('-', true, true), Some('ー'));
        // 句読点: punct トグルのみに従う（symbol トグルと独立）
        assert_eq!(zenkaku_symbol('.', true, false), Some('。'));
        assert_eq!(zenkaku_symbol(',', true, false), Some('、'));
        assert_eq!(zenkaku_symbol('.', false, true), None);
        assert_eq!(zenkaku_symbol(',', false, true), None);
        // 記号トグル OFF: 置換も幅畳み込みも全部 None（ASCII のまま。旧仕様の無条件全角を廃止）
        for c in ['/', '[', ']', '?', '~', ':', ';', '!', '@', '#', '(', ')', '=', '_'] {
            assert_eq!(zenkaku_symbol(c, true, false), None, "{c:?} は OFF で半角");
        }
        // 英数字は常に対象外（roman2kana に委ねる。is_ascii_punctuation が構造的に排除）
        assert_eq!(zenkaku_symbol('a', true, true), None);
        assert_eq!(zenkaku_symbol('1', true, true), None);
    }

    #[test]
    fn zenkaku_symbol_on_replaces_and_folds_all_ascii_punct() {
        // 置換3件（Mozc symbol_method 相当 — 幅でなく別文字への置換）
        assert_eq!(zenkaku_symbol('/', false, true), Some('・'));
        assert_eq!(zenkaku_symbol('[', false, true), Some('「'));
        assert_eq!(zenkaku_symbol(']', false, true), Some('」'));
        // 幅畳み込みは式（0x21..=0x7E → U+FF01..U+FF5E）。~ は U+FF5E＝Windows 正準
        //（U+301C 波ダッシュは CP932 非往復で ? に化けるため意図的に採らない — spec §1）。
        assert_eq!(zenkaku_symbol('~', false, true), Some('\u{FF5E}'));
        assert_eq!(zenkaku_symbol('?', false, true), Some('？'));
        assert_eq!(zenkaku_symbol(':', false, true), Some('：'));
        assert_eq!(zenkaku_symbol(';', false, true), Some('；'));
        assert_eq!(zenkaku_symbol('!', false, true), Some('！'));
        assert_eq!(zenkaku_symbol('@', false, true), Some('＠'));
        assert_eq!(zenkaku_symbol('#', false, true), Some('＃'));
        assert_eq!(zenkaku_symbol('(', false, true), Some('（'));
        assert_eq!(zenkaku_symbol(')', false, true), Some('）'));
        assert_eq!(zenkaku_symbol('=', false, true), Some('＝'));
        assert_eq!(zenkaku_symbol('_', false, true), Some('＿'));
        assert_eq!(zenkaku_symbol('\\', false, true), Some('＼'));
        assert_eq!(zenkaku_symbol('\'', false, true), Some('＇'));
        assert_eq!(zenkaku_symbol('`', false, true), Some('｀'));
    }

    #[test]
    fn to_kana_reading_char_maps_prolonged_sound() {
        assert_eq!(to_kana_reading_char('-'), 'ー'); // 長音符（nospacekey roman2kana が欠く）
        assert_eq!(to_kana_reading_char('a'), 'a');  // 英字は不変
        assert_eq!(to_kana_reading_char('1'), '1');  // 数字は不変
        assert_eq!(to_kana_reading_char('.'), '.');  // 他記号は engine に委ねる（不変）
    }

    #[test]
    fn to_katakana_shifts_hiragana_block_only() {
        assert_eq!(to_katakana("にほんご"), "ニホンゴ");
        assert_eq!(to_katakana("きょうー"), "キョウー"); // 長音符は共通（シフト対象外）
        assert_eq!(to_katakana("あa1"), "アa1");         // 非かなは素通し
    }

    #[test]
    fn to_hankaku_kana_handles_dakuten() {
        assert_eq!(to_hankaku_kana("がぱ"), "ｶﾞﾊﾟ");     // 濁点/半濁点は2単位へ分解
        assert_eq!(to_hankaku_kana("にほんご"), "ﾆﾎﾝｺﾞ");
        assert_eq!(to_hankaku_kana("きょう"), "ｷｮｳ");    // 小書きかな
    }

    #[test]
    fn to_zenkaku_ascii_maps_alnum_and_symbols() {
        assert_eq!(to_zenkaku_ascii("abC1!"), "ａｂＣ１！");
        assert_eq!(to_zenkaku_ascii("あ"), "あ"); // 非ASCIIは素通し
    }

    #[test]
    fn to_zenkaku_digits_maps_only_digits() {
        assert_eq!(to_zenkaku_digits("123"), "１２３");
        assert_eq!(to_zenkaku_digits("2024年"), "２０２４年"); // 漢字は不変
        assert_eq!(to_zenkaku_digits("a-b"), "a-b");           // 英字/記号は不変
        assert_eq!(to_zenkaku_digits("こーひー"), "こーひー");  // かなは不変
    }

    #[test]
    fn should_widen_digits_only_on_default_native_commits() {
        // 全角ON・native・既定確定 → 全角化
        assert!(should_widen_digits(true, false, "live"));
        assert!(should_widen_digits(true, false, "live_prefix"));
        assert!(should_widen_digits(true, false, "live_auto"));
        // 候補の明示選択は幅を変えない
        assert!(!should_widen_digits(true, false, "candidate"));
        assert!(!should_widen_digits(true, false, "candidate_prefix"));
        // settle 系（mode_toggle/navigate）は読みを確定するので既定確定＝全角化（候補選択のみ不変）。
        assert!(should_widen_digits(true, false, "mode_toggle"));
        assert!(should_widen_digits(true, false, "navigate"));
        // 半角設定 OFF は変えない
        assert!(!should_widen_digits(false, false, "live"));
        // direct モードは変えない
        assert!(!should_widen_digits(true, true, "live"));
    }

    // ---- 打鍵作法 Task4: F6-F10 の表記固定ラッチ ----

    #[test]
    fn notation_fixed_cleared_by_typing_backspace_and_reset() {
        let mut s = InputState::default();
        s.on_char('a');
        s.notation_fixed = true;      // F7 等で表記固定（OnKeyDown の F キーアームが立てる）
        s.on_char('b');
        assert!(!s.notation_fixed, "新たな打鍵でライブ変換再開＝固定解除");
        s.notation_fixed = true;
        s.on_backspace();
        assert!(!s.notation_fixed, "Backspace で読みが変わる＝固定解除");
        s.notation_fixed = true;
        s.reset();
        assert!(!s.notation_fixed, "確定/取消の reset で固定解除");
        s.notation_fixed = true;
        s.reseed_after_partial_commit("ご");
        assert!(!s.notation_fixed, "部分確定の reseed で固定解除（残り読みはライブ変換再開）");
    }

    // ---- Shift英語モード(shift_latin=compose): latin_from のライフサイクル ----

    #[test]
    fn latin_mode_starts_at_current_raw_position_and_persists() {
        let mut s = InputState::default();
        s.on_char('k'); s.on_char('y'); s.on_char('o'); s.on_char('u');
        s.on_char_latin('A');
        assert_eq!(s.latin_from, Some(4), "英語部分の開始=直前の raw 長");
        s.on_char_latin('b');
        assert_eq!(s.latin_from, Some(4), "2打目以降は開始位置不変");
        assert_eq!(s.raw, "kyouAb");
        assert!(s.latin_mode());
    }

    #[test]
    fn latin_mode_survives_backspace_into_kana_region() {
        let mut s = InputState::default();
        s.on_char('a');
        s.on_char_latin('B');
        s.on_backspace(); // 英語部分を全消し
        assert!(s.latin_mode(), "確定まで英語モード維持(MS-IME 同様)");
        assert_eq!(s.latin_from, Some(1));
        s.on_char_latin('c');
        assert_eq!(s.raw, "ac");
    }

    #[test]
    fn latin_from_clamps_when_backspace_crosses_boundary() {
        let mut s = InputState::default();
        s.on_char('a'); s.on_char('b');
        s.on_char_latin('C');
        s.on_backspace(); // 'C' 消滅 → raw="ab"(len2)、latin_from=Some(2) は範囲内のまま
        assert_eq!(s.latin_from, Some(2));
        s.on_backspace(); // 'b' 消滅 → raw="a"(len1) < 2 → クランプ
        assert_eq!(s.latin_from, Some(1));
        assert!(s.latin_mode());
    }

    #[test]
    fn latin_mode_implies_composing() {
        // 不変条件: latin_mode ⇒ composing。eaten 整合(gated は latin_mode を知らない)と
        // symbol_keydown の Kana 固定がこれに依存する。latin_from が残っていても composing で
        // なければ英語モードとは見なさない(将来 latin_from クリアを忘れる経路への構造的保険)。
        let mut s = InputState::default();
        s.on_char_latin('A');
        assert!(s.latin_mode());
        s.composing = false;
        assert!(!s.latin_mode(), "composing でなければ英語モードではない");
    }

    #[test]
    fn latin_mode_ends_when_raw_exhausted() {
        let mut s = InputState::default();
        s.on_char_latin('A');
        s.on_backspace();
        assert!(!s.latin_mode(), "raw 枯渇=合成終息でモード終了(Some(0) 残留は次の新規合成へ漏れる)");
    }

    #[test]
    fn latin_mode_cleared_by_reset_and_reseed() {
        let mut s = InputState::default();
        s.on_char_latin('A');
        s.reset();
        assert!(!s.latin_mode(), "確定/取消の reset で解除");
        s.on_char_latin('A');
        s.reseed_after_partial_commit("ご");
        assert!(!s.latin_mode(), "部分確定の残り読みはかな=英語モード解除");
    }

    // ---- セッション喪失リプレイ(バグ#2)の style 分割 ----

    #[test]
    fn split_replay_partitions_kana_and_direct() {
        use InsertStyle::*;
        assert_eq!(split_replay("kyouAb", Some(4)),
            vec![("kyou".to_string(), Kana), ("Ab".to_string(), Direct)]);
        assert_eq!(split_replay("Ab", Some(0)), vec![("Ab".to_string(), Direct)]);
        assert_eq!(split_replay("kyou", None), vec![("kyou".to_string(), Kana)]);
        // クランプ後(英語部分全消し)は 1 区間かなへ縮退(ワイヤ等価)
        assert_eq!(split_replay("kyou", Some(4)), vec![("kyou".to_string(), Kana)]);
    }
}
