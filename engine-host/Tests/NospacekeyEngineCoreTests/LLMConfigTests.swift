import XCTest
@testable import NospacekeyEngineCore

final class LLMConfigTests: XCTestCase {
    func testDisabledWhenNoKeyOrEndpoint() {
        XCTAssertFalse(LLMConfig.resolve(environment: [:]).enabled)
        XCTAssertFalse(LLMConfig.resolve(environment: ["NOSPACEKEY_LLM_API_KEY": "k"]).enabled)
        XCTAssertFalse(LLMConfig.resolve(environment: ["NOSPACEKEY_LLM_ENDPOINT": "https://x/v1"]).enabled)
    }

    func testEnabledWhenKeyAndEndpointPresent() {
        let c = LLMConfig.resolve(environment: [
            "NOSPACEKEY_LLM_API_KEY": "k", "NOSPACEKEY_LLM_ENDPOINT": "https://x/v1",
        ])
        XCTAssertTrue(c.enabled)
        XCTAssertEqual(c.apiKey, "k")
        XCTAssertEqual(c.endpoint, "https://x/v1")
    }

    func testDefaultsModelPromptTimeout() {
        let c = LLMConfig.resolve(environment: [:])
        XCTAssertEqual(c.model, "gpt-4o-mini")
        XCTAssertEqual(c.timeoutMs, 15000)
        XCTAssertFalse(c.prompt.isEmpty)
        XCTAssertFalse(c.echo)
    }

    func testOverrides() {
        let c = LLMConfig.resolve(environment: [
            "NOSPACEKEY_LLM_MODEL": "m", "NOSPACEKEY_LLM_PROMPT": "P",
            "NOSPACEKEY_LLM_TIMEOUT_MS": "3000", "NOSPACEKEY_LLM_ECHO": "1",
        ])
        XCTAssertEqual(c.model, "m")
        XCTAssertEqual(c.prompt, "P")
        XCTAssertEqual(c.timeoutMs, 3000)
        XCTAssertTrue(c.echo)
    }
}
