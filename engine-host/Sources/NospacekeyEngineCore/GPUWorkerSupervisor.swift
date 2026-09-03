import Foundation
import KanaKanjiConverterModuleWithDefaultDictionary

public enum GPUWorkerTransportStartResult: Equatable, Sendable {
    case ready(backend: String, device: String)
    case failure(GPUWorkerFailure)
}

public enum GPUWorkerTransportReply: Equatable, Sendable {
    case response(GPUWorkerResponse)
    case timeout
    case exit
    case crash
    case protocolMismatch
    case nativeFailure
}

/// The process/pipe seam is intentionally injectable.  Production uses the
/// same-executable named-pipe transport; tests can deterministically inject a
/// timeout, exit, crash, or malformed response without touching native code.
public protocol GPUWorkerTransport: AnyObject, Sendable {
    func start(generation: UInt64) -> GPUWorkerTransportStartResult
    func start(generation: UInt64,
               configuration: GPUWorkerRuntimeConfiguration?) -> GPUWorkerTransportStartResult
    func request(_ request: GPUWorkerRequest, timeout: TimeInterval) -> GPUWorkerTransportReply
    func terminate()
}

public extension GPUWorkerTransport {
    /// Compatibility default keeps deterministic test transports small while
    /// production transports receive the explicit canonical configuration.
    func start(generation: UInt64,
               configuration: GPUWorkerRuntimeConfiguration?) -> GPUWorkerTransportStartResult {
        _ = configuration
        return start(generation: generation)
    }
}

public enum GPUWorkerDisplayState: String, Equatable, Sendable {
    case stopped
    case preparing
    case gpuActive = "gpu_active"
    case classic
    case disabled
}

/// Sanitized status for UI/diagnostics.  It deliberately excludes worker
/// path, input, candidate text, generation, and attempt counters.
public struct GPUWorkerSupervisorSnapshot: Equatable, Sendable {
    public let state: GPUWorkerDisplayState
    public let backend: String?
    public let device: String?
    public let reason: String?

    public init(state: GPUWorkerDisplayState,
                backend: String? = nil,
                device: String? = nil,
                reason: String? = nil) {
        self.state = state
        self.backend = backend
        self.device = device
        self.reason = reason
    }
}

public enum GPUWorkerDeadlineTier: Sendable {
    /// Leave enough time in the external 1200 ms Convert deadline to return
    /// the already-computed classic result and reap the child.
    case convert
    /// Leave enough time in the external 400 ms Live deadline for the same
    /// fallback/reap path.
    case live

    public var workerBudget: TimeInterval {
        switch self {
        case .convert: return 0.9
        case .live: return 0.25
        }
    }
}

/// Owns the worker process generation and failure latch.  A request never
/// mutates classic state: the caller supplies the already-computed classic
/// ConversionResult and receives either that result or a reordered view of it.
public final class GPUWorkerSupervisor: @unchecked Sendable {
    private enum InternalState {
        case stopped
        case starting
        case ready
        case quarantined(GPUWorkerQuarantineReason)
        case disabled

        var isLive: Bool {
            switch self {
            case .starting, .ready: return true
            case .stopped, .quarantined, .disabled: return false
            }
        }
    }

    private let transport: GPUWorkerTransport
    private let allowsLazyStart: Bool
    /// State/status reads never wait for a native or pipe operation.
    private let stateLock = NSLock()
    /// Serializes start/request/terminate operations on the one child.
    private let operationLock = NSLock()
    private var internalState: InternalState = .stopped
    private var generation: UInt64 = 1
    private var nextRequestID: UInt64 = 0
    private var backend: String?
    private var device: String?
    private var reason: String?
    private var retryArmed = false
    private var terminatedGeneration: UInt64?
    /// Generation currently owning (or about to own) the transport.  Lifecycle
    /// changes use this to reap an old child without accidentally terminating a
    /// replacement that acquired operationLock first.
    private var transportGeneration: UInt64?
    private var runtimeConfiguration: GPUWorkerRuntimeConfiguration?

    public init(transport: GPUWorkerTransport,
                runtimeConfiguration: GPUWorkerRuntimeConfiguration? = nil,
                allowsLazyStart: Bool = true) {
        self.transport = transport
        self.runtimeConfiguration = runtimeConfiguration
        self.allowsLazyStart = allowsLazyStart
    }

    public var snapshot: GPUWorkerSupervisorSnapshot {
        stateLock.lock()
        defer { stateLock.unlock() }
        return snapshotLocked()
    }

    /// Public state alias used by engine status adapters.
    public var state: GPUWorkerSupervisorSnapshot { snapshot }

    public func rerank(
        classic: ConversionResult,
        snapshot: GPUWorkerCompositionSnapshot,
        leftContext: String?,
        nBest: Int,
        inferenceLimit: Int,
        deadline: TimeInterval = GPUWorkerDeadlineTier.convert.workerBudget
    ) -> GPUWorkerRerankDecision {
        // Empty and custom-mapped input remain classic without even spawning or
        // sending a worker request.
        guard !snapshot.convertTarget.isEmpty else {
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false)
        }
        guard snapshot.supportsGPUWorker else {
            // A custom input table is a supported classic request, but not a
            // worker request.  Do not turn this ordinary fallback into a
            // failure latch or a retry reason.
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false)
        }
        guard (try? snapshot.makeComposingText()) != nil else {
            return GPUWorkerRerankDecision(
                conversion: classic, usedWorker: false, failure: .unsupportedInput)
        }

        // A background warm-up owns the operation lock while native model
        // loading.  Do not wait behind it: this request must return the
        // already-computed classic result within the caller's deadline.
        stateLock.lock()
        let currentState = internalState
        stateLock.unlock()
        if case .starting = currentState {
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false)
        }
        if case .stopped = currentState, !allowsLazyStart {
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false)
        }
        if case .quarantined(let quarantineReason) = currentState {
            let failure: GPUWorkerRerankFailure = switch quarantineReason {
            case .timeout: .timeout
            case .workerExit: .workerExit
            case .crash: .crash
            case .workerProtocol: .workerProtocol
            case .candidateMismatch: .candidateMismatch
            case .nativeFailure: .nativeFailure
            case .invalidRuntimeDirectory: .invalidRuntimeDirectory
            case .backendPathRejected: .backendPathRejected
            case .backendUnavailable: .backendUnavailable
            case .gpuUnavailable: .gpuUnavailable
            case .modelLoad: .modelLoad
            case .contextLoad: .contextLoad
            case .decode: .decode
            case .warmup: .warmup
            }
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false, failure: failure)
        }

        operationLock.lock()
        defer { operationLock.unlock() }
        guard ensureReady() else {
            stateLock.lock()
            let failure = currentFailureLocked()
            stateLock.unlock()
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false, failure: failure)
        }
        stateLock.lock()
        guard case .ready = internalState, transportGeneration == generation else {
            let failure = currentFailureLocked()
            stateLock.unlock()
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false, failure: failure)
        }
        nextRequestID &+= 1
        let requestID = nextRequestID
        let currentGeneration = generation
        stateLock.unlock()
        let request = GPUWorkerRequest(
            snapshot: snapshot,
            leftContext: leftContext,
            nBest: max(10, nBest),
            inferenceLimit: max(1, inferenceLimit),
            requestID: requestID,
            generation: currentGeneration)
        let reply = transport.request(request, timeout: deadline)
        switch reply {
        case .response(let response):
            stateLock.lock()
            let generationStillCurrent = generation == currentGeneration
            stateLock.unlock()
            guard generationStillCurrent else {
                return GPUWorkerRerankDecision(
                    conversion: classic, usedWorker: false, failure: .workerProtocol)
            }
            let decision = GPUWorkerReranker.apply(
                response: response, to: classic,
                requestID: requestID, generation: currentGeneration,
                snapshot: snapshot)
            guard decision.failure == nil else {
                let mapped = Self.quarantineReason(for: decision.failure!)
                quarantine(mapped, expectedGeneration: currentGeneration)
                return GPUWorkerRerankDecision(conversion: classic, usedWorker: false,
                                               failure: Self.rerankFailure(for: mapped))
            }
            return decision
        case .timeout:
            quarantine(.timeout, expectedGeneration: currentGeneration)
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false, failure: .timeout)
        case .exit:
            quarantine(.workerExit, expectedGeneration: currentGeneration)
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false, failure: .workerExit)
        case .crash:
            quarantine(.crash, expectedGeneration: currentGeneration)
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false, failure: .crash)
        case .protocolMismatch:
            quarantine(.workerProtocol, expectedGeneration: currentGeneration)
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false, failure: .workerProtocol)
        case .nativeFailure:
            quarantine(.nativeFailure, expectedGeneration: currentGeneration)
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false, failure: .nativeFailure)
        }
    }

    /// Spawn/warm the child before user requests.  The native transport owns
    /// the dummy decode and returns ready only after typed GPU state and an
    /// increased decode counter have been observed.
    public func startWarmUp() {
        stateLock.lock()
        guard case .stopped = internalState else {
            stateLock.unlock()
            return
        }
        internalState = .starting
        retryArmed = false
        let warmGeneration = generation
        stateLock.unlock()
        Thread.detachNewThread { [weak self] in
            self?.performStart(generation: warmGeneration)
        }
    }

    /// Only explicit retry, model change, or runtime-directory change opens a
    /// quarantined generation. Duplicate retries while already armed are no-op.
    public func explicitRetry() {
        stateLock.lock()
        guard case .quarantined = internalState, !retryArmed else {
            stateLock.unlock()
            return
        }
        generation &+= 1
        internalState = .stopped
        retryArmed = true
        backend = nil
        device = nil
        reason = nil
        terminatedGeneration = nil
        stateLock.unlock()
    }

    public func modelOrRuntimeChanged(configuration: GPUWorkerRuntimeConfiguration? = nil) {
        stateLock.lock()
        let oldGeneration = generation
        let shouldTerminate = internalState.isLive || transportGeneration == oldGeneration
        generation &+= 1
        internalState = .stopped
        retryArmed = true
        backend = nil
        device = nil
        reason = nil
        terminatedGeneration = nil
        runtimeConfiguration = configuration
        stateLock.unlock()
        if shouldTerminate {
            scheduleTermination(of: oldGeneration)
        }
    }

    /// Ordinary settings reload intentionally preserves a failure latch.
    public func ordinaryReload() {}

    /// Stop an active worker when Zenzai is disabled.  This is a lifecycle
    /// operation, not a failure, so the public status becomes disabled.
    public func disable() {
        stateLock.lock()
        let oldGeneration = generation
        let shouldTerminate = internalState.isLive || transportGeneration == oldGeneration
        generation &+= 1
        internalState = .disabled
        retryArmed = false
        backend = nil
        device = nil
        reason = nil
        terminatedGeneration = nil
        stateLock.unlock()
        if shouldTerminate {
            scheduleTermination(of: oldGeneration)
        }
    }

    private func ensureReady() -> Bool {
        stateLock.lock()
        let state = internalState
        if case .ready = state {
            stateLock.unlock()
            return true
        }
        if case .quarantined = state {
            stateLock.unlock()
            return false
        }
        if case .starting = state {
            stateLock.unlock()
            return false
        }
        if case .disabled = state {
            stateLock.unlock()
            return false
        }
        internalState = .starting
        let startGeneration = generation
        let startConfiguration = runtimeConfiguration
        retryArmed = false
        transportGeneration = startGeneration
        stateLock.unlock()

        let startResult = transport.start(generation: startGeneration,
                                          configuration: startConfiguration)
        switch startResult {
        case .ready(let newBackend, let newDevice):
            stateLock.lock()
            let isCurrent = generation == startGeneration
                && (ifCaseStarting(internalState))
            if isCurrent {
                backend = newBackend.isEmpty ? nil : newBackend
                device = newDevice.isEmpty ? nil : newDevice
                reason = nil
                internalState = .ready
            } else if transportGeneration == startGeneration {
                transportGeneration = nil
            }
            stateLock.unlock()
            if !isCurrent { transport.terminate() }
            if !isCurrent { return false }
            return true
        case .failure(let failure):
            stateLock.lock()
            let isCurrent = generation == startGeneration
                && (ifCaseStarting(internalState))
            if !isCurrent, transportGeneration == startGeneration {
                transportGeneration = nil
            }
            stateLock.unlock()
            if isCurrent {
                quarantine(Self.quarantineReason(for: failure), expectedGeneration: startGeneration)
            } else {
                transport.terminate()
            }
            return false
        }
    }

    private func performStart(generation startGeneration: UInt64) {
        operationLock.lock()
        defer { operationLock.unlock() }
        stateLock.lock()
        guard generation == startGeneration, ifCaseStarting(internalState) else {
            stateLock.unlock()
            return
        }
        let startConfiguration = runtimeConfiguration
        transportGeneration = startGeneration
        stateLock.unlock()
        let result = transport.start(generation: startGeneration,
                                     configuration: startConfiguration)
        switch result {
        case .ready(let newBackend, let newDevice):
            stateLock.lock()
            let isCurrent = generation == startGeneration
                && ifCaseStarting(internalState)
            if isCurrent {
                backend = newBackend.isEmpty ? nil : newBackend
                device = newDevice.isEmpty ? nil : newDevice
                reason = nil
                internalState = .ready
            } else if transportGeneration == startGeneration {
                transportGeneration = nil
            }
            stateLock.unlock()
            if !isCurrent { transport.terminate() }
        case .failure(let failure):
            stateLock.lock()
            let isCurrent = generation == startGeneration
                && ifCaseStarting(internalState)
            if !isCurrent, transportGeneration == startGeneration {
                transportGeneration = nil
            }
            stateLock.unlock()
            if isCurrent {
                quarantine(Self.quarantineReason(for: failure), expectedGeneration: startGeneration)
            } else {
                transport.terminate()
            }
        }
    }

    private func ifCaseStarting(_ state: InternalState) -> Bool {
        if case .starting = state { return true }
        return false
    }

    /// Reap a stale transport without making a settings reload or disable wait
    /// behind a native start/request.  The immediate try keeps the common idle
    /// path synchronous for deterministic teardown; a busy operation is
    /// released on a detached reaper thread.
    private func scheduleTermination(of generation: UInt64) {
        if operationLock.lock(before: Date()) {
            terminateTransportLocked(for: generation)
            operationLock.unlock()
            return
        }
        Thread.detachNewThread { [weak self] in
            guard let self else { return }
            self.operationLock.lock()
            self.terminateTransportLocked(for: generation)
            self.operationLock.unlock()
        }
    }

    /// operationLock must be held.  A replacement generation may have started
    /// while the reaper waited; in that case the old cleanup is a no-op.
    private func terminateTransportLocked(for generation: UInt64) {
        stateLock.lock()
        guard transportGeneration == generation else {
            stateLock.unlock()
            return
        }
        transportGeneration = nil
        stateLock.unlock()
        transport.terminate()
    }

    private static func quarantineReason(for failure: GPUWorkerFailure) -> GPUWorkerQuarantineReason {
        switch failure {
        case .timeout: return .timeout
        case .workerExit: return .workerExit
        case .crash: return .crash
        case .protocolMismatch: return .workerProtocol
        case .nativeFailure, .unsupportedInput, .unavailable,
             .invalidRuntimeDirectory, .backendPathRejected, .backendUnavailable,
             .gpuUnavailable, .modelLoad, .contextLoad, .decode, .warmup:
            switch failure {
            case .invalidRuntimeDirectory: return .invalidRuntimeDirectory
            case .backendPathRejected: return .backendPathRejected
            case .backendUnavailable: return .backendUnavailable
            case .gpuUnavailable: return .gpuUnavailable
            case .modelLoad: return .modelLoad
            case .contextLoad: return .contextLoad
            case .decode: return .decode
            case .warmup: return .warmup
            default: return .nativeFailure
            }
        }
    }

    private static func quarantineReason(for failure: GPUWorkerRerankFailure) -> GPUWorkerQuarantineReason {
        switch failure {
        case .unknownCandidate, .duplicateCandidate, .candidateMismatch:
            return .candidateMismatch
        case .timeout: return .timeout
        case .workerExit: return .workerExit
        case .crash: return .crash
        case .workerProtocol, .protocolMismatch, .unsupportedInput:
            return .workerProtocol
        case .nativeFailure: return .nativeFailure
        case .invalidRuntimeDirectory: return .invalidRuntimeDirectory
        case .backendPathRejected: return .backendPathRejected
        case .backendUnavailable: return .backendUnavailable
        case .gpuUnavailable: return .gpuUnavailable
        case .modelLoad: return .modelLoad
        case .contextLoad: return .contextLoad
        case .decode: return .decode
        case .warmup: return .warmup
        }
    }

    private static func rerankFailure(for reason: GPUWorkerQuarantineReason) -> GPUWorkerRerankFailure {
        switch reason {
        case .timeout: return .timeout
        case .workerExit: return .workerExit
        case .crash: return .crash
        case .workerProtocol: return .workerProtocol
        case .candidateMismatch: return .candidateMismatch
        case .nativeFailure: return .nativeFailure
        case .invalidRuntimeDirectory: return .invalidRuntimeDirectory
        case .backendPathRejected: return .backendPathRejected
        case .backendUnavailable: return .backendUnavailable
        case .gpuUnavailable: return .gpuUnavailable
        case .modelLoad: return .modelLoad
        case .contextLoad: return .contextLoad
        case .decode: return .decode
        case .warmup: return .warmup
        }
    }

    private func quarantine(_ quarantineReason: GPUWorkerQuarantineReason,
                            expectedGeneration: UInt64) {
        stateLock.lock()
        guard generation == expectedGeneration else {
            stateLock.unlock()
            return
        }
        guard case .quarantined = internalState else {
            internalState = .quarantined(quarantineReason)
            reason = quarantineReason.rawValue
            let mustTerminate = terminatedGeneration != generation
            terminatedGeneration = generation
            transportGeneration = nil
            stateLock.unlock()
            if mustTerminate { transport.terminate() }
            return
        }
        stateLock.unlock()
    }

    private func currentFailureLocked() -> GPUWorkerRerankFailure? {
        guard case .quarantined(let value) = internalState else { return nil }
        switch value {
        case .timeout: return .timeout
        case .workerExit: return .workerExit
        case .crash: return .crash
        case .workerProtocol: return .workerProtocol
        case .candidateMismatch: return .candidateMismatch
        case .nativeFailure: return .nativeFailure
        case .invalidRuntimeDirectory: return .invalidRuntimeDirectory
        case .backendPathRejected: return .backendPathRejected
        case .backendUnavailable: return .backendUnavailable
        case .gpuUnavailable: return .gpuUnavailable
        case .modelLoad: return .modelLoad
        case .contextLoad: return .contextLoad
        case .decode: return .decode
        case .warmup: return .warmup
        }
    }

    private func snapshotLocked() -> GPUWorkerSupervisorSnapshot {
        switch internalState {
        case .stopped:
            return GPUWorkerSupervisorSnapshot(
                state: retryArmed ? .preparing : .stopped, backend: backend, device: device)
        case .starting:
            return GPUWorkerSupervisorSnapshot(state: .preparing, backend: backend, device: device)
        case .ready:
            return GPUWorkerSupervisorSnapshot(state: .gpuActive, backend: backend, device: device)
        case .quarantined:
            return GPUWorkerSupervisorSnapshot(state: .classic, backend: backend, device: device, reason: reason)
        case .disabled:
            return GPUWorkerSupervisorSnapshot(state: .disabled)
        }
    }
}
