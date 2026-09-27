//! 未確定入力の元入力ソースとその由来（計画書 4.3）。
//!
//! `CompositionSource` は未確定入力の編集可能なデータだけを対象にする。
//! ショートカットや物理キーコードの全履歴、確定後の全文は保持しない。要素は
//! 「原文字が分かる `Typed`」「読みだけが分かる `ResolvedKana`」「利用者が明示的に
//! 原文保持した `ExplicitLiteral`」に区別する。`ResolvedKana` を含む範囲の元入力
//! 復元は失敗し、過去の打鍵や別のローマ字表記を捏造しない。
//!
//! 例: `kyo → きょ` の `ょ` を削除して `き` になった場合、残った `き` は
//! `ResolvedKana` であり、`kyo` や `ki` をその原文として扱わない。後続の新しい
//! `Typed` 区間はそのまま解析対象になる。

use crate::position::{scalar_len, ReadingRange, SourceRange};

/// `Typed` 要素の入力スタイル。既存 composer の Kana / Direct に対応するが、
/// この crate は TSF・IPC に依存しないため型は共有しない。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceStyle {
    Kana,
    /// Kana keystrokes currently interpreted as Literal; explicit repair may resynthesize them.
    LiteralKana,
    Direct,
}

/// 要素の由来（計画書 4.3）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provenance {
    /// 原文字と入力スタイルが分かる部分。大小文字・記号を保存する。
    Typed { style: SourceStyle },
    /// 途中編集等で元文字への逆変換が一意でなくなった部分。現在の読みを保持する。
    ResolvedKana,
    /// 利用者が明示的に原文保持を指定した部分。自動推定より優先される。
    ExplicitLiteral,
}

/// 由来をもつソースの1要素。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SourceElement {
    pub provenance: Provenance,
    /// `Typed` / `ExplicitLiteral`: 元文字列（大小文字・記号を保存）。
    /// `ResolvedKana`: 現在の読み（原文の復元対象にならない）。
    pub source_text: String,
    /// この要素の現在の読み。
    pub reading: String,
}

/// 要素の source / reading 両座標上の配置。`CompositionSource::layout` が導出する。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ElementLayout {
    pub source: SourceRange,
    pub reading: ReadingRange,
    pub provenance: Provenance,
}

/// 未確定入力の元入力ソース。要素は source・reading とも先頭から過不足なく並ぶ。
/// 要素は由来づけの最小単位（composer unit 粒度）で、隣接する同由来要素は結合
/// しない。Literal / Japanese の区間境界は要素の内部に置けるため、結合すると
/// Projection の対応粒度を失う。解析の粒度は `analyzable_ranges` の連続領域で
/// 扱う。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompositionSource {
    elements: Vec<SourceElement>,
    /// 元入力の世代。呼出側（TIP）の編集単位で増やす（計画書 4.4 の source_revision）。
    revision: u64,
}

impl CompositionSource {
    pub fn empty(revision: u64) -> Self {
        CompositionSource {
            elements: Vec::new(),
            revision,
        }
    }

    /// 検証付き構築。要素列は空テキストを含まないこと。
    pub fn try_new(elements: Vec<SourceElement>, revision: u64) -> Result<Self, String> {
        let mut source = CompositionSource::empty(revision);
        for element in elements {
            source.push(element)?;
        }
        Ok(source)
    }

    /// 要素を末尾へ追加する。隣接する同由来要素も結合しない（unit 粒度を保つ）。
    pub fn push(&mut self, element: SourceElement) -> Result<(), String> {
        if element.source_text.is_empty() || element.reading.is_empty() {
            return Err("要素の text/reading が空".to_string());
        }
        self.elements.push(element);
        Ok(())
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn elements(&self) -> &[SourceElement] {
        &self.elements
    }

    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    /// ソース文字列の連結。`ResolvedKana` 部分は現在の読みがそのまま現れる。
    pub fn source_text(&self) -> String {
        self.elements
            .iter()
            .map(|element| element.source_text.as_str())
            .collect()
    }

    /// 読み文字列の連結。composer の canonical reading と一致する。
    pub fn reading_text(&self) -> String {
        self.elements
            .iter()
            .map(|element| element.reading.as_str())
            .collect()
    }

    pub fn source_len(&self) -> u32 {
        self.elements
            .iter()
            .map(|element| scalar_len(&element.source_text))
            .sum()
    }

    pub fn reading_len(&self) -> u32 {
        self.elements
            .iter()
            .map(|element| scalar_len(&element.reading))
            .sum()
    }

    /// 全要素の source / reading 座標上の配置。
    pub fn layout(&self) -> Vec<ElementLayout> {
        let mut layouts = Vec::with_capacity(self.elements.len());
        let mut source_at = 0u32;
        let mut reading_at = 0u32;
        for element in &self.elements {
            let source_len = scalar_len(&element.source_text);
            let reading_len = scalar_len(&element.reading);
            layouts.push(ElementLayout {
                source: SourceRange::new(source_at, source_at + source_len),
                reading: ReadingRange::new(reading_at, reading_at + reading_len),
                provenance: element.provenance,
            });
            source_at += source_len;
            reading_at += reading_len;
        }
        layouts
    }

    /// 位置を含む要素の番号。末尾位置は最終要素を返す。範囲外は None。
    pub fn element_index_at(&self, position: u32) -> Option<usize> {
        let mut at = 0u32;
        for (index, element) in self.elements.iter().enumerate() {
            let end = at + scalar_len(&element.source_text);
            if position < end || (position == end && index + 1 == self.elements.len()) {
                return Some(index);
            }
            at = end;
        }
        None
    }

    /// 元入力の復元。範囲が `Typed` / `ExplicitLiteral` の要素だけで被覆されるとき
    /// だけ Some。`Typed` 要素の途中で切れても、打鍵文字列の部分文字列として正当。
    /// `ResolvedKana` に触れる範囲は None（捏造しない）。
    pub fn original(&self, range: SourceRange) -> Option<String> {
        if range.is_empty() || range.end.get() > self.source_len() {
            return None;
        }
        let mut original = String::new();
        let mut at = 0u32;
        for element in &self.elements {
            let end = at + scalar_len(&element.source_text);
            if range.end.get() <= at {
                break;
            }
            if range.start.get() >= end {
                at = end;
                continue;
            }
            match element.provenance {
                Provenance::Typed { .. } | Provenance::ExplicitLiteral => {
                    // 範囲が複数要素にまたがるとき、2つ目以降は要素の先頭から切る
                    // （範囲の残りがこの要素に収まっている）。
                    let start = range.start.get().max(at) - at;
                    let stop = range.end.get().min(end) - at;
                    original.push_str(&slice_scalars(&element.source_text, start, stop));
                }
                Provenance::ResolvedKana => return None,
            }
            at = end;
        }
        Some(original)
    }

    /// 元入力が復元できるか（`original` が Some を返すか）。
    pub fn is_original_recoverable(&self, range: SourceRange) -> bool {
        self.original(range).is_some()
    }

    /// 判別対象になる `Typed` 連続領域。`ResolvedKana` と `ExplicitLiteral` は
    /// 区切りの境界として扱い、またがらせない（計画書 3-1）。
    pub fn analyzable_ranges(&self) -> Vec<SourceRange> {
        let mut ranges = Vec::new();
        let mut at = 0u32;
        let mut run: Option<(u32, u32)> = None;
        for element in &self.elements {
            let end = at + scalar_len(&element.source_text);
            match element.provenance {
                Provenance::Typed { .. } => {
                    let (start, _) = run.unwrap_or((at, at));
                    run = Some((start, end));
                }
                _ => {
                    if let Some((start, stop)) = run.take() {
                        ranges.push(SourceRange::new(start, stop));
                    }
                }
            }
            at = end;
        }
        if let Some((start, stop)) = run {
            ranges.push(SourceRange::new(start, stop));
        }
        ranges
    }
}

fn slice_scalars(text: &str, start: u32, stop: u32) -> String {
    text.chars()
        .skip(start as usize)
        .take((stop - start) as usize)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(source: &str, reading: &str, style: SourceStyle) -> SourceElement {
        SourceElement {
            provenance: Provenance::Typed { style },
            source_text: source.to_string(),
            reading: reading.to_string(),
        }
    }

    fn resolved(reading: &str) -> SourceElement {
        SourceElement {
            provenance: Provenance::ResolvedKana,
            source_text: reading.to_string(),
            reading: reading.to_string(),
        }
    }

    fn sample() -> CompositionSource {
        // kyo(きょ) + u(う) + [gap き] + ka(か) のイメージ。
        CompositionSource::try_new(
            vec![
                typed("kyou", "きょう", SourceStyle::Kana),
                resolved("す"),
                typed("ka", "か", SourceStyle::Kana),
            ],
            7,
        )
        .unwrap()
    }

    #[test]
    fn push_keeps_unit_granularity_without_merging() {
        let mut source = CompositionSource::empty(0);
        source
            .push(typed("kyo", "きょ", SourceStyle::Kana))
            .unwrap();
        source.push(typed("u", "う", SourceStyle::Kana)).unwrap();
        source.push(typed("A", "A", SourceStyle::Direct)).unwrap();
        // 同由来でも結合しない。区間境界を要素内部に置けるようにするため。
        assert_eq!(source.elements().len(), 3);
        assert_eq!(source.source_text(), "kyouA");
        assert_eq!(source.reading_text(), "きょうA");
    }

    #[test]
    fn try_new_rejects_empty_text() {
        assert!(CompositionSource::try_new(vec![resolved("")], 0).is_err());
        let source = CompositionSource::try_new(
            vec![
                typed("ka", "か", SourceStyle::Kana),
                typed("ki", "き", SourceStyle::Kana),
            ],
            0,
        )
        .unwrap();
        assert_eq!(source.elements().len(), 2);
    }

    #[test]
    fn layout_covers_source_and_reading_without_gaps() {
        let source = sample();
        let layout = source.layout();
        assert_eq!(layout.len(), 3);
        assert_eq!(layout[0].source, SourceRange::new(0, 4));
        assert_eq!(layout[0].reading, ReadingRange::new(0, 3));
        assert_eq!(layout[1].source, SourceRange::new(4, 5));
        assert_eq!(layout[1].reading, ReadingRange::new(3, 4));
        assert_eq!(layout[2].source, SourceRange::new(5, 7));
        assert_eq!(layout[2].reading, ReadingRange::new(4, 5));
        assert_eq!(source.source_text(), "kyouすka");
        assert_eq!(source.reading_text(), "きょうすか");
    }

    #[test]
    fn original_recovers_typed_runs_but_never_across_resolved_kana() {
        let source = sample();
        assert_eq!(
            source.original(SourceRange::new(0, 4)).as_deref(),
            Some("kyou")
        );
        // Typed 要素の途中で切れた部分文字列も正当。
        assert_eq!(
            source.original(SourceRange::new(1, 3)).as_deref(),
            Some("yo")
        );
        // ResolvedKana に触れる範囲は復元しない。
        assert!(source.original(SourceRange::new(0, 5)).is_none());
        assert!(source.original(SourceRange::new(4, 5)).is_none());
        assert_eq!(
            source.original(SourceRange::new(5, 7)).as_deref(),
            Some("ka")
        );
        assert!(!source.is_original_recoverable(SourceRange::new(3, 6)));
    }

    #[test]
    fn original_rejects_empty_and_out_of_range() {
        let source = sample();
        assert!(source.original(SourceRange::new(0, 0)).is_none());
        assert!(source.original(SourceRange::new(0, 99)).is_none());
    }

    #[test]
    fn original_spans_multiple_typed_elements_without_restarting_the_range() {
        // 複数の Typed 要素にまたがる範囲は、2つ目以降を要素の先頭から切り
        // 連結する。範囲先頭を各要素で引き算し直すと u32 アンダーフローになる
        // （回帰）。
        let source = CompositionSource::try_new(
            vec![
                typed("kyo", "きょ", SourceStyle::Kana),
                typed("u", "う", SourceStyle::Kana),
                typed("ha", "は", SourceStyle::Kana),
            ],
            0,
        )
        .unwrap();
        assert_eq!(
            source.original(SourceRange::new(0, 6)).as_deref(),
            Some("kyouha")
        );
        assert_eq!(
            source.original(SourceRange::new(1, 5)).as_deref(),
            Some("youh")
        );
        assert!(source.original(SourceRange::new(0, 7)).is_none());
    }

    #[test]
    fn analyzable_ranges_split_at_resolved_and_explicit_elements() {
        let source = CompositionSource::try_new(
            vec![
                typed("kyo", "きょ", SourceStyle::Kana),
                resolved("す"),
                typed("ka", "か", SourceStyle::Kana),
                SourceElement {
                    provenance: Provenance::ExplicitLiteral,
                    source_text: "Python".to_string(),
                    reading: "Python".to_string(),
                },
                typed("u", "う", SourceStyle::Kana),
            ],
            0,
        )
        .unwrap();
        assert_eq!(
            source.analyzable_ranges(),
            [
                SourceRange::new(0, 3),
                SourceRange::new(4, 6),
                SourceRange::new(12, 13),
            ],
            "「す」(3..4) と Python(6..12) は Typed 連続領域の区切り"
        );
    }

    #[test]
    fn element_index_at_clamps_to_the_last_element() {
        let source = sample();
        assert_eq!(source.element_index_at(0), Some(0));
        assert_eq!(source.element_index_at(3), Some(0));
        assert_eq!(source.element_index_at(4), Some(1));
        assert_eq!(source.element_index_at(7), Some(2));
        assert_eq!(source.element_index_at(8), None);
    }

    #[test]
    fn revision_is_carried_for_identity_checks() {
        let source = sample();
        assert_eq!(source.revision(), 7);
    }
}
