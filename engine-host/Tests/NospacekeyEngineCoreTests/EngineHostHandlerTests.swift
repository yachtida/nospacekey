import XCTest
import Foundation
@testable import NospacekeyEngineCore

final class EngineHostHandlerTests: XCTestCase {
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

    // version handshake: 新エンジンは StartSession 応答に proto=PROTO_VERSION を載せる。
    func testStartSessionCarriesProtoVersion() throws {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        let obj = try JSONSerialization.jsonObject(
            with: handler(1, Data(#"{"method":"StartSession"}"#.utf8)).reply) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "Session")
        XCTAssertEqual(obj["proto"] as? Int, 1)
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

    // 修正変換(Tab): TypoConvert は decode→dispatch→Candidates のスモーク（Insert 後）。
    func testTypoConvertDispatchesToCandidates() throws {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        guard let sid = sessionId(handler(1, Data(#"{"method":"StartSession"}"#.utf8))) else {
            return XCTFail("StartSession が session id を返さない")
        }
        _ = handler(1, Data(#"{"method":"Insert","params":{"session":\#(sid),"text":"nihongo"}}"#.utf8))
        let body = Data(#"{"method":"TypoConvert","params":{"session":\#(sid)}}"#.utf8)
        XCTAssertEqual(resultTag(handler(1, body)), "Candidates")
    }

    // 未知セッションへの TypoConvert は Error("no session") へ正規化される。
    func testTypoConvertUnknownSessionYieldsError() {
        let handler = makeEngineHandler(service: makeService(), serviceLock: NSLock())
        let body = Data(#"{"method":"TypoConvert","params":{"session":99999}}"#.utf8)
        XCTAssertEqual(resultTag(handler(1, body)), "Error")
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
}
