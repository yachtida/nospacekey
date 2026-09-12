import Foundation
import XCTest
import KanaKanjiConverterModuleWithDefaultDictionary
@testable import NospacekeyEngineCore

final class SnapshotClauseEngineTests: XCTestCase {
    private func service() -> ConversionService {
        ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: false, memoryDir: nil))
    }
    private func candidate(_ reading: String, _ surface: String, dataSurface: String? = nil) -> Candidate {
        Candidate(text: surface, value: 100, composingCount: .inputCount(reading.unicodeScalars.count),
            lastMid: MIDData.一般.mid, data: [DicdataElement(word: dataSurface ?? surface, ruby: reading,
                cid: CIDData.固有名詞.cid, mid: MIDData.一般.mid, value: 0)])
    }
    private func validate(_ data: SnapshotClauseData, text: String) throws {
        try ClauseCoordinates.validate(reading: data.reading, clauses: data.clauses, start: 0,
            end: UInt32(data.reading.unicodeScalars.count), text: text)
    }

    func testPartialCandidateKeepsUnconsumedSuffixAsReading() throws {
        let data = service().snapshotClausesForTesting(reading: "きょうは", candidate: candidate("キョウ", "今日"))
        try validate(data, text: "今日は")
        XCTAssertEqual(data.clauses.map(\.reading_end), [3, 4])
        XCTAssertEqual(data.clauses.map(\.state), [.converted, .reading])
        XCTAssertNotNil(data.clauses[0].candidate_token)
        XCTAssertNil(data.clauses[1].candidate_token)
    }

    func testSnapshotInletPreservesPartialConversionInExplicitAndLiveModes() throws {
        for explicit in [true, false] {
            let service = service()
            service.snapshotCandidatesForTesting = [candidate("キョウ", "今日")]
            let result = service.snapshot([SnapshotSegment(text: "きょうは", style: "direct")], explicit: explicit)
            XCTAssertEqual(result.text, "今日は")
            try validate(result.clauseData, text: result.text)
            XCTAssertEqual(result.clauseData.clauses.map(\.state), [.converted, .reading])
            XCTAssertEqual(result.clauseData.clauses.map(\.reading_end), [3, 4])
        }
    }

    func testMissingWrongReadingAndFailedPartialDecompositionKeepEntireReading() throws {
        let service = service()
        for native in [nil, candidate("アシタ", "明日"), candidate("キョウ", "今日", dataSurface: "異なる")] as [Candidate?] {
            let data = service.snapshotClausesForTesting(reading: "きょうは", candidate: native)
            try validate(data, text: "きょうは")
            XCTAssertEqual(data.clauses.map(\.state), [.reading])
        }
    }

    func testWholeCandidateMayFallbackToOneClauseWhenSurfaceDecompositionDiffers() throws {
        let data = service().snapshotClausesForTesting(reading: "きょう", candidate: candidate("キョウ", "今日", dataSurface: "異なる"))
        try validate(data, text: "今日")
        XCTAssertEqual(data.clauses.count, 1)
        XCTAssertEqual(data.clauses[0].state, .converted)
        XCTAssertEqual(data.clauses[0].reading_end, 3)
    }

    func testClassicNavigationFixtureHasMultipleCoveredClauses() throws {
        let result = service().snapshot([SnapshotSegment(text: "きょうはいいてんきです", style: "direct")], explicit: true)
        try validate(result.clauseData, text: result.text)
        XCTAssertEqual(result.clauseData.reading, "きょうはいいてんきです")
        XCTAssertGreaterThan(result.clauseData.clauses.count, 1, "fixture must exercise a nonempty following clause")
    }

    func testRealSnapshotAndCandidatesUseImmutableRequestKeys() throws {
        let service = service()
        let identity = ClauseSnapshotIdentity(composition: 8, revision: 13, configuration_generation: 2, connection_generation: 5)
        let snapshotKey = ConversionService.SnapshotEnhancementKey(composition: 8, revision: 13,
            configurationGeneration: 2, connectionGeneration: 5, conversionRevision: 0, requestID: 1)
        let first = service.snapshot([SnapshotSegment(text: "にほんご", style: "direct")], explicit: true, enhancementKey: snapshotKey)
        let second = service.snapshot([SnapshotSegment(text: "にほんご", style: "direct")], explicit: true, enhancementKey: snapshotKey)
        XCTAssertNotEqual(first.baseline, second.baseline)
        try validate(first.clauseData, text: first.text)
        let key = ClauseRequestKey(identity: identity, baseline: first.baseline, conversion_revision: 0, clause_id: 1, request_id: 2)
        let request = ClauseCandidatesRequest(key: key, reading: "にほんご", reading_start: 0, reading_end: 4, preceding_surfaces: [])
        let response = service.clauseCandidates(request)
        guard case .ready(let candidates) = response.outcome else { return XCTFail("expected candidates, got \(response)") }
        try request.validateCandidates(candidates)
        XCTAssertEqual(service.clauseCandidates(request), response)
        let changed = ClauseCandidatesRequest(key: key, reading: "てんき", reading_start: 0, reading_end: 3, preceding_surfaces: [])
        XCTAssertEqual(service.clauseCandidates(changed).outcome, .unavailable(.invalidRequest))
        let wrongOperation = ConvertClausesRequest(key: key, reading: "にほんご",
            clauses: [ClauseRange(id: 1, reading_start: 0, reading_end: 4)], preceding_surfaces: [])
        XCTAssertEqual(service.convertClauses(wrongOperation).outcome, .unavailable(.invalidRequest))
    }

    func testIntervalConversionKeepsRequestedBoundaryEvenWhenDictionaryWouldGroupIt() throws {
        let service = service()
        let snapshot = service.snapshot([SnapshotSegment(text: "きょうはてんき", style: "direct")], explicit: true,
            enhancementKey: .init(composition: 8, revision: 13, configurationGeneration: 2, connectionGeneration: 5))
        let key = ClauseRequestKey(identity: .init(composition: 8, revision: 13, configuration_generation: 2, connection_generation: 5),
            baseline: snapshot.baseline, conversion_revision: 1, clause_id: 10, request_id: 2)
        let request = ConvertClausesRequest(key: key, reading: "きょうはてんき",
            clauses: [ClauseRange(id: 10, reading_start: 0, reading_end: 1), ClauseRange(id: 11, reading_start: 1, reading_end: 7)],
            preceding_surfaces: [])
        let response = service.convertClauses(request)
        guard case .ready(let clauses) = response.outcome else { return XCTFail("expected exact intervals, got \(response)") }
        try request.validateResult(clauses)
        XCTAssertEqual(clauses.map(\.id), [10, 11])
        XCTAssertEqual(clauses.map(\.reading_end), [1, 7])
        XCTAssertEqual(service.convertClauses(request), response)
    }
}
