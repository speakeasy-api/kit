import AppKit

final class AppDelegate: NSObject, NSApplicationDelegate {
    var shutdown: (((@escaping () -> Void) -> Void))?
    private var replySent = false

    func applicationWillFinishLaunching(_ notification: Notification) {
        guard let iconURL = Bundle.main.url(forResource: "AppIcon", withExtension: "icns"),
              let icon = NSImage(contentsOf: iconURL) else { return }
        NSApplication.shared.applicationIconImage = icon
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard let shutdown else { return .terminateNow }
        replySent = false
        let finish = { [weak self] in
            guard let self, !self.replySent else { return }
            self.replySent = true
            sender.reply(toApplicationShouldTerminate: true)
        }
        shutdown(finish)
        DispatchQueue.main.asyncAfter(deadline: .now() + 10, execute: finish)
        return .terminateLater
    }
}
