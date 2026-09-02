import Foundation
import XCTest

final class SwarmStatsTests: XCTestCase {
    /// The same string the gateway's `mention_parser_ignores_email_boundaries_and_deduplicates`
    /// asserts on. The client re-derives what the gateway resolved at post time, so if one
    /// parser changes the counts stop describing the delivery that happened.
    func testMentionParserMatchesTheGateway() {
        XCTAssertEqual(
            swarmMentionedHandles(in: "@one mail@two @one; @three"),
            ["one", "three"]
        )
        XCTAssertEqual(swarmMentionedHandles(in: "@"), [])
        XCTAssertEqual(swarmMentionedHandles(in: "@@nested"), ["nested"])
        XCTAssertEqual(swarmMentionedHandles(in: "under_score99 @with_99"), ["with_99"])
    }

    func testKindsAndEdgesFollowAcceptedPostMentions() {
        let stats = SwarmStats.make(
            messages: [
                message(1, "s1", "amber", "@basil can you take the parser?"),
                message(2, "s2", "basil", "on it"),
                message(3, "s2", "basil", "@amber pushed, see notes@example.com"),
                message(4, "s1", "amber", "ready for review"),
            ]
        )

        XCTAssertEqual(stats.total, 4)
        XCTAssertEqual(stats.count(.directed), 2)
        XCTAssertEqual(stats.count(.broadcast), 2)
        XCTAssertEqual(stats.mentionEdges, 2)
    }

    func testDirectedHistorySurvivesRosterChanges() {
        // The gateway keeps board entries when basil leaves. The stored post was already
        // validated when it was written, so it must remain directed without today's roster.
        let stats = SwarmStats.make(
            messages: [message(1, "s1", "amber", "@basil can you take the parser?")]
        )

        XCTAssertEqual(stats.count(.directed), 1)
        XCTAssertEqual(stats.count(.broadcast), 0)
        XCTAssertEqual(stats.mentionEdges, 1)
    }

    func testEmptyBoardReportsNoActivity() {
        let stats = SwarmStats.make(messages: [])

        XCTAssertEqual(stats.total, 0)
        XCTAssertEqual(stats.count(.directed), 0)
        XCTAssertEqual(stats.count(.broadcast), 0)
        XCTAssertEqual(stats.mentionEdges, 0)
    }

    private func message(
        _ sequence: UInt64,
        _ botID: String,
        _ handle: String,
        _ text: String
    ) -> SwarmMessageRecord {
        SwarmMessageRecord(
            id: "m\(sequence)",
            sequence: sequence,
            authorBotId: botID,
            authorHandle: handle,
            sourceSessionId: "chat-\(sequence)",
            text: text,
            createdAtMs: Int64(sequence) * 1_000,
            inReplyToMessageId: nil,
            replyDepth: 0
        )
    }
}
