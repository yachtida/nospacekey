import Foundation

/// Owned by the service's learning lock. A replay never re-enters learning,
/// including after settings changed or the candidate tokens expired.
struct CommitReceiptLedger {
    private struct Entry {
        let receipt: CommitReceipt
        let outcome: CommitReceiptAck.Outcome
        let receivedAt: TimeInterval
    }
    private var entries: [CommitId: Entry] = [:]
    let capacity: Int

    init(capacity: Int = 4096) { self.capacity = capacity }

    mutating func process(_ receipt: CommitReceipt, now: TimeInterval,
                          apply: () -> CommitReceiptAck.Outcome) -> CommitReceiptAck {
        entries = entries.filter { now - $0.value.receivedAt < 60 }
        let outcome: CommitReceiptAck.Outcome
        if let prior = entries[receipt.commit_id] {
            if prior.receipt != receipt { outcome = .rejected(.conflict) }
            else if prior.outcome == .applied { outcome = .alreadyProcessed }
            else { outcome = prior.outcome }
        } else if entries.count >= capacity {
            outcome = .rejected(.capacity)
        } else {
            outcome = apply()
            entries[receipt.commit_id] = Entry(receipt: receipt, outcome: outcome, receivedAt: now)
        }
        return CommitReceiptAck(commitId: receipt.commit_id, outcome: outcome)
    }
}
