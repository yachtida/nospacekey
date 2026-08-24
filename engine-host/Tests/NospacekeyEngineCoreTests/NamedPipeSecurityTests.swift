import XCTest
import WinSDK
@testable import NospacekeyEngineCore

final class NamedPipeSecurityTests: XCTestCase {
    func testPipeSddlUsesProcessLogonSidAndRetainsRestrictedAccess() {
        let logonSid = "S-1-5-5-123-456"
        let sddl = pipeSddl(logonSid: logonSid)

        XCTAssertTrue(sddl.contains("(A;;GA;;;SY)"))
        XCTAssertTrue(sddl.contains("(A;;GA;;;BA)"))
        XCTAssertTrue(sddl.contains("(A;;0x12019b;;;" + logonSid + ")"))
        XCTAssertFalse(sddl.contains("GRGW"))
        XCTAssertFalse(sddl.contains("A;;GRGW;;;AU"))
        XCTAssertTrue(sddl.contains("(A;;0x12019b;;;AC)"))
        XCTAssertTrue(sddl.contains("(A;;0x12019b;;;S-1-15-2-2)"))
        XCTAssertTrue(sddl.contains("S:(ML;;NW;;;LW)"))
    }

    func testPersistentBootstrapPolicyIsNotPublishedPolicy() {
        let logonSid = "S-1-5-5-123-456"
        let bootstrap = bootstrapPipeSddl(logonSid: logonSid)
        XCTAssertTrue(bootstrap.contains("(A;;GRGW;;;" + logonSid + ")"))
        XCTAssertTrue(pipeSddl(logonSid: logonSid).contains("(A;;0x12019b;;;" + logonSid + ")"))
    }

    func testCurrentProcessLogonSidIsDynamicLogonSidOnWindowsToken() throws {
        guard let logonSid = currentProcessLogonSid() else {
            throw XCTSkip("TokenLogonSid is unavailable in this test environment")
        }
        XCTAssertTrue(logonSid.hasPrefix("S-1-5-5-"), "unexpected logon SID: " + logonSid)
    }

    func testDescriptorConversionFailureDoesNotPublishPipe() {
        var published = false
        let result = withPipeSecurityDescriptor(
            logonSid: "S-1-5-5-123-456",
            convert: { _ in nil },
            publish: { _ in
                published = true
                return true
            })

        XCTAssertNil(result)
        XCTAssertFalse(published, "a pipe must not be published without an explicit security descriptor")
    }

    func testNamedPipeRequestLimitAccepts256KiBAndRejectsOneMore() {
        XCTAssertTrue(isAcceptableNamedPipeRequestLength(namedPipeMaxRequestBodyLength))
        XCTAssertFalse(isAcceptableNamedPipeRequestLength(namedPipeMaxRequestBodyLength + 1))
    }

    func testResponseLimitAndBudgetAreIndependentFromRequestBudget() {
        XCTAssertEqual(namedPipeMaxResponseBodyLength, 16 * 1024 * 1024)
        XCTAssertEqual(namedPipeResponseBodyBudget, 16 * 1024 * 1024)
        let budget = ResponseBodyBudget(capacity: namedPipeResponseBodyBudget)
        let lease = budget.tryReserve(namedPipeMaxResponseBodyLength)
        XCTAssertNotNil(lease)
        XCTAssertEqual(budget.reservedBytes, namedPipeResponseBodyBudget)
        XCTAssertNil(budget.tryReserve(1))
        lease?.release()
        XCTAssertEqual(budget.reservedBytes, 0)
    }

    func testProductionDeadlineConstants() {
        XCTAssertEqual(namedPipeHeaderReadTimeoutMs, 300_000)
        XCTAssertEqual(namedPipeBodyReadTimeoutMs, 5_000)
        XCTAssertEqual(namedPipeReplyTimeoutMs, 5_000)
    }

    func testOverlappedCompletionUsesKernelReportedByteCount() {
        var completionCalls = 0
        let actual = completedOverlappedBytes { bytes in
            completionCalls += 1
            bytes = 17
            return true
        }
        XCTAssertEqual(completionCalls, 1)
        XCTAssertEqual(actual, 17)
    }

    func testOverlappedStorageLifetimeSpansOperationClosure() throws {
        guard let event = CreateEventW(nil, true, false, nil) else {
            throw XCTSkip("CreateEventW unavailable")
        }
        defer { CloseHandle(event) }

        var observedEvents: [HANDLE] = []
        let completed = withPersistentOverlapped(event: event) { pointer -> Bool in
            observedEvents.append(pointer.pointee.hEvent)
            pointer.pointee.Offset = 42
            observedEvents.append(pointer.pointee.hEvent)
            return pointer.pointee.Offset == 42
        }
        XCTAssertTrue(completed)
        XCTAssertEqual(observedEvents, [event, event])
    }

    func testPublishedDaclAllowsExactClientMaskRoundTrip() {
        let pipeName = #"\\.\pipe\nospacekey-mask-"# + UUID().uuidString
        let listening = DispatchSemaphore(value: 0)
        let returned = DispatchSemaphore(value: 0)
        let server = NamedPipeServer(pipeName: pipeName)
        Thread.detachNewThread {
            server.run(handler: { _, body in
                (Data(body == Data("ping".utf8) ? "pong".utf8 : "bad".utf8), false)
            }, oneShot: true, onListening: { listening.signal() })
            returned.signal()
        }
        XCTAssertEqual(listening.wait(timeout: .now() + 2), .success)

        let client: HANDLE = pipeName.withCString(encodedAs: UTF16.self) { p in
            CreateFileW(p, DWORD(0x0012_019b), 0, nil, DWORD(OPEN_EXISTING), 0, nil)
        }
        XCTAssertNotEqual(client, INVALID_HANDLE_VALUE, "exact published client mask must connect")
        guard client != INVALID_HANDLE_VALUE else { return }
        var requestLength = UInt32(4).littleEndian
        var request = Data(bytes: &requestLength, count: 4)
        request.append(Data("ping".utf8))
        let wrote = request.withUnsafeBytes { raw -> Bool in
            var count: DWORD = 0
            return WriteFile(client, raw.baseAddress!, DWORD(request.count), &count, nil) &&
                count == DWORD(request.count)
        }
        XCTAssertTrue(wrote)
        var responseLength = UInt32(0)
        let headerRead = withUnsafeMutableBytes(of: &responseLength) { raw -> Bool in
            var count: DWORD = 0
            return ReadFile(client, raw.baseAddress!, 4, &count, nil) && count == 4
        }
        XCTAssertTrue(headerRead)
        var response = Data(count: Int(UInt32(littleEndian: responseLength)))
        let responseCount = response.count
        let bodyRead = response.withUnsafeMutableBytes { raw -> Bool in
            var count: DWORD = 0
            return ReadFile(client, raw.baseAddress!, DWORD(responseCount), &count, nil) &&
                count == DWORD(responseCount)
        }
        XCTAssertTrue(bodyRead)
        XCTAssertEqual(response, Data("pong".utf8))
        CloseHandle(client)
        XCTAssertEqual(returned.wait(timeout: .now() + 2), .success)
    }

    func testRequestBodyBudgetRejectsBeyond8MiBAndReleasesLease() {
        let budget = RequestBodyBudget(capacity: namedPipeRequestBodyBudget)
        var leases: [RequestBodyLease] = []

        for _ in 0..<(namedPipeRequestBodyBudget / namedPipeMaxRequestBodyLength) {
            guard let lease = budget.tryReserve(namedPipeMaxRequestBodyLength) else {
                return XCTFail("the exact 8 MiB budget should be reservable")
            }
            leases.append(lease)
        }
        XCTAssertEqual(budget.reservedBytes, namedPipeRequestBodyBudget)
        XCTAssertNil(budget.tryReserve(1), "a request beyond the in-flight budget must be rejected")

        leases.removeAll()
        XCTAssertEqual(budget.reservedBytes, 0, "released connection leases must return their body budget")
        XCTAssertNotNil(budget.tryReserve(namedPipeMaxRequestBodyLength))
    }

    func testRequestBodyBudgetReleasesForShortReadTimeoutAndErrorSeams() throws {
        let budget = RequestBodyBudget(capacity: 4096)

        let shortRead = budget.withLease(1024) { false }
        XCTAssertEqual(shortRead, false)
        XCTAssertEqual(budget.reservedBytes, 0)

        enum ExpectedError: Error { case timeout }
        XCTAssertThrowsError(try budget.withLease(1024) {
            throw ExpectedError.timeout
        })
        XCTAssertEqual(budget.reservedBytes, 0)

        let successResult = budget.withLease(1024) { true }
        XCTAssertEqual(successResult, true)
        XCTAssertEqual(budget.reservedBytes, 0)
    }

    func testNamedPipeInstanceAndConnectionLimits() {
        XCTAssertEqual(namedPipeMaxInstances(oneShot: false), DWORD(namedPipePersistentConnectionLimit))
        XCTAssertEqual(namedPipeMaxInstances(oneShot: true), DWORD(namedPipeOneShotConnectionLimit))
    }

    func testPersistentConnectNoDataRaceRecyclesFixedHandle() {
        XCTAssertTrue(shouldRetryPersistentPipeConnectError(DWORD(ERROR_NO_DATA)))
        XCTAssertFalse(shouldRetryPersistentPipeConnectError(DWORD(ERROR_ACCESS_DENIED)))
        XCTAssertFalse(shouldRetryPersistentPipeConnectError(DWORD(ERROR_BROKEN_PIPE)))
    }

    /// Keep the real persistent pool occupied so this test distinguishes a fixed 64-instance
    /// pool from a server that silently creates/replaces instances. The background server and
    /// its handles are intentionally test-process scoped; the OS reclaims them at process exit.
    func testPersistentPoolOccupiesAllInstancesAndRecyclesOneHandle() {
        let pipeName = #"\\.\pipe\nospacekey-persistent-pool-"# + UUID().uuidString
        let listening = expectation(description: "persistent pool published")
        let server = NamedPipeServer(pipeName: pipeName)
        Thread.detachNewThread {
            server.run(handler: { _, body in
                (Data(body == Data("ping".utf8) ? "pong".utf8 : "bad".utf8), false)
            }, oneShot: false, onListening: {
                listening.fulfill()
            })
        }
        wait(for: [listening], timeout: 5)

        var clients: [HANDLE] = []
        defer {
            clients.forEach { CloseHandle($0) }
        }

        // Each successful CreateFileW occupies one of the 64 fixed instances. Retry only
        // transient startup/busy errors, with a hard test-side bound so a broken pool cannot hang
        // the test process.
        for _ in 0..<namedPipePersistentConnectionLimit {
            guard let client = openPersistentTestClient(pipeName, timeout: 5) else {
                XCTFail("could not occupy all \(namedPipePersistentConnectionLimit) instances")
                return
            }
            clients.append(client)
        }
        XCTAssertEqual(clients.count, namedPipePersistentConnectionLimit)

        let (overflow, overflowError) = tryOpenPersistentTestClientOnce(pipeName)
        XCTAssertEqual(overflow, INVALID_HANDLE_VALUE)
        XCTAssertEqual(
            overflowError,
            DWORD(ERROR_PIPE_BUSY),
            "65th connection should report ERROR_PIPE_BUSY for the occupied pool"
        )

        let first = clients.removeFirst()
        XCTAssertTrue(writePersistentTestFrame(first, body: Data("ping".utf8)))
        XCTAssertEqual(
            readPersistentTestFrame(first, timeout: 2),
            Data("pong".utf8),
            "a held pool connection must still round-trip"
        )
        CloseHandle(first)

        // Closing the first client makes its worker observe EOF, DisconnectNamedPipe, and reuse
        // the same fixed handle. A bounded retry proves the pool recycles rather than creating a
        // 65th instance.
        guard let recycled = openPersistentTestClient(pipeName, timeout: 5) else {
            XCTFail("released fixed instance was not recycled")
            return
        }
        clients.append(recycled)
        XCTAssertTrue(writePersistentTestFrame(recycled, body: Data("ping".utf8)))
        XCTAssertEqual(
            readPersistentTestFrame(recycled, timeout: 2),
            Data("pong".utf8),
            "recycled pool connection must round-trip"
        )
    }

    private func tryOpenPersistentTestClientOnce(_ pipeName: String) -> (HANDLE, DWORD) {
        var error = DWORD(ERROR_SUCCESS)
        let client = pipeName.withCString(encodedAs: UTF16.self) { p -> HANDLE in
            let handle = CreateFileW(
                p,
                DWORD(0x0012_019b),
                0,
                nil,
                DWORD(OPEN_EXISTING),
                0,
                nil)
            error = GetLastError()
            return handle ?? INVALID_HANDLE_VALUE
        }
        return (client, error)
    }

    private func openPersistentTestClient(_ pipeName: String, timeout: TimeInterval) -> HANDLE? {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            let (client, error) = tryOpenPersistentTestClientOnce(pipeName)
            if client != INVALID_HANDLE_VALUE {
                return client
            }
            guard error == DWORD(ERROR_PIPE_BUSY) ||
                    error == DWORD(ERROR_NO_DATA) ||
                    error == DWORD(ERROR_FILE_NOT_FOUND) else {
                return nil
            }
            Thread.sleep(forTimeInterval: 0.01)
        }
        return nil
    }

    private func writePersistentTestFrame(_ client: HANDLE, body: Data) -> Bool {
        var length = UInt32(body.count).littleEndian
        var frame = Data(bytes: &length, count: 4)
        frame.append(body)
        var offset = 0
        while offset < frame.count {
            var written: DWORD = 0
            let ok = frame.withUnsafeBytes { raw -> Bool in
                let pointer = raw.baseAddress!.advanced(by: offset)
                return WriteFile(client, pointer, DWORD(frame.count - offset), &written, nil)
            }
            guard ok, written > 0 else { return false }
            offset += Int(written)
        }
        return true
    }

    private func waitForPersistentTestBytes(_ client: HANDLE, deadline: Date) -> Bool {
        while Date() < deadline {
            var available: DWORD = 0
            if PeekNamedPipe(client, nil, 0, nil, &available, nil) && available > 0 {
                return true
            }
            Thread.sleep(forTimeInterval: 0.005)
        }
        return false
    }

    private func readPersistentTestFrame(_ client: HANDLE, timeout: TimeInterval) -> Data? {
        let deadline = Date().addingTimeInterval(timeout)
        var header = Data(count: 4)
        var headerOffset = 0
        while headerOffset < header.count {
            guard waitForPersistentTestBytes(client, deadline: deadline) else { return nil }
            var read: DWORD = 0
            let remaining = header.count - headerOffset
            let ok = header.withUnsafeMutableBytes { raw -> Bool in
                let pointer = raw.baseAddress!.advanced(by: headerOffset)
                return ReadFile(client, pointer, DWORD(remaining), &read, nil)
            }
            guard ok, read > 0 else { return nil }
            headerOffset += Int(read)
        }
        let length = header.withUnsafeBytes { raw -> Int in
            let bytes = raw.bindMemory(to: UInt8.self)
            return Int(UInt32(bytes[0]) |
                       (UInt32(bytes[1]) << 8) |
                       (UInt32(bytes[2]) << 16) |
                       (UInt32(bytes[3]) << 24))
        }
        guard length <= namedPipeMaxResponseBodyLength else { return nil }

        var body = Data(count: length)
        var bodyOffset = 0
        while bodyOffset < body.count {
            guard waitForPersistentTestBytes(client, deadline: deadline) else { return nil }
            var read: DWORD = 0
            let remaining = body.count - bodyOffset
            let ok = body.withUnsafeMutableBytes { raw -> Bool in
                let pointer = raw.baseAddress!.advanced(by: bodyOffset)
                return ReadFile(client, pointer, DWORD(remaining), &read, nil)
            }
            guard ok, read > 0 else { return nil }
            bodyOffset += Int(read)
        }
        return body
    }
}
