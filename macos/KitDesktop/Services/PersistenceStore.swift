import Foundation

enum PersistenceError: LocalizedError, Equatable {
    case unsupportedSchema(Int)
    case unreadableState(String)

    var errorDescription: String? {
        switch self {
        case .unsupportedSchema(let version): return "State schema \(version) is newer than this app supports."
        case .unreadableState(let message): return "Could not recover saved state: \(message)"
        }
    }
}

final class PersistenceStore {
    let fileURL: URL
    let backupURL: URL
    private let queue = DispatchQueue(label: "dev.kit.desktop.persistence", qos: .utility)

    init(fileURL: URL? = nil) {
        if let fileURL { self.fileURL = fileURL }
        else {
            let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            self.fileURL = base.appendingPathComponent("KitDesktop/state.json")
        }
        backupURL = self.fileURL.appendingPathExtension("backup")
    }

    func load() throws -> PersistedAppState {
        guard FileManager.default.fileExists(atPath: fileURL.path) else {
            guard FileManager.default.fileExists(atPath: backupURL.path) else { return PersistedAppState() }
            return try decode(Data(contentsOf: backupURL))
        }
        do { return try decode(Data(contentsOf: fileURL)) }
        catch let error as PersistenceError {
            if case .unsupportedSchema = error { throw error }
            return try recover(after: error)
        } catch {
            return try recover(after: error)
        }
    }

    func save(_ state: PersistedAppState) throws {
        let directory = fileURL.deletingLastPathComponent()
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let data = try Self.encoder.encode(state)
        _ = try decode(data)
        if FileManager.default.fileExists(atPath: fileURL.path) {
            let old = try Data(contentsOf: fileURL)
            do {
                _ = try decode(old)
                try? FileManager.default.removeItem(at: backupURL)
                try FileManager.default.copyItem(at: fileURL, to: backupURL)
            } catch let error as PersistenceError {
                if case .unsupportedSchema = error { throw error }
            } catch {}
        }
        try data.write(to: fileURL, options: [.atomic, .completeFileProtectionUnlessOpen])
    }

    func saveAsync(_ state: PersistedAppState, completion: @escaping (Error?) -> Void) {
        queue.async {
            let error: Error?
            do { try self.save(state); error = nil } catch let caught { error = caught }
            DispatchQueue.main.async { completion(error) }
        }
    }

    func flush() { queue.sync {} }

    private func recover(after original: Error) throws -> PersistedAppState {
        let quarantine = fileURL.deletingLastPathComponent().appendingPathComponent(
            "state.corrupt-\(Int(Date().timeIntervalSince1970 * 1000)).json"
        )
        try? FileManager.default.copyItem(at: fileURL, to: quarantine)
        if FileManager.default.fileExists(atPath: backupURL.path) {
            do { return try decode(Data(contentsOf: backupURL)) }
            catch let error as PersistenceError {
                if case .unsupportedSchema = error { throw error }
                throw PersistenceError.unreadableState("\(original.localizedDescription); backup: \(error.localizedDescription)")
            } catch {
                throw PersistenceError.unreadableState("\(original.localizedDescription); backup: \(error.localizedDescription)")
            }
        }
        throw PersistenceError.unreadableState(original.localizedDescription)
    }

    private func decode(_ data: Data) throws -> PersistedAppState {
        let value: Any
        do { value = try JSONSerialization.jsonObject(with: data) }
        catch { throw PersistenceError.unreadableState(error.localizedDescription) }
        guard var object = value as? [String: Any] else {
            throw PersistenceError.unreadableState("State root must be an object.")
        }

        let version: Int
        if let rawVersion = object["schemaVersion"] {
            guard !(rawVersion is Bool), let decoded = rawVersion as? Int, decoded >= 1 else {
                throw PersistenceError.unreadableState("schemaVersion must be a positive integer.")
            }
            version = decoded
        } else {
            version = 1
        }
        guard version <= PersistedAppState.currentSchemaVersion else {
            throw PersistenceError.unsupportedSchema(version)
        }

        try migrate(&object, from: version)
        try validateCurrentShape(object)
        let migrated = try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
        do { return try Self.decoder.decode(PersistedAppState.self, from: migrated) }
        catch { throw PersistenceError.unreadableState(error.localizedDescription) }
    }

    private func migrate(_ object: inout [String: Any], from version: Int) throws {
        if version < 3 {
            if object["workspaces"] == nil { object["workspaces"] = [] }
            if object["conversations"] == nil { object["conversations"] = [] }
        }
        guard var conversations = object["conversations"] as? [[String: Any]] else {
            throw PersistenceError.unreadableState("conversations must be an array of objects.")
        }

        if version < 3 {
            for index in conversations.indices {
                if conversations[index]["unread"] == nil { conversations[index]["unread"] = false }
                if conversations[index]["awaitingUser"] == nil { conversations[index]["awaitingUser"] = false }
            }
        }
        if version < 2 {
            for index in conversations.indices {
                if conversations[index]["provider"] == nil { conversations[index]["provider"] = "openai-subscription" }
                if conversations[index]["model"] == nil { conversations[index]["model"] = "gpt-5.4" }
                if conversations[index]["reasoningEffort"] == nil { conversations[index]["reasoningEffort"] = "default" }
            }
        }
        if version < 3 {
            for index in conversations.indices where conversations[index]["usesConfiguredDefaults"] == nil {
                conversations[index]["usesConfiguredDefaults"] = true
            }
        }
        object["conversations"] = conversations
        object["schemaVersion"] = PersistedAppState.currentSchemaVersion
    }

    private func validateCurrentShape(_ object: [String: Any]) throws {
        guard object["schemaVersion"] as? Int == PersistedAppState.currentSchemaVersion,
              object["workspaces"] is [[String: Any]],
              let conversations = object["conversations"] as? [[String: Any]] else {
            throw PersistenceError.unreadableState("State does not match the current schema.")
        }
        let requiredConversationKeys: Set<String> = [
            "id", "workspaceID", "title", "createdAt", "updatedAt", "unread",
            "awaitingUser", "provider", "model", "reasoningEffort", "usesConfiguredDefaults",
        ]
        for (index, conversation) in conversations.enumerated() {
            let hasRequiredKeys = requiredConversationKeys.isSubset(of: Set(conversation.keys))
            let hasRequiredTypes = conversation["id"] is String
                && conversation["workspaceID"] is String
                && conversation["title"] is String
                && conversation["createdAt"] is String
                && conversation["updatedAt"] is String
                && conversation["unread"] is Bool
                && conversation["awaitingUser"] is Bool
                && conversation["provider"] is String
                && conversation["model"] is String
                && conversation["reasoningEffort"] is String
                && conversation["usesConfiguredDefaults"] is Bool
            if !hasRequiredKeys || !hasRequiredTypes {
                throw PersistenceError.unreadableState("Conversation \(index) does not match the current schema.")
            }
        }
    }

    private static let encoder: JSONEncoder = {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        encoder.outputFormatting = [.sortedKeys]
        return encoder
    }()

    private static let decoder: JSONDecoder = {
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        return decoder
    }()
}
