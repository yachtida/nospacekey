import Foundation
import KanaKanjiConverterModuleWithDefaultDictionary
import NospacekeyLlamaRuntimeAdapter

/// Canonical, validated configuration handed to one worker generation.  It is
/// kept private to supervisor/transport operations and never appears in the
/// public status snapshot or diagnostics.
public struct GPUWorkerRuntimeConfiguration: Equatable, Sendable {
    public let modelURL: URL
    public let runtimeDirectory: URL
    public let inferenceLimit: Int

    public init?(config: ZenzaiConfig) {
        guard let modelURL = config.weightURL,
              let runtimeDirectory = config.runtimeDirectory else { return nil }
        self.init(modelURL: modelURL, runtimeDirectory: runtimeDirectory,
                  inferenceLimit: config.inferenceLimit)
    }

    public init?(modelURL: URL, runtimeDirectory: URL, inferenceLimit: Int) {
        guard modelURL.isFileURL, runtimeDirectory.isFileURL,
              !modelURL.path.isEmpty, !runtimeDirectory.path.isEmpty,
              inferenceLimit > 0 else { return nil }
        let fileManager = FileManager.default
        let canonicalModel = modelURL.resolvingSymlinksInPath().standardizedFileURL
        let canonicalRuntime = runtimeDirectory.resolvingSymlinksInPath().standardizedFileURL
        var isDirectory = ObjCBool(false)
        guard fileManager.fileExists(atPath: canonicalModel.path, isDirectory: nil),
              fileManager.fileExists(atPath: canonicalRuntime.path, isDirectory: &isDirectory),
              isDirectory.boolValue else { return nil }
        self.modelURL = canonicalModel
        self.runtimeDirectory = canonicalRuntime
        self.inferenceLimit = inferenceLimit
    }

    var wire: GPUWorkerRuntimeConfigurationWire {
        GPUWorkerRuntimeConfigurationWire(
            modelPath: modelURL.path, runtimeDirectory: runtimeDirectory.path,
            inferenceLimit: inferenceLimit)
    }
}

public struct GPUWorkerRuntimeConfigurationWire: Codable, Equatable, Sendable {
    public let modelPath: String
    public let runtimeDirectory: String
    public let inferenceLimit: Int

    public init(modelPath: String, runtimeDirectory: String, inferenceLimit: Int) {
        self.modelPath = modelPath
        self.runtimeDirectory = runtimeDirectory
        self.inferenceLimit = inferenceLimit
    }

    public func makeConfiguration() -> GPUWorkerRuntimeConfiguration? {
        GPUWorkerRuntimeConfiguration(
            modelURL: URL(fileURLWithPath: modelPath),
            runtimeDirectory: URL(fileURLWithPath: runtimeDirectory),
            inferenceLimit: inferenceLimit)
    }
}

/// A lossless, deliberately small representation of ComposingText for the GPU
/// worker boundary.  Candidate and DicdataElement never cross this boundary.
public struct GPUWorkerCompositionSnapshot: Codable, Equatable, Sendable {
    public let cursor: Int
    public let input: [GPUWorkerInputElement]
    public let convertTarget: String

    public init(cursor: Int, input: [GPUWorkerInputElement], convertTarget: String) {
        self.cursor = cursor
        self.input = input
        self.convertTarget = convertTarget
    }

    public init(_ composingText: ComposingText) {
        self.cursor = composingText.convertTargetCursorPosition
        self.input = composingText.input.map(GPUWorkerInputElement.init)
        self.convertTarget = composingText.convertTarget
    }

    /// Input mapped by an arbitrary user table cannot be reproduced by the
    /// worker without copying the user's table.  Such requests stay classic.
    public var supportsGPUWorker: Bool {
        input.allSatisfy { $0.inputStyle.supportsGPUWorker }
    }

    public func makeComposingText() throws -> ComposingText {
        guard cursor >= 0, cursor <= convertTarget.count else {
            throw GPUWorkerProtocolError.invalidComposition
        }
        let elements = try input.map { try $0.makeInputElement() }
        // Custom tables are intentionally never admitted to the worker, so
        // their exact input remains representable without requiring the
        // worker to have the user's table installed.  Built-in styles must be
        // internally consistent before crossing the process boundary.
        if supportsGPUWorker {
            var reconstructed = ComposingText()
            reconstructed.insertAtCursorPosition(elements)
            guard reconstructed.convertTarget == convertTarget else {
                throw GPUWorkerProtocolError.invalidComposition
            }
            _ = reconstructed.moveCursorFromCursorPosition(
                count: cursor - reconstructed.convertTargetCursorPosition)
            guard reconstructed.convertTargetCursorPosition == cursor else {
                throw GPUWorkerProtocolError.invalidComposition
            }
            return reconstructed
        }
        return ComposingText(
            convertTargetCursorPosition: cursor, input: elements, convertTarget: convertTarget)
    }
}

public struct GPUWorkerInputElement: Codable, Equatable, Sendable {
    public let piece: GPUWorkerInputPiece
    public let inputStyle: GPUWorkerInputStyle

    public init(piece: GPUWorkerInputPiece, inputStyle: GPUWorkerInputStyle) {
        self.piece = piece
        self.inputStyle = inputStyle
    }

    public init(_ element: ComposingText.InputElement) {
        self.piece = GPUWorkerInputPiece(element.piece)
        self.inputStyle = GPUWorkerInputStyle(element.inputStyle)
    }

    public func makeInputElement() throws -> ComposingText.InputElement {
        ComposingText.InputElement(
            piece: try piece.makeInputPiece(),
            inputStyle: try inputStyle.makeInputStyle())
    }
}

public enum GPUWorkerInputPiece: Codable, Equatable, Sendable {
    case character(String)
    case compositionSeparator
    case key(intention: String?, modifiers: [GPUWorkerModifier])

    public init(_ piece: InputPiece) {
        switch piece {
        case .character(let character):
            self = .character(String(character))
        case .compositionSeparator:
            self = .compositionSeparator
        case .key(let intention, let modifiers):
            self = .key(
                intention: intention.map(String.init),
                modifiers: modifiers.map(GPUWorkerModifier.init).sorted())
        }
    }

    public func makeInputPiece() throws -> InputPiece {
        switch self {
        case .character(let value):
            guard value.count == 1, let character = value.first else {
                throw GPUWorkerProtocolError.invalidComposition
            }
            return .character(character)
        case .compositionSeparator:
            return .compositionSeparator
        case .key(let intention, let modifiers):
            let character: Character?
            if let intention {
                guard intention.count == 1, let value = intention.first else {
                    throw GPUWorkerProtocolError.invalidComposition
                }
                character = value
            } else {
                character = nil
            }
            var nativeModifiers = Set<InputPiece.Modifier>()
            for modifier in modifiers {
                switch modifier {
                case .shift: nativeModifiers.insert(.shift)
                }
            }
            return .key(intention: character, modifiers: nativeModifiers)
        }
    }
}

public enum GPUWorkerModifier: String, Codable, Equatable, Hashable, Sendable, Comparable {
    case shift

    public init(_ modifier: InputPiece.Modifier) {
        switch modifier { case .shift: self = .shift }
    }

    public static func < (lhs: Self, rhs: Self) -> Bool {
        lhs.rawValue < rhs.rawValue
    }
}

public enum GPUWorkerInputStyle: Codable, Equatable, Sendable {
    case direct
    case roman2kana
    case mapped(id: GPUWorkerInputTableID)

    public init(_ style: InputStyle) {
        switch style {
        case .direct: self = .direct
        case .roman2kana: self = .roman2kana
        case .mapped(let id): self = .mapped(id: GPUWorkerInputTableID(id))
        }
    }

    public var supportsGPUWorker: Bool {
        switch self {
        case .direct, .roman2kana: return true
        case .mapped(let id): return id.supportsGPUWorker
        }
    }

    public func makeInputStyle() throws -> InputStyle {
        switch self {
        case .direct: return .direct
        case .roman2kana: return .roman2kana
        case .mapped(let id): return .mapped(id: try id.makeInputTableID())
        }
    }
}

/// InputTableID is not Codable in the upstream converter.  Keep every case in
/// the wire type so snapshots are lossless, while rejecting custom tables at
/// the worker admission seam.
public enum GPUWorkerInputTableID: Codable, Equatable, Sendable {
    case defaultRomanToKana
    case defaultAZIK
    case defaultKanaJIS
    case defaultKanaUS
    case empty
    case custom(String)
    case tableName(String)

    public init(_ id: InputTableID) {
        switch id {
        case .defaultRomanToKana: self = .defaultRomanToKana
        case .defaultAZIK: self = .defaultAZIK
        case .defaultKanaJIS: self = .defaultKanaJIS
        case .defaultKanaUS: self = .defaultKanaUS
        case .empty: self = .empty
        case .custom(let url): self = .custom(url.absoluteString)
        case .tableName(let name): self = .tableName(name)
        }
    }

    public var supportsGPUWorker: Bool {
        switch self {
        case .custom, .tableName: return false
        default: return true
        }
    }

    public func makeInputTableID() throws -> InputTableID {
        switch self {
        case .defaultRomanToKana: return .defaultRomanToKana
        case .defaultAZIK: return .defaultAZIK
        case .defaultKanaJIS: return .defaultKanaJIS
        case .defaultKanaUS: return .defaultKanaUS
        case .empty: return .empty
        case .custom(let value):
            guard let url = URL(string: value) else {
                throw GPUWorkerProtocolError.invalidComposition
            }
            return .custom(url)
        case .tableName(let value): return .tableName(value)
        }
    }
}

public enum GPUWorkerProtocolError: Error, Equatable, Sendable {
    case invalidComposition
    case unsupportedInputStyle
    case invalidRequest
    case invalidCandidate
}

public struct GPUWorkerRequest: Codable, Equatable, Sendable {
    public static let currentVersion: UInt32 = 4
    public let version: UInt32
    public let operation: GPUWorkerOperation
    public let snapshot: GPUWorkerCompositionSnapshot
    public let leftContext: String?
    public let nBest: Int
    public let inferenceLimit: Int
    public let requestID: UInt64
    public let generation: UInt64

    public init(
        operation: GPUWorkerOperation = .rank,
        snapshot: GPUWorkerCompositionSnapshot,
        leftContext: String?,
        nBest: Int,
        inferenceLimit: Int,
        requestID: UInt64,
        generation: UInt64,
        version: UInt32 = GPUWorkerRequest.currentVersion
    ) {
        self.version = version
        self.operation = operation
        self.snapshot = snapshot
        self.leftContext = leftContext
        self.nBest = nBest
        self.inferenceLimit = inferenceLimit
        self.requestID = requestID
        self.generation = generation
    }
}

public enum GPUWorkerOperation: String, Codable, Equatable, Sendable {
    case rank
    case handshake
}

public struct GPUWorkerHandshakeRequest: Codable, Equatable, Sendable {
    public let version: UInt32
    public let generation: UInt64
    public let configuration: GPUWorkerRuntimeConfigurationWire?

    public init(generation: UInt64,
                configuration: GPUWorkerRuntimeConfigurationWire? = nil,
                version: UInt32 = GPUWorkerRequest.currentVersion) {
        self.version = version
        self.generation = generation
        self.configuration = configuration
    }
}

public struct GPUWorkerHandshakeResponse: Codable, Equatable, Sendable {
    public let version: UInt32
    public let generation: UInt64
    public let ready: Bool
    public let backend: String?
    public let device: String?
    /// Wire mirror of the native runtime failure enum.  The adapter's enum is
    /// intentionally not made Codable just for this boundary.
    public let failure: GPUWorkerRuntimeFailure?

    public init(generation: UInt64, ready: Bool, backend: String? = nil,
                device: String? = nil, failure: GPUWorkerRuntimeFailure? = nil,
                version: UInt32 = GPUWorkerRequest.currentVersion) {
        self.version = version
        self.generation = generation
        self.ready = ready
        self.backend = backend
        self.device = device
        self.failure = failure
    }
}

public enum GPUWorkerRuntimeFailure: UInt32, Codable, Equatable, Sendable {
    case none = 0
    case invalidRuntimeDirectory = 1
    case backendPathRejected = 2
    case backendUnavailable = 3
    case gpuUnavailable = 4
    case modelLoad = 5
    case contextLoad = 6
    case decode = 7
    case unknown = 4_294_967_295

    public init(_ failure: ZenzaiRuntimeFailure) {
        self = Self(rawValue: failure.rawValue) ?? .unknown
    }

    public var zenzaiFailure: ZenzaiRuntimeFailure {
        ZenzaiRuntimeFailure(rawValue: rawValue) ?? .unknown
    }
}

/// Codable mirrors for the converter's non-Codable candidate graph.  These
/// types deliberately contain only public converter fields; the native
/// package remains untouched and the process boundary can therefore be
/// versioned independently.
public indirect enum GPUWorkerComposingCount: Codable, Equatable, Sendable {
    case inputCount(Int)
    case surfaceCount(Int)
    case composite(lhs: GPUWorkerComposingCount, rhs: GPUWorkerComposingCount)

    private enum CodingKeys: String, CodingKey {
        case kind
        case value
        case lhs
        case rhs
    }

    private enum Kind: String, Codable {
        case inputCount
        case surfaceCount
        case composite
    }

    private static let maxDepth = 64

    public init(_ count: ComposingCount) {
        switch count {
        case .inputCount(let value):
            self = .inputCount(value)
        case .surfaceCount(let value):
            self = .surfaceCount(value)
        case .composite(let lhs, let rhs):
            self = .composite(lhs: Self(lhs), rhs: Self(rhs))
        }
    }

    public init(from decoder: Decoder) throws {
        self = try Self.decode(from: decoder, depth: 0)
    }

    private static func decode(from decoder: Decoder, depth: Int) throws -> Self {
        guard depth <= maxDepth else {
            throw DecodingError.dataCorruptedError(
                in: try decoder.singleValueContainer(),
                debugDescription: "composingCount nesting is too deep")
        }
        let container = try decoder.container(keyedBy: CodingKeys.self)
        switch try container.decode(Kind.self, forKey: .kind) {
        case .inputCount:
            return .inputCount(try container.decode(Int.self, forKey: .value))
        case .surfaceCount:
            return .surfaceCount(try container.decode(Int.self, forKey: .value))
        case .composite:
            return .composite(
                lhs: try decode(from: container.superDecoder(forKey: .lhs), depth: depth + 1),
                rhs: try decode(from: container.superDecoder(forKey: .rhs), depth: depth + 1))
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .inputCount(let value):
            try container.encode(Kind.inputCount, forKey: .kind)
            try container.encode(value, forKey: .value)
        case .surfaceCount(let value):
            try container.encode(Kind.surfaceCount, forKey: .kind)
            try container.encode(value, forKey: .value)
        case .composite(let lhs, let rhs):
            try container.encode(Kind.composite, forKey: .kind)
            try container.encode(lhs, forKey: .lhs)
            try container.encode(rhs, forKey: .rhs)
        }
    }

    public func makeComposingCount() throws -> ComposingCount {
        try makeComposingCount(depth: 0).count
    }

    func canApply(to composingText: ComposingText) -> Bool {
        var prefix = composingText.prefixToCursorPosition()
        return apply(to: &prefix, depth: 0)
    }

    private func apply(to composingText: inout ComposingText, depth: Int) -> Bool {
        guard depth <= Self.maxDepth else { return false }
        switch self {
        case .inputCount(let value):
            guard Self.validCount(value), value <= composingText.input.count else { return false }
            composingText.prefixComplete(composingCount: .inputCount(value))
            return true
        case .surfaceCount(let value):
            guard Self.validCount(value), value <= composingText.convertTarget.count else { return false }
            composingText.prefixComplete(composingCount: .surfaceCount(value))
            return true
        case .composite(let lhs, let rhs):
            return lhs.apply(to: &composingText, depth: depth + 1)
                && rhs.apply(to: &composingText, depth: depth + 1)
        }
    }

    private func makeComposingCount(depth: Int) throws -> (count: ComposingCount, total: Int) {
        guard depth <= Self.maxDepth else { throw GPUWorkerProtocolError.invalidCandidate }
        switch self {
        case .inputCount(let value):
            guard Self.validCount(value) else { throw GPUWorkerProtocolError.invalidCandidate }
            return (.inputCount(value), value)
        case .surfaceCount(let value):
            guard Self.validCount(value) else { throw GPUWorkerProtocolError.invalidCandidate }
            return (.surfaceCount(value), value)
        case .composite(let lhs, let rhs):
            let left = try lhs.makeComposingCount(depth: depth + 1)
            let right = try rhs.makeComposingCount(depth: depth + 1)
            let (total, overflow) = left.total.addingReportingOverflow(right.total)
            guard !overflow, Self.validCount(total) else {
                throw GPUWorkerProtocolError.invalidCandidate
            }
            return (.composite(lhs: left.count, rhs: right.count), total)
        }
    }

    private static func validCount(_ value: Int) -> Bool {
        value >= 0 && value <= GPUWorkerWireLimits.maxCount
    }
}

public enum GPUWorkerCandidateAction: Codable, Equatable, Sendable {
    case moveCursor(Int)

    public init(_ action: CompleteAction) {
        switch action {
        case .moveCursor(let count): self = .moveCursor(count)
        }
    }

    public func makeAction() throws -> CompleteAction {
        switch self {
        case .moveCursor(let count):
            guard count >= -GPUWorkerWireLimits.maxCount,
                  count <= GPUWorkerWireLimits.maxCount else {
                throw GPUWorkerProtocolError.invalidCandidate
            }
            return .moveCursor(count)
        }
    }
}

/// Limits below are intentionally below the existing named-pipe frame caps.
/// They keep malformed values from turning a validly framed response into an
/// unbounded native object while leaving normal long-input candidates intact.
private enum GPUWorkerWireLimits {
    static let maxCount = 1_000_000
    static let maxID = 1_000_000_000
    static let maxStringBytes = 1_048_576
    static let maxArrayCount = 16_384
    static let maxAbsValue: Float32 = 1_000_000_000
}

public struct GPUWorkerDicdataElement: Codable, Equatable, Sendable {
    public let word: String
    public let ruby: String
    public let lcid: Int
    public let rcid: Int
    public let mid: Int
    /// Effective value (`DicdataElement.value()`) at the worker boundary.
    public let value: Float32
    public let metadataRawValue: UInt32

    public var effectiveValue: Float32 { value }
    public var metadata: UInt32 { metadataRawValue }

    public init(
        word: String,
        ruby: String,
        lcid: Int,
        rcid: Int,
        mid: Int,
        value: Float32,
        metadataRawValue: UInt32 = 0
    ) {
        self.word = word
        self.ruby = ruby
        self.lcid = lcid
        self.rcid = rcid
        self.mid = mid
        self.value = value
        self.metadataRawValue = metadataRawValue
    }

    public init(
        word: String,
        ruby: String,
        lcid: Int,
        rcid: Int,
        mid: Int,
        effectiveValue: Float32,
        metadataRawValue: UInt32 = 0
    ) {
        self.init(word: word, ruby: ruby, lcid: lcid, rcid: rcid, mid: mid,
                  value: effectiveValue, metadataRawValue: metadataRawValue)
    }

    public init(_ element: DicdataElement) {
        self.init(
            word: element.word, ruby: element.ruby, lcid: element.lcid,
            rcid: element.rcid, mid: element.mid, value: element.value(),
            metadataRawValue: element.metadata.rawValue)
    }

    public func makeDicdataElement() throws -> DicdataElement {
        guard word.utf8.count <= GPUWorkerWireLimits.maxStringBytes,
              ruby.utf8.count <= GPUWorkerWireLimits.maxStringBytes,
              Self.validID(lcid), Self.validID(rcid), Self.validID(mid),
              value.isFinite, value <= 0,
              abs(value) <= GPUWorkerWireLimits.maxAbsValue else {
            throw GPUWorkerProtocolError.invalidCandidate
        }
        // baseValue is package-private.  Reusing the public effective value as
        // base with adjust=0 preserves `value()` without editing the vendor.
        return DicdataElement(
            word: word, ruby: ruby, lcid: lcid, rcid: rcid, mid: mid,
            value: value, adjust: .zero,
            metadata: DicdataElementMetadata(rawValue: metadataRawValue))
    }

    public func makeElement() throws -> DicdataElement {
        try makeDicdataElement()
    }

    private static func validID(_ value: Int) -> Bool {
        value >= 0 && value <= GPUWorkerWireLimits.maxID
    }
}

public struct GPUWorkerCandidate: Codable, Equatable, Sendable {
    public let text: String
    public let value: Float32
    public let composingCount: GPUWorkerComposingCount
    public let lastMid: Int
    public let data: [GPUWorkerDicdataElement]
    public let actions: [GPUWorkerCandidateAction]
    public let inputable: Bool
    public let isLearningTarget: Bool

    public init(
        text: String,
        value: Float32,
        composingCount: GPUWorkerComposingCount,
        lastMid: Int,
        data: [GPUWorkerDicdataElement],
        actions: [GPUWorkerCandidateAction] = [],
        inputable: Bool = true,
        isLearningTarget: Bool = true
    ) {
        self.text = text
        self.value = value
        self.composingCount = composingCount
        self.lastMid = lastMid
        self.data = data
        self.actions = actions
        self.inputable = inputable
        self.isLearningTarget = isLearningTarget
    }

    public init(_ candidate: Candidate) {
        self.init(
            text: candidate.text, value: candidate.value,
            composingCount: GPUWorkerComposingCount(candidate.composingCount),
            lastMid: candidate.lastMid,
            data: candidate.data.map(GPUWorkerDicdataElement.init),
            actions: candidate.actions.map(GPUWorkerCandidateAction.init),
            inputable: candidate.inputable,
            isLearningTarget: candidate.isLearningTarget)
    }

    public func makeCandidate() throws -> Candidate {
        guard text.utf8.count <= GPUWorkerWireLimits.maxStringBytes,
              Self.validID(lastMid), value.isFinite,
              abs(value) <= GPUWorkerWireLimits.maxAbsValue,
              data.count <= GPUWorkerWireLimits.maxArrayCount,
              actions.count <= GPUWorkerWireLimits.maxArrayCount else {
            throw GPUWorkerProtocolError.invalidCandidate
        }
        let nativeCount = try composingCount.makeComposingCount()
        let nativeData = try data.map { try $0.makeDicdataElement() }
        let nativeActions = try actions.map { try $0.makeAction() }
        return Candidate(
            text: text, value: value, composingCount: nativeCount,
            lastMid: lastMid, data: nativeData, actions: nativeActions,
            inputable: inputable, isLearningTarget: isLearningTarget)
    }

    private static func validID(_ value: Int) -> Bool {
        value >= 0 && value <= GPUWorkerWireLimits.maxID
    }
}

// Short aliases keep tests and future callers independent from the process
// role while retaining an explicit product-owned wire type.
public typealias GPUWorkerCandidateWire = GPUWorkerCandidate
public typealias GPUWorkerDicdataElementWire = GPUWorkerDicdataElement
public typealias GPUWorkerComposingCountWire = GPUWorkerComposingCount
public typealias GPUWorkerCandidateActionWire = GPUWorkerCandidateAction

public struct GPUWorkerResponse: Codable, Equatable, Sendable {
    public let version: UInt32
    public let requestID: UInt64
    public let generation: UInt64
    public let mainResults: [GPUWorkerCandidate]
    public let firstClauseResults: [GPUWorkerCandidate]
    public let failure: GPUWorkerFailure?

    public init(
        requestID: UInt64,
        generation: UInt64,
        mainResults: [GPUWorkerCandidate] = [],
        firstClauseResults: [GPUWorkerCandidate] = [],
        failure: GPUWorkerFailure? = nil,
        version: UInt32 = GPUWorkerRequest.currentVersion
    ) {
        self.version = version
        self.requestID = requestID
        self.generation = generation
        self.mainResults = mainResults
        self.firstClauseResults = firstClauseResults
        self.failure = failure
    }

    /// Source compatibility for test seams written before candidate wire
    /// fields were added.  Production worker responses always use the typed
    /// initializer above.
    public init(
        requestID: UInt64,
        generation: UInt64,
        mainResults: [String],
        firstClauseResults: [String] = [],
        failure: GPUWorkerFailure? = nil,
        version: UInt32 = GPUWorkerRequest.currentVersion
    ) {
        self.init(
            requestID: requestID, generation: generation,
            mainResults: mainResults.map(Self.legacyCandidate),
            firstClauseResults: firstClauseResults.map(Self.legacyCandidate),
            failure: failure, version: version)
    }

    private static func legacyCandidate(_ text: String) -> GPUWorkerCandidate {
        GPUWorkerCandidate(
            text: text, value: 0, composingCount: .inputCount(0), lastMid: 0,
            data: [GPUWorkerDicdataElement(
                word: text, ruby: text, lcid: 0, rcid: 0, mid: 0, value: 0)],
            isLearningTarget: false)
    }
}

public enum GPUWorkerFailure: String, Codable, Equatable, Sendable {
    case timeout
    case workerExit
    case crash
    case protocolMismatch
    case nativeFailure
    case unsupportedInput
    case unavailable
    case invalidRuntimeDirectory
    case backendPathRejected
    case backendUnavailable
    case gpuUnavailable
    case modelLoad
    case contextLoad
    case decode
    case warmup
}

public enum GPUWorkerQuarantineReason: String, Codable, Equatable, Sendable {
    case timeout
    case workerExit = "worker_exit"
    case crash
    case workerProtocol = "worker_protocol"
    case candidateMismatch = "candidate_mismatch"
    case nativeFailure = "native_failure"
    case invalidRuntimeDirectory = "invalid_runtime_directory"
    case backendPathRejected = "backend_path_rejected"
    case backendUnavailable = "backend_unavailable"
    case gpuUnavailable = "gpu_unavailable"
    case modelLoad = "model_load"
    case contextLoad = "context_load"
    case decode = "decode"
    case warmup = "warmup"
}

/// Compatibility aliases keep the wire vocabulary short for callers that do
/// not need to mention the process role.
public typealias CompositionSnapshot = GPUWorkerCompositionSnapshot
public typealias WorkerRequest = GPUWorkerRequest
public typealias WorkerResponse = GPUWorkerResponse

public enum GPUWorkerRerankFailure: String, Codable, Equatable, Sendable {
    case unknownCandidate
    case duplicateCandidate
    case protocolMismatch
    case timeout
    case workerExit
    case crash
    case workerProtocol
    case candidateMismatch
    case nativeFailure
    case unsupportedInput
    case invalidRuntimeDirectory
    case backendPathRejected
    case backendUnavailable
    case gpuUnavailable
    case modelLoad
    case contextLoad
    case decode
    case warmup

    static func from(_ failure: GPUWorkerFailure) -> Self {
        switch failure {
        case .timeout: return .timeout
        case .workerExit: return .workerExit
        case .crash: return .crash
        case .protocolMismatch: return .workerProtocol
        case .nativeFailure: return .nativeFailure
        case .unsupportedInput: return .unsupportedInput
        case .unavailable: return .nativeFailure
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
}

public struct GPUWorkerRerankDecision: Sendable {
    public let conversion: ConversionResult
    public let usedWorker: Bool
    public let failure: GPUWorkerRerankFailure?

    public init(conversion: ConversionResult, usedWorker: Bool, failure: GPUWorkerRerankFailure? = nil) {
        self.conversion = conversion
        self.usedWorker = usedWorker
        self.failure = failure
    }
}

/// Validate and materialize the worker's authoritative candidate arrays.  A
/// text that uniquely identifies a classic candidate reuses that exact object
/// so learning/correction metadata remains the main process' source of truth.
/// GPU-only candidates are reconstructed from the public wire fields, but are
/// made unlearnable before entering any main-process state.
public enum GPUWorkerReranker {
    public static func apply(
        response: GPUWorkerResponse,
        to classic: ConversionResult,
        requestID: UInt64,
        generation: UInt64,
        snapshot: GPUWorkerCompositionSnapshot? = nil
    ) -> GPUWorkerRerankDecision {
        guard response.version == GPUWorkerRequest.currentVersion,
              response.requestID == requestID,
              response.generation == generation else {
            return GPUWorkerRerankDecision(
                conversion: classic, usedWorker: false, failure: .protocolMismatch)
        }
        if let failure = response.failure {
            return GPUWorkerRerankDecision(
                conversion: classic, usedWorker: false,
                failure: GPUWorkerRerankFailure.from(failure))
        }
        let composingText: ComposingText?
        if let snapshot {
            guard let reconstructed = try? snapshot.makeComposingText() else {
                return GPUWorkerRerankDecision(
                    conversion: classic, usedWorker: false, failure: .protocolMismatch)
            }
            composingText = reconstructed
        } else {
            composingText = nil
        }
        guard let main = materialize(
                response.mainResults, classic: classic.mainResults,
                composingText: composingText),
              let first = materialize(
                response.firstClauseResults, classic: classic.firstClauseResults,
                composingText: composingText) else {
            let failure: GPUWorkerRerankFailure =
                hasDuplicate(response.mainResults) || hasDuplicate(response.firstClauseResults)
                ? .duplicateCandidate : .candidateMismatch
            return GPUWorkerRerankDecision(conversion: classic, usedWorker: false, failure: failure)
        }
        var conversion = classic
        conversion.mainResults = main
        conversion.firstClauseResults = first
        return GPUWorkerRerankDecision(
            conversion: conversion,
            usedWorker: true)
    }

    private static func hasDuplicate(_ candidates: [GPUWorkerCandidate]) -> Bool {
        let texts = candidates.map(\.text)
        return texts.count != Set(texts).count
    }

    private static func materialize(
        _ worker: [GPUWorkerCandidate], classic: [Candidate],
        composingText: ComposingText?
    ) -> [Candidate]? {
        guard !worker.isEmpty else { return classic }
        guard !hasDuplicate(worker), worker.count <= GPUWorkerWireLimits.maxArrayCount else {
            return nil
        }
        var indicesByText: [String: [Int]] = [:]
        for (index, candidate) in classic.enumerated() {
            indicesByText[candidate.text, default: []].append(index)
        }

        var result: [Candidate] = []
        result.reserveCapacity(worker.count)
        for wireCandidate in worker {
            // Validate every field even when text matches classic.  Otherwise
            // malformed JSON could hide behind a harmless-looking text key.
            guard let decoded = try? wireCandidate.makeCandidate(),
                  composingText.map({ wireCandidate.composingCount.canApply(to: $0) }) ?? true else {
                return nil
            }
            if let indices = indicesByText[wireCandidate.text] {
                // A text key is safe only when it identifies exactly one
                // classic object.  The object itself is reused deliberately.
                guard indices.count == 1 else { return nil }
                result.append(classic[indices[0]])
                continue
            }

            // GPU-only candidates have no trusted learning provenance.  Keep
            // their display/commit structure, but never let them enter
            // correction or persistent-learning paths.
            guard Self.isStructurallyConsistent(decoded) else { return nil }
            var safe = decoded
            safe.isLearningTarget = false
            result.append(safe)
        }
        return result
    }

    private static func isStructurallyConsistent(_ candidate: Candidate) -> Bool {
        guard !candidate.data.isEmpty else {
            // A non-empty GPU result without dictionary provenance cannot be
            // committed or decomposed safely.  Empty data is valid only for
            // the converter's empty sentinel.
            return candidate.text.isEmpty
        }
        // A template candidate's rendered text intentionally differs from its
        // raw data words.  Such a GPU-only candidate cannot safely be learned
        // or decomposed without the vendor's private template state.
        return candidate.data.map(\.word).joined() == candidate.text
    }
}
