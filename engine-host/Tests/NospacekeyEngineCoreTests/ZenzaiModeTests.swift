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
