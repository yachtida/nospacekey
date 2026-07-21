import XCTest
@testable import NospacekeyEngineCore

/// SP3 ライブ変換: liveConvert(session:) は N_best=1 で「先頭候補(text)」と「現在の読み(reading)」を返す。
/// 古典モード（weightURL=nil）で検証し、Zenzai 実モデル無しでも走る。
final class LiveConvertTests: XCTestCase {
    private func makeService(autoCommit: AutoCommitStrength = .weak, autoCommitMaxReading: Int = 25) -> ConversionService {
        ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1), autoCommit: autoCommit,
                           autoCommitMaxReading: autoCommitMaxReading)
    }

    func testLiveConvertReturnsTopCandidateAndReading() {
        let svc = makeService()
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "nihongo")
        guard let r = svc.liveConvert(session: sid) else { return XCTFail("known session must not return nil") }
        XCTAssertEqual(r.reading, "にほんご")
        XCTAssertFalse(r.text.isEmpty, "live text should be non-empty")
        XCTAssertNil(r.committed, "allowAutoCommit を渡さない既定では読みを消費しない")
    }

    // ---- 自動確定（iOS nospacekey の先頭文節自動確定の移植） ----

    /// 1文字ずつ挿入しながら liveConvert(allowAutoCommit:true) を繰り返す打鍵シミュレーション。
    /// 各更新で「確定なしなら読み不変 / 確定ありなら読みが必ず縮む」の不変条件を検証し、
    /// 確定された文節列を返す。
    private func typeAndCollectAutoCommits(
        _ svc: ConversionService, session: Int, romaji: String,
        file: StaticString = #filePath, line: UInt = #line
    ) -> [String] {
        var committed: [String] = []
        for ch in romaji {
            guard let readingBefore = svc.insert(session: session, text: String(ch)) else {
                XCTFail("insert failed", file: file, line: line); return committed
            }
            guard let r = svc.liveConvert(session: session, allowAutoCommit: true) else {
                XCTFail("liveConvert failed", file: file, line: line); return committed
            }
            if let prefix = r.committed {
                XCTAssertFalse(prefix.isEmpty, "確定文節は非空", file: file, line: line)
                XCTAssertLessThan(r.reading.count, readingBefore.count,
                                  "自動確定後は残り読みが必ず縮む", file: file, line: line)
                committed.append(prefix)
            } else {
                XCTAssertEqual(r.reading, readingBefore,
                               "自動確定なしなら読みは不変", file: file, line: line)
            }
        }
        return committed
    }

    /// ultrastrong(=6): 打鍵していくと、先頭文節が直近6更新で安定した時点で自動確定が発火し、
    /// 残り読みで合成が継続する（iOS の自動確定と同じ挙動）。
    ///
    /// 入力は読点入りの文にする。裸の助詞境界（わたしは|がっこうへ 等）は辞書の複合エントリ
    /// （「は学校|ハガッコウ」「へ行き|ヘイキ」のような文節境界をまたぐ融合要素）が先頭文節に
    /// 吸着し続けて安定しないため、句読点のような硬い境界がないと発火が入力依存になる
    /// （iOS 本家 LiveConversionManager + 同一辞書でも同じ。実測: 2026-07-08）。
    func testAutoCommitFiresOnStableFirstClause() {
        let svc = makeService(autoCommit: .ultrastrong)
        let sid = svc.startSession()
        // わたしは、がっこうへいきます — 読点が文節境界を固定し「私は」が early に安定する。
        let committed = typeAndCollectAutoCommits(svc, session: sid, romaji: "watashiha,gakkouheikimasu")
        XCTAssertFalse(committed.isEmpty, "先頭文節が安定したら自動確定が発火する（ultrastrong=6）")
        // 確定後もセッションは生きており、残り読みへの追記が継続できる。
        XCTAssertNotNil(svc.insert(session: sid, text: "a"))
    }

    /// disabled: どれだけ打鍵しても自動確定は発火しない。
    func testAutoCommitDisabledNeverCommits() {
        let svc = makeService(autoCommit: .disabled)
        let sid = svc.startSession()
        let committed = typeAndCollectAutoCommits(svc, session: sid, romaji: "watashihagakkouheikimasu")
        XCTAssertTrue(committed.isEmpty, "disabled では読みを消費しない")
    }

    /// allowAutoCommit=false（Enter 直前の LiveConvert 等）は、強度設定にかかわらず読みを消費しない。
    /// エンジンが勝手に prefixComplete すると直後の Commit{0} が残り読みしか確定できなくなるため。
    func testAllowFlagFalseNeverConsumesReading() {
        let svc = makeService(autoCommit: .ultrastrong)
        let sid = svc.startSession()
        for ch in "watashihagakkouheikimasu" {
            let readingBefore = svc.insert(session: sid, text: String(ch))
            guard let r = svc.liveConvert(session: sid) else { return XCTFail("liveConvert failed") }
            XCTAssertNil(r.committed)
            XCTAssertEqual(r.reading, readingBefore)
        }
    }

    // ---- 読み長バックストップ（死のループ対策） ----

    /// 句読点なし・裸助詞境界のみの長文（先頭文節が安定しないため通常判定では発火しない —
    /// testAutoCommitFiresOnStableFirstClause のコメント参照）でも、maxReading を小さく設定すれば
    /// 読み長超過で強制確定が発火し、読みが頭打ちになる。
    func testLengthBackstopFiresWhenStableJudgmentNever() {
        let svc = makeService(autoCommit: .ultrastrong, autoCommitMaxReading: 8)
        let sid = svc.startSession()
        let committed = typeAndCollectAutoCommits(svc, session: sid, romaji: "watashihagakkouheikimasunanode")
        XCTAssertFalse(committed.isEmpty, "読み長バックストップが安全弁として発火する")
        // 確定後もセッションは生きており、残り読みへの追記が継続できる。
        XCTAssertNotNil(svc.insert(session: sid, text: "a"))
    }

    /// disabled のときはバックストップも従属して発火しない（既存の明示オプトアウトを尊重）。
    func testLengthBackstopDoesNotFireWhenAutoCommitDisabled() {
        let svc = makeService(autoCommit: .disabled, autoCommitMaxReading: 8)
        let sid = svc.startSession()
        let committed = typeAndCollectAutoCommits(svc, session: sid, romaji: "watashihagakkouheikimasunanode")
        XCTAssertTrue(committed.isEmpty, "disabled では読み長バックストップも無効")
    }

    /// maxReading=0（無効設定）では、通常判定が発火しない入力でも強制確定は起きない。
    func testLengthBackstopDisabledWhenMaxReadingIsZero() {
        let svc = makeService(autoCommit: .ultrastrong, autoCommitMaxReading: 0)
        let sid = svc.startSession()
        let committed = typeAndCollectAutoCommits(svc, session: sid, romaji: "watashihagakkouheikimasunanode")
        XCTAssertTrue(committed.isEmpty, "maxReading<=0 はバックストップ OFF")
    }

    // ---- AutoCommitStrength.resolve（env 解決） ----

    func testAutoCommitStrengthResolveDefaultsToWeakLikeIOS() {
        // iOS の AutomaticCompletionStrengthKey.defaultValue = .weak と同値。
        XCTAssertEqual(AutoCommitStrength.resolve(environment: [:]), .weak)
        XCTAssertEqual(AutoCommitStrength.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT": ""]), .weak)
        XCTAssertEqual(AutoCommitStrength.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT": "unknown"]), .weak)
    }

    func testAutoCommitStrengthResolveParsesValues() {
        XCTAssertEqual(AutoCommitStrength.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT": "disabled"]), .disabled)
        XCTAssertEqual(AutoCommitStrength.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT": "ULTRASTRONG"]), .ultrastrong)
        XCTAssertEqual(AutoCommitStrength.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT": "normal"]), .normal)
    }

    func testAutoCommitThresholdsMatchIOS() {
        // iOS AutoCompletionStrengthSetting.swift の threshold と一致（weak16/normal13/strong10/ultra6）。
        XCTAssertNil(AutoCommitStrength.disabled.threshold)
        XCTAssertEqual(AutoCommitStrength.weak.threshold, 16)
        XCTAssertEqual(AutoCommitStrength.normal.threshold, 13)
        XCTAssertEqual(AutoCommitStrength.strong.threshold, 10)
        XCTAssertEqual(AutoCommitStrength.ultrastrong.threshold, 6)
    }

    // ---- AutoCommitLengthBackstop.resolve（env 解決） ----

    func testAutoCommitLengthBackstopResolveDefaultsTo25() {
        XCTAssertEqual(AutoCommitLengthBackstop.resolve(environment: [:]), 25)
        XCTAssertEqual(AutoCommitLengthBackstop.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT_MAX_READING": "abc"]), 25)
    }

    func testAutoCommitLengthBackstopResolveParsesValue() {
        XCTAssertEqual(AutoCommitLengthBackstop.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT_MAX_READING": "12"]), 12)
    }

    func testAutoCommitLengthBackstopResolveNonPositiveMeansDisabled() {
        XCTAssertEqual(AutoCommitLengthBackstop.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT_MAX_READING": "0"]), 0)
        XCTAssertEqual(AutoCommitLengthBackstop.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT_MAX_READING": "-5"]), -5)
    }
}
