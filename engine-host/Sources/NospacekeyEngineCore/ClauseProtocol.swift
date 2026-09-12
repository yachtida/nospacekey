import Foundation

func sameWireText(_ lhs: String, _ rhs: String) -> Bool {
    lhs.utf8.elementsEqual(rhs.utf8)
}

func sameWireText(_ lhs: String?, _ rhs: String?) -> Bool {
    switch (lhs, rhs) {
    case (.none, .none): true
    case (.some(let lhs), .some(let rhs)): sameWireText(lhs, rhs)
    default: false
    }
}

struct ClauseSnapshotIdentity: Codable, Equatable, Hashable, Sendable {
    let composition: UInt64
    let revision: UInt64
    let configuration_generation: UInt64
    let connection_generation: UInt64
}

struct SnapshotResponseKey: Codable, Equatable, Sendable {
    let composition: UInt64
    let revision: UInt64
    let configuration_generation: UInt64
    let connection_generation: UInt64
    let baseline: UInt64
    let conversion_revision: UInt64
    let request_id: UInt64
}

struct SnapshotClauseData: Codable, Equatable, Sendable {
    let reading: String
    let conversion_revision: UInt64
    let request_id: UInt64
    let clauses: [WireClause]
    let sentence_token: String?

    static func == (lhs: Self, rhs: Self) -> Bool {
        sameWireText(lhs.reading, rhs.reading) && lhs.conversion_revision == rhs.conversion_revision
            && lhs.request_id == rhs.request_id && lhs.clauses == rhs.clauses && sameWireText(lhs.sentence_token, rhs.sentence_token)
    }
    init(reading: String, conversion_revision: UInt64, request_id: UInt64, clauses: [WireClause], sentence_token: String?) {
        self.reading = reading; self.conversion_revision = conversion_revision; self.request_id = request_id
        self.clauses = clauses; self.sentence_token = sentence_token
    }
    private enum Keys: String, CodingKey { case reading, conversion_revision, request_id, clauses, sentence_token }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        reading = try c.decode(String.self, forKey: .reading)
        conversion_revision = try c.decode(UInt64.self, forKey: .conversion_revision)
        request_id = try c.decode(UInt64.self, forKey: .request_id)
        clauses = try c.decode([WireClause].self, forKey: .clauses)
        sentence_token = try c.decode(String?.self, forKey: .sentence_token)
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Keys.self)
        try c.encode(reading, forKey: .reading); try c.encode(conversion_revision, forKey: .conversion_revision)
        try c.encode(request_id, forKey: .request_id); try c.encode(clauses, forKey: .clauses)
        try c.encode(sentence_token, forKey: .sentence_token)
    }
}

struct ClauseRequestKey: Codable, Equatable, Hashable, Sendable {
    let identity: ClauseSnapshotIdentity
    let baseline: UInt64
    let conversion_revision: UInt64
    let clause_id: UInt64
    let request_id: UInt64
}

enum WireClauseState: String, Codable, Sendable { case reading = "Reading", converted = "Converted" }

struct WireClause: Codable, Equatable, Sendable {
    let id: UInt64
    let reading_start: UInt32
    let reading_end: UInt32
    let state: WireClauseState
    let surface: String
    let candidate_token: String?

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.id == rhs.id && lhs.reading_start == rhs.reading_start && lhs.reading_end == rhs.reading_end
            && lhs.state == rhs.state && sameWireText(lhs.surface, rhs.surface) && sameWireText(lhs.candidate_token, rhs.candidate_token)
    }

    private enum CodingKeys: String, CodingKey { case id, reading_start, reading_end, state, surface, candidate_token }
    init(id: UInt64, reading_start: UInt32, reading_end: UInt32, state: WireClauseState, surface: String, candidate_token: String?) {
        self.id = id; self.reading_start = reading_start; self.reading_end = reading_end
        self.state = state; self.surface = surface; self.candidate_token = candidate_token
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(UInt64.self, forKey: .id)
        reading_start = try c.decode(UInt32.self, forKey: .reading_start)
        reading_end = try c.decode(UInt32.self, forKey: .reading_end)
        state = try c.decode(WireClauseState.self, forKey: .state)
        surface = try c.decode(String.self, forKey: .surface)
        candidate_token = try c.decode(String?.self, forKey: .candidate_token)
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(id, forKey: .id); try c.encode(reading_start, forKey: .reading_start)
        try c.encode(reading_end, forKey: .reading_end); try c.encode(state, forKey: .state)
        try c.encode(surface, forKey: .surface); try c.encode(candidate_token, forKey: .candidate_token)
    }
}

struct ClauseRange: Codable, Equatable, Sendable {
    let id: UInt64
    let reading_start: UInt32
    let reading_end: UInt32
}

struct PrecedingSurface: Codable, Equatable, Sendable {
    let clause_id: UInt64
    let reading_start: UInt32
    let reading_end: UInt32
    let surface: String
    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.clause_id == rhs.clause_id && lhs.reading_start == rhs.reading_start && lhs.reading_end == rhs.reading_end
            && sameWireText(lhs.surface, rhs.surface)
    }
}

struct ClauseCandidate: Codable, Equatable, Sendable {
    let surface: String
    let token: String
    let reading_start: UInt32
    let reading_end: UInt32
    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.reading_start == rhs.reading_start && lhs.reading_end == rhs.reading_end
            && sameWireText(lhs.surface, rhs.surface) && sameWireText(lhs.token, rhs.token)
    }
}

enum ClauseValidationError: Error { case reading, range, duplicateID, surface, token }

struct ClauseCandidatesRequest: Codable, Equatable, Sendable {
    let key: ClauseRequestKey
    let reading: String
    let reading_start: UInt32
    let reading_end: UInt32
    let preceding_surfaces: [PrecedingSurface]

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.key == rhs.key && sameWireText(lhs.reading, rhs.reading) && lhs.reading_start == rhs.reading_start
            && lhs.reading_end == rhs.reading_end && lhs.preceding_surfaces == rhs.preceding_surfaces
    }

    func validate() throws {
        try ClauseCoordinates.validatePreceding(reading: reading, preceding: preceding_surfaces, target: reading_start)
        guard reading_start < reading_end, try ClauseCoordinates.legalBoundaries(reading).contains(reading_end),
              !preceding_surfaces.contains(where: { $0.clause_id == key.clause_id }) else { throw ClauseValidationError.range }
    }

    func validateCandidates(_ candidates: [ClauseCandidate]) throws {
        try validate()
        guard !candidates.isEmpty else { throw ClauseValidationError.surface }
        for candidate in candidates {
            guard candidate.reading_start == reading_start, candidate.reading_end == reading_end else { throw ClauseValidationError.range }
            guard !candidate.surface.isEmpty else { throw ClauseValidationError.surface }
            guard !candidate.token.isEmpty else { throw ClauseValidationError.token }
        }
    }
}

struct ConvertClausesRequest: Codable, Equatable, Sendable {
    let key: ClauseRequestKey
    let reading: String
    let clauses: [ClauseRange]
    let preceding_surfaces: [PrecedingSurface]

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.key == rhs.key && sameWireText(lhs.reading, rhs.reading) && lhs.clauses == rhs.clauses
            && lhs.preceding_surfaces == rhs.preceding_surfaces
    }

    func validate() throws {
        guard let first = clauses.first else { throw ClauseValidationError.range }
        try ClauseCoordinates.validatePreceding(reading: reading, preceding: preceding_surfaces, target: first.reading_start)
        guard first.id == key.clause_id else { throw ClauseValidationError.range }
        let boundaries = Set(try ClauseCoordinates.legalBoundaries(reading))
        var cursor = first.reading_start
        var ids = Set(preceding_surfaces.map(\.clause_id))
        for range in clauses {
            guard ids.insert(range.id).inserted else { throw ClauseValidationError.duplicateID }
            guard range.reading_start == cursor, range.reading_end > cursor,
                  boundaries.contains(range.reading_end) else { throw ClauseValidationError.range }
            cursor = range.reading_end
        }
    }

    func validateResult(_ result: [WireClause]) throws {
        try validate()
        guard clauses.count == result.count, zip(clauses, result).allSatisfy({
            $0.id == $1.id && $0.reading_start == $1.reading_start && $0.reading_end == $1.reading_end
        }) else { throw ClauseValidationError.range }
        try ClauseCoordinates.validate(reading: reading, clauses: result, start: clauses.first!.reading_start,
            end: clauses.last!.reading_end, text: result.map(\.surface).joined())
    }
}

enum ClauseUnavailableReason: String, Codable, Sendable {
    case invalidRequest = "InvalidRequest", expired = "Expired", disconnected = "Disconnected"
    case noCandidates = "NoCandidates", busy = "Busy"
}

struct ClauseCandidatesResult: Codable, Equatable, Sendable {
    let key: ClauseRequestKey
    let outcome: Outcome
    enum Outcome: Equatable, Sendable {
        case pending, ready([ClauseCandidate]), unavailable(ClauseUnavailableReason)
    }
    private enum Keys: String, CodingKey { case key, status, candidates, reason }
    init(key: ClauseRequestKey, outcome: Outcome) { self.key = key; self.outcome = outcome }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        key = try c.decode(ClauseRequestKey.self, forKey: .key)
        switch try c.decode(String.self, forKey: .status) {
        case "Pending": outcome = .pending
        case "Ready": outcome = .ready(try c.decode([ClauseCandidate].self, forKey: .candidates))
        case "Unavailable": outcome = .unavailable(try c.decode(ClauseUnavailableReason.self, forKey: .reason))
        default: throw DecodingError.dataCorruptedError(forKey: .status, in: c, debugDescription: "unknown clause status")
        }
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Keys.self)
        try c.encode(key, forKey: .key)
        switch outcome {
        case .pending: try c.encode("Pending", forKey: .status)
        case .ready(let candidates):
            try c.encode("Ready", forKey: .status); try c.encode(candidates, forKey: .candidates)
        case .unavailable(let reason):
            try c.encode("Unavailable", forKey: .status); try c.encode(reason, forKey: .reason)
        }
    }
}

struct ConvertClausesResult: Codable, Equatable, Sendable {
    let key: ClauseRequestKey
    let outcome: Outcome
    enum Outcome: Equatable, Sendable { case pending, ready([WireClause]), unavailable(ClauseUnavailableReason) }
    private enum Keys: String, CodingKey { case key, status, clauses, reason }
    init(key: ClauseRequestKey, outcome: Outcome) { self.key = key; self.outcome = outcome }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        key = try c.decode(ClauseRequestKey.self, forKey: .key)
        switch try c.decode(String.self, forKey: .status) {
        case "Pending": outcome = .pending
        case "Ready": outcome = .ready(try c.decode([WireClause].self, forKey: .clauses))
        case "Unavailable": outcome = .unavailable(try c.decode(ClauseUnavailableReason.self, forKey: .reason))
        default: throw DecodingError.dataCorruptedError(forKey: .status, in: c, debugDescription: "unknown conversion status")
        }
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Keys.self)
        try c.encode(key, forKey: .key)
        switch outcome {
        case .pending: try c.encode("Pending", forKey: .status)
        case .ready(let clauses):
            try c.encode("Ready", forKey: .status); try c.encode(clauses, forKey: .clauses)
        case .unavailable(let reason):
            try c.encode("Unavailable", forKey: .status); try c.encode(reason, forKey: .reason)
        }
    }
}

struct CommitId: Codable, Equatable, Hashable, Sendable {
    let client_instance: String
    let sequence: UInt64
    static func == (lhs: Self, rhs: Self) -> Bool {
        sameWireText(lhs.client_instance, rhs.client_instance) && lhs.sequence == rhs.sequence
    }
    func hash(into hasher: inout Hasher) {
        hasher.combine(Array(client_instance.utf8)); hasher.combine(sequence)
    }
}
enum NoLearningReason: String, Codable, Sendable { case reading = "Reading", invalidated = "Invalidated", notLearningTarget = "NotLearningTarget" }
enum IntervalLearning: Codable, Equatable, Sendable {
    case candidate(token: String, explicitlySelected: Bool), none(NoLearningReason)
    static func == (lhs: Self, rhs: Self) -> Bool {
        switch (lhs, rhs) {
        case (.candidate(let lt, let le), .candidate(let rt, let re)): sameWireText(lt, rt) && le == re
        case (.none(let l), .none(let r)): l == r
        default: false
        }
    }
    private enum Keys: String, CodingKey { case kind, token, explicitlySelected = "explicitly_selected", reason }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        switch try c.decode(String.self, forKey: .kind) {
        case "Candidate": self = .candidate(token: try c.decode(String.self, forKey: .token), explicitlySelected: try c.decode(Bool.self, forKey: .explicitlySelected))
        case "None": self = .none(try c.decode(NoLearningReason.self, forKey: .reason))
        default: throw DecodingError.dataCorruptedError(forKey: .kind, in: c, debugDescription: "unknown learning kind")
        }
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Keys.self)
        switch self {
        case .candidate(let token, let explicitlySelected):
            try c.encode("Candidate", forKey: .kind); try c.encode(token, forKey: .token)
            try c.encode(explicitlySelected, forKey: .explicitlySelected)
        case .none(let reason):
            try c.encode("None", forKey: .kind); try c.encode(reason, forKey: .reason)
        }
    }
}
struct CommitInterval: Codable, Equatable, Sendable {
    let reading_start: UInt32
    let reading_end: UInt32
    let surface: String
    let learning: IntervalLearning
    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.reading_start == rhs.reading_start && lhs.reading_end == rhs.reading_end
            && sameWireText(lhs.surface, rhs.surface) && lhs.learning == rhs.learning
    }
}
struct CommitReceipt: Codable, Equatable, Sendable {
    let commit_id: CommitId
    let engine_epoch: String
    let learning_generation: UInt64
    let reading: String
    let text: String
    let intervals: [CommitInterval]
    let sentence_token: String?

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.commit_id == rhs.commit_id && sameWireText(lhs.engine_epoch, rhs.engine_epoch)
            && lhs.learning_generation == rhs.learning_generation && sameWireText(lhs.reading, rhs.reading)
            && sameWireText(lhs.text, rhs.text) && lhs.intervals == rhs.intervals && sameWireText(lhs.sentence_token, rhs.sentence_token)
    }

    private enum CodingKeys: String, CodingKey { case commit_id, engine_epoch, learning_generation, reading, text, intervals, sentence_token }
    init(commit_id: CommitId, engine_epoch: String, learning_generation: UInt64, reading: String, text: String,
         intervals: [CommitInterval], sentence_token: String?) {
        self.commit_id = commit_id; self.engine_epoch = engine_epoch; self.learning_generation = learning_generation
        self.reading = reading; self.text = text; self.intervals = intervals; self.sentence_token = sentence_token
    }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        commit_id = try c.decode(CommitId.self, forKey: .commit_id)
        engine_epoch = try c.decode(String.self, forKey: .engine_epoch)
        learning_generation = try c.decode(UInt64.self, forKey: .learning_generation)
        reading = try c.decode(String.self, forKey: .reading); text = try c.decode(String.self, forKey: .text)
        intervals = try c.decode([CommitInterval].self, forKey: .intervals)
        sentence_token = try c.decode(String?.self, forKey: .sentence_token)
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(commit_id, forKey: .commit_id); try c.encode(engine_epoch, forKey: .engine_epoch)
        try c.encode(learning_generation, forKey: .learning_generation); try c.encode(reading, forKey: .reading)
        try c.encode(text, forKey: .text); try c.encode(intervals, forKey: .intervals)
        try c.encode(sentence_token, forKey: .sentence_token)
    }

    func validate(tokenMatches: (String, CommitInterval) -> Bool) throws {
        guard ClauseCoordinates.normalize(reading).unicodeScalars.elementsEqual(reading.unicodeScalars) else { throw ClauseValidationError.reading }
        let boundaries = try ClauseCoordinates.legalBoundaries(reading)
        var cursor: UInt32 = 0
        var surface = ""
        for interval in intervals {
            guard interval.reading_start == cursor, interval.reading_end > cursor,
                  boundaries.contains(interval.reading_end) else { throw ClauseValidationError.range }
            guard !interval.surface.isEmpty else { throw ClauseValidationError.surface }
            switch interval.learning {
            case .candidate(let token, _):
                guard !token.isEmpty, tokenMatches(token, interval) else { throw ClauseValidationError.token }
            case .none(.reading):
                guard let part = ClauseCoordinates.slice(reading, start: cursor, end: interval.reading_end),
                      part.unicodeScalars.elementsEqual(interval.surface.unicodeScalars) else { throw ClauseValidationError.surface }
            case .none: break
            }
            cursor = interval.reading_end
            surface += interval.surface
        }
        guard cursor == boundaries.last else { throw ClauseValidationError.range }
        guard surface.unicodeScalars.elementsEqual(text.unicodeScalars) else { throw ClauseValidationError.surface }
    }
}

enum ReceiptRejection: String, Codable, Sendable {
    case conflict = "Conflict", expired = "Expired", staleLearningGeneration = "StaleLearningGeneration"
    case invalidToken = "InvalidToken", invalidIntervals = "InvalidIntervals", capacity = "Capacity"
}
struct CommitReceiptAck: Codable, Equatable, Sendable {
    let commit_id: CommitId
    let outcome: Outcome
    enum Outcome: Equatable, Sendable { case applied, alreadyProcessed, rejected(ReceiptRejection) }
    private enum Keys: String, CodingKey { case commit_id, status, reason }
    init(commitId: CommitId, outcome: Outcome) { self.commit_id = commitId; self.outcome = outcome }
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        commit_id = try c.decode(CommitId.self, forKey: .commit_id)
        switch try c.decode(String.self, forKey: .status) {
        case "Applied": outcome = .applied
        case "AlreadyProcessed": outcome = .alreadyProcessed
        case "Rejected": outcome = .rejected(try c.decode(ReceiptRejection.self, forKey: .reason))
        default: throw DecodingError.dataCorruptedError(forKey: .status, in: c, debugDescription: "unknown receipt status")
        }
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Keys.self)
        try c.encode(commit_id, forKey: .commit_id)
        switch outcome {
        case .applied: try c.encode("Applied", forKey: .status)
        case .alreadyProcessed: try c.encode("AlreadyProcessed", forKey: .status)
        case .rejected(let reason):
            try c.encode("Rejected", forKey: .status); try c.encode(reason, forKey: .reason)
        }
    }
}

enum ClauseCoordinates {
    static func normalize(_ input: String) -> String {
        String(String.UnicodeScalarView(input.unicodeScalars.map {
            (0x30a1...0x30f6).contains($0.value) ? Unicode.Scalar($0.value - 0x60)! : $0
        }))
    }

    static func legalBoundaries(_ reading: String) throws -> [UInt32] {
        guard let count = UInt32(exactly: reading.unicodeScalars.count) else { throw ClauseValidationError.range }
        var result: [UInt32] = [0]
        for (offset, scalar) in reading.unicodeScalars.enumerated().dropFirst() {
            if scalar.value != 0x3099 && scalar.value != 0x309a { result.append(UInt32(offset)) }
        }
        if count > 0 { result.append(count) }
        return result
    }

    static func slice(_ reading: String, start: UInt32, end: UInt32) -> String? {
        let scalars = Array(reading.unicodeScalars)
        guard start <= end, Int(end) <= scalars.count else { return nil }
        return String(String.UnicodeScalarView(scalars[Int(start)..<Int(end)]))
    }

    static func validate(reading: String, clauses: [WireClause], start: UInt32, end: UInt32, text: String) throws {
        // Swift String equality normalizes canonically; the wire contract preserves scalar spelling.
        guard normalize(reading).unicodeScalars.elementsEqual(reading.unicodeScalars) else { throw ClauseValidationError.reading }
        let boundaries = Set(try legalBoundaries(reading))
        guard start <= end, boundaries.contains(start), boundaries.contains(end) else { throw ClauseValidationError.range }
        var cursor = start
        var ids = Set<UInt64>()
        var surface = ""
        for clause in clauses {
            guard ids.insert(clause.id).inserted else { throw ClauseValidationError.duplicateID }
            guard clause.reading_start == cursor, clause.reading_end > cursor,
                  clause.reading_end <= end, boundaries.contains(clause.reading_end) else { throw ClauseValidationError.range }
            switch clause.state {
            case .reading:
                guard clause.candidate_token == nil else { throw ClauseValidationError.token }
                guard let part = slice(reading, start: cursor, end: clause.reading_end),
                      part.unicodeScalars.elementsEqual(clause.surface.unicodeScalars) else { throw ClauseValidationError.surface }
            case .converted:
                guard let token = clause.candidate_token, !token.isEmpty else { throw ClauseValidationError.token }
                guard !clause.surface.isEmpty else { throw ClauseValidationError.surface }
            }
            surface += clause.surface
            cursor = clause.reading_end
        }
        guard cursor == end else { throw ClauseValidationError.range }
        guard surface.unicodeScalars.elementsEqual(text.unicodeScalars) else { throw ClauseValidationError.surface }
    }

    static func validatePreceding(reading: String, preceding: [PrecedingSurface], target: UInt32) throws {
        guard normalize(reading).unicodeScalars.elementsEqual(reading.unicodeScalars) else { throw ClauseValidationError.reading }
        let boundaries = Set(try legalBoundaries(reading))
        var cursor: UInt32 = 0
        var ids = Set<UInt64>()
        for item in preceding {
            guard ids.insert(item.clause_id).inserted else { throw ClauseValidationError.duplicateID }
            guard item.reading_start == cursor, item.reading_end > cursor,
                  boundaries.contains(item.reading_end) else { throw ClauseValidationError.range }
            guard !item.surface.isEmpty else { throw ClauseValidationError.surface }
            cursor = item.reading_end
        }
        guard cursor == target, boundaries.contains(target) else { throw ClauseValidationError.range }
    }
}
