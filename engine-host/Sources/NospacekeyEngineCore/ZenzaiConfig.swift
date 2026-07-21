import Foundation

/// Zenzai の有効化・重みパス・推論上限を env と exe 隣の既定パスから解決する純粋ロジック。
/// グローバル状態に触れず、`environment`/`exeDir`/`fileExists` を注入してユニットテスト可能にする。
public struct ZenzaiConfig: Equatable {
    /// 重みファイル URL。nil なら古典変換（Zenzai 無効）。
    public let weightURL: URL?
    /// 推論回数上限（zenz の inferenceLimit）。
    public let inferenceLimit: Int

    public init(weightURL: URL?, inferenceLimit: Int) {
        self.weightURL = weightURL
        self.inferenceLimit = inferenceLimit
    }

    /// 既定モデルファイル名（HuggingFace Miwa-Keita/zenz-v3.1-small-gguf）。
    public static let defaultWeightFileName = "ggml-model-Q5_K_M.gguf"

    /// 解決順:
    /// 1. env `NOSPACEKEY_ZENZAI=off`（大文字小文字不問）→ 強制古典（weightURL=nil）
    /// 2. env `NOSPACEKEY_ZENZAI_WEIGHT` のパス
    /// 3. 既定 `<exeDir>/models/ggml-model-Q5_K_M.gguf`
    /// 2/3 の候補が実在すれば weightURL に採用、無ければ nil（古典）。
    public static func resolve(
        exeDir: URL,
        environment: [String: String],
        fileExists: (String) -> Bool = { FileManager.default.fileExists(atPath: $0) }
    ) -> ZenzaiConfig {
        let limit = environment["NOSPACEKEY_ZENZAI_INFERENCE_LIMIT"].flatMap(Int.init) ?? 1

        if environment["NOSPACEKEY_ZENZAI"]?.lowercased() == "off" {
            return ZenzaiConfig(weightURL: nil, inferenceLimit: limit)
        }

        let candidatePath: String
        if let explicit = environment["NOSPACEKEY_ZENZAI_WEIGHT"], !explicit.isEmpty {
            candidatePath = explicit
        } else {
            // .path は Windows では区切りが `/` になり得るが、既定の
            // FileManager.default.fileExists は両区切りを受理する。
            candidatePath = exeDir
                .appendingPathComponent("models")
                .appendingPathComponent(defaultWeightFileName)
                .path
        }

        if fileExists(candidatePath) {
            return ZenzaiConfig(weightURL: URL(fileURLWithPath: candidatePath), inferenceLimit: limit)
        }
        return ZenzaiConfig(weightURL: nil, inferenceLimit: limit)
    }
}
