import AppKit
import Carbon
import Foundation
import Network
import Observation
import Synchronization
import SwiftUI
import Testing
@testable import MobiusGatewayMenuBar

@Test @MainActor func pinnedVoiceKeepsItsCornerWhenControlsExpand() {
    let screen = CGRect(x: -1600, y: 40, width: 1600, height: 900)
    for corner in VoiceCorner.allCases {
        let compact = corner.frame(for: CGSize(width: 416, height: 96), in: screen)
        let expanded = corner.frame(for: CGSize(width: 416, height: 240), in: screen)
        #expect(screen.contains(compact) && screen.contains(expanded))
        #expect(compact.minX == expanded.minX)
        if corner == .topLeft || corner == .topRight {
            #expect(compact.maxY == expanded.maxY)
            #expect(expanded.maxY == screen.maxY - MobiusSpace.l)
        } else {
            #expect(compact.minY == expanded.minY)
            #expect(expanded.minY == screen.minY + MobiusSpace.l)
        }
    }
    let oversized = VoiceCorner.bottomRight.frame(
        for: CGSize(width: 3000, height: 2000), in: screen)
    #expect(screen.contains(oversized))
}

@Test @MainActor func pinnedControlsAndMiniModeKeepTheirCornerAndAllowMenuFocus() async throws {
    _ = NSApplication.shared
    // SwiftPM tests do not carry the app's compiled asset catalog.
    for name in ["aiScan", "folder", "chatCircle", "dotsThree", "mic01", "playFill", "plus"] {
        NSImage(size: NSSize(width: 16, height: 16)).setName("hi.\(name)")
    }
    let model = MenuBarModel()
    let suite = "mobius-mini-test-\(UUID())"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    let presentation = VoicePanelController(model: model, defaults: defaults)
    presentation.pin(to: .bottomRight)
    defer { presentation.unpin() }
    let panel = try #require(
        NSApplication.shared.windows.first { $0.delegate === presentation } as? NSPanel)
    let controls = NSHostingView(
        rootView: Text("Options").frame(height: 44).voiceControlsVisible(false))
    let hiddenSize = controls.fittingSize
    controls.rootView = Text("Options").frame(height: 44).voiceControlsVisible(true)
    #expect(controls.fittingSize == hiddenSize)
    #expect(hiddenSize.height == 44)
    #expect(panel.canBecomeKey && !panel.becomesKeyOnlyIfNeeded)
    try await eventually { panel.frame.width == 416 }
    let expandedFrame = panel.frame
    presentation.isMini = true
    try await eventually { panel.frame.size == CGSize(width: 96, height: 96) }
    #expect(panel.frame.maxX == expandedFrame.maxX && panel.frame.minY == expandedFrame.minY)
    model.message = "Voice needs attention"
    try await eventually { panel.frame.width == 416 }
    model.message = nil
    try await eventually { panel.frame.width == 96 }
    presentation.showControls()
    try await eventually { panel.frame.width == 416 }
    #expect(!presentation.isMini)
    presentation.windowWillClose(
        Notification(name: NSWindow.willCloseNotification, object: NSWindow()))
    #expect(presentation.corner == .bottomRight)
    model.toggleMicrophone()
    model.toggleVoice()
    #expect(!model.voice.isMuted && model.voiceCall == nil)
}

@Test(arguments: [false, true]) @MainActor func audioMeterDoesNotInvalidateVoiceMenus(mini: Bool)
    throws
{
    let model = MenuBarModel()
    let suite = "mobius-meter-test-\(UUID())"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    let presentation = VoicePanelController(model: model, defaults: defaults)
    presentation.isMini = mini
    let menuChanged = Mutex(false)
    withObservationTracking {
        _ = VoiceMenuView(model: model, presentation: presentation, isPinned: true).body
    } onChange: {
        menuChanged.withLock { $0 = true }
    }
    let waveformChanged = Mutex(false)
    withObservationTracking {
        _ = VoiceWaveform(voice: model.voice, playbackColor: .blue).body
    } onChange: {
        waveformChanged.withLock { $0 = true }
    }
    model.voice.updateAudioLevels(RealtimeAudioLevels(microphone: 0.4, playback: 0.2))
    #expect(!menuChanged.withLock { $0 })
    #expect(waveformChanged.withLock { $0 })
}

@Test @MainActor func voiceShortcutsRejectConflictsAndPersistAssignments() throws {
    _ = NSApplication.shared
    let suite = "mobius-shortcuts-test-\(UUID())"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    let shortcuts = VoiceShortcuts(defaults: defaults) { _ in
        Issue.record("Recording triggered a voice control")
    }
    func event(code: Int, character: Int, modifiers: NSEvent.ModifierFlags) throws -> NSEvent {
        let text = String(try #require(UnicodeScalar(character)))
        return try #require(
            NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: modifiers, timestamp: 0,
                windowNumber: 0, context: nil, characters: text, charactersIgnoringModifiers: text,
                isARepeat: false, keyCode: UInt16(code)))
    }
    let modifiers: NSEvent.ModifierFlags = [.command, .control, .option]
    let combined = try VoiceShortcut(
        event: event(code: kVK_ANSI_K, character: 107, modifiers: modifiers))
    #expect(combined.label == "⌃⌥⌘K")
    #expect(combined.modifiers == UInt32(controlKey | optionKey | cmdKey))
    let first = try VoiceShortcut(
        event: event(code: kVK_F18, character: NSF18FunctionKey, modifiers: modifiers))
    let second = try VoiceShortcut(
        event: event(code: kVK_F19, character: NSF19FunctionKey, modifiers: modifiers))
    #expect(first.label == "⌃⌥⌘F18")
    #expect(throws: VoiceShortcutError.self) {
        try VoiceShortcut(event: event(code: kVK_ANSI_V, character: 118, modifiers: []))
    }
    try shortcuts.set(first, for: .call)
    #expect(throws: VoiceShortcutError.self) { try shortcuts.set(first, for: .microphone) }
    #expect(shortcuts.shortcut(for: .call) == first && shortcuts.shortcut(for: .microphone) == nil)
    var blocker: EventHotKeyRef?
    #expect(
        RegisterEventHotKey(
            second.keyCode, second.modifiers, EventHotKeyID(signature: 0x74657374, id: 1),
            GetApplicationEventTarget(), OptionBits(kEventHotKeyExclusive), &blocker) == noErr)
    defer { if let blocker { UnregisterEventHotKey(blocker) } }
    #expect(throws: VoiceShortcutError.self) { try shortcuts.set(second, for: .call) }
    #expect(shortcuts.shortcut(for: .call) == first)
    shortcuts.beginRecording(.call)
    let restored = VoiceShortcuts(defaults: defaults) { _ in Issue.record("Unexpected shortcut") }
    #expect(restored.shortcut(for: .call) == first && restored.error == nil)
    try restored.set(nil, for: .call)
    #expect(restored.shortcut(for: .call) == nil)
    shortcuts.cancelRecording()
    #expect(shortcuts.recording == nil && shortcuts.error == nil)
}

@Test @MainActor func voiceUsesTheAudioEngineBackendWithoutStartingHardware() throws {
    let factory = try #require(RealtimeVoiceSession.factory)
    let device = factory.audioDeviceModule
    #expect(!device.isPlaying && !device.isRecording)
    // Manual rendering is supported by AudioEngine, not the old macOS device module.
    #expect(device.setManualRenderingMode(true) == 0)
    defer { _ = device.setManualRenderingMode(false) }
    #expect(device.isManualRenderingMode)
    #expect(!device.isEngineRunning)
}

@Test @MainActor func menuIconColorsSurviveControlTintAndAppearance() throws {
    let name = "menu-color-test-\(UUID())"
    let source = NSImage(size: NSSize(width: 8, height: 8), flipped: false) { rect in
        NSColor.white.setFill()
        rect.fill()
        return true
    }
    source.setName("hi.\(name)")
    for scheme in [ColorScheme.light, .dark] {
        let appearance = try #require(NSAppearance(named: scheme == .dark ? .darkAqua : .aqua))
        for tint in [Color(red: 1, green: 0, blue: 0), .primary] {
            var rendered: CGImage?
            var reference: CGImage?
            appearance.performAsCurrentDrawingAppearance {
                let renderer = ImageRenderer(
                    content: VoiceIcon.menuImage(name, color: tint, size: 8, scheme: scheme)
                        .foregroundStyle(.blue).tint(.blue)
                        .environment(\.colorScheme, scheme)
                )
                rendered = renderer.cgImage
                reference =
                    ImageRenderer(
                        content: VoiceIcon(name, size: 8).foregroundStyle(tint)
                            .environment(\.colorScheme, scheme)
                    ).cgImage
            }
            let bitmap = NSBitmapImageRep(cgImage: try #require(rendered))
            let referenceBitmap = NSBitmapImageRep(cgImage: try #require(reference))
            let color = try #require(bitmap.colorAt(x: 4, y: 4)?.usingColorSpace(.sRGB))
            let expected = try #require(referenceBitmap.colorAt(x: 4, y: 4)?.usingColorSpace(.sRGB))
            #expect(abs(color.redComponent - expected.redComponent) < 0.02)
            #expect(abs(color.greenComponent - expected.greenComponent) < 0.02)
            #expect(abs(color.blueComponent - expected.blueComponent) < 0.02)
        }
    }
}

@Test func localHandoffRejectsRemoteEndpointsAndVersionMismatch() throws {
    for endpoint in [
        "tcp://example.com:8741", "tcp://127.0.0.1:8741/path", "tcp://user@127.0.0.1:8741",
    ] {
        #expect(throws: (any Error).self) { try GatewayEndpoint(endpoint) }
    }
    #expect(try GatewayEndpoint("tcp://[::1]:8741").host == "::1")
    #expect(throws: (any Error).self) {
        try LocalGatewayConnection(
            data: Data(
                "{\"endpoint\":\"tcp://127.0.0.1:8741\",\"token\":\"test\",\"protocol_version\":0}"
                    .utf8))
    }
}

@Test @MainActor func chatsAndApprovalsUseTheSelectedSessionOverTheRealTransport() async throws {
    let fixture = try GatewayFixture()
    try await fixture.start()
    let suite = "mobius-menu-test-\(UUID())"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite); fixture.close() }
    let model = MenuBarModel(defaults: defaults)
    let connection = try fixture.handoff()
    model.connect { connection }
    try await eventually { model.chatIsReady }
    #expect(model.selectedChatID == "one")
    #expect(model.canStartVoice)
    #expect(model.chats.count == 4)  // Voice/subagent child sessions stay out of the picker.
    #expect(model.selectedBot?.handle == "builder")
    // Same folder names keep distinct workspace identities.
    #expect(model.workspacePaths.count == 2)
    #expect(model.workspaceName == "möbius")
    #expect(model.chatGroups.map(\.workspace) == ["/work/one/möbius", "/work/two/möbius"])
    #expect(model.chatGroups.map { $0.chats.map(\.id) } == [["one", "older"], ["other"]])
    model.beginNewChat(botID: "writer")
    #expect(model.selectedChatID == nil)
    #expect(model.selectedBot?.id == "writer")
    #expect(model.workspacePath == "/work/one/möbius")
    #expect(model.chatGroups.flatMap(\.chats).map(\.id) == ["two"])
    model.beginNewChat(workspace: "/work/two/möbius")
    #expect(model.chatGroups.flatMap(\.chats).map(\.id) == ["two"])
    #expect(model.canStartVoice)
    model.beginNewChat(botID: "missing")
    #expect(model.selectedBot?.id == "writer")
    #expect(!fixture.requests.contains { $0["type"]?.stringValue == "create_session" })
    let firstOpen = try #require(
        fixture.requests.first { $0["type"]?.stringValue == "open_session" })
    let second = try #require(model.chats.first { $0.id == "two" })
    model.openChat(second)
    #expect(!model.chatIsReady)
    let loadingChanged = Mutex(false)
    withObservationTracking {
        _ = model.status
    } onChange: {
        loadingChanged.withLock { $0 = true }
    }
    try await eventually { model.chatIsReady && model.selectedChatID == "two" }
    #expect(loadingChanged.withLock { $0 })
    fixture.opened(firstOpen)
    fixture.send(
        "agent_event",
        [
            "sessionId": .string("one"), "record": GatewayFixture.approvalRecord(id: "stale"),
        ])
    fixture.send(
        "agent_event",
        [
            "sessionId": .string("two"), "record": GatewayFixture.approvalRecord(id: "review"),
        ])
    try await eventually { model.approval?.id == "review" }
    #expect(model.selectedChatID == "two")
    fixture.send(
        "realtime_voice_started",
        [
            "sessionId": .string("one"), "requestId": .string("cancelled-call"),
            "voiceId": .string("late-voice"), "answerSdp": .string("unused"),
        ])
    try await eventually {
        fixture.requests.contains { $0["type"]?.stringValue == "end_realtime_voice" }
    }
    let ended = try #require(
        fixture.requests.last { $0["type"]?.stringValue == "end_realtime_voice" })
    #expect(ended["voiceId"]?.stringValue == "late-voice")
    #expect(model.voiceCall == nil)
    #expect(model.approval?.calls.first?.arguments.contains("echo hello") == true)
    model.resolveApproval(approve: true)
    try await eventually { fixture.requests.contains { $0["type"]?.stringValue == "submit" } }
    let submitted = try #require(fixture.requests.last { $0["type"]?.stringValue == "submit" })
    #expect(submitted["sessionId"]?.stringValue == "two")
    #expect(submitted["submission"]?["op"]?["id"]?.stringValue == "review")
    #expect(submitted["submission"]?["op"]?["decision"]?.stringValue == "approved")
    try await eventually { model.approval == nil }
    model.voice.updateAudioLevels(RealtimeAudioLevels(microphone: 0.4, playback: 0.8))
    model.voice.isMuted = true
    #expect(model.voice.audioLevels.microphone == 0)
    #expect(model.voice.audioLevels.playback == 0.8)
    await model.shutdown()
    #expect(model.voice.audioLevels == RealtimeAudioLevels())
    #expect(!model.isReady)
}

@Test @MainActor func disconnectStopsMediaAndDisablesApproval() async throws {
    let fixture = try GatewayFixture()
    try await fixture.start()
    let suite = "mobius-menu-disconnect-\(UUID())"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite); fixture.close() }
    let model = MenuBarModel(defaults: defaults)
    let connection = try fixture.handoff()
    model.connect { connection }
    try await eventually { model.chatIsReady }
    model.voice.updateAudioLevels(RealtimeAudioLevels(microphone: 0.4, playback: 0.8))
    fixture.close()
    try await eventually { !model.isReady }
    #expect(model.voice.audioLevels == RealtimeAudioLevels())
    #expect(!model.canStartVoice)
    #expect(model.message != nil)
    await model.shutdown()
}

@MainActor
private func eventually(_ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now + .seconds(5)
    while !condition() {
        guard ContinuousClock.now < deadline else {
            throw GatewayWireError.invalidFrame("Timed out waiting for gateway state.")
        }
        try await Task.sleep(for: .milliseconds(10))
    }
}

@MainActor
private final class GatewayFixture {
    let listener: NWListener
    var connection: NWConnection?
    var requests: [JSONValue] = []
    private var ready: CheckedContinuation<Void, Error>?
    private var task: Task<Void, Never>?

    init() throws { listener = try NWListener(using: .tcp, on: .any) }

    func start() async throws {
        try await withCheckedThrowingContinuation { continuation in
            ready = continuation
            listener.stateUpdateHandler = { [weak self] state in
                Task { @MainActor in
                    guard let self, let ready = self.ready else { return }
                    switch state {
                    case .ready: self.ready = nil; ready.resume()
                    case .failed(let error): self.ready = nil; ready.resume(throwing: error)
                    default: break
                    }
                }
            }
            listener.newConnectionHandler = { [weak self] connection in
                Task { @MainActor in self?.accept(connection) }
            }
            listener.start(queue: .main)
        }
    }

    func handoff() throws -> LocalGatewayConnection {
        let port = try #require(listener.port?.rawValue)
        let data = try JSONSerialization.data(withJSONObject: [
            "endpoint": "tcp://127.0.0.1:\(port)", "token": "fixture-only",
            "protocol_version": gatewayProtocolVersion,
        ])
        return try LocalGatewayConnection(data: data)
    }

    func close() { connection?.cancel(); listener.cancel(); task?.cancel() }

    func send(_ type: String, _ fields: [String: JSONValue] = [:]) {
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        guard let data = try? encoder.encode(GatewayRequest(type, fields)) else { return }
        var length = UInt32(data.count).bigEndian
        var frame = withUnsafeBytes(of: &length) { Data($0) }
        frame.append(data)
        connection?.send(content: frame, completion: .contentProcessed { _ in })
    }

    func opened(_ request: JSONValue) {
        let id = request["sessionId"] ?? .string("two")
        let requestID = request["requestId"] ?? .string("missing")
        send(
            "session_opened",
            [
                "requestId": requestID,
                "payload": .object([
                    "session": .object([
                        "sessionId": id, "model": .object(["route": .string("voice-route")]),
                    ])
                ]),
            ])
        send("session_replay_complete", ["requestId": requestID, "sessionId": id])
    }

    static func approvalRecord(id: String) -> JSONValue {
        .object([
            "event": .object([
                "msg": .object([
                    "type": .string("exec_approval_request"), "id": .string(id),
                    "turnId": .string("turn"), "reason": .string("Run a command"),
                    "calls": .array([
                        .object([
                            "callId": .string("call"), "name": .string("exec_command"),
                            "arguments": .object(["cmd": .string("echo hello")]),
                        ])
                    ]),
                ])
            ])
        ])
    }

    private func accept(_ connection: NWConnection) {
        self.connection = connection
        connection.start(queue: .main)
        task = Task { [weak self] in
            do {
                while let self, !Task.isCancelled {
                    let prefix = try await self.read(4)
                    let size = prefix.reduce(0) { ($0 << 8) | Int($1) }
                    guard size > 0, size < 8192 else { throw GatewayWireError.oversizedFrame(size) }
                    let decoder = JSONDecoder()
                    decoder.keyDecodingStrategy = .convertFromSnakeCase
                    let request = try decoder.decode(JSONValue.self, from: await self.read(size))
                    self.requests.append(request)
                    self.respond(request)
                }
            } catch { self?.connection?.cancel() }
        }
    }

    private func read(_ size: Int) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            connection?.receive(minimumIncompleteLength: size, maximumLength: size) {
                data, _, _, error in
                if let data, data.count == size {
                    continuation.resume(returning: data)
                } else {
                    continuation.resume(throwing: error ?? GatewayWireError.disconnected)
                }
            }
        }
    }

    private func respond(_ request: JSONValue) {
        switch request["type"]?.stringValue {
        case "authenticate":
            send("authenticated")
            send(
                "ready",
                [
                    "payload": .object([
                        "sessions": .array(Self.chats),
                        "bots": .array([
                            .object([
                                "id": .string("bot"), "name": .string("Builder"),
                                "handle": .string("builder"),
                                "description": .string("Builds software"),
                                "tint": .string("blue"),
                            ]),
                            .object([
                                "id": .string("writer"), "name": .string("Writer"),
                                "handle": .string("writer"),
                                "description": .string("Writes release notes"),
                                "tint": .string("green"),
                            ]),
                        ]),
                        "models": .array([
                            .object([
                                "route": .string("voice-route"),
                                "supportsRealtimeVoice": .bool(true),
                            ])
                        ]),
                    ])
                ])
        case "open_session": opened(request)
        case "list_sessions": send("sessions", ["sessions": .array(Self.chats)])
        case "submit": send("accepted", ["requestId": request["submission"]?["id"] ?? .null])
        default: break
        }
    }

    private static var chats: [JSONValue] {
        ["one", "two", "child", "other", "older"].enumerated().map { index, id in
            let workspace =
                switch id {
                case "other": "two"
                case "older": "one"
                default: id
                }
            return .object([
                "sessionId": .string(id),
                "title": .string(id == "one" ? "Computer use" : "Release notes"),
                "parentSessionId": id == "child" ? .string("one") : .null,
                "updatedAt": .integer(Int64(10 - index)),
                "sessionContext": .object([
                    "botId": .string(id == "two" ? "writer" : "bot"), "workspaceId": .string(id),
                    "workspaceLabel": .string(
                        "/work/\(workspace)/möbius"),
                ]),
                "activity": .object(["state": .string("idle")]),
            ])
        }
    }
}
