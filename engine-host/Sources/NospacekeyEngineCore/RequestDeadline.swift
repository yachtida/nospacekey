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
