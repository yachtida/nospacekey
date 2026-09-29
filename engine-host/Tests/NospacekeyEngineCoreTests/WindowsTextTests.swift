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

    func testHybridKeepsDictionaryClauseBoundariesInLiveAndExplicitConversion() throws {
        let reading = "きょうはいいてんきです"
        for explicit in [false, true] {
            for width in [1, 10] {
                let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
                    learning: .disabled, environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"],
                    fileSystem: .live, windowsTextProvider: .init { _, _, _ in ["Windows全文候補"] })
                let result = service.snapshot([.init(text: reading, style: "direct")],
                    explicit: explicit, liveSearchWidth: width)
                try ClauseCoordinates.validate(reading: reading, clauses: result.clauseData.clauses,
                    start: 0, end: UInt32(reading.unicodeScalars.count), text: result.text)
                XCTAssertGreaterThan(result.clauseData.clauses.count, 1)
                XCTAssertNotEqual(result.text, reading)
                XCTAssertTrue(result.clauseData.clauses.allSatisfy { $0.candidate_token != nil })
            }
        }
    }

    func testHybridLongLiveInputProposesAutoCommitWithoutSpace() throws {
        for width in [1, 10] {
            let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
                learning: .disabled, environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"],
                fileSystem: .live, windowsTextProvider: .init { _, _, _ in
                    XCTFail("ordinary hybrid live conversion must not wait for Windows alternatives")
                    return []
                })
            var reading = ""
            var committed = false
            for (offset, ch) in "きょうはいいてんきなのでながいぶんしょうをうちつづけています".enumerated() {
                reading.append(ch)
                let key = ConversionService.SnapshotEnhancementKey(composition: 1,
                    revision: UInt64(offset + 1), configurationGeneration: 1, connectionGeneration: 1)
                let result = service.snapshot([.init(text: reading, style: "direct")],
                    explicit: false, enhancementKey: key, snapshotConnection: 1, liveSearchWidth: width)
                if let proposal = result.autoCommit {
                    XCTAssertEqual(proposal.consumedReading + proposal.remaining, reading)
                    XCTAssertFalse(proposal.text.isEmpty)
                    XCTAssertFalse(proposal.remaining.isEmpty)
                    XCTAssertTrue(service.applySnapshotAutoCommitReceipt(connection: 1,
                        key: key, proposal: proposal.proposal))
                    committed = true
                    break
                }
            }
            XCTAssertTrue(committed, "long live input must commit a prefix before Space, width=\(width)")
        }
    }

    func testHybridAutoCommitContinuesAfterRomanInputIsReseededAsKana() throws {
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .disabled, environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"],
            fileSystem: .live, windowsTextProvider: .init { _, _, _ in [] })
        let roman = "kyouhaiitenkinanodenagaibunshouwoutitsuduketeimasu"
        let key = ConversionService.SnapshotEnhancementKey(composition: 1, revision: 1,
            configurationGeneration: 1, connectionGeneration: 1)
        let initial = service.snapshot([.init(text: roman, style: nil)], explicit: false,
            enhancementKey: key, snapshotConnection: 1, liveSearchWidth: 10)
        let first = try XCTUnwrap(initial.autoCommit)
        XCTAssertTrue(service.applySnapshotAutoCommitReceipt(connection: 1, key: key, proposal: first.proposal))
        var reading = first.remaining
        var commits = 0
        for (offset, ch) in "きょうはいいてんきなのでながいぶんしょうをうちつづけています".enumerated() {
            reading.append(ch)
            let nextKey = ConversionService.SnapshotEnhancementKey(composition: 1, revision: UInt64(offset + 2),
                configurationGeneration: 1, connectionGeneration: 1)
            let result = service.snapshot([.init(text: reading, style: "direct")], explicit: false,
                leftContext: first.text, enhancementKey: nextKey, snapshotConnection: 1, liveSearchWidth: 10)
            if let proposal = result.autoCommit {
                let retryKey = ConversionService.SnapshotEnhancementKey(composition: 1, revision: nextKey.revision,
                    configurationGeneration: 1, connectionGeneration: 1, requestID: UInt64(offset + 100))
                let retry = service.snapshot([.init(text: reading, style: "direct")], explicit: false,
                    leftContext: first.text, enhancementKey: retryKey, snapshotConnection: 1, liveSearchWidth: 10)
                XCTAssertEqual(retry.autoCommit?.proposal, proposal.proposal,
                    "A debounce/prediction retry of the same reading must retain its pending prefix")
                XCTAssertTrue(service.applySnapshotAutoCommitReceipt(connection: 1, key: retryKey, proposal: proposal.proposal))
                reading = proposal.remaining
                commits += 1
            }
        }
        XCTAssertGreaterThan(commits, 0)
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
        XCTAssertEqual(restored.inputPredictions(request("にゅう")).candidates.first?.surface, "辞書にない語",
            "A verified learned full reading can complete a shorter prefix before native predictions")
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

    func testHybridLiveNeverRequestsUnusedMicrosoftAlternatives() {
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .disabled, environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"],
            fileSystem: .live, windowsTextProvider: .init { _, _, _ in
                XCTFail("Hybrid live must not query Windows")
                return []
            })
        XCTAssertFalse(service.snapshot([.init(text: "にほんご", style: "direct")], explicit: false).text.isEmpty)
    }

    func testHybridPredictionPrefersLearningThenMicrosoftThenDictionary() throws {
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .init(enabled: true, memoryDir: nil),
            environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"], fileSystem: .live,
            windowsTextProvider: .init { _, _, limit in
                XCTAssertEqual(limit, 64)
                return ["Microsoft先頭", "Microsoft次点", "Microsoft先頭"]
            }, learningPersistenceForTesting: { _ in })
        let first = service.inputPredictions(request("がぞ"))
        XCTAssertEqual(Array(first.candidates.prefix(2).map(\.surface)), ["Microsoft先頭", "Microsoft次点"])
        let learned = try XCTUnwrap(first.candidates.first { $0.surface == "画像" })
        XCTAssertEqual(service.commitReceipt(receipt(service, candidate: learned,
            reading: "がぞ", explicit: true, prediction: true)).outcome, .applied)
        let ranked = service.inputPredictions(request("がぞ")).candidates.map(\.surface)
        XCTAssertEqual(Array(ranked.prefix(3)), ["画像", "Microsoft先頭", "Microsoft次点"])
        XCTAssertEqual(ranked.filter { $0 == "Microsoft先頭" }.count, 1)
        XCTAssertTrue(service.clearLearning())
        XCTAssertEqual(service.inputPredictions(request("がぞ")).candidates.first?.surface, "Microsoft先頭")
    }

    func testHybridPredictionPagesRetainBothSourcesAndTheirLearningRules() throws {
        let native = (0..<12).map { "Microsoft\($0)" }
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .init(enabled: true, memoryDir: nil),
            environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"], fileSystem: .live,
            windowsTextProvider: .init { _, _, _ in native + [native[0]] },
            learningPersistenceForTesting: { _ in })
        let predictions = service.inputPredictions(request("さんこう")).candidates
        XCTAssertEqual(Array(predictions.prefix(native.count).map(\.surface)), native)
        XCTAssertGreaterThan(predictions.count, native.count)
        XCTAssertLessThanOrEqual(predictions.count, 64)
        XCTAssertEqual(Set(predictions.map(\.surface)).count, predictions.count)
        XCTAssertEqual(Set(predictions.map(\.token)).count, predictions.count)
        XCTAssertTrue(predictions.allSatisfy { $0.reading_start == 0 && $0.reading_end == 4 })
        let windows = try XCTUnwrap(predictions.first { $0.surface == native[9] })
        let windowsReceipt = receipt(service, candidate: windows, reading: "さんこう",
            explicit: true, prediction: true)
        XCTAssertEqual(service.commitReceipt(windowsReceipt).outcome, .applied)
        XCTAssertEqual(service.commitReceipt(windowsReceipt).outcome, .alreadyProcessed)
        service.flushMaintenanceForTesting()
        XCTAssertEqual(service.recentLearningCountForTesting, 0)
        let dictionary = try XCTUnwrap(predictions.first { $0.surface == "参考" })
        XCTAssertEqual(service.commitReceipt(receipt(service, candidate: dictionary, reading: "さんこう",
            explicit: true, sequence: 2, prediction: true)).outcome, .applied)
        service.flushMaintenanceForTesting()
        XCTAssertEqual(service.recentLearningCountForTesting, 1)
    }

    func testHybridPredictionCapsLargeNativeListAndDeduplicatesDictionary() {
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .disabled, environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"],
            fileSystem: .live, windowsTextProvider: .init { _, _, limit in
                XCTAssertEqual(limit, 64)
                return (0..<80).map { "Microsoft\($0)" }
            })
        XCTAssertEqual(service.inputPredictions(request("さんこう")).candidates.map(\.surface),
            (0..<64).map { "Microsoft\($0)" })
        let duplicates = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .disabled, environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"],
            fileSystem: .live, windowsTextProvider: .init { _, _, _ in ["参考", "参考"] })
        let values = duplicates.inputPredictions(request("さんこう")).candidates.map(\.surface)
        XCTAssertEqual(values.first, "参考")
        XCTAssertEqual(values.filter { $0 == "参考" }.count, 1)
        XCTAssertGreaterThan(values.count, 9)
    }

    func testLearnedPredictionsContinuePastTheFirstPageInBothDictionaryModes() throws {
        let dir = try makeLearningDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let dictionary = dir.appendingPathComponent("user.json")
        let entries = (0..<12).map { ["ruby": "やちだ", "word": "学習候補\($0)", "pos": "名詞"] }
        try JSONSerialization.data(withJSONObject: entries).write(to: dictionary)
        for engine in ["azookey", "hybrid"] {
            let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
                learning: .init(enabled: true, memoryDir: nil),
                environment: ["NOSPACEKEY_CONVERSION_ENGINE": engine], fileSystem: .live,
                windowsTextProvider: .init { _, _, _ in [] }, learningPersistenceForTesting: { _ in })
            service.loadUserDictionary(from: dictionary)
            let offered = service.inputPredictions(request("やちだ")).candidates
                .filter { $0.surface.hasPrefix("学習候補") }
            XCTAssertEqual(offered.count, 12, engine)
            for (index, candidate) in offered.enumerated() {
                XCTAssertEqual(service.commitReceipt(receipt(service, candidate: candidate, reading: "やちだ",
                    explicit: true, sequence: UInt64(index + 1), prediction: true)).outcome, .applied)
            }
            service.flushMaintenanceForTesting()
            let exact = service.inputPredictions(request("やちだ")).candidates.map(\.surface)
            XCTAssertEqual(Array(exact.prefix(offered.count)), offered.reversed().map(\.surface), engine)
            let learned = service.inputPredictions(request("やち")).candidates.map(\.surface)
            XCTAssertEqual(Array(learned.prefix(offered.count)), offered.reversed().map(\.surface), engine)
        }
    }

    func testStoredMicrosoftCompletionsUseTheEngineBudgetWithoutAzooKeyLearning() throws {
        let dir = try makeLearningDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = MicrosoftLearningStore(directory: dir)
        for index in 0..<80 { store.record(reading: "やちだ", surface: "Microsoft学習\(index)") }
        store.flush()
        for (engine, limit) in [("hybrid", 64), ("microsoft", 80)] {
            let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
                learning: .init(enabled: true, memoryDir: dir),
                environment: ["NOSPACEKEY_CONVERSION_ENGINE": engine], fileSystem: .live,
                windowsTextProvider: .init { _, _, _ in [] })
            let predictions = service.inputPredictions(request("やち")).candidates
            XCTAssertEqual(predictions.map(\.surface),
                (0..<80).reversed().prefix(limit).map { "Microsoft学習\($0)" }, engine)
            let last = try XCTUnwrap(predictions.last)
            XCTAssertEqual(service.commitReceipt(receipt(service, candidate: last, reading: "やち",
                explicit: true, prediction: true)).outcome, .applied)
            service.flushMaintenanceForTesting()
            XCTAssertEqual(service.recentLearningCountForTesting, 0)
            XCTAssertFalse(service.inputPredictions(request("やちだ")).candidates
                .contains { $0.surface.hasPrefix("Microsoft学習") })
        }
    }

    func testHybridPredictionFallsBackToDictionaryWhenMicrosoftUnavailable() {
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .disabled, environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"],
            fileSystem: .live, windowsTextProvider: .init { _, _, _ in [] })
        XCTAssertTrue(service.inputPredictions(request("がぞ")).candidates.contains { $0.surface == "画像" })
    }

    func testEngineSwitchDuringWindowsPredictionDiscardsOldProviderResult() throws {
        let entered = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let finished = DispatchSemaphore(value: 0)
        let service = service(.init { _, _, _ in
            entered.signal()
            _ = release.wait(timeout: .now() + 3)
            return ["OldWindowsProvider"]
        })
        let query = request("がぞ")
        Thread.detachNewThread {
            XCTAssertTrue(service.inputPredictions(query).candidates.isEmpty)
            finished.signal()
        }
        XCTAssertEqual(entered.wait(timeout: .now() + 2), .success)
        XCTAssertTrue(service.reload(overrides: ["NOSPACEKEY_CONVERSION_ENGINE": "azookey",
            "NOSPACEKEY_ZENZAI": "off", "NOSPACEKEY_LEARNING": "0"]))
        release.signal()
        XCTAssertEqual(finished.wait(timeout: .now() + 3), .success)
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

    func testNativeWindowsProviderSurvivesConcurrentRepeatedQueries() throws {
        guard ProcessInfo.processInfo.environment["NOSPACEKEY_TEST_WINDOWS_TEXT"] == "1" else {
            throw XCTSkip("Opt-in Windows Japanese language API integration")
        }
        DispatchQueue.concurrentPerform(iterations: 12) { index in
            let prediction = index % 2 == 0
            let values = WindowsTextProvider.native.candidates("にゅうりょく", prediction, 9)
            XCTAssertFalse(values.isEmpty)
            XCTAssertEqual(Set(values).count, values.count)
            XCTAssertLessThanOrEqual(values.count, 9)
            if !prediction { XCTAssertTrue(values.contains("入力")) }
        }
    }
}
