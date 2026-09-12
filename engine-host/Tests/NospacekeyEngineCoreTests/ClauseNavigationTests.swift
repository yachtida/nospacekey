import Foundation
import XCTest
import KanaKanjiConverterModuleWithDefaultDictionary
@testable import NospacekeyEngineCore

/// 文節ナビゲーション（変換中の←/→）: MoveClause / SelectClauseCandidate / CommitClauses。
/// 変換は classic 固定（Zenzai 無し）で決定的にする（ConversionServiceTests と同じ流儀）。
/// 前提（複数文節に割れる・候補が複数ある）は guard-return で無言スキップせず、
/// XCTUnwrap/Assert で能動的に FAIL させる（item10 の自己証明パターン。旧形は実辞書の
/// 分割結果次第で 1 つもアサートせず PASS した — マージ後敵対レビューの記録の誤り節）。
final class ClauseNavigationTests: XCTestCase {
    private func classicService() -> ConversionService {
        ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
    }

    private func makeTempDir() throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-clause-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    private func learningService(_ dir: URL) -> ConversionService {
        ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                          learning: LearningSettings(enabled: true, memoryDir: dir))
    }

    /// insert→convert 済みのセッションを作る（文節ナビゲーションの前提状態）。
    private func convertedSession(_ svc: ConversionService, roman: String) -> Int {
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: roman)
        XCTAssertNotNil(svc.convert(session: sid))
        return sid
    }

    // ---- Protocol: decode/encode（Rust 側 protocol.rs のテストと対）----

    func testDecodeMoveClause() throws {
        let json = #"{"method":"MoveClause","params":{"session":7,"offset":1,"base_index":2}}"#.data(using: .utf8)!
        let req = try JSONDecoder().decode(Request.self, from: json)
        guard case let .moveClause(session, offset, baseIndex, leftContext) = req else {
            return XCTFail("not moveClause: \(req)")
        }
        XCTAssertEqual(session, 7)
        XCTAssertEqual(offset, 1)
        XCTAssertEqual(baseIndex, 2)
        XCTAssertNil(leftContext)
        XCTAssertEqual(req.sessionId, 7)
    }

    func testDecodeMoveClauseWithContextAndNegativeOffset() throws {
        let json = #"{"method":"MoveClause","params":{"session":7,"offset":-1,"base_index":0,"left_context":"私の"}}"#.data(using: .utf8)!
        let req = try JSONDecoder().decode(Request.self, from: json)
        guard case let .moveClause(_, offset, _, leftContext) = req else {
            return XCTFail("not moveClause: \(req)")
        }
        XCTAssertEqual(offset, -1)
        XCTAssertEqual(leftContext, "私の")
    }

    func testDecodeSelectClauseCandidateAndCommitClauses() throws {
        let sel = #"{"method":"SelectClauseCandidate","params":{"session":7,"index":3}}"#.data(using: .utf8)!
        let selReq = try JSONDecoder().decode(Request.self, from: sel)
        guard case let .selectClauseCandidate(session, index) = selReq else {
            return XCTFail("not selectClauseCandidate: \(selReq)")
        }
        XCTAssertEqual(session, 7)
        XCTAssertEqual(index, 3)
        XCTAssertEqual(selReq.sessionId, 7)

        let commit = #"{"method":"CommitClauses","params":{"session":7}}"#.data(using: .utf8)!
        let commitReq = try JSONDecoder().decode(Request.self, from: commit)
        guard case let .commitClauses(session2) = commitReq else {
            return XCTFail("not commitClauses: \(commitReq)")
        }
        XCTAssertEqual(session2, 7)
        XCTAssertEqual(commitReq.sessionId, 7)
    }

    func testEncodeClauseView() throws {
        let res = Response.clauseView(
            segments: ["今日は", "いい天気です"], selected: 1,
            candidates: ["いい天気です", "良い天気です"], candidateIndex: 0)
        let data = try JSONEncoder().encode(res)
        let obj = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "ClauseView")
        XCTAssertEqual(obj["segments"] as? [String], ["今日は", "いい天気です"])
        XCTAssertEqual(obj["selected"] as? Int, 1)
        XCTAssertEqual(obj["candidates"] as? [String], ["いい天気です", "良い天気です"])
        XCTAssertEqual(obj["candidate_index"] as? Int, 0)
    }

    // ---- MoveClause: 開始・移動・クランプ ----

    func testMoveClauseEntersClauseModeAfterConvert() throws {
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        let view = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0),
                                 "clause view expected after convert")
        XCTAssertEqual(view.selected, 0)
        // 種の契約: ビューが返った ⇒ 2 文節以上（1 文節は settle 劣化のため nil を返す）。
        XCTAssertGreaterThanOrEqual(view.segments.count, 2,
                                    "moveClause は 2 文節未満を文節モードにしない契約")
        XCTAssertFalse(view.candidates.isEmpty)
        // 候補窓の初期選択は「見えている文節」= segments[selected] と同一文字列（表示と選択の一致契約）。
        XCTAssertEqual(view.candidates[view.candidateIndex], view.segments[view.selected])
    }

    func testMoveClauseMovesAndClampsAtEnds() throws {
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        let first = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0),
                                  "clause view expected")
        XCTAssertGreaterThanOrEqual(first.segments.count, 2, "種の契約により必ず複数文節")
        // 左端で ← はクランプ（selected=0 のまま）。
        let clampedLeft = svc.moveClause(session: sid, offset: -1, baseIndex: 0)
        XCTAssertEqual(clampedLeft?.selected, 0)
        let moved = try XCTUnwrap(svc.moveClause(session: sid, offset: 1, baseIndex: 0))
        XCTAssertEqual(moved.selected, 1)
        XCTAssertEqual(moved.candidates[moved.candidateIndex], moved.segments[1])
        // 右端を超える offset はクランプ。
        let clampedRight = svc.moveClause(session: sid, offset: 999, baseIndex: 0)
        XCTAssertEqual(clampedRight?.selected, first.segments.count - 1)
    }

    func testMoveClauseWithoutConvertReturnsNil() {
        let svc = classicService()
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "nihongo")
        XCTAssertNil(svc.moveClause(session: sid, offset: 1, baseIndex: 0), "convert 前は候補キャッシュが無い")
    }

    func testMoveClauseUnknownSessionReturnsNil() {
        let svc = classicService()
        XCTAssertNil(svc.moveClause(session: 999, offset: 1, baseIndex: 0))
    }

    func testMoveClauseStaleAfterInsertReturnsNil() {
        // convert 後に読みが変わったら stale（commit の stale ガードと同じ規律）。
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouha")
        _ = svc.insert(session: sid, text: "a")
        XCTAssertNil(svc.moveClause(session: sid, offset: 0, baseIndex: 0))
    }

    // ---- MoveClause: 種の規律（敵対レビュー① — 選択の破棄防止）----

    func testMoveClauseDoesNotFallBackFromInvalidBaseIndex() {
        // 種にできない baseIndex で「最初の被覆候補」へ乗り換えると、ユーザーが選んでいる候補と
        // 別の変換へ preedit が黙って差し替わり、Enter がそれを確定する。乗り換えず nil が契約。
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        XCTAssertNil(svc.moveClause(session: sid, offset: 0, baseIndex: 9999))
        XCTAssertNil(svc.moveClause(session: sid, offset: 0, baseIndex: -1))
    }

    func testMoveClauseRejectsRepairedSeed() throws {
        // Tab（修正変換）の修復候補を選んだ状態の ←/→ は文節モードに入らず settle 劣化する
        // （修復候補は literal 読みを被覆しない — 乗り換えると Tab で直したタイポが復活する）。
        let svc = classicService()
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "shitekudassai")
        let cands = try XCTUnwrap(svc.typoConvert(session: sid))
        let repairedIdx = try XCTUnwrap(cands.firstIndex(of: "してください"),
                                        "前提: 修復仮説が出るはず（ConversionServiceTypoTests と同素材）")
        XCTAssertEqual(svc.typoRepairedIndices[sid]?.contains(repairedIdx), true,
                       "前提: 修復ブロック由来の index であるはず")
        XCTAssertNil(svc.moveClause(session: sid, offset: 1, baseIndex: repairedIdx))
    }

    func testMoveClauseRejectsSingleClauseSeed() {
        // 真の1文節は移動先が無い（辞書境界の再導出でも 1 文節にしかならず settle 劣化）。
        let svc = classicService()
        let sid = convertedSession(svc, roman: "ki")   // 読み1文字 = 構造的に 1 文節
        XCTAssertNil(svc.moveClause(session: sid, offset: 1, baseIndex: 0))
    }

    func testMoveClauseRederivesBoundariesForSingleElementSeed() throws {
        // 学習メモリの全文1エントリ（単一 DicdataElement）は境界情報を持たない — 実機受入で
        // 「よく打つ文ほど ←/→ が確定になる」として発覚（settle 確定がさらに学習を強化する
        // 自己強化ループ）。同一表層を学習抜きの辞書変換から引き直して文節モードに入ること。
        let svc = classicService()
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "kyouhaiitenkidesu")
        let texts = try XCTUnwrap(svc.convert(session: sid))
        // 実辞書が本当に出す全被覆表層を種にする（合成文字列だと照合が辞書データ依存の偽REDになる）。
        let full = try XCTUnwrap(
            (0..<texts.count).first { (svc.commitProbeRemaining(session: sid, index: $0) ?? "x").isEmpty },
            "full-coverage candidate expected")
        let surface = texts[full]
        let learned = Candidate(
            text: surface,
            value: 100,
            composingCount: .inputCount(17),
            lastMid: MIDData.一般.mid,
            data: [DicdataElement(word: surface, ruby: "キョウハイイテンキデス",
                                  cid: CIDData.固有名詞.cid, mid: MIDData.一般.mid, value: 0)])
        svc.cacheCandidatesForTesting(session: sid, candidates: [learned], target: "きょうはいいてんきです")
        let view = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0),
                                 "単一要素種でも辞書境界の再導出で文節モードに入る契約")
        XCTAssertGreaterThanOrEqual(view.segments.count, 2)
        XCTAssertEqual(view.segments.joined(), surface, "再導出は表層を変えない")
    }

    func testMoveClauseEntersClauseModeForLearnedWholeSentence() throws {
        // 決定実験(第2巡敵対レビュー): 実機バグの再現そのもの — 全文を確定して学習させた読みを
        // 再変換し、←/→ で文節モードに入れること。直前 convert のラティスが surface 再利用で
        // 引き継がれると、学習の全文ノードが noLearning でも生き残り再導出が不発になる。
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir)
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "kyouhaiitenkidesu")
        let texts = try XCTUnwrap(svc.convert(session: sid))
        let full = try XCTUnwrap(
            (0..<texts.count).first { (svc.commitProbeRemaining(session: sid, index: $0) ?? "x").isEmpty },
            "full-coverage candidate expected")
        _ = svc.commit(session: sid, index: full)
        svc.endSession(session: sid)   // 長期メモリへ flush = 全文1エントリが生まれる
        let sid2 = svc.startSession()
        _ = svc.insert(session: sid2, text: "kyouhaiitenkidesu")
        XCTAssertNotNil(svc.convert(session: sid2))
        // 前提の自己証明: 学習の全文1エントリが1位に来ている(これが崩れると本テストは
        // 通常経路と同義の空テストへ退化して静かに緑になる)。
        XCTAssertEqual(svc.cachedTopElementCountForTesting(session: sid2), 1,
                       "前提: 学習の全文1エントリ(単一要素)が変換1位")
        let view = try XCTUnwrap(svc.moveClause(session: sid2, offset: 0, baseIndex: 0),
                                 "学習済みの文でも文節モードに入れる(再導出の実効性)")
        XCTAssertGreaterThanOrEqual(view.segments.count, 2)
        svc.endSession(session: sid2)
    }

    func testLearningSurvivesFailedBoundaryRederivation() throws {
        // 再導出の noLearning は converter の永続 learningType を書き換える(vendor の
        // updateIfRequired)。失敗パス(真の1文節)で復元しないと、直後の確定学習と
        // endSession の flush が無言 no-op になる — 「←/→ を押しただけで以後の学習が
        // 消える」回帰を固定する(敵対レビュー Critical-1)。
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir)
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "ki")
        let cands = try XCTUnwrap(svc.convert(session: sid))
        XCTAssertGreaterThan(cands.count, 1, "同読み多候補の前提: \(cands)")
        XCTAssertNil(svc.moveClause(session: sid, offset: 1, baseIndex: 0),
                     "真の1文節は再導出も失敗して settle 劣化(残置バグの窓を開く前提)")
        let learned = cands[1]
        XCTAssertEqual(try XCTUnwrap(svc.commit(session: sid, index: 1)).text, learned)
        svc.endSession(session: sid)
        svc.flushMaintenanceForTesting()
        // 候補順位は訂正昇格(CorrectionStore)でも動くため、vendor 学習の生死は
        // ファイル生成で判定する — .nothing 残置だと save() が needUpdateMemory guard で
        // skip されファイルが作られない(昇格と無関係な決定的判別子)。
        let files = try FileManager.default.contentsOfDirectory(atPath: dir.path)
        XCTAssertTrue(files.contains("memory.louds"),
                      "moveClause 失敗後の endSession で学習ファイルが生成される(learningType の復元): \(files)")
        let sid2 = svc.startSession()
        _ = svc.insert(session: sid2, text: "ki")
        let cands2 = try XCTUnwrap(svc.convert(session: sid2))
        svc.endSession(session: sid2)
        XCTAssertEqual(cands2.first, learned,
                       "moveClause 失敗後の確定が学習に乗る(learningType の復元)")
    }

    func testMoveClauseStillRejectsUnderivableSingleElementSeed() {
        // 辞書が再現できない表層（学習にしか無い綴り）は再導出できず settle 劣化のまま。
        // 再導出が「同一表層」以外へ緩むと、見えている変換と別の文が文節モードに出る退行になる。
        let svc = classicService()
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "kyouhaiitenkidesu")
        let learned = Candidate(
            text: "キョウ晴イイ天キ㍑",
            value: 100,
            composingCount: .inputCount(17),
            lastMid: MIDData.一般.mid,
            data: [DicdataElement(word: "キョウ晴イイ天キ㍑", ruby: "キョウハイイテンキデス",
                                  cid: CIDData.固有名詞.cid, mid: MIDData.一般.mid, value: 0)])
        svc.cacheCandidatesForTesting(session: sid, candidates: [learned], target: "きょうはいいてんきです")
        XCTAssertNil(svc.moveClause(session: sid, offset: 0, baseIndex: 0))
    }

    func testMoveClauseRejectsPartialCoverageSeed() throws {
        // 第2R敵対レビュー③: 範囲内だが非被覆（前方一致）の baseIndex を拒否する covers guard を
        // 個別に固定する — 範囲外テストは手前の範囲 guard で、修復テストは covers でも落ちるため、
        // 旧フォールバックへの退行をどちらも検知できない。部分被覆候補の index は辞書データ依存
        // なので probe で探し、無ければ明示スキップ（CorrectionTests の同型パターン）。
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouhaharedesu")
        let cands = try XCTUnwrap(svc.convert(session: sid))
        let partial = (0..<cands.count).first {
            (svc.commitProbeRemaining(session: sid, index: $0) ?? "").isEmpty == false
        }
        guard let i = partial else { throw XCTSkip("no partial candidate in dictionary output") }
        XCTAssertNil(svc.moveClause(session: sid, offset: 0, baseIndex: i),
                     "非被覆候補を種に文節モードへ入ると、選択と別の変換へ差し替わる退行（旧①）")
    }

    // ---- MoveClause: 再推論の抑止（敵対レビュー③ — クランプ即返し・文節候補キャッシュ）----

    func testClampAndRevisitDoNotReinfer() throws {
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        let first = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0))
        XCTAssertGreaterThanOrEqual(first.segments.count, 2, "種の契約により必ず複数文節")
        let afterEntry = svc.clauseInferenceCountForTesting
        // 端クランプ（選択が動かない矢印）は再推論しない — OnKeyDown が同期 IPC で待つため、
        // 押しっぱなしの毎打鍵推論は IME ブロックとタイムアウト→接続断に直結する。
        let clamped = try XCTUnwrap(svc.moveClause(session: sid, offset: -1, baseIndex: 0))
        XCTAssertEqual(clamped.selected, 0)
        XCTAssertEqual(svc.clauseInferenceCountForTesting, afterEntry, "クランプで再推論しない")
        // 隣の文節へは推論 1 回、戻りはキャッシュで 0 回。
        _ = try XCTUnwrap(svc.moveClause(session: sid, offset: 1, baseIndex: 0))
        let afterMove = svc.clauseInferenceCountForTesting
        XCTAssertEqual(afterMove, afterEntry + 1)
        let back = try XCTUnwrap(svc.moveClause(session: sid, offset: -1, baseIndex: 0))
        XCTAssertEqual(back.selected, 0)
        XCTAssertEqual(svc.clauseInferenceCountForTesting, afterMove, "再訪はキャッシュから返す")
    }

    func testSelectInvalidatesDownstreamCandidateCache() throws {
        // 候補差し替えは後続文節の左文脈（先行文節表層）を変える＝後続のキャッシュは捨てて
        // 再推論する。捨てないと差し替え前の文脈で推論済みの候補列が並ぶ（第2R敵対レビュー⑤）。
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        let view = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0))
        XCTAssertGreaterThanOrEqual(view.candidates.count, 2,
                                    "前提: 先頭文節の候補が 2 つ以上（実辞書で 1 つなら素材の読みを変える）")
        _ = try XCTUnwrap(svc.moveClause(session: sid, offset: 1, baseIndex: 0))    // 文節1を推論
        _ = try XCTUnwrap(svc.moveClause(session: sid, offset: -1, baseIndex: 0))   // 文節0へ戻る（キャッシュ）
        let before = svc.clauseInferenceCountForTesting
        let other = (view.candidateIndex + 1) % view.candidates.count
        _ = try XCTUnwrap(svc.selectClauseCandidate(session: sid, index: other))    // 文節0を差し替え
        _ = try XCTUnwrap(svc.moveClause(session: sid, offset: 1, baseIndex: 0))    // 文節1へ再移動
        XCTAssertEqual(svc.clauseInferenceCountForTesting, before + 1,
                       "差し替え後の後続文節はキャッシュでなく再推論されるはず")
    }

    func testInsertDropsClauseState() {
        // 文節モード開始後の insert は文節状態ごと無効化する（invalidateCandidateCache 連動）。
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        XCTAssertNotNil(svc.moveClause(session: sid, offset: 0, baseIndex: 0))
        _ = svc.insert(session: sid, text: "a")
        XCTAssertNil(svc.moveClause(session: sid, offset: 0, baseIndex: 0))
        XCTAssertNil(svc.selectClauseCandidate(session: sid, index: 0))
        XCTAssertNil(svc.commitClauses(session: sid))
    }

    func testConvertResetsClauseState() throws {
        // 文節モード中に再 convert（Esc→Space 等）したら文節ナビゲーションは仕切り直し
        // （cacheCandidates が clauseState を落とす）。
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        let first = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0))
        XCTAssertGreaterThanOrEqual(first.segments.count, 2, "種の契約により必ず複数文節")
        _ = svc.moveClause(session: sid, offset: 1, baseIndex: 0)
        XCTAssertNotNil(svc.convert(session: sid))
        let fresh = svc.moveClause(session: sid, offset: 0, baseIndex: 0)
        XCTAssertEqual(fresh?.selected, 0, "再 convert 後は先頭文節から")
    }

    // ---- SelectClauseCandidate ----

    func testSelectClauseCandidateReplacesOnlySelectedSegment() throws {
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        let view = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0),
                                 "clause view expected")
        XCTAssertGreaterThanOrEqual(view.candidates.count, 2,
                                    "前提: 先頭文節の候補が 2 つ以上（実辞書で 1 つなら素材の読みを変える）")
        let other = (view.candidateIndex + 1) % view.candidates.count
        let updated = try XCTUnwrap(svc.selectClauseCandidate(session: sid, index: other),
                                    "clause view expected")
        XCTAssertEqual(updated.segments[view.selected], view.candidates[other], "選択文節の表層が差し替わる")
        XCTAssertEqual(updated.candidateIndex, other)
        // 他の文節は不変。
        for i in view.segments.indices where i != view.selected {
            XCTAssertEqual(updated.segments[i], view.segments[i], "非選択文節は不変")
        }
    }

    func testSelectClauseCandidateOutOfRangeReturnsNil() throws {
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        let view = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0),
                                 "clause view expected")
        XCTAssertNil(svc.selectClauseCandidate(session: sid, index: view.candidates.count))
        XCTAssertNil(svc.selectClauseCandidate(session: sid, index: -1))
    }

    func testSelectClauseCandidateWithoutClauseModeReturnsNil() {
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouha")
        XCTAssertNil(svc.selectClauseCandidate(session: sid, index: 0), "MoveClause 前は文節状態が無い")
    }

    // ---- CommitClauses ----

    func testCommitClausesReturnsJoinedSegmentsAndConsumesReading() throws {
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        let view = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0),
                                 "clause view expected")
        let committed = try XCTUnwrap(svc.commitClauses(session: sid), "commit expected")
        XCTAssertEqual(committed.text, view.segments.joined(), "確定 = 全文節表層の連結")
        XCTAssertEqual(committed.reading, "", "全消費")
        // 確定後は読みが空（次の insert がまっさらから始まる）。
        XCTAssertEqual(svc.insert(session: sid, text: "a"), "あ")
    }

    func testCommitClausesAfterCandidateChangeUsesReplacedSurface() throws {
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        let view = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0),
                                 "clause view expected")
        XCTAssertGreaterThanOrEqual(view.candidates.count, 2,
                                    "前提: 先頭文節の候補が 2 つ以上（実辞書で 1 つなら素材の読みを変える）")
        let other = (view.candidateIndex + 1) % view.candidates.count
        let updated = try XCTUnwrap(svc.selectClauseCandidate(session: sid, index: other),
                                    "clause view expected")
        let committed = try XCTUnwrap(svc.commitClauses(session: sid))
        XCTAssertEqual(committed.text, updated.segments.joined(), "差し替え後の表層で確定される")
        // 中核契約: 候補を差し替えたら確定文字列も変わる（候補は text で dedup 済み＝必ず別文字列）。
        XCTAssertNotEqual(committed.text, view.segments.joined(),
                          "差し替え前の連結が確定されるなら選択が捨てられている")
    }

    func testCommitClausesWithoutClauseModeReturnsNil() {
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouha")
        XCTAssertNil(svc.commitClauses(session: sid), "MoveClause 前は文節状態が無い")
    }

    // ---- 訂正昇格との接続（敵対レビュー② — 文節経路だけ「学習が効かない」が再発しないこと）----

    func testCommitClausesRecordsNonTopClauseChoice() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir)
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        let view = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0))
        XCTAssertGreaterThanOrEqual(view.candidates.count, 2,
                                    "前提: 先頭文節の候補が 2 つ以上（実辞書で 1 つなら素材の読みを変える）")
        let readings = try XCTUnwrap(svc.clauseReadingsForTesting(session: sid))
        // 記録される選択 = 「元の表層でもモデル1位でもない、学習対象の候補」。固定 index に
        // しないのは、実辞書は日付テンプレート等 isLearningTarget=false の候補を上位に混ぜる
        // ことがあり、正しい実装のまま永久 RED になるため（item48 の禁止値と同じ落とし穴）。
        // 期待値は文節候補の観測窓から直接計算する（共有 recordability マップは reconvert 専用 —
        // 文節ナビが書き込むと同一読みの reconvert エントリを壊すため書かない）。
        let recordable = Set(try XCTUnwrap(svc.clauseRecordableSurfacesForTesting(session: sid)))
        let modelTops = try XCTUnwrap(svc.clauseModelTopsForTesting(session: sid))
        let pick = try XCTUnwrap(
            (0..<view.candidates.count).first {
                let t = view.candidates[$0]
                return recordable.contains(t) && t != view.segments[0] && t != modelTops[0]
            },
            "前提: 元表層・モデル1位以外に学習対象の文節候補があるはず: \(view.candidates)")
        let updated = try XCTUnwrap(svc.selectClauseCandidate(session: sid, index: pick))
        _ = try XCTUnwrap(svc.commitClauses(session: sid))
        // 観測窓で直接見る（黒箱の並び比較は学習効果だけでも成立し偽緑 — CorrectionTests と同じ理由）。
        XCTAssertEqual(svc.correctionLookupForTesting(reading: readings[0]), updated.segments[0],
                       "元表層・モデル1位以外を明示選択した文節は訂正として記録される")
    }

    func testCommitClausesDoesNotRecordUntouchedOrTopChoice() throws {
        // 触っていない文節と、先頭候補（元表層 or モデル1位）を明示選択した文節は記録しない
        // （文脈依存語の固定回避 — spec §2(a) の除外を表層基準で移植）。
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir)
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        _ = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0))
        let readings = try XCTUnwrap(svc.clauseReadingsForTesting(session: sid))
        _ = try XCTUnwrap(svc.selectClauseCandidate(session: sid, index: 0))   // 先頭候補の明示選択
        _ = try XCTUnwrap(svc.commitClauses(session: sid))
        for r in readings {
            XCTAssertNil(svc.correctionLookupForTesting(reading: r), "reading=\(r) が誤記録されている")
        }
    }

    func testReselectingShownSurfaceIsNotRecorded() throws {
        // 第2R敵対レビュー①: 候補を眺めて元の表層へ選び直しただけ（画面上は何も変わらない）で
        // 訂正記録されると、その読みの全変換が全文脈で固定され、既存の正当な訂正が上書き破壊
        // される。「最後の選択が勝つ」＝訂正候補を試してから戻せば記録なし、を固定する。
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir)
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        let view = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0))
        XCTAssertGreaterThanOrEqual(view.candidates.count, 2,
                                    "前提: 先頭文節の候補が 2 つ以上（実辞書で 1 つなら素材の読みを変える）")
        let readings = try XCTUnwrap(svc.clauseReadingsForTesting(session: sid))
        // 「試す」候補は選んだまま確定すれば必ず記録される真の訂正候補にする（モデル1位を
        // 試すのでは「最初の選択が勝つ」への退行をこのテストが見逃す）。
        let recordable = Set(try XCTUnwrap(svc.clauseRecordableSurfacesForTesting(session: sid)))
        let modelTops = try XCTUnwrap(svc.clauseModelTopsForTesting(session: sid))
        let other = try XCTUnwrap(
            (0..<view.candidates.count).first {
                let t = view.candidates[$0]
                return recordable.contains(t) && t != view.segments[0] && t != modelTops[0]
            },
            "前提: 元表層・モデル1位以外に学習対象の文節候補があるはず: \(view.candidates)")
        _ = try XCTUnwrap(svc.selectClauseCandidate(session: sid, index: other))            // 試す
        _ = try XCTUnwrap(svc.selectClauseCandidate(session: sid, index: view.candidateIndex)) // 戻す
        _ = try XCTUnwrap(svc.commitClauses(session: sid))
        XCTAssertNil(svc.correctionLookupForTesting(reading: readings[0]),
                     "表示されていた表層へ戻しただけの選択が訂正記録されている")
    }

    func testSentenceLevelRejectionSurvivesClauseCommit() throws {
        // 第2R敵対レビュー②: 文候補窓で 1 位以外を選択（=訂正）→ 矢印で文節モード進入 →
        // 文節候補に触れず Enter。直接 Enter なら commit() が記録した文レベル訂正が、矢印
        // 1 打を挟んだだけで消えてはならない。
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir)
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "kyouhaiitenkidesu")
        let cands = try XCTUnwrap(svc.convert(session: sid))
        let reading = "きょうはいいてんきです"
        let recordable = Set(svc.recordableSurfacesForTesting(reading: reading))
        // commit(index:) が記録するはずの選択 = 1 位以外・全被覆・学習対象。moveClause の種にも
        // なれる（全被覆）候補を実データから探す。
        let j = try XCTUnwrap(
            (1..<cands.count).first {
                recordable.contains(cands[$0])
                    && (svc.commitProbeRemaining(session: sid, index: $0) ?? "x").isEmpty
            },
            "前提: 1位以外に全被覆の学習対象候補があるはず: \(cands)")
        guard svc.moveClause(session: sid, offset: 0, baseIndex: j) != nil else {
            // 全被覆でも 1 文節にしか分解されない候補は種になれない（仕様どおり settle 劣化）。
            // その場合この経路自体が存在しないので、前提不成立として明示スキップする。
            throw XCTSkip("candidate at \(j) does not decompose into 2+ clauses")
        }
        _ = try XCTUnwrap(svc.commitClauses(session: sid))
        XCTAssertEqual(svc.correctionLookupForTesting(reading: reading), cands[j],
                       "文候補窓の 1 位拒否が、文節モードを経由した確定で失われている")
    }

    func testClauseCandidatesIncludePromotedCorrection() throws {
        // 記録済みの訂正が文節候補の先頭にも昇格される（convert/reconvert と同じ規律）。
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir)
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        _ = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0))
        let readings = try XCTUnwrap(svc.clauseReadingsForTesting(session: sid))
        let alien = "今日葉@"   // 辞書に無い表層 = 学習効果と混同しない（CorrectionTests と同じ流儀）
        svc.recordForTesting(reading: readings[0], surface: alien)
        XCTAssertNotNil(svc.convert(session: sid))   // 仕切り直し（clauseState と文節候補キャッシュを破棄）
        let view = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0))
        XCTAssertEqual(view.candidates.first, alien, "訂正昇格が文節候補にも効く: \(view.candidates)")
        XCTAssertEqual(view.candidates[view.candidateIndex], view.segments[0], "初期選択は見えている文節のまま")
    }

    func testClauseSeedFromPromotedModelTopRemovesCorrection() throws {
        // 昇格発火時の候補窓は index 0=昇格候補・index 1=モデル1位。モデル1位を選んだ状態で
        // 文節モードへ入り、触らず確定しても「1位拒否」ではない — baseIndex != 0 だけを見ると
        // モデル1位が sentenceCorrection として記録され、既存訂正を上書き破壊する
        // （commit() と同じ基準線の破れの moveClause 種版）。帰結も commit() と揃える:
        // モデル1位のまま確定 = 昇格の拒否として既存訂正を削除する(un-learn)。
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = learningService(dir)
        let alien = "今日葉いい天気@"   // 辞書に無い表層 = 学習効果と混同しない
        let reading = "きょうはいいてんきです"
        svc.recordForTesting(reading: reading, surface: alien)
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        // 前提: 昇格が発火し index 1 がモデル1位（合成昇格候補は 1 文節なので index 0 は種になれない）。
        _ = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 1),
                          "モデル1位は複数文節に分解できるはず")
        _ = try XCTUnwrap(svc.commitClauses(session: sid))
        XCTAssertNil(svc.correctionLookupForTesting(reading: reading),
                     "モデル1位種の無変更確定は訂正を削除する(上書きでも温存でもなく)")
    }

    func testClauseNavigationDoesNotTouchRecordabilityMap() throws {
        // 記録可否マップは reconvert→RecordCorrection の照合専用。文節ナビが文節読みで
        // 書き込むと、同一読みの reconvert エントリ(別接続)を丸ごと差し替えて fail-closed
        // 棄却に落とす(第2R敵対レビュー③)。文節スコープの記録可否は clauseCorrections が
        // select 時に判定済みでマップを使わない — 書き込み自体を無くす。
        let svc = classicService()
        let sid = convertedSession(svc, roman: "kyouhaiitenkidesu")
        _ = try XCTUnwrap(svc.moveClause(session: sid, offset: 0, baseIndex: 0))
        let readings = try XCTUnwrap(svc.clauseReadingsForTesting(session: sid))
        XCTAssertTrue(svc.recordableSurfacesForTesting(reading: readings[0]).isEmpty,
                      "文節ナビが共有 recordability マップへ書いた(reading=\(readings[0]))")
    }

    func testMoveClauseRejectsSeedWithTextDataMismatch() throws {
        // 日付テンプレート候補は parseTemplate が text だけを実日付へ書き換え、data.word は
        // 生タグのまま残る（vendor Candidate.makePrefixClauseCandidate の注記）。文節分解は
        // data.word から表層を再構成するため、そのまま種にすると preedit/確定が生タグへ化ける。
        // 実辞書変換でテンプレート由来の複数文節候補を決定的には作れないため、注入窓で固定する。
        let svc = classicService()
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "kyouha")   // 読み「きょうは」
        let template = Candidate(
            text: "2026年08月01日は",   // parseTemplate 展開後の text
            value: 0,
            composingCount: .inputCount(6),
            lastMid: MIDData.一般.mid,
            data: [
                DicdataElement(word: #"<date format="yyyy年MM月dd日" type="western" language="ja_JP" delta="0" deltaunit="86400">"#,
                               ruby: "キョウ", cid: CIDData.固有名詞.cid, mid: MIDData.一般.mid, value: -18),
                DicdataElement(word: "は", ruby: "ハ", cid: CIDData.一般名詞.cid, mid: MIDData.一般.mid, value: 0),
            ],
            isLearningTarget: false)   // 本番のテンプレート候補は parseTemplate が false にする
        svc.cacheCandidatesForTesting(session: sid, candidates: [template], target: "きょうは")
        XCTAssertNil(svc.moveClause(session: sid, offset: 0, baseIndex: 0),
                     "text↔data 不整合の候補が文節モードに入った（生タグが preedit/確定へ漏れる）")
        // ガードが強すぎないことの対照: text == data.word 連結の普通の候補は文節モードに入れる。
        let normal = Candidate(
            text: "今日は",
            value: 0,
            composingCount: .inputCount(6),
            lastMid: MIDData.一般.mid,
            data: [
                DicdataElement(word: "今日", ruby: "キョウ", cid: CIDData.固有名詞.cid, mid: MIDData.一般.mid, value: 0),
                DicdataElement(word: "は", ruby: "ハ", cid: CIDData.一般名詞.cid, mid: MIDData.一般.mid, value: 0),
            ])
        svc.cacheCandidatesForTesting(session: sid, candidates: [normal], target: "きょうは")
        XCTAssertNotNil(svc.moveClause(session: sid, offset: 0, baseIndex: 0),
                        "整合する通常候補まで拒否している（ガード過剰）")
    }

    // ---- ハンドラ経路（EngineHost）----

    func testHandlerMoveClauseRoundtrip() throws {
        let svc = classicService()
        let handler = makeEngineHandler(service: svc, serviceLock: NSLock())
        func send(_ json: String) throws -> [String: Any] {
            let (reply, _) = handler(1, Data(json.utf8))
            return try JSONSerialization.jsonObject(with: reply) as! [String: Any]
        }
        let started = try send(#"{"method":"StartSession"}"#)
        let sid = started["session"] as! Int
        _ = try send(#"{"method":"Insert","params":{"session":\#(sid),"text":"kyouhaiitenkidesu"}}"#)
        _ = try send(#"{"method":"Convert","params":{"session":\#(sid)}}"#)
        let view = try send(#"{"method":"MoveClause","params":{"session":\#(sid),"offset":0,"base_index":0}}"#)
        XCTAssertEqual(view["result"] as? String, "ClauseView", "unexpected: \(view)")
        XCTAssertNotNil(view["segments"] as? [String])
        // 未変換セッションへの MoveClause は Error（TIP は劣化経路へ）。
        let started2 = try send(#"{"method":"StartSession"}"#)
        let sid2 = started2["session"] as! Int
        let declined = try send(#"{"method":"MoveClause","params":{"session":\#(sid2),"offset":0,"base_index":0}}"#)
        XCTAssertEqual(declined["result"] as? String, "Error")
    }
}
