import Foundation

/// Session-local retention, not a wire/persistence schema. Bound both a single
/// long message and mixed media, and coalesce tiny text deltas into small chunks.
enum TranscriptContentBuffer {
    static let maximumTextBytes = 256 * 1024
    static let maximumBytes = 8 * 1024 * 1024
    static let maximumBlocks = 256
    private static let textChunkBytes = 16 * 1024

    static func text(_ source: String) -> String {
        guard source.utf8.count > maximumTextBytes else { return source }
        var bytes = source.utf8.suffix(maximumTextBytes)
        while let first = bytes.first, first & 0xC0 == 0x80 { bytes = bytes.dropFirst() }
        return String(decoding: bytes, as: UTF8.self)
    }

    static func append(_ incoming: [DesktopContentBlock], to retained: [DesktopContentBlock] = []) -> [DesktopContentBlock] {
        var result = retained
        for raw in incoming {
            let block: DesktopContentBlock
            if case .text(let source) = raw { block = .text(text(source)) }
            else if byteCount(raw) > maximumBytes {
                block = .text("[Content exceeds desktop retention limit]")
            } else { block = raw }
            if case .text(let next) = block, case .text(let previous)? = result.last,
               previous.utf8.count + next.utf8.count <= textChunkBytes {
                result[result.count - 1] = .text(previous + next)
            } else { result.append(block) }
            // At most maximumBlocks + 1 entries are visited, independent of the
            // number of deltas received. This is a real retention/iterator bound.
            var bytes = result.reduce(0) { $0 + byteCount($1) }
            var textBytes = result.reduce(0) { count, block in
                if case .text(let value) = block { return count + value.utf8.count }
                return count
            }
            var removed = 0
            while result.count - removed > maximumBlocks || bytes > maximumBytes || textBytes > maximumTextBytes {
                let first = result[removed]
                bytes -= byteCount(first)
                if case .text(let value) = first { textBytes -= value.utf8.count }
                removed += 1
            }
            if removed > 0 { result.removeFirst(removed) }
        }
        return result
    }

    static func byteCount(_ block: DesktopContentBlock) -> Int {
        func bytes(_ strings: String?...) -> Int { strings.reduce(0) { $0 + ($1?.utf8.count ?? 0) } }
        switch block {
        case .text(let value): return value.utf8.count
        case .image(let data, let mime, let uri): return bytes(data, mime, uri)
        case .audio(let data, let mime): return bytes(data, mime)
        case .resourceLink(let uri, let name, let mime): return bytes(uri, name, mime)
        case .resource(let uri, let mime, let text, let blob): return bytes(uri, mime, text, blob)
        case .unknown(let type): return type.utf8.count
        }
    }
}
