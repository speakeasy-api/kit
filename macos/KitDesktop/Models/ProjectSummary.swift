import Foundation

/// A transient navigation snapshot derived from workspace and conversation metadata.
struct ProjectSummary: Identifiable {
    let workspace: Workspace
    let conversations: [Conversation]

    var id: UUID { workspace.id }
    var updatedAt: Date { conversations.first?.updatedAt ?? workspace.createdAt }
    var recentConversations: [Conversation] { Array(conversations.prefix(3)) }

    func matchesProject(_ query: String) -> Bool {
        let query = query.trimmingCharacters(in: .whitespacesAndNewlines)
        return query.isEmpty || workspace.name.localizedCaseInsensitiveContains(query)
            || workspace.path.localizedCaseInsensitiveContains(query)
    }

    func previewConversations(matching query: String) -> [Conversation] {
        guard !matchesProject(query) else { return recentConversations }
        let query = query.trimmingCharacters(in: .whitespacesAndNewlines)
        return Array(conversations.lazy.filter { $0.title.localizedCaseInsensitiveContains(query) }.prefix(3))
    }

    func matches(_ query: String) -> Bool {
        let query = query.trimmingCharacters(in: .whitespacesAndNewlines)
        return query.isEmpty || workspace.name.localizedCaseInsensitiveContains(query)
            || workspace.path.localizedCaseInsensitiveContains(query)
            || conversations.contains { $0.title.localizedCaseInsensitiveContains(query) }
    }
}
