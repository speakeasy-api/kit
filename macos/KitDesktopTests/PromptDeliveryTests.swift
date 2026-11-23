import SwiftUI
import XCTest
@testable import Kit

final class PromptDeliveryTests: XCTestCase {
    @MainActor
    func testWireMessageIDsAreScopedByRoleAndProjectionMatchesController() async throws {
        let controller = makeController(directory: temporaryDirectory())
        controller.start()
        try await waitUntil { controller.isReady }
        controller.draft = "MOCK_SHARED_MESSAGE_IDS"
        controller.send()
        try await waitUntil { controller.entries.contains { $0.role == .assistant && $0.text == "Assistant text" } && !controller.isRunning }
        XCTAssertEqual(controller.entries.filter { $0.role == .user }.map(\.text), ["User text"])
        XCTAssertTrue(controller.entries.contains { $0.role == .thought && $0.text == "Thought text" })
        XCTAssertEqual(controller.transcriptProjection.items.flatMap(\.entries).map(\.id), controller.entries.map(\.id))
        XCTAssertEqual(controller.transcriptProjection.items.flatMap(\.entries).map(\.text), controller.entries.map(\.text))
        XCTAssertTrue(controller.transcriptProjection.items.filter { !$0.isActivity }.flatMap(\.entries).contains { $0.text == "Assistant text" })
        await close(controller)
    }

    @MainActor
    func testWithdrawalCancelsOnlyTheMatchingUninsertedPrompt() async throws {
        let directory = temporaryDirectory()
        let controller = try await startRunningController(directory: directory)
        controller.draft = "withdraw me"
        controller.send()
        controller.draft = "leave me pending"
        controller.send()
        try await waitUntil { controller.pendingSteers.count == 2 && controller.pendingSteers.allSatisfy { $0.requestID != nil } }
        XCTAssertFalse(controller.entries.contains { $0.text == "withdraw me" || $0.text == "leave me pending" })

        let first = try XCTUnwrap(controller.pendingSteers.first)
        controller.withdrawPendingSteer(id: first.id)
        try await waitUntil { FileManager.default.fileExists(atPath: directory.appendingPathComponent("cancelled").path) }
        XCTAssertEqual(controller.pendingSteers.first?.isWithdrawing, true)
        try Data().write(to: directory.appendingPathComponent("cancel-reply"))
        try await waitUntil { controller.pendingSteers.map(\.text) == ["leave me pending"] }
        XCTAssertFalse(controller.entries.contains { $0.role == .error })
        XCTAssertTrue(controller.canCancel)
        try Data().write(to: directory.appendingPathComponent("insert"))
        try Data().write(to: directory.appendingPathComponent("finish"))
        try await waitUntil { controller.pendingSteers.isEmpty && controller.entries.contains { $0.text == "leave me pending" } }
        XCTAssertFalse(controller.entries.contains { $0.text == "withdraw me" })
        await close(controller)
    }

    @MainActor
    func testResponseBeforeUpdateAfterBestEffortWithdrawal() async throws {
        try await assertInsertion(updateFirst: false)
    }

    @MainActor
    func testUpdateBeforeResponseAfterBestEffortWithdrawal() async throws {
        try await assertInsertion(updateFirst: true)
    }

    @MainActor
    private func assertInsertion(updateFirst: Bool) async throws {
        let directory = temporaryDirectory()
        let controller = try await startRunningController(directory: directory, updateFirst: updateFirst, ignoreWithdrawal: true)
        controller.draft = "still delivered"
        controller.send()
        try await waitUntil { controller.pendingSteers.first?.requestID != nil }
        let item = try XCTUnwrap(controller.pendingSteers.first)
        controller.withdrawPendingSteer(id: item.id)
        try await waitUntil { FileManager.default.fileExists(atPath: directory.appendingPathComponent("cancelled").path) }
        XCTAssertEqual(controller.pendingSteers.first?.isWithdrawing, true)
        try Data().write(to: directory.appendingPathComponent("insert"))
        if updateFirst {
            try await waitUntil { controller.entries.contains { $0.role == .user && $0.text == "still delivered" } }
            XCTAssertEqual(controller.pendingSteers.first?.id, item.id)
        } else {
            try await waitUntil { controller.pendingSteers.isEmpty }
            XCTAssertFalse(controller.entries.contains { $0.text == "still delivered" })
        }
        try Data().write(to: directory.appendingPathComponent("finish"))
        try await waitUntil { controller.pendingSteers.isEmpty && controller.entries.contains { $0.role == .user && $0.text == "still delivered" } }
        XCTAssertEqual(controller.entries.filter { $0.role == .user && $0.text == "still delivered" }.count, 1)
        await close(controller)
    }

    @MainActor
    func testPendingSteerSurvivesIdleUntilItStartsTheNextTurn() async throws {
        let directory = temporaryDirectory()
        let controller = try await startRunningController(directory: directory)
        controller.draft = "starts the next turn"
        controller.send()
        try await waitUntil { controller.pendingSteers.first?.requestID != nil }
        try Data().write(to: directory.appendingPathComponent("idle"))
        try await waitUntil { !controller.isRunning }
        XCTAssertEqual(controller.pendingSteers.count, 1)
        try Data().write(to: directory.appendingPathComponent("insert"))
        try await waitUntil { controller.pendingSteers.isEmpty }
        try Data().write(to: directory.appendingPathComponent("finish"))
        try await waitUntil { controller.canCancel && controller.entries.contains { $0.text == "starts the next turn" } }
        await close(controller)
    }

    @MainActor
    func testOrdinaryPromptReconcilesAttachmentAndContentReplacementByMessageID() async throws {
        let directory = temporaryDirectory()
        let controller = makeController(directory: directory)
        controller.start()
        try await waitUntil { controller.isReady }

        let imageURL = directory.appendingPathComponent("echo.png")
        try Data([1]).write(to: imageURL)
        controller.attachments = [Attachment(url: imageURL, kind: .image, mimeType: "image/png", size: 1)]
        controller.send()
        try await waitUntil { controller.acceptsInput }
        let attachmentUsers = controller.entries.filter { $0.role == .user }
        XCTAssertEqual(attachmentUsers.count, 1)
        XCTAssertEqual(attachmentUsers.first?.presentation?.userMessage?.media.count, 1)

        for kind in ["empty", "null", "omitted"] {
            controller.draft = "replacement: \(kind)"
            controller.send()
            try await waitUntil { controller.acceptsInput }
            let replacement = try XCTUnwrap(controller.entries.last(where: { $0.role == .user }))
            if kind == "omitted" {
                XCTAssertEqual(replacement.text, "replacement: omitted")
            } else {
                XCTAssertEqual(replacement.text, "")
                XCTAssertEqual(replacement.contentBlocks, [])
                XCTAssertEqual(replacement.presentation?.userMessage?.media, [])
            }
        }
        XCTAssertEqual(controller.entries.filter { $0.role == .user }.count, 4)
        await close(controller)
    }

    @MainActor
    func testCompleteAudioEchoPlaysSentBytesAfterSourceChangesInBothOrders() async throws {
        for updateFirst in [false, true] {
            let directory = temporaryDirectory()
            let controller = makeController(directory: directory, updateFirst: updateFirst)
            controller.start()
            try await waitUntil { controller.isReady }

            let url = directory.appendingPathComponent("recording.wav")
            let audio = waveAudio(pcmBytes: 2)
            try audio.write(to: url)
            let attachment = Attachment(url: url, kind: .audio, mimeType: "audio/wav", size: Int64(audio.count))
            controller.draft = "audio: match"
            controller.attachments = [attachment]
            controller.send()
            try await waitUntil { controller.acceptsInput }

            let entry = try XCTUnwrap(controller.entries.last(where: { $0.role == .user }))
            let media = try XCTUnwrap(entry.presentation?.userMessage?.media.first)
            XCTAssertEqual(media.id, attachment.id)
            XCTAssertEqual(media.name, "recording.wav")
            XCTAssertEqual(media.data, audio)
            let resolved = TranscriptMedia(mimeType: media.mimeType, name: media.name, data: media.data, url: media.url)
            XCTAssertEqual(resolved.label, "recording.wav")

            try Data([0]).write(to: url)
            let playbackDirectory = directory.appendingPathComponent("playback", isDirectory: true)
            let cache = TranscriptMediaCache(playbackDirectory: playbackDirectory)
            let playback = await cache.openURL(resolved)
            let playbackURL = try XCTUnwrap(playback)
            XCTAssertNotEqual(playbackURL, url)
            XCTAssertEqual(try Data(contentsOf: playbackURL), audio)

            let files = FileManager.default
            let old = Date(timeIntervalSinceNow: -2 * 24 * 60 * 60)
            try files.setAttributes([.modificationDate: old], ofItemAtPath: playbackURL.path)
            let reopened = await cache.openURL(resolved)
            XCTAssertEqual(reopened, playbackURL)
            XCTAssertEqual(try files.contentsOfDirectory(atPath: playbackDirectory.path), [playbackURL.lastPathComponent])
            await cache.prunePlaybackFiles()
            XCTAssertTrue(files.fileExists(atPath: playbackURL.path), "Reopening refreshes the grace period")

            try Data([1]).write(to: playbackURL)
            let restored = await cache.openURL(resolved)
            XCTAssertEqual(restored, playbackURL)
            XCTAssertEqual(try Data(contentsOf: playbackURL), audio)
            await close(controller)

            try files.setAttributes([.modificationDate: old], ofItemAtPath: playbackURL.path)
            await TranscriptMediaCache(playbackDirectory: playbackDirectory).prunePlaybackFiles()
            XCTAssertFalse(files.fileExists(atPath: playbackURL.path))
            XCTAssertEqual(try Data(contentsOf: url), Data([0]), "Pruning does not touch the source")
        }
    }

    @MainActor
    func testPlaybackPruningLeavesUnownedNeighbors() async throws {
        let files = FileManager.default
        let directory = temporaryDirectory()
        let cache = TranscriptMediaCache(playbackDirectory: directory)
        let audio = waveAudio(pcmBytes: 2)
        let snapshot = await cache.openURL(TranscriptMedia(mimeType: "audio/wav", data: audio))
        let snapshotURL = try XCTUnwrap(snapshot)
        let neighbors = ["recording.wav", snapshotURL.lastPathComponent + ".sidecar"]
            .map { directory.appendingPathComponent($0) }
        let old = Date(timeIntervalSinceNow: -2 * 24 * 60 * 60)
        for url in neighbors {
            try audio.write(to: url)
            try files.setAttributes([.modificationDate: old], ofItemAtPath: url.path)
        }
        try files.setAttributes([.modificationDate: old], ofItemAtPath: snapshotURL.path)

        await cache.prunePlaybackFiles()
        XCTAssertFalse(files.fileExists(atPath: snapshotURL.path))
        for url in neighbors {
            XCTAssertEqual(try Data(contentsOf: url), audio)
        }
    }

    @MainActor
    func testPlaybackCapacityReclaimsExpiredSnapshotsButKeepsRecentOnes() async throws {
        let files = FileManager.default
        let directory = temporaryDirectory()
        let cache = TranscriptMediaCache(playbackDirectory: directory, playbackCapacity: 2)
        let first = TranscriptMedia(mimeType: "audio/wav", data: waveAudio(pcmBytes: 2))
        let second = TranscriptMedia(mimeType: "audio/wav", data: waveAudio(pcmBytes: 4))
        let third = TranscriptMedia(mimeType: "audio/wav", data: waveAudio(pcmBytes: 6))
        let firstURL = await cache.openURL(first)
        let secondURL = await cache.openURL(second)
        let expired = try XCTUnwrap(firstURL)
        let recent = try XCTUnwrap(secondURL)

        let reopenedCache = TranscriptMediaCache(playbackDirectory: directory, playbackCapacity: 2)
        let refused = await reopenedCache.openURL(third)
        XCTAssertNil(refused)
        XCTAssertEqual(Set(try files.contentsOfDirectory(atPath: directory.path)), [expired.lastPathComponent, recent.lastPathComponent])
        let reused = await reopenedCache.openURL(second)
        XCTAssertEqual(reused, recent, "A cache hit does not require an extra slot")

        try files.setAttributes([.modificationDate: Date(timeIntervalSinceNow: -2 * 24 * 60 * 60)], ofItemAtPath: expired.path)
        let admitted = await reopenedCache.openURL(third)
        let thirdURL = try XCTUnwrap(admitted)
        XCTAssertEqual(Set(try files.contentsOfDirectory(atPath: directory.path)), [recent.lastPathComponent, thirdURL.lastPathComponent])
        XCTAssertEqual(try Data(contentsOf: recent), second.data)
    }

    @MainActor
    func testLargeAudioEchoMatchesLocalFileInBothOrders() async throws {
        for updateFirst in [false, true] {
            let directory = temporaryDirectory()
            let controller = makeController(directory: directory, updateFirst: updateFirst)
            controller.start()
            try await waitUntil { controller.isReady }

            let url = directory.appendingPathComponent("recording.wav")
            // 1 MiB of 8-bit mono PCM, well above the Desktop inline preview cap.
            let audio = waveAudio(pcmBytes: 1024 * 1024)
            try audio.write(to: url)
            let attachment = Attachment(url: url, kind: .audio, mimeType: "audio/wav", size: Int64(audio.count))

            for action in ["match", "replace", "remove"] {
                controller.draft = "audio: \(action)"
                controller.attachments = [attachment]
                controller.send()
                try await waitUntil { controller.acceptsInput }
                let entry = try XCTUnwrap(controller.entries.last(where: { $0.role == .user }))
                let media = try XCTUnwrap(entry.presentation?.userMessage?.media)
                if action == "remove" {
                    XCTAssertTrue(media.isEmpty, "updateFirst=\(updateFirst)")
                } else {
                    XCTAssertEqual(media.count, 1, "updateFirst=\(updateFirst)")
                    XCTAssertEqual(media.first?.url, action == "match" ? url : nil, "updateFirst=\(updateFirst)")
                    XCTAssertEqual(media.first?.name, action == "match" ? "recording.wav" : nil)
                    XCTAssertNil(media.first?.data, "Echoed payload is above the inline limit")
                    if action == "match" {
                        let resolved = TranscriptMedia(mimeType: attachment.mimeType, url: media.first?.url)
                        let playbackURL = await TranscriptMediaCache().openURL(resolved)
                        XCTAssertEqual(playbackURL, url)
                        XCTAssertEqual(try Data(contentsOf: XCTUnwrap(playbackURL)), audio)
                    }
                }
            }
            XCTAssertEqual(controller.entries.filter { $0.role == .user }.count, 3)
            await close(controller)
        }
    }

    @MainActor
    func testRejectedPromptKeepsNewerDraftWhenAttachmentsCannotMerge() async throws {
        let directory = temporaryDirectory()
        let controller = makeController(directory: directory)
        controller.start()
        try await waitUntil { controller.isReady }

        let submitted = ["first.png", "second.png"].map { directory.appendingPathComponent($0) }
        for url in submitted {
            try Data().write(to: url)
            let file = try FileHandle(forWritingTo: url)
            try file.truncate(atOffset: UInt64(ConversationController.maximumAttachmentBytes))
            try file.close()
        }
        controller.draft = "held rejection"
        controller.addAttachments(submitted)
        controller.send()
        try await waitUntil { FileManager.default.fileExists(atPath: directory.appendingPathComponent("reject-held").path) }

        let newer = directory.appendingPathComponent("newer.png")
        try Data([1]).write(to: newer)
        controller.draft = "newer draft"
        controller.addAttachments([newer])
        try Data().write(to: directory.appendingPathComponent("reject-now"))
        try await waitUntil { controller.acceptsInput && !controller.savedDrafts.isEmpty }
        XCTAssertEqual(controller.draft, "newer draft")
        XCTAssertEqual(controller.attachments.map(\.url), [newer])

        let rejected = try XCTUnwrap(controller.savedDrafts.first)
        XCTAssertEqual(rejected.text, "held rejection")
        XCTAssertEqual(rejected.attachments.map(\.url), submitted)
        controller.restoreSavedDraft(id: rejected.id)
        XCTAssertEqual(controller.draft, "held rejection")
        XCTAssertEqual(controller.attachments.map(\.url), submitted)

        let savedNewer = try XCTUnwrap(controller.savedDrafts.first)
        XCTAssertEqual(savedNewer.text, "newer draft")
        XCTAssertEqual(savedNewer.attachments.map(\.url), [newer])
        controller.attachments = []
        controller.draft = ""
        controller.restoreSavedDraft(id: savedNewer.id)
        XCTAssertEqual(controller.draft, "newer draft")
        XCTAssertEqual(controller.attachments.map(\.url), [newer])
        XCTAssertTrue(controller.savedDrafts.isEmpty)
        await close(controller)
    }

    @MainActor
    func testSavedDraftPickerStaysCompactAndAllRejectedDraftsRemainRecoverable() async throws {
        let directory = temporaryDirectory()
        let controller = makeController(directory: directory)
        controller.start()
        try await waitUntil { controller.isReady }

        let files = try (0..<ConversationController.maximumAttachmentCount).map { index in
            let url = directory.appendingPathComponent("submitted-\(index).png")
            try Data([1]).write(to: url)
            return url
        }
        let newer = directory.appendingPathComponent("newer.png")
        try Data([1]).write(to: newer)
        let held = directory.appendingPathComponent("reject-held")
        let release = directory.appendingPathComponent("reject-now")
        let texts = (1...20).map { "held rejection \($0)" }
        for (index, text) in texts.enumerated() {
            controller.draft = text
            controller.attachments = []
            controller.addAttachments(files)
            controller.send()
            try await waitUntil { FileManager.default.fileExists(atPath: held.path) }

            controller.draft = "newer draft"
            controller.addAttachments([newer])
            try Data().write(to: release)
            try await waitUntil { controller.acceptsInput && controller.savedDrafts.count == index + 1 }
            try FileManager.default.removeItem(at: held)
            try FileManager.default.removeItem(at: release)
        }

        let saved = controller.savedDrafts
        XCTAssertEqual(saved.map(\.text), texts)
        func pickerHeight(_ drafts: [ConversationController.SavedDraft]) -> CGFloat {
            NSHostingView(rootView: SavedDraftPicker(drafts: drafts, recover: controller.restoreSavedDraft))
                .fittingSize.height
        }
        let singleDraftHeight = pickerHeight(Array(saved.prefix(1)))
        XCTAssertGreaterThan(singleDraftHeight, 0)
        XCTAssertEqual(pickerHeight(saved), singleDraftHeight, accuracy: 1)

        for item in saved {
            controller.draft = ""
            controller.attachments = []
            controller.restoreSavedDraft(id: item.id)
            XCTAssertEqual(controller.draft, item.text)
            XCTAssertEqual(controller.attachments.map(\.url), files)
        }
        XCTAssertTrue(controller.savedDrafts.isEmpty)
        await close(controller)
    }

    @MainActor
    func testRejectedSteerRestoresDraftWithoutSettlingForegroundTurn() async throws {
        let controller = try await startRunningController(directory: temporaryDirectory())
        controller.draft = "reject steer"
        controller.send()
        try await waitUntil { controller.pendingSteers.isEmpty && controller.status.contains("steer rejected") }
        XCTAssertEqual(controller.draft, "reject steer")
        XCTAssertTrue(controller.canCancel)
        XCTAssertFalse(controller.entries.contains { $0.role == .user && $0.text == "reject steer" })
        await close(controller)
    }

    @MainActor
    private func startRunningController(
        directory: URL, updateFirst: Bool = false, ignoreWithdrawal: Bool = false
    ) async throws -> ConversationController {
        let controller = makeController(directory: directory, updateFirst: updateFirst, ignoreWithdrawal: ignoreWithdrawal)
        controller.start()
        try await waitUntil { controller.isReady }
        controller.draft = "keep working"
        controller.send()
        try await waitUntil { controller.canSteer }
        return controller
    }

    @MainActor
    private func makeController(directory: URL, updateFirst: Bool = false, ignoreWithdrawal: Bool = false) -> ConversationController {
        let launch = ACPClient.LaunchOverride(executable: URL(fileURLWithPath: "/usr/bin/python3"), prefixArguments: [
            "-u", "-c", Self.agent, directory.path, updateFirst ? "update-first" : "response-first",
            ignoreWithdrawal ? "ignore-withdrawal" : "withdraw",
        ])
        return ConversationController(conversation: Conversation(workspaceID: UUID()), workspacePath: directory.path,
                                      client: ACPClient(launchOverride: launch, requestTimeout: 3, promptTimeout: 10))
    }

    private static let agent = #"""
    import base64, json, pathlib, queue, sys, threading

    directory = pathlib.Path(sys.argv[1])
    update_first = sys.argv[2] == "update-first"
    honor_withdrawal = sys.argv[3] == "withdraw"
    received = queue.Queue()
    held = {}
    inserted = {}
    withdrawn = {}
    was_idle = False
    rejection = None

    def read():
        for line in sys.stdin:
            received.put(json.loads(line))
        received.put(None)

    def respond(id, result):
        print(json.dumps({"jsonrpc": "2.0", "id": id, "result": result}))

    def error(id, code, message):
        print(json.dumps({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))

    def update(kind, **fields):
        print(json.dumps({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": "session", "update": {"sessionUpdate": kind, **fields}
        }}))

    def report(id, blocks):
        update("user_message", messageId=str(id), content=blocks)

    threading.Thread(target=read, daemon=True).start()
    while True:
        if rejection is not None and (directory / "reject-now").exists():
            error(rejection, -32602, "prompt rejected")
            rejection = None
        if not was_idle and (directory / "idle").exists():
            was_idle = True
            update("state_update", state="idle", stopReason="end_turn")
        if (directory / "cancel-reply").exists():
            for id in list(withdrawn):
                del withdrawn[id]
                error(id, -32800, "cancelled")
        if (directory / "insert").exists():
            for id, blocks in list(held.items()):
                del held[id]
                inserted[id] = blocks
                if update_first:
                    report(id, blocks)
                else:
                    respond(id, {"messageId": str(id)})
        if (directory / "finish").exists():
            for id, blocks in list(inserted.items()):
                del inserted[id]
                if update_first:
                    respond(id, {"messageId": str(id)})
                else:
                    report(id, blocks)
                if was_idle:
                    update("state_update", state="running")
        try:
            req = received.get(timeout=0.01)
        except queue.Empty:
            continue
        if req is None:
            break
        method, id, params = req["method"], req.get("id"), req.get("params", {})
        if method == "initialize":
            respond(id, {"protocolVersion": 2, "info": {"name": "test", "version": "0"},
                         "capabilities": {"session": {"prompt": {"image": {}, "audio": {}, "delivery": {"steer": {}}}}}})
        elif method == "session/new":
            respond(id, {"sessionId": "session"})
        elif method == "session/prompt":
            blocks = params["prompt"]
            if params.get("delivery") == "steer":
                if blocks[0]["text"] == "reject steer":
                    error(id, -32602, "steer rejected")
                else:
                    held[id] = blocks
            elif blocks[0]["text"].startswith("held rejection"):
                rejection = id
                (directory / "reject-held").touch()
            elif blocks[0]["text"].startswith("audio: "):
                action = blocks[0]["text"][len("audio: "):]
                if action == "remove":
                    blocks = blocks[:1]
                elif action == "replace":
                    # Same MIME and preview bytes, but not the same audio.
                    decoded = base64.b64decode(blocks[1]["data"])
                    blocks[1]["data"] = base64.b64encode(decoded[:-1] + b"\x00").decode()
                if update_first:
                    report(id, blocks)
                respond(id, {"messageId": str(id)})
                if not update_first:
                    report(id, blocks)
                update("state_update", state="idle", stopReason="end_turn")
            elif blocks[0]["text"] == "MOCK_SHARED_MESSAGE_IDS":
                report(id, [{"type": "text", "text": "User text"}])
                update("agent_thought_chunk", messageId=str(id), content={"type": "text", "text": "Thought text"})
                update("agent_message_chunk", messageId=str(id), content={"type": "text", "text": "Assistant text"})
                respond(id, {"messageId": str(id)})
                update("state_update", state="idle", stopReason="end_turn")
            else:
                respond(id, {"messageId": str(id)})
                text = blocks[0]["text"]
                if text == "replacement: empty":
                    report(id, [])
                elif text == "replacement: null":
                    report(id, None)
                elif text == "replacement: omitted":
                    update("user_message", messageId=str(id))
                else:
                    report(id, blocks)
                if text == "keep working":
                    update("state_update", state="running")
                else:
                    update("state_update", state="idle", stopReason="end_turn")
        elif method == "$/cancel_request":
            request_id = params["requestId"]
            if "id" not in req and request_id in held:
                (directory / "cancelled").touch()
                if honor_withdrawal:
                    withdrawn[request_id] = held.pop(request_id)
        elif method == "session/cancel":
            update("state_update", state="idle", stopReason="cancelled")
        elif method == "session/close":
            respond(id, {})
            break
    """#

    @MainActor
    private func close(_ controller: ConversationController) async {
        let closed = expectation(description: "closed")
        controller.close { closed.fulfill() }
        await fulfillment(of: [closed], timeout: 4)
    }

    @MainActor
    private func waitUntil(_ condition: @escaping @MainActor () -> Bool) async throws {
        for _ in 0..<150 {
            if condition() { return }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        XCTFail("Condition did not become true")
        throw NSError(domain: "PromptDeliveryTests", code: 1)
    }

    private func waveAudio(pcmBytes: Int) -> Data {
        var audio = Data("RIFF".utf8)
        withUnsafeBytes(of: UInt32(pcmBytes + 36).littleEndian) { audio.append(contentsOf: $0) }
        audio.append(contentsOf: "WAVEfmt ".utf8)
        audio.append(contentsOf: [16, 0, 0, 0, 1, 0, 1, 0, 64, 31, 0, 0, 64, 31, 0, 0, 1, 0, 8, 0])
        audio.append(contentsOf: "data".utf8)
        withUnsafeBytes(of: UInt32(pcmBytes).littleEndian) { audio.append(contentsOf: $0) }
        audio.append(Data(repeating: 128, count: pcmBytes))
        return audio
    }

    private func temporaryDirectory() -> URL {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }
}
