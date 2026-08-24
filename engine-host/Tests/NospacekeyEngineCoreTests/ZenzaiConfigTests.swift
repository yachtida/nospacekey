import XCTest
@testable import NospacekeyEngineCore

final class ZenzaiConfigTests: XCTestCase {
    private let exe = URL(fileURLWithPath: #"C:\app\NospacekeyEngineHost.exe"#).deletingLastPathComponent()

    func testOffEnvForcesClassicEvenWithWeight() {
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: ["NOSPACEKEY_ZENZAI": "off", "NOSPACEKEY_ZENZAI_WEIGHT": #"C:\m.gguf"#],
            fileExists: { _ in true }
        )
        XCTAssertNil(cfg.weightURL)
    }

    func testExplicitWeightUsedWhenExists() {
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: ["NOSPACEKEY_ZENZAI_WEIGHT": #"C:\m.gguf"#],
            fileExists: { $0 == #"C:\m.gguf"# },
            // 巡2 D3: 非nilを期待するテストは CPU ゲートを明示突破 — AVX2 非搭載機で
            // resolve が候補探索前に nil へ短路する環境依存失敗を防ぐ。
            cpuMeetsLlamaBaseline: true
        )
        XCTAssertEqual(cfg.weightURL, URL(fileURLWithPath: #"C:\m.gguf"#))
    }

    func testExplicitWeightIgnoredWhenMissing() {
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: ["NOSPACEKEY_ZENZAI_WEIGHT": #"C:\nope.gguf"#],
            fileExists: { _ in false }
        )
        XCTAssertNil(cfg.weightURL)
    }

    func testDefaultPathNextToExeUsedWhenPresent() {
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: [:],
            fileExists: { $0.contains("ggml-model-Q5_K_M.gguf") },
            cpuMeetsLlamaBaseline: true  // 巡2 D3: AVX2 非搭載機の CPU ゲート短路を回避
        )
        // 厳密な文字列比較は Windows の path 区切り表現に依存して脆いので、
        // 構造（.../models/ggml-model-Q5_K_M.gguf）で検証して appendingPathComponent の順序ミスを捕まえる。
        XCTAssertEqual(cfg.weightURL?.lastPathComponent, "ggml-model-Q5_K_M.gguf")
        XCTAssertEqual(cfg.weightURL?.deletingLastPathComponent().lastPathComponent, "models")
    }

    func testOffEnvIsCaseInsensitive() {
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: ["NOSPACEKEY_ZENZAI": "OFF", "NOSPACEKEY_ZENZAI_WEIGHT": #"C:\m.gguf"#],
            fileExists: { _ in true }
        )
        XCTAssertNil(cfg.weightURL)
    }

    func testMissingEverythingFallsBackToClassic() {
        let cfg = ZenzaiConfig.resolve(exeDir: exe, environment: [:], fileExists: { _ in false })
        XCTAssertNil(cfg.weightURL)
    }

    func testInferenceLimitDefaultsToOne() {
        let cfg = ZenzaiConfig.resolve(exeDir: exe, environment: [:], fileExists: { _ in false })
        XCTAssertEqual(cfg.inferenceLimit, 1)
    }

    func testInferenceLimitEnvOverride() {
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: ["NOSPACEKEY_ZENZAI_INFERENCE_LIMIT": "5"],
            fileExists: { _ in false }
        )
        XCTAssertEqual(cfg.inferenceLimit, 5)
    }

    func testCPUBelowLlamaBaselineForcesClassicEvenWithExplicitWeight() {
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: ["NOSPACEKEY_ZENZAI_WEIGHT": #"C:\m.gguf"#],
            fileExists: { _ in true },
            cpuMeetsLlamaBaseline: false
        )
        XCTAssertNil(cfg.weightURL)
    }

    func testCPUBelowLlamaBaselineForcesClassicForDefaultPath() {
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: [:],
            fileExists: { _ in true },
            cpuMeetsLlamaBaseline: false
        )
        XCTAssertNil(cfg.weightURL)
    }

    func testCPUBelowLlamaBaselineKeepsInferenceLimitResolution() {
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: ["NOSPACEKEY_ZENZAI_INFERENCE_LIMIT": "5"],
            fileExists: { _ in true },
            cpuMeetsLlamaBaseline: false
        )
        XCTAssertEqual(cfg.inferenceLimit, 5)
    }

    func testCPUMeetingBaselineStillAdoptsWeight() {
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: ["NOSPACEKEY_ZENZAI_WEIGHT": #"C:\m.gguf"#],
            fileExists: { $0 == #"C:\m.gguf"# },
            cpuMeetsLlamaBaseline: true
        )
        XCTAssertEqual(cfg.weightURL, URL(fileURLWithPath: #"C:\m.gguf"#))
    }

    func testInferenceLimitGarbageEnvFallsBackToDefault() {
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: ["NOSPACEKEY_ZENZAI_INFERENCE_LIMIT": "garbage"],
            fileExists: { _ in false }
        )
        XCTAssertEqual(cfg.inferenceLimit, 1)
    }

    // MARK: - 解決表の UI/detect_model との統一（明示 → per-user → exeDir）

    /// `%LOCALAPPDATA%\nospacekey\models\ggml-model-Q5_K_M.gguf`（設定UIの per-user DL 先）。
    private var userModelsPath: String {
        URL(fileURLWithPath: #"C:\u"#)
            .appendingPathComponent("nospacekey")
            .appendingPathComponent("models")
            .appendingPathComponent(ZenzaiConfig.defaultWeightFileName)
            .path
    }

    /// `<exeDir>\models\ggml-model-Q5_K_M.gguf`（インストーラ同梱先）。
    private var exeModelsPath: String {
        exe.appendingPathComponent("models")
            .appendingPathComponent(ZenzaiConfig.defaultWeightFileName)
            .path
    }

    func testPerUserModelUsedWhenExplicitMissing() {
        // 明示 weight が消失しても per-user 配置へフォールバック — UI 側 detect_model と
        // 同じ挙動（「導入済み」表示のままエンジンだけ古典へ落ちる解離の防止、UIバグ8）。
        // cpuMeetsLlamaBaseline: true — 実CPU照会に依存しない（AVX2 非搭載環境で候補探索前に
        // nil が返るのを防ぐ、巡1 G2-B）。
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: ["NOSPACEKEY_ZENZAI_WEIGHT": #"C:\gone.gguf"#, "LOCALAPPDATA": #"C:\u"#],
            fileExists: { $0 == userModelsPath },
            cpuMeetsLlamaBaseline: true
        )
        XCTAssertEqual(cfg.weightURL?.path, userModelsPath)
    }

    func testPerUserPreferredOverExeDir() {
        // per-user と exeDir 両方が実在すれば per-user（設定UI DL 由来）を優先。
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: ["LOCALAPPDATA": #"C:\u"#],
            fileExists: { $0 == userModelsPath || $0 == exeModelsPath },
            cpuMeetsLlamaBaseline: true
        )
        XCTAssertEqual(cfg.weightURL?.path, userModelsPath)
    }

    func testExeDirUsedWhenLocalAppDataAbsent() {
        // LOCALAPPDATA が無い異常 env でも exeDir 段は機能する。
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: [:],
            fileExists: { $0 == exeModelsPath },
            cpuMeetsLlamaBaseline: true
        )
        XCTAssertEqual(cfg.weightURL?.path, exeModelsPath)
    }
}
