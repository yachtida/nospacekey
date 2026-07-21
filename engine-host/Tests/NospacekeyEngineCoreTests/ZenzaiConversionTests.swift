import XCTest
@testable import NospacekeyEngineCore

/// 実モデルを要求する統合テスト。NOSPACEKEY_ZENZAI_WEIGHT が実在ファイルを指す時のみ実行。
/// それ以外（CI/未配置）では XCTSkip。llama 系 dll が PATH か test バンドル隣に必要。
///
/// 注意（既知の限界）: 「日本語」を含むかの assert は古典フォールバックでも満たされるため、
/// このテスト単体では Zenzai が実走したことを証明しない。実走の確証は
/// llama_model_loader のロードログ（149 tensors 等）で別途確認する運用とする。
/// 厳密な Zenzai/古典 判別はコンバータが load 成否を公開しないため SP2 では行わない（将来課題）。
final class ZenzaiConversionTests: XCTestCase {
    func testZenzaiConvertsNihongoToKanji() throws {
        let env = ProcessInfo.processInfo.environment
        guard let path = env["NOSPACEKEY_ZENZAI_WEIGHT"], FileManager.default.fileExists(atPath: path) else {
            throw XCTSkip("NOSPACEKEY_ZENZAI_WEIGHT 未設定 or ファイル無し → Zenzai 統合テストを skip")
        }
        let svc = ConversionService(config: ZenzaiConfig(weightURL: URL(fileURLWithPath: path), inferenceLimit: 1))
        XCTAssertTrue(svc.zenzaiEnabled)
        // cold start ③: ゲート（zenzaiReady）を開かないと makeOptions が .off に落とし Zenzai が走らない。
        // ゲートは背景 warmUp（実モデルロード）完了後に開くので、開くまで待ってから変換する
        // （待たないと convert が warmUp とロックを競り、ゲート閉側に転ぶと古典で走ってしまう）。
        svc.startWarmUp()
        let deadline = Date().addingTimeInterval(120)
        while !svc.zenzaiReady && Date() < deadline { Thread.sleep(forTimeInterval: 0.05) }
        XCTAssertTrue(svc.zenzaiReady, "warm-up（実モデルロード）が制限時間内に完了するはず")
        let sid = svc.startSession()
        for ch in "nihongo" { _ = svc.insert(session: sid, text: String(ch)) }
        let candidates = svc.convert(session: sid)
        XCTAssertTrue(candidates?.contains("日本語") ?? false, "expected 日本語 in \(String(describing: candidates))")
    }
}
