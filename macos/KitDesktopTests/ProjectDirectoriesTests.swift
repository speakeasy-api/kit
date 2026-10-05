import Foundation
import XCTest
@testable import Kit

final class ProjectDirectoriesTests: XCTestCase {
    private func load(version: Int?, directories: Any?, extra: [String: Any] = [:]) throws -> PersistedAppState {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: directory) }
        var workspace: [String: Any] = ["id": UUID().uuidString, "name": "Kit", "path": "/tmp/kit", "createdAt": "2025-01-01T00:00:00Z"]
        workspace.merge(extra) { _, new in new }
        if let directories { workspace["additionalDirectories"] = directories }
        var state: [String: Any] = ["workspaces": [workspace], "conversations": []]
        if let version { state["schemaVersion"] = version }
        let url = directory.appendingPathComponent("state.json")
        try JSONSerialization.data(withJSONObject: state).write(to: url)
        return try PersistenceStore(fileURL: url).load()
    }

    func testLegacyAndCurrentAbsentDirectoriesDefaultEmpty() throws {
        for version in [nil, 1, 2, 3] as [Int?] {
            XCTAssertEqual(try load(version: version, directories: nil).workspaces[0].additionalDirectories, [])
        }
    }

    func testExplicitDirectoriesSurviveLegacyAndMixedFields() throws {
        for version in [nil, 1, 2, 3] as [Int?] {
            let state = try load(version: version, directories: ["/tmp/extra"], extra: ["directories": ["/tmp/old"]])
            XCTAssertEqual(state.workspaces[0].additionalDirectories, ["/tmp/extra"])
        }
    }

    func testMalformedDirectoriesAreNotTreatedAsLegacy() {
        for malformed: Any in [NSNull(), "bad", 42, ["/tmp", 42] as [Any], ["path": "/tmp"]] {
            XCTAssertThrowsError(try load(version: 3, directories: malformed))
        }
    }

    func testWriterEmitsDirectoriesAndRoundTrips() throws {
        let state = try load(version: 3, directories: ["/tmp/extra"])
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        addTeardownBlock { try? FileManager.default.removeItem(at: directory) }
        let store = PersistenceStore(fileURL: directory.appendingPathComponent("state.json"))
        try store.save(state)
        XCTAssertEqual(try store.load(), state)
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: store.fileURL)) as? [String: Any])
        let workspaces = try XCTUnwrap(object["workspaces"] as? [[String: Any]])
        XCTAssertEqual(workspaces[0]["additionalDirectories"] as? [String], ["/tmp/extra"])
        XCTAssertEqual(object["schemaVersion"] as? Int, 3)
    }
}
