import SwiftUI

struct FocusedSubagentView: View {
    @ObservedObject var parent: ConversationController
    let childID: String
    @StateObject private var replay: FocusedSubagentController
    @Environment(\.dismiss) private var dismiss
    @State private var follow = true

    init(parent: ConversationController, childID: String) {
        self.parent = parent
        self.childID = childID
        _replay = StateObject(wrappedValue: FocusedSubagentController(transport: parent.subagentClient))
    }

    private var row: AgentRosterRow? { parent.agentRoster.rowsByID[childID] }
    private var canSteer: Bool {
        guard let row, parent.isReady else { return false }
        return parent.canSteerSubagent(id: row.id, generation: row.generation)
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                VStack(alignment: .leading, spacing: 3) {
                    Text(row?.name ?? "Subagent").font(.headline)
                    Text(row?.model.map { "\(row?.harness ?? "") · \($0)" } ?? row?.harness ?? "Closed child")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
                Toggle("Follow output", isOn: $follow).toggleStyle(.checkbox).font(.caption)
                Button("Done") { dismiss() }.keyboardShortcut(.cancelAction)
            }.padding(16)
            Divider()
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 16) {
                        ForEach(replay.entries) { entry in
                            VStack(alignment: .leading, spacing: 6) {
                                Text(entry.title).brandMicroLabel().foregroundStyle(.secondary)
                                Text(entry.text).font(.body).textSelection(.enabled)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                            }.id(entry.id)
                        }
                        Color.clear.frame(height: 1).id("child-bottom")
                    }.padding(20)
                }
                .onChange(of: replay.entries) { _, _ in
                    if follow { proxy.scrollTo("child-bottom", anchor: .bottom) }
                }
            }
            Divider()
            VStack(alignment: .leading, spacing: 8) {
                if replay.partial {
                    Label("Partial transcript: unsupported content or older retained output was omitted.", systemImage: "exclamationmark.triangle")
                        .font(.caption).foregroundStyle(Brand.ember)
                }
                HStack(spacing: 8) {
                    if replay.loading || replay.steering { ProgressView().controlSize(.mini) }
                    Text(replay.notice).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                }
                HStack(alignment: .bottom, spacing: 10) {
                    TextField("Steer this child with a message", text: $replay.draft, axis: .vertical)
                        .textFieldStyle(.roundedBorder).lineLimit(1...5)
                        .disabled(!replay.canSteer || replay.steering)
                    Button("Send", action: replay.send).buttonStyle(.borderedProminent)
                        .keyboardShortcut(.return, modifiers: .command)
                        .disabled(!replay.canSteer || replay.steering || replay.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || replay.draft.utf8.count > 16 * 1024)
                }
                if replay.draft.utf8.count > 16 * 1024 {
                    Text("Child steering messages must be no larger than 16 KiB.").font(.caption).foregroundStyle(Brand.vermilion)
                }
            }.padding(16)
        }
        .frame(minWidth: 560, idealWidth: 740, minHeight: 420, idealHeight: 640)
        .background(Brand.canvas)
        .onAppear { if let row { replay.focus(row, canSteer: canSteer) } }
        .onChange(of: row) { _, value in replay.update(value, canSteer: canSteer) }
        .onChange(of: canSteer) { _, value in replay.update(row, canSteer: value) }
        .onChange(of: parent.isReady) { _, ready in
            if !ready { replay.disconnected(); dismiss() }
        }
        .onChange(of: ObjectIdentifier(parent.subagentClient)) { _, _ in
            replay.disconnected()
            dismiss()
        }
        .onDisappear { replay.stop() }
    }
}
