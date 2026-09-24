import Foundation

struct Workspace: Codable, Identifiable, Equatable {
    let id: UUID
    var name: String
    var path: String
    var createdAt: Date

    init(id: UUID = UUID(), name: String, path: String, createdAt: Date = Date()) {
        self.id = id
        self.name = name
        self.path = path
        self.createdAt = createdAt
    }
}

struct Conversation: Codable, Identifiable, Equatable {
    let id: UUID
    let workspaceID: UUID
    var title: String
    var sessionID: String?
    var createdAt: Date
    var updatedAt: Date
    var unread: Bool
    var awaitingUser: Bool
    var provider: String
    var model: String
    var reasoningEffort: String
    var usesConfiguredDefaults: Bool

    init(
        id: UUID = UUID(), workspaceID: UUID, title: String = "New conversation",
        sessionID: String? = nil, createdAt: Date = Date(), updatedAt: Date = Date(),
        unread: Bool = false, awaitingUser: Bool = false,
        provider: String = "openai-subscription", model: String = "gpt-5.4",
        reasoningEffort: String = "default", usesConfiguredDefaults: Bool = true
    ) {
        self.id = id
        self.workspaceID = workspaceID
        self.title = title
        self.sessionID = sessionID
        self.createdAt = createdAt
        self.updatedAt = updatedAt
        self.unread = unread
        self.awaitingUser = awaitingUser
        self.provider = provider
        self.model = model
        self.reasoningEffort = reasoningEffort
        self.usesConfiguredDefaults = usesConfiguredDefaults
    }

    private enum CodingKeys: String, CodingKey {
        case id, workspaceID, title, sessionID, createdAt, updatedAt, unread, awaitingUser
        case provider, model, reasoningEffort, usesConfiguredDefaults
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        id = try values.decode(UUID.self, forKey: .id)
        workspaceID = try values.decode(UUID.self, forKey: .workspaceID)
        title = try values.decode(String.self, forKey: .title)
        sessionID = try values.decodeIfPresent(String.self, forKey: .sessionID)
        createdAt = try values.decode(Date.self, forKey: .createdAt)
        updatedAt = try values.decode(Date.self, forKey: .updatedAt)
        unread = try values.decodeIfPresent(Bool.self, forKey: .unread) ?? false
        awaitingUser = try values.decodeIfPresent(Bool.self, forKey: .awaitingUser) ?? false
        provider = try values.decodeIfPresent(String.self, forKey: .provider) ?? "openai-subscription"
        model = try values.decodeIfPresent(String.self, forKey: .model) ?? "gpt-5.4"
        reasoningEffort = try values.decodeIfPresent(String.self, forKey: .reasoningEffort) ?? "default"
        // Schema 1 and 2 stored hard-coded desktop fallbacks as if they were user
        // choices. Treat those records as inherited so the CLI can re-resolve config.
        usesConfiguredDefaults = try values.decodeIfPresent(Bool.self, forKey: .usesConfiguredDefaults) ?? true
    }
}

struct PersistedAppState: Codable, Equatable {
    static let currentSchemaVersion = 3
    var schemaVersion: Int
    var workspaces: [Workspace]
    var conversations: [Conversation]

    init(schemaVersion: Int = Self.currentSchemaVersion, workspaces: [Workspace] = [], conversations: [Conversation] = []) {
        self.schemaVersion = schemaVersion
        self.workspaces = workspaces
        self.conversations = conversations
    }

    private enum CodingKeys: String, CodingKey { case schemaVersion, workspaces, conversations }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        schemaVersion = try values.decodeIfPresent(Int.self, forKey: .schemaVersion) ?? 1
        guard schemaVersion <= Self.currentSchemaVersion else {
            throw PersistenceError.unsupportedSchema(schemaVersion)
        }
        workspaces = try values.decodeIfPresent([Workspace].self, forKey: .workspaces) ?? []
        conversations = try values.decodeIfPresent([Conversation].self, forKey: .conversations) ?? []
        schemaVersion = Self.currentSchemaVersion
    }
}

enum TranscriptRole: String { case user, assistant, thought, tool, plan, status, usage, error, duration }

enum ToolPresentationStatus: String, Equatable {
    case pending
    case inProgress = "in_progress"
    case completed
    case failed
    case unknown

    init(_ value: String?) {
        self = value.flatMap(Self.init(rawValue:)) ?? .unknown
    }

    var label: String { rawValue.replacingOccurrences(of: "_", with: " ") }
}

indirect enum PresentationJSON: Equatable {
    case object([String: PresentationJSON])
    case array([PresentationJSON])
    case string(String)
    case number(Double)
    case bool(Bool)
    case null

    init?(_ value: Any) {
        switch value {
        case is NSNull: self = .null
        case let value as Bool: self = .bool(value)
        case let value as NSNumber: self = .number(value.doubleValue)
        case let value as String: self = .string(value)
        case let value as [Any]: self = .array(value.compactMap(PresentationJSON.init))
        case let value as [String: Any]:
            self = .object(value.reduce(into: [:]) { result, item in result[item.key] = PresentationJSON(item.value) })
        default: return nil
        }
    }

    var formatted: String {
        let data = try? JSONSerialization.data(
            withJSONObject: foundationValue, options: [.prettyPrinted, .sortedKeys, .fragmentsAllowed]
        )
        return data.map { String(decoding: $0, as: UTF8.self) } ?? ""
    }

    private var foundationValue: Any {
        switch self {
        case .object(let value): return value.mapValues(\.foundationValue)
        case .array(let value): return value.map(\.foundationValue)
        case .string(let value): return value
        case .number(let value): return value
        case .bool(let value): return value
        case .null: return NSNull()
        }
    }
}

enum ComposeBackgroundRequest: Equatable {
    case immediate(Bool)
    case delay(seconds: Int)
}

struct ComposePresentation: Equatable {
    let intent: String?
    let script: String
    let input: PresentationJSON?
    let background: ComposeBackgroundRequest?
    var output: PresentationJSON?

    static func childToolSummary(_ toolNames: [String], maximumDistinctTools: Int = 4) -> String {
        let counts = Dictionary(toolNames.map { ($0, 1) }, uniquingKeysWith: +)
        return counts.sorted { lhs, rhs in
            lhs.value == rhs.value ? lhs.key < rhs.key : lhs.value > rhs.value
        }
        .prefix(max(0, maximumDistinctTools))
        .map { name, count in count > 1 ? "\(name) x \(count)" : name }
        .joined(separator: ", ")
    }
}

struct ToolPresentation: Equatable {
    var title: String
    var status: ToolPresentationStatus
    var detail: String
    var compose: ComposePresentation?
}

struct UserMediaPresentation: Identifiable, Equatable {
    let id: UUID
    let kind: AttachmentKind
    let mimeType: String
    let name: String?
    let data: Data?
    let url: URL?

    init(
        id: UUID = UUID(), kind: AttachmentKind, mimeType: String, name: String? = nil,
        data: Data? = nil, url: URL? = nil
    ) {
        self.id = id
        self.kind = kind
        self.mimeType = mimeType
        self.name = name
        self.data = data
        self.url = url
    }
}

struct UserMessagePresentation: Equatable {
    var text: String
    var media: [UserMediaPresentation]
}

struct TurnDurationPresentation: Equatable {
    let milliseconds: Int
}

struct ThoughtPresentation: Equatable {
    let milliseconds: Int
}

enum TranscriptPresentation: Equatable {
    case user(UserMessagePresentation)
    case thought(ThoughtPresentation)
    case tool(ToolPresentation)
    case turnDuration(TurnDurationPresentation)

    var userMessage: UserMessagePresentation? {
        guard case .user(let message) = self else { return nil }
        return message
    }

    var thought: ThoughtPresentation? {
        guard case .thought(let thought) = self else { return nil }
        return thought
    }

    var tool: ToolPresentation? {
        guard case .tool(let tool) = self else { return nil }
        return tool
    }
}

struct RuntimeChild: Identifiable, Equatable {
    let id: String
    var tool: String
    var summary: String
    var running: Bool
    var succeeded: Bool?
    var durationMS: Int?
}

enum SubagentStatus: String, Equatable {
    case starting, working, idle, removed

    var rank: Int {
        switch self {
        case .starting: 0
        case .working: 1
        case .idle: 2
        case .removed: 3
        }
    }
}

enum SubagentOutcome: String, Equatable { case success, failed }

struct AgentRosterRow: Identifiable, Equatable {
    let id: String
    let name: String
    let status: SubagentStatus
    let outcome: SubagentOutcome?
    let generation: UInt64
    let task: String
    let parentID: String?
    let parentName: String?
    let harness: String
    let model: String?
    let createdAtMS: UInt64
    let generationStartedAtMS: UInt64
    let generationFinishedAtMS: UInt64?
}

struct AgentRosterTreeRow: Identifiable, Equatable {
    var id: String { row.id }
    let row: AgentRosterRow
    let depth: Int
    let missingParent: Bool
}

struct AgentRosterCounts: Equatable {
    var total = 0
    var starting = 0
    var working = 0
    var idle = 0
}

struct AgentRoster: Equatable {
    private struct Version: Equatable, Comparable {
        let generation: UInt64
        let statusRank: Int

        static func < (lhs: Version, rhs: Version) -> Bool {
            (lhs.generation, lhs.statusRank) < (rhs.generation, rhs.statusRank)
        }
    }

    private(set) var rowsByID: [String: AgentRosterRow] = [:]
    private var versions: [String: Version] = [:]
    private var cleanedIDs: Set<String> = []

    var nextFailedRemovalExpiryMS: UInt64? {
        rowsByID.values.compactMap { row in
            guard row.status == .removed, row.outcome == .failed, let finished = row.generationFinishedAtMS else { return nil }
            let (expiry, overflow) = finished.addingReportingOverflow(4_000)
            return overflow ? UInt64.max : expiry
        }.min()
    }

    var counts: AgentRosterCounts {
        rowsByID.values.reduce(into: AgentRosterCounts()) { counts, row in
            switch row.status {
            case .starting: counts.starting += 1
            case .working: counts.working += 1
            case .idle: counts.idle += 1
            case .removed: return
            }
            counts.total += 1
        }
    }

    var treeRows: [AgentRosterTreeRow] {
        let ordered = rowsByID.values.sorted(by: Self.ordersBefore)
        var children: [String: [AgentRosterRow]] = [:]
        var roots: [AgentRosterRow] = []
        for row in ordered {
            if ancestryIsAcyclic(row), let parentID = row.parentID, rowsByID[parentID] != nil {
                children[parentID, default: []].append(row)
            } else {
                roots.append(row)
            }
        }
        var result: [AgentRosterTreeRow] = []
        func append(_ row: AgentRosterRow, depth: Int, missingParent: Bool) {
            result.append(AgentRosterTreeRow(row: row, depth: depth, missingParent: missingParent))
            for child in children[row.id] ?? [] { append(child, depth: depth + 1, missingParent: false) }
        }
        for root in roots {
            append(root, depth: 0, missingParent: root.parentID.map { rowsByID[$0] == nil } ?? false)
        }
        return result
    }

    mutating func reset() { self = AgentRoster() }

    @discardableResult
    mutating func apply(event: [String: Any], nowMS: UInt64) -> Bool {
        switch event["event"] as? String {
        case "subagent_state_changed":
            guard let row = Self.parseRow(event), !cleanedIDs.contains(row.id) else { return false }
            let incoming = Version(generation: row.generation, statusRank: row.status.rank)
            if let current = versions[row.id], incoming <= current { return false }
            versions[row.id] = incoming
            if row.status == .removed,
               row.outcome != .failed || row.generationFinishedAtMS.map({ nowMS - min(nowMS, $0) >= 4_000 }) != false {
                rowsByID.removeValue(forKey: row.id)
            } else {
                rowsByID[row.id] = row
            }
            return true
        case "subagent_descendants_removed":
            guard let ancestorID = event["ancestor_id"] as? String else { return false }
            var removed: Set<String> = []
            while true {
                let before = removed.count
                for row in rowsByID.values where row.id != ancestorID {
                    if row.parentID == ancestorID || row.parentID.map(removed.contains) == true { removed.insert(row.id) }
                }
                if removed.count == before { break }
            }
            rowsByID = rowsByID.filter { !removed.contains($0.key) }
            cleanedIDs.formUnion(removed)
            return true
        default: return false
        }
    }

    @discardableResult
    mutating func pruneExpired(at nowMS: UInt64) -> Bool {
        let before = rowsByID.count
        rowsByID = rowsByID.filter { _, row in
            row.status != .removed || row.outcome != .failed
                || row.generationFinishedAtMS.map { nowMS - min(nowMS, $0) < 4_000 } == true
        }
        return rowsByID.count != before
    }

    mutating func retireActive(at nowMS: UInt64) {
        for (id, row) in rowsByID where row.status == .starting || row.status == .working {
            let retired = AgentRosterRow(
                id: row.id, name: row.name, status: .removed, outcome: .failed,
                generation: row.generation, task: row.task, parentID: row.parentID,
                parentName: row.parentName, harness: row.harness, model: row.model,
                createdAtMS: row.createdAtMS, generationStartedAtMS: row.generationStartedAtMS,
                generationFinishedAtMS: row.generationFinishedAtMS ?? nowMS
            )
            rowsByID[id] = retired
            versions[id] = Version(generation: row.generation, statusRank: SubagentStatus.removed.rank)
        }
    }

    private func ancestryIsAcyclic(_ row: AgentRosterRow) -> Bool {
        var seen: Set<String> = []
        var current: AgentRosterRow? = row
        while let value = current {
            guard seen.insert(value.id).inserted else { return false }
            current = value.parentID.flatMap { rowsByID[$0] }
        }
        return true
    }

    private static func ordersBefore(_ lhs: AgentRosterRow, _ rhs: AgentRosterRow) -> Bool {
        if lhs.status.rank != rhs.status.rank { return lhs.status.rank < rhs.status.rank }
        if lhs.createdAtMS != rhs.createdAtMS { return lhs.createdAtMS < rhs.createdAtMS }
        return lhs.id < rhs.id
    }

    private static func parseRow(_ event: [String: Any]) -> AgentRosterRow? {
        guard
            let id = event["id"] as? String, let name = event["name"] as? String,
            let statusValue = event["status"] as? String, let status = SubagentStatus(rawValue: statusValue),
            let generation = unsigned(event["generation"]), let task = event["task"] as? String,
            let harness = event["harness"] as? String, let created = unsigned(event["created_at_unix_ms"]),
            let started = unsigned(event["generation_started_at_unix_ms"])
        else { return nil }
        let outcome = (event["outcome"] as? String).flatMap(SubagentOutcome.init(rawValue:))
        return AgentRosterRow(
            id: id, name: name, status: status, outcome: outcome, generation: generation, task: task,
            parentID: event["parent_id"] as? String, parentName: event["parent_name"] as? String,
            harness: harness, model: event["model"] as? String, createdAtMS: created,
            generationStartedAtMS: started, generationFinishedAtMS: unsigned(event["generation_finished_at_unix_ms"])
        )
    }

    private static func unsigned(_ value: Any?) -> UInt64? {
        guard let number = value as? NSNumber else { return nil }
        return UInt64(number.stringValue)
    }
}

struct TranscriptEntry: Identifiable {
    let id: UUID
    var role: TranscriptRole
    var title: String?
    var text: String
    var toolCallID: String?
    var isStreaming: Bool
    var formatted: AttributedString?
    var children: [RuntimeChild]
    var backgrounded: Bool
    var contentBlocks: [DesktopContentBlock]
    var presentation: TranscriptPresentation?

    init(
        id: UUID = UUID(), role: TranscriptRole, title: String? = nil, text: String,
        toolCallID: String? = nil, isStreaming: Bool = false, formatted: AttributedString? = nil,
        children: [RuntimeChild] = [], backgrounded: Bool = false,
        contentBlocks: [DesktopContentBlock] = [], presentation: TranscriptPresentation? = nil
    ) {
        self.id = id
        self.role = role
        self.title = title
        self.text = text
        self.toolCallID = toolCallID
        self.isStreaming = isStreaming
        self.formatted = formatted
        self.children = children
        self.backgrounded = backgrounded
        self.contentBlocks = contentBlocks
        self.presentation = presentation
    }
}

enum AttachmentKind: String { case image, audio }

struct Attachment: Identifiable, Equatable {
    let id: UUID
    let url: URL
    let kind: AttachmentKind
    let mimeType: String
    let size: Int64

    init(id: UUID = UUID(), url: URL, kind: AttachmentKind, mimeType: String, size: Int64) {
        self.id = id
        self.url = url
        self.kind = kind
        self.mimeType = mimeType
        self.size = size
    }
}

struct ConfigChoice: Identifiable, Equatable {
    var id: String { value }
    let value: String
    let name: String
}

struct ConfigGroup: Identifiable, Equatable {
    let id: String
    let name: String
    let choices: [ConfigChoice]
}

struct ConfigOption: Identifiable, Equatable {
    let id: String
    var name: String
    var category: String?
    var valueType: String = "select"
    var currentValue: String
    var groups: [ConfigGroup]

    var choices: [ConfigChoice] { groups.flatMap(\.choices) }
    var isReasoningEffort: Bool {
        category == "thought_level" || category == "thoughtLevel" || id == "reasoning_effort"
    }
}

struct AdvertisedCommand: Identifiable, Equatable {
    var id: String { name }
    let name: String
    let description: String
}
