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
            .session(7, proto: nil, boot: nil, engineEpoch: "fixture-engine", learningGeneration: 0, capabilities: nil),
            .session(7, proto: nil, boot: nil, engineEpoch: "fixture-engine", learningGeneration: 0, capabilities: ["mixed_input_v1"]),
            .mixedResult(composition: 8, revision: 13, configurationGeneration: 2,
                         connectionGeneration: 5, requestID: 4, sourceRevision: 9, planID: 3,
                         engineEpoch: "fixture-engine", learningGeneration: 0,
                         text: "今日はPython", spans: []),
            .reading(""),                                   // 空読みでもフレーム本体は非空
            .candidates([]),                                // 空候補でもフレーム本体は非空
            .ok,
            .error("no session"),
            .liveResult(seq: 1, text: "", reading: "", committed: nil),
            .llmResult(seq: 2, text: ""),
            .committed(text: "", reading: ""),              // 全消費（残り読み空）でもフレーム本体は非空
            .zenzaiStatus(state: "classic", backend: nil, device: nil, reason: nil,
                          liveLatency: nil, convertLatency: nil),
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
        XCTAssertEqual(try decode(#"{"method":"Reconvert","params":{"session":7,"surface":"にほんご"}}"#).sessionId, 7)
        XCTAssertEqual(try decode(#"{"method":"Commit","params":{"session":7,"index":0}}"#).sessionId, 7)
        XCTAssertEqual(try decode(#"{"method":"EndSession","params":{"session":7}}"#).sessionId, 7)
        XCTAssertEqual(try decode(#"{"method":"LiveConvert","params":{"session":7,"seq":1}}"#).sessionId, 7)
        XCTAssertEqual(try decode(#"{"method":"LlmConvert","params":{"session":7,"seq":1}}"#).sessionId, 7)
    }



    func testSnapshotAutoCommitBumpsProtocolGeneration() {
        XCTAssertEqual(ProtocolVersion.current, 11)
    }

    func testSnapshotAutoCommitProposalAndReceiptWireContract() throws {
        let response = Response.snapshotResult(
            composition: 8, revision: 13, configurationGeneration: 2,
            connectionGeneration: 5, text: "語", candidates: nil,
            candidateRemaining: nil, baseline: 41,
            autoCommit: AutoCommitProposal(
                proposal: 17, text: "日本", consumedReading: "にほん", remaining: "ご"),
            clauseData: SnapshotClauseData(reading: "ご", conversion_revision: 0, request_id: 1,
                clauses: [WireClause(id: 1, reading_start: 0, reading_end: 1, state: .converted, surface: "語", candidate_token: "fixture-candidate")], sentence_token: nil))
        let object = try JSONSerialization.jsonObject(with: JSONEncoder().encode(response)) as! [String: Any]
        let proposal = object["auto_commit"] as! [String: Any]
        XCTAssertEqual(proposal["proposal"] as? Int, 17)
        XCTAssertEqual(proposal["consumed_reading"] as? String, "にほん")
        XCTAssertEqual(proposal["remaining"] as? String, "ご")

        let json = #"{"method":"AutoCommitReceipt","params":{"composition":8,"revision":13,"configuration_generation":2,"connection_generation":5,"proposal":17}}"#
        let request = try JSONDecoder().decode(Request.self, from: Data(json.utf8))
        guard case .autoCommitReceipt(let composition, let revision, _, _, let id) = request else {
            return XCTFail("not receipt")
        }
        XCTAssertEqual(composition, 8)
        XCTAssertEqual(revision, 13)
        XCTAssertEqual(id, 17)
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

    func testDecodePrepareMaintenance() throws {
        let req = try JSONDecoder().decode(
            Request.self, from: Data(#"{"method":"PrepareMaintenance"}"#.utf8))
        guard case .prepareMaintenance = req else {
            return XCTFail("not prepareMaintenance: \(req)")
        }
        XCTAssertNil(req.sessionId)
    }

    func testDecodeZenzaiStatusOperationsHaveNoSession() throws {
        let query = try JSONDecoder().decode(
            Request.self, from: Data(#"{"method":"QueryZenzaiStatus"}"#.utf8))
        let retry = try JSONDecoder().decode(
            Request.self, from: Data(#"{"method":"RetryZenzai"}"#.utf8))
        guard case .queryZenzaiStatus = query else { return XCTFail("not QueryZenzaiStatus") }
        guard case .retryZenzai = retry else { return XCTFail("not RetryZenzai") }
        XCTAssertNil(query.sessionId)
        XCTAssertNil(retry.sessionId)
    }

    func testEncodeZenzaiStatusOmitsSensitiveAndOptionalFields() throws {
        let response = Response.zenzaiStatus(
            state: "classic", backend: nil, device: nil, reason: "backend_unavailable",
            liveLatency: nil, convertLatency: nil)
        let object = try JSONSerialization.jsonObject(
            with: JSONEncoder().encode(response)) as! [String: Any]
        XCTAssertEqual(object["result"] as? String, "ZenzaiStatus")
        XCTAssertEqual(object["state"] as? String, "classic")
        XCTAssertEqual(object["reason"] as? String, "backend_unavailable")
        for key in ["path", "input", "candidates", "generation", "model_load_attempts",
                    "context_init_attempts", "decode_attempts"] {
            XCTAssertNil(object[key], "runtime status must not expose \(key)")
        }
    }

    /// 集約速度統計は載せるが、nil なら wire から丸ごと省略する（旧エンジン互換）。
    /// Rust 側 `zenzai_status_response_encodes_latency_tiers` と一字一致で対にする。
    func testEncodeZenzaiStatusIncludesLatencyTiersWhenPresent() throws {
        let tier = ZenzaiLatencyTierStats(
            sampleCount: 100, p50Ms: 51.7, p95Ms: 62.6, maxMs: 72.2, timeoutCount: 2)
        let response = Response.zenzaiStatus(
            state: "gpu_active", backend: "Vulkan", device: "AMD Radeon(TM) 890M Graphics",
            reason: nil, liveLatency: tier, convertLatency: nil)
        let object = try JSONSerialization.jsonObject(
            with: JSONEncoder().encode(response)) as! [String: Any]
        let live = object["latency_live"] as? [String: Any]
        XCTAssertNotNil(live, "latency_live must be present when supplied")
        XCTAssertEqual(live?["count"] as? Int, 100)
        XCTAssertEqual(live?["p50_ms"] as? Double, 51.7)
        XCTAssertEqual(live?["p95_ms"] as? Double, 62.6)
        XCTAssertEqual(live?["max_ms"] as? Double, 72.2)
        XCTAssertEqual(live?["timeout_count"] as? Int, 2)
        XCTAssertNil(object["latency_convert"], "absent tier must be omitted entirely")
    }

    func testDecodeRecordCorrection() throws {
        // Rust 側 protocol.rs の serialize 出力と一字一句一致(record_correction_request_roundtrips と対)。
        // session を伴わない(確定済み訂正はどのセッションにも属さない — ClearLearning と同じ共有資源扱い)。
        let json = #"{"method":"RecordCorrection","params":{"reading":"みこみっと","surface":"未コミット"}}"#
        let req = try JSONDecoder().decode(Request.self, from: Data(json.utf8))
        guard case .recordCorrection(let r, let s) = req else { return XCTFail("not recordCorrection: \(req)") }
        XCTAssertEqual(r, "みこみっと")
        XCTAssertEqual(s, "未コミット")
        XCTAssertNil(req.sessionId)
    }

    func testEncodeSessionCarriesProto() throws {
        // 新エンジン: Session 応答に proto を載せる。dict 比較（キー順非保証のためバイト一致比較はしない）。
        let res = Response.session(7, proto: 9, boot: BuildInfo.version, engineEpoch: "fixture-engine", learningGeneration: 6, capabilities: nil)
        let obj = try JSONSerialization.jsonObject(with: JSONEncoder().encode(res)) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "Session")
        XCTAssertEqual(obj["session"] as? Int, 7)
        XCTAssertEqual(obj["proto"] as? Int, 9)
        XCTAssertEqual(obj["boot"] as? String, BuildInfo.version)
        XCTAssertNil(obj["capabilities"], "nil はキー省略＝capability 導入前と wire 形一致")
    }

    func testEncodeSessionCapabilities() throws {
        // capability 広告。Rust `Response::Session.capabilities` と対（一字一句一致規約）。
        let res = Response.session(7, proto: 9, boot: nil, engineEpoch: "fixture-engine", learningGeneration: 6, capabilities: ["mixed_input_v1"])
        let obj = try JSONSerialization.jsonObject(with: JSONEncoder().encode(res)) as! [String: Any]
        XCTAssertEqual(obj["capabilities"] as? [String], ["mixed_input_v1"])
    }

    func testDecodeMixedConvert() throws {
        // Rust `Request::MixedConvert` の wire 形と対。left_context 省略形（None）も受ける。
        let json = #"{"method":"MixedConvert","params":{"session":7,"composition":8,"revision":13,"configuration_generation":2,"connection_generation":5,"conversion_revision":1,"request_id":4,"source_revision":9,"plan_id":3,"spans":[{"kind":"japanese","reading_start":0,"reading_end":4,"text":"きょうは"},{"kind":"literal","reading_start":4,"reading_end":10,"text":"Python"}]}}"#
        let req = try JSONDecoder().decode(Request.self, from: Data(json.utf8))
        guard case .mixedConvert(let session, let composition, let revision,
                                 let configurationGeneration, let connectionGeneration,
                                 let conversionRevision, let requestID, let sourceRevision,
                                 let planID, let spans, let leftContext) = req
        else { return XCTFail("not mixedConvert: \(req)") }
        XCTAssertEqual(session, 7)
        XCTAssertEqual(composition, 8)
        XCTAssertEqual(revision, 13)
        XCTAssertEqual(configurationGeneration, 2)
        XCTAssertEqual(connectionGeneration, 5)
        XCTAssertEqual(conversionRevision, 1)
        XCTAssertEqual(requestID, 4)
        XCTAssertEqual(sourceRevision, 9)
        XCTAssertEqual(planID, 3)
        XCTAssertEqual(spans, [
            MixedSpan(kind: "japanese", readingStart: 0, readingEnd: 4, text: "きょうは"),
            MixedSpan(kind: "literal", readingStart: 4, readingEnd: 10, text: "Python"),
        ])
        XCTAssertNil(leftContext)
        XCTAssertEqual(req.sessionId, 7, "所有権ガードの対象")
    }

    func testEncodeMixedResult() throws {
        // Rust `Response::MixedResult` の wire 形と対。同一性キーは要求のエコー。
        let res = Response.mixedResult(composition: 8, revision: 13, configurationGeneration: 2,
                                       connectionGeneration: 5, requestID: 4, sourceRevision: 9,
                                       planID: 3, engineEpoch: "11111111-1111-4111-8111-111111111111",
                                       learningGeneration: 6,
                                       text: "今日はPython",
                                       spans: [MixedSpanResult(kind: "japanese", readingStart: 0,
                                                               readingEnd: 4, text: "今日は",
                                                               candidateToken: "engine-epoch:7"),
                                               MixedSpanResult(kind: "literal", readingStart: 4,
                                                               readingEnd: 10, text: "Python")])
        let obj = try JSONSerialization.jsonObject(with: JSONEncoder().encode(res)) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "MixedResult")
        XCTAssertEqual(obj["source_revision"] as? Int, 9)
        XCTAssertEqual(obj["plan_id"] as? Int, 3)
        XCTAssertEqual(obj["engine_epoch"] as? String, "11111111-1111-4111-8111-111111111111")
        XCTAssertEqual(obj["learning_generation"] as? Int, 6)
        XCTAssertEqual(obj["text"] as? String, "今日はPython")
        let spans = obj["spans"] as! [[String: Any]]
        XCTAssertEqual(spans[0]["kind"] as? String, "japanese")
        XCTAssertEqual(spans[0]["reading_start"] as? Int, 0)
        XCTAssertEqual(spans[0]["text"] as? String, "今日は")
        XCTAssertEqual(spans[0]["candidate_token"] as? String, "engine-epoch:7")
        XCTAssertNil(spans[1]["candidate_token"], "Literal（token なし）はキーを省略する")
    }

    func testEncodeSessionWithoutProtoOmitsKey() throws {
        // proto=nil はキー自体を省略＝handshake 導入前と wire 形一致（旧TIP互換。Rust 側 Option と対）。
        let res = Response.session(7, proto: nil, boot: nil, engineEpoch: "fixture-engine", learningGeneration: 0, capabilities: nil)
        let obj = try JSONSerialization.jsonObject(with: JSONEncoder().encode(res)) as! [String: Any]
        XCTAssertEqual(obj["result"] as? String, "Session")
        XCTAssertEqual(obj["session"] as? Int, 7)
        XCTAssertNil(obj["proto"])
        XCTAssertNil(obj["boot"])
    }
}
