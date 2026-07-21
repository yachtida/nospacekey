import XCTest
import Foundation
@testable import NospacekeyEngineCore

final class LearningSettingsTests: XCTestCase {
    func testUnsetEnvIsDisabled() {
        // 既定 OFF: 素の swift test / 手動起動が実ユーザーの %LOCALAPPDATA% を汚さない安全側。
        XCTAssertEqual(LearningSettings.resolve(environment: [:]), .disabled)
        XCTAssertEqual(LearningSettings.resolve(environment: ["NOSPACEKEY_LEARNING": "0"]), .disabled)
        XCTAssertEqual(LearningSettings.resolve(environment: ["NOSPACEKEY_LEARNING": "true"]), .disabled)
    }
    func testExplicitMemoryDirWins() {
        let s = LearningSettings.resolve(
            environment: ["NOSPACEKEY_LEARNING": "1", "NOSPACEKEY_MEMORY_DIR": #"C:\tmp\mem"#],
            ensureDir: { _ in true })
        XCTAssertTrue(s.enabled)
        XCTAssertEqual(s.memoryDir, URL(fileURLWithPath: #"C:\tmp\mem"#))
    }
    func testDefaultsToLocalAppData() {
        let s = LearningSettings.resolve(
            environment: ["NOSPACEKEY_LEARNING": "1", "LOCALAPPDATA": #"C:\Users\u\AppData\Local"#],
            ensureDir: { _ in true })
        XCTAssertTrue(s.enabled)
        XCTAssertEqual(s.memoryDir,
            URL(fileURLWithPath: #"C:\Users\u\AppData\Local"#)
                .appendingPathComponent("nospacekey").appendingPathComponent("memory"))
    }
    func testEnsureDirFailureDegradesToDisabled() {
        let s = LearningSettings.resolve(
            environment: ["NOSPACEKEY_LEARNING": "1", "NOSPACEKEY_MEMORY_DIR": #"C:\tmp\mem"#],
            ensureDir: { _ in false })
        XCTAssertEqual(s, .disabled)
    }
    func testNoLocalAppDataDegradesToDisabled() {
        XCTAssertEqual(
            LearningSettings.resolve(environment: ["NOSPACEKEY_LEARNING": "1"], ensureDir: { _ in true }),
            .disabled)
    }
    func testResolveDirIgnoresEnabledGate() {
        // 消去用: 学習 OFF でも「置かれる場所」は解決できる。
        XCTAssertEqual(
            LearningSettings.resolveDir(environment: ["LOCALAPPDATA": #"C:\lad"#]),
            URL(fileURLWithPath: #"C:\lad"#).appendingPathComponent("nospacekey").appendingPathComponent("memory"))
        XCTAssertNil(LearningSettings.resolveDir(environment: [:]))
    }
}
