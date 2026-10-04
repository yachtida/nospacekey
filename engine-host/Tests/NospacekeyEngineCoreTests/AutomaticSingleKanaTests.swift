import Foundation
import XCTest
import KanaKanjiConverterModuleWithDefaultDictionary
@testable import NospacekeyEngineCore

final class AutomaticSingleKanaTests: XCTestCase {
    private func service(engine: String = "azookey") -> ConversionService {
        ConversionService(config: .init(weightURL: nil, inferenceLimit: 1), learning: .disabled,
            autoCommit: .ultrastrong, environment: ["NOSPACEKEY_CONVERSION_ENGINE": engine],
            fileSystem: .live, windowsTextProvider: .init { _, _, _ in ["検証漢字"] })
    }

    private func candidate(_ reading: String, surface: String = "検証漢字") -> Candidate {
        Candidate(text: surface, value: 100, composingCount: .inputCount(reading.count),
            lastMid: MIDData.一般.mid,
            data: [DicdataElement(word: surface, ruby: ConversionService.toKatakana(reading),
                cid: CIDData.一般名詞.cid, mid: MIDData.一般.mid, value: 0)])
    }

    func testSingleKanaIncludesSmallVoicedAndDecomposedKana() {
        for (reading, expected) in [("あ", "あ"), ("が", "が"), ("ん", "ん"), ("ぁ", "ぁ"),
            ("っ", "っ"), ("ゃ", "ゃ"), ("ゔ", "ゔ"), ("ガ", "が"),
            ("か\u{3099}", "か\u{3099}"), ("ハ\u{309A}", "は\u{309A}")] {
            XCTAssertEqual(ConversionService.automaticSingleKana(reading), expected)
        }
        for reading in ["", "きゃ", "あい", "か\u{3099}く", "n", "A", "1", "。", "ー", "\u{3099}"] {
            XCTAssertNil(ConversionService.automaticSingleKana(reading), reading)
        }
    }

    func testLiveSnapshotsKeepSingleKanaWithoutCommitOrEnhancementAcrossEngines() throws {
        for engine in ["azookey", "hybrid", "microsoft"] {
            let svc = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
                learning: .disabled, autoCommit: .ultrastrong,
                environment: ["NOSPACEKEY_CONVERSION_ENGINE": engine], fileSystem: .live,
                windowsTextProvider: .init { _, _, _ in
                    XCTFail("一文字の自動変換ではWindows候補を照会しない")
                    return ["検証漢字"]
                })
            for (offset, reading) in ["ま", "あ", "が", "ぁ", "っ", "ん", "か\u{3099}", "ガ"].enumerated() {
                let key = ConversionService.SnapshotEnhancementKey(composition: 9, revision: UInt64(offset + 1),
                    configurationGeneration: 1, connectionGeneration: 1)
                svc.snapshotCandidatesForTesting = [candidate(reading)]
                for width in [1, 10] {
                    let result = svc.snapshot([.init(text: reading, style: "direct")], explicit: false,
                        enhancementKey: key, snapshotConnection: 1, liveSearchWidth: width)
                    XCTAssertEqual(result.text, ConversionService.normalizeKana(reading))
                    XCTAssertNil(result.autoCommit)
                    XCTAssertNil(result.candidates)
                    XCTAssertEqual(result.clauseData.clauses.map(\.state), [.reading])
                    XCTAssertTrue(result.clauseData.clauses.allSatisfy { $0.candidate_token == nil })
                    try ClauseCoordinates.validate(reading: result.clauseData.reading, clauses: result.clauseData.clauses,
                        start: 0, end: UInt32(result.clauseData.reading.unicodeScalars.count), text: result.text)
                    guard case .unavailable = svc.pollSnapshotEnhancement(key: key, baseline: result.baseline) else {
                        return XCTFail("一文字の表示を後から再変換しない")
                    }
                }
            }
        }
    }

    func testRomanInputIsCountedAfterKanaComposition() {
        let svc = service()
        for (roman, expected) in [("ma", "ま"), ("ka", "か"), ("ga", "が"), ("la", "ぁ"), ("ltu", "っ"), ("nn", "ん")] {
            let result = svc.snapshot([.init(text: roman, style: nil)], explicit: false)
            XCTAssertEqual(result.text, expected, roman)
            XCTAssertEqual(result.reading, expected, roman)
        }
    }

    func testLiveSnapshotReturnsToSingleKanaAfterDeletionAcrossEngines() {
        for engine in ["azookey", "hybrid", "microsoft"] {
            let svc = service(engine: engine)
            for (offset, roman) in ["ma", "made", "ma"].enumerated() {
                let reading = roman == "made" ? "まで" : "ま"
                let key = ConversionService.SnapshotEnhancementKey(composition: 9, revision: UInt64(offset + 1),
                    configurationGeneration: 1, connectionGeneration: 1)
                svc.snapshotCandidatesForTesting = [candidate(reading, surface: roman == "made" ? "間で" : "間")]
                let result = svc.snapshot([.init(text: roman, style: nil)], explicit: false,
                    enhancementKey: key, snapshotConnection: 1)
                XCTAssertEqual(result.reading, reading, engine)
                XCTAssertEqual(result.text, roman == "made" ? "間で" : "ま", engine)
                if roman == "ma" {
                    XCTAssertNil(result.autoCommit)
                    XCTAssertEqual(result.clauseData.clauses.map(\.state), [.reading])
                    guard case .unavailable = svc.pollSnapshotEnhancement(key: key, baseline: result.baseline) else {
                        return XCTFail("single kana must not enqueue a replacement conversion")
                    }
                }
            }
        }
    }

    func testExplicitConversionAndMulticharacterOrNonKanaLiveConversionStillUseCandidates() {
        for engine in ["azookey", "hybrid", "microsoft"] {
            let svc = service(engine: engine)
            for reading in ["ま", "あ", "が", "ぁ", "ん"] {
                svc.snapshotCandidatesForTesting = [candidate(reading)]
                let explicit = svc.snapshot([.init(text: reading, style: "direct")], explicit: true,
                    includeFlatCandidates: true)
                XCTAssertEqual(explicit.text, "検証漢字")
                XCTAssertTrue(explicit.candidates?.contains("検証漢字") == true)
                XCTAssertFalse(explicit.clauseData.flat_candidates?.isEmpty ?? true)
            }
        }
        let svc = service()
        for reading in ["あい", "きゃ", "。", "A"] {
            svc.snapshotCandidatesForTesting = [candidate(reading)]
            XCTAssertEqual(svc.snapshot([.init(text: reading, style: "direct")], explicit: false).text, "検証漢字")
        }
    }

    func testLegacyLiveResponseAndCachedCommitKeepHiraganaWhileExplicitConversionRemainsAvailable() throws {
        let svc = service(engine: "microsoft")
        for reading in ["ま", "あ", "が", "ぁ", "ん", "か\u{3099}"] {
            let sid = svc.startSession()
            _ = svc.insert(session: sid, text: reading, style: "direct")
            for _ in 0..<32 {
                let live = try XCTUnwrap(svc.liveConvert(session: sid, allowAutoCommit: true))
                XCTAssertEqual(live.text, reading)
                XCTAssertNil(live.committed)
            }
            XCTAssertEqual(try XCTUnwrap(svc.commit(session: sid, index: 0)).text, reading)
            svc.endSession(session: sid)
            let explicitSession = svc.startSession()
            _ = svc.insert(session: explicitSession, text: reading, style: "direct")
            XCTAssertTrue(svc.convert(session: explicitSession)?.contains("検証漢字") == true)
            svc.endSession(session: explicitSession)
        }
    }

    func testLegacyLiveConversionReturnsToSingleKanaAfterBackspaceAcrossEngines() throws {
        for engine in ["azookey", "hybrid", "microsoft"] {
            let svc = service(engine: engine)
            let sid = svc.startSession()
            XCTAssertEqual(svc.insert(session: sid, text: "ma"), "ま")
            XCTAssertEqual(try XCTUnwrap(svc.liveConvert(session: sid, allowAutoCommit: true)).text, "ま")
            XCTAssertEqual(svc.insert(session: sid, text: "de"), "まで")
            _ = svc.liveConvert(session: sid, allowAutoCommit: true)
            XCTAssertEqual(svc.backspace(session: sid), "ま")
            let live = try XCTUnwrap(svc.liveConvert(session: sid, allowAutoCommit: true))
            XCTAssertEqual(live.text, "ま", engine)
            XCTAssertNil(live.committed)
            XCTAssertEqual(try XCTUnwrap(svc.commit(session: sid, index: 0)).text, "ま", engine)
            svc.endSession(session: sid)
        }
    }

    func testSingleKanaSkipsZenzaiEvenWhenConfiguredAndReady() throws {
        let svc = ConversionService(config: .init(
            weightURL: URL(fileURLWithPath: "C:/missing/zenzai.gguf"), inferenceLimit: 1),
            learning: .disabled, autoCommit: .ultrastrong)
        svc.setZenzaiReadyForTesting(true)
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "ma")
        XCTAssertEqual(try XCTUnwrap(svc.liveConvert(session: sid, allowAutoCommit: true)).text, "ま")
        let result = svc.snapshot([.init(text: "ma", style: nil)], explicit: false)
        XCTAssertEqual(result.text, "ま")
        XCTAssertNil(result.autoCommit)
        XCTAssertEqual(svc.zenzaiVendorInvocationCountForTesting, 0)
        svc.endSession(session: sid)
    }
}
