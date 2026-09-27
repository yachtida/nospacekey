import Foundation

/// 混在変換の要求検証と成果（実装計画 §6.2–6.4、ADR-0007）。
/// Japanese span の変換は `ConversionService.mixedIntervalSurfaces`（対象区間変換）
/// へ委譲し、この型は判定（PR4）もソース編集（TIP 側）も持たない。
/// Literal span は原文を一字不動で連結する — 削除して日本語をつなげたり、
/// 仮の記号へ置換して後から戻したりしない。
enum MixedConversionService {
    static let japaneseKind = "japanese"
    static let literalKind = "literal"

    /// span 列の検証。すべて成り立つこと:
    /// - 空でなく、kind は japanese / literal のいずれか
    /// - 読み区間が先頭 (0) から隣接して並び、重複・欠落がない（完全被覆）
    /// - 区間文字列が空でなく、文字数が読み幅と一致する
    ///   （Japanese=読み、Literal=原文。どちらも採用 Projection の読み座標に同じ
    ///   文字数で載る — PR2 の契約）
    static func validate(spans: [MixedSpan]) -> Bool {
        guard !spans.isEmpty else { return false }
        var cursor: UInt32 = 0
        for span in spans {
            guard span.kind == japaneseKind || span.kind == literalKind else { return false }
            guard span.readingStart == cursor, span.readingEnd > span.readingStart else { return false }
            guard !span.text.isEmpty,
                  UInt32(span.text.unicodeScalars.count) == span.readingEnd - span.readingStart
            else { return false }
            cursor = span.readingEnd
        }
        return true
    }

    /// 混在変換の成果。`text` は span 表示の連結（Literal は原文）。`spans` は要求と
    /// 同じ順序・読み範囲で、Japanese は変換結果と学習 token、Literal は原文そのもの。
    struct Output: Equatable {
        let text: String
        let spans: [MixedSpanResult]
    }

    /// 検証済み span 列を変換する。変換できない（候補なし・期限切れ・token 上限）
    /// ときは nil — 呼出側は Error へ落とし、TIP は従来の候補経路を維持する
    /// （実装計画 §8.5: 未採用の Mixed 解析が失敗しても本文を勝手に変更しない）。
    static func convert(service: ConversionService, spans: [MixedSpan],
                        leftContext: String?, admissionDeadline: RequestDeadline?) -> Output? {
        guard validate(spans: spans) else { return nil }
        guard let converted = service.mixedIntervalSurfaces(spans: spans, leftContext: leftContext,
                                                            admissionDeadline: admissionDeadline)
        else { return nil }
        // 連結結果の検証: 全体テキストは span 表示の連結と一致し、Literal span の
        // 表示は原文と一字不動であること（計画書 §6.3 の連結検証）。
        guard converted.text == converted.spans.map(\.text).joined() else { return nil }
        for (request, result) in zip(spans, converted.spans) where request.kind == literalKind {
            guard result.text == request.text, result.candidateToken == nil else { return nil }
        }
        return Output(text: converted.text, spans: converted.spans)
    }
}
