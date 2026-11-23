import AppKit

final class AppDelegate: NSObject, NSApplicationDelegate {
    var shutdown: (((@escaping () -> Void) -> Void))?
    private var replySent = false
    private var playbackCleanup: Task<Void, Never>?

    func applicationWillFinishLaunching(_ notification: Notification) {
        guard let iconURL = Bundle.main.url(forResource: "AppIcon", withExtension: "icns"),
              let icon = NSImage(contentsOf: iconURL) else { return }
        NSApplication.shared.applicationIconImage = icon
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Sweep files left by earlier launches and expire them while Kit stays open.
        playbackCleanup = Task {
            while !Task.isCancelled {
                await TranscriptMediaCache.shared.prunePlaybackFiles()
                do { try await Task.sleep(for: .seconds(60 * 60)) } catch { return }
            }
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        playbackCleanup?.cancel()
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
