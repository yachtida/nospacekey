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
        let runtimeFolder = environment["NOSPACEKEY_PREDICTION_RUNTIME_DIR"].map(URL.init(fileURLWithPath:))
            ?? exe.deletingLastPathComponent().appending(path: "prediction-runtime")
        return Self(enabled: enabled, modelFolder: modelFolder, runtimeFolder: runtimeFolder)
    }

    var modelURL: URL { modelFolder.appending(path: Self.modelFilename) }
    var serverURL: URL { runtimeFolder.appending(path: "llama-server.exe") }
    var runtimeRevisionURL: URL { runtimeFolder.appending(path: "REVISION") }
    var verifiedReceiptURL: URL { modelFolder.appending(path: "VERIFIED") }

    var filesArePresent: Bool {
        [modelURL, serverURL, runtimeRevisionURL, verifiedReceiptURL]
            .allSatisfy { FileManager.default.fileExists(atPath: $0.path) }
    }

    var runtimeRevisionMatches: Bool {
        guard let text = try? String(contentsOf: runtimeRevisionURL, encoding: .utf8) else { return false }
        return text.trimmingCharacters(in: .whitespacesAndNewlines) == Self.llamaRevision
    }

    var verifiedReceiptMatches: Bool {
        (try? String(contentsOf: verifiedReceiptURL, encoding: .utf8)) == Self.verifiedReceipt
    }
}

let predictionProcessCreationFlags = DWORD(CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT)

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
    let processIdentifier: Int32

    init(executableURL: URL, arguments: [String]) throws {
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
        guard nullHandle != INVALID_HANDLE_VALUE else {
            throw LLMError(message: "cannot open null device for prediction process")
        }
        defer { CloseHandle(nullHandle) }

        var startup = STARTUPINFOW()
        startup.cb = DWORD(MemoryLayout<STARTUPINFOW>.size)
        startup.dwFlags = DWORD(STARTF_USESTDHANDLES) | DWORD(STARTF_USESHOWWINDOW)
        startup.wShowWindow = WORD(SW_HIDE)
        startup.hStdInput = nullHandle
        startup.hStdOutput = nullHandle
        startup.hStdError = nullHandle

        let commandLine = ([executableURL.path] + arguments)
            .map(quoteWindowsProcessArgument)
            .joined(separator: " ")
        var commandBuffer = Array(commandLine.utf16) + [0]
        var processInfo = PROCESS_INFORMATION()
        let workingDirectory = executableURL.deletingLastPathComponent().path
        let created = executableURL.path.withCString(encodedAs: UTF16.self) { executable in
            workingDirectory.withCString(encodedAs: UTF16.self) { directory in
                commandBuffer.withUnsafeMutableBufferPointer { command in
                    CreateProcessW(
                        executable, command.baseAddress, nil, nil, true,
                        predictionProcessCreationFlags, nil, directory,
                        &startup, &processInfo
                    )
                }
            }
        }
        guard created else {
            throw LLMError(message: "cannot start prediction process (win32=\(GetLastError()))")
        }
        CloseHandle(processInfo.hThread)
        handle = processInfo.hProcess
        processIdentifier = Int32(bitPattern: GetProcessId(processInfo.hProcess))
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

    deinit { CloseHandle(handle) }
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
            let arguments = [
                "-m", config.modelURL.path,
                "-t", String(max(1, min(12, ProcessInfo.processInfo.activeProcessorCount))),
                "-c", "512", "-np", "1", "--host", "127.0.0.1",
                "--port", String(port), "--no-webui", "--api-key", apiKey,
            ]
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
                return runtime
            } catch {
                lastError = error
                process?.terminate()
            }
        }
        throw lastError
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
    current == next && availability != .failed
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
        let service = PredictionService()
        service.reload(config: .resolve(environment: environment))
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
                restartAfterRuntimeExit(runtime)
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
        guard config.runtimeRevisionMatches, config.verifiedReceiptMatches else {
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

    /// A crashed llama-server must not leave the setting permanently enabled-but-silent. Mark the
    /// exact dead runtime failed, then reuse the normal generation-guarded reload path to restart it.
    private func restartAfterRuntimeExit(_ exitedRuntime: LlamaPredictionRuntime) {
        lock.lock()
        guard runtime === exitedRuntime, !exitedRuntime.isRunning, let config = currentConfig else {
            lock.unlock()
            return
        }
        availability = .failed
        lock.unlock()
        reload(config: config)
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
