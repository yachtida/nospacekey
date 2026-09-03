import Foundation

/// IPC プロトコルの互換世代。Rust `ipc::protocol::PROTO_VERSION` とミラー（一字一句一致規約）。
/// **wire 互換でも、その版が依存する op を追加したら両側同時に bump する** — 「互換が壊れた時だけ bump」だと
/// 新 op が再起動まで無言で decline / no-op になる。読み手が依存しない optional フィールドの追加
/// （encodeIfPresent で旧形とバイト一致）では bump しない。
enum ProtocolVersion {
    static let current: UInt32 = 8
}

struct AutoCommitProposal: Codable, Equatable {
    let proposal: UInt64
    let text: String
    let consumedReading: String
    let remaining: String

    private enum CodingKeys: String, CodingKey {
        case proposal, text, consumedReading = "consumed_reading", remaining
    }
}

struct SnapshotSegment: Codable, Equatable {
    let text: String
    let style: String?
}

enum Request: Decodable {
    case ping
    case startSession
    // style: "direct"=リテラル挿入(Shift英語モード)。nil=roman2kana(従来)。Rust 側は None の
    // ときキーを省略するので Optional デコードで旧 TIP 互換を保つ(left_context と同じ規約)。
    case insert(session: Int64, text: String, style: String?)
    case backspace(session: Int64)
    case convert(session: Int64, leftContext: String?)
    // 修正変換(Tab): ローマ字入力のタイポ修復仮説を先頭に立てた候補リストを返す。
    // Rust 側 `Request::TypoConvert` と対（一字一句一致規約。wire 形は Convert と同型）。
    case typoConvert(session: Int64, leftContext: String?)
    case commit(session: Int64, index: UInt32)
    case endSession(session: Int64)
    case reconvert(session: Int64, surface: String, leftContext: String?)
    case liveConvert(session: Int64, seq: UInt64, leftContext: String?, autoCommit: Bool)
    case liveSnapshot(composition: UInt64, revision: UInt64,
                      configurationGeneration: UInt64, connectionGeneration: UInt64,
                      segments: [SnapshotSegment], explicit: Bool, leftContext: String?)
    case pollSnapshotEnhancement(composition: UInt64, revision: UInt64,
                                 configurationGeneration: UInt64, connectionGeneration: UInt64,
                                 baseline: UInt64)
    case autoCommitReceipt(composition: UInt64, revision: UInt64,
                           configurationGeneration: UInt64, connectionGeneration: UInt64,
                           proposal: UInt64)
    case llmConvert(session: Int64, seq: UInt64, leftContext: String?)
    // ローカルインライン予測。通常変換とは別の接続・セッションで扱う。
    case predict(session: Int64, seq: UInt64, tokenIDs: [UInt32])
    // UU-5: 常駐エンジンへ最新設定を反映（session を伴わないプロセス全体設定）。
    case reloadConfig(ReloadConfigParams)
    // Spec2: 学習履歴の消去（session を伴わないプロセス全体操作）。
    case clearLearning
    // persist エンジンの graceful 停止（学習 flush → 応答後 exit）。session を伴わない
    // プロセス全体操作。Rust 側 `Request::Shutdown` と対（一字一句一致規約）。
    case shutdown
    // Zenzai runtime の sanitized 状態を問い合わせる。モデル導入状態とは別の観測。
    case queryZenzaiStatus
    // 失敗 latch の解除を伴う明示的な GPU 再試行。warm-up は engine 側で非同期に進む。
    case retryZenzai
    // 再変換訂正の通知(記録のみ・確定は TIP 側で完了済み)。Rust 側
    // `Request::RecordCorrection` と対（一字一句一致規約）。応答は既存 Ok。
    case recordCorrection(reading: String, surface: String)
    // カスタム辞書の再読込（session を伴わないプロセス全体操作）。Rust 側
    // `Request::ReloadDictionary` と対（一字一句一致規約）。エントリ列は載せない
    // ——エンジンがファイルを読み直す（保存成功後に送る規約なので常に最新が読める）。
    case reloadDictionary(enabled: Bool)
    // 文節ナビゲーション(変換中の←/→)。Rust 側 `Request::MoveClause` /
    // `SelectClauseCandidate` / `CommitClauses` と対（一字一句一致規約）。
    case moveClause(session: Int64, offset: Int, baseIndex: Int, leftContext: String?)
    case selectClauseCandidate(session: Int64, index: Int)
    case commitClauses(session: Int64)

    private enum Keys: String, CodingKey { case method, params }
    private struct InsertParams: Decodable { let session: Int64; let text: String; let style: String? }
    private struct SessionParams: Decodable { let session: Int64 }
    /// U9: Convert のみ left_context を持つ（SessionParams は Backspace/EndSession と共有のため触らない）。
    /// Rust 側は None のときキー自体を省略するので Optional（旧 TIP 互換もこれで担保）。
    private struct ConvertParams: Decodable { let session: Int64; let left_context: String? }
    private struct ReconvertParams: Decodable { let session: Int64; let surface: String; let left_context: String? }
    /// auto_commit は LiveConvert のみが使う（自動確定の許可 — Rust 側は false のときキー省略、
    /// 旧 TIP はキー自体を送らないので Optional。LlmConvert はこの構造体を共有するが無視する）。
    private struct LiveConvertParams: Decodable { let session: Int64; let seq: UInt64; let left_context: String?; let auto_commit: Bool? }
    private struct LiveSnapshotParams: Decodable {
        let composition: UInt64; let revision: UInt64
        let configuration_generation: UInt64; let connection_generation: UInt64
        let segments: [SnapshotSegment]; let explicit: Bool?; let left_context: String?
    }
    private struct SnapshotEnhancementParams: Decodable {
        let composition: UInt64; let revision: UInt64
        let configuration_generation: UInt64; let connection_generation: UInt64
        let baseline: UInt64
    }
    private struct AutoCommitReceiptParams: Decodable {
        let composition: UInt64; let revision: UInt64
        let configuration_generation: UInt64; let connection_generation: UInt64
        let proposal: UInt64
    }
    private struct PredictParams: Decodable { let session: Int64; let seq: UInt64; let token_ids: [UInt32] }
    private struct CommitParams: Decodable { let session: Int64; let index: UInt32 }
    private struct RecordCorrectionParams: Decodable { let reading: String; let surface: String }
    /// カスタム辞書: Rust `Request::ReloadDictionary` のフィールドと一字一句一致させること。
    private struct ReloadDictionaryParams: Decodable { let enabled: Bool }
    /// 文節ナビゲーション。left_context は Convert と同じ Optional 規約（Rust 側は None で省略）。
    private struct MoveClauseParams: Decodable {
        let session: Int64; let offset: Int; let base_index: UInt32; let left_context: String?
    }
    private struct SelectClauseCandidateParams: Decodable { let session: Int64; let index: UInt32 }
    /// UU-5: ReloadConfig の params。Rust `Request::ReloadConfig` のフィールドと一字一句一致させること。
    struct ReloadConfigParams: Decodable {
        let llm_enabled: Bool
        let llm_api_key: String
        let llm_endpoint: String
        let llm_model: String
        let llm_prompt: String
        let llm_timeout_ms: UInt32
        let zenzai_enabled: Bool
        let zenzai_weight: String
        // 旧 TIP は送らない。nil は「現行 runtime 設定を維持」。
        let inline_prediction_enabled: Bool?
        // Spec2: 学習トグル。旧 TIP は送らないので Optional（nil なら spawn 時 env のまま）。
        let learning_enabled: Bool?
        // 修正変換(Tab): 誤読み学習(ADR-0002)のトグル。旧 TIP は送らないので Optional
        // （learning_enabled と同じ互換規約）。
        let typo_learn_enabled: Bool?
        // Zenzai 推論上限。旧 TIP・診断 env override（D6）時の新 TIP は送らないので Optional
        // （nil なら spawn 時 env のまま — learning_enabled と同じ互換規約）。
        let zenzai_inference_limit: UInt32?
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        switch try c.decode(String.self, forKey: .method) {
        case "Ping": self = .ping
        case "StartSession": self = .startSession
        case "Insert": let p = try c.decode(InsertParams.self, forKey: .params); self = .insert(session: p.session, text: p.text, style: p.style)
        case "Backspace": let p = try c.decode(SessionParams.self, forKey: .params); self = .backspace(session: p.session)
        // U9: Convert のみ left_context を持つ。Rust 側は None のときキーを省略するので、
        // Optional デコードで旧TIP（キー無し）互換を保つ。
        case "Convert": let p = try c.decode(ConvertParams.self, forKey: .params); self = .convert(session: p.session, leftContext: p.left_context)
        // 修正変換(Tab): wire 形は Convert と同型なので ConvertParams を共有する。
        case "TypoConvert": let p = try c.decode(ConvertParams.self, forKey: .params); self = .typoConvert(session: p.session, leftContext: p.left_context)
        case "Reconvert": let p = try c.decode(ReconvertParams.self, forKey: .params); self = .reconvert(session: p.session, surface: p.surface, leftContext: p.left_context)
        case "Commit": let p = try c.decode(CommitParams.self, forKey: .params); self = .commit(session: p.session, index: p.index)
        case "LiveConvert": let p = try c.decode(LiveConvertParams.self, forKey: .params); self = .liveConvert(session: p.session, seq: p.seq, leftContext: p.left_context, autoCommit: p.auto_commit ?? false)
        case "LiveSnapshot":
            let p = try c.decode(LiveSnapshotParams.self, forKey: .params)
            self = .liveSnapshot(composition: p.composition, revision: p.revision,
                                 configurationGeneration: p.configuration_generation,
                                 connectionGeneration: p.connection_generation,
                                 segments: p.segments, explicit: p.explicit ?? false,
                                 leftContext: p.left_context)
        case "PollSnapshotEnhancement":
            let p = try c.decode(SnapshotEnhancementParams.self, forKey: .params)
            self = .pollSnapshotEnhancement(
                composition: p.composition, revision: p.revision,
                configurationGeneration: p.configuration_generation,
                connectionGeneration: p.connection_generation, baseline: p.baseline)
        case "AutoCommitReceipt":
            let p = try c.decode(AutoCommitReceiptParams.self, forKey: .params)
            self = .autoCommitReceipt(
                composition: p.composition, revision: p.revision,
                configurationGeneration: p.configuration_generation,
                connectionGeneration: p.connection_generation, proposal: p.proposal)
        case "LlmConvert": let p = try c.decode(LiveConvertParams.self, forKey: .params); self = .llmConvert(session: p.session, seq: p.seq, leftContext: p.left_context)
        case "Predict": let p = try c.decode(PredictParams.self, forKey: .params); self = .predict(session: p.session, seq: p.seq, tokenIDs: p.token_ids)
        case "EndSession": let p = try c.decode(SessionParams.self, forKey: .params); self = .endSession(session: p.session)
        case "ReloadConfig": let p = try c.decode(ReloadConfigParams.self, forKey: .params); self = .reloadConfig(p)
        case "ClearLearning": self = .clearLearning
        case "Shutdown": self = .shutdown
        case "QueryZenzaiStatus": self = .queryZenzaiStatus
        case "RetryZenzai": self = .retryZenzai
        case "RecordCorrection":
            let p = try c.decode(RecordCorrectionParams.self, forKey: .params)
            self = .recordCorrection(reading: p.reading, surface: p.surface)
        case "ReloadDictionary":
            let p = try c.decode(ReloadDictionaryParams.self, forKey: .params)
            self = .reloadDictionary(enabled: p.enabled)
        case "MoveClause":
            let p = try c.decode(MoveClauseParams.self, forKey: .params)
            self = .moveClause(session: p.session, offset: p.offset, baseIndex: Int(p.base_index), leftContext: p.left_context)
        case "SelectClauseCandidate":
            let p = try c.decode(SelectClauseCandidateParams.self, forKey: .params)
            self = .selectClauseCandidate(session: p.session, index: Int(p.index))
        case "CommitClauses":
            let p = try c.decode(SessionParams.self, forKey: .params)
            self = .commitClauses(session: p.session)
        case let m: throw DecodingError.dataCorruptedError(forKey: .method, in: c, debugDescription: "unknown method \(m)")
        }
    }
}

enum Response: Encodable {
    case pong
    // wire世代とEngineHost buildの完全一致だけをTIPが採用する。Rust `Response::Session` と対。
    case session(Int64, proto: UInt32?, boot: String?)
    case reading(String)
    case candidates([String])
    case ok
    case error(String)
    case liveResult(seq: UInt64, text: String, reading: String, committed: String?)
    case snapshotResult(composition: UInt64, revision: UInt64,
                        configurationGeneration: UInt64, connectionGeneration: UInt64,
                        text: String, candidates: [String]?, candidateRemaining: [String]?, baseline: UInt64,
                        autoCommit: AutoCommitProposal?)
    case snapshotEnhancement(composition: UInt64, revision: UInt64,
                             configurationGeneration: UInt64, connectionGeneration: UInt64,
                             baseline: UInt64, text: String,
                             candidates: [String]?, candidateRemaining: [String]?)
    case snapshotEnhancementPending
    case snapshotEnhancementUnavailable
    case llmResult(seq: UInt64, text: String)
    case prediction(seq: UInt64, text: String)
    case predictionUnavailable(seq: UInt64, state: String)
    case committed(text: String, reading: String)
    // 文節ナビゲーションのビュー。Rust 側 `Response::ClauseView` と対（一字一句一致規約）。
    case clauseView(segments: [String], selected: Int, candidates: [String], candidateIndex: Int)
    case zenzaiStatus(state: String, backend: String?, device: String?, reason: String?)

    private enum Keys: String, CodingKey {
        case result, session, reading, candidates, message, seq, text, committed, proto, boot, state
        case composition, revision, configurationGeneration = "configuration_generation"
        case connectionGeneration = "connection_generation"
        case segments, selected, backend, device, reason
        case candidateIndex = "candidate_index"
        case candidateRemaining = "candidate_remaining", baseline, autoCommit = "auto_commit"
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Keys.self)
        switch self {
        case .pong: try c.encode("Pong", forKey: .result)
        case .session(let s, let proto, let boot):
            try c.encode("Session", forKey: .result)
            try c.encode(s, forKey: .session)
            // nil のときキー省略＝handshake 導入前と wire 形一致（旧TIP互換。Rust 側 Option と対）。
            try c.encodeIfPresent(proto, forKey: .proto)
            try c.encodeIfPresent(boot, forKey: .boot)
        case .reading(let r): try c.encode("Reading", forKey: .result); try c.encode(r, forKey: .reading)
        case .candidates(let cs): try c.encode("Candidates", forKey: .result); try c.encode(cs, forKey: .candidates)
        case .liveResult(let seq, let text, let reading, let committed):
            try c.encode("LiveResult", forKey: .result)
            try c.encode(seq, forKey: .seq)
            try c.encode(text, forKey: .text)
            try c.encode(reading, forKey: .reading)
            // nil のときキー省略＝自動確定導入前と wire 形が一致（旧 TIP 互換。Rust 側 Option と対）。
            try c.encodeIfPresent(committed, forKey: .committed)
        case .snapshotResult(let composition, let revision, let configurationGeneration,
                             let connectionGeneration, let text, let candidates, let candidateRemaining, let baseline,
                             let autoCommit):
            try c.encode("SnapshotResult", forKey: .result)
            try c.encode(composition, forKey: .composition)
            try c.encode(revision, forKey: .revision)
            try c.encode(configurationGeneration, forKey: .configurationGeneration)
            try c.encode(connectionGeneration, forKey: .connectionGeneration)
            try c.encode(text, forKey: .text)
            try c.encodeIfPresent(candidates, forKey: .candidates)
            try c.encodeIfPresent(candidateRemaining, forKey: .candidateRemaining)
            try c.encode(baseline, forKey: .baseline)
            try c.encodeIfPresent(autoCommit, forKey: .autoCommit)
        case .snapshotEnhancement(let composition, let revision, let configurationGeneration,
                                  let connectionGeneration, let baseline, let text,
                                  let candidates, let candidateRemaining):
            try c.encode("SnapshotEnhancement", forKey: .result)
            try c.encode(composition, forKey: .composition)
            try c.encode(revision, forKey: .revision)
            try c.encode(configurationGeneration, forKey: .configurationGeneration)
            try c.encode(connectionGeneration, forKey: .connectionGeneration)
            try c.encode(baseline, forKey: .baseline)
            try c.encode(text, forKey: .text)
            try c.encodeIfPresent(candidates, forKey: .candidates)
            try c.encodeIfPresent(candidateRemaining, forKey: .candidateRemaining)
        case .snapshotEnhancementPending:
            try c.encode("SnapshotEnhancementPending", forKey: .result)
        case .snapshotEnhancementUnavailable:
            try c.encode("SnapshotEnhancementUnavailable", forKey: .result)
        case .llmResult(let seq, let text):
            try c.encode("LlmResult", forKey: .result)
            try c.encode(seq, forKey: .seq)
            try c.encode(text, forKey: .text)
        case .prediction(let seq, let text):
            try c.encode("Prediction", forKey: .result)
            try c.encode(seq, forKey: .seq)
            try c.encode(text, forKey: .text)
        case .predictionUnavailable(let seq, let state):
            try c.encode("PredictionUnavailable", forKey: .result)
            try c.encode(seq, forKey: .seq)
            try c.encode(state, forKey: .state)
        case .committed(let text, let reading):
            try c.encode("Committed", forKey: .result)
            try c.encode(text, forKey: .text)
            try c.encode(reading, forKey: .reading)
        case .clauseView(let segments, let selected, let candidates, let candidateIndex):
            try c.encode("ClauseView", forKey: .result)
            try c.encode(segments, forKey: .segments)
            try c.encode(selected, forKey: .selected)
            try c.encode(candidates, forKey: .candidates)
            try c.encode(candidateIndex, forKey: .candidateIndex)
        case .zenzaiStatus(let state, let backend, let device, let reason):
            try c.encode("ZenzaiStatus", forKey: .result)
            try c.encode(state, forKey: .state)
            try c.encodeIfPresent(backend, forKey: .backend)
            try c.encodeIfPresent(device, forKey: .device)
            try c.encodeIfPresent(reason, forKey: .reason)
        case .ok: try c.encode("Ok", forKey: .result)
        case .error(let m): try c.encode("Error", forKey: .result); try c.encode(m, forKey: .message)
        }
    }
}

extension Request {
    /// session を伴う op の session id（所有権チェック用 — UU-2）。ping/startSession は nil。
    /// 新しい case を足すときは必ずここにも並べること（session を伴うのに nil を返すと
    /// 所有権ガードを素通りする）。網羅 switch なので case 追加はコンパイルエラーで検出される。
    var sessionId: Int64? {
        switch self {
        case .ping, .startSession, .liveSnapshot, .pollSnapshotEnhancement, .autoCommitReceipt,
             .reloadConfig, .clearLearning, .shutdown, .queryZenzaiStatus,
             .retryZenzai, .recordCorrection,
             .reloadDictionary:
            // UU-5: ReloadConfig は session を伴わない（プロセス全体設定）。所有権ガード対象外。
            // Shutdown も同様（プロセス全体の graceful 停止）。RecordCorrection は確定済み訂正で
            // どのセッションにも属さない（ClearLearning と同じ共有資源扱い）。
            // ReloadDictionary もプロセス全体の共有資源（動的ユーザ辞書）の差し替え。
            return nil
        case .insert(let session, _, _),
             .reconvert(let session, _, _),
             .commit(let session, _),
             .liveConvert(let session, _, _, _),
             .llmConvert(let session, _, _),
             .predict(let session, _, _),
             .moveClause(let session, _, _, _):
            return session
        case .backspace(let session),
             .convert(let session, _),
             .typoConvert(let session, _),
             .endSession(let session),
             .selectClauseCandidate(let session, _),
             .commitClauses(let session):
            return session
        }
    }
}
