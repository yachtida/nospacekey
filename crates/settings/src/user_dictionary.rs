//! カスタム辞書(Issue #3): エントリ型・エンジン同値のキー正規化・入力検証。
//! ここで作る文字列がエンジンの動的ユーザ辞書ルックアップキーと**厳密一致**する必要がある
//! (spec §3.2)。正のミラー元は engine-host 側の2箇所:
//! - ひらがな→カタカナ写像: `ConversionService.swift` の `toKatakana`(672-677行、
//!   U+3041..=U+3096 に +0x60)。
//! - 品詞分類: `UserDictionary.swift` の `cid(for:)`(43-58行)。
//!
//! 両ファイルを変更するときは必ずこちらも直す(逆も同様)。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserDictEntry {
    pub ruby: String,
    pub word: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pos: Option<String>,
}

/// Zs(空白区分)+ U+0009 のみを前後からトリムする。`char::is_whitespace` は使わない
/// (U+000A/U+000D 等の改行まで削ってしまい、「改行は残す」という仕様と食い違う)。
pub fn trim_ws(s: &str) -> &str {
    s.trim_matches(is_trim_ws_char)
}

fn is_trim_ws_char(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
    )
}

/// U+3099(濁点)/U+309A(半濁点)の NFD 表記を正準合成する。エンジンの動的ユーザ辞書は
/// ruby を完全一致(NFC/NFD 正規化なし)で照合するため、これを通さないと Google/MS-IME
/// エクスポートに紛れがちな NFD 表記(か+U+3099)がエンジン側の「が」に一致しない。
/// 合成表は本関数1つに閉じる(呼び出し側から表の存在を意識させない)。
pub fn compose_kana(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(base) = chars.next() {
        if let Some(&mark) = chars.peek() {
            if let Some(composed) = compose_pair(base, mark) {
                out.push(composed);
                chars.next();
                continue;
            }
        }
        out.push(base);
    }
    out
}

fn compose_pair(base: char, mark: char) -> Option<char> {
    Some(match (base, mark) {
        ('か', '\u{3099}') => 'が', ('き', '\u{3099}') => 'ぎ', ('く', '\u{3099}') => 'ぐ',
        ('け', '\u{3099}') => 'げ', ('こ', '\u{3099}') => 'ご',
        ('さ', '\u{3099}') => 'ざ', ('し', '\u{3099}') => 'じ', ('す', '\u{3099}') => 'ず',
        ('せ', '\u{3099}') => 'ぜ', ('そ', '\u{3099}') => 'ぞ',
        ('た', '\u{3099}') => 'だ', ('ち', '\u{3099}') => 'ぢ', ('つ', '\u{3099}') => 'づ',
        ('て', '\u{3099}') => 'で', ('と', '\u{3099}') => 'ど',
        ('は', '\u{3099}') => 'ば', ('ひ', '\u{3099}') => 'び', ('ふ', '\u{3099}') => 'ぶ',
        ('へ', '\u{3099}') => 'べ', ('ほ', '\u{3099}') => 'ぼ',
        ('う', '\u{3099}') => 'ゔ', ('ゝ', '\u{3099}') => 'ゞ',
        ('カ', '\u{3099}') => 'ガ', ('キ', '\u{3099}') => 'ギ', ('ク', '\u{3099}') => 'グ',
        ('ケ', '\u{3099}') => 'ゲ', ('コ', '\u{3099}') => 'ゴ',
        ('サ', '\u{3099}') => 'ザ', ('シ', '\u{3099}') => 'ジ', ('ス', '\u{3099}') => 'ズ',
        ('セ', '\u{3099}') => 'ゼ', ('ソ', '\u{3099}') => 'ゾ',
        ('タ', '\u{3099}') => 'ダ', ('チ', '\u{3099}') => 'ヂ', ('ツ', '\u{3099}') => 'ヅ',
        ('テ', '\u{3099}') => 'デ', ('ト', '\u{3099}') => 'ド',
        ('ハ', '\u{3099}') => 'バ', ('ヒ', '\u{3099}') => 'ビ', ('フ', '\u{3099}') => 'ブ',
        ('ヘ', '\u{3099}') => 'ベ', ('ホ', '\u{3099}') => 'ボ',
        ('ウ', '\u{3099}') => 'ヴ',
        ('ワ', '\u{3099}') => 'ヷ', ('ヰ', '\u{3099}') => 'ヸ', ('ヱ', '\u{3099}') => 'ヹ',
        ('ヲ', '\u{3099}') => 'ヺ', ('ヽ', '\u{3099}') => 'ヾ',
        ('は', '\u{309A}') => 'ぱ', ('ひ', '\u{309A}') => 'ぴ', ('ふ', '\u{309A}') => 'ぷ',
        ('へ', '\u{309A}') => 'ぺ', ('ほ', '\u{309A}') => 'ぽ',
        ('ハ', '\u{309A}') => 'パ', ('ヒ', '\u{309A}') => 'ピ', ('フ', '\u{309A}') => 'プ',
        ('ヘ', '\u{309A}') => 'ペ', ('ホ', '\u{309A}') => 'ポ',
        _ => return None,
    })
}

/// エンジンの動的ユーザ辞書ルックアップキー(ひらがな→カタカナ→正準合成)と厳密一致させる
/// 唯一の正規化点。写像→合成の順は固定(先に合成すると濁点付きひらがなの写像先を
/// 別途足す必要が生じ、表が二重化する)。
pub fn normalize_key(ruby: &str) -> String {
    compose_kana(&hiragana_to_katakana(trim_ws(ruby)))
}

/// `ConversionService.toKatakana` と同じレンジ(U+3041..=U+3096 に +0x60)。他は素通し。
fn hiragana_to_katakana(s: &str) -> String {
    s.chars()
        .map(|c| {
            if ('\u{3041}'..='\u{3096}').contains(&c) {
                char::from_u32(c as u32 + 0x60).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

pub fn normalize_word(word: &str) -> String {
    compose_kana(trim_ws(word))
}

pub fn entry_key(e: &UserDictEntry) -> (String, String) {
    (normalize_key(&e.ruby), normalize_word(&e.word))
}

#[derive(Debug, PartialEq)]
pub enum UserDictError {
    InvalidRuby,
    EmptyField,
    TooLong,
    ControlChar,
    Duplicate,
    NotFound,
    Unreadable,
}

/// 両フィールドとも `compose_kana(trim_ws(x))` した後の値で判定する(UI/保存前に
/// 正規化済みの形で長さ・文字種を見せるため — 末尾空白や NFD かなだけで弾かれない)。
pub fn validate_entry(ruby: &str, word: &str) -> Result<(), UserDictError> {
    let ruby_n = compose_kana(trim_ws(ruby));
    let word_n = compose_kana(trim_ws(word));
    check_generic(&ruby_n)?;
    if !ruby_n.chars().all(is_valid_ruby_char) {
        return Err(UserDictError::InvalidRuby);
    }
    check_generic(&word_n)?;
    Ok(())
}

/// 非空/スカラ300以下/制御文字(U+0000-U+001F)拒否。ruby/word 共通の下限チェック。
fn check_generic(s: &str) -> Result<(), UserDictError> {
    let count = s.chars().count();
    if count == 0 {
        return Err(UserDictError::EmptyField);
    }
    if count > 300 {
        return Err(UserDictError::TooLong);
    }
    if s.chars().any(|c| ('\u{0000}'..='\u{001F}').contains(&c)) {
        return Err(UserDictError::ControlChar);
    }
    Ok(())
}

fn is_valid_ruby_char(c: char) -> bool {
    matches!(c, '\u{3041}'..='\u{3096}' | '\u{30A1}'..='\u{30F6}' | '\u{30FC}')
}

/// Swift `UserDictionary.cid(for:)`(43-58行)の分岐順ミラー。変更時は両方直す。
pub fn canonical_pos(pos: Option<&str>) -> &'static str {
    let Some(p) = pos.filter(|p| !p.is_empty()) else {
        return "名詞";
    };
    if p.contains("人名") {
        if p.contains("姓") {
            return "姓";
        }
        // 「人名(名)」等 — 「人名」自身の「名」に反応しないよう除去してから判定。
        if p.replace("人名", "").contains("名") {
            return "名";
        }
        return "人名";
    }
    if p == "姓" {
        return "姓";
    }
    if p == "名" {
        return "名";
    }
    if p.contains("組織") {
        return "組織";
    }
    if p.contains("地名") || p.contains("駅") {
        return "地名";
    }
    if p.contains("固有") {
        return "固有名詞";
    }
    if p.contains("数") {
        return "数";
    }
    "名詞"
}

/// %LOCALAPPDATA%\nospacekey\user_dictionary.json。無ければ None（呼び元は空辞書で劣化）。
pub fn dict_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("nospacekey").join("user_dictionary.json"))
}

#[derive(Debug, PartialEq)]
pub enum DictCorrupt {
    None,
    Quarantined,
}

/// どちらも mutation 拒否（spec §3.2）: 読めない/隔離できない状態で辞書操作を許すと、
/// 空続行 → 後続 save が原本を上書きして恒久消失する経路になる。
#[derive(Debug, PartialEq)]
pub enum DictLoadError {
    Unreadable,
    QuarantineFailed,
}

#[derive(Debug)]
pub struct LoadedDict {
    pub entries: Vec<UserDictEntry>,
    pub deduped: usize,
    pub corrupt: DictCorrupt,
}

pub fn load_from(path: &Path) -> Result<LoadedDict, DictLoadError> {
    load_from_with(path, |f, t| std::fs::rename(f, t), |f, t| std::fs::copy(f, t))
}

/// 隔離操作（rename/copy）を注入可能にした実体。**別々の引数にする**（1クロージャに畳むと
/// copy フォールバック未実装でも rename 成功時のテストが全緑になり、フォールバック欠落を
/// 検出できない偽緑になる）。
pub(crate) fn load_from_with<R, C>(path: &Path, rename: R, copy: C) -> Result<LoadedDict, DictLoadError>
where
    R: Fn(&Path, &Path) -> std::io::Result<()>,
    C: Fn(&Path, &Path) -> std::io::Result<u64>,
{
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(empty_loaded()),
        // NotFound 以外（権限拒否・共有違反等）を空へ畳むと、一過性 I/O 失敗1回で
        // dict_add が「空+1件」を保存し既存全エントリを恒久消失させる。
        Err(_) => return Err(DictLoadError::Unreadable),
    };
    if bytes.is_empty() {
        return Ok(empty_loaded());
    }
    let text = match decode_bytes(&bytes) {
        Some(t) => t,
        None => return quarantine(path, rename, copy),
    };
    // torn write の痕跡（空白のみ）は破損でない。隔離すると無駄なクラッタを生むので、
    // settings.json の load_reporting と同じ方針で空へ劣化する（隔離しない）。
    if text.trim().is_empty() {
        return Ok(empty_loaded());
    }
    match serde_json::from_str::<Vec<UserDictEntry>>(&text) {
        Ok(entries) => {
            let (entries, deduped) = dedup_entries(entries);
            Ok(LoadedDict { entries, deduped, corrupt: DictCorrupt::None })
        }
        Err(_) => quarantine(path, rename, copy),
    }
}

fn empty_loaded() -> LoadedDict {
    LoadedDict { entries: Vec::new(), deduped: 0, corrupt: DictCorrupt::None }
}

/// `entry_key`（最初の出現を残す）で重複除去する。同一読み・別語は別キーとして両方残る。
fn dedup_entries(entries: Vec<UserDictEntry>) -> (Vec<UserDictEntry>, usize) {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(entries.len());
    let mut deduped = 0usize;
    for e in entries {
        if seen.insert(entry_key(&e)) {
            out.push(e);
        } else {
            deduped += 1;
        }
    }
    (out, deduped)
}

/// rename を試み、失敗したら copy にフォールバック。両方失敗したら原本に一切触れず
/// `QuarantineFailed` を返す（mutation 拒否 — 空続行すると次の save が原本を上書きして
/// 恒久消失する）。
fn quarantine<R, C>(path: &Path, rename: R, copy: C) -> Result<LoadedDict, DictLoadError>
where
    R: Fn(&Path, &Path) -> std::io::Result<()>,
    C: Fn(&Path, &Path) -> std::io::Result<u64>,
{
    let dest = quarantine_dest_path(path);
    if rename(path, &dest).is_ok() || copy(path, &dest).is_ok() {
        return Ok(LoadedDict { entries: Vec::new(), deduped: 0, corrupt: DictCorrupt::Quarantined });
    }
    Err(DictLoadError::QuarantineFailed)
}

/// `user_dictionary.json.corrupt.<unix秒>.<pid>`。既存名衝突（同一秒に複数回破損）は
/// 連番で回避し、過去の退避（＝復旧可能な唯一のバックアップ）を上書き破壊しない
/// （settings.json の load_reporting と同じ流儀 — lib.rs 参照）。
fn quarantine_dest_path(path: &Path) -> PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let base = format!("json.corrupt.{secs}.{}", std::process::id());
    let mut dest = path.with_extension(&base);
    let mut n = 1u32;
    while dest.exists() {
        dest = path.with_extension(format!("{base}.{n}"));
        n += 1;
    }
    dest
}

/// Task1 の `save_atomic`（原子保存＋rename リトライ＋AppContainer ACE）をそのまま使う。
pub fn save_to(path: &Path, entries: &[UserDictEntry]) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(entries).map_err(std::io::Error::other)?;
    crate::save_atomic(path, &json)
}

#[derive(Debug, PartialEq)]
pub(crate) enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

/// 判別規則（確定値。brief 検算済み）: ①BOM(UTF-8/16LE/16BE) ②BOM無しで「NUL バイトが
/// 2個以上 かつ 全バイトの5%以上」→UTF-16（LE/BE は偶数位置NUL数 vs 奇数位置NUL数の
/// 多数決） ③それ以外→UTF-8。UTF-16LE の ASCII 文字は上位バイト(NUL)が奇数位置、
/// UTF-16BE は偶数位置に来るため、多数決で LE/BE を割る。
pub(crate) fn sniff_encoding(bytes: &[u8]) -> Encoding {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Encoding::Utf8;
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Encoding::Utf16Le;
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Encoding::Utf16Be;
    }
    let nul_count = bytes.iter().filter(|&&b| b == 0).count();
    if nul_count >= 2 && nul_count as f64 >= bytes.len() as f64 * 0.05 {
        let even_nul = bytes.iter().step_by(2).filter(|&&b| b == 0).count();
        let odd_nul = bytes.iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
        return if odd_nul > even_nul { Encoding::Utf16Le } else { Encoding::Utf16Be };
    }
    Encoding::Utf8
}

/// strict デコード（JSON 用）。lossy は禁止 — CP932 等の非対応バイト列を U+FFFD で
/// 読めてしまうと、そのまま mutation の read-modify-write に乗って文字化けごと
/// 上書き保存し、原本の内容を恒久的に破壊する（spec §3.2）。
pub(crate) fn decode_bytes(bytes: &[u8]) -> Option<String> {
    let s = match sniff_encoding(bytes) {
        Encoding::Utf8 => std::str::from_utf8(bytes).ok()?.to_string(),
        Encoding::Utf16Le => decode_utf16_strict(bytes, true)?,
        Encoding::Utf16Be => decode_utf16_strict(bytes, false)?,
    };
    Some(strip_leading_bom(s))
}

/// lossy デコード（TSV 専用）。had_replacement=true は不正バイト列を U+FFFD で埋めた
/// ことを示す（呼び出し側がインポート結果を利用者に警告するためのシグナル）。
pub(crate) fn decode_bytes_lossy(bytes: &[u8]) -> (String, bool) {
    let (s, had_replacement) = match sniff_encoding(bytes) {
        Encoding::Utf8 => {
            let cow = String::from_utf8_lossy(bytes);
            let had = matches!(cow, std::borrow::Cow::Owned(_));
            (cow.into_owned(), had)
        }
        Encoding::Utf16Le => decode_utf16_lossy(bytes, true),
        Encoding::Utf16Be => decode_utf16_lossy(bytes, false),
    };
    (strip_leading_bom(s), had_replacement)
}

/// BOM の有無に関わらず、対象エンコーディングでバイト列全体（BOM 込み）を復号してから
/// 先頭 U+FEFF を1個だけ取り除く。BOM バイトは UTF-8/UTF-16 どちらでも正しく U+FEFF に
/// 復号されるため、事前にバイト単位で BOM を剥がす分岐を持たずに済む。
fn strip_leading_bom(s: String) -> String {
    match s.strip_prefix('\u{FEFF}') {
        Some(rest) => rest.to_string(),
        None => s,
    }
}

fn utf16_units(bytes: &[u8], le: bool) -> Option<Vec<u16>> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    Some(
        bytes
            .chunks_exact(2)
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect(),
    )
}

fn decode_utf16_strict(bytes: &[u8], le: bool) -> Option<String> {
    let units = utf16_units(bytes, le)?;
    char::decode_utf16(units).collect::<Result<String, _>>().ok()
}

fn decode_utf16_lossy(bytes: &[u8], le: bool) -> (String, bool) {
    // 奇数長（対不完全）は末尾1バイトを切り捨てる。TSV インポート専用の lossy 経路なので
    // strict のように None で失敗させず、had_replacement で異常を呼び出し側へ伝える。
    let had_odd_tail = !bytes.len().is_multiple_of(2);
    let units = bytes.chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) });
    let mut had_replacement = had_odd_tail;
    let s: String = char::decode_utf16(units)
        .map(|r| match r {
            Ok(c) => c,
            Err(_) => {
                had_replacement = true;
                char::REPLACEMENT_CHARACTER
            }
        })
        .collect();
    (s, had_replacement)
}

pub struct ParsedTsv {
    pub rows: Vec<UserDictEntry>,
    pub had_replacement: bool,
}

/// Google/MS-IME 形式 TSV のインポート（spec §6）。行形式は
/// `読み<TAB>単語[<TAB>品詞[<TAB>コメント]]`。`!`/`#` 始まりと空行はコメント/ヘッダとして
/// スキップし、タブ区切りが2列未満の行（コメント文など）も無言でスキップする
/// （エラー扱いにすると MS-IME のヘッダ行だけで丸ごと失敗になる）。
pub fn parse_tsv(bytes: &[u8]) -> ParsedTsv {
    let (text, had_replacement) = decode_bytes_lossy(bytes);
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('!') || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 2 {
            continue;
        }
        rows.push(UserDictEntry {
            ruby: cols[0].to_string(),
            word: cols[1].to_string(),
            pos: cols.get(2).map(|s| s.to_string()),
        });
    }
    ParsedTsv { rows, had_replacement }
}

pub struct MergeReport {
    pub added: usize,
    pub skipped_dup: usize,
    pub skipped_invalid: usize,
    pub encoding_hint: bool,
}

/// インポート行を既存辞書へ追記マージする（spec §6）。`had_replacement` は生成側
/// （`parse_tsv`）の lossy デコード結果を素通しする — U+FFFD が word 側にだけ紛れて
/// `validate_entry` を通過する行（ruby は無事・word だけ化ける）もあり得るため、
/// 「invalid 行に U+FFFD」だけでは検知できない文字化けを拾うための保険。
pub fn merge_imported(existing: &mut Vec<UserDictEntry>, rows: Vec<UserDictEntry>, had_replacement: bool) -> MergeReport {
    let mut seen: std::collections::HashSet<(String, String)> = existing.iter().map(entry_key).collect();
    let mut added = 0usize;
    let mut skipped_dup = 0usize;
    let mut skipped_invalid = 0usize;
    let mut invalid_had_replacement = false;
    for row in rows {
        if validate_entry(&row.ruby, &row.word).is_err() {
            skipped_invalid += 1;
            if row.ruby.contains('\u{FFFD}') || row.word.contains('\u{FFFD}') {
                invalid_had_replacement = true;
            }
            continue;
        }
        if !seen.insert(entry_key(&row)) {
            skipped_dup += 1;
            continue;
        }
        existing.push(row);
        added += 1;
    }
    MergeReport { added, skipped_dup, skipped_invalid, encoding_hint: had_replacement || invalid_had_replacement }
}

pub struct ExportOutput {
    pub tsv: String,
    pub written: usize,
    pub skipped_control: usize,
}

/// Google 形式 TSV へのエクスポート（spec §6）。ソートキーは §5.1 と共通の
/// `(normalize_key(ruby), word)`（word は生値 — normalize_word だと表示順が
/// 正規化後の値になり、UI 一覧の並びと食い違う）。制御文字を含む行（ruby/word/pos の
/// いずれか）は TSV の列構造を壊すため出力せずスキップする。
pub fn to_google_tsv(entries: &[UserDictEntry]) -> ExportOutput {
    let mut sorted: Vec<&UserDictEntry> = entries.iter().collect();
    sorted.sort_by_cached_key(|e| (normalize_key(&e.ruby), e.word.clone()));
    let mut tsv = String::new();
    let mut written = 0usize;
    let mut skipped_control = 0usize;
    for e in sorted {
        let pos = canonical_pos(e.pos.as_deref());
        if has_control_char(&e.ruby) || has_control_char(&e.word) || has_control_char(pos) {
            skipped_control += 1;
            continue;
        }
        tsv.push_str(&e.ruby);
        tsv.push('\t');
        tsv.push_str(&e.word);
        tsv.push('\t');
        tsv.push_str(pos);
        tsv.push_str("\r\n");
        written += 1;
    }
    ExportOutput { tsv, written, skipped_control }
}

fn has_control_char(s: &str) -> bool {
    s.chars().any(|c| ('\u{0000}'..='\u{001F}').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_kana_and_rejects_others() {
        assert!(validate_entry("のーすぺーすきー", "NoSpaceKey").is_ok());
        assert!(validate_entry("ぺこ", r#"m(_ _"m)"#).is_ok()); // 内部空白・記号OK
        assert_eq!(validate_entry("kanji漢字", "x"), Err(UserDictError::InvalidRuby));
        assert_eq!(validate_entry("や ちだ", "x"), Err(UserDictError::InvalidRuby)); // 内部空白不可
        assert_eq!(validate_entry("", "x"), Err(UserDictError::EmptyField));
        assert_eq!(validate_entry("あ", "a\tb"), Err(UserDictError::ControlChar));
        assert_eq!(validate_entry("あ", "a\nb"), Err(UserDictError::ControlChar));
        let long = "あ".repeat(301);
        assert_eq!(validate_entry(&long, "x"), Err(UserDictError::TooLong));
        let emoji299 = format!("{}😀", "x".repeat(298)); // 299 スカラ=OK(JSの.lengthとの食い違い防止)
        assert!(validate_entry("あ", &emoji299).is_ok());
        assert!(validate_entry("か\u{3099}", "x").is_ok()); // NFD かなは合成されて合格
    }

    #[test]
    fn normalize_key_mirrors_engine_lookup_key() {
        assert_eq!(normalize_key("あっぷる"), "アップル");
        assert_eq!(normalize_key("アップル"), "アップル");
        assert_eq!(normalize_key("のーと"), "ノート"); // ーは不変
        assert_eq!(normalize_key("か\u{3099}"), normalize_key("が")); // ひらがな NFD
        assert_eq!(normalize_key("カ\u{3099}"), normalize_key("ガ")); // カタカナ NFD
        assert_eq!(normalize_key("わ\u{3099}"), normalize_key("ヷ")); // 写像→合成の順の固定
        assert_ne!(normalize_key("やちだ\n"), normalize_key("やちだ")); // 改行は trim しない
        assert_eq!(normalize_key(" やちだ\u{3000}"), "ヤチダ"); // Zs は trim
    }

    #[test]
    fn normalize_word_trims_and_composes() {
        assert_eq!(normalize_word("Apple "), normalize_word("Apple")); // Zs trim(エンジン同値)
        assert_eq!(normalize_word("いすゝ\u{3099}"), normalize_word("いすゞ")); // 踊り字合成
        assert_ne!(normalize_word("cafe\u{301}"), normalize_word("café")); // NFDラテンは合成しない(既知の限界)
    }

    #[test]
    fn canonical_pos_matches_swift_cid_table() {
        // Swift UserDictionary.cid(for:) と同じ分類表(spec §3.4)。変更時は両方直す。
        for (input, want) in [
            (None, "名詞"), (Some("名詞"), "名詞"), (Some("謎の品詞"), "名詞"),
            (Some("人名"), "人名"), (Some("人名(姓)"), "姓"), (Some("姓"), "姓"),
            (Some("名"), "名"), (Some("固有名詞"), "固有名詞"), (Some("組織"), "組織"),
            (Some("地名"), "地名"), (Some("駅"), "地名"), (Some("数"), "数"),
        ] { assert_eq!(canonical_pos(input), want, "input={input:?}"); }
    }

    // ---- load/save/dedup/破損隔離(spec §3.2, §8) ----

    fn tmpfile(case: &str, name: &str, bytes: &[u8]) -> PathBuf {
        // テストごとに専用 dir（共有すると並列実行で「.corrupt. が無い」系アサートが
        // 他テストの隔離産物を拾って偽REDになる）。
        let dir = std::env::temp_dir().join(format!("nsk-ud-{}-{case}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn load_absent_and_empty_are_ok_empty() {
        let missing = std::env::temp_dir().join("nsk-ud-none/x.json");
        let r = load_from(&missing).unwrap();
        assert!(r.entries.is_empty());
        assert_eq!(r.corrupt, DictCorrupt::None);
        let empty = tmpfile("empty", "empty.json", b"");
        let r = load_from(&empty).unwrap();
        assert_eq!(r.corrupt, DictCorrupt::None); // 空は破損でない(隔離なし)
        assert!(!empty.with_file_name("empty.json").parent().unwrap()
            .read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains(".corrupt.")));
    }

    #[test]
    fn load_io_error_is_unreadable_and_does_not_quarantine() {
        // 一過性 I/O 失敗を空に畳む実装(NotFound と他エラーの混同)はここで落ちる —
        // 混同すると共有違反1回で dict_add が「空+1件」を保存し既存全エントリが恒久消失する
        let dir = std::env::temp_dir().join(format!("nsk-ud-{}-ioerr", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load_from(&dir).unwrap_err(), DictLoadError::Unreadable); // dir を read → PermissionDenied
        assert!(dir.parent().unwrap().read_dir().unwrap()
            .all(|e| !e.as_ref().unwrap().file_name().to_string_lossy()
                .starts_with(&format!("nsk-ud-{}-ioerr", std::process::id())) ||
                !e.unwrap().file_name().to_string_lossy().contains(".corrupt.")));
        // ↑自テスト由来の隔離産物に限定して検査(%TEMP% 全走査だと無関係な残骸で偽RED)
    }

    #[test]
    fn load_whitespace_only_is_empty_not_corrupt() {
        let p = tmpfile("ws", "ws.json", b"  \r\n\t ");
        let r = load_from(&p).unwrap();
        assert!(r.entries.is_empty());
        assert_eq!(r.corrupt, DictCorrupt::None); // 空白のみは破損でない(隔離・警告なし)
    }

    #[test]
    fn load_accepts_bom_and_utf16_like_engine() {
        let json = r#"[{"ruby":"やちだ","word":"谷内田"}]"#;
        let mut bom = vec![0xEF, 0xBB, 0xBF]; bom.extend(json.as_bytes());
        assert_eq!(load_from(&tmpfile("enc", "bom.json", &bom)).unwrap().entries.len(), 1);
        let utf16le: Vec<u8> = json.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let mut le_bom = vec![0xFF, 0xFE]; le_bom.extend(&utf16le);
        assert_eq!(load_from(&tmpfile("enc", "le.json", &le_bom)).unwrap().entries.len(), 1);
        assert_eq!(load_from(&tmpfile("enc", "le_nobom.json", &utf16le)).unwrap().entries.len(), 1); // NUL判別
        let utf16be: Vec<u8> = json.encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        let mut be_bom = vec![0xFE, 0xFF]; be_bom.extend(&utf16be);
        assert_eq!(load_from(&tmpfile("enc", "be.json", &be_bom)).unwrap().entries.len(), 1);
    }

    #[test]
    fn load_quarantines_broken_json_and_rejects_cp932() {
        let p = tmpfile("quar", "broken.json", br#"[{"ruby":"x""#);
        let r = load_from(&p).unwrap();
        assert_eq!(r.corrupt, DictCorrupt::Quarantined);
        assert!(!p.exists()); // 原本は .corrupt.* へ退避済み
        // CP932(Shift-JIS)は strict デコード失敗=隔離(lossy で読めてしまうと mutation で全損 — spec§3.2)
        let cp932: &[u8] = &[0x5B, 0x7B, 0x22, 0x72, 0x75, 0x62, 0x79, 0x22, 0x3A, 0x22,
                             0x82, 0xE2, 0x22, 0x2C, 0x22, 0x77, 0x6F, 0x72, 0x64, 0x22,
                             0x3A, 0x22, 0x92, 0x4A, 0x22, 0x7D, 0x5D]; // [{"ruby":"や","word":"谷"}] の CP932
        let p2 = tmpfile("quar", "cp932.json", cp932);
        assert_eq!(load_from(&p2).unwrap().corrupt, DictCorrupt::Quarantined);
    }

    #[test]
    fn load_dedups_by_entry_key_keeping_first() {
        let json = r#"[{"ruby":"あっぷる","word":"Apple"},{"ruby":"アップル","word":"Apple"},
                       {"ruby":"やちだ","word":"谷内田"},{"ruby":"やちだ","word":"矢地田"}]"#;
        let r = load_from(&tmpfile("dup", "dup.json", json.as_bytes())).unwrap();
        assert_eq!(r.entries.len(), 3);   // 同一読み別 word は両方残る
        assert_eq!(r.deduped, 1);
        assert_eq!(r.entries[0].ruby, "あっぷる"); // 最初の出現
    }

    #[test]
    fn quarantine_falls_back_to_copy_when_rename_fails() {
        // rename 失敗+copy 成功 → Quarantined(.corrupt.* が copy で生成される)
        let p = tmpfile("qcopy", "broken.json", br#"[{"ruby":"x""#);
        let r = load_from_with(&p, |_f, _t| Err(std::io::Error::other("locked")),
                                   |f, t| std::fs::copy(f, t));
        assert_eq!(r.unwrap().corrupt, DictCorrupt::Quarantined);
        assert!(p.parent().unwrap().read_dir().unwrap()
            .any(|e| e.unwrap().file_name().to_string_lossy().contains(".corrupt.")));
    }

    #[test]
    fn quarantine_both_fail_refuses_and_preserves_original() {
        // spec§8: 隔離 rename・copy 両方失敗のモック → mutation 拒否で原本が上書きされない
        let p = tmpfile("qfail", "broken.json", br#"[{"ruby":"x""#);
        let r = load_from_with(&p, |_f, _t| Err(std::io::Error::other("locked")),
                                   |_f, _t| Err(std::io::Error::other("locked")));
        assert_eq!(r.unwrap_err(), DictLoadError::QuarantineFailed);
        assert!(p.exists()); // 原本はそのまま残る
    }

    #[test]
    fn save_load_roundtrip_preserves_pos_omission() {
        let p = std::env::temp_dir().join(format!("nsk-ud-rt-{}/d.json", std::process::id()));
        let es = vec![UserDictEntry { ruby: "ぺこ".into(), word: r#"m(_ _"m)"#.into(), pos: None }];
        save_to(&p, &es).unwrap();
        assert_eq!(load_from(&p).unwrap().entries, es);
        assert!(!std::fs::read_to_string(&p).unwrap().contains("pos")); // 省略維持(フォーマット無変更)
    }

    // ---- TSV インポート/エクスポート(spec §6, §8) ----

    #[test]
    fn parse_tsv_google_and_msime_forms() {
        let google = "のーすぺーすきー\tNoSpaceKey\t固有名詞\ncomment無し\n"; // 2列目以降欠落行はスキップ
        let r = parse_tsv(google.as_bytes());
        assert_eq!(r.rows.len(), 1);
        let msime = "!Microsoft IME Dictionary Tool\nぎっとはぶ\tGitHub\t名詞\n";
        assert_eq!(parse_tsv(msime.as_bytes()).rows.len(), 1); // !ヘッダスキップ
    }

    #[test]
    fn parse_tsv_encodings() {
        let body = "やちだ\t谷内田\t姓";
        let utf16le: Vec<u8> = body.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(parse_tsv(&utf16le).rows.len(), 1); // BOM無しLE(NUL比率+偶奇)
        let utf16be: Vec<u8> = body.encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        let mut be_bom = vec![0xFE, 0xFF];
        be_bom.extend(&utf16be);
        let r_be = parse_tsv(&be_bom);
        assert_eq!(r_be.rows.len(), 1);
        assert_eq!(r_be.rows[0].ruby, "やちだ"); // **内容まで見る** — U+FEFF が ruby 先頭に残ると偽緑
        let mut u8bom = vec![0xEF, 0xBB, 0xBF];
        u8bom.extend(body.as_bytes());
        let r_u8 = parse_tsv(&u8bom);
        assert_eq!(r_u8.rows[0].ruby, "やちだ"); // UTF-8 BOM 付き TSV(Excel 経由)も同様
        // 1行目が U+3000 開頭でも LE 判定が壊れない(偶奇「多数決」の固定 — 初出1個方式はここで落ちる)
        let tricky = "\u{3000}やちだ\t谷内田\tx\nあい\tアイ\tx\n";
        let le2: Vec<u8> = tricky.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(parse_tsv(&le2).rows.len(), 2);
        // BOM 無し UTF-16BE(偶数位置 NUL 多数 → BE 判定。「閾値超なら常に LE」実装はここで落ちる)
        let be_nobom: Vec<u8> = tricky.encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        assert_eq!(parse_tsv(&be_nobom).rows.len(), 2);
        // 末尾 NUL 1個の UTF-8 は UTF-8 のまま(比率閾値)
        let mut pad = body.as_bytes().to_vec();
        pad.push(0);
        assert_eq!(parse_tsv(&pad).rows.len(), 1);
    }

    #[test]
    fn parse_tsv_lossy_marks_replacement_on_invalid_utf8() {
        // had_replacement の**生成側**固定(merge テストはリテラル bool を手渡しするため死角 —
        // false 固定実装だと Shift-JIS 全滅が無言化する)
        let cp932: &[u8] = &[0x82, 0xE2, 0x82, 0xBF, 0x82, 0xBE, 0x09, 0x92, 0x4A, 0x93, 0xE0, 0x93, 0x63]; // やちだ\t谷内田
        let r = parse_tsv(cp932);
        assert!(r.had_replacement);
        assert!(r.rows.iter().any(|e| e.ruby.contains('\u{FFFD}')));
    }

    #[test]
    fn merge_counts_and_hint() {
        // 注: 以下の had_replacement 引数はリテラル(生成側は parse_tsv_lossy_... が固定)
        let mut existing = vec![UserDictEntry { ruby: "あっぷる".into(), word: "Apple".into(), pos: None }];
        let rows = vec![
            UserDictEntry { ruby: "アップル".into(), word: "Apple".into(), pos: None }, // dup(正規化キー)
            UserDictEntry { ruby: "ぎっとはぶ".into(), word: "GitHub".into(), pos: None }, // added
            UserDictEntry { ruby: "BAD読み".into(), word: "x".into(), pos: None },       // invalid
            UserDictEntry { ruby: "\u{FFFD}あ".into(), word: "x".into(), pos: None },   // invalid+hint
        ];
        let rep = merge_imported(&mut existing, rows, true);
        assert_eq!((rep.added, rep.skipped_dup, rep.skipped_invalid), (1, 1, 2));
        assert!(rep.encoding_hint);
        assert_eq!(existing.len(), 2);
    }

    #[test]
    fn hint_does_not_fire_on_plain_invalid_rows() {
        // 負例: U+FFFD 無しの不正行(非かな読み)だけではヒントを出さない
        // (「invalid が1件でもあれば true」実装の偽緑防止 — 正当な UTF-8 への誤警告になる)
        let mut existing = vec![];
        let rows = vec![UserDictEntry { ruby: "BAD読み".into(), word: "x".into(), pos: None }];
        let rep = merge_imported(&mut existing, rows, false);
        assert_eq!(rep.skipped_invalid, 1);
        assert!(!rep.encoding_hint);
    }

    #[test]
    fn hint_fires_on_replacement_even_if_all_rows_valid() {
        // 正例: invalid 行が0件でも had_replacement=true 単独でヒントを出す
        // (word 側だけ U+FFFD で化けて validate_entry を通過する行がある — ruby は
        // かな検査があるが word は文字種を見ないため、reading 正常・word 化けが invalid
        // に落ちずヒントの死角になる。この分岐だけを削っても他テストは全緑のままなので単独固定)
        let mut existing = vec![];
        let rows = vec![UserDictEntry { ruby: "あっぷる".into(), word: "Apple".into(), pos: None }];
        let rep = merge_imported(&mut existing, rows, true);
        assert_eq!(rep.skipped_invalid, 0);
        assert!(rep.encoding_hint);
    }

    #[test]
    fn export_roundtrips_and_skips_control() {
        let entries = vec![
            UserDictEntry { ruby: "ぎっとはぶ".into(), word: "GitHub".into(), pos: Some("謎の品詞".into()) },
            UserDictEntry { ruby: "あ".into(), word: "a\tb".into(), pos: None }, // 制御文字入り=スキップ
        ];
        let out = to_google_tsv(&entries);
        assert_eq!((out.written, out.skipped_control), (1, 1));
        assert!(out.tsv.contains("ぎっとはぶ\tGitHub\t名詞\r\n")); // 正準pos+CRLF
        let re = parse_tsv(out.tsv.as_bytes());
        assert_eq!(re.rows.len(), 1); // 往復
        assert_eq!(re.rows[0].pos.as_deref(), Some("名詞"));
    }
}
