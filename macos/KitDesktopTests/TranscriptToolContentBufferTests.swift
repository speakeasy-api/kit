import XCTest
@testable import Kit

final class TranscriptToolContentBufferTests: XCTestCase {
    func testSmallChunksAndExistingSnapshotArePreserved() {
        let value = TranscriptToolContentBuffer.append(.string("next"), to: .array([.string("first")]))
        XCTAssertEqual(value, .array([.string("first"), .string("next")]))
    }

    func testLongToolStreamIsBoundedAndMarksOmittedOutput() throws {
        var value: JSONValue?
        for index in 0..<1000 {
            value = TranscriptToolContentBuffer.append(.string("\(index):" + String(repeating: "x", count: 4096)), to: value)
        }
        let retained = try XCTUnwrap(value)
        XCTAssertLessThanOrEqual(try JSONEncoder().encode(retained).count, TranscriptToolContentBuffer.maximumBytes)
        guard case .array(let chunks) = retained else { return XCTFail("Expected tool chunks") }
        XCTAssertLessThanOrEqual(chunks.count, TranscriptToolContentBuffer.maximumChunks)
        XCTAssertEqual(chunks.first?.objectValue?["_kitDesktopRetention"], .bool(true))
        XCTAssertTrue(chunks.last?.stringValue?.hasPrefix("999:") == true)
    }

    func testOversizedNestedChunkRetainsAnExplicitPreview() throws {
        let original = JSONValue.object(["output": .string(String(repeating: "x", count: 512 * 1024))])
        let value = TranscriptToolContentBuffer.append(original, to: nil)
        XCTAssertLessThanOrEqual(try JSONEncoder().encode(value).count, TranscriptToolContentBuffer.maximumBytes)
        guard case .array(let chunks) = value else { return XCTFail("Expected tool chunks") }
        XCTAssertEqual(chunks.last?.objectValue?["truncated"], .bool(true))
        XCTAssertNotNil(chunks.last?.objectValue?["preview"]?.stringValue)
    }
}
