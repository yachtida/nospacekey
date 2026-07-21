import XCTest
@testable import NospacekeyEngineCore

final class ConversionServiceReconvertTests: XCTestCase {
    /// Zenzai 無効（古典変換）でテストする。
    private func makeService() -> ConversionService {
        ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
    }

    private func containsKanji(_ s: String) -> Bool {
        s.unicodeScalars.contains { (0x4E00...0x9FFF).contains($0.value) }
    }

    func testReconvertHiraganaYieldsKanjiCandidate() {
        let svc = makeService()
        let sid = svc.startSession()
        let cands = svc.reconvert(session: sid, surface: "にほんご")
        XCTAssertNotNil(cands)
        XCTAssertFalse(cands!.isEmpty)
        XCTAssertTrue(cands!.contains(where: containsKanji), "expected at least one kanji candidate, got \(cands!)")
    }

    func testReconvertKatakanaNormalizesToHiraganaReading() {
        let svc = makeService()
        let sid = svc.startSession()
        let cands = svc.reconvert(session: sid, surface: "ニホンゴ")
        XCTAssertNotNil(cands)
        XCTAssertTrue(cands!.contains(where: containsKanji), "katakana input should reconvert like hiragana, got \(cands ?? [])")
    }

    func testReconvertUnknownSessionIsNil() {
        let svc = makeService()
        XCTAssertNil(svc.reconvert(session: 9999, surface: "にほんご"))
    }

    func testNormalizeKanaMapsKatakanaKeepsHiraganaAndChoon() {
        XCTAssertEqual(ConversionService.normalizeKana("ニホンゴ"), "にほんご")
        XCTAssertEqual(ConversionService.normalizeKana("にほんご"), "にほんご")
        XCTAssertEqual(ConversionService.normalizeKana("ラーメン"), "らーめん") // ー(U+30FC) は不変
    }
}
