import XCTest
@testable import NospacekeyEngineCore

final class ProtocolTests: XCTestCase {
    func testDecodeLiveConvert() throws {
        // auto_commit 無し（旧TIP / auto_commit=false のとき Rust はキーを省略）→ false にデコード。
        let json = #"{"method":"LiveConvert","params":{"session":7,"seq":42}}"#.data(using: .utf8)!
        let req = try JSONDecoder().decode(Request.self, from: json)
        guard case let .liveConvert(session, seq, _, autoCommit) = req else { return XCTFail("not liveConvert: \(req)") }
        XCTAssertEqual(session, 7)
        XCTAssertEqual(seq, 42)
        XCTAssertFalse(autoCommit)
    }

    func testDecodeLiveConvertWithAutoCommit() throws {
        let json = #"{"method":"LiveConvert","params":{"session":7,"seq":42,"auto_commit":true}}"#.data(using: .utf8)!
        let req = try JSONDecoder().decode(Request.self, from: json)
        guard case let .liveConvert(_, _, _, autoCommit) = req else { return XCTFail("not liveConvert: \(req)") }
        XCTAssertTrue(autoCommit)
    }

    // U9: Convert の left_context デコード（あり／なしの両方が成功すること＝旧TIP互換）。
    func testConvertParamsDecodeLeftContext() throws {
        let withCtx = #"{"method":"Convert","params":{"session":7,"left_context":"私の"}}"#.data(using: .utf8)!
        let reqWith = try JSONDecoder().decode(Request.self, from: withCtx)
        guard case let .convert(session, leftContext) = reqWith else { return XCTFail("not convert: \(reqWith)") }
        XCTAssertEqual(session, 7)
        XCTAssertEqual(leftContext, "私の")

        let withoutCtx = #"{"method":"Convert","params":{"session":7}}"#.data(using: .utf8)!
        let reqWithout = try JSONDecoder().decode(Request.self, from: withoutCtx)
        guard case let .convert(session2, leftContext2) = reqWithout else { return XCTFail("not convert: \(reqWithout)") }
        XCTAssertEqual(session2, 7)
        XCTAssertNil(leftContext2)
    }

    // 修正変換(Tab): TypoConvert の left_context デコード（あり／なしの両方が成功すること）。
    // wire 形は Convert と同型（ConvertParams を共有する実装）。
    func testTypoConvertParamsDecodeLeftContext() throws {
        let withCtx = #"{"method":"TypoConvert","params":{"session":7,"left_context":"私の"}}"#.data(using: .utf8)!
        let reqWith = try JSONDecoder().decode(Request.self, from: withCtx)
        guard case let .typoConvert(session, leftContext) = reqWith else { return XCTFail("not typoConvert: \(reqWith)") }
        XCTAssertEqual(session, 7)
        XCTAssertEqual(leftContext, "私の")

        let withoutCtx = #"{"method":"TypoConvert","params":{"session":7}}"#.data(using: .utf8)!
        let reqWithout = try JSONDecoder().decode(Request.self, from: withoutCtx)
        guard case let .typoConvert(session2, leftContext2) = reqWithout else { return XCTFail("not typoConvert: \(reqWithout)") }
        XCTAssertEqual(session2, 7)
        XCTAssertNil(leftContext2)
        XCTAssertEqual(reqWithout.sessionId, 7)
    }

    func testEncodeLiveResult() throws {
        let res = Response.liveResult(seq: 42, text: "日本語", reading: "にほんご", committed: nil)
        let data = try JSONEncoder().encode(res)
        let obj = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "LiveResult")
        XCTAssertEqual(obj["seq"] as? Int, 42)
        XCTAssertEqual(obj["text"] as? String, "日本語")
        XCTAssertEqual(obj["reading"] as? String, "にほんご")
        // committed=nil はキー自体を省略（自動確定導入前と wire 形一致＝旧TIP互換。Rust protocol.rs と対）。
        XCTAssertNil(obj["committed"])
    }

    func testEncodeLiveResultWithCommitted() throws {
        let res = Response.liveResult(seq: 42, text: "入力", reading: "にゅうりょく", committed: "日本語")
        let obj = try JSONSerialization.jsonObject(with: JSONEncoder().encode(res)) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "LiveResult")
        XCTAssertEqual(obj["text"] as? String, "入力")
        XCTAssertEqual(obj["reading"] as? String, "にゅうりょく")
        XCTAssertEqual(obj["committed"] as? String, "日本語")
    }

    func testDecodeCommit() throws {
        let json = #"{"method":"Commit","params":{"session":7,"index":0}}"#.data(using: .utf8)!
        let req = try JSONDecoder().decode(Request.self, from: json)
        guard case let .commit(session, index) = req else { return XCTFail("not commit: \(req)") }
        XCTAssertEqual(session, 7)
        XCTAssertEqual(index, 0)
    }

    func testEncodeCommitted() throws {
        let res = Response.committed(text: "日本", reading: "ご")
        let obj = try JSONSerialization.jsonObject(with: JSONEncoder().encode(res)) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "Committed")
        XCTAssertEqual(obj["text"] as? String, "日本")
        XCTAssertEqual(obj["reading"] as? String, "ご")
    }

    func testDecodeLlmConvert() throws {
        let json = #"{"method":"LlmConvert","params":{"session":3,"seq":9}}"#.data(using: .utf8)!
        let req = try JSONDecoder().decode(Request.self, from: json)
        guard case let .llmConvert(session, seq, _) = req else { return XCTFail("not llmConvert: \(req)") }
        XCTAssertEqual(session, 3); XCTAssertEqual(seq, 9)
    }

    // UU-5: ReloadConfig の decode（Rust `Request::ReloadConfig` の wire 形と一致）。
    func testDecodeReloadConfig() throws {
        let json = #"""
        {"method":"ReloadConfig","params":{"llm_enabled":true,"llm_api_key":"sk-x","llm_endpoint":"https://e","llm_model":"gpt-4o-mini","llm_prompt":"p","llm_timeout_ms":15000,"zenzai_enabled":true,"zenzai_weight":"C:/w.gguf"}}
        """#.data(using: .utf8)!
        let req = try JSONDecoder().decode(Request.self, from: json)
        guard case let .reloadConfig(p) = req else { return XCTFail("not reloadConfig: \(req)") }
        XCTAssertTrue(p.llm_enabled)
        XCTAssertEqual(p.llm_api_key, "sk-x")
        XCTAssertEqual(p.llm_endpoint, "https://e")
        XCTAssertEqual(p.llm_model, "gpt-4o-mini")
        XCTAssertEqual(p.llm_timeout_ms, 15000)
        XCTAssertTrue(p.zenzai_enabled)
        XCTAssertEqual(p.zenzai_weight, "C:/w.gguf")
        // session を伴わない（所有権ガード対象外）。
        XCTAssertNil(req.sessionId)
        // 修正変換(Tab): typo_learn_enabled キー無し（旧TIP）は nil にデコードされる。
        XCTAssertNil(p.typo_learn_enabled)
    }

    // 修正変換(Tab): typo_learn_enabled キー有りは値どおりにデコードされる（新TIP → 新エンジン）。
    func testDecodeReloadConfigWithTypoLearnEnabled() throws {
        let json = #"""
        {"method":"ReloadConfig","params":{"llm_enabled":false,"llm_api_key":"","llm_endpoint":"","llm_model":"","llm_prompt":"","llm_timeout_ms":15000,"zenzai_enabled":false,"zenzai_weight":"","typo_learn_enabled":false}}
        """#.data(using: .utf8)!
        let req = try JSONDecoder().decode(Request.self, from: json)
        guard case let .reloadConfig(p) = req else { return XCTFail("not reloadConfig: \(req)") }
        XCTAssertEqual(p.typo_learn_enabled, false)
    }

    func testEncodeLlmResult() throws {
        let res = Response.llmResult(seq: 9, text: "この変換でおこなってください")
        let obj = try JSONSerialization.jsonObject(with: JSONEncoder().encode(res)) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "LlmResult")
        XCTAssertEqual(obj["seq"] as? Int, 9)
        XCTAssertEqual(obj["text"] as? String, "この変換でおこなってください")
    }

    /// M-1: encodeResponse は **決して空 Data を返さない**（空フレームは Rust 側で接続が落ちる）。
    /// 全 Response ケースが非空にエンコードされることを確認する。
    func testEncodeResponseNeverEmpty() {
        let cases: [Response] = [
            .pong,
            .session(7, proto: nil),
            .reading(""),                                   // 空読みでもフレーム本体は非空
            .candidates([]),                                // 空候補でもフレーム本体は非空
            .ok,
            .error("no session"),
            .liveResult(seq: 1, text: "", reading: "", committed: nil),
            .llmResult(seq: 2, text: ""),
            .committed(text: "", reading: ""),              // 全消費（残り読み空）でもフレーム本体は非空
        ]
        for c in cases {
            XCTAssertFalse(encodeResponse(c).isEmpty, "encodeResponse must never be empty for \(c)")
        }
    }

    /// M-1: 最後の手段のリテラルは非空で、JSON は dict として decode でき "result" == "Error"。
    /// このリテラルが Rust の Response::Error の wire 形と一致することを保証する。
    func testLastResortLiteralDecodesAsError() throws {
        let data = Data(#"{"result":"Error","message":"encode failed"}"#.utf8)
        XCTAssertFalse(data.isEmpty)
        let obj = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "Error")
    }

    func testSessionIdExtractsSessionBearingOps() throws {
        func decode(_ json: String) throws -> Request {
            try JSONDecoder().decode(Request.self, from: Data(json.utf8))
        }
        // session を伴わない op は nil。
        XCTAssertNil(try decode(#"{"method":"Ping"}"#).sessionId)
        XCTAssertNil(try decode(#"{"method":"StartSession"}"#).sessionId)
        // session を伴う全 op から id が取れる（wire 形は Rust protocol.rs のテストと同一）。
        XCTAssertEqual(try decode(#"{"method":"Insert","params":{"session":7,"text":"a"}}"#).sessionId, 7)
        XCTAssertEqual(try decode(#"{"method":"Backspace","params":{"session":7}}"#).sessionId, 7)
        XCTAssertEqual(try decode(#"{"method":"Convert","params":{"session":7}}"#).sessionId, 7)
        XCTAssertEqual(try decode(#"{"method":"TypoConvert","params":{"session":7}}"#).sessionId, 7)
        XCTAssertEqual(try decode(#"{"method":"Reconvert","params":{"session":7,"surface":"にほんご"}}"#).sessionId, 7)
        XCTAssertEqual(try decode(#"{"method":"Commit","params":{"session":7,"index":0}}"#).sessionId, 7)
        XCTAssertEqual(try decode(#"{"method":"EndSession","params":{"session":7}}"#).sessionId, 7)
        XCTAssertEqual(try decode(#"{"method":"LiveConvert","params":{"session":7,"seq":1}}"#).sessionId, 7)
        XCTAssertEqual(try decode(#"{"method":"LlmConvert","params":{"session":7,"seq":1}}"#).sessionId, 7)
    }

    // ---- Shift英語モード: Insert style（Rust protocol.rs のテストと wire 形一致）----

    func testInsertParamsDecodeStyle() throws {
        func decode(_ json: String) throws -> Request {
            try JSONDecoder().decode(Request.self, from: Data(json.utf8))
        }
        // style 無し(旧TIP)は nil = roman2kana 既定(後方互換)。
        if case .insert(let s, let t, let style) = try decode(#"{"method":"Insert","params":{"session":7,"text":"a"}}"#) {
            XCTAssertEqual(s, 7); XCTAssertEqual(t, "a"); XCTAssertNil(style)
        } else { XCTFail("not insert") }
        if case .insert(_, _, let style) = try decode(#"{"method":"Insert","params":{"session":7,"text":"A","style":"direct"}}"#) {
            XCTAssertEqual(style, "direct")
        } else { XCTFail("not insert") }
    }

    // ---- version handshake / Shutdown（Rust protocol.rs のテストと wire 形一致）----

    func testDecodeShutdown() throws {
        // 引数なし op（Ping/ClearLearning と同型）。session を伴わない（所有権ガード対象外）。
        let req = try JSONDecoder().decode(Request.self, from: Data(#"{"method":"Shutdown"}"#.utf8))
        guard case .shutdown = req else { return XCTFail("not shutdown: \(req)") }
        XCTAssertNil(req.sessionId)
    }

    func testEncodeSessionCarriesProto() throws {
        // 新エンジン: Session 応答に proto を載せる。dict 比較（キー順非保証のためバイト一致比較はしない）。
        let res = Response.session(7, proto: 1)
        let obj = try JSONSerialization.jsonObject(with: JSONEncoder().encode(res)) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "Session")
        XCTAssertEqual(obj["session"] as? Int, 7)
        XCTAssertEqual(obj["proto"] as? Int, 1)
    }

    func testEncodeSessionWithoutProtoOmitsKey() throws {
        // proto=nil はキー自体を省略＝handshake 導入前と wire 形一致（旧TIP互換。Rust 側 Option と対）。
        let res = Response.session(7, proto: nil)
        let obj = try JSONSerialization.jsonObject(with: JSONEncoder().encode(res)) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "Session")
        XCTAssertEqual(obj["session"] as? Int, 7)
        XCTAssertNil(obj["proto"])
    }
}
