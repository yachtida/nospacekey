import XCTest
@testable import NospacekeyEngineCore

final class BackgroundMaintenanceTests: XCTestCase {
    func testBacklogIsBoundedAndOverflowIsObservable() {
        let executor = BackgroundMaintenance(capacity: 1)
        let started = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        XCTAssertTrue(executor.submit(label: "running") {
            started.signal()
            release.wait()
        })
        XCTAssertEqual(started.wait(timeout: .now() + 1), .success)
        XCTAssertTrue(executor.submit(label: "pending") {})
        XCTAssertFalse(executor.submit(label: "overflow") {})
        XCTAssertEqual(executor.snapshot.pending, 1)
        XCTAssertEqual(executor.snapshot.dropped, 1)
        release.signal()
        executor.flushForTesting()
    }

    func testFailureDoesNotStopLaterMaintenance() {
        let executor = BackgroundMaintenance(capacity: 2)
        let completed = DispatchSemaphore(value: 0)
        XCTAssertTrue(executor.submit(label: "failed") { throw TestFailure.expected })
        XCTAssertTrue(executor.submit(label: "later") { completed.signal() })
        XCTAssertEqual(completed.wait(timeout: .now() + 1), .success)
        executor.flushForTesting()
        XCTAssertEqual(executor.snapshot.failed, 1)
    }

    func testLatestWorkCoalescesToOnePendingJob() {
        final class Values: @unchecked Sendable {
            let lock = NSLock()
            var items: [Int] = []
        }
        let executor = BackgroundMaintenance(capacity: 1)
        let started = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let values = Values()
        executor.submit(label: "running") { started.signal(); release.wait() }
        XCTAssertEqual(started.wait(timeout: .now() + 1), .success)
        XCTAssertTrue(executor.submitLatest(label: "dictionary") {
            values.lock.lock(); values.items.append(1); values.lock.unlock()
        })
        XCTAssertTrue(executor.submitLatest(label: "dictionary") {
            values.lock.lock(); values.items.append(2); values.lock.unlock()
        })
        XCTAssertEqual(executor.snapshot.pending, 1)
        release.signal()
        executor.barrier()
        values.lock.lock(); let observed = values.items; values.lock.unlock()
        XCTAssertEqual(observed, [2])
    }

    func testLatestWorkSubmittedWhileRunningLeavesOneTrailingJob() {
        final class Values: @unchecked Sendable {
            let lock = NSLock()
            var items: [Int] = []
        }
        let executor = BackgroundMaintenance(capacity: 1)
        let started = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let values = Values()
        executor.submitLatest(label: "dictionary") {
            values.lock.lock(); values.items.append(0); values.lock.unlock()
            started.signal(); release.wait()
        }
        XCTAssertEqual(started.wait(timeout: .now() + 1), .success)
        executor.submitLatest(label: "dictionary") {
            values.lock.lock(); values.items.append(1); values.lock.unlock()
        }
        executor.submitLatest(label: "dictionary") {
            values.lock.lock(); values.items.append(2); values.lock.unlock()
        }
        XCTAssertEqual(executor.snapshot.pending, 1)
        release.signal()
        executor.barrier()
        values.lock.lock(); let observed = values.items; values.lock.unlock()
        XCTAssertEqual(observed, [0, 2])
    }

    func testBarrierCannotOvertakeAnAcceptedFirstSubmission() {
        let drainEnqueued = DispatchSemaphore(value: 0)
        let releaseSubmit = DispatchSemaphore(value: 0)
        let workRan = DispatchSemaphore(value: 0)
        let barrierReturned = DispatchSemaphore(value: 0)
        let executor = BackgroundMaintenance(afterDrainEnqueuedForTesting: {
            drainEnqueued.signal()
            releaseSubmit.wait()
        })
        Thread.detachNewThread { executor.submit(label: "accepted") { workRan.signal() } }
        XCTAssertEqual(drainEnqueued.wait(timeout: .now() + 1), .success)
        Thread.detachNewThread { executor.barrier(); barrierReturned.signal() }
        XCTAssertEqual(barrierReturned.wait(timeout: .now() + .milliseconds(20)), .timedOut)
        releaseSubmit.signal()
        XCTAssertEqual(workRan.wait(timeout: .now() + 1), .success)
        XCTAssertEqual(barrierReturned.wait(timeout: .now() + 1), .success)
    }

    func testDelayedLatestKeepsOnlyTheNewestWaitingWork() {
        let executor = BackgroundMaintenance(capacity: 1)
        let value = LockedValue(0)
        XCTAssertTrue(executor.submitLatestAfter(label: "dictionary", delay: .seconds(30)) {
            value.set(1)
        })
        XCTAssertTrue(executor.submitLatestAfter(label: "dictionary", delay: .seconds(30)) {
            value.set(2)
        })
        XCTAssertEqual(executor.snapshot.pending, 1)
        XCTAssertTrue(executor.releaseDelayedForTesting())
        executor.barrier()
        XCTAssertEqual(value.get(), 2)
    }

    func testImmediateLatestReplacesDelayedWork() {
        let executor = BackgroundMaintenance(capacity: 1)
        let value = LockedValue(0)
        XCTAssertTrue(executor.submitLatestAfter(label: "dictionary", delay: .seconds(30)) {
            value.set(1)
        })
        XCTAssertTrue(executor.submitLatest(label: "dictionary") { value.set(2) })
        executor.barrier()
        XCTAssertEqual(value.get(), 2)
        XCTAssertFalse(executor.releaseDelayedForTesting())
    }

    func testBarrierDoesNotWaitForFutureDelayedWork() {
        let executor = BackgroundMaintenance(capacity: 1)
        let value = LockedValue(0)
        XCTAssertTrue(executor.submitLatestAfter(label: "dictionary", delay: .seconds(30)) {
            value.set(1)
        })
        executor.barrier()
        XCTAssertEqual(value.get(), 0)
        XCTAssertEqual(executor.snapshot.pending, 1)
        XCTAssertTrue(executor.releaseDelayedForTesting())
        executor.barrier()
    }

    private final class LockedValue: @unchecked Sendable {
        private let lock = NSLock()
        private var value: Int
        init(_ value: Int) { self.value = value }
        func set(_ newValue: Int) { lock.lock(); value = newValue; lock.unlock() }
        func get() -> Int { lock.lock(); defer { lock.unlock() }; return value }
    }

    private enum TestFailure: Error { case expected }
}
