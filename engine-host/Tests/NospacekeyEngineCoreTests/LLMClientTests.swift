import XCTest
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif
@testable import NospacekeyEngineCore

final class LLMClientTests: XCTestCase {
    private func cfg(echo: Bool = false) -> LLMConfig {
        LLMConfig(apiKey: "k", endpoint: "https://x/v1", model: "m",
                  prompt: "P", timeoutMs: 1000, echo: echo)
    }

    func testParsesContentAndStripsQuotes() {
        let body = #"{"choices":[{"message":{"content":"\"この変換でおこなってください\"\n"}}]}"#.data(using: .utf8)!
        let client = LLMClient(config: cfg(), send: { _ in .success(body) })
        guard case let .success(text) = client.convert(reading: "このへんかんでおこなってください") else {
            return XCTFail("expected success")
        }
        XCTAssertEqual(text, "この変換でおこなってください")
    }

    func testSendErrorPropagates() {
        let client = LLMClient(config: cfg(), send: { _ in .failure(LLMError(message: "boom")) })
        guard case let .failure(e) = client.convert(reading: "あ") else { return XCTFail("expected failure") }
        XCTAssertEqual(e.message, "boom")
    }

    func testEmptyApiKeyIsDisabledAndDoesNotSend() {
        // 空文字キー（present だが empty）＋有効 endpoint。enabled ゲートで HTTP を打たないこと。
        let emptyKeyCfg = LLMConfig(apiKey: "", endpoint: "https://x/v1", model: "m",
                                    prompt: "P", timeoutMs: 1000, echo: false)
        var sendInvoked = false
        let client = LLMClient(config: emptyKeyCfg, send: { _ in
            sendInvoked = true
            return .failure(LLMError(message: "should not be called"))
        })
        guard case let .failure(e) = client.convert(reading: "あ") else {
            return XCTFail("expected failure for empty apiKey")
        }
        XCTAssertEqual(e.message, "llm disabled")
        XCTAssertFalse(sendInvoked, "send must not be invoked when llm is disabled")
    }

    func testEmptyEndpointIsDisabledAndDoesNotSend() {
        // 空文字 endpoint（present だが empty）＋有効 key。同様にゲートされること。
        let emptyEndpointCfg = LLMConfig(apiKey: "k", endpoint: "", model: "m",
                                         prompt: "P", timeoutMs: 1000, echo: false)
        var sendInvoked = false
        let client = LLMClient(config: emptyEndpointCfg, send: { _ in
            sendInvoked = true
            return .failure(LLMError(message: "should not be called"))
        })
        guard case let .failure(e) = client.convert(reading: "あ") else {
            return XCTFail("expected failure for empty endpoint")
        }
        XCTAssertEqual(e.message, "llm disabled")
        XCTAssertFalse(sendInvoked, "send must not be invoked when llm is disabled")
    }

    func testEmptyChoicesIsFailure() {
        let client = LLMClient(config: cfg(), send: { _ in .success(#"{"choices":[]}"#.data(using: .utf8)!) })
        guard case .failure = client.convert(reading: "あ") else { return XCTFail("expected failure") }
    }

    func testRequestCarriesAuthAndModelAndReading() {
        var captured: URLRequest?
        let client = LLMClient(config: cfg(), send: { req in
            captured = req
            return .success(#"{"choices":[{"message":{"content":"ok"}}]}"#.data(using: .utf8)!)
        })
        _ = client.convert(reading: "にほんご")
        let req = try! XCTUnwrap(captured)
        XCTAssertEqual(req.value(forHTTPHeaderField: "Authorization"), "Bearer k")
        XCTAssertEqual(req.url?.absoluteString, "https://x/v1/chat/completions")
        let json = try! JSONSerialization.jsonObject(with: req.httpBody ?? Data()) as! [String: Any]
        XCTAssertEqual(json["model"] as? String, "m")
        let messages = json["messages"] as! [[String: String]]
        XCTAssertEqual(messages.first?["role"], "system")
        XCTAssertEqual(messages.last?["content"], "にほんご")
    }

    func testStripQuotesKeepsApostrophes() {
        // ASCII の ' は剥がさない（短縮形先頭の誤食防止）。
        XCTAssertEqual(LLMClient.stripQuotes("'hello'"), "'hello'")
        XCTAssertEqual(LLMClient.stripQuotes("'tis the season"), "'tis the season")
    }

    func testStripQuotesKeepsStructuralQuotes() {
        // 内部に同じ区切りを含む引用は構造的とみなし剥がさない。
        XCTAssertEqual(LLMClient.stripQuotes("「A」「B」"), "「A」「B」")
        XCTAssertEqual(LLMClient.stripQuotes("\"a\"b\"c\""), "\"a\"b\"c\"")
    }

    func testStripQuotesStripsSimpleWrap() {
        // 全体を1組で囲んだだけ（内部に区切り無し）は従来どおり剥がす。
        XCTAssertEqual(LLMClient.stripQuotes("「日本語」"), "日本語")
        XCTAssertEqual(LLMClient.stripQuotes("\"日本語\""), "日本語")
    }

    // U9: leftContext なしは従来のメッセージ配列とバイト等価。ありは system にのみ参考文脈を追記する。
    func testBuildMessagesAppendsContextToSystemOnly() {
        let withoutCtx = LLMClient.buildMessages(prompt: "P", reading: "R", leftContext: nil)
        XCTAssertEqual(withoutCtx as NSArray, [
            ["role": "system", "content": "P"],
            ["role": "user", "content": "R"],
        ] as NSArray)

        let withCtx = LLMClient.buildMessages(prompt: "P", reading: "R", leftContext: "C")
        XCTAssertEqual(withCtx.count, 2)
        XCTAssertEqual(withCtx[0]["role"], "system")
        XCTAssertEqual(withCtx[0]["content"], "P\n直前の文脈(参考): C")
        XCTAssertEqual(withCtx[1]["role"], "user")
        XCTAssertEqual(withCtx[1]["content"], "R")
    }
}

extension LLMClientTests {
    /// env に実キー/エンドポイントがある時だけ実 POST する feasibility テスト。
    /// 実行: NOSPACEKEY_LLM_API_KEY=... NOSPACEKEY_LLM_ENDPOINT=... swift test --filter testRealPostIfConfigured
    func testRealPostIfConfigured() throws {
        let cfg = LLMConfig.resolve(environment: ProcessInfo.processInfo.environment)
        try XCTSkipUnless(cfg.enabled, "set NOSPACEKEY_LLM_API_KEY/ENDPOINT to run")
        let client = LLMClient(config: cfg) // 既定 urlSessionSend = 実HTTP
        switch client.convert(reading: "このへんかんでおこなってくだっさい") {
        case .success(let text):
            print("ev=s0a_real_post ok text=\(text)")
            XCTAssertFalse(text.isEmpty)
        case .failure(let e):
            XCTFail("real POST failed (URLSession on Windows?): \(e.message)")
        }
    }
}

extension LLMClientTests {
    func testServiceLlmConvertUsesInjectedClient() {
        let mock = LLMClient(config: cfg(), send: { _ in
            .success(#"{"choices":[{"message":{"content":"日本語"}}]}"#.data(using: .utf8)!)
        })
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1), llmClient: mock)
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "nihongo")
        guard case let .success(text) = svc.llmConvert(session: sid) else { return XCTFail("expected success") }
        XCTAssertEqual(text, "日本語")
    }

    func testServiceLlmConvertEchoMode() {
        let echoCfg = LLMConfig(apiKey: nil, endpoint: nil, model: "m", prompt: "P", timeoutMs: 1000, echo: true)
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                                    llmClient: LLMClient(config: echoCfg))
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "a")
        guard case let .success(text) = svc.llmConvert(session: sid) else { return XCTFail("expected echo success") }
        XCTAssertTrue(text.hasPrefix("LLM:"))
    }

    func testServiceLlmConvertDisabledIsFailure() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1)) // 既定=未設定
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "a")
        guard case .failure = svc.llmConvert(session: sid) else { return XCTFail("expected failure") }
    }
}
