import XCTest
@testable import Kit

final class FocusedSubagentTests: XCTestCase {
    @MainActor
    func testReplayRejectsStaleFocusGenerationAndInvalidCursor() throws {
        let transport = ChildTransport()
        let controller = FocusedSubagentController(transport: transport)
        controller.focus(row(), canSteer: true)
        let stale = transport.reads[0].completion
        controller.focus(row(generation: 2), canSteer: false)
        stale(.success(page(generation: 1, cursor: 10, text: "stale")))
        XCTAssertTrue(controller.entries.isEmpty)
        transport.reads[1].completion(.success(page(generation: 1, cursor: 10, text: "wrong generation")))
        XCTAssertTrue(controller.entries.isEmpty)
        XCTAssertFalse(controller.canSteer)
        XCTAssertTrue(controller.notice.contains("generation or cursor"))
        controller.focus(row(), canSteer: true)
        transport.reads.last!.completion(.success(page(generation: 1, cursor: 0, text: "invalid cursor")))
        XCTAssertTrue(controller.entries.isEmpty)
        XCTAssertFalse(controller.canSteer)
    }

    @MainActor
    func testCaughtUpStartingChildResumesPollingWhenWorking() {
        let transport = ChildTransport()
        let controller = FocusedSubagentController(transport: transport)
        controller.focus(row(status: .starting), canSteer: true)
        transport.reads[0].completion(.success(SubagentTranscriptPage(updates: [], nextCursor: 0, generation: 1, caughtUp: true)))
        XCTAssertFalse(controller.loading)
        XCTAssertFalse(controller.canSteer)
        controller.update(row(status: .working), canSteer: true)
        XCTAssertEqual(transport.reads.count, 2)
        controller.update(row(status: .working), canSteer: true)
        XCTAssertEqual(transport.reads.count, 2)
        transport.reads[1].completion(.success(page(generation: 1, cursor: 10, text: "Child output after startup")))
        XCTAssertEqual(controller.entries.first?.text, "Child output after startup")
        XCTAssertEqual(controller.cursor, 10)
        XCTAssertTrue(controller.canSteer)
        controller.stop()
    }

    @MainActor
    func testSteeringCapabilityLifecycleAndStaleAcknowledgement() {
        let transport = ChildTransport()
        let controller = FocusedSubagentController(transport: transport)
        controller.focus(row(), canSteer: false)
        controller.draft = "A child instruction"
        controller.send()
        XCTAssertTrue(transport.steers.isEmpty)
        controller.update(row(), canSteer: true)
        controller.send()
        XCTAssertEqual(transport.steers.first?.prompt, "A child instruction")
        XCTAssertTrue(controller.steering)
        controller.send()
        XCTAssertEqual(transport.steers.count, 1)
        controller.focus(row(generation: 2), canSteer: true)
        controller.draft = "New generation draft"
        transport.steers[0].completion(.success(["receipt": [:]]))
        XCTAssertEqual(controller.draft, "New generation draft")
        controller.update(row(generation: 2, status: .idle), canSteer: true)
        XCTAssertFalse(controller.canSteer)
        controller.stop()
    }

    @MainActor
    func testReplayIsBoundedAndDescendantsAreReadOnly() {
        let transport = ChildTransport()
        let controller = FocusedSubagentController(transport: transport)
        controller.focus(row(status: .idle), canSteer: true)
        let updates = (0..<505).map { index in message(id: String(index), text: String(repeating: "x", count: 17_000)) }
        transport.reads[0].completion(.success(SubagentTranscriptPage(updates: updates, nextCursor: 20, generation: 1, caughtUp: true)))
        XCTAssertEqual(controller.entries.count, FocusedSubagentController.maximumEntries)
        XCTAssertTrue(controller.entries.allSatisfy { $0.text.count <= FocusedSubagentController.maximumTextCharacters })
        XCTAssertTrue(controller.partial)
        XCTAssertFalse(controller.canSteer)
        XCTAssertEqual(controller.cursor, 20)
        controller.focus(row(parent: "ancestor"), canSteer: true)
        XCTAssertEqual(transport.reads.count, 1)
        XCTAssertFalse(controller.canSteer)
        XCTAssertTrue(controller.notice.contains("Descendant"))
    }

    @MainActor
    func testClosingRejectsReplayAndPreservesFailedSteerDraft() {
        let transport = ChildTransport()
        let controller = FocusedSubagentController(transport: transport)
        controller.focus(row(), canSteer: true)
        controller.draft = "Keep this draft"
        controller.send()
        transport.steers[0].completion(.failure(ACPClientError.protocolError("child stopped")))
        XCTAssertEqual(controller.draft, "Keep this draft")
        XCTAssertFalse(controller.steering)
        controller.stop()
        transport.reads[0].completion(.success(page(generation: 1, cursor: 20, text: "late")))
        XCTAssertTrue(controller.entries.isEmpty)
    }

    @MainActor
    func testToolContentChunksAppendAndStatusPatchPreservesOutput() {
        let transport = ChildTransport()
        let controller = FocusedSubagentController(transport: transport)
        controller.focus(row(status: .idle), canSteer: false)
        let updates: [JSONValue] = [
            .object(["sessionUpdate": .string("tool_call"), "toolCallId": .string("call"), "title": .string("Shell"), "status": .string("in_progress")]),
            .object(["sessionUpdate": .string("tool_call_content_chunk"), "toolCallId": .string("call"), "content": .string("first chunk")]),
            .object(["sessionUpdate": .string("tool_call_content_chunk"), "toolCallId": .string("call"), "content": .string("second chunk")]),
            .object(["sessionUpdate": .string("tool_call_update"), "toolCallId": .string("call"), "status": .string("completed")]),
        ]
        transport.reads[0].completion(.success(SubagentTranscriptPage(updates: updates, nextCursor: 30, generation: 1, caughtUp: true)))
        XCTAssertEqual(controller.entries.count, 1)
        XCTAssertEqual(controller.entries[0].text, "\"first chunk\"\n\"second chunk\"")
        XCTAssertTrue(controller.entries[0].title.contains("completed"))
        XCTAssertFalse(controller.partial)
    }

    func testModelSelectionUsesCurrentAdvertisement() {
        let option = ConfigOption(id: "model", name: "Model", currentValue: "a", groups: [ConfigGroup(id: "provider", name: "Provider", choices: [ConfigChoice(value: "a", name: "A"), ConfigChoice(value: "b", name: "B")])])
        XCTAssertNotNil(ModelSelection.option(in: [option], id: "model", value: "b", disabled: false))
        XCTAssertNil(ModelSelection.option(in: [option], id: "model", value: "b", disabled: true))
        XCTAssertNil(ModelSelection.option(in: [option], id: "model", value: "a", disabled: false))
        XCTAssertNil(ModelSelection.option(in: [option], id: "model", value: "removed", disabled: false))
        XCTAssertNil(ModelSelection.option(in: [], id: "model", value: "b", disabled: false))
    }

    func testChildProtocolFixtureUsesSeparateReplayAndGenerationCheckedSteering() {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let client = ACPClient(launchOverride: .init(executable: URL(fileURLWithPath: "/usr/bin/python3"), prefixArguments: [root.appendingPathComponent("fixtures/mock-acp-v2.py").path, "--models"]))
        var parentMessages = 0
        client.onUpdate = { update in
            if case .agentMessage = update { parentMessages += 1 }
        }
        let ready = expectation(description: "ready")
        client.start(options: ACPLaunchOptions(root: root.path, sessionID: "child-test", resume: false, provider: nil, model: nil, reasoningEffort: nil), loading: false) { result in
            if case .failure(let error) = result { XCTFail(error.localizedDescription) }
            ready.fulfill()
        }
        wait(for: [ready], timeout: 3)
        let read = expectation(description: "child replay")
        client.readSubagentTranscript(id: "fixture-child", generation: 1, cursor: 0) { result in
            switch result {
            case .success(let page):
                XCTAssertEqual(page.generation, 1)
                XCTAssertEqual(page.nextCursor, 128)
                XCTAssertEqual(page.updates.count, 1)
                XCTAssertTrue(page.caughtUp)
            case .failure(let error): XCTFail(error.localizedDescription)
            }
            read.fulfill()
        }
        let steer = expectation(description: "child steer")
        client.steerSubagent(id: "fixture-child", generation: 1, prompt: "Continue") { result in
            if case .failure(let error) = result { XCTFail(error.localizedDescription) }
            steer.fulfill()
        }
        let stale = expectation(description: "stale generation")
        client.steerSubagent(id: "fixture-child", generation: 2, prompt: "Stale") { result in
            if case .success = result { XCTFail("Stale generation must fail") }
            stale.fulfill()
        }
        wait(for: [read, steer, stale], timeout: 3)
        XCTAssertEqual(parentMessages, 0, "Child replay must not enter the parent update stream")
        let caughtUp = expectation(description: "caught up cursor")
        client.readSubagentTranscript(id: "fixture-child", generation: 1, cursor: 128) { result in
            switch result {
            case .success(let page):
                XCTAssertTrue(page.updates.isEmpty)
                XCTAssertEqual(page.nextCursor, 128)
            case .failure(let error): XCTFail(error.localizedDescription)
            }
            caughtUp.fulfill()
        }
        wait(for: [caughtUp], timeout: 3)
        let closed = expectation(description: "closed")
        client.close(activeTurn: false) { closed.fulfill() }
        wait(for: [closed], timeout: 3)
    }

    private func row(generation: UInt64 = 1, status: SubagentStatus = .working, parent: String? = nil) -> AgentRosterRow {
        AgentRosterRow(id: "child", name: "Child", status: status, outcome: nil, generation: generation, task: "Work", parentID: parent, parentName: nil, harness: "acp.kit", model: nil, createdAtMS: 1, generationStartedAtMS: 1, generationFinishedAtMS: nil)
    }
    private func message(id: String = "message", text: String) -> JSONValue {
        .object(["sessionUpdate": .string("agent_message_chunk"), "messageId": .string(id), "content": .object(["type": .string("text"), "text": .string(text)])])
    }
    private func page(generation: UInt64, cursor: UInt64, text: String) -> SubagentTranscriptPage {
        SubagentTranscriptPage(updates: [message(text: text)], nextCursor: cursor, generation: generation, caughtUp: true)
    }
}

private final class ChildTransport: FocusedSubagentTransport {
    struct Read {
        let completion: (Result<SubagentTranscriptPage, Error>) -> Void
    }
    struct Steer {
        let prompt: String
        let completion: (Result<[String: Any], Error>) -> Void
    }
    var reads: [Read] = []
    var steers: [Steer] = []
    func readSubagentTranscript(id: String, generation: UInt64, cursor: UInt64, completion: @escaping (Result<SubagentTranscriptPage, Error>) -> Void) {
        reads.append(Read(completion: completion))
    }
    func steerSubagent(id: String, generation: UInt64, prompt: String, completion: @escaping (Result<[String: Any], Error>) -> Void) {
        steers.append(Steer(prompt: prompt, completion: completion))
    }
}
