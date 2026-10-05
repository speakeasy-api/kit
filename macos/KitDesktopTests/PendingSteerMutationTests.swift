import XCTest
@testable import Kit

final class PendingSteerMutationTests: XCTestCase {
    @MainActor
    func testWireMessageIDsAreScopedByRoleAndProjectionMatchesController() async throws {
        let controller = makeController(arguments: [])
        controller.start()
        try await waitUntil { controller.isReady }
        controller.draft = "MOCK_SHARED_MESSAGE_IDS"
        controller.send()
        try await waitUntil { controller.entries.contains { $0.role == .assistant && $0.text == "Assistant text" } && !controller.isRunning }
        XCTAssertTrue(controller.entries.contains { $0.role == .user && $0.text == "User text" })
        XCTAssertTrue(controller.entries.contains { $0.role == .thought && $0.text == "Thought text" })
        XCTAssertEqual(controller.transcriptProjection.items.flatMap(\.entries).map(\.id), controller.entries.map(\.id))
        XCTAssertEqual(controller.transcriptProjection.items.flatMap(\.entries).map(\.text), controller.entries.map(\.text))
        XCTAssertTrue(controller.transcriptProjection.items.filter { !$0.isActivity }.flatMap(\.entries).contains { $0.text == "Assistant text" })
        await close(controller)
    }

    @MainActor
    func testPendingTextCanBeEditedRejectedAndRevoked() async throws {
        let delivery = temporaryDirectory().appendingPathComponent("deliver")
        let controller = makeController(arguments: ["--steer", "--pending-replace", "--inject-release=\(delivery.path)"])
        controller.start()
        try await waitUntil { controller.isReady }
        controller.draft = "MOCK_HANG"
        controller.send()
        try await waitUntil { controller.canSteer }
        XCTAssertTrue(controller.supportsPendingSteerEdit)
        controller.draft = "original"
        controller.send()
        try await waitUntil { controller.pendingSteers.count == 1 }
        let id = try XCTUnwrap(controller.pendingSteers.first?.id)
        controller.replacePendingSteer(id: id, text: "edited")
        try await waitUntil { controller.pendingSteers.first?.text == "edited" && controller.isMutatingSteerIDs.isEmpty }
        controller.replacePendingSteer(id: id, text: "MOCK_REJECT_REPLACE")
        try await waitUntil { controller.isMutatingSteerIDs.isEmpty && controller.entries.contains { $0.role == .error } }
        XCTAssertEqual(controller.pendingSteers.first?.text, "edited")
        controller.revokePendingSteer(id: id)
        try await waitUntil { controller.pendingSteers.isEmpty }
        try Data().write(to: delivery)
        await close(controller)
        XCTAssertFalse(controller.entries.contains { $0.role == .user && $0.text == "edited" })
    }

    @MainActor
    func testDeliveryWinsOverDelayedReplacementReply() async throws {
        let directory = temporaryDirectory()
        let delivery = directory.appendingPathComponent("deliver")
        let acknowledgment = directory.appendingPathComponent("ack")
        let controller = makeController(arguments: ["--steer", "--pending-replace", "--inject-release=\(delivery.path)", "--mutation-ack-release=\(acknowledgment.path)", "--mutation-committed=\(directory.appendingPathComponent("committed").path)"])
        controller.start()
        try await waitUntil { controller.isReady }
        controller.draft = "MOCK_HANG"; controller.send()
        try await waitUntil { controller.canSteer }
        controller.draft = "original"; controller.send()
        try await waitUntil { controller.pendingSteers.count == 1 }
        let id = try XCTUnwrap(controller.pendingSteers.first?.id)
        controller.replacePendingSteer(id: id, text: "edited")
        XCTAssertTrue(controller.isMutatingSteerIDs.contains(id))
        try await waitUntil { FileManager.default.fileExists(atPath: directory.appendingPathComponent("committed").path) }
        try Data().write(to: delivery)
        try await waitUntil { controller.entries.contains { $0.role == .user && $0.text == "edited" } }
        XCTAssertTrue(controller.pendingSteers.isEmpty)
        try Data().write(to: acknowledgment)
        try await waitUntil { controller.entries.contains { $0.text == "Mutation acknowledged" } }
        XCTAssertTrue(controller.pendingSteers.isEmpty)
        XCTAssertTrue(controller.isMutatingSteerIDs.isEmpty)
        await close(controller)
    }

    @MainActor
    func testDelayedInjectionAcknowledgmentCannotEnterNextTurn() async throws {
        let directory = temporaryDirectory()
        let prompt = directory.appendingPathComponent("prompt")
        let acknowledgment = directory.appendingPathComponent("ack")
        let delivery = directory.appendingPathComponent("deliver")
        let controller = makeController(arguments: ["--steer", "--prompt-release=\(prompt.path)", "--prompt-release-text=first", "--inject-ack-release=\(acknowledgment.path)", "--inject-release=\(delivery.path)"])
        controller.start()
        try await waitUntil { controller.isReady }
        controller.draft = "first"; controller.send()
        try await waitUntil { controller.canSteer }
        controller.draft = "old queued prompt"; controller.send()
        XCTAssertTrue(controller.isInjecting)
        try Data().write(to: prompt)
        try await waitUntil { !controller.isRunning }
        controller.draft = "MOCK_HANG"; controller.send()
        try await waitUntil { controller.canSteer }
        try Data().write(to: acknowledgment)
        try await waitUntil { controller.entries.contains { $0.text == "Injection acknowledged" } }
        XCTAssertTrue(controller.pendingSteers.isEmpty)
        XCTAssertFalse(controller.isInjecting)
        await close(controller)
    }

    @MainActor
    private func makeController(arguments: [String]) -> ConversationController {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let launch = ACPClient.LaunchOverride(executable: URL(fileURLWithPath: "/usr/bin/python3"), prefixArguments: [root.appendingPathComponent("fixtures/mock-acp-v2.py").path] + arguments)
        return ConversationController(conversation: Conversation(workspaceID: UUID()), workspacePath: root.path, client: ACPClient(launchOverride: launch, requestTimeout: 3, promptTimeout: 3))
    }

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
        throw NSError(domain: "PendingSteerMutationTests", code: 1)
    }

    private func temporaryDirectory() -> URL {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }
}
