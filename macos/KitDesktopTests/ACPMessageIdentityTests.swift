import XCTest
@testable import Kit

final class ACPMessageIdentityTests: XCTestCase {
    @MainActor
    func testWireMessageIDsAreScopedByRoleDuringReplay() async throws {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let client = ACPClient(launchOverride: ACPClient.LaunchOverride(
            executable: URL(fileURLWithPath: "/usr/bin/python3"),
            prefixArguments: [root.appendingPathComponent("fixtures/mock-acp-v2.py").path, "--models", "--replay-shared-ids"]
        ), requestTimeout: 2, promptTimeout: 2)
        let controller = ConversationController(
            conversation: Conversation(workspaceID: UUID(), sessionID: "shared-session"),
            workspacePath: root.path, client: client
        )
        let ready = expectation(description: "shared identities replayed")
        controller.onSessionReady = { _, _ in ready.fulfill() }
        controller.start()
        await fulfillment(of: [ready], timeout: 4)
        XCTAssertEqual(controller.entries.map(\.role), [.user, .thought, .assistant])
        XCTAssertEqual(controller.transcriptProjection.items.map(\.isActivity), [false, true, false])
        XCTAssertEqual(controller.entries.map(\.text), ["text", "text", "answer"])
        let closed = expectation(description: "shared identities helper closed")
        controller.close { closed.fulfill() }
        await fulfillment(of: [closed], timeout: 4)
    }
}
