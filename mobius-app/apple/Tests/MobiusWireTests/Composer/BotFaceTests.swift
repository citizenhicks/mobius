@testable import Mobius
import XCTest

@MainActor
final class BotFaceTests: XCTestCase {
    func testChatMoodPriority() {
        func mood(
            ready: Bool = true, reaction: BotMood? = nil, listening: Bool = false,
            draft: Bool = false, running: Bool = false
        ) -> BotMood {
            .chat(
                isReady: ready, reaction: reaction, isListening: listening,
                hasDraft: draft, isRunning: running
            )
        }
        XCTAssertEqual(mood(), .idle)
        XCTAssertEqual(mood(running: true), .thinking)
        XCTAssertEqual(mood(draft: true, running: true), .watching)
        XCTAssertEqual(mood(listening: true, draft: true, running: true), .listening)
        XCTAssertEqual(mood(reaction: .happy, listening: true), .happy)
        XCTAssertEqual(mood(reaction: .dizzy, draft: true), .dizzy)
        XCTAssertEqual(mood(ready: false, reaction: .dizzy, running: true), .sleepy)
    }
    func testOnlyTerminalEntriesFromTheFinishedTurnTriggerAReaction() {
        let reply = TranscriptEntry(
            id: "reply", text: "Done", kind: .assistant, format: "plain", pending: false,
            turnID: "turn", turnTerminal: true)
        XCTAssertEqual(BotMood.completion(for: "turn", in: [reply]), .happy)
        reply.tone = "error"
        XCTAssertEqual(BotMood.completion(for: "turn", in: [reply]), .dizzy)
        XCTAssertNil(BotMood.completion(for: nil, in: [reply]))
        XCTAssertNil(BotMood.completion(for: "another-chat-turn", in: [reply]))
        XCTAssertNil(BotMood.completion(for: "turn", in: []))
        reply.turnTerminal = false
        XCTAssertNil(BotMood.completion(for: "turn", in: [reply]))
    }

}
