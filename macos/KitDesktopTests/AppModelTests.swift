import XCTest
@testable import Kit

final class AppModelTests: XCTestCase {
    @MainActor
    func testAttentionOnlyAppliesToUnfocusedConversations() {
        let focused = AppModel.attentionState(reason: "end_turn", isFocused: true)
        XCTAssertFalse(focused.awaitingUser)
        XCTAssertFalse(focused.unread)

        let hidden = AppModel.attentionState(reason: "end_turn", isFocused: false)
        XCTAssertTrue(hidden.awaitingUser)
        XCTAssertTrue(hidden.unread)

        let failed = AppModel.attentionState(reason: "error", isFocused: false)
        XCTAssertFalse(failed.awaitingUser)
        XCTAssertTrue(failed.unread)
    }

    @MainActor
    func testFocusingConversationClearsPersistedAttention() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = PersistenceStore(fileURL: directory.appendingPathComponent("state.json"))
        let workspace = Workspace(name: "Project", path: directory.path)
        let conversation = Conversation(
            workspaceID: workspace.id, unread: true, awaitingUser: true
        )
        try store.save(PersistedAppState(workspaces: [workspace], conversations: [conversation]))
        let model = AppModel(store: store, catalogLoader: nil, requestNotificationAuthorization: false)
        model.selectedConversationID = conversation.id

        model.appBecameActive()

        let updated = try XCTUnwrap(model.state.conversations.first)
        XCTAssertFalse(updated.unread)
        XCTAssertFalse(updated.awaitingUser)
        store.flush()
    }

    @MainActor
    func testUnsupportedNewerStateKeepsAppModelReadOnlyAndPreservesBytes() throws {
        let directory = temporaryDirectory()
        let url = directory.appendingPathComponent("state.json")
        let original = Data("{\"schemaVersion\":99,\"workspaces\":[{\"future\":true}],\"conversations\":[]}".utf8)
        try original.write(to: url)
        let store = PersistenceStore(fileURL: url)

        let model = AppModel(store: store, catalogLoader: nil, requestNotificationAuthorization: false)
        XCTAssertTrue(model.persistenceIsReadOnly)
        XCTAssertNotNil(model.persistenceError)

        model.addWorkspace(path: directory.path)
        model.createConversation()
        model.reconcileSessionCatalog([
            ACPSessionInfo(sessionID: "future", title: "Future", updatedAt: Date())
        ], for: UUID())
        store.flush()

        XCTAssertEqual(try Data(contentsOf: url), original)
        XCTAssertTrue(model.state.workspaces.isEmpty)
        XCTAssertTrue(model.state.conversations.isEmpty)
    }

    @MainActor
    func testUnsupportedBackupKeepsAppModelReadOnlyWhenPrimaryIsMissing() throws {
        let directory = temporaryDirectory()
        let store = PersistenceStore(fileURL: directory.appendingPathComponent("state.json"))
        let original = Data("{\"schemaVersion\":99,\"workspaces\":[],\"conversations\":[]}".utf8)
        try original.write(to: store.backupURL)

        let model = AppModel(store: store, catalogLoader: nil, requestNotificationAuthorization: false)

        XCTAssertTrue(model.persistenceIsReadOnly)
        model.addWorkspace(path: directory.path)
        store.flush()
        XCTAssertTrue(model.state.workspaces.isEmpty)
        XCTAssertEqual(try Data(contentsOf: store.backupURL), original)
        XCTAssertFalse(FileManager.default.fileExists(atPath: store.fileURL.path))
    }

    @MainActor
    func testCatalogMergeUsesKitMetadataAndPreservesDesktopMetadata() throws {
        let directory = temporaryDirectory()
        let store = PersistenceStore(fileURL: directory.appendingPathComponent("state.json"))
        let workspace = Workspace(name: "Project", path: directory.path)
        let createdAt = Date(timeIntervalSince1970: 10)
        let existing = Conversation(
            workspaceID: workspace.id, title: "Desktop title", sessionID: "existing",
            createdAt: createdAt, updatedAt: Date(timeIntervalSince1970: 20),
            unread: true, awaitingUser: true, provider: "custom", model: "custom-model",
            reasoningEffort: "high", usesConfiguredDefaults: false
        )
        let otherWorkspace = Workspace(name: "Other", path: directory.appendingPathComponent("other").path)
        let colliding = Conversation(
            workspaceID: otherWorkspace.id, title: "Other workspace title", sessionID: "discovered"
        )
        let subagent = Conversation(workspaceID: workspace.id, title: "Child", sessionID: "child")
        try store.save(PersistedAppState(
            workspaces: [workspace, otherWorkspace], conversations: [existing, colliding, subagent]
        ))
        let model = AppModel(store: store, catalogLoader: nil, requestNotificationAuthorization: false)
        let canonicalUpdate = Date(timeIntervalSince1970: 100)
        let discoveredUpdate = Date(timeIntervalSince1970: 90)

        model.reconcileSessionCatalog([
            ACPSessionInfo(sessionID: "existing", title: "Kit title", updatedAt: canonicalUpdate),
            ACPSessionInfo(sessionID: "discovered", title: "Discovered", updatedAt: discoveredUpdate),
            ACPSessionInfo(sessionID: "child", title: "Child", updatedAt: discoveredUpdate, isSubagent: true),
        ], for: workspace.id)
        store.flush()

        let merged = try XCTUnwrap(model.state.conversations.first { $0.sessionID == "existing" })
        XCTAssertEqual(merged.id, existing.id)
        XCTAssertEqual(merged.title, "Kit title")
        XCTAssertEqual(merged.updatedAt, canonicalUpdate)
        XCTAssertEqual(merged.createdAt, createdAt)
        XCTAssertTrue(merged.unread)
        XCTAssertTrue(merged.awaitingUser)
        XCTAssertEqual(merged.provider, "custom")
        XCTAssertFalse(merged.usesConfiguredDefaults)
        let discovered = try XCTUnwrap(model.state.conversations.first {
            $0.workspaceID == workspace.id && $0.sessionID == "discovered"
        })
        XCTAssertEqual(discovered.createdAt, discoveredUpdate)
        XCTAssertEqual(discovered.updatedAt, discoveredUpdate)
        XCTAssertFalse(model.state.conversations.contains { $0.id == subagent.id })
        let preservedCollision = try XCTUnwrap(model.state.conversations.first {
            $0.workspaceID == otherWorkspace.id && $0.sessionID == "discovered"
        })
        XCTAssertEqual(preservedCollision.title, "Other workspace title")
    }

    @MainActor
    func testCatalogOmissionRemovesDurableSessionAndLaterDiscoveryRestoresIt() throws {
        let directory = temporaryDirectory()
        let store = PersistenceStore(fileURL: directory.appendingPathComponent("state.json"))
        let workspace = Workspace(name: "Project", path: directory.path)
        let missing = Conversation(workspaceID: workspace.id, title: "Missing", sessionID: "missing")
        try store.save(PersistedAppState(workspaces: [workspace], conversations: [missing]))
        let model = AppModel(store: store, catalogLoader: nil, requestNotificationAuthorization: false)
        model.selectedConversationID = missing.id

        model.reconcileSessionCatalog([], for: workspace.id)

        XCTAssertFalse(model.state.conversations.contains { $0.id == missing.id })
        XCTAssertNil(model.selectedConversationID)

        model.reconcileSessionCatalog([
            ACPSessionInfo(sessionID: "missing", title: "Recovered", updatedAt: Date(timeIntervalSince1970: 50))
        ], for: workspace.id)
        XCTAssertEqual(model.state.conversations.first?.sessionID, "missing")
        XCTAssertEqual(model.state.conversations.first?.title, "Recovered")
    }

    @MainActor
    func testSessionReadyCoalescesCatalogDiscoveredDuplicate() throws {
        let directory = temporaryDirectory()
        let store = PersistenceStore(fileURL: directory.appendingPathComponent("state.json"))
        let workspace = Workspace(name: "Project", path: directory.path)
        let connecting = Conversation(workspaceID: workspace.id)
        let discovered = Conversation(workspaceID: workspace.id, title: "Catalog title", sessionID: "shared-session")
        try store.save(PersistedAppState(workspaces: [workspace], conversations: [connecting, discovered]))
        let model = AppModel(store: store, catalogLoader: nil, requestNotificationAuthorization: false)
        model.selectedConversationID = discovered.id

        model.sessionBecameReady(conversationID: connecting.id, sessionID: "shared-session")

        let rows = model.state.conversations.filter { $0.sessionID == "shared-session" }
        XCTAssertEqual(rows.map(\.id), [connecting.id])
        XCTAssertEqual(rows.first?.title, "Catalog title")
        XCTAssertEqual(model.selectedConversationID, connecting.id)
    }

    @MainActor
    func testCatalogOmissionPreservesLiveControllerSession() async throws {
        let directory = temporaryDirectory()
        let store = PersistenceStore(fileURL: directory.appendingPathComponent("state.json"))
        let workspace = Workspace(name: "Project", path: repositoryRoot.path)
        let conversation = Conversation(workspaceID: workspace.id, sessionID: "live-session")
        try store.save(PersistedAppState(workspaces: [workspace], conversations: [conversation]))
        let fixture = repositoryRoot.appendingPathComponent("fixtures/mock-acp-v2.py")
        let launch = ACPClient.LaunchOverride(
            executable: URL(fileURLWithPath: "/usr/bin/python3"),
            prefixArguments: [fixture.path, "--models"]
        )
        let model = AppModel(
            store: store, catalogLoader: nil,
            controllerFactory: { conversation, path in
                ConversationController(
                    conversation: conversation, workspacePath: path,
                    client: ACPClient(launchOverride: launch, requestTimeout: 2, promptTimeout: 2)
                )
            },
            requestNotificationAuthorization: false
        )
        model.selectConversation(conversation.id)
        try await waitUntil { model.controllers[conversation.id]?.isReady == true }

        model.reconcileSessionCatalog([], for: workspace.id)

        XCTAssertTrue(model.state.conversations.contains { $0.id == conversation.id })
        let closed = expectation(description: "closed")
        model.closeAll { closed.fulfill() }
        await fulfillment(of: [closed], timeout: 4)
    }

    @MainActor
    func testFailedCachedControllerRetriesInPlaceWhenReselected() async throws {
        let directory = temporaryDirectory()
        let marker = directory.appendingPathComponent("failed-once")
        let store = PersistenceStore(fileURL: directory.appendingPathComponent("state.json"))
        let workspace = Workspace(name: "Project", path: repositoryRoot.path)
        let activeConversation = Conversation(workspaceID: workspace.id)
        let conversation = Conversation(workspaceID: workspace.id, sessionID: "retry-session")
        try store.save(PersistedAppState(
            workspaces: [workspace], conversations: [activeConversation, conversation]
        ))
        let fixture = repositoryRoot.appendingPathComponent("fixtures/mock-acp-v2.py")
        let launch = ACPClient.LaunchOverride(
            executable: URL(fileURLWithPath: "/usr/bin/python3"),
            prefixArguments: [fixture.path, "--models"],
            environment: ["MOCK_FAIL_LOAD_ONCE_FILE": marker.path]
        )
        let model = AppModel(
            store: store, catalogLoader: nil,
            controllerFactory: { conversation, path in
                ConversationController(
                    conversation: conversation, workspacePath: path,
                    client: ACPClient(launchOverride: launch, requestTimeout: 2, promptTimeout: 2)
                )
            },
            requestNotificationAuthorization: false
        )

        model.selectConversation(activeConversation.id)
        try await waitUntil { model.controllers[activeConversation.id]?.isReady == true }
        let activeController = try XCTUnwrap(model.controllers[activeConversation.id])
        XCTAssertEqual(model.selectedConversationID, activeConversation.id)

        model.selectConversation(conversation.id)
        try await waitUntil { model.controllers[conversation.id]?.isRetryable == true }
        let cached = try XCTUnwrap(model.controllers[conversation.id])
        XCTAssertEqual(model.selectedConversationID, activeConversation.id)
        XCTAssertTrue(model.selectedController === activeController)

        model.selectConversation(conversation.id)
        try await waitUntil { model.controllers[conversation.id]?.isReady == true }

        XCTAssertTrue(model.controllers[conversation.id] === cached)
        XCTAssertEqual(model.selectedConversationID, conversation.id)
        XCTAssertEqual(cached.entries.filter { $0.role == .assistant }.last?.text, "replayed assistant")
        let closed = expectation(description: "closed")
        model.closeAll { closed.fulfill() }
        await fulfillment(of: [closed], timeout: 4)
    }

    @MainActor
    func testReselectingLockedConversationAttemptsClaim() async throws {
        let directory = temporaryDirectory()
        let store = PersistenceStore(fileURL: directory.appendingPathComponent("state.json"))
        let workspace = Workspace(name: "Project", path: repositoryRoot.path)
        let activeConversation = Conversation(workspaceID: workspace.id)
        let lockedConversation = Conversation(workspaceID: workspace.id, sessionID: "locked-session")
        try store.save(PersistedAppState(
            workspaces: [workspace], conversations: [activeConversation, lockedConversation]
        ))
        let fixture = repositoryRoot.appendingPathComponent("fixtures/mock-acp-v2.py")
        let pidFile = directory.appendingPathComponent("stale-helper-pids")
        let launch = ACPClient.LaunchOverride(
            executable: URL(fileURLWithPath: "/usr/bin/python3"),
            prefixArguments: [fixture.path, "--models"],
            environment: [
                "MOCK_STALE_LOCK": "1", "MOCK_STALE_LOCK_PID_FILE": pidFile.path,
            ]
        )
        let model = AppModel(
            store: store, catalogLoader: nil,
            controllerFactory: { conversation, path in
                ConversationController(
                    conversation: conversation, workspacePath: path,
                    client: ACPClient(launchOverride: launch, requestTimeout: 2, promptTimeout: 2)
                )
            },
            requestNotificationAuthorization: false
        )

        model.selectedConversationID = activeConversation.id
        model.selectConversation(lockedConversation.id)
        try await waitUntil("lock state was not published") { model.lockedConversationIDs.contains(lockedConversation.id) }
        XCTAssertEqual(model.selectedConversationID, activeConversation.id)

        model.selectConversation(lockedConversation.id)
        try await waitUntil("claimed conversation was not selected") {
            model.selectedConversationID == lockedConversation.id
                && !model.lockedConversationIDs.contains(lockedConversation.id)
        }

        let closed = expectation(description: "closed")
        model.closeAll { closed.fulfill() }
        await fulfillment(of: [closed], timeout: 4)
    }

    @MainActor
    private func waitUntil(
        _ message: String = "Condition was not met before timeout",
        timeout: TimeInterval = 6, condition: @escaping @MainActor () -> Bool
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if condition() { return }
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        XCTFail(message)
        throw NSError(domain: "AppModelTests", code: 1)
    }

    private var repositoryRoot: URL {
        URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    }

    private func temporaryDirectory() -> URL {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }
}

final class AgentRosterTests: XCTestCase {
    func testGenerationAwareLifecycleRejectsStaleAndDuplicateEvents() throws {
        var roster = AgentRoster()
        XCTAssertTrue(roster.apply(event: event(id: "a", name: "Scout", status: "starting", generation: 1), nowMS: 1_100))
        XCTAssertTrue(roster.apply(event: event(id: "a", name: "Scout", status: "working", generation: 1), nowMS: 1_200))
        XCTAssertFalse(roster.apply(event: event(id: "a", name: "Stale", status: "starting", generation: 1), nowMS: 1_300))
        XCTAssertFalse(roster.apply(event: event(id: "a", name: "Duplicate", status: "working", generation: 1), nowMS: 1_400))
        XCTAssertTrue(roster.apply(event: event(id: "a", name: "Scout", status: "idle", outcome: "success", generation: 1, finished: 1_500), nowMS: 1_500))
        XCTAssertTrue(roster.apply(event: event(id: "a", name: "Scout", status: "starting", generation: 2, started: 2_000), nowMS: 2_000))

        let row = try XCTUnwrap(roster.rowsByID["a"])
        XCTAssertEqual(row.name, "Scout")
        XCTAssertEqual(row.status, .starting)
        XCTAssertEqual(row.generation, 2)
        XCTAssertEqual(roster.counts, AgentRosterCounts(total: 1, starting: 1, working: 0, idle: 0))
    }

    func testTreePreservesAncestryAndMarksMissingParents() throws {
        var roster = AgentRoster()
        XCTAssertTrue(roster.apply(event: event(id: "child", name: "Builder", status: "working", parentID: "parent", parentName: "Lead", created: 20), nowMS: 100))
        XCTAssertEqual(roster.treeRows.map(\.row.id), ["child"])
        XCTAssertTrue(try XCTUnwrap(roster.treeRows.first).missingParent)

        XCTAssertTrue(roster.apply(event: event(id: "parent", name: "Lead", status: "idle", created: 10), nowMS: 100))
        XCTAssertTrue(roster.apply(event: event(id: "grandchild", name: "Reviewer", status: "starting", parentID: "child", parentName: "Builder", created: 30), nowMS: 100))
        XCTAssertEqual(roster.treeRows.map(\.row.id), ["parent", "child", "grandchild"])
        XCTAssertEqual(roster.treeRows.map(\.depth), [0, 1, 2])
        XCTAssertFalse(roster.treeRows.contains(where: \.missingParent))
    }

    func testDescendantRemovalIsTransitiveAndTombstonesDelayedEvents() {
        var roster = AgentRoster()
        _ = roster.apply(event: event(id: "root", name: "Root", status: "idle"), nowMS: 100)
        _ = roster.apply(event: event(id: "child", name: "Child", status: "idle", parentID: "root"), nowMS: 100)
        _ = roster.apply(event: event(id: "grandchild", name: "Grandchild", status: "working", parentID: "child"), nowMS: 100)
        _ = roster.apply(event: event(id: "peer", name: "Peer", status: "idle"), nowMS: 100)

        XCTAssertTrue(roster.apply(event: ["event": "subagent_descendants_removed", "ancestor_id": "root"], nowMS: 200))
        XCTAssertEqual(Set(roster.rowsByID.keys), Set(["root", "peer"]))
        XCTAssertFalse(roster.apply(event: event(id: "grandchild", name: "Late", status: "working", generation: 99, parentID: "child"), nowMS: 300))
        XCTAssertNil(roster.rowsByID["grandchild"])
    }

    func testRemovalOutcomeAndRetirementMatchRosterSemantics() throws {
        var roster = AgentRoster()
        _ = roster.apply(event: event(id: "success", name: "Done", status: "idle", outcome: "success"), nowMS: 1_000)
        XCTAssertTrue(roster.apply(event: event(id: "success", name: "Done", status: "removed", outcome: "success", finished: 1_100), nowMS: 1_100))
        XCTAssertNil(roster.rowsByID["success"])

        _ = roster.apply(event: event(id: "failed", name: "Failed", status: "working"), nowMS: 1_000)
        XCTAssertTrue(roster.apply(event: event(id: "failed", name: "Failed", status: "removed", outcome: "failed", finished: 1_100), nowMS: 1_200))
        XCTAssertEqual(roster.rowsByID["failed"]?.status, .removed)
        XCTAssertEqual(roster.counts.total, 0)
        XCTAssertFalse(roster.pruneExpired(at: 5_099))
        XCTAssertTrue(roster.pruneExpired(at: 5_100))
        XCTAssertNil(roster.rowsByID["failed"])

        _ = roster.apply(event: event(id: "active", name: "Active", status: "working", started: 2_000), nowMS: 2_000)
        roster.retireActive(at: 2_500)
        let retired = try XCTUnwrap(roster.rowsByID["active"])
        XCTAssertEqual(retired.status, .removed)
        XCTAssertEqual(retired.outcome, .failed)
        XCTAssertEqual(retired.generationFinishedAtMS, 2_500)
    }

    func testUnknownRuntimeEventsAndStatusesAreIgnored() {
        var roster = AgentRoster()
        XCTAssertFalse(roster.apply(event: ["event": "future_private_event", "payload": true], nowMS: 1))
        XCTAssertFalse(roster.apply(event: event(id: "future", name: "Future", status: "paused"), nowMS: 1))
        XCTAssertTrue(roster.rowsByID.isEmpty)
    }

    @MainActor
    func testControllerRejectsStaleSessionEventsAndResetsRosterState() {
        let conversation = Conversation(workspaceID: UUID())
        let controller = ConversationController(conversation: conversation, workspacePath: "/tmp")
        controller.prepareRuntimeSession("s-current")
        controller.applyRuntime(["event": "session_started", "session_id": "s-stale"])
        controller.applyRuntime(event(id: "stale", name: "Stale", status: "working"))
        XCTAssertTrue(controller.agentRoster.rowsByID.isEmpty)

        controller.applyRuntime(["event": "session_started", "session_id": "s-current"])
        controller.applyRuntime(event(id: "current", name: "Current", status: "working"))
        XCTAssertEqual(controller.agentRoster.rowsByID["current"]?.name, "Current")

        controller.applyRuntime(["event": "future_private_event", "payload": true])
        XCTAssertEqual(controller.agentRoster.rowsByID.count, 1)
        controller.prepareRuntimeSession("s-next")
        XCTAssertTrue(controller.agentRoster.rowsByID.isEmpty)
        XCTAssertNil(controller.runtimeSessionID)
    }

    @MainActor
    func testRosterPresentationIsAvailableForMatchingSessionWithTranscript() {
        XCTAssertFalse(ConversationController.shouldPresentAgentRoster(expectedSessionID: nil, runtimeSessionID: nil, transcriptIsEmpty: false))
        XCTAssertFalse(ConversationController.shouldPresentAgentRoster(expectedSessionID: "s-1", runtimeSessionID: nil, transcriptIsEmpty: false))
        XCTAssertFalse(ConversationController.shouldPresentAgentRoster(expectedSessionID: "s-1", runtimeSessionID: "s-old", transcriptIsEmpty: false))
        XCTAssertFalse(ConversationController.shouldPresentAgentRoster(expectedSessionID: "s-1", runtimeSessionID: "s-1", transcriptIsEmpty: true))
        XCTAssertTrue(ConversationController.shouldPresentAgentRoster(expectedSessionID: "s-1", runtimeSessionID: "s-1", transcriptIsEmpty: false))
    }

    private func event(
        id: String, name: String, status: String, outcome: String? = nil, generation: UInt64 = 1,
        parentID: String? = nil, parentName: String? = nil, created: UInt64 = 10,
        started: UInt64 = 100, finished: UInt64? = nil
    ) -> [String: Any] {
        var value: [String: Any] = [
            "event": "subagent_state_changed", "id": id, "name": name, "status": status,
            "generation": generation, "task": "Task for \(name)", "harness": "acp.kit",
            "created_at_unix_ms": created, "generation_started_at_unix_ms": started
        ]
        if let outcome { value["outcome"] = outcome }
        if let parentID { value["parent_id"] = parentID }
        if let parentName { value["parent_name"] = parentName }
        if let finished { value["generation_finished_at_unix_ms"] = finished }
        return value
    }
}
