import Foundation
import XCTest
@testable import NospacekeyEngineCore
#if os(Windows)
@testable import NospacekeyLlamaRuntimeAdapter
#endif

private final class StubZenzaiRuntimeClient: ZenzaiRuntimeClient, @unchecked Sendable {
    private let lock = NSLock()
    private var nextStatuses: [ZenzaiRuntimeStatus]
    private var configuredStatus: ZenzaiRuntimeStatus
    private(set) var configureDirectories: [URL] = []
    private(set) var configureRetries: [Bool] = []
    private(set) var statusCallCount = 0

    init(configure: ZenzaiRuntimeStatus, statuses: [ZenzaiRuntimeStatus] = []) {
        self.configuredStatus = configure
        self.nextStatuses = statuses
    }

    func configure(trustedRuntimeDirectory: URL, explicitRetry: Bool) -> ZenzaiRuntimeStatus {
        lock.lock()
        defer { lock.unlock() }
        configureDirectories.append(trustedRuntimeDirectory)
        configureRetries.append(explicitRetry)
        return configuredStatus
    }

    func status() -> ZenzaiRuntimeStatus {
        lock.lock()
        defer { lock.unlock() }
        statusCallCount += 1
        if !nextStatuses.isEmpty { return nextStatuses.removeFirst() }
        return configuredStatus
    }

    func append(_ statuses: ZenzaiRuntimeStatus...) {
        lock.lock()
        nextStatuses.append(contentsOf: statuses)
        lock.unlock()
    }

    func setConfiguredStatus(_ status: ZenzaiRuntimeStatus) {
        lock.lock()
        configuredStatus = status
        lock.unlock()
    }

    func snapshot() -> (configureCount: Int, statusCount: Int, retries: [Bool]) {
        lock.lock()
        defer { lock.unlock() }
        return (configureDirectories.count, statusCallCount, configureRetries)
    }

    func configuredRuntimeDirectories() -> [URL] {
        lock.lock()
        defer { lock.unlock() }
        return configureDirectories
    }
}

final class ZenzaiRuntimeStateTests: XCTestCase {
    private let runtimeDirectory = URL(fileURLWithPath: "C:/trusted/zenzai-runtime")
    private static let existingModelURL: URL = {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(
            "zenzai-runtime-state-fixture.gguf")
        if !FileManager.default.fileExists(atPath: url.path) {
            try? Data("fixture".utf8).write(to: url)
        }
        return url
    }()

    private func config(weight: URL? = ZenzaiRuntimeStateTests.existingModelURL,
                        disabledReason: ZenzaiDisabledReason? = nil) -> ZenzaiConfig {
        ZenzaiConfig(weightURL: weight, inferenceLimit: 1,
                     runtimeDirectory: runtimeDirectory, disabledReason: disabledReason)
    }

    private func waitForState(
        _ service: ConversionService,
        timeout: TimeInterval = 2,
        file: StaticString = #filePath,
        line: UInt = #line,
        predicate: (ZenzaiRuntimeState) -> Bool
    ) {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if predicate(service.zenzaiRuntimeState) { return }
            Thread.sleep(forTimeInterval: 0.01)
        }
        XCTFail("runtime state did not reach expected value: \(service.zenzaiRuntimeState)",
                file: file, line: line)
    }

    func testDisabledConfigDoesNotProbeAndIsClassicReadySynchronously() {
        let client = StubZenzaiRuntimeClient(configure: .unconfigured)
        let service = ConversionService(
            config: config(weight: nil, disabledReason: .userDisabled), runtimeClient: client)

        service.startWarmUp()

        XCTAssertEqual(service.zenzaiRuntimeState, .classic(reason: .userDisabled))
        XCTAssertTrue(service.zenzaiReady)
        XCTAssertEqual(client.snapshot().configureCount, 0)
        XCTAssertEqual(client.snapshot().statusCount, 0)
    }

    func testMissingModelDoesNotProbeAndIsClassicReadySynchronously() {
        let missing = FileManager.default.temporaryDirectory.appendingPathComponent(
            "zenzai-runtime-state-missing-\(UUID().uuidString).gguf")
        let client = StubZenzaiRuntimeClient(configure: .unconfigured)
        let service = ConversionService(config: config(weight: missing), runtimeClient: client)

        service.startWarmUp()

        XCTAssertEqual(service.zenzaiRuntimeState, .classic(reason: .modelMissing))
        XCTAssertTrue(service.zenzaiReady)
        XCTAssertEqual(client.snapshot().configureCount, 0)
        XCTAssertEqual(client.snapshot().statusCount, 0)
    }

    func testModelDirectoryDoesNotProbeAndIsClassicReadySynchronously() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(
            "zenzai-runtime-state-model-directory-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let client = StubZenzaiRuntimeClient(configure: .unconfigured)
        let service = ConversionService(config: config(weight: directory), runtimeClient: client)

        service.startWarmUp()

        XCTAssertEqual(service.zenzaiRuntimeState, .classic(reason: .modelMissing))
        XCTAssertTrue(service.zenzaiReady)
        XCTAssertEqual(client.snapshot().configureCount, 0)
        XCTAssertEqual(client.snapshot().statusCount, 0)
    }

    func testConfigureFailureLatchesClassicAndDoesNotRetryOnRequests() {
        let client = StubZenzaiRuntimeClient(
            configure: ZenzaiRuntimeStatus(state: .failed, failure: .backendUnavailable))
        let service = ConversionService(config: config(), runtimeClient: client)
        service.startWarmUp()

        XCTAssertEqual(service.zenzaiRuntimeState, .classic(reason: .backendUnavailable))
        service.startWarmUp()
        let session = service.startSession()
        _ = service.insert(session: session, text: "nihongo")
        for _ in 0..<100 { XCTAssertNotNil(service.convert(session: session)) }
        let snapshot = client.snapshot()
        XCTAssertEqual(snapshot.configureCount, 1)
        XCTAssertEqual(snapshot.statusCount, 0)
    }

    func testWarmUpRequiresDecodeAttemptBeforeGpuActive() {
        let client = StubZenzaiRuntimeClient(
            configure: ZenzaiRuntimeStatus(state: .gpuActive, backend: "Vulkan", device: "Radeon 890M"),
            statuses: [
                ZenzaiRuntimeStatus(state: .gpuActive, backend: "Vulkan", device: "Radeon 890M"),
                ZenzaiRuntimeStatus(state: .gpuActive, backend: "Vulkan", device: "Radeon 890M")
            ])
        let service = ConversionService(config: config(), runtimeClient: client)

        service.startWarmUp()

        waitForState(service) { state in
            if case .classic(reason: .warmupFailed) = state { return true }
            return false
        }
        XCTAssertEqual(client.snapshot().configureCount, 1)
    }

    func testWarmUpTransitionsToGpuActiveOnlyAfterDecode() {
        let active = ZenzaiRuntimeStatus(state: .gpuActive, backend: "Vulkan", device: "Radeon 890M")
        let client = StubZenzaiRuntimeClient(
            configure: active,
            statuses: [
                ZenzaiRuntimeStatus(state: .gpuActive, generation: 1, decodeAttempts: 1,
                                    backend: "Vulkan", device: "Radeon 890M"),
                ZenzaiRuntimeStatus(state: .gpuActive, generation: 1, decodeAttempts: 1,
                                    backend: "Vulkan", device: "Radeon 890M")
            ])
        let service = ConversionService(config: config(), runtimeClient: client)

        service.startWarmUp()

        waitForState(service) { $0 == .gpuActive(device: "Radeon 890M") }
        XCTAssertTrue(service.makeOptionsZenzaiRequestForTesting().zenzaiOn)
    }

    func testRequestFailureDiscardsZenzaiResultAndRetriesClassic() {
        let configured = ZenzaiRuntimeStatus(state: .gpuActive, generation: 1,
                                             backend: "Vulkan", device: "GPU")
        let active = ZenzaiRuntimeStatus(state: .gpuActive, generation: 1,
                                         decodeAttempts: 1, backend: "Vulkan", device: "GPU")
        let failed = ZenzaiRuntimeStatus(state: .failed, failure: .decode, generation: 1)
        let client = StubZenzaiRuntimeClient(
            configure: configured,
            statuses: [active, active, failed])
        let service = ConversionService(config: config(), runtimeClient: client)
        service.startWarmUp()
        waitForState(service) { $0 == .gpuActive(device: "GPU") }

        let session = service.startSession()
        _ = service.insert(session: session, text: "nihongo")
        service.setClassicResetPendingForTesting()
        let resetCount = service.classicResetStateForTesting.count
        XCTAssertFalse(service.convert(session: session)?.isEmpty ?? true)
        XCTAssertEqual(service.zenzaiRuntimeState,
                       ZenzaiRuntimeState.classic(reason: .decodeFailed))
        XCTAssertFalse(service.makeOptionsZenzaiRequestForTesting().zenzaiOn)
        XCTAssertFalse(service.classicResetStateForTesting.pending)
        XCTAssertEqual(service.classicResetStateForTesting.count, resetCount + 1,
                       "native失敗結果を捨ててclassicを再実行する前に別セッション文脈を破棄する")
        let count = client.snapshot().statusCount
        _ = service.convert(session: session)
        XCTAssertEqual(client.snapshot().statusCount, count)
    }

    func testOrdinaryReloadPreservesRuntimeFailureLatch() throws {
        let model = FileManager.default.temporaryDirectory.appendingPathComponent("zenzai-\(UUID().uuidString).gguf")
        try Data("model".utf8).write(to: model)
        defer { try? FileManager.default.removeItem(at: model) }

        let client = StubZenzaiRuntimeClient(
            configure: ZenzaiRuntimeStatus(state: .failed, failure: .backendUnavailable))
        let service = ConversionService(config: config(weight: model), runtimeClient: client)
        service.startWarmUp()
        XCTAssertEqual(service.zenzaiRuntimeState, .classic(reason: .backendUnavailable))

        XCTAssertTrue(service.reload(overrides: [
            "NOSPACEKEY_ZENZAI": "on",
            "NOSPACEKEY_ZENZAI_WEIGHT": model.path,
            "NOSPACEKEY_ZENZAI_RUNTIME_DIR": runtimeDirectory.path,
            "NOSPACEKEY_LEARNING": "0"
        ], cpuMeetsLlamaBaseline: true))

        XCTAssertEqual(service.zenzaiRuntimeState, .classic(reason: .backendUnavailable))
        XCTAssertEqual(client.snapshot().configureCount, 1)
    }

    func testExplicitRetryClearsFailureAndUsesExplicitRetryFlag() {
        let configured = ZenzaiRuntimeStatus(state: .gpuActive, generation: 2,
                                             backend: "Vulkan", device: "GPU")
        let active = ZenzaiRuntimeStatus(state: .gpuActive, generation: 2,
                                         decodeAttempts: 1, backend: "Vulkan", device: "GPU")
        let client = StubZenzaiRuntimeClient(
            configure: ZenzaiRuntimeStatus(state: .failed, failure: .backendUnavailable),
            statuses: [active, active])
        let service = ConversionService(config: config(), runtimeClient: client)
        service.startWarmUp()
        XCTAssertEqual(service.zenzaiRuntimeState, .classic(reason: .backendUnavailable))

        client.setConfiguredStatus(configured)
        service.retryZenzai()
        waitForState(service) { $0 == .gpuActive(device: "GPU") }
        XCTAssertEqual(client.snapshot().retries, [false, true])
    }

    func testModelChangeStartsANewRuntimeGenerationProbe() throws {
        let modelA = FileManager.default.temporaryDirectory.appendingPathComponent(
            "zenzai-runtime-state-a-\(UUID().uuidString).gguf")
        let modelB = FileManager.default.temporaryDirectory.appendingPathComponent(
            "zenzai-runtime-state-b-\(UUID().uuidString).gguf")
        try Data("model-a".utf8).write(to: modelA)
        try Data("model-b".utf8).write(to: modelB)
        defer {
            try? FileManager.default.removeItem(at: modelA)
            try? FileManager.default.removeItem(at: modelB)
        }

        let active = ZenzaiRuntimeStatus(state: .gpuActive, generation: 2,
                                         decodeAttempts: 1, backend: "Vulkan", device: "GPU")
        let client = StubZenzaiRuntimeClient(
            configure: ZenzaiRuntimeStatus(state: .failed, failure: .backendUnavailable),
            statuses: [active, active])
        let service = ConversionService(config: config(weight: modelA), runtimeClient: client)
        service.startWarmUp()
        XCTAssertEqual(service.zenzaiRuntimeState, .classic(reason: .backendUnavailable))

        client.setConfiguredStatus(ZenzaiRuntimeStatus(state: .gpuActive, generation: 2,
                                                       backend: "Vulkan", device: "GPU"))
        XCTAssertTrue(service.reload(overrides: [
            "NOSPACEKEY_ZENZAI": "on",
            "NOSPACEKEY_ZENZAI_WEIGHT": modelB.path,
            "NOSPACEKEY_ZENZAI_RUNTIME_DIR": runtimeDirectory.path
        ], cpuMeetsLlamaBaseline: true))

        waitForState(service) { $0 == .gpuActive(device: "GPU") }
        XCTAssertEqual(client.snapshot().configureCount, 2)
        XCTAssertEqual(client.snapshot().retries, [false, true])
        XCTAssertEqual(service.zenzaiWeightURLForTesting, modelB)
    }

    func testRuntimeDirectoryChangeStartsANewRuntimeGenerationProbe() throws {
        let runtimeA = FileManager.default.temporaryDirectory.appendingPathComponent(
            "zenzai-runtime-state-runtime-a-\(UUID().uuidString)")
        let runtimeB = FileManager.default.temporaryDirectory.appendingPathComponent(
            "zenzai-runtime-state-runtime-b-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: runtimeA, withIntermediateDirectories: false)
        try FileManager.default.createDirectory(at: runtimeB, withIntermediateDirectories: false)
        defer {
            try? FileManager.default.removeItem(at: runtimeA)
            try? FileManager.default.removeItem(at: runtimeB)
        }

        let active = ZenzaiRuntimeStatus(state: .gpuActive, generation: 2,
                                         decodeAttempts: 1, backend: "Vulkan", device: "GPU")
        let client = StubZenzaiRuntimeClient(
            configure: ZenzaiRuntimeStatus(state: .failed, failure: .backendUnavailable),
            statuses: [active, active])
        let service = ConversionService(
            config: ZenzaiConfig(weightURL: Self.existingModelURL, inferenceLimit: 1,
                                 runtimeDirectory: runtimeA),
            runtimeClient: client)
        service.startWarmUp()
        XCTAssertEqual(service.zenzaiRuntimeState, .classic(reason: .backendUnavailable))

        client.setConfiguredStatus(ZenzaiRuntimeStatus(state: .gpuActive, generation: 2,
                                                       backend: "Vulkan", device: "GPU"))
        XCTAssertTrue(service.reload(overrides: [
            "NOSPACEKEY_ZENZAI": "on",
            "NOSPACEKEY_ZENZAI_WEIGHT": Self.existingModelURL.path,
            "NOSPACEKEY_ZENZAI_RUNTIME_DIR": runtimeB.path
        ], cpuMeetsLlamaBaseline: true))

        waitForState(service) { $0 == .gpuActive(device: "GPU") }
        let configuredDirectories = client.configuredRuntimeDirectories().map {
            $0.standardizedFileURL.path.trimmingCharacters(in: CharacterSet(charactersIn: "\\/"))
        }
        let expectedDirectories = [runtimeA, runtimeB].map {
            $0.standardizedFileURL.path.trimmingCharacters(in: CharacterSet(charactersIn: "\\/"))
        }
        XCTAssertEqual(configuredDirectories, expectedDirectories)
        XCTAssertEqual(client.snapshot().retries, [false, true])
    }

    #if os(Windows) && DEBUG
    func testNativeAdapterRejectsABIAndStructSizeMismatch() {
        let status = NativeZenzaiRuntimeClient.validateConfigureResultForTesting(
            result: 0,
            abiVersion: 99,
            structSize: 0,
            stateRaw: 1,
            failureRaw: 0)

        XCTAssertEqual(status.state, .failed)
        XCTAssertEqual(status.failure, .unknown)
    }

    func testNativeAdapterRejectsInconsistentStatusStateAndFailure() {
        let status = NativeZenzaiRuntimeClient.validateStatusResultForTesting(
            result: 0,
            abiVersion: 1,
            structSize: NativeZenzaiRuntimeClient.expectedStructSizeForTesting,
            stateRaw: 2,
            failureRaw: 0)

        XCTAssertEqual(status.state, .failed)
        XCTAssertEqual(status.failure, .unknown)
    }
    #endif
}
