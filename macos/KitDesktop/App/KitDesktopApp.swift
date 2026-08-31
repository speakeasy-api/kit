import SwiftUI

@main
struct KitDesktopApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var model = AppModel()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(model)
                .onAppear { appDelegate.shutdown = { completion in model.closeAll(completion: completion) } }
                .frame(minWidth: 980, minHeight: 640)
        }
        .windowStyle(.hiddenTitleBar)
        .commands {
            CommandGroup(after: .newItem) {
                Button("New Conversation") { model.createConversation() }
                    .keyboardShortcut("n", modifiers: [.command, .shift])
                    .disabled(model.selectedWorkspaceID == nil)
            }
        }
    }
}
