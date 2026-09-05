import Foundation
import XCTest

final class TranscriptProjectionTests: XCTestCase {
    private func entry(
        _ id: String,
        presentationID: String? = nil,
        kind: TranscriptEntry.Kind = .event,
        pending: Bool = false,
        turnID: String? = nil,
        startsTurn: Bool = false,
        turnTerminal: Bool = false,
        turnElapsedMs: UInt64? = nil,
        recordedAtMs: Int64? = nil
    ) -> TranscriptEntry {
        TranscriptEntry(
            id: id,
            presentationID: presentationID,
            text: id,
            kind: kind,
            role: kind == .assistant ? nil : .tool,
            format: "plain_text",
            pending: pending,
            turnID: turnID,
            startsTurn: startsTurn,
            turnTerminal: turnTerminal,
            turnElapsedMs: turnElapsedMs,
            recordedAtMs: recordedAtMs
        )
    }

    private var phrase: TranscriptWaitingPhrase {
        TranscriptWaitingPhrase(startedAt: Date(timeIntervalSince1970: 0), order: ["thinking"])
    }

    /// A run at the tail shows the phrase in its own summary line; with no run to hold it the
    /// phrase takes a line of its own.
    func testWaitingPhraseGoesToTheTailRunWhenThereIsOne() {
        let event = entry("event:1")
        let message = entry("wire:1", presentationID: "step:final_answer:0", kind: .assistant)

        XCTAssertEqual(
            TranscriptProjection(entries: [message, event], waitingPhrase: phrase).waiting,
            .row("event:1", phrase)
        )
        XCTAssertEqual(
            TranscriptProjection(entries: [event, message], waitingPhrase: phrase).waiting,
            .standaloneLine(phrase)
        )
    }

    /// Ownership follows the renderable tail. A new run can take the fixed summary slot without
    /// leaving a standalone line behind above it.
    func testWaitingPhraseMovesToANewTailRun() {
        let message = entry("wire:1", presentationID: "step:final_answer:0", kind: .assistant)
        let standalone = TranscriptProjection(entries: [message], waitingPhrase: phrase)
        XCTAssertEqual(standalone.waiting, .standaloneLine(phrase))

        let event = entry("event:1")
        let joined = TranscriptProjection(
            entries: [message, event],
            waitingPhrase: phrase,
            previous: standalone
        )

        XCTAssertEqual(joined.waiting, .row("event:1", phrase))
        XCTAssertEqual(
            TranscriptProjection(entries: [message, event], previous: joined).waiting,
            .absent
        )
    }

    func testWaitingPhraseLeavesAnActivityRowWhenNarrativeBecomesTheTail() {
        let event = entry("event:1")
        let running = TranscriptProjection(entries: [event], waitingPhrase: phrase)
        XCTAssertEqual(running.waiting, .row("event:1", phrase))

        let message = entry("wire:1", presentationID: "step:commentary:0", kind: .commentary)
        let continued = TranscriptProjection(
            entries: [event, message],
            waitingPhrase: phrase,
            previous: running
        )

        XCTAssertEqual(continued.waiting, .standaloneLine(phrase))
    }

    /// A run with nothing but files still owns the phrase. It is not hidden: `EventGroupView`
    /// draws its summary slot whenever it holds one, so the phrase appears in the row rather
    /// than as a second line below it that would have to leave again.
    func testFileOnlyActivityRowOwnsTheWaitingPhrase() {
        let event = entry("event:1")
        event.text = ""
        event.files = [SessionFileReference(
            id: "file:1",
            name: "result.txt",
            size: 1,
            mediaType: "text/plain"
        )]

        let projection = TranscriptProjection(entries: [event], waitingPhrase: phrase)

        XCTAssertEqual(projection.waiting, .row("event:1", phrase))
    }

    func testStandaloneWaitingLineIsInitialStructure() {
        let projection = TranscriptProjection(entries: [], waitingPhrase: phrase)

        XCTAssertEqual(projection.waiting, .standaloneLine(phrase))
        XCTAssertEqual(projection.structuralRevision, 1)
    }

    /// The line is a row's worth of height. The phrase rotating inside it is not.
    func testStandaloneLineIsStructuralAndItsPhraseIsNot() {
        let message = entry("wire:1", presentationID: "step:final_answer:0", kind: .assistant)
        let settled = TranscriptProjection(entries: [message])
        let waiting = TranscriptProjection(
            entries: [message],
            waitingPhrase: phrase,
            previous: settled
        )
        XCTAssertEqual(waiting.structuralRevision, settled.structuralRevision + 1)

        let rotated = TranscriptWaitingPhrase(
            startedAt: phrase.startedAt,
            order: ["something else"]
        )
        let later = TranscriptProjection(
            entries: [message],
            waitingPhrase: rotated,
            previous: waiting
        )
        XCTAssertEqual(later.structuralRevision, waiting.structuralRevision)
    }

    func testTextDeltaLeavesStructuralRevisionUnchanged() {
        let message = entry("wire:1", presentationID: "step:final_answer:0", kind: .assistant)
        let first = TranscriptProjection(entries: [message])

        message.text += " more"
        let second = TranscriptProjection(entries: [message], previous: first)

        XCTAssertEqual(second.structuralRevision, first.structuralRevision)
        XCTAssertTrue(second.rows[0].records[0] === message)
    }

    func testSequentialActivityWithDifferentTurnIDsStaysGrouped() {
        let firstEvent = entry("event:1", turnID: "child-turn-1")
        let first = TranscriptProjection(entries: [firstEvent])
        let secondEvent = entry("event:2", turnID: "child-turn-2")
        let second = TranscriptProjection(entries: [firstEvent, secondEvent], previous: first)

        XCTAssertEqual(second.rows.count, 1)
        XCTAssertEqual(second.rows[0].records.map(\.presentationID), ["event:1", "event:2"])
        XCTAssertEqual(second.structuralRevision, first.structuralRevision)
    }

    func testCompletedTurnCollapsesMixedChildActivityIntoWorkedGroup() {
        let entries = [
            entry("user", kind: .user, turnID: "turn-root", startsTurn: true),
            entry("root-work", turnID: "turn-root"),
            entry("child-work", turnID: "turn-child"),
            entry("final", kind: .assistant, turnID: "turn-root", turnTerminal: true),
        ]

        let projection = TranscriptProjection(entries: entries)

        XCTAssertEqual(projection.rows.map(\.kind), [.user, .workedGroup, .narrative])
        XCTAssertEqual(
            projection.rows[1].records.map(\.id),
            ["root-work", "child-work"]
        )
    }

    func testBotIdentityDoesNotForceAnAssistantMessageOntoTheInputSide() {
        let bot = TranscriptEntry(
            id: "bot-message",
            text: "I found an option",
            kind: .assistant,
            format: "plain_text",
            pending: false,
            messageMetadata: TranscriptMessageMetadata(
                author: .peer(
                    messageID: "bot-message",
                    sessionID: "session-1",
                    handle: "researcher",
                    symbol: nil
                ),
                delivery: .turn
            )
        )
        let projection = TranscriptProjection(entries: [bot])

        XCTAssertEqual(bot.kind, .assistant)
        XCTAssertEqual(projection.rows.map(\.kind), [.narrative])
        XCTAssertEqual(bot.messageMetadata?.author.peerFields?.handle, "researcher")
    }

    func testNewActivityRunBumpsStructuralRevisionOnce() {
        let firstEvent = entry("event:1")
        let user = entry("user:1", kind: .user)
        let settled = TranscriptProjection(entries: [firstEvent, user])
        let nextEvent = entry("event:2")
        let running = TranscriptProjection(entries: [firstEvent, user, nextEvent], previous: settled)

        XCTAssertEqual(running.rows.map(\.id), ["event:1", "user:1", "event:2"])
        XCTAssertEqual(running.structuralRevision, settled.structuralRevision + 1)
    }

    func testReplacementWithTheSamePresentationIdentityKeepsTheRowStable() {
        let streamed = entry(
            "model-stream:1",
            presentationID: "step:commentary:0",
            kind: .commentary
        )
        let first = TranscriptProjection(entries: [streamed])
        let snapshot = entry(
            "model-output:1",
            presentationID: "step:commentary:0",
            kind: .commentary
        )
        snapshot.text = "replacement"

        let second = TranscriptProjection(entries: [snapshot], previous: first)

        XCTAssertEqual(second.rows[0].id, first.rows[0].id)
        XCTAssertEqual(second.structuralRevision, first.structuralRevision)
        XCTAssertTrue(second.rows[0].records[0] === snapshot)
    }

    func testBoundaryAndSizingUsePresentationSemantics() {
        let firstEvent = entry("wire:1", presentationID: "event:first")
        let secondEvent = entry("wire:2", presentationID: "event:second")
        let user = entry("wire:3", presentationID: "user:first", kind: .user)
        let narrative = entry(
            "wire:4",
            presentationID: "step:final_answer:0",
            kind: .assistant
        )

        let projection = TranscriptProjection(
            entries: [firstEvent, secondEvent, user, narrative],
            breakBefore: secondEvent.presentationID
        )

        XCTAssertEqual(projection.rows.map(\.id), [
            "event:first",
            "event:second",
            "user:first",
            "step:final_answer:0",
        ])
        XCTAssertEqual(projection.rows.map(\.sizing), [
            .fixedSummary,
            .fixedSummary,
            .intrinsic,
            .intrinsic,
        ])
    }

    func testCompletedTurnCollapsesWorkOnlyAfterTheFinalMessageFinishes() {
        let turnID = "turn-1"
        let user = entry(
            "user",
            kind: .user,
            turnID: turnID,
            startsTurn: true,
            recordedAtMs: 1_000
        )
        let commentary = entry(
            "commentary",
            kind: .commentary,
            turnID: turnID,
            recordedAtMs: 1_500
        )
        let event = entry(
            "event",
            turnID: turnID,
            recordedAtMs: 2_000
        )
        let steering = entry(
            "steering",
            kind: .user,
            turnID: turnID,
            recordedAtMs: 2_500
        )
        let final = entry(
            "final",
            kind: .assistant,
            pending: true,
            turnID: turnID,
            turnTerminal: true,
            recordedAtMs: 4_200
        )

        let running = TranscriptProjection(
            entries: [user, commentary, event, steering, final]
        )
        XCTAssertEqual(running.rows.map(\.kind), [
            .user,
            .narrative,
            .activityGroup,
            .user,
            .narrative,
        ])

        final.pending = false
        let completed = TranscriptProjection(
            entries: [user, commentary, event, steering, final],
            previous: running
        )

        XCTAssertEqual(completed.rows.map(\.kind), [.user, .workedGroup, .narrative])
        XCTAssertEqual(
            completed.rows[1].records.map(\.id),
            ["commentary", "event", "steering"]
        )
        XCTAssertEqual(completed.rows[1].elapsedMs, 3_200)
        XCTAssertEqual(TranscriptProjection.turnCount(
            in: [user, commentary, event, steering, final]
        ), 1)
    }

    func testTurnWindowKeepsSteeringInsideCompletedTurn() {
        let turnID = "turn-1"
        let earlier = [
            entry("earlier-user", kind: .user, turnID: "turn-0", startsTurn: true),
            entry("earlier-final", kind: .assistant, turnID: "turn-0", turnTerminal: true),
        ]
        let user = entry(
            "user",
            kind: .user,
            turnID: turnID,
            startsTurn: true,
            recordedAtMs: 1_000
        )
        let commentary = entry(
            "commentary",
            kind: .commentary,
            turnID: turnID,
            recordedAtMs: 1_500
        )
        let steering = entry(
            "steering",
            kind: .user,
            turnID: turnID,
            recordedAtMs: 2_500
        )
        let final = entry(
            "final",
            kind: .assistant,
            turnID: turnID,
            turnTerminal: true,
            turnElapsedMs: 3_200,
            recordedAtMs: 4_200
        )

        let currentTurn = [user, commentary, steering, final]
        let window = TranscriptProjection.turnWindow(
            from: earlier + currentTurn,
            maximumTurns: 1
        )
        let projection = TranscriptProjection(entries: window.entries)

        XCTAssertEqual(window.entries.map(\.id), currentTurn.map(\.id))
        XCTAssertEqual(window.turnCount, 1)
        XCTAssertTrue(window.hasEarlierEntries)
        XCTAssertEqual(projection.rows.map(\.kind), [.user, .workedGroup, .narrative])
        XCTAssertEqual(projection.rows[1].records.map(\.id), ["commentary", "steering"])
        XCTAssertEqual(projection.rows[1].elapsedMs, 3_200)

        let bothTurns = TranscriptProjection.turnWindow(
            from: earlier + currentTurn,
            maximumTurns: 2
        )
        XCTAssertEqual(bothTurns.entries.map(\.id), (earlier + currentTurn).map(\.id))
        XCTAssertEqual(bothTurns.turnCount, 2)
        XCTAssertFalse(bothTurns.hasEarlierEntries)
    }

    func testTurnWindowMatchesWholeTurnSuffixes() {
        let turns = (0..<5).map { index in
            let turnID = "turn-\(index)"
            return [
                entry("user-\(index)", kind: .user, turnID: turnID, startsTurn: true),
                entry("work-\(index)", kind: .commentary, turnID: turnID),
                entry(
                    "final-\(index)",
                    kind: .assistant,
                    turnID: turnID,
                    turnTerminal: true
                ),
            ]
        }
        let entries = turns.flatMap { $0 }

        for maximumTurns in 1...(turns.count + 2) {
            let expectedTurns = Array(turns.suffix(maximumTurns))
            let expectedEntries = expectedTurns.flatMap { $0 }
            let window = TranscriptProjection.turnWindow(
                from: entries,
                maximumTurns: maximumTurns
            )

            XCTAssertEqual(window.entries.map(\.id), expectedEntries.map(\.id))
            XCTAssertEqual(window.turnCount, expectedTurns.count)
            XCTAssertEqual(window.hasEarlierEntries, turns.count > expectedTurns.count)
            XCTAssertTrue(zip(window.entries, expectedEntries).allSatisfy { $0 === $1 })
        }
    }

    func testTurnWindowDoesNotCountAnUnmarkedPrefixPastItsLimit() {
        let prefix = entry("partial", kind: .commentary, turnID: "turn-0")
        let latest = [
            entry("user", kind: .user, turnID: "turn-1", startsTurn: true),
            entry("work", kind: .commentary, turnID: "turn-1"),
            entry("final", kind: .assistant, turnID: "turn-1", turnTerminal: true),
        ]

        let window = TranscriptProjection.turnWindow(
            from: [prefix] + latest,
            maximumTurns: 1
        )

        XCTAssertEqual(window.entries.map(\.id), latest.map(\.id))
        XCTAssertEqual(window.turnCount, 1)
        XCTAssertTrue(window.hasEarlierEntries)
    }
}
