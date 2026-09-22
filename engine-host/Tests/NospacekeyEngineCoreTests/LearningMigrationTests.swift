import Foundation
import XCTest
import WinSDK
import KanaKanjiConverterModuleWithDefaultDictionary
@testable import NospacekeyEngineCore

final class LearningMigrationTests: XCTestCase {
    private func temporaryBase() throws -> URL {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("nsk-migration-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }

    private func seed(_ directory: URL) throws -> [String: Data] {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let service = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: true, memoryDir: directory), processRole: .mainClassicOnly)
        let session = service.startSession()
        _ = service.insert(session: session, text: "コウ")
        _ = service.convert(session: session)
        let candidate = Candidate(text: "試験甲", value: -10, composingCount: .inputCount(2),
            lastMid: MIDData.一般.mid,
            data: [DicdataElement(word: "試験甲", ruby: "コウ", cid: CIDData.一般名詞.cid,
                                 mid: MIDData.一般.mid, value: -10)])
        service.cacheCandidatesForTesting(session: session, candidates: [candidate], target: "コウ")
        _ = service.commit(session: session, index: 0)
        service.endSession(session: session)
        service.prepareForShutdown()
        return try LearningMigration.snapshot(directory)
    }

    func testRealLearnedWordSurvivesInheritanceAndSourceIsUnchanged() throws {
        let base = try temporaryBase()
        let source = base.appendingPathComponent("1.0.0")
        let original = try seed(source)
        XCTAssertEqual(LearningMigration.inherit(base: base, version: "2.0.0"), .inherited)
        let target = base.appendingPathComponent("2.0.0")
        XCTAssertEqual(try LearningMigration.snapshot(target), original)
        XCTAssertEqual(try LearningMigration.snapshot(source), original)
        let service = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: true, memoryDir: target), processRole: .mainClassicOnly)
        let session = service.startSession()
        _ = service.insert(session: session, text: "コウ")
        XCTAssertEqual(try XCTUnwrap(service.convert(session: session)).first, "試験甲")
        XCTAssertTrue(service.clearLearning())
        service.endSession(session: session)
        service.prepareForShutdown()
        // Old history still exists in the source. A deliberate clear must not re-import it.
        XCTAssertEqual(LearningMigration.inherit(base: base, version: "2.0.0"), .existing)
        XCTAssertFalse(FileManager.default.fileExists(atPath: target.appendingPathComponent("memory0.loudstxt3").path))
    }

    func testLegacyRootIsEligibleButExistingDestinationIsPreserved() throws {
        let base = try temporaryBase()
        _ = try seed(base)
        let target = base.appendingPathComponent("2.0.0")
        try FileManager.default.createDirectory(at: target, withIntermediateDirectories: false)
        let foreign = target.appendingPathComponent("keep.txt")
        try Data("unchanged".utf8).write(to: foreign)
        XCTAssertEqual(LearningMigration.inherit(base: base, version: "2.0.0"), .existing)
        XCTAssertEqual(try String(contentsOf: foreign, encoding: .utf8), "unchanged")
        try FileManager.default.removeItem(at: foreign)
        XCTAssertEqual(LearningMigration.inherit(base: base, version: "2.0.0"), .inherited)
    }

    func testNewestBrokenSourceIsRejectedWithoutFallingBackToOlderHistory() throws {
        let base = try temporaryBase()
        let older = base.appendingPathComponent("1.0.0")
        let broken = base.appendingPathComponent("1.1.0")
        _ = try seed(older)
        _ = try seed(broken)
        try FileManager.default.setAttributes([.modificationDate: Date(timeIntervalSince1970: 1)],
            ofItemAtPath: older.appendingPathComponent("memory.memorymetadata").path)
        try Data([0]).write(to: broken.appendingPathComponent("memory0.loudstxt3"))
        XCTAssertEqual(LearningMigration.inherit(base: base, version: "2.0.0"), .rejected)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: base.appendingPathComponent("2.0.0").path),
                       [LearningMigration.marker])
        XCTAssertEqual(LearningMigration.inherit(base: base, version: "2.0.0"), .existing)
    }

    func testInterruptedVendorSaveCanBeRetriedAfterRecovery() throws {
        let base = try temporaryBase()
        let source = base.appendingPathComponent("1.0.0")
        _ = try seed(source)
        try Data().write(to: source.appendingPathComponent(".pause"))
        XCTAssertThrowsError(try LearningMigration.snapshot(source))
        XCTAssertEqual(LearningMigration.inherit(base: base, version: "2.0.0"), .unavailable)
        try FileManager.default.removeItem(at: source.appendingPathComponent(".pause"))
        XCTAssertEqual(LearningMigration.inherit(base: base, version: "2.0.0"), .inherited)
    }

    func testSavingStoreWithMissingCanonicalMetadataIsNotMistakenForNoHistory() throws {
        let base = try temporaryBase()
        let source = base.appendingPathComponent("1.1.0")
        _ = try seed(base.appendingPathComponent("1.0.0"))
        _ = try seed(source)
        let metadata = source.appendingPathComponent("memory.memorymetadata")
        let original = try Data(contentsOf: metadata)
        try Data().write(to: source.appendingPathComponent(".pause"))
        try FileManager.default.removeItem(at: metadata)
        XCTAssertEqual(LearningMigration.inherit(base: base, version: "2.0.0"), .unavailable)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: base.appendingPathComponent("2.0.0").path), [])
        try original.write(to: metadata)
        try FileManager.default.removeItem(at: source.appendingPathComponent(".pause"))
        XCTAssertEqual(LearningMigration.inherit(base: base, version: "2.0.0"), .inherited)
    }

    func testNoSourceInitializesOnlyOnceAndStaleStageIsRemoved() throws {
        let base = try temporaryBase()
        let stage = base.appendingPathComponent(LearningMigration.stageName("2.0.0"))
        try FileManager.default.createDirectory(at: stage, withIntermediateDirectories: false)
        try Data().write(to: stage.appendingPathComponent(LearningMigration.marker))
        try Data("old private data".utf8).write(to: stage.appendingPathComponent("memory0.loudstxt3"))
        XCTAssertEqual(LearningMigration.inherit(base: base, version: "2.0.0"), .empty)
        XCTAssertFalse(FileManager.default.fileExists(atPath: stage.path))
        _ = try seed(base.appendingPathComponent("1.0.0"))
        XCTAssertEqual(LearningMigration.inherit(base: base, version: "2.0.0"), .existing)
    }

    func testUpgradeOrderingIncludesBetaAndExcludesNewerBuilds() {
        XCTAssertTrue(LearningMigration.isEarlierBuild("1.6.0-beta.10", than: "1.6.0"))
        XCTAssertTrue(LearningMigration.isEarlierBuild("1.6.0-beta.9", than: "1.6.0-beta.10"))
        XCTAssertTrue(LearningMigration.isEarlierBuild("1.5.0+build.2", than: "1.6.0"))
        XCTAssertFalse(LearningMigration.isEarlierBuild("1.6.0", than: "1.6.0-beta.10"))
        XCTAssertFalse(LearningMigration.isEarlierBuild("1.7.0", than: "1.6.0"))
        XCTAssertFalse(LearningMigration.isEarlierBuild("1.6.0+build.2", than: "1.6.0"))
        XCTAssertFalse(LearningMigration.isEarlierBuild("../1.0.0", than: "1.6.0"))
    }

    func testUnownedStageIsPreserved() throws {
        let base = try temporaryBase()
        let stage = base.appendingPathComponent(LearningMigration.stageName("2.0.0"))
        try FileManager.default.createDirectory(at: stage, withIntermediateDirectories: false)
        try Data("keep".utf8).write(to: stage.appendingPathComponent("foreign.txt"))
        XCTAssertEqual(LearningMigration.inherit(base: base, version: "2.0.0"), .unavailable)
        XCTAssertEqual(try String(contentsOf: stage.appendingPathComponent("foreign.txt"), encoding: .utf8), "keep")
    }

    func testHeldStageCannotBeReplacedAndHandleOperationsPreserveIdentity() throws {
        let base = try temporaryBase()
        let stage = base.appendingPathComponent("owned")
        let renamed = base.appendingPathComponent("renamed")
        try FileManager.default.createDirectory(at: stage, withIntermediateDirectories: false)
        let lease = try LearningFileLease(stage, directory: true, deletable: true)
        let replaced = stage.path.withCString(encodedAs: UTF16.self) { from in
            renamed.path.withCString(encodedAs: UTF16.self) { MoveFileExW(from, $0, 0) }
        }
        XCTAssertFalse(replaced, "The open stage must deny path-based replacement")
        try lease.move(to: renamed)
        XCTAssertFalse(FileManager.default.fileExists(atPath: stage.path))
        XCTAssertTrue(FileManager.default.fileExists(atPath: renamed.path))
        let foreign = renamed.appendingPathComponent("foreign.txt")
        try Data("keep".utf8).write(to: foreign)
        XCTAssertThrowsError(try lease.delete(), "Deleting a nonempty directory must never recurse")
        XCTAssertEqual(try String(contentsOf: foreign, encoding: .utf8), "keep")
        try FileManager.default.removeItem(at: foreign)
        try lease.delete()
        XCTAssertFalse(FileManager.default.fileExists(atPath: renamed.path))
    }

    func testEveryTruncatedArtifactIsRejectedBeforeVendorLoad() throws {
        let base = try temporaryBase()
        let valid = try seed(base)
        for (name, data) in valid {
            for count in Set([0, 1, data.count / 2, data.count - 1]) where count >= 0 && count < data.count {
                var files = valid
                files[name] = data.prefix(count)
                XCTAssertThrowsError(try LearningStoreValidation.validate(files), "\(name) length=\(count)")
            }
        }
        var files = valid
        files["memory.louds"]!.append(Data(repeating: 0xff, count: 64))
        XCTAssertThrowsError(try LearningStoreValidation.validate(files), "Only one word of padding is valid")
        files = valid
        files["memory.louds"] = Data(repeating: 0xff, count: valid["memory.louds"]!.count)
        XCTAssertThrowsError(try LearningStoreValidation.validate(files))
        files = valid
        files["memory.loudschars2"]![0] = 255
        XCTAssertThrowsError(try LearningStoreValidation.validate(files))
    }
}
