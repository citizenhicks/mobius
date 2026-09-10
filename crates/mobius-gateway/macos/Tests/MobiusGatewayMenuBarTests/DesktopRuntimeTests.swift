import AppKit
import ApplicationServices
import Testing

@testable import MobiusGatewayMenuBar

@Suite(.serialized)
@MainActor
struct DesktopRuntimeTests {
    @Test func textInputPreservesUnicodeAcrossEventLimitsAndEmptyInput() throws {
        #expect(try DesktopRuntime.textChunks("").isEmpty)
        #expect(throws: DesktopError.self) { try DesktopRuntime.textChunks("line\nbreak") }
        let text = String(repeating: "a", count: 19) + "🦋möbius" + String(repeating: "x", count: 40)
        let chunks = try DesktopRuntime.textChunks(text)
        #expect(chunks.allSatisfy { !$0.isEmpty && $0.count <= 20 })
        #expect(chunks.map { String(decoding: $0, as: UTF16.self) }.joined() == text)
    }

    @Test func desktopCoordinatesMapImagePixelsToOffsetDisplay() throws {
        let screenshot = desktopScreenshot()
        #expect(try DesktopRuntime.screenPoint(screenshot, x: 0, y: 0) == CGPoint(x: -1600, y: 40))
        #expect(
            try DesktopRuntime.screenPoint(screenshot, x: 1600, y: 900) == CGPoint(x: -800, y: 490))
        for (x, y) in [(-1.0, 0.0), (3200, 0), (0, 1800), (.infinity, 0), (0, .nan)] {
            #expect(throws: DesktopError.self) {
                try DesktopRuntime.screenPoint(screenshot, x: x, y: y)
            }
        }
    }

    @Test func desktopCoordinatesRejectExpiredAndChangedDisplayObservations() {
        let runtime = DesktopRuntime()
        let request: JSONValue = .object([
            "screenshotId": .string("screen"), "x": .number(1), "y": .number(1),
        ])
        runtime.screenshots["screen"] = desktopScreenshot(
            captured: .now.advanced(by: .seconds(-61)))
        #expect(throws: DesktopError.self) { try runtime.observedPoint(request) }
        runtime.screenshots["screen"] = desktopScreenshot(frame: .zero)
        #expect(throws: DesktopError.self) { try runtime.observedPoint(request) }
        runtime.clearObservations()
        #expect(throws: DesktopError.self) { try runtime.observedPoint(request) }
    }

    @Test func disabledDesktopRequestRepliesWithoutPerformingInput() async throws {
        let runtime = DesktopRuntime()
        var messages: [GatewayRequest] = []
        runtime.connected { messages.append($0) }
        #expect(messages.isEmpty)
        try runtime.receive(desktopRequest())
        for _ in 0..<100
        where !messages.contains(where: { $0.body["type"]?.stringValue == "desktop_control_reply" })
        {
            try await Task.sleep(for: .milliseconds(1))
        }
        let response = try #require(
            messages.first { $0.body["type"]?.stringValue == "desktop_control_reply" })
        #expect(
            response.body["response"]?["error"]?.stringValue?.contains("Enable Mac control") == true
        )
        #expect(response.body["response"]?["result"] == nil)
        runtime.stop()
    }

    @Test func desktopStopAndDisconnectCancelPendingRequestsAndClearObservations()
        async throws
    {
        let runtime = DesktopRuntime()
        var messages: [GatewayRequest] = []
        runtime.connected { messages.append($0) }
        for disconnect in [false, true] {
            try runtime.receive(desktopRequest())
            #expect(runtime.isActive)
            runtime.screenshots["screen"] = desktopScreenshot()
            runtime.elements["element"] = .init(
                value: AXUIElementCreateSystemWide(), pid: 0, role: "AXButton", label: "Test")
            if disconnect { runtime.disconnected() } else { runtime.stop() }
            #expect(!runtime.enabled && !runtime.isActive)
            #expect(runtime.screenshots.isEmpty && runtime.elements.isEmpty)
            await Task.yield()
        }
        #expect(!messages.contains { $0.body["type"]?.stringValue == "desktop_control_reply" })
    }

    @Test func inactiveDesktopSessionDisablesControlAndClearsObservations() {
        let runtime = DesktopRuntime()
        for name in [
            NSWorkspace.sessionDidResignActiveNotification, NSWorkspace.screensDidSleepNotification,
        ] {
            runtime.enabled = true
            runtime.screenshots["screen"] = desktopScreenshot()
            NSWorkspace.shared.notificationCenter.post(name: name, object: nil)
            #expect(!runtime.enabled && !runtime.isActive && runtime.screenshots.isEmpty)
        }
    }

    private func desktopScreenshot(
        frame: CGRect = CGRect(x: -1600, y: 40, width: 1600, height: 900),
        captured: ContinuousClock.Instant = .now
    ) -> DesktopRuntime.Screenshot {
        .init(
            displayID: CGMainDisplayID(), frame: frame, width: 3200, height: 1800,
            frontmostPID: nil, captured: captured)
    }

    private func desktopRequest() throws -> GatewayEnvelope {
        try GatewayRequest(
            "desktop_control_requested",
            [
                "requestId": .string("request"), "executionId": .string("execution"),
                "sessionId": .string("session"),
                "request": .object([
                    "action": .string("press_key"), "pid": .integer(0), "key": .string("a"),
                    "modifiers": .array([]),
                ]),
            ]
        ).body.decode(GatewayEnvelope.self)
    }

}
