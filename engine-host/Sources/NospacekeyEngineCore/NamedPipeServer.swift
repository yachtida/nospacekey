import WinSDK
import Foundation

/// サーバが受け付けるリクエスト本体の最大バイト数。
///
/// Rust クライアントの 16 MiB framing/response 上限とは別物。サーバのリクエストは
/// JSON 操作であり、長さ前置だけで大きなバッファを確保させないため、ここでは 256 KiB
/// に制限する。
let namedPipeMaxRequestBodyLength = 256 * 1024

/// 接続中の全リクエスト本体が占有できるプロセス全体の上限。
let namedPipeRequestBodyBudget = 8 * 1024 * 1024

/// 常駐サーバの同時接続/処理スレッド上限と oneShot のインスタンス上限。
let namedPipePersistentConnectionLimit = 64
let namedPipeOneShotConnectionLimit = 1

/// response は Rust 側の wire 上限を維持する（request だけ 256 KiB に絞る）。
let namedPipeMaxResponseBodyLength = 16 * 1024 * 1024
let namedPipeResponseBodyBudget = 16 * 1024 * 1024

/// production の絶対 deadline。header の idle 待ちは長く許容するが、body/reply は
/// 各フレーム全体で一つの deadline を共有する。
let namedPipeHeaderReadTimeoutMs = 300_000
let namedPipeBodyReadTimeoutMs = 5_000
let namedPipeReplyTimeoutMs = 5_000

/// `CreateNamedPipeW` に渡す instance 数。値を一箇所に集約し、persistent が
/// 255-instance fallback に戻らないことを pure seam で検証できるようにする。
func namedPipeMaxInstances(oneShot: Bool) -> DWORD {
    DWORD(oneShot ? namedPipeOneShotConnectionLimit : namedPipePersistentConnectionLimit)
}

/// 長さ前置を読んだ直後、バッファを作る前に行う受理判定。
func isAcceptableNamedPipeRequestLength(_ length: Int) -> Bool {
    length >= 0 && length <= namedPipeMaxRequestBodyLength
}

/// 接続ごとのリクエスト本体を受理するための、thread-safe な in-flight 予算。
/// 予約に成功した lease は handler が返るまで保持し、deinit/release のどちらからでも
/// 一度だけ解放できる。長さ検査と Data allocation の間にこの予約を置くことで、複数接続の
/// 合計メモリ使用量も上限内に留める。
final class RequestBodyBudget: @unchecked Sendable {
    let capacity: Int
    private let lock = NSLock()
    private var reserved = 0

    init(capacity: Int = namedPipeRequestBodyBudget) {
        precondition(capacity >= 0)
        self.capacity = capacity
    }

    var reservedBytes: Int {
        lock.lock()
        defer { lock.unlock() }
        return reserved
    }

    func tryReserve(_ bytes: Int) -> RequestBodyLease? {
        guard bytes >= 0 else { return nil }
        lock.lock()
        guard bytes <= capacity - reserved else {
            lock.unlock()
            return nil
        }
        reserved += bytes
        lock.unlock()
        return RequestBodyLease(budget: self, bytes: bytes)
    }

    fileprivate func release(_ bytes: Int) {
        lock.lock()
        // A lease is the sole owner of each reservation. Keep this defensive guard so a
        // malformed caller cannot underflow the accounting and admit excess memory.
        reserved = max(0, reserved - bytes)
        lock.unlock()
    }

    /// Execute an operation while retaining a reservation. `defer` covers every return/throw
    /// path, while the lease deinit remains a second safety net for direct callers/tests.
    @discardableResult
    func withLease<T>(_ bytes: Int, _ operation: () throws -> T) rethrows -> T? {
        guard let lease = tryReserve(bytes) else { return nil }
        defer { lease.release() }
        return try operation()
    }
}

final class RequestBodyLease: @unchecked Sendable {
    private let budget: RequestBodyBudget
    private let bytes: Int
    private let lock = NSLock()
    private var released = false

    fileprivate init(budget: RequestBodyBudget, bytes: Int) {
        self.budget = budget
        self.bytes = bytes
    }

    func release() {
        lock.lock()
        guard !released else {
            lock.unlock()
            return
        }
        released = true
        lock.unlock()
        budget.release(bytes)
    }

    deinit { release() }
}

/// response 本体用の process-wide budget。request と同じ lease 規律を使うが、handler が
/// 返した Data の長さを検査した後、write の間だけ保持する。
final class ResponseBodyBudget: @unchecked Sendable {
    let capacity: Int
    private let lock = NSLock()
    private var reserved = 0

    init(capacity: Int = namedPipeResponseBodyBudget) {
        precondition(capacity >= 0)
        self.capacity = capacity
    }

    var reservedBytes: Int {
        lock.lock()
        defer { lock.unlock() }
        return reserved
    }

    func tryReserve(_ bytes: Int) -> ResponseBodyLease? {
        guard bytes >= 0 else { return nil }
        lock.lock()
        guard bytes <= capacity - reserved else {
            lock.unlock()
            return nil
        }
        reserved += bytes
        lock.unlock()
        return ResponseBodyLease(budget: self, bytes: bytes)
    }

    fileprivate func release(_ bytes: Int) {
        lock.lock()
        reserved = max(0, reserved - bytes)
        lock.unlock()
    }

    @discardableResult
    func withLease<T>(_ bytes: Int, _ operation: () throws -> T) rethrows -> T? {
        guard let lease = tryReserve(bytes) else { return nil }
        defer { lease.release() }
        return try operation()
    }
}

final class ResponseBodyLease: @unchecked Sendable {
    private let budget: ResponseBodyBudget
    private let bytes: Int
    private let lock = NSLock()
    private var released = false

    fileprivate init(budget: ResponseBodyBudget, bytes: Int) {
        self.budget = budget
        self.bytes = bytes
    }

    func release() {
        lock.lock()
        guard !released else {
            lock.unlock()
            return
        }
        released = true
        lock.unlock()
        budget.release(bytes)
    }

    deinit { release() }
}

final class ConnectionIDSource: @unchecked Sendable {
    private let lock = NSLock()
    private var nextID = 1

    func next() -> Int {
        lock.lock()
        defer { lock.unlock() }
        let id = nextID
        nextID += 1
        return id
    }
}

private let processRequestBodyBudget = RequestBodyBudget()
private let processResponseBodyBudget = ResponseBodyBudget()

/// クロージャを1回だけ実行するガード。`run` の onListening を accept ループから呼ぶための
/// 部品で、単体テストできるよう型として切り出す（ループ内のローカル Bool だと「1回だけ」が
/// daemon で複数接続を流す細工でしか観測できない）。
final class OnceFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var fired = false
    func fireOnce(_ body: () -> Void) {
        lock.lock()
        let already = fired
        fired = true
        lock.unlock()
        if !already { body() }
    }
}

/// The server and its same-logon-session clients share this explicit DACL.  The
/// logon SID is intentionally supplied by the caller so this formatter remains
/// a pure function (and so its policy can be tested without Win32 state).
func pipeSddl(logonSid: String) -> String {
    "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12019b;;;" + logonSid
        + ")(A;;0x12019b;;;AC)(A;;0x12019b;;;S-1-15-2-2)S:(ML;;NW;;;LW)"
}

/// Persistent pool bootstrap policy. While all fixed instances are being created the owner
/// needs `GRGW` to create the additional instances; it is never published to clients. Once the
/// whole pool is present, `SetSecurityInfo` shrinks the pipe object to `pipeSddl`'s exact mask.
func bootstrapPipeSddl(logonSid: String) -> String {
    "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;" + logonSid
        + ")(A;;0x12019b;;;AC)(A;;0x12019b;;;S-1-15-2-2)S:(ML;;NW;;;LW)"
}

/// Convert one SDDL string to a descriptor owned by LocalAlloc/LocalFree.
/// `nil` is deliberately returned for every conversion anomaly: callers must
/// not publish a pipe with a default security descriptor in that case.
func convertPipeSddlToSecurityDescriptor(_ sddl: String) -> PSECURITY_DESCRIPTOR? {
    var pSD: PSECURITY_DESCRIPTOR? = nil
    let (converted, lastError) = sddl.withCString(encodedAs: UTF16.self) { p -> (Bool, DWORD) in
        let ok = ConvertStringSecurityDescriptorToSecurityDescriptorW(
            p, DWORD(SDDL_REVISION_1), &pSD, nil)
        let error = GetLastError()
        return (ok != false, error)
    }
    guard converted else {
        engineLog("nospacekey-engine pipe acl: SDDL conversion failed error=\(lastError)\n")
        return nil
    }
    guard let pSD else {
        engineLog("nospacekey-engine pipe acl: SDDL conversion returned nil descriptor\n")
        return nil
    }
    return pSD
}

/// Execute the publication step only while an explicit descriptor is alive.
/// This is the fail-closed seam: a conversion failure never invokes `publish`.
/// The descriptor remains allocated for the whole publication operation and is
/// released exactly once when that operation returns.
@discardableResult
func withPipeSecurityDescriptor<T>(
    logonSid: String,
    convert: (String) -> PSECURITY_DESCRIPTOR? = convertPipeSddlToSecurityDescriptor,
    publish: (PSECURITY_DESCRIPTOR) -> T
) -> T? {
    guard let pSD = convert(pipeSddl(logonSid: logonSid)) else { return nil }
    defer { LocalFree(pSD) }
    return publish(pSD)
}

/// Replace the bootstrap DACL on the named-pipe object with the published DACL. The descriptor
/// remains owned by the surrounding `withPipeSecurityDescriptor` scope while this call runs.
func setPublishedPipeDacl(_ hPipe: HANDLE, descriptor: PSECURITY_DESCRIPTOR) -> Bool {
    var present = WindowsBool(false)
    var dacl: PACL? = nil
    var defaulted = WindowsBool(false)
    let gotDacl = GetSecurityDescriptorDacl(descriptor, &present, &dacl, &defaulted)
    guard gotDacl != false, present != WindowsBool(false), let dacl else {
        engineLog("nospacekey-engine pipe acl: GetSecurityDescriptorDacl failed\n")
        return false
    }
    let status = SetSecurityInfo(
        hPipe,
        SE_KERNEL_OBJECT,
        DWORD(DACL_SECURITY_INFORMATION),
        nil,
        nil,
        dacl,
        nil)
    guard status == DWORD(ERROR_SUCCESS) else {
        engineLog("nospacekey-engine pipe acl: SetSecurityInfo failed error=\(status)\n")
        return false
    }
    return true
}

/// Return the logon-session SID (S-1-5-5-X-Y) from the current process token.
/// TokenLogonSid is documented as a single-entry TOKEN_GROUPS value; the
/// count/pointer/validity checks below keep malformed token data fail-closed.
func currentProcessLogonSid() -> String? {
    var token: HANDLE? = nil
    let opened = OpenProcessToken(GetCurrentProcess(), DWORD(TOKEN_QUERY), &token)
    let openError = GetLastError()
    guard opened != false, let token else {
        engineLog("nospacekey-engine pipe acl: OpenProcessToken failed error=\(openError)\n")
        return nil
    }
    defer { CloseHandle(token) }

    var requiredLength: DWORD = 0
    let queried = GetTokenInformation(
        token, TokenLogonSid, nil, 0, &requiredLength)
    let queryError = GetLastError()
    guard queried == false,
          queryError == DWORD(ERROR_INSUFFICIENT_BUFFER),
          requiredLength > 0 else {
        engineLog("nospacekey-engine pipe acl: TokenLogonSid size query failed error=\(queryError)\n")
        return nil
    }

    let byteCount = Int(requiredLength)
    let groupAlignment = MemoryLayout<SID_AND_ATTRIBUTES>.alignment
    let firstGroupOffset = (MemoryLayout<DWORD>.size + groupAlignment - 1) & ~(groupAlignment - 1)
    let minimumByteCount = firstGroupOffset + MemoryLayout<SID_AND_ATTRIBUTES>.size
    guard byteCount >= minimumByteCount,
          minimumByteCount >= MemoryLayout<TOKEN_GROUPS>.size else {
        engineLog("nospacekey-engine pipe acl: malformed TokenLogonSid size=\(requiredLength)\n")
        return nil
    }

    let raw = UnsafeMutableRawPointer.allocate(
        byteCount: byteCount,
        alignment: max(MemoryLayout<TOKEN_GROUPS>.alignment, groupAlignment))
    defer { raw.deallocate() }

    var returnedLength: DWORD = 0
    let fetched = GetTokenInformation(
        token, TokenLogonSid, raw, requiredLength, &returnedLength)
    let fetchError = GetLastError()
    guard fetched != false,
          Int(returnedLength) >= minimumByteCount else {
        engineLog("nospacekey-engine pipe acl: TokenLogonSid read failed error=\(fetchError)\n")
        return nil
    }

    let groups = raw.assumingMemoryBound(to: TOKEN_GROUPS.self)
    guard groups.pointee.GroupCount == 1 else {
        engineLog("nospacekey-engine pipe acl: TokenLogonSid group count is not one\n")
        return nil
    }
    let firstGroup = raw.advanced(by: firstGroupOffset)
        .assumingMemoryBound(to: SID_AND_ATTRIBUTES.self)
    guard let sid = firstGroup.pointee.Sid, IsValidSid(sid) != false else {
        engineLog("nospacekey-engine pipe acl: TokenLogonSid is invalid\n")
        return nil
    }

    var stringSid: UnsafeMutablePointer<WCHAR>? = nil
    let converted = ConvertSidToStringSidW(sid, &stringSid)
    let conversionError = GetLastError()
    guard converted != false, let stringSid else {
        engineLog("nospacekey-engine pipe acl: ConvertSidToStringSidW failed error=\(conversionError)\n")
        return nil
    }
    defer { LocalFree(stringSid) }

    // Make the Swift copy before LocalFree releases the Win32 buffer.
    let copied = String(decodingCString: UnsafePointer(stringSid), as: UTF16.self)
    guard copied.hasPrefix("S-1-5-5-") else {
        engineLog("nospacekey-engine pipe acl: unexpected TokenLogonSid \(copied)\n")
        return nil
    }
    return copied
}

private enum PipeIOResult {
    case completed(DWORD)
    case timedOut
    case failed(DWORD)
}

private enum PipeConnectResult {
    case connected
    case timedOut
    case failed(DWORD)
}

/// A client can disconnect between two persistent-pool connect attempts. Win32 reports that
/// harmless recycle race as ERROR_NO_DATA; the fixed handle remains valid after DisconnectNamedPipe
/// and must be retried instead of being removed from the pool.
func shouldRetryPersistentPipeConnectError(_ error: DWORD) -> Bool {
    error == DWORD(ERROR_NO_DATA)
}

/// Read the completed byte count from the kernel completion query.  In particular, this does
/// not accept a byte count supplied by the initial ReadFile/WriteFile call: overlapped handles
/// require those APIs' byte-count out parameter to be nil.
@discardableResult
func completedOverlappedBytes(
    _ completion: (inout DWORD) -> Bool
) -> DWORD? {
    var bytes: DWORD = 0
    guard completion(&bytes) else { return nil }
    return bytes
}

private func monotonicMilliseconds() -> UInt64 {
    DispatchTime.now().uptimeNanoseconds / 1_000_000
}

private func deadlineAfterMilliseconds(_ milliseconds: Int) -> UInt64 {
    monotonicMilliseconds() + UInt64(max(0, milliseconds))
}

private func remainingMilliseconds(until deadline: UInt64) -> DWORD {
    let now = monotonicMilliseconds()
    if now >= deadline { return 0 }
    return DWORD(min(UInt64(DWORD.max), deadline - now))
}

/// Keep the OVERLAPPED storage at a stable address until the operation has either completed or
/// been cancelled and reaped. Swift's implicit inout pointer for a local variable is only
/// guaranteed during the call expression, while an IO_PENDING operation can retain the pointer
/// in the kernel after that expression returns.
@discardableResult
func withPersistentOverlapped<T>(
    event: HANDLE,
    _ body: (UnsafeMutablePointer<OVERLAPPED>) -> T
) -> T {
    let pointer = UnsafeMutablePointer<OVERLAPPED>.allocate(capacity: 1)
    pointer.initialize(to: OVERLAPPED())
    pointer.pointee.hEvent = event
    defer {
        pointer.deinitialize(count: 1)
        pointer.deallocate()
    }
    return body(pointer)
}

/// Start one overlapped operation and wait until its absolute deadline. A timeout always calls
/// CancelIoEx and then reaps the event before returning, so the OVERLAPPED/buffer can safely leave
/// scope. ERROR_NOT_FOUND is the expected cancel-vs-completion race and is still reaped.
/// `deadline == nil` waits indefinitely; WAIT_FAILED still follows the shared cancel+reap path.
/// Only the next-request header wait may pass nil, so body/reply frames stay on finite deadlines
/// at the type level.
private func waitForOverlapped(
    _ hPipe: HANDLE,
    deadline: UInt64?,
    start: (UnsafeMutablePointer<OVERLAPPED>) -> Bool
) -> PipeIOResult {
    if let deadline {
        guard remainingMilliseconds(until: deadline) > 0 else {
            return .timedOut
        }
    }
    guard let event = CreateEventW(nil, true, false, nil) else {
        return .failed(GetLastError())
    }
    defer { CloseHandle(event) }

    return withPersistentOverlapped(event: event) { overlapped in
        let started = start(overlapped)
        if started {
            let completed = completedOverlappedBytes { bytes in
                GetOverlappedResult(hPipe, overlapped, &bytes, false)
            }
            return completed.map { .completed($0) } ?? .failed(GetLastError())
        }
        let startError = GetLastError()
        guard startError == DWORD(ERROR_IO_PENDING) else {
            return .failed(startError)
        }

        // 待機直前に再計算する: event 作成と I/O 開始が予算を消費しても、絶対 deadline が
        // それらで延長されてはならない(nil は無期限のまま)。
        let waitResult = WaitForSingleObject(
            event,
            deadline.map { remainingMilliseconds(until: $0) } ?? DWORD(INFINITE))
        if waitResult == DWORD(WAIT_OBJECT_0) {
            let completed = completedOverlappedBytes { bytes in
                GetOverlappedResult(hPipe, overlapped, &bytes, false)
            }
            return completed.map { .completed($0) } ?? .failed(GetLastError())
        }

        // WAIT_TIMEOUT and WAIT_FAILED both terminate the connection. Cancel then wait forever
        // for completion: returning while the kernel still owns the OVERLAPPED is a use-after-free.
        let waitError = GetLastError()
        let cancelled = CancelIoEx(hPipe, overlapped)
        let cancelError = GetLastError()
        if !cancelled && cancelError != DWORD(ERROR_NOT_FOUND) {
            engineLog("nospacekey-engine pipe io: CancelIoEx failed error=\(cancelError)\n")
        }
        // Normally the event is signalled by completion. If the reap wait itself fails, use
        // GetOverlappedResult's blocking mode as the final lifetime barrier. The pointer remains
        // allocated by withPersistentOverlapped until this whole closure returns.
        let reapWait = WaitForSingleObject(event, DWORD(INFINITE))
        if reapWait == DWORD(WAIT_OBJECT_0) {
            _ = completedOverlappedBytes { bytes in
                GetOverlappedResult(hPipe, overlapped, &bytes, false)
            }
        } else {
            var bytes: DWORD = 0
            _ = GetOverlappedResult(hPipe, overlapped, &bytes, true)
        }
        if waitResult == DWORD(WAIT_TIMEOUT) {
            return .timedOut
        }
        return .failed(waitError)
    }
}

private func connectOverlapped(_ hPipe: HANDLE, deadline: UInt64) -> PipeConnectResult {
    guard remainingMilliseconds(until: deadline) > 0 else {
        return .timedOut
    }
    guard let event = CreateEventW(nil, true, false, nil) else {
        return .failed(GetLastError())
    }
    defer { CloseHandle(event) }

    return withPersistentOverlapped(event: event) { overlapped in
        let connected = ConnectNamedPipe(hPipe, overlapped)
        if connected { return .connected }
        let connectError = GetLastError()
        if connectError == DWORD(ERROR_PIPE_CONNECTED) { return .connected }
        guard connectError == DWORD(ERROR_IO_PENDING) else { return .failed(connectError) }

        let waitResult = WaitForSingleObject(event, remainingMilliseconds(until: deadline))
        if waitResult == DWORD(WAIT_OBJECT_0) {
            var transferred: DWORD = 0
            return GetOverlappedResult(hPipe, overlapped, &transferred, false)
                || GetLastError() == DWORD(ERROR_PIPE_CONNECTED)
                ? .connected
                : .failed(GetLastError())
        }
        let waitError = GetLastError()
        let cancelled = CancelIoEx(hPipe, overlapped)
        let cancelError = GetLastError()
        if !cancelled && cancelError != DWORD(ERROR_NOT_FOUND) {
            engineLog("nospacekey-engine pipe connect: CancelIoEx failed error=\(cancelError)\n")
        }
        let reapWait = WaitForSingleObject(event, DWORD(INFINITE))
        if reapWait == DWORD(WAIT_OBJECT_0) {
            var transferred: DWORD = 0
            _ = GetOverlappedResult(hPipe, overlapped, &transferred, false)
        } else {
            var transferred: DWORD = 0
            _ = GetOverlappedResult(hPipe, overlapped, &transferred, true)
        }
        if waitResult == DWORD(WAIT_TIMEOUT) {
            return .timedOut
        }
        return .failed(waitError)
    }
}

/// 受信フレーム本体のように「フレーム全体で一つの絶対期限」を持つ読み取り。有限 deadline
/// 専用 — nil を受ける入口は readNextRequestHeader にだけ存在する。
private func readExactOverlapped(_ hPipe: HANDLE, count: Int, deadline: UInt64) -> Data? {
    readExactOverlappedCore(hPipe, count: count, deadline: deadline)
}

/// 次リクエストの header(4 バイト)待ち。deadline == nil は無期限(oneShot GPU ワーカー)。
/// 5 分アイドルでワーカーが自発終了しないようにする専用入口で、型を分けることで
/// body 読み取りへ nil が流れるのを防ぐ。
private func readNextRequestHeader(_ hPipe: HANDLE, deadline: UInt64?) -> Data? {
    readExactOverlappedCore(hPipe, count: MemoryLayout<UInt32>.size, deadline: deadline)
}

private func readExactOverlappedCore(_ hPipe: HANDLE, count: Int, deadline: UInt64?) -> Data? {
    if count == 0 { return Data() }
    var data = Data(count: count)
    var offset = 0
    while offset < count {
        let result = data.withUnsafeMutableBytes { raw -> PipeIOResult in
            let destination = raw.baseAddress!.advanced(by: offset)
            return waitForOverlapped(hPipe, deadline: deadline) { overlapped in
                ReadFile(hPipe, destination, DWORD(count - offset), nil, overlapped)
            }
        }
        guard case .completed(let read) = result, read > 0 else { return nil }
        offset += Int(read)
    }
    return data
}

private func writeAllOverlapped(_ hPipe: HANDLE, data: Data, deadline: UInt64) -> Bool {
    var offset = 0
    while offset < data.count {
        let result = data.withUnsafeBytes { raw -> PipeIOResult in
            let source = raw.baseAddress!.advanced(by: offset)
            return waitForOverlapped(hPipe, deadline: deadline) { overlapped in
                WriteFile(hPipe, source, DWORD(data.count - offset), nil, overlapped)
            }
        }
        guard case .completed(let wrote) = result, wrote > 0 else { return false }
        offset += Int(wrote)
    }
    return true
}

/// 固定 instance pool を overlapped I/O で処理する名前付きパイプサーバ。
///
/// **常駐モード**（oneShot=false）: nMaxInstances=64 で複数パイプインスタンスを生成し、
/// 64 個の pipe instance を起動時に作り、各 instance を一つの detached worker が再利用する。
/// 複数の TIP クライアントが同時接続でき、作成後の接続では CreateNamedPipeW を呼ばない。
/// リクエストハンドラ自体の直列化は呼び出し元（EngineHost.serviceLock）が担う。
///
/// **oneShot モード**（oneShot=true）: nMaxInstances=1 の単一インスタンス。1接続を処理して
/// 切断したら run を抜けてプロセスを終了する。TIP がプロセス毎に一意パイプ名でエンジンを
/// 起動する後方互換モード（1クライアント専用）。
final class NamedPipeServer: @unchecked Sendable {
    let pipeName: String   // 例: #"\\.\pipe\nospacekey-engine"#
    private let requestBodyBudget: RequestBodyBudget
    private let responseBodyBudget: ResponseBodyBudget

    init(pipeName: String,
         requestBodyBudget: RequestBodyBudget = processRequestBodyBudget,
         responseBodyBudget: ResponseBodyBudget = processResponseBodyBudget) {
        self.pipeName = pipeName
        self.requestBodyBudget = requestBodyBudget
        self.responseBodyBudget = responseBodyBudget
    }

    /// handler: (接続id, 受信した1リクエスト本体(JSON)) -> 返信本体(JSON)。長さ前置はこのクラスが付与/除去する。
    /// 接続id は接続ごとに一意（accept のたびに単調増加）。常駐モードでは複数 TIP クライアントが
    /// 別接続で同時接続しうるため、ハンドラはこの id で「どの接続のセッションか」を識別できる。
    /// onDisconnect: 常駐モードで接続が切れた（serve が抜けた）際に、その接続id で1回呼ばれる。
    /// TIP が EndSession を送らずパイプを落とす経路（タイムアウト劣化・アプリ強制終了）でも
    /// サーバ側で当該接続のセッションを掃除できるようにする（Bug 2）。呼び出し元は serviceLock 下で処理すること。
    /// oneShot=true なら1接続を処理して切断したら **run を抜けて終了** する。
    /// TIP はプロセス毎に一意パイプ名でこのエンジンを起動する＝1クライアント専用なので、
    /// 接続が切れたら（＝ホストアプリ終了/IME 非活性化）プロセスを残さず終わらせる。
    /// onListening: persistent では全固定 instance 作成と DACL 縮小の成功後、oneShot では
    /// pipe 作成成功後に1回だけ呼ばれる。呼ばれた時点でクライアントは接続できる＝「接続可能に
    /// なった後」を要求する処理（カスタム辞書の再読 — spec §4.1 の spawn 窓閉塞）の起点。
    /// requestHeaderIdleTimeoutMs: 接続確立後の「次リクエスト header 待ち」の idle タイムアウト(ms)。
    /// nil = 無期限(oneShot 専用 — 単一クライアントが接続を保持し続ける GPU ワーカー用)。
    /// 初回 accept 待ちは常に namedPipeHeaderReadTimeoutMs のまま。
    func run(handler: @escaping @Sendable (Int, Data) -> (reply: Data, exitAfterReply: Bool),
             onDisconnect: @escaping @Sendable (Int) -> Void = { _ in },
             oneShot: Bool = false,
             requestHeaderIdleTimeoutMs: Int? = namedPipeHeaderReadTimeoutMs,
             exitHook: @escaping @Sendable () -> Void = { exit(0) },
             onListening: @escaping @Sendable () -> Void = {}) {
        // 無期限 header 待ち(nil)は「単一クライアントが接続を保持する」oneShot 専用。
        // persistent の quiet-period recycle(タイムアウトで accept へ戻る)と両立しないため。
        precondition(oneShot || requestHeaderIdleTimeoutMs != nil,
                     "requestHeaderIdleTimeoutMs = nil is oneShot-only")
        guard let logonSid = currentProcessLogonSid() else {
            engineLog("nospacekey-engine pipe acl: current process logon SID unavailable; refusing pipe\n")
            return
        }
        guard let handles = createPipePool(logonSid: logonSid, oneShot: oneShot) else {
            engineLog("nospacekey-engine pipe acl: fixed pipe pool unavailable; refusing pipe\n")
            return
        }
        // Publication is after every instance exists and after DACL shrink. A client can only
        // connect once the entire fixed pool is ready.
        onListening()

        let ids = ConnectionIDSource()
        if oneShot {
            let hPipe = handles[0]
            guard case .connected = connectOverlapped(
                hPipe, deadline: deadlineAfterMilliseconds(namedPipeHeaderReadTimeoutMs)) else {
                CloseHandle(hPipe)
                return
            }
            serveConnected(hPipe, connId: ids.next(), headerIdleTimeoutMs: requestHeaderIdleTimeoutMs,
                           handler: handler, exitHook: exitHook)
            DisconnectNamedPipe(hPipe)
            CloseHandle(hPipe)
            return
        }

        // One detached worker owns one fixed handle for its entire lifetime. It never creates a
        // replacement instance: after a disconnect it returns the same handle to ConnectNamedPipe.
        let group = DispatchGroup()
        for handle in handles {
            let handleInt = Int(bitPattern: handle)
            group.enter()
            Thread.detachNewThread { [self] in
                defer { group.leave() }
                let hPipe = UnsafeMutableRawPointer(bitPattern: handleInt)!
                servePersistent(hPipe, ids: ids, headerIdleTimeoutMs: requestHeaderIdleTimeoutMs,
                                handler: handler, onDisconnect: onDisconnect,
                                exitHook: exitHook)
            }
        }
        group.wait()
    }

    private func createPipePool(logonSid: String, oneShot: Bool) -> [HANDLE]? {
        if oneShot {
            let result = withPipeSecurityDescriptor(logonSid: logonSid) { descriptor -> HANDLE in
                var sa = SECURITY_ATTRIBUTES()
                sa.nLength = DWORD(MemoryLayout<SECURITY_ATTRIBUTES>.size)
                sa.lpSecurityDescriptor = descriptor
                sa.bInheritHandle = false
                return pipeName.withCString(encodedAs: UTF16.self) { name in
                    withUnsafeMutablePointer(to: &sa) { saPtr in
                        let openMode = DWORD(PIPE_ACCESS_DUPLEX) | DWORD(FILE_FLAG_OVERLAPPED)
                        let h = CreateNamedPipeW(name, openMode,
                            DWORD(PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT), 1,
                            64 * 1024, 64 * 1024, 0, saPtr)
                        return h ?? INVALID_HANDLE_VALUE
                    }
                }
            }
            guard let hPipe = result, hPipe != INVALID_HANDLE_VALUE else { return nil }
            return [hPipe]
        }

        var handles: [HANDLE] = []
        let bootstrapped = withPipeSecurityDescriptor(
            logonSid: logonSid,
            convert: { _ in convertPipeSddlToSecurityDescriptor(bootstrapPipeSddl(logonSid: logonSid)) },
            publish: { descriptor -> Bool in
                var sa = SECURITY_ATTRIBUTES()
                sa.nLength = DWORD(MemoryLayout<SECURITY_ATTRIBUTES>.size)
                sa.lpSecurityDescriptor = descriptor
                sa.bInheritHandle = false
                for index in 0..<namedPipePersistentConnectionLimit {
                    let openMode = DWORD(PIPE_ACCESS_DUPLEX) | DWORD(FILE_FLAG_OVERLAPPED)
                        | (index == 0 ? DWORD(FILE_FLAG_FIRST_PIPE_INSTANCE) | DWORD(WRITE_DAC) : 0)
                    let h = pipeName.withCString(encodedAs: UTF16.self) { name in
                        withUnsafeMutablePointer(to: &sa) { saPtr in
                            CreateNamedPipeW(name, openMode,
                                DWORD(PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT),
                                namedPipeMaxInstances(oneShot: false),
                                64 * 1024, 64 * 1024, 0, saPtr)
                        }
                    }
                    guard let h, h != INVALID_HANDLE_VALUE else { return false }
                    handles.append(h)
                }
                return true
            }) == true
        guard bootstrapped else {
            handles.forEach { CloseHandle($0) }
            return nil
        }

        let published = withPipeSecurityDescriptor(logonSid: logonSid) { descriptor in
            guard let first = handles.first else { return false }
            return setPublishedPipeDacl(first, descriptor: descriptor)
        } == true
        guard published else {
            handles.forEach { CloseHandle($0) }
            return nil
        }
        return handles
    }

    private func servePersistent(_ hPipe: HANDLE, ids: ConnectionIDSource,
                                 headerIdleTimeoutMs: Int?,
                                 handler: @escaping @Sendable (Int, Data) -> (reply: Data, exitAfterReply: Bool),
                                 onDisconnect: @escaping @Sendable (Int) -> Void,
                                 exitHook: @escaping @Sendable () -> Void) {
        while true {
            switch connectOverlapped(
                hPipe, deadline: deadlineAfterMilliseconds(namedPipeHeaderReadTimeoutMs)) {
            case .connected:
                break
            case .timedOut:
                // Return the fixed handle to the pool after a quiet-period timeout; no new
                // instance is created and the next ConnectNamedPipe reuses this same handle.
                DisconnectNamedPipe(hPipe)
                continue
            case .failed(let error):
                if shouldRetryPersistentPipeConnectError(error) {
                    // A client can win the disconnect/connect race. Reset this exact fixed
                    // instance and let the worker retry; all other failures retire the handle.
                    DisconnectNamedPipe(hPipe)
                    continue
                }
                CloseHandle(hPipe)
                return
            }
            let connId = ids.next()
            _ = serveConnected(hPipe, connId: connId, headerIdleTimeoutMs: headerIdleTimeoutMs,
                               handler: handler, exitHook: exitHook)
            // serveConnected has returned only after all request/response leases are released.
            onDisconnect(connId)
            DisconnectNamedPipe(hPipe)
        }
        CloseHandle(hPipe)
    }

    @discardableResult
    private func serveConnected(_ hPipe: HANDLE, connId: Int,
                                headerIdleTimeoutMs: Int?,
                                handler: @escaping @Sendable (Int, Data) -> (reply: Data, exitAfterReply: Bool),
                                exitHook: @escaping @Sendable () -> Void) -> Bool {
        while true {
            let headerDeadline = headerIdleTimeoutMs.map { deadlineAfterMilliseconds($0) }
            guard let lenData = readNextRequestHeader(hPipe, deadline: headerDeadline) else {
                return false
            }
            let n = lenData.withUnsafeBytes { raw -> Int in
                let bytes = raw.bindMemory(to: UInt8.self)
                return Int(UInt32(bytes[0])
                    | (UInt32(bytes[1]) << 8)
                    | (UInt32(bytes[2]) << 16)
                    | (UInt32(bytes[3]) << 24))
            }
            guard isAcceptableNamedPipeRequestLength(n) else { return false }

            let result: (shouldContinue: Bool, exitAfterReply: Bool)? = requestBodyBudget.withLease(n) {
                let bodyDeadline = deadlineAfterMilliseconds(namedPipeBodyReadTimeoutMs)
                guard let body = readExactOverlapped(hPipe, count: n, deadline: bodyDeadline) else {
                    return (false, false)
                }
                let (replyBody, exitAfterReply) = handler(connId, body)
                guard replyBody.count <= namedPipeMaxResponseBodyLength,
                      let replyLength = UInt32(exactly: replyBody.count) else {
                    return (false, exitAfterReply)
                }
                guard let responseResult = responseBodyBudget.withLease(replyBody.count, {
                          let replyDeadline = deadlineAfterMilliseconds(namedPipeReplyTimeoutMs)
                          var length = replyLength.littleEndian
                          let header = Data(bytes: &length, count: MemoryLayout<UInt32>.size)
                          guard writeAllOverlapped(hPipe, data: header, deadline: replyDeadline),
                                writeAllOverlapped(hPipe, data: replyBody, deadline: replyDeadline) else {
                              return (false, exitAfterReply)
                          }
                          return (true, exitAfterReply)
                      }) else { return (false, exitAfterReply) }
                return responseResult
            }
            guard let result else { return false }
            if result.exitAfterReply { exitHook() }
            guard result.shouldContinue else { return false }
        }
    }
}
