import AppKit
import SwiftUI

struct ProjectDirectoriesView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    let workspaceID: UUID

    private var workspace: Workspace? { model.state.workspaces.first { $0.id == workspaceID } }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                Text("Project Directories").font(.title2)
                Spacer()
                Button("Done") { dismiss() }.keyboardShortcut(.cancelAction)
            }
            if let workspace {
                Text(workspace.name).font(.headline)
                Text("Root: \(workspace.path)").textSelection(.enabled)
                Text("Additional directories are sent when a conversation next starts or resumes. Already connected sessions are unchanged.")
                    .font(.callout).foregroundStyle(.secondary)
                List {
                    ForEach(workspace.additionalDirectories, id: \.self) { directory in
                        HStack {
                            Text(directory).textSelection(.enabled)
                            Spacer()
                            Button { model.removeProjectDirectory(directory, from: workspaceID) } label: {
                                Image(systemName: "minus.circle")
                            }.accessibilityLabel("Remove \(directory)")
                        }
                    }
                }
                Button("Add Directory…") {
                    let panel = NSOpenPanel()
                    panel.canChooseDirectories = true
                    panel.canChooseFiles = false
                    panel.allowsMultipleSelection = true
                    panel.prompt = "Add"
                    if panel.runModal() == .OK {
                        for url in panel.urls { model.addProjectDirectory(url.path, to: workspaceID) }
                    }
                }
            } else { Text("This project is no longer available.") }
        }.padding(24).frame(width: 620, height: 400)
    }
}
