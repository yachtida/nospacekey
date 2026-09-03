import Foundation
@_exported import NospacekeyLlamaRuntimeAdapter

/// The engine-level state of the GPU-required Zenzai path.
public enum ZenzaiRuntimeState: Equatable, Sendable {
    case classic(reason: ZenzaiClassicReason)
    case probing
    case warming
    case gpuActive(device: String)
}

/// Sanitized state intended for the settings UI. It deliberately has no model path,
/// input, candidate, generation, or attempt-counter fields.
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

    public init(state: DisplayState, backend: String? = nil, device: String? = nil,
                reason: String? = nil) {
        self.state = state
        self.backend = backend
        self.device = device
        self.reason = reason
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
