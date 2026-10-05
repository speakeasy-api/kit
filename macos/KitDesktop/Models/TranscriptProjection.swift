import Foundation
import Combine

/// A stable display identity: messages stand alone; adjacent internal events share one activity.
/// Hidden activity entries are retained here, not instantiated as SwiftUI views.
@MainActor
final class TranscriptItem: ObservableObject, Identifiable {
    let id: UUID
    let isActivity: Bool
    @Published private(set) var entries: [TranscriptEntry]
    private(set) var runningCount = 0
    private(set) var errorCount = 0
    private(set) var summary = "Activity"
    private var indices: [UUID: Int] = [:]

    var isExpandableActivity: Bool {
        isActivity && !(entries.count == 1 && entries[0].role == .duration)
    }

    init(_ entry: TranscriptEntry) {
        id = entry.id
        isActivity = entry.role != .user && entry.role != .assistant
        entries = []
        append(entry)
    }

    func append(_ entry: TranscriptEntry) {
        indices[entry.id] = entries.count
        adjust(entry, by: 1)
        summary = Self.summary(entry)
        entries.append(entry)
    }

    func update(_ entry: TranscriptEntry) {
        guard let index = indices[entry.id] else { return }
        adjust(entries[index], by: -1)
        adjust(entry, by: 1)
        if entry.isStreaming || index == entries.count - 1 { summary = Self.summary(entry) }
        entries[index] = entry
    }

    private func adjust(_ entry: TranscriptEntry, by delta: Int) {
        if entry.isStreaming { runningCount += delta }
        if entry.role == .error || entry.presentation?.tool?.status == .failed
            || entry.children.contains(where: { $0.succeeded == false }) {
            errorCount += delta
        }
    }

    private static func summary(_ entry: TranscriptEntry) -> String {
        switch entry.role {
        case .thought: return entry.isStreaming ? "Thinking…" : "Thought"
        case .tool: return entry.presentation?.tool?.title ?? entry.title ?? "Tool"
        case .plan: return "Updating plan"
        case .duration: return "Activity completed"
        case .error: return "Activity encountered an error"
        default: return entry.title ?? "Activity"
        }
    }
}

/// Appends visit only the new suffix; edits visit only explicitly invalidated entries.
/// Deletion/retention/replay rebuilds grouping, preserving surviving row IDs (not row object instances).
/// This iterator boundary avoids scanning a completed thread on each streaming token.
@MainActor
final class TranscriptProjection: ObservableObject {
    @Published private(set) var items: [TranscriptItem] = []
    private var itemByEntry: [UUID: TranscriptItem] = [:]
    private var projectedCount = 0

    func synchronize(_ entries: [TranscriptEntry], changedIndices: Set<Int> = [], rebuilding: Bool = false) {
        if rebuilding || entries.count < projectedCount {
            items = []
            itemByEntry = [:]
            projectedCount = 0
        }
        let previousCount = projectedCount
        for entry in entries.dropFirst(previousCount) {
            let activity = entry.role != .user && entry.role != .assistant
            let item: TranscriptItem
            if activity, let last = items.last, last.isActivity {
                item = last
                item.append(entry)
            } else {
                item = TranscriptItem(entry)
                items.append(item)
            }
            itemByEntry[entry.id] = item
        }
        projectedCount = entries.count
        for index in changedIndices where index < previousCount && entries.indices.contains(index) {
            let entry = entries[index]
            itemByEntry[entry.id]?.update(entry)
        }
    }
}
