import Foundation

/// `NOSPACEKEY_LOG` が有効(非空・"0"以外)のときだけ true。テスト用に値を注入できる純関数。
func logEnabled(_ v: String?) -> Bool {
    guard let v, !v.isEmpty, v != "0" else { return false }
    return true
}

/// 起動時に1回だけ評価してキャッシュする診断ログ判定（Swift のグローバルは遅延・スレッド安全に初期化される）。
private let engineLogEnabled = logEnabled(ProcessInfo.processInfo.environment["NOSPACEKEY_LOG"])
private let engineLogSink = EngineLogSink()

private final class EngineLogSink: @unchecked Sendable {
    private let lock = NSLock()
    private let queue = DispatchQueue(label: "nospacekey.diagnostics")
    private var pending: [Data] = []
    private var running = false
    private var dropped: UInt64 = 0

    func submit(_ data: Data) {
        lock.lock()
        guard pending.count < 256 else {
            dropped &+= 1
            lock.unlock()
            return
        }
        pending.append(data)
        let shouldStart = !running
        if shouldStart { running = true }
        lock.unlock()
        if shouldStart { queue.async { [weak self] in self?.drain() } }
    }

    private func drain() {
        while true {
            lock.lock()
            guard !pending.isEmpty else {
                running = false
                lock.unlock()
                return
            }
            let data = pending.removeFirst()
            let lost = dropped
            dropped = 0
            lock.unlock()
            if lost > 0 {
                FileHandle.standardError.write(Data("ev=diagnostic_overflow dropped=\(lost)\n".utf8))
            }
            FileHandle.standardError.write(data)
        }
    }
}

/// 診断行に epoch ms を前置する（tip 側 tip_log の `ts=` と同一キー）。純関数（テスト用に時刻注入）。
/// 2026-07-09 高CPU診断で「ログに時刻が無く時間帯突合が全て推測」が最大の障害だった対策。
func timestampedEngineLogLine(_ s: String, epochMs: Int64) -> String {
    "ts=\(epochMs) \(s)"
}

/// 診断出力の唯一の出口。無効時は何も書かない。stderr は Rust 側が
/// `NOSPACEKEY_LOG` 有効時のみ `nospacekey-engine.log` へリダイレクトする（二重防御）。
func engineLog(_ s: String) {
    guard engineLogEnabled else { return }
    let ms = Int64(Date().timeIntervalSince1970 * 1000)
    engineLogSink.submit(Data(timestampedEngineLogLine(s, epochMs: ms).utf8))
}
