import XCTest
@testable import Kit

final class ProjectSearchTests: XCTestCase {
    func testTitleSearchPreviewsOlderMatchesInsteadOfUnrelatedRecentConversations() {
        let workspace = Workspace(name: "Kit", path: "/projects/kit")
        let conversations = (0..<7).map { index in
            Conversation(workspaceID: workspace.id, title: index < 3 ? "Recent \(index)" : "Needle \(index)")
        }
        let project = ProjectSummary(workspace: workspace, conversations: conversations)
        XCTAssertTrue(project.matches("needle"))
        XCTAssertEqual(project.previewConversations(matching: " NEEDLE ").map(\.id), Array(conversations[3...5]).map(\.id))
        XCTAssertEqual(project.previewConversations(matching: "kit").map(\.id), Array(conversations.prefix(3)).map(\.id))
        XCTAssertEqual(project.previewConversations(matching: "").map(\.id), Array(conversations.prefix(3)).map(\.id))
        XCTAssertTrue(project.previewConversations(matching: "missing").isEmpty)
    }
}
