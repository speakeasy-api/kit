import AppKit
import Darwin
import Foundation
import XCTest
@testable import Kit

final class ACPProcessTests: XCTestCase {
    func testFixtureStreamsRichUpdatesEncodesMediaTimesOutAndCloses() throws {
        let client = makeClient(promptTimeout: 1.0)
        let ready = expectation(description: "ready")
        let runtime = expectation(description: "runtime events"); runtime.expectedFulfillmentCount = 3
        let diagnostic = expectation(description: "diagnostic")
        let exited = expectation(description: "exited")
        var updates: [DesktopUpdate] = []
        var runtimeEvents: [[String: Any]] = []
        var diagnostics: [String] = []
        var idleExpectation: XCTestExpectation?
        client.onRuntimeEvent = { runtimeEvents.append($0); runtime.fulfill() }
        client.onDiagnostic = { diagnostics.append($0); if $0 == "mock diagnostic" { diagnostic.fulfill() } }
        client.onUpdate = { update in
            updates.append(update)
            if case .state(let state) = update, state.state == "idle" { idleExpectation?.fulfill(); idleExpectation = nil }
        }
        client.onExit = { _ in exited.fulfill() }
        client.start(options: options(sessionID: "desktop-new", resume: false), loading: false) { result in
            if case .failure(let error) = result { XCTFail(error.localizedDescription) }
            ready.fulfill()
        }
        wait(for: [ready], timeout: 3)

        let rich = expectation(description: "rich prompt accepted")
        let richIdle = expectation(description: "rich prompt idle")
        idleExpectation = richIdle
        client.prompt(text: "MOCK_RICH_OUTPUT", attachments: []) { result in
            if case .failure(let error) = result { XCTFail(error.localizedDescription) }
            rich.fulfill()
        }
        wait(for: [rich, richIdle, runtime, diagnostic], timeout: 3)
        XCTAssertTrue(updates.contains { if case .agentThought = $0 { true } else { false } })
        XCTAssertTrue(updates.contains { if case .toolCallUpdate = $0 { true } else { false } })
        XCTAssertTrue(updates.contains { if case .usage = $0 { true } else { false } })
        let richBlocks = updates.flatMap { update -> [DesktopContentBlock] in
            guard case .agentMessage(let message) = update else { return [] }
            return message.content
        }
        XCTAssertEqual(richBlocks, [.image(data: "aGVsbG8=", mimeType: "image/png", uri: nil), .text("rich done")])
        XCTAssertEqual(runtimeEvents.compactMap { $0["event"] as? String }, ["session_started", "child_started", "child_finished"])
        XCTAssertEqual(runtimeEvents.first?["session_id"] as? String, "desktop-new")
        XCTAssertTrue(diagnostics.contains("mock diagnostic"))

        let directory = temporaryDirectory()
        let imageURL = directory.appendingPathComponent("image.png")
        try Data([0x89, 0x50, 0x4e, 0x47]).write(to: imageURL)
        let media = Attachment(url: imageURL, kind: .image, mimeType: "image/png", size: 4)
        let mediaDone = expectation(description: "media prompt accepted")
        let mediaIdle = expectation(description: "media prompt idle")
        idleExpectation = mediaIdle
        client.prompt(text: "MOCK_MEDIA", attachments: [media]) { result in
            if case .failure(let error) = result { XCTFail(error.localizedDescription) }
            mediaDone.fulfill()
        }
        wait(for: [mediaDone, mediaIdle], timeout: 3)
        let texts = updates.flatMap { update -> [String] in
            guard case .agentMessage(let message) = update else { return [] }
            return message.content.compactMap { if case .text(let text) = $0 { return text }; return nil }
        }
        XCTAssertTrue(texts.contains("text,image"))

        let accepted = expectation(description: "hanging prompt accepted")
        client.prompt(text: "MOCK_HANG", attachments: []) { result in
            if case .failure(let error) = result { XCTFail(error.localizedDescription) }
            accepted.fulfill()
        }
        wait(for: [accepted], timeout: 1)
        client.cancel()
        client.close(activeTurn: false)
        wait(for: [exited], timeout: 3)
    }

    func testLoadReplayUpdatesArriveBeforeLoadCompletion() {
        let client = makeClient(promptTimeout: 2)
        let ready = expectation(description: "loaded")
        let exited = expectation(description: "exited")
        var order: [String] = []
        client.onUpdate = { update in
            switch update {
            case .userMessage: order.append("user_message_chunk")
            case .agentMessage: order.append("agent_message_chunk")
            default: break
            }
        }
        client.onExit = { _ in exited.fulfill() }
        client.start(options: options(sessionID: "loaded-session", resume: true), loading: true) { result in
            if case .failure(let error) = result { XCTFail(error.localizedDescription) }
            order.append("ready")
            ready.fulfill()
        }
        wait(for: [ready], timeout: 3)
        XCTAssertEqual(Array(order.prefix(3)), ["user_message_chunk", "agent_message_chunk", "ready"])
        client.close(activeTurn: false)
        wait(for: [exited], timeout: 3)
    }

    @MainActor
    func testPromisedAttachmentReceiptBlocksUntilMaterialized() throws {
        let controller = ConversationController(
            conversation: Conversation(workspaceID: UUID()),
            workspacePath: repositoryRoot.path,
            client: makeClient(promptTimeout: 2)
        )
        let image = temporaryDirectory().appendingPathComponent("promised.png")
        try Data([0x89, 0x50, 0x4e, 0x47]).write(to: image)

        controller.beginReceivingAttachments(1)
        XCTAssertEqual(controller.pendingAttachmentReceipts, 1)
        controller.finishReceivingAttachment(.success(image))

        XCTAssertEqual(controller.pendingAttachmentReceipts, 0)
        XCTAssertEqual(controller.attachments.map(\.url), [image.standardizedFileURL])
    }

    @MainActor
    func testAttachmentReadFailureKeepsDraftAndPendingFiles() {
        let client = makeClient(promptTimeout: 2)
        let conversation = Conversation(workspaceID: UUID())
        let controller = ConversationController(conversation: conversation, workspacePath: repositoryRoot.path, client: client)
        let ready = expectation(description: "ready")
        controller.onSessionReady = { _, _ in ready.fulfill() }
        controller.start()
        wait(for: [ready], timeout: 3)

        let missing = Attachment(url: temporaryDirectory().appendingPathComponent("missing.png"), kind: .image, mimeType: "image/png", size: 1)
        controller.draft = "keep this"
        controller.attachments = [missing]
        controller.send()
        let failed = expectation(description: "attachment failure")
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) {
            XCTAssertEqual(controller.draft, "keep this")
            XCTAssertEqual(controller.attachments, [missing])
            XCTAssertTrue(controller.status.hasPrefix("Error:"))
            failed.fulfill()
        }
        wait(for: [failed], timeout: 2)
        let closed = expectation(description: "closed")
        controller.close { closed.fulfill() }
        wait(for: [closed], timeout: 3)
    }

    @MainActor
    func testSteeringQueuesPendingMessageUntilDeliveredAndRestoresRejectedDraft() async throws {
        let release = temporaryDirectory().appendingPathComponent("release-injection")
        let client = makeClient(
            promptTimeout: 2, fixtureArguments: ["--models", "--steer", "--inject-release=" + release.path]
        )
        let controller = ConversationController(
            conversation: Conversation(workspaceID: UUID()), workspacePath: repositoryRoot.path, client: client
        )
        controller.start()
        try await waitUntil { controller.isReady }

        controller.draft = "MOCK_HANG"
        controller.send()
        try await waitUntil { controller.canSteer }

        controller.draft = "change direction"
        controller.send()
        try await waitUntil { controller.pendingSteers.count == 1 }
        XCTAssertEqual(controller.pendingSteers.first?.summary, "change direction")
        XCTAssertTrue(controller.draft.isEmpty)
        XCTAssertFalse(controller.entries.contains { $0.role == .user && $0.text == "change direction" })

        controller.draft = "then add tests"
        controller.send()
        try await waitUntil { controller.pendingSteers.count == 2 }
        XCTAssertEqual(controller.pendingSteers.map(\.summary), ["change direction", "then add tests"])

        try Data().write(to: release)
        try await waitUntil {
            controller.pendingSteers.isEmpty
                && controller.entries.contains { $0.role == .user && $0.text == "change direction" }
                && controller.entries.contains { $0.role == .user && $0.text == "then add tests" }
        }

        controller.draft = "MOCK_REJECT_INJECT"
        controller.send()
        try await waitUntil { controller.status.contains("injection rejected") }
        XCTAssertEqual(controller.draft, "MOCK_REJECT_INJECT")

        let closed = expectation(description: "closed")
        controller.close { closed.fulfill() }
        await fulfillment(of: [closed], timeout: 4)
    }

    @MainActor
    func testRejectedPromptRestoresComposerAndRemovesOptimisticTranscript() throws {
        let client = makeClient(promptTimeout: 2)
        let controller = ConversationController(conversation: Conversation(workspaceID: UUID()), workspacePath: repositoryRoot.path, client: client)
        let ready = expectation(description: "ready")
        controller.onSessionReady = { _, _ in ready.fulfill() }
        controller.start()
        wait(for: [ready], timeout: 3)

        let imageURL = temporaryDirectory().appendingPathComponent("restore.png")
        try Data([0x89]).write(to: imageURL)
        let image = Attachment(url: imageURL, kind: .image, mimeType: "image/png", size: 1)
        controller.draft = "MOCK_REJECT"
        controller.attachments = [image]
        let finished = expectation(description: "rejected")
        controller.onTurnFinished = { reason in XCTAssertEqual(reason, "error"); finished.fulfill() }
        controller.send()
        wait(for: [finished], timeout: 3)

        XCTAssertEqual(controller.draft, "MOCK_REJECT")
        XCTAssertEqual(controller.attachments, [image])
        XCTAssertFalse(controller.entries.contains { $0.role == .user })
        XCTAssertTrue(controller.status.contains("prompt rejected"))
        let closed = expectation(description: "closed"); controller.close { closed.fulfill() }; wait(for: [closed], timeout: 3)
    }

    @MainActor
    func testAcceptedEchoAppearsOnceAndInterruptionStartsReplacementResponse() {
        let client = makeClient(promptTimeout: 2)
        let controller = ConversationController(conversation: Conversation(workspaceID: UUID()), workspacePath: repositoryRoot.path, client: client)
        let ready = expectation(description: "ready")
        controller.onSessionReady = { _, _ in ready.fulfill() }
        controller.start()
        wait(for: [ready], timeout: 3)

        let echoFinished = expectation(description: "echo finished")
        controller.onTurnFinished = { _ in echoFinished.fulfill() }
        controller.draft = "MOCK_ECHO"
        controller.send()
        wait(for: [echoFinished], timeout: 3)
        XCTAssertEqual(controller.entries.filter { $0.role == .user && $0.text == "MOCK_ECHO" }.count, 1)

        let replacementFinished = expectation(description: "replacement finished")
        controller.onTurnFinished = { _ in replacementFinished.fulfill() }
        controller.draft = "MOCK_INTERRUPTION"
        controller.send()
        wait(for: [replacementFinished], timeout: 3)
        let assistant = controller.entries.filter { $0.role == .assistant }
        XCTAssertFalse(assistant.contains { $0.text.contains("stale response") })
        XCTAssertTrue(assistant.contains { $0.text == "fresh responsereplacement done" })
        controller.copyLastResponse()
        XCTAssertEqual(NSPasteboard.general.string(forType: .string), "fresh responsereplacement done")
        let closed = expectation(description: "closed"); controller.close { closed.fulfill() }; wait(for: [closed], timeout: 3)
    }

    @MainActor
    func testUnknownUpdatesAreIgnoredDuplicateSettlementsAreIdempotentAndBackgroundWorkSurvivesTurnEnd() throws {
        let client = makeClient(promptTimeout: 2)
        let controller = ConversationController(conversation: Conversation(workspaceID: UUID()), workspacePath: repositoryRoot.path, client: client)
        let ready = expectation(description: "ready")
        controller.onSessionReady = { _, _ in ready.fulfill() }
        controller.start()
        wait(for: [ready], timeout: 3)

        var starts = 0
        var finishes = 0
        let settlementFinished = expectation(description: "settlement finished"); settlementFinished.expectedFulfillmentCount = 2
        controller.onTurnStarted = { _ in starts += 1 }
        controller.onTurnFinished = { _ in finishes += 1; settlementFinished.fulfill() }
        controller.draft = "MOCK_UNKNOWN MOCK_SETTLEMENT MOCK_BACKGROUND"
        controller.send()
        wait(for: [settlementFinished], timeout: 3)

        XCTAssertEqual(starts, 2)
        XCTAssertEqual(finishes, 2)
        XCTAssertTrue(controller.diagnostics.contains { $0.contains("vendor_future_update") })
        XCTAssertFalse(controller.entries.contains { $0.text.contains("opaque") || $0.text.contains("vendor_future_update") })
        let background = try XCTUnwrap(controller.entries.first { $0.toolCallID == "background-1" })
        XCTAssertTrue(background.backgrounded)
        XCTAssertTrue(background.isStreaming)
        let closed = expectation(description: "closed"); controller.close { closed.fulfill() }; wait(for: [closed], timeout: 3)
    }

    func testCloseCompletesAfterLaunchFailure() {
        let launch = ACPClient.LaunchOverride(executable: URL(fileURLWithPath: "/definitely/missing/kit"))
        let client = ACPClient(launchOverride: launch, requestTimeout: 1, promptTimeout: 1)
        let failed = expectation(description: "start failed")
        client.start(options: options(sessionID: "failed", resume: false), loading: false) { result in
            if case .success = result { XCTFail("expected launch failure") }
            failed.fulfill()
        }
        wait(for: [failed], timeout: 2)
        let closed = expectation(description: "close completed")
        client.close(activeTurn: false) { closed.fulfill() }
        wait(for: [closed], timeout: 1)
    }

    @MainActor
    func testFixtureRichUpdatesCoverReducerFallbacks() {
        let client = makeClient(promptTimeout: 2)
        let controller = ConversationController(
            conversation: Conversation(workspaceID: UUID()),
            workspacePath: repositoryRoot.path,
            client: client
        )
        let ready = expectation(description: "ready")
        let finished = expectation(description: "finished")
        controller.onSessionReady = { _, _ in ready.fulfill() }
        controller.onTurnFinished = { _ in finished.fulfill() }
        controller.start()
        wait(for: [ready], timeout: 3)
        controller.draft = "MOCK_RICH_OUTPUT"
        controller.send()
        wait(for: [finished], timeout: 3)

        XCTAssertTrue(controller.entries.contains { $0.role == .thought })
        XCTAssertTrue(controller.entries.contains {
            $0.role == .tool && $0.title == "Inspect files" && !$0.isStreaming && $0.presentation?.tool?.status == .completed
        })
        XCTAssertTrue(controller.entries.contains { $0.role == .plan && $0.text.contains("[completed] Inspect") })
        XCTAssertFalse(controller.entries.contains { $0.role == .status && $0.title == "fixture edge" })
        XCTAssertEqual(controller.contextUsed, 10)
        XCTAssertEqual(controller.contextSize, 100)

        let closed = expectation(description: "closed")
        controller.close { closed.fulfill() }
        wait(for: [closed], timeout: 3)
    }

    @MainActor
    func testControllerPreservesDraftWhitespaceAndPermissionCancellationIsNested() throws {
        let log = temporaryDirectory().appendingPathComponent("requests.jsonl")
        let client = makeClient(promptTimeout: 2, fixtureArguments: ["--models", "--request-log=" + log.path])
        let controller = ConversationController(conversation: Conversation(workspaceID: UUID()), workspacePath: repositoryRoot.path, client: client)
        let ready = expectation(description: "ready"); controller.onSessionReady = { _, _ in ready.fulfill() }
        controller.start(); wait(for: [ready], timeout: 3)

        let original = "  MOCK_PERMISSION  \n"
        let finished = expectation(description: "finished"); controller.onTurnFinished = { _ in finished.fulfill() }
        controller.draft = original
        controller.send()
        wait(for: [finished], timeout: 3)

        var records: [[String: Any]] = []
        for _ in 0..<50 {
            records = try String(contentsOf: log, encoding: .utf8).split(whereSeparator: \.isNewline)
                .compactMap { try? JSONSerialization.jsonObject(with: Data($0.utf8)) as? [String: Any] }
            if records.contains(where: { $0["id"] as? String == "permission-1" }) { break }
            usleep(20_000)
        }
        XCTAssertEqual(records.last(where: { $0["method"] as? String == "session/prompt" })?["text"] as? String, original)
        let response = records.first(where: { $0["id"] as? String == "permission-1" })?["result"] as? [String: Any]
        XCTAssertEqual((response?["outcome"] as? [String: Any])?["outcome"] as? String, "cancelled")
        let closed = expectation(description: "closed"); controller.close { closed.fulfill() }; wait(for: [closed], timeout: 3)
    }

    func testRejectsUnnegotiatedImageAndAudioBeforeSendingPrompt() throws {
        let log = temporaryDirectory().appendingPathComponent("requests.jsonl")
        let client = makeClient(
            promptTimeout: 2, fixtureArguments: ["--models", "--prompt-capabilities=none", "--request-log=" + log.path]
        )
        let ready = expectation(description: "ready")
        client.start(options: options(sessionID: "unsupported-media", resume: false), loading: false) { _ in ready.fulfill() }
        wait(for: [ready], timeout: 3)

        let directory = temporaryDirectory()
        let imageURL = directory.appendingPathComponent("image.png"); try Data([1]).write(to: imageURL)
        let audioURL = directory.appendingPathComponent("audio.wav"); try Data([2]).write(to: audioURL)
        let rejected = expectation(description: "rejected media"); rejected.expectedFulfillmentCount = 2
        for attachment in [
            Attachment(url: imageURL, kind: .image, mimeType: "image/png", size: 1),
            Attachment(url: audioURL, kind: .audio, mimeType: "audio/wav", size: 1),
        ] {
            client.prompt(text: "media", attachments: [attachment]) { result in
                guard case .failure(let error) = result else { XCTFail("expected unsupported attachment"); rejected.fulfill(); return }
                XCTAssertTrue(error.localizedDescription.contains("does not support"))
                rejected.fulfill()
            }
        }
        wait(for: [rejected], timeout: 3)
        let records = try String(contentsOf: log, encoding: .utf8).split(whereSeparator: \.isNewline)
            .compactMap { try? JSONSerialization.jsonObject(with: Data($0.utf8)) as? [String: Any] }
        XCTAssertFalse(records.contains { $0["method"] as? String == "session/prompt" })
        client.close(activeTurn: false)
    }

    @MainActor
    func testAttachmentOnlyEchoAppearsOnceAndExplicitEmptyReplacementWins() throws {
        let client = makeClient(promptTimeout: 2)
        let controller = ConversationController(conversation: Conversation(workspaceID: UUID()), workspacePath: repositoryRoot.path, client: client)
        let ready = expectation(description: "ready"); controller.onSessionReady = { _, _ in ready.fulfill() }
        controller.start(); wait(for: [ready], timeout: 3)

        let imageURL = temporaryDirectory().appendingPathComponent("echo.png"); try Data([1]).write(to: imageURL)
        controller.attachments = [Attachment(url: imageURL, kind: .image, mimeType: "image/png", size: 1)]
        let attachmentFinished = expectation(description: "attachment finished")
        controller.onTurnFinished = { _ in attachmentFinished.fulfill() }
        controller.send(); wait(for: [attachmentFinished], timeout: 3)
        let attachmentUsers = controller.entries.filter { $0.role == .user }
        XCTAssertEqual(attachmentUsers.count, 1)
        XCTAssertEqual(attachmentUsers[0].presentation?.userMessage?.media.count, 1)

        let replacementFinished = expectation(description: "replacement finished")
        controller.onTurnFinished = { _ in replacementFinished.fulfill() }
        controller.draft = "MOCK_EMPTY_REPLACEMENT"
        controller.send(); wait(for: [replacementFinished], timeout: 3)
        let replacement = try XCTUnwrap(controller.entries.last(where: { $0.role == .user }))
        XCTAssertEqual(replacement.text, "")
        XCTAssertEqual(replacement.contentBlocks, [])
        XCTAssertEqual(replacement.presentation?.userMessage?.media, [])

        let nullFinished = expectation(description: "null finished")
        controller.onTurnFinished = { _ in nullFinished.fulfill() }
        controller.draft = "MOCK_NULL_REPLACEMENT"
        controller.send(); wait(for: [nullFinished], timeout: 3)
        let nullReplacement = try XCTUnwrap(controller.entries.last(where: { $0.role == .user }))
        XCTAssertEqual(nullReplacement.text, "")
        XCTAssertEqual(nullReplacement.contentBlocks, [])

        let omittedFinished = expectation(description: "omitted finished")
        controller.onTurnFinished = { _ in omittedFinished.fulfill() }
        controller.draft = "MOCK_OMITTED_REPLACEMENT"
        controller.send(); wait(for: [omittedFinished], timeout: 3)
        XCTAssertEqual(controller.entries.last(where: { $0.role == .user })?.text, "MOCK_OMITTED_REPLACEMENT")
        let closed = expectation(description: "closed"); controller.close { closed.fulfill() }; wait(for: [closed], timeout: 3)
    }

    @MainActor
    func testBackgroundFlagRecomputesWhenRawInputArrivesAndClears() {
        let client = makeClient(promptTimeout: 2)
        let controller = ConversationController(conversation: Conversation(workspaceID: UUID()), workspacePath: repositoryRoot.path, client: client)
        let ready = expectation(description: "ready"); controller.onSessionReady = { _, _ in ready.fulfill() }
        controller.start(); wait(for: [ready], timeout: 3)
        let finished = expectation(description: "finished"); controller.onTurnFinished = { _ in finished.fulfill() }
        controller.draft = "MOCK_BACKGROUND_RECOMPUTE"
        controller.send(); wait(for: [finished], timeout: 3)
        XCTAssertEqual(controller.entries.first(where: { $0.toolCallID == "late-background" })?.backgrounded, true)
        XCTAssertEqual(controller.entries.first(where: { $0.toolCallID == "cleared-background" })?.backgrounded, false)
        let closed = expectation(description: "closed"); controller.close { closed.fulfill() }; wait(for: [closed], timeout: 3)
    }

    func testRealKitLifecycleSmoke() throws {
        let binary = repositoryRoot.appendingPathComponent("target/debug/kit")
        try XCTSkipUnless(FileManager.default.isExecutableFile(atPath: binary.path), "cargo build --locked --bin kit is required")
        let home = temporaryDirectory()
        let root = temporaryDirectory()
        let environment = [
            "HOME": home.path,
            "OPENROUTER_API_KEY": "desktop-smoke-test",
            "OPENROUTER_BASE_URL": "http://127.0.0.1:9/v1",
        ]
        let launch = ACPClient.LaunchOverride(executable: binary, prefixArguments: [], environment: environment)
        let options = ACPLaunchOptions(
            root: root.path, sessionID: "desktop-real-smoke", resume: false,
            provider: "openrouter", model: "openai/gpt-4o-mini", reasoningEffort: "default"
        )

        let created = ACPClient(launchOverride: launch, requestTimeout: 5, promptTimeout: 5)
        var createdSessionID: String?
        let ready = expectation(description: "real kit session/new")
        created.start(options: options, loading: false) { result in
            do { createdSessionID = try result.get()["sessionId"] as? String }
            catch { XCTFail(error.localizedDescription) }
            ready.fulfill()
        }
        wait(for: [ready], timeout: 10)

        let prompted = expectation(description: "real kit session/prompt")
        created.prompt(text: "wait until cancelled", attachments: []) { _ in prompted.fulfill() }
        let closed = expectation(description: "real kit session/cancel and session/close")
        created.close(activeTurn: true) { closed.fulfill() }
        wait(for: [prompted, closed], timeout: 12)

        let loaded = ACPClient(launchOverride: launch, requestTimeout: 5, promptTimeout: 5)
        let resumed = expectation(description: "real kit session/resume")
        let resumedOptions = ACPLaunchOptions(
            root: root.path, sessionID: try XCTUnwrap(createdSessionID), resume: true,
            provider: "openrouter", model: "openai/gpt-4o-mini", reasoningEffort: "default"
        )
        loaded.start(options: resumedOptions, loading: true) { result in
            if case .failure(let error) = result { XCTFail(error.localizedDescription) }
            resumed.fulfill()
        }
        wait(for: [resumed], timeout: 10)
        let loadClosed = expectation(description: "loaded session close")
        loaded.close(activeTurn: false) { loadClosed.fulfill() }
        wait(for: [loadClosed], timeout: 5)
    }

    func testDrainsUnterminatedFinalResponseBeforeReportingExit() {
        let client = makeClient(promptTimeout: 2, environment: ["MOCK_EXIT_TAIL": "1"])
        let ready = expectation(description: "final response handled")
        let exited = expectation(description: "exited")
        client.onExit = { status in XCTAssertEqual(status, 0); exited.fulfill() }
        client.start(options: options(sessionID: "tail", resume: false), loading: false) { result in
            if case .failure(let error) = result { XCTFail(error.localizedDescription) }
            ready.fulfill()
        }
        wait(for: [ready, exited], timeout: 3)
    }

    func testCloseKillsProcessGroupDescendantThatIgnoresTerm() throws {
        let pidFile = temporaryDirectory().appendingPathComponent("child.pid")
        let client = makeClient(promptTimeout: 2, environment: ["MOCK_CHILD_PID_FILE": pidFile.path])
        let ready = expectation(description: "ready")
        let exited = expectation(description: "exited")
        client.onExit = { _ in exited.fulfill() }
        client.start(options: options(sessionID: "group", resume: false), loading: false) { result in
            if case .failure(let error) = result { XCTFail(error.localizedDescription) }
            ready.fulfill()
        }
        wait(for: [ready], timeout: 3)
        let childPID = try XCTUnwrap(Int32(String(contentsOf: pidFile, encoding: .utf8)))
        addTeardownBlock { _ = Darwin.kill(childPID, SIGKILL) }
        XCTAssertEqual(Darwin.kill(childPID, 0), 0)
        client.close(activeTurn: false)
        wait(for: [exited], timeout: 4)
        for _ in 0..<20 where Darwin.kill(childPID, 0) == 0 { usleep(50_000) }
        XCTAssertEqual(Darwin.kill(childPID, 0), -1)
        XCTAssertEqual(errno, ESRCH)
    }

    func testActiveTurnShutdownIsBoundedWhenCancelAndCloseAreIgnored() {
        let fixture = repositoryRoot.appendingPathComponent("fixtures/mock-acp-v2.py")
        let neverRelease = temporaryDirectory().appendingPathComponent("never-release")
        let launch = ACPClient.LaunchOverride(
            executable: URL(fileURLWithPath: "/usr/bin/python3"),
            prefixArguments: [fixture.path, "--models", "--ignore-close", "--prompt-release=" + neverRelease.path]
        )
        let client = ACPClient(launchOverride: launch, requestTimeout: 2, promptTimeout: 30)
        let ready = expectation(description: "ready")
        client.start(options: options(sessionID: "ignored-shutdown", resume: false), loading: false) { result in
            if case .failure(let error) = result { XCTFail(error.localizedDescription) }
            ready.fulfill()
        }
        wait(for: [ready], timeout: 3)
        let promptSent = expectation(description: "prompt sent")
        client.prompt(text: "keep running", attachments: [], onSent: { promptSent.fulfill() }) { _ in }
        wait(for: [promptSent], timeout: 1)

        let started = Date()
        let closed = expectation(description: "bounded close")
        client.close(activeTurn: true) { closed.fulfill() }
        wait(for: [closed], timeout: 7)
        XCTAssertLessThan(Date().timeIntervalSince(started), 7)
    }

    func testLeaderCrashTerminatesLiveDescendantHoldingInheritedPipes() throws {
        let pidFile = temporaryDirectory().appendingPathComponent("crash-child.pid")
        let fixture = repositoryRoot.appendingPathComponent("fixtures/mock-acp-v2.py")
        let launch = ACPClient.LaunchOverride(
            executable: URL(fileURLWithPath: "/usr/bin/python3"),
            prefixArguments: [fixture.path, "--models", "--crash-after-new"],
            environment: ["MOCK_CHILD_PID_FILE": pidFile.path]
        )
        let client = ACPClient(launchOverride: launch, requestTimeout: 2, promptTimeout: 2)
        let ready = expectation(description: "ready before crash")
        let exited = expectation(description: "leader crashed")
        client.onExit = { status in
            XCTAssertEqual(status, 86)
            exited.fulfill()
        }
        client.start(options: options(sessionID: "crash-group", resume: false), loading: false) { result in
            if case .failure(let error) = result { XCTFail(error.localizedDescription) }
            ready.fulfill()
        }
        wait(for: [ready, exited], timeout: 5)

        let childPID = try XCTUnwrap(Int32(String(contentsOf: pidFile, encoding: .utf8)))
        addTeardownBlock { _ = Darwin.kill(childPID, SIGKILL) }
        for _ in 0..<40 where Darwin.kill(childPID, 0) == 0 { usleep(50_000) }
        XCTAssertEqual(Darwin.kill(childPID, 0), -1)
        XCTAssertEqual(errno, ESRCH)
    }

    @MainActor
    func testShutdownDuringStaleLockCloseDoesNotLaunchRetryHelper() async throws {
        let pidFile = temporaryDirectory().appendingPathComponent("shutdown-stale-helper-pids")
        let fixture = repositoryRoot.appendingPathComponent("fixtures/mock-acp-v2.py")
        let launch = ACPClient.LaunchOverride(
            executable: URL(fileURLWithPath: "/usr/bin/python3"),
            prefixArguments: [fixture.path, "--models", "--ignore-close"],
            environment: ["MOCK_STALE_LOCK": "1", "MOCK_STALE_LOCK_PID_FILE": pidFile.path]
        )
        let controller = ConversationController(
            conversation: Conversation(workspaceID: UUID(), sessionID: "shutdown-locked"),
            workspacePath: repositoryRoot.path,
            client: ACPClient(launchOverride: launch, requestTimeout: 2, promptTimeout: 2)
        )
        controller.start()
        try await waitUntil { FileManager.default.fileExists(atPath: pidFile.path) }
        try await Task.sleep(nanoseconds: 200_000_000)

        let closed = expectation(description: "closed without retry")
        controller.close { closed.fulfill() }
        await fulfillment(of: [closed], timeout: 5)
        try await Task.sleep(nanoseconds: 300_000_000)

        let launches = try String(contentsOf: pidFile, encoding: .utf8).split(whereSeparator: \.isNewline)
        XCTAssertEqual(launches.count, 1)
        XCTAssertTrue(launches[0].hasSuffix(":normal"))
    }

    func testListsEveryV2SessionCatalogPage() {
        let client = makeClient(promptTimeout: 2)
        let listed = expectation(description: "listed")
        var sessions: [ACPSessionInfo] = []

        client.listSessions(root: repositoryRoot.path) { result in
            do { sessions = try result.get() }
            catch { XCTFail(error.localizedDescription) }
            listed.fulfill()
        }

        wait(for: [listed], timeout: 4)
        XCTAssertEqual(sessions.map(\.sessionID), ["catalog-new", "catalog-old"])
        XCTAssertEqual(sessions.map(\.title), ["Newest session", "Older session"])
        XCTAssertTrue(sessions.allSatisfy { $0.updatedAt != nil })
    }

    @MainActor
    func testStaleLockRecoveryWaitsForFailedHelperThenForcesOneRetry() throws {
        let pidFile = temporaryDirectory().appendingPathComponent("stale-helper-pids")
        let client = makeClient(promptTimeout: 2, environment: [
            "MOCK_STALE_LOCK": "1", "MOCK_STALE_LOCK_PID_FILE": pidFile.path,
        ])
        let conversation = Conversation(workspaceID: UUID(), sessionID: "locked-session")
        let controller = ConversationController(
            conversation: conversation, workspacePath: repositoryRoot.path, client: client
        )
        let ready = expectation(description: "recovered")
        controller.onSessionReady = { sessionID, _ in
            XCTAssertEqual(sessionID, "locked-session")
            ready.fulfill()
        }

        controller.start()

        wait(for: [ready], timeout: 7)
        XCTAssertTrue(controller.isReady)
        XCTAssertFalse(controller.isRetryable)
        XCTAssertEqual(controller.entries.filter { $0.role == .user }.last?.text, "replayed user")
        let launches = try String(contentsOf: pidFile, encoding: .utf8).split(whereSeparator: \.isNewline)
        XCTAssertEqual(launches.count, 2)
        XCTAssertTrue(launches[0].hasSuffix(":normal"))
        XCTAssertTrue(launches[1].hasSuffix(":force"))
        let closed = expectation(description: "closed")
        controller.close { closed.fulfill() }
        wait(for: [closed], timeout: 3)
    }

    @MainActor
    func testRetryAfterNewSessionExitLoadsCanonicalReplayWithoutDuplicates() async throws {
        let fixture = repositoryRoot.appendingPathComponent("fixtures/mock-acp-v2.py")
        let launch = ACPClient.LaunchOverride(
            executable: URL(fileURLWithPath: "/usr/bin/python3"),
            prefixArguments: [fixture.path, "--models", "--exit-after-prompt"]
        )
        let controller = ConversationController(
            conversation: Conversation(workspaceID: UUID()), workspacePath: repositoryRoot.path,
            client: ACPClient(launchOverride: launch, requestTimeout: 2, promptTimeout: 2)
        )
        controller.start()
        try await waitUntil { controller.isReady }
        controller.draft = "exit and retry"
        controller.send()
        try await waitUntil { controller.isRetryable && !controller.isReady }

        controller.retryIfNeeded()
        try await waitUntil { controller.isReady && controller.status == "Continued session" }

        XCTAssertEqual(controller.entries.filter { $0.role == .user }.map(\.text), ["replayed user"])
        XCTAssertEqual(controller.entries.filter { $0.role == .assistant }.map(\.text), ["replayed assistant"])
        let closed = expectation(description: "closed")
        controller.close { closed.fulfill() }
        await fulfillment(of: [closed], timeout: 4)
    }


    func testRejectsNonV2Negotiation() {
        let fixture = repositoryRoot.appendingPathComponent("fixtures/mock-acp-v2.py")
        let launch = ACPClient.LaunchOverride(
            executable: URL(fileURLWithPath: "/usr/bin/python3"),
            prefixArguments: [fixture.path, "--protocol-version=1"]
        )
        let client = ACPClient(launchOverride: launch, requestTimeout: 2, promptTimeout: 2)
        let rejected = expectation(description: "negotiation rejected")
        client.start(options: options(sessionID: "bad-version", resume: false), loading: false) { result in
            guard case .failure(let error) = result else { XCTFail("expected rejection"); rejected.fulfill(); return }
            XCTAssertTrue(error.localizedDescription.contains("non-v2"))
            rejected.fulfill()
        }
        wait(for: [rejected], timeout: 3)
        client.close(activeTurn: false)
    }

    func testSlashPrefixedTextIsSubmittedUnchanged() throws {
        let log = temporaryDirectory().appendingPathComponent("requests.jsonl")
        let fixture = repositoryRoot.appendingPathComponent("fixtures/mock-acp-v2.py")
        let launch = ACPClient.LaunchOverride(
            executable: URL(fileURLWithPath: "/usr/bin/python3"),
            prefixArguments: [fixture.path, "--request-log=" + log.path]
        )
        let client = ACPClient(launchOverride: launch, requestTimeout: 2, promptTimeout: 2)
        let ready = expectation(description: "ready")
        client.start(options: options(sessionID: "slash", resume: false), loading: false) { _ in ready.fulfill() }
        wait(for: [ready], timeout: 3)

        let original = "/compact  keep this text exactly"
        let accepted = expectation(description: "accepted")
        client.prompt(text: original, attachments: []) { _ in accepted.fulfill() }
        wait(for: [accepted], timeout: 2)
        let records = try String(contentsOf: log, encoding: .utf8).split(whereSeparator: \.isNewline)
            .compactMap { try? JSONSerialization.jsonObject(with: Data($0.utf8)) as? [String: Any] }
        XCTAssertEqual(records.last(where: { $0["method"] as? String == "session/prompt" })?["text"] as? String, original)
        client.close(activeTurn: true)
    }

    @MainActor
    private func waitUntil(
        timeout: TimeInterval = 6, condition: @escaping @MainActor () -> Bool
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if condition() { return }
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        XCTFail("Condition was not met before timeout")
        throw NSError(domain: "ACPProcessTests", code: 1)
    }

    private func makeClient(
        promptTimeout: TimeInterval, environment: [String: String] = [:], fixtureArguments: [String] = ["--models"]
    ) -> ACPClient {
        let fixture = repositoryRoot.appendingPathComponent("fixtures/mock-acp-v2.py")
        let launch = ACPClient.LaunchOverride(executable: URL(fileURLWithPath: "/usr/bin/python3"), prefixArguments: [fixture.path] + fixtureArguments, environment: environment)
        return ACPClient(launchOverride: launch, requestTimeout: 2, promptTimeout: promptTimeout)
    }

    private func options(sessionID: String, resume: Bool) -> ACPLaunchOptions {
        ACPLaunchOptions(root: repositoryRoot.path, sessionID: sessionID, resume: resume, provider: "openai-subscription", model: "gpt-5.4", reasoningEffort: "default")
    }

    private var repositoryRoot: URL {
        URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    }

    private func temporaryDirectory() -> URL {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }
}
