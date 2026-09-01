import SwiftUI

@main
struct KitDesktopApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var model = AppModel()
    @AppStorage("interfaceZoomLevel") private var interfaceZoomLevel = 3

    private static let zoomSizes: [DynamicTypeSize] = [
        .xSmall, .small, .medium, .large, .xLarge, .xxLarge, .xxxLarge,
    ]
    private static let defaultZoomLevel = 3

    private var boundedZoomLevel: Int {
        min(max(interfaceZoomLevel, 0), Self.zoomSizes.count - 1)
    }

    private var interfaceSize: DynamicTypeSize {
        Self.zoomSizes[boundedZoomLevel]
    }

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(model)
                .environment(\.dynamicTypeSize, interfaceSize)
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
            CommandGroup(after: .toolbar) {
                Divider()
                Button("Zoom In") {
                    interfaceZoomLevel = min(boundedZoomLevel + 1, Self.zoomSizes.count - 1)
                }
                .keyboardShortcut("+", modifiers: .command)
                .disabled(boundedZoomLevel >= Self.zoomSizes.count - 1)

                Button("Zoom Out") {
                    interfaceZoomLevel = max(boundedZoomLevel - 1, 0)
                }
                .keyboardShortcut("-", modifiers: .command)
                .disabled(boundedZoomLevel <= 0)

                Button("Actual Size") { interfaceZoomLevel = Self.defaultZoomLevel }
                    .keyboardShortcut("0", modifiers: .command)
                    .disabled(interfaceZoomLevel == Self.defaultZoomLevel)
            }
        }
    }
}
