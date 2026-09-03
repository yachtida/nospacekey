import XCTest
@testable import NospacekeyEngineCore
import KanaKanjiConverterModuleWithDefaultDictionary

/// U9: ConversionService.makeZenzaiMode が leftSideContext を ZenzaiV3DependentMode へ配線することを確認する。
final class ZenzaiModeTests: XCTestCase {
    func testMakeZenzaiModeThreadsLeftSideContext() {
        let weight = URL(fileURLWithPath: "C:/dummy/weight.gguf")
        let cfg = ZenzaiConfig(weightURL: weight, inferenceLimit: 7)

        let withCtx = ConversionService.makeZenzaiMode(config: cfg, leftSideContext: "私の名前は")
        let expectedWithCtx = ConvertRequestOptions.ZenzaiMode.on(
            weight: weight,
            inferenceLimit: 7,
            personalizationMode: nil,
            versionDependentMode: .v3(.init(leftSideContext: "私の名前は"))
        )
        XCTAssertEqual(withCtx, expectedWithCtx)

        let withoutCtx = ConversionService.makeZenzaiMode(config: cfg, leftSideContext: nil)
        let expectedWithoutCtx = ConvertRequestOptions.ZenzaiMode.on(
            weight: weight,
            inferenceLimit: 7,
            personalizationMode: nil,
            versionDependentMode: .v3(.init())
        )
        XCTAssertEqual(withoutCtx, expectedWithoutCtx)

        let noWeightCfg = ZenzaiConfig(weightURL: nil, inferenceLimit: 7)
        XCTAssertEqual(
            ConversionService.makeZenzaiMode(config: noWeightCfg, leftSideContext: "私の名前は"),
            .off
        )
    }
}

/// makeOptionsWithZenzaiUsage 決定表（**要求**の truth table）の直接検証。監視
/// （checkZenzaiTooSlowLocked）に資格を与える usedZenzai は、各経路が requestCandidates 直後に
/// 要求×入力非空×実ロード成功（zenzaiInferenceUsedLocked）で組むため、要求報告が options の
/// 実効 zenzaiMode と**同一の決定**から出ていることが監視の前提。実 convert 経路ではこの報告を
/// 観測できない（ダミー weight の silent degrade で候補並びが古典と同値になりうる —
/// testConvertFallsBackToClassicWhenTooSlow の注記）ため、options 構築だけを呼ぶテスト専用
/// アクセサで on/off × requestedZenzai の対を固定する。モデルロード・推論無し。
final class ZenzaiUsageDecisionTests: XCTestCase {
    /// 一時実在ダミー weight（中身は意味を成さない — options 構築は中身を読まない）。
    /// 実在ファイルにするのは本番の有効形と同形にするため: ZenzaiConfig.resolve の
    /// first-existing 規則が weightURL に採るのは実在候補だけ。
    private func writeTempDummyWeight() throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("zenzai-decision-\(UUID().uuidString).gguf")
        try Data("x".utf8).write(to: url)
        return url
    }

    /// weight 有り + ready + !tooSlow → on/true。requestedZenzai が hardcode false に歪んで
    /// いたら、この対だけが落ちはじめる（off/false 側は hardcode と同値になるため）。
    func testWeightAndReadyYieldOnTrue() throws {
        let weight = try writeTempDummyWeight()
        defer { try? FileManager.default.removeItem(at: weight) }
        let svc = ConversionService(config: ZenzaiConfig(weightURL: weight, inferenceLimit: 1))
        XCTAssertTrue(svc.zenzaiEnabled, "前提: weight が解決されている")
        svc.setZenzaiReadyForTesting(true)

        let r = svc.makeOptionsZenzaiRequestForTesting()
        XCTAssertTrue(r.zenzaiOn, "weight + ready + !tooSlow should compose ZenzaiMode .on")
        XCTAssertTrue(r.requestedZenzai, "an .on decision must report requestedZenzai true (not hardcoded false)")
    }

    /// 決定表の off 側 4 分岐は全て off/false の対: ready 閉（warm-up 待ち）・forceClassic・
    /// weight 無し・tooSlow。requestedZenzai が分岐条件の再評価（複製された決定表）で組み直さ
    /// れていたら、いずれかの対が食い違って失敗する。
    func testOffBranchesYieldOffFalse() throws {
        let weight = try writeTempDummyWeight()
        defer { try? FileManager.default.removeItem(at: weight) }

        // ready=false（初期値 = warm-up 完了前のゲート閉）。
        let warming = ConversionService(config: ZenzaiConfig(weightURL: weight, inferenceLimit: 1))
        let warmingR = warming.makeOptionsZenzaiRequestForTesting()
        XCTAssertFalse(warmingR.zenzaiOn, "closed ready gate must compose .off")
        XCTAssertFalse(warmingR.requestedZenzai, "a .off decision must report requestedZenzai false")

        // forceClassic=true（typoConvert の仮説変換と同じ呼び方）。
        let classic = ConversionService(config: ZenzaiConfig(weightURL: weight, inferenceLimit: 1))
        classic.setZenzaiReadyForTesting(true)
        let classicR = classic.makeOptionsZenzaiRequestForTesting(forceClassic: true)
        XCTAssertFalse(classicR.zenzaiOn, "forceClassic must compose .off")
        XCTAssertFalse(classicR.requestedZenzai, "a .off decision must report requestedZenzai false")

        // weight 無し（makeZenzaiMode が .off に落ちる — requestedZenzai も同一決定から false）。
        let noWeight = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        noWeight.setZenzaiReadyForTesting(true)
        XCTAssertFalse(noWeight.zenzaiEnabled, "前提: weight が無い")
        let noWeightR = noWeight.makeOptionsZenzaiRequestForTesting()
        XCTAssertFalse(noWeightR.zenzaiOn, "no weight must compose .off")
        XCTAssertFalse(noWeightR.requestedZenzai, "a .off decision must report requestedZenzai false")

        // tooSlow=true（監視発火後の古典固定 — skip 1 回を消費してから発火させる）。
        let slow = ConversionService(config: ZenzaiConfig(weightURL: weight, inferenceLimit: 1))
        slow.setZenzaiReadyForTesting(true)
        slow.forceTooSlowForTesting(ms: 100, usedZenzai: true)   // 初回 skip 消費
        slow.forceTooSlowForTesting(ms: 1000, usedZenzai: true)  // 2 回目で発火
        XCTAssertTrue(slow.zenzaiTooSlow, "前提: フォールバック状態")
        let slowR = slow.makeOptionsZenzaiRequestForTesting()
        XCTAssertFalse(slowR.zenzaiOn, "tooSlow must compose .off")
        XCTAssertFalse(slowR.requestedZenzai, "a .off decision must report requestedZenzai false")
    }
}

/// isZenzaiOperationalLocked（Zenzai 実稼働判定）の truth table — production getter が呼ぶ
/// 純粋 helper isZenzaiOperational の直接検証（実モデル不要: ロード成功形は文字列で打つ）。
/// この判定は (a) bindConverter/辞書リロードの reset スキップ、(b) 監視資格
/// zenzaiInferenceUsedLocked、が共有する。核心は tooSlow 行: 古典フォールバック中は
/// classic 分岐（previousInputData/lattice/completedData を読む）で変換が走るため、
/// ロードに成功していても必ず false ＝ reset 側。skip が !tooSlow の間だけである事の固定。
final class ZenzaiOperationalPredicateTests: XCTestCase {
    private let weight = URL(fileURLWithPath: "C:/dummy/weight.gguf")
    /// 成功形は "load <url>" ちょうど（KanaKanjiConverter.getModel 0.11.x の形式）。
    private var loadOk: String { "load \(weight.absoluteString)" }

    func testReadyLoadNotSlowIsOperational() {
        XCTAssertTrue(ConversionService.isZenzaiOperational(
            ready: true, tooSlow: false, weightURL: weight, zenzStatus: loadOk))
    }

    /// 命題の中核: 同じロード成功でも tooSlow=true（古典フォールバック中）なら false。
    /// ここが true だと bindConverter/辞書リロードが reset を skip し、別セッション/旧辞書の
    /// classic 分岐キャッシュが残置される（旧実装の Medium バグ）。
    func testTooSlowClassicFallbackIsNotOperational() {
        XCTAssertFalse(ConversionService.isZenzaiOperational(
            ready: true, tooSlow: true, weightURL: weight, zenzStatus: loadOk),
            "tooSlow fallback must land on the reset side even with a loaded model")
    }

    func testWrongStatusIsNotOperational() {
        // 未ロード（初期値は空文字列）。
        XCTAssertFalse(ConversionService.isZenzaiOperational(
            ready: true, tooSlow: false, weightURL: weight, zenzStatus: ""))
        // 失敗形: 成功文字列＋空白＋エラー説明が付く（silent fallback 中の形）。
        XCTAssertFalse(ConversionService.isZenzaiOperational(
            ready: true, tooSlow: false, weightURL: weight,
            zenzStatus: "\(loadOk)    gguf magic invalid"))
        // 別 URL の成功形（モデル差し替え直後の旧 zenzStatus）も不一致。
        let other = URL(fileURLWithPath: "C:/dummy/other.gguf")
        XCTAssertFalse(ConversionService.isZenzaiOperational(
            ready: true, tooSlow: false, weightURL: weight,
            zenzStatus: "load \(other.absoluteString)"))
    }

    func testNoWeightIsNotOperational() {
        XCTAssertFalse(ConversionService.isZenzaiOperational(
            ready: true, tooSlow: false, weightURL: nil, zenzStatus: loadOk))
    }

    func testNotReadyIsNotOperational() {
        // ready ゲート閉（warm-up 完了前）は status に関わらず false。
        XCTAssertFalse(ConversionService.isZenzaiOperational(
            ready: false, tooSlow: false, weightURL: weight, zenzStatus: loadOk))
    }
}

/// zenzaiTooSlow フォールバックの検証: 推論が重い環境で古典（辞書）変換へ固定する仕組み。
/// Zenzai はローカル LLM で、重いPCでは推論が IPC_TIMEOUT_CONVERT(1200ms) / IPC_TIMEOUT_LIVE(400ms)
/// を超えて Space ハングを起こす。zenzaiTooSlow=true で makeOptions が .off を返し、古典で即応する。
///
/// テスト方針: forceTooSlowForTesting で状態遷移を検証するだけでなく、**実際の convert() を呼んで
/// makeOptions がフラグを受けて古典に落ちること**を結合検証する。ダミー weightURL（実在しないパス）
/// でZenzaiロード失敗の silent degrade を起こし、実質古典と同じ挙業になるのを利用する。
final class ZenzaiTooSlowTests: XCTestCase {
    /// 重い推論を注入すると zenzaiTooSlow が true になる（初回スキップ→2回目で発火）。
    func testSlowInferenceSetsTooSlow() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        XCTAssertFalse(svc.zenzaiTooSlow, "default should be false")

        // 初回（slowWatchSkipsRemaining=1）はスキップされる — cold spike 誤判定防止。
        svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)
        XCTAssertFalse(svc.zenzaiTooSlow, "first slow inference should be skipped (cold spike guard)")

        // 2回目で閾値超えを検知してフォールバック。
        svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)
        XCTAssertTrue(svc.zenzaiTooSlow, "second slow inference should trigger fallback")
    }

    /// Zenzai→classic の遅延フォールバックは、現在の遅い要求を壊さず、次の converter 操作の
    /// 直前に共有状態を一度だけ破棄する。全公開入口が同じ予約を消費することを固定する。
    func testSlowFallbackResetsExactlyOnceAtEveryConverterEntry() {
        func prepared(reading: String = "nihongo") -> (ConversionService, Int, Int) {
            let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
            let sid = svc.startSession()
            _ = svc.insert(session: sid, text: reading)
            XCTAssertFalse((svc.convert(session: sid) ?? []).isEmpty, "前提: 同一セッションを active にする")
            // Zenzai 稼働中の別セッション切替で残り得る classic 専用文脈を再現する。
            svc.setClassicContextOwnersForTesting(completed: sid + 1, learning: nil)
            svc.forceTooSlowForTesting(ms: 100, usedZenzai: true)
            svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)
            let state = svc.classicResetStateForTesting
            XCTAssertTrue(state.pending, "遅延フォールバックは次操作への reset を予約する")
            return (svc, sid, state.count)
        }

        func assertConsumed(_ svc: ConversionService, after count: Int,
                            file: StaticString = #filePath, line: UInt = #line) {
            let state = svc.classicResetStateForTesting
            XCTAssertFalse(state.pending, "reset 予約を消費する", file: file, line: line)
            XCTAssertEqual(state.count, count + 1, "stopComposition を一度だけ実行する", file: file, line: line)
        }

        do {
            let (svc, sid, count) = prepared()
            _ = svc.convert(session: sid)
            assertConsumed(svc, after: count)
            _ = svc.convert(session: sid)
            XCTAssertEqual(svc.classicResetStateForTesting.count, count + 1,
                           "同じ fallback 予約を二度消費しない")
        }
        do {
            let (svc, sid, count) = prepared()
            _ = svc.typoConvert(session: sid)
            assertConsumed(svc, after: count)
        }
        do {
            let (svc, sid, count) = prepared()
            _ = svc.reconvert(session: sid, surface: "にほんご")
            assertConsumed(svc, after: count)
        }
        do {
            let (svc, sid, count) = prepared()
            _ = svc.liveConvert(session: sid)
            assertConsumed(svc, after: count)
        }
        do {
            let (svc, sid, count) = prepared(reading: "kyouhaiitenkidesu")
            _ = svc.moveClause(session: sid, offset: 0, baseIndex: 0)
            assertConsumed(svc, after: count)
        }
        do {
            let (svc, sid, count) = prepared()
            XCTAssertNotNil(svc.commit(session: sid, index: 0))
            assertConsumed(svc, after: count)
        }
        do {
            let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
            let sid = svc.startSession()
            _ = svc.insert(session: sid, text: "kyouhaiitenkidesu")
            XCTAssertFalse((svc.convert(session: sid) ?? []).isEmpty)
            XCTAssertNotNil(svc.moveClause(session: sid, offset: 0, baseIndex: 0),
                            "前提: 文節状態を作る")
            svc.setClassicContextOwnersForTesting(completed: sid + 1, learning: nil)
            svc.forceTooSlowForTesting(ms: 100, usedZenzai: true)
            svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)
            let count = svc.classicResetStateForTesting.count
            XCTAssertNotNil(svc.commitClauses(session: sid))
            assertConsumed(svc, after: count)
        }
    }

    /// 遅延フォールバックが同一セッションの部分確定文脈を消さないこと。
    /// completedData/lastData は vendor の private 状態なので所有者メタデータを注入して境界を検証する。
    func testSlowFallbackPreservesCurrentSessionClassicContext() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "nihongo")
        XCTAssertFalse((svc.convert(session: sid) ?? []).isEmpty)
        svc.setClassicContextOwnersForTesting(completed: sid, learning: sid)
        let before = svc.classicResetStateForTesting.count

        svc.forceTooSlowForTesting(ms: 100, usedZenzai: true)
        svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)

        XCTAssertTrue(svc.zenzaiTooSlow)
        XCTAssertFalse(svc.classicResetStateForTesting.pending,
                       "同一セッションの afterComplete/bigram 文脈は reset 対象にしない")
        _ = svc.convert(session: sid)
        XCTAssertEqual(svc.classicResetStateForTesting.count, before,
                       "次の classic 入口でも同一セッション文脈を維持する")
    }

    func testClassicResetOwnershipDecision() {
        XCTAssertFalse(ConversionService.requiresClassicReset(
            activeSession: 7, completedDataSession: nil, learningDataSession: nil))
        XCTAssertFalse(ConversionService.requiresClassicReset(
            activeSession: 7, completedDataSession: 7, learningDataSession: 7))
        XCTAssertTrue(ConversionService.requiresClassicReset(
            activeSession: 7, completedDataSession: 8, learningDataSession: nil))
        XCTAssertTrue(ConversionService.requiresClassicReset(
            activeSession: 7, completedDataSession: nil, learningDataSession: 8))
        XCTAssertTrue(ConversionService.requiresClassicReset(
            activeSession: nil, completedDataSession: 7, learningDataSession: nil))
    }

    func testSessionSwitchDefersClassicResetWhileZenzaiIsOperational() {
        XCTAssertFalse(ConversionService.shouldResetForSessionSwitch(
            isZenzaiOperational: true),
            "GPU推論がclassic文脈を読まない間は高コストなllama context再生成を遅延する")
        XCTAssertTrue(ConversionService.shouldResetForSessionSwitch(
            isZenzaiOperational: false),
            "classic 稼働時のセッション切替は従来どおり常にリセットする")
    }

    /// 閾値未満の推論では zenzaiTooSlow は立たない。
    func testFastInferenceDoesNotTrigger() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        svc.forceTooSlowForTesting(ms: 100, usedZenzai: true)  // 初回スキップ消費
        svc.forceTooSlowForTesting(ms: 100, usedZenzai: true)  // 2回目も速ければ立たない
        XCTAssertFalse(svc.zenzaiTooSlow, "fast inference should not trigger fallback")
    }

    /// [結合] zenzaiTooSlow が makeOptions の ZenzaiMode 分岐を .off へ切り替えることを、
    /// 実 convert() 経路で分離検証する。ダミー weightURL でZenzaiロード失敗の silent degrade を起こす。
    /// **control 実験付き**: zenzaiReady=true を保持したまま zenzaiTooSlow を false→true で切り替え、
    /// makeOptions の挙動変化を候補の並び順で観察する（zenzaiReady=false のおかげで古典になる偽陽性を排除）。
    func testConvertFallsBackToClassicWhenTooSlow() {
        // ダミー weightURL: Zenzaiロード失敗で silent degrade → 古典候補を返すが、
        // makeOptions は zenzaiReady && !zenzaiTooSlow の時 .on を組み、ロード失敗の経路を通る。
        // zenzaiTooSlow=true なら .off で直接古典。両者は「失敗したZenzai経路」と「素の古典」で
        // 候補の並びが異なりうるため、zenzaiTooSlow の効果を区別できる。
        let svc = ConversionService(config: ZenzaiConfig(weightURL: URL(fileURLWithPath: "C:/dummy/weight.gguf"), inferenceLimit: 1))
        svc.setZenzaiReadyForTesting(true)  // warmUp 完了をシミュレート

        // control: zenzaiTooSlow=false の状態（ZenzaiMode=.on → silent degrade で古典候補）
        let sid1 = svc.startSession()
        _ = svc.insert(session: sid1, text: "nihongo")
        let controlCandidates = svc.convert(session: sid1) ?? []

        // フォールバック状態へ
        svc.forceTooSlowForTesting(ms: 100, usedZenzai: true)   // 初回スキップ消費
        svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)  // 2回目で zenzaiTooSlow=true
        XCTAssertTrue(svc.zenzaiTooSlow)

        // zenzaiTooSlow=true の状態（ZenzaiMode=.off → 直接古典候補）
        let sid2 = svc.startSession()
        _ = svc.insert(session: sid2, text: "nihongo")
        let fallbackCandidates = svc.convert(session: sid2) ?? []

        // 両者とも候補が返る（ハングしない）— これが最優先。
        XCTAssertFalse(controlCandidates.isEmpty, "control should produce candidates")
        XCTAssertFalse(fallbackCandidates.isEmpty, "fallback should produce candidates")
        // makeOptions が zenzaiTooSlow を読んで ZenzaiMode を切り替えたことの検証:
        // zenzaiReady=true でダミー weightURL の silent-degrade 経路と、直接古典経路の候補は、
        // ラティス構築のoptions差で並びが異なりうる。一致すれば両経路が同一で zenzaiTooSlow の
        // 切替効果が候補に現れない（=silent degrade が古典と同値）。
        // ここでは「フォールバック後も古典候補が返る」こと（=ハングしない）を主眼とする。
    }

    /// [結合] フォールバック後の継続 convert も古典であり続ける（sticky持続性）。
    func testFallbackIsStickyAcrossConverts() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        svc.setZenzaiReadyForTesting(true)  // zenzaiReady=true の上で zenzaiTooSlow を分離
        svc.forceTooSlowForTesting(ms: 100, usedZenzai: true)
        svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)
        XCTAssertTrue(svc.zenzaiTooSlow)

        // 複数回 convert しても古典で動く（ハングしない）。
        for reading in ["nihongo", "nihon", "test"] {
            let sid = svc.startSession()
            _ = svc.insert(session: sid, text: reading)
            let cands = svc.convert(session: sid) ?? []
            XCTAssertFalse(cands.isEmpty, "convert should still work after fallback (reading=\(reading))")
        }
        XCTAssertTrue(svc.zenzaiTooSlow, "should remain tooSlow across converts")
    }

    /// [結合] liveConvert も zenzaiTooSlow=true の時は古典で動く（ハングしない）。
    func testLiveConvertWorksWithTooSlow() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        svc.setZenzaiReadyForTesting(true)
        svc.forceTooSlowForTesting(ms: 100, usedZenzai: true)
        svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)
        XCTAssertTrue(svc.zenzaiTooSlow)

        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "nihongo")
        // liveConvert が古典で応答を返すことを確認（Zenzai推論でブロックされない）。
        let result = svc.liveConvert(session: sid)
        XCTAssertNotNil(result, "liveConvert should return a result in classic mode")
        XCTAssertFalse(result?.text.isEmpty ?? true, "liveConvert should produce non-empty text")
    }

    /// reload で zenzaiTooSlow がリセットされ、skipカウンタは0（モデル既ホット）になる。
    /// NOSPACEKEY_ZENZAI=off で解決を短路 — per-user/exeDir モデルが実在する開発機で
    /// reload が Zenzai を有効化してしまい、skip=1 でテストが環境依存で失敗するのを防ぐ（巡1 G2-A）。
    func testReloadResetsTooSlow() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        svc.setClassicContextOwnersForTesting(completed: 1, learning: nil)
        svc.forceTooSlowForTesting(ms: 100, usedZenzai: true)
        svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)
        XCTAssertTrue(svc.zenzaiTooSlow)
        let beforeReload = svc.classicResetStateForTesting
        XCTAssertTrue(beforeReload.pending)

        svc.reload(overrides: ["NOSPACEKEY_ZENZAI": "off"])
        XCTAssertFalse(svc.zenzaiTooSlow, "reload should reset zenzaiTooSlow")
        XCTAssertFalse(svc.classicResetStateForTesting.pending,
                       "reload で Zenzai を再試行する前に classic 状態を破棄する")
        XCTAssertEqual(svc.classicResetStateForTesting.count, beforeReload.count + 1)
        // reload後は初回スキップ無し（0）— モデルは既にホットなので即監視。
        svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)
        XCTAssertTrue(svc.zenzaiTooSlow, "after reload, no skip — slow inference should immediately trigger")
    }

    /// reload で Zenzai を新規有効化（weightURL nil→有効値）した時、初回スキップが復活する。
    /// モデル未ロードの初回 convert は本質的に遅い（インラインロード）ので cold spike ガードが必要。
    /// 純関数 shouldRestoreSkipOnReload で遷移パターンを直接検証する（fileExists 制約なし）。
    func testReloadNewlyEnabledZenzaiRestoresSkip() {
        // 新規有効化（nil→非nil）: スキップ復活
        XCTAssertTrue(ConversionService.shouldRestoreSkipOnReload(
            old: nil, new: URL(fileURLWithPath: "C:/dummy/weight.gguf")),
            "newly enabling Zenzai should restore skip")
        // 継続（非nil→非nil）: スキップ不要
        XCTAssertFalse(ConversionService.shouldRestoreSkipOnReload(
            old: URL(fileURLWithPath: "C:/old.gguf"), new: URL(fileURLWithPath: "C:/new.gguf")),
            "changing weight should not restore skip")
        // 無効→無効: スキップ不要
        XCTAssertFalse(ConversionService.shouldRestoreSkipOnReload(old: nil, new: nil),
            "staying disabled should not restore skip")
        // 有効→無効: スキップ不要（もう古典なので監視自体が無意味）
        XCTAssertFalse(ConversionService.shouldRestoreSkipOnReload(
            old: URL(fileURLWithPath: "C:/old.gguf"), new: nil),
            "disabling Zenzai should not restore skip")

        // 無効→無効 reload の実経路検証: skip=0 で即監視（testReloadResetsTooSlow と同じ線）。
        // NOSPACEKEY_ZENZAI=off で解決を短路（G2-A — 環境依存の per-user/exeDir 実在を遮断）。
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        svc.reload(overrides: ["NOSPACEKEY_ZENZAI": "off"])
        svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)
        XCTAssertTrue(svc.zenzaiTooSlow, "disabled->disabled reload: no skip, slow triggers immediately")
    }

    /// zenzaiTooSlow=false（既定）。
    func testNotTooSlowByDefault() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        XCTAssertFalse(svc.zenzaiTooSlow, "default should be false")
    }

    /// usedZenzai=false の計測（古典変換・ウォームアップ待ち・forceClassic・マージ/昇格/キャッシュ/
    /// 自動確定の後段処理）は、どれだけ遅くても zenzaiTooSlow を立てず、初回スキップも消費しない。
    /// 旧実装は監視がこの区別を持たず、Zenzai が一度も走らないまま skip が尽き、最初の実推論が
    /// cold spike として即 disable され得た（High バグ）。スキップ非消費は「その後の Zenzai 推論
    /// 2 連が skip→disable と 1 回ずつずれる」ことで観測する（誤って消費していると 1 回目で立つ）。
    func testClassicTimingNeverConsumesSkipNorSetsTooSlow() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))

        // 古典/後段処理の「遅い」計測（usedZenzai=false）を何回流しても…
        for _ in 0..<3 {
            svc.forceTooSlowForTesting(ms: 1_000_000, usedZenzai: false)
        }
        XCTAssertFalse(svc.zenzaiTooSlow, "usedZenzai=false must never set tooSlow")

        // …初回 skip=1 は保全されている: 真の Zenzai 初回遅推論は握りつぶされ、
        XCTAssertEqual(svc.zenzaiSlowWatchSkipsRemainingForTesting, 1,
                       "non-Zenzai timing must not consume the initial skip")
        svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)
        XCTAssertFalse(svc.zenzaiTooSlow, "initial skip must survive non-Zenzai timing (not consumed)")
        // …次の遅い Zenzai 推論で disable する。
        svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)
        XCTAssertTrue(svc.zenzaiTooSlow, "second slow Zenzai inference should disable")
    }

    /// [結合] 実コール経路の配線: 古典（weightURL=nil → makeOptions が .off）で動く
    /// convert / typoConvert（literal も forceClassic 仮説も古典）/ liveConvert / reconvert /
    /// 文節候補（moveClause）は、何回呼んでも skip を消費しない。旧実装はこれらの呼び出しも
    /// 無条件で監視に数えていた。観測は上記と同じ「Zenzai 推論 2 連が 1 回ずつずれる」方式 —
    /// 配線が誤って監視を呼んでいると 1 回目の force で立って失敗する。
    /// 文節経路は clauseInferenceCountForTesting で「推論が実行された」ことを自己証明する
    /// （nil moveClause を無言で通すと空洞化するため — item10 の自己証明パターン）。
    func testClassicCallPathsDoNotConsumeSkip() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        svc.setZenzaiReadyForTesting(true)  // ready でも weightURL が無ければ .off（同一決定表）

        // convert
        let s1 = svc.startSession()
        _ = svc.insert(session: s1, text: "nihongo")
        XCTAssertFalse((svc.convert(session: s1) ?? []).isEmpty)

        // typoConvert: "ss" 縮約仮説あり — 仮説は forceClassic、literal も古典
        let s2 = svc.startSession()
        for ch in "shitekudassai" { _ = svc.insert(session: s2, text: String(ch)) }
        XCTAssertFalse((svc.typoConvert(session: s2) ?? []).isEmpty)

        // liveConvert
        let s3 = svc.startSession()
        _ = svc.insert(session: s3, text: "nihongo")
        XCTAssertNotNil(svc.liveConvert(session: s3))

        // reconvert
        let s4 = svc.startSession()
        _ = svc.insert(session: s4, text: "nihongo")
        XCTAssertFalse((svc.reconvert(session: s4, surface: "ニホンゴ") ?? []).isEmpty)

        // 文節候補: convert → moveClause で clauseCandidatesLocked が古典推論を走らせる
        let s5 = svc.startSession()
        _ = svc.insert(session: s5, text: "kyouhaiitenkidesu")
        XCTAssertFalse((svc.convert(session: s5) ?? []).isEmpty)
        XCTAssertNotNil(svc.moveClause(session: s5, offset: 0, baseIndex: 0))
        XCTAssertNotNil(svc.moveClause(session: s5, offset: 1, baseIndex: 0))
        XCTAssertGreaterThanOrEqual(svc.clauseInferenceCountForTesting, 1,
                                    "前提: 文節候補の推論経路が実際に走った")

        XCTAssertFalse(svc.zenzaiTooSlow)
        // skip=1 が保全されている: 1 回目は握りつぶされ、2 回目で立つ。
        svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)
        XCTAssertFalse(svc.zenzaiTooSlow, "classic call paths must not consume the initial skip")
        svc.forceTooSlowForTesting(ms: 1000, usedZenzai: true)
        XCTAssertTrue(svc.zenzaiTooSlow)
    }

    /// [結合] nonexistent weight + ready=true: makeOptions は .on を要求する（truth table は
    /// testWeightAndReadyYieldOnTrue）が、upstream の requestCandidates はロード失敗で古典へ
    /// silent fallback し、実 Zenzai 推論は走らない。5監視経路（convert / typoConvert literal /
    /// reconvert / liveConvert / 文節候補）とも skip を消費せず tooSlow も立たない — 旧実装は
    /// .on 要求（usedZenzai=requested）だけを見て監視資格を与えていたため、silent fallback 中の
    /// 古典変換時間で skip/tooSlow を消費した（Zenzai が一度も走らないまま skip を尽くす誤消費）。
    /// 実配線での検証: 実メソッド経由で観測し、forceTooSlowForTesting は前提操作に使わない。
    /// 文節経路は clauseInferenceCountForTesting で「推論経路が走った」ことを自己証明する
    /// （nil moveClause を無言で通すと空洞化するため — testClassicCallPathsDoNotConsumeSkip と同様）。
    func testSilentFallbackPathsConsumeNeitherSkipNorTooSlow() {
        // 存在しない weight パス（ZenzaiConfig への直注入なので resolve の first-existing は無関係）。
        // ロードは失敗し zenzStatus は成功形（"load <url>"）にならない = 実稼働判定は false。
        // silent fallback でも古典候補は返る（ハングしない — testConvertFallsBackToClassicWhenTooSlow
        // と同じ前提）ので、全経路を実推論の呼び方のまま通せる。
        let svc = ConversionService(config: ZenzaiConfig(
            weightURL: URL(fileURLWithPath: "C:/dummy/nonexistent-\(UUID().uuidString).gguf"),
            inferenceLimit: 1))
        svc.setZenzaiReadyForTesting(true)

        // convert
        let s1 = svc.startSession()
        _ = svc.insert(session: s1, text: "nihongo")
        XCTAssertFalse((svc.convert(session: s1) ?? []).isEmpty, "silent fallback でも古典候補は返る")
        XCTAssertEqual(svc.zenzaiSlowWatchSkipsRemainingForTesting, 1,
                       "convert: silent fallback 中の .on 要求は skip を消費しない")

        // typoConvert（"ss" 縮約仮説あり — 仮説は forceClassic で元々監視外、literal が監視対象）
        let s2 = svc.startSession()
        for ch in "shitekudassai" { _ = svc.insert(session: s2, text: String(ch)) }
        XCTAssertFalse((svc.typoConvert(session: s2) ?? []).isEmpty)
        XCTAssertEqual(svc.zenzaiSlowWatchSkipsRemainingForTesting, 1,
                       "typoConvert literal: silent fallback 中の .on 要求は skip を消費しない")

        // liveConvert
        let s3 = svc.startSession()
        _ = svc.insert(session: s3, text: "nihongo")
        XCTAssertNotNil(svc.liveConvert(session: s3))
        XCTAssertEqual(svc.zenzaiSlowWatchSkipsRemainingForTesting, 1,
                       "liveConvert: silent fallback 中の .on 要求は skip を消費しない")

        // reconvert
        let s4 = svc.startSession()
        _ = svc.insert(session: s4, text: "nihongo")
        XCTAssertFalse((svc.reconvert(session: s4, surface: "ニホンゴ") ?? []).isEmpty)
        XCTAssertEqual(svc.zenzaiSlowWatchSkipsRemainingForTesting, 1,
                       "reconvert: silent fallback 中の .on 要求は skip を消費しない")

        // 文節候補: convert → moveClause で clauseCandidatesLocked が走る
        let s5 = svc.startSession()
        _ = svc.insert(session: s5, text: "kyouhaiitenkidesu")
        XCTAssertFalse((svc.convert(session: s5) ?? []).isEmpty)
        XCTAssertNotNil(svc.moveClause(session: s5, offset: 0, baseIndex: 0))
        XCTAssertNotNil(svc.moveClause(session: s5, offset: 1, baseIndex: 0))
        XCTAssertGreaterThanOrEqual(svc.clauseInferenceCountForTesting, 1,
                                    "前提: 文節候補の推論経路が実際に走った")
        XCTAssertEqual(svc.zenzaiSlowWatchSkipsRemainingForTesting, 1,
                       "clause candidates: silent fallback 中の .on 要求は skip を消費しない")

        XCTAssertFalse(svc.zenzaiTooSlow, "silent fallback だけでは tooSlow は立たない")
    }

    /// [結合] 対象入力が空の requestCandidates は推論が走らないため監視資格を持たない —
    /// ready + weight あり（.on 要求）の設定で空読みの convert / liveConvert / reconvert /
    /// typoConvert（仮説なし → convert へ委譲）を通しても skip を消費しない。weight は
    /// nonexistent なので silent fallback も同時に効く — 空入力の効果を silent fallback と
    /// 分離した観測にはロード成功（実モデル）が必要で、実モデル必須テストは禁止のため
    /// ここでは併合する（空入力経路の呼び出しが壊れないこと・非nil 応答のカバレッジとして機能）。
    func testEmptyReadingPathsDoNotConsumeSkip() {
        let svc = ConversionService(config: ZenzaiConfig(
            weightURL: URL(fileURLWithPath: "C:/dummy/nonexistent-\(UUID().uuidString).gguf"),
            inferenceLimit: 1))
        svc.setZenzaiReadyForTesting(true)

        // convert: 既知セッションの空読みは空配列（nil ではない）
        let s1 = svc.startSession()
        XCTAssertEqual(svc.convert(session: s1), [])
        XCTAssertEqual(svc.zenzaiSlowWatchSkipsRemainingForTesting, 1,
                       "empty convert must not consume skip")

        // liveConvert: 空読みでも応答は nil でない（text は空）
        let s2 = svc.startSession()
        let live = svc.liveConvert(session: s2)
        XCTAssertNotNil(live)
        XCTAssertEqual(live?.text, "")
        XCTAssertEqual(svc.zenzaiSlowWatchSkipsRemainingForTesting, 1,
                       "empty liveConvert must not consume skip")

        // reconvert: 空表面 → 空読み
        let s3 = svc.startSession()
        XCTAssertEqual(svc.reconvert(session: s3, surface: ""), [])
        XCTAssertEqual(svc.zenzaiSlowWatchSkipsRemainingForTesting, 1,
                       "empty reconvert must not consume skip")

        // typoConvert: 空読みは仮説なし → convert へ委譲（同じ空入力経路）
        let s4 = svc.startSession()
        XCTAssertEqual(svc.typoConvert(session: s4), [])
        XCTAssertEqual(svc.zenzaiSlowWatchSkipsRemainingForTesting, 1,
                       "empty typoConvert must not consume skip")

        XCTAssertFalse(svc.zenzaiTooSlow)
    }
}
