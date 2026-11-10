import AppKit
@preconcurrency import ApplicationServices
import ScreenCaptureKit

/// A macOS privacy permission Mac control needs.
enum DesktopPermission: UInt8 {
    case accessibility = 0
    case screenRecording = 1

    /// The System Settings name, which macOS 27 changed for Accessibility.
    var name: String {
        switch self {
        case .accessibility:
            ProcessInfo.processInfo.isOperatingSystemAtLeast(
                OperatingSystemVersion(majorVersion: 27, minorVersion: 0, patchVersion: 0))
                ? "Device Control and Data Access" : "Accessibility"
        case .screenRecording: "Screen Recording"
        }
    }

    var settingsURL: URL {
        URL(
            string: "x-apple.systempreferences:com.apple.preference.security?Privacy_"
                + (self == .accessibility ? "Accessibility" : "ScreenCapture"))!
    }
}

@MainActor
final class DesktopRuntime {
    /// The person's choice. It outlasts relaunches, reconnects, a sleeping screen and a
    /// revoked permission; control resumes once everything it needs is back.
    var enabled: Bool {
        didSet {
            defaults.set(enabled, forKey: Self.enabledKey)
            rejection = nil
            if enabled { requestMissingPermission() } else { pause() }
            publishAvailability()
        }
    }
    private(set) var hasAccessibility = false
    private(set) var hasScreenRecording = false
    private(set) var isRegistered = false
    var hasPermissions: Bool { hasAccessibility && hasScreenRecording }
    var isActive: Bool { executionID != nil }
    /// Called when anything the Settings page shows changes.
    var onChange: (() -> Void)?
    /// The app's accent, as 0xRRGGBB, for the Bot's cursor.
    var accent: UInt32 = 0x5E81AC

    let defaults: UserDefaults
    /// Apps allowed once, for the rest of the current chat.
    var onceApps: Set<String> = []
    /// Inspect outlines: a counter for their IDs and each app's last rows, for `changes`.
    var snapshotCount = 0
    var outlines: [pid_t: [String]] = [:]
    /// The Bot's own pointer position, in Quartz coordinates.
    var botPoint: CGPoint?
    var webAccessible: Set<pid_t> = []
    let askToUse: @MainActor (String) async -> DesktopAppChoice
    private var send: ((GatewayRequest) -> Void)?
    private var task: Task<Void, Never>?
    private var executionID: String?
    private var sessionID: String?
    private var registrationID: String?
    private var publishedAvailability = false
    private var sessionActive = true
    private var rejection: String?
    private var lastStatus: (UInt8, String?) = (0, nil)
    private var permissionMonitor: Task<Void, Never>?
    private let readPermissions: () -> (accessibility: Bool, screenRecording: Bool)
    private let present: @MainActor (DesktopPermission, _ firstTime: Bool) -> Void
    private var cursorOverlay: DesktopCursorOverlay?
    private var sessionObservers: [NSObjectProtocol] = []
    var elements: [String: Element] = [:]
    var screenshots: [String: Screenshot] = [:]
    var lastActionTarget: (app: NSRunningApplication, displayID: CGDirectDisplayID?)?

    private static let enabledKey = "desktop-control-enabled"
    private static func requestedKey(_ permission: DesktopPermission) -> String {
        "desktop-control-requested-\(permission.rawValue)"
    }

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
        var app: NSRunningApplication? = nil
    }

    init(
        defaults: UserDefaults = .standard,
        readPermissions: @escaping () -> (accessibility: Bool, screenRecording: Bool) = {
            (DesktopRuntime.isAccessibilityTrusted(), CGPreflightScreenCaptureAccess())
        },
        present: @escaping @MainActor (DesktopPermission, _ firstTime: Bool) -> Void =
            DesktopRuntime.present,
        askToUse: @escaping @MainActor (String) async -> DesktopAppChoice = DesktopRuntime
            .promptToUse
    ) {
        self.askToUse = askToUse
        self.defaults = defaults
        self.readPermissions = readPermissions
        self.present = present
        enabled = defaults.bool(forKey: Self.enabledKey)
        let center = NSWorkspace.shared.notificationCenter
        let session: [(Notification.Name, Bool)] = [
            (NSWorkspace.sessionDidResignActiveNotification, false),
            (NSWorkspace.screensDidSleepNotification, false),
            (NSWorkspace.sessionDidBecomeActiveNotification, true),
            (NSWorkspace.screensDidWakeNotification, true),
        ]
        for (name, active) in session {
            sessionObservers.append(
                center.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                    MainActor.assumeIsolated { self?.setSessionActive(active) }
                })
        }
        refreshPermissions()
        permissionMonitor = Task { [weak self] in
            while !Task.isCancelled {
                do { try await Task.sleep(for: .seconds(1)) } catch { return }
                self?.refreshPermissions()
            }
        }
    }

    isolated deinit {
        permissionMonitor?.cancel()
        cursorOverlay?.hide()
        for observer in sessionObservers {
            NSWorkspace.shared.notificationCenter.removeObserver(observer)
        }
    }

    func connected(send: @escaping (GatewayRequest) -> Void) {
        self.send = send
        rejection = nil
        refreshPermissions()
        publishAvailability()
    }

    func disconnected() {
        send = nil
        registrationID = nil
        publishedAvailability = false
        isRegistered = false
        pause()
        notifyIfChanged()
    }

    /// Whether macOS has already been asked; it shows each prompt only once.
    func wasRequested(_ permission: DesktopPermission) -> Bool {
        defaults.bool(forKey: Self.requestedKey(permission))
    }

    /// Shows the system prompt the first time and System Settings after that.
    func request(_ permission: DesktopPermission) {
        let firstTime = !wasRequested(permission)
        defaults.set(true, forKey: Self.requestedKey(permission))
        present(permission, firstTime)
        refreshPermissions()
        notifyIfChanged()
    }

    static func isAccessibilityTrusted() -> Bool { AXIsProcessTrusted() }

    /// System Settings can show möbius as allowed while macOS still refuses this copy (an
    /// entry left by another build). Clears möbius's entry so the next prompt is fresh.
    func resetAndRequest(_ permission: DesktopPermission) {
        let reset = Process()
        reset.executableURL = URL(fileURLWithPath: "/usr/bin/tccutil")
        reset.arguments = [
            "reset", permission == .accessibility ? "Accessibility" : "ScreenCapture",
            Bundle.main.bundleIdentifier ?? "app.mobius.desktop",
        ]
        try? reset.run()
        reset.waitUntilExit()
        defaults.set(false, forKey: Self.requestedKey(permission))
        request(permission)
    }

    private static func present(_ permission: DesktopPermission, firstTime: Bool) {
        guard firstTime else {
            NSWorkspace.shared.open(permission.settingsURL)
            return
        }
        switch permission {
        case .accessibility:
            let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true]
            _ = AXIsProcessTrustedWithOptions(options as CFDictionary)
        case .screenRecording:
            _ = CGRequestScreenCaptureAccess()
        }
    }

    /// One prompt at a time: Accessibility first, then Screen Recording once it is allowed.
    private func requestMissingPermission() {
        if !hasAccessibility {
            if !wasRequested(.accessibility) { request(.accessibility) }
        } else if !hasScreenRecording, !wasRequested(.screenRecording) {
            request(.screenRecording)
        }
    }

    func refreshPermissions() {
        let permissions = readPermissions()
        let gainedAccessibility = permissions.accessibility && !hasAccessibility
        hasAccessibility = permissions.accessibility
        hasScreenRecording = permissions.screenRecording
        if !hasPermissions { pause() }
        if enabled && gainedAccessibility { requestMissingPermission() }
        publishAvailability()
        notifyIfChanged()
    }

    /// The screen slept or the person switched users: stop now and resume when they return.
    func stop() { setSessionActive(false) }

    private func setSessionActive(_ active: Bool) {
        sessionActive = active
        if !active { pause() }
        publishAvailability()
        notifyIfChanged()
    }

    private func pause() {
        stopExecution()
        clearObservations()
        sessionID = nil
    }

    /// What stands between the switch and a Bot using this Mac, in the words Settings shows.
    var reason: String? {
        guard enabled else { return nil }
        if let rejection { return rejection }
        if !hasAccessibility {
            return wasRequested(.accessibility)
                ? "Waiting for \(DesktopPermission.accessibility.name). Turn on möbius in System Settings."
                : "Allow \(DesktopPermission.accessibility.name) so Bots can use the mouse and keyboard."
        }
        if !hasScreenRecording {
            return wasRequested(.screenRecording)
                ? "Waiting for Screen Recording. After you allow it, quit and reopen möbius."
                : "Allow Screen Recording so Bots can see the screen."
        }
        if send == nil { return "Connect to the gateway on this Mac to continue." }
        if !sessionActive { return "Paused while the screen is locked or asleep." }
        return isRegistered ? nil : "Connecting to the gateway…"
    }

    /// enabled 1, Accessibility 2, Screen Recording 4, registered 8, 16 and 32 once each
    /// permission has been requested, and 64 where Accessibility is called Device Control
    /// and Data Access.
    var statusBits: UInt8 {
        (enabled ? 1 : 0) | (hasAccessibility ? 2 : 0) | (hasScreenRecording ? 4 : 0)
            | (isRegistered ? 8 : 0) | (wasRequested(.accessibility) ? 16 : 0)
            | (wasRequested(.screenRecording) ? 32 : 0)
            | (DesktopPermission.accessibility.name == "Accessibility" ? 0 : 64)
    }

    func notifyIfChanged() {
        let status = (statusBits, reason)
        guard status != lastStatus else { return }
        lastStatus = status
        onChange?()
    }

    private func publishAvailability() {
        let available = enabled && hasPermissions && sessionActive && rejection == nil
        guard let send, available != publishedAvailability else { return }
        let requestID = UUID().uuidString
        registrationID = requestID
        publishedAvailability = available
        isRegistered = false
        send(
            GatewayRequest(
                "set_desktop_runtime",
                [
                    "requestId": .string(requestID), "enabled": .bool(available),
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
                stopExecution(settleCursor: true)
                if sessionID != session {
                    clearObservations()
                    onceApps.removeAll()
                }
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
                    self.showCursor()
                    response = .object(["result": try await self.perform(request)])
                } catch {
                    response = .object(["error": .string(error.localizedDescription)])
                }
                guard !Task.isCancelled, self.executionID == execution else { return }
                self.reply(requestID, response: response)
                self.task = nil
            }
        case "desktop_control_ended":
            if body["executionId"]?.stringValue == executionID { stopExecution(settleCursor: true) }
        case "accepted", "rejected":
            if let registrationID, body["requestId"]?.stringValue == registrationID {
                self.registrationID = nil
                isRegistered = envelope.type == "accepted" && publishedAvailability
                if envelope.type == "rejected" {
                    rejection = Self.rejectionReason(body["message"]?.stringValue)
                    publishedAvailability = false
                }
                notifyIfChanged()
            }
        default: break
        }
    }

    /// The gateway's refusal in plain words; it stays until the switch or connection changes.
    static func rejectionReason(_ message: String?) -> String {
        switch message {
        case "only the local Mac app can host desktop control":
            "Computer control works only with the gateway running on this Mac."
        case "another Mac app already hosts desktop control":
            "Another möbius window on this Mac already provides computer control."
        default: message ?? "The gateway didn’t accept computer control."
        }
    }

    private func reply(_ requestID: String, response: JSONValue) {
        send?(
            GatewayRequest(
                "desktop_control_reply",
                [
                    "requestId": .string(requestID), "response": response,
                ]))
    }

    private func stopExecution(settleCursor: Bool = false) {
        task?.cancel()
        task = nil
        executionID = nil
        lastActionTarget = nil
        if settleCursor {
            cursorOverlay?.settle()
        } else {
            cursorOverlay?.hide()
        }
    }

    func showCursor() {
        if cursorOverlay == nil { cursorOverlay = DesktopCursorOverlay() }
        cursorOverlay?.prepare(accent: accent)
        // Where the Bot last pointed, or the middle of the main display; never the person's pointer.
        if !isCursorVisible {
            let display = CGDisplayBounds(CGMainDisplayID())
            moveCursor(to: botPoint ?? CGPoint(x: display.midX, y: display.midY))
        }
    }

    func moveCursor(to quartzPoint: CGPoint) {
        guard !Task.isCancelled else { return }
        botPoint = quartzPoint
        guard
            let screen = NSScreen.screens.first(where: { screen in
                guard
                    let displayID = screen.deviceDescription[
                        NSDeviceDescriptionKey("NSScreenNumber")]
                        as? CGDirectDisplayID
                else { return false }
                return CGDisplayBounds(displayID).contains(quartzPoint)
            }),
            let displayID = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")]
                as? CGDirectDisplayID
        else { return }
        cursorOverlay?.move(
            to: Self.appKitPoint(
                quartzPoint, displayBounds: CGDisplayBounds(displayID), screenFrame: screen.frame))
    }

    var cursorWindowID: CGWindowID? { cursorOverlay?.windowID }
    var isCursorVisible: Bool { cursorOverlay?.isVisible == true }

    static func appKitPoint(
        _ quartzPoint: CGPoint, displayBounds: CGRect, screenFrame: CGRect
    ) -> CGPoint {
        CGPoint(
            x: screenFrame.minX + quartzPoint.x - displayBounds.minX,
            y: screenFrame.maxY - quartzPoint.y + displayBounds.minY)
    }

    func clearObservations() {
        elements.removeAll()
        screenshots.removeAll()
    }

    func requireControl() throws {
        try Task.checkCancellation()
        guard enabled, send != nil, Self.isAccessibilityTrusted(), CGPreflightScreenCaptureAccess()
        else {
            throw DesktopError(
                "Computer control isn’t set up on this Mac. Ask the person to finish setup in möbius Settings → Security."
            )
        }
        guard let session = CGSessionCopyCurrentDictionary() as? [String: Any],
            session[kCGSessionOnConsoleKey as String] as? Bool == true,
            session["CGSSessionScreenIsLocked"] as? Bool != true
        else { throw DesktopError("The Mac is locked or its desktop session is inactive.") }
    }

    func perform(_ request: JSONValue) async throws -> JSONValue {
        let action = try request.requiredString("action")
        if [
            "open_app", "activate", "press", "set_value", "click", "move", "drag", "scroll",
            "type_text", "press_key",
        ].contains(action) {
            lastActionTarget = nil
        }
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
            try await authorize(
                bundle: bundleID,
                name: FileManager.default.displayName(atPath: url.path)
                    .replacingOccurrences(of: ".app", with: ""))
            let configuration = NSWorkspace.OpenConfiguration()
            configuration.activates = true
            let app = try await NSWorkspace.shared.openApplication(
                at: url, configuration: configuration)
            try requireControl()
            clearObservations()
            lastActionTarget = (app, nil)
            return .object([
                "pid": .integer(Int64(app.processIdentifier)),
                "name": .string(app.localizedName ?? bundleID),
            ])
        case "activate":
            try fields(request, allowed: ["pid"])
            let app = try application(request)
            try await authorize(pid: app.processIdentifier)
            try requireControl()
            guard app.activate() else { throw DesktopError("The app could not be activated.") }
            clearObservations()
            lastActionTarget = (app, nil)
            return .bool(true)
        case "inspect":
            try fields(request, allowed: ["pid", "query", "changes"])
            let app = try application(request)
            try await authorize(pid: app.processIdentifier)
            return try await inspect(
                app, query: request["query"]?.stringValue,
                changes: request["changes"] == .bool(true))
        case "press", "set_value":
            if let id = request["elementId"]?.stringValue, let element = elements[id] {
                try await authorize(pid: element.pid)
            }
            try requireControl()
            return try accessibilityAction(request, action: action)
        case "screenshot":
            return try await capture(request)
        case "zoom":
            return try await zoom(request)
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

    // Keep capture limits local: desktop requests cross independently upgraded app/gateway versions.
    // Core validates the captured dimensions before model admission.
    static func fittedScreenshotSize(width: Int, height: Int)
        -> (width: Int, height: Int)
    {
        let longest = max(width, height, 1)
        func scaled(_ side: Int) -> (width: Int, height: Int) {
            (max(1, width * side / longest), max(1, height * side / longest))
        }
        var low = 1, high = min(longest, 1280)
        while low < high {
            let middle = low + (high - low + 1) / 2
            let size = scaled(middle)
            if ((size.width + 31) / 32) * ((size.height + 31) / 32) <= 2500 {
                low = middle
            } else {
                high = middle - 1
            }
        }
        return scaled(low)
    }

    func screenshotTarget(_ request: JSONValue) async throws
        -> (app: NSRunningApplication?, displayID: CGDirectDisplayID)
    {
        try fields(request, allowed: ["pid", "displayId", "afterAction"])
        let app: NSRunningApplication?
        if let afterAction = request["afterAction"] {
            guard afterAction == .bool(true), request["pid"] == nil,
                request["displayId"] == nil, let target = lastActionTarget
            else {
                throw DesktopError(
                    "There is no app action to capture. Use desktop.screenshotApp(pid).")
            }
            try requireScreenshotApp(target.app)
            return (target.app, target.displayID ?? CGMainDisplayID())
        }
        if request["pid"] != nil {
            let selected = try application(request)
            try await authorize(pid: selected.processIdentifier)
            try requireScreenshotApp(selected)
            app = selected
        } else {
            app = nil
        }
        let displayID: CGDirectDisplayID
        if let value = request["displayId"] {
            guard let number = value.intValue, let id = CGDirectDisplayID(exactly: number) else {
                throw DesktopError("Choose a display from desktop.displays().")
            }
            displayID = id
        } else {
            displayID = CGMainDisplayID()
        }
        return (app, displayID)
    }

    private func capture(_ request: JSONValue) async throws -> JSONValue {
        let (app, displayID) = try await screenshotTarget(request)
        let bounds = CGDisplayBounds(displayID)
        let width = Int(bounds.width), height = Int(bounds.height)
        guard width > 0, height > 0, width <= 8192, height <= 8192, width * height <= 20_000_000
        else {
            throw DesktopError("This display exceeds the screenshot size limit.")
        }
        let fitted = Self.fittedScreenshotSize(width: width, height: height)
        let (image, png) = try await image(of: displayID, app: app, region: nil, size: fitted)
        let id = UUID().uuidString
        if screenshots.count >= 8 { screenshots.removeAll() }
        screenshots[id] = Screenshot(
            displayID: displayID, frame: bounds, width: image.width, height: image.height,
            frontmostPID: NSWorkspace.shared.frontmostApplication?.processIdentifier,
            captured: .now,
            app: app
        )
        var result: [String: JSONValue] = [
            "screenshotId": .string(id), "displayId": .integer(Int64(displayID)),
            "width": .integer(Int64(image.width)), "height": .integer(Int64(image.height)),
            "displayFrame": Self.frame(bounds),
            "coordinates": .string(
                "Use image pixels from this screenshot; the runtime maps them to the selected display."
            ),
            "png": .string(png.base64EncodedString()),
        ]
        if let app { result["pid"] = .integer(Int64(app.processIdentifier)) }
        return .object(result)
    }

    /// A region of an earlier screenshot at the display's full detail. Clicks keep using the
    /// original screenshot's pixels.
    func zoom(_ request: JSONValue) async throws -> JSONValue {
        try fields(request, allowed: ["screenshotId", "x", "y", "width", "height"])
        let id = try request.requiredString("screenshotId")
        guard let screenshot = screenshots[id] else {
            throw DesktopError(
                "This screenshot is stale. Capture the display again before zooming.")
        }
        let origin = try observedPoint(request)
        if let app = screenshot.app { try requireScreenshotApp(app) }
        let corner = try Self.screenPoint(
            screenshot,
            x: min(
                Double(screenshot.width) - 1, try number(request, "x") + number(request, "width")),
            y: min(
                Double(screenshot.height) - 1, try number(request, "y") + number(request, "height"))
        )
        let region = CGRect(
            x: origin.x - screenshot.frame.minX, y: origin.y - screenshot.frame.minY,
            width: corner.x - origin.x, height: corner.y - origin.y)
        guard region.width >= 8, region.height >= 8 else {
            throw DesktopError("Zoom into a region at least 8 pixels wide and high.")
        }
        let scale =
            NSScreen.screens.first {
                $0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? CGDirectDisplayID
                    == screenshot.displayID
            }?.backingScaleFactor ?? 2
        let fitted = Self.fittedScreenshotSize(
            width: Int(region.width * scale), height: Int(region.height * scale))
        let (image, png) = try await image(
            of: screenshot.displayID, app: screenshot.app, region: region, size: fitted)
        return .object([
            "zoomOf": .string(id), "width": .integer(Int64(image.width)),
            "height": .integer(Int64(image.height)),
            "coordinates": .string(
                "A detail view only; click with the original screenshot's pixels."),
            "png": .string(png.base64EncodedString()),
        ])
    }

    /// One capture of a display, or of a region of it in display points, without the Bot's cursor.
    private func image(
        of displayID: CGDirectDisplayID, app: NSRunningApplication?, region: CGRect?,
        size: (width: Int, height: Int)
    ) async throws -> (CGImage, Data) {
        let content = try await SCShareableContent.excludingDesktopWindows(
            false, onScreenWindowsOnly: true)
        try requireControl()
        guard let display = content.displays.first(where: { $0.displayID == displayID }) else {
            throw DesktopError("The selected display is no longer available.")
        }
        let configuration = SCStreamConfiguration()
        configuration.width = size.width
        configuration.height = size.height
        if let region { configuration.sourceRect = region }
        configuration.showsCursor = app == nil
        configuration.captureResolution = region == nil ? .nominal : .best
        let filter: SCContentFilter
        if let app {
            try requireScreenshotApp(app)
            guard
                let selected = content.applications.first(where: {
                    $0.processID == app.processIdentifier
                        && $0.bundleIdentifier == app.bundleIdentifier
                })
            else { throw DesktopError("The selected app is no longer available for capture.") }
            filter = SCContentFilter(display: display, including: [selected], exceptingWindows: [])
            filter.includeMenuBar = false
            configuration.includeChildWindows = false
        } else {
            let excludedWindows =
                cursorWindowID.map { windowID in
                    content.windows.filter { $0.windowID == windowID }
                } ?? []
            filter = SCContentFilter(display: display, excludingWindows: excludedWindows)
        }
        let image = try await SCScreenshotManager.captureImage(
            contentFilter: filter, configuration: configuration)
        try requireControl()
        if let app { try requireScreenshotApp(app) }
        guard
            let png = NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:]),
            png.count <= 32 * 1024 * 1024
        else {
            throw DesktopError("The screenshot could not be encoded within its size limit.")
        }
        return (image, png)
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
