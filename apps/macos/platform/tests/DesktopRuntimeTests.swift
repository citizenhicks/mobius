import AppKit
import ApplicationServices
import Testing

@testable import MobiusNative

@Suite(.serialized)
@MainActor
struct DesktopRuntimeTests {
    private func isolatedDefaults() -> UserDefaults {
        UserDefaults(suiteName: "desktop-runtime-tests-\(UUID().uuidString)")!
    }

    @Test(arguments: [true, false])
    func permissionsAreRequestedOneAtATimeAndControlResumesAfterRevocation(
        revokeAccessibility: Bool
    ) throws {
        let defaults = isolatedDefaults()
        var permissions = (accessibility: false, screenRecording: false)
        var prompts: [(DesktopPermission, Bool)] = []
        let runtime = DesktopRuntime(
            defaults: defaults, readPermissions: { permissions },
            present: { prompts.append(($0, $1)) })
        var messages: [GatewayRequest] = []
        runtime.connected { messages.append($0) }
        defer { runtime.disconnected() }
        runtime.enabled = true
        #expect(prompts.map(\.0) == [.accessibility] && prompts[0].1)
        #expect(
            runtime.reason?.contains("Waiting for \(DesktopPermission.accessibility.name)") == true
                && messages.isEmpty)
        permissions.accessibility = true
        runtime.refreshPermissions()
        #expect(prompts.map(\.0) == [.accessibility, .screenRecording] && messages.isEmpty)
        #expect(runtime.reason?.contains("quit and reopen") == true)
        permissions.screenRecording = true
        runtime.refreshPermissions()
        #expect(messages.count == 1 && messages.last?.body["enabled"] == .bool(true))
        try runtime.receive(registrationReply("accepted", to: messages[0]))
        #expect(runtime.isRegistered && runtime.reason == nil)
        try runtime.receive(desktopRequest())
        runtime.screenshots["screen"] = desktopScreenshot()
        if revokeAccessibility {
            permissions.accessibility = false
        } else {
            permissions.screenRecording = false
        }
        runtime.refreshPermissions()
        #expect(runtime.enabled && !runtime.isRegistered && !runtime.isActive)
        #expect(runtime.screenshots.isEmpty)
        #expect(messages.count == 2 && messages.last?.body["enabled"] == .bool(false))
        permissions = (true, true)
        runtime.refreshPermissions()
        #expect(messages.count == 3 && messages.last?.body["enabled"] == .bool(true))
        runtime.request(.accessibility)
        #expect(prompts.last.map { $0.0 == .accessibility && !$0.1 } == true)
        let relaunched = DesktopRuntime(
            defaults: defaults, readPermissions: { permissions }, present: { _, _ in })
        #expect(relaunched.enabled && relaunched.wasRequested(.screenRecording))
    }

    @Test func registrationRejectAndStaleRepliesDoNotRepublishOrClaimReadiness() throws {
        let runtime = DesktopRuntime(
            defaults: isolatedDefaults(), readPermissions: { (true, true) }, present: { _, _ in })
        var messages: [GatewayRequest] = []
        runtime.enabled = true
        runtime.connected { messages.append($0) }
        defer { runtime.disconnected() }
        #expect(messages.count == 1)
        try runtime.receive(registrationReply("rejected", to: messages[0]))
        runtime.refreshPermissions()
        #expect(runtime.enabled && !runtime.isRegistered && messages.count == 1)
        #expect(runtime.reason == "Rejected")
        try runtime.receive(registrationReply("accepted", to: messages[0]))
        #expect(!runtime.isRegistered)
        runtime.enabled = false
        runtime.enabled = true
        #expect(messages.count == 2)
        runtime.disconnected()
        runtime.connected { messages.append($0) }
        #expect(messages.count == 3)
        #expect(
            DesktopRuntime.rejectionReason("only the local Mac app can host desktop control")
                == "Computer control works only with the gateway running on this Mac.")
    }

    private func registrationReply(_ type: String, to request: GatewayRequest) throws
        -> GatewayEnvelope
    {
        try GatewayRequest(
            type,
            ["requestId": try #require(request.body["requestId"]), "message": .string("Rejected")]
        ).body.decode(GatewayEnvelope.self)
    }

    @Test func appsAreAskedOnceAndSystemAppsAreNeverControlled() async throws {
        var asked: [String] = []
        let runtime = DesktopRuntime(
            defaults: isolatedDefaults(), readPermissions: { (true, true) }, present: { _, _ in },
            askToUse: { name in
                asked.append(name)
                switch name {
                case "Safari": return .once
                case "kitty": return .always
                default: return .deny
                }
            })
        try await runtime.authorize(bundle: "com.apple.Safari", name: "Safari")
        try await runtime.authorize(bundle: "com.apple.Safari", name: "Safari")
        #expect(asked == ["Safari"], "once lasts for the chat")
        try await runtime.authorize(bundle: "net.kovidgoyal.kitty", name: "kitty")
        await #expect(throws: DesktopError.self) {
            try await runtime.authorize(bundle: "com.apple.mail", name: "Mail")
        }
        await #expect(throws: DesktopError.self) {
            try await runtime.authorize(bundle: "com.apple.mail", name: "Mail")
        }
        await #expect(throws: DesktopError.self) {
            try await runtime.authorize(bundle: "com.apple.systempreferences", name: "Settings")
        }
        #expect(asked == ["Safari", "kitty", "Mail"])
        #expect(runtime.appChoiceSummary == "Always: kitty · Not allowed: Mail")
        runtime.forgetAppChoices()
        #expect(runtime.appChoiceSummary == nil)
    }

    @Test(
        .enabled("Requires an available GUI app") {
            await DesktopRuntimeTests.runningCaptureApp() != nil
        }, arguments: [DesktopAppChoice.once, .always, .deny])
    func canceledAppConsentCannotChangeTheNextChatsChoicesOrCaptureTarget(
        choice: DesktopAppChoice
    ) async throws {
        let app = try #require(Self.runningCaptureApp())
        var pending: CheckedContinuation<DesktopAppChoice, Never>?
        let runtime = DesktopRuntime(
            defaults: isolatedDefaults(), readPermissions: { (true, true) }, present: { _, _ in },
            askToUse: { _ in await withCheckedContinuation { pending = $0 } })
        let action = Task {
            try await runtime.perform(
                .object([
                    "action": .string("activate"),
                    "pid": .integer(Int64(app.processIdentifier)),
                ]))
        }
        for _ in 0..<100 where pending == nil { await Task.yield() }
        let answer = try #require(pending)
        action.cancel()
        runtime.onceApps = ["next-chat"]
        runtime.lastActionTarget = (app, 123)
        answer.resume(returning: choice)
        await #expect(throws: CancellationError.self) { _ = try await action.value }
        #expect(runtime.onceApps == ["next-chat"] && runtime.appChoices.isEmpty)
        #expect(runtime.lastActionTarget?.app === app && runtime.lastActionTarget?.displayID == 123)
    }

    @Test func sessionEndingShortcutsAreRefused() throws {
        #expect(throws: DesktopError.self) {
            try DesktopRuntime.refuseSystemShortcut(key: "q", flags: [.maskCommand, .maskShift])
        }
        #expect(throws: DesktopError.self) {
            try DesktopRuntime.refuseSystemShortcut(key: "q", flags: [.maskCommand, .maskControl])
        }
        #expect(throws: DesktopError.self) {
            try DesktopRuntime.refuseSystemShortcut(
                key: "escape", flags: [.maskCommand, .maskAlternate])
        }
        try DesktopRuntime.refuseSystemShortcut(key: "q", flags: [.maskCommand])
        try DesktopRuntime.refuseSystemShortcut(key: "l", flags: [.maskCommand])
    }

    @Test func screenshotTargetsRejectInvalidScopeWithoutFallingBackToDisplay() async throws {
        let runtime = DesktopRuntime(
            defaults: isolatedDefaults(), readPermissions: { (true, true) }, present: { _, _ in })
        for fields: [String: JSONValue] in [
            ["pid": .null], ["pid": .integer(0)], ["pid": .string("123")],
            ["pid": .integer(Int64(ProcessInfo.processInfo.processIdentifier))],
            ["pid": .integer(Int64.max)], ["pid": .number(1.5)],
            ["afterAction": .bool(true)], ["afterAction": .bool(false)],
            ["afterAction": .bool(true), "pid": .integer(1)],
            ["afterAction": .bool(true), "displayId": .integer(1)],
            ["displayId": .null], ["displayId": .integer(-1)], ["unknown": .bool(true)],
        ] {
            await #expect(throws: DesktopError.self) {
                _ = try await runtime.screenshotTarget(.object(fields))
            }
        }
        let display = try await runtime.screenshotTarget(.object([:]))
        #expect(display.app == nil && display.displayID == CGMainDisplayID())
        let otherDisplay = try await runtime.screenshotTarget(.object(["displayId": .integer(123)]))
        #expect(otherDisplay.app == nil && otherDisplay.displayID == 123)
    }

    private static func runningCaptureApp() -> NSRunningApplication? {
        NSWorkspace.shared.runningApplications.first {
            $0.activationPolicy == .regular
                && $0.processIdentifier != ProcessInfo.processInfo.processIdentifier
                && !DesktopRuntime.neverControlled.contains($0.bundleIdentifier ?? "")
                && $0.bundleIdentifier != Bundle.main.bundleIdentifier
        }
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
            response.body["response"]?["error"]?.stringValue?.contains("isn’t set up on this Mac")
                == true
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
            runtime.showCursor()
            runtime.moveCursor(to: CGDisplayBounds(CGMainDisplayID()).origin)
            #expect(runtime.isCursorVisible)
            if disconnect { runtime.disconnected() } else { runtime.stop() }
            #expect(!runtime.isActive)
            #expect(runtime.screenshots.isEmpty && runtime.elements.isEmpty)
            #expect(!runtime.isCursorVisible)
            await Task.yield()
        }
        #expect(!messages.contains { $0.body["type"]?.stringValue == "desktop_control_reply" })
    }

    private func desktopScreenshot() -> DesktopRuntime.Screenshot {
        .init(
            displayID: CGMainDisplayID(), frame: CGRect(x: -1600, y: 40, width: 1600, height: 900),
            width: 3200, height: 1800, frontmostPID: nil, captured: .now)
    }

    private func desktopRequest() throws -> GatewayEnvelope
    {
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
