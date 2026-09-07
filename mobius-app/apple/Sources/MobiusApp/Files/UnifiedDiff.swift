import Foundation

struct UnifiedDiffDocument: Equatable, Sendable {
    let files: [UnifiedDiffFile]
    let isTruncated: Bool

    init(_ source: String) {
        var parser = UnifiedDiffParser()
        self = parser.parse(source)
    }

    fileprivate init(files: [UnifiedDiffFile], isTruncated: Bool) {
        self.files = files
        self.isTruncated = isTruncated
    }

    var added: Int { files.reduce(0) { $0 + $1.added } }
    var removed: Int { files.reduce(0) { $0 + $1.removed } }

    var fileChanges: [UnifiedDiffFileChange] {
        var changes: [UnifiedDiffFileChange] = []
        var indices: [String: Int] = [:]
        for file in files {
            if let index = indices[file.path] {
                let change = changes[index]
                changes[index] = UnifiedDiffFileChange(
                    path: file.path,
                    added: change.added + file.added,
                    removed: change.removed + file.removed
                )
            } else {
                indices[file.path] = changes.count
                changes.append(UnifiedDiffFileChange(
                    path: file.path,
                    added: file.added,
                    removed: file.removed
                ))
            }
        }
        return changes
    }
}

struct UnifiedDiffFileChange: Identifiable, Equatable, Sendable {
    let path: String
    let added: Int
    let removed: Int

    var id: String { path }
}

struct UnifiedDiffFile: Identifiable, Equatable, Sendable {
    let id: Int
    let path: String
    let rows: [UnifiedDiffRow]
    let added: Int
    let removed: Int
}

struct UnifiedDiffRow: Identifiable, Equatable, Sendable {
    enum Kind: Equatable, Sendable {
        case hunk(UnifiedDiffHunk)
        case addition
        case removal
        case context
        case metadata
    }

    let id: Int
    let kind: Kind
    let oldNumber: Int?
    let newNumber: Int?
    let text: String
}

struct UnifiedDiffHunk: Equatable, Sendable {
    let oldRange: UnifiedDiffRange
    let newRange: UnifiedDiffRange
    let added: Int
    let removed: Int

    var title: LocalizedStringResource {
        let range = newRange.count >= oldRange.count ? newRange : oldRange
        guard range.count > 1 else { return "Line \(range.start)" }
        return "Lines \(range.start)–\(range.start + range.count - 1)"
    }
}

struct UnifiedDiffRange: Equatable, Sendable {
    let start: Int
    let count: Int
}

private struct PendingDiffFile {
    var fallbackPath: String?
    var oldPath: String?
    var newPath: String?
    var metadata: [String] = []
    var rows: [UnifiedDiffRow] = []
    var added = 0
    var removed = 0
    var hasHunks = false
}

private struct PendingDiffHunk {
    let header: String
    let oldRange: UnifiedDiffRange
    let newRange: UnifiedDiffRange
    var oldLine: Int
    var newLine: Int
    var lines: [PendingDiffLine] = []
    var added = 0
    var removed = 0
}

private struct PendingDiffLine {
    let kind: UnifiedDiffRow.Kind
    let oldNumber: Int?
    let newNumber: Int?
    let text: String
}

private struct UnifiedDiffParser {
    private static let maximumRenderedLineCharacters = 4_096

    private var files: [UnifiedDiffFile] = []
    private var fileIndices: [String: Int] = [:]
    private var file: PendingDiffFile?
    private var hunk: PendingDiffHunk?
    private var nextFileID = 0
    private var nextRowID = 0
    private var isTruncated = false

    mutating func parse(_ source: String) -> UnifiedDiffDocument {
        let lines = source.split(separator: "\n", omittingEmptySubsequences: false)
        for (index, slice) in lines.enumerated() {
            if index.isMultiple(of: 1_024), Task<Never, Never>.isCancelled { break }
            consume(String(slice))
        }
        flushFile()
        return UnifiedDiffDocument(files: files, isTruncated: isTruncated)
    }

    private mutating func consume(_ raw: String) {
        if raw == "[diff truncated]" {
            isTruncated = true
            return
        }
        if raw.hasPrefix("diff --git ") {
            flushFile()
            file = PendingDiffFile(fallbackPath: Self.path(fromGitHeader: raw))
            nextRowID = 0
            return
        }
        if file == nil, raw.hasPrefix("--- ") {
            file = PendingDiffFile(fallbackPath: nil)
            nextRowID = 0
        }
        guard file != nil else { return }
        if raw.hasPrefix("@@") {
            flushHunk()
            let ranges = Self.hunkRanges(raw)
            hunk = PendingDiffHunk(
                header: raw,
                oldRange: ranges.old,
                newRange: ranges.new,
                oldLine: ranges.old.start,
                newLine: ranges.new.start
            )
            return
        }
        if hunk != nil {
            consumeHunkLine(raw)
            return
        }
        if raw.hasPrefix("--- ") {
            file?.oldPath = Self.cleanPath(String(raw.dropFirst(4)))
        } else if raw.hasPrefix("+++ ") {
            file?.newPath = Self.cleanPath(String(raw.dropFirst(4)))
        } else if raw.hasPrefix("rename from ") {
            file?.oldPath = Self.cleanPath(String(raw.dropFirst(12)))
            file?.metadata.append(raw)
        } else if raw.hasPrefix("rename to ") {
            file?.newPath = Self.cleanPath(String(raw.dropFirst(10)))
            file?.metadata.append(raw)
        } else if raw.hasPrefix("copy from ") {
            file?.oldPath = Self.cleanPath(String(raw.dropFirst(10)))
            file?.metadata.append(raw)
        } else if raw.hasPrefix("copy to ") {
            file?.newPath = Self.cleanPath(String(raw.dropFirst(8)))
            file?.metadata.append(raw)
        } else if !raw.isEmpty {
            file?.metadata.append(raw)
        }
    }

    private mutating func consumeHunkLine(_ raw: String) {
        guard var pending = hunk else { return }
        // Release the second Array reference before appending so large hunks keep unique
        // storage instead of triggering copy-on-write for every line.
        hunk = nil
        let line: PendingDiffLine?
        if raw.hasPrefix("+") {
            line = PendingDiffLine(
                kind: .addition,
                oldNumber: nil,
                newNumber: pending.newLine,
                text: Self.renderedText(raw.dropFirst())
            )
            pending.newLine += 1
            pending.added += 1
        } else if raw.hasPrefix("-") {
            line = PendingDiffLine(
                kind: .removal,
                oldNumber: pending.oldLine,
                newNumber: nil,
                text: Self.renderedText(raw.dropFirst())
            )
            pending.oldLine += 1
            pending.removed += 1
        } else if raw.hasPrefix(" ") {
            line = PendingDiffLine(
                kind: .context,
                oldNumber: pending.oldLine,
                newNumber: pending.newLine,
                text: Self.renderedText(raw.dropFirst())
            )
            pending.oldLine += 1
            pending.newLine += 1
        } else if !raw.isEmpty {
            line = PendingDiffLine(
                kind: .metadata,
                oldNumber: nil,
                newNumber: nil,
                text: Self.renderedText(raw[...])
            )
        } else {
            line = nil
        }
        if let line { pending.lines.append(line) }
        hunk = pending
    }

    private mutating func flushHunk() {
        guard let hunk else { return }
        file?.hasHunks = true
        file?.added += hunk.added
        file?.removed += hunk.removed
        appendRow(
            kind: .hunk(UnifiedDiffHunk(
                oldRange: hunk.oldRange,
                newRange: hunk.newRange,
                added: hunk.added,
                removed: hunk.removed
            )),
            text: hunk.header
        )
        for line in hunk.lines {
            appendRow(
                kind: line.kind,
                oldNumber: line.oldNumber,
                newNumber: line.newNumber,
                text: line.text
            )
        }
        self.hunk = nil
    }

    private mutating func flushFile() {
        flushHunk()
        guard var pending = file else { return }
        if !pending.hasHunks {
            for text in pending.metadata {
                pending.rows.append(UnifiedDiffRow(
                    id: nextRowID,
                    kind: .metadata,
                    oldNumber: nil,
                    newNumber: nil,
                    text: text
                ))
                nextRowID += 1
            }
        }
        let path = pending.newPath ?? pending.oldPath ?? pending.fallbackPath ?? "Code changes"
        guard !pending.rows.isEmpty else {
            self.file = nil
            return
        }
        if let index = fileIndices[path] {
            let existing = files[index]
            let rowOffset = existing.rows.count
            let appendedRows = pending.rows.enumerated().map { offset, row in
                UnifiedDiffRow(
                    id: rowOffset + offset,
                    kind: row.kind,
                    oldNumber: row.oldNumber,
                    newNumber: row.newNumber,
                    text: row.text
                )
            }
            files[index] = UnifiedDiffFile(
                id: existing.id,
                path: path,
                rows: existing.rows + appendedRows,
                added: existing.added + pending.added,
                removed: existing.removed + pending.removed
            )
        } else {
            fileIndices[path] = files.count
            files.append(UnifiedDiffFile(
                id: nextFileID,
                path: path,
                rows: pending.rows,
                added: pending.added,
                removed: pending.removed
            ))
            nextFileID += 1
        }
        self.file = nil
    }

    private mutating func appendRow(
        kind: UnifiedDiffRow.Kind,
        oldNumber: Int? = nil,
        newNumber: Int? = nil,
        text: String
    ) {
        file?.rows.append(UnifiedDiffRow(
            id: nextRowID,
            kind: kind,
            oldNumber: oldNumber,
            newNumber: newNumber,
            text: text
        ))
        nextRowID += 1
    }

    private static func hunkRanges(
        _ header: String
    ) -> (old: UnifiedDiffRange, new: UnifiedDiffRange) {
        let fields = header.split(separator: " ")
        return (
            range(fields.first { $0.hasPrefix("-") }),
            range(fields.first { $0.hasPrefix("+") })
        )
    }

    private static func range(_ field: Substring?) -> UnifiedDiffRange {
        guard let field else { return UnifiedDiffRange(start: 0, count: 0) }
        let values = field.dropFirst().split(separator: ",", maxSplits: 1)
        return UnifiedDiffRange(
            start: Int(values.first ?? "") ?? 0,
            count: values.count == 2 ? Int(values[1]) ?? 0 : 1
        )
    }

    private static func path(fromGitHeader header: String) -> String? {
        if let marker = header.range(of: " b/", options: .backwards) {
            return cleanPath(String(header[marker.lowerBound...].dropFirst()))
        }
        if let marker = header.range(of: "\"b/", options: .backwards) {
            return cleanPath(String(header[marker.lowerBound...]))
        }
        return header.split(separator: " ").last.flatMap { cleanPath(String($0)) }
    }

    private static func cleanPath(_ raw: String) -> String? {
        var path = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if path.hasPrefix("\"") && path.hasSuffix("\"") {
            path.removeFirst()
            path.removeLast()
        }
        guard path != "/dev/null", !path.isEmpty else { return nil }
        if path.hasPrefix("a/") || path.hasPrefix("b/") { path.removeFirst(2) }
        return path
    }

    private static func renderedText(_ text: Substring) -> String {
        guard text.count > maximumRenderedLineCharacters else { return String(text) }
        return String(text.prefix(maximumRenderedLineCharacters)) + " …"
    }
}
