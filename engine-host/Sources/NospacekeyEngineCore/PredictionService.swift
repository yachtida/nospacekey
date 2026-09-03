import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
import WinSDK

private func predictionDiagnostic(_ message: String) {
    guard ProcessInfo.processInfo.environment["NOSPACEKEY_PREDICTION_DIAGNOSTICS"] == "1" else { return }
    FileHandle.standardError.write(Data("prediction diagnostic: \(message)\n".utf8))
}

enum PredictionAvailability: String, Equatable {
    case disabled
    case loading
    case missingModel = "missing_model"
    case unsupportedCPU = "unsupported_cpu"
    case failed
    case ready
}

struct PredictionRuntimeConfig: Equatable {
    static let modelFilename = "llm-jp-3-150m-q8_0-c060ca9.gguf"
    static let modelSHA256 = "191f2fdf41a6f64f00ec6b4fcc39ec6164bb13b41d3609ac8f5b2b6149a23a6d"
    static let modelRevision = "b112feef602fff752e4dac4c30af6a2c2fa41c7a"
    static let llamaRevision = "c060ca974c773c7c3d17fd1b66dc9d312bc292c0"
    static let runtimeRequiredFilenames = [
        "llama-server.exe", "llama-server-impl.dll", "llama-common.dll",
        "llama.dll", "ggml.dll", "ggml-base.dll", "ggml-cpu.dll",
        "ggml-vulkan.dll", "mtmd.dll", "REVISION", "BUILD-RECEIPT.txt",
    ]
    static let runtimeOptionalFilenames = [
        "vcomp140.dll", "vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll",
    ]
    static let buildReceiptFilename = "BUILD-RECEIPT.txt"
    static let buildReceipt = "schema=nospacekey-inline-prediction-vulkan-v1\n"
        + "llama_revision=c060ca974c773c7c3d17fd1b66dc9d312bc292c0\n"
        + "build_shared_libs=ON\n"
        + "ggml_backend_dl=ON\n"
        + "ggml_vulkan=ON\n"
        + "ggml_native=OFF\n"
        + "ggml_avx2=ON\n"
        + "backend=Vulkan\n"
        + "device=Vulkan0\n"
        + "gpu_layers=all\n"
    static let verifiedReceipt = "schema=1\n"
        + "model_sha256=191f2fdf41a6f64f00ec6b4fcc39ec6164bb13b41d3609ac8f5b2b6149a23a6d\n"
        + "tokenizer_sha256=955dc1fa623fab38cc92a3f4ee172423ae6d73201c4207569bfdf5626bc733f0\n"

    let enabled: Bool
    let modelFolder: URL
    let runtimeFolder: URL

    static func resolve(environment: [String: String] = ProcessInfo.processInfo.environment) -> Self {
        let enabled = environment["NOSPACEKEY_INLINE_PREDICTION"] == "1"
            || environment["NOSPACEKEY_INLINE_PREDICTION"]?.lowercased() == "on"
        let local = environment["LOCALAPPDATA"].map(URL.init(fileURLWithPath:))
            ?? FileManager.default.homeDirectoryForCurrentUser.appending(path: "AppData/Local")
        let modelFolder = environment["NOSPACEKEY_PREDICTION_MODEL_DIR"].map(URL.init(fileURLWithPath:))
            ?? local.appending(path: "Nospacekey/models/inline-prediction")
        let exe = Bundle.main.executableURL ?? URL(fileURLWithPath: CommandLine.arguments[0])
        let runtimeFolder = exe.deletingLastPathComponent().appending(path: "prediction-runtime")
        return Self(enabled: enabled, modelFolder: modelFolder, runtimeFolder: runtimeFolder)
    }

    var modelURL: URL { modelFolder.appending(path: Self.modelFilename) }
    var canonicalRuntimeFolder: URL {
        runtimeFolder.resolvingSymlinksInPath().standardizedFileURL
    }
    var serverURL: URL { canonicalRuntimeFolder.appending(path: "llama-server.exe") }
    var runtimeRevisionURL: URL { canonicalRuntimeFolder.appending(path: "REVISION") }
    var vulkanBackendURL: URL { canonicalRuntimeFolder.appending(path: "ggml-vulkan.dll") }
    var buildReceiptURL: URL {
        canonicalRuntimeFolder.appending(path: Self.buildReceiptFilename)
    }
    var verifiedReceiptURL: URL { modelFolder.appending(path: "VERIFIED") }
    var runtimeRevision: String { Self.llamaRevision }

    var filesArePresent: Bool {
        [modelURL, serverURL, runtimeRevisionURL, verifiedReceiptURL,
         vulkanBackendURL, buildReceiptURL]
            .allSatisfy { FileManager.default.fileExists(atPath: $0.path) }
    }

    var runtimeRevisionMatches: Bool {
        guard let text = try? String(contentsOf: runtimeRevisionURL, encoding: .utf8) else { return false }
        return text.trimmingCharacters(in: .whitespacesAndNewlines) == Self.llamaRevision
    }

    var verifiedReceiptMatches: Bool {
        (try? String(contentsOf: verifiedReceiptURL, encoding: .utf8)) == Self.verifiedReceipt
    }

    var buildReceiptMatches: Bool {
        guard let receipt = try? String(contentsOf: buildReceiptURL, encoding: .utf8) else {
            return false
        }
        return receipt.replacingOccurrences(of: "\r\n", with: "\n") == Self.buildReceipt
    }

    var runtimeBundleIsValid: Bool {
        let standardized = runtimeFolder.standardizedFileURL
        guard let rootValues = try? standardized.resourceValues(forKeys: [.isSymbolicLinkKey]),
              rootValues.isSymbolicLink != true,
              let entries = try? FileManager.default.contentsOfDirectory(
                  at: standardized,
                  includingPropertiesForKeys: [.isSymbolicLinkKey],
                  options: []
              ) else { return false }
        let allowed = Set(Self.runtimeRequiredFilenames + Self.runtimeOptionalFilenames)
        let present = Set(entries.map(\.lastPathComponent))
        guard present.isSubset(of: allowed),
              Set(Self.runtimeRequiredFilenames).isSubset(of: present) else { return false }
        let entriesByName = Dictionary(uniqueKeysWithValues: entries.map { ($0.lastPathComponent, $0) })
        return present.allSatisfy { name in
            guard let url = entriesByName[name],
                  let values = try? url.resourceValues(forKeys: [.isSymbolicLinkKey]),
                  values.isSymbolicLink != true,
                  let attributes = try? FileManager.default.attributesOfItem(atPath: url.path),
                  attributes[.type] as? FileAttributeType == .typeRegular,
                  let size = attributes[.size] as? NSNumber else { return false }
            return size.intValue > 0
        }
    }
}

struct PredictionGPUEvidence: Equatable {
    let hasVulkanDevice: Bool
    let offloadedLayers: Int?
    let totalLayers: Int?

    var isValid: Bool {
        guard hasVulkanDevice, let offloadedLayers, let totalLayers else { return false }
        return offloadedLayers > 0 && totalLayers > 0 && offloadedLayers == totalLayers
    }
}

enum PredictionRuntimeFailureDisposition: Equatable {
    case retryNextPort
    case terminal
}

func classifyPredictionRuntimeFailure(_ log: String) -> PredictionRuntimeFailureDisposition {
    let lowered = log.lowercased()
    let bindFailure = lowered.contains("couldn't bind http server socket")
        || lowered.contains("failed to bind")
        || lowered.contains("bind failed")
    let addressInUse = lowered.contains("address already in use")
        || lowered.contains("address is already in use")
        || lowered.contains("wsaeaddrinuse")
        || lowered.contains("eaddrinuse")
        || lowered.contains("only one usage of each socket address")
    return bindFailure || addressInUse ? .retryNextPort : .terminal
}

func parsePredictionGPUEvidence(_ log: String) -> PredictionGPUEvidence {
    var hasVulkanDevice = false
    var offloadedLayers: Int?
    var totalLayers: Int?

    for line in log.split(whereSeparator: \.isNewline) {
        let lowered = line.lowercased()
        if lowered.contains("using device vulkan0") {
            hasVulkanDevice = true
        }
        guard let marker = lowered.range(of: "offloaded") else { continue }
        let fields = lowered[marker.upperBound...].split { $0 == " " || $0 == "\t" }
        guard fields.count >= 4,
              fields[1] == "layers", fields[2] == "to", fields[3] == "gpu" else { continue }
        let counts = fields[0].split(separator: "/")
        guard counts.count == 2,
              let offloaded = Int(counts[0]), let total = Int(counts[1]) else { continue }
        offloadedLayers = offloaded
        totalLayers = total
    }

    return PredictionGPUEvidence(hasVulkanDevice: hasVulkanDevice,
                                 offloadedLayers: offloadedLayers,
                                 totalLayers: totalLayers)
}

func predictionProcessEnvironment(from environment: [String: String]) -> [String: String] {
    environment.filter { key, _ in
        let normalized = key.uppercased()
        return normalized != "GGML_BACKEND_PATH"
            && !normalized.hasPrefix("LLAMA_ARG_")
            && !normalized.hasPrefix("GGML_VK_")
            && !normalized.hasPrefix("GGML_VULKAN_")
            && !normalized.hasPrefix("VK_")
    }
}

private func windowsEnvironmentBlock(_ environment: [String: String]) -> [UInt16] {
    let entries = environment.keys.sorted { lhs, rhs in
        lhs.uppercased() == rhs.uppercased() ? lhs < rhs : lhs.uppercased() < rhs.uppercased()
    }.compactMap { key -> [UInt16]? in
        guard let value = environment[key] else { return nil }
        return Array("\(key)=\(value)\0".utf16)
    }
    return entries.flatMap { $0 } + [0]
}

func predictionProcessWorkingDirectory(config: PredictionRuntimeConfig) -> URL {
    config.canonicalRuntimeFolder
}

func predictionServerArguments(config: PredictionRuntimeConfig, port: Int,
                               apiKey: String) -> [String] {
    [
        "-m", config.modelURL.path,
        "-t", String(max(1, min(12, ProcessInfo.processInfo.activeProcessorCount))),
        "-c", "512", "-np", "1", "--host", "127.0.0.1",
        "--port", String(port), "--no-webui", "--api-key", apiKey,
        "--device", "Vulkan0", "--n-gpu-layers", "all",
        "--log-verbosity", "4",
    ]
}

private let predictionProcessHandleListAttribute = DWORD_PTR(0x0002_0002)

// CreateProcess snapshots inheritable handles, so clearing a temporary handle after launch must
// not race another child launch that still uses bInheritHandles.
let engineInheritableHandleProcessCreationLock = NSLock()

let predictionProcessCreationFlags = DWORD(CREATE_NO_WINDOW)
    | DWORD(CREATE_UNICODE_ENVIRONMENT)
    | DWORD(EXTENDED_STARTUPINFO_PRESENT)

private func quoteWindowsProcessArgument(_ argument: String) -> String {
    guard argument.isEmpty || argument.contains(where: { $0 == " " || $0 == "\t" || $0 == "\"" })
    else { return argument }
    var quoted = "\""
    var backslashes = 0
    for character in argument {
        if character == "\\" {
            backslashes += 1
        } else if character == "\"" {
            quoted += String(repeating: "\\", count: backslashes * 2 + 1)
            quoted.append(character)
            backslashes = 0
        } else {
            quoted += String(repeating: "\\", count: backslashes)
            quoted.append(character)
            backslashes = 0
        }
    }
    quoted += String(repeating: "\\", count: backslashes * 2)
    quoted.append("\"")
    return quoted
}

private final class HiddenProcess: @unchecked Sendable {
    private let handle: HANDLE
    private let outputLogHandle: HANDLE
    let outputLogURL: URL
    let processIdentifier: Int32

    init(executableURL: URL, arguments: [String], environment: [String: String] =
         predictionProcessEnvironment(from: ProcessInfo.processInfo.environment)) throws {
        var security = SECURITY_ATTRIBUTES()
        security.nLength = DWORD(MemoryLayout<SECURITY_ATTRIBUTES>.size)
        security.bInheritHandle = true
        let nullHandle = "NUL".withCString(encodedAs: UTF16.self) { path in
            CreateFileW(
                path, DWORD(GENERIC_READ) | DWORD(GENERIC_WRITE),
                DWORD(FILE_SHARE_READ | FILE_SHARE_WRITE), &security,
                DWORD(OPEN_EXISTING), DWORD(FILE_ATTRIBUTE_NORMAL), nil
            )
        }
        guard let nullHandle, nullHandle != INVALID_HANDLE_VALUE else {
            throw LLMError(message: "cannot open null device for prediction process")
        }
        defer { CloseHandle(nullHandle) }

        let logURL = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-prediction-\(UUID().uuidString).log")
        let logHandle = logURL.path.withCString(encodedAs: UTF16.self) { path in
            CreateFileW(
                path, DWORD(GENERIC_READ) | DWORD(GENERIC_WRITE),
                DWORD(FILE_SHARE_READ) | DWORD(FILE_SHARE_WRITE), &security,
                DWORD(CREATE_ALWAYS), DWORD(FILE_ATTRIBUTE_NORMAL), nil
            )
        }
        guard let logHandle, logHandle != INVALID_HANDLE_VALUE else {
            throw LLMError(message: "cannot open prediction process log")
        }
        var keepLog = false
        defer {
            if !keepLog {
                CloseHandle(logHandle)
                try? FileManager.default.removeItem(at: logURL)
            }
        }

        var startup = STARTUPINFOW()
        startup.cb = DWORD(MemoryLayout<STARTUPINFOEXW>.size)
        startup.dwFlags = DWORD(STARTF_USESTDHANDLES) | DWORD(STARTF_USESHOWWINDOW)
        startup.wShowWindow = WORD(SW_HIDE)
        startup.hStdInput = nullHandle
        startup.hStdOutput = logHandle
        startup.hStdError = logHandle

        var attributeListSize = SIZE_T(0)
        _ = InitializeProcThreadAttributeList(nil, 1, 0, &attributeListSize)
        guard attributeListSize > 0 else {
            throw LLMError(message: "cannot size prediction process attributes")
        }
        let attributeStorage = UnsafeMutableRawPointer.allocate(
            byteCount: Int(attributeListSize),
            alignment: MemoryLayout<UnsafeMutableRawPointer>.alignment
        )
        defer { attributeStorage.deallocate() }
        let attributeList = OpaquePointer(attributeStorage)
        guard InitializeProcThreadAttributeList(attributeList, 1, 0, &attributeListSize) else {
            throw LLMError(message: "cannot initialize prediction process attributes")
        }
        defer { DeleteProcThreadAttributeList(attributeList) }
        var inheritedHandles = [nullHandle, logHandle]
        let handlesUpdated = inheritedHandles.withUnsafeMutableBytes { handles in
            UpdateProcThreadAttribute(
                attributeList, 0, predictionProcessHandleListAttribute,
                handles.baseAddress, SIZE_T(handles.count), nil, nil
            )
        }
        guard handlesUpdated else {
            throw LLMError(message: "cannot restrict prediction process handles")
        }
        var startupExtended = STARTUPINFOEXW()
        startupExtended.StartupInfo = startup
        startupExtended.lpAttributeList = attributeList

        let commandLine = ([executableURL.path] + arguments)
            .map(quoteWindowsProcessArgument)
            .joined(separator: " ")
        var commandBuffer = Array(commandLine.utf16) + [0]
        var environmentBuffer = windowsEnvironmentBlock(environment)
        var processInfo = PROCESS_INFORMATION()
        let workingDirectory = executableURL.deletingLastPathComponent()
            .resolvingSymlinksInPath().standardizedFileURL.path
        engineInheritableHandleProcessCreationLock.lock()
        let created = executableURL.path.withCString(encodedAs: UTF16.self) { executable in
            workingDirectory.withCString(encodedAs: UTF16.self) { directory in
                environmentBuffer.withUnsafeMutableBufferPointer { environment in
                    commandBuffer.withUnsafeMutableBufferPointer { command in
                        withUnsafeMutablePointer(to: &startupExtended) { startupPointer in
                            let startupInfo = UnsafeMutableRawPointer(startupPointer)
                                .assumingMemoryBound(to: STARTUPINFOW.self)
                            return CreateProcessW(
                                executable, command.baseAddress, nil, nil, true,
                                predictionProcessCreationFlags,
                                UnsafeMutableRawPointer(environment.baseAddress), directory,
                                startupInfo, &processInfo
                            )
                        }
                    }
                }
            }
        }
        guard created else {
            engineInheritableHandleProcessCreationLock.unlock()
            throw LLMError(message: "cannot start prediction process (win32=\(GetLastError()))")
        }
        let logHandleSealed = SetHandleInformation(logHandle, DWORD(HANDLE_FLAG_INHERIT), 0)
        engineInheritableHandleProcessCreationLock.unlock()
        guard logHandleSealed else {
            _ = TerminateProcess(processInfo.hProcess, 1)
            CloseHandle(processInfo.hThread)
            CloseHandle(processInfo.hProcess)
            throw LLMError(message: "cannot seal prediction process log handle")
        }
        CloseHandle(processInfo.hThread)
        handle = processInfo.hProcess
        outputLogHandle = logHandle
        outputLogURL = logURL
        processIdentifier = Int32(bitPattern: GetProcessId(processInfo.hProcess))
        keepLog = true
    }

    var isRunning: Bool {
        var code = DWORD()
        return GetExitCodeProcess(handle, &code) && code == DWORD(STILL_ACTIVE)
    }

    func terminate() {
        if isRunning {
            _ = TerminateProcess(handle, 1)
            _ = WaitForSingleObject(handle, 5_000)
        }
    }

    func readOutputLog() -> String {
        _ = FlushFileBuffers(outputLogHandle)
        return (try? String(contentsOf: outputLogURL, encoding: .utf8)) ?? ""
    }

    deinit {
        CloseHandle(handle)
        CloseHandle(outputLogHandle)
        try? FileManager.default.removeItem(at: outputLogURL)
    }
}

private final class KillOnCloseJob: @unchecked Sendable {
    private let handle: HANDLE

    init(processID: Int32) throws {
        guard let job = CreateJobObjectW(nil, nil) else {
            throw LLMError(message: "cannot create prediction process job")
        }
        var info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION()
        info.BasicLimitInformation.LimitFlags = DWORD(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)
        let configured = withUnsafePointer(to: &info) { pointer in
            SetInformationJobObject(
                job, JobObjectExtendedLimitInformation,
                UnsafeMutableRawPointer(mutating: pointer),
                DWORD(MemoryLayout<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>.size)
            )
        }
        guard configured else {
            CloseHandle(job)
            throw LLMError(message: "cannot configure prediction process job")
        }
        let access = PROCESS_SET_QUOTA | PROCESS_TERMINATE
        guard let child = OpenProcess(DWORD(access), false, DWORD(processID)) else {
            CloseHandle(job)
            throw LLMError(message: "cannot open prediction process")
        }
        let assigned = AssignProcessToJobObject(job, child)
        CloseHandle(child)
        guard assigned else {
            CloseHandle(job)
            throw LLMError(message: "cannot contain prediction process")
        }
        handle = job
    }

    deinit { CloseHandle(handle) }
}

private final class LlamaPredictionRuntime: @unchecked Sendable {
    private let process: HiddenProcess
    private let job: KillOnCloseJob
    private let endpoint: URL
    private let apiKey: String

    private init(process: HiddenProcess, job: KillOnCloseJob, endpoint: URL, apiKey: String) {
        self.process = process
        self.job = job
        self.endpoint = endpoint
        self.apiKey = apiKey
    }

    static func load(config: PredictionRuntimeConfig,
                     isCancelled: @Sendable () -> Bool) async throws -> LlamaPredictionRuntime {
        let portBase = 49_152 + Int(GetCurrentProcessId() % 12_000)
        var lastError: Error = LLMError(message: "prediction runtime failed")
        for offset in 0..<8 {
            if isCancelled() { throw CancellationError() }
            let port = portBase + offset
            let apiKey = UUID().uuidString
            var process: HiddenProcess?
            let arguments = predictionServerArguments(config: config, port: port, apiKey: apiKey)
            do {
                let launched = try HiddenProcess(executableURL: config.serverURL,
                                                 arguments: arguments)
                process = launched
                let job = try KillOnCloseJob(processID: launched.processIdentifier)
                setBelowNormalPriority(pid: launched.processIdentifier)
                let endpoint = URL(string: "http://127.0.0.1:\(port)")!
                try waitUntilReady(endpoint: endpoint, process: launched, apiKey: apiKey,
                                   isCancelled: isCancelled)
                let runtime = LlamaPredictionRuntime(process: launched, job: job,
                                                     endpoint: endpoint, apiKey: apiKey)
                guard runtime.generate(tokenIDs: [
                    1, 46_275, 30_751, 55_574, 31_120, 29_314, 30_857, 78_564, 78_466, 66_700, 99_248,
                ],
                                       timeout: 10, isCancelled: isCancelled) != nil else {
                    throw LLMError(message: "prediction runtime warm-up failed")
                }
                try waitForGPUEvidence(process: launched, isCancelled: isCancelled)
                return runtime
            } catch {
                let logBeforeTermination = process?.readOutputLog() ?? ""
                process?.terminate()
                let logAfterTermination = process?.readOutputLog() ?? ""
                let failureLog = logBeforeTermination + "\n" + logAfterTermination
                guard classifyPredictionRuntimeFailure(failureLog) == .retryNextPort else {
                    throw error
                }
                lastError = error
                predictionDiagnostic("prediction runtime port unavailable; trying next port")
            }
        }
        throw lastError
    }

    private static func waitForGPUEvidence(process: HiddenProcess,
                                           isCancelled: @Sendable () -> Bool) throws {
        let deadline = Date().addingTimeInterval(5)
        while Date() < deadline {
            if isCancelled() { throw CancellationError() }
            let evidence = parsePredictionGPUEvidence(process.readOutputLog())
            if evidence.isValid { return }
            guard process.isRunning else {
                throw LLMError(message: "prediction runtime exited before GPU evidence")
            }
            Thread.sleep(forTimeInterval: 0.05)
        }
        throw LLMError(message: "prediction runtime GPU evidence missing")
    }

    func shutdown() {
        process.terminate()
    }

    var isRunning: Bool { process.isRunning }

    // TIP の表示期限と同じ 400ms で実処理も打ち切る。FoundationNetworking の keep-alive 再利用は
    // Windows で偽 timeout を起こしたため、下で Connection: close を指定して各要求を独立させる。
    func generate(tokenIDs: [UInt32], timeout: TimeInterval = 0.4,
                  isCancelled: @Sendable () -> Bool) -> String? {
        if isCancelled() { return nil }
        let payload: [String: Any] = [
            "prompt": tokenIDs,
            "n_predict": 24,
            "temperature": 0,
            "seed": 0,
            "stop": ["。", "！", "？", "\n"],
            "cache_prompt": false,
        ]
        guard let body = try? JSONSerialization.data(withJSONObject: payload),
              let url = URL(string: "completion", relativeTo: endpoint) else {
            predictionDiagnostic("request serialization failed")
            return nil
        }
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        // FoundationNetworking on Windows can report a spurious -1001 around short
        // timeout intervals. Keep its transport timeout generous and enforce the
        // product deadline in send(_:deadline:isCancelled:) below.
        request.timeoutInterval = max(timeout, 2)
        request.setValue("application/json; charset=utf-8", forHTTPHeaderField: "Content-Type")
        request.setValue("Bearer \(apiKey)", forHTTPHeaderField: "Authorization")
        request.setValue("close", forHTTPHeaderField: "Connection")
        request.httpBody = body
        guard let data = Self.send(request, deadline: timeout, isCancelled: isCancelled),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            predictionDiagnostic("completion transport or response decoding failed")
            return nil
        }
        var raw = object["content"] as? String ?? ""
        if object["stop_type"] as? String == "word",
           let word = object["stopping_word"] as? String,
           ["。", "！", "？"].contains(word) {
            raw += word
        }
        let sanitized = sanitizePrediction(raw)
        predictionDiagnostic("completion decoded chars=\(raw.count) accepted=\(sanitized != nil)")
        return sanitized
    }

    private static func send(_ request: URLRequest, deadline timeout: TimeInterval,
                             isCancelled: @Sendable () -> Bool) -> Data? {
        final class Box: @unchecked Sendable { var data: Data?; var status: Int?; var error: String? }
        let box = Box()
        let semaphore = DispatchSemaphore(value: 0)
        let task = URLSession.shared.dataTask(with: request) { data, response, error in
            if let http = response as? HTTPURLResponse {
                box.status = http.statusCode
            }
            box.error = error?.localizedDescription
            if let http = response as? HTTPURLResponse, (200..<300).contains(http.statusCode) {
                box.data = data
            }
            semaphore.signal()
        }
        task.resume()
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if isCancelled() { task.cancel(); predictionDiagnostic("request cancelled"); return nil }
            if semaphore.wait(timeout: .now() + .milliseconds(10)) == .success {
                if box.data == nil {
                    predictionDiagnostic("request failed status=\(box.status.map(String.init) ?? "none") error=\(box.error ?? "none")")
                }
                return box.data
            }
        }
        task.cancel()
        predictionDiagnostic("request deadline exceeded")
        return nil
    }

    private static func waitUntilReady(endpoint: URL, process: HiddenProcess, apiKey: String,
                                       isCancelled: @Sendable () -> Bool) throws {
        let deadline = Date().addingTimeInterval(60)
        while Date() < deadline {
            if isCancelled() { throw CancellationError() }
            guard process.isRunning else { throw LLMError(message: "llama-server exited") }
            if let url = URL(string: "health", relativeTo: endpoint) {
                var request = URLRequest(url: url)
                request.timeoutInterval = 0.25
                request.setValue("Bearer \(apiKey)", forHTTPHeaderField: "Authorization")
                if let data = send(request, deadline: 0.25, isCancelled: isCancelled),
                   let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                   object["status"] as? String == "ok" { return }
            }
            Thread.sleep(forTimeInterval: 0.05)
        }
        throw LLMError(message: "llama-server warm-up timeout")
    }

    private static func setBelowNormalPriority(pid: Int32) {
        let handle = OpenProcess(DWORD(PROCESS_SET_INFORMATION), false, DWORD(pid))
        guard let handle else { return }
        _ = SetPriorityClass(handle, DWORD(BELOW_NORMAL_PRIORITY_CLASS))
        CloseHandle(handle)
    }
}

func shouldSkipPredictionReload(current: PredictionRuntimeConfig?, next: PredictionRuntimeConfig,
                                availability: PredictionAvailability) -> Bool {
    current == next
}

func sanitizePrediction(_ raw: String) -> String? {
    var text = raw.drop(while: \.isWhitespace)
    if let lineEnd = text.firstIndex(where: { $0 == "\r" || $0 == "\n" }) {
        text = text[..<lineEnd]
    }
    if let punctuation = text.firstIndex(where: { "。！？".contains($0) }) {
        text = text[...punctuation]
    }
    let candidate = String(text.prefix(16))
    guard candidate.count >= 2,
          !candidate.localizedCaseInsensitiveContains("http://"),
          !candidate.localizedCaseInsensitiveContains("https://"),
          !candidate.localizedCaseInsensitiveContains("www."),
          !candidate.contains("\u{fffd}"),
          candidate.unicodeScalars.allSatisfy({
              !CharacterSet.controlCharacters.contains($0)
                  && $0.properties.generalCategory != .format
          }) else { return nil }
    let chars = Array(candidate)
    if chars.indices.contains(where: { index in
        index + 3 < chars.count && chars[index] == chars[index + 1]
            && chars[index] == chars[index + 2] && chars[index] == chars[index + 3]
    }) { return nil }
    for unitLength in 2...4 where chars.count >= unitLength * 3 {
        for start in 0...(chars.count - unitLength * 3) {
            let unit = chars[start..<(start + unitLength)]
            if unit.elementsEqual(chars[(start + unitLength)..<(start + unitLength * 2)])
                && unit.elementsEqual(chars[(start + unitLength * 2)..<(start + unitLength * 3)]) {
                return nil
            }
        }
    }
    return candidate
}

final class PredictionService: @unchecked Sendable {
    typealias Generator = @Sendable (_ tokenIDs: [UInt32], _ isCancelled: @Sendable () -> Bool) -> String?

    private let lock = NSLock()
    private let generationLock = NSLock()
    private var requestGeneration: UInt64 = 0
    private var configGeneration: UInt64 = 0
    private var availability: PredictionAvailability
    private let injectedGenerator: Generator?
    private var runtime: LlamaPredictionRuntime?
    private var currentConfig: PredictionRuntimeConfig?

    init(availability: PredictionAvailability = .disabled, generator: Generator? = nil) {
        self.availability = availability
        self.injectedGenerator = generator
    }

    static func configured(environment: [String: String] = ProcessInfo.processInfo.environment) -> PredictionService {
        configured(config: .resolve(environment: environment))
    }

    static func configured(config: PredictionRuntimeConfig) -> PredictionService {
        let service = PredictionService()
        service.reload(config: config)
        return service
    }

    func predict(seq: UInt64, tokenIDs: [UInt32]) -> Response {
        lock.lock()
        let generation = requestGeneration
        let state = availability
        let generator = injectedGenerator
        let runtime = runtime
        lock.unlock()
        guard state == .ready, generator != nil || runtime != nil else {
            return .predictionUnavailable(seq: seq, state: state.rawValue)
        }
        let cancelled: @Sendable () -> Bool = { [weak self] in
            guard let self else { return true }
            self.lock.lock(); defer { self.lock.unlock() }
            return self.requestGeneration != generation
        }
        generationLock.lock(); defer { generationLock.unlock() }
        let text = generator?(tokenIDs, cancelled)
            ?? runtime?.generate(tokenIDs: tokenIDs, isCancelled: cancelled)
        guard let text else {
            if generator == nil, let runtime, !runtime.isRunning {
                markRuntimeFailed(runtime)
            }
            return .predictionUnavailable(seq: seq, state: "failed")
        }
        guard !cancelled() else {
            return .predictionUnavailable(seq: seq, state: "stale")
        }
        return .prediction(seq: seq, text: text)
    }

    func cancel() {
        lock.lock(); requestGeneration &+= 1; lock.unlock()
    }

    func setAvailability(_ state: PredictionAvailability) {
        lock.lock(); requestGeneration &+= 1; availability = state; lock.unlock()
    }

    func availabilityState() -> PredictionAvailability {
        lock.lock(); defer { lock.unlock() }
        return availability
    }

    func reload(config: PredictionRuntimeConfig) {
        lock.lock()
        if shouldSkipPredictionReload(current: currentConfig, next: config,
                                      availability: availability) {
            lock.unlock()
            return
        }
        currentConfig = config
        requestGeneration &+= 1
        configGeneration &+= 1
        let loadGeneration = configGeneration
        let oldRuntime = runtime
        runtime = nil
        if !config.enabled {
            availability = .disabled
            lock.unlock()
            oldRuntime?.shutdown()
            return
        }
        guard IsProcessorFeaturePresent(DWORD(40)) else {
            availability = .unsupportedCPU
            lock.unlock()
            oldRuntime?.shutdown()
            return
        }
        guard config.filesArePresent else {
            availability = .missingModel
            lock.unlock()
            oldRuntime?.shutdown()
            return
        }
        guard config.runtimeBundleIsValid, config.runtimeRevisionMatches,
              config.verifiedReceiptMatches, config.buildReceiptMatches else {
            availability = .failed
            lock.unlock()
            oldRuntime?.shutdown()
            return
        }
        availability = .loading
        lock.unlock()
        oldRuntime?.shutdown()

        Task.detached(priority: .background) { [weak self] in
            let cancelled: @Sendable () -> Bool = { [weak self] in
                guard let self else { return true }
                self.lock.lock(); defer { self.lock.unlock() }
                return self.configGeneration != loadGeneration
            }
            do {
                let loaded = try await LlamaPredictionRuntime.load(config: config,
                                                                   isCancelled: cancelled)
                guard let self else { loaded.shutdown(); return }
                self.install(loaded: loaded, generation: loadGeneration)
            } catch {
                self?.markLoadFailed(generation: loadGeneration)
            }
        }
    }

    private func markRuntimeFailed(_ exitedRuntime: LlamaPredictionRuntime) {
        lock.lock()
        guard runtime === exitedRuntime, !exitedRuntime.isRunning else {
            lock.unlock()
            return
        }
        runtime = nil
        availability = .failed
        requestGeneration &+= 1
        lock.unlock()
    }

    private func install(loaded: LlamaPredictionRuntime, generation: UInt64) {
        lock.lock()
        if configGeneration == generation {
            runtime = loaded
            availability = .ready
            lock.unlock()
        } else {
            lock.unlock()
            loaded.shutdown()
        }
    }

    private func markLoadFailed(generation: UInt64) {
        lock.lock()
        if configGeneration == generation { availability = .failed }
        lock.unlock()
    }

    func shutdown() {
        lock.lock()
        requestGeneration &+= 1
        configGeneration &+= 1
        availability = .disabled
        let oldRuntime = runtime
        runtime = nil
        lock.unlock()
        oldRuntime?.shutdown()
    }
}
