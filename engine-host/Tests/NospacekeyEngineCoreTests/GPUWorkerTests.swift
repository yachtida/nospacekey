import Foundation
import XCTest
import KanaKanjiConverterModuleWithDefaultDictionary
@testable import NospacekeyEngineCore

#if DEBUG
final class GPUWorkerFaultTests: XCTestCase {
    func testTypedFaultNamesMapToPrivateWorkerHandshakeFailures() {
        XCTAssertEqual(
            GPUWorkerTestFault.parse("typed:gpu_unavailable")?.reason,
            "gpu_unavailable")
        XCTAssertEqual(
            GPUWorkerTestFault.parse("typed:backend_unavailable")?.runtimeFailure,
            .backendUnavailable)
        XCTAssertEqual(
            GPUWorkerTestFault.parse("typed:driver_unavailable")?.runtimeFailure,
            .gpuUnavailable)
        XCTAssertEqual(
            GPUWorkerTestFault.parse("typed:model_load")?.runtimeFailure,
            .modelLoad)
        XCTAssertEqual(
            GPUWorkerTestFault.parse("typed:context_load")?.runtimeFailure,
            .contextLoad)
        XCTAssertNil(GPUWorkerTestFault.parse("typed:warmup")?.runtimeFailure)
        XCTAssertNil(GPUWorkerTestFault.parse("typed:not-a-real-fault"))
    }
}
#endif

final class GPUWorkerProtocolTests: XCTestCase {
    func testCompositionSnapshotRoundTripsCursorPiecesAndStylesLosslessly() throws {
        var composing = ComposingText()
        composing.insertAtCursorPosition([
                .init(piece: .character("k"), inputStyle: .roman2kana),
                .init(piece: .character("あ"), inputStyle: .direct),
                .init(piece: .key(intention: "x", modifiers: [.shift]), inputStyle: .mapped(id: .defaultAZIK)),
                .init(piece: .compositionSeparator, inputStyle: .mapped(id: .empty)),
            ])
        _ = composing.moveCursorFromCursorPosition(count: -1)

        let snapshot = GPUWorkerCompositionSnapshot(composing)
        let encoded = try JSONEncoder().encode(snapshot)
        let decoded = try JSONDecoder().decode(GPUWorkerCompositionSnapshot.self, from: encoded)

        XCTAssertEqual(decoded, snapshot)
        XCTAssertEqual(try decoded.makeComposingText(), composing)
        XCTAssertTrue(decoded.supportsGPUWorker)
    }

    func testUnsupportedCustomMappingIsRepresentedButNotAdmittedToGPUWorker() {
        let snapshot = GPUWorkerCompositionSnapshot(
            cursor: 1,
            input: [GPUWorkerInputElement(
                piece: .character("a"),
                inputStyle: .mapped(id: .tableName("user-table")))],
            convertTarget: "あ")

        XCTAssertFalse(snapshot.supportsGPUWorker)
        XCTAssertNoThrow(try snapshot.makeComposingText())
    }

    func testMalformedSupportedSnapshotIsRejectedBeforeWorkerRequest() {
        let malformed = GPUWorkerCompositionSnapshot(
            cursor: 1,
            input: [GPUWorkerInputElement(piece: .character("k"), inputStyle: .roman2kana)],
            convertTarget: "壊")
        XCTAssertThrowsError(try malformed.makeComposingText()) { error in
            XCTAssertEqual(error as? GPUWorkerProtocolError, .invalidComposition)
        }
    }

    func testRankRequestRoundTripsWithoutCompositionResetControl() throws {
        let request = GPUWorkerRequest(
            snapshot: GPUWorkerCompositionSnapshot(cursor: 0, input: [], convertTarget: ""),
            leftContext: nil, nBest: 10, inferenceLimit: 1,
            requestID: 7, generation: 3)

        let data = try JSONEncoder().encode(request)
        XCTAssertEqual(try JSONDecoder().decode(GPUWorkerRequest.self, from: data), request)
        let json = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertNil(json["resetComposition"])
    }
}

final class GPUWorkerRerankerTests: XCTestCase {
    private func candidate(_ text: String) -> Candidate {
        Candidate(
            text: text, value: 0, composingCount: .inputCount(1), lastMid: 0,
            data: [DicdataElement(word: text, ruby: text, cid: 0, mid: 0, value: 0)])
    }

    private var classic: ConversionResult {
        let converter = KanaKanjiConverter.withDefaultDictionary()
        var result = converter.requestCandidates(
            ComposingText(), options: .init(N_best: 4,
                                             requireJapanesePrediction: false,
                                             requireEnglishPrediction: false,
                                             keyboardLanguage: .ja_JP,
                                             fullWidthRomanCandidate: false,
                                             learningType: .nothing,
                                             memoryDirectoryURL: FileManager.default.temporaryDirectory,
                                             sharedContainerURL: FileManager.default.temporaryDirectory,
                                             textReplacer: .withDefaultEmojiDictionary(),
                                             specialCandidateProviders: nil,
                                             zenzaiMode: .off,
                                             metadata: .init(versionString: "GPUWorkerTests")))
        result.mainResults = [candidate("A"), candidate("custom"), candidate("C"), candidate("D")]
        result.firstClauseResults = [candidate("first-A"), candidate("first-C")]
        return result
    }

    private func response(main: [String], first: [String] = []) -> GPUWorkerResponse {
        GPUWorkerResponse(requestID: 11, generation: 4,
                          mainResults: main, firstClauseResults: first)
    }

    func testSnapshotEnhancementMayOnlyReorderClassicBodiesAndConsumedReadingRanges() {
        let baseline = [candidate("A"), candidate("B")]
        XCTAssertTrue(ConversionService.isSnapshotEnhancement(
            [baseline[1], baseline[0]], of: baseline))
        XCTAssertFalse(ConversionService.isSnapshotEnhancement(
            [candidate("GPU-only")], of: baseline))
        XCTAssertFalse(ConversionService.isSnapshotEnhancement(
            [baseline[1]], of: baseline))
        XCTAssertFalse(ConversionService.isSnapshotEnhancement(
            [baseline[0], baseline[0]], of: baseline))
        let duplicateBaseline = [baseline[0], baseline[0], baseline[1]]
        XCTAssertTrue(ConversionService.isSnapshotEnhancement(
            [baseline[0], baseline[1], baseline[0]], of: duplicateBaseline))
        let changedRange = Candidate(
            text: "A", value: 0, composingCount: .inputCount(1), lastMid: 0,
            data: [DicdataElement(word: "A", ruby: "AA", cid: 0, mid: 0, value: 0)])
        XCTAssertFalse(ConversionService.isSnapshotEnhancement(
            [changedRange], of: baseline))
    }

    func testWorkerCandidateArraysAreAuthoritativeAndReuseMatchingClassicObjects() {
        let result = GPUWorkerReranker.apply(
            response: response(main: ["C", "A"], first: ["first-C", "first-A"]),
            to: classic, requestID: 11, generation: 4)

        XCTAssertTrue(result.usedWorker)
        XCTAssertNil(result.failure)
        XCTAssertEqual(result.conversion.mainResults.map(\.text), ["C", "A"])
        XCTAssertEqual(result.conversion.firstClauseResults.map(\.text), ["first-C", "first-A"])
    }

    func testGPUOnlyCandidateIsAdoptedAndMadeUnlearnable() {
        let gpuOnly = GPUWorkerCandidate(
            text: "worker-only", value: -2,
            composingCount: .composite(lhs: .inputCount(1), rhs: .surfaceCount(1)),
            lastMid: 7,
            data: [GPUWorkerDicdataElement(
                word: "worker-only", ruby: "ワーカー", lcid: 1, rcid: 2, mid: 7,
                value: -2, metadataRawValue: DicdataElementMetadata.isLearned.rawValue)],
            actions: [.moveCursor(-1)], inputable: false, isLearningTarget: true)
        let result = GPUWorkerReranker.apply(
            response: GPUWorkerResponse(requestID: 11, generation: 4,
                                        mainResults: [gpuOnly]),
            to: classic, requestID: 11, generation: 4)

        XCTAssertTrue(result.usedWorker)
        XCTAssertNil(result.failure)
        let adopted = result.conversion.mainResults
        XCTAssertEqual(adopted.map(\.text), ["worker-only"])
        XCTAssertEqual(adopted.first?.lastMid, 7)
        XCTAssertEqual(adopted.first?.actions, [.moveCursor(-1)])
        XCTAssertFalse(adopted.first?.isLearningTarget ?? true,
                       "GPU-only provenance must never enter learning")
        XCTAssertEqual(
            result.conversion.firstClauseResults.map(\.text),
            classic.firstClauseResults.map(\.text),
            "an omitted worker array must preserve the already-valid classic candidates")
    }

    func testMatchingCandidateReusesClassicLearningMetadata() {
        let classicCandidate = Candidate(
            text: "A", value: -99, composingCount: .inputCount(8), lastMid: 123,
            data: [DicdataElement(
                word: "A", ruby: "エー", cid: 4, mid: 5, value: -9,
                metadata: .isLearned)],
            actions: [.moveCursor(3)], inputable: false, isLearningTarget: true)
        var source = classic
        source.mainResults = [classicCandidate]
        let wire = GPUWorkerCandidate(
            text: "A", value: -1, composingCount: .inputCount(1), lastMid: 0,
            data: [GPUWorkerDicdataElement(
                word: "A", ruby: "A", lcid: 0, rcid: 0, mid: 0, value: 0)],
            inputable: true, isLearningTarget: false)

        let result = GPUWorkerReranker.apply(
            response: GPUWorkerResponse(requestID: 11, generation: 4,
                                        mainResults: [wire]),
            to: source, requestID: 11, generation: 4)
        let adopted = result.conversion.mainResults

        XCTAssertTrue(result.usedWorker)
        XCTAssertNil(result.failure)
        XCTAssertEqual(adopted.first?.value, classicCandidate.value)
        XCTAssertEqual(adopted.first?.composingCount, classicCandidate.composingCount)
        XCTAssertEqual(adopted.first?.lastMid, classicCandidate.lastMid)
        XCTAssertEqual(adopted.first?.data, classicCandidate.data)
        XCTAssertEqual(adopted.first?.actions, classicCandidate.actions)
        XCTAssertEqual(adopted.first?.inputable, classicCandidate.inputable)
        XCTAssertEqual(adopted.first?.isLearningTarget, classicCandidate.isLearningTarget)
    }

    func testWorkerOnlyTextIsAdoptedWhenUsingLegacySeam() {
        let result = GPUWorkerReranker.apply(
            response: response(main: ["C", "worker-only", "A"]),
            to: classic, requestID: 11, generation: 4)

        XCTAssertTrue(result.usedWorker)
        XCTAssertNil(result.failure)
        XCTAssertEqual(result.conversion.mainResults.map(\.text), ["C", "worker-only", "A"])
        XCTAssertFalse(result.conversion.mainResults[1].isLearningTarget)
    }

    func testDuplicateWorkerTextFallsBackToWholeClassicResult() {
        let result = GPUWorkerReranker.apply(
            response: response(main: ["C", "C"]),
            to: classic, requestID: 11, generation: 4)

        XCTAssertFalse(result.usedWorker)
        XCTAssertEqual(result.failure, .duplicateCandidate)
        XCTAssertEqual(result.conversion.firstClauseResults.map(\.text), classic.firstClauseResults.map(\.text))
    }

    func testProtocolMismatchFallsBackWithoutUsingWorkerCandidates() {
        let result = GPUWorkerReranker.apply(
            response: GPUWorkerResponse(requestID: 9, generation: 4, mainResults: ["C"]),
            to: classic, requestID: 11, generation: 4)

        XCTAssertFalse(result.usedWorker)
        XCTAssertEqual(result.failure, .protocolMismatch)
        XCTAssertEqual(result.conversion.mainResults.map(\.text), classic.mainResults.map(\.text))
    }

    func testInvalidCandidateWireFallsBackAsCandidateMismatch() {
        let invalid = GPUWorkerCandidate(
            text: "bad", value: .infinity, composingCount: .inputCount(1), lastMid: 0,
            data: [GPUWorkerDicdataElement(
                word: "bad", ruby: "bad", lcid: 0, rcid: 0, mid: 0, value: 0)])
        let result = GPUWorkerReranker.apply(
            response: GPUWorkerResponse(requestID: 11, generation: 4,
                                        mainResults: [invalid]),
            to: classic, requestID: 11, generation: 4)

        XCTAssertFalse(result.usedWorker)
        XCTAssertEqual(result.failure, .candidateMismatch)
        XCTAssertEqual(result.conversion.mainResults.map(\.text), classic.mainResults.map(\.text))
    }

    func testCandidateCountThatExceedsTheRequestFallsBackEvenWhenTextMatchesClassic() {
        let snapshot = GPUWorkerCompositionSnapshot(
            cursor: 1,
            input: [GPUWorkerInputElement(piece: .character("a"), inputStyle: .direct)],
            convertTarget: "a")
        let invalid = GPUWorkerCandidate(
            text: "A", value: 0, composingCount: .inputCount(2), lastMid: 0,
            data: [GPUWorkerDicdataElement(
                word: "A", ruby: "A", lcid: 0, rcid: 0, mid: 0, value: 0)])

        let result = GPUWorkerReranker.apply(
            response: GPUWorkerResponse(
                requestID: 11, generation: 4, mainResults: [invalid]),
            to: classic, requestID: 11, generation: 4, snapshot: snapshot)

        XCTAssertFalse(result.usedWorker)
        XCTAssertEqual(result.failure, .candidateMismatch)
        XCTAssertEqual(result.conversion.mainResults.map(\.text), classic.mainResults.map(\.text))
    }

    func testCandidateWireRoundTripsAllPublicCandidateMeaning() throws {
        let original = Candidate(
            text: "日本語", value: -3.25,
            composingCount: .composite(lhs: .inputCount(2), rhs: .surfaceCount(3)),
            lastMid: 42,
            data: [DicdataElement(
                word: "日本語", ruby: "ニホンゴ", lcid: 11, rcid: 12, mid: 42,
                value: -8, metadata: [.isLearned, .isFromUserDictionary])],
            actions: [.moveCursor(-2), .moveCursor(4)], inputable: false,
            isLearningTarget: false)
        let wire = GPUWorkerCandidate(original)
        let decoded = try JSONDecoder().decode(
            GPUWorkerCandidate.self, from: JSONEncoder().encode(wire))
        let restored = try decoded.makeCandidate()

        XCTAssertEqual(restored.text, original.text)
        XCTAssertEqual(restored.value, original.value)
        XCTAssertEqual(restored.composingCount, original.composingCount)
        XCTAssertEqual(restored.lastMid, original.lastMid)
        XCTAssertEqual(restored.data, original.data.map {
            DicdataElement(
                word: $0.word, ruby: $0.ruby, lcid: $0.lcid, rcid: $0.rcid,
                mid: $0.mid, value: $0.value(), metadata: $0.metadata)
        })
        XCTAssertEqual(restored.actions, original.actions)
        XCTAssertEqual(restored.inputable, original.inputable)
        XCTAssertEqual(restored.isLearningTarget, original.isLearningTarget)
    }
}

private final class FakeGPUWorkerTransport: GPUWorkerTransport, @unchecked Sendable {
    enum Start {
        case ready
        case failure(GPUWorkerFailure)
    }

    private let lock = NSLock()
    var startResult: Start = .ready
    var replies: [GPUWorkerTransportReply] = []
    private(set) var starts: [UInt64] = []
    private(set) var configurations: [GPUWorkerRuntimeConfiguration?] = []
    private(set) var requests: [GPUWorkerRequest] = []
    private(set) var terminateCount = 0
    var blockRequests = false
    var blockStarts = false
    let requestStarted = DispatchSemaphore(value: 0)
    let releaseRequest = DispatchSemaphore(value: 0)
    let startStarted = DispatchSemaphore(value: 0)
    let releaseStart = DispatchSemaphore(value: 0)

    func start(generation: UInt64) -> GPUWorkerTransportStartResult {
        start(generation: generation, configuration: nil)
    }

    func start(generation: UInt64,
               configuration: GPUWorkerRuntimeConfiguration?) -> GPUWorkerTransportStartResult {
        lock.lock()
        starts.append(generation)
        configurations.append(configuration)
        let result: GPUWorkerTransportStartResult
        switch startResult {
        case .ready:
            result = .ready(backend: "Vulkan", device: "Radeon 890M")
        case .failure(let failure):
            result = .failure(failure)
        }
        lock.unlock()
        if blockStarts {
            startStarted.signal()
            releaseStart.wait()
        }
        return result
    }

    func request(_ request: GPUWorkerRequest, timeout: TimeInterval) -> GPUWorkerTransportReply {
        lock.lock(); defer { lock.unlock() }
        requests.append(request)
        if blockRequests {
            requestStarted.signal()
            releaseRequest.wait()
        }
        return replies.isEmpty ? .timeout : replies.removeFirst()
    }

    func terminate() {
        lock.lock(); defer { lock.unlock() }
        terminateCount += 1
    }
}

private final class CountingRuntimeClient: ZenzaiRuntimeClient, @unchecked Sendable {
    private let lock = NSLock()
    private(set) var configureCount = 0
    private var current = ZenzaiRuntimeStatus.unconfigured

    func configure(trustedRuntimeDirectory: URL, explicitRetry: Bool) -> ZenzaiRuntimeStatus {
        lock.lock(); configureCount += 1; lock.unlock()
        return current
    }

    func status() -> ZenzaiRuntimeStatus {
        lock.lock(); defer { lock.unlock() }
        return current
    }
}

final class GPUWorkerSupervisorTests: XCTestCase {
    private func classic() -> ConversionResult {
        let converter = KanaKanjiConverter.withDefaultDictionary()
        var result = converter.requestCandidates(
            ComposingText(), options: .init(N_best: 3,
                                             requireJapanesePrediction: false,
                                             requireEnglishPrediction: false,
                                             keyboardLanguage: .ja_JP,
                                             fullWidthRomanCandidate: false,
                                             learningType: .nothing,
                                             memoryDirectoryURL: FileManager.default.temporaryDirectory,
                                             sharedContainerURL: FileManager.default.temporaryDirectory,
                                             textReplacer: .withDefaultEmojiDictionary(),
                                             specialCandidateProviders: nil,
                                             zenzaiMode: .off,
                                             metadata: .init(versionString: "GPUWorkerTests")))
        func c(_ text: String) -> Candidate {
            Candidate(text: text, value: 0, composingCount: .inputCount(1), lastMid: 0, data: [])
        }
        result.mainResults = [c("A"), c("B"), c("C")]
        result.firstClauseResults = [c("A")]
        return result
    }

    private func snapshot() -> GPUWorkerCompositionSnapshot {
        GPUWorkerCompositionSnapshot(
            cursor: 1,
            input: [GPUWorkerInputElement(piece: .character("あ"), inputStyle: .direct)],
            convertTarget: "あ")
    }

    func testTimeoutTerminatesOnceLatchesAndSameRequestReturnsClassic() {
        let transport = FakeGPUWorkerTransport()
        transport.replies = [.timeout]
        let supervisor = GPUWorkerSupervisor(transport: transport)

        let first = supervisor.rerank(classic: classic(), snapshot: snapshot(), leftContext: nil,
                                      nBest: 1, inferenceLimit: 1, deadline: 1.2)
        XCTAssertFalse(first.usedWorker)
        XCTAssertEqual(first.failure, .timeout)
        XCTAssertEqual(transport.starts, [1])
        XCTAssertEqual(transport.terminateCount, 1)
        XCTAssertEqual(supervisor.snapshot.state, .classic)
        XCTAssertEqual(supervisor.snapshot.reason, GPUWorkerQuarantineReason.timeout.rawValue)

        let second = supervisor.rerank(classic: classic(), snapshot: snapshot(), leftContext: nil,
                                       nBest: 1, inferenceLimit: 1, deadline: 1.2)
        XCTAssertFalse(second.usedWorker)
        XCTAssertEqual(transport.starts, [1], "next request must not respawn after quarantine")
        XCTAssertEqual(transport.terminateCount, 1)
    }

    func testCrashAndProtocolMismatchLatchWorkerWithoutChangingClassic() {
        let transport = FakeGPUWorkerTransport()
        transport.replies = [.crash]
        let supervisor = GPUWorkerSupervisor(transport: transport)
        _ = supervisor.rerank(classic: classic(), snapshot: snapshot(), leftContext: nil,
                              nBest: 10, inferenceLimit: 1, deadline: 1.2)
        XCTAssertEqual(supervisor.snapshot.reason, GPUWorkerQuarantineReason.crash.rawValue)
        XCTAssertEqual(transport.terminateCount, 1)

        supervisor.explicitRetry()
        XCTAssertEqual(supervisor.snapshot.state, .preparing)
        XCTAssertEqual(transport.starts, [1], "retry arms one generation and does not eagerly duplicate spawn")
    }

    func testProtocolMismatchAndGenerationMismatchQuarantineWithoutRespawn() {
        for reply in [
            GPUWorkerTransportReply.protocolMismatch,
            .response(GPUWorkerResponse(
                requestID: 1, generation: 2, mainResults: ["C"], firstClauseResults: ["A"]))
        ] {
            let transport = FakeGPUWorkerTransport()
            transport.replies = [reply]
            let supervisor = GPUWorkerSupervisor(transport: transport)

            let result = supervisor.rerank(
                classic: classic(), snapshot: snapshot(), leftContext: nil,
                nBest: 10, inferenceLimit: 1, deadline: 1.2)
            XCTAssertFalse(result.usedWorker)
            XCTAssertEqual(result.failure, .workerProtocol)
            XCTAssertEqual(supervisor.snapshot.reason, GPUWorkerQuarantineReason.workerProtocol.rawValue)
            XCTAssertEqual(transport.terminateCount, 1)

            _ = supervisor.rerank(
                classic: classic(), snapshot: snapshot(), leftContext: nil,
                nBest: 10, inferenceLimit: 1, deadline: 1.2)
            XCTAssertEqual(transport.starts, [1], "a quarantined generation must not respawn")
        }
    }

    func testWorkerExitQuarantinesAndTypedStartFailureIsSanitized() {
        let exitTransport = FakeGPUWorkerTransport()
        exitTransport.replies = [.exit]
        let exitSupervisor = GPUWorkerSupervisor(transport: exitTransport)
        let exitResult = exitSupervisor.rerank(
            classic: classic(), snapshot: snapshot(), leftContext: nil,
            nBest: 10, inferenceLimit: 1, deadline: 1.2)
        XCTAssertEqual(exitResult.failure, .workerExit)
        XCTAssertEqual(exitSupervisor.snapshot.reason, GPUWorkerQuarantineReason.workerExit.rawValue)
        XCTAssertEqual(exitTransport.terminateCount, 1)

        let nativeTransport = FakeGPUWorkerTransport()
        nativeTransport.startResult = .failure(.gpuUnavailable)
        let nativeSupervisor = GPUWorkerSupervisor(transport: nativeTransport)
        nativeSupervisor.startWarmUp()
        let deadline = Date().addingTimeInterval(1)
        while nativeSupervisor.snapshot.state != .classic && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.005)
        }
        XCTAssertEqual(nativeSupervisor.snapshot.reason,
                       GPUWorkerQuarantineReason.gpuUnavailable.rawValue)
        XCTAssertNil(nativeSupervisor.snapshot.backend)
        XCTAssertNil(nativeSupervisor.snapshot.device)
    }

    func testFailedRetryCanBeExplicitlyRetriedOnceAfterLatch() {
        let transport = FakeGPUWorkerTransport()
        transport.startResult = .failure(.timeout)
        let supervisor = GPUWorkerSupervisor(transport: transport)
        supervisor.startWarmUp()
        let firstDeadline = Date().addingTimeInterval(1)
        while supervisor.snapshot.state != .classic && Date() < firstDeadline {
            Thread.sleep(forTimeInterval: 0.005)
        }
        XCTAssertEqual(supervisor.snapshot.reason, GPUWorkerQuarantineReason.timeout.rawValue)

        transport.startResult = .ready
        supervisor.explicitRetry()
        supervisor.explicitRetry()
        supervisor.startWarmUp()
        let secondDeadline = Date().addingTimeInterval(1)
        while supervisor.snapshot.state != .gpuActive && Date() < secondDeadline {
            Thread.sleep(forTimeInterval: 0.005)
        }
        XCTAssertEqual(supervisor.snapshot.state, .gpuActive)
        XCTAssertEqual(transport.starts, [1, 2], "one explicit retry opens exactly one generation")
    }

    func testEmptyInputNeverStartsWorker() {
        let transport = FakeGPUWorkerTransport()
        let supervisor = GPUWorkerSupervisor(transport: transport)
        let result = supervisor.rerank(
            classic: classic(),
            snapshot: GPUWorkerCompositionSnapshot(cursor: 0, input: [], convertTarget: ""),
            leftContext: nil, nBest: 10, inferenceLimit: 1)
        XCTAssertFalse(result.usedWorker)
        XCTAssertNil(result.failure)
        XCTAssertTrue(transport.starts.isEmpty)
    }

    func testCompletedCompositionDoesNotResetTheNextWorkerRank() {
        let transport = FakeGPUWorkerTransport()
        transport.replies = [
            .response(GPUWorkerResponse(
                requestID: 1, generation: 1, mainResults: ["A"], firstClauseResults: ["A"])),
            .response(GPUWorkerResponse(
                requestID: 2, generation: 1, mainResults: ["A"], firstClauseResults: ["A"])),
        ]
        let supervisor = GPUWorkerSupervisor(transport: transport)

        _ = supervisor.rerank(
            classic: classic(), snapshot: snapshot(), leftContext: nil,
            nBest: 10, inferenceLimit: 1)
        _ = supervisor.rerank(
            classic: classic(), snapshot: snapshot(), leftContext: nil,
            nBest: 10, inferenceLimit: 1)

        XCTAssertEqual(transport.requests.count, 2)
    }

    func testExplicitRetryStartsSingleNewGenerationAndDedupe() {
        let transport = FakeGPUWorkerTransport()
        transport.replies = [.timeout]
        let supervisor = GPUWorkerSupervisor(transport: transport)
        _ = supervisor.rerank(classic: classic(), snapshot: snapshot(), leftContext: nil,
                              nBest: 10, inferenceLimit: 1, deadline: 1.2)
        supervisor.explicitRetry()
        supervisor.explicitRetry()
        transport.replies = [.response(GPUWorkerResponse(
            requestID: 2, generation: 2, mainResults: ["C", "A"], firstClauseResults: ["A"]))]
        let result = supervisor.rerank(classic: classic(), snapshot: snapshot(), leftContext: nil,
                                       nBest: 10, inferenceLimit: 1, deadline: 1.2)
        XCTAssertTrue(result.usedWorker)
        XCTAssertEqual(transport.starts, [1, 2])
        XCTAssertEqual(transport.requests.last?.generation, 2)
    }

    func testOrdinaryReloadDoesNotClearQuarantineAndUnsupportedMappingDoesNotSpawn() {
        let transport = FakeGPUWorkerTransport()
        transport.replies = [.timeout]
        let supervisor = GPUWorkerSupervisor(transport: transport)
        _ = supervisor.rerank(classic: classic(), snapshot: snapshot(), leftContext: nil,
                              nBest: 10, inferenceLimit: 1, deadline: 1.2)
        supervisor.ordinaryReload()
        let unsupported = GPUWorkerCompositionSnapshot(
            cursor: 1, input: [GPUWorkerInputElement(
                piece: .character("a"), inputStyle: .mapped(id: .tableName("custom")))],
            convertTarget: "あ")
        let result = supervisor.rerank(classic: classic(), snapshot: unsupported, leftContext: nil,
                                       nBest: 10, inferenceLimit: 1, deadline: 1.2)
        XCTAssertFalse(result.usedWorker)
        XCTAssertEqual(transport.starts, [1])
        XCTAssertEqual(supervisor.snapshot.reason, GPUWorkerQuarantineReason.timeout.rawValue)
    }

    func testSnapshotDoesNotWaitForBlockedWorkerRequest() {
        let transport = FakeGPUWorkerTransport()
        transport.blockRequests = true
        transport.replies = [.timeout]
        let supervisor = GPUWorkerSupervisor(transport: transport)
        let done = DispatchSemaphore(value: 0)
        let classic = classic()
        let snapshot = snapshot()
        Thread.detachNewThread {
            _ = supervisor.rerank(classic: classic, snapshot: snapshot, leftContext: nil,
                                  nBest: 10, inferenceLimit: 1, deadline: 1.2)
            done.signal()
        }
        XCTAssertEqual(transport.requestStarted.wait(timeout: .now() + 1), .success)
        let start = DispatchTime.now().uptimeNanoseconds
        _ = supervisor.snapshot
        let elapsed = Double(DispatchTime.now().uptimeNanoseconds - start) / 1_000_000
        XCTAssertLessThan(elapsed, 100, "status snapshot must not wait for native request")
        transport.releaseRequest.signal()
        XCTAssertEqual(done.wait(timeout: .now() + 1), .success)
    }

    func testModelRuntimeChangeReturnsWhileWorkerStartIsInFlight() {
        let transport = FakeGPUWorkerTransport()
        transport.blockStarts = true
        let supervisor = GPUWorkerSupervisor(transport: transport)
        supervisor.startWarmUp()
        XCTAssertEqual(transport.startStarted.wait(timeout: .now() + 1), .success)

        let changed = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            supervisor.modelOrRuntimeChanged()
            changed.signal()
        }
        XCTAssertEqual(
            changed.wait(timeout: .now() + 0.1), .success,
            "model reload must publish the new generation without waiting for worker start")

        transport.releaseStart.signal()
        let deadline = Date().addingTimeInterval(1)
        while transport.terminateCount == 0 && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.005)
        }
        XCTAssertEqual(transport.terminateCount, 1)
    }

    func testDisableReturnsWhileWorkerStartIsInFlight() {
        let transport = FakeGPUWorkerTransport()
        transport.blockStarts = true
        let supervisor = GPUWorkerSupervisor(transport: transport)
        supervisor.startWarmUp()
        XCTAssertEqual(transport.startStarted.wait(timeout: .now() + 1), .success)

        let disabled = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            supervisor.disable()
            disabled.signal()
        }
        XCTAssertEqual(
            disabled.wait(timeout: .now() + 0.1), .success,
            "disable must publish disabled state without waiting for worker start")
        XCTAssertEqual(supervisor.snapshot.state, .disabled)

        transport.releaseStart.signal()
        let deadline = Date().addingTimeInterval(1)
        while transport.terminateCount == 0 && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.005)
        }
        XCTAssertEqual(transport.terminateCount, 1)
    }

    func testDisableReturnsWhileWorkerRequestIsInFlight() {
        let transport = FakeGPUWorkerTransport()
        transport.blockRequests = true
        transport.replies = [.timeout]
        let supervisor = GPUWorkerSupervisor(transport: transport)
        let requestDone = DispatchSemaphore(value: 0)
        let classicResult = classic()
        let composition = snapshot()
        Thread.detachNewThread {
            _ = supervisor.rerank(
                classic: classicResult, snapshot: composition, leftContext: nil,
                nBest: 10, inferenceLimit: 1, deadline: 1.2)
            requestDone.signal()
        }
        XCTAssertEqual(transport.requestStarted.wait(timeout: .now() + 1), .success)

        let disabled = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            supervisor.disable()
            disabled.signal()
        }
        XCTAssertEqual(
            disabled.wait(timeout: .now() + 0.1), .success,
            "disable must not wait for a request's TIP-sized deadline")
        XCTAssertEqual(supervisor.snapshot.state, .disabled)

        transport.releaseRequest.signal()
        XCTAssertEqual(requestDone.wait(timeout: .now() + 1), .success)
        let deadline = Date().addingTimeInterval(1)
        while transport.terminateCount == 0 && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.005)
        }
        XCTAssertEqual(transport.terminateCount, 1)
    }

    func testStaleWarmUpCannotLeaveAnOldChildAfterModelChange() {
        let runtimeDirectory = FileManager.default.temporaryDirectory
            .appendingPathComponent("gpu-worker-stale-runtime-\(UUID().uuidString)")
        let modelAURL = runtimeDirectory.appendingPathComponent("model-a.gguf")
        let modelBURL = runtimeDirectory.appendingPathComponent("model-b.gguf")
        try? FileManager.default.createDirectory(
            at: runtimeDirectory, withIntermediateDirectories: true)
        try? Data("a".utf8).write(to: modelAURL)
        try? Data("b".utf8).write(to: modelBURL)
        defer { try? FileManager.default.removeItem(at: runtimeDirectory) }
        let modelA = GPUWorkerRuntimeConfiguration(
            modelURL: modelAURL,
            runtimeDirectory: runtimeDirectory,
            inferenceLimit: 1)!
        let modelB = GPUWorkerRuntimeConfiguration(
            modelURL: modelBURL,
            runtimeDirectory: runtimeDirectory,
            inferenceLimit: 2)!
        let transport = FakeGPUWorkerTransport()
        transport.blockStarts = true
        let supervisor = GPUWorkerSupervisor(
            transport: transport, runtimeConfiguration: modelA, allowsLazyStart: false)

        supervisor.startWarmUp()
        XCTAssertEqual(transport.startStarted.wait(timeout: .now() + 1), .success)
        let changed = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            supervisor.modelOrRuntimeChanged(configuration: modelB)
            changed.signal()
        }
        XCTAssertEqual(changed.wait(timeout: .now() + 0.1), .success)
        transport.releaseStart.signal()

        let firstStartDeadline = Date().addingTimeInterval(1)
        while transport.terminateCount == 0 && Date() < firstStartDeadline {
            Thread.sleep(forTimeInterval: 0.005)
        }
        XCTAssertEqual(transport.terminateCount, 1)

        transport.blockStarts = false
        supervisor.startWarmUp()
        let secondStartDeadline = Date().addingTimeInterval(1)
        while supervisor.snapshot.state != .gpuActive && Date() < secondStartDeadline {
            Thread.sleep(forTimeInterval: 0.005)
        }
        XCTAssertEqual(transport.starts, [1, 2])
        XCTAssertEqual(transport.configurations[1], modelB)
        XCTAssertEqual(transport.terminateCount, 1,
                       "cleanup for the stale generation must not terminate the replacement child")
    }

    func testMainRoleKeepsNativeRuntimeOffAndBuildsTenCandidateWorkerPool() throws {
        let model = FileManager.default.temporaryDirectory
            .appendingPathComponent("gpu-worker-main-model-\(UUID().uuidString).gguf")
        try Data("model".utf8).write(to: model)
        defer { try? FileManager.default.removeItem(at: model) }
        let runtimeDirectory = FileManager.default.temporaryDirectory
        let config = ZenzaiConfig(
            weightURL: model, inferenceLimit: 3, runtimeDirectory: runtimeDirectory)
        let transport = FakeGPUWorkerTransport()
        transport.replies = [.timeout]
        let runtime = CountingRuntimeClient()
        let supervisor = GPUWorkerSupervisor(
            transport: transport,
            runtimeConfiguration: GPUWorkerRuntimeConfiguration(config: config),
            allowsLazyStart: false)
        let service = ConversionService(
            config: config,
            runtimeClient: runtime,
            processRole: .mainClassicOnly,
            gpuWorkerSupervisor: supervisor)

        service.startWarmUp()
        let readyDeadline = Date().addingTimeInterval(1)
        while supervisor.snapshot.state != .gpuActive && Date() < readyDeadline {
            Thread.sleep(forTimeInterval: 0.005)
        }
        let sid = service.startSession()
        _ = service.insert(session: sid, text: "nihongo")
        XCTAssertNotNil(service.convert(session: sid))
        XCTAssertEqual(runtime.configureCount, 0, "main must never configure native Zenzai")
        XCTAssertEqual(transport.requests.first?.nBest, 10)
        XCTAssertEqual(
            transport.configurations.first??.modelURL,
            model.resolvingSymlinksInPath().standardizedFileURL)
    }

    func testMainEndSessionDoesNotResetTheNextWorkerRank() throws {
        let model = FileManager.default.temporaryDirectory
            .appendingPathComponent("gpu-worker-session-model-\(UUID().uuidString).gguf")
        try Data("model".utf8).write(to: model)
        defer { try? FileManager.default.removeItem(at: model) }
        let config = ZenzaiConfig(
            weightURL: model, inferenceLimit: 1,
            runtimeDirectory: FileManager.default.temporaryDirectory)
        let transport = FakeGPUWorkerTransport()
        transport.replies = [
            .response(GPUWorkerResponse(requestID: 1, generation: 1)),
            .response(GPUWorkerResponse(requestID: 2, generation: 1)),
        ]
        let supervisor = GPUWorkerSupervisor(
            transport: transport,
            runtimeConfiguration: GPUWorkerRuntimeConfiguration(config: config),
            allowsLazyStart: false)
        let service = ConversionService(
            config: config, runtimeClient: CountingRuntimeClient(),
            processRole: .mainClassicOnly, gpuWorkerSupervisor: supervisor)
        service.startWarmUp()
        let readyDeadline = Date().addingTimeInterval(1)
        while supervisor.snapshot.state != .gpuActive && Date() < readyDeadline {
            Thread.sleep(forTimeInterval: 0.005)
        }

        let first = service.startSession()
        _ = service.insert(session: first, text: "nihongo")
        _ = service.convert(session: first)
        service.endSession(session: first)
        let second = service.startSession()
        _ = service.insert(session: second, text: "nihongo")
        _ = service.convert(session: second)

        XCTAssertEqual(transport.requests.count, 2)
    }

    func testMainModelReloadKeepsNativeRuntimeOffWhileStartingNewWorkerConfig() throws {
        let modelA = FileManager.default.temporaryDirectory
            .appendingPathComponent("gpu-worker-reload-a-\(UUID().uuidString).gguf")
        let modelB = FileManager.default.temporaryDirectory
            .appendingPathComponent("gpu-worker-reload-b-\(UUID().uuidString).gguf")
        try Data("a".utf8).write(to: modelA)
        try Data("b".utf8).write(to: modelB)
        defer {
            try? FileManager.default.removeItem(at: modelA)
            try? FileManager.default.removeItem(at: modelB)
        }
        let runtimeDirectory = FileManager.default.temporaryDirectory
        let config = ZenzaiConfig(
            weightURL: modelA, inferenceLimit: 1, runtimeDirectory: runtimeDirectory)
        let transport = FakeGPUWorkerTransport()
        let supervisor = GPUWorkerSupervisor(
            transport: transport,
            runtimeConfiguration: GPUWorkerRuntimeConfiguration(config: config),
            allowsLazyStart: false)
        let runtime = CountingRuntimeClient()
        let service = ConversionService(
            config: config, runtimeClient: runtime, processRole: .mainClassicOnly,
            gpuWorkerSupervisor: supervisor)

        XCTAssertTrue(service.reload(
            overrides: [
                "NOSPACEKEY_ZENZAI": "on",
                "NOSPACEKEY_ZENZAI_WEIGHT": modelB.path,
                "NOSPACEKEY_ZENZAI_RUNTIME_DIR": runtimeDirectory.path,
                "NOSPACEKEY_ZENZAI_INFERENCE_LIMIT": "2"
            ], cpuMeetsLlamaBaseline: true))
        let deadline = Date().addingTimeInterval(1)
        while transport.starts.isEmpty && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.005)
        }
        XCTAssertEqual(runtime.configureCount, 0, "reload must never reopen Main's native Zenzai")
        XCTAssertEqual(transport.configurations.first??.modelURL,
                       modelB.resolvingSymlinksInPath().standardizedFileURL)
        XCTAssertEqual(transport.configurations.first??.inferenceLimit, 2)
    }

    func testModelRuntimeChangeTerminatesReadyWorkerAndPassesNewGenerationConfig() throws {
        let modelA = FileManager.default.temporaryDirectory
            .appendingPathComponent("gpu-worker-model-a-\(UUID().uuidString).gguf")
        let modelB = FileManager.default.temporaryDirectory
            .appendingPathComponent("gpu-worker-model-b-\(UUID().uuidString).gguf")
        try Data("a".utf8).write(to: modelA)
        try Data("b".utf8).write(to: modelB)
        defer {
            try? FileManager.default.removeItem(at: modelA)
            try? FileManager.default.removeItem(at: modelB)
        }
        let configA = GPUWorkerRuntimeConfiguration(
            modelURL: modelA, runtimeDirectory: FileManager.default.temporaryDirectory,
            inferenceLimit: 1)!
        let configB = GPUWorkerRuntimeConfiguration(
            modelURL: modelB, runtimeDirectory: FileManager.default.temporaryDirectory,
            inferenceLimit: 2)!
        let transport = FakeGPUWorkerTransport()
        let supervisor = GPUWorkerSupervisor(
            transport: transport, runtimeConfiguration: configA, allowsLazyStart: false)
        supervisor.startWarmUp()
        let deadline = Date().addingTimeInterval(1)
        while supervisor.snapshot.state != .gpuActive && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.005)
        }
        supervisor.modelOrRuntimeChanged(configuration: configB)
        XCTAssertEqual(transport.terminateCount, 1)
        supervisor.startWarmUp()
        let secondDeadline = Date().addingTimeInterval(1)
        while transport.starts.count < 2 && Date() < secondDeadline {
            Thread.sleep(forTimeInterval: 0.005)
        }
        XCTAssertEqual(transport.starts, [1, 2])
        XCTAssertEqual(transport.configurations[1], configB)
    }
}
