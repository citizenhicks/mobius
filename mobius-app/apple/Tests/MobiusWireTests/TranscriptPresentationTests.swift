import Foundation
import XCTest

final class TranscriptMarkdownSelectionTests: XCTestCase {
    func testPlainMarkdownParagraphsShareOneSelectableValue() throws {
        let prose = try XCTUnwrap(continuousProseMarkdown(
            "## Subject\n\nFirst **bold** paragraph.\n\nSecond paragraph."
        ))

        XCTAssertEqual(
            String(prose.characters),
            "Subject\n\nFirst bold paragraph.\n\nSecond paragraph."
        )
        XCTAssertTrue(
            prose.runs.first?.inlinePresentationIntent?.contains(.stronglyEmphasized) == true
        )
        XCTAssertNil(continuousProseMarkdown("First paragraph.\n\n- A list item"))
    }
}

final class TranscriptWaitingNoteTests: XCTestCase {
    func testShowsOnlyWhileATurnRunsWithNothingPending() {
        // A pending row shimmers on its own, and a pending assistant message means text is
        // arriving — neither is waiting.
        XCTAssertTrue(
            TranscriptWaitingNote.isWaiting(
                hasActiveTurn: true,
                lastEntryIsPending: false,
                connectionIsReady: true,
                hasPendingApproval: false,
                hasPendingPicker: false
            )
        )
        XCTAssertFalse(
            TranscriptWaitingNote.isWaiting(
                hasActiveTurn: true,
                lastEntryIsPending: true,
                connectionIsReady: true,
                hasPendingApproval: false,
                hasPendingPicker: false
            )
        )
        XCTAssertFalse(
            TranscriptWaitingNote.isWaiting(
                hasActiveTurn: false,
                lastEntryIsPending: false,
                connectionIsReady: true,
                hasPendingApproval: false,
                hasPendingPicker: false
            )
        )
        XCTAssertFalse(
            TranscriptWaitingNote.isWaiting(
                hasActiveTurn: true,
                lastEntryIsPending: false,
                connectionIsReady: false,
                hasPendingApproval: false,
                hasPendingPicker: false
            )
        )
        XCTAssertFalse(
            TranscriptWaitingNote.isWaiting(
                hasActiveTurn: true,
                lastEntryIsPending: false,
                connectionIsReady: true,
                hasPendingApproval: true,
                hasPendingPicker: false
            )
        )
        XCTAssertFalse(
            TranscriptWaitingNote.isWaiting(
                hasActiveTurn: true,
                lastEntryIsPending: false,
                connectionIsReady: true,
                hasPendingApproval: false,
                hasPendingPicker: true
            )
        )
    }

    func testRotationStaysInRangeAndAdvancesOnSchedule() {
        let order = ["first", "second", "third"]
        let first = TranscriptWaitingNote.message(in: order, elapsed: 0)

        // Holds for the rotation window, then moves on.
        XCTAssertEqual(
            TranscriptWaitingNote.message(
                in: order,
                elapsed: TranscriptWaitingNote.rotation - 0.1
            ),
            first
        )
        XCTAssertEqual(
            TranscriptWaitingNote.message(in: order, elapsed: TranscriptWaitingNote.rotation),
            "second"
        )
        XCTAssertEqual(
            TranscriptWaitingNote.message(
                in: order,
                elapsed: Double(order.count) * TranscriptWaitingNote.rotation
            ),
            first
        )
    }
}

final class TranscriptEventLineTests: XCTestCase {
    private func entry(
        id: String,
        text: String,
        kind: TranscriptEntry.Kind = .event,
        tone: String = "neutral",
        format: String = "plain_text",
        capability: String? = nil,
        role: FrontendBlockRole? = nil,
        title: String = "",
        group: String? = nil
    ) -> TranscriptEntry {
        TranscriptEntry(
            id: id,
            text: text,
            kind: kind,
            capability: capability,
            role: role,
            title: title,
            group: group,
            format: format,
            tone: tone,
            pending: false
        )
    }

    func testUsesTypedPresentationWithoutParsingIDOrProse() {
        let call = entry(
            id: "misleading/legacy/id",
            text: "◉ This remains body text\ntotal 8",
            capability: "tools",
            role: .tool,
            title: "Run command"
        )

        XCTAssertEqual(call.capability, "tools")
        XCTAssertEqual(call.headline, "Run command")
        XCTAssertEqual(call.eventDetail, "◉ This remains body text\ntotal 8")
    }

    func testDoesNotInferMissingMetadata() {
        let bare = entry(id: "9C4F-2B", text: "")

        XCTAssertNil(bare.capability)
        XCTAssertNil(bare.role)
        XCTAssertEqual(bare.headline, "")
        XCTAssertEqual(bare.eventDetail, "")
    }

    func testSummaryCountsAndPluralisesByCategory() {
        let entries = [
            entry(id: "a", text: "", role: .tool),
            entry(id: "b", text: "", role: .tool),
            entry(id: "c", text: "", role: .activity),
            entry(id: "d", text: "", role: .webSearch),
            entry(id: "e", text: "", kind: .error, tone: "error", role: .tool)
        ]

        XCTAssertEqual(
            TranscriptEntry.summary(for: entries),
            "2 tool calls • 1 web search • 1 event • 1 error"
        )
        XCTAssertEqual(TranscriptEntry.summary(for: [entries[0]]), "1 tool call")
        // "search" takes -es, which a bare +"s" would get wrong.
        XCTAssertEqual(
            TranscriptEntry.summary(for: [entries[3], entries[3]]),
            "2 web searches"
        )
    }

    func testGroupsSequentialMixedActivityAcrossCapabilitiesAndGroups() {
        let tool = entry(
            id: "tool",
            text: "Read a file",
            capability: "tools",
            role: .tool,
            group: "tools/turn"
        )
        let event = entry(
            id: "event",
            text: "Delegated a task",
            capability: "subagents",
            role: .activity,
            group: "subagents/turn"
        )
        let search = entry(
            id: "search",
            text: "Searched the web",
            capability: "web_search",
            role: .webSearch,
            group: "web_search/turn"
        )
        let reasoning = entry(id: "reasoning", text: "Thinking", kind: .reasoning)
        let laterTool = entry(id: "later-tool", text: "Checked again", role: .tool)
        let notice = entry(id: "notice", text: "Needs attention", role: .notice)

        let rows = TranscriptProjection(
            entries: [tool, event, search, reasoning, laterTool, notice]
        ).rows
        XCTAssertEqual(
            rows.map { $0.records.map(\.id) },
            [["tool", "event", "search", "reasoning", "later-tool", "notice"]]
        )

        XCTAssertEqual(
            TranscriptProjection(
                entries: [tool, event],
                breakBefore: event.presentationID
            ).rows.map { $0.records.map(\.id) },
            [["tool"], ["event"]]
        )
    }

    func testGroupingKeepsOnlyTheNarrativeAlone() {
        // The user's message, commentary, and the final message always stand alone; everything
        // else — reasoning, approvals, artifacts, notices, untyped — joins the run around them.
        let question = entry(id: "question", text: "Do the thing", kind: .user)
        let thinking = entry(id: "thinking", text: "Planning", kind: .reasoning)
        let tool = entry(id: "tool", text: "Read a file", role: .tool)
        let approval = entry(id: "approval", text: "AI approved", role: .approval)
        let artifact = entry(id: "artifact", text: "Wrote a file", role: .artifact)
        let commentary = entry(id: "commentary", text: "Halfway there", kind: .commentary)
        let notice = entry(id: "notice", text: "Context low", role: .notice)
        let untyped = entry(id: "untyped", text: "Something happened")
        let answer = entry(id: "answer", text: "Done", kind: .assistant)

        let rows = TranscriptProjection(
            entries: [question, thinking, tool, approval, artifact, commentary, notice, untyped, answer]
        ).rows
        XCTAssertEqual(
            rows.map { $0.records.map(\.id) },
            [
                ["question"],
                ["thinking", "tool", "approval", "artifact"],
                ["commentary"],
                ["notice", "untyped"],
                ["answer"]
            ]
        )
        XCTAssertEqual(
            TranscriptEntry.summary(for: [thinking, tool, approval, artifact, notice]),
            "1 thought • 1 tool call • 1 approval • 1 artifact • 1 event"
        )
    }

    func testWebSearchUsesOnlyTheTypedRole() {
        XCTAssertTrue(entry(id: "anything", text: "not search prose", role: .webSearch).isWebSearch)
        XCTAssertFalse(entry(
            id: "web_search/deceptive",
            text: "Search the web",
            capability: "web_search",
            role: .tool
        ).isWebSearch)
    }
}

/// What the transcript does while a run arrives, step by step.
///
/// The scroll view animates its bottom-anchor correction on `structuralRevision`, so one
/// logical arrival that bumps it more than once is one bump the reader sees.
final class TranscriptRunArrivalTests: XCTestCase {
    private func message(_ id: String) -> TranscriptEntry {
        TranscriptEntry(
            id: id,
            presentationID: "step:final_answer:0",
            text: "a settled answer",
            kind: .assistant,
            format: "plain_text",
            pending: false
        )
    }

    /// `hasActivityLineContent` is `!title.isEmpty || !text.isEmpty`, so an event that has not
    /// named itself yet is a row the group draws nothing for.
    private func activity(_ id: String, title: String) -> TranscriptEntry {
        TranscriptEntry(
            id: id,
            text: "",
            kind: .event,
            role: .tool,
            title: title,
            format: "plain_text",
            pending: false
        )
    }

    private var phrase: TranscriptWaitingPhrase {
        TranscriptWaitingPhrase(startedAt: Date(timeIntervalSince1970: 0), order: ["thinking"])
    }

    /// Replays a sequence of transcript states the way `TranscriptView` consumes them and
    /// reports every state the scroll view would react to.
    private func trace(
        _ steps: [(label: String, entries: [TranscriptEntry], waiting: Bool)]
    ) -> [(label: String, revision: UInt64, rows: [String], waiting: TranscriptWaitingSlot)] {
        var previous: TranscriptProjection?
        var out: [(String, UInt64, [String], TranscriptWaitingSlot)] = []
        for step in steps {
            let projection = TranscriptProjection(
                entries: step.entries,
                waitingPhrase: step.waiting ? phrase : nil,
                previous: previous
            )
            previous = projection
            out.append((
                step.label,
                projection.structuralRevision,
                projection.rows.map(\.id),
                projection.waiting
            ))
        }
        return out
    }

    private func report(
        _ trace: [(label: String, revision: UInt64, rows: [String], waiting: TranscriptWaitingSlot)]
    ) {
        var lines = ["--- rev | rows | waiting ---"]
        for step in trace {
            let waiting: String
            switch step.waiting {
            case .absent: waiting = "absent"
            case .standaloneLine: waiting = "STANDALONE LINE (own row)"
            case .row(let id, _): waiting = "in row \(id)"
            }
            lines.append("  \(step.revision)  \(step.label.padding(toLength: 26, withPad: " ", startingAt: 0)) rows=\(step.rows) \(waiting)")
        }
        let url = URL(fileURLWithPath: "/tmp/mobius-trace.txt")
        let text = lines.joined(separator: "\n") + "\n\n"
        if let handle = try? FileHandle(forWritingTo: url) {
            handle.seekToEndOfFile()
            handle.write(Data(text.utf8))
            try? handle.close()
        } else {
            try? text.write(to: url, atomically: true, encoding: .utf8)
        }
    }

    /// Three tool calls landing together, each one already carrying its title.
    func testParallelBatchWhereEveryEventArrivesNamed() {
        let answer = message("wire:1")
        let a = activity("event:a", title: "Read")
        let b = activity("event:b", title: "Grep")
        let c = activity("event:c", title: "Bash")
        let steps = [
            ("answer, waiting", [answer], true),
            ("+ a", [answer, a], true),
            ("+ b", [answer, a, b], true),
            ("+ c", [answer, a, b, c], true),
        ]
        let trace = self.trace(steps)
        report(trace)
        XCTAssertEqual(trace.map(\.revision).reduce(into: Set()) { $0.insert($1) }.count, 2)
    }

    /// The same batch, but the first event has not named itself when its row is created.
    func testParallelBatchWhereTheFirstEventArrivesUnnamed() {
        let answer = message("wire:1")
        let a = activity("event:a", title: "")
        let named = activity("event:a", title: "Read")
        let b = activity("event:b", title: "Grep")
        let steps = [
            ("answer, waiting", [answer], true),
            ("+ a (unnamed)", [answer, a], true),
            ("a named", [answer, named], true),
            ("+ b", [answer, named, b], true),
        ]
        let trace = self.trace(steps)
        report(trace)
        XCTAssertEqual(
            Set(trace.map(\.revision)).count, 2,
            "one arrival must move the transcript once, not twice"
        )
        XCTAssertEqual(trace[1].waiting, .row("event:a", phrase))
    }

    /// A batch whose records land out of sequence order, which is what `mergeHistory`
    /// rebuilding by sequence would produce.
    func testBatchArrivingOutOfSequenceOrder() {
        let answer = message("wire:1")
        let a = activity("event:a", title: "Read")
        let b = activity("event:b", title: "Grep")
        let steps = [
            ("answer, waiting", [answer], true),
            ("+ b arrives first", [answer, b], true),
            ("a sorts ahead of b", [answer, a, b], true),
        ]
        let trace = self.trace(steps)
        report(trace)
        XCTAssertEqual(
            trace[1].rows.last, trace[2].rows.last,
            "the run changed identity when an earlier record sorted ahead of it"
        )
        XCTAssertEqual(trace[1].revision, trace[2].revision)
    }

    func testBatchKeepsItsIdentityAcrossWindowShiftAndHistoryPrepend() {
        let a = activity("event:a", title: "Read")
        let b = activity("event:b", title: "Grep")
        let c = activity("event:c", title: "Bash")
        let first = TranscriptProjection(entries: [a, b])
        let shifted = TranscriptProjection(entries: [b, c], previous: first)
        let restored = TranscriptProjection(
            entries: [a, b, c],
            breakBefore: b.presentationID,
            previous: shifted
        )

        XCTAssertEqual(first.rows.last?.id, shifted.rows.last?.id)
        XCTAssertEqual(first.structuralRevision, shifted.structuralRevision)
        XCTAssertEqual(shifted.rows.last?.id, restored.rows.last?.id)
        XCTAssertEqual(Set(restored.rows.map(\.id)).count, restored.rows.count)
    }
}
