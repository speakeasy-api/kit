import XCTest
@testable import Kit

/// Regression coverage for the presentation domain, not ACP/controller integration.
final class TranscriptProjectionRegressionTests: XCTestCase {
    @MainActor
    func testOneSynchronizationCanAppendMessagesAndCompleteAnOlderTool() throws {
        var entries = [
            TranscriptEntry(role: .user, text: "Question"),
            TranscriptEntry(role: .tool, text: "Pending", toolCallID: "background", isStreaming: true),
            TranscriptEntry(role: .assistant, text: "Visible partial answer", isStreaming: true),
        ]
        let projection = TranscriptProjection()
        projection.synchronize(entries)
        let activity = projection.items[1]
        let assistant = projection.items[2]

        // A delivery batch can contain both new rows and edits behind the tail.
        entries[1].text = "Completed background output"
        entries[1].isStreaming = false
        entries[2].text = "Visible final answer"
        entries[2].isStreaming = false
        entries.append(TranscriptEntry(role: .status, text: "Verified"))
        entries.append(TranscriptEntry(role: .assistant, text: "Final conclusion"))
        projection.synchronize(entries, changedIndices: [1, 2, 3, 4])

        XCTAssertTrue(projection.items[1] === activity)
        XCTAssertTrue(projection.items[2] === assistant)
        XCTAssertEqual(activity.runningCount, 0)
        XCTAssertEqual(activity.entries.first?.text, "Completed background output")
        XCTAssertEqual(assistant.entries.first?.text, "Visible final answer")
        XCTAssertFalse(assistant.isActivity)
        XCTAssertFalse(try XCTUnwrap(projection.items.last).isActivity)
        XCTAssertEqual(projection.items.flatMap(\.entries).map(\.id), entries.map(\.id))
        XCTAssertEqual(projection.items.flatMap(\.entries).map(\.text), entries.map(\.text))
    }

    @MainActor
    func testRemovalAndRetentionRebuildRemapLaterEditsWithoutLosingFinalAnswer() throws {
        var entries = [
            TranscriptEntry(role: .user, text: "Old prompt"),
            TranscriptEntry(role: .thought, text: "Reasoning"),
            TranscriptEntry(role: .plan, text: "Temporary plan"),
            TranscriptEntry(role: .tool, text: "Pending", toolCallID: "tool", isStreaming: true),
            TranscriptEntry(role: .assistant, text: "Partial answer", isStreaming: true),
        ]
        let projection = TranscriptProjection()
        projection.synchronize(entries)
        let activityID = projection.items[1].id
        let answerID = try XCTUnwrap(projection.items.last).id

        entries.remove(at: 2) // Plan removal shifts the tool's source index.
        entries.removeFirst() // Retention also shifts the activity's source index.
        projection.synchronize(entries, rebuilding: true)
        XCTAssertEqual(projection.items.map(\.id), [activityID, answerID])

        entries[1].text = "Final tool output"
        entries[1].isStreaming = false
        entries[2].text = "Authoritative final answer"
        entries[2].isStreaming = false
        projection.synchronize(entries, changedIndices: [1, 2])
        XCTAssertEqual(projection.items.first?.entries.last?.text, "Final tool output")
        XCTAssertEqual(projection.items.first?.runningCount, 0)
        XCTAssertEqual(projection.items.last?.entries.first?.text, "Authoritative final answer")
        XCTAssertEqual(projection.items.map(\.id), [activityID, answerID])

        // Replay a complete snapshot after edits: display IDs and final text survive.
        projection.synchronize(entries, rebuilding: true)
        XCTAssertEqual(projection.items.map(\.id), [activityID, answerID])
        XCTAssertEqual(projection.items.flatMap(\.entries).map(\.text), entries.map(\.text))
    }

    @MainActor
    func testSameLengthReplayReplacesOldEntryMappingsAndResetCanStartAnotherThread() {
        let projection = TranscriptProjection()
        let old = [TranscriptEntry(role: .assistant, text: "Previous thread")]
        projection.synchronize(old)
        var replacement = [TranscriptEntry(role: .assistant, text: "Replayed partial", isStreaming: true)]
        projection.synchronize(replacement, rebuilding: true)
        replacement[0].text = "Replayed final"
        replacement[0].isStreaming = false
        projection.synchronize(replacement, changedIndices: [0])
        XCTAssertEqual(projection.items.map(\.id), replacement.map(\.id))
        XCTAssertEqual(projection.items.first?.entries.first?.text, "Replayed final")
        XCTAssertFalse(projection.items.flatMap(\.entries).contains { $0.id == old[0].id })

        projection.synchronize([], rebuilding: true)
        XCTAssertTrue(projection.items.isEmpty)
        projection.synchronize(old)
        XCTAssertEqual(projection.items.map(\.id), old.map(\.id))
        XCTAssertEqual(projection.items.first?.entries.first?.text, "Previous thread")
    }

    @MainActor
    func testMeasureProjectionStreamingTailOfThousandRowThread() {
        var entries = (0..<1000).map { index in
            TranscriptEntry(role: .assistant, text: "Completed row \(index)")
        }
        entries[999].isStreaming = true
        let projection = TranscriptProjection()
        projection.synchronize(entries)
        let tail = projection.items.last
        let tailID = entries[999].id

        // Measures only the real projection update domain, not the controller,
        // SwiftUI layout, or ACP transport. No timing threshold or work counter.
        // Each iteration reuses the completed prefix and changes one tail entry.
        measure {
            MainActor.assumeIsolated {
                for _ in 0..<200 {
                    entries[999].text += " tail"
                    projection.synchronize(entries, changedIndices: [999])
                }
            }
        }
        XCTAssertTrue(projection.items.last === tail)
        XCTAssertEqual(projection.items.last?.id, tailID)
        XCTAssertEqual(projection.items.last?.entries.first?.text, entries[999].text)
        XCTAssertEqual(projection.items.first?.entries.first?.text, "Completed row 0")
    }
}
