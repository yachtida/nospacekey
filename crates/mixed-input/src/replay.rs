//! 打鍵列再生の骨格。PR1 は scalar 単位の素朴な編集だけを持つ。
//! 本物の由来管理（`ResolvedKana`、かな削除・未完ローマ字契約）は PR2 の
//! `CompositionSource` が担う。その置き換え時に、打鍵列が宣言ソースを再現できる
//! という契約（E_REPLAY_CONSISTENCY）を引き継ぐこと。

use crate::episode::EpisodeEvent;
use crate::plan::SegmentKind;
use crate::position::SourceRange;

#[derive(Debug)]
pub struct ReplayError(pub String);

#[derive(Debug, Default)]
pub struct ReplayTrace {
    pub final_pending: String,
    /// commit 済み接頭辞の scalar 数。
    pub committed_total: u32,
    /// commit 済み接頭辞の内容。E_REPLAY_CONSISTENCY は総量ではなく
    /// この内容まで含めて宣言ソースと一致を見る。
    pub committed_source: String,
    /// CommitPrefix が起きた絶対 scalar 位置（commit fence 検査用）。
    pub commit_boundaries: Vec<u32>,
    /// 未確定側で効力を持つ明示変更（絶対 scalar 位置）。後の指定は重なる部分だけを
    /// 置き換え、編集に追従し、Escape で解除される。検証は最後の1件では
    /// なくこの全件を要求する。
    pub reinterpretations: Vec<(SourceRange, SegmentKind)>,
    /// 確定済み区間へ分離された明示変更。確定済み文面はもう編集・Escape の対象に
    /// ならないため、未確定側の解除で消さず、検証の照合対象に残す。
    pub committed_reinterpretations: Vec<(SourceRange, SegmentKind)>,
}

fn byte_offset_at_scalar(s: &str, scalar_index: usize) -> usize {
    s.char_indices()
        .nth(scalar_index)
        .map(|(byte, _)| byte)
        .unwrap_or(s.len())
}

/// 明示変更の集合へ新しい指定を反映する。重なる部分だけを後の指定が置き換え、
/// 重ならない両端は従来の指定が生きる（「該当区間だけ直す」契約・計画書 2.1）。
fn apply_reinterpret(
    active: &mut Vec<(SourceRange, SegmentKind)>,
    range: SourceRange,
    kind: SegmentKind,
) {
    let (new_start, new_end) = (range.start.get(), range.end.get());
    let mut flanks: Vec<(SourceRange, SegmentKind)> = Vec::new();
    active.retain(|(existing, existing_kind)| {
        let (start, end) = (existing.start.get(), existing.end.get());
        if end <= new_start || start >= new_end {
            return true;
        }
        if start < new_start {
            flanks.push((SourceRange::new(start, new_start), *existing_kind));
        }
        if new_end < end {
            flanks.push((SourceRange::new(new_end, end), *existing_kind));
        }
        false
    });
    active.append(&mut flanks);
    active.push((range, kind));
    active.sort_by_key(|(existing, _)| existing.start.get());
}

/// [at, at + len) への挿入を明示変更へ反映する。手前の挿入は位置をずらし、
/// 内側の挿入は範囲を伸ばす。右端と外側の挿入は動かさない。
fn shift_active_for_insert(active: &mut [(SourceRange, SegmentKind)], at: u32, len: u32) {
    for (range, _) in active {
        let (start, end) = (range.start.get(), range.end.get());
        let (start, end) = if at <= start {
            (start + len, end + len)
        } else if at < end {
            (start, end + len)
        } else {
            (start, end)
        };
        *range = SourceRange::new(start, end);
    }
}

/// [d0, d1) の削除を明示変更へ反映する。手前の削除で位置をずらし、重なる
/// 指定は生き残った文字へ縮め、文字を失った指定は解除する。
fn shift_active_for_delete(active: &mut Vec<(SourceRange, SegmentKind)>, d0: u32, d1: u32) {
    let width = d1 - d0;
    active.retain_mut(|(range, _)| {
        let (start, end) = (range.start.get(), range.end.get());
        if end <= d0 {
            true
        } else if start >= d1 {
            *range = SourceRange::new(start - width, end - width);
            true
        } else {
            let removed = end.min(d1) - start.max(d0);
            let new_start = start.min(d0);
            let kept = (end - start) - removed;
            if kept == 0 {
                false
            } else {
                *range = SourceRange::new(new_start, new_start + kept);
                true
            }
        }
    });
}

/// 確定境界で明示変更を確定済み側と未確定側へ分ける。境界をまたぐ指定は
/// 境界で2つに割る（同じ解釈が両側に続く）。以後の Escape・編集は未確定側
/// だけを対象にする。
fn split_committed_at(
    active: &mut Vec<(SourceRange, SegmentKind)>,
    committed: &mut Vec<(SourceRange, SegmentKind)>,
    boundary: u32,
) {
    active.retain_mut(|(range, kind)| {
        let (start, end) = (range.start.get(), range.end.get());
        if end <= boundary {
            committed.push((*range, *kind));
            false
        } else if start < boundary {
            committed.push((SourceRange::new(start, boundary), *kind));
            *range = SourceRange::new(boundary, end);
            true
        } else {
            true
        }
    });
}

/// 打鍵列を素朴な source buffer に適用し、最終 pending を導く。
/// caret は未確定領域内の絶対 scalar 位置として扱う。
pub fn replay(events: &[EpisodeEvent]) -> Result<ReplayTrace, ReplayError> {
    let mut trace = ReplayTrace::default();
    let mut pending = String::new();
    // pending 先頭からの scalar 位置。
    let mut caret: usize = 0;
    for event in events {
        match event {
            EpisodeEvent::Type { text } => {
                let byte = byte_offset_at_scalar(&pending, caret);
                pending.insert_str(byte, text);
                shift_active_for_insert(
                    &mut trace.reinterpretations,
                    trace.committed_total + caret as u32,
                    text.chars().count() as u32,
                );
                caret += text.chars().count();
            }
            EpisodeEvent::DeleteBackward { units } => {
                let units = *units as usize;
                if caret < units {
                    return Err(ReplayError(format!(
                        "先頭を超えて {units} scalar 削除しようとした"
                    )));
                }
                let from = byte_offset_at_scalar(&pending, caret - units);
                let to = byte_offset_at_scalar(&pending, caret);
                pending.replace_range(from..to, "");
                shift_active_for_delete(
                    &mut trace.reinterpretations,
                    trace.committed_total + (caret - units) as u32,
                    trace.committed_total + caret as u32,
                );
                caret -= units;
            }
            EpisodeEvent::MoveTo { position } => {
                let len = pending.chars().count() as u32;
                if *position < trace.committed_total || *position > trace.committed_total + len {
                    return Err(ReplayError(format!(
                        "caret 位置 {position} が未確定範囲 [{}..{}] 外",
                        trace.committed_total,
                        trace.committed_total + len
                    )));
                }
                caret = (*position - trace.committed_total) as usize;
            }
            EpisodeEvent::SelectCandidate { .. } => {}
            EpisodeEvent::Reinterpret { range, as_kind } => {
                let len = pending.chars().count() as u32;
                if range.start.get() < trace.committed_total
                    || range.end.get() > trace.committed_total + len
                {
                    return Err(ReplayError(format!(
                        "明示変更範囲 {:?} が未確定範囲外",
                        range
                    )));
                }
                apply_reinterpret(&mut trace.reinterpretations, *range, *as_kind);
            }
            EpisodeEvent::CommitAll => {
                trace.committed_source.push_str(&pending);
                trace.committed_total += pending.chars().count() as u32;
                pending.clear();
                caret = 0;
                split_committed_at(
                    &mut trace.reinterpretations,
                    &mut trace.committed_reinterpretations,
                    trace.committed_total,
                );
            }
            EpisodeEvent::CommitPrefix { scalars } => {
                let scalars_usize = *scalars as usize;
                let len = pending.chars().count();
                if scalars_usize > len {
                    return Err(ReplayError(format!(
                        "未確定 {len} scalar より多く {scalars_usize} 確定しようとした"
                    )));
                }
                let byte = byte_offset_at_scalar(&pending, scalars_usize);
                trace.committed_source.push_str(&pending[..byte]);
                pending.replace_range(0..byte, "");
                trace.committed_total += *scalars;
                trace.commit_boundaries.push(trace.committed_total);
                caret = caret.saturating_sub(scalars_usize);
                split_committed_at(
                    &mut trace.reinterpretations,
                    &mut trace.committed_reinterpretations,
                    trace.committed_total,
                );
            }
            EpisodeEvent::Escape => {
                // 曖昧例の Escape は解釈を既定へ戻す（計画書 8.2）。未確定側の
                // 明示変更を全件解除する。確定済み側は commit 時に分離済みで、
                // 利用者が確定した選択なので Escape では消さない。
                trace.reinterpretations.clear();
            }
            EpisodeEvent::Annotate { .. } => {}
        }
    }
    trace.final_pending = pending;
    Ok(trace)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace_of(specs: &[&str]) -> ReplayTrace {
        let events = specs
            .iter()
            .map(|spec| EpisodeEvent::parse(spec).expect("イベントを解析できる"))
            .collect::<Vec<_>>();
        replay(&events).expect("再生できる")
    }

    #[test]
    fn reinterpret_overwrites_only_the_overlapping_part() {
        let trace = trace_of(&[
            "type:abcdef",
            "reinterpret:0:6:literal",
            "reinterpret:2:4:japanese",
        ]);
        assert_eq!(
            trace.reinterpretations,
            [
                (SourceRange::new(0, 2), SegmentKind::Literal),
                (SourceRange::new(2, 4), SegmentKind::Japanese),
                (SourceRange::new(4, 6), SegmentKind::Literal),
            ]
        );
    }

    #[test]
    fn disjoint_reinterprets_all_stay_effective() {
        let trace = trace_of(&[
            "type:madeare",
            "reinterpret:0:4:literal",
            "reinterpret:4:7:literal",
        ]);
        assert_eq!(trace.reinterpretations.len(), 2);
    }

    #[test]
    fn delete_before_reinterpret_shifts_its_range() {
        let trace = trace_of(&[
            "type:nomade",
            "reinterpret:2:6:literal",
            "move_to:1",
            "delete_backward:1",
        ]);
        assert_eq!(trace.final_pending, "omade");
        assert_eq!(
            trace.reinterpretations,
            [(SourceRange::new(1, 5), SegmentKind::Literal)]
        );
    }

    #[test]
    fn delete_inside_reinterpret_shrinks_it() {
        let trace = trace_of(&[
            "type:nomade",
            "reinterpret:2:6:literal",
            "move_to:6",
            "delete_backward:1",
        ]);
        assert_eq!(trace.final_pending, "nomad");
        assert_eq!(
            trace.reinterpretations,
            [(SourceRange::new(2, 5), SegmentKind::Literal)]
        );
    }

    #[test]
    fn deleting_the_whole_reinterpret_releases_it() {
        let trace = trace_of(&[
            "type:nomade",
            "reinterpret:2:6:literal",
            "move_to:6",
            "delete_backward:4",
        ]);
        assert!(trace.reinterpretations.is_empty());
    }

    #[test]
    fn insert_inside_reinterpret_extends_it() {
        let trace = trace_of(&[
            "type:nomade",
            "reinterpret:2:6:literal",
            "move_to:4",
            "type:xy",
        ]);
        assert_eq!(trace.final_pending, "nomaxyde");
        assert_eq!(
            trace.reinterpretations,
            [(SourceRange::new(2, 8), SegmentKind::Literal)]
        );
    }

    #[test]
    fn commit_prefix_keeps_the_committed_text() {
        let trace = trace_of(&["type:kore", "commit_prefix:2", "type:!"]);
        assert_eq!(trace.committed_source, "ko");
        assert_eq!(trace.committed_total, 2);
        assert_eq!(trace.commit_boundaries, [2]);
        assert_eq!(trace.final_pending, "re!");
    }

    #[test]
    fn escape_releases_all_reinterprets() {
        let trace = trace_of(&["type:made", "reinterpret:0:4:literal", "escape"]);
        assert!(trace.reinterpretations.is_empty());
    }

    #[test]
    fn commit_prefix_moves_reinterprets_to_the_committed_side() {
        let trace = trace_of(&["type:madeno", "reinterpret:0:4:literal", "commit_prefix:4"]);
        assert_eq!(
            trace.committed_reinterpretations,
            [(SourceRange::new(0, 4), SegmentKind::Literal)]
        );
        assert!(trace.reinterpretations.is_empty());
    }

    #[test]
    fn commit_prefix_splits_a_reinterpret_spanning_the_boundary() {
        let trace = trace_of(&["type:madeno", "reinterpret:0:6:literal", "commit_prefix:4"]);
        assert_eq!(
            trace.committed_reinterpretations,
            [(SourceRange::new(0, 4), SegmentKind::Literal)]
        );
        assert_eq!(
            trace.reinterpretations,
            [(SourceRange::new(4, 6), SegmentKind::Literal)]
        );
    }

    #[test]
    fn commit_all_moves_all_reinterprets_to_the_committed_side() {
        let trace = trace_of(&["type:made", "reinterpret:0:4:literal", "commit_all"]);
        assert_eq!(
            trace.committed_reinterpretations,
            [(SourceRange::new(0, 4), SegmentKind::Literal)]
        );
        assert!(trace.reinterpretations.is_empty());
    }

    #[test]
    fn escape_keeps_the_committed_side() {
        let trace = trace_of(&[
            "type:madeno",
            "reinterpret:0:4:literal",
            "commit_prefix:4",
            "escape",
        ]);
        assert_eq!(
            trace.committed_reinterpretations,
            [(SourceRange::new(0, 4), SegmentKind::Literal)]
        );
        assert!(trace.reinterpretations.is_empty());
    }
}
