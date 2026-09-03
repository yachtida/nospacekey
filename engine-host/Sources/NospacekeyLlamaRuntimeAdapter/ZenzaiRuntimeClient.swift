import Foundation

#if os(Windows)
import NospacekeyLlamaRuntime
#endif

/// Failure categories exported by the patched llama runtime.
public enum ZenzaiRuntimeFailure: UInt32, Equatable, Sendable {
    case none = 0
    case invalidRuntimeDirectory = 1
    case backendPathRejected = 2
    case backendUnavailable = 3
    case gpuUnavailable = 4
    case modelLoad = 5
    case contextLoad = 6
    case decode = 7
    case unknown = 4_294_967_295
}

/// Sanitized status returned by the C runtime seam. It deliberately contains no paths,
/// input text, candidate text, or other request data.
public struct ZenzaiRuntimeStatus: Equatable, Sendable {
    public enum State: Equatable, Sendable {
        case unconfigured
        case gpuActive
        case failed
    }

    public let state: State
    public let failure: ZenzaiRuntimeFailure
    public let generation: UInt64
    public let modelLoadAttempts: UInt64
    public let contextInitAttempts: UInt64
    public let decodeAttempts: UInt64
    public let backend: String
    public let device: String

    public init(
        state: State,
        failure: ZenzaiRuntimeFailure = .none,
        generation: UInt64 = 0,
        modelLoadAttempts: UInt64 = 0,
        contextInitAttempts: UInt64 = 0,
        decodeAttempts: UInt64 = 0,
        backend: String = "",
        device: String = ""
    ) {
        self.state = state
        self.failure = failure
        self.generation = generation
        self.modelLoadAttempts = modelLoadAttempts
        self.contextInitAttempts = contextInitAttempts
        self.decodeAttempts = decodeAttempts
        self.backend = backend
        self.device = device
    }

    public static let unconfigured = ZenzaiRuntimeStatus(state: .unconfigured)
}

/// Public seam used by ConversionService. Tests can inject a deterministic client without
/// loading native DLLs or touching the converter's private runtime state.
public protocol ZenzaiRuntimeClient: AnyObject, Sendable {
    func configure(trustedRuntimeDirectory: URL, explicitRetry: Bool) -> ZenzaiRuntimeStatus
    func status() -> ZenzaiRuntimeStatus
}

/// Production implementation of the runtime seam. Native calls are kept in this adapter so
/// ConversionService remains mockable and does not know the C ABI layout.
public final class NativeZenzaiRuntimeClient: ZenzaiRuntimeClient, @unchecked Sendable {
    public init() {}

    public func configure(trustedRuntimeDirectory: URL, explicitRetry: Bool) -> ZenzaiRuntimeStatus {
#if os(Windows)
        return trustedRuntimeDirectory.path.withCString { path in
            withStatus(operation: .configure) { status in
                nsk_llama_runtime_configure(path, explicitRetry ? 1 : 0, status)
            }
        }
#else
        return .unconfigured
#endif
    }

    public func status() -> ZenzaiRuntimeStatus {
#if os(Windows)
        return withStatus(operation: .status) { status in
            nsk_llama_runtime_status(status)
        }
#else
        return .unconfigured
#endif
    }

#if os(Windows)
    private enum Operation {
        case configure
        case status
    }

    private static let expectedABIVersion: UInt32 = 1

    private func withStatus(
        operation: Operation,
        _ body: (UnsafeMutablePointer<nsk_llama_runtime_status>) -> Int32
    ) -> ZenzaiRuntimeStatus {
        let raw = UnsafeMutableRawPointer.allocate(
            byteCount: MemoryLayout<nsk_llama_runtime_status>.size,
            alignment: MemoryLayout<nsk_llama_runtime_status>.alignment)
        raw.initializeMemory(as: UInt8.self, repeating: 0,
                             count: MemoryLayout<nsk_llama_runtime_status>.size)
        defer { raw.deallocate() }

        let status = raw.assumingMemoryBound(to: nsk_llama_runtime_status.self)
        // Seed the negotiation fields before entering the ABI. The patched runtime
        // overwrites them, while an incompatible DLL cannot be mistaken for a valid one.
        status.pointee.abi_version = Self.expectedABIVersion
        status.pointee.struct_size = UInt32(MemoryLayout<nsk_llama_runtime_status>.size)
        let result = body(status)
        let value = status.pointee
        return Self.validatedStatus(
            operation: operation,
            result: result,
            abiVersion: value.abi_version,
            structSize: value.struct_size,
            stateRaw: value.state,
            failureRaw: value.failure,
            generation: value.generation,
            modelLoadAttempts: value.model_load_attempts,
            contextInitAttempts: value.context_init_attempts,
            decodeAttempts: value.decode_attempts,
            backend: string(from: value.backend),
            device: string(from: value.device))
    }

    private static func validatedStatus(
        operation: Operation,
        result: Int32,
        abiVersion: UInt32,
        structSize: UInt32,
        stateRaw: UInt32,
        failureRaw: UInt32,
        generation: UInt64,
        modelLoadAttempts: UInt64,
        contextInitAttempts: UInt64,
        decodeAttempts: UInt64,
        backend: String,
        device: String
    ) -> ZenzaiRuntimeStatus {
        let expectedSize = UInt32(MemoryLayout<nsk_llama_runtime_status>.size)
        guard abiVersion == Self.expectedABIVersion, structSize == expectedSize else {
            return .init(state: .failed, failure: .unknown)
        }
        let state: ZenzaiRuntimeStatus.State
        switch stateRaw {
        case 1: state = .gpuActive
        case 2: state = .failed
        case 0: state = .unconfigured
        default: return .init(state: .failed, failure: .unknown)
        }
        let decoded = ZenzaiRuntimeStatus(
            state: state,
            failure: ZenzaiRuntimeFailure(rawValue: failureRaw) ?? .unknown,
            generation: generation,
            modelLoadAttempts: modelLoadAttempts,
            contextInitAttempts: contextInitAttempts,
            decodeAttempts: decodeAttempts,
            backend: backend,
            device: device)
        switch operation {
        case .configure:
            let validResult = (result == 0 && decoded.state == .gpuActive && decoded.failure == .none) ||
                (result != 0 && decoded.state == .failed && decoded.failure != .none)
            return validResult ? decoded : .init(state: .failed, failure: .unknown)
        case .status:
            let stateFailureIsConsistent: Bool
            switch decoded.state {
            case .unconfigured, .gpuActive:
                stateFailureIsConsistent = decoded.failure == .none
            case .failed:
                stateFailureIsConsistent = decoded.failure != .none
            }
            return result == 0 && stateFailureIsConsistent
                ? decoded
                : .init(state: .failed, failure: .unknown)
        }
    }

    #if DEBUG
    static func validateConfigureResultForTesting(
        result: Int32,
        abiVersion: UInt32,
        structSize: UInt32,
        stateRaw: UInt32,
        failureRaw: UInt32
    ) -> ZenzaiRuntimeStatus {
        validatedStatus(
            operation: .configure,
            result: result,
            abiVersion: abiVersion,
            structSize: structSize,
            stateRaw: stateRaw,
            failureRaw: failureRaw,
            generation: 0,
            modelLoadAttempts: 0,
            contextInitAttempts: 0,
            decodeAttempts: 0,
            backend: "",
            device: "")
    }

    static var expectedStructSizeForTesting: UInt32 {
        UInt32(MemoryLayout<nsk_llama_runtime_status>.size)
    }

    static func validateStatusResultForTesting(
        result: Int32,
        abiVersion: UInt32,
        structSize: UInt32,
        stateRaw: UInt32,
        failureRaw: UInt32
    ) -> ZenzaiRuntimeStatus {
        validatedStatus(
            operation: .status,
            result: result,
            abiVersion: abiVersion,
            structSize: structSize,
            stateRaw: stateRaw,
            failureRaw: failureRaw,
            generation: 0,
            modelLoadAttempts: 0,
            contextInitAttempts: 0,
            decodeAttempts: 0,
            backend: "",
            device: "")
    }
    #endif

    private func string<T>(from tuple: T) -> String {
        var value = tuple
        return withUnsafePointer(to: &value) { pointer in
            pointer.withMemoryRebound(to: CChar.self, capacity: MemoryLayout<T>.size) {
                let bytes = UnsafeBufferPointer(start: $0, count: MemoryLayout<T>.size)
                    .prefix { $0 != 0 }
                    .map { UInt8(bitPattern: $0) }
                return String(decoding: bytes, as: UTF8.self)
            }
        }
    }
#endif
}
