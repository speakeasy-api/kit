import AppKit
import SwiftUI

enum MarkdownTableAlignment: Equatable { case leading, center, trailing }

struct MarkdownDocument: Equatable {
    enum Block: Equatable {
        case heading(level: Int, text: String)
        case paragraph(String)
        case unordered([String])
        case ordered([String])
        case quote(String)
        case code(language: String?, text: String)
        case table(headers: [String], alignments: [MarkdownTableAlignment], rows: [[String]])
        case rule
    }

    let blocks: [Block]

    init(_ source: String) {
        let lines = source.replacingOccurrences(of: "\r\n", with: "\n").components(separatedBy: "\n")
        var result: [Block] = []
        var index = 0
        while index < lines.count {
            let line = lines[index]
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if trimmed.isEmpty { index += 1; continue }

            if trimmed.hasPrefix("```") {
                let language = String(trimmed.dropFirst(3)).trimmingCharacters(in: .whitespaces)
                index += 1
                var code: [String] = []
                while index < lines.count, !lines[index].trimmingCharacters(in: .whitespaces).hasPrefix("```") {
                    code.append(lines[index]); index += 1
                }
                if index < lines.count { index += 1 }
                result.append(.code(language: language.isEmpty ? nil : language, text: code.joined(separator: "\n")))
                continue
            }

            if let heading = Self.heading(line) { result.append(heading); index += 1; continue }
            if Self.isRule(trimmed) { result.append(.rule); index += 1; continue }
            if index + 1 < lines.count,
               let headers = Self.tableRow(line),
               let alignments = Self.tableAlignments(lines[index + 1], columns: headers.count) {
                index += 2
                var rows: [[String]] = []
                while index < lines.count, let row = Self.tableRow(lines[index]), !lines[index].trimmingCharacters(in: .whitespaces).isEmpty {
                    rows.append(Array((row + Array(repeating: "", count: headers.count)).prefix(headers.count)))
                    index += 1
                }
                result.append(.table(headers: headers, alignments: alignments, rows: rows))
                continue
            }

            if Self.unorderedItem(line) != nil {
                var items: [String] = []
                while index < lines.count, let item = Self.unorderedItem(lines[index]) { items.append(item); index += 1 }
                result.append(.unordered(items)); continue
            }
            if Self.orderedItem(line) != nil {
                var items: [String] = []
                while index < lines.count, let item = Self.orderedItem(lines[index]) { items.append(item); index += 1 }
                result.append(.ordered(items)); continue
            }
            if trimmed.hasPrefix(">") {
                var quote: [String] = []
                while index < lines.count {
                    let candidate = lines[index].trimmingCharacters(in: .whitespaces)
                    guard candidate.hasPrefix(">") else { break }
                    quote.append(String(candidate.dropFirst()).trimmingCharacters(in: .whitespaces))
                    index += 1
                }
                result.append(.quote(quote.joined(separator: "\n"))); continue
            }

            var paragraph: [String] = [line]
            index += 1
            while index < lines.count {
                let candidate = lines[index]
                let candidateTrimmed = candidate.trimmingCharacters(in: .whitespaces)
                let startsTable = index + 1 < lines.count && Self.tableRow(candidate) != nil && Self.tableAlignments(lines[index + 1], columns: Self.tableRow(candidate)?.count ?? 0) != nil
                if candidateTrimmed.isEmpty || candidateTrimmed.hasPrefix("```") || Self.heading(candidate) != nil || Self.isRule(candidateTrimmed) || Self.unorderedItem(candidate) != nil || Self.orderedItem(candidate) != nil || candidateTrimmed.hasPrefix(">") || startsTable { break }
                paragraph.append(candidate); index += 1
            }
            result.append(.paragraph(paragraph.joined(separator: "\n")))
        }
        blocks = result
    }

    private static func heading(_ line: String) -> Block? {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        let count = trimmed.prefix(while: { $0 == "#" }).count
        guard (1...6).contains(count), trimmed.dropFirst(count).first == " " else { return nil }
        return .heading(level: count, text: String(trimmed.dropFirst(count + 1)))
    }

    private static func isRule(_ line: String) -> Bool {
        let compact = line.replacingOccurrences(of: " ", with: "")
        guard compact.count >= 3, let first = compact.first, ["-", "*", "_"].contains(first) else { return false }
        return compact.allSatisfy { $0 == first }
    }

    private static func unorderedItem(_ line: String) -> String? {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        for marker in ["- ", "* ", "+ "] where trimmed.hasPrefix(marker) { return String(trimmed.dropFirst(2)) }
        return nil
    }

    private static func orderedItem(_ line: String) -> String? {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        guard let dot = trimmed.firstIndex(of: "."), dot != trimmed.startIndex else { return nil }
        let number = trimmed[..<dot]
        let after = trimmed.index(after: dot)
        guard number.allSatisfy(\.isNumber), after < trimmed.endIndex, trimmed[after] == " " else { return nil }
        return String(trimmed[trimmed.index(after: after)...])
    }

    private static func tableRow(_ line: String) -> [String]? {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        guard trimmed.contains("|") else { return nil }
        var value = trimmed
        if value.hasPrefix("|") { value.removeFirst() }
        if value.hasSuffix("|") { value.removeLast() }
        var cells: [String] = []
        var cell = ""
        let characters = Array(value)
        var index = 0
        while index < characters.count {
            let character = characters[index]
            if character == "\\", index + 1 < characters.count, characters[index + 1] == "|" {
                cell.append("|")
                index += 2
            } else if character == "|" {
                cells.append(cell.trimmingCharacters(in: .whitespaces)); cell = ""
                index += 1
            } else {
                cell.append(character)
                index += 1
            }
        }
        cells.append(cell.trimmingCharacters(in: .whitespaces))
        return cells.count > 1 ? cells : nil
    }

    private static func tableAlignments(_ line: String, columns: Int) -> [MarkdownTableAlignment]? {
        guard columns > 0, let cells = tableRow(line), cells.count == columns else { return nil }
        var result: [MarkdownTableAlignment] = []
        for cell in cells {
            let marker = cell.trimmingCharacters(in: CharacterSet(charactersIn: ":"))
            guard marker.count >= 3, marker.allSatisfy({ $0 == "-" }) else { return nil }
            if cell.hasPrefix(":"), cell.hasSuffix(":") { result.append(.center) }
            else if cell.hasSuffix(":") { result.append(.trailing) }
            else { result.append(.leading) }
        }
        return result
    }
}

struct MarkdownView: View {
    let source: String
    private var document: MarkdownDocument { MarkdownDocument(source) }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            ForEach(Array(document.blocks.enumerated()), id: \.offset) { _, block in
                blockView(block)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    @ViewBuilder
    private func blockView(_ block: MarkdownDocument.Block) -> some View {
        switch block {
        case .heading(let level, let text):
            if level <= 2 {
                InlineMarkdown(text).brandDisplay(level == 1 ? 25 : 21).padding(.top, 5)
            } else {
                InlineMarkdown(text).font(headingFont(level)).fontWeight(.semibold).padding(.top, 2)
            }
        case .paragraph(let text):
            InlineMarkdown(text).font(.body).lineSpacing(3)
        case .unordered(let items):
            VStack(alignment: .leading, spacing: 7) {
                ForEach(Array(items.enumerated()), id: \.offset) { _, item in
                    HStack(alignment: .firstTextBaseline, spacing: 9) {
                        Text("•").foregroundStyle(.secondary); InlineMarkdown(item).frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
            }.padding(.leading, 5)
        case .ordered(let items):
            VStack(alignment: .leading, spacing: 7) {
                ForEach(Array(items.enumerated()), id: \.offset) { index, item in
                    HStack(alignment: .firstTextBaseline, spacing: 9) {
                        Text("\(index + 1).").monospacedDigit().foregroundStyle(.secondary).frame(minWidth: 18, alignment: .trailing)
                        InlineMarkdown(item).frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
            }
        case .quote(let text):
            HStack(alignment: .top, spacing: 11) {
                Capsule().fill(Brand.primary.opacity(0.35)).frame(width: 2)
                InlineMarkdown(text).foregroundStyle(.secondary)
            }.padding(.vertical, 2)
        case .code(let language, let text):
            CodeBlockView(language: language, code: text)
        case .table(let headers, let alignments, let rows):
            MarkdownTableView(headers: headers, alignments: alignments, rows: rows)
        case .rule:
            Divider().padding(.vertical, 4)
        }
    }

    private func headingFont(_ level: Int) -> Font {
        level == 3 ? .headline : .body
    }
}

private struct MarkdownTableView: View {
    let headers: [String]
    let alignments: [MarkdownTableAlignment]
    let rows: [[String]]

    var body: some View {
        ScrollView(.horizontal) {
            Grid(alignment: .leading, horizontalSpacing: 0, verticalSpacing: 0) {
                GridRow {
                    ForEach(Array(headers.enumerated()), id: \.offset) { index, header in
                        InlineMarkdown(header).font(.callout.weight(.semibold))
                            .padding(.horizontal, 10).padding(.vertical, 7)
                            .frame(maxWidth: .infinity, alignment: alignment(at: index))
                    }
                }
                Divider().gridCellUnsizedAxes(.horizontal)
                ForEach(Array(rows.enumerated()), id: \.offset) { index, row in
                    GridRow {
                        ForEach(Array(row.enumerated()), id: \.offset) { column, cell in
                            InlineMarkdown(cell).font(.callout)
                                .padding(.horizontal, 10).padding(.vertical, 7)
                                .frame(maxWidth: .infinity, alignment: alignment(at: column))
                        }
                    }
                    .background(index.isMultiple(of: 2) ? Color.clear : Color.primary.opacity(0.035))
                }
            }
            .overlay { RoundedRectangle(cornerRadius: Brand.Radius.small).stroke(Brand.hairline) }
            .clipShape(RoundedRectangle(cornerRadius: Brand.Radius.small))
        }
    }

    private func alignment(at index: Int) -> Alignment {
        guard alignments.indices.contains(index) else { return .leading }
        return switch alignments[index] {
        case .leading: .leading
        case .center: .center
        case .trailing: .trailing
        }
    }
}

private struct InlineMarkdown: View {
    let value: AttributedString

    init(_ source: String) {
        value = (try? AttributedString(markdown: source, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace))) ?? AttributedString(source)
    }

    var body: some View { Text(value).textSelection(.enabled) }
}

private struct CodeBlockView: View {
    let language: String?
    let code: String
    @State private var copied = false

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text(language ?? "Code").brandMicroLabel().foregroundStyle(.secondary)
                Spacer()
                Button {
                    NSPasteboard.general.clearContents(); NSPasteboard.general.setString(code, forType: .string)
                    copied = true
                    DispatchQueue.main.asyncAfter(deadline: .now() + 1.2) { copied = false }
                } label: { Label(copied ? "Copied" : "Copy", systemImage: copied ? "checkmark" : "doc.on.doc") }
                .buttonStyle(.plain).font(.caption).foregroundStyle(.secondary).pointingHandCursor()
            }.padding(.horizontal, 12).padding(.vertical, 8)
            Divider()
            ScrollView(.horizontal) {
                Text(code).font(.system(.callout, design: .monospaced)).textSelection(.enabled)
                    .padding(12).frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .background(Color(nsColor: .textBackgroundColor).opacity(0.55), in: RoundedRectangle(cornerRadius: Brand.Radius.medium))
        .overlay { RoundedRectangle(cornerRadius: Brand.Radius.medium).stroke(Brand.hairline) }
    }
}
