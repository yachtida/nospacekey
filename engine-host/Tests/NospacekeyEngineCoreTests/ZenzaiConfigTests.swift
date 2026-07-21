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
            fileExists: { $0 == #"C:\m.gguf"# }
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
            fileExists: { $0.contains("ggml-model-Q5_K_M.gguf") }
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

    func testInferenceLimitGarbageEnvFallsBackToDefault() {
        let cfg = ZenzaiConfig.resolve(
            exeDir: exe,
            environment: ["NOSPACEKEY_ZENZAI_INFERENCE_LIMIT": "garbage"],
            fileExists: { _ in false }
        )
        XCTAssertEqual(cfg.inferenceLimit, 1)
    }
}
