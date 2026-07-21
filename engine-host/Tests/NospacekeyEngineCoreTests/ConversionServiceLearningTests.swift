import XCTest
import Foundation
@testable import NospacekeyEngineCore

final class ConversionServiceLearningTests: XCTestCase {
    private func makeTempDir() throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-learn-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }
    private func learningService(_ dir: URL) -> ConversionService {
        ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                          learning: LearningSettings(enabled: true, memoryDir: dir))
    }

    /// ①+②: 確定→endSession（フラッシュ）でファイル生成、再セッションで学習が候補順位に反映。
    func testCommitLearnsAndFlushPersists() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir)
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "kisha")
        let cands = try XCTUnwrap(svc.convert(session: sid))
        XCTAssertGreaterThan(cands.count, 1, "同読み多候補の前提: \(cands)")
        let target = 1 // 先頭以外を確定して順位変化を観測可能にする
        let learned = cands[target]
        let committed = try XCTUnwrap(svc.commit(session: sid, index: target))
        XCTAssertEqual(committed.text, learned)
        svc.endSession(session: sid) // sessions 空 → フラッシュ
        let files = try FileManager.default.contentsOfDirectory(atPath: dir.path)
        XCTAssertTrue(files.contains("memory.louds"), "endSession フラッシュで学習ファイル生成: \(files)")
        // 学習反映: 同読みを再変換すると確定した候補が先頭に来る（temporal memory の強い直近ブースト）。
        let sid2 = svc.startSession()
        _ = svc.insert(session: sid2, text: "kisha")
        let cands2 = try XCTUnwrap(svc.convert(session: sid2))
        XCTAssertEqual(cands2.first, learned, "確定候補が学習で先頭に来るはず: \(cands2)")
        svc.endSession(session: sid2)
    }

    /// ③: clearLearning で RAM+ディスクが消える。
    func testClearLearningRemovesFiles() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir)
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "kisha")
        _ = svc.convert(session: sid)
        _ = svc.commit(session: sid, index: 1)
        svc.endSession(session: sid) // フラッシュしてファイルを作る
        XCTAssertTrue(try FileManager.default.contentsOfDirectory(atPath: dir.path).contains("memory.louds"))
        XCTAssertTrue(svc.clearLearning(), "temp dir の学習ファイルは消し切れるはず")
        let after = try FileManager.default.contentsOfDirectory(atPath: dir.path)
        XCTAssertFalse(after.contains("memory.louds"), "clearLearning でディスクの学習ファイルが消える: \(after)")
    }

    /// ④: 学習 OFF（既定 .disabled）では memoryDir 相当に何も書かれない。
    func testDisabledWritesNothing() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        // learning を渡さない既定 = .disabled。dir は「もし書くならここ」の観測点として渡さない
        // （disabled は memoryDir=nil なので makeOptions は temp の workDir を使う）。
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "kisha")
        _ = svc.convert(session: sid)
        _ = svc.commit(session: sid, index: 1)
        svc.endSession(session: sid)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: dir.path), [],
                       "学習 OFF では観測 dir に何も作られない")
    }

    /// ⑤: reload で OFF に切り替えると、保留中（未フラッシュ）の学習が先に保存される。
    func testReloadToOffFlushesPending() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir)
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "kisha")
        _ = svc.convert(session: sid)
        _ = svc.commit(session: sid, index: 1)   // RAM に学習（未フラッシュ）
        // endSession せずに OFF へ reload → 保留分がフラッシュされてから切り替わる。
        svc.reload(overrides: ["NOSPACEKEY_LEARNING": "0"])
        let files = try FileManager.default.contentsOfDirectory(atPath: dir.path)
        XCTAssertTrue(files.contains("memory.louds"), "OFF 切替前に保留分がフラッシュされる: \(files)")
        svc.endSession(session: sid)
    }

    /// graceful 停止: endSession せずに prepareForShutdown を呼ぶと、保留中（未フラッシュ）の
    /// 学習がディスクへ保存される。Shutdown → 応答後 exit の前段で、composition 保持中の
    /// RAM 学習を強制終了で落とさないための要件（⑤の reload-to-off と同じ flush 経路を停止でも通す）。
    func testPrepareForShutdownFlushesPendingLearning() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir)
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "kisha")
        _ = svc.convert(session: sid)
        _ = svc.commit(session: sid, index: 1)   // RAM に学習（未フラッシュ）
        svc.prepareForShutdown()                  // endSession せず停止前段だけ → 保留分をフラッシュ
        let files = try FileManager.default.contentsOfDirectory(atPath: dir.path)
        XCTAssertTrue(files.contains("memory.louds"), "prepareForShutdown で保留学習がフラッシュされる: \(files)")
        svc.endSession(session: sid)
    }

    /// ⑥（Task 8 の前提）: liveConvert が先頭候補をキャッシュし、Commit(0) がそれを確定する。
    func testLiveConvertCachesTopCandidateForCommit() throws {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "nihongo")
        let live = try XCTUnwrap(svc.liveConvert(session: sid))
        let committed = try XCTUnwrap(svc.commit(session: sid, index: 0),
                                      "liveConvert 後の Commit(0) はキャッシュで成功するはず")
        XCTAssertEqual(committed.text, live.text, "Commit(0) はライブ表示と同じ候補を確定する")
        svc.endSession(session: sid)
    }
}
