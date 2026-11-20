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
    import json, pathlib, queue, sys, threading

    directory = pathlib.Path(sys.argv[1])
    update_first = sys.argv[2] == "update-first"
    honor_withdrawal = sys.argv[3] == "withdraw"
    received = queue.Queue()
    held = {}
    inserted = {}
    withdrawn = {}
    was_idle = False

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
                         "capabilities": {"session": {"prompt": {"image": {}, "delivery": {"steer": {}}}}}})
        elif method == "session/new":
            respond(id, {"sessionId": "session"})
        elif method == "session/prompt":
            blocks = params["prompt"]
            if params.get("delivery") == "steer":
                if blocks[0]["text"] == "reject steer":
                    error(id, -32602, "steer rejected")
                else:
                    held[id] = blocks
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

    private func temporaryDirectory() -> URL {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }
}
