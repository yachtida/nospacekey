import Foundation
import KanaKanjiConverterModule

/// Bounded process-local ranking for successfully applied receipts. Durable vendor learning stays
/// on its own converter, so neither classic publication nor its mutable learning trie is involved.
final class RecentLearningOverlay: @unchecked Sendable {
    struct EvaluationCounts: Equatable {
        var learnedRanges = 0
        var candidateIdentities = 0
        var candidateElements = 0
    }

    private struct Identity: Hashable {
        let ruby: String
        let surface: String
        let consumedReading: Int
        let range: ComposingRange
    }

    private indirect enum ComposingRange: Hashable {
        case input(Int)
        case surface(Int)
        case composite(ComposingRange, ComposingRange)

        init(_ value: ComposingCount) {
            switch value {
            case .inputCount(let count): self = .input(count)
            case .surfaceCount(let count): self = .surface(count)
            case .composite(let lhs, let rhs): self = .composite(Self(lhs), Self(rhs))
            }
        }

        func consume(_ composing: inout ComposingText) -> Bool {
            switch self {
            case .input(let count):
                guard count >= 0, count <= composing.input.count else { return false }
                composing.prefixComplete(composingCount: .inputCount(count))
                return true
            case .surface(let count):
                guard count >= 0, count <= composing.convertTarget.count else { return false }
                composing.prefixComplete(composingCount: .surfaceCount(count))
                return true
            case .composite(let lhs, let rhs):
                return lhs.consume(&composing) && rhs.consume(&composing)
            }
        }
    }

    private struct PrefixIdentity: Hashable {
        let ruby: String
        let surface: String
        let consumedReading: Int
    }

    private struct LearningIndex {
        var exact: [Identity: Int] = [:]
        var prefix: [PrefixIdentity: Int] = [:]
    }

    private struct DecoratedCandidate {
        let candidate: Candidate
        let identity: Identity?
        let rank: Int
        let offset: Int
    }

    private let capacity: Int
    private let lock = NSLock()
    private var entries: [Identity] = []

    init(capacity: Int = 128) { self.capacity = max(1, capacity) }

    func record(_ candidate: Candidate) {
        guard let identity = Self.identity(of: candidate) else { return }
        lock.lock()
        entries.removeAll { $0 == identity }
        entries.insert(identity, at: 0)
        if entries.count > capacity { entries.removeLast(entries.count - capacity) }
        lock.unlock()
    }

    func rank(mainResults: [Candidate], firstClauseResults: [Candidate], composing: ComposingText)
        -> (main: [Candidate], firstClause: [Candidate])
    {
        rankImpl(mainResults: mainResults, firstClauseResults: firstClauseResults,
                 composing: composing).result
    }

    func rankForTesting(mainResults: [Candidate], firstClauseResults: [Candidate],
                        composing: ComposingText)
        -> (main: [Candidate], firstClause: [Candidate], evaluations: EvaluationCounts)
    {
        let ranked = rankImpl(mainResults: mainResults, firstClauseResults: firstClauseResults,
                              composing: composing)
        return (ranked.result.main, ranked.result.firstClause, ranked.evaluations)
    }

    private func rankImpl(mainResults: [Candidate], firstClauseResults: [Candidate],
                          composing: ComposingText)
        -> (result: (main: [Candidate], firstClause: [Candidate]), evaluations: EvaluationCounts)
    {
        lock.lock()
        let learned = entries
        lock.unlock()
        guard !learned.isEmpty else {
            return ((mainResults, firstClauseResults), EvaluationCounts())
        }
        var evaluations = EvaluationCounts()
        let index = Self.learningIndex(learned, composing: composing, evaluations: &evaluations)
        let main = Self.rankMain(mainResults, index: index, evaluations: &evaluations)
        let firstClause = Self.rankFirstClause(
            firstClauseResults, index: index, evaluations: &evaluations)
        return ((main, firstClause), evaluations)
    }

    func clear() {
        lock.lock()
        entries.removeAll(keepingCapacity: true)
        lock.unlock()
    }

    var count: Int {
        lock.lock()
        defer { lock.unlock() }
        return entries.count
    }

    private static func learningIndex(_ learned: [Identity], composing: ComposingText,
                                      evaluations: inout EvaluationCounts) -> LearningIndex {
        var index = LearningIndex()
        for (rank, identity) in learned.enumerated() {
            evaluations.learnedRanges += 1
            guard rangeMatches(identity, composing: composing) else { continue }
            if index.exact[identity] == nil { index.exact[identity] = rank }
            let prefix = PrefixIdentity(
                ruby: identity.ruby, surface: identity.surface,
                consumedReading: identity.consumedReading)
            if index.prefix[prefix] == nil { index.prefix[prefix] = rank }
        }
        return index
    }

    private static func rankMain(_ candidates: [Candidate], index: LearningIndex,
                                 evaluations: inout EvaluationCounts) -> [Candidate] {
        let decorated = candidates.enumerated().map { offset, candidate in
            evaluations.candidateIdentities += 1
            var ruby = ""
            var surface = ""
            var rank = Int.max
            for element in candidate.data {
                evaluations.candidateElements += 1
                ruby += element.ruby
                surface += element.word
                guard let normalized = CorrectionStore.normalizedKey(ruby) else { continue }
                let prefix = PrefixIdentity(
                    ruby: normalized, surface: surface, consumedReading: normalized.count)
                rank = min(rank, index.prefix[prefix] ?? Int.max)
            }
            let normalized = CorrectionStore.normalizedKey(ruby)
            let identity = normalized.map {
                Identity(ruby: $0, surface: candidate.text, consumedReading: $0.count,
                         range: ComposingRange(candidate.composingCount))
            }
            return DecoratedCandidate(
                candidate: candidate, identity: identity, rank: rank, offset: offset)
        }
        return rankUnique(decorated)
    }

    private static func rankFirstClause(_ candidates: [Candidate], index: LearningIndex,
                                        evaluations: inout EvaluationCounts) -> [Candidate] {
        let decorated = candidates.enumerated().map { offset, candidate in
            evaluations.candidateIdentities += 1
            var ruby = ""
            for element in candidate.data {
                evaluations.candidateElements += 1
                ruby += element.ruby
            }
            let normalized = CorrectionStore.normalizedKey(ruby)
            let identity = normalized.map {
                Identity(ruby: $0, surface: candidate.text, consumedReading: $0.count,
                         range: ComposingRange(candidate.composingCount))
            }
            return DecoratedCandidate(
                candidate: candidate, identity: identity,
                rank: identity.flatMap { index.exact[$0] } ?? Int.max, offset: offset)
        }
        return rankUnique(decorated)
    }

    private static func rankUnique(_ candidates: [DecoratedCandidate]) -> [Candidate] {
        var seen: Set<Identity> = []
        let unique = candidates.filter { candidate in
            guard let identity = candidate.identity else { return true }
            return seen.insert(identity).inserted
        }
        return unique.sorted {
            $0.rank == $1.rank ? $0.offset < $1.offset : $0.rank < $1.rank
        }.map(\.candidate)
    }

    private static func identity(of candidate: Candidate) -> Identity? {
        guard let ruby = CorrectionStore.normalizedKey(candidate.data.map(\.ruby).joined()) else { return nil }
        return Identity(ruby: ruby, surface: candidate.text, consumedReading: ruby.count,
                        range: ComposingRange(candidate.composingCount))
    }

    private static func rangeMatches(_ identity: Identity, composing: ComposingText) -> Bool {
        var remainder = composing
        let reading = composing.convertTarget
        guard identity.range.consume(&remainder), remainder.convertTarget.count <= reading.count else { return false }
        let consumed = String(reading.dropLast(remainder.convertTarget.count))
        return CorrectionStore.normalizedKey(consumed) == identity.ruby
    }
}
