import XCTest
@testable import NospacekeyEngineCore

final class TypoRepairTests: XCTestCase {
    /// 同一英字のちょうど2連打(s)を1文字へ縮約する（実例: shitekudassai）。
    func testExactDoubleCollapses() {
        XCTAssertEqual(TypoRepair.hypotheses(roman: "shitekudassai"), ["shitekudasai"])
    }

    /// 縮約対象は子音に限らない（母音の2連打も同様に縮約する）。
    func testVowelDoubleCollapses() {
        XCTAssertEqual(TypoRepair.hypotheses(roman: "kudasaai"), ["kudasai"])
    }

    /// 縮約サイトが複数あるとき: 単一サイト(左→右)、続いて2サイト同時の順で列挙する。
    func testMultiSiteSinglesThenPairs() {
        XCTAssertEqual(TypoRepair.hypotheses(roman: "ssass"), ["sass", "ssas", "sas"])
    }

    /// 仮説は計8件でキャップする（4サイトなら 単一4 + ペア(先頭4件) = 8）。
    func testCapAtEightHypotheses() {
        let hyps = TypoRepair.hypotheses(roman: "ssakkattappa")
        XCTAssertEqual(hyps.count, 8)
        XCTAssertEqual(Array(hyps.prefix(4)),
                       ["sakkattappa", "ssakattappa", "ssakkatappa", "ssakkattapa"])
    }

    /// 3連打以上は意図的な連打とみなし縮約対象にしない（run の一部は縮約サイトにならない）。
    func testTripleOrLongerRunIsIntentional() {
        XCTAssertEqual(TypoRepair.hypotheses(roman: "wwww"), [])
    }

    /// n の2連打は「ん」の正当な定石であり縮約対象から除外する。
    func testDoubleNIsExcluded() {
        XCTAssertEqual(TypoRepair.hypotheses(roman: "konnichiwa"), [])
    }
}
