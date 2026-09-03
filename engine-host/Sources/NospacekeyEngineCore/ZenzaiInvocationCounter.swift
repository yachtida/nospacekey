import Foundation

/// Counts calls that actually enter the vendor converter with Zenzai enabled.
/// The counter is deliberately structural: a classic-only process can prove
/// that its vendor invocation count is zero without relying on a declaration
/// log or a test-only assignment.
struct ZenzaiInvocationCounter: Sendable {
    private(set) var value: UInt64 = 0

    mutating func recordInvocation() {
        value &+= 1
    }
}
