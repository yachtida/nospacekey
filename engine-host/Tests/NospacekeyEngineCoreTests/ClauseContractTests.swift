import Foundation
import XCTest
@testable import NospacekeyEngineCore

final class ClauseContractTests: XCTestCase {
    private func fixture(_ name: String) throws -> Data {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
        return try Data(contentsOf: root.appendingPathComponent("docs/design/clause-navigation-p2/\(name).json"))
    }

    func testSharedScalarCoordinatesPreserveSpellingAndUTF16Units() throws {
        struct Case: Decodable { let id: String; let input: String; let normalized: String; let boundaries: [UInt32]; let utf16: Int }
        struct Suite: Decodable { let normalization: [Case] }
        for item in try JSONDecoder().decode(Suite.self, from: fixture("state-fixtures")).normalization {
            let normalized = ClauseCoordinates.normalize(item.input)
            XCTAssertTrue(normalized.unicodeScalars.elementsEqual(item.normalized.unicodeScalars), item.id)
            XCTAssertTrue(ClauseCoordinates.normalize(normalized).unicodeScalars.elementsEqual(normalized.unicodeScalars))
            XCTAssertEqual(try ClauseCoordinates.legalBoundaries(normalized), item.boundaries, item.id)
            XCTAssertEqual(normalized.utf16.count, item.utf16, item.id)
        }
        XCTAssertEqual(try ClauseCoordinates.legalBoundaries("\u{3099}\u{309a}あ"), [0, 2, 3])
        XCTAssertNil(ClauseCoordinates.slice("あ", start: .max, end: .max))
        XCTAssertEqual(ClauseCoordinates.slice("𠮷あ", start: 1, end: 2), "あ")
    }

    private struct Snapshot: Codable, Equatable { let reading: String; let text: String; let clauses: [WireClause] }
    private struct SessionWire: Codable, Equatable {
        let result: String
        let session: Int64
        let proto: UInt32
        let boot: String
        let engine_epoch: String
        let learning_generation: UInt64
    }
    private struct Token: Decodable { let token: String; let reading_start: UInt32; let reading_end: UInt32; let surface: String }
    private enum Wire: Decodable {
        case snapshot(Snapshot), candidates(ClauseCandidatesRequest), conversion(ConvertClausesRequest)
        case candidatesResult(ClauseCandidatesResult), conversionResult(ConvertClausesResult)
        case receipt(CommitReceipt), ack(CommitReceiptAck), unrelated
        case session(SessionWire), enhancementWait(SnapshotResponseKey, Bool)
        private enum Keys: String, CodingKey { case method, params, result }
        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: Keys.self)
            if try c.decodeIfPresent(String.self, forKey: .method) != nil {
                _ = try Request(from: decoder)
            }
            switch try c.decodeIfPresent(String.self, forKey: .method) {
            case "ClauseCandidates": self = .candidates(try c.decode(ClauseCandidatesRequest.self, forKey: .params))
            case "ConvertClauses": self = .conversion(try c.decode(ConvertClausesRequest.self, forKey: .params))
            case "CommitReceipt": self = .receipt(try c.decode(CommitReceipt.self, forKey: .params))
            default:
                switch try c.decodeIfPresent(String.self, forKey: .result) {
                case "Session": self = .session(try SessionWire(from: decoder))
                case "SnapshotEnhancementPending": self = .enhancementWait(try SnapshotResponseKey(from: decoder), true)
                case "SnapshotEnhancementUnavailable": self = .enhancementWait(try SnapshotResponseKey(from: decoder), false)
                case "SnapshotResult", "SnapshotEnhancement":
                    _ = try SnapshotResponseKey(from: decoder)
                    _ = try SnapshotClauseData(from: decoder)
                    self = .snapshot(try Snapshot(from: decoder))
                case "ClauseCandidatesResult": self = .candidatesResult(try ClauseCandidatesResult(from: decoder))
                case "ConvertClausesResult": self = .conversionResult(try ConvertClausesResult(from: decoder))
                case "CommitReceiptAck": self = .ack(try CommitReceiptAck(from: decoder))
                default: self = .unrelated
                }
            }
        }
    }
    private struct Case: Decodable {
        let id: String
        let expect: String
        let wire: Wire?
        private enum Keys: String, CodingKey { case id, expect, wire }
        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: Keys.self)
            id = try c.decode(String.self, forKey: .id)
            expect = try c.decode(String.self, forKey: .expect)
            do { wire = try c.decode(Wire.self, forKey: .wire) }
            catch { if expect != "reject-decode" { throw error }; wire = nil }
        }
    }
    private struct Suite: Decodable { let examples: [Case]; let token_catalog: [Token] }
    private func roundtrip<T: Codable & Equatable>(_ value: T) throws {
        XCTAssertEqual(try JSONDecoder().decode(T.self, from: JSONEncoder().encode(value)), value)
    }

    func testSharedWireFixturesRejectInvalidNumbersReadingLossAndWrongLearning() throws {
        let suite = try JSONDecoder().decode(Suite.self, from: fixture("wire-fixtures"))
        for item in suite.examples {
            if item.expect == "reject-decode" { XCTAssertNil(item.wire, item.id); continue }
            let wire = try XCTUnwrap(item.wire)
            func validate() throws {
                switch wire {
                case .snapshot(let snapshot):
                    try roundtrip(snapshot)
                    try ClauseCoordinates.validate(reading: snapshot.reading, clauses: snapshot.clauses,
                        start: 0, end: UInt32(snapshot.reading.unicodeScalars.count), text: snapshot.text)
                case .candidates(let request):
                    try roundtrip(request); try request.validate()
                    if item.id == "u64-max" { XCTAssertEqual(request.key.request_id, UInt64.max) }
                case .conversion(let request): try roundtrip(request); try request.validate()
                case .candidatesResult(let response):
                    try roundtrip(response)
                    XCTAssertEqual(try JSONDecoder().decode(ClauseCandidatesResult.self, from: JSONEncoder().encode(Response.clauseCandidatesResult(response))), response)
                case .conversionResult(let response):
                    try roundtrip(response)
                    XCTAssertEqual(try JSONDecoder().decode(ConvertClausesResult.self, from: JSONEncoder().encode(Response.convertClausesResult(response))), response)
                case .ack(let ack):
                    try roundtrip(ack)
                    XCTAssertEqual(try JSONDecoder().decode(CommitReceiptAck.self, from: JSONEncoder().encode(Response.commitReceiptAck(ack))), ack)
                case .receipt(let receipt):
                    try roundtrip(receipt)
                    try receipt.validate { token, interval in suite.token_catalog.contains {
                        $0.token == token && $0.reading_start == interval.reading_start && $0.reading_end == interval.reading_end
                            && $0.surface.unicodeScalars.elementsEqual(interval.surface.unicodeScalars)
                    } }
                case .unrelated: break
                case .session(let session):
                    try roundtrip(session)
                    let response = Response.session(session.session, proto: session.proto, boot: session.boot,
                        engineEpoch: session.engine_epoch, learningGeneration: session.learning_generation)
                    XCTAssertEqual(try JSONDecoder().decode(SessionWire.self, from: JSONEncoder().encode(response)), session)
                case .enhancementWait(let key, let pending):
                    try roundtrip(key)
                    let response = pending ? Response.snapshotEnhancementPending(key) : Response.snapshotEnhancementUnavailable(key)
                    XCTAssertEqual(try JSONDecoder().decode(SnapshotResponseKey.self, from: JSONEncoder().encode(response)), key)
                }
            }
            if item.expect == "reject-semantic" { XCTAssertThrowsError(try validate(), item.id) }
            else { XCTAssertNoThrow(try validate(), item.id) }
        }
    }

    func testWireRejectsCanonicalEquivalenceAndSplitCombiningMarks() throws {
        let clause = WireClause(id: 1, reading_start: 0, reading_end: 2, state: .reading, surface: "が", candidate_token: nil)
        XCTAssertThrowsError(try ClauseCoordinates.validate(reading: "か\u{3099}", clauses: [clause], start: 0, end: 2, text: "が"))
        let split = WireClause(id: 1, reading_start: 0, reading_end: 1, state: .reading, surface: "か", candidate_token: nil)
        XCTAssertThrowsError(try ClauseCoordinates.validate(reading: "か\u{3099}", clauses: [split], start: 0, end: 2, text: "か"))
    }

    func testSameKeyDifferentScalarPayloadsRemainDifferent() throws {
        let identity = ClauseSnapshotIdentity(composition: 1, revision: 1, configuration_generation: 1, connection_generation: 1)
        let key = ClauseRequestKey(identity: identity, baseline: 1, conversion_revision: 1, clause_id: 2, request_id: 1)
        func candidates(_ surface: String) -> ClauseCandidatesRequest {
            ClauseCandidatesRequest(key: key, reading: "があ", reading_start: 1, reading_end: 2,
                preceding_surfaces: [PrecedingSurface(clause_id: 1, reading_start: 0, reading_end: 1, surface: surface)])
        }
        let first = candidates("が"), second = candidates("か\u{3099}")
        try first.validate(); try second.validate()
        XCTAssertNotEqual(first, second)
        func conversion(_ preceding: [PrecedingSurface]) -> ConvertClausesRequest {
            ConvertClausesRequest(key: key, reading: "があ", clauses: [ClauseRange(id: 2, reading_start: 1, reading_end: 2)], preceding_surfaces: preceding)
        }
        let boundaryFirst = conversion(first.preceding_surfaces), boundarySecond = conversion(second.preceding_surfaces)
        try boundaryFirst.validate(); try boundarySecond.validate()
        XCTAssertNotEqual(boundaryFirst, boundarySecond)
        func receipt(_ surface: String) -> CommitReceipt {
            CommitReceipt(commit_id: CommitId(client_instance: "22222222-2222-4222-8222-222222222222", sequence: 1),
                engine_epoch: "11111111-1111-4111-8111-111111111111", learning_generation: 1, reading: "が", text: surface,
                intervals: [CommitInterval(reading_start: 0, reading_end: 1, surface: surface, learning: .none(.invalidated))], sentence_token: nil)
        }
        let receiptFirst = receipt("が"), receiptSecond = receipt("か\u{3099}")
        try receiptFirst.validate { _, _ in true }; try receiptSecond.validate { _, _ in true }
        XCTAssertNotEqual(receiptFirst, receiptSecond)
        XCTAssertEqual(try JSONDecoder().decode(CommitReceipt.self, from: JSONEncoder().encode(receiptSecond)), receiptSecond)
    }
}
