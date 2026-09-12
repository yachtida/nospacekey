import Foundation
import XCTest
@testable import NospacekeyEngineCore

final class ZenzaiInvocationCounterTests: XCTestCase {
    func testMainClassicProcessDoesNotEnterVendorZenzaiSeam() {
        let service = ConversionService(
            config: ZenzaiConfig(
                weightURL: URL(fileURLWithPath: "C:/missing/zenzai.gguf"), inferenceLimit: 1),
            processRole: .mainClassicOnly)
        service.setZenzaiReadyForTesting(true)

        let session = service.startSession()
        _ = service.insert(session: session, text: "nihongo")
        _ = service.convert(session: session)

        XCTAssertEqual(service.zenzaiVendorInvocationCountForTesting, 0)
    }

    func testLegacyProcessCountsARequestThatEntersVendorZenzaiSeam() {
        let service = ConversionService(
            config: ZenzaiConfig(
                weightURL: URL(fileURLWithPath: "C:/missing/zenzai.gguf"), inferenceLimit: 1))
        service.setZenzaiReadyForTesting(true)

        let session = service.startSession()
        _ = service.insert(session: session, text: "nihongo")
        _ = service.convert(session: session)

        XCTAssertGreaterThan(service.zenzaiVendorInvocationCountForTesting, 0)
    }
}
