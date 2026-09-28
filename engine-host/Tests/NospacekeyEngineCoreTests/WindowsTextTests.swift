import Foundation
import XCTest
import KanaKanjiConverterModuleWithDefaultDictionary
@testable import NospacekeyEngineCore

final class WindowsTextTests: XCTestCase {
    private func makeLearningDirectory() throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-ms-learning-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    private func request(_ reading: String) -> ClauseCandidatesRequest {
        .init(key: .init(identity: .init(composition: 1, revision: 1,
            configuration_generation: 1, connection_generation: 1), baseline: 0,
            conversion_revision: 0, clause_id: 1, request_id: 1), reading: reading,
            reading_start: 0, reading_end: UInt32(reading.unicodeScalars.count), preceding_surfaces: [])
    }

    private func service(_ provider: WindowsTextProvider = .init { reading, prediction, _ in
        prediction ? ["入力", "入力してください", "入力フォーム", "入力フォーム"] : ["入力", "WindowsProviderFixture"]
    }, learningDirectory: URL? = nil) -> ConversionService {
        ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .init(enabled: true, memoryDir: learningDirectory),
            environment: ["NOSPACEKEY_CONVERSION_ENGINE": "microsoft"],
            fileSystem: .live, windowsTextProvider: provider)
    }

    private func receipt(_ service: ConversionService, candidate: ClauseCandidate,
                         reading: String, explicit: Bool, sequence: UInt64 = 1,
                         prediction: Bool = false) -> CommitReceipt {
        CommitReceipt(commit_id: .init(client_instance: "a1111111-1111-4111-8111-111111111111", sequence: sequence),
            engine_epoch: service.engineEpoch, learning_generation: service.currentLearningGeneration,
            reading: reading, text: candidate.surface,
            intervals: [.init(reading_start: 0, reading_end: UInt32(reading.unicodeScalars.count),
                surface: candidate.surface, learning: .candidate(token: candidate.token,
                    explicitlySelected: explicit))],
            sentence_token: prediction ? candidate.token : nil)
    }

    func testDefaultAndUnknownEnvironmentKeepAzooKey() {
        XCTAssertEqual(ConversionEngine.resolve(environment: [:]), .azookey)
        XCTAssertEqual(ConversionEngine.resolve(environment: ["NOSPACEKEY_CONVERSION_ENGINE": "unknown"]), .azookey)
        XCTAssertEqual(ConversionEngine.resolve(environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"]), .hybrid)
    }

    private func candidate(_ surface: String) -> Candidate {
        Candidate(text: surface, value: 1, composingCount: .inputCount(1),
            lastMid: MIDData.一般.mid,
            data: [DicdataElement(word: surface, ruby: "ア", cid: CIDData.一般名詞.cid,
                mid: MIDData.一般.mid, value: 0)])
    }

    func testHybridInterleavesInSourceOrderAndAzooKeyWinsDuplicates() {
        let azookey = (1...5).map { candidate("\($0)") }
        let microsoft = ["a", "b", "3", "c", "d", "e"].map(candidate)
        let (mixed, microsoftOnly) = ConversionService.interleaveAzookeyFirst(azookey, microsoft)
        XCTAssertEqual(mixed.map(\.text), ["1", "a", "2", "b", "3", "c", "4", "d", "5", "e"])
        XCTAssertEqual(microsoftOnly, Set(["a", "b", "c", "d", "e"]))
        XCTAssertTrue(mixed[4].isLearningTarget)
    }

    func testHybridSnapshotPreservesAzooKeyTopAndAddsWindowsOnlyCandidates() throws {
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .init(enabled: false, memoryDir: nil),
            environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"], fileSystem: .live,
            windowsTextProvider: .init { _, prediction, _ in
                prediction ? ["予測追加"] : ["Windows追加A", "Windows追加B"]
            })
        service.snapshotCandidatesForTesting = [candidate("AzooKey先頭"), candidate("AzooKey次点")]
        let segments = [SnapshotSegment(text: "あ", style: "direct")]
        let explicit = service.snapshot(segments, explicit: true, includeFlatCandidates: true)
        XCTAssertEqual(explicit.candidates, ["AzooKey先頭", "Windows追加A", "AzooKey次点", "Windows追加B"])
        XCTAssertEqual(service.snapshot(segments, explicit: false).text, "AzooKey先頭")
        XCTAssertEqual(explicit.clauseData.flat_candidates?.map(\.surface), explicit.candidates)
        XCTAssertTrue(service.inputPredictions(request("あ")).candidates.map(\.surface)
            .contains("予測追加"))
    }

    func testSelectedMicrosoftOnlyWordBecomesReviewableWithoutAutomaticLearning() throws {
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .init(enabled: false, memoryDir: nil),
            environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"], fileSystem: .live,
            windowsTextProvider: .init { _, _, _ in ["AzooKey先頭", "辞書にない語"] })
        service.snapshotCandidatesForTesting = [candidate("AzooKey先頭")]
        let snapshot = service.snapshot([.init(text: "あ", style: "direct")],
            explicit: true, includeFlatCandidates: true)
        let selected = try XCTUnwrap(snapshot.clauseData.flat_candidates?.first {
            $0.surface == "辞書にない語"
        })
        let receipt = CommitReceipt(commit_id: .init(client_instance: "a1111111-1111-4111-8111-111111111111", sequence: 1),
            engine_epoch: service.engineEpoch, learning_generation: service.currentLearningGeneration,
            reading: "あ", text: selected.surface,
            intervals: [.init(reading_start: 0, reading_end: 1, surface: selected.surface,
                learning: .candidate(token: selected.token, explicitlySelected: true))], sentence_token: nil)
        XCTAssertEqual(service.commitReceipt(receipt).outcome, .applied)
        XCTAssertEqual(service.commitReceipt(receipt).outcome, .alreadyProcessed)
        XCTAssertEqual(service.recentMicrosoftSelections(), [.init(ruby: "あ", word: "辞書にない語")])
        XCTAssertEqual(service.recentLearningCountForTesting, 0)
    }

    func testExplicitMicrosoftConversionLearnsFullReadingAndSurvivesRestart() throws {
        let dir = try makeLearningDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let provider = WindowsTextProvider { _, prediction, _ in
            prediction ? ["入力予測"] : ["入力", "辞書にない語"]
        }
        let original = service(provider, learningDirectory: dir)
        let segments = [SnapshotSegment(text: "にゅうりょく", style: "direct")]
        let snapshot = original.snapshot(segments, explicit: true, includeFlatCandidates: true)
        let selected = try XCTUnwrap(snapshot.clauseData.flat_candidates?.first {
            $0.surface == "辞書にない語"
        })
        XCTAssertEqual(original.commitReceipt(receipt(original, candidate: selected,
            reading: "にゅうりょく", explicit: true)).outcome, .applied)
        original.prepareForShutdown()

        let restored = service(.init { _, _, _ in [] }, learningDirectory: dir)
        XCTAssertEqual(restored.snapshot(segments, explicit: true).candidates, ["辞書にない語"])
        XCTAssertEqual(restored.snapshot(segments, explicit: false).text, "辞書にない語")
        XCTAssertTrue(restored.inputPredictions(request("にゅうりょく")).candidates.isEmpty,
            "A learned conversion must never become a Tab prediction")
        XCTAssertTrue(restored.clearLearning())
        XCTAssertFalse(FileManager.default.fileExists(atPath:
            dir.appendingPathComponent("microsoft-candidates.json").path))
        XCTAssertTrue(restored.snapshot(segments, explicit: true).candidates?.isEmpty == true)
    }

    func testHybridLearnedMicrosoftWordKeepsAzooKeyFirst() throws {
        let dir = try makeLearningDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        func hybrid(_ provider: WindowsTextProvider) -> ConversionService {
            let result = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
                learning: .init(enabled: true, memoryDir: dir),
                environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"],
                fileSystem: .live, windowsTextProvider: provider)
            result.snapshotCandidatesForTesting = [candidate("AzooKey先頭")]
            return result
        }
        let original = hybrid(.init { _, _, _ in ["AzooKey先頭", "辞書にない語"] })
        let segments = [SnapshotSegment(text: "あ", style: "direct")]
        let snapshot = original.snapshot(segments, explicit: true, includeFlatCandidates: true)
        let selected = try XCTUnwrap(snapshot.clauseData.flat_candidates?.first {
            $0.surface == "辞書にない語"
        })
        XCTAssertEqual(original.commitReceipt(receipt(original, candidate: selected,
            reading: "あ", explicit: true)).outcome, .applied)
        original.prepareForShutdown()

        let restored = hybrid(.init { _, _, _ in [] })
        XCTAssertEqual(restored.snapshot(segments, explicit: true).candidates,
            ["AzooKey先頭", "辞書にない語"])
        XCTAssertEqual(restored.snapshot(segments, explicit: false).text, "AzooKey先頭")
        XCTAssertEqual(restored.recentLearningCountForTesting, 0,
            "Microsoft-only candidates must not enter AzooKey learning")
    }

    func testLearningEnabledAfterDisabledStartupUsesResolvedDirectory() throws {
        let base = try makeLearningDirectory()
        defer { try? FileManager.default.removeItem(at: base) }
        let directory = base.appendingPathComponent(BuildInfo.version)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let prior = MicrosoftLearningStore(directory: directory)
        prior.record(reading: "にゅうりょく", surface: "以前の選択")
        prior.flush()

        let original = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .disabled,
            environment: ["NOSPACEKEY_CONVERSION_ENGINE": "microsoft",
                "NOSPACEKEY_MEMORY_DIR": base.path], fileSystem: .live,
            windowsTextProvider: .init { _, _, _ in ["新しい選択"] })
        let segments = [SnapshotSegment(text: "にゅうりょく", style: "direct")]
        XCTAssertEqual(original.snapshot(segments, explicit: true).candidates, ["新しい選択"])
        XCTAssertTrue(original.reload(overrides: [
            "NOSPACEKEY_CONVERSION_ENGINE": "microsoft", "NOSPACEKEY_LEARNING": "1",
            "NOSPACEKEY_MEMORY_DIR": base.path, "NOSPACEKEY_ZENZAI": "off"
        ]))
        let snapshot = original.snapshot(segments, explicit: true, includeFlatCandidates: true)
        XCTAssertEqual(snapshot.candidates, ["以前の選択", "新しい選択"])
        let selected = try XCTUnwrap(snapshot.clauseData.flat_candidates?.first {
            $0.surface == "新しい選択"
        })
        XCTAssertEqual(original.commitReceipt(receipt(original, candidate: selected,
            reading: "にゅうりょく", explicit: true)).outcome, .applied)
        original.prepareForShutdown()
        XCTAssertEqual(MicrosoftLearningStore(directory: directory).surfaces(reading: "にゅうりょく"),
            ["新しい選択", "以前の選択"])
    }

    func testTabPredictionAndImplicitMicrosoftConversionDoNotLearn() throws {
        let dir = try makeLearningDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let original = service(.init { _, prediction, _ in
            prediction ? ["入力フォーム"] : ["入力"]
        }, learningDirectory: dir)
        let predicted = try XCTUnwrap(original.inputPredictions(request("にゅうりょく")).candidates.first)
        XCTAssertEqual(original.commitReceipt(receipt(original, candidate: predicted,
            reading: "にゅうりょく", explicit: true, prediction: true)).outcome, .applied)
        XCTAssertTrue(original.recentMicrosoftSelections().isEmpty)
        let live = original.snapshot([.init(text: "にゅうりょく", style: "direct")], explicit: false)
        let token = try XCTUnwrap(live.clauseData.clauses.first?.candidate_token)
        let implicit = ClauseCandidate(surface: live.text, token: token, reading_start: 0,
            reading_end: UInt32(live.reading.unicodeScalars.count))
        XCTAssertEqual(original.commitReceipt(receipt(original, candidate: implicit,
            reading: live.reading, explicit: false, sequence: 2)).outcome, .applied)
        original.prepareForShutdown()
        XCTAssertFalse(FileManager.default.fileExists(atPath:
            dir.appendingPathComponent("microsoft-candidates.json").path))
    }

    func testWindowsCandidatesServeExplicitAndLiveConversionWithExactReadingCoverage() {
        let service = service()
        let segments = [SnapshotSegment(text: "にゅうりょく", style: "direct")]
        let explicit = service.snapshot(segments, explicit: true, includeFlatCandidates: true)
        XCTAssertEqual(explicit.candidates, ["入力", "WindowsProviderFixture"])
        XCTAssertEqual(explicit.candidateRemaining, ["", ""])
        let live = service.snapshot(segments, explicit: false)
        XCTAssertEqual(live.text, "入力")
        XCTAssertEqual(live.reading, "にゅうりょく")
        XCTAssertNil(live.autoCommit)
        XCTAssertFalse(service.zenzaiEnabled)
        XCTAssertTrue(service.reload(overrides: ["NOSPACEKEY_ZENZAI": "off", "NOSPACEKEY_LEARNING": "0"]))
        XCTAssertEqual(service.snapshot(segments, explicit: true).candidates, explicit.candidates,
                       "Older ReloadConfig without an engine field must preserve the selected provider")
    }

    func testPredictionKeepsWindowsOrderAndCommitsOnlyTypedPrefixWithoutAzooKeyLearning() throws {
        let service = service()
        let predictions = service.inputPredictions(request("にゅうりょく"))
        XCTAssertEqual(predictions.candidates.map(\.surface), ["入力", "入力してください", "入力フォーム"])
        let candidate = try XCTUnwrap(predictions.candidates.last)
        let receipt = CommitReceipt(commit_id: .init(client_instance: "a1111111-1111-4111-8111-111111111111", sequence: 1),
            engine_epoch: service.engineEpoch, learning_generation: service.currentLearningGeneration,
            reading: "にゅうりょく", text: candidate.surface,
            intervals: [.init(reading_start: 0, reading_end: 6, surface: candidate.surface,
                learning: .candidate(token: candidate.token, explicitlySelected: true))], sentence_token: candidate.token)
        XCTAssertEqual(service.commitReceipt(receipt).outcome, .applied)
        XCTAssertEqual(service.commitReceipt(receipt).outcome, .alreadyProcessed)
        service.flushMaintenanceForTesting()
        XCTAssertEqual(service.recentLearningCountForTesting, 0)
        XCTAssertTrue(service.reload(overrides: ["NOSPACEKEY_CONVERSION_ENGINE": "azookey", "NOSPACEKEY_ZENZAI": "off", "NOSPACEKEY_LEARNING": "0"]))
        let switched = service.snapshot([.init(text: "にゅうりょく", style: "direct")], explicit: true)
        XCTAssertFalse(switched.candidates?.contains("WindowsProviderFixture") == true)
        let stale = CommitReceipt(commit_id: .init(client_instance: "a1111111-1111-4111-8111-111111111111", sequence: 2),
            engine_epoch: service.engineEpoch, learning_generation: service.currentLearningGeneration,
            reading: receipt.reading, text: receipt.text, intervals: receipt.intervals, sentence_token: nil)
        if case .rejected = service.commitReceipt(stale).outcome {} else { XCTFail("Old engine token accepted") }
    }

    func testLongWindowsLiveConversionDoesNotInventAutomaticClauseBoundaries() {
        let service = service(.init { _, _, _ in ["長い文章の候補"] })
        let reading = String(repeating: "にほんご", count: 12)
        let result = service.snapshot([.init(text: reading, style: "direct")], explicit: false)
        XCTAssertEqual(result.text, "長い文章の候補")
        XCTAssertEqual(result.reading, reading)
        XCTAssertNil(result.autoCommit)
        XCTAssertEqual(result.clauseData.clauses.count, 1)
    }

    func testUnavailableWindowsProviderPreservesReadingAndHasNoPrediction() {
        let service = service(.init { _, _, _ in [] })
        let result = service.snapshot([.init(text: "にゅうりょく", style: "direct")], explicit: false)
        XCTAssertEqual(result.text, "にゅうりょく")
        XCTAssertTrue(service.inputPredictions(request("にゅうりょく")).candidates.isEmpty)
    }

    func testSustainedWindowsPredictionKeepsNewCandidatesAfterTokenCapacity() {
        let service = service(.init { _, _, _ in (0..<256).map { "候補\($0)" } })
        for _ in 0..<40 {
            XCTAssertEqual(service.inputPredictions(request("にゅうりょく")).candidates.count, 256)
        }
    }

    func testUnavailableProviderReadingReceiptDoesNotEnterAzooKeyLearning() throws {
        let service = service(.init { _, _, _ in [] })
        let live = service.snapshot([.init(text: "にゅうりょく", style: "direct")], explicit: false)
        let token = try XCTUnwrap(live.clauseData.clauses.first?.candidate_token)
        let receipt = CommitReceipt(commit_id: .init(client_instance: "a1111111-1111-4111-8111-111111111111", sequence: 1),
            engine_epoch: service.engineEpoch, learning_generation: service.currentLearningGeneration,
            reading: live.reading, text: live.text,
            intervals: [.init(reading_start: 0, reading_end: 6, surface: live.text,
                learning: .candidate(token: token, explicitlySelected: false))], sentence_token: nil)
        XCTAssertEqual(service.commitReceipt(receipt).outcome, .applied)
        service.flushMaintenanceForTesting()
        XCTAssertEqual(service.recentLearningCountForTesting, 0)
    }

    func testNativeWindowsAPIConversionAndPrediction() throws {
        guard ProcessInfo.processInfo.environment["NOSPACEKEY_TEST_WINDOWS_TEXT"] == "1" else {
            throw XCTSkip("Opt-in Windows Japanese language API integration")
        }
        XCTAssertTrue(WindowsTextProvider.native.candidates("にゅうりょく", false, 256).contains("入力"))
        XCTAssertTrue(WindowsTextProvider.native.candidates("にゅうりょく", true, 256).contains("入力フォーム"))
        XCTAssertTrue(WindowsTextProvider.native.candidates("きょうはいいてんきです", false, 256).contains("今日はいい天気です"))
    }
}
