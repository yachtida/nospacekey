import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

public struct LLMError: Error, Equatable { public let message: String
    public init(message: String) { self.message = message } }

/// OpenAI 互換 /chat/completions へ POST して補正文を得る。
/// HTTP 送信は `send` で注入する（既定は URLSession 同期送信。テストはモックを渡す）。
///
/// `convert` はエンジンの単一処理スレッド上で数秒ブロックし得るが、その間に他の IME 要求が
/// 並行することは無い: パイプは単一インスタンスで、TIP は唯一の接続を専用 LLM ワーカへ move し
/// UI を入力ロックしてポーリングする（crates/tip/src/llm_worker.rs / NamedPipeServer.swift 参照）。
public final class LLMClient {
    private let config: LLMConfig
    private let send: (URLRequest) -> Result<Data, LLMError>

    public init(config: LLMConfig,
                send: @escaping (URLRequest) -> Result<Data, LLMError> = LLMClient.urlSessionSend) {
        self.config = config
        self.send = send
    }

    /// echo モード（テスト用）か。ConversionService がこれを見て HTTP を迂回する。
    public var isEcho: Bool { config.echo }

    /// U9: メッセージ配列の構築を切り出す（テスト容易化のため static・純関数）。
    /// leftContext が nil のとき、従来の payload とバイト等価な配列を返す（機微文脈の後方互換）。
    /// leftContext があるときは system メッセージにのみ参考文脈として追記する（user はそのまま）。
    static func buildMessages(prompt: String, reading: String, leftContext: String?) -> [[String: String]] {
        let systemContent: String
        if let ctx = leftContext, !ctx.isEmpty {
            systemContent = "\(prompt)\n直前の文脈(参考): \(ctx)"
        } else {
            systemContent = prompt
        }
        return [
            ["role": "system", "content": systemContent],
            ["role": "user", "content": reading],
        ]
    }

    /// 読み（ひらがな）を補正文へ。失敗は LLMError。
    /// `leftContext`: U9 — ドキュメント本文という機微データ。ここではログへ内容を出さない。
    public func convert(reading: String, leftContext: String? = nil) -> Result<String, LLMError> {
        guard config.enabled, let endpoint = config.endpoint, let key = config.apiKey else {
            return .failure(LLMError(message: "llm disabled"))
        }
        guard let url = URL(string: endpoint.hasSuffix("/") ? endpoint + "chat/completions"
                                                            : endpoint + "/chat/completions") else {
            return .failure(LLMError(message: "bad endpoint"))
        }
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.timeoutInterval = Double(config.timeoutMs) / 1000.0
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.setValue("Bearer \(key)", forHTTPHeaderField: "Authorization")
        let payload: [String: Any] = [
            "model": config.model,
            "messages": LLMClient.buildMessages(prompt: config.prompt, reading: reading, leftContext: leftContext),
            "temperature": 0.2,
        ]
        guard let body = try? JSONSerialization.data(withJSONObject: payload) else {
            return .failure(LLMError(message: "encode failed"))
        }
        req.httpBody = body

        switch send(req) {
        case .failure(let e): return .failure(e)
        case .success(let data):
            guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let choices = obj["choices"] as? [[String: Any]],
                  let first = choices.first,
                  let message = first["message"] as? [String: Any],
                  let content = message["content"] as? String else {
                return .failure(LLMError(message: "no content"))
            }
            let trimmed = content.trimmingCharacters(in: .whitespacesAndNewlines)
            let unquoted = LLMClient.stripQuotes(trimmed)
            if unquoted.isEmpty { return .failure(LLMError(message: "empty content")) }
            return .success(unquoted)
        }
    }

    /// モデルが補正文全体を引用符で囲んだ場合のみ外側1組を剥がす。
    /// - ASCII の ' は短縮形（it's 等）先頭を誤食する恐れがあるため対象にしない。
    /// - 内部に同じ区切りを含む場合は構造的な引用（複数の鉤括弧など）とみなし剥がさない。
    static func stripQuotes(_ s: String) -> String {
        let pairs: [(Character, Character)] = [("\"", "\""), ("「", "」")]
        for (l, r) in pairs where s.count >= 2 && s.first == l && s.last == r {
            let inner = s.dropFirst().dropLast()
            if !inner.contains(l) && !inner.contains(r) {
                return String(inner)
            }
        }
        return s
    }

    /// 既定の HTTP 送信: URLSession を同期化（DispatchSemaphore）。タイムアウトは URLRequest 側。
    public static func urlSessionSend(_ req: URLRequest) -> Result<Data, LLMError> {
        // Swift 6 strict concurrency: @Sendable な completion クロージャ内では captured var を
        // 直接 mutate できないため、参照型の箱経由で結果を書き戻す（セマフォで同期するので安全）。
        final class Box: @unchecked Sendable { var out: Result<Data, LLMError> = .failure(LLMError(message: "no response")) }
        let box = Box()
        let sem = DispatchSemaphore(value: 0)
        let task = URLSession.shared.dataTask(with: req) { data, resp, err in
            defer { sem.signal() }
            if let err = err { box.out = .failure(LLMError(message: "\(err)")); return }
            if let http = resp as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
                box.out = .failure(LLMError(message: "http \(http.statusCode)")); return
            }
            if let data = data { box.out = .success(data) }
        }
        task.resume()
        // 通常は URLRequest.timeoutInterval で必ず completion が呼ばれるが、万一呼ばれなくても
        // ワーカスレッドを永久ブロックさせない保険として request timeout + 余裕で打ち切る。
        let waitTimeout = DispatchTime.now() + .seconds(Int(req.timeoutInterval) + 5)
        if sem.wait(timeout: waitTimeout) == .timedOut {
            task.cancel()
            return .failure(LLMError(message: "timeout"))
        }
        return box.out
    }
}
