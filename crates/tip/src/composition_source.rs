//! LocalKanaComposer の状態から mixed_input::CompositionSource を導出する（混在入力 PR2）。
//!
//! InputJournal が完全 composer-unit として保持する範囲だけが Typed（原文字列と
//! スタイルが分かる）。途中編集・部分確定の reseed 等で unit を失った読み範囲は
//! ResolvedKana として現在の読みを保持し、元入力の復元は失敗する。由来を証明
//! できない範囲の原文字を捏造しない（計画書 4.3）。ExplicitLiteral は利用者の
//! 明示指定（PR5）まで何も置かない。
//!
//! この導出は既存の original_input / replay_segments と同じ情報源だけを参照し、
//! composer の状態は書き換えない。既存のかな入力・削除・文節操作の契約は
//! ここでは変更しない。

use crate::local_kana_composer::{InputStyle, LocalKanaComposer};
use mixed_input::position::scalar_len;
use mixed_input::source::{CompositionSource, Provenance, SourceElement, SourceStyle};

/// composer の現在状態から元入力ソースを導出する。
pub(crate) fn build(composer: &LocalKanaComposer, revision: u64) -> CompositionSource {
    let reading = composer.reading().to_string();
    let reading_len = scalar_len(&reading);

    let mut source = CompositionSource::empty(revision);
    let mut covered_to = 0u32;
    for unit in composer.effective_input_units() {
        let (start, end) = (unit.start.0, unit.end.0);
        debug_assert!(start >= covered_to, "journal unit は昇順・非重複の前提");
        if start > covered_to {
            // unit のない読み範囲 = 由来を証明できない。読みを ResolvedKana として
            // 保持し、原文は復元しない。
            push_resolved(&mut source, &reading, covered_to, start);
        }
        // journal が保持するのは打鍵時の由来。engine 再送時の凍結（Direct 化、
        // adopt_conversion_reading の stable 作り直しなど）とは独立なので、読みを
        // 取り込んだ後も元打鍵が Kana だった unit は Kana のまま解析できる。
        let _ = source.push(SourceElement {
            provenance: Provenance::Typed {
                style: if unit.literal && unit.style == InputStyle::Kana {
                    SourceStyle::LiteralKana
                } else { to_source_style(unit.style) },
            },
            source_text: unit.original,
            reading: scalar_slice(&reading, start, end),
        });
        covered_to = end;
    }
    if covered_to < reading_len {
        push_resolved(&mut source, &reading, covered_to, reading_len);
    }
    source
}

fn push_resolved(source: &mut CompositionSource, reading: &str, start: u32, end: u32) {
    let text = scalar_slice(reading, start, end);
    let _ = source.push(SourceElement {
        provenance: Provenance::ResolvedKana,
        source_text: text.clone(),
        reading: text,
    });
}

fn scalar_slice(text: &str, start: u32, end: u32) -> String {
    text.chars()
        .skip(start as usize)
        .take((end - start) as usize)
        .collect()
}

fn to_source_style(style: InputStyle) -> SourceStyle {
    match style {
        InputStyle::Kana => SourceStyle::Kana,
        InputStyle::Direct => SourceStyle::Direct,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_kana_composer::InputStyle;
    use mixed_input::position::SourceRange;

    fn typed_elements(source: &CompositionSource) -> Vec<(String, String, SourceStyle)> {
        source
            .elements()
            .iter()
            .filter_map(|element| match element.provenance {
                Provenance::Typed { style } => {
                    Some((element.source_text.clone(), element.reading.clone(), style))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn backspace_reopen_keeps_the_caps_lock_original_of_the_frozen_tail() {
        // CapsLock の BC → Backspace。'b' はローマ字拒否で Direct リテラル凍結に
        // なり、Backspace の reopen で pending へ戻る。journal 凍結 unit の原文字
        // "B" を truncate 前に回収し、読み "b" を原文字として登録し直さない。
        let mut composer = LocalKanaComposer::default();
        composer.push_with_original('b', InputStyle::Kana, Some('B'));
        composer.push_with_original('c', InputStyle::Kana, Some('C'));
        composer.backspace();
        assert_eq!(composer.reading(), "b");
        assert_eq!(
            composer
                .original_input(
                    ipc::clause::ReadingPosition(0),
                    ipc::clause::ReadingPosition(1)
                )
                .as_deref(),
            Some("B")
        );
        let source = composer.composition_source(0);
        assert_eq!(source.source_text(), "B");
        assert_eq!(source.reading_text(), "b");
    }

    #[test]
    fn retained_pending_prefix_keeps_its_caps_lock_original() {
        // CapsLock の KY → retain_prefix("k")（文節末尾削除の読み prefix 保持と同じ
        // 経路）。保持する pending 部分の原文字は pending_originals から切り出し、
        // 読み "k" をそのまま原文字にしない。
        let mut composer = LocalKanaComposer::default();
        composer.push_with_original('k', InputStyle::Kana, Some('K'));
        composer.push_with_original('y', InputStyle::Kana, Some('Y'));
        assert!(composer.retain_prefix("k"));
        assert_eq!(composer.reading(), "k");
        assert_eq!(
            composer
                .original_input(
                    ipc::clause::ReadingPosition(0),
                    ipc::clause::ReadingPosition(1)
                )
                .as_deref(),
            Some("K")
        );
        // 破棄した側の原文字（Y）を残したままだと、続く打鍵の対応がずれて
        // 元入力が壊れる。保持直後でなく後続入力まで検証する。
        composer.push('a', InputStyle::Kana);
        assert_eq!(composer.reading(), "kあ");
        assert_eq!(
            composer
                .original_input(
                    ipc::clause::ReadingPosition(0),
                    ipc::clause::ReadingPosition(2)
                )
                .as_deref(),
            Some("Ka"),
            "破棄した Y が混入しない"
        );
    }

    #[test]
    fn reopen_after_partial_unit_delete_keeps_originals_unknown() {
        // CapsLock の KY → カーソル移動で凍結（1 unit）→ 末尾へ戻す → Backspace。
        // unit 内部の削除で journal unit が境界ごと落ち、reopen 範囲の原文字は
        // 証明できなくなる。以降の打鍵で unit 化しても原文字を捏造せず、
        // original_input は None（CompositionSource は ResolvedKana）を維持する。
        let mut composer = LocalKanaComposer::default();
        composer.push_with_original('k', InputStyle::Kana, Some('K'));
        composer.push_with_original('y', InputStyle::Kana, Some('Y'));
        assert!(composer.set_cursor(ipc::clause::ReadingPosition(0)));
        assert!(composer.set_cursor(ipc::clause::ReadingPosition(2)));
        composer.backspace();
        assert_eq!(composer.reading(), "k");
        assert_eq!(
            composer.original_input(
                ipc::clause::ReadingPosition(0),
                ipc::clause::ReadingPosition(1)
            ),
            None,
            "部分削除後の原文字は不明"
        );

        composer.push('a', InputStyle::Kana);
        assert_eq!(composer.reading(), "か");
        assert_eq!(
            composer.original_input(
                ipc::clause::ReadingPosition(0),
                ipc::clause::ReadingPosition(1)
            ),
            None,
            "再合成後も原文字不明を維持し、誤った Typed に戻さない"
        );
        let source = composer.composition_source(0);
        assert_eq!(source.reading_text(), "か");
        assert_eq!(
            source.elements()[0].provenance,
            mixed_input::source::Provenance::ResolvedKana
        );
    }

    #[test]
    fn reopen_across_straddling_units_keeps_originals_unknown() {
        // SH と WH を別々にカーソル移動で凍結し、末尾 h を削って reopen する。
        // reopen 範囲 [1,3) を SH unit（[0,2) を被覆）がまたぎ、journal.original の
        // 境界検証なしに回収すると「範囲外の S と欠落が文字数で相殺」して誤った
        // 原文字を既知にしてしまう。被覆が証明できない範囲は原文字不明を維持する。
        let mut composer = LocalKanaComposer::default();
        for (ch, original) in [('s', 'S'), ('h', 'H')] {
            composer.push_with_original(ch, InputStyle::Kana, Some(original));
        }
        assert!(composer.set_cursor(ipc::clause::ReadingPosition(0)));
        assert!(composer.set_cursor(ipc::clause::ReadingPosition(2)));
        for (ch, original) in [('w', 'W'), ('h', 'H')] {
            composer.push_with_original(ch, InputStyle::Kana, Some(original));
        }
        assert!(composer.set_cursor(ipc::clause::ReadingPosition(0)));
        assert!(composer.set_cursor(ipc::clause::ReadingPosition(4)));
        composer.backspace();
        assert_eq!(composer.reading(), "shw");
        assert_eq!(
            composer.original_input(
                ipc::clause::ReadingPosition(1),
                ipc::clause::ReadingPosition(3)
            ),
            None,
            "境界をまたぐ SH unit の原文字で穴埋めしない"
        );

        composer.push('a', InputStyle::Kana);
        assert_eq!(composer.reading(), "sふぁ");
        assert_eq!(
            composer.original_input(
                ipc::clause::ReadingPosition(1),
                ipc::clause::ReadingPosition(3)
            ),
            None,
            "再合成後も原文字不明を維持する"
        );
        let source = composer.composition_source(0);
        assert!(
            source
                .elements()
                .iter()
                .all(|element| element.provenance == mixed_input::source::Provenance::ResolvedKana),
            "ふぁ（と由来を失った先頭 s）は Typed に戻さない"
        );
    }

    #[test]
    fn caps_lock_passthrough_units_do_not_map_interior_positions() {
        // 未完の CapsLock KY を全体 Japanese として投影する。再合成 unit の読みは
        // "ky" だが、identity（内部位置の保存写像）は元打鍵と読みの同一文字列比較
        // で決めるため、"KY" ≠ "ky" の内部位置は対応させない。
        let mut composer = LocalKanaComposer::default();
        composer.push_with_original('k', InputStyle::Kana, Some('K'));
        composer.push_with_original('y', InputStyle::Kana, Some('Y'));
        let source = composer.composition_source(0);
        assert_eq!(source.source_text(), "KY");
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection = mixed_input::projection::Projection::build(0, &source, &plan).unwrap();
        assert_eq!(projection.reading(), "ky");
        assert_eq!(
            projection.source_to_reading(mixed_input::position::SourcePosition::new(1)),
            None,
            "元打鍵 KY と読み ky は同一文字列でないため内部は対応させない"
        );
    }

    #[test]
    fn caps_lock_mixed_boundary_splits_inside_a_unit_with_saved_original() {
        // CapsLock で PYTHON、解除して no。composer は pythonno を合成し、英字末尾の
        // N と no の n が1 unit（original "Nn" → 読み ん）になる。再合成は composer
        // へ渡した正規化済み入力（小文字）で行うため、Literal(PYTHON) +
        // Japanese(no) の Plan は unit 内部の境界で切れて読みは PYTHONの になる。
        let mut composer = LocalKanaComposer::default();
        for (ch, original) in [
            ('p', 'P'),
            ('y', 'Y'),
            ('t', 'T'),
            ('h', 'H'),
            ('o', 'O'),
            ('n', 'N'),
        ] {
            composer.push_with_original(ch, InputStyle::Kana, Some(original));
        }
        for ch in "no".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        assert_eq!(composer.reading(), "pyてょんお");
        let source = composer.composition_source(1);
        assert_eq!(source.source_text(), "PYTHONno");
        assert_eq!(source.reading_text(), composer.reading());

        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Literal, "PYTHON".to_string()),
                (SegmentKind::Japanese, "no".to_string()),
            ],
        )
        .unwrap();
        let projection = mixed_input::projection::Projection::build(9, &source, &plan)
            .expect("CapsLock 原文字を持つ unit 内部の言語境界で切れる");
        assert_eq!(projection.reading(), "PYTHONの");
    }

    #[test]
    fn caps_lock_typed_case_is_preserved_in_source_and_literal_projection() {
        // CapsLock で A,P,I を打鍵したかな入力。読み合成は小文字ローマ字のまま
        // （あぴ）で既存のかな経路を変えず、元入力（journal original）には実打鍵の
        // 大文字を保存する。Literal 区間は「API」を一字不動で保持する。
        let mut composer = LocalKanaComposer::default();
        for (ch, original) in [('a', 'A'), ('p', 'P'), ('i', 'I')] {
            composer.push_with_original(ch, InputStyle::Kana, Some(original));
        }
        assert_eq!(composer.reading(), "あぴ");
        assert_eq!(
            composer
                .original_input(
                    ipc::clause::ReadingPosition(0),
                    ipc::clause::ReadingPosition(2)
                )
                .as_deref(),
            Some("API")
        );

        let source = composer.composition_source(2);
        assert_eq!(source.source_text(), "API");
        assert_eq!(source.reading_text(), "あぴ");
        assert_eq!(
            source.original(SourceRange::new(0, 3)).as_deref(),
            Some("API")
        );

        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        let literal = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Literal, "API".to_string())],
        )
        .unwrap();
        let projection = mixed_input::projection::Projection::build(1, &source, &literal)
            .expect("Typed だけで被覆された Literal は構築できる");
        assert_eq!(projection.spans[0].reading_text, "API");
        assert_eq!(projection.display_surface(), "API");

        // 全体 Japanese の Plan でも読みは composer の canonical reading のまま。
        let whole = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection = mixed_input::projection::Projection::build(1, &source, &whole).unwrap();
        assert_eq!(projection.reading(), composer.reading());
    }

    #[test]
    fn caps_lock_tail_pending_n_finalizes_with_the_typed_original() {
        // 末尾未完 n の完成（finalize_pending_n）でも、unit の元打鍵には
        // CapsLock の実打鍵（N）を残す。
        let mut composer = LocalKanaComposer::default();
        composer.push_with_original('n', InputStyle::Kana, Some('N'));
        assert!(composer.finalize_pending_n());
        assert_eq!(composer.reading(), "ん");
        assert_eq!(
            composer
                .original_input(
                    ipc::clause::ReadingPosition(0),
                    ipc::clause::ReadingPosition(1)
                )
                .as_deref(),
            Some("N")
        );
    }

    #[test]
    fn typed_kana_input_keeps_originals_per_unit() {
        let mut composer = LocalKanaComposer::default();
        for ch in "kyouha".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        let source = composer.composition_source(1);
        assert_eq!(
            typed_elements(&source),
            vec![
                ("kyo".into(), "きょ".into(), SourceStyle::Kana),
                ("u".into(), "う".into(), SourceStyle::Kana),
                ("ha".into(), "は".into(), SourceStyle::Kana),
            ]
        );
        assert_eq!(source.source_text(), "kyouha");
        assert_eq!(source.reading_text(), "きょうは");
        assert_eq!(source.revision(), 1);
        // 全体が Typed なので解析対象は全体の1連続領域。
        assert_eq!(source.analyzable_ranges(), [SourceRange::new(0, 6)]);
    }

    #[test]
    fn direct_input_keeps_case_as_typed_direct() {
        let mut composer = LocalKanaComposer::default();
        for ch in "aA".chars() {
            composer.push(ch, InputStyle::Direct);
        }
        let source = composer.composition_source(0);
        assert_eq!(
            typed_elements(&source),
            vec![
                ("a".into(), "a".into(), SourceStyle::Direct),
                ("A".into(), "A".into(), SourceStyle::Direct),
            ],
            "一時英数・明示 Direct 打鍵は大小文字を保存する"
        );
    }

    #[test]
    fn mid_unit_deletion_downgrades_to_resolved_kana_without_fabrication() {
        // kyo → きょ → ょ削除 → き。残った き の原文（kyo / ki のどちらも）を
        // 捏造しない。後続の打鍵は新しい Typed として解析できる。
        let mut composer = LocalKanaComposer::default();
        for ch in "kyo".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.backspace();
        let source = composer.composition_source(0);
        assert_eq!(composer.reading(), "き");
        assert_eq!(source.elements().len(), 1);
        assert_eq!(source.elements()[0].provenance, Provenance::ResolvedKana);
        assert!(source.original(SourceRange::new(0, 1)).is_none());

        for ch in "au".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        let source = composer.composition_source(0);
        // ResolvedKana の「き」は読みがそのままソースに現れ、後続は元文字。
        assert_eq!(source.source_text(), "きau");
        assert_eq!(
            typed_elements(&source),
            vec![
                ("a".into(), "あ".into(), SourceStyle::Kana),
                ("u".into(), "う".into(), SourceStyle::Kana),
            ],
            "先頭の ResolvedKana 以外は Typed"
        );
        assert_eq!(source.analyzable_ranges(), [SourceRange::new(1, 3)]);
    }

    #[test]
    fn prefix_commit_resets_provenance_and_new_keys_are_typed() {
        let mut composer = LocalKanaComposer::default();
        for ch in "kyo".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.reseed_reading("きょう");
        let source = composer.composition_source(0);
        // reseed 残りは由来を証明できない（計画書 4.3: 部分確定の残り）。
        assert!(source
            .elements()
            .iter()
            .all(|element| { element.provenance == Provenance::ResolvedKana }));
        for ch in "ka".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        let source = composer.composition_source(0);
        // source_text は元打鍵（ka）。reseed 残りは読みがそのまま現れる。
        assert_eq!(source.source_text(), "きょうka");
        assert_eq!(
            typed_elements(&source),
            vec![("ka".into(), "か".into(), SourceStyle::Kana)]
        );
    }

    #[test]
    fn pending_unfinished_roman_is_typed_with_its_own_text() {
        let mut composer = LocalKanaComposer::default();
        // konn + n で「こんn」。末尾 n が未完了のまま見えている状態。
        for ch in "konnn".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        assert_eq!(composer.reading(), "こんn");
        let source = composer.composition_source(0);
        assert_eq!(
            typed_elements(&source),
            vec![
                ("ko".into(), "こ".into(), SourceStyle::Kana),
                ("nn".into(), "ん".into(), SourceStyle::Kana),
                ("n".into(), "n".into(), SourceStyle::Kana),
            ],
            "未完ローマ字 n も元文字が分かる Typed"
        );
        assert_eq!(source.source_text(), "konnn");
    }

    #[test]
    fn effective_units_agree_with_original_input_on_every_boundary_pair() {
        // 由来视图と既存の original_input は同じ journal が正本。unit 境界の全組で
        // 復元結果が一致することを確認する（互換の維持条件）。
        let mut composer = LocalKanaComposer::default();
        for ch in "kyounn".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.push('A', InputStyle::Direct);
        composer.backspace(); // A を削る（stable 末尾の Direct unit が消える）
        composer.push('i', InputStyle::Kana);
        // kyo unit の途中（き|ょ）へ挿入し、unit を失う範囲を作る。
        assert!(composer.set_cursor(ipc::clause::ReadingPosition(1)));
        composer.push('a', InputStyle::Kana);

        let source = composer.composition_source(0);
        assert!(source.elements()[0].provenance == Provenance::ResolvedKana);
        let layout = source.layout();
        for (index, start) in layout.iter().enumerate() {
            for end in layout.iter().skip(index) {
                let via_original = composer.original_input(
                    ipc::clause::ReadingPosition(start.reading.start.get()),
                    ipc::clause::ReadingPosition(end.reading.end.get()),
                );
                let via_source = source.original(SourceRange::new(
                    start.source.start.get(),
                    end.source.end.get(),
                ));
                assert_eq!(
                    via_original,
                    via_source,
                    "reading [{}, {}) で original_input と CompositionSource の復元が不一致",
                    start.reading.start.get(),
                    end.reading.end.get()
                );
            }
        }
    }

    #[test]
    fn projection_splits_language_boundaries_inside_composer_units() {
        // 実 composer で pythonno を打つと、末尾の nn（python の n + no の n）が
        // 1 unit になる。Literal(python) + Japanese(no) の Plan は unit 内部で切れて
        // もよく、Japanese 側の読みは区間原文 no → の へ再合成する。
        let mut composer = LocalKanaComposer::default();
        for ch in "pythonno".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        let source = composer.composition_source(3);
        assert_eq!(source.source_text(), "pythonno");
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Literal, "python".to_string()),
                (SegmentKind::Japanese, "no".to_string()),
            ],
        )
        .unwrap();
        let projection = mixed_input::projection::Projection::build(7, &source, &plan)
            .expect("composer-unit 内部の言語境界で切れる");
        assert_eq!(projection.reading(), "pythonの");
        assert_eq!(projection.source_revision, 3, "revision は source 由来");
    }

    #[test]
    fn projection_after_reading_adoption_keeps_kana_provenance_at_language_boundaries() {
        // adopt_conversion_reading は stable を再送用 Direct で作り直すが、journal
        // が保持するのは打鍵時の由来。取り込み後に Literal/Japanese の言語境界で
        // composer-unit（nn → ん）の内部を分けても、Direct 扱いによる同じ scalar
        // オフセットでの読み切り出しで「ん」が欠落しない。
        let mut composer = LocalKanaComposer::default();
        for ch in "pythonnohon".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        assert_eq!(composer.reading(), "pyてょんおほn");
        assert!(composer.adopt_conversion_reading("pyてょんおほん"));

        let source = composer.composition_source(7);
        assert_eq!(source.source_text(), "pythonnohon");
        assert_eq!(source.reading_text(), "pyてょんおほん");
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Literal, "python".to_string()),
                (SegmentKind::Japanese, "nohon".to_string()),
            ],
        )
        .unwrap();
        let projection = mixed_input::projection::Projection::build(7, &source, &plan)
            .expect("読み取り込み後のソースでも unit 内部の言語境界で切れる");
        assert_eq!(projection.reading(), "pythonのほん");
        assert_eq!(projection.source_revision, 7, "revision は source 由来");

        // 全体 Japanese の Plan でも取り込み後の canonical reading を保つ。
        let whole = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection = mixed_input::projection::Projection::build(7, &source, &whole)
            .expect("全体 Japanese の Plan は構築できる");
        assert_eq!(projection.reading(), composer.reading());
    }

    #[test]
    fn projection_preserves_reading_of_a_kana_unit_with_rewritten_original() {
        // 物理 `-` は to_kana_reading_char で `ー` として composer へ渡り、
        // preserve_last_literal_original('-') が journal の元打鍵を "-" へ書き換える。
        // source_text "-" の再合成は "ー" に整合しないので、Projection は保存済みの
        // 読み "ー" を引き継ぐ（canonical reading を "-" の passthrough で壊さない）。
        let mut composer = LocalKanaComposer::default();
        composer.push('ー', InputStyle::Kana);
        composer.preserve_last_literal_original('-');
        let source = composer.composition_source(0);
        assert_eq!(source.source_text(), "-");
        assert_eq!(source.reading_text(), "ー");

        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection = mixed_input::projection::Projection::build(0, &source, &plan)
            .expect("全体 Japanese の Plan は構築できる");
        assert_eq!(projection.reading(), "ー");
    }

    #[test]
    fn projection_preserves_canonical_reading_for_wa_rudo_with_prolonged_sound() {
        // wa-rudo（物理キー、`-` は長音符へ写像）→ わーるど。`-` を含む実入力が
        // composer → CompositionSource → Projection でずれないことの統合確認。
        let mut composer = LocalKanaComposer::default();
        for ch in "wa-rudo".chars() {
            // key_event_sink と同じ打鍵作法: to_kana_reading_char で積む文字を決め、
            // 写像が変わったときだけ元打鍵を保存する。
            let mapped = crate::input_state::to_kana_reading_char(ch);
            composer.push(mapped, InputStyle::Kana);
            if mapped != ch {
                composer.preserve_last_literal_original(ch);
            }
        }
        assert_eq!(composer.reading(), "わーるど");
        let source = composer.composition_source(0);
        assert_eq!(source.source_text(), "wa-rudo");
        assert_eq!(source.reading_text(), "わーるど");

        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection = mixed_input::projection::Projection::build(0, &source, &plan)
            .expect("全体 Japanese の Plan は構築できる");
        assert_eq!(projection.reading(), composer.reading());
    }

    #[test]
    fn whole_japanese_projection_preserves_the_reading_across_provenance() {
        // 途中編集で ResolvedKana に落ちた読みと、完成済み ん・未完 n が混ざっても、
        // 全体 Japanese の Projection は composer の canonical reading を保つ。
        // 再合成が保存済みの読み（ResolvedKana）を元打鍵として再解釈したり、
        // finalize_pending_n 済みの ん を未完 n へ戻したりしないことの統合確認。
        let mut composer = LocalKanaComposer::default();
        for ch in "kyo".chars() {
            composer.push(ch, InputStyle::Kana);
        }
        composer.backspace(); // き → ResolvedKana
        for ch in "honn".chars() {
            composer.push(ch, InputStyle::Kana);
        } // ほん（nn は完成済み unit）
        composer.push('n', InputStyle::Kana); // 末尾は未完 n
        assert_eq!(composer.reading(), "きほんn");
        let source = composer.composition_source(0);
        assert_eq!(source.reading_text(), composer.reading());
        use mixed_input::plan::{InterpretationPlan, SegmentKind};
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection = mixed_input::projection::Projection::build(0, &source, &plan)
            .expect("全体 Japanese の Plan は構築できる");
        assert_eq!(projection.reading(), composer.reading());
    }

    #[test]
    fn reading_resynthesis_matches_the_composer_for_straight_typing() {
        // Plan 適用前の composition と、全入力を読み再合成した結果は一致する
        // （straight 打鍵に限る。途中編集で由来が ResolvedKana に落ちた状態は
        // 再合成の前提が変わるため対象外）。
        for input in [
            "konnichiha",
            "gakkou",
            "ryokou",
            "xya",
            "watashi。",
            "caceco",
            "qwe",
            "sye",
            "wyi",
            "xn",
            "zl",
            "n。",
            "nn",
            "hon",
            "konnn",
            "pythonno",
        ] {
            let mut composer = LocalKanaComposer::default();
            for ch in input.chars() {
                composer.push(ch, InputStyle::Kana);
            }
            let units = mixed_input::roman::synthesize(input, false);
            let reading: String = units.iter().map(|unit| unit.kana.as_str()).collect();
            assert_eq!(reading, composer.reading(), "input={input:?}");
            let originals: String = units.iter().map(|unit| unit.original.as_str()).collect();
            assert_eq!(originals, input, "unit 元打鍵の連結が入力と一致: {input:?}");
        }
    }
}
