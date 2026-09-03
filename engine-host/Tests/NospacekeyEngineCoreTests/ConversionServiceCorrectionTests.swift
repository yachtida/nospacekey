import Foundation
import XCTest
import KanaKanjiConverterModuleWithDefaultDictionary
@testable import NospacekeyEngineCore

final class ConversionServiceCorrectionTests: XCTestCase {
    private var dir: URL!
    private var svc: ConversionService!

    override func setUp() {
        dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-corrsvc-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        svc = ConversionService(
            config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: true, memoryDir: dir))
    }
    override func tearDown() { try? FileManager.default.removeItem(at: dir) }

    private func newSession(reading: String) -> Int {
        let s = svc.startSession()
        _ = svc.insert(session: s, text: reading)
        return s
    }

    func testNonTopCommitIsPromotedNextTime() {
        let s1 = newSession(reading: "nihongo")
        let cands = svc.convert(session: s1)!
        XCTAssertGreaterThan(cands.count, 1)
        let chosen = cands[1]                       // 1位以外を明示選択=訂正
        let r = svc.commit(session: s1, index: 1)!
        XCTAssertTrue(r.reading.isEmpty, "index1 が全被覆でない辞書出力なら読みを変える")
        svc.endSession(session: s1)

        // 主アサートは観測窓(黒箱の並び比較は学習効果だけでも成立し偽緑になる —
        // 既存 LearningTests が「学習だけで先頭に来る」ことを実証済みのため)。
        XCTAssertEqual(svc.correctionLookupForTesting(reading: "にほんご"), chosen)

        let s2 = newSession(reading: "nihongo")
        let cands2 = svc.convert(session: s2)!
        XCTAssertEqual(cands2.first, chosen)        // 昇格+学習の複合結果としても1位
        XCTAssertEqual(cands2.filter { $0 == chosen }.count, 1)   // dedup で重複しない
        svc.endSession(session: s2)
    }

    func testTopCommitIsNotRecorded() {
        // 陰性検証は黒箱(並び比較)だと学習効果と区別できないため、観測窓で直接見る。
        let s1 = newSession(reading: "nihongo")
        _ = svc.convert(session: s1)!
        _ = svc.commit(session: s1, index: 0)       // 1位確定=訂正ではない
        svc.endSession(session: s1)
        XCTAssertNil(svc.correctionLookupForTesting(reading: "にほんご"))
    }

    func testUnknownSurfaceIsInsertedAtTopAndLearnsOnCommit() {
        // 候補列に無い表層でも先頭挿入される(Zenzai 排除ケースの再現は素の変換では
        // 作れないため、record 相当を直接仕込んで昇格側だけ検証)。
        let s1 = newSession(reading: "nihongo")
        let cands1 = svc.convert(session: s1)!
        let alien = "似本languages"   // 辞書に存在しない表層
        XCTAssertFalse(cands1.contains(alien))
        svc.endSession(session: s1)
        svc.recordForTesting(reading: "にほんご", surface: alien)

        let s2 = newSession(reading: "nihongo")
        let cands2 = svc.convert(session: s2)!
        XCTAssertEqual(cands2.first, alien)
        XCTAssertEqual(cands2.filter { $0 == alien }.count, 1)
        // 挿入合成候補の確定は全消費+学習に乗る
        let r = svc.commit(session: s2, index: 0)!
        XCTAssertEqual(r.text, alien)
        XCTAssertEqual(r.reading, "")
        svc.endSession(session: s2)
        // 学習効果の分離観測: 昇格テーブルを消してから再変換し、
        // 学習単独で候補列に現れることを確認する。
        svc.clearCorrectionsForTesting()
        let s3 = newSession(reading: "nihongo")
        XCTAssertTrue(svc.convert(session: s3)!.contains(alien))
        svc.endSession(session: s3)
    }

    func testPromotedListIndexStaysConsistentOnCommit() {
        // 昇格で並びがずれても cachedCandidates と返却列は同順(spec §3(a))。
        svc.recordForTesting(reading: "にほんご", surface: "似本languages")
        let s = newSession(reading: "nihongo")
        let cands = svc.convert(session: s)!
        XCTAssertGreaterThan(cands.count, 2)
        let r = svc.commit(session: s, index: 2)!   // 昇格後 index>0 で確定
        XCTAssertEqual(r.text, cands[2])
        svc.endSession(session: s)
    }

    func testPartialCommitIsNotRecorded() throws {
        // 前方一致の部分確定(remaining 非空)は記録しない。部分被覆候補は辞書データ依存
        // なので、probe で残り読みが出る index を探し、無ければスキップする。
        let s1 = newSession(reading: "kyouhaharedesu")
        let cands = svc.convert(session: s1)!
        XCTAssertGreaterThan(cands.count, 1)   // 空/1件だと Range 構築が trap するため先に確認
        let partial = (1..<cands.count).first {
            (svc.commitProbeRemaining(session: s1, index: $0) ?? "").isEmpty == false
        }
        guard let i = partial else { throw XCTSkip("no partial candidate in dictionary output") }
        let r = svc.commit(session: s1, index: i)!
        XCTAssertFalse(r.reading.isEmpty)   // 本当に部分確定になったこと(前提の確認)
        svc.endSession(session: s1)
        XCTAssertNil(svc.correctionLookupForTesting(reading: "きょうははれです"))
    }

    func testRepairedCommitIsNotRecorded() {
        // 修正変換の修復確定(isRepaired)は記録しない(ADR-0002 キルスイッチ迂回の防止)。
        // 入力は既存 ConversionServiceTypoTests が実証済みの "shitekudassai"
        // (ss の2連打ちょうど → 修復仮説「してください」が決定的に出る)。
        let s = svc.startSession()
        _ = svc.insert(session: s, text: "shitekudassai")
        let cands = svc.typoConvert(session: s)!
        guard let repairedIdx = cands.firstIndex(of: "してください"),
              svc.typoRepairedIndices[s]?.contains(repairedIdx) == true else {
            return XCTFail("repair hypothesis missing: \(cands)")
        }
        _ = svc.commit(session: s, index: repairedIdx)
        svc.endSession(session: s)
        // 誤読み・修復読みのどちらのキーでも記録されていない
        XCTAssertNil(svc.correctionLookupForTesting(reading: "してくだっさい"))
        XCTAssertNil(svc.correctionLookupForTesting(reading: "してください"))
    }

    func testDateTemplateCommitIsNotRecorded() throws {
        // spec エッジ表「日付テンプレート候補」: isLearningTarget=false の展開候補を訂正として
        // 記録すると、展開済みの古い日付が恒久1位化する。既存 testDateTemplateExpandsToToday...
        // が「kyou で実日付候補が出る」ことを実証済みなので、その候補を index≠0 で確定して
        // 記録されないことを見る。
        svc.loadUserDictionary(from: nil)   // 組み込みテンプレートのみ
        let s = newSession(reading: "kyou")
        let formatter = DateFormatter()
        formatter.dateFormat = "yyyy年MM月dd日"
        formatter.locale = Locale(identifier: "ja_JP")
        formatter.calendar = Calendar(identifier: .gregorian)
        let today = formatter.string(from: Date())
        let cands = svc.convert(session: s)!
        guard let dateIdx = cands.firstIndex(of: today), dateIdx != 0 else {
            svc.endSession(session: s)
            throw XCTSkip("date candidate missing or at index 0: \(cands)")
        }
        _ = svc.commit(session: s, index: dateIdx)
        svc.endSession(session: s)
        XCTAssertNil(svc.correctionLookupForTesting(reading: "きょう"))
    }

    func testRecordCorrectionRejectsNonCoveringSurface() throws {
        // spec エッジ表「再変換で非被覆候補を選択」: 記録可否マップの被覆条件で棄却される
        // (fail-closed。全読みキーに部分表層が載ると昇格時の無言データ欠落=PR#8同型)。
        let s = svc.startSession()
        let cands = svc.reconvert(session: s, surface: "きょうははれです")!
        svc.endSession(session: s)
        let recordable = Set(svc.recordableSurfacesForTesting(reading: "きょうははれです"))
        guard let nonCovering = cands.first(where: { !recordable.contains($0) }) else {
            throw XCTSkip("all reconvert candidates are covering for this dictionary output")
        }
        svc.recordCorrection(reading: "きょうははれです", surface: nonCovering)
        XCTAssertNil(svc.correctionLookupForTesting(reading: "きょうははれです"))
    }

    func testRecordCorrectionRequiresRecordabilityMap() {
        // fail-closed: convert/reconvert を経ていない読みは記録されない
        svc.recordCorrection(reading: "みこみっと", surface: "未コミット")
        XCTAssertNil(svc.correctionLookupForTesting(reading: "みこみっと"))
    }

    func testRecordCorrectionAcceptsRecordableSurfaceFromReconvertList() {
        let s = svc.startSession()
        let cands = svc.reconvert(session: s, surface: "にほんご")!
        svc.endSession(session: s)
        // 記録可(=isLearningTarget かつ全被覆)な表層をマップ観測窓から選ぶ。
        // cands[1] 固定にしないのは、リスト末尾に部分被覆候補が連結されるため
        // index が被覆かどうかは辞書データ依存だから。
        let recordable = svc.recordableSurfacesForTesting(reading: "にほんご")
        // 素の1位と同じ表層だと最終アサートが昇格と無関係に成立する(部分的偽緑)ため、
        // 非1位の記録可表層が無ければ fail に倒す。
        guard let surface = recordable.first(where: { $0 != cands.first }) else {
            return XCTFail("no recordable non-top surface in reconvert output: \(recordable)")
        }
        svc.recordCorrection(reading: "にほんご", surface: surface)
        XCTAssertEqual(svc.correctionLookupForTesting(reading: "にほんご"), surface)

        let s2 = newSession(reading: "nihongo")
        XCTAssertEqual(svc.convert(session: s2)!.first, surface)
        svc.endSession(session: s2)
    }

    func testRecordCorrectionDoesNotFeedLearning() {
        // spec §2(b): RecordCorrection は updateLearningData を呼ばない(偽 bigram 防止)。
        // 観測: 記録後に昇格テーブルだけ消すと、候補列は素の並びへ戻る
        // (学習に乗っていれば表層が浮上して並びが変わる)。
        let s0 = newSession(reading: "nihongo")
        let base = svc.convert(session: s0)!
        svc.endSession(session: s0)

        let s = svc.startSession()
        _ = svc.reconvert(session: s, surface: "にほんご")!
        svc.endSession(session: s)
        let recordable = svc.recordableSurfacesForTesting(reading: "にほんご")
        guard let surface = recordable.first(where: { $0 != base.first }) else {
            return XCTFail("no recordable non-top surface")
        }
        svc.recordCorrection(reading: "にほんご", surface: surface)
        svc.clearCorrectionsForTesting()

        let s2 = newSession(reading: "nihongo")
        XCTAssertEqual(svc.convert(session: s2)!.first, base.first)   // 学習は動いていない
        svc.endSession(session: s2)
    }

    func testReconvertListIsPromoted() {
        // spec §3(b): 前回の訂正が再変換の初期表示1位に出る。
        svc.recordForTesting(reading: "にほんご", surface: "似本languages")
        let s = svc.startSession()
        let cands = svc.reconvert(session: s, surface: "にほんご")!
        XCTAssertEqual(cands.first, "似本languages")
        XCTAssertEqual(cands.filter { $0 == "似本languages" }.count, 1)
        svc.endSession(session: s)
    }

    func testClearLearningRemovesPromotion() {
        let s1 = newSession(reading: "nihongo")
        _ = svc.convert(session: s1)!
        let r = svc.commit(session: s1, index: 1)!
        XCTAssertTrue(r.reading.isEmpty, "index1 が全被覆でない辞書出力なら読みを変える")
        svc.endSession(session: s1)
        XCTAssertTrue(svc.clearLearning())
        XCTAssertNil(svc.correctionLookupForTesting(reading: "にほんご"))   // 主アサート=観測窓
        XCTAssertFalse(FileManager.default.fileExists(
            atPath: dir.appendingPathComponent("corrections.json").path))
    }

    func testDisabledLearningIsFullNoop() {
        let off = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let s = off.startSession()
        _ = off.insert(session: s, text: "nihongo")
        let cands = off.convert(session: s)!
        _ = off.commit(session: s, index: 1)
        off.endSession(session: s)
        off.recordCorrection(reading: "にほんご", surface: "任意")
        XCTAssertNil(off.correctionLookupForTesting(reading: "にほんご"))
        let s2 = off.startSession()
        _ = off.insert(session: s2, text: "nihongo")
        XCTAssertEqual(off.convert(session: s2)!.first, cands.first)
        off.endSession(session: s2)
    }

    func testLivePromotionChangesDisplayAndCommit0() {
        // 学習効果と分離するため、学習に乗り得ない合成表層を recordForTesting で直接仕込む
        // (通常 commit(index:1) で作ると updateLearningData も走り、昇格未実装でも
        // 表示が変わって偽緑になる)。
        let alien = "似本languages"
        svc.recordForTesting(reading: "にほんご", surface: alien)

        let s = newSession(reading: "nihongo")
        let live = svc.liveConvert(session: s)!
        XCTAssertEqual(live.text, alien)            // ライブ表示が訂正表層になる
        XCTAssertNil(live.committed)
        let r = svc.commit(session: s, index: 0)!   // Enter 相当 = cache 先頭
        XCTAssertEqual(r.text, alien)               // 表示と確定が一致
        XCTAssertEqual(r.reading, "")
        svc.endSession(session: s)
    }

    func testLiveAutoCommitTurnSkipsPromotion() {
        // 自動確定が発火した呼び出しでは昇格が引っ込む(spec §3(c)3)。長文読みの
        // 「読み全体」キーに合成表層を仕込み、length バックストップを発火させて、
        // 表示にも確定にも合成表層が混ざらないことを確認する。
        let alien = "似本languages"
        let s = svc.startSession()
        // autoCommitMaxReading(既定25)超の読みで length トリガを決定的に発火させる:
        // "kyouhaharewokitai" は 10 かな → ×3 = 30 かな > 25。
        let reading = svc.insert(
            session: s, text: String(repeating: "kyouhaharewokitai", count: 3))!
        XCTAssertGreaterThan(reading.count, 25)      // バックストップ前提の確認
        svc.recordForTesting(reading: reading, surface: alien)
        let live = svc.liveConvert(session: s, allowAutoCommit: true)!
        guard let committed = live.committed else {
            svc.endSession(session: s)
            return XCTFail("length backstop did not fire (reading=\(reading.count) kana)")
        }
        XCTAssertNotEqual(committed, alien)          // 確定はモデル候補基準
        XCTAssertFalse(live.text.contains(alien))    // 残り表示にも昇格表層が混ざらない
        // spec §3(c)1: 自動確定回は cache を空に保つ(短縮後読みで stale 候補を載せない)
        XCTAssertNil(svc.commit(session: s, index: 0))
        svc.endSession(session: s)
    }

    func testModelTopCommitAfterPromotionRemovesCorrection() throws {
        // 昇格発火時は表示 index 0=昇格候補・index 1=モデル1位。commit の「index != 0 =訂正」
        // 基準線が昇格で破れると、モデル1位の選択が訂正記録され既存訂正を上書き破壊する
        // （文節経路は表層基準(cf6aca3)で除外済み — 同じ操作の文レベル版）。
        // 記録しないだけでは誤登録した訂正の除去手段が ClearLearning(全消し)しか無くなるため、
        // モデル1位の明示選択は「昇格の拒否」として既存訂正を削除する(un-learn)。
        let alien = "似本languages"
        svc.recordForTesting(reading: "にほんご", surface: alien)
        let s = newSession(reading: "nihongo")
        let cands = svc.convert(session: s)!
        XCTAssertEqual(cands.first, alien)              // 前提: 昇格が発火している
        XCTAssertGreaterThan(cands.count, 1)
        let r = try XCTUnwrap(svc.commit(session: s, index: 1))   // モデル1位を明示選択
        XCTAssertEqual(r.text, cands[1])
        XCTAssertTrue(r.reading.isEmpty, "index1 が全被覆でない辞書出力なら読みを変える")
        svc.endSession(session: s)
        XCTAssertNil(svc.correctionLookupForTesting(reading: "にほんご"),
                     "モデル1位の明示選択は訂正を削除する(記録でも温存でもなく)")
        // 削除の帰結: 次の変換は昇格なしのモデル順に戻る。
        let s2 = newSession(reading: "nihongo")
        XCTAssertNotEqual(svc.convert(session: s2)!.first, alien)
        svc.endSession(session: s2)
    }

    func testRecordCorrectionWithModelTopIsNeverRecorded() {
        // 再変換経路の同型: TIP は index != 0 で RecordCorrection を送るが、昇格発火時の
        // index 1 はモデル1位。エンジン側の照合でモデル1位表層は記録しない。
        let s = svc.startSession()
        let cands = svc.reconvert(session: s, surface: "にほんご")!
        svc.endSession(session: s)
        let top = cands[0]   // 訂正未登録なので = モデル1位
        // 前提の自己証明: top が既存ルール(学習対象+全被覆)では記録可であること。これが無いと
        // 非被覆棄却だけで緑になり、モデル1位除外を巻き戻しても通る偽緑になる。
        XCTAssertTrue(svc.recordableSurfacesForTesting(reading: "にほんご").contains(top),
                      "前提: モデル1位が記録可表層でないと除外基準を検証できない: \(top)")
        svc.recordCorrection(reading: "にほんご", surface: top)
        XCTAssertNil(svc.correctionLookupForTesting(reading: "にほんご"),
                     "モデル1位表層の RecordCorrection が記録された")
    }

    func testRecordCorrectionWithModelTopKeepsExistingCorrection() {
        // 再変換経路では un-learn しない: RecordCorrection の照合基盤は共有 32 件マップで、
        // 別接続の同一読み変換が modelTop を上書きし得る。stale 基準での削除は
        // fail-destructive(棄却なら再訂正で済むが、削除は訂正データの喪失)なので、
        // un-learn はセッションローカルに昇格発火を確認できる commit/文節種経路に限定する。
        // モデル1位選択は記録もせず削除もせず温存(un-learn したければ通常変換の候補窓から)。
        let alien = "似本languages"
        svc.recordForTesting(reading: "にほんご", surface: alien)
        let s = svc.startSession()
        let cands = svc.reconvert(session: s, surface: "にほんご")!
        svc.endSession(session: s)
        XCTAssertEqual(cands.first, alien)   // 前提: 昇格発火 → index 1 がモデル1位
        XCTAssertGreaterThan(cands.count, 1)
        svc.recordCorrection(reading: "にほんご", surface: cands[1])
        XCTAssertEqual(svc.correctionLookupForTesting(reading: "にほんご"), alien,
                       "再変換窓のモデル1位選択が既存訂正を消した(記録 or 削除)")
    }

    func testModelTopCommitWithoutPromotionDoesNotUnlearn() throws {
        // typoConvert 窓は昇格を挿入しない(修復ブロック+literal)ため、literal 1位(=modelTop)が
        // index>0 に来る。昇格が起きていない窓でのモデル1位選択は「昇格の拒否」ではない —
        // un-learn の誤発火で訂正が消えてはならない(第3R敵対レビュー N-1)。窓の形を注入で
        // 固定するのは、実 typoConvert の literal 1位と修復候補の text 衝突が辞書依存で
        // 決定的に作れないため。
        let alien = "似本languages"
        svc.recordForTesting(reading: "にほんご", surface: alien)
        let s = newSession(reading: "nihongo")
        func cand(_ text: String) -> Candidate {
            Candidate(text: text, value: 0, composingCount: .inputCount(7),
                      lastMid: MIDData.一般.mid,
                      data: [DicdataElement(word: text, ruby: "ニホンゴ",
                                            cid: CIDData.一般名詞.cid, mid: MIDData.一般.mid, value: 0)])
        }
        svc.cacheCandidatesForTesting(session: s, candidates: [cand("偽修復"), cand("日本語")],
                                      target: "にほんご", modelTop: "日本語", promoted: false)
        let r = try XCTUnwrap(svc.commit(session: s, index: 1))   // modelTop を index>0 で確定
        XCTAssertEqual(r.text, "日本語")
        XCTAssertTrue(r.reading.isEmpty,
                      "前提: 全消費確定でないと un-learn は remaining 条件で止まり別理由の緑になる")
        svc.endSession(session: s)
        XCTAssertEqual(svc.correctionLookupForTesting(reading: "にほんご"), alien,
                       "昇格なし窓のモデル1位確定が訂正を削除した")
    }

    func testRecordCorrectionSurvivesEightNewerReadings() {
        // 共有 persist エンジンでは、reconvert 応答→ユーザーの候補選択→RecordCorrection の間に
        // 別接続の変換が記録可否マップを進める。直近8読みで追い出されると訂正が無言で失われる。
        let s = svc.startSession()
        let cands = svc.reconvert(session: s, surface: "にほんご")!
        svc.endSession(session: s)
        let recordable = svc.recordableSurfacesForTesting(reading: "にほんご")
        guard let surface = recordable.first(where: { $0 != cands.first }) else {
            return XCTFail("no recordable non-top surface in reconvert output: \(recordable)")
        }
        for filler in ["あき", "はる", "なつ", "ふゆ", "やま", "かわ", "うみ", "そら"] {
            let f = svc.startSession()
            _ = svc.reconvert(session: f, surface: filler)
            svc.endSession(session: f)
        }
        svc.recordCorrection(reading: "にほんご", surface: surface)
        XCTAssertEqual(svc.correctionLookupForTesting(reading: "にほんご"), surface,
                       "直近8読み分の別変換が挟まっただけで記録可否マップから追い出された")
    }

    func testStaleModelTopSurvivesEntryOverwriteEndToEnd() {
        // 第4R①の e2e 回帰: 別接続の同一読み変換がエントリを上書きした後でも、ユーザーが
        // 見ていた旧1位表層の RecordCorrection は false-accept されない(modelTops の持ち越しが
        // noteRecordability→recordabilityVerdict の実配線で効くこと。純関数テストだけでは
        // carried の受け渡し脱落を検出できない — 第5R N-2)。
        func cand(_ text: String) -> Candidate {
            Candidate(text: text, value: 0, composingCount: .inputCount(4),
                      lastMid: MIDData.一般.mid,
                      data: [DicdataElement(word: text, ruby: "ニホンゴ",
                                            cid: CIDData.一般名詞.cid, mid: MIDData.一般.mid, value: 0)])
        }
        // 接続Aの窓: 1位=旧トップA。接続Bが同一読みを変換して上書き: 1位=新トップB、
        // Aは非1位として残存(=surfaces 上は記録可。ここで false-accept が起きるのが欠陥形)。
        svc.noteRecordabilityForTesting(reading: "にほんご", candidates: [cand("旧トップA"), cand("並候補C")])
        svc.noteRecordabilityForTesting(reading: "にほんご", candidates: [cand("新トップB"), cand("旧トップA"), cand("並候補C")])
        svc.recordCorrection(reading: "にほんご", surface: "旧トップA")
        XCTAssertNil(svc.correctionLookupForTesting(reading: "にほんご"),
                     "上書き前のモデル1位が false-accept され訂正として記録された")
        // 陽性対照: どちらの窓でも1位でない表層は普通に記録される(エントリ自体は生きている)。
        svc.recordCorrection(reading: "にほんご", surface: "並候補C")
        XCTAssertEqual(svc.correctionLookupForTesting(reading: "にほんご"), "並候補C")
    }

    func testMergedModelTopsCarriesRecentTopsAcrossOverwrites() {
        // 同一読みエントリの上書き(別接続の同一読み変換)で modelTop を単一値のまま差し替えると、
        // ユーザーが「1位でよい」のつもりで選んだ旧1位が record 側で false-accept され
        // 既存訂正を上書き破壊する(第4R敵対レビュー①)。直近観測の集合として持ち越す。
        XCTAssertEqual(ConversionService.mergedModelTops(new: "B", carried: ["A"]), ["B", "A"])
        XCTAssertEqual(ConversionService.mergedModelTops(new: "A", carried: ["A"]), ["A"])   // dedup
        XCTAssertEqual(ConversionService.mergedModelTops(new: nil, carried: ["A"]), ["A"])
        XCTAssertEqual(ConversionService.mergedModelTops(new: "E", carried: ["D", "C", "B", "A"]),
                       ["E", "D", "C", "B"])   // 上限4・新しい観測が先頭
    }

    func testCommitRecordPersistsOnBackgroundLane() {
        // Persistence follows the RAM update without holding up the commit caller.2個目の ConversionService を立てるのは
        // 辞書二重ロード+mmap 共有の不安定要因になるため、ファイルと fresh な
        // CorrectionStore の再読込で観測する(Store 自体の永続性は Task 2 で担保済み)。
        let s1 = newSession(reading: "nihongo")
        let cands = svc.convert(session: s1)!
        let r = svc.commit(session: s1, index: 1)!
        XCTAssertTrue(r.reading.isEmpty, "index1 が全被覆でない辞書出力なら読みを変える")
        svc.flushMaintenanceForTesting()
        // endSessionを待たずにbackground persistenceが完了している
        XCTAssertTrue(FileManager.default.fileExists(
            atPath: dir.appendingPathComponent("corrections.json").path))
        XCTAssertEqual(CorrectionStore(directory: dir).lookup(reading: "にほんご"), cands[1])
        svc.endSession(session: s1)
    }
}
