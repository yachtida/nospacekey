//! 記号全角化: 写像の単一真実源 + 個別選択対象カタログ + ビットマスク型（Task 1, 2026-08-02 spec）。
//! `crates/tip/src/input_state.rs` の `zenkaku_symbol`/`zenkaku_of` を移設した。tip / config は
//! どちらも settings への path 依存のみ持ち逆方向は無いため、写像をここへ置けば config 側で
//! JS に表を再実装する「表の穴の再生産」（2026-07-16 spec §2）を構造的に防げる（spec §2）。

use std::collections::BTreeSet;

/// かな入力打鍵の記号の既定幅（分類順に畳む）: 長音符=無条件 → 句読点=punct トグル →
/// 記号=symbol トグル AND 個別選択集合 → 置換3件(/[]→・「」 = Mozc symbol_method 相当) →
/// 残りは is_ascii_punctuation 全域を式で全角形へ。英数字は構造的に対象外
///（roman2kana に委ねる）。VK でなく ToUnicode 結果の文字で引く — 記号 VK は
/// レイアウト依存（JIS/US）のため VK 固定マップは禁止（設計ロック 2026-07-07）。
/// 個別表を全記号に持たないのは、表の穴（旧 !/@ の US 到達不能・~→U+301C 混入）を
/// 再生産しないため（2026-07-16 spec §2）。
/// idle(合成開始)と composition 畳み込みの両方が呼ぶ単一マップ（`-` と全記号を同仕様に）。
/// `chars` の判定はここでのみ行う — 呼び出し側で `symbol_full_width && chars.contains(c)` を
/// 計算してから bool を渡す形にしない（判定が呼び出し側へ散ると単一真実源が壊れる。
/// 2026-08-02 spec §4）。
pub fn zenkaku_symbol(
    c: char,
    punct_full_width: bool,
    symbol_full_width: bool,
    chars: SymbolCharSet,
) -> Option<char> {
    Some(match c {
        '-' => 'ー',
        ',' => if punct_full_width { '、' } else { return None },
        '.' => if punct_full_width { '。' } else { return None },
        _ if !(symbol_full_width && chars.contains(c)) => return None,
        '/' => '・', '[' => '「', ']' => '」',
        c if c.is_ascii_punctuation() => zenkaku_of(c),
        _ => return None,
    })
}

/// ASCII 印字可能域の機械写像（0x21..=0x7E → U+FF01..=U+FF5E）。`~`→～(U+FF5E) はここから
/// 出る＝Windows 正準。Mozc/iOS 版の U+301C（波ダッシュ）は CP932 で ? に化けるため採らない。
pub fn zenkaku_of(c: char) -> char {
    char::from_u32(c as u32 - 0x21 + 0xFF01).unwrap_or(c)
}

/// 個別選択対象記号（29件）のビットマスク集合。`0x21..=0x7E`（94コードポイント）を bit へ
/// 写す `u128` newtype。TIP の設定キャッシュが `Cell<T: Copy>` で統一されているため
///（`text_service.rs:480-497`）、`RefCell<BTreeSet>` を持ち込まずに `Cell<SymbolCharSet>` へ
/// 載せるための表現（spec §2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolCharSet(u128);

impl SymbolCharSet {
    /// `0x21..=0x7E` の全94ビットが立った集合。
    pub const ALL: SymbolCharSet = SymbolCharSet((1u128 << 94) - 1);
    /// 空集合。TIP キャッシュの Activate 前初期値（現行 `Cell::new(false)` と同じ
    /// 「未設定=効かない」側 — spec §2）。
    pub const EMPTY: SymbolCharSet = SymbolCharSet(0);

    /// `c` が集合に含まれるか。範囲外文字（`0x21..=0x7E` 外）は常に false。
    pub fn contains(&self, c: char) -> bool {
        match bit_index(c) {
            Some(i) => self.0 & (1u128 << i) != 0,
            None => false,
        }
    }

    /// 集合が空か。
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitAnd for SymbolCharSet {
    type Output = SymbolCharSet;
    fn bitand(self, rhs: SymbolCharSet) -> SymbolCharSet {
        SymbolCharSet(self.0 & rhs.0)
    }
}

/// `0x21..=0x7E` → `0..94` のビット位置。範囲外は None。
fn bit_index(c: char) -> Option<u32> {
    let n = c as u32;
    (0x21..=0x7E).contains(&n).then_some(n - 0x21)
}

/// 範囲外文字（`0x21..=0x7E` 外）は脱落する — 往復恒等ではない（非可逆。spec §6 MI-1）。
impl From<&BTreeSet<char>> for SymbolCharSet {
    fn from(set: &BTreeSet<char>) -> Self {
        let mut mask = 0u128;
        for &c in set {
            if let Some(i) = bit_index(c) {
                mask |= 1u128 << i;
            }
        }
        SymbolCharSet(mask)
    }
}

/// 個別選択対象29件の (半角, 全角プレビュー) カタログ。`is_ascii_punctuation()` かつ
/// `- , .` 以外を列挙する（句読点=punct トグル管轄・長音符=無条件のため対象外 — spec §1）。
/// プレビューは `zenkaku_symbol` を「トグル ON・`SymbolCharSet::ALL`」で呼んで導出する —
/// 個別表は持たない。`ALL` を使うのは、既定集合（`default_full_width_chars`）が本関数から
/// 導出されるため（下記）、逆に既定集合を参照すると定義循環（実行時は無限再帰）になるから
/// （spec §2 IMP-1）。カタログは選択状態と無関係な集合非依存が正しい。
pub fn symbol_targets() -> impl Iterator<Item = (char, char)> {
    (0x21u32..=0x7E).filter_map(|n| {
        let c = char::from_u32(n).expect("0x21..=0x7E is valid char range");
        (c.is_ascii_punctuation() && !matches!(c, '-' | ',' | '.')).then(|| {
            let full = zenkaku_symbol(c, false, true, SymbolCharSet::ALL)
                .expect("is_ascii_punctuation implies zenkaku_symbol returns Some");
            (c, full)
        })
    })
}

/// `symbol_targets()` の半角側全件（`full_width_chars` の既定 = 全29記号）。「is_ascii_punctuation
/// かつ `- , .` 以外」の条件を2箇所に独立に書かない（導出式の二重定義も表の穴と同型の欠陥。
/// spec §3 MI-2）。
pub(crate) fn default_full_width_chars() -> BTreeSet<char> {
    symbol_targets().map(|(half, _)| half).collect()
}

/// `full_width_chars` の寛容デシリアライズ。コンテナ+要素の2層で防御する（spec §3）:
/// - **コンテナ**: 配列でない値（手編集の文字列/null/オブジェクト等）は既定（全29）へ
///   フォールバックする。空集合でなく既定へ倒すのは、空へ倒すと「トグル ON なのに何も
///   全角化されない」= 機能が黙って死ぬ側の驚きになるため（未知値は驚きの小さい既定へ倒す
///   前例 = `shift_latin_is_compose` と同方向。代償は spec §3 参照）。
/// - **要素**: 配列なら要素ごとに検証し、1文字の文字列のみ採用、それ以外（複数文字・数値等）
///   は黙って読み飛ばす。対象外文字（`-` や英数字）の1文字要素は捨てずに保持する（実効判定は
///   `effective_chars` が対象29との積を取るため挙動に影響しない）。
///
/// 素の `BTreeSet<char>` derive だと不正値が `Settings` 全体のパース失敗を招き、settings.json
/// が corrupt 退避されて keymap/外観含む全設定が既定へ劣化する。防御をフィールド内で完結させ、
/// blast radius を当該フィールドに限定する。
pub(crate) fn de_symbol_chars<'de, D>(deserializer: D) -> Result<BTreeSet<char>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let value = serde_json::Value::deserialize(deserializer)?;
    let Some(items) = value.as_array() else {
        return Ok(default_full_width_chars());
    };
    let mut set = BTreeSet::new();
    for item in items {
        if let Some(s) = item.as_str() {
            let mut chars = s.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                set.insert(c);
            }
        }
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prolonged_sound_and_punctuation_ignore_chars_set_content() {
        // -,. の判定は集合の内容に関わらず現行挙動のまま（長音符=無条件、句読点=punct トグルのみ）。
        assert_eq!(zenkaku_symbol('-', false, false, SymbolCharSet::EMPTY), Some('ー'));
        assert_eq!(zenkaku_symbol('-', true, true, SymbolCharSet::EMPTY), Some('ー'));
        assert_eq!(zenkaku_symbol('.', true, false, SymbolCharSet::EMPTY), Some('。'));
        assert_eq!(zenkaku_symbol(',', true, false, SymbolCharSet::EMPTY), Some('、'));
        assert_eq!(zenkaku_symbol('.', false, true, SymbolCharSet::ALL), None);
        assert_eq!(zenkaku_symbol(',', false, true, SymbolCharSet::ALL), None);
    }

    #[test]
    fn empty_char_set_behaves_like_symbol_toggle_off_for_symbol_chars() {
        // 空集合なら全記号が半角のまま（不変条件Aのみの証明。overlay=false は別途
        // effective_chars/symbol_overlay のテストで証明する — spec §6）。
        for (c, _) in symbol_targets() {
            assert_eq!(zenkaku_symbol(c, false, true, SymbolCharSet::EMPTY), None, "{c:?}");
        }
    }

    #[test]
    fn master_toggle_off_yields_half_width_even_when_char_set_is_full() {
        // TIP は overlay=false でも effective_chars()（非空）をキャッシュへ載せる
        //（text_service.rs の Activate は overlay と集合を独立に格納する）ため、
        // トグル OFF の正しさは guard の symbol_full_width 連言だけが担保する。
        // このテストが無いと guard から symbol_full_width を削る変異が全緑で通る。
        let defaults = SymbolCharSet::from(&default_full_width_chars());
        for (c, _) in symbol_targets() {
            assert_eq!(zenkaku_symbol(c, false, false, defaults), None, "{c:?}");
            assert_eq!(zenkaku_symbol(c, true, false, defaults), None, "{c:?}");
        }
    }

    #[test]
    fn issue1_example_exclamation_and_question_on_slash_and_at_off() {
        // Issue #1 の要望例: `!` `?` は全角、`/` `@` は半角。
        let mut chars: BTreeSet<char> = symbol_targets().map(|(h, _)| h).collect();
        chars.remove(&'/');
        chars.remove(&'@');
        let set = SymbolCharSet::from(&chars);
        assert_eq!(zenkaku_symbol('!', false, true, set), Some('！'));
        assert_eq!(zenkaku_symbol('?', false, true, set), Some('？'));
        assert_eq!(zenkaku_symbol('/', false, true, set), None);
        assert_eq!(zenkaku_symbol('@', false, true, set), None);
    }

    #[test]
    fn symbol_char_set_contains_range_boundaries_and_drops_out_of_range_chars_on_construction() {
        let set = SymbolCharSet::from(&BTreeSet::from(['!', '~', 'あ']));
        assert!(set.contains('!'), "0x21 境界");
        assert!(set.contains('~'), "0x7E 境界");
        assert!(!set.contains('a'), "範囲内だが未追加");
        assert!(!set.contains('あ'), "範囲外は構築時に脱落する（非可逆）");
        assert!(SymbolCharSet::ALL.contains('!'));
        assert!(!SymbolCharSet::EMPTY.contains('!'));
    }

    #[test]
    fn symbol_targets_returns_29_entries_excluding_dash_comma_period() {
        let targets: Vec<(char, char)> = symbol_targets().collect();
        assert_eq!(targets.len(), 29);
        assert!(targets.iter().all(|(h, _)| !matches!(h, '-' | ',' | '.')));
        assert!(targets.iter().all(|(h, full)| h != full), "プレビューは全角化されているはず");
        // 置換系3件のプレビューは設定画面がユーザーへ見せるドキュメントそのもの
        //（「全角スラッシュでなく中黒」— spec §5）。カタログの full 側を直接固定する。
        assert!(targets.contains(&('/', '・')));
        assert!(targets.contains(&('[', '「')));
        assert!(targets.contains(&(']', '」')));
        assert!(targets.contains(&('!', '！')));
    }

    #[test]
    fn symbol_targets_all_29_entries_match_pre_move_mapping_with_default_set() {
        // 移設回帰の網羅テスト（受入#1「現行と同一挙動」の最も安い証明 — spec §6）:
        // 全29文字を既定集合で判定し、移設前の写像（置換3件 + 残り26は zenkaku_of の式）と一致すること。
        let defaults = SymbolCharSet::from(&default_full_width_chars());
        for (c, _) in symbol_targets() {
            let expected = match c {
                '/' => '・',
                '[' => '「',
                ']' => '」',
                _ => char::from_u32(c as u32 - 0x21 + 0xFF01).unwrap(),
            };
            assert_eq!(zenkaku_symbol(c, false, true, defaults), Some(expected), "{c:?}");
        }
    }

    #[test]
    fn de_symbol_chars_skips_invalid_elements_and_keeps_valid_ones() {
        let json = r#"["!","ab",42,"?"]"#;
        let set: BTreeSet<char> = serde_json::from_str::<Wrapper>(&format!(r#"{{"chars":{json}}}"#))
            .unwrap()
            .chars;
        assert_eq!(set, BTreeSet::from(['!', '?']));
    }

    #[test]
    fn de_symbol_chars_falls_back_to_default_29_for_non_array_containers() {
        for bad in [r#""!?""#, "null", "{}"] {
            let json = format!(r#"{{"chars":{bad}}}"#);
            let set: BTreeSet<char> = serde_json::from_str::<Wrapper>(&json).unwrap().chars;
            assert_eq!(set, default_full_width_chars(), "container {bad:?} should fall back to default 29");
        }
    }

    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[serde(deserialize_with = "de_symbol_chars")]
        chars: BTreeSet<char>,
    }
}
