import Foundation
import XCTest
@testable import NospacekeyEngineCore

final class CorrectionStoreTests: XCTestCase {
    private var dir: URL!

    override func setUp() {
        dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-corr-test-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    }
    override func tearDown() { try? FileManager.default.removeItem(at: dir) }

    func testRecordAndLookupNormalizesKatakanaReading() {
        let s = CorrectionStore(directory: dir)
        s.record(reading: "ミコミット", surface: "未コミット")
        XCTAssertEqual(s.lookup(reading: "みこみっと"), "未コミット")
        XCTAssertEqual(s.lookup(reading: "ミコミット"), "未コミット")
    }

    func testRemoveDeletesEntryAndPersistsViaFlush() {
        // モデル1位の明示選択=昇格の拒否(un-learn)。remove しないと誤登録の除去手段が
        // ClearLearning(全消し)しか無い(第2R敵対レビュー②)。
        let s = CorrectionStore(directory: dir)
        s.record(reading: "にほんご", surface: "似本languages")
        s.flush()   // 本番の順序(record 直後の無条件 flush)を再現 — これが無いと record の
                    // dirty が remove の flush まで生き残り、remove が dirty を立て忘れても
                    // 最終アサートが緑になる(un-learn が再起動で復活する失敗モードを見逃す)
        XCTAssertTrue(s.remove(reading: "ニホンゴ"))     // 正規化キーで消える
        XCTAssertNil(s.lookup(reading: "にほんご"))
        XCTAssertFalse(s.remove(reading: "にほんご"))    // 不在の remove は false(flush 不要の判定用)
        s.flush()
        XCTAssertNil(CorrectionStore(directory: dir).lookup(reading: "にほんご"))
    }

    func testLatestRecordWins() {
        let s = CorrectionStore(directory: dir)
        s.record(reading: "みこみっと", surface: "未コミット")
        s.record(reading: "みこみっと", surface: "見込みっと")
        XCTAssertEqual(s.lookup(reading: "みこみっと"), "見込みっと")
    }

    func testNonKanaAndEmptyReadingsAreRejected() {
        let s = CorrectionStore(directory: dir)
        s.record(reading: "nihongo", surface: "日本語")   // ASCII → 棄却
        s.record(reading: "見込みっと", surface: "x")      // 漢字混在 → 棄却
        s.record(reading: "", surface: "x")                // 空 → 棄却
        XCTAssertNil(s.lookup(reading: "nihongo"))
        XCTAssertNil(s.lookup(reading: "見込みっと"))
        XCTAssertNil(s.lookup(reading: ""))
    }

    func testChoonReadingIsAccepted() {
        let s = CorrectionStore(directory: dir)
        s.record(reading: "わーるど", surface: "ワールド")   // 長音符はかな扱い
        XCTAssertEqual(s.lookup(reading: "わーるど"), "ワールド")
    }

    func testLruEvictsOldestBeyondCap() {
        let s = CorrectionStore(directory: dir)
        // かなのみのユニーク読みを cap+1 件生成(2文字かな組合せ: 44^2 = 1936 > 1001)
        let kana = Array("あいうえおかきくけこさしすせそたちつてとなにぬねのはひふへほまみむめもやゆよらりるれろわ")
        var readings: [String] = []
        outer: for a in kana { for b in kana {
            readings.append(String([a, b]))
            if readings.count == CorrectionStore.maxEntries + 1 { break outer }
        } }
        for r in readings { s.record(reading: r, surface: "X" + r) }
        XCTAssertNil(s.lookup(reading: readings[0]))                       // 最古は追い出し
        XCTAssertEqual(s.lookup(reading: readings[1]), "X" + readings[1])  // 2番目以降は残存
    }

    func testLookupDoesNotRefreshLru() {
        let s = CorrectionStore(directory: dir)
        s.record(reading: "ふるい", surface: "古い")
        s.record(reading: "あたらしい", surface: "新しい")
        _ = s.lookup(reading: "ふるい")   // 参照しても寿命は伸びない
        XCTAssertEqual(s.entriesForTesting.first?.reading, "あたらしい")
    }

    func testFlushAndReloadRoundTrip() {
        let s = CorrectionStore(directory: dir)
        s.record(reading: "みこみっと", surface: "未コミット")
        s.flush()
        let s2 = CorrectionStore(directory: dir)
        XCTAssertEqual(s2.lookup(reading: "みこみっと"), "未コミット")
    }

    func testCorruptJsonStartsEmpty() {
        try? Data("{broken".utf8).write(to: dir.appendingPathComponent("corrections.json"))
        let s = CorrectionStore(directory: dir)
        XCTAssertNil(s.lookup(reading: "みこみっと"))
        s.record(reading: "みこみっと", surface: "未コミット")   // 破損後も動く
        XCTAssertEqual(s.lookup(reading: "みこみっと"), "未コミット")
    }

    func testClearRemovesFileAndMemory() {
        let s = CorrectionStore(directory: dir)
        s.record(reading: "みこみっと", surface: "未コミット")
        s.flush()
        s.clear()
        XCTAssertNil(s.lookup(reading: "みこみっと"))
        XCTAssertFalse(FileManager.default.fileExists(
            atPath: dir.appendingPathComponent("corrections.json").path))
    }

    func testNilDirectoryIsMemoryOnly() {
        let s = CorrectionStore(directory: nil)
        s.record(reading: "みこみっと", surface: "未コミット")
        XCTAssertEqual(s.lookup(reading: "みこみっと"), "未コミット")
        s.flush()   // no-op(クラッシュしない)
        s.clear()
    }
}
