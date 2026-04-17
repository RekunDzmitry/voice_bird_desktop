import XCTest
@testable import VoiceBirdWhisperKit

final class ProtocolTests: XCTestCase {
    func testOutEventEncodes() throws {
        let e = OutEvent(type: "ready", model: "tiny.en", t0: nil, t1: nil, text: nil, message: nil)
        let data = try JSONEncoder().encode(e)
        let s = String(data: data, encoding: .utf8)!
        XCTAssertTrue(s.contains("\"type\":\"ready\""))
        XCTAssertTrue(s.contains("\"model\":\"tiny.en\""))
    }
}
