import Combine
import XCTest
@testable import Kit

final class ProjectNavigationLifecycleTests: XCTestCase {
    @MainActor
    func testBackgroundSessionReadinessDoesNotDismissProjects() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let release = directory.appendingPathComponent("release")
        let workspace = Workspace(name: "Project", path: directory.path)
        let conversation = Conversation(workspaceID: workspace.id)
        let store = PersistenceStore(fileURL: directory.appendingPathComponent("state.json"))
        try store.save(PersistedAppState(workspaces: [workspace], conversations: [conversation]))
        let model = AppModel(store: store, catalogLoader: nil, controllerFactory: { conversation, path in
            let client = ACPClient(launchOverride: ACPClient.LaunchOverride(
                executable: URL(fileURLWithPath: "/usr/bin/python3"),
                prefixArguments: [root.appendingPathComponent("fixtures/mock-acp-v2.py").path, "--models", "--new-release=" + release.path]
            ), requestTimeout: 3, promptTimeout: 3)
            return ConversationController(conversation: conversation, workspacePath: path, client: client)
        }, requestNotificationAuthorization: false)
        model.selectConversation(conversation.id)
        let controller = try XCTUnwrap(model.controllers[conversation.id])
        let ready = expectation(description: "background session ready")
        let subscription = controller.$isReady.filter { $0 }.prefix(1).sink { _ in ready.fulfill() }
        defer { subscription.cancel() }
        model.showProjects()
        try Data().write(to: release)
        await fulfillment(of: [ready], timeout: 5)
        XCTAssertTrue(model.showingProjects)
        XCTAssertEqual(model.selectedConversationID, conversation.id)
        let closed = expectation(description: "background session closed")
        model.closeAll { closed.fulfill() }
        await fulfillment(of: [closed], timeout: 4)
    }
}
