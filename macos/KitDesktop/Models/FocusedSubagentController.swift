import Foundation
import Combine

struct SubagentTranscriptPage: Decodable {
    let updates: [JSONValue]
    let nextCursor: UInt64
    let generation: UInt64
    let caughtUp: Bool
    enum CodingKeys: String, CodingKey {
        case updates, generation
        case nextCursor = "next_cursor"
        case caughtUp = "caught_up"
    }
}

// The same boundary serves the live ACP connection and protocol fixtures.
protocol FocusedSubagentTransport: AnyObject {
    func readSubagentTranscript(id: String, generation: UInt64, cursor: UInt64,
                               completion: @escaping (Result<SubagentTranscriptPage, Error>) -> Void)
    func steerSubagent(id: String, generation: UInt64, prompt: String,
                      completion: @escaping (Result<[String: Any], Error>) -> Void)
}
extension ACPClient: FocusedSubagentTransport {}

struct FocusedSubagentEntry: Identifiable, Equatable {
    let id: String
    var title: String
    var text: String
}

@MainActor
final class FocusedSubagentController: ObservableObject {
    static let maximumEntries = 500
    static let maximumTextCharacters = 16_384
    @Published private(set) var entries: [FocusedSubagentEntry] = []
    @Published private(set) var notice = "Loading child transcript…"
    @Published private(set) var partial = false
    @Published private(set) var loading = false
    @Published private(set) var steering = false
    @Published private(set) var canSteer = false
    @Published var draft = ""
    private(set) var cursor: UInt64 = 0
    private(set) var generation: UInt64 = 0
    private let transport: FocusedSubagentTransport
    private var childID = ""
    private var epoch = UUID()
    private var active = false
    private var capability = false
    private var readEnabled = false
    private var pendingRead = false
    private var poll: Task<Void, Never>?

    init(transport: FocusedSubagentTransport) { self.transport = transport }

    func focus(_ row: AgentRosterRow, canSteer: Bool) {
        stop()
        childID = row.id
        generation = row.generation
        cursor = 0
        entries = []
        partial = false
        active = row.status == .working
        capability = canSteer
        readEnabled = row.parentID == nil
        self.canSteer = active && capability && readEnabled
        guard readEnabled else {
            notice = "Descendant transcript inspection is not supported."
            return
        }
        loading = true
        notice = "Loading child transcript…"
        readNext()
    }

    func update(_ row: AgentRosterRow?, canSteer: Bool) {
        guard let row, row.id == childID else {
            stop()
            notice = "Child closed; transcript is read-only."
            return
        }
        guard row.generation >= generation else { return }
        if row.generation != generation { focus(row, canSteer: canSteer); return }
        active = row.status == .working
        capability = canSteer
        self.canSteer = active && capability && readEnabled
        if !loading { refreshNotice() }
    }

    func stop() {
        epoch = UUID()
        poll?.cancel()
        poll = nil
        readEnabled = false
        pendingRead = false
        loading = false
        steering = false
        canSteer = false
    }

    func disconnected() {
        stop()
        notice = "Connection closed; transcript is read-only."
    }

    func send() {
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canSteer, !steering, !text.isEmpty, text.utf8.count <= 16 * 1024 else { return }
        let token = epoch
        let submitted = draft
        steering = true
        notice = "Sending steer to child…"
        transport.steerSubagent(id: childID, generation: generation, prompt: text) { [weak self] result in
            guard let self, self.epoch == token else { return }
            self.steering = false
            switch result {
            case .success:
                if self.draft == submitted { self.draft = "" }
                self.notice = "Steer accepted."
            case .failure(let error): self.notice = "Steer failed: \(error.localizedDescription)"
            }
        }
    }

    private func readNext() {
        guard readEnabled, !pendingRead else { return }
        pendingRead = true
        let token = epoch
        let requestedCursor = cursor
        let requestedGeneration = generation
        transport.readSubagentTranscript(id: childID, generation: generation, cursor: cursor) { [weak self] result in
            guard let self, self.epoch == token, self.cursor == requestedCursor,
                  self.generation == requestedGeneration, self.readEnabled else { return }
            self.pendingRead = false
            switch result {
            case .failure(let error):
                self.stop()
                self.notice = "Transcript unavailable: \(error.localizedDescription). Reopen to retry."
            case .success(let page):
                guard page.generation == requestedGeneration, page.nextCursor >= requestedCursor,
                      page.updates.isEmpty || page.nextCursor > requestedCursor else {
                    self.stop()
                    self.notice = "Transcript generation or cursor changed. Reopen to resync."
                    return
                }
                for value in page.updates.prefix(2048) { self.apply(value) }
                if page.updates.count > 2048 { self.partial = true }
                self.cursor = page.nextCursor
                if page.caughtUp { self.loading = false }
                self.refreshNotice()
                // Drain idle transcripts once, then stop polling. Empty writer pages back off too.
                if !self.active && page.caughtUp { return }
                let delay: UInt64 = page.caughtUp || page.nextCursor == requestedCursor ? 500_000_000 : 10_000_000
                self.poll = Task { [weak self] in
                    do { try await Task.sleep(nanoseconds: delay) } catch { return }
                    guard let self, self.epoch == token else { return }
                    self.readNext()
                }
            }
        }
    }

    private func refreshNotice() {
        if steering { notice = "Sending steer to child…" }
        else if loading { notice = "Loading child transcript…" }
        else if !active { notice = "Child idle or closed; transcript is read-only." }
        else if !canSteer { notice = "Read-only: this child has not advertised steering support." }
        else { notice = "Text steering available. Attachments are not supported by the child steering protocol." }
    }

    private func apply(_ value: JSONValue) {
        do {
            let update = try DesktopUpdate(wire: value)
            switch update {
            case .userMessage(let message): appendMessage(message, role: "You")
            case .agentMessage(let message): appendMessage(message, role: "Assistant")
            case .agentThought(let message): appendMessage(message, role: "Reasoning")
            case .toolCall(let tool), .toolCallUpdate(let tool):
                let id = "tool:" + (tool.toolCallId ?? UUID().uuidString)
                let existing = entries.first { $0.id == id }
                let body = tool.content.map(renderJSON) ?? tool.rawOutput.map(renderJSON) ?? existing?.text ?? ""
                put(id: id, title: (tool.title ?? existing?.title ?? "Tool") + (tool.status.map { " · \($0)" } ?? ""), text: body)
            case .toolCallContent(let id, let content):
                let key = "tool:" + id
                let existing = entries.first { $0.id == key }
                let chunk = renderJSON(content)
                let previous = existing?.text ?? ""
                put(id: key, title: existing?.title ?? "Tool",
                    text: previous.isEmpty ? chunk : previous + "\n" + chunk)
            case .plan(let plan):
                switch plan {
                case .items(let id, let items): put(id: "plan:" + id, title: "Plan", text: items.map { "\($0.status ?? "pending"): \($0.content)" }.joined(separator: "\n"))
                case .markdown(let id, let content): put(id: "plan:" + id, title: "Plan", text: content)
                case .file(let id, let uri): put(id: "plan:" + id, title: "Plan file", text: uri)
                case .unknown: partial = true
                }
            case .planRemoved(let id): entries.removeAll { $0.id == "plan:" + id }
            case .usage, .tokenUsage, .configOptions, .sessionInfo, .availableCommands, .currentMode, .state, .turnState: break
            default: partial = true
            }
        } catch { partial = true }
    }

    private func appendMessage(_ message: DesktopMessageUpdate, role: String) {
        let id = role + ":" + message.messageId
        let text = message.content.map(renderBlock).joined()
        let previous = message.replace ? "" : entries.first { $0.id == id }?.text ?? ""
        guard message.hasContent else { return }
        put(id: id, title: role, text: previous + text)
    }

    private func put(id: String, title: String, text: String) {
        let clipped = String(text.prefix(Self.maximumTextCharacters))
        if clipped != text { partial = true }
        let entry = FocusedSubagentEntry(id: id, title: String(title.prefix(300)), text: clipped)
        if let index = entries.firstIndex(where: { $0.id == id }) { entries[index] = entry }
        else { entries.append(entry) }
        if entries.count > Self.maximumEntries {
            entries.removeFirst(entries.count - Self.maximumEntries)
            partial = true
        }
    }

    private func renderBlock(_ block: DesktopContentBlock) -> String {
        switch block {
        case .text(let text): return text
        case .resourceLink(let uri, let name, _): return "\n\(name ?? "Resource"): \(uri)\n"
        case .resource(let uri, _, let text, _): return text ?? "\nResource: \(uri ?? "embedded")\n"
        case .image(_, _, let uri): partial = true; return "\n[Image\(uri.map { ": \($0)" } ?? "")]\n"
        case .audio: partial = true; return "\n[Audio]\n"
        case .unknown: partial = true; return "\n[Unsupported content]\n"
        }
    }

    private func renderJSON(_ value: JSONValue) -> String {
        guard let data = try? JSONEncoder().encode(value), let text = String(data: data, encoding: .utf8) else { return "" }
        return text
    }
}
