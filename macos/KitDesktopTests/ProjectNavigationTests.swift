import XCTest
@testable import Kit

final class ProjectNavigationTests: XCTestCase {
    @MainActor
    func testProjectSnapshotsSortGroupSearchAndLimitPreviews() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = PersistenceStore(fileURL: directory.appendingPathComponent("state.json"))
        let older = Workspace(name: "Library", path: "/projects/library", createdAt: Date(timeIntervalSince1970: 1))
        let newer = Workspace(name: "Desktop", path: "/projects/desktop", createdAt: Date(timeIntervalSince1970: 2))
        let conversations = (0..<5).map {
            Conversation(workspaceID: older.id, title: "Task \($0)", updatedAt: Date(timeIntervalSince1970: Double($0 + 10)))
        }
        try store.save(PersistedAppState(workspaces: [newer, older], conversations: conversations))
        let model = AppModel(store: store, catalogLoader: nil, requestNotificationAuthorization: false)

        let projects = model.projects
        XCTAssertEqual(projects.map(\.id), [older.id, newer.id])
        XCTAssertEqual(projects[0].recentConversations.map(\.title), ["Task 4", "Task 3", "Task 2"])
        XCTAssertEqual(projects[0].conversations.count, 5)
        XCTAssertTrue(projects[1].recentConversations.isEmpty)
        XCTAssertTrue(projects[0].matches(" LIBRARY "))
        XCTAssertTrue(projects[0].matches("/projects/lib"))
        XCTAssertTrue(projects[0].matches("task 0"))
        XCTAssertFalse(projects[1].matches("task 0"))
    }

    @MainActor
    func testProjectsPageDoesNotMarkHiddenConversationRead() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = PersistenceStore(fileURL: directory.appendingPathComponent("state.json"))
        let workspace = Workspace(name: "Project", path: directory.path)
        let conversation = Conversation(workspaceID: workspace.id, unread: true, awaitingUser: true)
        try store.save(PersistedAppState(workspaces: [workspace], conversations: [conversation]))
        let model = AppModel(store: store, catalogLoader: nil, requestNotificationAuthorization: false)
        model.selectedConversationID = conversation.id
        model.showProjects()
        model.appBecameActive()
        XCTAssertTrue(model.showingProjects)
        XCTAssertTrue(try XCTUnwrap(model.state.conversations.first).unread)
        XCTAssertEqual(model.selectedConversationID, conversation.id)
        model.selectWorkspace(workspace.id)
        XCTAssertFalse(model.showingProjects)
        model.appBecameActive()
        XCTAssertFalse(try XCTUnwrap(model.state.conversations.first).unread)
        store.flush()
    }

    @MainActor
    func testCrossProjectNavigationKeepsControllerAndDraft() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let store = PersistenceStore(fileURL: directory.appendingPathComponent("state.json"))
        let first = Workspace(name: "First", path: root.path)
        let second = Workspace(name: "Second", path: root.path)
        let conversation = Conversation(workspaceID: second.id)
        try store.save(PersistedAppState(workspaces: [first, second], conversations: [conversation]))
        let launch = ACPClient.LaunchOverride(
            executable: URL(fileURLWithPath: "/usr/bin/python3"),
            prefixArguments: [root.appendingPathComponent("fixtures/mock-acp-v2.py").path, "--models"]
        )
        let model = AppModel(store: store, catalogLoader: nil, controllerFactory: { conversation, path in
            ConversationController(conversation: conversation, workspacePath: path,
                                   client: ACPClient(launchOverride: launch, requestTimeout: 2, promptTimeout: 2))
        }, requestNotificationAuthorization: false)
        model.selectConversation(conversation.id)
        XCTAssertEqual(model.selectedWorkspaceID, second.id)
        let controller = try XCTUnwrap(model.selectedController)
        controller.draft = "Keep this thought"
        model.showProjects()
        model.selectWorkspace(first.id)
        model.selectConversation(conversation.id)
        XCTAssertTrue(model.selectedController === controller)
        XCTAssertEqual(model.selectedController?.draft, "Keep this thought")
        XCTAssertEqual(model.selectedWorkspaceID, second.id)
        XCTAssertFalse(model.showingProjects)
        model.createConversation(in: first.id)
        XCTAssertEqual(model.selectedWorkspaceID, first.id)
        XCTAssertEqual(model.state.conversations.count, 2)
        XCTAssertEqual(model.state.conversations.last?.workspaceID, first.id)
        XCTAssertTrue(model.controllers[conversation.id] === controller)
        XCTAssertEqual(controller.draft, "Keep this thought")
        let closed = expectation(description: "controllers closed")
        model.closeAll { closed.fulfill() }
        await fulfillment(of: [closed], timeout: 4)
    }
}
