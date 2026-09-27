//! 候補辺グラフ上の k-best 区間DP（計画書 5.2）。
//!
//! 位置 0..n を頂点、採点済み辺を弧とする完全被覆パスの上位 k 件を求める。
//! スコアは辺スコアの総和 + 言語切替ペナルティ。タイブレークは
//! (スコア降順, 境界列の辞書順昇順, kind 列の辞書順昇順) で固定し、
//! 入力が同じなら常に同じ順序を返す（近似のビーム刈り込みは使わない）。

use crate::classify::edge::Edge;
use crate::plan::SegmentKind;

/// 言語切替の遷移コスト（計画書 5.2。「普通の日本語を英字にしてしまう誤り」の
/// コストを高めるため、日本語→Literal の切替は追加コストが乗る）。
#[derive(Clone, Copy, Debug)]
pub struct TransitionCosts {
    /// kind が変わる切り替え1回の基本コスト（正の値でペナルティ）。
    pub switch: f64,
    /// Japanese→Literal への切り替え1回の追加コスト（正の値）。
    pub ja_to_literal: f64,
}

impl Default for TransitionCosts {
    fn default() -> Self {
        TransitionCosts {
            switch: 2.0,
            ja_to_literal: 1.0,
        }
    }
}

/// 採点済みのパス1件。
#[derive(Clone, Debug)]
pub struct Path {
    pub score: f64,
    /// (辺, 辺スコア)。ソース先頭から順。
    pub steps: Vec<(Edge, f64)>,
}

impl Path {
    pub fn ordering_key(&self) -> Vec<(u32, u32, u8)> {
        // 辺スコアを除いた構造だけの辞書順（kind は Japanese=0 < Literal=1）。
        self.steps
            .iter()
            .map(|(edge, _)| {
                (
                    edge.range.start.get(),
                    edge.range.end.get(),
                    match edge.kind {
                        SegmentKind::Japanese => 0u8,
                        SegmentKind::Literal => 1u8,
                    },
                )
            })
            .collect()
    }

    fn better_than(&self, other: &Path) -> bool {
        self.score
            .partial_cmp(&other.score)
            .map(|ordering| match ordering {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => self.ordering_key() < other.ordering_key(),
            })
            .unwrap_or(true)
    }
}

/// ソース長（scalar 数）と採点済み辺から k-best を返す。スコア降順。
///
/// 状態は (位置, 直前辺の kind)。遷移コストは直前の kind に依存するため、
/// 位置だけの k-best だと「今後の切替ペナルティが軽い」経路を誤って刈れる。
/// kind 別に上位 k を保持することで近似のビーム刈り込みを導入しない
/// （計画書 5.2）。
///
/// 辺列は区間列（Plan）そのものとして列挙する: 直前と同じ kind の辺では
/// 拡張しない（隣接する同 kind は Plan では1つの区間に結合されるため、別々に
/// 持っても意味的に同じ Plan になる）。この結果、k-best の各パスはすべて
/// 異なる Plan であり、正規化・重複排除なしに上位 k 件の解釈と margin が
/// 確定する。
pub fn k_best(
    length: u32,
    scored: &[(Edge, f64)],
    k: usize,
    transition: &TransitionCosts,
) -> Vec<Path> {
    let mut by_start: Vec<Vec<usize>> = vec![Vec::new(); length as usize + 1];
    for (index, (edge, _)) in scored.iter().enumerate() {
        let start = edge.range.start.get() as usize;
        by_start[start].push(index);
    }

    // dp[pos][kind_slot] = 位置 pos を kind で終えたパスの上位 k 件。辺は常に
    // 右向き（end > pos）なので、位置 pos を始点として処理する時点で pos への
    // 拡張は完了している。所有権ごと取り出して借用衝突を避ける。
    let mut dp: Vec<[Vec<Path>; 2]> = vec![[Vec::new(), Vec::new()]; length as usize + 1];
    dp[0][kind_index(SegmentKind::Japanese)].push(Path {
        score: 0.0,
        steps: Vec::new(),
    });
    for pos in 0..length as usize {
        if dp[pos].iter().all(|list| list.is_empty()) {
            continue;
        }
        let prefixes = std::mem::take(&mut dp[pos]);
        for candidates in &prefixes {
            for &edge_index in &by_start[pos] {
                let (edge, edge_score) = &scored[edge_index];
                let end = edge.range.end.get() as usize;
                if end <= pos {
                    continue;
                }
                for prefix in candidates {
                    // 直前と同 kind の辺では拡張しない（Plan では結合されるため、
                    // 意味的に同じ区間列になる）。空パスはどの kind からでも始められる。
                    if prefix
                        .steps
                        .last()
                        .is_some_and(|(last, _)| last.kind == edge.kind)
                    {
                        continue;
                    }
                    let transition = transition_score(&prefix.steps, edge, transition);
                    let mut extended = Path {
                        score: prefix.score + edge_score + transition,
                        steps: prefix.steps.clone(),
                    };
                    extended.steps.push((edge.clone(), *edge_score));
                    insert_best(&mut dp[end][kind_index(edge.kind)], extended, k);
                }
            }
        }
    }
    let [ja, lit] = dp.pop().unwrap_or_default();
    let mut merged: Vec<Path> = ja.into_iter().chain(lit).collect();
    // マージ後も DP 内と同じ全順序（スコア降順 → 境界列辞書順）で固定する。
    merged.sort_by(|a, b| {
        if a.better_than(b) {
            std::cmp::Ordering::Less
        } else if b.better_than(a) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    merged
}

fn kind_index(kind: SegmentKind) -> usize {
    match kind {
        SegmentKind::Japanese => 0,
        SegmentKind::Literal => 1,
    }
}

/// 言語切替の遷移スコア（ペナルティ。正の cost を負のスコアへ足す）。
fn transition_score(prefix: &[(Edge, f64)], next: &Edge, costs: &TransitionCosts) -> f64 {
    let Some((last, _)) = prefix.last() else {
        return 0.0;
    };
    if last.kind == next.kind {
        0.0
    } else {
        let mut penalty = costs.switch;
        if last.kind == SegmentKind::Japanese {
            penalty += costs.ja_to_literal;
        }
        -penalty
    }
}

fn insert_best(list: &mut Vec<Path>, candidate: Path, k: usize) {
    let mut position = list.len();
    for (index, existing) in list.iter().enumerate() {
        if candidate.better_than(existing) {
            position = index;
            break;
        }
    }
    if position >= list.len() && list.len() >= k {
        return;
    }
    list.insert(position, candidate);
    list.truncate(k);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::position::SourceRange;

    fn edge(start: u32, end: u32, kind: SegmentKind) -> Edge {
        Edge {
            range: SourceRange::new(start, end),
            kind,
            finalize_tail_n: false,
            dict: None,
            fixed: false,
        }
    }

    #[test]
    fn finds_top_paths_in_score_order() {
        let scored = vec![
            (edge(0, 2, SegmentKind::Literal), 3.0),
            (edge(0, 2, SegmentKind::Japanese), 1.0),
            (edge(2, 4, SegmentKind::Japanese), 2.0),
            (edge(0, 4, SegmentKind::Japanese), 4.0),
        ];
        let paths = k_best(4, &scored, 3, &TransitionCosts::default());
        // 同 kind 連結（JA[0,2)+JA[2,4)）は Plan として同一になるため列挙しない。
        // 残る全体被覆は [0,4)JA と Literal→JA の2つ。
        assert_eq!(paths.len(), 2);
        // 全体1辺 (4.0) が先頭。Literal+JA は切替ペナルティ（switch のみ。
        // ja_to_literal は Japanese→Literal 方向だけに乗る）。
        assert!((paths[0].score - 4.0).abs() < 1e-9);
        assert!(
            (paths[1].score - (3.0 + 2.0 - TransitionCosts::default().switch)).abs() < 1e-9,
            "Literal→Japanese の切替: {}",
            paths[1].score
        );
    }

    #[test]
    fn same_kind_chains_are_not_enumerated_as_distinct_paths() {
        // ka|ka のような同 kind 分割は Plan（区間列）として同一なので、k-best に
        // 別経路として現れない。全体1辺だけが列挙される。
        let scored = vec![
            (edge(0, 2, SegmentKind::Japanese), 2.0),
            (edge(2, 4, SegmentKind::Japanese), 2.0),
            (edge(0, 4, SegmentKind::Japanese), 4.0),
        ];
        let paths = k_best(4, &scored, 3, &TransitionCosts::default());
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].steps.len(), 1);
        assert_eq!(paths[0].steps[0].0.range, SourceRange::new(0, 4));
    }

    #[test]
    fn equal_scores_break_ties_by_boundary_order_deterministically() {
        let costs = TransitionCosts::default();
        let scored = vec![
            (edge(0, 2, SegmentKind::Literal), 5.0),
            (edge(2, 4, SegmentKind::Japanese), 1.0),
            (edge(0, 4, SegmentKind::Japanese), 4.0),
        ];
        let first = k_best(4, &scored, 2, &costs);
        let second = k_best(4, &scored, 2, &costs);
        assert_eq!(first.len(), 2);
        // Literal→JA の切替ペナルティ込みで全体 JA と同スコア (4.0)。
        assert!((first[0].score - 4.0).abs() < 1e-9);
        assert!((first[1].score - 4.0).abs() < 1e-9);
        // 同スコアでも順序は固定。境界列の辞書順で (0,2)… が (0,4) より先。
        assert_eq!(first[0].ordering_key(), second[0].ordering_key());
        assert_eq!(first[0].steps.len(), 2);
        assert_eq!(first[1].steps.len(), 1);
    }

    #[test]
    fn gap_breaks_coverage() {
        let scored = vec![(edge(0, 2, SegmentKind::Japanese), 1.0)];
        let paths = k_best(4, &scored, 2, &TransitionCosts::default());
        assert!(paths.is_empty(), "2..4 を被覆する辺が無いので空");
    }
}
