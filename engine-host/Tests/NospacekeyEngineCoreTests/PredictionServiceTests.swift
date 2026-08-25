import XCTest
import Foundation
import WinSDK
@testable import NospacekeyEngineCore

final class PredictionServiceTests: XCTestCase {
    func testPredictionRuntimeLaunchCannotCreateAConsoleWindow() {
        XCTAssertNotEqual(predictionProcessCreationFlags & DWORD(CREATE_NO_WINDOW), 0)
    }

    func testSanitizerMatchesProductLimitsAndStopsAtSentenceEnd() {
        XCTAssertEqual(sanitizePrediction("  会議を始めます。後続"), "会議を始めます。")
        XCTAssertEqual(sanitizePrediction("abcdefghijklmnopq"), "abcdefghijklmnop")
        XCTAssertNil(sanitizePrediction("a"))
        XCTAssertNil(sanitizePrediction("https://example.com"))
        XCTAssertNil(sanitizePrediction("ああああ"))
        XCTAssertNil(sanitizePrediction("abcabcabc"))
        XCTAssertNil(sanitizePrediction("正常\u{0007}ではない"))
        XCTAssertEqual(sanitizePrediction("　会議を始めます"), "会議を始めます")
        XCTAssertNil(sanitizePrediction("不可視\u{200d}文字"))
    }

    func testRuntimeConfigPinsTheEvaluatedArtifacts() {
        XCTAssertEqual(PredictionRuntimeConfig.modelFilename, "llm-jp-3-150m-q8_0-c060ca9.gguf")
        XCTAssertEqual(PredictionRuntimeConfig.modelSHA256,
                       "191f2fdf41a6f64f00ec6b4fcc39ec6164bb13b41d3609ac8f5b2b6149a23a6d")
        XCTAssertEqual(PredictionRuntimeConfig.modelRevision,
                       "b112feef602fff752e4dac4c30af6a2c2fa41c7a")
        XCTAssertEqual(PredictionRuntimeConfig.llamaRevision,
                       "c060ca974c773c7c3d17fd1b66dc9d312bc292c0")
    }

    func testRuntimeConfigIsDisabledByDefault() {
        let config = PredictionRuntimeConfig.resolve(environment: ["LOCALAPPDATA": "C:\\test"])
        XCTAssertFalse(config.enabled)
        XCTAssertTrue(config.modelFolder.path.replacingOccurrences(of: "\\", with: "/")
            .hasSuffix("Nospacekey/models/inline-prediction"))
    }

    func testFailedRuntimeDoesNotSuppressSameConfigReload() {
        let config = PredictionRuntimeConfig.resolve(environment: ["LOCALAPPDATA": "C:\\test"])
        XCTAssertTrue(shouldSkipPredictionReload(current: config, next: config,
                                                 availability: .ready))
        XCTAssertFalse(shouldSkipPredictionReload(current: config, next: config,
                                                  availability: .failed))
    }
    func testUnavailableStateIsExplicitAndKeepsSequence() throws {
        let service = PredictionService(availability: .loading)
        let data = try JSONEncoder().encode(service.predict(seq: 8, tokenIDs: [1, 2]))
        let obj = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "PredictionUnavailable")
        XCTAssertEqual(obj["seq"] as? Int, 8)
        XCTAssertEqual(obj["state"] as? String, "loading")
    }

    func testReadyGeneratorReturnsSequenceCorrelatedPrediction() throws {
        let service = PredictionService(availability: .ready) { context, _ in
            XCTAssertEqual(context, [1, 50_014, 28_998, 65_484, 29_282])
            return "会議です"
        }
        let data = try JSONEncoder().encode(service.predict(
            seq: 42, tokenIDs: [1, 50_014, 28_998, 65_484, 29_282]
        ))
        let obj = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "Prediction")
        XCTAssertEqual(obj["seq"] as? Int, 42)
        XCTAssertEqual(obj["text"] as? String, "会議です")
    }

    func testCancelMakesCompletedWorkStale() throws {
        final class Box: @unchecked Sendable { var service: PredictionService? }
        let box = Box()
        let service = PredictionService(availability: .ready) { _, _ in
            box.service?.cancel()
            return "古い結果"
        }
        box.service = service
        let data = try JSONEncoder().encode(service.predict(seq: 9, tokenIDs: [1, 2]))
        let obj = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "PredictionUnavailable")
        XCTAssertEqual(obj["state"] as? String, "stale")
    }

    func testEvaluatedRuntimeMeetsWarmLatencyGate() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard environment["NOSPACEKEY_RUN_PREDICTION_MODEL_TEST"] == "1" else {
            throw XCTSkip("set NOSPACEKEY_RUN_PREDICTION_MODEL_TEST=1 to run the local model test")
        }
        let service = PredictionService.configured(environment: environment)
        defer { service.shutdown() }
        let loadDeadline = Date().addingTimeInterval(90)
        while service.availabilityState() == .loading && Date() < loadDeadline {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertEqual(service.availabilityState(), .ready)

        let prompts: [[UInt32]] = Array(repeating: [
            1, 46_275, 30_751, 55_574, 31_120, 29_314, 30_857, 78_564, 78_466, 66_700, 99_248,
        ], count: 10)
        XCTAssertTrue(JSONSerialization.isValidJSONObject(["prompt": prompts[0]]))
        var latencies: [Double] = []
        for (index, tokenIDs) in prompts.enumerated() {
            let start = ContinuousClock.now
            let response = service.predict(seq: UInt64(index), tokenIDs: tokenIDs)
            let elapsed = start.duration(to: ContinuousClock.now).components
            latencies.append(Double(elapsed.seconds) * 1_000
                + Double(elapsed.attoseconds) / 1e15)
            guard case .prediction(let seq, let text) = response else {
                let diagnostic = (try? JSONEncoder().encode(response))
                    .flatMap { String(data: $0, encoding: .utf8) } ?? "unencodable"
                return XCTFail("runtime returned no prediction for case \(index): \(diagnostic)")
            }
            XCTAssertEqual(seq, UInt64(index))
            XCTAssertTrue((2...16).contains(text.count))
        }
        let sorted = latencies.sorted()
        let p95 = sorted[min(sorted.count - 1, Int(ceil(Double(sorted.count) * 0.95)) - 1)]
        print("prediction model warm latency p95=\(String(format: "%.1f", p95))ms")
        XCTAssertLessThanOrEqual(p95, 200, "warm p95 was \(p95) ms")
    }
}
