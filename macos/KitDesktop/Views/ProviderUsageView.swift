import SwiftUI

struct ProviderUsageView: View {
    @Environment(\.dismiss) private var dismiss
    @State private var report: String?
    @State private var error: String?
    @State private var loading = true
    @State private var refreshID = UUID()

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                Text("Provider & Account Usage").font(.title2)
                Spacer()
                Button("Done") { dismiss() }.keyboardShortcut(.cancelAction)
            }
            Text("Account quotas reported by authenticated providers, separate from this conversation’s token usage.")
                .foregroundStyle(.secondary)
            if loading { ProgressView("Fetching usage…") }
            if let error { Text(error).foregroundStyle(.red).textSelection(.enabled) }
            if let report {
                ScrollView { Text(report).font(.system(.body, design: .monospaced)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading) }
            }
            Spacer(minLength: 0)
            Button("Refresh") { refreshID = UUID() }.disabled(loading)
        }
        .padding(24).frame(width: 620, height: 440)
        .task(id: refreshID) {
            loading = true
            error = nil
            do {
                let result = try await ProviderUsageService().fetch()
                guard !Task.isCancelled else { return }
                report = result
            } catch {
                guard !Task.isCancelled else { return }
                self.error = error.localizedDescription
            }
            loading = false
        }
    }
}
