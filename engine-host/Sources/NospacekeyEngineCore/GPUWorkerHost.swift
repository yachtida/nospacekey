import Foundation
import WinSDK

#if DEBUG
enum GPUWorkerTestFault: String, Equatable, Sendable {
    case gpuUnavailable = "typed:gpu_unavailable"
    case backendUnavailable = "typed:backend_unavailable"
    case driverUnavailable = "typed:driver_unavailable"
    case modelLoad = "typed:model_load"
    case contextLoad = "typed:context_load"
    case warmup = "typed:warmup"

    static func parse(_ value: String?) -> Self? {
        guard let value else { return nil }
        return Self(rawValue: value)
    }

    var reason: String {
        switch self {
        case .gpuUnavailable: return "gpu_unavailable"
        case .backendUnavailable: return "backend_unavailable"
        case .driverUnavailable: return "gpu_unavailable"
        case .modelLoad: return "model_load"
        case .contextLoad: return "context_load"
        case .warmup: return "warmup"
        }
    }

    var runtimeFailure: GPUWorkerRuntimeFailure? {
        switch self {
        case .gpuUnavailable, .driverUnavailable: return .gpuUnavailable
        case .backendUnavailable: return .backendUnavailable
        case .modelLoad: return .modelLoad
        case .contextLoad: return .contextLoad
        case .warmup: return nil
        }
    }
}
#endif

private final class GPUWorkerHostSession: @unchecked Sendable {
    private let lock = NSLock()
    private var service: ConversionService?
    private var generation: UInt64?
    private var temporaryDirectory: URL?
    private var handshakeCompleted = false

    func handle(_ body: Data) -> (reply: Data, exitAfterReply: Bool) {
        if !handshakeCompleted {
            return handleHandshake(body)
        }
        guard let request = try? Framing.decode(GPUWorkerRequest.self, from: body) else {
            return response(GPUWorkerResponse(
                requestID: 0, generation: generation ?? 0, failure: .protocolMismatch))
        }
        return handleRank(request)
    }

    func cleanup() {
        lock.lock()
        let directory = temporaryDirectory
        service = nil
        temporaryDirectory = nil
        generation = nil
        lock.unlock()
        if let directory { try? FileManager.default.removeItem(at: directory) }
    }

    private func handleHandshake(_ body: Data) -> (reply: Data, exitAfterReply: Bool) {
        guard let request = try? Framing.decode(GPUWorkerHandshakeRequest.self, from: body),
              request.version == GPUWorkerRequest.currentVersion,
              let wireConfiguration = request.configuration,
              let configuration = wireConfiguration.makeConfiguration() else {
            return handshakeResponse(
                GPUWorkerHandshakeResponse(generation: 0, ready: false, failure: .unknown))
        }

#if DEBUG
        if let fault = ProcessInfo.processInfo.environment["NOSPACEKEY_GPU_WORKER_TEST_FAULT"] {
            if fault == "crash" { exit(70) }
            if fault.hasPrefix("delay:") {
                let milliseconds = Int(fault.dropFirst("delay:".count)) ?? 0
                Thread.sleep(forTimeInterval: min(10, max(0, Double(milliseconds) / 1000)))
            }
        }
        if let fault = GPUWorkerTestFault.parse(
            ProcessInfo.processInfo.environment["NOSPACEKEY_GPU_WORKER_TEST_FAULT"]) {
            let response = GPUWorkerHandshakeResponse(
                generation: request.generation, ready: false,
                failure: fault.runtimeFailure)
            engineLog("ev=zenzai_worker_fault_injected reason=\(fault.reason)\n")
            engineLog("ev=zenzai_worker_native_attempts model_load=0 context_init=0 decode=0\n")
            handshakeCompleted = true
            return handshakeResponse(response)
        }
#endif

        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-gpu-worker-\(UUID().uuidString)", isDirectory: true)
        do {
            try FileManager.default.createDirectory(
                at: directory, withIntermediateDirectories: true)
        } catch {
            return handshakeResponse(
                GPUWorkerHandshakeResponse(
                    generation: request.generation, ready: false, failure: .unknown))
        }
        let config = ZenzaiConfig(
            weightURL: configuration.modelURL,
            inferenceLimit: configuration.inferenceLimit,
            runtimeDirectory: configuration.runtimeDirectory)
        let worker = ConversionService(
            config: config,
            learning: .disabled,
            environment: [:],
            processRole: .gpuWorker,
            privateTemporaryDirectory: directory)
        lock.lock()
        service = worker
        generation = request.generation
        temporaryDirectory = directory
        lock.unlock()

        worker.startWarmUp()
        let deadline = Date().addingTimeInterval(5)
        while Date() < deadline {
            let state = worker.zenzaiRuntimeSnapshot.state
            if state != .preparing { break }
            Thread.sleep(forTimeInterval: 0.01)
        }
        let response = worker.gpuWorkerHandshake(generation: request.generation)
        handshakeCompleted = true
        let nativeStatus = worker.zenzaiRuntimeStatus
        engineLog(
            "ev=zenzai_worker_native_attempts model_load=\(nativeStatus.modelLoadAttempts) " +
            "context_init=\(nativeStatus.contextInitAttempts) " +
            "decode=\(nativeStatus.decodeAttempts)\n")
        if response.ready {
            let backend = response.backend ?? "unknown"
            let device = response.device ?? "unknown"
            engineLog("ev=zenzai_worker_gpu_active backend=\(backend) device=\(device)\n")
        } else {
            let reason = response.failure.map { String(describing: $0) } ?? "unknown"
            engineLog("ev=zenzai_worker_warmup_failed reason=\(reason)\n")
        }
        return handshakeResponse(response)
    }

    private func handleRank(_ request: GPUWorkerRequest) -> (reply: Data, exitAfterReply: Bool) {
        lock.lock()
        let worker = service
        let expectedGeneration = generation
        lock.unlock()
        guard request.version == GPUWorkerRequest.currentVersion,
              request.operation == .rank,
              expectedGeneration == request.generation,
              let worker else {
            return response(GPUWorkerResponse(
                requestID: request.requestID, generation: request.generation,
                failure: .protocolMismatch))
        }

#if DEBUG
        if ProcessInfo.processInfo.environment["NOSPACEKEY_GPU_WORKER_TEST_FAULT"] == "crash-rank" {
            exit(71)
        }
        if let fault = ProcessInfo.processInfo.environment["NOSPACEKEY_GPU_WORKER_TEST_FAULT"],
           fault.hasPrefix("delay-rank:") {
            let milliseconds = Int(fault.dropFirst("delay-rank:".count)) ?? 0
            Thread.sleep(forTimeInterval: min(10, max(0, Double(milliseconds) / 1000)))
        }
#endif

        let evaluation = worker.evaluateGPUWorker(
            snapshot: request.snapshot,
            leftContext: request.leftContext,
            nBest: request.nBest,
            inferenceLimit: request.inferenceLimit)
        guard let conversion = evaluation.conversion else {
            return response(GPUWorkerResponse(
                requestID: request.requestID, generation: request.generation,
                failure: evaluation.failure ?? .nativeFailure))
        }
        return response(GPUWorkerResponse(
            requestID: request.requestID,
            generation: request.generation,
            mainResults: conversion.mainResults.map(GPUWorkerCandidate.init),
            firstClauseResults: conversion.firstClauseResults.map(GPUWorkerCandidate.init)))
    }

    private func handshakeResponse(_ response: GPUWorkerHandshakeResponse)
        -> (reply: Data, exitAfterReply: Bool) {
        let data = (try? JSONEncoder().encode(response)) ?? Data(#"{}"#.utf8)
        return (data, false)
    }

    private func response(_ response: GPUWorkerResponse)
        -> (reply: Data, exitAfterReply: Bool) {
        let data = (try? JSONEncoder().encode(response)) ?? Data(#"{}"#.utf8)
        return (data, false)
    }
}

/// runGPUWorkerHost の待ち受けポリシー。テストが「ワーカーは次リクエスト header 待ちを
/// 無期限にする(nil)」配線を直接検証するための seam。
struct GPUWorkerListenConfiguration {
    let oneShot = true
    let requestHeaderIdleTimeoutMs: Int? = nil
}

func makeGPUWorkerListenConfiguration() -> GPUWorkerListenConfiguration {
    GPUWorkerListenConfiguration()
}

/// Entry point used only by the same executable's `--zenzai-gpu-worker` mode.
/// The worker accepts one private pipe connection and never serves the public
/// engine protocol.
public func runGPUWorkerHost(pipeName: String) {
    let session = GPUWorkerHostSession()
    defer { session.cleanup() }
    let listen = makeGPUWorkerListenConfiguration()
    // Why not idle timeout: 親エンジンが 5 分間 rank を送らないだけでワーカーが exit 0 で
    // 自発終了し、次の変換が worker_exit quarantine(明示再試行まで古典固定)に落ちる。
    // 異常時の回収は job(KILL_ON_JOB_CLOSE)・pipe EOF・supervisor の terminate() が担い、
    // spawn 後の connect/handshake 失敗時の回収も supervisor の後続 terminate() 依存 —
    // idle timeout に依存する終了経路は持たせない。
    NamedPipeServer(pipeName: pipeName).run(
        handler: { _, body in session.handle(body) },
        oneShot: listen.oneShot,
        requestHeaderIdleTimeoutMs: listen.requestHeaderIdleTimeoutMs,
        exitHook: {},
        onListening: {})
}
