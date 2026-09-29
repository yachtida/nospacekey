import Foundation
import XCTest
import KanaKanjiConverterModuleWithDefaultDictionary
@testable import NospacekeyEngineCore

final class InputPredictionTests: XCTestCase {
    private func request(_ reading: String) -> ClauseCandidatesRequest {
        .init(key: .init(identity: .init(composition: 1, revision: 1,
            configuration_generation: 1, connection_generation: 1), baseline: 0,
            conversion_revision: 0, clause_id: 1, request_id: 1), reading: reading,
            reading_start: 0, reading_end: UInt32(reading.unicodeScalars.count), preceding_surfaces: [])
    }

    func testDictionaryCompletesShortReadingWithoutGPUOrLearning() throws {
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .init(enabled: false, memoryDir: nil))
        let result = service.inputPredictions(request("がぞ"))
        XCTAssertTrue(result.candidates.contains { $0.surface == "画像" }, "\(result.candidates)")
        XCTAssertTrue(result.candidates.allSatisfy { $0.reading_start == 0 && $0.reading_end == 2 })
        XCTAssertTrue(service.inputPredictions(request("")).candidates.isEmpty)
        XCTAssertTrue(service.inputPredictions(request("がぞk")).candidates.isEmpty)
        XCTAssertEqual(service.recentLearningCountForTesting, 0)
    }

    func testExactReadingCandidatesRemainAlongsideCompletions() throws {
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .init(enabled: false, memoryDir: nil))
        let result = service.inputPredictions(request("きょう"))
        XCTAssertTrue(result.candidates.contains { $0.surface == "今日" }, "\(result.candidates)")
    }

    func testReferenceReadingsIncludeExactMatchesAndStayDistinct() {
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .init(enabled: false, memoryDir: nil))
        for (reading, expected) in [("にゅうりょく", "入力"), ("にゅうりょくちゅう", "入力中"),
                                    ("さんこう", "参考"), ("ほっかいどうには", "北海道には")] {
            let values = service.inputPredictions(request(reading)).candidates.map(\.surface)
            XCTAssertTrue(values.contains(expected), "\(reading): \(values)")
            XCTAssertEqual(values.count, Set(values).count)
            XCTAssertLessThanOrEqual(values.count, 64)
            if reading == "にゅうりょくちゅう" {
                XCTAssertFalse(values.contains("入力虫"))
            }
            if reading == "ほっかいどうには" {
                XCTAssertFalse(values.contains("北海道煮派"))
            }
            if reading == "さんこう" {
                XCTAssertGreaterThan(values.count, 9)
            }
            print("input-prediction \(reading): \(values.count) \(values)")
        }
    }

    func testCandidatesContinueBeyondNineAndKeepABoundedTotal() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .init(enabled: false, memoryDir: nil))
        let dictionary = dir.appendingPathComponent("user.json")
        for count in [24, 80] {
            let entries = (0..<count).map { ["ruby": "やちだ", "word": "候補\($0)", "pos": "名詞"] }
            try JSONSerialization.data(withJSONObject: entries).write(to: dictionary)
            service.loadUserDictionary(from: dictionary)
            let result = service.inputPredictions(request("やちだ")).candidates
            XCTAssertEqual(result.filter { $0.surface.hasPrefix("候補") }.count, min(count, 64))
            XCTAssertEqual(Set(result.map(\.token)).count, result.count)
            XCTAssertTrue(result.allSatisfy { $0.reading_start == 0 && $0.reading_end == 3 })
        }
    }

    func testPredictionReceiptLearnsFullReadingOnceAndChecksConsumedReading() throws {
        final class Learned: @unchecked Sendable {
            let lock = NSLock()
            var values: [String] = []
            func add(_ candidate: Candidate) { lock.lock(); values.append(candidate.data.map(\.ruby).joined()); lock.unlock() }
        }
        let learned = Learned()
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .init(enabled: true, memoryDir: dir), fileSystem: .live,
            learningPersistenceForTesting: { learned.add($0) })
        let candidate = try XCTUnwrap(service.inputPredictions(request("がぞ")).candidates.first { $0.surface == "画像" })
        func receipt(_ reading: String, sequence: UInt64) -> CommitReceipt {
            .init(commit_id: .init(client_instance: "a1111111-1111-4111-8111-111111111111", sequence: sequence),
                engine_epoch: service.engineEpoch, learning_generation: service.currentLearningGeneration,
                reading: reading, text: candidate.surface,
                intervals: [.init(reading_start: 0, reading_end: 2, surface: candidate.surface,
                    learning: .candidate(token: candidate.token, explicitlySelected: true))], sentence_token: candidate.token)
        }
        XCTAssertEqual(service.commitReceipt(receipt("がざ", sequence: 1)).outcome, .rejected(.invalidToken))
        let valid = receipt("がぞ", sequence: 2)
        XCTAssertEqual(service.commitReceipt(valid).outcome, .applied)
        XCTAssertEqual(service.commitReceipt(valid).outcome, .alreadyProcessed)
        service.flushMaintenanceForTesting()
        XCTAssertEqual(learned.values, ["ガゾウ"])
        XCTAssertEqual(service.inputPredictions(request("が" )).candidates.first?.surface, "画像")
        XCTAssertEqual(service.inputPredictions(request("がぞう")).candidates.first?.surface, "画像")
        XCTAssertTrue(service.clearLearning())
        XCTAssertEqual(service.recentLearningCountForTesting, 0)
    }
    func testUserDictionaryCompletesButPersistedSyntheticLearningIsNotSearched() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let legacy = "LegacySyntheticReadingPair"
        do {
            let old = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
                learning: .init(enabled: true, memoryDir: dir))
            old.recordForTesting(reading: "がぞろん", surface: legacy)
            let session = old.startSession()
            _ = old.insert(session: session, text: "gazoronn")
            XCTAssertEqual(old.convert(session: session)?.first, legacy)
            XCTAssertEqual(old.commit(session: session, index: 0)?.text, legacy)
            old.endSession(session: session)
            old.flushMaintenanceForTesting()
        }
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .init(enabled: true, memoryDir: dir))
        XCTAssertFalse(service.inputPredictions(request("がぞ")).candidates.contains { $0.surface == legacy })
        let dictionary = dir.appendingPathComponent("user.json")
        try Data(#"[{"ruby":"やちだてすと","word":"PredictionUserDictionary","pos":"名詞"}]"#.utf8).write(to: dictionary)
        service.loadUserDictionary(from: dictionary)
        XCTAssertTrue(service.inputPredictions(request("やちだて")).candidates.contains { $0.surface == "PredictionUserDictionary" })
        service.loadUserDictionary(from: nil)
        XCTAssertFalse(service.inputPredictions(request("やちだて")).candidates.contains { $0.surface == "PredictionUserDictionary" })
    }

}
