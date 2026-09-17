import Foundation
import XCTest
@testable import NospacekeyEngineCore

final class LiveSearchWidthTests: XCTestCase {
    func testAccuracyLiveMatchesExplicitScopeConversion() {
        let service = ConversionService(
            config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: false, memoryDir: nil))
        let input = [SnapshotSegment(text: "すこーぷがいなので", style: "direct")]
        let live = service.snapshot(input, explicit: false, liveSearchWidth: 10)
        let explicit = service.snapshot(input, explicit: true)
        XCTAssertEqual(explicit.text, "スコープ外なので")
        XCTAssertEqual(live.text, explicit.text)
        XCTAssertNil(live.candidates, "live display still returns just one surface")
        let speed = service.snapshot(input, explicit: false, liveSearchWidth: 1)
        XCTAssertEqual(speed.text, "スコープ買いなので", "switching back must restore the narrow search")
    }

    func testWireSettingReachesClassicConversionAndLeavesExplicitWidthUnchanged() throws {
        let service = ConversionService(
            config: ZenzaiConfig(weightURL: nil, inferenceLimit: 3),
            learning: LearningSettings(enabled: false, memoryDir: nil))
        let handler = makeEngineHandler(service: service, serviceLock: NSLock())
        for explicit in [false, true] {
            for width in [nil, 1, 10, 2] as [Int?] {
                var params: [String: Any] = [
                    "composition": 8, "revision": 1, "configuration_generation": 1,
                    "connection_generation": 1, "conversion_revision": 0, "request_id": 1,
                    "segments": [["text": "すこーぷがいなので", "style": "direct"]],
                    "explicit": explicit,
                ]
                if let width { params["live_search_width"] = width }
                let request = try JSONSerialization.data(withJSONObject: ["method": "LiveSnapshot", "params": params])
                let reply = try JSONSerialization.jsonObject(with: handler(1, request).reply) as! [String: Any]
                XCTAssertEqual(reply["result"] as? String, "SnapshotResult")
                XCTAssertEqual(reply["text"] as? String,
                    explicit || width == 10 ? "スコープ外なので" : "スコープ買いなので")
            }
        }
        XCTAssertEqual(service.zenzaiInferenceLimit, 3)
    }

#if !DEBUG
    func testReleaseSearchWidthLatency() {
        let service = ConversionService(
            config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: false, memoryDir: nil))
        let corpus = ["すこーぷがいなので", "きょうはいいてんきなのでとしょかんでほんをよんでからいえにかえります"]
        for text in corpus {
            let input = [SnapshotSegment(text: text, style: "direct")]
            var samples: [Int: [Double]] = [1: [], 10: []]
            for round in 0..<44 {
                for width in round.isMultiple(of: 2) ? [1, 10] : [10, 1] {
                    let start = DispatchTime.now().uptimeNanoseconds
                    let result = service.snapshot(input, explicit: false, liveSearchWidth: width)
                    let elapsed = Double(DispatchTime.now().uptimeNanoseconds - start) / 1_000_000
                    XCTAssertFalse(result.text.isEmpty)
                    if round >= 4 { samples[width, default: []].append(elapsed) }
                }
            }
            for width in [1, 10] {
                let values = samples[width]!.sorted()
                print("live_search_width_bench chars=\(text.count) width=\(width) samples=\(values.count) p50_ms=\(values[19]) p95_ms=\(values[37])")
            }
        }
    }
#endif
}
