import SwiftUI

struct PendingSteerEditor: View {
    @ObservedObject var controller: ConversationController
    let id: String
    @State private var text: String
    @State private var submitted: String?
    @Environment(\.dismiss) private var dismiss

    init(controller: ConversationController, item: ConversationController.PendingSteer) {
        self.controller = controller
        id = item.id
        _text = State(initialValue: item.text)
    }

    private var current: ConversationController.PendingSteer? { controller.pendingSteers.first { $0.id == id } }
    private var busy: Bool { controller.isMutatingSteerIDs.contains(id) }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Edit queued message").font(.headline)
            Text("Update the message before Kit receives it. Your current composer draft stays unchanged.")
                .font(.callout).foregroundStyle(.secondary)
            TextEditor(text: $text).font(.body).frame(minHeight: 140)
                .disabled(busy).accessibilityLabel("Queued message")
            if submitted != nil && !busy { Text(controller.status).font(.caption).foregroundStyle(.secondary).textSelection(.enabled) }
            HStack {
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Spacer()
                if busy { ProgressView().controlSize(.small) }
                Button("Save") {
                    submitted = text
                    controller.replacePendingSteer(id: id, text: text)
                }
                .buttonStyle(.borderedProminent).keyboardShortcut(.return, modifiers: .command)
                .disabled(busy || !controller.isReady || !controller.supportsPendingSteerEdit || current == nil
                          || text == current?.text || text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }.padding(24).frame(width: 500)
            .onChange(of: controller.pendingSteers, initial: true) { _, _ in
                guard let current else { dismiss(); return }
                if let submitted, current.text == submitted { dismiss() }
            }
    }
}
