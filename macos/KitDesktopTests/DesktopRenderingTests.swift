import AppKit
import SwiftUI
import XCTest
@testable import Kit

final class DesktopRenderingTests: XCTestCase {
    @MainActor
    func testProjectsRenderInBothAppearances() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("kit-desktop-preview-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let store = PersistenceStore(fileURL: directory.appendingPathComponent("state.json"))
        let now = Date(timeIntervalSince1970: 1_791_150_000)
        let workspaces = [
            Workspace(name: "Kit", path: "/Projects/kit", createdAt: now),
            Workspace(name: "Marketing", path: "/Projects/marketing", createdAt: now),
            Workspace(name: "Examples", path: "/Projects/examples", createdAt: now),
        ]
        let conversations = [
            Conversation(workspaceID: workspaces[0].id, title: "Give the desktop its own identity", updatedAt: now),
            Conversation(workspaceID: workspaces[0].id, title: "Make long threads feel instant", updatedAt: now.addingTimeInterval(-300), unread: true),
            Conversation(workspaceID: workspaces[0].id, title: "Review the release notes", updatedAt: now.addingTimeInterval(-600)),
            Conversation(workspaceID: workspaces[1].id, title: "Document the project workflow", updatedAt: now.addingTimeInterval(-900)),
        ]
        try store.save(PersistedAppState(workspaces: workspaces, conversations: conversations))
        let model = AppModel(store: store, catalogLoader: nil, requestNotificationAuthorization: false)
        for (name, scheme) in [("light", ColorScheme.light), ("dark", ColorScheme.dark)] {
            let content = ProjectsView(projects: model.projects, addFolder: {})
                .environmentObject(model).environment(\.colorScheme, scheme)
                .frame(width: 980, height: 900)
            // ImageRenderer does not render AppKit-backed scrolling content.
            let hosting = NSHostingView(rootView: content)
            let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 980, height: 900),
                                  styleMask: [.borderless], backing: .buffered, defer: false)
            window.isReleasedWhenClosed = false
            window.contentView = hosting
            window.appearance = NSAppearance(named: scheme == .dark ? .darkAqua : .aqua)
            hosting.layoutSubtreeIfNeeded()
            hosting.displayIfNeeded()
            let bitmap = try XCTUnwrap(hosting.bitmapImageRepForCachingDisplay(in: hosting.bounds))
            hosting.cacheDisplay(in: hosting.bounds, to: bitmap)
            window.close()
            XCTAssertGreaterThan(bitmap.pixelsWide, 0)
            let png = try XCTUnwrap(bitmap.representation(using: .png, properties: [:]))
            let path = directory.appendingPathComponent("projects-\(name).png")
            try png.write(to: path)
            print("Desktop preview: \(path.path)")
        }
        store.flush()
    }
}
