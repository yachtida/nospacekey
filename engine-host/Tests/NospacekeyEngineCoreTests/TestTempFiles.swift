import Foundation

/// テスト用の一時 JSON ファイルを書き出す（後片付けは呼び出し側の removeItem）。
/// ファイル名を UUID で一意にするのは、並行実行のテスト同士が同じパスを奪い合わないため。
func writeTempJson(_ s: String) throws -> URL {
    let url = FileManager.default.temporaryDirectory
        .appendingPathComponent("ud-test-\(UUID().uuidString).json")
    try Data(s.utf8).write(to: url)
    return url
}
