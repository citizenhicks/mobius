import Foundation
import Testing

@testable import MobiusNative

@Suite(.serialized)
@MainActor
struct NativeVoiceTests {
    @Test func anEmptyPollKeepsTheOwnedJSONArrayABI() throws {
        _ = NativeBridge.shared.drain()
        let pointer = try #require(nativePoll())
        defer { nativeFree(pointer) }
        #expect(String(cString: pointer) == "[]")
    }

    @Test func disconnectInvalidatesQueuedRequestsAndPendingTitles() throws {
        let bridge = NativeBridge.shared
        bridge.connected(true)
        let generation = bridge.connectionGeneration
        bridge.emit(.object(["type": .string("gateway_request"), "request": .object([:])]))
        bridge.emit(.object(["type": .string("title"), "title": .string("Old title")]))
        bridge.connected(false)
        bridge.emit(.object(["type": .string("gateway_request"), "request": .object([:])]))
        let events = try JSONDecoder().decode([JSONValue].self, from: Data(bridge.drain().utf8))
        #expect(bridge.connectionGeneration != generation)
        #expect(
            !events.contains {
                ["gateway_request", "title"].contains($0["type"]?.stringValue ?? "")
            })
    }

    @Test func staleVoiceStartClosesTheProviderCallWithoutOpeningAudio() throws {
        var events: [JSONValue] = []
        let voice = NativeVoice { events.append($0) }
        try voice.receive(
            GatewayRequest(
                "realtime_voice_started",
                [
                    "sessionId": .string("previous-chat"), "requestId": .string("old-request"),
                    "voiceId": .string("provider-call"), "answerSdp": .string("unused"),
                ]
            ).body.decode(GatewayEnvelope.self))
        #expect(events.count == 1)
        #expect(events.first?["request"]?["type"] == .string("end_realtime_voice"))
        #expect(events.first?["request"]?["voiceId"] == .string("provider-call"))
        #expect(voice.status["active"] == .bool(false))
    }
}
