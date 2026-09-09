import XCTest
@testable import NospacekeyEngineCore

/// pipe 名埋め込み版の抽出と runEngineHost の版一致検証（2026-09-06 版混在事故の回帰）。
/// 旧版 TIP が DLL 隣の新版 exe を旧版 pipe 名で起動でき、learning presence（版はバイナリ
/// 実体で決まる）を占有して新版 TIP の起動まで弾く事故を、起動入口の拒否で塞ぐ。
final class EngineHostPipeNameTests: XCTestCase {
    func testEmbeddedBuildIsExtractedFromStablePipeName() {
        // crates/ipc pipe_name_for_session の生成形式。
        XCTAssertEqual(
            pipeNameEmbeddedBuild(#"\\.\pipe\nospacekey-engine.v8.b1.2.2-beta.13.s1"#),
            "1.2.2-beta.13")
        XCTAssertEqual(
            pipeNameEmbeddedBuild(#"\\.\pipe\nospacekey-engine.v8.b1.2.2-beta.12+e104a00.s7"#),
            "1.2.2-beta.12+e104a00")
        XCTAssertEqual(
            pipeNameEmbeddedBuild(#"\\.\pipe\nospacekey-engine.v9.b2.0.0.s12345"#),
            "2.0.0")
    }

    func testNonStablePipeNamesReturnNilSoTestsKeepArbitraryNames() {
        // デフォルト引数やテスト用の任意名は検証対象にしない（nil = skip 契約）。
        XCTAssertNil(pipeNameEmbeddedBuild(#"\\.\pipe\nospacekey-engine"#))
        XCTAssertNil(pipeNameEmbeddedBuild(#"\\.\pipe\nospacekey-engine.test"#))
        // 末尾が .s数字 でない形式は安定名ではない。
        XCTAssertNil(pipeNameEmbeddedBuild(#"\\.\pipe\nospacekey-engine.v8.b1.2.2-beta.13"#))
        XCTAssertNil(pipeNameEmbeddedBuild(#"\\.\pipe\nospacekey-engine.v8.b1.2.2-beta.13.sx"#))
        // .b セグメントが無い安定名形式も対象外。
        XCTAssertNil(pipeNameEmbeddedBuild(#"\\.\pipe\nospacekey-engine.v8.s1"#))
    }

    func testExtractionUsesTheLastBSegmentBeforeTheTrailingSession() {
        // build 内に ".s" を含む文字列が来ても末尾セッション区切りを優先する。
        // （現行の版命名には現れないが、抽出が後ろから走ることの固定。）
        XCTAssertEqual(
            pipeNameEmbeddedBuild(#"\\.\pipe\nospacekey-engine.v8.bb1.0.s2"#),
            "b1.0")
    }
}
