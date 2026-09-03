import Foundation
#if os(Windows)
import WinSDK
#endif

/// Production transport for the isolated worker.  It deliberately keeps all
/// wire I/O behind the GPUWorkerTransport seam so fault-injection tests do not
/// need to spawn a native process.
public final class NativeGPUWorkerTransport: GPUWorkerTransport, @unchecked Sendable {
#if os(Windows)
    private let lock = NSLock()
    private var process: HANDLE?
    private var job: HANDLE?
    private var pipe: HANDLE?
    private var pipeName: String?
#if DEBUG
    // The production path always calls CancelIoEx.  This hook makes the
    // cancellation-failure lifetime path deterministic in the Windows E2E
    // test without changing release behavior.
    nonisolated(unsafe) static var cancelIoExForTesting:
        ((HANDLE, UnsafeMutablePointer<OVERLAPPED>) -> (cancelled: Bool, error: DWORD))?
#endif
#endif
    private let executableURL: URL

    public init(executableURL: URL? = nil) {
        self.executableURL = executableURL ??
            (Bundle.main.executableURL ?? URL(fileURLWithPath: CommandLine.arguments[0]))
    }

    public func start(generation: UInt64) -> GPUWorkerTransportStartResult {
        start(generation: generation, configuration: nil)
    }

    public func start(generation: UInt64,
                     configuration: GPUWorkerRuntimeConfiguration?) -> GPUWorkerTransportStartResult {
#if os(Windows)
        guard configuration != nil else {
            return .failure(.invalidRuntimeDirectory)
        }
        terminate()
        let name = Self.makePipeName()
        guard spawn(pipeName: name) else { return .failure(.unavailable) }
        guard connect(to: name, timeout: 3) else {
            return .failure(.workerExit)
        }
        let handshake = GPUWorkerHandshakeRequest(
            generation: generation, configuration: configuration?.wire)
        guard let body = try? JSONEncoder().encode(handshake) else {
            return .failure(.protocolMismatch)
        }
        let handshakeResult = performHandshake(body: body, timeout: 5)
        if case .timedOut = handshakeResult {
            return .failure(.timeout)
        }
        if case .failed = handshakeResult {
            let failure: GPUWorkerFailure = processIsRunning() ? .protocolMismatch :
                (processExitedWithFailure() ? .crash : .workerExit)
            return .failure(failure)
        }
        guard case .success(let responseBody) = handshakeResult,
              let response = try? JSONDecoder().decode(GPUWorkerHandshakeResponse.self, from: responseBody),
              response.version == GPUWorkerRequest.currentVersion,
              response.generation == generation else {
            return .failure(.protocolMismatch)
        }
        guard response.ready else {
            return .failure(Self.mapRuntimeFailure(response.failure))
        }
        let backend = response.backend ?? ""
        let device = response.device ?? ""
        engineLog("ev=zenzai_worker_ready backend=\(backend) device=\(device)\n")
        return .ready(backend: response.backend ?? "", device: response.device ?? "")
#else
        _ = generation
        _ = configuration
        return .failure(.unavailable)
#endif
    }

    public func request(_ request: GPUWorkerRequest, timeout: TimeInterval) -> GPUWorkerTransportReply {
#if os(Windows)
        guard let body = try? JSONEncoder().encode(request) else { return .protocolMismatch }
        guard let pipe = currentPipe() else { return .exit }
        let deadline = deadlineAfter(timeout)
        switch writeFrame(body, deadline: deadline, on: pipe) {
        case .timedOut:
            return .timeout
        case .failed:
            return processIsRunning() ? .nativeFailure :
                (processExitedWithFailure() ? .crash : .exit)
        case .success:
            switch readFrame(deadline: deadline, on: pipe) {
            case .success(let responseBody):
                guard let response = try? JSONDecoder().decode(
                    GPUWorkerResponse.self, from: responseBody) else {
                    return .protocolMismatch
                }
                return .response(response)
            case .timedOut:
                return .timeout
            case .failed:
                return processIsRunning() ? .nativeFailure :
                    (processExitedWithFailure() ? .crash : .exit)
            }
        }
#else
        _ = request
        _ = timeout
        return .exit
#endif
    }

    public func terminate() {
#if os(Windows)
        lock.lock()
        let oldPipe = pipe
        let oldProcess = process
        let oldJob = job
        pipe = nil
        process = nil
        job = nil
        pipeName = nil
        lock.unlock()
        if let oldPipe { CloseHandle(oldPipe) }
        if let oldProcess {
            var code = DWORD(0)
            if GetExitCodeProcess(oldProcess, &code), code == DWORD(STILL_ACTIVE) {
                _ = TerminateProcess(oldProcess, 1)
            }
            // Reaping a killed child may take longer than the caller's live
            // deadline. Keep the handles alive on a detached reaper instead of
            // making quarantine wait behind process teardown.
            Self.reap(oldProcess: oldProcess, oldJob: oldJob)
        } else if let oldJob {
            CloseHandle(oldJob)
        }
#endif
    }

#if os(Windows)
    private enum HandshakeResult {
        case success(Data)
        case failed
        case timedOut
    }

    private enum PipeIOResult {
        case completed(DWORD)
        case timedOut
        case failed(DWORD)

        var isTimedOut: Bool {
            if case .timedOut = self { return true }
            return false
        }
    }

    private enum FrameResult {
        case success(Data)
        case timedOut
        case failed
    }

    /// Owns a buffer passed to an overlapped operation.  The pointer must stay
    /// valid after the caller's `withUnsafeBytes` scope when cancellation is
    /// racing with completion, so the operation gets an independent allocation.
    private final class OwnedOverlappedBuffer {
        let pointer: UnsafeMutableRawPointer
        let count: Int

        init(count: Int) {
            self.count = count
            pointer = UnsafeMutableRawPointer.allocate(
                byteCount: max(1, count), alignment: MemoryLayout<UInt8>.alignment)
            pointer.initializeMemory(as: UInt8.self, repeating: 0, count: max(1, count))
        }

        convenience init(copying data: Data) {
            self.init(count: data.count)
            guard !data.isEmpty else { return }
            data.withUnsafeBytes { bytes in
                pointer.copyMemory(from: bytes.baseAddress!, byteCount: data.count)
            }
        }

        func makeData(count: Int) -> Data {
            Data(bytes: pointer, count: count)
        }

        deinit {
            pointer.deallocate()
        }
    }

    /// Keeps every object the kernel can still reference alive.  A duplicate
    /// pipe handle lets the transport close its current handle immediately
    /// after a deadline without invalidating the detached reaper's result call.
    private final class PendingOverlappedOperation: @unchecked Sendable {
        let pipe: HANDLE
        let event: HANDLE
        let overlapped: UnsafeMutablePointer<OVERLAPPED>
        let buffer: OwnedOverlappedBuffer

        init?(pipe: HANDLE, buffer: OwnedOverlappedBuffer) {
            var duplicate: HANDLE?
            guard DuplicateHandle(
                GetCurrentProcess(), pipe, GetCurrentProcess(), &duplicate,
                DWORD(0), false, DWORD(DUPLICATE_SAME_ACCESS)),
                let duplicate else {
                return nil
            }
            guard let event = CreateEventW(nil, true, false, nil) else {
                CloseHandle(duplicate)
                return nil
            }
            self.pipe = duplicate
            self.event = event
            self.buffer = buffer
            overlapped = UnsafeMutablePointer<OVERLAPPED>.allocate(capacity: 1)
            overlapped.initialize(to: OVERLAPPED())
            overlapped.pointee.hEvent = event
        }

        deinit {
            overlapped.deinitialize(count: 1)
            overlapped.deallocate()
            CloseHandle(event)
            CloseHandle(pipe)
        }
    }

    private static func reap(oldProcess: HANDLE, oldJob: HANDLE?) {
        let processAddress = Int(bitPattern: oldProcess)
        let jobAddress = oldJob.map { Int(bitPattern: $0) }
        Thread.detachNewThread {
            guard let process = UnsafeMutableRawPointer(bitPattern: processAddress) else { return }
            let job = jobAddress.flatMap { UnsafeMutableRawPointer(bitPattern: $0) }
            let waitResult = WaitForSingleObject(process, 5_000)
            if waitResult == DWORD(WAIT_TIMEOUT) {
                _ = TerminateProcess(process, 1)
                _ = WaitForSingleObject(process, 100)
            }
            if let job { CloseHandle(job) }
            CloseHandle(process)
        }
    }

    /// A synchronous ReadFile cannot be given a deadline. Use an overlapped
    /// client pipe so timeout can cancel and reap the kernel operation before
    /// the handle is closed or the worker is quarantined.
    private func performHandshake(body: Data, timeout: TimeInterval) -> HandshakeResult {
        guard let pipe = currentPipe() else { return .failed }
        let deadline = deadlineAfter(timeout)
        switch writeFrame(body, deadline: deadline, on: pipe) {
        case .timedOut:
            return .timedOut
        case .failed:
            return .failed
        case .success:
            switch readFrame(deadline: deadline, on: pipe) {
            case .success(let response): return .success(response)
            case .timedOut: return .timedOut
            case .failed: return .failed
            }
        }
    }

    private static func makePipeName() -> String {
        // UUID/nonce is never logged and is not used as a status field.
        "\\\\.\\pipe\\nospacekey-zenzai-worker-" + UUID().uuidString
    }

    private static func quote(_ value: String) -> String {
        guard value.contains(where: { $0 == " " || $0 == "\"" }) else { return value }
        return "\"" + value.replacingOccurrences(of: "\"", with: "\\\"") + "\""
    }

    private func spawn(pipeName: String) -> Bool {
        var startup = STARTUPINFOW()
        startup.cb = DWORD(MemoryLayout<STARTUPINFOW>.size)
        startup.dwFlags = DWORD(STARTF_USESTDHANDLES) | DWORD(STARTF_USESHOWWINDOW)
        startup.wShowWindow = WORD(SW_HIDE)
        startup.hStdInput = GetStdHandle(STD_INPUT_HANDLE)
        startup.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE)
        startup.hStdError = GetStdHandle(STD_ERROR_HANDLE)
        let command = [Self.quote(executableURL.path), "--zenzai-gpu-worker", Self.quote(pipeName)]
            .joined(separator: " ")
        var commandBuffer = Array(command.utf16) + [0]
        var processInfo = PROCESS_INFORMATION()
        let directory = executableURL.deletingLastPathComponent().path
        let flags = DWORD(CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT)
        engineInheritableHandleProcessCreationLock.lock()
        let created = executableURL.path.withCString(encodedAs: UTF16.self) { executable in
            directory.withCString(encodedAs: UTF16.self) { workingDirectory in
                commandBuffer.withUnsafeMutableBufferPointer { commandLine in
                    CreateProcessW(executable, commandLine.baseAddress, nil, nil, true,
                                   flags, nil, workingDirectory, &startup, &processInfo)
                }
            }
        }
        engineInheritableHandleProcessCreationLock.unlock()
        guard created else { return false }
        CloseHandle(processInfo.hThread)
        guard let job = CreateJobObjectW(nil, nil) else {
            _ = TerminateProcess(processInfo.hProcess, 1)
            CloseHandle(processInfo.hProcess)
            return false
        }
        var info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION()
        info.BasicLimitInformation.LimitFlags = DWORD(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)
        let configured = withUnsafePointer(to: &info) { pointer in
            SetInformationJobObject(job, JobObjectExtendedLimitInformation,
                                     UnsafeMutableRawPointer(mutating: pointer),
                                     DWORD(MemoryLayout<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>.size))
        }
        guard configured, AssignProcessToJobObject(job, processInfo.hProcess) else {
            CloseHandle(job)
            _ = TerminateProcess(processInfo.hProcess, 1)
            CloseHandle(processInfo.hProcess)
            return false
        }
        lock.lock()
        process = processInfo.hProcess
        self.job = job
        self.pipeName = pipeName
        lock.unlock()
        engineLog("ev=zenzai_worker_spawn pid=\(GetProcessId(processInfo.hProcess))\n")
        return true
    }

    private func currentPipe() -> HANDLE? {
        lock.lock(); defer { lock.unlock() }
        return pipe
    }

    private func processIsRunning() -> Bool {
        lock.lock(); defer { lock.unlock() }
        guard let process else { return false }
        var code = DWORD(0)
        return GetExitCodeProcess(process, &code) && code == DWORD(STILL_ACTIVE)
    }

    private func processExitedWithFailure() -> Bool {
        lock.lock(); defer { lock.unlock() }
        guard let process else { return false }
        var code = DWORD(0)
        guard GetExitCodeProcess(process, &code), code != DWORD(STILL_ACTIVE) else {
            return false
        }
        return code != 0
    }

    private func connect(to name: String, timeout: TimeInterval) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            let handle = name.withCString(encodedAs: UTF16.self) { path in
            CreateFileW(path, DWORD(UInt32(GENERIC_READ) | UInt32(GENERIC_WRITE)),
                            DWORD(UInt32(FILE_SHARE_READ) | UInt32(FILE_SHARE_WRITE)), nil,
                            DWORD(OPEN_EXISTING),
                            DWORD(FILE_ATTRIBUTE_NORMAL) | DWORD(FILE_FLAG_OVERLAPPED), nil)
            }
            if let handle, handle != INVALID_HANDLE_VALUE {
                lock.lock(); pipe = handle; lock.unlock()
                return true
            }
            Thread.sleep(forTimeInterval: 0.01)
        }
        return false
    }

    private func writeFrame(_ body: Data, deadline: UInt64, on pipe: HANDLE) -> FrameResult {
        guard body.count <= namedPipeMaxRequestBodyLength,
              let length = UInt32(exactly: body.count) else { return .failed }
        var frameLength = length.littleEndian
        let headerBuffer = OwnedOverlappedBuffer(count: MemoryLayout<UInt32>.size)
        withUnsafeBytes(of: &frameLength) { bytes in
            headerBuffer.pointer.copyMemory(
                from: bytes.baseAddress!, byteCount: MemoryLayout<UInt32>.size)
        }
        let header = waitForOverlapped(pipe, deadline: deadline, buffer: headerBuffer) {
            pipe, pointer, count, overlapped in
            WriteFile(pipe, pointer, DWORD(count), nil, overlapped)
        }
        guard case .completed(let headerBytes) = header, headerBytes == 4 else {
            return header.isTimedOut ? .timedOut : .failed
        }
        guard !body.isEmpty else { return .success(Data()) }
        let bodyBuffer = OwnedOverlappedBuffer(copying: body)
        let bodyResult = waitForOverlapped(pipe, deadline: deadline, buffer: bodyBuffer) {
            pipe, pointer, count, overlapped in
            WriteFile(pipe, pointer, DWORD(count), nil, overlapped)
        }
        guard case .completed(let bodyBytes) = bodyResult,
              bodyBytes == DWORD(body.count) else {
            return bodyResult.isTimedOut ? .timedOut : .failed
        }
        return .success(Data())
    }

    private func readFrame(deadline: UInt64, on pipe: HANDLE) -> FrameResult {
        let headerResult = readExact(pipe, count: 4, deadline: deadline)
        guard case .success(let header) = headerResult else { return headerResult }
        let length = header.withUnsafeBytes { raw -> Int in
            let bytes = raw.bindMemory(to: UInt8.self)
            return Int(UInt32(bytes[0]) | (UInt32(bytes[1]) << 8) |
                       (UInt32(bytes[2]) << 16) | (UInt32(bytes[3]) << 24))
        }
        guard length <= namedPipeMaxResponseBodyLength else { return .failed }
        return readExact(pipe, count: length, deadline: deadline)
    }

    private func readExact(_ pipe: HANDLE, count: Int, deadline: UInt64) -> FrameResult {
        guard count >= 0 else { return .failed }
        if count == 0 { return .success(Data()) }
        var data = Data(count: count)
        var offset = 0
        while offset < count {
            let buffer = OwnedOverlappedBuffer(count: count - offset)
            let result = waitForOverlapped(pipe, deadline: deadline, buffer: buffer) {
                pipe, pointer, count, overlapped in
                ReadFile(pipe, pointer, DWORD(count), nil, overlapped)
            }
            switch result {
            case .completed(let bytes) where bytes > 0:
                let transferred = min(Int(bytes), buffer.count)
                data.replaceSubrange(
                    offset..<(offset + transferred), with: buffer.makeData(count: transferred))
                offset += transferred
            case .timedOut:
                return .timedOut
            case .completed, .failed:
                return .failed
            }
        }
        return .success(data)
    }

    private func waitForOverlapped(
        _ pipe: HANDLE,
        deadline: UInt64,
        buffer: OwnedOverlappedBuffer,
        start: (HANDLE, UnsafeMutableRawPointer, Int, UnsafeMutablePointer<OVERLAPPED>) -> Bool
    ) -> PipeIOResult {
        guard remainingMilliseconds(until: deadline) > 0 else { return .timedOut }
        guard let operation = PendingOverlappedOperation(pipe: pipe, buffer: buffer) else {
            return .failed(GetLastError())
        }

        guard !start(operation.pipe, buffer.pointer, buffer.count, operation.overlapped) else {
            var bytes = DWORD(0)
            return GetOverlappedResult(operation.pipe, operation.overlapped, &bytes, false)
                ? .completed(bytes) : .failed(GetLastError())
        }
        let startError = GetLastError()
        guard startError == DWORD(ERROR_IO_PENDING) else { return .failed(startError) }

        let waitResult = WaitForSingleObject(
            operation.event, remainingMilliseconds(until: deadline))
        if waitResult == DWORD(WAIT_OBJECT_0) {
            var bytes = DWORD(0)
            return GetOverlappedResult(operation.pipe, operation.overlapped, &bytes, false)
                ? .completed(bytes) : .failed(GetLastError())
        }

        let waitError = GetLastError()
        let cancellation: (cancelled: Bool, error: DWORD)
#if DEBUG
        if let cancelIoExForTesting = Self.cancelIoExForTesting {
            cancellation = cancelIoExForTesting(operation.pipe, operation.overlapped)
        } else {
            let cancelled = CancelIoEx(operation.pipe, operation.overlapped)
            cancellation = (cancelled, GetLastError())
        }
#else
        let didCancel = CancelIoEx(operation.pipe, operation.overlapped)
        cancellation = (didCancel, GetLastError())
#endif
        let cancelled = cancellation.cancelled
        let cancelError = cancellation.error
        if !cancelled && cancelError != DWORD(ERROR_NOT_FOUND) {
            engineLog("ev=zenzai_worker_pipe_cancel_failed error=\(cancelError)\n")
        }

        // Completion can race cancellation. If it is already complete, release
        // the operation here; otherwise transfer the complete operation (pipe,
        // OVERLAPPED, event, and buffer) to a detached reaper. The caller keeps
        // its deadline instead of waiting for an unbounded kernel operation.
        var bytes = DWORD(0)
        if GetOverlappedResult(operation.pipe, operation.overlapped, &bytes, false) {
            return waitResult == DWORD(WAIT_TIMEOUT) ? .timedOut : .failed(waitError)
        }
        Self.reap(operation)
        return waitResult == DWORD(WAIT_TIMEOUT) ? .timedOut : .failed(waitError)
    }

    private static func reap(_ operation: PendingOverlappedOperation) {
        Thread.detachNewThread {
            var bytes = DWORD(0)
            _ = GetOverlappedResult(operation.pipe, operation.overlapped, &bytes, true)
        }
    }

    private func deadlineAfter(_ timeout: TimeInterval) -> UInt64 {
        let milliseconds = max(0, Int64(timeout * 1_000))
        let now = DispatchTime.now().uptimeNanoseconds / 1_000_000
        return now + UInt64(milliseconds)
    }

    private func remainingMilliseconds(until deadline: UInt64) -> DWORD {
        let now = DispatchTime.now().uptimeNanoseconds / 1_000_000
        guard now < deadline else { return 0 }
        return DWORD(min(UInt64(DWORD.max), deadline - now))
    }

    private static func mapRuntimeFailure(_ failure: GPUWorkerRuntimeFailure?) -> GPUWorkerFailure {
        guard let failure else { return .warmup }
        switch failure {
        case .invalidRuntimeDirectory: return .invalidRuntimeDirectory
        case .backendPathRejected: return .backendPathRejected
        case .backendUnavailable: return .backendUnavailable
        case .gpuUnavailable: return .gpuUnavailable
        case .modelLoad: return .modelLoad
        case .contextLoad: return .contextLoad
        case .decode: return .decode
        case .unknown, .none: return .warmup
        }
    }
#endif
}
