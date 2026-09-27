//! 候補区間辺の生成（計画書 5.1）。
//!
//! 各合法な source 境界の組に対して、次の辺を作る:
//! - 日本語に解釈できる辺（ローマ字規則で完全に読みへ合成できる区間。末尾の
//!   単発 `n` だけが残る区間は既存の確定規則と同じ `finalize` 辺として別扱い）。
//! - 辞書に一致する Literal 辺（一般語・技術用語の別レイヤー）と、Latin 系連続列
//!   の内部を含む未知 Literal 辺（列の部分区間すべて）。
//! - 由来の確かな固定辺: `ExplicitLiteral` は Literal 固定、`ResolvedKana` は
//!   日本語固定（原文が復元できない範囲を Literal にしない）。
//!
//! 辺は「どこに切れ得るか」だけを決め、良さの順序はスコアと区間DP（計画書 5.2）
//! が付ける。ローマ字として解釈しにくいことだけを英語の確定理由にしない。

use crate::plan::SegmentKind;
use crate::position::SourceRange;
use crate::roman::{self, RomanStep};
use crate::source::{CompositionSource, Provenance};
use std::collections::{HashMap, HashSet};

/// 辞書のレイヤー（計画書 9.3）。技術用語だけに最適化し過ぎない。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DictLayer {
    General,
    Tech,
}

/// 語彙集合（小文字比較はしない。原文の大小文字を保存した一致だけ）。
#[derive(Clone, Default, Debug)]
pub struct Dictionary {
    words: HashMap<String, DictLayer>,
}

impl Dictionary {
    pub fn new() -> Self {
        Dictionary {
            words: HashMap::new(),
        }
    }

    pub fn insert(&mut self, word: &str, layer: DictLayer) {
        if !word.is_empty() {
            self.words.insert(word.to_string(), layer);
        }
    }

    pub fn layer_of(&self, word: &str) -> Option<DictLayer> {
        self.words.get(word).copied()
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    pub fn words(&self) -> impl Iterator<Item = (&str, DictLayer)> {
        self.words
            .iter()
            .map(|(word, layer)| (word.as_str(), *layer))
    }
}

/// 生成された1辺。スコア不含（`crate::classify` の採点が付ける）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Edge {
    pub range: SourceRange,
    pub kind: SegmentKind,
    /// Japanese 辺のみ: 末尾の単発 `n` を確定規則で完成させる辺。
    pub finalize_tail_n: bool,
    /// Literal 辺のみ: 辞書一致レイヤー。
    pub dict: Option<DictLayer>,
    /// 由来による固定辺（ExplicitLiteral / ResolvedKana）。
    pub fixed: bool,
}

/// 各位置の解析上の扱い。固定辺の制約と Literal 連続列の判定に使う。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PositionRule {
    /// 自由（Typed Kana / Direct の未確定入力）。
    Free,
    /// 利用者の明示 Literal。Literal 辺のみ。
    ExplicitLiteral,
    /// 読みだけが分かる日本語。日本語辺のみ（含めてよい）。
    ResolvedKana,
}

/// ASCII 大文字を小文字へ戻す（CapsLock 原文字の読み合成。非 ASCII はそのまま）。
fn lowercase_ascii(ch: char) -> char {
    if ch.is_ascii_uppercase() {
        ch.to_ascii_lowercase()
    } else {
        ch
    }
}

/// Literal 辺になり得る文字（英字・数字・記号・空白）。カナ・漢字等の非 ASCII は
/// Literal 連続列に含めない（ResolvedKana 由来の日本語）。
fn is_literalish(ch: char) -> bool {
    ch.is_ascii()
}

/// 未確定入力ソースから候補辺を生成する。戻り値は開始位置の昇順。
pub fn generate_edges(source: &CompositionSource, dictionary: &Dictionary) -> Vec<Edge> {
    let text: Vec<char> = source.source_text().chars().collect();
    let layout = source.layout();
    let mut rules = vec![PositionRule::Free; text.len()];
    for element in &layout {
        let rule = match element.provenance {
            Provenance::ExplicitLiteral => PositionRule::ExplicitLiteral,
            Provenance::ResolvedKana => PositionRule::ResolvedKana,
            Provenance::Typed { .. } => PositionRule::Free,
        };
        for offset in element.source.start.get()..element.source.end.get() {
            rules[offset as usize] = rule;
        }
    }

    let mut edges: Vec<Edge> = Vec::new();
    let mut seen: HashSet<(u32, u32, SegmentKind, bool)> = HashSet::new();
    let mut push = |edges: &mut Vec<Edge>, edge: Edge| {
        if seen.insert((
            edge.range.start.get(),
            edge.range.end.get(),
            edge.kind,
            edge.finalize_tail_n,
        )) {
            edges.push(edge);
        }
    };

    // 合法な source 境界（PR2 の Projection と同じ規則）。要素の境界は常に合法。
    // 要素の内部は、保存済み読みと原文が一致する（= 内部で切っても対応が壊れない）
    // 要素のときだけ合法。非1:1 の unit 内部（nn→ん 等）を切る Plan は
    // Projection が SavedReadingNotSplittable で拒否するため、候補生成の時点で
    // 同じ境界集合を使う（採用不能な候補を作らない）。
    let mut legal_boundary = vec![false; text.len() + 1];
    legal_boundary[0] = true;
    legal_boundary[text.len()] = true;
    for (element, element_layout) in source.elements().iter().zip(&layout) {
        let (start, end) = (
            element_layout.source.start.get(),
            element_layout.source.end.get(),
        );
        legal_boundary[end as usize] = true;
        if element.source_text == element.reading {
            for at in start..end {
                legal_boundary[at as usize] = true;
            }
        }
    }

    // 日本語辺: 各開始位置からローマ字状態機械を伸ばし、pending が空になった境界
    // （と末尾 n の finalize 変種）で辺を作る。ASCII は CapsLock 原文字を小文字へ
    // 戻して合成し、非 ASCII（ResolvedKana のカナ等）は通過を許す。ASCII の通過
    // （ローマ字として解釈できない英字・数字・記号）が混ざった時点で打ち切る。
    for start in 0..text.len() {
        if !legal_boundary[start] || rules[start] == PositionRule::ExplicitLiteral {
            continue;
        }
        let mut pending = String::new();
        let mut aborted = false;
        for end in (start + 1)..=text.len() {
            if rules[end - 1] == PositionRule::ExplicitLiteral {
                // 明示 Literal の位置を日本語辺が被覆しない（利用者指定が優先）。
                aborted = true;
            }
            let ch = lowercase_ascii(text[end - 1]);
            if ch.is_ascii() && !ch.is_ascii_alphabetic() {
                // 数字・記号はローマ字経路で消費しない（Literal 辺が担う）。
                aborted = true;
            }
            if aborted {
                break;
            }
            pending.push(ch);
            loop {
                match roman::pending_step(&pending, roman::lookup_roman) {
                    RomanStep::Hold => break,
                    RomanStep::Unit { drain, .. } => {
                        pending.drain(..drain);
                    }
                    RomanStep::Literal { ch, alphabetic } => {
                        if alphabetic {
                            // ローマ字として解釈できない ASCII 英字。
                            aborted = true;
                            break;
                        }
                        // 非 ASCII の通過（ResolvedKana のカナ）は日本語区間に含める。
                        pending.drain(..ch.len_utf8());
                    }
                }
            }
            if aborted {
                break;
            }
            if !legal_boundary[end] {
                // 境界として不合法な位置では辺を出さない（状態機械は続行）。
                continue;
            }
            if pending.is_empty() {
                push(
                    &mut edges,
                    Edge {
                        range: SourceRange::new(start as u32, end as u32),
                        kind: SegmentKind::Japanese,
                        finalize_tail_n: false,
                        dict: None,
                        fixed: false,
                    },
                );
            } else if pending == "n" {
                push(
                    &mut edges,
                    Edge {
                        range: SourceRange::new(start as u32, end as u32),
                        kind: SegmentKind::Japanese,
                        finalize_tail_n: true,
                        dict: None,
                        fixed: false,
                    },
                );
            }
        }
    }

    // 固定辺: ExplicitLiteral の連続 run と ResolvedKana の連続 run。
    let mut index = 0;
    while index < text.len() {
        let rule = rules[index];
        if rule == PositionRule::Free {
            index += 1;
            continue;
        }
        let mut end = index + 1;
        while end < text.len() && rules[end] == rule {
            end += 1;
        }
        let edge = Edge {
            range: SourceRange::new(index as u32, end as u32),
            kind: if rule == PositionRule::ExplicitLiteral {
                SegmentKind::Literal
            } else {
                SegmentKind::Japanese
            },
            finalize_tail_n: false,
            dict: None,
            fixed: true,
        };
        push(&mut edges, edge);
        index = end;
    }

    // 辞書の Literal 辺（ExplicitLiteral / ResolvedKana と重ならない位置だけ）。
    let mut words: Vec<&String> = dictionary.words.keys().collect();
    words.sort();
    for word in words {
        let mut from = 0;
        while let Some(hit) = source.source_text()[from..].find(word.as_str()) {
            let byte_start = from + hit;
            let scalar_start = char_byte_to_scalar(&text, byte_start);
            let scalar_len = word.chars().count() as u32;
            let scalar_end = scalar_start + scalar_len;
            if legal_boundary[scalar_start as usize]
                && legal_boundary[scalar_end as usize]
                && (covers_only(&rules, scalar_start, scalar_end, PositionRule::Free)
                    || covers_only(
                        &rules,
                        scalar_start,
                        scalar_end,
                        PositionRule::ExplicitLiteral,
                    ))
            {
                push(
                    &mut edges,
                    Edge {
                        range: SourceRange::new(scalar_start, scalar_end),
                        kind: SegmentKind::Literal,
                        finalize_tail_n: false,
                        dict: dictionary.layer_of(word),
                        fixed: false,
                    },
                );
            }
            from = byte_start + word.len();
        }
    }

    // 未知 Literal 辺: Latin 系連続列（ASCII run）のすべての部分区間。
    // 同じ Latin 連続列の内部も区切る（計画書 5.2。辞書一致だけでは区切らない）。
    let mut run_start = 0;
    while run_start < text.len() {
        if !is_literalish(text[run_start]) || rules[run_start] == PositionRule::ResolvedKana {
            run_start += 1;
            continue;
        }
        let mut run_end = run_start + 1;
        while run_end < text.len()
            && is_literalish(text[run_end])
            && rules[run_end] != PositionRule::ResolvedKana
        {
            run_end += 1;
        }
        for start in run_start..run_end {
            if !legal_boundary[start] {
                continue;
            }
            for end in (start + 1)..=run_end {
                if !legal_boundary[end]
                    || covers_any(&rules, start as u32, end as u32, PositionRule::ResolvedKana)
                {
                    continue;
                }
                let word: String = text[start..end].iter().collect();
                push(
                    &mut edges,
                    Edge {
                        range: SourceRange::new(start as u32, end as u32),
                        kind: SegmentKind::Literal,
                        finalize_tail_n: false,
                        dict: dictionary.layer_of(&word),
                        fixed: false,
                    },
                );
            }
        }
        run_start = run_end;
    }

    edges.sort_by_key(|edge| (edge.range.start.get(), edge.range.end.get()));
    edges
}

fn char_byte_to_scalar(chars: &[char], byte: usize) -> u32 {
    let mut scalar = 0u32;
    let mut acc = 0usize;
    for ch in chars {
        if acc >= byte {
            break;
        }
        acc += ch.len_utf8();
        scalar += 1;
    }
    scalar
}

/// [start, end) がすべて指定の扱いか。
fn covers_only(rules: &[PositionRule], start: u32, end: u32, rule: PositionRule) -> bool {
    rules[start as usize..end as usize]
        .iter()
        .all(|r| *r == rule)
}

/// [start, end) に指定の扱いが1つでも含まれるか。
fn covers_any(rules: &[PositionRule], start: u32, end: u32, rule: PositionRule) -> bool {
    rules[start as usize..end as usize]
        .iter()
        .any(|r| *r == rule)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{SourceElement, SourceStyle};

    fn typed(source: &str) -> CompositionSource {
        let mut composed = CompositionSource::empty(0);
        composed
            .push(SourceElement {
                provenance: Provenance::Typed {
                    style: SourceStyle::Kana,
                },
                source_text: source.to_string(),
                reading: source.to_string(),
            })
            .unwrap();
        composed
    }

    fn find<'a>(edges: &'a [Edge], start: u32, end: u32, kind: SegmentKind) -> Option<&'a Edge> {
        edges
            .iter()
            .find(|e| e.range.start.get() == start && e.range.end.get() == end && e.kind == kind)
    }

    #[test]
    fn japanese_edges_cover_complete_roman_intervals() {
        let edges = generate_edges(&typed("kaka"), &Dictionary::new());
        // ka|ka の両方、kaka 全体。
        assert!(find(&edges, 0, 2, SegmentKind::Japanese).is_some());
        assert!(find(&edges, 2, 4, SegmentKind::Japanese).is_some());
        assert!(find(&edges, 0, 4, SegmentKind::Japanese).is_some());
        // k 単独（未完）は日本語辺にならない。
        assert!(find(&edges, 0, 1, SegmentKind::Japanese).is_none());
        assert!(find(&edges, 2, 3, SegmentKind::Japanese).is_none());
    }

    #[test]
    fn trailing_single_n_is_a_finalize_edge_only() {
        let edges = generate_edges(&typed("kakun"), &Dictionary::new());
        let edge = find(&edges, 0, 5, SegmentKind::Japanese).expect("kakun 全体の finalize 辺");
        assert!(edge.finalize_tail_n);
        assert!(!find(&edges, 0, 4, SegmentKind::Japanese)
            .map(|e| e.finalize_tail_n)
            .unwrap_or(false));
    }

    #[test]
    fn uppercase_source_still_makes_japanese_edges_via_lowercase() {
        // CapsLock 原文字: source は大文字でも読み合成は小文字で行う。
        let edges = generate_edges(&typed("KA"), &Dictionary::new());
        assert!(find(&edges, 0, 2, SegmentKind::Japanese).is_some());
    }

    #[test]
    fn digits_and_symbols_only_make_literal_edges() {
        let edges = generate_edges(&typed("a1b"), &Dictionary::new());
        // '1' は日本語辺の経路を断つ。run 全体の Literal 辺がある。
        assert!(!edges.iter().any(|e| e.kind == SegmentKind::Japanese
            && e.range.start.get() <= 1
            && e.range.end.get() > 1));
        assert!(find(&edges, 0, 3, SegmentKind::Literal).is_some());
        assert!(find(&edges, 1, 2, SegmentKind::Literal).is_some());
    }

    #[test]
    fn dictionary_word_makes_a_literal_edge_with_layer() {
        let mut dictionary = Dictionary::new();
        dictionary.insert("github", DictLayer::Tech);
        let edges = generate_edges(&typed("githubno"), &dictionary);
        let edge = find(&edges, 0, 6, SegmentKind::Literal).expect("github の辞書辺");
        assert_eq!(edge.dict, Some(DictLayer::Tech));
        // run の部分区間にも未知 Literal 辺があり、その辞書判定も付く。
        assert_eq!(
            find(&edges, 0, 6, SegmentKind::Literal).map(|e| e.dict),
            Some(Some(DictLayer::Tech))
        );
    }

    #[test]
    fn explicit_literal_is_fixed_and_excludes_japanese() {
        let mut composed = CompositionSource::empty(0);
        composed
            .push(SourceElement {
                provenance: Provenance::ExplicitLiteral,
                source_text: "abc".to_string(),
                reading: "abc".to_string(),
            })
            .unwrap();
        let edges = generate_edges(&composed, &Dictionary::new());
        let fixed = edges
            .iter()
            .find(|e| e.fixed && e.kind == SegmentKind::Literal)
            .expect("ExplicitLiteral の固定辺");
        assert_eq!(fixed.range, SourceRange::new(0, 3));
        // abc はローマ字解釈できるが、明示 Literal なので日本語辺を作らない。
        assert!(!edges
            .iter()
            .any(|e| e.kind == SegmentKind::Japanese && e.range.start.get() == 0));
    }

    #[test]
    fn non_one_to_one_unit_interiors_are_not_boundaries() {
        // 保存済み読みと原文が一致しない要素（nn→ん 等）の内部では切らない。
        // Projection が SavedReadingNotSplittable で拒否する Plan を候補に
        // 出さない（「合法な source 境界」だけに辺を作る）。
        let mut composed = CompositionSource::empty(0);
        composed
            .push(SourceElement {
                provenance: Provenance::Typed {
                    style: SourceStyle::Direct,
                },
                source_text: "nn".to_string(),
                reading: "ん".to_string(),
            })
            .unwrap();
        composed
            .push(SourceElement {
                provenance: Provenance::Typed {
                    style: SourceStyle::Kana,
                },
                source_text: "ka".to_string(),
                reading: "か".to_string(),
            })
            .unwrap();
        let edges = generate_edges(&composed, &Dictionary::new());
        for edge in &edges {
            let (start, end) = (edge.range.start.get(), edge.range.end.get());
            // nn の内部 = 位置1。辺の開始・終端が位置1になるものは作らない。
            assert!(
                start != 1 && end != 1,
                "nn の内部（境界1）で切る辺がある: {edge:?}"
            );
        }
        // 要素境界では切れる: nn 全体（日本語辺）と ka。
        assert!(find(&edges, 0, 2, SegmentKind::Japanese).is_some());
        assert!(find(&edges, 2, 4, SegmentKind::Japanese).is_some());
    }

    #[test]
    fn resolved_kana_forces_japanese() {
        let mut composed = CompositionSource::empty(0);
        composed
            .push(SourceElement {
                provenance: Provenance::ResolvedKana,
                source_text: "か".to_string(),
                reading: "か".to_string(),
            })
            .unwrap();
        composed
            .push(SourceElement {
                provenance: Provenance::Typed {
                    style: SourceStyle::Kana,
                },
                source_text: "ta".to_string(),
                reading: "た".to_string(),
            })
            .unwrap();
        let edges = generate_edges(&composed, &Dictionary::new());
        // か（ResolvedKana）を含む Literal 辺は作らない。
        assert!(!edges.iter().any(|e| e.kind == SegmentKind::Literal
            && e.range.start.get() == 0
            && e.range.end.get() > 0));
        // か|ta は日本語辺として接続できる（非 ASCII の通過を許す）。
        assert!(edges.iter().any(|e| e.kind == SegmentKind::Japanese
            && e.range == SourceRange::new(0, 3)
            && !e.finalize_tail_n));
    }
}
