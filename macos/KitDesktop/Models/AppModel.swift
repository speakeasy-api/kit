import AppKit
import Foundation
import UserNotifications

@MainActor
final class AppModel: ObservableObject {
    typealias CatalogLoader = @MainActor (String, @escaping (Result<[ACPSessionInfo], Error>) -> Void) -> Void
    typealias ControllerFactory = (Conversation, String) -> ConversationController

    @Published private(set) var state: PersistedAppState
    @Published private(set) var controllers: [UUID: ConversationController] = [:]
    @Published private(set) var activity: [UUID: Bool] = [:]
    @Published private(set) var lockedConversationIDs: Set<UUID> = []
    @Published var selectedWorkspaceID: UUID?
    @Published var selectedConversationID: UUID?
    @Published var persistenceError: String?
    @Published private(set) var persistenceIsReadOnly = false

    private let store: PersistenceStore
    private let catalogLoader: CatalogLoader?
    private let controllerFactory: ControllerFactory
    private var catalogGenerations: [UUID: Int] = [:]
    private var pendingConversationID: UUID?
    private var isClosing = false

    init(
        store: PersistenceStore = PersistenceStore(),
        catalogLoader: CatalogLoader? = AppModel.defaultCatalogLoader,
        controllerFactory: ControllerFactory? = nil,
        requestNotificationAuthorization: Bool = true
    ) {
        self.store = store
        self.catalogLoader = catalogLoader
        self.controllerFactory = controllerFactory ?? { ConversationController(conversation: $0, workspacePath: $1) }
        do { state = try store.load() }
        catch let error as PersistenceError {
            state = PersistedAppState()
            persistenceError = error.localizedDescription
            if case .unsupportedSchema = error { persistenceIsReadOnly = true }
        } catch {
            state = PersistedAppState()
            persistenceError = error.localizedDescription
        }
        selectedWorkspaceID = state.workspaces.first?.id
        if requestNotificationAuthorization {
            UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { _, _ in }
        }
        if let selectedWorkspaceID { refreshSessionCatalog(for: selectedWorkspaceID) }
    }

    var selectedWorkspace: Workspace? { state.workspaces.first { $0.id == selectedWorkspaceID } }
    var selectedController: ConversationController? { selectedConversationID.flatMap { controllers[$0] } }

    var workspaceConversations: [Conversation] {
        state.conversations.filter { $0.workspaceID == selectedWorkspaceID }.sorted { $0.updatedAt > $1.updatedAt }
    }

    func addWorkspace(path: String) {
        guard allowPersistenceMutation() else { return }
        let standardized = URL(fileURLWithPath: path).standardizedFileURL.path
        if let existing = state.workspaces.first(where: { $0.path == standardized }) { selectWorkspace(existing.id); return }
        let name = URL(fileURLWithPath: standardized).lastPathComponent
        let workspace = Workspace(name: name.isEmpty ? standardized : name, path: standardized)
        state.workspaces.append(workspace)
        selectedWorkspaceID = workspace.id
        selectedConversationID = nil
        save()
        refreshSessionCatalog(for: workspace.id)
    }

    func selectWorkspace(_ id: UUID?) {
        selectedWorkspaceID = id
        if let selectedConversationID, !state.conversations.contains(where: { $0.id == selectedConversationID && $0.workspaceID == id }) { self.selectedConversationID = nil }
        if let pendingConversationID, !state.conversations.contains(where: { $0.id == pendingConversationID && $0.workspaceID == id }) { self.pendingConversationID = nil }
        if let id { refreshSessionCatalog(for: id) }
    }

    func createConversation() {
        guard allowPersistenceMutation() else { return }
        guard let workspaceID = selectedWorkspaceID else { return }
        let conversation = Conversation(workspaceID: workspaceID)
        state.conversations.append(conversation)
        save()
        selectConversation(conversation.id)
    }

    func selectConversation(_ id: UUID?) {
        guard let id else {
            pendingConversationID = nil
            selectedConversationID = nil
            return
        }
        guard let conversation = state.conversations.first(where: { $0.id == id }) else { return }
        if conversation.sessionID == nil || controllers[id]?.isReady == true {
            pendingConversationID = nil
            commitSelection(id)
        } else {
            pendingConversationID = id
        }
        openConversationIfNeeded(id)
    }

    private func commitSelection(_ id: UUID) {
        selectedConversationID = id
        updateConversation(id) { item in
            item.unread = false
            item.awaitingUser = false
        }
        UNUserNotificationCenter.current().removeDeliveredNotifications(withIdentifiers: [id.uuidString])
    }

    func appBecameActive() {
        guard let id = selectedConversationID else { return }
        updateConversation(id) { item in
            item.unread = false
            item.awaitingUser = false
        }
        UNUserNotificationCenter.current().removeDeliveredNotifications(withIdentifiers: [id.uuidString])
    }

    func closeAll(completion: (() -> Void)? = nil) {
        isClosing = true
        store.flush()
        let active = Array(controllers.values)
        guard !active.isEmpty else { completion?(); return }
        var remaining = active.count
        for controller in active {
            controller.close {
                remaining -= 1
                if remaining == 0 { completion?() }
            }
        }
    }

    func reconcileSessionCatalog(_ catalog: [ACPSessionInfo], for workspaceID: UUID) {
        guard allowPersistenceMutation() else { return }
        let subagentSessionIDs = Set(catalog.filter(\.isSubagent).map(\.sessionID))
        let rootCatalog = catalog.filter { !$0.isSubagent }
        var merged = state.conversations
        var indexBySessionID: [String: Int] = [:]
        var reservedIndexBySessionID: [String: Int] = [:]
        for (index, conversation) in merged.enumerated() where conversation.workspaceID == workspaceID {
            if let sessionID = conversation.sessionID, indexBySessionID[sessionID] == nil {
                indexBySessionID[sessionID] = index
            }
            if let controller = controllers[conversation.id] {
                reservedIndexBySessionID[controller.reservedSessionID] = index
            }
        }

        var preferredIndexBySessionID: [String: Int] = [:]
        for session in rootCatalog {
            let index: Int
            if let reserved = reservedIndexBySessionID[session.sessionID] {
                index = reserved
                merged[index].sessionID = session.sessionID
            } else if let existing = indexBySessionID[session.sessionID] {
                index = existing
            } else {
                let timestamp = session.updatedAt ?? Date()
                let conversation = Conversation(
                    workspaceID: workspaceID, title: session.title ?? "Kit session",
                    sessionID: session.sessionID, createdAt: timestamp, updatedAt: timestamp
                )
                index = merged.count
                merged.append(conversation)
            }
            preferredIndexBySessionID[session.sessionID] = index
            if let title = session.title { merged[index].title = title }
            if let updatedAt = session.updatedAt { merged[index].updatedAt = updatedAt }
        }

        let catalogIDs = Set(rootCatalog.map(\.sessionID))
        let preferredIDBySessionID = preferredIndexBySessionID.mapValues { merged[$0].id }
        var removedIDs: Set<UUID> = []
        var replacements: [UUID: UUID] = [:]
        merged = merged.compactMap { conversation in
            guard conversation.workspaceID == workspaceID, let sessionID = conversation.sessionID else {
                return conversation
            }
            if subagentSessionIDs.contains(sessionID) {
                removedIDs.insert(conversation.id)
                return nil
            }
            guard catalogIDs.contains(sessionID), let preferredID = preferredIDBySessionID[sessionID] else {
                // A live controller is itself a recovery path and may briefly outrun an
                // eventually consistent catalog. Only prune omitted dormant sessions.
                guard controllers[conversation.id] == nil else { return conversation }
                removedIDs.insert(conversation.id)
                return nil
            }
            guard preferredID == conversation.id else {
                removedIDs.insert(conversation.id)
                replacements[conversation.id] = preferredID
                return nil
            }
            return conversation
        }
        removeControllersAndRepairSelection(removedIDs, replacements: replacements)
        guard merged != state.conversations else { return }
        state.conversations = merged
        save()
    }

    func sessionBecameReady(conversationID: UUID, sessionID: String) {
        guard allowPersistenceMutation(),
              let preferred = state.conversations.first(where: { $0.id == conversationID }) else { return }
        let duplicates = state.conversations.filter {
            $0.id != conversationID && $0.workspaceID == preferred.workspaceID && $0.sessionID == sessionID
        }
        let duplicateIDs = Set(duplicates.map(\.id))
        let replacements = Dictionary(uniqueKeysWithValues: duplicates.map { ($0.id, conversationID) })
        let catalogMetadata = duplicates.max { $0.updatedAt < $1.updatedAt }
        state.conversations.removeAll { duplicateIDs.contains($0.id) }
        removeControllersAndRepairSelection(duplicateIDs, replacements: replacements)
        guard let index = state.conversations.firstIndex(where: { $0.id == conversationID }) else { return }
        state.conversations[index].sessionID = sessionID
        if let catalogMetadata {
            state.conversations[index].title = catalogMetadata.title
            state.conversations[index].createdAt = min(state.conversations[index].createdAt, catalogMetadata.createdAt)
        }
        state.conversations[index].updatedAt = Date()
        save()
    }

    private func removeControllersAndRepairSelection(_ removedIDs: Set<UUID>, replacements: [UUID: UUID]) {
        guard !removedIDs.isEmpty else { return }
        for id in removedIDs {
            controllers.removeValue(forKey: id)?.close()
            activity.removeValue(forKey: id)
            lockedConversationIDs.remove(id)
        }
        if let selectedConversationID, removedIDs.contains(selectedConversationID) {
            self.selectedConversationID = replacements[selectedConversationID]
        }
        if let pendingConversationID, removedIDs.contains(pendingConversationID) {
            self.pendingConversationID = replacements[pendingConversationID]
        }
    }

    private func refreshSessionCatalog(for workspaceID: UUID) {
        guard let catalogLoader, !persistenceIsReadOnly, !isClosing,
              let workspace = state.workspaces.first(where: { $0.id == workspaceID }) else { return }
        let generation = (catalogGenerations[workspaceID] ?? 0) + 1
        catalogGenerations[workspaceID] = generation
        let canonicalRoot = URL(fileURLWithPath: workspace.path).resolvingSymlinksInPath().standardizedFileURL.path
        catalogLoader(canonicalRoot) { [weak self] result in
            Task { @MainActor in
                guard let self, !self.isClosing, self.catalogGenerations[workspaceID] == generation,
                      case .success(let sessions) = result else { return }
                self.reconcileSessionCatalog(sessions, for: workspaceID)
            }
        }
    }

    private static func defaultCatalogLoader(
        root: String, completion: @escaping (Result<[ACPSessionInfo], Error>) -> Void
    ) {
        ACPClient().listSessions(root: root, completion: completion)
    }

    private func openConversationIfNeeded(_ id: UUID) {
        if let controller = controllers[id] {
            controller.retryIfNeeded()
            return
        }
        guard let conversation = state.conversations.first(where: { $0.id == id }),
              let workspace = state.workspaces.first(where: { $0.id == conversation.workspaceID }) else { return }
        let controller = controllerFactory(conversation, workspace.path)
        controller.onSessionReady = { [weak self] sessionID, _ in
            guard let self else { return }
            self.sessionBecameReady(conversationID: id, sessionID: sessionID)
            if self.pendingConversationID == id || self.selectedConversationID == id {
                self.pendingConversationID = nil
                self.commitSelection(id)
            }
            if let workspaceID = self.state.conversations.first(where: { $0.id == id })?.workspaceID {
                self.refreshSessionCatalog(for: workspaceID)
            }
        }
        controller.onTurnStarted = { [weak self] prompt in
            self?.updateConversation(id) { item in
                item.awaitingUser = false; item.unread = false; item.updatedAt = Date()
                if item.title == "New conversation" && !prompt.isEmpty { item.title = String(prompt.prefix(64)).replacingOccurrences(of: "\n", with: " ") }
            }
        }
        controller.onTurnFinished = { [weak self] reason in self?.turnFinished(id: id, reason: reason) }
        controller.onTitleChanged = { [weak self] title in self?.updateConversation(id) { $0.title = title } }
        controller.onActivityChanged = { [weak self] active in self?.activity[id] = active }
        controller.onLockChanged = { [weak self] locked in
            guard let self else { return }
            if locked { self.lockedConversationIDs.insert(id) }
            else { self.lockedConversationIDs.remove(id) }
        }
        controller.onConfigChanged = { [weak self] provider, model, effort, userSelected in
            self?.updateConversation(id) { item in
                item.provider = provider
                item.model = model
                item.reasoningEffort = effort
                if userSelected { item.usesConfiguredDefaults = false }
            }
        }
        controllers[id] = controller
        activity[id] = false
        controller.start()
    }

    private func turnFinished(id: UUID, reason: String) {
        let inactive = NSApp == nil || !NSApp.isActive
        let hidden = selectedConversationID != id
        let needsAttention = inactive || hidden
        let attention = Self.attentionState(reason: reason, isFocused: !needsAttention)
        updateConversation(id) { item in
            item.awaitingUser = attention.awaitingUser
            item.unread = attention.unread
            item.updatedAt = Date()
        }
        if let workspaceID = state.conversations.first(where: { $0.id == id })?.workspaceID {
            refreshSessionCatalog(for: workspaceID)
        }
        guard inactive, let conversation = state.conversations.first(where: { $0.id == id }) else { return }
        let content = UNMutableNotificationContent()
        content.title = conversation.title
        content.body = reason == "end_turn" ? "Kit finished and is awaiting your input." : "Kit turn finished: \(reason)."
        content.sound = .default
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: id.uuidString, content: content, trigger: nil))
    }

    static func attentionState(reason: String, isFocused: Bool) -> (awaitingUser: Bool, unread: Bool) {
        guard !isFocused else { return (false, false) }
        return (reason == "end_turn", true)
    }

    private func updateConversation(_ id: UUID, change: (inout Conversation) -> Void) {
        guard allowPersistenceMutation(), let index = state.conversations.firstIndex(where: { $0.id == id }) else { return }
        change(&state.conversations[index])
        save()
    }

    private func save() {
        guard !persistenceIsReadOnly else { return }
        let snapshot = state
        store.saveAsync(snapshot) { [weak self] error in self?.persistenceError = error?.localizedDescription }
    }

    private func allowPersistenceMutation() -> Bool {
        guard persistenceIsReadOnly else { return true }
        persistenceError = "Saved state was created by a newer Kit Desktop. This version is read-only and left it unchanged."
        return false
    }
}
