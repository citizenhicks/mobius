import AppKit
import ApplicationServices
import Testing

@testable import MobiusGatewayMenuBar

@Suite(.serialized)
@MainActor
struct DesktopRuntimeTests {
    @Test(arguments: [true, false])
    func permissionGrantsRegisterAndRevocationStopsWithoutReenabling(revokeAccessibility: Bool)
        throws
    {
        var permissions = (accessibility: false, screenRecording: false)
        let runtime = DesktopRuntime { permissions }
        var messages: [GatewayRequest] = []
        runtime.connected { messages.append($0) }
        defer { runtime.disconnected() }
        runtime.enabled = true
        runtime.refreshPermissions()
        #expect(runtime.enabled && messages.isEmpty)
        permissions.accessibility = true
        runtime.refreshPermissions()
        #expect(runtime.hasAccessibility && !runtime.hasPermissions && messages.isEmpty)
        permissions.accessibility = false
        runtime.refreshPermissions()
        #expect(!runtime.enabled && messages.isEmpty)
        runtime.enabled = true
        #expect(runtime.message == nil)
        permissions.accessibility = true
        permissions.screenRecording = true
        runtime.refreshPermissions()
        #expect(messages.count == 1 && !runtime.isRegistered)
        #expect(messages.last?.body["enabled"] == .bool(true))
        try runtime.receive(registrationReply("accepted", to: messages[0]))
        #expect(runtime.isRegistered)
        runtime.refreshPermissions()
        #expect(messages.count == 1)
        try runtime.receive(desktopRequest())
        runtime.screenshots["screen"] = desktopScreenshot()
        if revokeAccessibility {
            permissions.accessibility = false
        } else {
            permissions.screenRecording = false
        }
        runtime.refreshPermissions()
        #expect(!runtime.enabled && !runtime.isRegistered && !runtime.isActive)
        #expect(runtime.screenshots.isEmpty)
        #expect(messages.count == 2 && messages.last?.body["enabled"] == .bool(false))
        permissions = (true, true)
        runtime.refreshPermissions()
        #expect(!runtime.enabled && messages.count == 2)
        runtime.enabled = true
        runtime.stop()
        runtime.refreshPermissions()
        #expect(!runtime.enabled && messages.count == 4)
    }

    @Test func registrationRejectAndStaleRepliesDoNotRepublishOrClaimReadiness() throws {
        let runtime = DesktopRuntime { (true, true) }
        var messages: [GatewayRequest] = []
        runtime.refreshPermissions()
        runtime.enabled = true
        runtime.connected { messages.append($0) }
        defer { runtime.disconnected() }
        #expect(messages.count == 1)
        try runtime.receive(registrationReply("rejected", to: messages[0]))
        runtime.refreshPermissions()
        #expect(!runtime.enabled && !runtime.isRegistered && messages.count == 1)
        try runtime.receive(registrationReply("accepted", to: messages[0]))
        #expect(!runtime.isRegistered)
        runtime.enabled = true
        runtime.stop()
        try runtime.receive(registrationReply("accepted", to: messages[1]))
        #expect(!runtime.isRegistered)
        try runtime.receive(registrationReply("rejected", to: messages[2]))
        runtime.refreshPermissions()
        #expect(messages.count == 3)
        runtime.disconnected()
        runtime.connected { messages.append($0) }
        runtime.enabled = true
        #expect(messages.count == 4)
    }

    @Test func connectedRuntimeMonitorsPermissionsWithoutAnOpenMenu() async throws {
        var granted = false
        let runtime = DesktopRuntime { (granted, granted) }
        var messages: [GatewayRequest] = []
        runtime.connected { messages.append($0) }
        defer { runtime.disconnected() }
        runtime.enabled = true
        granted = true
        for _ in 0..<100 where messages.isEmpty {
            try await Task.sleep(for: .milliseconds(20))
        }
        #expect(runtime.hasPermissions && messages.count == 1)
        granted = false
        for _ in 0..<100 where runtime.enabled {
            try await Task.sleep(for: .milliseconds(20))
        }
        #expect(!runtime.hasPermissions && !runtime.enabled && messages.count == 2)
    }

    private func registrationReply(_ type: String, to request: GatewayRequest) throws
        -> GatewayEnvelope
    {
        try GatewayRequest(
            type,
            ["requestId": try #require(request.body["requestId"]), "message": .string("Rejected")]
        ).body.decode(GatewayEnvelope.self)
    }

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

    @Test func cursorOverlayIsClickThroughAndStopsCleanly() {
        _ = NSApplication.shared
        NSImage(size: NSSize(width: 16, height: 16)).setName("hi.mousePointer01")
        let overlay = DesktopCursorOverlay()
        overlay.prepare(tint: "red")
        #expect(!overlay.isVisible && overlay.windowID != nil)
        overlay.move(to: CGPoint(x: 100, y: 100))
        #expect(overlay.isVisible)
        let panel = overlay.windowID.flatMap { windowID in
            NSApplication.shared.windows.first { $0.windowNumber == windowID }
        }
        #expect(panel?.ignoresMouseEvents == true)
        #expect(panel?.styleMask.contains(.nonactivatingPanel) == true)
        #expect(panel?.sharingType == NSWindow.SharingType.none)
        #expect(panel?.frame.origin == CGPoint(x: 95, y: 69))
        let frame = panel?.frame
        overlay.prepare(tint: "red")
        #expect(overlay.isVisible && panel?.frame == frame)
        overlay.hide()
        #expect(!overlay.isVisible)
    }

    @Test func cursorPointConvertsQuartzTopLeftToAppKitBottomLeft() {
        #expect(
            DesktopRuntime.appKitPoint(
                CGPoint(x: 0, y: 40),
                displayBounds: CGRect(x: -1600, y: 40, width: 1600, height: 900),
                screenFrame: CGRect(x: -1600, y: 0, width: 1600, height: 900))
                == CGPoint(x: 0, y: 900))
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
        _ = NSApplication.shared
        NSImage(size: NSSize(width: 16, height: 16)).setName("hi.mousePointer01")
        let runtime = DesktopRuntime()
        var messages: [GatewayRequest] = []
        runtime.connected { messages.append($0) }
        for disconnect in [false, true] {
            try runtime.receive(desktopRequest())
            #expect(runtime.isActive)
            runtime.screenshots["screen"] = desktopScreenshot()
            runtime.elements["element"] = .init(
                value: AXUIElementCreateSystemWide(), pid: 0, role: "AXButton", label: "Test")
            runtime.showCursor(tint: "red")
            runtime.moveCursor(to: CGDisplayBounds(CGMainDisplayID()).origin)
            #expect(runtime.isCursorVisible)
            if disconnect { runtime.disconnected() } else { runtime.stop() }
            #expect(!runtime.enabled && !runtime.isActive)
            #expect(runtime.screenshots.isEmpty && runtime.elements.isEmpty)
            #expect(!runtime.isCursorVisible)
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
