import Foundation
import XCTest
@testable import Kit

final class JSONLineParserTests: XCTestCase {
    func testEmitsOnlyCompleteFragmentedLines() throws {
        var parser = JSONLineParser()
        XCTAssertTrue(try parser.append(Data(#"{"jsonrpc":"2.0","id":1"#.utf8)).isEmpty)

        let lines = try parser.append(Data("}\r\n{\"jsonrpc\":\"2.0\",\"id\":2}\n".utf8))

        XCTAssertEqual(lines.count, 2)
        XCTAssertEqual(try RPCEnvelope.parse(lines[0]).id, .integer(1))
        XCTAssertEqual(try RPCEnvelope.parse(lines[1]).id, .integer(2))
    }

    func testRejectsAnUnboundedPartialLine() throws {
        var parser = JSONLineParser(maximumLineBytes: 8)
        XCTAssertThrowsError(try parser.append(Data(repeating: 0x61, count: 9)))
    }

    func testPreservesTrailingLineUntilFinish() throws {
        var parser = JSONLineParser()
        XCTAssertTrue(try parser.append(Data(#"{"jsonrpc":"2.0","method":"session/update"}"#.utf8)).isEmpty)
        let tail = try XCTUnwrap(parser.finish())
        XCTAssertEqual(try RPCEnvelope.parse(tail).method, "session/update")
        XCTAssertNil(try parser.finish())
    }
}
