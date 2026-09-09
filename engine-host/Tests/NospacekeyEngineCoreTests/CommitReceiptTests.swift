import Foundation
import XCTest
import KanaKanjiConverterModuleWithDefaultDictionary
@testable import NospacekeyEngineCore

final class CommitReceiptTests: XCTestCase {
    private final class Events: @unchecked Sendable {
        private let lock = NSLock()
        private var values: [String] = []
        func record(_ candidate: Candidate) { lock.lock(); defer { lock.unlock() }; values.append(candidate.text) }
        func read() -> [String] { lock.lock(); defer { lock.unlock() }; return values }
    }
    private func service(_ events: Events) -> ConversionService {
        ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: true, memoryDir: FileManager.default.temporaryDirectory),
            fileSystem: .live, learningPersistenceForTesting: { events.record($0) })
    }
    private func candidate(_ ruby: String, _ text: String) -> Candidate {
        Candidate(text: text, value: 100, composingCount: .inputCount(ruby.count),
            lastMid: MIDData.一般.mid, data: [DicdataElement(word: text, ruby: ruby,
                cid: CIDData.固有名詞.cid, mid: MIDData.一般.mid, value: 0)])
    }
    private func receipt(_ service: ConversionService, id: CommitId? = nil,
                         generation: UInt64? = nil, reading: String = "きょう",
                         text: String = "今日", intervals: [CommitInterval], sentence: String? = nil) -> CommitReceipt {
        CommitReceipt(commit_id: id ?? CommitId(client_instance: UUID().uuidString, sequence: 1),
            engine_epoch: service.engineEpoch, learning_generation: generation ?? service.currentLearningGeneration,
            reading: reading, text: text, intervals: intervals, sentence_token: sentence)
    }
    private func interval(_ token: String, end: UInt32 = 3, surface: String = "今日") -> CommitInterval {
        CommitInterval(reading_start: 0, reading_end: end, surface: surface,
                       learning: .candidate(token: token, explicitlySelected: false))
    }

    func testAckLossReplaysWithoutLearningAgainAfterTokenExpiryOrSessionEnd() throws {
        let events = Events()
        let tracked = self.service(events)
        tracked.clauseClock = { 100 }
        let data = tracked.snapshotClausesForTesting(reading: "きょう", candidate: candidate("キョウ", "今日"))
        let token = try XCTUnwrap(data.clauses.first?.candidate_token)
        let payload = receipt(tracked, intervals: [interval(token)])
        let session = tracked.startSession()
        tracked.endSession(session: session)
        XCTAssertEqual(tracked.commitReceipt(payload).outcome, .applied)
        tracked.clauseClock = { 159 }
        XCTAssertEqual(tracked.commitReceipt(payload).outcome, .alreadyProcessed)
        tracked.flushMaintenanceForTesting()
        XCTAssertEqual(events.read(), ["今日"])
    }

    func testSentenceSeedCorrectionAndUnlearnMatchLegacySelectionConditions() throws {
        for (index, top, promoted, target, expected) in [
            (0, "今日", false, true, Optional("既存")),
            (1, "今日", false, true, Optional("既存")),
            (1, "今日", true, true, nil),
            (1, "京", false, true, Optional("今日")),
            (1, "京", false, false, Optional("既存"))
        ] {
            let dir = FileManager.default.temporaryDirectory.appendingPathComponent("sentence-receipt-\(UUID().uuidString)")
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            defer { try? FileManager.default.removeItem(at: dir) }
            let tracked = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                learning: LearningSettings(enabled: true, memoryDir: dir), fileSystem: .live, learningPersistenceForTesting: { _ in })
            tracked.recordForTesting(reading: "きょう", surface: "既存")
            var chosen = candidate("キョウ", "今日")
            chosen.isLearningTarget = target
            let token = try XCTUnwrap(tracked.retainClauseCandidateForTesting(chosen, start: 0, end: 3,
                modelTop: top, sentenceSelectionIndex: index, promoted: promoted))
            let payload = receipt(tracked, intervals: [interval(token.token)], sentence: token.token)
            XCTAssertEqual(tracked.commitReceipt(payload).outcome, .applied)
            XCTAssertEqual(tracked.correctionLookupForTesting(reading: "きょう"), expected)
            tracked.recordForTesting(reading: "きょう", surface: "再訂正")
            XCTAssertEqual(tracked.commitReceipt(payload).outcome, .alreadyProcessed)
            XCTAssertEqual(tracked.correctionLookupForTesting(reading: "きょう"), "再訂正")
            tracked.flushMaintenanceForTesting()
        }
    }

    func testSentenceSeedIsValidatedBeforeAnyLearningAndOnlyActsOnUnchangedSurface() throws {
        let events = Events()
        let tracked = service(events)
        let selected = try XCTUnwrap(tracked.retainClauseCandidateForTesting(candidate("キョウ", "今日"),
            start: 0, end: 3, modelTop: "京", sentenceSelectionIndex: 1))
        let changed = try XCTUnwrap(tracked.retainClauseCandidateForTesting(candidate("キョウ", "京"), start: 0, end: 3))
        let bad = receipt(tracked, intervals: [interval(selected.token)], sentence: "invented")
        XCTAssertEqual(tracked.commitReceipt(bad).outcome, .rejected(.invalidToken))
        tracked.flushMaintenanceForTesting()
        XCTAssertTrue(events.read().isEmpty)
        tracked.recordForTesting(reading: "きょう", surface: "既存")
        let modified = receipt(tracked, text: "京", intervals: [interval(changed.token, surface: "京")], sentence: selected.token)
        XCTAssertEqual(tracked.commitReceipt(modified).outcome, .applied)
        XCTAssertEqual(tracked.correctionLookupForTesting(reading: "きょう"), "既存")
        // An ordinary interval token has no sentence selection provenance.
        XCTAssertEqual(tracked.commitReceipt(receipt(tracked, intervals: [interval(selected.token)], sentence: changed.token)).outcome,
            .rejected(.invalidToken))
        tracked.flushMaintenanceForTesting()
    }

    func testExpiredSentenceSeedRejectsEvenWhenAllIntervalTokensAreFresh() throws {
        let events = Events()
        let tracked = service(events)
        tracked.clauseClock = { 100 }
        let seed = try XCTUnwrap(tracked.retainClauseCandidateForTesting(candidate("キョウ", "今日"),
            start: 0, end: 3, modelTop: "京", sentenceSelectionIndex: 1))
        tracked.clauseClock = { 160 }
        let fresh = try XCTUnwrap(tracked.retainClauseCandidateForTesting(candidate("キョウ", "今日"), start: 0, end: 3))
        XCTAssertEqual(tracked.commitReceipt(receipt(tracked, intervals: [interval(fresh.token)], sentence: seed.token)).outcome,
            .rejected(.expired))
        tracked.flushMaintenanceForTesting()
        XCTAssertTrue(events.read().isEmpty)
    }

    func testNativeWholeReadingCandidateTokenCarriesSentenceUnlearnProvenance() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("sentence-native-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let tracked = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: true, memoryDir: dir), fileSystem: .live, learningPersistenceForTesting: { _ in })
        let identity = ClauseSnapshotIdentity(composition: 1, revision: 1, configuration_generation: 1, connection_generation: 1)
        let snapshotKey = ConversionService.SnapshotEnhancementKey(composition: 1, revision: 1,
            configurationGeneration: 1, connectionGeneration: 1, conversionRevision: 0, requestID: 1)
        let baseline = tracked.snapshot([SnapshotSegment(text: "きょう", style: "direct")], explicit: true,
            enhancementKey: snapshotKey).baseline
        func request(_ id: UInt64) -> ClauseCandidatesRequest {
            ClauseCandidatesRequest(key: ClauseRequestKey(identity: identity, baseline: baseline,
                conversion_revision: 0, clause_id: 1, request_id: id), reading: "きょう",
                reading_start: 0, reading_end: 3, preceding_surfaces: [])
        }
        guard case .ready(let original) = tracked.clauseCandidates(request(2)).outcome,
              let top = original.first else { return XCTFail("native candidates unavailable") }
        tracked.recordForTesting(reading: "きょう", surface: "全文訂正試験")
        guard case .ready(let promoted) = tracked.clauseCandidates(request(3)).outcome,
              let chosen = promoted.first(where: { $0.surface == top.surface }) else {
            return XCTFail("model top missing after promotion")
        }
        XCTAssertEqual(promoted.first?.surface, "全文訂正試験")
        let payload = receipt(tracked, text: chosen.surface, intervals: [interval(chosen.token, surface: chosen.surface)],
            sentence: chosen.token)
        XCTAssertEqual(tracked.commitReceipt(payload).outcome, .applied)
        XCTAssertNil(tracked.correctionLookupForTesting(reading: "きょう"))
        tracked.flushMaintenanceForTesting()
    }

    func testWholeReceiptValidationPreventsPartialLearningAndStoresRejection() throws {
        let events = Events()
        let tracked = self.service(events)
        let token = try XCTUnwrap(tracked.snapshotClausesForTesting(reading: "きょう",
            candidate: candidate("キョウ", "今日")).clauses.first?.candidate_token)
        let payload = receipt(tracked, reading: "きょうは", text: "今日は", intervals: [interval(token),
            CommitInterval(reading_start: 3, reading_end: 4, surface: "は",
                           learning: .candidate(token: "unknown", explicitlySelected: false))])
        XCTAssertEqual(tracked.commitReceipt(payload).outcome, .rejected(.invalidToken))
        XCTAssertEqual(tracked.commitReceipt(payload).outcome, .rejected(.invalidToken))
        tracked.flushMaintenanceForTesting()
        XCTAssertTrue(events.read().isEmpty)
        let changed = receipt(tracked, id: payload.commit_id, intervals: [interval(token)])
        XCTAssertEqual(tracked.commitReceipt(changed).outcome, .rejected(.conflict))
    }

    func testExpiredTokenRejectsBeforeLearningButLedgerLivesFromFirstReceipt() throws {
        let tracked = service(Events())
        tracked.clauseClock = { 100 }
        let token = try XCTUnwrap(tracked.snapshotClausesForTesting(reading: "きょう",
            candidate: candidate("キョウ", "今日")).clauses.first?.candidate_token)
        let payload = receipt(tracked, intervals: [interval(token)])
        tracked.clauseClock = { 159 }
        XCTAssertEqual(tracked.commitReceipt(payload).outcome, .applied)
        tracked.clauseClock = { 160 }
        XCTAssertEqual(tracked.commitReceipt(payload).outcome, .alreadyProcessed)
        let newPayload = receipt(tracked, intervals: [interval(token)])
        XCTAssertEqual(tracked.commitReceipt(newPayload).outcome, .rejected(.expired))
        // Pruning storage must preserve Expired versus an invented token.
        _ = tracked.snapshotClausesForTesting(reading: "きょう", candidate: candidate("キョウ", "今日"))
        XCTAssertEqual(tracked.commitReceipt(receipt(tracked, intervals: [interval(token)])).outcome, .rejected(.expired))
    }

    func testTokenCannotBeReboundToAnotherReadingOrSurface() throws {
        let tracked = service(Events())
        let token = try XCTUnwrap(tracked.snapshotClausesForTesting(reading: "きょう",
            candidate: candidate("キョウ", "今日")).clauses.first?.candidate_token)
        XCTAssertEqual(tracked.commitReceipt(receipt(tracked, reading: "あした", intervals: [interval(token)])).outcome,
                       .rejected(.invalidToken))
        XCTAssertEqual(tracked.commitReceipt(receipt(tracked, text: "凶", intervals: [interval(token, surface: "凶")])).outcome,
                       .rejected(.invalidToken))
    }

    func testSnapshotDecompositionPreservesNonLearningTarget() throws {
        let events = Events()
        let actual = service(events)
        var excluded = candidate("キョウ", "今日")
        excluded.isLearningTarget = false
        let token = try XCTUnwrap(actual.snapshotClausesForTesting(reading: "きょう", candidate: excluded)
            .clauses.first?.candidate_token)
        XCTAssertEqual(actual.commitReceipt(receipt(actual, intervals: [interval(token)])).outcome, .applied)
        actual.flushMaintenanceForTesting()
        XCTAssertTrue(events.read().isEmpty)
    }

    func testLedgerBoundsAndLifetimeDoNotRefreshOnReplay() {
        let tracked = service(Events())
        let payload = receipt(tracked, reading: "きょう", text: "きょう", intervals: [
            CommitInterval(reading_start: 0, reading_end: 3, surface: "きょう", learning: .none(.reading))])
        var ledger = CommitReceiptLedger(capacity: 1)
        var count = 0
        XCTAssertEqual(ledger.process(payload, now: 0) { count += 1; return .applied }.outcome, .applied)
        XCTAssertEqual(ledger.process(payload, now: 59) { XCTFail("replay"); return .applied }.outcome, .alreadyProcessed)
        let other = receipt(tracked, intervals: [])
        XCTAssertEqual(ledger.process(other, now: 59) { XCTFail("capacity"); return .applied }.outcome, .rejected(.capacity))
        XCTAssertEqual(ledger.process(other, now: 60) { count += 1; return .rejected(.invalidIntervals) }.outcome, .rejected(.invalidIntervals))
        XCTAssertEqual(ledger.process(other, now: 61) { XCTFail("rejected replay"); return .applied }.outcome, .rejected(.invalidIntervals))
        XCTAssertEqual(count, 2)
    }

    func testSavedLearningPreservesAdjacentRunButDoesNotBridgeNonLearningIntervals() throws {
        for gap in ["none", "reading", "nonTarget"] {
            let dir = FileManager.default.temporaryDirectory.appendingPathComponent("receipt-runs-\(UUID().uuidString)")
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            defer { try? FileManager.default.removeItem(at: dir) }
            let tracked = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                learning: LearningSettings(enabled: true, memoryDir: dir))
            let first = try XCTUnwrap(tracked.retainClauseCandidateForTesting(candidate("コウ", "試験甲"), start: 0, end: 2))
            var intervals = [interval(first.token, end: 2, surface: "試験甲")]
            var reading = "こう"
            if gap != "none" {
                reading += "は"
                var learning: IntervalLearning = .none(.reading)
                if gap == "nonTarget" {
                    var excluded = candidate("ハ", "は")
                    excluded.isLearningTarget = false
                    let token = try XCTUnwrap(tracked.retainClauseCandidateForTesting(excluded, start: 2, end: 3))
                    learning = .candidate(token: token.token, explicitlySelected: true)
                }
                intervals.append(CommitInterval(reading_start: 2, reading_end: 3, surface: "は", learning: learning))
            }
            let start = UInt32(reading.unicodeScalars.count)
            let last = try XCTUnwrap(tracked.retainClauseCandidateForTesting(candidate("オツ", "試験乙"), start: start, end: start + 2))
            intervals.append(CommitInterval(reading_start: start, reading_end: start + 2, surface: "試験乙",
                learning: .candidate(token: last.token, explicitlySelected: false)))
            reading += "おつ"
            XCTAssertEqual(tracked.commitReceipt(receipt(tracked, reading: reading,
                text: intervals.map(\.surface).joined(), intervals: intervals)).outcome, .applied)
            tracked.flushMaintenanceForTesting()
            let files = try FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)
                .filter { $0.lastPathComponent.hasPrefix("memory") && $0.pathExtension == "loudstxt3" }
            XCTAssertFalse(files.isEmpty, "missing persistence cannot prove run isolation")
            let bytes = try files.map { try Data(contentsOf: $0) }
            func contains(_ text: String) -> Bool { bytes.contains { $0.range(of: Data(("\t" + text).utf8)) != nil } }
            XCTAssertTrue(contains("試験甲"), gap)
            XCTAssertTrue(contains("試験乙"), gap)
            XCTAssertEqual(contains("試験甲試験乙"), gap == "none", gap)
            tracked.prepareForShutdown()
        }
    }

    func testSettingsChangeIsBusyDuringWriteAndOldReceiptsStayTerminatedAfterReenable() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("receipt-config-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
        let tracked = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: true, memoryDir: dir), fileSystem: .live,
            learningPersistenceForTesting: { _ in entered.signal(); release.wait() })
        let token = try XCTUnwrap(tracked.snapshotClausesForTesting(reading: "きょう",
            candidate: candidate("キョウ", "今日")).clauses.first?.candidate_token)
        let payload = receipt(tracked, intervals: [interval(token)])
        XCTAssertEqual(tracked.commitReceipt(payload).outcome, .applied)
        XCTAssertEqual(entered.wait(timeout: .now() + 2), .success)
        let off = ["NOSPACEKEY_LEARNING": "0", "NOSPACEKEY_ZENZAI": "off", "NOSPACEKEY_MEMORY_DIR": dir.path]
        XCTAssertFalse(tracked.reload(overrides: off), "OFF cannot report completion while an old write is executing")
        XCTAssertEqual(tracked.currentLearningGeneration, payload.learning_generation)
        release.signal()
        tracked.flushMaintenanceForTesting()
        XCTAssertTrue(tracked.reload(overrides: off))
        XCTAssertGreaterThan(tracked.currentLearningGeneration, payload.learning_generation)
        XCTAssertEqual(tracked.commitReceipt(payload).outcome, .alreadyProcessed)
        let pending = receipt(tracked, generation: payload.learning_generation, intervals: [interval(token)])
        XCTAssertEqual(tracked.commitReceipt(pending).outcome, .rejected(.staleLearningGeneration))
        let generation = tracked.currentLearningGeneration
        XCTAssertTrue(tracked.reload(overrides: ["NOSPACEKEY_LEARNING": "1", "NOSPACEKEY_ZENZAI": "off", "NOSPACEKEY_MEMORY_DIR": dir.path]))
        XCTAssertGreaterThan(tracked.currentLearningGeneration, generation)
        XCTAssertEqual(tracked.commitReceipt(pending).outcome, .rejected(.staleLearningGeneration))
        XCTAssertEqual(tracked.commitReceipt(payload).outcome, .alreadyProcessed)
    }
}
