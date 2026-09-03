import XCTest
import Foundation
@testable import NospacekeyEngineCore

private enum LearningFileSystemListResult {
    case values([String])
    case failure(Error)
}

private final class LearningFileSystemStub {
    var listResults: [LearningFileSystemListResult]
    var removeError: Error?
    private(set) var listCallCount = 0
    private(set) var removed: [String] = []

    init(_ listResults: [LearningFileSystemListResult], removeError: Error? = nil) {
        self.listResults = listResults
        self.removeError = removeError
    }

    var fileSystem: LearningFileSystem {
        LearningFileSystem(
            list: { [self] _ in
                listCallCount += 1
                guard !listResults.isEmpty else { return [] }
                switch listResults.removeFirst() {
                case .values(let files): return files
                case .failure(let error): throw error
                }
            },
            remove: { [self] url in
                removed.append(url.lastPathComponent)
                if let removeError { throw removeError }
            }
        )
    }
}

/// Vendor reset の実行順序と metadata preflight を同時に観測する seam。list は remove 済み
/// の entry を次回列挙から除くので、通常の clear 成功後 verify も再現できる。
private final class LearningFileSystemTracker {
    let root: URL
    var names: [String]
    var metadataByName: [String: LearningPathMetadata]
    var listError: Error?
    var rootMetadata = LearningPathMetadata(isDirectory: true, isRegularFile: false,
                                             isReparsePoint: false)
    private(set) var removed: [String] = []
    private(set) var resetCallCount = 0

    init(root: URL, names: [String], metadataByName: [String: LearningPathMetadata] = [:]) {
        self.root = root
        self.names = names
        self.metadataByName = metadataByName
    }

    var fileSystem: LearningFileSystem {
        LearningFileSystem(
            list: { [self] _ in
                if let listError { throw listError }
                return names.filter { !removed.contains($0) }
            },
            remove: { [self] url in removed.append(url.lastPathComponent) },
            resetMemory: { [self] in resetCallCount += 1 },
            metadata: { [self] url in
                if url.path == root.path { return rootMetadata }
                return metadataByName[url.lastPathComponent]
                    ?? LearningPathMetadata(isDirectory: false, isRegularFile: true,
                                             isReparsePoint: false)
            })
    }
}

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
    private func seamedService(_ dir: URL, stub: LearningFileSystemStub) -> ConversionService {
        // disabled avoids KanaKanjiConverter.resetMemory touching the real filesystem; the
        // explicit memoryDir still exercises the clearLearning directory selection.
        ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                          learning: LearningSettings(enabled: false, memoryDir: dir),
                          fileSystem: stub.fileSystem)
    }
    private func noteLearningRequest(_ service: ConversionService) {
        let session = service.startSession()
        _ = service.insert(session: session, text: "kisha")
        XCTAssertNotNil(service.convert(session: session))
    }
    private func genericFileError() -> NSError {
        NSError(domain: "LearningFileSystemTests", code: 1)
    }
    private func missingFileError() -> CocoaError {
        CocoaError(.fileNoSuchFile)
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
        svc.endSession(session: sid) // sessions 空 → backgroundフラッシュ
        svc.flushMaintenanceForTesting()
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
        svc.endSession(session: sid) // backgroundフラッシュしてファイルを作る
        svc.flushMaintenanceForTesting()
        XCTAssertTrue(try FileManager.default.contentsOfDirectory(atPath: dir.path).contains("memory.louds"))
        XCTAssertTrue(svc.clearLearning(), "temp dir の学習ファイルは消し切れるはず")
        let after = try FileManager.default.contentsOfDirectory(atPath: dir.path)
        XCTAssertFalse(after.contains("memory.louds"), "clearLearning でディスクの学習ファイルが消える: \(after)")
    }

    func testClearLearningRejectsInitialEnumerationFailure() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let stub = LearningFileSystemStub([.failure(genericFileError())])
        let svc = seamedService(dir, stub: stub)

        XCTAssertFalse(svc.clearLearning())
        XCTAssertEqual(stub.listCallCount, 1)
        XCTAssertTrue(stub.removed.isEmpty)
    }

    func testClearLearningTreatsMissingDirectoryAsAlreadyCleared() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let stub = LearningFileSystemStub([.failure(missingFileError())])
        let svc = seamedService(dir, stub: stub)

        XCTAssertTrue(svc.clearLearning())
        XCTAssertEqual(stub.listCallCount, 1)
        XCTAssertTrue(stub.removed.isEmpty)
    }

    func testClearLearningRejectsMissingDirectoryWhenVendorTemporaryMayRemain() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let tracker = LearningFileSystemTracker(root: dir, names: [])
        tracker.listError = missingFileError()
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                                    learning: LearningSettings(enabled: true, memoryDir: dir),
                                    fileSystem: tracker.fileSystem)
        let session = svc.startSession()
        _ = svc.insert(session: session, text: "kisha")
        _ = svc.convert(session: session)
        _ = svc.commit(session: session, index: 1)

        XCTAssertFalse(svc.clearLearning(),
                       "vendor temporary が残り得る状態で root 消失を成功扱いしない")
        XCTAssertEqual(tracker.resetCallCount, 0,
                       "path 消失後に vendor resetMemory を呼ばない")
    }

    func testClearLearningRejectsDeleteFailure() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let stub = LearningFileSystemStub(
            [.values(["memory.louds"]), .values(["memory.louds"])],
            removeError: genericFileError())
        let svc = seamedService(dir, stub: stub)

        XCTAssertFalse(svc.clearLearning())
        XCTAssertEqual(stub.removed, ["memory.louds"])
    }

    func testClearLearningTreatsMissingDeleteAsBenign() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let stub = LearningFileSystemStub(
            [.values(["memory.louds"]), .values([])],
            removeError: missingFileError())
        let svc = seamedService(dir, stub: stub)

        XCTAssertTrue(svc.clearLearning())
        XCTAssertEqual(stub.removed, ["memory.louds"])
    }

    func testClearLearningRejectsVerificationEnumerationFailure() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let stub = LearningFileSystemStub(
            [.values([".pause"]), .failure(genericFileError())])
        let svc = seamedService(dir, stub: stub)

        XCTAssertFalse(svc.clearLearning())
        XCTAssertEqual(stub.removed, [".pause"])
    }

    func testClearLearningRejectsAllowlistedResidual() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let stub = LearningFileSystemStub(
            [.values(["corrections.json"]), .values(["corrections.json"])])
        let svc = seamedService(dir, stub: stub)

        XCTAssertFalse(svc.clearLearning())
        XCTAssertEqual(stub.removed, ["corrections.json"])
    }

    func testClearLearningKeepsForeignFilesAndSucceedsWhenTargetsAreGone() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let stub = LearningFileSystemStub(
            [.values(["memory.louds", "foreign.txt"]), .values(["foreign.txt"])])
        let svc = seamedService(dir, stub: stub)

        XCTAssertTrue(svc.clearLearning())
        XCTAssertEqual(stub.removed, ["memory.louds"])
    }

    func testClearLearningDeletesCanonicalAsciiLearningShards() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let stub = LearningFileSystemStub(
            [.values(["memory0.loudstxt3", "memory1.loudstxt3", "memory0.loudstxt3.2", "foreign.txt"]),
             .values(["foreign.txt"])])
        let svc = seamedService(dir, stub: stub)

        XCTAssertTrue(svc.clearLearning())
        XCTAssertEqual(stub.removed,
                       ["memory0.loudstxt3", "memory1.loudstxt3", "memory0.loudstxt3.2"])
    }

    func testClearLearningPreservesNonCanonicalShardNames() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let foreign = ["memory00.loudstxt3", "memory01.loudstxt3", "memory０.loudstxt3"]
        let stub = LearningFileSystemStub([.values(foreign), .values(foreign)])
        let svc = seamedService(dir, stub: stub)

        XCTAssertTrue(svc.clearLearning(), "非canonical shard は学習 allowlist 対象外として保持")
        XCTAssertTrue(stub.removed.isEmpty)
    }

    func testClearLearningPreservesOverflowShardName() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let overflow = String(Int.max) + "0"
        let foreign = "memory\(overflow).loudstxt3"
        let stub = LearningFileSystemStub([.values([foreign]), .values([foreign])])
        let svc = seamedService(dir, stub: stub)

        XCTAssertTrue(svc.clearLearning(), "Int 範囲外の shard は foreign として保持")
        XCTAssertTrue(stub.removed.isEmpty)
    }

    func testClearLearningRejectsNonCanonicalShardsBeforeVendorReset() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let tracker = LearningFileSystemTracker(
            root: dir,
            names: ["memory0.loudstxt3", "memory00.loudstxt3", "memory01.loudstxt3", "memory０.loudstxt3"])
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                                    learning: LearningSettings(enabled: true, memoryDir: dir),
                                    fileSystem: tracker.fileSystem)
        noteLearningRequest(svc)

        XCTAssertFalse(svc.clearLearning(), "foreign shard suffix は vendor reset 前に fail-closed")
        XCTAssertEqual(tracker.resetCallCount, 0)
        XCTAssertTrue(tracker.removed.isEmpty)
    }

    func testClearLearningRejectsOverflowShardBeforeVendorReset() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let overflow = String(Int.max) + "0"
        let foreign = "memory\(overflow).loudstxt3"
        let tracker = LearningFileSystemTracker(root: dir,
                                                names: ["memory0.loudstxt3", foreign])
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                                    learning: LearningSettings(enabled: true, memoryDir: dir),
                                    fileSystem: tracker.fileSystem)
        noteLearningRequest(svc)

        XCTAssertFalse(svc.clearLearning(), "Int 範囲外の shard は vendor reset 前に fail-closed")
        XCTAssertEqual(tracker.resetCallCount, 0)
        XCTAssertTrue(tracker.removed.isEmpty)
    }

    func testClearLearningPreflightsDirectoryAndReparseTargetsBeforeDeleting() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let metadata: [String: LearningPathMetadata] = [
            "memory.louds": LearningPathMetadata(isDirectory: true, isRegularFile: false,
                                                  isReparsePoint: false),
            "foreign.txt": LearningPathMetadata(isDirectory: false, isRegularFile: false,
                                                 isReparsePoint: true),
        ]
        let stub = LearningFileSystem(
            list: { _ in ["memory.louds", "foreign.txt"] },
            remove: { url in XCTFail("unsafe target must not be removed: \(url)") },
            metadata: { url in
                if url == dir { return LearningPathMetadata(isDirectory: true, isRegularFile: false,
                                                            isReparsePoint: false) }
                return metadata[url.lastPathComponent]
            })
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                                    learning: LearningSettings(enabled: false, memoryDir: dir),
                                    fileSystem: stub)

        XCTAssertFalse(svc.clearLearning())
    }

    func testClearLearningRejectsForeignVendorSuffixBeforeReset() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let tracker = LearningFileSystemTracker(root: dir,
                                                names: ["memory.louds", "foreign.louds"])
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                                    learning: LearningSettings(enabled: true, memoryDir: dir),
                                    fileSystem: tracker.fileSystem)
        noteLearningRequest(svc)

        XCTAssertFalse(svc.clearLearning(), "vendor suffix の foreign entry は fail-closed")
        XCTAssertEqual(tracker.resetCallCount, 0, "unsafe preflight 前に vendor reset しない")
        XCTAssertTrue(tracker.removed.isEmpty, "partial deletion もしない")
    }

    func testClearLearningRejectsForeignVendorReparseBeforeReset() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let tracker = LearningFileSystemTracker(
            root: dir,
            names: ["memory.louds", "foreign.louds"],
            metadataByName: [
                "foreign.louds": LearningPathMetadata(isDirectory: false, isRegularFile: false,
                                                       isReparsePoint: true)
            ])
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                                    learning: LearningSettings(enabled: true, memoryDir: dir),
                                    fileSystem: tracker.fileSystem)
        noteLearningRequest(svc)

        XCTAssertFalse(svc.clearLearning(), "foreign reparse suffix は fail-closed")
        XCTAssertEqual(tracker.resetCallCount, 0)
        XCTAssertTrue(tracker.removed.isEmpty)
    }

    func testClearLearningRejectsAllowlistedDirectoryBeforeReset() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let tracker = LearningFileSystemTracker(
            root: dir,
            names: ["memory.louds", "foreign.txt"],
            metadataByName: [
                "memory.louds": LearningPathMetadata(isDirectory: true, isRegularFile: false,
                                                      isReparsePoint: false)
            ])
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                                    learning: LearningSettings(enabled: true, memoryDir: dir),
                                    fileSystem: tracker.fileSystem)
        noteLearningRequest(svc)

        XCTAssertFalse(svc.clearLearning(), "allowlist entry が directory なら fail-closed")
        XCTAssertEqual(tracker.resetCallCount, 0)
        XCTAssertTrue(tracker.removed.isEmpty)
    }

    func testClearLearningRejectsReparseRootBeforeReset() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let tracker = LearningFileSystemTracker(root: dir, names: ["memory.louds"])
        tracker.rootMetadata = LearningPathMetadata(isDirectory: true, isRegularFile: false,
                                                     isReparsePoint: true)
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                                    learning: LearningSettings(enabled: true, memoryDir: dir),
                                    fileSystem: tracker.fileSystem)
        noteLearningRequest(svc)

        XCTAssertFalse(svc.clearLearning(), "root reparse は fail-closed")
        XCTAssertEqual(tracker.resetCallCount, 0)
        XCTAssertTrue(tracker.removed.isEmpty)
    }

    func testClearLearningPerformsVendorResetOnlyAfterSafeEnabledRequest() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let tracker = LearningFileSystemTracker(root: dir,
                                                names: ["memory.louds", "foreign.txt"])
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                                    learning: LearningSettings(enabled: true, memoryDir: dir),
                                    fileSystem: tracker.fileSystem)
        noteLearningRequest(svc)

        XCTAssertTrue(svc.clearLearning())
        XCTAssertEqual(tracker.resetCallCount, 1, "同期済み actual root の safe ON reset のみ許可")
        XCTAssertEqual(tracker.removed, ["memory.louds"], "foreign regular file は保持")
    }

    func testReloadBeforeFirstRequestNeverResetsWorkDir() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        // NOSPACEKEY_MEMORY_DIR は base dir: reload 後の clear root は base + BuildInfo.version
        // （ビルド毎の学習状態分離）。tracker の root も同じ versioned dir を指すため、
        // 直書きでなく BuildInfo.version から組み立てる（bump で壊れない）。
        let tracker = LearningFileSystemTracker(
            root: dir.appendingPathComponent(BuildInfo.version), names: ["memory.louds"])
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                                    learning: LearningSettings(enabled: true, memoryDir: dir),
                                    fileSystem: tracker.fileSystem)

        XCTAssertTrue(svc.reload(overrides: ["NOSPACEKEY_LEARNING": "0"]))
        XCTAssertTrue(svc.reload(overrides: [
            "NOSPACEKEY_LEARNING": "1",
            "NOSPACEKEY_MEMORY_DIR": dir.path
        ]))
        XCTAssertTrue(svc.clearLearning(), "reload before first request でも disk root は clear 可能")
        XCTAssertEqual(tracker.resetCallCount, 0, "vendor config unknown/workDir の reset はしない")
        XCTAssertEqual(tracker.removed, ["memory.louds"])
    }

    func testReloadToOffKeepsVendorTemporaryStateAcrossOffRequest() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let tracker = LearningFileSystemTracker(root: dir, names: ["memory.louds"])
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                                    learning: LearningSettings(enabled: true, memoryDir: dir),
                                    fileSystem: tracker.fileSystem)
        let session = svc.startSession()
        _ = svc.insert(session: session, text: "kisha")
        _ = svc.convert(session: session)
        _ = svc.commit(session: session, index: 1)

        XCTAssertTrue(svc.reload(overrides: ["NOSPACEKEY_LEARNING": "0"]))
        // vendor の .nothing request は temporary trie を空にしない（updateConfig の early return）。
        XCTAssertNotNil(svc.convert(session: session))
        XCTAssertFalse(svc.clearLearning(), "OFF request 後に RAM clear 成功を偽装しない")
        XCTAssertEqual(tracker.resetCallCount, 0)
    }

    func testClearLearningDeletesLegacyLearningMemoryFile() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let stub = LearningFileSystemStub(
            [.values(["learningMemory.txt", "foreign.txt"]), .values(["foreign.txt"])])
        let svc = seamedService(dir, stub: stub)

        XCTAssertTrue(svc.clearLearning())
        XCTAssertEqual(stub.removed, ["learningMemory.txt"])
    }

    func testClearLearningClearsCorrectionMemoryEvenWhenEnumerationFails() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let stub = LearningFileSystemStub([.failure(genericFileError())])
        let svc = seamedService(dir, stub: stub)
        svc.recordForTesting(reading: "にほんご", surface: "日本語")
        XCTAssertNotNil(svc.correctionLookupForTesting(reading: "にほんご"))

        XCTAssertFalse(svc.clearLearning())
        XCTAssertNil(svc.correctionLookupForTesting(reading: "にほんご"))
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

    func testReloadToOffWithPendingLearningMakesClearFailClosed() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir)
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "kisha")
        _ = svc.convert(session: sid)
        _ = svc.commit(session: sid, index: 1)

        XCTAssertTrue(svc.reload(overrides: ["NOSPACEKEY_LEARNING": "0"]))
        XCTAssertFalse(svc.clearLearning(), "OFF 切替前 flush の成否を観測できない間は Error にする")
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
