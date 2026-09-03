import XCTest
import WinSDK
@testable import NospacekeyEngineCore

/// GPU ワーカーの「5 分アイドル自発終了(worker_exit quarantine の根因)」回帰テスト。
///
/// run の default 引数(namedPipeHeaderReadTimeoutMs = 300_000)が従来値であることは
/// NamedPipeSecurityTests.testProductionDeadlineConstants の定数ピンが担保する。
/// 有限側・無期限側の両テストが同じ「request A → reply A → idle」の形をとることで、
/// 実症状(1 リクエスト完了後の次 header 待ち)の両面を固定する。
final class GPUWorkerIdleTimeoutTests: XCTestCase {

    // ---- クライアント側ヘルパー ----

    private func connectClient(_ pipeName: String) -> HANDLE {
        // WinSDK の GENERIC_READ は DWORD、GENERIC_WRITE は Int32 なので型を揃えてから OR する。
        pipeName.withCString(encodedAs: UTF16.self) { p in
            CreateFileW(p, GENERIC_READ | DWORD(GENERIC_WRITE), 0, nil,
                        DWORD(OPEN_EXISTING), DWORD(FILE_FLAG_OVERLAPPED), nil)
        }
    }

    /// 同期 ReadFile には期限を付けられない。サーバが reply を返さない回帰が
    /// 失敗報告ではなくテストランナー全体の停止にならないよう、overlapped + 5 秒期限で待つ。
    private func waitForClientIO(
        _ client: HANDLE,
        _ start: (UnsafeMutablePointer<OVERLAPPED>) -> Bool
    ) -> DWORD? {
        guard let event = CreateEventW(nil, true, false, nil) else { return nil }
        defer { CloseHandle(event) }
        let pointer = UnsafeMutablePointer<OVERLAPPED>.allocate(capacity: 1)
        pointer.initialize(to: OVERLAPPED())
        pointer.pointee.hEvent = event
        defer {
            pointer.deinitialize(count: 1)
            pointer.deallocate()
        }
        if start(pointer) {
            var bytes: DWORD = 0
            return GetOverlappedResult(client, pointer, &bytes, false) ? bytes : nil
        }
        guard GetLastError() == DWORD(ERROR_IO_PENDING) else { return nil }
        var waitResult = WaitForSingleObject(event, 5_000)
        if waitResult != DWORD(WAIT_OBJECT_0) {
            _ = CancelIoEx(client, pointer)
            waitResult = WaitForSingleObject(event, DWORD(INFINITE))
        }
        var bytes: DWORD = 0
        guard waitResult == DWORD(WAIT_OBJECT_0),
              GetOverlappedResult(client, pointer, &bytes, false) else { return nil }
        return bytes
    }

    private func writeAll(_ client: HANDLE, _ data: Data) -> Bool {
        data.withUnsafeBytes { raw in
            guard let base = raw.baseAddress else { return false }
            var offset = 0
            while offset < data.count {
                guard let written = waitForClientIO(client, { pointer in
                    WriteFile(client, base.advanced(by: offset),
                              DWORD(data.count - offset), nil, pointer)
                }), written > 0 else { return false }
                offset += Int(written)
            }
            return true
        }
    }

    private func readExact(_ client: HANDLE, count: Int) -> Data? {
        var data = Data(count: count)
        var offset = 0
        while offset < count {
            let got = data.withUnsafeMutableBytes { raw -> DWORD? in
                guard let base = raw.baseAddress else { return nil }
                return waitForClientIO(client, { pointer in
                    ReadFile(client, base.advanced(by: offset),
                             DWORD(count - offset), nil, pointer)
                })
            }
            guard let got, got > 0 else { return nil }
            offset += Int(got)
        }
        return data
    }

    /// 長さ前置(4 バイト LE) + 本文。サーバの serveConnected フレーム規約と同じ形。
    private func sendFrame(_ client: HANDLE, body: Data) -> Bool {
        var length = UInt32(body.count).littleEndian
        var buffer = Data()
        withUnsafeBytes(of: &length) { buffer.append(contentsOf: $0) }
        buffer.append(body)
        return writeAll(client, buffer)
    }

    private func readFrame(_ client: HANDLE) -> Data? {
        guard let header = readExact(client, count: 4) else { return nil }
        let length = header.withUnsafeBytes { raw -> Int in
            let bytes = raw.bindMemory(to: UInt8.self)
            return Int(UInt32(bytes[0]) | (UInt32(bytes[1]) << 8) |
                       (UInt32(bytes[2]) << 16) | (UInt32(bytes[3]) << 24))
        }
        return readExact(client, count: length)
    }

    /// 有限 timeout では実症状と同じ「request A 完了 → idle」の次 header 待ちで run が戻る。
    /// nil 側の対照であり、無期限化した差分が本当に存在すること(次 header 待ちに有限が
    /// 劲き続けていること)を固定する。
    func testOneShotRunExitsWhenInjectedHeaderIdleTimeoutElapses() {
        let pipeName = #"\\.\pipe\nospacekey-idle-control-"# + UUID().uuidString
        let server = NamedPipeServer(pipeName: pipeName)
        let listening = DispatchSemaphore(value: 0)
        let returned = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            server.run(handler: { _, body in (body, false) },
                       oneShot: true,
                       requestHeaderIdleTimeoutMs: 200,
                       onListening: { listening.signal() })
            returned.signal()
        }
        XCTAssertEqual(listening.wait(timeout: .now() + 5), .success, "onListening が呼ばれない")

        let client = connectClient(pipeName)
        XCTAssertNotEqual(client, INVALID_HANDLE_VALUE, "クライアント接続が失敗した")

        let payloadA = Data("request-a".utf8)
        XCTAssertTrue(sendFrame(client, body: payloadA), "request A の送信が失敗した")
        XCTAssertEqual(readFrame(client), payloadA, "request A への echo reply が返らない")

        // request A 完了後の idle(注入 timeout 200ms の 3 倍)で次 header 待ちが発火する。
        XCTAssertEqual(returned.wait(timeout: .now() + 2), .success,
                       "注入した 200ms の header idle timeout で run が戻るべき")
        CloseHandle(client)
    }

    /// nil(無期限)では idle 窓をまたいで request B に応答し続け、クライアント切断(EOF)で
    /// 初めて run が戻る。5 分アイドル自発終了の実症状を短時間に縮約した再現。
    func testOneShotRunWithNilHeaderIdleTimeoutServesRequestAcrossIdleWindow() {
        let pipeName = #"\\.\pipe\nospacekey-idle-nil-"# + UUID().uuidString
        let server = NamedPipeServer(pipeName: pipeName)
        let listening = DispatchSemaphore(value: 0)
        let returned = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            server.run(handler: { _, body in (body, false) },
                       oneShot: true,
                       requestHeaderIdleTimeoutMs: nil,
                       onListening: { listening.signal() })
            returned.signal()
        }
        XCTAssertEqual(listening.wait(timeout: .now() + 5), .success, "onListening が呼ばれない")

        let client = connectClient(pipeName)
        XCTAssertNotEqual(client, INVALID_HANDLE_VALUE, "クライアント接続が失敗した")

        let payloadA = Data("request-a".utf8)
        XCTAssertTrue(sendFrame(client, body: payloadA), "request A の送信が失敗した")
        XCTAssertEqual(readFrame(client), payloadA, "request A への echo reply が返らない")

        // idle 窓: コントロール(200ms)が発火する 3 倍。ここを生き延びるのが本修正の本体。
        Thread.sleep(forTimeInterval: 0.6)

        let payloadB = Data("request-b".utf8)
        XCTAssertTrue(sendFrame(client, body: payloadB), "idle 後の request B の送信が失敗した")
        XCTAssertEqual(readFrame(client), payloadB, "idle 後の request B に reply が返らない")

        XCTAssertEqual(returned.wait(timeout: .now() + 0.1), .timedOut,
                       "nil では run が戻ってはいけない(アイドル自発終了の回帰)")

        // 切断(EOF)では run が戻る — テスト後始末と、親が pipe を閉じた時の正常終了経路の確認。
        CloseHandle(client)
        XCTAssertEqual(returned.wait(timeout: .now() + 5), .success,
                       "クライアント切断後も run が戻らない(スレッド・ハンドル漏れ)")
    }

    func testGPUWorkerListenConfigurationKeepsHeaderWaitInfinite() {
        let listen = makeGPUWorkerListenConfiguration()
        XCTAssertTrue(listen.oneShot, "ワーカーは単一接続の oneShot 待ち受け")
        XCTAssertNil(listen.requestHeaderIdleTimeoutMs,
                     "ワーカーの次リクエスト header 待ちは無期限でなければならない(5 分アイドル自発終了の回帰)")
    }
}
