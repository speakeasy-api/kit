import Foundation

/// Bounds a single streaming tool's retained JSON, not just its visible text.
enum TranscriptToolContentBuffer {
    static let maximumBytes = 256 * 1024
    static let maximumChunks = 64
    private static let marker: JSONValue = .object([
        "type": .string("text"),
        "text": .string("[Earlier tool output omitted by desktop retention limit]"),
        "_kitDesktopRetention": .bool(true),
    ])

    static func append(_ incoming: JSONValue, to retained: JSONValue?) -> JSONValue {
        var chunks: [JSONValue]
        if case .array(let existing)? = retained { chunks = existing }
        else { chunks = retained.map { [$0] } ?? [] }
        var omitted = chunks.first?.objectValue?["_kitDesktopRetention"] == .bool(true)
        if omitted { chunks.removeFirst() }
        let encoded = (try? JSONEncoder().encode(incoming)) ?? Data()
        let chunk: JSONValue = encoded.count > maximumBytes / 2
            ? .object(["truncated": .bool(true), "bytes": .integer(Int64(encoded.count)),
                       "preview": .string(String(decoding: encoded.prefix(16 * 1024), as: UTF8.self))])
            : incoming
        chunks.append(chunk)
        while chunks.count + (omitted ? 1 : 0) > maximumChunks {
            chunks.removeFirst()
            omitted = true
        }
        while !chunks.isEmpty {
            let value = JSONValue.array((omitted ? [marker] : []) + chunks)
            if let bytes = try? JSONEncoder().encode(value), bytes.count <= maximumBytes { return value }
            chunks.removeFirst()
            omitted = true
        }
        return .array([marker])
    }
}
