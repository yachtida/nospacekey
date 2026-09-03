import Foundation

final class BackgroundMaintenance: @unchecked Sendable {
    struct Snapshot: Equatable {
        let pending: Int
        let dropped: UInt64
        let failed: UInt64
    }

    typealias Work = @Sendable () throws -> Void

    private let capacity: Int
    private let afterDrainEnqueuedForTesting: (@Sendable () -> Void)?
    private let queue = DispatchQueue(label: "nospacekey.maintenance")
    private let delayedTimer: DispatchSourceTimer
    private let lock = NSLock()
    private var pending: [(String, Work)] = []
    private var coalesced: [String: Work] = [:]
    private var coalescedOrder: [String] = []
    private var delayed: [String: (deadline: DispatchTime, work: Work)] = [:]
    private var preferCoalesced = true
    private var running = false
    private var dropped: UInt64 = 0
    private var failed: UInt64 = 0

    init(capacity: Int = 64,
         afterDrainEnqueuedForTesting: (@Sendable () -> Void)? = nil) {
        self.capacity = max(1, capacity)
        self.afterDrainEnqueuedForTesting = afterDrainEnqueuedForTesting
        self.delayedTimer = DispatchSource.makeTimerSource(queue: queue)
        delayedTimer.setEventHandler { [weak self] in self?.promoteDueDelayed() }
        delayedTimer.schedule(deadline: .distantFuture)
        delayedTimer.resume()
    }

    @discardableResult
    func submit(label: String, _ work: @escaping Work) -> Bool {
        lock.lock()
        guard pending.count < capacity else {
            dropped &+= 1
            let count = dropped
            lock.unlock()
            engineLog("ev=maintenance_overflow op=\(label) dropped=\(count)\n")
            return false
        }
        pending.append((label, work))
        let shouldStart = !running
        if shouldStart { running = true }
        if shouldStart { enqueueDrainLocked() }
        lock.unlock()
        return true
    }

    /// Keep at most one pending job per label. Repeated requests replace the pending work, while
    /// a request arriving during execution leaves exactly one follow-up carrying the latest state.
    @discardableResult
    func submitLatest(label: String, _ work: @escaping Work) -> Bool {
        lock.lock()
        if delayed.removeValue(forKey: label) != nil { scheduleNextDelayedLocked() }
        if coalesced[label] != nil {
            coalesced[label] = work
            lock.unlock()
            return true
        }
        guard coalesced.count + delayed.count < capacity else {
            dropped &+= 1
            let count = dropped
            lock.unlock()
            engineLog("ev=maintenance_overflow op=\(label) dropped=\(count)\n")
            return false
        }
        coalesced[label] = work
        coalescedOrder.append(label)
        let shouldStart = !running
        if shouldStart { running = true; enqueueDrainLocked() }
        lock.unlock()
        return true
    }

    /// Schedule one replaceable retry per label. An immediate submitLatest for the same label wins.
    @discardableResult
    func submitLatestAfter(label: String, delay: DispatchTimeInterval,
                           _ work: @escaping Work) -> Bool {
        let deadline = DispatchTime.now() + delay
        lock.lock()
        guard coalesced[label] == nil else {
            lock.unlock()
            return false
        }
        if delayed[label] == nil, coalesced.count + delayed.count >= capacity {
            dropped &+= 1
            let count = dropped
            lock.unlock()
            engineLog("ev=maintenance_overflow op=\(label) dropped=\(count)\n")
            return false
        }
        delayed[label] = (deadline, work)
        scheduleNextDelayedLocked()
        lock.unlock()
        return true
    }

    var snapshot: Snapshot {
        lock.lock()
        defer { lock.unlock() }
        return Snapshot(pending: pending.count + coalesced.count + delayed.count,
                        dropped: dropped, failed: failed)
    }

    /// Waits for work already runnable when the fence enters the serial queue. Future delayed work
    /// is intentionally excluded; callers use generation invalidation when it must become stale.
    func barrier() {
        queue.sync {}
    }

    func flushForTesting() { barrier() }

    @discardableResult
    func releaseDelayedForTesting() -> Bool {
        lock.lock()
        let scheduled = delayed.sorted { $0.value.deadline.uptimeNanoseconds < $1.value.deadline.uptimeNanoseconds }
        delayed.removeAll()
        delayedTimer.schedule(deadline: .distantFuture)
        var promoted = false
        for (label, item) in scheduled where coalesced[label] == nil {
            coalesced[label] = item.work
            coalescedOrder.append(label)
            promoted = true
        }
        let shouldStart = promoted && !running
        if shouldStart { running = true; enqueueDrainLocked() }
        lock.unlock()
        return promoted
    }

    private func enqueueDrainLocked() {
        queue.async { [weak self] in self?.drain() }
        afterDrainEnqueuedForTesting?()
    }

    private func scheduleNextDelayedLocked() {
        guard let next = delayed.values.min(by: {
            $0.deadline.uptimeNanoseconds < $1.deadline.uptimeNanoseconds
        }) else {
            delayedTimer.schedule(deadline: .distantFuture)
            return
        }
        delayedTimer.schedule(deadline: next.deadline, leeway: .milliseconds(5))
    }

    private func promoteDueDelayed() {
        lock.lock()
        let now = DispatchTime.now().uptimeNanoseconds
        let due = delayed.filter { $0.value.deadline.uptimeNanoseconds <= now }
        for (label, item) in due {
            delayed[label] = nil
            if coalesced[label] == nil {
                coalesced[label] = item.work
                coalescedOrder.append(label)
            }
        }
        scheduleNextDelayedLocked()
        let shouldStart = !due.isEmpty && !running && !coalesced.isEmpty
        if shouldStart { running = true }
        lock.unlock()
        // The timer already runs on `queue`. Draining inline keeps a due job ahead of a barrier
        // that was enqueued after the timer event; re-enqueuing would let that fence overtake it.
        if shouldStart { drain() }
    }

    private func drain() {
        while true {
            lock.lock()
            let item: (String, Work)?
            if !coalescedOrder.isEmpty && (preferCoalesced || pending.isEmpty) {
                let label = coalescedOrder.removeFirst()
                item = coalesced.removeValue(forKey: label).map { (label, $0) }
                preferCoalesced = false
            } else if !pending.isEmpty {
                item = pending.removeFirst()
                preferCoalesced = true
            } else if !coalescedOrder.isEmpty {
                let label = coalescedOrder.removeFirst()
                item = coalesced.removeValue(forKey: label).map { (label, $0) }
                preferCoalesced = false
            } else {
                running = false
                lock.unlock()
                return
            }
            lock.unlock()
            guard let (label, work) = item else { continue }
            do {
                try work()
            } catch {
                lock.lock()
                failed &+= 1
                lock.unlock()
                engineLog("ev=maintenance_failed op=\(label) error=\(error)\n")
            }
        }
    }
}
