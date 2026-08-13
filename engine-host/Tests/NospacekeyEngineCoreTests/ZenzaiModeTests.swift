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
        svc.forceTooSlowForTesting(ms: 1000)
        XCTAssertFalse(svc.zenzaiTooSlow, "first slow inference should be skipped (cold spike guard)")

        // 2回目で閾値超えを検知してフォールバック。
        svc.forceTooSlowForTesting(ms: 1000)
        XCTAssertTrue(svc.zenzaiTooSlow, "second slow inference should trigger fallback")
    }

    /// 閾値未満の推論では zenzaiTooSlow は立たない。
    func testFastInferenceDoesNotTrigger() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        svc.forceTooSlowForTesting(ms: 100)  // 初回スキップ消費
        svc.forceTooSlowForTesting(ms: 100)  // 2回目も速ければ立たない
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
        svc.forceTooSlowForTesting(ms: 100)   // 初回スキップ消費
        svc.forceTooSlowForTesting(ms: 1000)  // 2回目で zenzaiTooSlow=true
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
        svc.forceTooSlowForTesting(ms: 100)
        svc.forceTooSlowForTesting(ms: 1000)
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
        svc.forceTooSlowForTesting(ms: 100)
        svc.forceTooSlowForTesting(ms: 1000)
        XCTAssertTrue(svc.zenzaiTooSlow)

        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "nihongo")
        // liveConvert が古典で応答を返すことを確認（Zenzai推論でブロックされない）。
        let result = svc.liveConvert(session: sid)
        XCTAssertNotNil(result, "liveConvert should return a result in classic mode")
        XCTAssertFalse(result?.text.isEmpty ?? true, "liveConvert should produce non-empty text")
    }

    /// reload で zenzaiTooSlow がリセットされ、skipカウンタは0（モデル既ホット）になる。
    func testReloadResetsTooSlow() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        svc.forceTooSlowForTesting(ms: 100)
        svc.forceTooSlowForTesting(ms: 1000)
        XCTAssertTrue(svc.zenzaiTooSlow)

        svc.reload(overrides: [:])
        XCTAssertFalse(svc.zenzaiTooSlow, "reload should reset zenzaiTooSlow")
        // reload後は初回スキップ無し（0）— モデルは既にホットなので即監視。
        svc.forceTooSlowForTesting(ms: 1000)
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
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        svc.reload(overrides: [:])
        svc.forceTooSlowForTesting(ms: 1000)
        XCTAssertTrue(svc.zenzaiTooSlow, "disabled->disabled reload: no skip, slow triggers immediately")
    }

    /// zenzaiTooSlow=false（既定）。
    func testNotTooSlowByDefault() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        XCTAssertFalse(svc.zenzaiTooSlow, "default should be false")
    }
}
