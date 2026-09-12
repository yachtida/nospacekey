import Foundation
@_exported import NospacekeyLlamaRuntimeAdapter

/// The engine-level state of the GPU-required Zenzai path.
public enum ZenzaiRuntimeState: Equatable, Sendable {
    case classic(reason: ZenzaiClassicReason)
    case probing
    case warming
    case gpuActive(device: String)
}

/// Sanitized per-tier speed stats for the settings UI: aggregated numbers
/// only, never raw inputs. Percentiles cover successful requests; budget
/// misses are reported as a separate timeout count.  CodingKeys mirror the
/// Rust `ZenzaiLatencyTier` wire shape one-for-one.
public struct ZenzaiLatencyTierStats: Equatable, Sendable, Codable {
    public enum CodingKeys: String, CodingKey {
        case sampleCount = "count"
        case p50Ms = "p50_ms"
        case p95Ms = "p95_ms"
        case maxMs = "max_ms"
        case timeoutCount = "timeout_count"
    }

    public let sampleCount: Int
    public let p50Ms: Double
    public let p95Ms: Double
    public let maxMs: Double
    public let timeoutCount: Int

    public init(sampleCount: Int, p50Ms: Double, p95Ms: Double, maxMs: Double,
                timeoutCount: Int) {
        self.sampleCount = sampleCount
        self.p50Ms = p50Ms
        self.p95Ms = p95Ms
        self.maxMs = maxMs
        self.timeoutCount = timeoutCount
    }
}

/// Sanitized state intended for the settings UI. It deliberately has no model path,
/// input, candidate, or generation fields. Aggregated speed stats carry no
/// user content and are included so users can tune the inference limit
/// against observed latency.
public struct ZenzaiRuntimeSnapshot: Equatable, Sendable {
    public enum DisplayState: String, Equatable, Sendable {
        case disabled
        case preparing
        case gpuActive = "gpu_active"
        case classic
    }

    public let state: DisplayState
    public let backend: String?
    public let device: String?
    public let reason: String?
    public let liveLatency: ZenzaiLatencyTierStats?
    public let convertLatency: ZenzaiLatencyTierStats?

    public init(state: DisplayState, backend: String? = nil, device: String? = nil,
                reason: String? = nil, liveLatency: ZenzaiLatencyTierStats? = nil,
                convertLatency: ZenzaiLatencyTierStats? = nil) {
        self.state = state
        self.backend = backend
        self.device = device
        self.reason = reason
        self.liveLatency = liveLatency
        self.convertLatency = convertLatency
    }
}

/// Reasons that keep the engine on the classic converter path.
public enum ZenzaiClassicReason: Equatable, Sendable {
    case userDisabled
    case modelMissing
    case cpuUnsupported
    case notStarted
    case invalidRuntimeDirectory
    case backendPathRejected
    case backendUnavailable
    case gpuUnavailable
    case modelLoadFailed
    case contextLoadFailed
    case decodeFailed
    case warmupFailed
    case tooSlow
    case unknownRuntimeFailure
}

extension ZenzaiClassicReason: CustomStringConvertible {
    public var description: String {
        switch self {
        case .userDisabled: return "user_disabled"
        case .modelMissing: return "model_missing"
        case .cpuUnsupported: return "cpu_unsupported"
        case .notStarted: return "not_started"
        case .invalidRuntimeDirectory: return "invalid_runtime_directory"
        case .backendPathRejected: return "backend_path_rejected"
        case .backendUnavailable: return "backend_unavailable"
        case .gpuUnavailable: return "gpu_unavailable"
        case .modelLoadFailed: return "model_load"
        case .contextLoadFailed: return "context_load"
        case .decodeFailed: return "decode"
        case .warmupFailed: return "warmup"
        case .tooSlow: return "slow_inference"
        case .unknownRuntimeFailure: return "runtime_failure"
        }
    }
}

extension ZenzaiRuntimeStatus {
    var classicReason: ZenzaiClassicReason? {
        guard state == .failed || failure != .none else { return nil }
        switch failure {
        case .none: return .unknownRuntimeFailure
        case .invalidRuntimeDirectory: return .invalidRuntimeDirectory
        case .backendPathRejected: return .backendPathRejected
        case .backendUnavailable: return .backendUnavailable
        case .gpuUnavailable: return .gpuUnavailable
        case .modelLoad: return .modelLoadFailed
        case .contextLoad: return .contextLoadFailed
        case .decode: return .decodeFailed
        case .unknown: return .unknownRuntimeFailure
        }
    }
}
