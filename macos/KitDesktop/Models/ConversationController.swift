import AppKit
import Foundation

@MainActor
final class ConversationController: ObservableObject {
    static let maximumEntries = 1000
    static let maximumAttachmentCount = 8
    static let maximumAttachmentBytes: Int64 = 10 * 1024 * 1024
    static let maximumTotalAttachmentBytes: Int64 = 20 * 1024 * 1024

    @Published var entries: [TranscriptEntry] = []
    @Published var configOptions: [ConfigOption] = []
    @Published var advertisedCommands: [AdvertisedCommand] = []
    @Published var draft = ""
    @Published var attachments: [Attachment] = []
    @Published var diagnostics: [String] = []
    @Published var status = "Connecting…"
    @Published var isReady = false
    @Published var isRunning = false
    @Published var contextUsed: Int?
    @Published var contextSize: Int?
    @Published var tokenUsage: DesktopTokenUsage?
    @Published var transcriptRevision = 0
    @Published private(set) var pendingAttachmentReceipts = 0
    @Published private(set) var isRetryable = false
    @Published private(set) var isLocked = false
    @Published private(set) var agentRoster = AgentRoster()
    @Published private(set) var runtimeSessionID: String?
    @Published private(set) var pendingSteers: [PendingSteer] = []
    @Published private(set) var canSteer = false
    @Published private(set) var isInjecting = false

    struct PendingSteer: Identifiable, Equatable {
        let id: String
        let text: String
        let attachmentCount: Int

        var summary: String {
            if !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return text }
            return attachmentCount == 1 ? "1 attachment" : "\(attachmentCount) attachments"
        }
    }

    let conversationID: UUID
    var canCancel: Bool { foregroundRunning }
    var acceptsInput: Bool {
        isReady && ((foregroundRunning && canSteer && !isInjecting) || (!foregroundRunning && autonomousTurns.isEmpty))
    }
    private enum Turn: Hashable { case foreground(UUID), autonomous(Int) }
    private enum Settlement {
        case started(Turn, prompt: String)
        case terminal(Turn, reason: String, error: Error?)
        case processExit(Int32)
        case shutdown
    }
    private struct ComposerTransaction {
        let turn: Turn
        let text: String
        let originalDraft: String
        let attachments: [Attachment]
        var transcriptEntryID: UUID?
        var echoedText = ""
        var echoedTextComplete = false
        var echoedAttachmentIndex = 0
    }

    private struct InjectionSnapshot {
        let text: String
        let attachments: [Attachment]
    }

    var shouldPresentAgentRoster: Bool {
        Self.shouldPresentAgentRoster(
            expectedSessionID: expectedRuntimeSessionID, runtimeSessionID: runtimeSessionID,
            transcriptIsEmpty: entries.isEmpty
        )
    }

    static func shouldPresentAgentRoster(
        expectedSessionID: String?, runtimeSessionID: String?, transcriptIsEmpty: Bool
    ) -> Bool {
        expectedSessionID != nil && runtimeSessionID == expectedSessionID && !transcriptIsEmpty
    }

    private var client: ACPClient
    private let workspacePath: String
    private let launchSessionID: String
    private var conversation: Conversation
    private var foregroundRunning = false
    private var autonomousTurns: Set<Int> = []
    private var streamingEntryIDs: Set<UUID> = []
    private var messageEntryIDs: [String: UUID] = [:]
    private var planEntryIDs: [String: UUID] = [:]
    private var toolStates: [String: DesktopToolUpdate] = [:]
    private var activeThoughtEntryID: UUID?
    private var activeThoughtStartedAt: ContinuousClock.Instant?
    private var thoughtTexts: [String: String] = [:]
    private var thoughtBlocks: [String: [DesktopContentBlock]] = [:]
    private let clock = ContinuousClock()
    private var foregroundStartedAt: ContinuousClock.Instant?
    private var autonomousStartedAt: [Int: ContinuousClock.Instant] = [:]
    private var latestAssistantSource = ""
    private var activeTurns: Set<Turn> = []
    private var settledTurns: Set<Turn> = []
    private var composerTransaction: ComposerTransaction?
    private var shuttingDown = false
    private var retrying = false
    private var clientGeneration: UInt64 = 0
    private var startupUpdates: [DesktopUpdate]?
    private var expectedRuntimeSessionID: String?
    private var rosterPruneWorkItem: DispatchWorkItem?

    var onSessionReady: ((String, [ConfigOption]) -> Void)?
    var onTurnStarted: ((String) -> Void)?
    var onTurnFinished: ((String) -> Void)?
    var onTitleChanged: ((String) -> Void)?
    var onActivityChanged: ((Bool) -> Void)?
    var onLockChanged: ((Bool) -> Void)?
    var onConfigChanged: ((String, String, String, Bool) -> Void)?

    init(conversation: Conversation, workspacePath: String, client: ACPClient = ACPClient()) {
        conversationID = conversation.id
        self.conversation = conversation
        self.workspacePath = workspacePath
        launchSessionID = conversation.sessionID ?? "s-desktop-\(UUID().uuidString.replacingOccurrences(of: "-", with: ""))"
        self.client = client
    }

    var reservedSessionID: String { conversation.sessionID ?? launchSessionID }

    func start() { start(force: false) }

    func retryIfNeeded() {
        if isLocked {
            claimLockedSession()
            return
        }
        guard isRetryable, !retrying, !shuttingDown else { return }
        retrying = true
        isRetryable = false
        status = "Reconnecting…"
        replaceClientAfterClosing(force: false)
    }

    func claimLockedSession() {
        guard isLocked, !retrying, !shuttingDown else { return }
        retrying = true
        setLocked(false)
        status = "Claiming thread…"
        replaceClientAfterClosing(force: true)
    }

    private func start(force: Bool) {
        guard !shuttingDown else { return }
        status = force ? "Recovering stale session lock…" : "Connecting…"
        let persisted = conversation.sessionID
        let activeClient = client
        let generation = clientGeneration
        startupUpdates = persisted == nil ? nil : []
        activeClient.onUpdate = { [weak self, weak activeClient] update in
            guard let self, let activeClient, self.isCurrentClient(activeClient, generation: generation), !self.shuttingDown else { return }
            if self.startupUpdates != nil { self.startupUpdates?.append(update) }
            else { self.apply(update) }
        }
        activeClient.onRuntimeEvent = { [weak self, weak activeClient] event in
            guard let self, let activeClient, self.isCurrentClient(activeClient, generation: generation), !self.shuttingDown else { return }
            self.applyRuntime(event)
        }
        activeClient.onDiagnostic = { [weak self, weak activeClient] text in
            guard let self, let activeClient, self.isCurrentClient(activeClient, generation: generation), !self.shuttingDown else { return }
            self.recordDiagnostic(text, updateStatus: !text.hasPrefix("Ignored unsupported desktop update:") && !text.hasPrefix("Ignored malformed desktop update:"))
        }
        activeClient.onExit = { [weak self, weak activeClient] code in
            guard let self, let activeClient, self.isCurrentClient(activeClient, generation: generation) else { return }
            self.reduceSettlement(.processExit(code))
            var roster = self.agentRoster
            roster.retireActive(at: Self.nowMilliseconds())
            self.agentRoster = roster
            self.scheduleRosterPrune()
            if !self.shuttingDown, !self.retrying { self.isRetryable = true }
        }
        let sessionID = persisted ?? launchSessionID
        prepareRuntimeSession(sessionID)
        let inheritsConfig = conversation.usesConfiguredDefaults
        let options = ACPLaunchOptions(
            root: workspacePath, sessionID: sessionID, resume: persisted != nil,
            provider: inheritsConfig ? nil : conversation.provider,
            model: inheritsConfig ? nil : conversation.model,
            reasoningEffort: inheritsConfig ? nil : conversation.reasoningEffort,
            force: force
        )
        activeClient.start(options: options, loading: persisted != nil) { [weak self, weak activeClient] result in
            guard let self, let activeClient, self.isCurrentClient(activeClient, generation: generation), !self.shuttingDown else { return }
            switch result {
            case .failure(let error):
                self.startupUpdates = nil
                if persisted != nil, Self.isSessionLockError(error) {
                    self.isRetryable = false
                    self.setLocked(true)
                    self.status = "Thread is open in another process"
                } else {
                    self.isRetryable = true
                    self.fail(error)
                }
            case .success(let payload):
                self.isRetryable = false
                self.setLocked(false)
                self.configOptions = Self.parseConfigOptions(payload["configOptions"])
                if let replay = self.startupUpdates {
                    self.replaceTranscript(with: replay)
                }
                self.startupUpdates = nil
                self.isReady = true
                self.status = persisted == nil ? "Ready" : "Continued session"
                self.finishStreamingEntries(preservingBackgroundWork: true)
                let resolved = payload["sessionId"] as? String ?? sessionID
                self.expectedRuntimeSessionID = resolved
                self.conversation.sessionID = resolved
                self.onSessionReady?(resolved, self.configOptions)
                self.publishCurrentConfig()
            }
        }
    }

    func send() {
        let text = draft
        guard acceptsInput, !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !attachments.isEmpty else { return }
        let files = attachments
        if foregroundRunning {
            sendSteer(text: text, attachments: files)
            return
        }
        let turn = Turn.foreground(UUID())
        composerTransaction = ComposerTransaction(turn: turn, text: text, originalDraft: draft, attachments: files)
        draft = ""
        attachments = []
        latestAssistantSource = ""
        foregroundStartedAt = nil
        status = "Sending…"
        reduceSettlement(.started(turn, prompt: text))
        client.prompt(text: text, attachments: files, onSent: { [weak self] in
            guard let self, self.composerTransaction?.turn == turn else { return }
            self.foregroundStartedAt = self.clock.now
            self.status = "Running…"
            let media = files.map { file in
                UserMediaPresentation(kind: file.kind, mimeType: file.mimeType, name: file.url.lastPathComponent, url: file.url)
            }
            let blocks: [DesktopContentBlock] = [.text(text)] + files.map { file in
                file.kind == .image ? .image(data: nil, mimeType: file.mimeType, uri: file.url.absoluteString) : .audio(data: nil, mimeType: file.mimeType)
            }
            let entry = TranscriptEntry(
                role: .user, text: text, contentBlocks: blocks,
                presentation: .user(UserMessagePresentation(text: text, media: media))
            )
            self.appendEntry(entry)
            self.finalizeLastEntry()
            self.composerTransaction?.transcriptEntryID = entry.id
        }) { [weak self] result in
            guard let self else { return }
            switch result {
            case .failure(let error): self.reduceSettlement(.terminal(turn, reason: "error", error: error))
            case .success:
                // ACP v2 acknowledges prompt acceptance here. State updates settle the turn.
                break
            }
        }
    }

    func cancel() {
        guard foregroundRunning else { return }
        canSteer = false
        status = "Cancelling…"
        client.cancel()
    }

    func choose(_ option: ConfigOption, value: String) {
        let wireValue: ACPSessionConfigValue
        switch option.valueType {
        case "select": wireValue = .select(value)
        case "boolean":
            guard value == "true" || value == "false" else { return }
            wireValue = .boolean(value == "true")
        default: return
        }
        client.setConfig(id: option.id, value: wireValue) { [weak self] result in
            guard let self else { return }
            switch result {
            case .failure(let error): self.fail(error)
            case .success(let payload):
                if let refreshed = payload["configOptions"] {
                    self.configOptions = Self.parseConfigOptions(refreshed)
                } else if let index = self.configOptions.firstIndex(where: { $0.id == option.id }) {
                    self.configOptions[index].currentValue = value
                }
                self.publishCurrentConfig(userSelected: true)
            }
        }
    }

    func detachCompose(callID: String) {
        client.detachCompose(callID: callID) { [weak self] result in
            guard let self else { return }
            switch result {
            case .failure(let error): self.fail(error)
            case .success(let payload):
                if payload["detached"] as? Bool == true, let index = self.entries.firstIndex(where: { $0.toolCallID == callID }) {
                    self.entries[index].backgrounded = true
                    self.entries[index].isStreaming = true
                    self.streamingEntryIDs.insert(self.entries[index].id)
                    self.status = "Compose call moved to background"
                    self.transcriptRevision += 1
                    self.refreshActivity()
                }
            }
        }
    }

    func cancelBackground(callID: String) {
        client.cancelBackground(callID: callID) { [weak self] result in
            if case .failure(let error) = result { self?.fail(error) }
        }
    }

    func copyLastResponse() {
        guard !latestAssistantSource.isEmpty else { status = "No assistant response to copy"; return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(latestAssistantSource, forType: .string)
        status = "Copied latest assistant response as Markdown"
    }

    func addAttachments(_ urls: [URL]) {
        var next = attachments
        for url in urls where !next.contains(where: { $0.url == url }) {
            guard next.count < Self.maximumAttachmentCount else { fail(ACPClientError.attachment("At most 8 attachments can be pending")); break }
            do { next.append(try Self.attachment(url)) } catch { fail(error) }
        }
        let total = next.reduce(Int64(0)) { $0 + $1.size }
        guard total <= Self.maximumTotalAttachmentBytes else { fail(ACPClientError.attachment("Attachments exceed the 20 MiB total limit")); return }
        attachments = next
    }

    func beginReceivingAttachments(_ count: Int) {
        pendingAttachmentReceipts += max(0, count)
    }

    func finishReceivingAttachment(_ result: Result<URL, Error>) {
        pendingAttachmentReceipts = max(0, pendingAttachmentReceipts - 1)
        switch result {
        case .success(let url): addAttachments([url])
        case .failure(let error): fail(ACPClientError.attachment("Could not receive attachment: \(error.localizedDescription)"))
        }
    }

    func removeAttachment(_ id: UUID) { attachments.removeAll { $0.id == id } }

    func close(completion: (() -> Void)? = nil) {
        let hadActiveTurn = !activeTurns.isEmpty
        reduceSettlement(.shutdown)
        client.close(activeTurn: hadActiveTurn) { [weak self] in
            self?.status = "Closed"
            self?.isRetryable = false
            completion?()
        }
    }

    private func apply(_ update: DesktopUpdate) {
        switch update {
        case .userMessage(let message):
            finishActiveThought()
            applyMessage(message, role: .user)
        case .agentMessage(let message):
            finishActiveThought()
            applyMessage(message, role: .assistant)
        case .agentThought(let message): applyThought(message)
        case .toolCall(let update), .toolCallUpdate(let update):
            finishActiveThought()
            updateTool(update)
        case .toolCallContent(let id, let content):
            finishActiveThought()
            appendToolContent(id: id, content: content)
        case .plan(let plan):
            finishActiveThought()
            applyPlan(plan)
        case .planRemoved(let id): removePlan(id: id)
        case .usage(let usage): contextUsed = usage.used; contextSize = usage.size
        case .tokenUsage(let usage): tokenUsage = usage
        case .configOptions(let options): configOptions = Self.parseConfigOptions(options.anyValue); publishCurrentConfig()
        case .sessionInfo(let info): if info.titlePresent { onTitleChanged?(info.title ?? "") }
        case .availableCommands(let commands): advertisedCommands = commands.map { AdvertisedCommand(name: $0.name, description: $0.description ?? "") }
        case .state(let state): applySessionState(state)
        case .turnState(let state): applyTurnState(state)
        case .notice(let notice):
            finishActiveThought()
            status = notice.title
            appendEntry(TranscriptEntry(role: .status, title: notice.severity?.capitalized ?? "Notice", text: notice.description ?? notice.title))
        case .compaction(let compaction):
            finishActiveThought()
            status = compaction.error ?? "Compaction " + compaction.status.replacingOccurrences(of: "_", with: " ")
            let text = compaction.summary?.map(Self.contentText).joined() ?? status
            upsertSingleton(role: .status, title: "Compaction", text: text)
        case .compactionChunk(_, let content):
            finishActiveThought()
            appendChunk(role: .status, content: content)
        case .currentMode: break
        case .unknown: break
        }
    }

    private func applyThought(_ message: DesktopMessageUpdate) {
        let blocks = message.content
        let text = blocks.map(Self.contentText).joined()
        let currentText = message.replace
            ? text
            : (thoughtTexts[message.messageId] ?? "") + text
        let currentBlocks = message.replace
            ? blocks
            : (thoughtBlocks[message.messageId] ?? []) + blocks
        thoughtTexts[message.messageId] = String(currentText.suffix(256 * 1024))
        thoughtBlocks[message.messageId] = currentBlocks

        if activeThoughtEntryID == nil {
            let entry = TranscriptEntry(role: .thought, text: "", isStreaming: true)
            appendEntry(entry)
            activeThoughtEntryID = entry.id
            activeThoughtStartedAt = clock.now
            streamingEntryIDs.insert(entry.id)
        }
        guard let entryID = activeThoughtEntryID,
              let index = entries.firstIndex(where: { $0.id == entryID }) else { return }
        entries[index].text = thoughtTexts[message.messageId] ?? ""
        entries[index].contentBlocks = thoughtBlocks[message.messageId] ?? []
        entries[index].formatted = nil
        entries[index].isStreaming = true
        messageEntryIDs[message.messageId] = entryID
        transcriptRevision += 1
    }

    private func applyMessage(_ message: DesktopMessageUpdate, role: TranscriptRole) {
        let deliveredSteer = role == .user && pendingSteers.contains(where: { $0.id == message.messageId })
        if deliveredSteer { pendingSteers.removeAll { $0.id == message.messageId } }
        if message.replace, !message.hasContent {
            if role == .user, !deliveredSteer, let optimistic = composerTransaction?.transcriptEntryID {
                messageEntryIDs[message.messageId] = optimistic
            }
            return
        }
        if role == .user, !deliveredSteer, composerTransaction != nil {
            if !message.content.isEmpty, consumeComposerEcho(message.content, requireComplete: message.replace) {
                if let optimistic = composerTransaction?.transcriptEntryID { messageEntryIDs[message.messageId] = optimistic }
                return
            }
            if message.replace, let optimistic = composerTransaction?.transcriptEntryID {
                messageEntryIDs[message.messageId] = optimistic
            }
        }
        let blocks = message.content
        let text = blocks.map(Self.contentText).joined()
        if role == .assistant { latestAssistantSource = message.replace ? text : latestAssistantSource + text }
        if let entryID = messageEntryIDs[message.messageId], let index = entries.firstIndex(where: { $0.id == entryID }) {
            if message.replace {
                entries[index].contentBlocks = blocks
                entries[index].text = String(text.suffix(256 * 1024))
                if role == .user {
                    entries[index].presentation = .user(UserMessagePresentation(
                        text: entries[index].text, media: blocks.flatMap { Self.userContent($0).media }
                    ))
                }
            } else {
                entries[index].contentBlocks.append(contentsOf: blocks)
                entries[index].text = String((entries[index].text + text).suffix(256 * 1024))
            }
            entries[index].formatted = nil
            entries[index].isStreaming = true
            streamingEntryIDs.insert(entryID)
            transcriptRevision += 1
            return
        }
        let limitedText = String(text.suffix(256 * 1024))
        let presentation: TranscriptPresentation? = role == .user
            ? .user(UserMessagePresentation(text: limitedText, media: blocks.flatMap { Self.userContent($0).media }))
            : nil
        let entry = TranscriptEntry(
            role: role, text: limitedText, isStreaming: true, contentBlocks: blocks, presentation: presentation
        )
        appendEntry(entry)
        messageEntryIDs[message.messageId] = entry.id
        streamingEntryIDs.insert(entry.id)
    }

    private func applySessionState(_ state: DesktopSessionState) {
        switch state.state {
        case "running":
            canSteer = client.supportsSteering && foregroundRunning
            status = "Running…"
        case "requires_action":
            canSteer = false
            status = "Action required"
        case "idle":
            canSteer = false
            pendingSteers.removeAll()
            if let usage = state.usage { tokenUsage = usage }
            if let turn = activeTurns.first(where: { if case .foreground = $0 { return true }; return false }) {
                reduceSettlement(.terminal(turn, reason: state.stopReason ?? "end_turn", error: nil))
            }
        default: break
        }
    }

    private func applyPlan(_ plan: DesktopPlanContent) {
        let text: String
        switch plan {
        case .items(_, let entries):
            text = entries.map { "[" + ($0.status ?? "pending") + "] " + $0.content }.joined(separator: "\n")
        case .file(_, let uri): text = "[Plan file](" + uri + ")"
        case .markdown(_, let content): text = content
        case .unknown: return
        }
        if let entryID = planEntryIDs[plan.id], let index = entries.firstIndex(where: { $0.id == entryID }) {
            entries[index].text = text; entries[index].formatted = Self.markdown(text)
        } else {
            let entry = TranscriptEntry(role: .plan, title: "Plan", text: text, formatted: Self.markdown(text))
            appendEntry(entry); planEntryIDs[plan.id] = entry.id
        }
        transcriptRevision += 1
    }

    private func removePlan(id: String) {
        if let entryID = planEntryIDs.removeValue(forKey: id) { entries.removeAll { $0.id == entryID } }
        transcriptRevision += 1
    }

    private func applyTurnState(_ state: DesktopTurnState) {
        let turn = Turn.autonomous(state.turnId)
        if state.active { reduceSettlement(.started(turn, prompt: "")) }
        else { reduceSettlement(.terminal(turn, reason: state.error == nil ? "end_turn" : "error", error: state.error.map { ACPClientError.protocolError($0) })) }
        status = state.error ?? (state.active ? "Background turn started" : "Background turn finished")
        upsertSingleton(role: .status, title: "Status", text: status)
    }

    func applyRuntime(_ event: [String: Any]) {
        guard let kind = event["event"] as? String else { return }
        if kind == "session_started" {
            runtimeSessionID = event["session_id"] as? String
            return
        }
        guard expectedRuntimeSessionID != nil, runtimeSessionID == expectedRuntimeSessionID else { return }
        var roster = agentRoster
        if roster.apply(event: event, nowMS: Self.nowMilliseconds()) {
            agentRoster = roster
            scheduleRosterPrune()
            return
        }
        switch kind {
        case "child_started":
            guard let call = event["call"] as? String, let parent = Self.parentCall(call), let index = entries.firstIndex(where: { $0.toolCallID == parent }) else { return }
            entries[index].children.append(RuntimeChild(id: call, tool: event["tool"] as? String ?? "tool", summary: event["summary"] as? String ?? "", running: true, succeeded: nil, durationMS: nil))
        case "child_finished":
            guard let call = event["call"] as? String, let parent = Self.parentCall(call), let entry = entries.firstIndex(where: { $0.toolCallID == parent }), let child = entries[entry].children.firstIndex(where: { $0.id == call }) else { return }
            entries[entry].children[child].running = false
            entries[entry].children[child].succeeded = event["ok"] as? Bool
            entries[entry].children[child].summary = event["summary"] as? String ?? entries[entry].children[child].summary
            entries[entry].children[child].durationMS = (event["millis"] as? NSNumber)?.intValue
        case "compaction_started": status = "Compacting context…"
        case "compaction_finished": status = (event["ok"] as? Bool == true) ? "Context compaction finished" : "Context compaction failed"
        default: return
        }
        transcriptRevision += 1
    }

    private func appendChunk(role: TranscriptRole, content: DesktopContentBlock) {
        if role == .user {
            appendUserContent(content)
            return
        }
        let text = Self.contentText(content)
        guard !text.isEmpty || content != .text("") else { return }
        if let index = entries.indices.last, entries[index].role == role, entries[index].isStreaming {
            entries[index].text = String((entries[index].text + text).suffix(256 * 1024))
            entries[index].contentBlocks.append(content)
            entries[index].formatted = nil
        } else {
            appendEntry(TranscriptEntry(role: role, text: String(text.suffix(256 * 1024)), isStreaming: true, contentBlocks: [content]))
            if let id = entries.last?.id { streamingEntryIDs.insert(id) }
        }
        transcriptRevision += 1
    }

    private func appendUserContent(_ block: DesktopContentBlock) {
        let content = Self.userContent(block)
        guard !content.text.isEmpty || !content.media.isEmpty || block != .text("") else { return }
        if let index = entries.indices.last, entries[index].role == .user, entries[index].isStreaming {
            var current = entries[index].presentation?.userMessage
                ?? UserMessagePresentation(text: entries[index].text, media: [])
            entries[index].text = String((entries[index].text + content.text).suffix(256 * 1024))
            entries[index].contentBlocks.append(block)
            current.text = entries[index].text
            current.media.append(contentsOf: content.media)
            entries[index].presentation = .user(current)
            entries[index].formatted = nil
        } else {
            let text = String(content.text.suffix(256 * 1024))
            appendEntry(TranscriptEntry(
                role: .user, text: text, isStreaming: true, contentBlocks: [block],
                presentation: .user(UserMessagePresentation(text: text, media: content.media))
            ))
            if let id = entries.last?.id { streamingEntryIDs.insert(id) }
        }
        transcriptRevision += 1
    }

    private func appendTurnDuration(since start: ContinuousClock.Instant?) {
        finishActiveThought()
        guard let start else { return }
        appendEntry(TranscriptEntry(
            role: .duration, text: "",
            presentation: .turnDuration(Self.turnDuration(start.duration(to: clock.now)))
        ))
    }

    static func turnDuration(_ elapsed: Duration) -> TurnDurationPresentation {
        let components = elapsed.components
        let milliseconds = components.seconds * 1_000 + components.attoseconds / 1_000_000_000_000_000
        return TurnDurationPresentation(milliseconds: max(0, Int(clamping: milliseconds)))
    }

    private func appendEntry(_ entry: TranscriptEntry) {
        entries.append(entry)
        if entries.count > Self.maximumEntries {
            let count = entries.count - Self.maximumEntries
            let removed = entries.prefix(count).map(\.id)
            entries.removeFirst(count)
            streamingEntryIDs.subtract(removed)
        }
        transcriptRevision += 1
    }

    private func updateTool(_ patch: DesktopToolUpdate) {
        guard let id = patch.toolCallId else { return }
        let update = (toolStates[id] ?? DesktopToolUpdate(toolCallId: id)).merging(patch)
        toolStates[id] = update
        let dictionary = Self.toolDictionary(update)
        if let index = entries.lastIndex(where: { $0.role == .tool && $0.toolCallID == id }) {
            let tool = Self.toolPresentation(dictionary)
            entries[index].title = tool.title
            entries[index].text = tool.detail
            entries[index].presentation = .tool(tool)
            if patch.present.contains("rawInput") {
                entries[index].backgrounded = (update.rawInput?.anyValue as? [String: Any])?["background"] as? Bool == true
            }
            entries[index].isStreaming = tool.status == .inProgress || tool.status == .pending
            if entries[index].isStreaming { streamingEntryIDs.insert(entries[index].id) }
            else { streamingEntryIDs.remove(entries[index].id) }
        } else {
            let tool = Self.toolPresentation(dictionary)
            let entry = TranscriptEntry(
                role: .tool, title: tool.title, text: tool.detail, toolCallID: id,
                isStreaming: tool.status == .inProgress || tool.status == .pending,
                backgrounded: (update.rawInput?.anyValue as? [String: Any])?["background"] as? Bool == true,
                presentation: .tool(tool)
            )
            appendEntry(entry)
            if entry.isStreaming { streamingEntryIDs.insert(entry.id) }
        }
        transcriptRevision += 1
        refreshActivity()
    }

    private func appendToolContent(id: String, content: JSONValue) {
        var patch = toolStates[id] ?? DesktopToolUpdate(toolCallId: id)
        var chunks: [JSONValue]
        if case .array(let existing)? = patch.content { chunks = existing } else { chunks = [] }
        chunks.append(content)
        patch.content = .array(chunks)
        patch.present.insert("content")
        updateTool(patch)
    }

    private func upsertSingleton(role: TranscriptRole, title: String, text: String) {
        if let index = entries.lastIndex(where: { $0.role == role }) { entries[index].text = text; entries[index].formatted = Self.markdown(text) }
        else { appendEntry(TranscriptEntry(role: role, title: title, text: text, formatted: Self.markdown(text))) }
        transcriptRevision += 1
    }

    private func finishActiveThought() {
        guard let entryID = activeThoughtEntryID else { return }
        if let index = entries.firstIndex(where: { $0.id == entryID }) {
            let elapsed = activeThoughtStartedAt.map { $0.duration(to: clock.now) } ?? .zero
            entries[index].isStreaming = false
            entries[index].formatted = Self.markdown(entries[index].text)
            entries[index].presentation = .thought(ThoughtPresentation(
                milliseconds: Self.turnDuration(elapsed).milliseconds
            ))
        }
        streamingEntryIDs.remove(entryID)
        activeThoughtEntryID = nil
        activeThoughtStartedAt = nil
        thoughtTexts.removeAll(keepingCapacity: true)
        thoughtBlocks.removeAll(keepingCapacity: true)
        transcriptRevision += 1
    }

    private func finishStreamingEntries(preservingBackgroundWork: Bool) {
        finishActiveThought()
        var retained: Set<UUID> = []
        for id in streamingEntryIDs {
            guard let index = entries.firstIndex(where: { $0.id == id }) else { continue }
            if preservingBackgroundWork, entries[index].role == .tool, entries[index].backgrounded, entries[index].isStreaming {
                retained.insert(id)
                continue
            }
            entries[index].isStreaming = false
            entries[index].formatted = Self.markdown(entries[index].text)
        }
        streamingEntryIDs = retained
        transcriptRevision += 1
        refreshActivity()
    }

    private func finishResponseStreams() {
        let responseIDs = streamingEntryIDs.filter { id in
            guard let entry = entries.first(where: { $0.id == id }) else { return false }
            return entry.role == .assistant || entry.role == .thought
        }
        for id in responseIDs {
            guard let index = entries.firstIndex(where: { $0.id == id }) else { continue }
            entries[index].isStreaming = false
            entries[index].formatted = Self.markdown(entries[index].text)
        }
        streamingEntryIDs.subtract(responseIDs)
        transcriptRevision += 1
    }

    private func consumeComposerEcho(_ contents: [DesktopContentBlock], requireComplete: Bool) -> Bool {
        guard var transaction = composerTransaction else { return false }
        for content in contents {
            switch content {
            case .text(let text):
                if transaction.text.isEmpty {
                    guard text.isEmpty else { return false }
                    transaction.echoedTextComplete = true
                } else {
                    guard !transaction.echoedTextComplete else { return false }
                    let candidate = transaction.echoedText + text
                    guard transaction.text.hasPrefix(candidate) else { return false }
                    transaction.echoedText = candidate
                    transaction.echoedTextComplete = candidate == transaction.text
                }
            case .image:
                guard transaction.echoedAttachmentIndex < transaction.attachments.count, transaction.attachments[transaction.echoedAttachmentIndex].kind == .image else { return false }
                transaction.echoedAttachmentIndex += 1
            case .audio:
                guard transaction.echoedAttachmentIndex < transaction.attachments.count, transaction.attachments[transaction.echoedAttachmentIndex].kind == .audio else { return false }
                transaction.echoedAttachmentIndex += 1
            default: return false
            }
        }
        if requireComplete {
            guard (transaction.text.isEmpty || transaction.echoedTextComplete),
                  transaction.echoedAttachmentIndex == transaction.attachments.count else { return false }
        }
        composerTransaction = transaction
        return true
    }

    private func reduceSettlement(_ event: Settlement) {
        switch event {
        case .started(let turn, let prompt):
            guard !activeTurns.contains(turn), !settledTurns.contains(turn) else { return }
            activeTurns.insert(turn)
            switch turn {
            case .foreground:
                foregroundRunning = true
            case .autonomous(let id):
                autonomousTurns.insert(id)
                autonomousStartedAt[id] = clock.now
            }
            onTurnStarted?(prompt)
            refreshActivity()
        case .terminal(let turn, let reason, let error):
            guard activeTurns.remove(turn) != nil else { return }
            settledTurns.insert(turn)
            switch turn {
            case .foreground:
                appendTurnDuration(since: foregroundStartedAt)
                foregroundStartedAt = nil
                foregroundRunning = false
                canSteer = false
                isInjecting = false
                pendingSteers.removeAll()
                if let transaction = composerTransaction, transaction.turn == turn {
                    if let error {
                        restoreComposer(transaction)
                        if let entryID = transaction.transcriptEntryID { entries.removeAll { $0.id == entryID } }
                        fail(error)
                    }
                    composerTransaction = nil
                }
            case .autonomous(let id):
                appendTurnDuration(since: autonomousStartedAt.removeValue(forKey: id))
                autonomousTurns.remove(id)
            }
            finishStreamingEntries(preservingBackgroundWork: true)
            if error == nil { status = reason == "cancelled" ? "Cancelled" : "Ready · \(reason.replacingOccurrences(of: "_", with: " "))" }
            if !shuttingDown { onTurnFinished?(error == nil ? reason : "error") }
            refreshActivity()
        case .processExit(let code):
            let turns = activeTurns
            for turn in turns { reduceSettlement(.terminal(turn, reason: "error", error: ACPClientError.process("Kit exited (status \(code))"))) }
            isReady = false
            finishStreamingEntries(preservingBackgroundWork: false)
            if shuttingDown { status = "Closed" } else { status = "Kit exited (status \(code))" }
        case .shutdown:
            guard !shuttingDown else { return }
            shuttingDown = true
            status = "Closing…"
            isReady = false
            let turns = activeTurns
            for turn in turns { reduceSettlement(.terminal(turn, reason: "cancelled", error: nil)) }
            finishStreamingEntries(preservingBackgroundWork: false)
        }
    }

    private func restoreComposer(_ transaction: ComposerTransaction) {
        if draft.isEmpty { draft = transaction.originalDraft }
        else if !transaction.originalDraft.isEmpty, draft != transaction.originalDraft { draft = transaction.originalDraft + "\n" + draft }
        var restored = transaction.attachments
        restored.append(contentsOf: attachments.filter { current in !restored.contains(where: { $0.url == current.url }) })
        attachments = restored
    }

    private func sendSteer(text: String, attachments files: [Attachment]) {
        let snapshot = InjectionSnapshot(text: text, attachments: files)
        draft = ""
        attachments = []
        isInjecting = true
        status = "Queueing…"
        client.inject(text: text, attachments: files) { [weak self] result in
            guard let self else { return }
            self.isInjecting = false
            switch result {
            case .failure(let error):
                self.restoreInjection(snapshot)
                self.fail(error)
            case .success(let response):
                guard self.foregroundRunning else { return }
                self.pendingSteers.append(PendingSteer(
                    id: response.messageId, text: text, attachmentCount: files.count
                ))
                self.status = "Running…"
            }
        }
    }

    private func restoreInjection(_ snapshot: InjectionSnapshot) {
        if draft.isEmpty { draft = snapshot.text }
        else if !snapshot.text.isEmpty, draft != snapshot.text { draft = snapshot.text + "\n" + draft }
        var restored = snapshot.attachments
        var totalBytes = restored.reduce(Int64(0)) { $0 + $1.size }
        for current in attachments where !restored.contains(where: { $0.url == current.url }) {
            guard restored.count < Self.maximumAttachmentCount,
                  totalBytes + current.size <= Self.maximumTotalAttachmentBytes
            else { continue }
            restored.append(current)
            totalBytes += current.size
        }
        attachments = restored
    }

    private func finalizeLastEntry() {
        guard let index = entries.indices.last else { return }
        entries[index].formatted = Self.markdown(entries[index].text)
    }

    private func recordDiagnostic(_ line: String, updateStatus: Bool = true) {
        diagnostics.append(line)
        if diagnostics.count > 50 { diagnostics.removeFirst(diagnostics.count - 50) }
        if updateStatus { status = line }
    }

    private func refreshActivity() {
        isRunning = foregroundRunning || !autonomousTurns.isEmpty
        let toolRunning = entries.contains { $0.role == .tool && $0.isStreaming }
        onActivityChanged?(isRunning || toolRunning)
    }

    private func publishCurrentConfig(userSelected: Bool = false) {
        let modelValue = configOptions.first(where: { $0.id == "model" })?.currentValue ?? "\(conversation.provider):\(conversation.model)"
        let pieces = modelValue.split(separator: ":", maxSplits: 1).map(String.init)
        let provider = pieces.count == 2 ? pieces[0] : conversation.provider
        let model = pieces.count == 2 ? pieces[1] : modelValue
        let effort = Self.reasoningEffort(in: configOptions, fallback: conversation.reasoningEffort)
        conversation.provider = provider
        conversation.model = model
        conversation.reasoningEffort = effort
        if userSelected { conversation.usesConfiguredDefaults = false }
        onConfigChanged?(provider, model, effort, userSelected)
    }

    static func reasoningEffort(in options: [ConfigOption], fallback: String) -> String {
        options.first(where: \.isReasoningEffort)?.currentValue ?? fallback
    }

    private func replaceTranscript(with updates: [DesktopUpdate]) {
        entries.removeAll(keepingCapacity: true)
        streamingEntryIDs.removeAll()
        messageEntryIDs.removeAll()
        planEntryIDs.removeAll()
        toolStates.removeAll()
        activeThoughtEntryID = nil
        activeThoughtStartedAt = nil
        thoughtTexts.removeAll(keepingCapacity: true)
        thoughtBlocks.removeAll(keepingCapacity: true)
        latestAssistantSource = ""
        contextUsed = nil
        contextSize = nil
        transcriptRevision += 1
        for update in updates { apply(update) }
    }

    private func replaceClientAfterClosing(force: Bool) {
        let previous = client
        let generation = clientGeneration
        previous.close(activeTurn: false) { [weak self, weak previous] in
            guard let self, let previous, !self.shuttingDown,
                  self.isCurrentClient(previous, generation: generation) else { return }
            self.clientGeneration &+= 1
            self.client = previous.replacement()
            self.retrying = false
            self.start(force: force)
        }
    }

    private func isCurrentClient(_ candidate: ACPClient, generation: UInt64) -> Bool {
        client === candidate && clientGeneration == generation
    }

    private static func isSessionLockError(_ error: Error) -> Bool {
        guard case ACPClientError.remote(_, let message) = error else { return false }
        let normalized = message.lowercased()
        return normalized.contains("session is locked")
            || normalized.contains("session is actively locked")
            || normalized.contains("thread is locked")
            || normalized.contains("locked by another process")
    }

    private func setLocked(_ locked: Bool) {
        guard isLocked != locked else { return }
        isLocked = locked
        onLockChanged?(locked)
    }

    private func fail(_ error: Error) {
        finishActiveThought()
        status = "Error: \(error.localizedDescription)"
        appendEntry(TranscriptEntry(role: .error, text: error.localizedDescription, formatted: Self.markdown(error.localizedDescription)))
    }

    static func parseConfigOptions(_ value: Any?) -> [ConfigOption] {
        (value as? [[String: Any]] ?? []).compactMap { item in
            guard let id = item["configId"] as? String,
                  let type = item["type"] as? String,
                  type == "select" || type == "boolean" else { return nil }
            let raw = item["options"] as? [[String: Any]] ?? []
            var groups: [ConfigGroup] = []
            var ungrouped: [ConfigChoice] = []
            for option in raw {
                if let value = option["value"] as? String { ungrouped.append(ConfigChoice(value: value, name: option["name"] as? String ?? value)) }
                else if let nested = option["options"] as? [[String: Any]] {
                    let choices = nested.compactMap { child -> ConfigChoice? in
                        guard let value = child["value"] as? String else { return nil }
                        return ConfigChoice(value: value, name: child["name"] as? String ?? value)
                    }
                    let groupID = option["groupId"] as? String ?? option["group"] as? String ?? option["name"] as? String ?? UUID().uuidString
                    groups.append(ConfigGroup(id: groupID, name: option["name"] as? String ?? groupID, choices: choices))
                }
            }
            if type == "boolean" { ungrouped = [ConfigChoice(value: "true", name: "On"), ConfigChoice(value: "false", name: "Off")] }
            if !ungrouped.isEmpty { groups.insert(ConfigGroup(id: "default", name: "Options", choices: ungrouped), at: 0) }
            let current: String
            if let boolean = item["currentValue"] as? Bool { current = boolean ? "true" : "false" }
            else { current = item["currentValue"] as? String ?? groups.first?.choices.first?.value ?? "" }
            return ConfigOption(id: id, name: item["name"] as? String ?? id, category: item["category"] as? String, valueType: type, currentValue: current, groups: groups)
        }
    }

    private static func attachment(_ url: URL) throws -> Attachment {
        let canonical = url.standardizedFileURL
        let values = try canonical.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey])
        guard values.isRegularFile == true else { throw ACPClientError.attachment("\(url.lastPathComponent) is not a regular file") }
        let size = Int64(values.fileSize ?? 0)
        guard size <= maximumAttachmentBytes else { throw ACPClientError.attachment("\(url.lastPathComponent) exceeds the 10 MiB limit") }
        let ext = canonical.pathExtension.lowercased()
        let type: (AttachmentKind, String)? = switch ext {
        case "png": (.image, "image/png")
        case "jpg", "jpeg": (.image, "image/jpeg")
        case "gif": (.image, "image/gif")
        case "webp": (.image, "image/webp")
        case "wav": (.audio, "audio/wav")
        case "mp3": (.audio, "audio/mpeg")
        default: nil
        }
        guard let type else { throw ACPClientError.attachment("Only PNG, JPEG, GIF, WebP, WAV, and MP3 attachments are supported") }
        return Attachment(url: canonical, kind: type.0, mimeType: type.1, size: size)
    }

    func prepareRuntimeSession(_ sessionID: String) {
        rosterPruneWorkItem?.cancel()
        rosterPruneWorkItem = nil
        expectedRuntimeSessionID = sessionID
        runtimeSessionID = nil
        agentRoster = AgentRoster()
        canSteer = false
        isInjecting = false
        pendingSteers.removeAll()
    }

    private func scheduleRosterPrune() {
        rosterPruneWorkItem?.cancel()
        guard let expiry = agentRoster.nextFailedRemovalExpiryMS else {
            rosterPruneWorkItem = nil
            return
        }
        let now = Self.nowMilliseconds()
        let delay = Double(expiry - min(expiry, now)) / 1_000
        let work = DispatchWorkItem { [weak self] in
            guard let self else { return }
            var roster = self.agentRoster
            if roster.pruneExpired(at: Self.nowMilliseconds()) { self.agentRoster = roster }
            self.scheduleRosterPrune()
        }
        rosterPruneWorkItem = work
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: work)
    }

    private static func nowMilliseconds() -> UInt64 {
        UInt64(max(0, Date().timeIntervalSince1970 * 1_000))
    }

    private static func parentCall(_ call: String) -> String? { call.range(of: ":compose:", options: .backwards).map { String(call[..<$0.lowerBound]) } }

    private static func userContent(_ content: DesktopContentBlock) -> UserMessagePresentation {
        switch content {
        case .text(let text):
            return UserMessagePresentation(text: text, media: [])
        case .image(let data, let mimeType, let uri):
            return UserMessagePresentation(text: "", media: [UserMediaPresentation(
                kind: .image, mimeType: mimeType ?? "image/*",
                data: data.flatMap { Data(base64Encoded: $0) }, url: uri.flatMap(URL.init(string:))
            )])
        case .audio(let data, let mimeType):
            return UserMessagePresentation(text: "", media: [UserMediaPresentation(
                kind: .audio, mimeType: mimeType ?? "audio/*", data: data.flatMap { Data(base64Encoded: $0) }
            )])
        case .resourceLink(let uri, let name, let mimeType):
            guard let kind = Self.attachmentKind(mimeType) else {
                return UserMessagePresentation(text: uri, media: [])
            }
            return UserMessagePresentation(text: "", media: [UserMediaPresentation(
                kind: kind, mimeType: mimeType ?? (kind == .image ? "image/*" : "audio/*"),
                name: name, url: URL(string: uri)
            )])
        case .resource(let uri, let mimeType, let text, let blob):
            guard let kind = Self.attachmentKind(mimeType) else {
                return UserMessagePresentation(text: text ?? uri ?? Self.contentText(content), media: [])
            }
            return UserMessagePresentation(text: text ?? "", media: [UserMediaPresentation(
                kind: kind, mimeType: mimeType ?? (kind == .image ? "image/*" : "audio/*"),
                data: blob.flatMap { Data(base64Encoded: $0) }, url: uri.flatMap(URL.init(string:))
            )])
        case .unknown:
            return UserMessagePresentation(text: Self.contentText(content), media: [])
        }
    }

    private static func attachmentKind(_ mimeType: String?) -> AttachmentKind? {
        if mimeType?.hasPrefix("image/") == true { return .image }
        if mimeType?.hasPrefix("audio/") == true { return .audio }
        return nil
    }

    static func userContent(_ value: Any?) -> UserMessagePresentation {
        guard let content = value as? [String: Any] else {
            return UserMessagePresentation(text: describe(value), media: [])
        }
        let type = content["type"] as? String
        let advertisedMIME = content["mimeType"] as? String
        let kind: AttachmentKind? = switch type {
        case "image": .image
        case "audio": .audio
        case "resource_link" where advertisedMIME?.hasPrefix("image/") == true: .image
        case "resource_link" where advertisedMIME?.hasPrefix("audio/") == true: .audio
        default: nil
        }
        guard let kind else { return UserMessagePresentation(text: contentText(value), media: []) }
        let mimeType = advertisedMIME ?? (kind == .image ? "image/*" : "audio/*")
        let encoded = content["data"] as? String ?? content["blob"] as? String
        let url = (content["uri"] as? String).flatMap(URL.init(string:))
        let media = UserMediaPresentation(
            kind: kind, mimeType: mimeType, name: content["name"] as? String,
            data: encoded.flatMap { Data(base64Encoded: $0) }, url: url
        )
        return UserMessagePresentation(text: "", media: [media])
    }

    private static func toolDictionary(_ update: DesktopToolUpdate) -> [String: Any] {
        var dictionary: [String: Any] = [:]
        if let title = update.title { dictionary["title"] = title }
        if let status = update.status { dictionary["status"] = status }
        if let content = update.content { dictionary["content"] = content.anyValue }
        if let rawInput = update.rawInput { dictionary["rawInput"] = rawInput.anyValue }
        if let rawOutput = update.rawOutput { dictionary["rawOutput"] = rawOutput.anyValue }
        return dictionary
    }

    static func toolPresentation(_ update: [String: Any]) -> ToolPresentation {
        let rawInput = update["rawInput"] as? [String: Any]
        let detail = toolDetail(update)
        let allowedComposeKeys: Set<String> = ["script", "input", "background"]
        let isComposeInput = rawInput.map { Set($0.keys).isSubset(of: allowedComposeKeys) } == true
        let compose = isComposeInput ? rawInput?["script"].flatMap { $0 as? String }.map { script in
            let background: ComposeBackgroundRequest? = switch rawInput?["background"] {
            case let value as Bool: .immediate(value)
            case let value as NSNumber: .delay(seconds: value.intValue)
            default: nil
            }
            return ComposePresentation(
                script: script, input: rawInput?["input"].flatMap(PresentationJSON.init),
                background: background, output: update["rawOutput"].flatMap(PresentationJSON.init)
            )
        } : nil
        return ToolPresentation(
            title: update["title"] as? String ?? "Tool",
            status: (update["status"] as? String).map(ToolPresentationStatus.init) ?? .inProgress,
            detail: detail, compose: compose
        )
    }

    private static func contentText(_ content: DesktopContentBlock) -> String {
        switch content {
        case .text(let text): return text
        case .image(_, let mimeType, let uri): return uri.map { "[Image output](\($0))" } ?? "Image output (inline \(mimeType ?? "image"))"
        case .audio(_, let mimeType): return "Audio output (inline \(mimeType ?? "audio"))"
        case .resourceLink(let uri, _, _): return uri
        case .resource(let uri, let mimeType, let text, _): return text ?? uri ?? "Resource output (inline \(mimeType ?? "resource"))"
        case .unknown(let type): return "Unsupported content (\(type))"
        }
    }

    private static func contentText(_ value: Any?) -> String {
        guard let content = value as? [String: Any] else { return describe(value) }
        switch content["type"] as? String {
        case "text": return content["text"] as? String ?? ""
        case "image":
            if let uri = content["uri"] as? String { return "[Image output](\(uri))" }
            return "Image output (inline \(content["mimeType"] as? String ?? "image"))"
        case "audio": return "Audio output (inline \(content["mimeType"] as? String ?? "audio"))"
        case "resource_link": return content["uri"] as? String ?? "Resource"
        default: return describe(content.filter { $0.key != "data" && $0.key != "blob" })
        }
    }

    private static func toolDetail(_ update: [String: Any]) -> String {
        if let content = update["content"] as? [[String: Any]] {
            let text = content.compactMap { item -> String? in
                if item["type"] as? String == "diff" {
                    let path = item["path"] as? String ?? "diff"
                    let lines = (item["newText"] as? String ?? "").split(separator: "\n", omittingEmptySubsequences: false).count
                    return "\(path) · \(lines) lines"
                }
                guard let body = item["content"] as? [String: Any] else { return nil }
                return contentText(body)
            }.joined(separator: "\n")
            if !text.isEmpty { return bounded(text) }
        }
        let value = update["rawOutput"] ?? update["rawInput"]
        return bounded(readableToolOutput(value) ?? describe(value))
    }

    private static func readableToolOutput(_ value: Any?) -> String? {
        if let text = value as? String {
            if let data = text.data(using: .utf8),
               let decoded = try? JSONSerialization.jsonObject(with: data),
               let readable = readableToolOutput(decoded) { return readable }
            return text
        }
        guard let object = value as? [String: Any] else { return nil }
        if let preview = object["preview"] as? String, !preview.isEmpty { return preview }
        if let text = object["text"] as? String, !text.isEmpty { return text }
        var sections: [String] = []
        if let stdout = object["stdout"] as? String, !stdout.isEmpty { sections.append(stdout) }
        if let stderr = object["stderr"] as? String, !stderr.isEmpty { sections.append("stderr:\n\(stderr)") }
        if !sections.isEmpty { return sections.joined(separator: "\n") }
        if let command = object["command"] as? String { return command }
        for key in ["output", "content", "message"] {
            if let nested = object[key], let readable = readableToolOutput(nested), !readable.isEmpty { return readable }
        }
        return nil
    }

    private static func bounded(_ text: String) -> String {
        let lines = text.split(separator: "\n", omittingEmptySubsequences: false).prefix(5000).joined(separator: "\n")
        return String(lines.prefix(128 * 1024))
    }

    private static func describe(_ value: Any?) -> String {
        guard let value else { return "" }
        if let text = value as? String { return text }
        if JSONSerialization.isValidJSONObject(value), let data = try? JSONSerialization.data(withJSONObject: value, options: [.prettyPrinted, .sortedKeys]) { return String(decoding: data.prefix(128 * 1024), as: UTF8.self) }
        return String(String(describing: value).prefix(128 * 1024))
    }

    private static func markdown(_ text: String) -> AttributedString {
        (try? AttributedString(markdown: text, options: .init(interpretedSyntax: .full))) ?? AttributedString(text)
    }
}
