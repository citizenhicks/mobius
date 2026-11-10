import Testing

@testable import MobiusNative

@MainActor
struct NativeDictationTests {
    @Test func startingDoesNotQueueAStopThatDiscardsTheNewTranscript() async {
        var events: [JSONValue] = []
        let dictation = NativeDictation { events.append($0) }
        dictation.start()
        #expect(events == [.object(["type": .string("dictation_state"), "active": .bool(true)])])
        // Stop before the permission task runs: this test never requests microphone access.
        dictation.stop()
        await Task.yield()
        #expect(events.count == 2)
        #expect(events.last?["active"] == .bool(false))
    }
}
