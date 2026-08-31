import Foundation
import XCTest
@testable import Kit

final class PersistenceStoreTests: XCTestCase {
    func testMissingFileLoadsEmptyState() throws {
        let url = temporaryDirectory().appendingPathComponent("state.json")
        XCTAssertEqual(try PersistenceStore(fileURL: url).load(), PersistedAppState())
    }

    func testRoundTripsWorkspaceScopedConversations() throws {
        let url = temporaryDirectory().appendingPathComponent("nested/state.json")
        let workspace = Workspace(id: UUID(), name: "Kit", path: "/tmp/kit", createdAt: Date(timeIntervalSince1970: 10))
        let conversation = Conversation(
            id: UUID(), workspaceID: workspace.id, title: "Continue me", sessionID: "session-42",
            createdAt: Date(timeIntervalSince1970: 20), updatedAt: Date(timeIntervalSince1970: 30),
            unread: true, awaitingUser: true, usesConfiguredDefaults: false
        )
        let expected = PersistedAppState(workspaces: [workspace], conversations: [conversation])
        let store = PersistenceStore(fileURL: url)

        try store.save(expected)

        XCTAssertEqual(try store.load(), expected)
    }

    func testMigratesVersionlessV1StateWithConversationDefaults() throws {
        let directory = temporaryDirectory()
        let url = directory.appendingPathComponent("state.json")
        let workspaceID = UUID()
        let conversationID = UUID()
        let json = """
        {"workspaces":[{"id":"\(workspaceID)","name":"Kit","path":"/tmp/kit","createdAt":"1970-01-01T00:00:10Z"}],"conversations":[{"id":"\(conversationID)","workspaceID":"\(workspaceID)","title":"Old","createdAt":"1970-01-01T00:00:20Z","updatedAt":"1970-01-01T00:00:30Z"}]}
        """
        let original = Data(json.utf8)
        try original.write(to: url)

        let loaded = try PersistenceStore(fileURL: url).load()

        XCTAssertEqual(try Data(contentsOf: url), original)
        XCTAssertEqual(loaded.schemaVersion, PersistedAppState.currentSchemaVersion)
        XCTAssertEqual(loaded.conversations.first?.provider, "openai-subscription")
        XCTAssertEqual(loaded.conversations.first?.reasoningEffort, "default")
        XCTAssertEqual(loaded.conversations.first?.usesConfiguredDefaults, true)
    }

    func testMigratesV2HardCodedFallbacksToConfiguredDefaults() throws {
        let url = temporaryDirectory().appendingPathComponent("state.json")
        let workspaceID = UUID()
        let conversationID = UUID()
        let json = """
        {"schemaVersion":2,"workspaces":[{"id":"\(workspaceID)","name":"Kit","path":"/tmp/kit","createdAt":"1970-01-01T00:00:10Z"}],"conversations":[{"id":"\(conversationID)","workspaceID":"\(workspaceID)","title":"Old","createdAt":"1970-01-01T00:00:20Z","updatedAt":"1970-01-01T00:00:30Z","provider":"openai-subscription","model":"gpt-5.4","reasoningEffort":"default"}]}
        """
        try Data(json.utf8).write(to: url)

        let loaded = try PersistenceStore(fileURL: url).load()

        XCTAssertEqual(loaded.schemaVersion, 3)
        XCTAssertEqual(loaded.conversations.first?.usesConfiguredDefaults, true)
    }

    func testMigrationPreservesCurrentValueWhenLegacyAndCurrentKeysAreMixed() throws {
        let url = temporaryDirectory().appendingPathComponent("state.json")
        let workspaceID = UUID()
        let conversationID = UUID()
        let json = """
        {"schemaVersion":2,"workspaces":[{"id":"\(workspaceID)","name":"Kit","path":"/tmp/kit","createdAt":"1970-01-01T00:00:10Z"}],"conversations":[{"id":"\(conversationID)","workspaceID":"\(workspaceID)","title":"Explicit","createdAt":"1970-01-01T00:00:20Z","updatedAt":"1970-01-01T00:00:30Z","unread":false,"awaitingUser":false,"provider":"custom","model":"custom-model","reasoningEffort":"high","usesConfiguredDefaults":false}]}
        """
        try Data(json.utf8).write(to: url)

        let loaded = try PersistenceStore(fileURL: url).load()

        XCTAssertEqual(loaded.conversations.first?.usesConfiguredDefaults, false)
        XCTAssertEqual(loaded.conversations.first?.provider, "custom")
    }

    func testMalformedCurrentStateIsRejectedWithoutChangingPrimaryBytes() throws {
        let directory = temporaryDirectory()
        let url = directory.appendingPathComponent("state.json")
        let original = Data("{\"schemaVersion\":3,\"workspaces\":[],\"conversations\":[{\"id\":\"missing-fields\"}]}".utf8)
        try original.write(to: url)

        XCTAssertThrowsError(try PersistenceStore(fileURL: url).load())
        XCTAssertEqual(try Data(contentsOf: url), original)
    }

    func testWriterEmitsOnlyTheStrictCurrentShape() throws {
        let url = temporaryDirectory().appendingPathComponent("state.json")
        let workspace = Workspace(name: "Kit", path: "/tmp/kit")
        let conversation = Conversation(workspaceID: workspace.id)

        try PersistenceStore(fileURL: url).save(PersistedAppState(workspaces: [workspace], conversations: [conversation]))

        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        XCTAssertEqual(object["schemaVersion"] as? Int, PersistedAppState.currentSchemaVersion)
        let conversations = try XCTUnwrap(object["conversations"] as? [[String: Any]])
        XCTAssertEqual(conversations.count, 1)
        XCTAssertEqual(conversations[0]["usesConfiguredDefaults"] as? Bool, true)
        XCTAssertNotNil(conversations[0]["provider"] as? String)
        XCTAssertNotNil(conversations[0]["reasoningEffort"] as? String)
    }

    func testQuarantinesCorruptPrimaryAndRecoversBackup() throws {
        let directory = temporaryDirectory()
        let url = directory.appendingPathComponent("state.json")
        let store = PersistenceStore(fileURL: url)
        let first = PersistedAppState(workspaces: [Workspace(name: "First", path: "/first")])
        let second = PersistedAppState(workspaces: [Workspace(name: "Second", path: "/second")])
        try store.save(first)
        try store.save(second)
        try Data("not-json".utf8).write(to: url)

        let recovered = try store.load()

        XCTAssertEqual(recovered.workspaces.first?.name, "First")
        let files = try FileManager.default.contentsOfDirectory(atPath: directory.path)
        XCTAssertTrue(files.contains { $0.hasPrefix("state.corrupt-") })
    }

    func testLoadsBackupWhenPrimaryIsMissing() throws {
        let directory = temporaryDirectory()
        let url = directory.appendingPathComponent("state.json")
        let store = PersistenceStore(fileURL: url)
        let backup = PersistedAppState(workspaces: [Workspace(name: "Backup", path: "/backup")])
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        try encoder.encode(backup).write(to: store.backupURL)

        let loaded = try store.load()

        XCTAssertEqual(loaded.workspaces.first?.name, "Backup")
        XCTAssertFalse(FileManager.default.fileExists(atPath: url.path))
    }

    func testUnsupportedSchemaFromBackupIsPropagated() throws {
        let directory = temporaryDirectory()
        let url = directory.appendingPathComponent("state.json")
        let store = PersistenceStore(fileURL: url)
        try Data("{\"schemaVersion\":99,\"workspaces\":[],\"conversations\":[]}".utf8)
            .write(to: store.backupURL)

        XCTAssertThrowsError(try store.load()) { error in
            XCTAssertEqual(error as? PersistenceError, .unsupportedSchema(99))
        }
    }

    func testNewerSchemaIsPreservedAndRejected() throws {
        let url = temporaryDirectory().appendingPathComponent("state.json")
        let original = Data("{\"schemaVersion\":99,\"workspaces\":[],\"conversations\":[]}".utf8)
        try original.write(to: url)
        let store = PersistenceStore(fileURL: url)

        XCTAssertThrowsError(try store.load())
        XCTAssertThrowsError(try store.save(PersistedAppState()))
        XCTAssertEqual(try Data(contentsOf: url), original)
    }

    private func temporaryDirectory() -> URL {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }
}
