import XCTest
@testable import NospacekeyEngineCore

final class FramingTests: XCTestCase {
    func testEncodePrependsLittleEndianLength() throws {
        let data = try Framing.encode(Response.reading("にほんご"))
        XCTAssertGreaterThan(data.count, 4)
        let len = data.prefix(4).withUnsafeBytes { UInt32(littleEndian: $0.load(as: UInt32.self)) }
        XCTAssertEqual(Int(len), data.count - 4)
        let body = data.suffix(from: 4)
        let s = String(data: body, encoding: .utf8)!
        XCTAssertTrue(s.contains("Reading"))
        XCTAssertTrue(s.contains("にほんご"))
    }
}
