// Handwritten Kit-specific presentation and private-extension adapters layered on ACP v2.
import Foundation

enum JSONValue: Codable, Equatable {
    case null
    case bool(Bool)
    case integer(Int64)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    init(from decoder: Decoder) throws {
        let value = try decoder.singleValueContainer()
        if value.decodeNil() { self = .null }
        else if let decoded = try? value.decode(Bool.self) { self = .bool(decoded) }
        else if let decoded = try? value.decode(Int64.self) { self = .integer(decoded) }
        else if let decoded = try? value.decode(Double.self) { self = .number(decoded) }
        else if let decoded = try? value.decode(String.self) { self = .string(decoded) }
        else if let decoded = try? value.decode([JSONValue].self) { self = .array(decoded) }
        else { self = .object(try value.decode([String: JSONValue].self)) }
    }

    func encode(to encoder: Encoder) throws {
        var value = encoder.singleValueContainer()
        switch self {
        case .null: try value.encodeNil()
        case .bool(let decoded): try value.encode(decoded)
        case .integer(let decoded): try value.encode(decoded)
        case .number(let decoded): try value.encode(decoded)
        case .string(let decoded): try value.encode(decoded)
        case .array(let decoded): try value.encode(decoded)
        case .object(let decoded): try value.encode(decoded)
        }
    }

    var anyValue: Any {
        switch self {
        case .null: NSNull()
        case .bool(let value): value
        case .integer(let value): value
        case .number(let value): value
        case .string(let value): value
        case .array(let values): values.map(\.anyValue)
        case .object(let values): values.mapValues(\.anyValue)
        }
    }

    fileprivate var objectValue: [String: JSONValue]? { if case .object(let value) = self { value } else { nil } }
    fileprivate var stringValue: String? { if case .string(let value) = self { value } else { nil } }
    fileprivate var boolValue: Bool? { if case .bool(let value) = self { value } else { nil } }
    fileprivate var intValue: Int? {
        switch self { case .integer(let value): Int(value); case .number(let value): Int(value); default: nil }
    }

    fileprivate func limitingPayloadStrings(maximumBytes: Int, previewBytes: Int) -> JSONValue {
        switch self {
        case .string(let value): .string(Self.limit(value, maximumBytes: maximumBytes, previewBytes: previewBytes))
        case .array(let values): .array(values.map { $0.limitingPayloadStrings(maximumBytes: maximumBytes, previewBytes: previewBytes) })
        case .object(let values): .object(values.mapValues { $0.limitingPayloadStrings(maximumBytes: maximumBytes, previewBytes: previewBytes) })
        default: self
        }
    }

    fileprivate static func limit(_ value: String, maximumBytes: Int, previewBytes: Int) -> String {
        let bytes = Array(value.utf8)
        guard bytes.count > maximumBytes else { return value }
        let preview = String(decoding: bytes.prefix(previewBytes), as: UTF8.self)
        return "\(preview)\n… [truncated \(bytes.count - previewBytes) bytes]"
    }
}

enum DesktopContentBlock: Codable, Equatable {
    case text(String)
    case image(data: String?, mimeType: String?, uri: String?)
    case audio(data: String?, mimeType: String?)
    case resourceLink(uri: String, name: String?, mimeType: String?)
    case resource(uri: String?, mimeType: String?, text: String?, blob: String?)
    case unknown(type: String)

    private enum CodingKeys: String, CodingKey { case type, text, data, mimeType, uri, name, resource, blob }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        let type = try values.decodeIfPresent(String.self, forKey: .type) ?? "unknown"
        switch type {
        case "text": self = .text(try values.decodeIfPresent(String.self, forKey: .text) ?? "")
        case "image": self = .image(data: try values.decodeIfPresent(String.self, forKey: .data), mimeType: try values.decodeIfPresent(String.self, forKey: .mimeType), uri: try values.decodeIfPresent(String.self, forKey: .uri))
        case "audio": self = .audio(data: try values.decodeIfPresent(String.self, forKey: .data), mimeType: try values.decodeIfPresent(String.self, forKey: .mimeType))
        case "resource_link": self = .resourceLink(uri: try values.decode(String.self, forKey: .uri), name: try values.decodeIfPresent(String.self, forKey: .name), mimeType: try values.decodeIfPresent(String.self, forKey: .mimeType))
        case "resource":
            let resource = try values.decodeIfPresent([String: JSONValue].self, forKey: .resource) ?? [:]
            self = .resource(uri: resource["uri"]?.stringValue, mimeType: resource["mimeType"]?.stringValue, text: resource["text"]?.stringValue, blob: resource["blob"]?.stringValue)
        default: self = .unknown(type: type)
        }
    }

    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .text(let text): try values.encode("text", forKey: .type); try values.encode(text, forKey: .text)
        case .image(let data, let mimeType, let uri):
            try values.encode("image", forKey: .type); try values.encodeIfPresent(data, forKey: .data); try values.encodeIfPresent(mimeType, forKey: .mimeType); try values.encodeIfPresent(uri, forKey: .uri)
        case .audio(let data, let mimeType):
            try values.encode("audio", forKey: .type); try values.encodeIfPresent(data, forKey: .data); try values.encodeIfPresent(mimeType, forKey: .mimeType)
        case .resourceLink(let uri, let name, let mimeType):
            try values.encode("resource_link", forKey: .type); try values.encode(uri, forKey: .uri); try values.encodeIfPresent(name, forKey: .name); try values.encodeIfPresent(mimeType, forKey: .mimeType)
        case .resource(let uri, let mimeType, let text, let blob):
            try values.encode("resource", forKey: .type)
            var resource: [String: JSONValue] = [:]
            if let uri { resource["uri"] = .string(uri) }; if let mimeType { resource["mimeType"] = .string(mimeType) }
            if let text { resource["text"] = .string(text) }; if let blob { resource["blob"] = .string(blob) }
            try values.encode(resource, forKey: .resource)
        case .unknown(let type): try values.encode(type, forKey: .type)
        }
    }

    fileprivate func limited(maximumBytes: Int, previewBytes: Int) -> DesktopContentBlock {
        switch self {
        case .text(let text): .text(JSONValue.limit(text, maximumBytes: maximumBytes, previewBytes: previewBytes))
        case .image(let data, let mimeType, let uri): .image(data: data.map { JSONValue.limit($0, maximumBytes: maximumBytes, previewBytes: previewBytes) }, mimeType: mimeType, uri: uri)
        case .audio(let data, let mimeType): .audio(data: data.map { JSONValue.limit($0, maximumBytes: maximumBytes, previewBytes: previewBytes) }, mimeType: mimeType)
        case .resource(let uri, let mimeType, let text, let blob): .resource(uri: uri, mimeType: mimeType, text: text.map { JSONValue.limit($0, maximumBytes: maximumBytes, previewBytes: previewBytes) }, blob: blob.map { JSONValue.limit($0, maximumBytes: maximumBytes, previewBytes: previewBytes) })
        default: self
        }
    }
}

struct DesktopMessageUpdate: Equatable {
    var messageId: String
    var content: [DesktopContentBlock]
    var replace: Bool
    var hasContent: Bool = true
}

struct DesktopToolUpdate: Equatable {
    var toolCallId: String? = nil
    var title: String? = nil
    var status: String? = nil
    var kind: String? = nil
    var content: JSONValue? = nil
    var rawInput: JSONValue? = nil
    var rawOutput: JSONValue? = nil
    var name: String? = nil
    var locations: JSONValue? = nil
    var meta: JSONValue? = nil
    var present: Set<String> = []
    var cleared: Set<String> = []

    func merging(_ patch: DesktopToolUpdate) -> DesktopToolUpdate {
        var result = self
        func supplied(_ field: String) -> Bool { patch.present.contains(field) }
        if supplied("title") { result.title = patch.cleared.contains("title") ? nil : patch.title }
        if supplied("kind") { result.kind = patch.cleared.contains("kind") ? nil : patch.kind }
        if supplied("status") { result.status = patch.cleared.contains("status") ? nil : patch.status }
        if supplied("content") { result.content = patch.cleared.contains("content") ? nil : patch.content }
        if supplied("rawInput") { result.rawInput = patch.cleared.contains("rawInput") ? nil : patch.rawInput }
        if supplied("rawOutput") { result.rawOutput = patch.cleared.contains("rawOutput") ? nil : patch.rawOutput }
        if supplied("name") { result.name = patch.cleared.contains("name") ? nil : patch.name }
        if supplied("locations") { result.locations = patch.cleared.contains("locations") ? nil : patch.locations }
        if supplied("_meta") { result.meta = patch.cleared.contains("_meta") ? nil : patch.meta }
        result.present.formUnion(patch.present)
        result.cleared = result.cleared.subtracting(patch.present).union(patch.cleared)
        return result
    }
}

struct DesktopPlanEntry: Codable, Equatable { var content: String; var status: String? = nil; var priority: String? }
enum DesktopPlanContent: Equatable {
    case items(id: String, entries: [DesktopPlanEntry])
    case file(id: String, uri: String)
    case markdown(id: String, content: String)
    case unknown(id: String, type: String, raw: JSONValue)

    var id: String {
        switch self {
        case .items(let id, _), .file(let id, _), .markdown(let id, _), .unknown(let id, _, _): id
        }
    }
}
struct DesktopUsageUpdate: Codable, Equatable { var used: Int?; var size: Int? }
struct DesktopTokenUsage: Codable, Equatable { var totalTokens: Int?; var inputTokens: Int?; var outputTokens: Int?; var thoughtTokens: Int?; var cachedReadTokens: Int?; var cachedWriteTokens: Int? }
struct DesktopCommand: Codable, Equatable { var name: String; var description: String? = nil }
struct DesktopSessionInfo: Equatable { var title: String?; var updatedAt: String?; var titlePresent: Bool; var updatedAtPresent: Bool }
struct DesktopNotice: Codable, Equatable { var severity: String?; var title: String; var description: String? = nil }
struct DesktopCompaction: Codable, Equatable { var compactionId: String; var status: String; var summary: [DesktopContentBlock]?; var error: String? }
struct DesktopSessionState: Codable, Equatable {
    var state: String
    var stopReason: String?
    var usage: DesktopTokenUsage?
}
struct DesktopTurnState: Codable, Equatable {
    var turnId: Int
    var active: Bool
    var error: String?
    private enum CodingKeys: String, CodingKey { case turnId = "turn_id", active, error }
}

enum DesktopUpdate: Equatable {
    case userMessage(DesktopMessageUpdate), agentMessage(DesktopMessageUpdate), agentThought(DesktopMessageUpdate)
    case toolCall(DesktopToolUpdate), toolCallUpdate(DesktopToolUpdate), toolCallContent(id: String, content: JSONValue)
    case plan(DesktopPlanContent), planRemoved(id: String), usage(DesktopUsageUpdate), tokenUsage(DesktopTokenUsage)
    case configOptions(JSONValue), sessionInfo(DesktopSessionInfo), availableCommands([DesktopCommand])
    case currentMode(JSONValue), notice(DesktopNotice), compaction(DesktopCompaction), compactionChunk(id: String, content: DesktopContentBlock)
    case state(DesktopSessionState), turnState(DesktopTurnState), unknown(kind: String)

    init(wire raw: JSONValue) throws {
        guard let object = raw.objectValue else { throw ACPClientError.protocolError("Desktop update is not an object") }
        let kind = object["sessionUpdate"]?.stringValue ?? "unknown"
        func decode<T: Decodable>(_ type: T.Type, _ value: JSONValue = raw) throws -> T {
            try JSONDecoder().decode(type, from: JSONEncoder().encode(value))
        }
        func message(replace: Bool) throws -> DesktopMessageUpdate {
            guard let id = object["messageId"]?.stringValue else { throw ACPClientError.protocolError("ACP message update omitted messageId") }
            if replace {
                guard let value = object["content"] else { return DesktopMessageUpdate(messageId: id, content: [], replace: true, hasContent: false) }
                if case .null = value { return DesktopMessageUpdate(messageId: id, content: [], replace: true) }
                return DesktopMessageUpdate(messageId: id, content: try decode([DesktopContentBlock].self, value), replace: true)
            }
            return DesktopMessageUpdate(messageId: id, content: [try decode(DesktopContentBlock.self, object["content"] ?? .null)], replace: false)
        }
        func tool() throws -> DesktopToolUpdate {
            let fields = ["title", "kind", "status", "content", "rawInput", "rawOutput", "name", "locations", "_meta"]
            var present = Set<String>(), cleared = Set<String>()
            for field in fields where object[field] != nil {
                present.insert(field)
                if case .null? = object[field] { cleared.insert(field) }
            }
            return DesktopToolUpdate(
                toolCallId: object["toolCallId"]?.stringValue, title: object["title"]?.stringValue,
                status: object["status"]?.stringValue, kind: object["kind"]?.stringValue,
                content: object["content"], rawInput: object["rawInput"], rawOutput: object["rawOutput"],
                name: object["name"]?.stringValue, locations: object["locations"], meta: object["_meta"],
                present: present, cleared: cleared
            )
        }
        switch kind {
        case "user_message_chunk": self = .userMessage(try message(replace: false))
        case "user_message": self = .userMessage(try message(replace: true))
        case "agent_message_chunk": self = .agentMessage(try message(replace: false))
        case "agent_message": self = .agentMessage(try message(replace: true))
        case "agent_thought_chunk": self = .agentThought(try message(replace: false))
        case "agent_thought": self = .agentThought(try message(replace: true))
        case "tool_call": self = .toolCall(try tool()) // Legacy Kit fixture compatibility.
        case "tool_call_update": self = .toolCallUpdate(try tool())
        case "tool_call_content_chunk":
            guard let id = object["toolCallId"]?.stringValue, let content = object["content"] else { throw ACPClientError.protocolError("Malformed tool content chunk") }
            self = .toolCallContent(id: id, content: content)
        case "plan":
            let entries = object["entries"].flatMap { try? decode([DesktopPlanEntry].self, $0) } ?? []
            self = .plan(.items(id: "default", entries: entries))
        case "plan_update":
            guard let rawPlan = object["plan"], let plan = rawPlan.objectValue,
                  let id = plan["planId"]?.stringValue else {
                throw ACPClientError.protocolError("Malformed plan update")
            }
            switch plan["type"]?.stringValue {
            case "items":
                guard let value = plan["entries"] else { throw ACPClientError.protocolError("Item plan omitted entries") }
                self = .plan(.items(id: id, entries: try decode([DesktopPlanEntry].self, value)))
            case "file":
                guard let uri = plan["uri"]?.stringValue else { throw ACPClientError.protocolError("File plan omitted uri") }
                self = .plan(.file(id: id, uri: uri))
            case "markdown":
                guard let content = plan["content"]?.stringValue else { throw ACPClientError.protocolError("Markdown plan omitted content") }
                self = .plan(.markdown(id: id, content: content))
            case let type?: self = .plan(.unknown(id: id, type: type, raw: rawPlan))
            case nil: throw ACPClientError.protocolError("Plan update omitted type")
            }
        case "plan_removed":
            guard let id = object["planId"]?.stringValue else { throw ACPClientError.protocolError("Plan removal omitted planId") }
            self = .planRemoved(id: id)
        case "usage_update": self = .usage(try decode(DesktopUsageUpdate.self))
        case "config_option_update": self = .configOptions(object["configOptions"] ?? .array([]))
        case "session_info_update": self = .sessionInfo(DesktopSessionInfo(
            title: object["title"]?.stringValue, updatedAt: object["updatedAt"]?.stringValue,
            titlePresent: object["title"] != nil, updatedAtPresent: object["updatedAt"] != nil
        ))
        case "available_commands_update": self = .availableCommands(try decode([DesktopCommand].self, object["availableCommands"] ?? .array([])))
        case "state_update":
            let state = try decode(DesktopSessionState.self)
            self = .state(state)
        case "notice": self = .notice(try decode(DesktopNotice.self))
        case "compaction_update": self = .compaction(try decode(DesktopCompaction.self))
        case "compaction_summary_chunk":
            guard let id = object["compactionId"]?.stringValue, let content = object["content"] else { throw ACPClientError.protocolError("Malformed compaction chunk") }
            self = .compactionChunk(id: id, content: try decode(DesktopContentBlock.self, content))
        case "current_mode_update": self = .currentMode(raw)
        case "status": self = .turnState(try decode(DesktopTurnState.self))
        default: self = .unknown(kind: kind)
        }
    }

    func limited(maximumBytes: Int = 256 * 1024, previewBytes: Int = 16 * 1024) -> DesktopUpdate {
        switch self {
        case .userMessage(var value): value.content = value.content.map { $0.limited(maximumBytes: maximumBytes, previewBytes: previewBytes) }; return .userMessage(value)
        case .agentMessage(var value): value.content = value.content.map { $0.limited(maximumBytes: maximumBytes, previewBytes: previewBytes) }; return .agentMessage(value)
        case .agentThought(var value): value.content = value.content.map { $0.limited(maximumBytes: maximumBytes, previewBytes: previewBytes) }; return .agentThought(value)
        case .toolCall(var value): value.rawInput = value.rawInput?.limitingPayloadStrings(maximumBytes: maximumBytes, previewBytes: previewBytes); value.rawOutput = value.rawOutput?.limitingPayloadStrings(maximumBytes: maximumBytes, previewBytes: previewBytes); return .toolCall(value)
        case .toolCallUpdate(var value): value.rawInput = value.rawInput?.limitingPayloadStrings(maximumBytes: maximumBytes, previewBytes: previewBytes); value.rawOutput = value.rawOutput?.limitingPayloadStrings(maximumBytes: maximumBytes, previewBytes: previewBytes); return .toolCallUpdate(value)
        default: return self
        }
    }

    var coalescingKey: String? {
        let role: String, value: DesktopMessageUpdate
        switch self { case .userMessage(let item): role = "user"; value = item; case .agentMessage(let item): role = "agent"; value = item; case .agentThought(let item): role = "thought"; value = item; default: return nil }
        guard !value.replace, value.content.count == 1, case .text = value.content[0] else { return nil }
        return role + "|" + value.messageId
    }

    var textChunk: String? {
        let value: DesktopMessageUpdate
        switch self { case .userMessage(let item), .agentMessage(let item), .agentThought(let item): value = item; default: return nil }
        guard !value.replace, value.content.count == 1, case .text(let text) = value.content[0] else { return nil }; return text
    }

    func replacingText(_ text: String) -> DesktopUpdate {
        switch self {
        case .userMessage(var value): value.content = [.text(text)]; return .userMessage(value)
        case .agentMessage(var value): value.content = [.text(text)]; return .agentMessage(value)
        case .agentThought(var value): value.content = [.text(text)]; return .agentThought(value)
        default: return self
        }
    }
}
