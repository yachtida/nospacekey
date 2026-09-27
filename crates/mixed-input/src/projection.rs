//! 解釈 Plan の CompositionSource への適用結果（計画書 4.4）。
//!
//! `Projection` は Plan から作った読み・区間・表示への対応を持ち、
//! `plan_id + source_revision` で同一性を保証する。混在候補は文字列だけではなく
//! この Projection ごと採用する（計画書 3-3）。PR2 の時点では表示文字列は
//! 「Japanese=読み・Literal=原文」の等価な初期値で、変換結果の注入は PR3 が行う。
//!
//! Plan の区間境界は CompositionSource の要素（composer-unit）境界と独立に置ける。
//! 編集時の由来は要素が保証し、解釈の読みは元打鍵の再合成が読みに整合する Kana
//! 打鍵の連続範囲だけ区間の原文から `roman::synthesize` で再合成する。
//! `ResolvedKana`・Direct と、整合しない Kana unit（`-`→`ー` の打鍵作法変換）の
//! 保存済み読みは再解釈しない。例えば `pythonno` は composer が末尾の `nn` を
//! 1 unit にするが、Literal(python) + Japanese(no) の Plan は unit 内部で切って
//! `no` → `の` を得る。

use crate::plan::{InterpretationPlan, SegmentKind};
use crate::position::{
    scalar_len, utf16_len, DisplayUtf16Position, ReadingPosition, ReadingRange, SourcePosition,
    SourceRange,
};
use crate::roman;
use crate::source::{CompositionSource, ElementLayout, Provenance, SourceStyle};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionError {
    /// Plan の span 列がソースを過不足なく被覆しない。
    CoverMismatch(String),
    /// Literal span の元文字が復元できない（`ResolvedKana` を含む）。
    LiteralOriginalUnknown { span_index: usize },
    /// Literal span text が復元した原文と一字不動でない。
    LiteralTextMismatch { span_index: usize },
    /// 利用者明示の `ExplicitLiteral` 要素が Japanese span に含まれている。
    ExplicitLiteralNotKept { span_index: usize },
    /// 保存済み読みを引き継ぐ要素のうち元入力と読みが一致しない unit の内部に
    /// 区間境界が入り、source / reading の対応を構築できない。
    SavedReadingNotSplittable { span_index: usize },
}

/// span 内の1つの source / reading 対応単位。composer-unit とは独立に、Literal は
/// 区間全体で1単位、Japanese は読み再合成の unit 粒度で置く。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PieceProjection {
    pub source: SourceRange,
    pub reading: ReadingRange,
    /// 元打鍵と読みが同一文字列で、内部位置の保存写像を保証できるか。Literal span は
    /// 全体で true。Japanese span は unit ごとに `原文 == 読み` かで決まる
    /// （`va → ゔぁ` のような同長でも非同一の unit は false。計画書 3-2）。
    pub identity: bool,
    /// 対応単位の元入力の由来（PR3 の採用が composer/journal へ反映する方法を決める）。
    /// KanaRun: Kana 打鍵の再合成 run。Inherited: 保存読みの引き継ぎ（Direct 打鍵、
    /// または再合成が読みに整合しない Kana unit — 再合成 run の境界）。Unknown:
    /// 元打鍵不明（ResolvedKana）。
    pub provenance: PieceProvenance,
}

/// 対応単位の元入力の由来。`Inherited { direct }` の direct は journal への
/// 登録スタイル（true=Direct）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PieceProvenance {
    KanaRun,
    Inherited { direct: bool },
    Unknown,
}

/// Projection を構成する1区間。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProjectionSpan {
    pub kind: SegmentKind,
    pub source: SourceRange,
    pub reading: ReadingRange,
    /// Japanese: ソース上の文字列。Literal: 原文。
    pub source_text: String,
    /// Japanese: この区間の読み（原文から再合成）。Literal: 原文（一字不動）。
    pub reading_text: String,
    /// 表示文字列。PR2 では reading_text と同一。PR3 で変換結果が入る。
    pub display_text: String,
    pub pieces: Vec<PieceProjection>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Projection {
    pub plan_id: u64,
    pub source_revision: u64,
    pub spans: Vec<ProjectionSpan>,
}

/// 採用した Plan を composer（編集状態）へ反映するための unit（PR3）。
/// kind が journal への反映方法を決める:
/// - Literal: 原文 1 unit。journal へは**文字ごと**の Direct unit で登録する
///   （1文字削除で境界をまたぐ unit 全体を落とし、残りの原入力対応を失わないため）。
/// - Kana: Kana 打鍵の再合成 unit。journal へ Kana として登録。
/// - Direct: 再合成を分断する境界（Direct 打鍵の保存読み）。journal へ Direct で登録し、
///   隣接する Kana unit との再合成 run への混入を防ぐ。
/// - Unknown: 元打鍵不明（ResolvedKana 由来）。stable へは載せるが journal へは
///   登録しない（原入力の捏造を防ぐ — original_input は None を維持）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AdoptionUnit {
    /// この unit の読み。composer の stable にそのまま載る。
    pub reading: String,
    /// 元打鍵（journal の original）。Unknown では使わない。
    pub original: String,
    pub kind: AdoptionKind,
    /// 採用前ソース上の対応範囲（絶対座標）。composer が採用前の編集状態
    /// （凍結範囲・未完 pending）との対応を文字列推測でなく座標で引き継ぐために使う。
    pub source: SourceRange,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AdoptionKind {
    Literal,
    Kana,
    Direct,
    Unknown,
}

impl Projection {
    /// Plan を CompositionSource へ適用する。
    ///
    /// - span 列はソースを過不足なく被覆すること
    /// - Literal span の元文字は復元可能で span text と一字不動であること
    /// - `ExplicitLiteral` 要素は Literal span に含まれること（明示指定は自動推定に
    ///   勝つ、計画書 2.1）
    /// - Japanese span の読みは、元打鍵の再合成が保存済みの読みに整合する Kana
    ///   打鍵の連続範囲だけ、区間の原文から再合成する。区間境界は composer-unit の
    ///   内部に置ける。それ以外の読み（`ResolvedKana`、Direct、整合しない Kana
    ///   unit）は再解釈せず引き継ぎ、再合成はその境界をまたがない。元入力と読みが
    ///   一致しない引き継ぎ unit の内部に境界が入る場合は
    ///   `SavedReadingNotSplittable` で失敗する（文字を欠落させて成功させない）。
    ///   末尾の未完 `n` の完成はソース側の unit 状態に従う（計画書 5.1）。読みが
    ///   既に ん の unit は完成を引き継ぎ、未完の `n` は Plan の終端状態として
    ///   未完のまま残す
    ///
    /// `source_revision` は引数ではなく `source.revision()` から設定する
    /// （plan_id + source_revision による同一性を構築時に歪めない）。
    pub fn build(
        plan_id: u64,
        source: &CompositionSource,
        plan: &InterpretationPlan,
    ) -> Result<Self, ProjectionError> {
        let source_text = source.source_text();
        let source_len = scalar_len(&source_text);
        let layout = source.layout();

        let mut spans = Vec::with_capacity(plan.spans.len());
        let mut source_at = 0u32;
        let mut reading_at = 0u32;
        for (index, span) in plan.spans.iter().enumerate() {
            if span.range.start.get() != source_at {
                return Err(ProjectionError::CoverMismatch(format!(
                    "span {index} の開始 {} が期待位置 {source_at} と違う",
                    span.range.start.get()
                )));
            }
            if span.range.slice(&source_text) != span.text {
                return Err(ProjectionError::CoverMismatch(format!(
                    "span {index} の text がソースと一致しない"
                )));
            }
            // span と重なる要素。由来の検証（ResolvedKana / ExplicitLiteral）と、
            // 由来・入力スタイルごとの読みの作り分けに使う。Kana 打鍵範囲の再合成
            // では unit の分割に要素の読みを使わない（unit 内部で切れるため）。
            let covered: Vec<(&ElementLayout, &crate::source::SourceElement)> = layout
                .iter()
                .zip(source.elements())
                .filter(|(entry, _)| {
                    entry.source.start.get() < span.range.end.get()
                        && span.range.start.get() < entry.source.end.get()
                })
                .collect();

            let (reading_text, pieces): (String, Vec<PieceProjection>) = match span.kind {
                SegmentKind::Literal => {
                    if covered
                        .iter()
                        .any(|(entry, _)| entry.provenance == Provenance::ResolvedKana)
                    {
                        return Err(ProjectionError::LiteralOriginalUnknown { span_index: index });
                    }
                    let original = source.original(span.range).unwrap_or_default();
                    if original != span.text {
                        return Err(ProjectionError::LiteralTextMismatch { span_index: index });
                    }
                    // 読みは原文そのもの。区間全体が内部位置まで一致する1単位になる。
                    (
                        span.text.clone(),
                        vec![PieceProjection {
                            source: span.range,
                            reading: ReadingRange::new(
                                reading_at,
                                reading_at + scalar_len(&span.text),
                            ),
                            identity: true,
                            provenance: PieceProvenance::Inherited { direct: true },
                        }],
                    )
                }
                SegmentKind::Japanese => {
                    if covered
                        .iter()
                        .any(|(entry, _)| entry.provenance == Provenance::ExplicitLiteral)
                    {
                        return Err(ProjectionError::ExplicitLiteralNotKept { span_index: index });
                    }
                    // 由来・入力スタイルごとに読みを作る。再合成は「元打鍵の再合成が
                    // 保存済みの読みに整合する」Kana 打鍵の連続範囲に限る。それ以外
                    // （ResolvedKana、Direct、整合しない Kana unit）は保存済みの読み
                    // を引き継ぎ、再合成がその境界をまたがない。
                    let mut reading_text = String::new();
                    let mut pieces: Vec<PieceProjection> = Vec::new();
                    let mut piece_source_at = span.range.start.get();
                    let mut run = String::new();
                    let mut run_tail_completed = false;
                    for (entry, element) in &covered {
                        let clip_start = entry.source.start.get().max(span.range.start.get());
                        let clip_end = entry.source.end.get().min(span.range.end.get());
                        let clip = |text: &str| {
                            scalar_slice(
                                text,
                                clip_start - entry.source.start.get(),
                                clip_end - entry.source.start.get(),
                            )
                        };
                        match element.provenance {
                            Provenance::Typed {
                                style: SourceStyle::Kana,
                            } if resynthesizes_to_reading(
                                &element.source_text,
                                &element.reading,
                            ) =>
                            {
                                // run には元打鍵（大小文字を含む）を積む。再合成は
                                // push_kana_run が composer 入力（ASCII 小文字化）で
                                // 行い、identity は元打鍵側の文字列で比較する。末尾
                                // unit の完成状態だけ記録する。
                                run.push_str(&clip(&element.source_text));
                                run_tail_completed = element.reading.ends_with('ん');
                            }
                            Provenance::ExplicitLiteral => {
                                unreachable!("ExplicitLiteral は先に拒否している")
                            }
                            // 保存済みの読みを引き継ぐ。ResolvedKana の source_text は
                            // 現在の読み、Direct の打鍵は読みに直す前の文字、Kana でも
                            // 元打鍵の再合成が読みに整合しない unit は composer 入力が
                            // 読みと同じ1文字だった（resynthesizes_to_reading 参照）。
                            _ => {
                                // 読みの切り出しは source 側の scalar オフセットをそのまま
                                // 使う。元入力と読みが一致しない unit（nn → ん など）の内部
                                // では同じ位置が別の文字に当たり、読みを欠落させてしまう。
                                // 欠落したまま成功させるより明示的なエラーにする。
                                let interior = clip_start > entry.source.start.get()
                                    || clip_end < entry.source.end.get();
                                if interior && element.source_text != element.reading {
                                    return Err(ProjectionError::SavedReadingNotSplittable {
                                        span_index: index,
                                    });
                                }
                                piece_source_at += push_kana_run(
                                    &mut run,
                                    &mut run_tail_completed,
                                    piece_source_at,
                                    reading_at + scalar_len(&reading_text),
                                    &mut reading_text,
                                    &mut pieces,
                                );
                                let source_clip = clip(&element.source_text);
                                // unit 全体の引き継ぎでは読みを source 側 scalar
                                // オフセットで切らない。読みが元入力より長い unit
                                // （契約上排除されていない）で、保存済み読みを切り
                                // 詰めて成功させない。内部切断が許されるのは
                                // source_text == reading が検証済みのときだけなので、
                                // そのとき clip と同一になる。
                                let reading_clip = if interior {
                                    clip(&element.reading)
                                } else {
                                    element.reading.clone()
                                };
                                let source_len = scalar_len(&source_clip);
                                let reading_len = scalar_len(&reading_clip);
                                let reading_cursor = reading_at + scalar_len(&reading_text);
                                pieces.push(PieceProjection {
                                    source: SourceRange::new(
                                        piece_source_at,
                                        piece_source_at + source_len,
                                    ),
                                    reading: ReadingRange::new(
                                        reading_cursor,
                                        reading_cursor + reading_len,
                                    ),
                                    identity: source_clip == reading_clip,
                                    // 引き継ぎ unit の journal 登録スタイルは元の要素に
                                    // 従う（Direct 境界の保存）。ResolvedKana は元打鍵
                                    // 不明として journal へ登録しない。
                                    provenance: match element.provenance {
                                        Provenance::Typed { style: SourceStyle::Kana } => {
                                            PieceProvenance::Inherited { direct: false }
                                        }
                                        Provenance::Typed { style: SourceStyle::Direct } => {
                                            PieceProvenance::Inherited { direct: true }
                                        }
                                        Provenance::ResolvedKana => PieceProvenance::Unknown,
                                        Provenance::ExplicitLiteral => PieceProvenance::Inherited { direct: true },
                                    },
                                });
                                reading_text.push_str(&reading_clip);
                                piece_source_at += source_len;
                            }
                        }
                    }
                    piece_source_at += push_kana_run(
                        &mut run,
                        &mut run_tail_completed,
                        piece_source_at,
                        reading_at + scalar_len(&reading_text),
                        &mut reading_text,
                        &mut pieces,
                    );
                    debug_assert_eq!(piece_source_at, span.range.end.get());
                    (reading_text, pieces)
                }
            };

            let span_reading_len = scalar_len(&reading_text);
            spans.push(ProjectionSpan {
                kind: span.kind,
                source: span.range,
                reading: ReadingRange::new(reading_at, reading_at + span_reading_len),
                source_text: span.text.clone(),
                // PR2: 表示は読み（Japanese）・原文（Literal）と同一。PR3 で変換結果へ差し替える。
                display_text: reading_text.clone(),
                reading_text,
                pieces,
            });
            reading_at += span_reading_len;
            source_at = span.range.end.get();
        }
        if source_at != source_len {
            return Err(ProjectionError::CoverMismatch(format!(
                "span 列がソースを被覆し終えない（{source_at}/{source_len}）"
            )));
        }

        Ok(Projection {
            plan_id,
            source_revision: source.revision(),
            spans,
        })
    }

    /// 読み全体（span の reading_text の連結）。
    pub fn reading(&self) -> String {
        self.spans
            .iter()
            .map(|span| span.reading_text.as_str())
            .collect()
    }

    /// 表示文字列全体（span の display_text の連結）。PR2 では reading と同一。
    pub fn display_surface(&self) -> String {
        self.spans
            .iter()
            .map(|span| span.display_text.as_str())
            .collect()
    }

    /// composer（編集状態）へ反映する unit 列（PR3）。Literal span は原文 1 unit、
    /// Japanese span は PieceProjection（対応単位）ごと。読み・元打鍵とも span の
    /// 部分文字列で、連結は Projection の読み・ソースと一致する。
    pub fn adoption_units(&self) -> Vec<AdoptionUnit> {
        let mut units = Vec::new();
        for span in &self.spans {
            match span.kind {
                SegmentKind::Literal => units.push(AdoptionUnit {
                    reading: span.reading_text.clone(),
                    original: span.reading_text.clone(),
                    kind: AdoptionKind::Literal,
                    source: span.source,
                }),
                SegmentKind::Japanese => {
                    for piece in &span.pieces {
                        let reading = scalar_slice(
                            &span.reading_text,
                            piece.reading.start.get() - span.reading.start.get(),
                            piece.reading.end.get() - span.reading.start.get(),
                        );
                        let original = scalar_slice(
                            &span.source_text,
                            piece.source.start.get() - span.source.start.get(),
                            piece.source.end.get() - span.source.start.get(),
                        );
                        let kind = match piece.provenance {
                            PieceProvenance::KanaRun => AdoptionKind::Kana,
                            PieceProvenance::Inherited { direct: true } => AdoptionKind::Direct,
                            PieceProvenance::Inherited { direct: false } => AdoptionKind::Kana,
                            PieceProvenance::Unknown => AdoptionKind::Unknown,
                        };
                        units.push(AdoptionUnit {
                            reading,
                            original,
                            kind,
                            source: piece.source,
                        });
                    }
                }
            }
        }
        units
    }

    pub fn display_utf16_len(&self) -> u32 {
        self.spans
            .iter()
            .map(|span| utf16_len(&span.display_text))
            .sum()
    }

    /// 元入力位置 → 読み位置。元打鍵と読みが同一文字列の単位（Literal span、
    /// 原文=読みの再合成 unit）の内部は保存して写す。それ以外の単位は境界のみ。
    /// 長さが同じだけでは内部を対応させない（`va → ゔぁ`。計画書 3-2）。
    pub fn source_to_reading(&self, position: SourcePosition) -> Option<ReadingPosition> {
        for span in &self.spans {
            for piece in &span.pieces {
                let (s, e) = (piece.source.start.get(), piece.source.end.get());
                if s <= position.get() && position.get() <= e {
                    return map_position(
                        position.get(),
                        s,
                        e,
                        piece.reading.start.get(),
                        piece.reading.end.get(),
                        piece.identity,
                    )
                    .map(ReadingPosition::new);
                }
            }
        }
        None
    }

    /// 読み位置 → 元入力位置。`source_to_reading` と同じ規則で逆に写す。
    pub fn reading_to_source(&self, position: ReadingPosition) -> Option<SourcePosition> {
        for span in &self.spans {
            for piece in &span.pieces {
                let (s, e) = (piece.reading.start.get(), piece.reading.end.get());
                if s <= position.get() && position.get() <= e {
                    return map_position(
                        position.get(),
                        s,
                        e,
                        piece.source.start.get(),
                        piece.source.end.get(),
                        piece.identity,
                    )
                    .map(SourcePosition::new);
                }
            }
        }
        None
    }

    /// 読み位置 → 表示文字列上の UTF-16 位置。区間の包含は半開区間で判定し、
    /// 区間の終端は次の区間の先頭として扱う。`display_text != reading_text` の
    /// 区間（PR3 の変換結果）は先頭と、文字列全体の末尾に当たる終端だけを対応
    /// させ、内部は対応から外す。
    pub fn reading_to_display_utf16(
        &self,
        position: ReadingPosition,
    ) -> Option<DisplayUtf16Position> {
        let mut utf16_at = 0u32;
        let last = self.spans.len().saturating_sub(1);
        for (index, span) in self.spans.iter().enumerate() {
            let start = span.reading.start.get();
            let end = span.reading.end.get();
            let pos = position.get();
            if start <= pos && pos < end {
                if span.display_text != span.reading_text && pos != start {
                    // 変換済み表示の内部位置は PR2 の対応契約の外。
                    return None;
                }
                let offset = pos - start;
                let within: u32 = span
                    .display_text
                    .chars()
                    .take(offset as usize)
                    .map(|ch| ch.len_utf16() as u32)
                    .sum();
                return Some(DisplayUtf16Position::new(utf16_at + within));
            }
            if index == last && pos == end {
                // 文字列全体の末尾。変換済み区間でも表示終端へ対応させる。
                return Some(DisplayUtf16Position::new(
                    utf16_at + utf16_len(&span.display_text),
                ));
            }
            utf16_at += utf16_len(&span.display_text);
        }
        None
    }
}

/// Kana 打鍵の連続範囲（`run`、元打鍵の連結。大小文字を含む）を読みへ再合成して
/// 出力する。再合成の入力には composer へ渡した正規化済み文字列（`roman_input`）
/// を使い、`identity`（内部位置の保存写像）の判定には元打鍵側の文字列と読みの
/// 同一比較を使う。`ResolvedKana` / `Direct` がはさまれるところで区切って呼ぶ。
/// 未完の末尾 `n` の完成はソース側の unit 状態に従う。`tail_completed`（範囲末尾
/// 要素の読みが既に ん。finalize_pending_n 済み）なら完成を引き継ぎ、false なら
/// Plan の終端状態として未完のまま残す（計画書 5.1）。位置（最終 span か）だけでは、
/// 入力中の未完 `n` と完成済みの ん を区別できない。範囲が空なら何もしない。
/// 戻り値は消費した原文の scalar 長。
fn push_kana_run(
    run: &mut String,
    tail_completed: &mut bool,
    source_at: u32,
    reading_at: u32,
    reading_text: &mut String,
    pieces: &mut Vec<PieceProjection>,
) -> u32 {
    if run.is_empty() {
        return 0;
    }
    let mut units = roman::synthesize(&roman_input(run), false);
    if *tail_completed {
        if let Some(last) = units.last_mut() {
            // finalize=false で残る original == kana の unit は未完ローマ字だけで、
            // 完成させるのは既存の確定規則と同じ単発 n → ん のみ。
            if last.original == "n" && last.kana == "n" {
                last.kana = "ん".to_string();
            }
        }
    }
    let mut unit_source_at = source_at;
    let mut unit_reading_at = reading_at;
    let mut typed = run.chars();
    for unit in &units {
        let unit_source_len = scalar_len(&unit.original);
        let unit_reading_len = scalar_len(&unit.kana);
        // ASCII 小文字化は文字数を保存するため、unit ごとの元打鍵は run から同じ
        // 文字数で切り出せる。identity はこの元打鍵と読みの同一文字列比較
        // （CapsLock の "KY" と読み "ky" は一致しない）。
        let typed_source: String = typed.by_ref().take(unit_source_len as usize).collect();
        pieces.push(PieceProjection {
            source: SourceRange::new(unit_source_at, unit_source_at + unit_source_len),
            reading: ReadingRange::new(unit_reading_at, unit_reading_at + unit_reading_len),
            identity: typed_source == unit.kana,
            provenance: PieceProvenance::KanaRun,
        });
        unit_source_at += unit_source_len;
        unit_reading_at += unit_reading_len;
    }
    reading_text.extend(units.iter().map(|unit| unit.kana.as_str()));
    let consumed = scalar_len(run);
    run.clear();
    *tail_completed = false;
    consumed
}

/// 文字列の scalar 位置 [start, stop) を切る。
fn scalar_slice(text: &str, start: u32, stop: u32) -> String {
    text.chars()
        .skip(start as usize)
        .take((stop - start) as usize)
        .collect()
}

/// Kana 要素の元打鍵を再合成した結果が、保存済みの読みに一致するか。一致しない
/// unit は composer へ渡した文字が元打鍵と違う（物理 `-` を to_kana_reading_char
/// で `ー` として composer へ渡し、preserve_last_literal_original で元打鍵 `-` を
/// 保存した unit）。composer が元打鍵を書き換えるのは、composer 入力の文字がその
/// まま unit の読みになる非ローマ字1文字か、CapsLock 大文字の小文字正規化
/// （`roman_input` 参照）のときに限るため、読みの引き継ぎが composer 入力の再現と
/// 一致する。composer 入力と読みが別文字になる新しい形状ができたら、composer
/// 入力を要素に別に持たせる（replay_text 相当）必要がある。
fn resynthesizes_to_reading(original: &str, reading: &str) -> bool {
    let resynthesized: String = roman::synthesize(&roman_input(original), false)
        .iter()
        .map(|unit| unit.kana.as_str())
        .collect();
    resynthesized == reading
}

/// Typed Kana unit の再合成に使う composer 入力。CapsLock 打鍵は
/// key_event_sink::resolve_az_char が読み合成用の小文字と元打鍵の大文字を分ける
/// ため、composer 入力 == 元打鍵の ASCII 小文字化が成り立つ。大小文字以外の差分
/// （`-` → `ー` など）はここでは同一視せず、resynthesizes_to_reading の判定で
/// 引き継ぎへ落とす。ASCII の小文字化は文字数を変えないため、元打鍵座標の
/// PieceProjection と unit 境界はずれない。
fn roman_input(original: &str) -> String {
    original.chars().map(|ch| ch.to_ascii_lowercase()).collect()
}

/// [from_start, from_end) の単位内位置を [to_start, to_end) へ写す。
/// `identity` は元打鍵と読みが同一文字列で内部位置が一致することを保証する。
/// それ以外は境界のみ。長さの一致だけでは対応させない（`va → ゔぁ`）。
fn map_position(
    position: u32,
    from_start: u32,
    from_end: u32,
    to_start: u32,
    to_end: u32,
    identity: bool,
) -> Option<u32> {
    if identity {
        debug_assert_eq!(from_end - from_start, to_end - to_start);
        return Some(to_start + (position - from_start));
    }
    if position == from_start {
        Some(to_start)
    } else if position == from_end {
        Some(to_end)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{SourceElement, SourceStyle};

    fn typed(source: &str, reading: &str) -> SourceElement {
        SourceElement {
            provenance: Provenance::Typed {
                style: SourceStyle::Kana,
            },
            source_text: source.to_string(),
            reading: reading.to_string(),
        }
    }

    fn explicit(source: &str) -> SourceElement {
        SourceElement {
            provenance: Provenance::ExplicitLiteral,
            source_text: source.to_string(),
            reading: source.to_string(),
        }
    }

    fn direct(source: &str) -> SourceElement {
        SourceElement {
            provenance: Provenance::Typed {
                style: SourceStyle::Direct,
            },
            source_text: source.to_string(),
            reading: source.to_string(),
        }
    }

    fn resolved(reading: &str) -> SourceElement {
        SourceElement {
            provenance: Provenance::ResolvedKana,
            source_text: reading.to_string(),
            reading: reading.to_string(),
        }
    }

    /// github(ぎてゅb) + no(の) + tukaikata(つかいかた) の合成例。
    /// 読みは composer の実際の出方に依らず、対応の検証のための固定値。
    fn sample_source() -> CompositionSource {
        CompositionSource::try_new(
            vec![
                typed("github", "ぎてゅb"),
                typed("no", "の"),
                typed("tukaikata", "つかいかた"),
            ],
            12,
        )
        .unwrap()
    }

    #[test]
    fn build_projects_a_mixed_plan_with_per_span_correspondence() {
        let source = sample_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Literal, "github".to_string()),
                (SegmentKind::Japanese, "notukaikata".to_string()),
            ],
        )
        .unwrap();
        let projection = Projection::build(1, &source, &plan).expect("構築できる");

        assert_eq!(projection.plan_id, 1);
        assert_eq!(projection.source_revision, 12, "revision は source 由来");
        assert_eq!(projection.spans.len(), 2);
        let literal = &projection.spans[0];
        assert_eq!(literal.source, SourceRange::new(0, 6));
        assert_eq!(literal.reading, ReadingRange::new(0, 6));
        assert_eq!(literal.reading_text, "github");
        assert_eq!(literal.display_text, "github");
        assert_eq!(literal.pieces.len(), 1);
        assert!(literal.pieces[0].identity);
        let japanese = &projection.spans[1];
        assert_eq!(japanese.source, SourceRange::new(6, 17));
        assert_eq!(japanese.reading, ReadingRange::new(6, 12));
        assert_eq!(japanese.reading_text, "のつかいかた");
        // Japanese span の対応単位は読み再合成の unit 粒度（no, tu, ka, i, ka, ta）。
        assert_eq!(japanese.pieces.len(), 6);
        assert_eq!(japanese.pieces[0].source, SourceRange::new(6, 8));
        assert_eq!(japanese.pieces[0].reading, ReadingRange::new(6, 7));
        assert!(!japanese.pieces[0].identity);
        assert_eq!(projection.reading(), "githubのつかいかた");
        assert_eq!(projection.display_surface(), "githubのつかいかた");
    }

    #[test]
    fn build_rejects_a_plan_that_does_not_cover_the_source() {
        let source = sample_source();
        // 「github」だけに対して作った Plan は、より長いソースを被覆しない。
        let short =
            InterpretationPlan::build("github", &[(SegmentKind::Literal, "github".to_string())])
                .unwrap();
        assert!(matches!(
            Projection::build(0, &source, &short),
            Err(ProjectionError::CoverMismatch(_))
        ));
    }

    #[test]
    fn build_splits_composer_units_at_language_boundaries() {
        // LocalKanaComposer は `nn` を1 unit（original "nn" → ん）にする。`pythonno` の
        // Literal(python) / Japanese(no) 境界はこの unit 内部に置れる。Japanese 側の
        // 読みは区間原文 `no` から `の` へ再合成する。
        let source = CompositionSource::try_new(
            vec![
                typed("p", "p"),
                typed("y", "y"),
                typed("tho", "てょ"),
                typed("nn", "ん"),
                typed("o", "お"),
            ],
            4,
        )
        .unwrap();
        assert_eq!(source.source_text(), "pythonno");
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Literal, "python".to_string()),
                (SegmentKind::Japanese, "no".to_string()),
            ],
        )
        .unwrap();
        let projection = Projection::build(2, &source, &plan).expect("unit 内部の境界で切れる");
        assert_eq!(projection.reading(), "pythonの");
        assert_eq!(projection.source_revision, 4);
        let literal = &projection.spans[0];
        assert_eq!(literal.pieces.len(), 1, "Literal は区間全体で1単位");
        assert!(literal.pieces[0].identity);
        let japanese = &projection.spans[1];
        assert_eq!(japanese.pieces.len(), 1, "no → の の1 unit");
        assert!(!japanese.pieces[0].identity);
    }

    #[test]
    fn build_rejects_a_boundary_inside_a_mismatched_saved_reading_unit() {
        // 保存済み読みの引き継ぎ（再合成しない要素）で、元入力と読みが一致しない
        // unit の内部に境界が入るとき、同じ scalar オフセットでの切り出しは
        // 読みを欠落させる（nn の後半 n に対し ん の後半は空）。欠落したまま
        // 成功するより明示的なエラーにする。
        let source = CompositionSource::try_new(
            vec![
                // 実 composer の凍結 unit は元入力と読みが一致するが、契約上は
                // 元入力と読みが一致しない引き継ぎ unit も拒否できることを固定する。
                SourceElement {
                    provenance: Provenance::Typed {
                        style: SourceStyle::Direct,
                    },
                    source_text: "nn".to_string(),
                    reading: "ん".to_string(),
                },
                typed("o", "お"),
            ],
            0,
        )
        .unwrap();
        assert_eq!(source.source_text(), "nno");
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Japanese, "n".to_string()),
                (SegmentKind::Japanese, "no".to_string()),
            ],
        )
        .unwrap();
        assert_eq!(
            Projection::build(0, &source, &plan),
            Err(ProjectionError::SavedReadingNotSplittable { span_index: 0 })
        );
        // 境界が unit の外にある Plan は引き継ぎで通る。
        let safe = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Japanese, "nn".to_string()),
                (SegmentKind::Japanese, "o".to_string()),
            ],
        )
        .unwrap();
        let projection = Projection::build(0, &source, &safe).unwrap();
        assert_eq!(projection.reading(), "んお");
    }

    #[test]
    fn whole_unit_inheritance_keeps_a_saved_reading_longer_than_its_source() {
        // unit 全体の引き継ぎでは読みを source 側 scalar オフセットで切らない。
        // 読みが元入力より長い unit（try_new は空でないことしか検証しない）でも、
        // 保存済み読みを元入力の長さに切り詰めて成功させない。
        let source = CompositionSource::try_new(
            vec![SourceElement {
                provenance: Provenance::Typed {
                    style: SourceStyle::Direct,
                },
                source_text: "x".to_string(),
                reading: "あい".to_string(),
            }],
            0,
        )
        .unwrap();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection =
            Projection::build(0, &source, &plan).expect("unit 全体の引き継ぎは構築できる");
        assert_eq!(projection.reading(), "あい");
        let piece = &projection.spans[0].pieces[0];
        assert_eq!(piece.source, SourceRange::new(0, 1));
        assert_eq!(piece.reading, ReadingRange::new(0, 2));
        assert!(!piece.identity);
    }

    #[test]
    fn build_rejects_a_literal_span_over_resolved_kana() {
        let source = CompositionSource::try_new(
            vec![typed("kyou", "きょう"), resolved("す"), typed("ka", "か")],
            0,
        )
        .unwrap();
        // 「す」は ResolvedKana。全体 Literal は原文が復元できない。
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Literal, source.source_text())],
        )
        .unwrap();
        assert_eq!(
            Projection::build(0, &source, &plan),
            Err(ProjectionError::LiteralOriginalUnknown { span_index: 0 })
        );
    }

    #[test]
    fn build_keeps_explicit_literal_out_of_japanese_spans() {
        let source = CompositionSource::try_new(
            vec![typed("kyo", "きょ"), explicit("A"), typed("u", "う")],
            0,
        )
        .unwrap();
        let all_japanese = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        assert_eq!(
            Projection::build(0, &source, &all_japanese),
            Err(ProjectionError::ExplicitLiteralNotKept { span_index: 0 })
        );
        // Literal として保持する Plan は通る。
        let kept = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Japanese, "kyo".to_string()),
                (SegmentKind::Literal, "A".to_string()),
                (SegmentKind::Japanese, "u".to_string()),
            ],
        )
        .unwrap();
        let projection = Projection::build(0, &source, &kept).unwrap();
        assert_eq!(projection.reading(), "きょAう");
    }

    #[test]
    fn trailing_pending_n_is_not_finalized_at_the_source_end() {
        // composer は入力末尾の単発 n を未完のまま見せる（konnn → こんn）。最終 span
        // がソース末尾で終わるとき、Plan の読みも完成済みにしない（計画書 5.1）。
        let source = CompositionSource::try_new(
            vec![typed("ko", "こ"), typed("nn", "ん"), typed("n", "n")],
            0,
        )
        .unwrap();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection = Projection::build(0, &source, &plan).unwrap();
        assert_eq!(projection.reading(), "こんn");
    }

    #[test]
    fn trailing_n_completion_follows_the_source_unit_state() {
        // 末尾 n の完成は位置（最終 span か）だけでは決められない。ソースの unit が
        // 未完（読み "n"）なら Plan の終端状態として未完のまま、finalize_pending_n
        // 済み（読み ん）なら完成を引き継ぐ。全体 Japanese の Plan は両方で
        // ソースの読みと同じ結果になる。
        for (tail_reading, expected) in [("n", "ほn"), ("ん", "ほん")] {
            let source =
                CompositionSource::try_new(vec![typed("ho", "ほ"), typed("n", tail_reading)], 0)
                    .unwrap();
            let plan = InterpretationPlan::build(
                &source.source_text(),
                &[(SegmentKind::Japanese, source.source_text())],
            )
            .unwrap();
            let projection = Projection::build(0, &source, &plan).unwrap();
            assert_eq!(
                projection.reading(),
                expected,
                "末尾 unit の読み={tail_reading}"
            );
        }
    }

    #[test]
    fn japanese_span_keeps_the_saved_reading_of_resolved_kana() {
        // ResolvedKana の source_text は元打鍵ではなく現在の読み（TIP も source_text
        // と reading の両方へ同じ読みを入れる）。Japanese 区間への適用だけでローマ字
        // として再解釈せず、"ai" を あい にしない。
        let source = CompositionSource::try_new(vec![resolved("ai")], 0).unwrap();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection = Projection::build(0, &source, &plan).unwrap();
        assert_eq!(projection.reading(), "ai");
        assert!(
            projection.spans[0].pieces[0].identity,
            "保存済み読みは原文と同一文字列で、内部位置も対応する"
        );

        // 再合成は ResolvedKana の境界をまたがない。隣接 Kana と連結して再合成すると
        // kiaika → きあいか になる。引き継ぎでは きaiか のまま。
        let source = CompositionSource::try_new(
            vec![typed("ki", "き"), resolved("ai"), typed("ka", "か")],
            0,
        )
        .unwrap();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection = Projection::build(0, &source, &plan).unwrap();
        assert_eq!(projection.reading(), "きaiか");
    }

    #[test]
    fn japanese_span_does_not_reinterpret_direct_input_as_romaji() {
        // Direct の "a" は読みに直す前の打鍵。Japanese 区間への適用だけで あ に
        // かな化しない。隣接 Kana との連結再合成（ai → あい）にも入れない。
        let source = CompositionSource::try_new(vec![direct("a"), typed("i", "い")], 0).unwrap();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection = Projection::build(0, &source, &plan).unwrap();
        assert_eq!(projection.reading(), "aい");
        assert!(
            projection.spans[0].pieces[0].identity,
            "打鍵と読みが同一文字列"
        );
    }

    #[test]
    fn japanese_span_carries_kana_units_whose_original_does_not_resynthesize() {
        // 物理 `-` は to_kana_reading_char で `ー` として composer へ渡り、
        // preserve_last_literal_original('-') が元打鍵を保存する。元打鍵 "-" の
        // 再合成は保存済みの読み "ー" に整合しないので、読みを引き継ぐ。混在入力
        // を通すだけで canonical reading が "-" の passthrough に変わらない。
        let source = CompositionSource::try_new(
            vec![typed("wa", "わ"), typed("-", "ー"), typed("ru", "る")],
            0,
        )
        .unwrap();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, source.source_text())],
        )
        .unwrap();
        let projection = Projection::build(0, &source, &plan).unwrap();
        assert_eq!(projection.reading(), "わーる");
        // 引き継ぎ部は元打鍵と読みが別文字なので、内部位置は対応させない。
        assert!(!projection.spans[0].pieces[1].identity);
    }

    #[test]
    fn trailing_n_before_a_literal_span_finalizes_like_finalize_pending_n() {
        // 日本語→Literal 境界の末尾 n は既存の確定規則（n → ん）と一致させる。
        let source =
            CompositionSource::try_new(vec![typed("won", "をん"), typed("p", "p")], 0).unwrap();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Japanese, "won".to_string()),
                (SegmentKind::Literal, "p".to_string()),
            ],
        )
        .unwrap();
        let projection = Projection::build(0, &source, &plan).unwrap();
        assert_eq!(projection.reading(), "をんp");
    }

    #[test]
    fn equal_length_units_map_interiors_only_when_source_equals_reading() {
        let source = CompositionSource::try_new(vec![typed("va", "ゔぁ")], 0).unwrap();
        let japanese = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Japanese, "va".to_string())],
        )
        .unwrap();
        let projection = Projection::build(0, &source, &japanese).unwrap();
        assert_eq!(projection.reading(), "ゔぁ");
        // va → ゔぁ はどちらも2 scalar だが1つの変換 unit。内部位置は対応しない。
        assert_eq!(projection.source_to_reading(SourcePosition::new(1)), None);
        assert_eq!(projection.reading_to_source(ReadingPosition::new(1)), None);
        assert_eq!(
            projection.source_to_reading(SourcePosition::new(0)),
            Some(ReadingPosition::new(0))
        );
        assert_eq!(
            projection.source_to_reading(SourcePosition::new(2)),
            Some(ReadingPosition::new(2))
        );

        let literal = InterpretationPlan::build(
            &source.source_text(),
            &[(SegmentKind::Literal, "va".to_string())],
        )
        .unwrap();
        let projection = Projection::build(0, &source, &literal).unwrap();
        // Literal は原文=読み。内部位置まで対応する。
        assert_eq!(
            projection.source_to_reading(SourcePosition::new(1)),
            Some(ReadingPosition::new(1))
        );
        assert_eq!(
            projection.reading_to_source(ReadingPosition::new(1)),
            Some(SourcePosition::new(1))
        );
    }

    #[test]
    fn source_and_reading_positions_map_at_boundaries_and_identity_runs() {
        let source = sample_source();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Literal, "github".to_string()),
                (SegmentKind::Japanese, "notukaikata".to_string()),
            ],
        )
        .unwrap();
        let projection = Projection::build(0, &source, &plan).unwrap();

        // Literal 内部は読みが原文に差し替わるため source/reading 同長（保存写像）。
        assert_eq!(
            projection.source_to_reading(SourcePosition::new(3)),
            Some(ReadingPosition::new(3))
        );
        // Japanese 側は再合成 unit の境界でのみ対応する。no(2 scalar) → の(1 scalar)。
        assert_eq!(
            projection.source_to_reading(SourcePosition::new(6)),
            Some(ReadingPosition::new(6))
        );
        assert_eq!(
            projection.source_to_reading(SourcePosition::new(8)),
            Some(ReadingPosition::new(7)),
            "no と tu の境界は読みでは 6+1"
        );
        assert_eq!(
            projection.source_to_reading(SourcePosition::new(7)),
            None,
            "no(2 scalar) → の(1 scalar) の内部位置は対応しない"
        );
        assert_eq!(
            projection.source_to_reading(SourcePosition::new(10)),
            Some(ReadingPosition::new(8)),
            "tu と ka の境界（再合成 unit 粒度では対応する）"
        );
        assert_eq!(
            projection.reading_to_source(ReadingPosition::new(7)),
            Some(SourcePosition::new(8)),
            "の と つ の境界は source 8 へ戻る"
        );
        assert_eq!(
            projection.reading_to_source(ReadingPosition::new(9)),
            Some(SourcePosition::new(12)),
            "か と い の境界も再合成 unit 粒度で対応する"
        );
        assert_eq!(
            projection.reading_to_source(ReadingPosition::new(12)),
            Some(SourcePosition::new(17)),
            "読み末尾（ta の末尾境界）はソース末尾へ"
        );
        assert_eq!(projection.source_to_reading(SourcePosition::new(99)), None);
    }

    #[test]
    fn reading_to_display_utf16_counts_surrogate_pairs() {
        let source = CompositionSource::try_new(
            vec![typed("kyo", "きょ"), typed("🌍", "🌍"), typed("u", "う")],
            0,
        )
        .unwrap();
        let plan = InterpretationPlan::build(
            &source.source_text(),
            &[
                (SegmentKind::Japanese, "kyo".to_string()),
                (SegmentKind::Literal, "🌍".to_string()),
                (SegmentKind::Japanese, "u".to_string()),
            ],
        )
        .unwrap();
        let projection = Projection::build(0, &source, &plan).unwrap();
        // 読み「きょ🌍う」= scalar 4 / UTF-16 5。emoji 手前は 2、emoji 末尾は 4。
        assert_eq!(projection.display_utf16_len(), 5);
        assert_eq!(
            projection.reading_to_display_utf16(ReadingPosition::new(2)),
            Some(DisplayUtf16Position::new(2))
        );
        assert_eq!(
            projection.reading_to_display_utf16(ReadingPosition::new(3)),
            Some(DisplayUtf16Position::new(4))
        );
        assert_eq!(
            projection.reading_to_display_utf16(ReadingPosition::new(4)),
            Some(DisplayUtf16Position::new(5))
        );
    }

    #[test]
    fn reading_to_display_utf16_maps_converted_span_head_and_tail() {
        // PR3 の表示差し替え（display != reading）を先取りした形。変換済み区間は
        // 先頭と終端を対応させ、内部だけ対応外にする。区間の終端は次の区間の先頭
        // として扱い、先行する変換済み区間に捕まらない。
        let projection = Projection {
            plan_id: 0,
            source_revision: 0,
            spans: vec![
                ProjectionSpan {
                    kind: SegmentKind::Japanese,
                    source: SourceRange::new(0, 4),
                    reading: ReadingRange::new(0, 3),
                    source_text: "kyou".to_string(),
                    reading_text: "きょう".to_string(),
                    display_text: "今日".to_string(),
                    pieces: vec![],
                },
                ProjectionSpan {
                    kind: SegmentKind::Literal,
                    source: SourceRange::new(4, 10),
                    reading: ReadingRange::new(3, 9),
                    source_text: "Python".to_string(),
                    reading_text: "Python".to_string(),
                    display_text: "Python".to_string(),
                    pieces: vec![],
                },
            ],
        };
        // 読み「きょうPython」= 9 scalar / 表示「今日Python」= 8 UTF-16 unit。
        assert_eq!(projection.display_utf16_len(), 8);
        assert_eq!(
            projection.reading_to_display_utf16(ReadingPosition::new(0)),
            Some(DisplayUtf16Position::new(0)),
            "変換済み区間の先頭"
        );
        assert_eq!(
            projection.reading_to_display_utf16(ReadingPosition::new(1)),
            None,
            "変換済み区間の内部"
        );
        assert_eq!(
            projection.reading_to_display_utf16(ReadingPosition::new(2)),
            None,
            "変換済み区間の内部"
        );
        assert_eq!(
            projection.reading_to_display_utf16(ReadingPosition::new(3)),
            Some(DisplayUtf16Position::new(2)),
            "Japanese 終端 = Literal 先頭。表示 UTF-16 位置 2"
        );
        assert_eq!(
            projection.reading_to_display_utf16(ReadingPosition::new(5)),
            Some(DisplayUtf16Position::new(4)),
            "Literal 内部（表示=読み）"
        );
        assert_eq!(
            projection.reading_to_display_utf16(ReadingPosition::new(9)),
            Some(DisplayUtf16Position::new(8)),
            "文字列全体の末尾"
        );
    }

    #[test]
    fn reading_to_display_utf16_maps_the_end_of_a_trailing_converted_span() {
        // 最後の区間が変換済み Japanese でも、文字列全体の末尾は表示末尾へ対応する。
        let projection = Projection {
            plan_id: 0,
            source_revision: 0,
            spans: vec![
                ProjectionSpan {
                    kind: SegmentKind::Literal,
                    source: SourceRange::new(0, 6),
                    reading: ReadingRange::new(0, 6),
                    source_text: "Python".to_string(),
                    reading_text: "Python".to_string(),
                    display_text: "Python".to_string(),
                    pieces: vec![],
                },
                ProjectionSpan {
                    kind: SegmentKind::Japanese,
                    source: SourceRange::new(6, 10),
                    reading: ReadingRange::new(6, 9),
                    source_text: "kyou".to_string(),
                    reading_text: "きょう".to_string(),
                    display_text: "今日".to_string(),
                    pieces: vec![],
                },
            ],
        };
        assert_eq!(projection.display_utf16_len(), 8);
        assert_eq!(
            projection.reading_to_display_utf16(ReadingPosition::new(6)),
            Some(DisplayUtf16Position::new(6)),
            "変換済み区間の先頭"
        );
        assert_eq!(
            projection.reading_to_display_utf16(ReadingPosition::new(7)),
            None,
            "変換済み区間の内部"
        );
        assert_eq!(
            projection.reading_to_display_utf16(ReadingPosition::new(9)),
            Some(DisplayUtf16Position::new(8)),
            "文字列全体の末尾 = 変換済み区間の終端も表示末尾へ"
        );
    }
}
