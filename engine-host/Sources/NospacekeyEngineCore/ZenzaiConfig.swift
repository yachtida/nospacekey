import Foundation
import WinSDK

/// Zenzai の有効化・重みパス・推論上限を、明示 weight → per-user(%LOCALAPPDATA%) → exe 隣の
/// 3段解決表から解決する純粋ロジック（設定UI crates/config/src/download.rs の detect_model と同一の表）。
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

    /// vendor の llama.cpp は AVX2 ベースラインでビルドされる（scripts/build-llama.ps1）ため、
    /// AVX2 非搭載 CPU で重みをロードして推論すると 0xC000001D（不正命令）でプロセスごと即死し、
    /// Zenzai どころか古典変換まで巻き添えになる（v1.0.0 の Core Ultra 実配布事故）。
    /// llama 側に照会しない（ggml_cpu_has_avx2 は DLL ロード後でないと呼べず、判定したい状況は
    /// まさにその DLL のコードを実行したくない状況）ので OS の CPU 機能照会で判定する。
    /// Win10 2004 未満は PF_AVX2 が未定義で常に false になるが、その場合も古典変換への
    /// graceful な退行であり、クラッシュ側に倒れることはない。
    public static let runtimeCPUMeetsLlamaBaseline: Bool = {
        let supported = IsProcessorFeaturePresent(DWORD(40 /* PF_AVX2_INSTRUCTIONS_AVAILABLE */))
        if !supported {
            engineLog("ev=zenzai_disabled reason=cpu_no_avx2\n")
            return false
        }
        return true
    }()

    /// 解決順（設定UI crates/config/src/download.rs の detect_model と同一の表）:
    /// 1. env `NOSPACEKEY_ZENZAI=off`（大文字小文字不問）→ 強制古典（weightURL=nil）
    /// 2. CPU が llama ビルドのベースライン（AVX2）未満 → 強制古典（クラッシュ防止）
    /// 3. env `NOSPACEKEY_ZENZAI_WEIGHT` のパス
    /// 4. `%LOCALAPPDATA%\nospacekey\models\ggml-model-Q5_K_M.gguf`（設定UIの per-user DL 先）
    /// 5. 既定 `<exeDir>/models/ggml-model-Q5_K_M.gguf`
    /// 3〜5 のうち最初に**実在**した候補を採用、無ければ nil（古典）。明示パスが設定済みでも
    /// 消失していれば次候補へフォールバックする — UI 側 detect_model と同じ挙動に揃えることで
    /// 「UI は導入済み表示・エンジンは古典にサイレント劣化」の解離を防ぐ（UIバグ8）。
    public static func resolve(
        exeDir: URL,
        environment: [String: String],
        fileExists: (String) -> Bool = { FileManager.default.fileExists(atPath: $0) },
        cpuMeetsLlamaBaseline: Bool = ZenzaiConfig.runtimeCPUMeetsLlamaBaseline
    ) -> ZenzaiConfig {
        let limit = environment["NOSPACEKEY_ZENZAI_INFERENCE_LIMIT"].flatMap(Int.init) ?? 1

        if environment["NOSPACEKEY_ZENZAI"]?.lowercased() == "off" {
            return ZenzaiConfig(weightURL: nil, inferenceLimit: limit)
        }
        if !cpuMeetsLlamaBaseline {
            return ZenzaiConfig(weightURL: nil, inferenceLimit: limit)
        }

        var candidates: [String] = []
        if let explicit = environment["NOSPACEKEY_ZENZAI_WEIGHT"], !explicit.isEmpty {
            candidates.append(explicit)
        }
        if let localAppData = environment["LOCALAPPDATA"], !localAppData.isEmpty {
            candidates.append(
                URL(fileURLWithPath: localAppData)
                    .appendingPathComponent("nospacekey")
                    .appendingPathComponent("models")
                    .appendingPathComponent(defaultWeightFileName)
                    .path
            )
        }
        candidates.append(
            exeDir
                .appendingPathComponent("models")
                .appendingPathComponent(defaultWeightFileName)
                .path
        )

        for candidatePath in candidates where fileExists(candidatePath) {
            return ZenzaiConfig(weightURL: URL(fileURLWithPath: candidatePath), inferenceLimit: limit)
        }
        return ZenzaiConfig(weightURL: nil, inferenceLimit: limit)
    }
}
