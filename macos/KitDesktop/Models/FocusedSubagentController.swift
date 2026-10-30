import Foundation
import CoreFoundation
import Combine

// Child observations are connection-local ACP notifications, never transcript RPCs.
protocol FocusedSubagentTransport: AnyObject {
    var childSessions: DesktopChildSessions { get }
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
    private(set) var generation: UInt64 = 0
    private let transport: FocusedSubagentTransport
    private var childID = ""
    private var epoch = UUID()
    private var active = false
    private var capability = false
    private var readEnabled = false
    private var sessionMessages = DesktopSessionMessages()
    private var observation: AnyCancellable?

    init(transport: FocusedSubagentTransport) { self.transport = transport }

    func focus(_ row: AgentRosterRow, canSteer: Bool) {
        stop()
        childID = row.id
        generation = row.generation
        entries = []
        sessionMessages = DesktopSessionMessages()
        partial = true // ACP reconnects can replay only a bounded tail.
        active = row.status == .working
        capability = canSteer
        readEnabled = true
        self.canSteer = active && capability && row.parentID == nil
        for value in transport.childSessions.history[row.id] ?? [] { apply(value) }
        observation = transport.childSessions.updates.sink { [weak self] id, value in
            guard let self, self.readEnabled, id == self.childID else { return }
            self.apply(value)
        }
        refreshNotice()
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
        self.canSteer = active && capability && readEnabled && row.parentID == nil
        if !loading { refreshNotice() }
    }

    func stop() {
        epoch = UUID()
        observation = nil
        readEnabled = false
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

    private func refreshNotice() {
        if steering { notice = "Sending steer to child…" }
        else if loading { notice = "Loading child transcript…" }
        else if !active { notice = "Child idle or closed; transcript is read-only." }
        else if !canSteer { notice = "Read-only: this child has not advertised steering support." }
        else { notice = "Text steering available. Attachments are not supported by the child steering protocol." }
    }

    private func apply(_ value: JSONValue) {
        do {
            if let message = sessionMessages.apply(value, session: childID) {
                partial = partial || message.partial
                put(id: "session:" + message.messageID, title: message.title, text: message.text)
                return
            }
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

// Main-queue confined, ephemeral state. Ownership must be announced parent-first.
final class DesktopChildSessions {
    let updates = PassthroughSubject<(String, JSONValue), Never>()
    private(set) var roster = AgentRoster()
    private(set) var history: [String: [JSONValue]] = [:]
    private var owners: [String: String] = [:]
    private var snapshots: [String: [String: Any]] = [:]
    private var retained: [(String, Int)] = []
    private var bytes = 0

    func reset() {
        roster.reset()
        history.removeAll()
        owners.removeAll()
        snapshots.removeAll()
        retained.removeAll()
        bytes = 0
    }

    @discardableResult
    func receive(root: String, session: String, update: [String: Any]) -> Bool {
        guard session == root || owners[session] != nil,
              let data = try? JSONSerialization.data(withJSONObject: update), data.count <= 64 * 1024 else { return false }
        if update["sessionUpdate"] as? String == "subagent_update" {
            guard let id = update["sessionId"] as? String, !id.isEmpty, id != root, id != session,
                  owners[id] == nil || owners[id] == session,
                  owners[id] != nil || owners.count < 1024 else { return false }
            let old = roster.rowsByID[id]
            let meta = update["_meta"] as? [String: Any]
            let grant = meta?["kit/subagent/steer"] as? [String: Any]
            let generation = (grant?["generation"] as? NSNumber).flatMap { number -> UInt64? in
                guard CFGetTypeID(number) != CFBooleanGetTypeID(), number.doubleValue >= 0,
                      number.stringValue == String(number.uint64Value) else { return nil }
                return number.uint64Value
            }
            if let generation, let old, generation < old.generation { return false }
            owners[id] = session
            var snapshot = snapshots[id] ?? [:]
            for key in ["title", "description", "state"] where update[key] != nil { snapshot[key] = update[key] }
            snapshots[id] = snapshot
            let state = (snapshot["state"] as? [String: Any])?["state"] as? String
            let status: SubagentStatus = state == "running" || state == "requires_action" ? .working : (state == nil ? .starting : .idle)
            let currentGeneration = generation ?? old?.generation ?? 0
            var canSteer = roster.canSteer(id: id, generation: currentGeneration)
            if old?.generation != currentGeneration || status != .working { canSteer = false }
            if update["_meta"] is NSNull || meta?["kit/subagent/steer"] != nil {
                let permission = grant?["canSteer"] as? NSNumber
                canSteer = generation != nil && permission.map {
                    CFGetTypeID($0) == CFBooleanGetTypeID() && $0.boolValue
                } == true
            }
            canSteer = canSteer && session == root && status == .working
            let row = AgentRosterRow(id: id, name: snapshot["title"] as? String ?? id, status: status,
                outcome: nil, generation: currentGeneration, task: snapshot["description"] as? String ?? "",
                parentID: session == root ? nil : session, parentName: nil, harness: "", model: nil,
                createdAtMS: 0, generationStartedAtMS: 0, generationFinishedAtMS: nil)
            roster.applyStreamed(row: row, canSteer: canSteer)
            return true
        }
        guard session != root, let wire = try? JSONDecoder().decode(JSONValue.self, from: data) else { return false }
        if update["sessionUpdate"] as? String == "state_update", let owner = owners[session] {
            var state = update
            state.removeValue(forKey: "sessionUpdate")
            // State changes revoke grants; running alone cannot restore them.
            _ = receive(root: root, session: owner, update: [
                "sessionUpdate": "subagent_update", "sessionId": session, "state": state
            ])
        }
        history[session, default: []].append(wire)
        retained.append((session, data.count))
        bytes += data.count
        while retained.count > 512 || bytes > 4 * 1024 * 1024 {
            let (owner, size) = retained.removeFirst()
            history[owner]?.removeFirst()
            bytes -= size
        }
        updates.send((session, wire))
        return true
    }
}
