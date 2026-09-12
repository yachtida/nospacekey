import XCTest
import KanaKanjiConverterModule
@testable import NospacekeyEngineCore

final class RecentLearningOverlayTests: XCTestCase {
    func testLatestLearnedCandidateIsPromotedAndCapacityIsBounded() {
        let overlay = RecentLearningOverlay(capacity: 1)
        let first = candidate(text: "日本", ruby: "ニホン", value: 1)
        let second = candidate(text: "二本", ruby: "ニホン", value: 42)
        let duplicateSurface = candidate(text: "日本", ruby: "ニホン", value: 3)
        overlay.record(second)
        let ranked = overlay.rank(
            mainResults: [first, second, duplicateSurface], firstClauseResults: [],
            composing: composing("にほん"))
        XCTAssertEqual(ranked.main.map(\.text), ["二本", "日本"])
        XCTAssertEqual(ranked.main.map(\.value), [42, 1])
        overlay.record(candidate(text: "学校", ruby: "ガッコウ"))
        XCTAssertEqual(overlay.count, 1)
        XCTAssertEqual(overlay.rank(
            mainResults: [first, second], firstClauseResults: [],
            composing: composing("にほん")).main.map(\.text), ["日本", "二本"])
    }

    func testPrefixLearningRanksFirstClauseAndContainingWholeWithoutMixingSameSurfaceRanges() {
        let overlay = RecentLearningOverlay()
        let partial = candidate(text: "日本", ruby: "ニホン", value: 7, range: .surfaceCount(3))
        let fullSameSurface = candidate(
            text: "日本", ruby: "ニホンゴ", value: 8, range: .surfaceCount(4))
        let other = candidate(text: "二本", ruby: "ニホン", value: 1, range: .surfaceCount(3))
        let containing = candidate(
            text: "日本語", value: 2, range: .surfaceCount(4),
            elements: [("日本", "ニホン"), ("語", "ゴ")])
        let otherWhole = candidate(
            text: "二本語", value: 3, range: .surfaceCount(4),
            elements: [("二本", "ニホン"), ("語", "ゴ")])

        overlay.record(partial)
        overlay.record(fullSameSurface)
        overlay.record(partial)
        XCTAssertEqual(overlay.count, 2, "exact identity dedups, same surface with another range remains")

        let ranked = overlay.rankForTesting(
            mainResults: [otherWhole, containing],
            firstClauseResults: [fullSameSurface, other, partial, partial],
            composing: composing("にほんご"))
        XCTAssertEqual(ranked.main.map(\.text), ["日本語", "二本語"])
        XCTAssertEqual(ranked.firstClause.map { "\($0.text):\($0.rubyCount)" },
                       ["日本:3", "日本:4", "二本:3"])
        XCTAssertEqual(ranked.evaluations,
                       RecentLearningOverlay.EvaluationCounts(
                           learnedRanges: 2, candidateIdentities: 6, candidateElements: 8),
                       "each learned range and candidate element must be evaluated only once")
    }

    func testClearRemovesAllImmediateLearning() {
        let overlay = RecentLearningOverlay()
        overlay.record(candidate(text: "二本", ruby: "ニホン"))
        overlay.clear()
        XCTAssertEqual(overlay.count, 0)
    }

    private func composing(_ reading: String) -> ComposingText {
        var value = ComposingText()
        value.insertAtCursorPosition(reading, inputStyle: .direct)
        return value
    }

    private func candidate(text: String, ruby: String, value: PValue = 0,
                           range: ComposingCount? = nil) -> Candidate {
        candidate(text: text, value: value, range: range ?? .surfaceCount(ruby.count),
                  elements: [(text, ruby)])
    }

    private func candidate(text: String, value: PValue, range: ComposingCount,
                           elements: [(String, String)]) -> Candidate {
        Candidate(text: text, value: value, composingCount: range,
                  lastMid: MIDData.一般.mid,
                  data: elements.map { word, ruby in
                      DicdataElement(word: word, ruby: ruby, cid: CIDData.一般名詞.cid,
                                     mid: MIDData.一般.mid, value: 0)
                  })
    }
}
