import Foundation
import XCTest
@testable import Kit

final class ACPProtocolTests: XCTestCase {
    func testLaunchArgumentsInheritKitConfigUntilUserOverrides() {
        let inherited = ACPLaunchOptions(
            root: "/tmp/project", sessionID: "session", resume: false,
            provider: nil, model: nil, reasoningEffort: nil
        )
        let inheritedArguments = ACPClient.commandArguments(options: inherited)
        XCTAssertFalse(inheritedArguments.contains("--provider"))
        XCTAssertFalse(inheritedArguments.contains("--model"))
        XCTAssertFalse(inheritedArguments.contains("--reasoning-effort"))

        let explicit = ACPLaunchOptions(
            root: "/tmp/project", sessionID: "session", resume: true,
            provider: "openai-subscription", model: "gpt-5.6-sol", reasoningEffort: "high"
        )
        XCTAssertEqual(ACPClient.commandArguments(options: explicit), [
            "serve", "--stdio-protocol-version", "2", "--root", "/tmp/project",
            "--model", "gpt-5.6-sol",
            "--provider", "openai-subscription",
            "--reasoning-effort", "high",
            "--session-id", "session", "--resume",
        ])

        let forced = ACPLaunchOptions(
            root: "/tmp/project", sessionID: "session", resume: true,
            provider: nil, model: nil, reasoningEffort: nil, force: true
        )
        XCTAssertEqual(ACPClient.commandArguments(options: forced).suffix(3), ["session", "--resume", "--force"])
        XCTAssertEqual(ACPClient.catalogCommandArguments(root: "/tmp/project"), [
            "serve", "--stdio-protocol-version", "2", "--root", "/tmp/project",
        ])
    }

    func testParsesV2SessionListMetadata() throws {
        let page = try ACPClient.parseSessionPage([
            "sessions": [[
                "sessionId": "s-123", "title": "Canonical title",
                "updatedAt": "2026-08-30T12:34:56.123Z",
                "_meta": ["dev.kit.subagent": true],
            ]],
            "nextCursor": "offset:100",
        ])

        XCTAssertEqual(page.sessions.map(\.sessionID), ["s-123"])
        XCTAssertEqual(page.sessions.first?.title, "Canonical title")
        XCTAssertNotNil(page.sessions.first?.updatedAt)
        XCTAssertEqual(page.sessions.first?.isSubagent, true)
        XCTAssertEqual(page.nextCursor, "offset:100")
    }

    func testFindsCargoInstalledKitWhenGUIPathIsMinimal() throws {
        let home = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        let cargoBin = home.appendingPathComponent(".cargo/bin", isDirectory: true)
        let executable = cargoBin.appendingPathComponent("kit")
        try FileManager.default.createDirectory(at: cargoBin, withIntermediateDirectories: true)
        try Data("#!/bin/sh\n".utf8).write(to: executable)
        XCTAssertEqual(chmod(executable.path, 0o755), 0)
        defer { try? FileManager.default.removeItem(at: home) }

        let resolved = ACPClient.installedExecutable(environment: [
            "HOME": home.path, "PATH": "/usr/bin:/bin",
        ])

        XCTAssertEqual(resolved?.standardizedFileURL, executable.standardizedFileURL)
    }

    func testParsesSessionUpdateNotification() throws {
        let data = Data(#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}}}"#.utf8)
        let envelope = try RPCEnvelope.parse(data)
        XCTAssertEqual(envelope.method, "session/update")
        let update = try XCTUnwrap(envelope.params?["update"] as? [String: Any])
        XCTAssertEqual(update["sessionUpdate"] as? String, "agent_message_chunk")
    }

    func testDecodesTypedDesktopUpdatesAndUsesACPVersionTwo() throws {
        let data = Data(#"{"sessionUpdate":"agent_message_chunk","messageId":"m1","content":{"type":"image","data":"aGVsbG8=","mimeType":"image/png","uri":"file:///preview.png"}}"#.utf8)
        let wire = try JSONDecoder().decode(JSONValue.self, from: data)
        let update = try DesktopUpdate(wire: wire)
        guard case .agentMessage(let message) = update, case .image(let payload, let mimeType, let uri) = message.content.first else {
            return XCTFail("expected typed image update")
        }
        XCTAssertEqual(message.messageId, "m1")
        XCTAssertEqual(payload, "aGVsbG8=")
        XCTAssertEqual(mimeType, "image/png")
        XCTAssertEqual(uri, "file:///preview.png")
        XCTAssertEqual(ACPClient.protocolVersion, 2)
    }

    func testInjectionCapabilitiesAndWireModelsUseSteeringMode() throws {
        let initialized = try JSONDecoder().decode(ACPInitializeResponse.self, from: Data(#"{"protocolVersion":2,"info":{"name":"kit","version":"1"},"capabilities":{"session":{"inject":{"modes":["steer"],"steerInStream":["finish"]}}}}"#.utf8))
        XCTAssertEqual(initialized.capabilities?.session?.inject?.modes, ["steer"])
        XCTAssertEqual(initialized.capabilities?.session?.inject?.steerInStream, ["finish"])

        let request = ACPInjectSessionRequest(
            sessionId: "s-1", mode: "steer",
            content: [.object(["type": .string("text"), "text": .string("next")])]
        )
        let encoded = try XCTUnwrap(JSONSerialization.jsonObject(with: JSONEncoder().encode(request)) as? [String: Any])
        XCTAssertEqual(encoded["sessionId"] as? String, "s-1")
        XCTAssertEqual(encoded["mode"] as? String, "steer")
        XCTAssertEqual((encoded["content"] as? [[String: Any]])?.first?["text"] as? String, "next")

        let response = try JSONDecoder().decode(ACPInjectSessionResponse.self, from: Data(#"{"messageId":"queued-1"}"#.utf8))
        XCTAssertEqual(response.messageId, "queued-1")
    }

    func testLimitsOnlyOversizedMediaPayloadAndPreservesTypedMetadata() {
        let original = DesktopUpdate.agentMessage(DesktopMessageUpdate(
            messageId: "m1", content: [.image(data: String(repeating: "x", count: 100), mimeType: "image/png", uri: "file:///preview.png")], replace: false
        ))
        let limited = original.limited(maximumBytes: 32, previewBytes: 8)
        guard case .agentMessage(let message) = limited, case .image(let payload, let mimeType, let uri) = message.content.first else {
            return XCTFail("expected typed image update")
        }
        XCTAssertTrue(payload?.contains("truncated 92 bytes") == true)
        XCTAssertEqual(mimeType, "image/png")
        XCTAssertEqual(uri, "file:///preview.png")
        XCTAssertEqual(message.messageId, "m1")
    }

    func testDesktopUpdateRoundTripPreservesTurnStateAndLargeIntegers() throws {
        let value = try JSONDecoder().decode(JSONValue.self, from: Data("9007199254740993".utf8))
        XCTAssertEqual(value, .integer(9_007_199_254_740_993))
    }

    func testUnknownDesktopUpdateIsForwardCompatible() throws {
        let data = Data(#"{"sessionUpdate":"vendor_future_update","opaque":{"secret":true}}"#.utf8)
        let wire = try JSONDecoder().decode(JSONValue.self, from: data)
        XCTAssertEqual(try DesktopUpdate(wire: wire), .unknown(kind: "vendor_future_update"))
    }

    @MainActor
    func testPreservesAdvertisedReasoningChoicesAndLabels() {
        let options: [[String: Any]] = [[
            "configId": "thinking", "name": "Thinking", "category": "thought_level",
            "type": "select", "currentValue": "inherit",
            "options": [[
                "group": "effort", "name": "Effort",
                "options": [
                    ["value": "inherit", "name": "Server default"],
                    ["value": "brief", "name": "Brief"],
                    ["value": "deepest", "name": "Deepest"],
                ],
            ]],
        ]]

        let parsed = ConversationController.parseConfigOptions(options)

        XCTAssertEqual(parsed.first?.currentValue, "inherit")
        XCTAssertEqual(parsed.first?.choices.map(\.value), ["inherit", "brief", "deepest"])
        XCTAssertEqual(parsed.first?.choices.map(\.name), ["Server default", "Brief", "Deepest"])
        XCTAssertEqual(ConversationController.reasoningEffort(in: parsed, fallback: "old"), "inherit")
    }

    @MainActor
    func testParsesTypedComposeAndInlineUserMediaPresentation() {
        let tool = ConversationController.toolPresentation([
            "title": "Run program", "status": "pending",
            "rawInput": [
                "script": "return shell({ command: \"pwd\" })",
                "input": ["root": "/tmp"], "background": 5,
            ],
            "rawOutput": ["text": "done"],
        ])
        XCTAssertEqual(tool.title, "Run program")
        XCTAssertEqual(tool.status, .pending)
        XCTAssertEqual(tool.compose?.script, "return shell({ command: \"pwd\" })")
        XCTAssertEqual(tool.compose?.input, .object(["root": .string("/tmp")]))
        XCTAssertEqual(tool.compose?.background, .delay(seconds: 5))
        XCTAssertEqual(tool.compose?.output, .object(["text": .string("done")]))
        XCTAssertNil(ConversationController.toolPresentation([
            "rawInput": ["script": "not compose", "command": "other schema"],
        ]).compose)

        let media = ConversationController.userContent([
            "type": "image", "mimeType": "image/png",
            "data": Data([0, 1, 2]).base64EncodedString(),
        ])
        XCTAssertEqual(media.text, "")
        XCTAssertEqual(media.media.first?.kind, .image)
        XCTAssertEqual(media.media.first?.data, Data([0, 1, 2]))

        let linked = ConversationController.userContent([
            "type": "resource_link", "mimeType": "image/webp",
            "name": "diagram", "uri": "file:///tmp/diagram.webp",
        ])
        XCTAssertEqual(linked.media.first?.url, URL(fileURLWithPath: "/tmp/diagram.webp"))
    }

    func testLargeInlineUserMediaSurvivesTransportCapping() {
        let encoded = Data(repeating: 7, count: 200 * 1024).base64EncodedString()
        let update: [String: Any] = [
            "sessionUpdate": "user_message_chunk",
            "content": ["type": "image", "mimeType": "image/png", "data": encoded],
        ]

        let capped = ACPClient.capLargeFields(update)
        let content = capped["content"] as? [String: Any]

        XCTAssertEqual(content?["data"] as? String, encoded)
    }

    @MainActor
    func testTurnDurationUsesTypedTranscriptPresentation() {
        let duration = ConversationController.turnDuration(.milliseconds(1_250))
        let entry = TranscriptEntry(role: .duration, text: "", presentation: .turnDuration(duration))

        XCTAssertEqual(duration.milliseconds, 1_250)
        XCTAssertEqual(entry.presentation, .turnDuration(TurnDurationPresentation(milliseconds: 1_250)))
    }

    @MainActor
    func testParsesGroupedKitConfigOptions() throws {
        let options: [[String: Any]] = [[
            "configId": "model", "name": "Model", "type": "select", "currentValue": "openrouter:one",
            "options": [["group": "openrouter", "name": "OpenRouter", "options": [["value": "openrouter:one", "name": "One"]]]]
        ]]

        let parsed = ConversationController.parseConfigOptions(options)

        XCTAssertEqual(parsed.first?.currentValue, "openrouter:one")
        XCTAssertEqual(parsed.first?.groups.first?.name, "OpenRouter")
        XCTAssertEqual(parsed.first?.choices.first?.value, "openrouter:one")
    }

    func testParsesJSONRPCBatch() throws {
        let data = Data(#"[{"jsonrpc":"2.0","id":1,"result":{}},{"jsonrpc":"2.0","id":2,"result":{}}]"#.utf8)
        XCTAssertEqual(try RPCEnvelope.parseMany(data).map(\.id), [.integer(1), .integer(2)])
    }

    func testPreservesStringRequestIDs() throws {
        let request = try RPCEnvelope.parse(Data(#"{"jsonrpc":"2.0","id":"agent-1","method":"client/example","params":{}}"#.utf8))
        XCTAssertEqual(request.id, .string("agent-1"))
    }

    func testParsesResponseAndRemoteError() throws {
        let response = try RPCEnvelope.parse(Data(#"{"jsonrpc":"2.0","id":7,"result":{"sessionId":"abc"}}"#.utf8))
        XCTAssertEqual(response.id, .integer(7))
        XCTAssertEqual(response.result?["sessionId"] as? String, "abc")

        let failure = try RPCEnvelope.parse(Data(#"{"jsonrpc":"2.0","id":8,"error":{"code":-32602,"message":"bad option"}}"#.utf8))
        XCTAssertEqual(failure.error?.code, -32602)
        XCTAssertEqual(failure.error?.message, "bad option")
    }
    func testV2GoldenUpdatesCoverPatchesPlansStateAndExtensions() throws {
        func decode(_ json: String) throws -> DesktopUpdate {
            let wire = try JSONDecoder().decode(JSONValue.self, from: Data(json.utf8))
            return try DesktopUpdate(wire: wire)
        }

        let tool = try decode(#"{"sessionUpdate":"tool_call_update","toolCallId":"t1","title":null,"rawInput":{"command":"pwd"}}"#)
        guard case .toolCallUpdate(let patch) = tool else { return XCTFail("expected tool patch") }
        XCTAssertTrue(patch.present.contains("title"))
        XCTAssertTrue(patch.cleared.contains("title"))
        XCTAssertFalse(patch.present.contains("status"))

        XCTAssertEqual(
            try decode(#"{"sessionUpdate":"plan_removed","planId":"p1"}"#),
            .planRemoved(id: "p1")
        )
        XCTAssertThrowsError(try decode(#"{"sessionUpdate":"plan_update","plan":{"planId":"p1"}}"#))
        XCTAssertThrowsError(try decode(#"{"sessionUpdate":"plan_removed"}"#))

        let idle = try decode(#"{"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn","usage":{"totalTokens":9,"inputTokens":4,"outputTokens":5}}"#)
        guard case .state(let state) = idle else { return XCTFail("expected state") }
        XCTAssertEqual(state.usage?.totalTokens, 9)

        let compaction = try decode(#"{"sessionUpdate":"compaction_update","compactionId":"c1","status":"completed","summary":[{"type":"text","text":"summary"}]}"#)
        guard case .compaction(let update) = compaction else { return XCTFail("expected compaction") }
        XCTAssertEqual(update.summary, [.text("summary")])
    }

    func testPermissionCancellationEncodesNestedTaggedOutcome() throws {
        let data = try JSONEncoder().encode(ACPRequestPermissionResponse())
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let outcome = try XCTUnwrap(object["outcome"] as? [String: Any])
        XCTAssertEqual(outcome["outcome"] as? String, "cancelled")
    }

    func testToolPatchDistinguishesMissingNullAndValueAndIDOnlyPreservesState() throws {
        func decode(_ json: String) throws -> DesktopToolUpdate {
            let wire = try JSONDecoder().decode(JSONValue.self, from: Data(json.utf8))
            guard case .toolCallUpdate(let patch) = try DesktopUpdate(wire: wire) else {
                throw ACPClientError.protocolError("expected tool patch")
            }
            return patch
        }

        let missing = try decode(#"{"sessionUpdate":"tool_call_update","toolCallId":"t1"}"#)
        let cleared = try decode(#"{"sessionUpdate":"tool_call_update","toolCallId":"t1","title":null}"#)
        let value = try decode(#"{"sessionUpdate":"tool_call_update","toolCallId":"t1","title":"new"}"#)
        XCTAssertFalse(missing.present.contains("title"))
        XCTAssertTrue(cleared.present.contains("title")); XCTAssertTrue(cleared.cleared.contains("title"))
        XCTAssertEqual(value.title, "new"); XCTAssertFalse(value.cleared.contains("title"))

        let original = DesktopToolUpdate(
            toolCallId: "t1", title: "title", status: "running", kind: "execute",
            content: .string("content"), rawInput: .object(["input": .string("value")]),
            rawOutput: .string("output"), name: "tool", locations: .array([.string("file")]),
            meta: .object(["vendor": .bool(true)])
        )
        XCTAssertEqual(original.merging(missing), original)
    }

    func testUserReplacementDistinguishesOmittedNullAndEmptyContent() throws {
        func decode(_ json: String) throws -> DesktopMessageUpdate {
            let wire = try JSONDecoder().decode(JSONValue.self, from: Data(json.utf8))
            guard case .userMessage(let message) = try DesktopUpdate(wire: wire) else {
                throw ACPClientError.protocolError("expected user message")
            }
            return message
        }
        let omitted = try decode(#"{"sessionUpdate":"user_message","messageId":"u1"}"#)
        let null = try decode(#"{"sessionUpdate":"user_message","messageId":"u1","content":null}"#)
        let empty = try decode(#"{"sessionUpdate":"user_message","messageId":"u1","content":[]}"#)
        XCTAssertFalse(omitted.hasContent)
        XCTAssertTrue(null.hasContent); XCTAssertEqual(null.content, [])
        XCTAssertTrue(empty.hasContent); XCTAssertEqual(empty.content, [])
    }

    func testPlanContentVariantsRemainDistinctAndUnknownIsPreserved() throws {
        func decode(_ json: String) throws -> DesktopUpdate {
            let wire = try JSONDecoder().decode(JSONValue.self, from: Data(json.utf8))
            return try DesktopUpdate(wire: wire)
        }
        XCTAssertEqual(
            try decode(#"{"sessionUpdate":"plan_update","plan":{"type":"items","planId":"p","entries":[{"content":"Do it"}]}}"#),
            .plan(.items(id: "p", entries: [DesktopPlanEntry(content: "Do it", status: nil, priority: nil)]))
        )
        XCTAssertEqual(
            try decode(#"{"sessionUpdate":"plan_update","plan":{"type":"file","planId":"p","uri":"file:///tmp/plan.md"}}"#),
            .plan(.file(id: "p", uri: "file:///tmp/plan.md"))
        )
        XCTAssertEqual(
            try decode(##"{"sessionUpdate":"plan_update","plan":{"type":"markdown","planId":"p","content":"# Plan"}}"##),
            .plan(.markdown(id: "p", content: "# Plan"))
        )
        let unknown = try decode(#"{"sessionUpdate":"plan_update","plan":{"type":"vendor","planId":"p","opaque":true}}"#)
        guard case .plan(.unknown(let id, let type, let raw)) = unknown else { return XCTFail("expected unknown plan") }
        XCTAssertEqual(id, "p"); XCTAssertEqual(type, "vendor")
        XCTAssertEqual((raw.anyValue as? [String: Any])?["opaque"] as? Bool, true)
    }

    @MainActor
    func testUnknownConfigKindsAreNotWritable() {
        let options: [[String: Any]] = [
            ["configId": "known", "name": "Known", "type": "select", "options": [["value": "a"]]],
            ["configId": "future", "name": "Future", "type": "vendor-slider", "currentValue": "5"],
            ["configId": "missing-type", "name": "Missing type", "currentValue": "a"],
            ["id": "legacy-id", "name": "Legacy ID", "type": "select"],
        ]
        XCTAssertEqual(ConversationController.parseConfigOptions(options).map(\.id), ["known"])
    }

    func testGeneratedTypedConfigValuesEncodeV2Discriminants() throws {
        let select = ACPSetSessionConfigOptionRequest(sessionId: "s1", configId: "model", value: .select("provider:model"))
        let boolean = ACPSetSessionConfigOptionRequest(sessionId: "s1", configId: "enabled", value: .boolean(true))
        let selectObject = try JSONSerialization.jsonObject(with: JSONEncoder().encode(select)) as? [String: Any]
        let booleanObject = try JSONSerialization.jsonObject(with: JSONEncoder().encode(boolean)) as? [String: Any]
        XCTAssertEqual(selectObject?["type"] as? String, "id")
        XCTAssertEqual(selectObject?["value"] as? String, "provider:model")
        XCTAssertEqual(booleanObject?["type"] as? String, "boolean")
        XCTAssertEqual(booleanObject?["value"] as? Bool, true)
    }

}
