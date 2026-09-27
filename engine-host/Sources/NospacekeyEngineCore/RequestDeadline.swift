import Foundation
import WinSDK

/// The Windows boot clock is shared by TIP and engine. An optional deadline is
/// backwards compatible; old clients receive the server's bounded admission budget.
struct RequestEnvelope: Decodable {
    let request: Request
    let deadlineTickMilliseconds: UInt64?

    private enum CodingKeys: String, CodingKey { case deadline_tick_ms }
    init(from decoder: Decoder) throws {
        request = try Request(from: decoder)
        deadlineTickMilliseconds = try decoder.container(keyedBy: CodingKeys.self)
            .decodeIfPresent(UInt64.self, forKey: .deadline_tick_ms)
    }

    func admissionDeadline(receivedAt: UInt64) -> RequestDeadline? {
        let budget: UInt64
        switch request {
        case .liveSnapshot(_, _, _, _, _, let explicit, _, _, _, _): budget = explicit ? 1_200 : 400
        case .inputPredictions: budget = 400
        case .clauseCandidates, .convertClauses: budget = 1_200
        // 混在変換は明示変換と同じ予算。区間数で積み増しせず、要求全体で共有する
        // （実装計画 §6.4 — MixedConversionService が残り時間を各区間へ配る）。
        case .mixedConvert: budget = 1_200
        default: return nil // Never drop commits, receipts, or lifecycle operations.
        }
        let limit = receivedAt.addingReportingOverflow(budget)
        let serverDeadline = limit.overflow ? UInt64.max : limit.partialValue
        return RequestDeadline(tickMilliseconds: min(deadlineTickMilliseconds ?? serverDeadline, serverDeadline))
    }
}

struct RequestDeadline: Sendable {
    let tickMilliseconds: UInt64
    var expired: Bool { GetTickCount64() >= tickMilliseconds }

    /// 残り予算（秒）。期限切れ後は 0。下位の変換呼び出しが「この待機を始めても
    /// 要求全体の期限に収まるか」を判定するのに使う（GPU ワーカーの予算と比較）。
    var remainingSeconds: TimeInterval {
        let now = GetTickCount64()
        guard now < tickMilliseconds else { return 0 }
        return Double(tickMilliseconds - now) / 1_000
    }

    func acquire(_ lock: NSLock) -> Bool {
        while true {
            let now = GetTickCount64()
            guard now < tickMilliseconds else { return false }
            // Short wall-clock waits are rechecked against the boot clock, including
            // after acquiring the lock. Queueing behind either lock spends one budget.
            let slice = Double(min(tickMilliseconds - now, 20)) / 1_000
            if lock.lock(before: Date(timeIntervalSinceNow: slice)) {
                if expired { lock.unlock(); return false }
                return true
            }
        }
    }

    static func acquire(_ lock: NSLock, before deadline: RequestDeadline?) -> Bool {
        if let deadline { return deadline.acquire(lock) }
        lock.lock()
        return true
    }
}
