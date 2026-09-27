//! 位置の契約。計画書 4.2 の3種類（元入力・読み・表示）を置く。
//!
//! 元入力・読みは Unicode scalar 位置、表示は UTF-16 code unit 位置。Rust の byte
//! offset・Swift の Character 数・UTF-16 code unit 数と暗黙に変換しない。ipc crate
//! の `ReadingPosition` と同じ意味の読み位置を別型で持つのは、TIP 境界での変換を
//! 明示させるため（ASCII の評価例で数値が一致しても型は共有しない）。

use serde::Deserialize;

/// 未確定入力ソース上の Unicode scalar 位置。0 始まり。
/// UTF-8 byte 位置でも UTF-16 code unit 位置でもない。暗黙変換しない（計画書 4.2）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Deserialize)]
pub struct SourcePosition(u32);

impl SourcePosition {
    pub fn new(value: u32) -> Self {
        SourcePosition(value)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// 半開区間 [start, end)。空区間は解釈の辺として使わない。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
pub struct SourceRange {
    pub start: SourcePosition,
    pub end: SourcePosition,
}

impl SourceRange {
    pub fn new(start: u32, end: u32) -> Self {
        SourceRange {
            start: SourcePosition(start),
            end: SourcePosition(end),
        }
    }

    /// 区間の scalar 数。ASCII だけの評価例で数値が一致しても型は共有しない。
    pub fn len(self) -> u32 {
        self.end.0.saturating_sub(self.start.0)
    }

    pub fn is_empty(self) -> bool {
        self.end.0 <= self.start.0
    }

    /// ソース文字列の該当部分を scalar 単位で切り出す。
    pub fn slice(self, source: &str) -> String {
        source
            .chars()
            .skip(self.start.0 as usize)
            .take(self.len() as usize)
            .collect()
    }
}

/// 文字列の scalar 長。UTF-16 code unit 数・書記素数といつも同じとは限らない。
pub fn scalar_len(source: &str) -> u32 {
    source.chars().count() as u32
}

/// 採用中の解釈の読み上の Unicode scalar 位置。0 始まり。
/// ipc::clause::ReadingPosition と同じ意味だが型は共有しない。TIP 境界で変換する。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct ReadingPosition(u32);

impl ReadingPosition {
    pub fn new(value: u32) -> Self {
        ReadingPosition(value)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// 読み上の半開区間 [start, end)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReadingRange {
    pub start: ReadingPosition,
    pub end: ReadingPosition,
}

impl ReadingRange {
    pub fn new(start: u32, end: u32) -> Self {
        ReadingRange {
            start: ReadingPosition(start),
            end: ReadingPosition(end),
        }
    }

    pub fn len(self) -> u32 {
        self.end.0.saturating_sub(self.start.0)
    }

    pub fn is_empty(self) -> bool {
        self.end.0 <= self.start.0
    }
}

/// 表示文字列上の UTF-16 code unit 位置。TSF へ渡す文字列の座標系。
/// surrogate pair は 2 code unit になる（`utf16_len` も参照）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct DisplayUtf16Position(u32);

impl DisplayUtf16Position {
    pub fn new(value: u32) -> Self {
        DisplayUtf16Position(value)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// 文字列の UTF-16 code unit 数。表示位置の計算にだけ使う。
pub fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_len_differs_from_grapheme_and_utf16_for_decomposed_text() {
        // e + 結合アクセントは 2 scalar / 1 書記素 / 1 UTF-16 code unit。
        let decomposed = "cafe\u{0301}";
        assert_eq!(scalar_len(decomposed), 5);
        // surrogate pair は 1 scalar / 2 UTF-16 code unit。
        assert_eq!(scalar_len("🌍"), 1);
    }

    #[test]
    fn slice_cuts_by_scalar_index() {
        let source = "きょうはPythonwotukau";
        // kana は1文字が1 scalar。「きょうは」は4 scalar。
        let range = SourceRange::new(4, 10);
        assert_eq!(range.slice(source), "Python");
        assert_eq!(range.len(), 6);
    }

    #[test]
    fn utf16_len_counts_surrogate_pairs_as_two_units() {
        // 「きょうは」は4 scalar / 4 code unit。emoji は1 scalar / 2 code unit。
        assert_eq!(utf16_len("きょうは"), 4);
        assert_eq!(utf16_len("🌍"), 2);
        assert_eq!(scalar_len("🌍"), 1);
    }
}
