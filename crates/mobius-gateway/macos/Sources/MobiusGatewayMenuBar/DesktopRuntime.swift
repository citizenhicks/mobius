import AppKit
@preconcurrency import ApplicationServices
import Observation
import ScreenCaptureKit

@MainActor
@Observable
final class DesktopRuntime {
    var enabled = false {
        didSet {
            if !enabled {
                stopExecution()
                clearObservations()
                sessionID = nil
            }
            publishAvailability()
        }
    }
    private(set) var hasPermissions = false
    var isActive: Bool { executionID != nil }
    var message: String?

    @ObservationIgnored private var send: ((GatewayRequest) -> Void)?
    @ObservationIgnored private var task: Task<Void, Never>?
    private var executionID: String?
    @ObservationIgnored private var sessionID: String?
    @ObservationIgnored private var registrationID: String?
    @ObservationIgnored private var sessionObservers: [NSObjectProtocol] = []
    @ObservationIgnored var elements: [String: Element] = [:]
    @ObservationIgnored var screenshots: [String: Screenshot] = [:]

    struct Element {
        let value: AXUIElement
        let pid: pid_t
        let role: String
        let label: String
    }

    struct Screenshot {
        let displayID: CGDirectDisplayID
        let frame: CGRect
        let width: Int
        let height: Int
        let frontmostPID: pid_t?
        let captured: ContinuousClock.Instant
    }

    init() {
        let center = NSWorkspace.shared.notificationCenter
        for name in [
            NSWorkspace.sessionDidResignActiveNotification, NSWorkspace.screensDidSleepNotification,
        ] {
            sessionObservers.append(
                center.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                    MainActor.assumeIsolated { self?.stop() }
                })
        }
    }

    isolated deinit {
        for observer in sessionObservers {
            NSWorkspace.shared.notificationCenter.removeObserver(observer)
        }
    }

    func connected(send: @escaping (GatewayRequest) -> Void) {
        self.send = send
        refreshPermissions()
    }

    func disconnected() {
        send = nil
        enabled = false
        registrationID = nil
    }

    func requestPermissions() {
        let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true]
        _ = AXIsProcessTrustedWithOptions(options as CFDictionary)
        if !CGPreflightScreenCaptureAccess() { _ = CGRequestScreenCaptureAccess() }
        refreshPermissions()
    }

    func refreshPermissions() {
        hasPermissions = AXIsProcessTrusted() && CGPreflightScreenCaptureAccess()
        if !hasPermissions { enabled = false } else { publishAvailability() }
    }

    func stop() {
        enabled = false
        message = "Mac control stopped. Enable it again when you want a Bot to continue."
    }

    private func publishAvailability() {
        guard enabled && hasPermissions || registrationID != nil else { return }
        let requestID = UUID().uuidString
        registrationID = requestID
        send?(
            GatewayRequest(
                "set_desktop_runtime",
                [
                    "requestId": .string(requestID), "enabled": .bool(enabled && hasPermissions),
                ]))
    }

    func receive(_ envelope: GatewayEnvelope) throws {
        let body = envelope.body
        switch envelope.type {
        case "desktop_control_requested":
            let execution = try body.requiredString("executionId")
            let session = try body.requiredString("sessionId")
            let requestID = try body.requiredString("requestId")
            guard let request = body["request"] else {
                throw DesktopError("Desktop request is missing.")
            }
            if executionID != execution {
                stopExecution()
                if sessionID != session { clearObservations() }
                sessionID = session
                executionID = execution
            }
            guard task == nil else {
                reply(
                    requestID,
                    response: .object([
                        "error": .string(
                            "Await the current desktop action before starting another.")
                    ]))
                return
            }
            task = Task { [weak self] in
                guard let self else { return }
                let response: JSONValue
                do {
                    try self.requireControl()
                    response = .object(["result": try await self.perform(request)])
                } catch {
                    response = .object(["error": .string(error.localizedDescription)])
                }
                guard !Task.isCancelled, self.executionID == execution else { return }
                self.reply(requestID, response: response)
                self.task = nil;
            }
        case "desktop_control_ended":
            if body["executionId"]?.stringValue == executionID { stopExecution() }
        case "rejected":
            if body["requestId"]?.stringValue == registrationID {
                message = body["message"]?.stringValue
                enabled = false
            }
        default: break
        }
    }

    private func reply(_ requestID: String, response: JSONValue) {
        if let error = response["error"]?.stringValue { message = error }
        send?(
            GatewayRequest(
                "desktop_control_reply",
                [
                    "requestId": .string(requestID), "response": response,
                ]))
    }

    private func stopExecution() {
        task?.cancel()
        task = nil
        executionID = nil
    }

    func clearObservations() {
        elements.removeAll()
        screenshots.removeAll()
    }

    func requireControl() throws {
        try Task.checkCancellation()
        guard enabled, send != nil, AXIsProcessTrusted(), CGPreflightScreenCaptureAccess() else {
            throw DesktopError(
                "Enable Mac control and grant Accessibility and Screen Recording access in the möbius-app app."
            )
        }
        guard let session = CGSessionCopyCurrentDictionary() as? [String: Any],
            session[kCGSessionOnConsoleKey as String] as? Bool == true,
            session["CGSSessionScreenIsLocked"] as? Bool != true
        else { throw DesktopError("The Mac is locked or its desktop session is inactive.") }
    }

    private func perform(_ request: JSONValue) async throws -> JSONValue {
        let action = try request.requiredString("action")
        switch action {
        case "apps":
            try fields(request, allowed: [])
            return .array(
                NSWorkspace.shared.runningApplications.compactMap { app in
                    guard app.activationPolicy == .regular,
                        app.processIdentifier != ProcessInfo.processInfo.processIdentifier
                    else { return nil }
                    return .object([
                        "pid": .integer(Int64(app.processIdentifier)),
                        "name": .string(app.localizedName ?? "Application"),
                        "bundleId": app.bundleIdentifier.map(JSONValue.string) ?? .null,
                        "active": .bool(app.isActive),
                    ])
                })
        case "displays":
            try fields(request, allowed: [])
            return .array(try displayRecords())
        case "open_app":
            try fields(request, allowed: ["bundleId"])
            let bundleID = try request.requiredString("bundleId")
            guard bundleID != Bundle.main.bundleIdentifier, bundleID.utf8.count <= 256,
                let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: bundleID)
            else { throw DesktopError("The requested installed app is unavailable.") }
            let configuration = NSWorkspace.OpenConfiguration()
            configuration.activates = true
            let app = try await NSWorkspace.shared.openApplication(
                at: url, configuration: configuration)
            try requireControl()
            clearObservations()
            return .object([
                "pid": .integer(Int64(app.processIdentifier)),
                "name": .string(app.localizedName ?? bundleID),
            ])
        case "activate":
            try fields(request, allowed: ["pid"])
            let app = try application(request)
            guard app.activate() else { throw DesktopError("The app could not be activated.") }
            clearObservations()
            return .bool(true)
        case "inspect":
            try fields(request, allowed: ["pid"])
            return try await inspect(application(request))
        case "press", "set_value":
            return try accessibilityAction(request, action: action)
        case "screenshot":
            try fields(request, allowed: ["displayId"])
            return try await capture(request)
        case "click", "move", "drag", "scroll":
            return try await pointerAction(request, action: action)
        case "type_text", "press_key":
            return try await keyboardAction(request, action: action)
        default: throw DesktopError("Unknown desktop action.")
        }
    }

    func application(_ request: JSONValue) throws -> NSRunningApplication {
        guard let pid = request["pid"]?.intValue, let processID = pid_t(exactly: pid),
            processID > 0,
            processID != ProcessInfo.processInfo.processIdentifier,
            let app = NSRunningApplication(processIdentifier: processID), !app.isTerminated
        else { throw DesktopError("Choose a running app from desktop.apps().") }
        return app
    }

    func fields(_ request: JSONValue, allowed: Set<String>) throws {
        guard let fields = request.objectValue,
            Set(fields.keys).isSubset(of: allowed.union(["action"]))
        else {
            throw DesktopError("The desktop action contains unsupported fields.")
        }
    }

    private func displayRecords() throws -> [JSONValue] {
        var count: UInt32 = 0
        guard CGGetActiveDisplayList(0, nil, &count) == .success else {
            throw DesktopError("Displays could not be read.")
        }
        var displays = [CGDirectDisplayID](repeating: 0, count: Int(count))
        guard CGGetActiveDisplayList(count, &displays, &count) == .success else {
            throw DesktopError("Displays could not be read.")
        }
        return displays.prefix(Int(count)).map { display in
            .object([
                "displayId": .integer(Int64(display)),
                "frame": Self.frame(CGDisplayBounds(display)),
            ])
        }
    }

    private func capture(_ request: JSONValue) async throws -> JSONValue {
        let displayID: CGDirectDisplayID
        if let value = request["displayId"] {
            guard let number = value.intValue, let id = CGDirectDisplayID(exactly: number) else {
                throw DesktopError("Choose a display from desktop.displays().")
            }
            displayID = id
        } else {
            displayID = CGMainDisplayID()
        }
        let content = try await SCShareableContent.excludingDesktopWindows(
            false, onScreenWindowsOnly: true)
        try requireControl()
        guard let display = content.displays.first(where: { $0.displayID == displayID }) else {
            throw DesktopError("The selected display is no longer available.")
        }
        let bounds = CGDisplayBounds(displayID)
        let width = Int(bounds.width), height = Int(bounds.height)
        guard width > 0, height > 0, width <= 8192, height <= 8192, width * height <= 20_000_000
        else {
            throw DesktopError("This display exceeds the screenshot size limit.")
        }
        let configuration = SCStreamConfiguration()
        configuration.width = width
        configuration.height = height
        configuration.showsCursor = true
        configuration.captureResolution = .nominal
        let filter = SCContentFilter(display: display, excludingWindows: [])
        let image = try await SCScreenshotManager.captureImage(
            contentFilter: filter, configuration: configuration)
        try requireControl()
        guard
            let png = NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:]),
            png.count <= 32 * 1024 * 1024
        else {
            throw DesktopError("The screenshot could not be encoded within its size limit.")
        }
        let id = UUID().uuidString
        if screenshots.count >= 8 { screenshots.removeAll() }
        screenshots[id] = Screenshot(
            displayID: displayID, frame: bounds, width: image.width, height: image.height,
            frontmostPID: NSWorkspace.shared.frontmostApplication?.processIdentifier, captured: .now
        )
        return .object([
            "screenshotId": .string(id), "displayId": .integer(Int64(displayID)),
            "width": .integer(Int64(image.width)), "height": .integer(Int64(image.height)),
            "displayFrame": Self.frame(bounds),
            "coordinates": .string(
                "Use image pixels from this screenshot; the runtime maps them to the selected display."
            ),
            "png": .string(png.base64EncodedString()),
        ])
    }

    static func frame(_ rect: CGRect) -> JSONValue {
        .object([
            "x": .number(rect.minX), "y": .number(rect.minY), "width": .number(rect.width),
            "height": .number(rect.height),
        ])
    }
}

struct DesktopError: LocalizedError {
    let message: String
    init(_ message: String) { self.message = message }
    var errorDescription: String? { message }
}
