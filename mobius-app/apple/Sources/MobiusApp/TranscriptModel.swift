import Foundation
import Observation

@Observable
final class TranscriptEntry: Identifiable {
    enum Kind: String, Codable, Sendable {
        case user
        case assistant
        case commentary
        case reasoning
        case event
        case error
    }

    let id: String
    let presentationID: String
    var text: String
    var kind: Kind
    var capability: String?
    var role: FrontendBlockRole?
    var update: FrontendBlockUpdate?
    var title: String
    var symbol: String?
    var group: String?
    var format: String
    var tone: String
    var pending: Bool
    var modelStepID: String?
    var turnID: String?
    var startsTurn: Bool
    var turnTerminal: Bool
    var turnElapsedMs: UInt64?
    var sourceSequence: UInt64?
    var recordedAtMs: Int64?
    var messageTarget: MessageTarget?
    var files: [SessionFileReference]

    init(
        id: String,
        presentationID: String? = nil,
        text: String,
        kind: Kind,
        capability: String? = nil,
        role: FrontendBlockRole? = nil,
        update: FrontendBlockUpdate? = nil,
        title: String = "",
        symbol: String? = nil,
        group: String? = nil,
        format: String,
        tone: String = "neutral",
        pending: Bool,
        modelStepID: String? = nil,
        turnID: String? = nil,
        startsTurn: Bool = false,
        turnTerminal: Bool = false,
        turnElapsedMs: UInt64? = nil,
        sourceSequence: UInt64? = nil,
        recordedAtMs: Int64? = nil,
        messageTarget: MessageTarget? = nil,
        files: [SessionFileReference] = []
    ) {
        self.id = id
        self.presentationID = presentationID ?? id
        self.text = text
        self.kind = kind
        self.capability = capability
        self.role = role
        self.update = update
        self.title = title
        self.symbol = symbol
        self.group = group
        self.format = format
        self.tone = tone
        self.pending = pending
        self.modelStepID = modelStepID
        self.turnID = turnID
        self.startsTurn = startsTurn
        self.turnTerminal = turnTerminal
        self.turnElapsedMs = turnElapsedMs
        self.sourceSequence = sourceSequence
        self.recordedAtMs = recordedAtMs
        self.messageTarget = messageTarget
        self.files = files
    }
}

extension TranscriptEntry.Kind {
    /// Everything that is not the narrative: it rides behind a group summary rather than
    /// taking a line of the timeline to itself.
    var isActivity: Bool {
        self == .event || self == .error || self == .reasoning
    }

    var narrativePhase: String? {
        switch self {
        case .assistant: "final_answer"
        case .commentary: "commentary"
        case .reasoning: "reasoning"
        case .user, .event, .error: nil
        }
    }
}

extension TranscriptEntry {
    var hasActivityLineContent: Bool { !title.isEmpty || !text.isEmpty }
}

typealias TranscriptPresentationID = String

enum TranscriptRowSizing: Equatable {
    case fixedSummary
    case intrinsic
}

struct TranscriptPresentationRow: Identifiable {
    enum Kind: Equatable {
        case user
        case narrative
        case activityGroup
        case workedGroup
    }

    let id: TranscriptPresentationID
    let records: [TranscriptEntry]
    let sizing: TranscriptRowSizing
    let kind: Kind
    let elapsedMs: UInt64?

    init(
        id: TranscriptPresentationID,
        records: [TranscriptEntry],
        sizing: TranscriptRowSizing,
        kind: Kind,
        elapsedMs: UInt64? = nil
    ) {
        self.id = id
        self.records = records
        self.sizing = sizing
        self.kind = kind
        self.elapsedMs = elapsedMs
    }
}

struct TranscriptWaitingPhrase: Equatable {
    let startedAt: Date
    let order: [String]
}

/// Where the waiting phrase is drawn, if anywhere.
///
/// A run at the tail shows it in place of its summary, so a gap between steps costs no
/// height. With no run to hold it — the turn has only just started, or a message is the last
/// thing on screen — it takes a line of its own, which the next run then replaces.
enum TranscriptWaitingSlot: Equatable {
    case absent
    case standaloneLine(TranscriptWaitingPhrase)
    case row(TranscriptPresentationID, TranscriptWaitingPhrase)

    var isStandaloneLine: Bool {
        if case .standaloneLine = self { return true }
        return false
    }

    func phrase(forRow id: TranscriptPresentationID) -> TranscriptWaitingPhrase? {
        guard case .row(id, let phrase) = self else { return nil }
        return phrase
    }
}

struct TranscriptProjection {
    let rows: [TranscriptPresentationRow]
    let waiting: TranscriptWaitingSlot
    let structuralRevision: UInt64

    private struct RowStructure: Equatable {
        let id: TranscriptPresentationID
        let sizing: TranscriptRowSizing
        let kind: TranscriptPresentationRow.Kind
    }

    private struct Structure: Equatable {
        let rows: [RowStructure]
        /// The standalone line is a row's worth of height, so it belongs to the structure.
        /// The phrase rotating inside it does not.
        let showsStandaloneLine: Bool
    }

    private let structure: Structure

    init(
        entries: [TranscriptEntry],
        breakBefore boundaryID: TranscriptPresentationID? = nil,
        waitingPhrase: TranscriptWaitingPhrase? = nil,
        previous: TranscriptProjection? = nil
    ) {
        let rows = Self.rows(from: entries, breakBefore: boundaryID, previous: previous)
        let waiting = Self.waitingSlot(
            for: waitingPhrase,
            rows: rows
        )
        let structure = Structure(
            rows: rows.map { RowStructure(id: $0.id, sizing: $0.sizing, kind: $0.kind) },
            showsStandaloneLine: waiting.isStandaloneLine
        )
        let structuralRevision: UInt64
        if let previous {
            structuralRevision = previous.structure == structure
                ? previous.structuralRevision
                : previous.structuralRevision &+ 1
        } else {
            structuralRevision = structure.rows.isEmpty && !structure.showsStandaloneLine ? 0 : 1
        }

        self.rows = rows
        self.waiting = waiting
        self.structuralRevision = structuralRevision
        self.structure = structure
    }

    static func turnWindow(
        from entries: [TranscriptEntry],
        maximumTurns: Int
    ) -> (entries: [TranscriptEntry], turnCount: Int, hasEarlierEntries: Bool) {
        let maximumTurns = max(0, maximumTurns)
        guard maximumTurns > 0, !entries.isEmpty else {
            return ([], 0, !entries.isEmpty)
        }

        let turnStarts = entries.indices.filter { entries[$0].startsTurn }
        if !turnStarts.isEmpty {
            let includedMarkedTurns = min(maximumTurns, turnStarts.count)
            let firstIncludedTurn = turnStarts.count - includedMarkedTurns
            let includesLeadingTurn = turnStarts[0] > entries.startIndex
                && maximumTurns > includedMarkedTurns
            let start = includesLeadingTurn
                ? entries.startIndex
                : turnStarts[firstIncludedTurn]
            return (
                Array(entries[start...]),
                includedMarkedTurns + (includesLeadingTurn ? 1 : 0),
                start > entries.startIndex
            )
        }

        var start = entries.index(before: entries.endIndex)
        var turnCount = 1
        while start > entries.startIndex {
            if entries[start].turnID != entries[start - 1].turnID {
                if turnCount == maximumTurns { break }
                turnCount += 1
            }
            start -= 1
        }
        return (Array(entries[start...]), turnCount, start > entries.startIndex)
    }

    static func turnCount(in entries: [TranscriptEntry]) -> Int {
        guard !entries.isEmpty else { return 0 }
        let turnStarts = entries.indices.filter { entries[$0].startsTurn }
        if let firstTurnStart = turnStarts.first {
            return turnStarts.count + (firstTurnStart > entries.startIndex ? 1 : 0)
        }
        var count = 1
        for index in entries.indices.dropFirst()
            where entries[index].turnID != entries[index - 1].turnID {
            count += 1
        }
        return count
    }

    /// Only the current tail can own the phrase, and it owns it from the moment it exists.
    ///
    /// Waiting for the run to have named itself first cost a bump: the row is created by the
    /// first event of a batch, which may arrive before its title, so for that beat the
    /// transcript held a run *and* a standalone line, then lost the line once the name landed.
    /// Two height changes for one arrival. A run takes the slot as soon as it has one.
    private static func waitingSlot(
        for phrase: TranscriptWaitingPhrase?,
        rows: [TranscriptPresentationRow]
    ) -> TranscriptWaitingSlot {
        guard let phrase else { return .absent }
        guard let tailRun = rows.last, tailRun.kind == .activityGroup else {
            return .standaloneLine(phrase)
        }
        return .row(tailRun.id, phrase)
    }

    private static func rows(
        from entries: [TranscriptEntry],
        breakBefore boundaryID: TranscriptPresentationID?,
        previous: TranscriptProjection?
    ) -> [TranscriptPresentationRow] {
        let rows = groupedRows(from: entries, breakBefore: boundaryID)
        var previousActivityRows = previous?.rows.filter { $0.kind == .activityGroup } ?? []
        var reusedIDs: [Int: TranscriptPresentationID] = [:]

        // Claim old anchors first. Once its original record leaves the display window, an ID
        // is only an identity: loading that record back must not steal it from the visible run.
        // ponytail: O(n²) over the bounded visible transcript; index only if profiling asks.
        for (index, row) in rows.enumerated() where row.kind == .activityGroup {
            let recordIDs = Set(row.records.map(\.presentationID))
            guard let match = previousActivityRows.firstIndex(where: { previousRow in
                previousRow.records.contains { $0.presentationID == previousRow.id }
                    && recordIDs.contains(previousRow.id)
            }) else { continue }
            reusedIDs[index] = previousActivityRows.remove(at: match).id
        }
        for (index, row) in rows.enumerated()
            where row.kind == .activityGroup && reusedIDs[index] == nil {
            let recordIDs = Set(row.records.map(\.presentationID))
            guard let match = previousActivityRows.firstIndex(where: { previousRow in
                previousRow.records.contains { recordIDs.contains($0.presentationID) }
            }) else { continue }
            reusedIDs[index] = previousActivityRows.remove(at: match).id
        }

        let reservedIDs = Set(reusedIDs.values)
        let defaultIDs = Set(rows.map(\.id))
        var claimedIDs = Set<TranscriptPresentationID>()
        let stableRows = rows.enumerated().map { index, row in
            var id = reusedIDs[index] ?? row.id
            if row.kind == .activityGroup,
               reusedIDs[index] == nil,
               reservedIDs.contains(id) || claimedIDs.contains(id) {
                var suffix = 1
                repeat {
                    id = "\(row.id):activity-group:\(suffix)"
                    suffix += 1
                } while reservedIDs.contains(id)
                    || defaultIDs.contains(id)
                    || claimedIDs.contains(id)
            }
            claimedIDs.insert(id)
            guard id != row.id else { return row }
            return TranscriptPresentationRow(
                id: id,
                records: row.records,
                sizing: row.sizing,
                kind: row.kind,
                elapsedMs: row.elapsedMs
            )
        }
        return collapseCompletedWork(in: stableRows)
    }

    private static func groupedRows(
        from entries: [TranscriptEntry],
        breakBefore boundaryID: TranscriptPresentationID?
    ) -> [TranscriptPresentationRow] {
        var rows: [TranscriptPresentationRow] = []
        var activity: [TranscriptEntry] = []

        func appendActivity() {
            guard let first = activity.first else { return }
            rows.append(TranscriptPresentationRow(
                id: first.presentationID,
                records: activity,
                sizing: .fixedSummary,
                kind: .activityGroup
            ))
            activity = []
        }

        for entry in entries {
            if entry.presentationID == boundaryID { appendActivity() }
            if entry.kind.isActivity {
                if entry.turnTerminal { appendActivity() }
                activity.append(entry)
                if entry.turnTerminal { appendActivity() }
                continue
            }
            appendActivity()
            let isUser = entry.kind == .user
            rows.append(TranscriptPresentationRow(
                id: entry.presentationID,
                records: [entry],
                sizing: .intrinsic,
                kind: isUser ? .user : .narrative
            ))
        }
        appendActivity()
        return rows
    }

    private static func collapseCompletedWork(
        in rows: [TranscriptPresentationRow]
    ) -> [TranscriptPresentationRow] {
        guard rows.contains(where: isTurnStart) else {
            return collapseBySharedTurnID(in: rows)
        }

        var collapsed: [TranscriptPresentationRow] = []
        var start = 0
        for end in rows.indices.dropFirst() where isTurnStart(rows[end]) {
            collapsed.append(contentsOf: collapseTurnSegment(Array(rows[start..<end])))
            start = end
        }
        collapsed.append(contentsOf: collapseTurnSegment(Array(rows[start...])))
        return collapsed
    }

    private static func collapseBySharedTurnID(
        in rows: [TranscriptPresentationRow]
    ) -> [TranscriptPresentationRow] {
        var collapsed: [TranscriptPresentationRow] = []
        var start = 0
        while start < rows.count {
            guard let turnID = sharedTurnID(for: rows[start]) else {
                collapsed.append(rows[start])
                start += 1
                continue
            }
            var end = start + 1
            while end < rows.count, sharedTurnID(for: rows[end]) == turnID { end += 1 }
            collapsed.append(contentsOf: collapsedTurn(
                Array(rows[start..<end]),
                turnID: turnID
            ))
            start = end
        }
        return collapsed
    }

    private static func collapseTurnSegment(
        _ rows: [TranscriptPresentationRow]
    ) -> [TranscriptPresentationRow] {
        guard let terminalID = rows
            .flatMap(\.records)
            .first(where: \.turnTerminal)?
            .turnID
        else { return rows }
        return collapsedTurn(rows, turnID: terminalID)
    }

    private static func isTurnStart(_ row: TranscriptPresentationRow) -> Bool {
        row.records.contains(where: \.startsTurn)
    }

    private static func collapsedTurn(
        _ rows: [TranscriptPresentationRow],
        turnID: String
    ) -> [TranscriptPresentationRow] {
        let terminalRows = rows.filter { row in
            row.records.contains(where: \.turnTerminal)
        }
        guard !terminalRows.isEmpty,
              terminalRows.allSatisfy({ row in row.records.allSatisfy { !$0.pending } })
        else { return rows }

        let primaryUserIndex = rows.firstIndex { row in
            row.kind == .user && row.records.contains(where: \.startsTurn)
        }
        let workRows: [TranscriptPresentationRow] = rows.enumerated().compactMap {
            index, row -> TranscriptPresentationRow? in
            guard index != primaryUserIndex,
                  !terminalRows.contains(where: { $0.id == row.id })
            else { return nil }
            return row
        }
        guard !workRows.isEmpty else { return rows }

        let records = workRows.flatMap(\.records)
        let elapsedMs = terminalRows
            .flatMap(\.records)
            .compactMap(\.turnElapsedMs)
            .max() ?? {
                let startedAtMs = rows.flatMap(\.records).compactMap(\.recordedAtMs).min()
                let completedAtMs = terminalRows
                    .flatMap(\.records)
                    .compactMap(\.recordedAtMs)
                    .max()
                return startedAtMs.flatMap { startedAtMs in
                    completedAtMs.map { UInt64(max(0, $0 - startedAtMs)) }
                }
            }()
        var result: [TranscriptPresentationRow] = []
        if let primaryUserIndex { result.append(rows[primaryUserIndex]) }
        result.append(TranscriptPresentationRow(
            id: "turn-work:\(turnID.utf8.count):\(turnID)",
            records: records,
            sizing: .fixedSummary,
            kind: .workedGroup,
            elapsedMs: elapsedMs
        ))
        result.append(contentsOf: terminalRows)
        return result
    }

    private static func sharedTurnID(for row: TranscriptPresentationRow) -> String? {
        guard let turnID = row.records.first?.turnID,
              row.records.allSatisfy({ $0.turnID == turnID })
        else { return nil }
        return turnID
    }
}

/// Typed transcript presentation supplied by the framework.
extension TranscriptEntry {
    static func narrativePresentationID(
        modelStepID: String,
        phase: String,
        ordinal: Int
    ) -> String {
        "\(modelStepID):\(phase):\(ordinal)"
    }

    var headline: String { title }

    /// Everything under the heading — the tool output the one-line row hides.
    var eventDetail: String {
        text
    }

    /// Hosted web search is identified by its protocol role, independent of title or owner.
    var isWebSearch: Bool {
        role == .webSearch
    }

    /// "2 thoughts • 3 tool calls • 4 web searches • 1 approval • 2 events • 1 error", skipping
    /// the empty categories.
    static func summary(for entries: [TranscriptEntry]) -> String {
        var thoughts = 0
        var tools = 0
        var searches = 0
        var approvals = 0
        var artifacts = 0
        var events = 0
        var errors = 0
        for entry in entries {
            if entry.kind == .error || entry.tone == "error" {
                errors += 1
            } else if entry.kind == .reasoning {
                thoughts += 1
            } else if entry.isWebSearch {
                searches += 1
            } else if entry.role == .tool {
                tools += 1
            } else if entry.role == .approval {
                approvals += 1
            } else if entry.role == .artifact {
                artifacts += 1
            } else {
                events += 1
            }
        }
        return [
            (thoughts, "thought"), (tools, "tool call"), (searches, "web search"),
            (approvals, "approval"), (artifacts, "artifact"), (events, "event"),
            (errors, "error")
        ]
        .filter { $0.0 > 0 }
        .map { counted($0.0, $0.1) }
        .joined(separator: " • ")
    }

    private static func counted(_ count: Int, _ noun: String) -> String {
        guard count != 1 else { return "1 \(noun)" }
        let sibilant = ["ch", "sh", "s", "x"].contains { noun.hasSuffix($0) }
        return "\(count) \(noun)\(sibilant ? "es" : "s")"
    }
}
