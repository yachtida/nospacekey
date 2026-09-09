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

/// Which conversion path is asking the worker.  Live-tier callers treat the
/// deadline as a freshness budget (a miss is a weak health signal), while
/// convert-tier callers treat it as a strong one.  Snapshot callers are the
/// background enhancement path and are the only ones eligible for the
/// diagnostic grace mode.
public enum GPUWorkerCaller: String, Sendable {
    case convert
    case live
    case liveSnapshot = "live_snapshot"
    case explicitSnapshot = "explicit_snapshot"

    var isLiveTier: Bool { self == .live || self == .liveSnapshot }
    var isBackgroundSnapshot: Bool { self == .liveSnapshot || self == .explicitSnapshot }
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
        /// A rank timeout terminated the child; a cooldown-armed background
        /// retry owns the next spawn.  Requests short-circuit to classic.
        case retryPending
        /// Auto-respawned child is healthy enough to serve requests, but one
        /// more failure (without an intervening valid rank) escalates.
        case probation
        /// Two live-tier timeouts in a row: live enhancement is suppressed;
        /// the next convert-tier request arms a single background probe.
        case liveSuppressed
        case quarantined(GPUWorkerQuarantineReason)
        case disabled

        var isLive: Bool {
            switch self {
            case .starting, .ready, .probation, .liveSuppressed: return true
            case .stopped, .retryPending, .quarantined, .disabled: return false
            }
        }

        /// States from which rerank may send a transport request.
        var isRequestable: Bool {
            switch self {
            case .ready, .probation: return true
            default: return false
            }
        }
    }

    /// Hard ceiling for the diagnostic grace mode.  A background snapshot
    /// request that misses its soft deadline keeps waiting up to this bound so
    /// the late-response behaviour can be measured instead of destroyed.
    public static let graceHardDeadline: TimeInterval = 3.0
    /// Cooldown before an auto-respawn actually spawns.  Short repetition of
    /// process creation plus GPU model load would pressure the driver.
    public static let defaultRetryCooldown: TimeInterval = 3.0
    /// Latency samples accumulated before a summary line is emitted.
    fileprivate static let latencySummaryBatch = 100

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
    /// Generation whose successful auto-respawn must land in probation rather
    /// than ready.  Lifecycle resets clear it so a stale retry cannot downgrade
    /// a user-initiated start.
    private var autoRetryGeneration: UInt64?
    /// Consecutive rank timeouts without an intervening valid rank response.
    /// Reset only by a valid rank success or a generation-opening lifecycle op.
    private var consecutiveTimeouts = 0
    private let retryCooldown: TimeInterval
    private let graceEnabled: Bool
    private let latencyTracker = GPUWorkerLatencyTracker()

    public init(transport: GPUWorkerTransport,
                runtimeConfiguration: GPUWorkerRuntimeConfiguration? = nil,
                allowsLazyStart: Bool = true,
                retryCooldown: TimeInterval = GPUWorkerSupervisor.defaultRetryCooldown,
                graceEnabled: Bool? = nil) {
        self.transport = transport
        self.runtimeConfiguration = runtimeConfiguration
        self.allowsLazyStart = allowsLazyStart
        self.retryCooldown = retryCooldown
        self.graceEnabled = gpuTraceEnabledForTesting(graceEnabled)
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
        deadline: TimeInterval = GPUWorkerDeadlineTier.convert.workerBudget,
        caller: GPUWorkerCaller = .convert
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
        let traceGeneration = generation
        stateLock.unlock()
        if case .starting = currentState {
            traceShortCircuit(state: "starting", caller: caller, generation: traceGeneration)
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false)
        }
        if case .retryPending = currentState {
            traceShortCircuit(state: "retry_pending", caller: caller, generation: traceGeneration)
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false)
        }
        if case .stopped = currentState, !allowsLazyStart {
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false)
        }
        if case .liveSuppressed = currentState {
            if caller.isLiveTier {
                traceShortCircuit(state: "live_suppressed", caller: caller, generation: traceGeneration)
                return GPUWorkerRerankDecision(conversion: classic, usedWorker: false)
            }
            // The strong caller is the recovery probe trigger: arm one
            // background respawn and answer this request with classic without
            // waiting behind a spawn.
            armStrongCallerProbe(caller: caller)
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
            traceShortCircuit(state: "quarantined", reason: quarantineReason.rawValue,
                              caller: caller, generation: traceGeneration)
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false, failure: failure)
        }

        // Bound foreground contention in production too. The 50ms wait fits
        // between the worker budget (900ms) and the TIP's IPC deadline (1200ms).
        if !caller.isBackgroundSnapshot,
           !operationLock.lock(before: Date().addingTimeInterval(0.05)) {
            traceShortCircuit(state: "operation_lock_busy", caller: caller, generation: traceGeneration)
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false)
        } else if caller.isBackgroundSnapshot {
            operationLock.lock()
        }
        defer { operationLock.unlock() }
        guard ensureReady() else {
            stateLock.lock()
            let failure = currentFailureLocked()
            stateLock.unlock()
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false, failure: failure)
        }
        stateLock.lock()
        guard internalState.isRequestable, transportGeneration == generation else {
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
        let useGrace = graceEnabled && caller.isBackgroundSnapshot
        let effectiveDeadline = useGrace ? Self.graceHardDeadline : deadline
        gpuEngineTraceLog(
            "ev=zenzai_worker_request_attempt caller=\(caller.rawValue) request_id=\(requestID) " +
            "generation=\(currentGeneration) deadline_ms=\(Int(deadline * 1000))\n",
            enabled: graceEnabled)
        let requestStart = DispatchTime.now()
        let reply = transport.request(request, timeout: effectiveDeadline)
        let elapsedMs = Double(DispatchTime.now().uptimeNanoseconds &- requestStart.uptimeNanoseconds)
            / 1_000_000
        gpuEngineTraceLog(
            "ev=zenzai_worker_request_end caller=\(caller.rawValue) request_id=\(requestID) " +
            "generation=\(currentGeneration) outcome=\(traceOutcome(reply)) " +
            "elapsed_ms=\(String(format: "%.1f", elapsedMs))\n",
            enabled: graceEnabled)
        latencyTracker.record(caller: caller, elapsedMs: elapsedMs) { [weak self] line in
            self?.logEvent(line)
        }
        if useGrace, elapsedMs > deadline * 1000 {
            // The soft deadline is a freshness budget, not a health verdict.
            // In diagnostic mode keep waiting to the hard bound so the real
            // response time is observable, then discard the (stale) result.
            logEvent(
                "ev=zenzai_worker_soft_deadline_exceeded caller=\(caller.rawValue) " +
                "request_id=\(requestID) generation=\(currentGeneration) " +
                "soft_ms=\(Int(deadline * 1000))\n")
            if case .response(let response) = reply {
                let valid = response.requestID == requestID && response.generation == currentGeneration
                logEvent(
                    "ev=zenzai_worker_late_response request_id=\(requestID) " +
                    "generation=\(currentGeneration) elapsed_ms=\(String(format: "%.1f", elapsedMs)) " +
                    "valid=\(valid) action=discard\n")
                if valid { noteRankSuccess(generation: currentGeneration) }
                return GPUWorkerRerankDecision(conversion: classic, usedWorker: false)
            }
            logEvent(
                "ev=zenzai_worker_hard_timeout caller=\(caller.rawValue) request_id=\(requestID) " +
                "generation=\(currentGeneration) hard_ms=\(Int(Self.graceHardDeadline * 1000)) " +
                "action=terminate\n")
            handleRankTimeout(caller: caller, generation: currentGeneration, deadline: deadline)
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false, failure: .timeout)
        }
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
            noteRankSuccess(generation: currentGeneration)
            return decision
        case .timeout:
            handleRankTimeout(caller: caller, generation: currentGeneration, deadline: deadline)
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
    /// quarantined or degraded generation. Duplicate retries while already
    /// armed are no-op.
    public func explicitRetry() {
        stateLock.lock()
        switch internalState {
        case .quarantined, .retryPending, .probation, .liveSuppressed:
            guard !retryArmed else {
                stateLock.unlock()
                return
            }
        case .stopped, .starting, .disabled, .ready:
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
        autoRetryGeneration = nil
        consecutiveTimeouts = 0
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
        autoRetryGeneration = nil
        consecutiveTimeouts = 0
        runtimeConfiguration = configuration
        stateLock.unlock()
        if shouldTerminate {
            scheduleTermination(of: oldGeneration, trigger: "model_or_runtime_change")
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
        autoRetryGeneration = nil
        consecutiveTimeouts = 0
        stateLock.unlock()
        if shouldTerminate {
            scheduleTermination(of: oldGeneration, trigger: "disable")
        }
    }

    private func ensureReady() -> Bool {
        stateLock.lock()
        let state = internalState
        if case .ready = state {
            stateLock.unlock()
            return true
        }
        if case .probation = state {
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
        if case .retryPending = state {
            // The cooldown-armed retry owns the next spawn; do not lazy-start
            // around it.
            stateLock.unlock()
            return false
        }
        if case .liveSuppressed = state {
            // Only the strong-caller probe path may respawn from here, and it
            // arms a background spawn instead of blocking this request.
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
            var probationary = false
            if isCurrent {
                backend = newBackend.isEmpty ? nil : newBackend
                device = newDevice.isEmpty ? nil : newDevice
                reason = nil
                probationary = autoRetryGeneration == startGeneration
                autoRetryGeneration = nil
                internalState = probationary ? .probation : .ready
            } else if transportGeneration == startGeneration {
                transportGeneration = nil
            }
            stateLock.unlock()
            if !isCurrent { transport.terminate() }
            if !isCurrent { return false }
            if probationary {
                logEvent("ev=zenzai_worker_retry_ready generation=\(startGeneration)\n")
            }
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
                logEvent("ev=zenzai_worker_start_failed failure=" +
                         "\(Self.quarantineReason(for: failure).rawValue) " +
                         "generation=\(startGeneration)\n")
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
            var probationary = false
            if isCurrent {
                backend = newBackend.isEmpty ? nil : newBackend
                device = newDevice.isEmpty ? nil : newDevice
                reason = nil
                probationary = autoRetryGeneration == startGeneration
                autoRetryGeneration = nil
                internalState = probationary ? .probation : .ready
            } else if transportGeneration == startGeneration {
                transportGeneration = nil
            }
            stateLock.unlock()
            if !isCurrent { transport.terminate() }
            if probationary {
                logEvent("ev=zenzai_worker_retry_ready generation=\(startGeneration)\n")
            }
        case .failure(let failure):
            stateLock.lock()
            let isCurrent = generation == startGeneration
                && ifCaseStarting(internalState)
            if !isCurrent, transportGeneration == startGeneration {
                transportGeneration = nil
            }
            stateLock.unlock()
            if isCurrent {
                logEvent("ev=zenzai_worker_start_failed failure=" +
                         "\(Self.quarantineReason(for: failure).rawValue) " +
                         "generation=\(startGeneration)\n")
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
    private func scheduleTermination(of generation: UInt64, trigger: String) {
        if operationLock.lock(before: Date()) {
            terminateTransportLocked(for: generation, trigger: trigger)
            operationLock.unlock()
            return
        }
        Thread.detachNewThread { [weak self] in
            guard let self else { return }
            self.operationLock.lock()
            self.terminateTransportLocked(for: generation, trigger: trigger)
            self.operationLock.unlock()
        }
    }

    /// operationLock must be held.  A replacement generation may have started
    /// while the reaper waited; in that case the old cleanup is a no-op.
    private func terminateTransportLocked(for generation: UInt64,
                                          trigger: String = "lifecycle") {
        stateLock.lock()
        guard transportGeneration == generation else {
            stateLock.unlock()
            return
        }
        transportGeneration = nil
        stateLock.unlock()
        logEvent("ev=zenzai_worker_terminate trigger=\(trigger) generation=\(generation)\n")
        transport.terminate()
    }

    // MARK: - Timeout recovery

    /// operationLock is held by the rerank caller.  Terminates the timed-out
    /// child (a late reply would desync the single-pipe framing), then opens a
    /// new generation and arms one cooldown-delayed background respawn.  A
    /// second consecutive failure escalates: convert-tier to a permanent
    /// latch, live-tier to live-only suppression.
    private func handleRankTimeout(caller: GPUWorkerCaller,
                                   generation currentGeneration: UInt64,
                                   deadline: TimeInterval) {
        stateLock.lock()
        guard generation == currentGeneration, internalState.isRequestable else {
            stateLock.unlock()
            return
        }
        consecutiveTimeouts += 1
        let streak = consecutiveTimeouts
        let mustTerminate = terminatedGeneration != generation
        terminatedGeneration = generation
        transportGeneration = nil
        switch internalState {
        case .ready:
            generation &+= 1
            let retryGeneration = generation
            internalState = .retryPending
            reason = nil
            autoRetryGeneration = retryGeneration
            stateLock.unlock()
            logEvent("ev=zenzai_worker_timeout caller=\(caller.rawValue) " +
                     "generation=\(currentGeneration) deadline_ms=\(Int(deadline * 1000)) " +
                     "streak=\(streak) action=schedule_retry\n")
            if mustTerminate {
                logEvent("ev=zenzai_worker_terminate trigger=timeout " +
                         "generation=\(currentGeneration)\n")
                transport.terminate()
            }
            scheduleAutoRetry(retryGeneration: retryGeneration)
        case .probation:
            if caller.isLiveTier {
                internalState = .liveSuppressed
                reason = nil
                stateLock.unlock()
                logEvent("ev=zenzai_worker_timeout caller=\(caller.rawValue) " +
                         "generation=\(currentGeneration) deadline_ms=\(Int(deadline * 1000)) " +
                         "streak=\(streak) action=live_suppress\n")
            } else {
                stateLock.unlock()
                logEvent("ev=zenzai_worker_timeout caller=\(caller.rawValue) " +
                         "generation=\(currentGeneration) deadline_ms=\(Int(deadline * 1000)) " +
                         "streak=\(streak) action=quarantine\n")
                quarantine(.timeout, expectedGeneration: currentGeneration)
            }
            if mustTerminate {
                logEvent("ev=zenzai_worker_terminate trigger=timeout " +
                         "generation=\(currentGeneration)\n")
                transport.terminate()
            }
        default:
            stateLock.unlock()
        }
    }

    /// The respawn runs on its own thread so no conversion request ever waits
    /// behind process creation or model load.  The generation token aborts the
    /// retry when a lifecycle change (disable, model change, explicit retry)
    /// opened a newer generation while the cooldown was running.
    private func scheduleAutoRetry(retryGeneration: UInt64) {
        logEvent("ev=zenzai_worker_retry_scheduled cooldown_ms=\(Int(retryCooldown * 1000)) " +
                 "generation=\(retryGeneration)\n")
        Thread.detachNewThread { [weak self] in
            if let self, self.retryCooldown > 0 {
                Thread.sleep(forTimeInterval: self.retryCooldown)
            }
            guard let self else { return }
            self.stateLock.lock()
            guard case .retryPending = self.internalState,
                  self.generation == retryGeneration,
                  self.autoRetryGeneration == retryGeneration else {
                self.stateLock.unlock()
                self.logEvent("ev=zenzai_worker_retry_aborted " +
                              "reason=generation_or_state_changed generation=\(retryGeneration)\n")
                return
            }
            self.internalState = .starting
            self.retryArmed = false
            self.stateLock.unlock()
            self.logEvent("ev=zenzai_worker_retry_start generation=\(retryGeneration)\n")
            self.performStart(generation: retryGeneration)
        }
    }

    /// A convert-tier request while live enhancement is suppressed is the
    /// recovery probe: arm one background respawn.  The triggering request
    /// itself is answered with classic.
    private func armStrongCallerProbe(caller: GPUWorkerCaller) {
        stateLock.lock()
        guard case .liveSuppressed = internalState else {
            stateLock.unlock()
            return
        }
        generation &+= 1
        let probeGeneration = generation
        internalState = .retryPending
        reason = nil
        autoRetryGeneration = probeGeneration
        stateLock.unlock()
        logEvent("ev=zenzai_worker_probe_scheduled caller=\(caller.rawValue) " +
                 "cooldown_ms=\(Int(retryCooldown * 1000)) generation=\(probeGeneration)\n")
        scheduleAutoRetry(retryGeneration: probeGeneration)
    }

    /// Only a valid rank response (and, on the apply path, candidate
    /// validation) may clear the failure escalation.  A plain respawn does
    /// not: the observed failure loop is "spawn ok, ready ok, first rank
    /// times out".
    private func noteRankSuccess(generation currentGeneration: UInt64) {
        stateLock.lock()
        guard generation == currentGeneration else {
            stateLock.unlock()
            return
        }
        guard case .probation = internalState else {
            consecutiveTimeouts = 0
            stateLock.unlock()
            return
        }
        internalState = .ready
        consecutiveTimeouts = 0
        stateLock.unlock()
        logEvent("ev=zenzai_worker_latch_reset reason=valid_rank " +
                 "generation=\(currentGeneration)\n")
    }

    // MARK: - Diagnostics

    private func logEvent(_ line: String) {
        engineLog(line)
    }

    private func traceShortCircuit(state: String, reason: String? = nil,
                                   caller: GPUWorkerCaller, generation: UInt64) {
        gpuEngineTraceLog(
            "ev=zenzai_worker_short_circuit state=\(state)" +
            (reason.map { " reason=\($0)" } ?? "") +
            " caller=\(caller.rawValue) generation=\(generation)\n",
            enabled: graceEnabled)
    }

    private func traceOutcome(_ reply: GPUWorkerTransportReply) -> String {
        switch reply {
        case .response: return "response"
        case .timeout: return "timeout"
        case .exit: return "exit"
        case .crash: return "crash"
        case .protocolMismatch: return "protocol_mismatch"
        case .nativeFailure: return "native_failure"
        }
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
            autoRetryGeneration = nil
            let streak = consecutiveTimeouts
            let mustTerminate = terminatedGeneration != generation
            terminatedGeneration = generation
            transportGeneration = nil
            stateLock.unlock()
            logEvent("ev=zenzai_worker_quarantine reason=\(quarantineReason.rawValue) " +
                     "generation=\(expectedGeneration) streak=\(streak) action=terminate\n")
            if mustTerminate {
                logEvent("ev=zenzai_worker_terminate trigger=quarantine " +
                         "reason=\(quarantineReason.rawValue) generation=\(expectedGeneration)\n")
                transport.terminate()
            }
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
        case .retryPending:
            // A respawn is already scheduled: report preparing instead of the
            // old timeout reason so the UI does not show a stale latch.
            return GPUWorkerSupervisorSnapshot(state: .preparing, backend: nil, device: nil)
        case .probation:
            return GPUWorkerSupervisorSnapshot(state: .gpuActive, backend: backend, device: device)
        case .liveSuppressed:
            // Convert-tier requests still use the worker; only live
            // enhancement is suppressed.
            return GPUWorkerSupervisorSnapshot(state: .gpuActive, backend: backend, device: device)
        case .ready:
            return GPUWorkerSupervisorSnapshot(state: .gpuActive, backend: backend, device: device)
        case .quarantined:
            return GPUWorkerSupervisorSnapshot(state: .classic, backend: backend, device: device, reason: reason)
        case .disabled:
            return GPUWorkerSupervisorSnapshot(state: .disabled)
        }
    }
}

/// Per-tier latency samples with a batched summary line.  Success timings are
/// the evidence for (or against) "the live budget is below the on-device
/// P95", which single failure logs can never show.
private final class GPUWorkerLatencyTracker: @unchecked Sendable {
    private let lock = NSLock()
    private var liveSamples: [Double] = []
    private var convertSamples: [Double] = []

    func record(caller: GPUWorkerCaller, elapsedMs: Double, emit: (String) -> Void) {
        lock.lock()
        if caller.isLiveTier {
            liveSamples.append(elapsedMs)
        } else {
            convertSamples.append(elapsedMs)
        }
        var summaries: [String] = []
        if liveSamples.count >= GPUWorkerSupervisor.latencySummaryBatch {
            summaries.append(Self.summaryLine(tier: "live", samples: liveSamples))
            liveSamples = []
        }
        if convertSamples.count >= GPUWorkerSupervisor.latencySummaryBatch {
            summaries.append(Self.summaryLine(tier: "convert", samples: convertSamples))
            convertSamples = []
        }
        lock.unlock()
        summaries.forEach(emit)
    }

    private static func summaryLine(tier: String, samples: [Double]) -> String {
        let sorted = samples.sorted()
        func percentile(_ fraction: Double) -> Double {
            let index = Int((Double(sorted.count) - 1) * fraction)
            return sorted[max(0, min(sorted.count - 1, index))]
        }
        return "ev=zenzai_worker_latency_summary caller=\(tier) count=\(sorted.count) " +
            "p50_ms=\(String(format: "%.1f", percentile(0.5))) " +
            "p95_ms=\(String(format: "%.1f", percentile(0.95))) " +
            "max_ms=\(String(format: "%.1f", sorted.last ?? 0))\n"
    }
}
