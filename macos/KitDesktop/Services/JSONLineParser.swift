import Foundation

struct JSONLineParser {
    private var buffer = Data()
    private let maximumLineBytes: Int

    init(maximumLineBytes: Int = 16 * 1024 * 1024) { self.maximumLineBytes = maximumLineBytes }

    mutating func append(_ data: Data) throws -> [Data] {
        guard !data.isEmpty else { return [] }
        buffer.append(data)
        var lines: [Data] = []
        var start = buffer.startIndex
        while start < buffer.endIndex, let newline = buffer[start...].firstIndex(of: 0x0A) {
            let count = buffer.distance(from: start, to: newline)
            guard count <= maximumLineBytes else { throw ACPClientError.protocolError("ACP line exceeds \(maximumLineBytes) bytes") }
            var line = Data(buffer[start..<newline])
            if line.last == 0x0D { line.removeLast() }
            if !line.isEmpty { lines.append(line) }
            start = buffer.index(after: newline)
        }
        if start > buffer.startIndex { buffer.removeSubrange(buffer.startIndex..<start) }
        guard buffer.count <= maximumLineBytes else { throw ACPClientError.protocolError("Incomplete ACP line exceeds \(maximumLineBytes) bytes") }
        return lines
    }

    mutating func finish() throws -> Data? {
        defer { buffer.removeAll(keepingCapacity: false) }
        guard !buffer.isEmpty else { return nil }
        guard buffer.count <= maximumLineBytes else { throw ACPClientError.protocolError("ACP line exceeds \(maximumLineBytes) bytes") }
        var line = buffer
        if line.last == 0x0D { line.removeLast() }
        return line.isEmpty ? nil : line
    }
}

enum RPCID: Equatable {
    case integer(Int)
    case string(String)

    var jsonValue: Any {
        switch self { case .integer(let value): return value; case .string(let value): return value }
    }
}

struct RPCEnvelope {
    let id: RPCID?
    let method: String?
    let params: [String: Any]?
    let result: [String: Any]?
    let error: RPCErrorPayload?

    static func parse(_ data: Data) throws -> RPCEnvelope {
        let messages = try parseMany(data)
        guard messages.count == 1, let message = messages.first else { throw ACPClientError.protocolError("Expected one JSON-RPC envelope") }
        return message
    }

    static func parseMany(_ data: Data) throws -> [RPCEnvelope] {
        let value = try JSONSerialization.jsonObject(with: data)
        if let object = value as? [String: Any] { return [try parse(object)] }
        if let batch = value as? [[String: Any]], !batch.isEmpty { return try batch.map(parse) }
        throw ACPClientError.protocolError("Invalid JSON-RPC envelope")
    }

    private static func parse(_ object: [String: Any]) throws -> RPCEnvelope {
        guard object["jsonrpc"] as? String == "2.0" else { throw ACPClientError.protocolError("Invalid JSON-RPC version") }
        let id: RPCID?
        if let value = object["id"] as? NSNumber { id = .integer(value.intValue) }
        else if let value = object["id"] as? String { id = .string(value) }
        else { id = nil }
        let error: RPCErrorPayload?
        if let payload = object["error"] as? [String: Any] {
            error = RPCErrorPayload(code: (payload["code"] as? NSNumber)?.intValue ?? -1, message: payload["message"] as? String ?? "Unknown JSON-RPC error")
        } else { error = nil }
        return RPCEnvelope(
            id: id, method: object["method"] as? String, params: object["params"] as? [String: Any],
            result: object["result"] as? [String: Any], error: error
        )
    }
}

struct RPCErrorPayload: Error { let code: Int; let message: String }

enum ACPClientError: LocalizedError {
    case missingBinary
    case process(String)
    case protocolError(String)
    case remote(Int, String)
    case timeout(String)
    case attachment(String)

    var errorDescription: String? {
        switch self {
        case .missingBinary: return "Kit CLI not found. Bundle Helpers/kit or set KIT_BINARY for Debug development."
        case .process(let message), .protocolError(let message), .attachment(let message): return message
        case .remote(let code, let message): return "ACP error \(code): \(message)"
        case .timeout(let method): return "ACP request \(method) timed out"
        }
    }
}
