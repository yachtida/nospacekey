import XCTest
@testable import NospacekeyEngineCore

final class LoggingTests: XCTestCase {
    func testLogEnabledRules() {
        XCTAssertFalse(logEnabled(nil))
        XCTAssertFalse(logEnabled(""))
        XCTAssertFalse(logEnabled("0"))
        XCTAssertTrue(logEnabled("1"))
        XCTAssertTrue(logEnabled("true"))
    }

    // 診断行には epoch ms を前置する（tip 側 tip_log の `ts=` と同一キー。
    // 2026-07-09 高CPU診断でログに時刻が無く時間帯突合が全て推測になった反省）。
    func testTimestampPrefix() {
        XCTAssertEqual(
            timestampedEngineLogLine("ev=coldstart stage=warmup ms=1.0\n", epochMs: 1_752_000_000_123),
            "ts=1752000000123 ev=coldstart stage=warmup ms=1.0\n"
        )
    }
}
