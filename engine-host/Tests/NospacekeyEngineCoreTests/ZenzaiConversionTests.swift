import XCTest
@testable import NospacekeyEngineCore

/// 実モデルを要求する統合テスト。NOSPACEKEY_ZENZAI_WEIGHT が実在ファイルを指す時のみ実行。
/// それ以外（CI/未配置）では XCTSkip。patched runtime directoryも必要。
///
/// native statusのGPU active / backend / device / decode attemptを候補結果と併せて確認する。
final class ZenzaiConversionTests: XCTestCase {
    private func makeRealModelService(environment: [String: String]) throws -> ConversionService {
        guard let path = environment["NOSPACEKEY_ZENZAI_WEIGHT"],
              FileManager.default.fileExists(atPath: path),
              let runtimePath = environment["NOSPACEKEY_ZENZAI_RUNTIME_DIR"],
              FileManager.default.fileExists(atPath: runtimePath) else {
            throw XCTSkip("実モデルまたはpatched runtime directoryが未配置 → Zenzai統合テストをskip")
        }
        let runtimeDirectory = URL(fileURLWithPath: runtimePath)
            .resolvingSymlinksInPath()
            .standardizedFileURL
        return ConversionService(config: ZenzaiConfig(
            weightURL: URL(fileURLWithPath: path),
            inferenceLimit: 1,
            runtimeDirectory: runtimeDirectory))
    }

    private func startWarmUpAndAssertGPU(_ service: ConversionService) {
        service.startWarmUp()
        let deadline = Date().addingTimeInterval(120)
        while Date() < deadline {
            if case .gpuActive = service.zenzaiRuntimeState { break }
            if case .classic(let reason) = service.zenzaiRuntimeState {
                XCTFail("warm-up latched classic: \(reason)")
                return
            }
            Thread.sleep(forTimeInterval: 0.05)
        }
        guard case .gpuActive = service.zenzaiRuntimeState else {
            XCTFail("warm-up（実モデルロード）がGPU activeへ遷移するはず")
            return
        }
        XCTAssertTrue(service.zenzaiReady)
        let status = service.zenzaiRuntimeStatus
        XCTAssertEqual(status.state, .gpuActive)
        XCTAssertEqual(status.failure, .none)
        XCTAssertFalse(status.backend.isEmpty)
        XCTAssertFalse(status.device.isEmpty)
        XCTAssertGreaterThan(status.decodeAttempts, 0)
    }

    func testZenzaiConvertsNihongoToKanji() throws {
        let env = ProcessInfo.processInfo.environment
        let svc = try makeRealModelService(environment: env)
        XCTAssertTrue(svc.zenzaiEnabled)
        startWarmUpAndAssertGPU(svc)
        let sid = svc.startSession()
        for ch in "nihongo" { _ = svc.insert(session: sid, text: String(ch)) }
        let candidates = svc.convert(session: sid)
        XCTAssertTrue(candidates?.contains("日本語") ?? false, "expected 日本語 in \(String(describing: candidates))")
    }

    /// audit H2 回帰: Zenzai 実稼働中のセッション切替（アプリ間フォーカス切替相当）は llama reset を
    /// スキップするようになった（bindConverter の注記参照）。ここでは切替を往復させても変換・確定が
    /// 正しく動き続けることを実走で確認する（スキップ自体の直接観測は ev=llama_reset_skipped ログで
    /// 行うが、GPU active と decode attempt は上の typed status assertions で検証する）。
    func testSessionSwitchKeepsConvertingWithRealModel() throws {
        let env = ProcessInfo.processInfo.environment
        let svc = try makeRealModelService(environment: env)
        startWarmUpAndAssertGPU(svc)

        // アプリ A で変換・確定 → アプリ B へ切替えて変換 → A へ戻って続きを変換。
        let a = svc.startSession()
        for ch in "kyouha" { _ = svc.insert(session: a, text: String(ch)) }
        XCTAssertFalse(svc.convert(session: a)?.isEmpty ?? true, "A の初回変換が空")

        let b = svc.startSession()   // 切替 1（reset スキップ経路）
        for ch in "nihongo" { _ = svc.insert(session: b, text: String(ch)) }
        let bCands = svc.convert(session: b)
        XCTAssertTrue(bCands?.contains("日本語") ?? false, "expected 日本語 in \(String(describing: bCands))")
        _ = svc.commit(session: b, index: bCands!.firstIndex(of: "日本語")!)

        // 切替 2: A へ戻る。B の確定文脈（completedData 等）は切替時に一掃する。
        for ch in "hare" { _ = svc.insert(session: a, text: String(ch)) }
        XCTAssertFalse(svc.convert(session: a)?.isEmpty ?? true, "切替復帰後の A の変換が空")
    }
}
