import XCTest
@testable import Kit

final class DesktopUsageTests: XCTestCase {
    func testUsageDecodesLegacyAndCostReports() throws {
        let decoder = JSONDecoder()
        let legacy = try decoder.decode(DesktopUsageUpdate.self, from: Data(#"{"used":12,"size":100}"#.utf8))
        XCTAssertEqual(legacy.used, 12)
        XCTAssertNil(legacy.cost)
        let current = try decoder.decode(DesktopUsageUpdate.self, from: Data(#"{"used":40,"size":100,"cost":{"amount":1.25,"currency":"USD"}}"#.utf8))
        XCTAssertEqual(current.cost, DesktopCost(amount: 1.25, currency: "USD"))
    }

    func testChildCostSnapshotsSurviveRemovalWithoutDoubleCounting() {
        var roster = AgentRoster()
        XCTAssertTrue(roster.apply(event: usage("child", amount: 1), nowMS: 1))
        XCTAssertTrue(roster.apply(event: usage("child", amount: 1.25), nowMS: 2))
        XCTAssertTrue(roster.apply(event: usage("other", amount: 2, currency: "EUR"), nowMS: 3))
        XCTAssertEqual(roster.costTotals, ["USD": 1.25, "EUR": 2])
        XCTAssertTrue(roster.usageByID.isEmpty, "Unknown rows must not appear from telemetry alone")
        roster.pruneExpired(at: 5_000)
        XCTAssertEqual(roster.costTotals["USD"], 1.25)
        roster.reset()
        XCTAssertTrue(roster.costTotals.isEmpty)
    }

    func testKnownChildContextSurvivesLifecycleUpdatesAndCleansUp() {
        var roster = AgentRoster()
        XCTAssertTrue(roster.apply(event: lifecycle(status: "working", generation: 1), nowMS: 1))
        XCTAssertTrue(roster.apply(event: usage("child", amount: 0.5), nowMS: 2))
        XCTAssertEqual(roster.usageByID["child"]?.used, 40)
        XCTAssertTrue(roster.apply(event: lifecycle(status: "idle", generation: 1), nowMS: 3))
        XCTAssertEqual(roster.usageByID["child"]?.size, 100)
        XCTAssertTrue(roster.apply(event: lifecycle(status: "removed", generation: 1), nowMS: 4))
        XCTAssertNil(roster.usageByID["child"])
        XCTAssertEqual(roster.costTotals["USD"], 0.5)
    }

    func testSteeringRequiresCurrentGenerationCapability() {
        var roster = AgentRoster()
        XCTAssertFalse(roster.canSteer(id: "child", generation: 1))
        roster.apply(event: lifecycle(status: "working", generation: 1), nowMS: 1)
        XCTAssertFalse(roster.canSteer(id: "child", generation: 1))
        let supported: [String: Any] = ["event": "subagent_capabilities", "id": "child", "generation": 1, "can_steer": true]
        XCTAssertTrue(roster.apply(event: supported, nowMS: 2))
        XCTAssertTrue(roster.canSteer(id: "child", generation: 1))
        roster.apply(event: lifecycle(status: "idle", generation: 1), nowMS: 3)
        XCTAssertFalse(roster.canSteer(id: "child", generation: 1))
        roster.apply(event: lifecycle(status: "working", generation: 2), nowMS: 4)
        XCTAssertFalse(roster.apply(event: supported, nowMS: 5))
        XCTAssertFalse(roster.canSteer(id: "child", generation: 2))
        roster.apply(event: ["event": "subagent_capabilities", "id": "child", "generation": 2, "can_steer": true], nowMS: 6)
        XCTAssertTrue(roster.canSteer(id: "child", generation: 2))
        roster.invalidateLiveState()
        XCTAssertFalse(roster.canSteer(id: "child", generation: 2))
    }

    func testTransportGapInvalidatesOnlyLiveObservations() {
        var roster = AgentRoster()
        roster.apply(event: lifecycle(status: "working", generation: 1), nowMS: 1)
        roster.apply(event: usage("child", amount: 1), nowMS: 2)
        roster.invalidateLiveState()
        XCTAssertTrue(roster.rowsByID.isEmpty)
        XCTAssertTrue(roster.usageByID.isEmpty)
        XCTAssertEqual(roster.costTotals, ["USD": 1])
        XCTAssertTrue(roster.apply(event: lifecycle(status: "idle", generation: 1), nowMS: 3))
        XCTAssertEqual(roster.rowsByID["child"]?.status, .idle)
    }

    func testInvalidCostsDoNotReplaceValidSnapshots() {
        var roster = AgentRoster()
        roster.apply(event: usage("child", amount: 1), nowMS: 1)
        for amount in [-1.0, Double.infinity, Double.nan] {
            roster.apply(event: usage("child", amount: amount), nowMS: 2)
        }
        roster.apply(event: usage("child", amount: 3, currency: "  "), nowMS: 3)
        XCTAssertEqual(roster.costTotals, ["USD": 1])
    }

    private func usage(_ id: String, amount: Double, currency: String = "USD") -> [String: Any] {
        ["event": "subagent_usage", "id": id, "used": 40, "size": 100,
         "cost": ["amount": amount, "currency": currency]]
    }

    private func lifecycle(status: String, generation: Int) -> [String: Any] {
        ["event": "subagent_state_changed", "id": "child", "name": "Scout", "status": status,
         "generation": generation, "task": "Inspect", "harness": "acp.kit",
         "created_at_unix_ms": 1, "generation_started_at_unix_ms": 1]
    }
}
