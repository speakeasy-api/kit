import XCTest
import Combine
@testable import Kit

final class FocusedSubagentTests: XCTestCase {
    func testModelSelectionUsesCurrentAdvertisement() {
        let option = ConfigOption(id: "model", name: "Model", currentValue: "a", groups: [ConfigGroup(id: "provider", name: "Provider", choices: [ConfigChoice(value: "a", name: "A"), ConfigChoice(value: "b", name: "B")])])
        XCTAssertNotNil(ModelSelection.option(in: [option], id: "model", value: "b", disabled: false))
        XCTAssertNil(ModelSelection.option(in: [option], id: "model", value: "b", disabled: true))
        XCTAssertNil(ModelSelection.option(in: [option], id: "model", value: "a", disabled: false))
        XCTAssertNil(ModelSelection.option(in: [option], id: "model", value: "removed", disabled: false))
        XCTAssertNil(ModelSelection.option(in: [], id: "model", value: "b", disabled: false))
    }


    @MainActor
    func testToolContentChunksAppendAndStatusPatchPreservesOutput() {
        let transport = ChildTransport()
        let controller = FocusedSubagentController(transport: transport)
        transport.childSessions.receive(root: "root", session: "root", update: announcement(state: "idle"))
        controller.focus(transport.childSessions.roster.rowsByID["child"]!, canSteer: false)
        let updates: [JSONValue] = [
            .object(["sessionUpdate": .string("tool_call"), "toolCallId": .string("call"), "title": .string("Shell"), "status": .string("in_progress")]),
            .object(["sessionUpdate": .string("tool_call_content_chunk"), "toolCallId": .string("call"), "content": .string("first chunk")]),
            .object(["sessionUpdate": .string("tool_call_content_chunk"), "toolCallId": .string("call"), "content": .string("second chunk")]),
            .object(["sessionUpdate": .string("tool_call_update"), "toolCallId": .string("call"), "status": .string("completed")]),
        ]
        for value in updates {
            let data = try! JSONEncoder().encode(value)
            let update = try! JSONSerialization.jsonObject(with: data) as! [String: Any]
            transport.childSessions.receive(root: "root", session: "child", update: update)
        }
        XCTAssertEqual(controller.entries.count, 1)
        XCTAssertEqual(controller.entries[0].text, "\"first chunk\"\n\"second chunk\"")
        XCTAssertTrue(controller.entries[0].title.contains("completed"))
        XCTAssertTrue(controller.partial)
    }


    private func announcement(_ id: String = "child", generation: Int = 1, steer: Bool = true, state: String = "running") -> [String: Any] {
        ["sessionUpdate": "subagent_update", "sessionId": id, "title": "Child", "state": ["state": state],
         "_meta": ["kit/subagent/steer": ["generation": generation, "canSteer": steer]]]
    }
    private func message(_ text: String) -> [String: Any] {
        ["sessionUpdate": "agent_message_chunk", "messageId": "m", "content": ["type": "text", "text": text]]
    }

    @MainActor
    func testRoutedNotificationsAndImmutableParentFirstOwnership() {
        let store = DesktopChildSessions()
        XCTAssertFalse(store.receive(root: "root", session: "unknown", update: announcement()))
        XCTAssertFalse(store.receive(root: "root", session: "child", update: message("early")))
        XCTAssertTrue(store.receive(root: "root", session: "root", update: announcement()))
        XCTAssertTrue(store.receive(root: "root", session: "child", update: announcement("nested")))
        XCTAssertFalse(store.receive(root: "root", session: "root", update: announcement("nested")))
        XCTAssertFalse(store.receive(root: "root", session: "nested", update: announcement("root")))
        XCTAssertFalse(store.receive(root: "root", session: "nested", update: announcement()))
        XCTAssertFalse(store.roster.canSteer(id: "nested", generation: 1))
        XCTAssertTrue(store.receive(root: "root", session: "nested", update: message("nested output")))
        XCTAssertEqual(store.history["nested"]?.count, 1)
        XCTAssertNil(store.history["root"])
    }

    @MainActor
    func testSteeringRequiresFreshExactGenerationGrant() {
        let store = DesktopChildSessions()
        store.receive(root: "root", session: "root", update: announcement(generation: 2))
        XCTAssertTrue(store.roster.canSteer(id: "child", generation: 2))
        store.receive(root: "root", session: "root", update: announcement(generation: 1))
        XCTAssertEqual(store.roster.rowsByID["child"]?.generation, 2)
        store.receive(root: "root", session: "root", update: ["sessionUpdate": "subagent_update", "sessionId": "child", "state": ["state": "idle"]])
        store.receive(root: "root", session: "root", update: ["sessionUpdate": "subagent_update", "sessionId": "child", "state": ["state": "running"], "capabilities": ["cancel": [:]]])
        XCTAssertFalse(store.roster.canSteer(id: "child", generation: 2))
        store.receive(root: "root", session: "root", update: announcement(generation: 2))
        store.receive(root: "root", session: "root", update: ["sessionUpdate": "subagent_update", "sessionId": "child", "_meta": NSNull()])
        XCTAssertFalse(store.roster.canSteer(id: "child", generation: 2))
        store.receive(root: "root", session: "root", update: announcement(generation: 2))
        store.receive(root: "root", session: "child", update: ["sessionUpdate": "state_update", "state": "idle"])
        XCTAssertFalse(store.roster.canSteer(id: "child", generation: 2))
        store.receive(root: "root", session: "root", update: ["sessionUpdate": "subagent_update", "sessionId": "child", "state": ["state": "running"], "_meta": ["kit/subagent/steer": ["generation": 2, "canSteer": 1]]])
        XCTAssertFalse(store.roster.canSteer(id: "child", generation: 2))
        store.reset()
        XCTAssertFalse(store.receive(root: "root", session: "child", update: message("late")))
        store.receive(root: "root", session: "root", update: announcement(steer: false))
        XCTAssertFalse(store.roster.canSteer(id: "child", generation: 1))
    }

    @MainActor
    func testLiveAndRetainedHistorySessionMessagesAndSteerFailure() {
        let transport = ChildTransport()
        let store = transport.childSessions
        store.receive(root: "root", session: "root", update: announcement())
        store.receive(root: "root", session: "child", update: message("retained"))
        let controller = FocusedSubagentController(transport: transport)
        controller.focus(store.roster.rowsByID["child"]!, canSteer: true)
        store.receive(root: "root", session: "child", update: message(" live"))
        XCTAssertEqual(controller.entries.first?.text, "retained live")
        store.receive(root: "root", session: "child", update: ["sessionUpdate": "session_message", "messageId": "directed", "senderSessionId": "root", "recipientSessionId": "child", "content": [["type": "text", "text": "instruction"]]])
        XCTAssertEqual(controller.entries.last?.title, "From root")
        controller.draft = "Keep draft"
        controller.send()
        transport.completion?(.failure(ACPClientError.protocolError("stopped")))
        XCTAssertEqual(controller.draft, "Keep draft")
        controller.disconnected()
        XCTAssertFalse(controller.canSteer)
        store.receive(root: "root", session: "child", update: message("late"))
        XCTAssertEqual(controller.entries.first?.text, "retained live")
    }

    @MainActor
    func testACPClientRoutesChildNotificationsWithoutTranscriptRPC() async {
        let script = #"""
import json, sys

def send(value):
    print(json.dumps(value), flush=True)
def update(session, value):
    send({"jsonrpc":"2.0", "method":"session/update", "params":{"sessionId":session, "update":value}})
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if "id" not in request: continue
    result = {}
    if method == "initialize":
        result = {"protocolVersion":2,"info":{"name":"child-fixture","version":"1"},"capabilities":{}}
    elif method == "session/new": result = {"sessionId":"root"}
    elif method == "kit/subagent/steer" and request["params"]["generation"] != 1:
        send({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32602,"message":"stale generation"}})
        continue
    elif method == "kit/subagent/transcript/read":
        raise RuntimeError("Removed RPC must never be called")
    send({"jsonrpc":"2.0","id":request["id"],"result":result})
    if method == "session/new":
        update("root", {"sessionUpdate":"subagent_update","sessionId":"child","state":{"state":"running"},"_meta":{"kit/subagent/steer":{"generation":1,"canSteer":True}}})
        update("unrelated", {"sessionUpdate":"subagent_update","sessionId":"intruder"})
        update("unrelated", {"sessionUpdate":"agent_message_chunk","messageId":"bad","content":{"type":"text","text":"bad"}})
        update("child", {"sessionUpdate":"agent_message_chunk","messageId":"m","content":{"type":"text","text":"child output"}})
        update("root", {"sessionUpdate":"session_message","messageId":"directed","senderSessionId":"root","recipientSessionId":"child","content":[{"type":"text","text":"Delegate"}]})
        update("root", {"sessionUpdate":"session_message_chunk","messageId":"directed","content":{"type":"text","text":" now"}})
        update("child", {"sessionUpdate":"session_message","messageId":"directed","senderSessionId":"child","recipientSessionId":"root","content":[{"type":"text","text":"Child reply"}]})
        update("unrelated", {"sessionUpdate":"session_message","messageId":"intruder","senderSessionId":"root","content":[{"type":"text","text":"bad"}]})
        update("root", {"sessionUpdate":"session_message","messageId":"reply","senderSessionId":"child","recipientSessionId":"root","content":[{"type":"text","text":"Reply"}]})
        update("root", {"sessionUpdate":"session_message_chunk","messageId":"reply","content":{"type":"text","text":" done"}})
"""#
        let client = ACPClient(launchOverride: .init(executable: URL(fileURLWithPath: "/usr/bin/python3"), prefixArguments: ["-c", script]))
        let parent = ConversationController(conversation: Conversation(workspaceID: UUID()), workspacePath: NSTemporaryDirectory(), client: client)
        let ready = expectation(description: "ready")
        let received = expectation(description: "root directed reply")
        parent.onSessionReady = { _, _ in ready.fulfill() }
        var delivered = false
        let observation = parent.$transcriptRevision.sink { _ in
            if !delivered, parent.entries.contains(where: { $0.text == "Reply done" }) {
                delivered = true
                received.fulfill()
            }
        }
        parent.start()
        await fulfillment(of: [ready, received], timeout: 5)
        observation.cancel()
        XCTAssertEqual(parent.entries.map(\.text), ["Delegate now", "Reply done"])
        XCTAssertEqual(parent.entries.map(\.title), ["To child", "From child"])
        XCTAssertTrue(parent.entries.allSatisfy { $0.role == .status })
        let child = FocusedSubagentController(transport: client)
        child.focus(client.childSessions.roster.rowsByID["child"]!, canSteer: true)
        XCTAssertEqual(child.entries.map(\.text), ["child output", "Child reply"])
        XCTAssertEqual(child.entries.last?.title, "To root")
        XCTAssertNil(client.childSessions.roster.rowsByID["intruder"])
        let accepted = expectation(description: "steer accepted")
        client.steerSubagent(id: "child", generation: 1, prompt: "Continue") { result in
            if case .failure(let error) = result { XCTFail(error.localizedDescription) }
            accepted.fulfill()
        }
        let stale = expectation(description: "stale steer")
        client.steerSubagent(id: "child", generation: 2, prompt: "Stale") { result in
            if case .success = result { XCTFail("Stale generation accepted") }
            stale.fulfill()
        }
        await fulfillment(of: [accepted, stale], timeout: 5)
        let closed = expectation(description: "closed")
        parent.close { closed.fulfill() }
        await fulfillment(of: [closed], timeout: 5)
        XCTAssertTrue(client.childSessions.history.isEmpty)
    }

    @MainActor
    func testDirectedMessagePatchesRetainAndExplicitlyClearIdentity() {
        let transport = ChildTransport()
        transport.childSessions.receive(root: "root", session: "root", update: announcement())
        let child = FocusedSubagentController(transport: transport)
        child.focus(transport.childSessions.roster.rowsByID["child"]!, canSteer: false)
        func patch(_ fields: [String: Any]) {
            var update = fields
            update["sessionUpdate"] = "session_message"
            update["messageId"] = "m"
            transport.childSessions.receive(root: "root", session: "child", update: update)
        }
        patch(["senderSessionId": "child", "recipientSessionId": "root", "content": [["type": "text", "text": "Retained"]]])
        patch(["recipientSessionId": "sibling"])
        XCTAssertEqual(child.entries.last?.title, "To sibling")
        XCTAssertEqual(child.entries.last?.text, "Retained")
        patch(["recipientSessionId": NSNull()])
        XCTAssertEqual(child.entries.last?.title, "To session")
        patch(["senderSessionId": NSNull()])
        XCTAssertEqual(child.entries.last?.title, "Session message")
        patch(["senderSessionId": "root"])
        XCTAssertEqual(child.entries.last?.title, "From root")
        patch(["senderSessionId": NSNull(), "content": NSNull()])
        XCTAssertEqual(child.entries.last?.title, "Session message")
        XCTAssertEqual(child.entries.last?.text, "")
    }

    func testDirectedReducerScopesIDsAndBoundsRetainedPatchState() {
        var messages = DesktopSessionMessages()
        func wire(_ id: String, sender: String? = nil) -> JSONValue {
            var fields: [String: JSONValue] = ["sessionUpdate": .string("session_message"), "messageId": .string(id)]
            if let sender { fields["senderSessionId"] = .string(sender) }
            return .object(fields)
        }
        _ = messages.apply(wire("same", sender: "root"), session: "root")
        _ = messages.apply(wire("same", sender: "other"), session: "child")
        XCTAssertEqual(messages.apply(wire("same"), session: "root")?.title, "To session")
        XCTAssertEqual(messages.apply(wire("same"), session: "child")?.title, "From other")
        for index in 0..<DesktopSessionMessages.maximumMessages {
            _ = messages.apply(wire(String(index), sender: "root"), session: "root")
        }
        XCTAssertEqual(messages.apply(wire("same"), session: "root")?.title, "Session message")
    }

    @MainActor
    func testHistoryAndFocusedEntriesAreBounded() {
        let transport = ChildTransport()
        let store = transport.childSessions
        store.receive(root: "root", session: "root", update: announcement())
        for index in 0..<600 {
            var update = message(String(repeating: "x", count: 20_000))
            update["messageId"] = String(index)
            store.receive(root: "root", session: "child", update: update)
        }
        XCTAssertLessThanOrEqual(store.history["child"]?.count ?? 0, 512)
        let controller = FocusedSubagentController(transport: transport)
        controller.focus(store.roster.rowsByID["child"]!, canSteer: false)
        XCTAssertLessThanOrEqual(controller.entries.count, FocusedSubagentController.maximumEntries)
        XCTAssertTrue(controller.entries.allSatisfy { $0.text.count <= FocusedSubagentController.maximumTextCharacters })
        XCTAssertTrue(controller.partial)
    }
}

private final class ChildTransport: FocusedSubagentTransport {
    let childSessions = DesktopChildSessions()
    var completion: ((Result<[String: Any], Error>) -> Void)?
    func steerSubagent(id: String, generation: UInt64, prompt: String, completion: @escaping (Result<[String: Any], Error>) -> Void) {
        self.completion = completion
    }
}
