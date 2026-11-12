import AppKit
import Foundation
import CoreText
import UserNotifications

@_cdecl("mobius_native_configure_text")
public func nativeConfigureText() {
    // Keep normal font weights: GPUI otherwise adds CoreGraphics stroke dilation.
    // Registration is process-local and respects explicit user preferences.
    UserDefaults.standard.register(defaults: ["AppleFontSmoothing": 0])
    if let folder = Bundle.main.resourceURL?.appendingPathComponent("fonts"),
        let files = FileManager.default.enumerator(at: folder, includingPropertiesForKeys: nil)
    {
        for case let url as URL in files where ["ttf", "otf"].contains(url.pathExtension) {
            CTFontManagerRegisterFontsForURL(url as CFURL, .process, nil)
        }
    }
}

/// Native events for Rust.
extension JSONValue {
    /// Shown as a toast.
    static func error(_ message: String) -> JSONValue {
        .object(["type": .string("error"), "message": .string(message)])
    }

    /// Asks Rust to render again.
    static let statusChanged = JSONValue.object(["type": .string("status_changed")])
}

/// The gateway still owns all agent behavior. This bridge only executes native capabilities.
@MainActor
final class NativeBridge: NSObject, UNUserNotificationCenterDelegate {
    static let shared = NativeBridge()
    let desktop = DesktopRuntime()
    lazy var panel = NativePanel(emit: { [weak self] in self?.emit($0) })
    private var events: [JSONValue] = []
    var wake: (@convention(c) () -> Void)?
    private var notificationTag: UInt8 = 0
    private(set) var isConnected = false
    private(set) var connectionGeneration: UInt64 = 0
    lazy var voice = NativeVoice(emit: { [weak self] in self?.emit($0) })
    lazy var dictation = NativeDictation(emit: { [weak self] in self?.emit($0) })
    lazy var cloud = CloudConfig.current.map { config in
        NativeCloud(config: config, emit: { [weak self] in self?.emit($0) })
    }
    lazy var updates = NativeUpdates.live(emit: { [weak self] in self?.emit($0) })

    override private init() {
        super.init()
        if NSImage(named: "hi.mousePointer01") == nil {
            let bundled = Bundle.main.resourceURL?.appendingPathComponent(
                "icons/hi.mousePointer01.svg")
            let image =
                bundled.flatMap { NSImage(contentsOf: $0) }
                ?? NSImage(contentsOfFile: desktopCursorAsset)
            image?.setName("hi.mousePointer01")
        }
        if Bundle.main.bundleIdentifier != nil {
            UNUserNotificationCenter.current().delegate = self
        }
        desktop.onChange = { [weak self] in self?.emit(.statusChanged) }
        emit(voice.status)
    }

    func emit(_ event: JSONValue) {
        guard event["type"]?.stringValue != "gateway_request" || isConnected else { return }
        events.append(event)
        wake?()
    }

    func fail(_ error: Error) {
        emit(.error(error.localizedDescription))
    }

    /// Unlike stopping, finishing keeps queued text: the last words are still arriving.
    func finishDictation() { dictation.finish() }

    func setDictation(_ enabled: Bool) {
        // A new draft must never consume queued text or a stop from the previous recording.
        events.removeAll {
            ["dictation", "dictation_state"].contains($0["type"]?.stringValue ?? "")
        }
        if enabled {
            voice.stop()
            dictation.start()
        } else {
            dictation.stop()
        }
    }

    func drain() -> String {
        guard !events.isEmpty else { return "[]" }
        let pending = events
        events.removeAll(keepingCapacity: true)
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        guard let data = try? encoder.encode(pending) else { return "[]" }
        return String(decoding: data, as: UTF8.self)
    }

    func connected(_ connected: Bool) {
        connectionGeneration &+= 1
        isConnected = connected
        guard connected else {
            voice.stop(notifyGateway: false)
            dictation.stop()
            events.removeAll {
                ["gateway_request", "title", "recap", "dictation"].contains($0["type"]?.stringValue ?? "")
            }
            desktop.disconnected()
            return
        }
        desktop.connected { [weak self] request in
            self?.emit(.object(["type": .string("gateway_request"), "request": request.body]))
        }
    }

    func notify(sessionID: String, title: String, body: String) {
        // Unbundled development builds cannot post notifications; drop the request quietly.
        guard Bundle.main.bundleIdentifier != nil else { return }
        Task {
            do {
                let center = UNUserNotificationCenter.current()
                guard try await center.requestAuthorization(options: [.alert, .sound]) else {
                    return
                }
                let content = UNMutableNotificationContent()
                content.title = title
                content.body = body
                content.sound = .default
                content.userInfo = ["sessionId": sessionID]
                try await center.add(
                    UNNotificationRequest(
                        identifier: UUID().uuidString, content: content, trigger: nil))
            } catch let error as UNError where error.code == .notificationsNotAllowed {
                // Declining optional notifications must not turn a completed chat into an error.
            } catch { fail(error) }
        }
    }

    /// The last known notification permission, refreshed in the background; a change
    /// asks Rust to render again.
    func notificationStatus() -> UInt8 {
        guard Bundle.main.bundleIdentifier != nil else { return 0 }
        UNUserNotificationCenter.current().getNotificationSettings { settings in
            let tag: UInt8 =
                switch settings.authorizationStatus {
                case .denied: 1
                case .authorized, .provisional, .ephemeral: 3
                default: 0
                }
            Task { @MainActor in
                let bridge = NativeBridge.shared
                guard bridge.notificationTag != tag else { return }
                bridge.notificationTag = tag
                bridge.emit(.statusChanged)
            }
        }
        return notificationTag
    }

    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        guard let session = response.notification.request.content.userInfo["sessionId"] as? String
        else { return }
        await MainActor.run {
            emit(.object(["type": .string("notification_opened"), "session_id": .string(session)]))
            NativeStatusItem.shared.show()
        }
    }
}

@_cdecl("mobius_native_poll")
public func nativePoll() -> UnsafeMutablePointer<CChar>? {
    strdup(MainActor.assumeIsolated { NativeBridge.shared.drain() })
}

@_cdecl("mobius_native_set_wake")
public func nativeSetWake(_ callback: (@convention(c) () -> Void)?) {
    MainActor.assumeIsolated { NativeBridge.shared.wake = callback }
}

@_cdecl("mobius_native_free")
public func nativeFree(_ pointer: UnsafeMutablePointer<CChar>?) { free(pointer) }

@_cdecl("mobius_native_connected")
public func nativeConnected(_ connected: Bool) {
    MainActor.assumeIsolated { NativeBridge.shared.connected(connected) }
}

@_cdecl("mobius_native_gateway")
public func nativeGateway(_ json: UnsafePointer<CChar>) {
    let data = Data(String(cString: json).utf8)
    MainActor.assumeIsolated {
        do {
            let decoder = JSONDecoder()
            decoder.keyDecodingStrategy = .convertFromSnakeCase
            let message = try decoder.decode(GatewayEnvelope.self, from: data)
            if message.type.hasPrefix("realtime_voice_") {
                try NativeBridge.shared.voice.receive(message)
            } else {
                try NativeBridge.shared.desktop.receive(message)
            }
        } catch { NativeBridge.shared.fail(error) }
    }
}

@_cdecl("mobius_native_desktop")
public func nativeDesktop(_ enabled: Bool) {
    MainActor.assumeIsolated { NativeBridge.shared.desktop.enabled = enabled }
}

/// 0 asks for Accessibility, 1 for Screen Recording: the system prompt the first time,
/// System Settings after that.
@_cdecl("mobius_native_desktop_request")
public func nativeDesktopRequest(_ permission: UInt8) {
    MainActor.assumeIsolated {
        DesktopPermission(rawValue: permission).map(NativeBridge.shared.desktop.request)
    }
}

/// Clears möbius's entry for the permission and asks again.
@_cdecl("mobius_native_desktop_reset")
public func nativeDesktopReset(_ permission: UInt8) {
    MainActor.assumeIsolated {
        DesktopPermission(rawValue: permission).map(NativeBridge.shared.desktop.resetAndRequest)
    }
}

@_cdecl("mobius_native_desktop_status")
public func nativeDesktopStatus() -> UInt8 {
    MainActor.assumeIsolated { NativeBridge.shared.desktop.statusBits }
}

/// Why computer control is not working yet, or null; freed with `mobius_native_free`.
@_cdecl("mobius_native_desktop_reason")
public func nativeDesktopReason() -> UnsafeMutablePointer<CChar>? {
    MainActor.assumeIsolated { NativeBridge.shared.desktop.reason }.flatMap { strdup($0) }
}

/// The remembered per-app choices, or null; freed with `mobius_native_free`.
@_cdecl("mobius_native_desktop_apps")
public func nativeDesktopApps() -> UnsafeMutablePointer<CChar>? {
    MainActor.assumeIsolated { NativeBridge.shared.desktop.appChoiceSummary }.flatMap { strdup($0) }
}

@_cdecl("mobius_native_desktop_forget_apps")
public func nativeDesktopForgetApps() {
    MainActor.assumeIsolated { NativeBridge.shared.desktop.forgetAppChoices() }
}

@_cdecl("mobius_native_desktop_accent")
public func nativeDesktopAccent(_ rgb: UInt32) {
    MainActor.assumeIsolated { NativeBridge.shared.desktop.accent = rgb }
}

/// Quits and opens möbius again; Screen Recording takes effect only in a new process.
@_cdecl("mobius_native_relaunch")
public func nativeRelaunch() {
    MainActor.assumeIsolated {
        let path = Bundle.main.bundleURL.path
        let relaunch = Process()
        relaunch.executableURL = URL(fileURLWithPath: "/bin/sh")
        relaunch.arguments = [
            "-c", "while kill -0 $1 2>/dev/null; do sleep 0.2; done; open \"$2\"",
            "relaunch", String(ProcessInfo.processInfo.processIdentifier), path,
        ]
        do {
            try relaunch.run()
            NSApp.terminate(nil)
        } catch { NativeBridge.shared.fail(error) }
    }
}

@_cdecl("mobius_native_reduce_motion")
public func nativeReduceMotion() -> Bool {
    NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
}

@_cdecl("mobius_native_notify")
public func nativeNotify(
    _ session: UnsafePointer<CChar>, _ title: UnsafePointer<CChar>, _ body: UnsafePointer<CChar>
) {
    let session = String(cString: session)
    let title = String(cString: title)
    let body = String(cString: body)
    MainActor.assumeIsolated {
        NativeBridge.shared.notify(sessionID: session, title: title, body: body)
    }
}

@_cdecl("mobius_native_notification_status")
public func nativeNotificationStatus() -> UInt8 {
    MainActor.assumeIsolated { NativeBridge.shared.notificationStatus() }
}

@_cdecl("mobius_native_dictation")
public func nativeDictation(_ enabled: Bool) {
    MainActor.assumeIsolated { NativeBridge.shared.setDictation(enabled) }
}

@_cdecl("mobius_native_dictation_finish")
public func nativeDictationFinish() {
    MainActor.assumeIsolated { NativeBridge.shared.finishDictation() }
}
