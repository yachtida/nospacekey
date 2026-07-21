import Foundation

/// 外部LLM（OpenAI互換）の設定を env から解決する純ロジック。
/// グローバル状態に触れず environment を注入してユニットテスト可能にする（ZenzaiConfig と同型）。
public struct LLMConfig: Equatable {
    public let apiKey: String?
    public let endpoint: String?   // base URL 例: https://api.openai.com/v1
    public let model: String
    public let prompt: String
    public let timeoutMs: Int
    public let echo: Bool           // テスト専用: true なら HTTP を呼ばず "LLM:"+reading を返す

    public init(apiKey: String?, endpoint: String?, model: String, prompt: String, timeoutMs: Int, echo: Bool) {
        self.apiKey = apiKey; self.endpoint = endpoint; self.model = model
        self.prompt = prompt; self.timeoutMs = timeoutMs; self.echo = echo
    }

    /// API キーとエンドポイントが揃っていれば有効。
    public var enabled: Bool { (apiKey?.isEmpty == false) && (endpoint?.isEmpty == false) }

    public static let defaultModel = "gpt-4o-mini"
    public static let defaultPrompt = """
    次の『読み（ひらがな）』を、文脈的に最も自然で正しい漢字かな交じりの日本語文に変換してください。\
    誤変換や読みの打ち間違いも訂正してください。変換結果の本文のみを出力し、説明・引用符・前置きは一切付けないでください。
    """

    public static func resolve(environment: [String: String]) -> LLMConfig {
        let key = environment["NOSPACEKEY_LLM_API_KEY"]
        let endpoint = environment["NOSPACEKEY_LLM_ENDPOINT"]
        let model = environment["NOSPACEKEY_LLM_MODEL"].flatMap { $0.isEmpty ? nil : $0 } ?? defaultModel
        let prompt = environment["NOSPACEKEY_LLM_PROMPT"].flatMap { $0.isEmpty ? nil : $0 } ?? defaultPrompt
        let timeout = environment["NOSPACEKEY_LLM_TIMEOUT_MS"].flatMap(Int.init) ?? 15000
        let echo = ["1", "true", "yes"].contains(environment["NOSPACEKEY_LLM_ECHO"]?.lowercased() ?? "")
        return LLMConfig(apiKey: key, endpoint: endpoint, model: model, prompt: prompt, timeoutMs: timeout, echo: echo)
    }
}
