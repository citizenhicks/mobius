import Testing

@testable import MobiusNative

@MainActor @Test func disconnectDiscardsRecapsQueuedForThePreviousGateway() {
    let bridge = NativeBridge.shared
    _ = bridge.drain()
    bridge.emit(.object(["type": .string("recap"), "request_id": .string("old-chat")]))
    bridge.connected(false)
    #expect(!bridge.drain().contains("old-chat"))
}
