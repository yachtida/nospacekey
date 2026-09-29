import XCTest
import Foundation
import WinSDK
@testable import NospacekeyEngineCore

/// 背景スレッドが書いた応答をテスト本体へ渡す箱（Swift 6 の Sendable 検査のため —
/// 生の var キャプチャも XCTestCase 自身のキャプチャも @Sendable クロージャでは弾かれる）。
private final class ReplyBox: @unchecked Sendable {
    private let lock = NSLock()
    private var stored: Data?
    var data: Data? {
        get { lock.lock(); defer { lock.unlock() }; return stored }
        set { lock.lock(); stored = newValue; lock.unlock() }
    }
}

final class EngineHostHandlerTests: XCTestCase {
    func testBlockedWindowsPredictionDoesNotBlockHybridLiveSnapshot() throws {
        let entered = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let finished = DispatchSemaphore(value: 0)
        let service = ConversionService(config: .init(weightURL: nil, inferenceLimit: 1),
            learning: .disabled, environment: ["NOSPACEKEY_CONVERSION_ENGINE": "hybrid"],
            fileSystem: .live, windowsTextProvider: .init { _, prediction, _ in
                if prediction {
                    entered.signal()
                    _ = release.wait(timeout: .now() + 3)
                }
                return ["Windows予測"]
            })
        let handler = makeEngineHandler(service: service, serviceLock: NSLock())
        let prediction = Data(#"{"method":"InputPredictions","params":{"key":{"identity":{"composition":1,"revision":1,"configuration_generation":1,"connection_generation":1},"baseline":0,"conversion_revision":0,"clause_id":1,"request_id":1},"reading":"がぞ","reading_start":0,"reading_end":2,"preceding_surfaces":[]}}"#.utf8)
        Thread.detachNewThread {
            _ = handler(1, prediction)
            finished.signal()
        }
        XCTAssertEqual(entered.wait(timeout: .now() + 2), .success)
        // The provider remains blocked until after the live reply. A serialized
        // handler would exhaust the live deadline and return Error instead.
        let live = handler(2, try snapshotRequest())
        release.signal()
        XCTAssertEqual(resultTag(live), "SnapshotResult")
        XCTAssertEqual(finished.wait(timeout: .now() + 3), .success)
    }

    private func snapshotRequest(deadline: UInt64? = nil) throws -> Data {
        var wire: [String: Any] = ["method": "LiveSnapshot", "params": [
            "composition": 1, "revision": 1, "configuration_generation": 1,
            "connection_generation": 1, "conversion_revision": 0, "request_id": 1,
            "segments": [["text": "a"]], "explicit": false,
        ]]
        if let deadline { wire["deadline_tick_ms"] = deadline }
        return try JSONSerialization.data(withJSONObject: wire)
    }

    func testExpiredSnapshotDoesNotWaitForServiceLockOrRunConversion() throws {
        let serviceLock = NSLock()
        let service = makeService()
        let handler = makeEngineHandler(service: service, serviceLock: serviceLock)
        serviceLock.lock()
        defer { serviceLock.unlock() }
        let request = try snapshotRequest(deadline: GetTickCount64() + 60)
        let reply = ReplyBox()
        let done = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            reply.data = handler(1, request).reply
            done.signal()
        }
        XCTAssertEqual(done.wait(timeout: .now() + 2), .success)
        XCTAssertEqual(resultTag((reply.data ?? Data(), false)), "Error")
        XCTAssertEqual(service.zenzaiVendorInvocationCountForTesting, 0)
    }

    func testSnapshotDeadlineAlsoBoundsConverterLockWait() throws {
        let service = makeService()
        let serviceLock = NSLock()
        let handler = makeEngineHandler(service: service, serviceLock: serviceLock)
        let release = service.beginConverterLockHoldForTesting()
        defer { release() }
        let request = try snapshotRequest(deadline: GetTickCount64() + 60)
        let reply = ReplyBox()
        let done = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            reply.data = handler(1, request).reply
            done.signal()
        }
        XCTAssertEqual(done.wait(timeout: .now() + 2), .success)
        XCTAssertEqual(resultTag((reply.data ?? Data(), false)), "Error")
        // Timed-out work must release the global lock for another client.
        let available = serviceLock.try()
        XCTAssertTrue(available)
        if available { serviceLock.unlock() }
    }

    func testDeadlineIsOptionalCappedAndIgnoredForMutatingRequests() throws {
        let old = try JSONDecoder().decode(RequestEnvelope.self, from: snapshotRequest())
        XCTAssertEqual(old.admissionDeadline(receivedAt: 100)?.tickMilliseconds, 500)
        let distant = try JSONDecoder().decode(RequestEnvelope.self, from: snapshotRequest(deadline: UInt64.max))
        XCTAssertEqual(distant.admissionDeadline(receivedAt: 100)?.tickMilliseconds, 500)
        let expired = try JSONDecoder().decode(RequestEnvelope.self, from: snapshotRequest(deadline: 0))
        XCTAssertTrue(expired.admissionDeadline(receivedAt: GetTickCount64())!.expired)
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        let started = handler(7, Data(#"{"method":"StartSession","deadline_tick_ms":0}"#.utf8))
        let session = try XCTUnwrap(sessionId(started))
        let ended = handler(7, Data(#"{"method":"EndSession","params":{"session":\#(session)},"deadline_tick_ms":0}"#.utf8))
        XCTAssertEqual(resultTag(ended), "Ok")
    }

    func testEndSessionAcknowledgesBeforeDeferredConverterCleanupCompletes() throws {
        let service = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let session = service.startSession(connection: 17)
        let release = service.beginConverterLockHoldForTesting()
        let handler = makeEngineHandler(service: service, serviceLock: NSLock())
        let request = Data(#"{"method":"EndSession","params":{"session":\#(session)}}"#.utf8)

        let finished = DispatchSemaphore(value: 0)
        let reply = ReplyBox()
        Thread.detachNewThread {
            reply.data = handler(17, request).reply
            finished.signal()
        }
        XCTAssertEqual(finished.wait(timeout: .now() + .milliseconds(100)), .success,
                       "EndSession acknowledgement must not wait for converter maintenance")
        XCTAssertEqual(resultTag((reply.data ?? Data(), false)), "Ok")
        release()
        service.flushMaintenanceForTesting()
    }
    /// 応答 JSON の "result" タグを取り出す（Response は Encodable のみなので生 JSON で検証する）。
    /// handler は (reply, exitAfterReply) を返すので outcome を直接受けて reply を検証する。
    func resultTag(_ outcome: (reply: Data, exitAfterReply: Bool)) -> String? {
        let obj = try? JSONSerialization.jsonObject(with: outcome.reply) as? [String: Any]
        return obj?["result"] as? String
    }
    /// StartSession 応答から session id を取り出す。
    func sessionId(_ outcome: (reply: Data, exitAfterReply: Bool)) -> Int64? {
        let obj = try? JSONSerialization.jsonObject(with: outcome.reply) as? [String: Any]
        return (obj?["session"] as? NSNumber)?.int64Value
    }
    func makeService() -> ConversionService {
        ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
    }

    func testPingRoundtripsThroughHandler() {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        let resp = handler(1, Data(#"{"method":"Ping"}"#.utf8))
        XCTAssertEqual(resultTag(resp), "Pong")
    }

    func testPingDoesNotWaitForTheConversionServiceLock() {
        let serviceLock = NSLock()
        let handler = makeEngineHandler(service: makeService(), serviceLock: serviceLock)
        serviceLock.lock()
        defer { serviceLock.unlock() }
        let reply = ReplyBox()
        let done = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            reply.data = handler(1, Data(#"{"method":"Ping"}"#.utf8)).reply
            done.signal()
        }
        XCTAssertEqual(done.wait(timeout: .now() + 1), .success)
        XCTAssertEqual(resultTag((reply.data ?? Data(), false)), "Pong")
    }

    func testZenzaiStatusQueryDoesNotWaitForConverterLock() throws {
        let service = ConversionService(
            config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1,
                                 disabledReason: .userDisabled))
        let handler = makeEngineHandler(service: service, serviceLock: NSLock())
        let release = service.beginConverterLockHoldForTesting()
        defer { release() }
        let reply = ReplyBox()
        let done = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            reply.data = handler(1, Data(#"{"method":"QueryZenzaiStatus"}"#.utf8)).reply
            done.signal()
        }
        XCTAssertEqual(done.wait(timeout: .now() + 2), .success,
                       "status query must use the non-blocking snapshot")
        let object = try JSONSerialization.jsonObject(with: reply.data ?? Data()) as! [String: Any]
        XCTAssertEqual(object["result"] as? String, "ZenzaiStatus")
        XCTAssertEqual(object["state"] as? String, "disabled")
        XCTAssertEqual(object["reason"] as? String, "user_disabled")
        XCTAssertNil(object["path"])
        XCTAssertNil(object["input"])
        XCTAssertNil(object["candidates"])
        XCTAssertNil(object["latency_live"], "absent stats must be omitted on the wire")
    }

    func testZenzaiRetryIsAcknowledgedBeforeConverterLockIsAvailable() {
        let service = makeService()
        let handler = makeEngineHandler(service: service, serviceLock: NSLock())
        let release = service.beginConverterLockHoldForTesting()
        defer { release() }
        let reply = ReplyBox()
        let done = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            reply.data = handler(1, Data(#"{"method":"RetryZenzai"}"#.utf8)).reply
            done.signal()
        }
        XCTAssertEqual(done.wait(timeout: .now() + 2), .success,
                       "retry acknowledgement must not wait for native warm-up")
        XCTAssertEqual(resultTag((reply.data ?? Data(), false)), "Ok")
    }

    // version handshake: 新エンジンは StartSession 応答に proto=PROTO_VERSION を載せる。
    func testStartSessionCarriesProtoVersion() throws {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        let obj = try JSONSerialization.jsonObject(
            with: handler(1, Data(#"{"method":"StartSession"}"#.utf8)).reply) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "Session")
        XCTAssertEqual(obj["proto"] as? Int, 11)
        XCTAssertEqual(obj["boot"] as? String, BuildInfo.version)
    }

    // graceful 停止: Shutdown は Ok を返し、かつ「応答後に exit」を要求する（実際の exit(0) は
    // NamedPipeServer が writeAll 成功後に行う＝ここでは handler の契約 exitAfterReply=true を固定）。
    // makeService() は learning .disabled なので prepareForShutdown の flush は no-op（実 dir を汚さない）。
    func testShutdownReturnsOkAndRequestsExitAfterReply() {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        let outcome = handler(1, Data(#"{"method":"Shutdown"}"#.utf8))
        XCTAssertEqual(resultTag(outcome), "Ok")
        XCTAssertTrue(outcome.exitAfterReply)
    }

    func testMaintenanceShutdownRejectsActiveInputAndStopsOnlyWhenIdle() {
        let service = makeService()
        let handler = makeEngineHandler(service: service, serviceLock: NSLock())
        let started = handler(7, Data(#"{"method":"StartSession"}"#.utf8))
        let session = sessionId(started)!
        let request = Data(#"{"method":"PrepareMaintenance"}"#.utf8)
        let busy = handler(9, request)
        XCTAssertEqual(resultTag(busy), "Error")
        XCTAssertFalse(busy.exitAfterReply)

        _ = handler(7, Data(#"{"method":"EndSession","params":{"session":\#(session)}}"#.utf8))
        let accepted = handler(9, request)
        XCTAssertEqual(resultTag(accepted), "Ok")
        XCTAssertTrue(accepted.exitAfterReply)
        XCTAssertEqual(resultTag(handler(7, Data(#"{"method":"StartSession"}"#.utf8))), "Error")
    }

    // 通常 op は exit を要求しない（Shutdown 以外で誤って engine が落ちないことの固定）。
    func testNonShutdownDoesNotRequestExit() {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        XCTAssertFalse(handler(1, Data(#"{"method":"Ping"}"#.utf8)).exitAfterReply)
    }

    func testMalformedBodyYieldsErrorNotCrash() {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        XCTAssertEqual(resultTag(handler(1, Data("not json".utf8))), "Error")
    }





    // UU-5: ReloadConfig は session を伴わずに Ok を返す（decode→dispatch→反映のスモーク）。
    func testReloadConfigDispatchesToOk() {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        let body = Data(#"{"method":"ReloadConfig","params":{"llm_enabled":false,"llm_api_key":"","llm_endpoint":"","llm_model":"","llm_prompt":"","llm_timeout_ms":15000,"zenzai_enabled":false,"zenzai_weight":""}}"#.utf8)
        XCTAssertEqual(resultTag(handler(1, body)), "Ok")
    }

    // Spec2: ClearLearning は session を伴わず Ok を返す（decode→dispatch→反映のスモーク）。
    // ⚠サービスには**一時 dir の学習設定を注入**する。makeService()（= learning .disabled）のままだと
    // clearLearning の dir フォールバックが**開発機の実 %LOCALAPPDATA%\nospacekey\memory を消す**（C-4）。
    func testClearLearningDispatchesToOk() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-clear-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                                    learning: LearningSettings(enabled: true, memoryDir: dir))
        let handler = makeEngineHandler(service: svc, serviceLock: NSLock())
        XCTAssertEqual(resultTag(handler(1, Data(#"{"method":"ClearLearning"}"#.utf8))), "Ok")
    }

    func testClearLearningFailureIsReturnedAsIpcError() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-clear-error-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let fileSystem = LearningFileSystem(
            list: { _ in throw NSError(domain: "EngineHostHandlerTests", code: 1) },
            remove: { _ in }
        )
        let svc = ConversionService(
            config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: false, memoryDir: dir),
            fileSystem: fileSystem)
        let handler = makeEngineHandler(service: svc, serviceLock: NSLock())

        let outcome = handler(1, Data(#"{"method":"ClearLearning"}"#.utf8))
        XCTAssertEqual(resultTag(outcome), "Error")
    }

    func testBlockedClearDoesNotHoldTheGlobalRequestLockAndConcurrentClearFailsFast() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-clear-concurrent-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let persistenceStarted = DispatchSemaphore(value: 0)
        let releasePersistence = DispatchSemaphore(value: 0)
        let clearStarted = DispatchSemaphore(value: 0)
        let service = ConversionService(
            config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: true, memoryDir: dir),
            autoCommit: .ultrastrong, autoCommitMaxReading: 8,
            fileSystem: .live,
            learningPersistenceForTesting: { _ in
                persistenceStarted.signal()
                releasePersistence.wait()
            },
            learningClearStartedForTesting: { clearStarted.signal() })
        guard let (key, proposal) = firstSnapshotProposal(service) else {
            return XCTFail("representative input must propose an auto commit")
        }
        XCTAssertTrue(service.applySnapshotAutoCommitReceipt(
            connection: 1, key: key, proposal: proposal.proposal))
        XCTAssertEqual(persistenceStarted.wait(timeout: .now() + 2), .success)

        let handler = makeEngineHandler(service: service, serviceLock: NSLock())
        let leaderDone = DispatchSemaphore(value: 0)
        let leaderReply = ReplyBox()
        Thread.detachNewThread {
            leaderReply.data = handler(1, Data(#"{"method":"ClearLearning"}"#.utf8)).reply
            leaderDone.signal()
        }
        XCTAssertEqual(clearStarted.wait(timeout: .now() + 2), .success)

        XCTAssertEqual(resultTag(handler(2, Data(#"{"method":"StartSession"}"#.utf8))), "Session",
                       "ClearLearning must not hold the global request lock while persistence drains")
        XCTAssertEqual(resultTag(handler(3, Data(#"{"method":"ClearLearning"}"#.utf8))), "Error",
                       "concurrent ClearLearning must not consume another fixed pipe worker")
        XCTAssertEqual(leaderDone.wait(timeout: .now() + .milliseconds(20)), .timedOut)
        releasePersistence.signal()
        XCTAssertEqual(leaderDone.wait(timeout: .now() + 2), .success)
        XCTAssertEqual(resultTag((leaderReply.data ?? Data(), false)), "Ok")
    }

    func testSnapshotReceiptAcceptsNonzeroRequestStampAndRejectsWrongIdentity() throws {
        let service = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: false, memoryDir: nil),
            autoCommit: .ultrastrong, autoCommitMaxReading: 8)
        let handler = makeEngineHandler(service: service, serviceLock: NSLock())
        var raw = ""
        for (offset, character) in "watashiha,gakkouheikimasu".enumerated() {
            raw.append(character)
            let revision = offset + 1
            let request = Data(#"{"method":"LiveSnapshot","params":{"composition":700,"revision":\#(revision),"configuration_generation":2,"connection_generation":5,"conversion_revision":3,"request_id":\#(revision + 100),"segments":[{"text":"\#(raw)"}]}}"#.utf8)
            let response = try XCTUnwrap(try JSONSerialization.jsonObject(with: handler(1, request).reply) as? [String: Any])
            guard let proposal = response["auto_commit"] as? [String: Any],
                  let id = proposal["proposal"] as? NSNumber else { continue }
            func receipt(_ generation: Int) -> Data {
                Data(#"{"method":"AutoCommitReceipt","params":{"composition":700,"revision":\#(revision),"configuration_generation":\#(generation),"connection_generation":5,"proposal":\#(id.uint64Value)}}"#.utf8)
            }
            XCTAssertEqual(resultTag(handler(1, receipt(9))), "Error")
            XCTAssertEqual(resultTag(handler(2, receipt(2))), "Error")
            XCTAssertEqual(resultTag(handler(1, receipt(2))), "Ok")
            XCTAssertEqual(resultTag(handler(1, receipt(2))), "Ok")
            return
        }
        XCTFail("representative input must propose an auto commit")
    }

    private func firstSnapshotProposal(_ service: ConversionService)
        -> (ConversionService.SnapshotEnhancementKey, ConversionService.SnapshotAutoCommitProposal)? {
        var raw = ""
        for (offset, character) in "watashiha,gakkouheikimasu".enumerated() {
            raw.append(character)
            let key = ConversionService.SnapshotEnhancementKey(
                composition: 700, revision: UInt64(offset + 1),
                configurationGeneration: 2, connectionGeneration: 5)
            let result = service.snapshot(
                [SnapshotSegment(text: raw, style: nil)], explicit: false,
                enhancementKey: key, snapshotConnection: 1)
            if let proposal = result.autoCommit { return (key, proposal) }
        }
        return nil
    }

    // 訂正昇格: RecordCorrection の decode→dispatch→反映。reading/surface は switch の
    // 位置バインドなので、入れ替えバグはこの end-to-end 観測でしか検出できない
    // （reading をかな・surface を漢字にして、入れ替わると かなフィルタで棄却され lookup が nil になる）。
    func testRecordCorrectionRoutesReadingAndSurfaceCorrectly() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-reccorr-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                                    learning: LearningSettings(enabled: true, memoryDir: dir))
        // 記録可否マップを張る(fail-closed のため、convert/reconvert を経ない記録は棄却される)。
        let s = svc.startSession()
        // ひらがな literal 候補("にほんご"自身)は除外する: reading==surface だと
        // 入れ替えバグでも同じアサートが通り、テストの検出力がハッシュ順次第で消える。
        guard let surface = { () -> String? in
            let modelTop = svc.reconvert(session: s, surface: "にほんご")?.first
            return svc.recordableSurfacesForTesting(reading: "にほんご")
                .sorted().first(where: { $0 != "にほんご" && $0 != modelTop })
        }() else {
            svc.endSession(session: s)
            return XCTFail("no recordable surface")
        }
        svc.endSession(session: s)
        let handler = makeEngineHandler(service: svc, serviceLock: NSLock())
        let body = Data(#"{"method":"RecordCorrection","params":{"reading":"にほんご","surface":"\#(surface)"}}"#.utf8)
        XCTAssertEqual(resultTag(handler(1, body)), "Ok")
        XCTAssertEqual(svc.correctionLookupForTesting(reading: "にほんご"), surface)
    }

    // Spec2: learning_enabled 付き ReloadConfig が decode でき Ok（新 TIP → 新エンジン）。
    // false を送る — true だと reload の resolve(ensureDir) が実 %LOCALAPPDATA% に dir を作る副作用がある。
    func testReloadConfigWithLearningFieldOk() {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        let body = Data(#"{"method":"ReloadConfig","params":{"llm_enabled":false,"llm_api_key":"","llm_endpoint":"","llm_model":"","llm_prompt":"","llm_timeout_ms":15000,"zenzai_enabled":false,"zenzai_weight":"","learning_enabled":false}}"#.utf8)
        XCTAssertEqual(resultTag(handler(1, body)), "Ok")
    }
    // 互換: learning_enabled 無しの旧 TIP からの ReloadConfig も従来どおり Ok
    // （既存 testReloadConfigDispatchesToOk がそのまま担保 — フィールドを Bool? にする理由）。

    // 修正変換(Tab): typo_learn_enabled 付き ReloadConfig が decode でき Ok（新 TIP → 新エンジン）。
    func testReloadConfigWithTypoLearnFieldOk() {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        let body = Data(#"{"method":"ReloadConfig","params":{"llm_enabled":false,"llm_api_key":"","llm_endpoint":"","llm_model":"","llm_prompt":"","llm_timeout_ms":15000,"zenzai_enabled":false,"zenzai_weight":"","typo_learn_enabled":false}}"#.utf8)
        XCTAssertEqual(resultTag(handler(1, body)), "Ok")
    }

    // Zenzai 推論上限付き ReloadConfig が decode され、service の実効値に反映される
    // （新 TIP → 新エンジン。weightURL nil でも ZenzaiConfig.resolve は limit を env から拾う）。
    func testReloadConfigWithInferenceLimitAppliesToService() {
        let svc = makeService()
        let handler = makeEngineHandler(service: svc, serviceLock: NSLock())
        let body = Data(#"{"method":"ReloadConfig","params":{"llm_enabled":false,"llm_api_key":"","llm_endpoint":"","llm_model":"","llm_prompt":"","llm_timeout_ms":15000,"zenzai_enabled":false,"zenzai_weight":"","zenzai_inference_limit":7}}"#.utf8)
        XCTAssertEqual(resultTag(handler(1, body)), "Ok")
        XCTAssertEqual(svc.zenzaiInferenceLimit, 7)
    }
    // 互換: zenzai_inference_limit 無しの旧 TIP からの ReloadConfig も従来どおり Ok
    // （既存 testReloadConfigDispatchesToOk がそのまま担保 — フィールドを UInt32? にする理由）。



    // カスタム辞書: ReloadDictionary ハンドラは converterLock を待たない（spec §4.1 の眼目）。
    // 別スレッドがロックを保持している間でも desired 更新+enqueue だけで即 Ok を返す。
    // 壊れた実装（ハンドラ内 blocking lock）は Ok が返らず timeout で赤になる（ハングはしない）。
    func testReloadDictionaryReturnsOkImmediatelyWhileConverterLockHeld() {
        let svc = makeService()
        let handler = makeEngineHandler(service: svc, serviceLock: NSLock())
        let release = svc.beginConverterLockHoldForTesting()
        defer { release() }
        let reply = ReplyBox()
        let done = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            let out = handler(1, Data(#"{"method":"ReloadDictionary","params":{"enabled":true}}"#.utf8))
            reply.data = out.reply
            done.signal()
        }
        XCTAssertEqual(done.wait(timeout: .now() + 2), .success,
                       "ReloadDictionary ハンドラが converterLock を待っている")
        XCTAssertEqual(resultTag((reply.data ?? Data(), false)), "Ok")
    }

    // 巡3 Z8/D5: ReloadConfig は converterLock が warm-up/変換中で取れないとき busy を
    // Error("reload busy ...") で返す — 無条件 .ok を返す成功詐称（旧実装）への回帰固定。
    // beginConverterLockHoldForTesting で busy 条件を決定的に作る（ReloadDictionary 版と同型）。
    func testReloadConfigBusyReturnsErrorNotOk() {
        let svc = makeService()
        let handler = makeEngineHandler(service: svc, serviceLock: NSLock())
        let release = svc.beginConverterLockHoldForTesting()
        defer { release() }
        let reply = ReplyBox()
        let done = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            let out = handler(1, Data(#"{"method":"ReloadConfig","params":{"llm_enabled":false,"llm_api_key":"","llm_endpoint":"","llm_model":"","llm_prompt":"","llm_timeout_ms":15000,"zenzai_enabled":false,"zenzai_weight":""}}"#.utf8))
            reply.data = out.reply
            done.signal()
        }
        XCTAssertEqual(done.wait(timeout: .now() + 2), .success,
                       "ReloadConfig ハンドラが converterLock を待っている（非ブロックのはず）")
        let data = reply.data ?? Data()
        XCTAssertEqual(resultTag((data, false)), "Error")
        let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        let message = obj?["message"] as? String ?? ""
        XCTAssertTrue(message.hasPrefix("reload busy"),
                      "busy の Error であるべき（actual: \(message)）— .ok に戻すデグレ")
    }

    func testCrossConnectionSessionAccessIsDeniedAsNoSession() {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        // conn 1 がセッションを作る。
        guard let sid = sessionId(handler(1, Data(#"{"method":"StartSession"}"#.utf8))) else {
            return XCTFail("StartSession が session id を返さない")
        }
        let insert = Data(#"{"method":"Insert","params":{"session":\#(sid),"text":"a"}}"#.utf8)
        let end = Data(#"{"method":"EndSession","params":{"session":\#(sid)}}"#.utf8)
        // conn 2 からの操作は未知セッションと同じ Error（"no session"）に正規化される。
        XCTAssertEqual(resultTag(handler(2, insert)), "Error")
        XCTAssertEqual(resultTag(handler(2, end)), "Error")
        // conn 2 の EndSession では壊れておらず、所有者 conn 1 は従来どおり使える。
        XCTAssertEqual(resultTag(handler(1, insert)), "Reading")
        // 所有者自身の EndSession は従来どおり Ok。
        XCTAssertEqual(resultTag(handler(1, end)), "Ok")
        // 終了後は所有者でも no session（未知セッションへの正規化と同型）。
        XCTAssertEqual(resultTag(handler(1, insert)), "Error")
    }

    func testLiveSnapshotRebuildsStyledInputAndEchoesIdentity() throws {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        let request = Data(#"{"method":"LiveSnapshot","params":{"conversion_revision":0,"request_id":1,"composition":8,"revision":13,"configuration_generation":2,"connection_generation":5,"segments":[{"text":"nihongo"},{"text":"GPU","style":"direct"}]}}"#.utf8)
        let outcome = handler(4, request)
        let object = try JSONSerialization.jsonObject(with: outcome.reply) as! [String: Any]
        XCTAssertEqual(object["result"] as? String, "SnapshotResult")
        XCTAssertEqual(object["composition"] as? Int, 8)
        XCTAssertEqual(object["revision"] as? Int, 13)
        XCTAssertEqual(object["configuration_generation"] as? Int, 2)
        XCTAssertEqual(object["connection_generation"] as? Int, 5)
        XCTAssertNotNil(object["baseline"] as? NSNumber)
        XCTAssertEqual(object["reading"] as? String, "にほんごGPU")
        XCTAssertEqual(object["conversion_revision"] as? Int, 0)
        XCTAssertEqual(object["request_id"] as? Int, 1)
        let clauses = try JSONDecoder().decode(SnapshotClauseData.self, from: outcome.reply)
        try ClauseCoordinates.validate(reading: clauses.reading, clauses: clauses.clauses,
            start: 0, end: UInt32(clauses.reading.unicodeScalars.count), text: object["text"] as! String)
        XCTAssertNil(object["candidates"])
    }

    func testSnapshotEnhancementPollIsTerminalWhenGPUIsUnavailable() throws {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        let classic = handler(1, Data(#"{"method":"LiveSnapshot","params":{"conversion_revision":0,"request_id":1,"composition":8,"revision":13,"configuration_generation":2,"connection_generation":5,"segments":[{"text":"nihongo"}]}}"#.utf8))
        let object = try JSONSerialization.jsonObject(with: classic.reply) as! [String: Any]
        let baseline = try XCTUnwrap(object["baseline"] as? NSNumber).uint64Value
        let poll = Data("{\"method\":\"PollSnapshotEnhancement\",\"params\":{\"conversion_revision\":0,\"request_id\":1,\"composition\":8,\"revision\":13,\"configuration_generation\":2,\"connection_generation\":5,\"baseline\":\(baseline)}}".utf8)
        XCTAssertEqual(resultTag(handler(1, poll)), "SnapshotEnhancementUnavailable")
    }

    func testSnapshotEnhancementPollDoesNotWaitForConversionServiceLock() {
        let serviceLock = NSLock()
        let handler = makeEngineHandler(service: makeService(), serviceLock: serviceLock)
        serviceLock.lock()
        defer { serviceLock.unlock() }
        let reply = ReplyBox()
        let done = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            reply.data = handler(1, Data(#"{"method":"PollSnapshotEnhancement","params":{"conversion_revision":0,"request_id":1,"composition":8,"revision":13,"configuration_generation":2,"connection_generation":5,"baseline":42}}"#.utf8)).reply
            done.signal()
        }
        XCTAssertEqual(done.wait(timeout: .now() + 1), .success)
        XCTAssertEqual(resultTag((reply.data ?? Data(), false)), "SnapshotEnhancementUnavailable")
    }

    func testExplicitSnapshotReturnsClassicCandidatesWithTheSameIdentity() throws {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        let request = Data(#"{"method":"LiveSnapshot","params":{"conversion_revision":0,"request_id":1,"composition":8,"revision":13,"configuration_generation":2,"connection_generation":5,"segments":[{"text":"nihongo"}],"explicit":true}}"#.utf8)
        let outcome = handler(4, request)
        let object = try JSONSerialization.jsonObject(with: outcome.reply) as! [String: Any]
        XCTAssertEqual(object["result"] as? String, "SnapshotResult")
        XCTAssertEqual(object["composition"] as? Int, 8)
        XCTAssertEqual(object["revision"] as? Int, 13)
        let candidates = object["candidates"] as? [String]
        XCTAssertFalse(candidates?.isEmpty ?? true)
        let remaining = object["candidate_remaining"] as? [String]
        XCTAssertEqual(remaining?.count, candidates?.count)
    }

    func testSnapshotReconstructionMatchesPinnedRepresentativeReadings() {
        for (roman, expected) in [
            ("nihongo", "にほんご"),
            ("gakkou", "がっこう"),
            ("xya", "ゃ"),
            ("nn", "ん"),
        ] {
            let composing = ConversionService.makeSnapshotComposing([
                SnapshotSegment(text: roman, style: nil)
            ])
            XCTAssertEqual(composing.convertTarget, expected, roman)
        }
        let styled = ConversionService.makeSnapshotComposing([
            SnapshotSegment(text: "kyou", style: nil),
            SnapshotSegment(text: "GPU", style: "direct")
        ])
        XCTAssertEqual(styled.convertTarget, "きょうGPU")
    }
}
