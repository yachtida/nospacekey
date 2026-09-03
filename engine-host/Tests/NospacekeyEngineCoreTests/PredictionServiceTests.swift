import XCTest
import Foundation
import WinSDK
@testable import NospacekeyEngineCore

final class PredictionServiceTests: XCTestCase {
    func testPredictionRuntimeLaunchCannotCreateAConsoleWindow() {
        XCTAssertNotEqual(predictionProcessCreationFlags & DWORD(CREATE_NO_WINDOW), 0)
        XCTAssertNotEqual(predictionProcessCreationFlags & DWORD(EXTENDED_STARTUPINFO_PRESENT), 0)
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

    func testRuntimeConfigRequiresVulkanBackendAndFixedBuildReceipt() throws {
        XCTAssertEqual(PredictionRuntimeConfig.buildReceiptFilename, "BUILD-RECEIPT.txt")
        XCTAssertEqual(PredictionRuntimeConfig.buildReceipt,
                       "schema=nospacekey-inline-prediction-vulkan-v1\n"
                       + "llama_revision=c060ca974c773c7c3d17fd1b66dc9d312bc292c0\n"
                       + "build_shared_libs=ON\n"
                       + "ggml_backend_dl=ON\n"
                       + "ggml_vulkan=ON\n"
                       + "ggml_native=OFF\n"
                       + "ggml_avx2=ON\n"
                       + "backend=Vulkan\n"
                       + "device=Vulkan0\n"
                       + "gpu_layers=all\n")

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-prediction-bundle-\(UUID().uuidString)")
        let modelFolder = root.appendingPathComponent("model")
        let runtimeFolder = root.appendingPathComponent("runtime")
        try FileManager.default.createDirectory(at: modelFolder, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: runtimeFolder, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        let config = PredictionRuntimeConfig(enabled: true, modelFolder: modelFolder,
                                             runtimeFolder: runtimeFolder)
        try Data().write(to: config.modelURL)
        for name in PredictionRuntimeConfig.runtimeRequiredFilenames {
            try Data([1]).write(to: runtimeFolder.appendingPathComponent(name))
        }
        try Data(config.runtimeRevision.utf8).write(to: config.runtimeRevisionURL)
        try Data(PredictionRuntimeConfig.verifiedReceipt.utf8).write(to: config.verifiedReceiptURL)
        try Data(PredictionRuntimeConfig.buildReceipt.utf8).write(to: config.buildReceiptURL)

        XCTAssertTrue(config.filesArePresent)
        XCTAssertTrue(config.runtimeBundleIsValid)
        XCTAssertTrue(config.buildReceiptMatches)
        try Data("schema=wrong\n".utf8).write(to: config.buildReceiptURL)
        XCTAssertFalse(config.buildReceiptMatches)
        try Data(PredictionRuntimeConfig.buildReceipt.utf8).write(to: config.buildReceiptURL)
        try FileManager.default.removeItem(at: config.vulkanBackendURL)
        XCTAssertFalse(config.filesArePresent)
        try Data([1]).write(to: config.vulkanBackendURL)
        try FileManager.default.removeItem(at: config.buildReceiptURL)
        XCTAssertFalse(config.filesArePresent)

        try Data(PredictionRuntimeConfig.buildReceipt.utf8).write(to: config.buildReceiptURL)
        try Data([1]).write(to: runtimeFolder.appendingPathComponent("unexpected.dll"))
        XCTAssertFalse(config.runtimeBundleIsValid)
        try FileManager.default.removeItem(at: runtimeFolder.appendingPathComponent("unexpected.dll"))
        try Data().write(to: runtimeFolder.appendingPathComponent("vcomp140.dll"))
        XCTAssertFalse(config.runtimeBundleIsValid)
    }

    func testPredictionServerRequiresExplicitVulkanOffload() {
        let config = PredictionRuntimeConfig.resolve(environment: ["LOCALAPPDATA": "C:\\test"])
        let arguments = predictionServerArguments(config: config, port: 49_152, apiKey: "test-key")

        func value(after flag: String) -> String? {
            guard let index = arguments.firstIndex(of: flag), arguments.indices.contains(index + 1)
            else { return nil }
            return arguments[index + 1]
        }

        XCTAssertEqual(value(after: "--device"), "Vulkan0")
        XCTAssertEqual(value(after: "--n-gpu-layers"), "all")
        XCTAssertEqual(value(after: "--log-verbosity"), "4")
        XCTAssertFalse(arguments.contains("--verbose"))
    }

    func testPredictionProcessEnvironmentDropsBackendOverrides() {
        let environment = predictionProcessEnvironment(from: [
            "GGML_BACKEND_PATH": "C:\\untrusted",
            "ggml_backend_path": "C:\\also-untrusted",
            "LLAMA_ARG_DEVICE": "none",
            "llama_arg_n_gpu_layers": "0",
            "GGML_VK_VISIBLE_DEVICES": "Other GPU",
            "ggml_vulkan_output_tensor": "0",
            "VK_ICD_FILENAMES": "C:\\untrusted-icd.json",
            "SAFE_SETTING": "kept",
        ])

        XCTAssertNil(environment["GGML_BACKEND_PATH"])
        XCTAssertNil(environment["ggml_backend_path"])
        XCTAssertNil(environment["LLAMA_ARG_DEVICE"])
        XCTAssertNil(environment["llama_arg_n_gpu_layers"])
        XCTAssertNil(environment["GGML_VK_VISIBLE_DEVICES"])
        XCTAssertNil(environment["ggml_vulkan_output_tensor"])
        XCTAssertNil(environment["VK_ICD_FILENAMES"])
        XCTAssertEqual(environment["SAFE_SETTING"], "kept")
    }

    func testPredictionProcessUsesCanonicalRuntimeDirectory() {
        let config = PredictionRuntimeConfig(
            enabled: true,
            modelFolder: URL(fileURLWithPath: "C:\\test\\model"),
            runtimeFolder: URL(fileURLWithPath: "C:\\test\\runtime\\..\\runtime")
        )
        XCTAssertEqual(predictionProcessWorkingDirectory(config: config),
                       config.runtimeFolder.resolvingSymlinksInPath().standardizedFileURL)
    }

    func testPredictionGPUEvidenceRequiresVulkanDeviceAndCompleteOffload() {
        let valid = parsePredictionGPUEvidence("""
        llama_model_load: using device Vulkan0 (Generic GPU)
        llama_model_loader: offloaded 13/13 layers to GPU
        """)
        XCTAssertEqual(valid.offloadedLayers, 13)
        XCTAssertEqual(valid.totalLayers, 13)
        XCTAssertTrue(valid.isValid)

        XCTAssertFalse(parsePredictionGPUEvidence("""
        llama_model_load: using device Vulkan0 (Generic GPU)
        llama_model_loader: offloaded 12/13 layers to GPU
        """).isValid)
        XCTAssertFalse(parsePredictionGPUEvidence(
            "llama_model_loader: offloaded 13/13 layers to GPU\n").isValid)
        XCTAssertFalse(parsePredictionGPUEvidence("""
        llama_model_load: using device Vulkan0 (Generic GPU)
        llama_model_loader: offloaded 0/0 layers to GPU
        """).isValid)
    }

    func testPredictionRuntimeRetriesOnlyPortBindingFailures() {
        XCTAssertEqual(
            classifyPredictionRuntimeFailure(
                "error: couldn't bind HTTP server socket, hostname: 127.0.0.1, port: 49152\n"
            ),
            .retryNextPort
        )
        XCTAssertEqual(
            classifyPredictionRuntimeFailure("bind failed: address already in use\n"),
            .retryNextPort
        )
        XCTAssertEqual(
            classifyPredictionRuntimeFailure("llama_model_loader: failed to load model\n"),
            .terminal
        )
        XCTAssertEqual(
            classifyPredictionRuntimeFailure("prediction runtime GPU evidence missing\n"),
            .terminal
        )
    }

    func testRuntimeConfigIsDisabledByDefault() {
        let config = PredictionRuntimeConfig.resolve(environment: [
            "LOCALAPPDATA": "C:\\test",
            "NOSPACEKEY_PREDICTION_RUNTIME_DIR": "C:\\untrusted-runtime",
        ])
        XCTAssertFalse(config.enabled)
        XCTAssertTrue(config.modelFolder.path.replacingOccurrences(of: "\\", with: "/")
            .hasSuffix("Nospacekey/models/inline-prediction"))
        XCTAssertFalse(config.runtimeFolder.path.lowercased().contains("untrusted-runtime"))
    }

    func testFailedRuntimeSkipsSameConfigUntilExplicitConfigChange() {
        let disabled = PredictionRuntimeConfig.resolve(environment: ["LOCALAPPDATA": "C:\\test"])
        let config = PredictionRuntimeConfig(enabled: true, modelFolder: disabled.modelFolder,
                                              runtimeFolder: disabled.runtimeFolder)
        XCTAssertTrue(shouldSkipPredictionReload(current: config, next: config,
                                                 availability: .ready))
        XCTAssertTrue(shouldSkipPredictionReload(current: config, next: config,
                                                  availability: .failed))

        XCTAssertFalse(shouldSkipPredictionReload(current: config, next: disabled,
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
        let resolved = PredictionRuntimeConfig.resolve(environment: environment)
        let sourceRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent()
        let config = PredictionRuntimeConfig(
            enabled: resolved.enabled,
            modelFolder: resolved.modelFolder,
            runtimeFolder: sourceRoot.appendingPathComponent("prediction-runtime")
        )
        let service = PredictionService.configured(config: config)
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
        let latencyLimit = environment["NOSPACEKEY_PREDICTION_CPU_CONTENTION_TEST"] == "1"
            ? 400.0 : 200.0
        XCTAssertLessThanOrEqual(p95, latencyLimit, "warm p95 was \(p95) ms")
    }
}
