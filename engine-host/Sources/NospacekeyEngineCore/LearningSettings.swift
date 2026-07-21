import Foundation

/// 学習（変換履歴）の有効/無効と永続化先を env から解決する純粋ロジック。
/// ZenzaiConfig と同型: グローバル状態に触れず environment/ensureDir を注入してテスト可能にする。
///
/// 既定は **OFF**（`NOSPACEKEY_LEARNING` が "1" のときだけ ON）。
/// 製品経路では TIP が settings.json（learning.enabled 既定 true）から resolve_env_map で
/// 必ず "1"/"0" を注入するためユーザー既定は ON。素の swift test / 手動起動では env が無く
/// OFF — テストが実ユーザーの %LOCALAPPDATA% を汚さない安全側既定（spec Task0 修正）。
public struct LearningSettings: Equatable, Sendable {
    /// 学習が有効か（memoryDir の準備に成功した場合のみ true）。
    public let enabled: Bool
    /// 学習データの永続化先。enabled=false のときは nil。
    public let memoryDir: URL?

    public init(enabled: Bool, memoryDir: URL?) {
        self.enabled = enabled
        self.memoryDir = memoryDir
    }

    public static let disabled = LearningSettings(enabled: false, memoryDir: nil)

    /// 解決順:
    /// 1. `NOSPACEKEY_LEARNING` != "1" → OFF（dir も解決しない）
    /// 2. dir = `NOSPACEKEY_MEMORY_DIR`（非空なら優先）or `%LOCALAPPDATA%\nospacekey\memory`
    /// 3. ensureDir(dir) 失敗（LOCALAPPDATA 不在含む）→ OFF に劣化（黙って壊れない: 呼び出し側で log）
    public static func resolve(
        environment: [String: String],
        ensureDir: (URL) -> Bool = { url in
            (try? FileManager.default.createDirectory(
                at: url, withIntermediateDirectories: true)) != nil
        }
    ) -> LearningSettings {
        guard environment["NOSPACEKEY_LEARNING"] == "1" else { return .disabled }
        guard let dir = resolveDir(environment: environment), ensureDir(dir) else {
            return .disabled
        }
        return LearningSettings(enabled: true, memoryDir: dir)
    }

    /// 学習の有効/無効に関わらず「学習データが置かれる場所」を解決する（履歴消去用）。
    public static func resolveDir(environment: [String: String]) -> URL? {
        if let explicit = environment["NOSPACEKEY_MEMORY_DIR"], !explicit.isEmpty {
            return URL(fileURLWithPath: explicit)
        }
        if let lad = environment["LOCALAPPDATA"], !lad.isEmpty {
            return URL(fileURLWithPath: lad)
                .appendingPathComponent("nospacekey").appendingPathComponent("memory")
        }
        return nil
    }
}
