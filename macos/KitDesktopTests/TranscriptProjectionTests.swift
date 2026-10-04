import XCTest
@testable import Kit

final class TranscriptProjectionTests: XCTestCase {
    @MainActor
    func testMessagesRemainStandaloneAndAllInternalEventsAreRetained() {
        let roles: [TranscriptRole] = [.user, .thought, .tool, .plan, .status, .duration, .usage, .error, .assistant, .tool, .assistant]
        let entries = roles.map { TranscriptEntry(role: $0, text: $0.rawValue) }
        let projection = TranscriptProjection()
        projection.synchronize(entries)
        XCTAssertEqual(projection.items.map(\.isActivity), [false, true, false, true, false])
        XCTAssertEqual(projection.items.flatMap(\.entries).map(\.id), entries.map(\.id))
        XCTAssertEqual(projection.items[1].errorCount, 1)
        XCTAssertEqual(projection.items[1].entries.map(\.role), Array(roles[1...7]))
    }

    @MainActor
    func testDurationOnlyItemIsNotExpandable() {
        let projection = TranscriptProjection()
        let answer = TranscriptEntry(role: .assistant, text: "Done")
        let duration = TranscriptEntry(role: .duration, text: "took 3 s")
        projection.synchronize([answer, duration])
        XCTAssertFalse(projection.items[0].isExpandableActivity)
        XCTAssertFalse(projection.items[1].isExpandableActivity)
        XCTAssertEqual(projection.items[1].entries.map(\.id), [duration.id])
        projection.synchronize([answer, duration, TranscriptEntry(role: .status, text: "Notice")])
        XCTAssertTrue(projection.items[1].isExpandableActivity)
    }

    @MainActor
    func testStreamingAndNonTailToolUpdatesKeepDisplayIdentity() {
        var entries = [TranscriptEntry(role: .assistant, text: "completed"), TranscriptEntry(role: .tool, text: "pending", isStreaming: true)]
        let projection = TranscriptProjection()
        projection.synchronize(entries)
        let message = projection.items[0]
        let activity = projection.items[1]
        entries.append(TranscriptEntry(role: .assistant, text: "answer", isStreaming: true))
        projection.synchronize(entries)
        entries[1].text = "all output"
        entries[1].isStreaming = false
        entries[1].children = [RuntimeChild(id: "nested", tool: "shell", summary: "full nested output", running: false, succeeded: false, durationMS: 3)]
        projection.synchronize(entries, changedIndices: [1])
        XCTAssertTrue(projection.items[0] === message)
        XCTAssertTrue(projection.items[1] === activity)
        XCTAssertEqual(activity.runningCount, 0)
        XCTAssertEqual(activity.errorCount, 1)
        XCTAssertEqual(activity.entries[0].children[0].summary, "full nested output")
        XCTAssertEqual(activity.entries[0].text, "all output")
        XCTAssertEqual(projection.items[2].entries[0].text, "answer")
    }

    @MainActor
    func testAppendMergesActivityAndRetentionRebuildPreservesSurvivingIDs() {
        var entries = [TranscriptEntry(role: .user, text: "prompt"), TranscriptEntry(role: .thought, text: "thought")]
        let projection = TranscriptProjection()
        projection.synchronize(entries)
        let activityID = projection.items[1].id
        entries.append(TranscriptEntry(role: .tool, text: "output"))
        projection.synchronize(entries)
        XCTAssertEqual(projection.items.count, 2)
        XCTAssertEqual(projection.items[1].id, activityID)
        entries.removeFirst()
        projection.synchronize(entries, rebuilding: true)
        XCTAssertEqual(projection.items.map(\.id), [activityID])
        XCTAssertEqual(projection.items[0].entries.count, 2)
    }

    func testSingleLongMessageCoalescesDeltasAndBoundsUnicodeAndMedia() {
        var blocks: [DesktopContentBlock] = []
        let delta = String(repeating: "🙂", count: 128)
        for _ in 0..<1024 { blocks = TranscriptContentBuffer.append([.text(delta)], to: blocks) }
        let text = blocks.compactMap { block -> String? in if case .text(let value) = block { return value }; return nil }.joined()
        XCTAssertLessThanOrEqual(text.utf8.count, TranscriptContentBuffer.maximumTextBytes)
        XCTAssertLessThan(blocks.count, 1024)
        XCTAssertFalse(text.contains("�"))
        XCTAssertTrue(text.hasSuffix(delta))
        let enormous = String(repeating: "a", count: TranscriptContentBuffer.maximumBytes + 1)
        blocks = TranscriptContentBuffer.append([.image(data: enormous, mimeType: "image/png", uri: nil)], to: blocks)
        XCTAssertLessThanOrEqual(blocks.reduce(0) { $0 + TranscriptContentBuffer.byteCount($1) }, TranscriptContentBuffer.maximumBytes)
        let links = (0..<1024).map { DesktopContentBlock.resourceLink(uri: "file:///\($0)", name: nil, mimeType: nil) }
        XCTAssertLessThanOrEqual(TranscriptContentBuffer.append(links).count, TranscriptContentBuffer.maximumBlocks)
    }

    func testLongSingleMessageAccumulationBenchmark() {
        // A benchmark, not a wall-clock assertion or test-only work counter.
        measure {
            var blocks: [DesktopContentBlock] = []
            for _ in 0..<2048 {
                blocks = TranscriptContentBuffer.append([.text(String(repeating: "x", count: 256))], to: blocks)
            }
            XCTAssertLessThanOrEqual(blocks.reduce(0) { $0 + TranscriptContentBuffer.byteCount($1) }, TranscriptContentBuffer.maximumTextBytes)
        }
    }

    @MainActor
    func testRuntimeLossRejectsBufferedEventsAndHealthyHeartbeatDoesNotResurrectRoster() {
        let controller = ConversationController(conversation: Conversation(workspaceID: UUID()), workspacePath: "/tmp")
        controller.prepareRuntimeSession("current")
        // Heartbeat is process-scoped and may precede attachment identity.
        controller.applyRuntime(["event": "runlet_transport", "available": true])
        controller.applyRuntime(["event": "session_started", "session_id": "current"])
        let event: [String: Any] = ["event": "subagent_state_changed", "id": "child", "name": "Child", "status": "working", "generation": 1, "task": "task", "harness": "acp.kit", "created_at_unix_ms": 1, "generation_started_at_unix_ms": 2]
        controller.applyRuntime(event)
        XCTAssertEqual(controller.agentRoster.rowsByID.count, 1)
        controller.expireRuntimeLease(at: ContinuousClock().now.advanced(by: .seconds(6)))
        XCTAssertEqual(controller.runtimeTransportAvailable, false)
        XCTAssertTrue(controller.agentRoster.rowsByID.isEmpty)
        controller.applyRuntime(event)
        XCTAssertTrue(controller.agentRoster.rowsByID.isEmpty)
        XCTAssertGreaterThan(controller.transcriptProjection.items.last?.errorCount ?? 0, 0)
        controller.applyRuntime(["event": "runlet_transport", "available": true])
        XCTAssertTrue(controller.agentRoster.rowsByID.isEmpty)
        controller.applyRuntime(event)
        XCTAssertEqual(controller.agentRoster.rowsByID.count, 1)
        controller.prepareRuntimeSession("next")
        XCTAssertNil(controller.runtimeTransportAvailable)
        XCTAssertTrue(controller.agentRoster.rowsByID.isEmpty)
    }
}
