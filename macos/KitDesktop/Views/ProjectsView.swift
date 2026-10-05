import SwiftUI

struct ProjectsView: View {
    @EnvironmentObject private var model: AppModel
    let projects: [ProjectSummary]
    let addFolder: () -> Void
    @State private var query = ""
    @State private var directoryWorkspace: Workspace?
    @State private var showingProviderUsage = false

    private var filteredProjects: [ProjectSummary] { projects.filter { $0.matches(query) } }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                HStack(alignment: .top) {
                    VStack(alignment: .leading, spacing: 7) {
                        Text("YOUR WORKSPACE").brandMicroLabel().foregroundStyle(Brand.ember)
                        Text("Projects").brandDisplay(36)
                        Text("Pick up where you left off.").foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("Provider Usage") { showingProviderUsage = true }
                    Button(action: addFolder) { Label("Add Folder", systemImage: "folder.badge.plus") }
                        .buttonStyle(.borderedProminent).pointingHandCursor()
                }
                HStack {
                    Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                    TextField("Search projects and conversations", text: $query).textFieldStyle(.plain)
                    if !query.isEmpty {
                        Button { query = "" } label: { Image(systemName: "xmark.circle.fill") }
                            .buttonStyle(.plain).accessibilityLabel("Clear search")
                    }
                }.padding(12).background(Brand.paper, in: RoundedRectangle(cornerRadius: 8))
                HStack {
                    Text("\(filteredProjects.count) PROJECTS").brandMicroLabel()
                    Spacer()
                    Label("Recently updated", systemImage: "clock").font(.caption)
                }.foregroundStyle(.secondary)
                if filteredProjects.isEmpty {
                    ContentUnavailableView(
                        projects.isEmpty ? "Make room for your next idea" : "No matching projects",
                        systemImage: projects.isEmpty ? "folder.badge.plus" : "magnifyingglass",
                        description: Text(projects.isEmpty ? "Add a folder to start a conversation with Kit." : "Try another project name, path, or conversation title.")
                    )
                }
                LazyVStack(spacing: 16) {
                    ForEach(filteredProjects) { project in
                        projectCard(project)
                    }
                }
            }.padding(32).frame(maxWidth: 1000).frame(maxWidth: .infinity)
        }.background(Brand.canvas)
            .sheet(item: $directoryWorkspace) { workspace in
                ProjectDirectoriesView(workspaceID: workspace.id).environmentObject(model)
            }
            .sheet(isPresented: $showingProviderUsage) { ProviderUsageView() }
    }

    private func projectCard(_ project: ProjectSummary) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 12) {
                Image(systemName: "folder.fill").font(.title2).foregroundStyle(Brand.ember)
                Button { model.selectWorkspace(project.id) } label: {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(project.workspace.name).font(.headline)
                        Text(project.workspace.path).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                    }.contentShape(Rectangle())
                }.buttonStyle(.plain).pointingHandCursor()
                Spacer()
                Button("Directories…") { directoryWorkspace = project.workspace }
                Button { model.createConversation(in: project.id) } label: {
                    Label("New Conversation", systemImage: "square.and.pencil")
                }.pointingHandCursor()
            }
            Divider()
            if project.conversations.isEmpty {
                Text("No conversations yet. Start something new.").font(.callout).foregroundStyle(.secondary).padding(.vertical, 8)
            } else {
                if !project.matchesProject(query) {
                    Text("Matching conversations").font(.caption).foregroundStyle(.secondary)
                }
                ForEach(project.previewConversations(matching: query)) { conversation in
                    Button { model.selectConversation(conversation.id) } label: {
                        HStack(spacing: 10) {
                            ProjectConversationStatus(conversation: conversation, running: model.activity[conversation.id] == true)
                            Text(conversation.title).lineLimit(1)
                            Spacer()
                            if model.lockedConversationIDs.contains(conversation.id) {
                                Image(systemName: "lock.fill").foregroundStyle(.secondary)
                            }
                            Text(conversation.updatedAt, style: .relative).font(.caption).foregroundStyle(.secondary)
                            Image(systemName: "chevron.right").font(.caption2).foregroundStyle(.tertiary)
                        }.padding(.vertical, 4).contentShape(Rectangle())
                    }.buttonStyle(.plain).pointingHandCursor()
                }
            }
            HStack {
                Text("\(project.conversations.count) conversations").font(.caption).foregroundStyle(.secondary)
                Spacer()
                Text(project.updatedAt, style: .date).font(.caption).foregroundStyle(.tertiary)
            }
        }.padding(20).background(Brand.paper, in: RoundedRectangle(cornerRadius: 12))
            .overlay { RoundedRectangle(cornerRadius: 12).stroke(Brand.hairline) }
    }
}

struct ProjectConversationStatus: View {
    let conversation: Conversation
    let running: Bool

    var body: some View {
        Group {
            if running {
                ProgressView().controlSize(.mini).accessibilityLabel("Running")
            } else if conversation.unread || conversation.awaitingUser {
                Circle().fill(Brand.ember).frame(width: 7, height: 7).accessibilityLabel("Unread")
            } else {
                Image(systemName: "bubble.left").font(.caption).foregroundStyle(.tertiary)
            }
        }.frame(width: 16)
    }
}
