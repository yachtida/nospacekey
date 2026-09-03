import Foundation
import XCTest
import KanaKanjiConverterModuleWithDefaultDictionary
@testable import NospacekeyEngineCore

#if os(Windows)
import WinSDK

/// Exercises the production named-pipe transport against the same executable's
/// worker mode. The test intentionally delays rank after the handshake, so the
/// live worker budget is the only deadline that can decide the request.
final class GPUWorkerProcessTransportTests: XCTestCase {
    func testRankTimeoutReturnsClassicWithinLiveDeadlineAndLatchesWorker() throws {
        let packageRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
        let host = try XCTUnwrap(Self.findHostExecutable(packageRoot: packageRoot))
        let model = packageRoot.appendingPathComponent("models/ggml-model-Q5_K_M.gguf")
        let runtime = packageRoot.appendingPathComponent("vendor/llama/vulkan")
        let configuration = try XCTUnwrap(
            GPUWorkerRuntimeConfiguration(modelURL: model, runtimeDirectory: runtime,
                                          inferenceLimit: 1))
        let transport = NativeGPUWorkerTransport(executableURL: host)
        let supervisor = GPUWorkerSupervisor(
            transport: transport, runtimeConfiguration: configuration, allowsLazyStart: false)

        let oldFault = Self.environmentValue("NOSPACEKEY_GPU_WORKER_TEST_FAULT")
        Self.setEnvironmentValue("NOSPACEKEY_GPU_WORKER_TEST_FAULT", "delay-rank:1000")
        defer {
            Self.setEnvironmentValue("NOSPACEKEY_GPU_WORKER_TEST_FAULT", oldFault)
            transport.terminate()
        }

        supervisor.startWarmUp()
        let warmupDeadline = Date().addingTimeInterval(20)
        while supervisor.snapshot.state == .preparing && Date() < warmupDeadline {
            Thread.sleep(forTimeInterval: 0.01)
        }
        if supervisor.snapshot.state == .classic {
            throw XCTSkip("GPU worker warm-up unavailable: \(supervisor.snapshot.reason ?? "unknown")")
        }
        XCTAssertEqual(supervisor.snapshot.state, .gpuActive)

#if DEBUG
        let oldCancelHook = NativeGPUWorkerTransport.cancelIoExForTesting
        // Exercise the failure branch explicitly: CancelIoEx can fail after a
        // timeout, but the caller must still return within the live budget.
        NativeGPUWorkerTransport.cancelIoExForTesting = { _, _ in
            (cancelled: false, error: DWORD(ERROR_ACCESS_DENIED))
        }
        defer { NativeGPUWorkerTransport.cancelIoExForTesting = oldCancelHook }
#endif

        let classic = Self.classicConversion()
        let start = DispatchTime.now().uptimeNanoseconds
        let decision = supervisor.rerank(
            classic: classic, snapshot: Self.snapshot(), leftContext: nil,
            nBest: 10, inferenceLimit: 1, deadline: GPUWorkerDeadlineTier.live.workerBudget)
        let elapsedMilliseconds = Double(DispatchTime.now().uptimeNanoseconds - start) / 1_000_000

        XCTAssertFalse(decision.usedWorker)
        XCTAssertEqual(decision.failure, .timeout)
        XCTAssertEqual(decision.conversion.mainResults.map(\.text), ["classic"])
        XCTAssertEqual(supervisor.snapshot.state, .classic)
        XCTAssertEqual(supervisor.snapshot.reason, GPUWorkerQuarantineReason.timeout.rawValue)
        XCTAssertLessThan(elapsedMilliseconds, 400,
                          "live timeout plus worker cleanup must fit the external 400 ms deadline")

        let latchedStart = DispatchTime.now().uptimeNanoseconds
        let latched = supervisor.rerank(
            classic: classic, snapshot: Self.snapshot(), leftContext: nil,
            nBest: 10, inferenceLimit: 1, deadline: GPUWorkerDeadlineTier.live.workerBudget)
        let latchedMilliseconds = Double(DispatchTime.now().uptimeNanoseconds - latchedStart) / 1_000_000
        XCTAssertFalse(latched.usedWorker)
        XCTAssertEqual(latched.failure, .timeout)
        XCTAssertLessThan(latchedMilliseconds, 50)
    }

    private static func findHostExecutable(packageRoot: URL) -> URL? {
        let candidates = [
            packageRoot.appendingPathComponent(".build/x86_64-unknown-windows-msvc/debug/NospacekeyEngineHost.exe"),
            packageRoot.appendingPathComponent(".build/debug/NospacekeyEngineHost.exe"),
        ]
        return candidates.first { FileManager.default.isExecutableFile(atPath: $0.path) }
    }

    private static func environmentValue(_ key: String) -> String? {
        key.withCString(encodedAs: UTF16.self) { keyPointer -> String? in
            let length = GetEnvironmentVariableW(keyPointer, nil, 0)
            guard length > 0 else { return nil }
            var buffer = [WCHAR](repeating: 0, count: Int(length) + 1)
            let copied = GetEnvironmentVariableW(keyPointer, &buffer, DWORD(buffer.count))
            guard copied > 0 else { return nil }
            return String(decoding: buffer.prefix(Int(copied)), as: UTF16.self)
        }
    }

    private static func setEnvironmentValue(_ key: String, _ value: String?) {
        key.withCString(encodedAs: UTF16.self) { keyPointer in
            if let value {
                value.withCString(encodedAs: UTF16.self) { valuePointer in
                    _ = SetEnvironmentVariableW(keyPointer, valuePointer)
                }
            } else {
                _ = SetEnvironmentVariableW(keyPointer, nil)
            }
        }
    }

    private static func snapshot() -> GPUWorkerCompositionSnapshot {
        GPUWorkerCompositionSnapshot(
            cursor: 1,
            input: [GPUWorkerInputElement(piece: .character("あ"), inputStyle: .direct)],
            convertTarget: "あ")
    }

    private static func classicConversion() -> ConversionResult {
        let candidate = Candidate(
            text: "classic", value: 0, composingCount: .inputCount(1), lastMid: 0, data: [])
        var result = KanaKanjiConverter.withDefaultDictionary().requestCandidates(
            ComposingText(), options: .init(
                N_best: 1, requireJapanesePrediction: false, requireEnglishPrediction: false,
                keyboardLanguage: .ja_JP, fullWidthRomanCandidate: false,
                learningType: .nothing, memoryDirectoryURL: FileManager.default.temporaryDirectory,
                sharedContainerURL: FileManager.default.temporaryDirectory,
                textReplacer: .withDefaultEmojiDictionary(), specialCandidateProviders: nil,
                zenzaiMode: .off, metadata: .init(versionString: "GPUWorkerProcessTransportTests")))
        result.mainResults = [candidate]
        result.firstClauseResults = [candidate]
        return result
    }
}
#endif
