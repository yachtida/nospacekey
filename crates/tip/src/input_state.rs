//! COM非依存の入力状態機械。ここをTDDする。
/// 入力フェーズ。Composing=通常ライブ / AwaitingLlm=LLM変換中（入力ロック）。
#[derive(Default, Debug, PartialEq)]
pub enum Phase {
    #[default]
    Composing,
    AwaitingLlm,
}

#[derive(Default, Debug, PartialEq)]
pub struct InputState {
    pub raw: String,     // 打鍵で貯めたローマ字（エンジンに送る生入力）
    pub composing: bool, // composition中か
    pub live_seq: u64,   // ライブ変換要求のシーケンス番号（A2 の古い応答破棄用）
    pub llm_seq: u64,
    pub phase: Phase,
    /// 打鍵作法 Task4: F6-F10 で表記を固定したか(Some=どの表記で固定したか)。Some の間、
    /// Enter/settle は engine のライブ変換結果を参照せず表示中の live_text を直確定する
    /// （F7 のカタカナが engine の漢字結果で上書き確定されるのを防ぐ）。新たな打鍵/
    /// Backspace/確定/取消で解除。表記種別を持つのは NotationRotate(無変換連打)の巡回起点
    /// 「現在の表記の次」に必要なため。
    pub notation_fixed: Option<crate::keymap::Notation>,
    /// Shift英語モード(shift_latin=compose): raw 内で direct 挿入部分が始まるバイト位置。
    /// Some=英語モード中。bool でなく位置を持つのはセッション喪失リプレイが
    /// かな部/英語部の style 分割に必要なため。合成終息(reset/reseed/raw 枯渇)で None。
    pub latin_from: Option<usize>,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    StartOrUpdatePreedit(String), // preeditに表示すべき文字列
    #[allow(dead_code)] // テスト専用: prod は OnKeyDown 内に同等処理をインライン化（テストモデル）
    RequestConvert, // 候補要求（Space）
    #[allow(dead_code)] // テスト専用: prod は OnKeyDown 内に同等処理をインライン化（テストモデル）
    Commit, // 確定（Enter）
    #[cfg(test)]
    Cancel,       // 取消（Esc）の参照モデル
    #[allow(dead_code)] // テスト専用: prod は start_llm_convert を直接呼ぶ（テストモデル）
    RequestLlmConvert, // 外部LLM変換要求（Tab）
    Pass,                         // IMEは関与しない
}

impl InputState {
    pub fn on_char(&mut self, ch: char) -> Action {
        self.raw.push(ch);
        self.composing = true;
        self.notation_fixed = None; // 新たな打鍵でライブ変換が再開する＝表記固定は解除
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
        if self.composing {
            Action::RequestConvert
        } else {
            Action::Pass
        }
    }
    #[allow(dead_code)] // テスト専用: prod の Enter 処理は OnKeyDown にインライン化（テストモデル）
    pub fn on_enter(&self) -> Action {
        if self.composing {
            Action::Commit
        } else {
            Action::Pass
        }
    }
    #[cfg(test)]
    pub fn on_escape(&mut self) -> Action {
        if self.composing {
            self.reset();
            Action::Cancel
        } else {
            Action::Pass
        }
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
            self.notation_fixed = None; // 読みが変わりライブ変換が再開する＝表記固定は解除
            Action::StartOrUpdatePreedit(self.raw.clone())
        } else {
            Action::Pass
        }
    }
    pub fn reset(&mut self) {
        self.raw.clear();
        self.composing = false;
        self.phase = Phase::Composing;
        self.notation_fixed = None;
        self.latin_from = None;
    }

    /// 巡10(round10): 空 Backspace の cancel 拒否時に呼ぶ — on_backspace が最終文字削除で
    /// composing=false に済ませたのを巻き戻す。raw は空のままだが、composing を保てば
    /// 再押下の Backspace/Esc が composing gate を通り cancel 再試行の経路へ戻る
    /// （戻さないと文書に composition が残るのに TIP は idle 扱いになり、閉じる手段が消える）。
    pub fn resume_composing_after_cancel_reject(&mut self) {
        self.composing = true;
    }

    /// 前方一致候補の部分確定後、残りの canonical text で composition を継続する状態に整える。
    /// `raw` を残りで満たすのは on_backspace の composing 判定をエンジン側の残りと
    /// **1:1 で同期**させるため（raw が空のままだと最初の Backspace で composing を取りこぼし、
    /// 2かな以上の残り読みが途中で打ち切られる＝データロス再発。defect#1）。raw が前面に出るのは
    /// エンジン応答失敗時の劣化フォールバックのみで、その場合も正しい残りを表示できる。
    pub fn reseed_after_partial_commit_with_latin(
        &mut self,
        remaining: &str,
        latin_from: Option<usize>,
    ) {
        self.raw = remaining.to_string();
        self.composing = true;
        self.phase = Phase::Composing;
        self.notation_fixed = None; // 残り読みのライブ変換が再開する（arm_debounce と対）
        self.latin_from = latin_from;
    }
    /// ライブ変換要求ごとに seq を1つ進めて返す（TIP 採番）。
    pub fn bump_live_seq(&mut self) -> u64 {
        self.live_seq += 1;
        self.live_seq
    }
    /// Tab: composition 中かつ Composing フェーズのときだけ LLM 変換を要求する。
    #[allow(dead_code)] // テスト専用: prod は VK_TAB→start_llm_convert を直接呼ぶ（テストモデル）
    pub fn on_tab(&self) -> Action {
        if self.composing && self.phase == Phase::Composing {
            Action::RequestLlmConvert
        } else {
            Action::Pass
        }
    }
    /// LLM 変換要求ごとに seq を1つ進める（世代ガード用・TIP 採番）。
    pub fn bump_llm_seq(&mut self) -> u64 {
        self.llm_seq += 1;
        self.llm_seq
    }
    pub fn awaiting_llm(&self) -> bool {
        self.phase == Phase::AwaitingLlm
    }
    pub fn set_awaiting_llm(&mut self, on: bool) {
        self.phase = if on {
            Phase::AwaitingLlm
        } else {
            Phase::Composing
        };
    }
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
#[cfg(test)]
pub fn plan_commit(outcome: Option<(String, String)>, resolved_text: &str) -> CommitPlan {
    match outcome {
        Some((prefix, remaining)) if !remaining.is_empty() => {
            CommitPlan::PartialReseed { prefix, remaining }
        }
        _ => CommitPlan::FullReset {
            text: resolved_text.to_string(),
        },
    }
}

/// ライブ変換のエンジン往復を行ってよいか。表示（`restore_live_preedit`）と確定
/// （VK_RETURN / `settle_active_input`）の**両方**がこの単一述語を見る。false のとき各経路は
/// `live=None` を渡し、表示中の `live_text` をそのまま確定・描き戻す。
/// Why not(呼び出し側で live_enabled と notation_fixed を個別に見る): 3 経路が別々に条件を
/// 持つと、片方だけ更新したときに「見えている文字列と確定される文字列」がずれる
/// （`should_widen_digits` と同じ轍。0bdb0b9 の再発）。
/// Why not(`plan_live_enter` の中で live_enabled を見る): エンジン往復には副作用
/// （`bump_live_seq`・IPC）があるので、呼んだ後に結果を捨てるのでは止めたことにならない。
pub fn should_consult_live_engine(
    live_enabled: bool,
    notation_fixed: Option<crate::keymap::Notation>,
) -> bool {
    live_enabled && notation_fixed.is_none()
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
            let text = if !live_text.is_empty() {
                live_text.to_string()
            } else {
                last_reading.to_string()
            };
            LiveEnterPlan::DirectCommit { text }
        }
    }
}

/// 候補窓だけを閉じて composition を残す経路（Esc / Behavior::Abort）で preedit へ描き戻す文字列。
/// Why not(`live` を取らず `plan_live_enter(None, ..)` の劣化枝だけを使う): ライブ変換 ON で
/// 「変換キーが `arm_debounce` の 30ms 以内に来て `disarm_debounce` された」場合、`live_text` は
/// 読みのまま残るのに閉じた後の Enter / settle は `engine_live_convert` を試す。劣化枝だけで
/// 描き戻すと「かなが見えているのに漢字が確定される」ズレになる。確定側と同じ `live`
/// （＝同じ `should_consult_live_engine` で決めたもの）を受けて同じ分岐を通す。
/// Why not(空文字を返す): `run_preedit` を空文字で呼ぶと composition が空になり、直後の Enter が
/// 何も確定しない見え方になる。素材が無いときは None＝preedit をそのままにする。
pub fn preedit_after_candidates_closed(
    live: Option<String>,
    live_text: &str,
    last_reading: &str,
) -> Option<String> {
    let text = match plan_live_enter(live, live_text, last_reading) {
        LiveEnterPlan::EngineCommit { text } | LiveEnterPlan::DirectCommit { text } => text,
    };
    Some(text).filter(|t| !t.is_empty())
}

/// commit対象文字列を決める純関数（テスト可能）。エンジン失敗時は読みのまま確定する劣化動作。
#[allow(dead_code)] // テスト専用: prod は OnKeyDown に同等の select-or-fallback をインライン化（参照モデル）
pub fn commit_text(
    convert_result: Result<Vec<String>, ()>,
    selected: usize,
    fallback_reading: &str,
) -> String {
    match convert_result {
        Ok(cands) if !cands.is_empty() => cands
            .get(selected)
            .cloned()
            .unwrap_or_else(|| cands[0].clone()),
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
    if clamped.is_empty() {
        None
    } else {
        Some(clamped)
    }
}

// ---- 打鍵作法バンドル: 表記変換の純関数（Task 1）----
// 記号の写像（zenkaku_symbol / zenkaku_of）は settings::symbol へ移設した（2026-08-02 spec §2）:
// 設定 UI が29記号の変換先プレビューを出すのに写像が要り、config → tip 依存は方向が誤りのため。

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
    s.chars()
        .map(|c| match c as u32 {
            0x3041..=0x3096 => char::from_u32(c as u32 + 0x60).unwrap_or(c),
            _ => c,
        })
        .collect()
}

/// ひらがな→半角カナ（U+FF61-FF9F）。濁点/半濁点は「基底半角カナ+ﾞ/ﾟ」の2単位へ分解する固定表。
/// 表に無い文字（漢字/英数/記号）は素通し。
pub fn to_hankaku_kana(s: &str) -> String {
    // 五十音全段＋濁音＋半濁音＋小書き＋ん/長音符＋IME 句読点。ゐ/ゑ は半角カナに無いので素通し。
    const TABLE: &[(char, &str)] = &[
        ('あ', "ｱ"),
        ('い', "ｲ"),
        ('う', "ｳ"),
        ('え', "ｴ"),
        ('お', "ｵ"),
        ('か', "ｶ"),
        ('き', "ｷ"),
        ('く', "ｸ"),
        ('け', "ｹ"),
        ('こ', "ｺ"),
        ('さ', "ｻ"),
        ('し', "ｼ"),
        ('す', "ｽ"),
        ('せ', "ｾ"),
        ('そ', "ｿ"),
        ('た', "ﾀ"),
        ('ち', "ﾁ"),
        ('つ', "ﾂ"),
        ('て', "ﾃ"),
        ('と', "ﾄ"),
        ('な', "ﾅ"),
        ('に', "ﾆ"),
        ('ぬ', "ﾇ"),
        ('ね', "ﾈ"),
        ('の', "ﾉ"),
        ('は', "ﾊ"),
        ('ひ', "ﾋ"),
        ('ふ', "ﾌ"),
        ('へ', "ﾍ"),
        ('ほ', "ﾎ"),
        ('ま', "ﾏ"),
        ('み', "ﾐ"),
        ('む', "ﾑ"),
        ('め', "ﾒ"),
        ('も', "ﾓ"),
        ('や', "ﾔ"),
        ('ゆ', "ﾕ"),
        ('よ', "ﾖ"),
        ('ら', "ﾗ"),
        ('り', "ﾘ"),
        ('る', "ﾙ"),
        ('れ', "ﾚ"),
        ('ろ', "ﾛ"),
        ('わ', "ﾜ"),
        ('を', "ｦ"),
        ('ん', "ﾝ"),
        ('が', "ｶﾞ"),
        ('ぎ', "ｷﾞ"),
        ('ぐ', "ｸﾞ"),
        ('げ', "ｹﾞ"),
        ('ご', "ｺﾞ"),
        ('ざ', "ｻﾞ"),
        ('じ', "ｼﾞ"),
        ('ず', "ｽﾞ"),
        ('ぜ', "ｾﾞ"),
        ('ぞ', "ｿﾞ"),
        ('だ', "ﾀﾞ"),
        ('ぢ', "ﾁﾞ"),
        ('づ', "ﾂﾞ"),
        ('で', "ﾃﾞ"),
        ('ど', "ﾄﾞ"),
        ('ば', "ﾊﾞ"),
        ('び', "ﾋﾞ"),
        ('ぶ', "ﾌﾞ"),
        ('べ', "ﾍﾞ"),
        ('ぼ', "ﾎﾞ"),
        ('ぱ', "ﾊﾟ"),
        ('ぴ', "ﾋﾟ"),
        ('ぷ', "ﾌﾟ"),
        ('ぺ', "ﾍﾟ"),
        ('ぽ', "ﾎﾟ"),
        ('ゔ', "ｳﾞ"),
        ('ぁ', "ｧ"),
        ('ぃ', "ｨ"),
        ('ぅ', "ｩ"),
        ('ぇ', "ｪ"),
        ('ぉ', "ｫ"),
        ('ゃ', "ｬ"),
        ('ゅ', "ｭ"),
        ('ょ', "ｮ"),
        ('っ', "ｯ"),
        ('ー', "ｰ"),
        ('。', "｡"),
        ('、', "､"),
        ('「', "｢"),
        ('」', "｣"),
        ('・', "･"),
    ];
    s.chars()
        .map(|c| {
            TABLE
                .iter()
                .find(|(k, _)| *k == c)
                .map(|(_, v)| (*v).to_string())
                .unwrap_or_else(|| c.to_string())
        })
        .collect()
}

/// ASCII 印字可能文字（0x21-0x7E）→全角（U+FF01-FF5E）。空白は U+3000。非 ASCII は素通し。
/// `zenkaku_of` は範囲ガードを持たない機械式なので、範囲アームの**中でだけ**呼ぶ
/// （全域に適用すると全角英数表記で日本語が壊れる — spec §2 MI-c）。
pub fn to_zenkaku_ascii(s: &str) -> String {
    s.chars()
        .map(|c| match c as u32 {
            0x20 => '\u{3000}',
            0x21..=0x7E => settings::symbol::zenkaku_of(c),
            _ => c,
        })
        .collect()
}

/// 全角文字を打鍵の半角 ASCII へ戻す。F10（半角英数）専用: 句読点/記号は合成へ入る時点で
/// `zenkaku_symbol` により全角へ畳み込まれて raw に積まれるため、素の raw では「、。」等が
/// 全角のまま残る（F8 の to_hankaku_kana だけが句読点表を持つ非対称の実装漏れ — 実機報告
/// 2026-08-03）。U+FF01-FF5E は機械逆写像、U+3000 は空白、それ以外は zenkaku_symbol を
/// 全トグル ON・全集合で走査して逆引きする（ー→- 、→, 。→. ・→/ 「→[ 」→]）。
/// Why not（個別の逆引き表）: 表を二重に持つと表の穴を再生産する（2026-07-16 spec §2 と同じ
/// 理由）。全トグル ON で引くのは「畳み込まれ得た文字を全て戻す」ため — トグル OFF なら
/// raw は元々半角でこの走査に当たらない。部分確定 reseed 後の raw はかな（M-2 の既知の限界）
/// でかなは素通しだが、ー だけ '-' へ落ちる — raw が打鍵でない時点で表示は既に劣化して
/// おり許容。
pub fn to_hankaku_ascii(s: &str) -> String {
    use settings::symbol::{zenkaku_symbol, SymbolCharSet};
    s.chars()
        .map(|c| match c as u32 {
            0x3000 => ' ',
            0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFF01 + 0x21).unwrap_or(c),
            _ => (0x21u32..=0x7E)
                .filter_map(char::from_u32)
                .find(|&h| zenkaku_symbol(h, true, true, SymbolCharSet::ALL) == Some(c))
                .unwrap_or(c),
        })
        .collect()
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
/// preedit 表示側と確定側の**両方**がこの単一述語を見る（設計 2026-07-09 §4「表示（preedit）整合」）。
/// Why not(確定側だけで判定する): 表示と確定が別々に幅を決めると「打っている間は半角なのに
/// 確定したら全角」になる。実際 preedit 側の実装が落ちていて、その不一致が報告された。
pub fn should_widen_digits(
    number_full_width: bool,
    direct: bool,
    latin: bool,
    notation_fixed: Option<crate::keymap::Notation>,
    source: &str,
) -> bool {
    // 表記固定のうち半角側（F10 半角英数 / 半角カナ）だけが数字幅の指定を兼ねる。
    // Why not(notation_fixed.is_some() で一律に止める): F6/F7 は かな の表記を変えるだけなので、
    // カタカナにした途端に数字が半角へ戻る＝設定「数字は全角」を無関係な操作が覆すことになる。
    let halfwidth_notation = matches!(
        notation_fixed,
        Some(crate::keymap::Notation::HankakuEisu | crate::keymap::Notation::HankakuKana)
    );
    number_full_width
        && !direct
        && !latin
        && !halfwidth_notation
        && !matches!(source, "candidate" | "candidate_prefix" | "clause")
}

/// 文節ナビゲーション: 文節ビューの選択文節を preedit（UTF-16）上の区間へ写す純関数。
/// 戻り値は (開始, 長さ)。TSF の ITfRange::ShiftStart/ShiftEnd は UTF-16 コード単位で数える
/// ため、Rust の文字数（char）でなく encode_utf16 の長さで合算する（サロゲートペアの絵文字/
/// 拡張漢字で下線位置がずれないように）。selected が範囲外なら長さ 0（ハイライト無し）。
pub fn clause_target_utf16(segments: &[String], selected: usize) -> (usize, usize) {
    let start: usize = segments
        .iter()
        .take(selected)
        .map(|s| s.encode_utf16().count())
        .sum();
    let len = segments
        .get(selected)
        .map(|s| s.encode_utf16().count())
        .unwrap_or(0);
    (start, len)
}

/// エンジン Insert の挿入解釈(IPC style フィールドの TIP 内表現)。
/// Kana=roman2kana(従来・ワイヤ上は style 省略) / Direct=リテラル(Shift英語モード)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InsertStyle {
    Kana,
    Direct,
}

/// 再変換の対象種別。`None`=対象なし / `Latin`=ローマ字リプレイ / `Surface`=かな表層を
/// エンジンへ .direct で渡す / `NonKana`=漢字・混在等（再変換せず無害に離脱）。
#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum ReconvertKind {
    #[default]
    None,
    Latin,
    Surface,
    NonKana,
}

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
    if s.is_empty() {
        return ReconvertKind::None;
    }
    if s.chars().all(|c| c.is_ascii_alphabetic() || c == '-') {
        return ReconvertKind::Latin;
    }
    if s.chars().all(is_kana) {
        return ReconvertKind::Surface;
    }
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
    fn resume_after_cancel_reject_keeps_retriable_state() {
        // 巡10(round10): 空 Backspace の cancel 拒否後の中間状態 — composing=true・raw 空。
        // 再 Backspace は on_backspace の空 pop で panic せず composing=false に落ち
        // （呼び出し側の do_cancel 再試行へ合流）、latin_from も None のまま漏れない。
        // 巡12(round12): latin_from の非漏出は英語モード経由(on_char_latin)で実測する —
        // 最終文字削除でモード解除済みの状態を resume が復活させないことの固定。
        let mut s = InputState::default();
        s.on_char('あ');
        assert_eq!(s.on_backspace(), Action::StartOrUpdatePreedit("".into()));
        assert!(!s.composing);
        s.resume_composing_after_cancel_reject();
        assert!(s.composing);
        assert!(s.raw.is_empty());
        assert_eq!(s.on_backspace(), Action::StartOrUpdatePreedit("".into()));
        assert!(!s.composing);

        let mut latin = InputState::default();
        let _ = latin.on_char_latin('A');
        assert!(latin.latin_from.is_some());
        let _ = latin.on_backspace(); // 英字を削り切る → latin_from は None に解除
        assert!(latin.latin_from.is_none());
        latin.resume_composing_after_cancel_reject();
        assert!(latin.latin_from.is_none()); // resume は composing だけ戻し、モードは復活させない
        assert!(!latin.latin_mode());
    }
    #[test]
    fn resume_after_cancel_reject_keeps_escape_cancel() {
        // 巻き戻し中間状態では Esc(on_escape)が Cancel を返す — Esc 再押下の cancel
        // 再試行経路が生きていることの状態機械レベルの固定。
        let mut s = InputState::default();
        s.on_char('あ');
        let _ = s.on_backspace();
        s.resume_composing_after_cancel_reject();
        assert_eq!(s.on_escape(), Action::Cancel);
    }
    #[test]
    fn commit_falls_back_to_reading_on_engine_error() {
        assert_eq!(commit_text(Err(()), 0, "にほんご"), "にほんご");
    }
    #[test]
    fn commit_uses_selected_candidate() {
        assert_eq!(
            commit_text(Ok(vec!["日本語".into(), "にほんご".into()]), 0, "にほんご"),
            "日本語"
        );
    }
    #[test]
    fn live_seq_is_monotonic() {
        let mut s = InputState::default();
        assert_eq!(s.bump_live_seq(), 1);
        assert_eq!(s.bump_live_seq(), 2);
    }
    #[test]
    fn fresh_live_only_for_matching_seq() {
        assert!(is_fresh_live(5, 5)); // 最新 seq の応答だけ採用
        assert!(!is_fresh_live(4, 5)); // 古い応答は捨てる（A2 用）
    }

    #[test]
    fn latin_run_span_basic() {
        assert_eq!(latin_run_span("nihongo"), 7); // 全部ラテン
        assert_eq!(latin_run_span("React nihongo"), 7); // 直前スペースで停止 → "nihongo"
        assert_eq!(latin_run_span(""), 0); // 空
        assert_eq!(latin_run_span("abc "), 0); // 末尾が空白 → 0
        assert_eq!(latin_run_span("abc123"), 0); // 末尾が数字 → 0
        assert_eq!(latin_run_span("日本語go"), 2); // 非ASCIIで停止 → "go"
        assert_eq!(latin_run_span("a.b"), 1); // 句読点で停止 → "b"
    }

    #[test]
    fn latin_run_span_spans_hyphen() {
        // ローマ字の長音を表す半角ハイフン `-` は境界にしない（`wa-rudo`→ワールド 用）。
        // これが無いと後方スキャンが `-` で止まり `rudo` だけを掴む（本バグ）。
        assert_eq!(latin_run_span("wa-rudo"), 7); // ハイフンを跨いで全体を掴む
        assert_eq!(latin_run_span("React wa-rudo"), 7); // 直前スペースで停止 → "wa-rudo"
                                                        // 他の境界は不変（`-` だけを許可、他記号・空白・数字・非ASCIIは依然境界）。
        assert_eq!(latin_run_span("a.b"), 1); // 句読点は依然境界
        assert_eq!(latin_run_span("abc "), 0); // 末尾空白は依然 0
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
        assert_eq!(classify_reconvert_selection("nihongo"), Latin); // 純ASCII英字
        assert_eq!(classify_reconvert_selection("にほんご"), Surface); // ひらがな
        assert_eq!(classify_reconvert_selection("ニホンゴ"), Surface); // カタカナ
        assert_eq!(classify_reconvert_selection("ラーメン"), Surface); // 長音符込みカタカナ
        assert_eq!(classify_reconvert_selection("日本語"), NonKana); // 漢字
        assert_eq!(classify_reconvert_selection("日本ご"), NonKana); // 漢字+かな混在
        assert_eq!(classify_reconvert_selection("にほん go"), NonKana); // かな+ラテン混在(空白含む)
        assert_eq!(classify_reconvert_selection("abc123"), NonKana); // 英字+数字
        assert_eq!(classify_reconvert_selection(""), None); // 空
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
        assert_eq!(
            plan,
            CommitPlan::PartialReseed {
                prefix: "日本".into(),
                remaining: "ご".into()
            }
        );
    }

    #[test]
    fn plan_commit_full_when_no_remaining() {
        // 残り読みが空（全消費）→ 従来どおりの全確定（resolved_text を確定）。
        let plan = plan_commit(Some(("日本語".into(), "".into())), "日本語");
        assert_eq!(
            plan,
            CommitPlan::FullReset {
                text: "日本語".into()
            }
        );
    }

    #[test]
    fn plan_commit_full_on_engine_failure_uses_resolved_text() {
        // エンジン失敗(None)→ TIP 解決済み文字列で全確定（劣化＝従来挙動・バイト等価）。
        let plan = plan_commit(None, "にほんご");
        assert_eq!(
            plan,
            CommitPlan::FullReset {
                text: "にほんご".into()
            }
        );
    }

    // ---- ライブ確定（候補窓なし Enter）の engine Commit(0) 合流（Spec2） ----

    #[test]
    fn plan_live_enter_prefers_engine_result() {
        // エンジンのライブ変換が生きていれば Commit(0) 経由（学習に乗せる）。
        let p = plan_live_enter(Some("日本語".into()), "にほんご", "nihongo");
        assert_eq!(
            p,
            LiveEnterPlan::EngineCommit {
                text: "日本語".into()
            }
        );
    }
    #[test]
    fn plan_live_enter_degrades_to_live_text() {
        // エンジン劣化(None): 表示中のライブ文字列で直確定（従来挙動・学習なし）。
        let p = plan_live_enter(None, "にほんご", "nihongo");
        assert_eq!(
            p,
            LiveEnterPlan::DirectCommit {
                text: "にほんご".into()
            }
        );
    }
    #[test]
    fn plan_live_enter_falls_back_to_reading() {
        // ライブ文字列も空: 読みで直確定（従来の unwrap 連鎖の最終段と同値）。
        let p = plan_live_enter(None, "", "nihongo");
        assert_eq!(
            p,
            LiveEnterPlan::DirectCommit {
                text: "nihongo".into()
            }
        );
    }
    #[test]
    fn plan_live_enter_all_empty_commits_empty() {
        // 巡12(round12): 空 BS cancel 拒否巻き戻し中の Enter は live/live_text/reading すべて
        // 空 — DirectCommit{""} になり、呼び出し側の空確定(SetText 成功時に限り composition
        // を畳る cancel 代わり・拒否時は中間状態のまま再試行)に合流することのピン止め。
        let p = plan_live_enter(None, "", "");
        assert_eq!(p, LiveEnterPlan::DirectCommit { text: "".into() });
    }
    #[test]
    fn plan_live_enter_empty_engine_result_degrades() {
        // エンジンが空文字を返したら劣化扱い（従来の .filter(!empty) と同値）。
        let p = plan_live_enter(Some(String::new()), "あ", "a");
        assert_eq!(p, LiveEnterPlan::DirectCommit { text: "あ".into() });
    }

    // ---- ライブ変換 OFF: 見えている読みがそのまま確定される（設定 OFF は OFF を意味する） ----

    #[test]
    fn live_conversion_off_never_consults_the_engine() {
        assert!(should_consult_live_engine(true, None));
        assert!(!should_consult_live_engine(false, None));
    }
    #[test]
    fn notation_fixed_never_consults_the_engine_even_with_live_conversion_on() {
        use crate::keymap::Notation;
        assert!(!should_consult_live_engine(true, Some(Notation::Katakana)));
        assert!(!should_consult_live_engine(false, Some(Notation::Katakana)));
    }
    /// 3 経路（VK_RETURN / settle / restore_live_preedit）が組み立てる `live` 素材を、述語を実際に
    /// 通して再現するヘルパ。`None` を直に渡すとテストが「OFF なら live=None」を前提にしてしまい、
    /// 述語が壊れても通る。
    fn live_material(
        live_enabled: bool,
        notation_fixed: Option<crate::keymap::Notation>,
        engine_would_say: &str,
    ) -> Option<String> {
        if should_consult_live_engine(live_enabled, notation_fixed) {
            Some(engine_would_say.into())
        } else {
            None
        }
    }

    #[test]
    fn live_conversion_off_commits_the_reading_shown_in_the_preedit() {
        // OFF ではデバウンスが走らないので live_text は読みのまま＝画面に見えている文字列。
        let p = plan_live_enter(live_material(false, None, "日本語"), "にほんご", "にほんご");
        assert_eq!(
            p,
            LiveEnterPlan::DirectCommit {
                text: "にほんご".into()
            }
        );
    }
    #[test]
    fn live_conversion_off_shows_after_esc_exactly_what_enter_then_commits() {
        // 表示側と確定側に同じ述語で作った素材を渡す＝候補窓を閉じた後の見た目と確定が一致する。
        let lt = "にほんご";
        let shown =
            preedit_after_candidates_closed(live_material(false, None, "日本語"), lt, "にほんご");
        let committed = match plan_live_enter(live_material(false, None, "日本語"), lt, "にほんご")
        {
            LiveEnterPlan::EngineCommit { text } | LiveEnterPlan::DirectCommit { text } => text,
        };
        assert_eq!(shown, Some(committed));
        assert_eq!(shown, Some("にほんご".into()));
    }
    #[test]
    fn fixed_notation_commits_the_displayed_katakana_whether_live_conversion_is_on_or_off() {
        use crate::keymap::Notation;
        // F7 のカタカナは live_text に載る＝ON/OFF どちらでも表記固定の見た目がそのまま確定される。
        for on in [true, false] {
            let live = live_material(on, Some(Notation::Katakana), "日本語");
            assert_eq!(
                plan_live_enter(live, "ニホンゴ", "にほんご"),
                LiveEnterPlan::DirectCommit {
                    text: "ニホンゴ".into()
                }
            );
        }
    }
    #[test]
    fn live_conversion_on_still_commits_the_engine_result() {
        let p = plan_live_enter(live_material(true, None, "日本語"), "にほんご", "にほんご");
        assert_eq!(
            p,
            LiveEnterPlan::EngineCommit {
                text: "日本語".into()
            }
        );
    }
    #[test]
    fn live_conversion_on_shows_after_esc_exactly_what_enter_then_commits() {
        let lt = "にほんご";
        let shown =
            preedit_after_candidates_closed(live_material(true, None, "日本語"), lt, "にほんご");
        let committed = match plan_live_enter(live_material(true, None, "日本語"), lt, "にほんご")
        {
            LiveEnterPlan::EngineCommit { text } | LiveEnterPlan::DirectCommit { text } => text,
        };
        assert_eq!(shown, Some(committed));
        assert_eq!(shown, Some("日本語".into()));
    }

    #[test]
    fn closing_candidates_restores_the_preedit_to_what_enter_then_commits() {
        // 閉じた後の Enter はエンジンのライブ変換結果を確定するので、描き戻しもそれになる。
        assert_eq!(
            preedit_after_candidates_closed(Some("日本語".into()), "にほんご", "にほんご"),
            Some("日本語".into())
        );
    }
    #[test]
    fn closing_candidates_prefers_the_engine_result_over_the_stale_live_text() {
        // デバウンス未発火では live_text は読みのまま残る。それを描き戻すと
        // 「かなが見えているのに漢字が確定される」ので、確定側と同じくエンジン結果を優先する。
        assert_eq!(
            preedit_after_candidates_closed(Some("日本".into()), "にほん", "にほん"),
            Some("日本".into())
        );
    }
    #[test]
    fn closing_candidates_falls_back_to_the_live_text_then_the_reading_when_the_engine_is_dead() {
        assert_eq!(
            preedit_after_candidates_closed(None, "日本語", "にほんご"),
            Some("日本語".into())
        );
        assert_eq!(
            preedit_after_candidates_closed(None, "", "にほんご"),
            Some("にほんご".into())
        );
        assert_eq!(
            preedit_after_candidates_closed(Some(String::new()), "", "にほんご"),
            Some("にほんご".into())
        );
    }
    #[test]
    fn closing_candidates_leaves_the_preedit_alone_when_there_is_no_material() {
        assert_eq!(preedit_after_candidates_closed(None, "", ""), None);
    }

    #[test]
    fn reseed_keeps_composing_until_remaining_exhausted() {
        // 部分確定後、残り読み(2かな)で reseed。on_backspace は残り読みと 1:1 で縮み、
        // 最後の1かなを消すまで composing を維持する（defect#1 回帰: composing 早期 false を防ぐ）。
        let mut s = InputState::default();
        s.reseed_after_partial_commit_with_latin("ほご", None); // 2 かな
        assert!(s.composing);
        assert_eq!(s.raw, "ほご");
        s.on_backspace(); // 1かな消す
        assert!(s.composing, "残り読みが残る間は composing を維持");
        s.on_backspace(); // 最後の1かな
        assert!(!s.composing, "残り読み枯渇で composing 解除");
    }

    #[test]
    fn reseed_single_kana_drops_composing_on_one_backspace() {
        let mut s = InputState::default();
        s.reseed_after_partial_commit_with_latin("ご", None); // 1 かな
        assert!(s.composing);
        s.on_backspace();
        assert!(!s.composing);
    }

    // ---- U9: sanitize_left_context ----

    #[test]
    fn left_context_keeps_text_after_last_newline() {
        assert_eq!(sanitize_left_context("a\nbc"), Some("bc".into()));
        assert_eq!(sanitize_left_context("a\r\nbc"), Some("bc".into()));
        assert_eq!(
            sanitize_left_context("一行目\n二行目\n私の名前は"),
            Some("私の名前は".into())
        );
    }

    #[test]
    fn left_context_cuts_at_embedded_object_and_line_separators() {
        // U+FFFC(TS_CHAR_EMBEDDED)・U+2028/U+2029(Zl/Zp — Cc でも \r\n でもない)は区切り。
        assert_eq!(
            sanitize_left_context("画像\u{FFFC}のあと"),
            Some("のあと".into())
        );
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
        assert_eq!(
            sanitize_left_context("\u{FFFD}こんにちは"),
            Some("こんにちは".into())
        );
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
        assert_eq!(
            sanitize_left_context("私の名前は"),
            Some("私の名前は".into())
        );
    }

    // ---- 打鍵作法バンドル: 表記変換の純関数 ----

    #[test]
    fn to_kana_reading_char_maps_prolonged_sound() {
        assert_eq!(to_kana_reading_char('-'), 'ー'); // 長音符（nospacekey roman2kana が欠く）
        assert_eq!(to_kana_reading_char('a'), 'a'); // 英字は不変
        assert_eq!(to_kana_reading_char('1'), '1'); // 数字は不変
        assert_eq!(to_kana_reading_char('.'), '.'); // 他記号は engine に委ねる（不変）
    }

    #[test]
    fn to_katakana_shifts_hiragana_block_only() {
        assert_eq!(to_katakana("にほんご"), "ニホンゴ");
        assert_eq!(to_katakana("きょうー"), "キョウー"); // 長音符は共通（シフト対象外）
        assert_eq!(to_katakana("あa1"), "アa1"); // 非かなは素通し
    }

    #[test]
    fn to_hankaku_kana_handles_dakuten() {
        assert_eq!(to_hankaku_kana("がぱ"), "ｶﾞﾊﾟ"); // 濁点/半濁点は2単位へ分解
        assert_eq!(to_hankaku_kana("にほんご"), "ﾆﾎﾝｺﾞ");
        assert_eq!(to_hankaku_kana("きょう"), "ｷｮｳ"); // 小書きかな
    }

    #[test]
    fn to_zenkaku_ascii_maps_alnum_and_symbols() {
        assert_eq!(to_zenkaku_ascii("abC1!"), "ａｂＣ１！");
        assert_eq!(to_zenkaku_ascii("あ"), "あ"); // 非ASCIIは素通し
    }

    #[test]
    fn to_hankaku_ascii_reverses_folded_punctuation_to_keystrokes() {
        // F10 の核: 合成時に畳み込まれた全角句読点が打鍵の半角へ戻る（実機報告 2026-08-03）。
        assert_eq!(to_hankaku_ascii("、。"), ",.");
        assert_eq!(to_hankaku_ascii("a、。"), "a,.");
    }

    #[test]
    fn to_hankaku_ascii_reverses_symbol_folds_and_prolonged_sound() {
        assert_eq!(to_hankaku_ascii("・「」"), "/[]"); // 置換3件（Mozc symbol_method 相当）の逆
        assert_eq!(to_hankaku_ascii("waーdo"), "wa-do"); // `-` 打鍵の長音符化の逆
        assert_eq!(to_hankaku_ascii("！？～"), "!?~"); // 機械写像域（U+FF01-FF5E）の逆
        assert_eq!(to_hankaku_ascii("ＡＢ１２\u{3000}"), "AB12 ");
    }

    #[test]
    fn to_hankaku_ascii_passes_kana_through() {
        // 部分確定 reseed 後の raw はかな（M-2）— 変換対象外の文字は壊さない。
        assert_eq!(to_hankaku_ascii("にほんご"), "にほんご");
    }

    #[test]
    fn to_hankaku_ascii_roundtrips_to_zenkaku_ascii() {
        let src = "abc123!?[]/,.-";
        assert_eq!(to_hankaku_ascii(&to_zenkaku_ascii(src)), src);
    }

    #[test]
    fn f9_pipeline_widens_folded_punctuation_to_zenkaku_ascii() {
        // F9(全角英数)の素材整形: 畳み込み済み raw を半角へ戻してから機械全角化すると
        // 「、。」が「，．」になる(F10 と同じ非対称の鏡像 — 敵対レビュー M-1)。
        assert_eq!(to_zenkaku_ascii(&to_hankaku_ascii("a、。")), "ａ，．");
        assert_eq!(to_zenkaku_ascii(&to_hankaku_ascii("waーdo")), "ｗａ－ｄｏ");
    }

    #[test]
    fn to_zenkaku_digits_maps_only_digits() {
        assert_eq!(to_zenkaku_digits("123"), "１２３");
        assert_eq!(to_zenkaku_digits("2024年"), "２０２４年"); // 漢字は不変
        assert_eq!(to_zenkaku_digits("a-b"), "a-b"); // 英字/記号は不変
        assert_eq!(to_zenkaku_digits("こーひー"), "こーひー"); // かなは不変
    }

    #[test]
    fn should_widen_digits_only_on_default_native_commits() {
        // 引数: (number_full_width, direct, latin, notation_fixed, source)
        // 全角ON・native・既定確定 → 全角化
        assert!(should_widen_digits(true, false, false, None, "live"));
        assert!(should_widen_digits(true, false, false, None, "live_prefix"));
        // 候補の明示選択は幅を変えない（文節ナビゲーション確定も候補選択の一種）
        assert!(!should_widen_digits(true, false, false, None, "candidate"));
        assert!(!should_widen_digits(
            true,
            false,
            false,
            None,
            "candidate_prefix"
        ));
        assert!(!should_widen_digits(true, false, false, None, "clause"));
        // settle 系（mode_toggle/navigate）は読みを確定するので既定確定＝全角化（候補選択のみ不変）。
        assert!(should_widen_digits(true, false, false, None, "mode_toggle"));
        assert!(should_widen_digits(true, false, false, None, "navigate"));
        // 半角設定 OFF は変えない
        assert!(!should_widen_digits(false, false, false, None, "live"));
        // direct モードは変えない
        assert!(!should_widen_digits(true, true, false, None, "live"));
    }

    // ---- 文節ナビゲーション: 選択文節 → UTF-16 区間 ----

    #[test]
    fn clause_target_maps_selected_segment_to_utf16_span() {
        let segs = vec!["今日は".to_string(), "いい天気です".to_string()];
        assert_eq!(clause_target_utf16(&segs, 0), (0, 3));
        assert_eq!(clause_target_utf16(&segs, 1), (3, 6));
    }

    #[test]
    fn clause_target_counts_utf16_units_not_chars() {
        // サロゲートペア（𩸽 = U+29E3D は UTF-16 で 2 単位）を含む文節でも下線区間がずれない。
        let segs = vec!["𩸽".to_string(), "定食".to_string()];
        assert_eq!(clause_target_utf16(&segs, 0), (0, 2));
        assert_eq!(clause_target_utf16(&segs, 1), (2, 2));
    }

    #[test]
    fn clause_target_out_of_range_is_zero_length() {
        let segs = vec!["今日は".to_string()];
        assert_eq!(clause_target_utf16(&segs, 5), (3, 0)); // 範囲外はハイライト無し
        assert_eq!(clause_target_utf16(&[], 0), (0, 0));
    }

    #[test]
    fn latin_mode_keeps_digits_halfwidth() {
        // 英語モード(Shift+英字)は生 ASCII を継ぎ足す＝「iPhone7」の 7 は半角のまま。
        // 入力側がそう約束している以上、確定側が全角へ書き換えてはならない。
        assert!(!should_widen_digits(
            true, false, /*latin=*/ true, None, "live"
        ));
        assert!(!should_widen_digits(true, false, true, None, "mode_toggle"));
    }

    #[test]
    fn halfwidth_notation_keeps_digits_halfwidth() {
        use crate::keymap::Notation;
        // F10「半角英数」/半角カナで表記を固定した確定を全角へ書き換えるのは機能名と真逆。
        assert!(!should_widen_digits(
            true,
            false,
            false,
            Some(Notation::HankakuEisu),
            "live"
        ));
        assert!(!should_widen_digits(
            true,
            false,
            false,
            Some(Notation::HankakuKana),
            "navigate"
        ));
    }

    #[test]
    fn fullwidth_and_kana_notations_still_widen_digits() {
        use crate::keymap::Notation;
        // F6/F7（ひらがな/カタカナ）は かな の表記を変えるだけで数字幅の指定ではない。
        // ここまで半角へ倒すと「カタカナにしたら数字だけ半角に戻った」になる。
        assert!(should_widen_digits(
            true,
            false,
            false,
            Some(Notation::Katakana),
            "live"
        ));
        assert!(should_widen_digits(
            true,
            false,
            false,
            Some(Notation::Hiragana),
            "live"
        ));
        // 全角英数は to_zenkaku_ascii が数字も全角化済み＝widen は no-op だが、判定としては全角側。
        assert!(should_widen_digits(
            true,
            false,
            false,
            Some(Notation::ZenkakuEisu),
            "live"
        ));
    }

    // ---- 打鍵作法 Task4: F6-F10 の表記固定ラッチ ----

    #[test]
    fn notation_fixed_cleared_by_typing_backspace_and_reset() {
        use crate::keymap::Notation;
        let mut s = InputState::default();
        s.on_char('a');
        s.notation_fixed = Some(Notation::Katakana); // F7 等で表記固定(OnKeyDown 側が立てる)
        s.on_char('b');
        assert!(
            s.notation_fixed.is_none(),
            "新たな打鍵でライブ変換再開＝固定解除"
        );
        s.notation_fixed = Some(Notation::Katakana);
        s.on_backspace();
        assert!(
            s.notation_fixed.is_none(),
            "Backspace で読みが変わる＝固定解除"
        );
        s.notation_fixed = Some(Notation::HankakuKana);
        s.reset();
        assert!(s.notation_fixed.is_none(), "確定/取消の reset で固定解除");
        s.notation_fixed = Some(Notation::Hiragana);
        s.reseed_after_partial_commit_with_latin("ご", None);
        assert!(
            s.notation_fixed.is_none(),
            "部分確定の reseed で固定解除（残り読みはライブ変換再開）"
        );
    }

    // ---- Shift英語モード(shift_latin=compose): latin_from のライフサイクル ----

    #[test]
    fn latin_mode_starts_at_current_raw_position_and_persists() {
        let mut s = InputState::default();
        s.on_char('k');
        s.on_char('y');
        s.on_char('o');
        s.on_char('u');
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
        s.on_char('a');
        s.on_char('b');
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
        assert!(
            !s.latin_mode(),
            "raw 枯渇=合成終息でモード終了(Some(0) 残留は次の新規合成へ漏れる)"
        );
    }

    #[test]
    fn latin_mode_cleared_by_reset_and_reseed() {
        let mut s = InputState::default();
        s.on_char_latin('A');
        s.reset();
        assert!(!s.latin_mode(), "確定/取消の reset で解除");
        s.on_char_latin('A');
        s.reseed_after_partial_commit_with_latin("ご", None);
        assert!(!s.latin_mode(), "部分確定の残り読みはかな=英語モード解除");
    }
}
