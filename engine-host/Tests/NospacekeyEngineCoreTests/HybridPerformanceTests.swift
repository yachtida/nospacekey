import Foundation
import XCTest
@testable import NospacekeyEngineCore

/// Opt-in, real Windows API measurements. Keep this workload identical when comparing revisions.
final class HybridPerformanceTests: XCTestCase {
    func testHybridLatency() throws {
        let env = ProcessInfo.processInfo.environment
        guard env["NOSPACEKEY_BENCH_HYBRID"] == "1" else {
            throw XCTSkip("Set NOSPACEKEY_BENCH_HYBRID=1 for the native latency benchmark")
        }
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .disabled, environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"],
            fileSystem: .live)
        let readings = ["にゅう", "にゅうりょく", "きょう", "きょうはいいてんきです", "がぞ", "へんかん", "にほんご", "せってい"]
        var samples: [String: [Double]] = [:]
        var empty: [String: Int] = [:]
        var count: UInt64 = 0
        for iteration in 0..<13 {
            for reading in readings {
                count += 1
                let request = ClauseCandidatesRequest(key: .init(identity: .init(composition: count,
                    revision: 1, configuration_generation: 1, connection_generation: 1), baseline: 0,
                    conversion_revision: 0, clause_id: 1, request_id: count), reading: reading,
                    reading_start: 0, reading_end: UInt32(reading.unicodeScalars.count), preceding_surfaces: [])
                for kind in ["live", "prediction"] {
                    let started = DispatchTime.now().uptimeNanoseconds
                    let missing: Bool
                    if kind == "live" {
                        missing = service.snapshot([.init(text: reading, style: "direct")], explicit: false).text.isEmpty
                    } else {
                        missing = service.inputPredictions(request).candidates.isEmpty
                    }
                    let ms = Double(DispatchTime.now().uptimeNanoseconds - started) / 1_000_000
                    if iteration > 0 {
                        samples[kind, default: []].append(ms)
                        if missing { empty[kind, default: 0] += 1 }
                    } else {
                        print("HYBRID_COLD kind=\(kind) index=\(count) ms=\(ms)")
                    }
                }
            }
        }
        var report: [String: Any] = ["label": env["NOSPACEKEY_BENCH_LABEL"] ?? "unlabelled"]
        for kind in ["live", "prediction"] {
            let values = samples[kind]!.sorted()
            let result: [String: Any] = ["samples": values.count, "p50_ms": values[values.count / 2],
                "p95_ms": values[Int(Double(values.count - 1) * 0.95)], "max_ms": values.last!,
                "empty": empty[kind, default: 0], "raw_ms": samples[kind]!]
            report[kind] = result
            print("HYBRID_BENCH kind=\(kind) n=\(values.count) p50_ms=\(values[values.count / 2]) p95_ms=\(values[Int(Double(values.count - 1) * 0.95)]) max_ms=\(values.last!) empty=\(empty[kind, default: 0])")
        }
        if let output = env["NOSPACEKEY_BENCH_OUTPUT"] {
            try JSONSerialization.data(withJSONObject: report, options: [.prettyPrinted, .sortedKeys])
                .write(to: URL(fileURLWithPath: output))
        }
        service.prepareForShutdown()
    }
}
