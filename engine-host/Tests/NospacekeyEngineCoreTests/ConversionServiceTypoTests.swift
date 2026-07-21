import XCTest
import Foundation
@testable import NospacekeyEngineCore

final class ConversionServiceTypoTests: XCTestCase {
    private func makeTempDir() throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-typo-learn-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    private func classicService() -> ConversionService {
        ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
    }

    private func learningService(_ dir: URL, typoLearn: Bool = true) -> ConversionService {
        ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                          learning: LearningSettings(enabled: true, memoryDir: dir),
                          typoLearn: typoLearn)
    }

    /// 修正候補(仮説変換)ブロックが先頭に立ち、literal(そのまま)変換の崩壊列も後続に含まれる。
    func testRepairedCandidateLeadsListForDoubleS() throws {
        let svc = classicService()
        let sid = svc.startSession()
        for ch in "shitekudassai" { _ = svc.insert(session: sid, text: String(ch)) }
        let cands = try XCTUnwrap(svc.typoConvert(session: sid))
        XCTAssertEqual(cands.first, "してください", "expected repaired candidate to lead \(cands)")
        XCTAssertTrue(cands.contains("してく獺祭"), "expected literal block to follow \(cands)")
    }

    /// 修復パターンが無い読みでは typoConvert は convert と同じ内容を返す（上位互換）。
    func testFallbackEqualsConvertWhenNoPattern() throws {
        let svc = classicService()
        let sid = svc.startSession()
        for ch in "nihongo" { _ = svc.insert(session: sid, text: String(ch)) }
        let cands = try XCTUnwrap(svc.typoConvert(session: sid))
        XCTAssertTrue(cands.contains("日本語"), "expected 日本語 in \(cands)")
    }

    /// 修復候補を Commit すると読み全体を消費し、(誤読み, 修復表記) の合成ペアが学習される
    /// （次回、通常の convert でも誤読みのまま「してください」が浮上する）。
    func testCommitRepairedConsumesAllAndLearnsTypoReading() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir)
        let sid = svc.startSession()
        for ch in "shitekudassai" { _ = svc.insert(session: sid, text: String(ch)) }
        let cands = try XCTUnwrap(svc.typoConvert(session: sid))
        let idx = try XCTUnwrap(cands.firstIndex(of: "してください"))
        let committed = try XCTUnwrap(svc.commit(session: sid, index: idx))
        XCTAssertEqual(committed.text, "してください")
        XCTAssertEqual(committed.reading, "", "修復候補の確定は読み全体を消費するはず")
        svc.endSession(session: sid) // sessions 空 → フラッシュ

        let sid2 = svc.startSession()
        for ch in "shitekudassai" { _ = svc.insert(session: sid2, text: String(ch)) }
        let cands2 = try XCTUnwrap(svc.convert(session: sid2))
        XCTAssertTrue(cands2.contains("してください"),
                      "誤読み学習により通常 convert でも修復候補が浮上するはず: \(cands2)")
        svc.endSession(session: sid2)
    }

    /// typoLearn=false のときは合成ペア学習をスキップする（誤読みのままでは浮上しない）。
    func testTypoLearnOffSkipsPairLearning() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir, typoLearn: false)
        let sid = svc.startSession()
        for ch in "shitekudassai" { _ = svc.insert(session: sid, text: String(ch)) }
        let cands = try XCTUnwrap(svc.typoConvert(session: sid))
        let idx = try XCTUnwrap(cands.firstIndex(of: "してください"))
        _ = try XCTUnwrap(svc.commit(session: sid, index: idx))
        svc.endSession(session: sid)

        let sid2 = svc.startSession()
        for ch in "shitekudassai" { _ = svc.insert(session: sid2, text: String(ch)) }
        let cands2 = try XCTUnwrap(svc.convert(session: sid2))
        XCTAssertFalse(cands2.contains("してください"),
                       "typoLearn=false では誤読みに紐づく合成ペア学習は起きないはず: \(cands2)")
        svc.endSession(session: sid2)
    }

    /// 未知セッションは nil（既知セッションの空でない結果と区別する）。
    func testUnknownSessionReturnsNilForTypoConvert() {
        let svc = classicService()
        XCTAssertNil(svc.typoConvert(session: 99999))
    }

    /// レビューCritical: typoConvert で立てた修復 index は、読みが変わらないまま通常の convert()/
    /// liveConvert() が呼ばれても(Tab→Esc→Space/Enter相当)残ってはいけない。stale なままだと、
    /// 通常の literal 候補を commit したときに誤って「修復候補の確定」に分類され、残り読みが
    /// 全損した上に誤った合成ペアが学習されてしまう。
    ///
    /// 不変条件を直接検査する（間接的に「commit の reading が食い違う」ことでの証明は、部分被覆の
    /// literal 候補が低 index に来るかどうか実辞書データに左右され再現性が無い —
    /// "shitekudassai"/"kudasaai"/"nihhongo" いずれも低 index は常にフルカバー候補で埋まり、
    /// 前方一致候補は index 11+ に来ることを実測で確認した。フルカバー候補は誤分類されても
    /// reading=="" は変わらず、誤って合成学習される (ruby,word) ペアも自身の正当な学習と一致し
    /// 無害化されてしまうため、reading の食い違いでは検出できない）。
    func testStaleRepairedIndicesDoNotSurviveNormalConvert() throws {
        let svcConvert = classicService()
        let sidConvert = svcConvert.startSession()
        for ch in "shitekudassai" { _ = svcConvert.insert(session: sidConvert, text: String(ch)) }
        _ = try XCTUnwrap(svcConvert.typoConvert(session: sidConvert))       // 修復 index が立つ
        XCTAssertNotNil(svcConvert.typoRepairedIndices[sidConvert], "前提: typoConvert 直後は修復 index が立っているはず")
        _ = try XCTUnwrap(svcConvert.convert(session: sidConvert))          // Esc→Space 相当
        XCTAssertNil(svcConvert.typoRepairedIndices[sidConvert],
                     "convert() は cachedCandidates を通常候補で上書きするので、修復 index も消えるはず")

        let svcLive = classicService()
        let sidLive = svcLive.startSession()
        for ch in "shitekudassai" { _ = svcLive.insert(session: sidLive, text: String(ch)) }
        _ = try XCTUnwrap(svcLive.typoConvert(session: sidLive))
        XCTAssertNotNil(svcLive.typoRepairedIndices[sidLive], "前提: typoConvert 直後は修復 index が立っているはず")
        _ = try XCTUnwrap(svcLive.liveConvert(session: sidLive))            // Esc→Enter 相当
        XCTAssertNil(svcLive.typoRepairedIndices[sidLive],
                     "liveConvert() も cachedCandidates を通常候補で上書きするので、修復 index も消えるはず")
    }
}
