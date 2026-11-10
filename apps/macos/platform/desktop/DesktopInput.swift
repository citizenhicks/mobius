import AppKit
import Carbon

extension DesktopRuntime {
    static func screenPoint(_ screenshot: Screenshot, x: Double, y: Double) throws -> CGPoint {
        guard x.isFinite, y.isFinite, x >= 0, y >= 0, x < Double(screenshot.width),
            y < Double(screenshot.height)
        else {
            throw DesktopError("Coordinates must be inside the observed screenshot.")
        }
        return CGPoint(
            x: screenshot.frame.minX + x * screenshot.frame.width / Double(screenshot.width),
            y: screenshot.frame.minY + y * screenshot.frame.height / Double(screenshot.height))
    }

    /// A screenshot stays usable while its display is unchanged; the person may switch apps
    /// in the meantime, since each event is checked against the window under its point.
    func observedPoint(_ request: JSONValue, x: String = "x", y: String = "y") throws -> CGPoint {
        let id = try request.requiredString("screenshotId")
        guard let screenshot = screenshots[id],
            screenshot.captured.duration(to: .now) < .seconds(60),
            CGDisplayIsActive(screenshot.displayID) != 0,
            CGDisplayBounds(screenshot.displayID) == screenshot.frame
        else {
            throw DesktopError("This screenshot is stale. Capture the display again before acting.")
        }
        return try Self.screenPoint(screenshot, x: number(request, x), y: number(request, y))
    }

    func pointerAction(_ request: JSONValue, action: String) async throws -> JSONValue {
        let common: Set<String> = ["screenshotId", "x", "y"]
        let extras: Set<String>
        switch action {
        case "click": extras = ["button", "clicks"]
        case "drag": extras = ["toX", "toY"]
        case "scroll": extras = ["deltaX", "deltaY"]
        default: extras = []
        }
        try fields(request, allowed: common.union(extras))
        let point = try observedPoint(request)
        Self.gesture = Int64(clock_gettime_nsec_np(CLOCK_UPTIME_RAW) % 1_000_000_000)
        // The Bot's events go to the window under its point, so the person's pointer stays put.
        var target = Self.window(at: point)
        let screenshot = screenshots[try request.requiredString("screenshotId")]
        if let app = screenshot?.app {
            try requireScreenshotApp(app)
            try Self.requirePointerScope(appPID: app.processIdentifier, target: target)
            target?.screenshotApp = app
        }
        if let target { try await authorize(pid: target.pid) }
        if let app = target?.screenshotApp { try requireScreenshotApp(app) }
        // Without a window, Quartz moves the real pointer; give the person theirs back.
        let home = CGEvent(source: nil)?.location
        defer { if target == nil, let home { CGWarpMouseCursorPosition(home) } }
        if action == "drag" {
            let destination = try observedPoint(request, x: "toX", y: "toY")
            clearObservations()
            try mouse(.leftMouseDown, at: point, state: 1, target: target)
            defer { try? mouse(.leftMouseUp, at: destination, state: 1, target: target) }
            for step in 1...10 {
                try await Task.sleep(for: .milliseconds(20))
                try requireControl()
                let fraction = Double(step) / 10
                try mouse(
                    .leftMouseDragged,
                    at: CGPoint(
                        x: point.x + (destination.x - point.x) * fraction,
                        y: point.y + (destination.y - point.y) * fraction),
                    state: 1, target: target)
            }
        } else if action == "scroll" {
            let dx = try number(request, "deltaX")
            let dy = try number(request, "deltaY")
            guard abs(dx) <= 10_000, abs(dy) <= 10_000,
                let event = CGEvent(
                    scrollWheelEvent2Source: nil, units: .pixel, wheelCount: 2,
                    wheel1: Int32(-dy), wheel2: Int32(-dx), wheel3: 0)
            else { throw DesktopError("Scroll distance exceeds its limit.") }
            clearObservations()
            try mouse(.mouseMoved, at: point, target: target)
            event.location = point
            if let app = target?.screenshotApp { try requireScreenshotApp(app) }
            Self.deliver(event, at: point, to: target)
        } else if action == "move" {
            try mouse(.mouseMoved, at: point, target: target)
        } else {
            guard let buttonName = request["button"]?.stringValue,
                ["left", "right"].contains(buttonName), let clicks = request["clicks"]?.intValue,
                (1...2).contains(clicks)
            else { throw DesktopError("Use a left or right click, with a count of one or two.") }
            let right = buttonName == "right"
            clearObservations()
            try await click(at: point, right: right, clicks: clicks, target: target)
        }
        if let target,
            let app = target.screenshotApp ?? NSRunningApplication(processIdentifier: target.pid)
        {
            lastActionTarget = (app, screenshot?.displayID)
        }
        return .bool(true)
    }

    /// cua-driver's background click: hover the point, prime Chromium's user-activation check
    /// with a click off screen, then click, every event stamped as one gesture.
    private func click(at point: CGPoint, right: Bool, clicks: Int, target: PointerTarget?)
        async throws
    {
        let (down, up): (CGEventType, CGEventType) =
            right ? (.rightMouseDown, .rightMouseUp) : (.leftMouseDown, .leftMouseUp)
        let button: CGMouseButton = right ? .right : .left
        if let target {
            SkyLight.activateWithoutRaising(pid: target.pid, window: CGWindowID(target.window))
            try await Task.sleep(for: .milliseconds(30))
        }
        try mouse(.mouseMoved, at: point, phase: 2, target: target)
        try await Task.sleep(for: .milliseconds(15))
        if let target, !right, SkyLight.isAvailable {
            let offscreen = CGPoint(x: -1, y: -1)
            try mouse(.leftMouseDown, at: offscreen, state: 1, phase: 1, target: target)
            try await Task.sleep(for: .milliseconds(1))
            try mouse(.leftMouseUp, at: offscreen, state: 1, phase: 2, target: target)
            try await Task.sleep(for: .milliseconds(100))
        }
        for count in 1...clicks {
            try requireControl()
            try mouse(down, at: point, button: button, state: count, phase: 3, target: target)
            try await Task.sleep(for: .milliseconds(1))
            try mouse(up, at: point, button: button, state: count, phase: 3, target: target)
            if count < clicks { try await Task.sleep(for: .milliseconds(80)) }
        }
        moveCursor(to: point)
    }

    func keyboardAction(_ request: JSONValue, action: String) async throws -> JSONValue {
        try fields(
            request,
            allowed: action == "type_text" ? ["pid", "text"] : ["pid", "key", "modifiers"])
        let app = try application(request)
        try await authorize(pid: app.processIdentifier)
        let code: CGKeyCode
        var chunks: [[UniChar]] = [[]]
        var flags: CGEventFlags = []
        if action == "type_text" {
            let text = try request.requiredString("text")
            guard text.utf8.count <= 16_000 else {
                throw DesktopError("Text exceeds the desktop input limit.")
            }
            chunks = try Self.textChunks(text)
            moveCursorToFocus(in: app)
            try requireControl()
            if insertText(text, in: app) {
                clearObservations()
                lastActionTarget = (app, nil)
                return .bool(true)
            }
            code = 0
        } else {
            let key = try request.requiredString("key").lowercased()
            guard let keyCode = Self.keyCodes[key], let modifiers = request["modifiers"]?.arrayValue
            else {
                throw DesktopError("Use a documented key and modifier list.")
            }
            code = CGKeyCode(keyCode)
            for modifier in modifiers {
                switch modifier.stringValue {
                case "command": flags.insert(.maskCommand)
                case "shift": flags.insert(.maskShift)
                case "option": flags.insert(.maskAlternate)
                case "control": flags.insert(.maskControl)
                default: throw DesktopError("Unknown keyboard modifier.")
                }
            }
            try Self.refuseSystemShortcut(key: key, flags: flags)
            moveCursorToFocus(in: app)
        }
        for (index, characters) in chunks.enumerated() {
            if index > 0 { try await Task.sleep(for: .milliseconds(5)) }
            try requireControl()
            let events = try [true, false].map { down in
                guard let event = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down)
                else {
                    throw DesktopError("Keyboard events are unavailable.")
                }
                event.flags = flags
                characters.withUnsafeBufferPointer { text in
                    if let start = text.baseAddress {
                        event.keyboardSetUnicodeString(
                            stringLength: text.count, unicodeString: start)
                    }
                }
                return event
            }
            clearObservations()
            // To the app itself, so typing never lands in whatever the person has in front.
            for event in events {
                SkyLight.post(event, to: app.processIdentifier, authenticated: true)
            }
        }
        lastActionTarget = (app, nil)
        return .bool(true)
    }

    /// Inserts text in one accessibility write where the focused field supports it; terminals
    /// and Chromium ignore such writes, so they get keystrokes.
    private func insertText(_ text: String, in app: NSRunningApplication) -> Bool {
        let keystrokesOnly: Set<String> = [
            "com.apple.Terminal", "net.kovidgoyal.kitty", "com.googlecode.iterm2",
            "com.mitchellh.ghostty", "dev.warp.Warp-Stable", "com.google.Chrome",
            "com.brave.Browser", "com.microsoft.edgemac", "company.thebrowser.Browser",
            "org.chromium.Chromium",
        ]
        guard !keystrokesOnly.contains(app.bundleIdentifier ?? "") else { return false }
        let root = AXUIElementCreateApplication(app.processIdentifier)
        AXUIElementSetMessagingTimeout(root, 0.2)
        var settable: DarwinBoolean = false
        guard let focused: AXUIElement = Self.attribute(root, kAXFocusedUIElementAttribute),
            AXUIElementIsAttributeSettable(
                focused, kAXSelectedTextAttribute as CFString, &settable) == .success,
            settable.boolValue
        else { return false }
        let before: String? = Self.attribute(focused, kAXValueAttribute)
        guard
            AXUIElementSetAttributeValue(
                focused, kAXSelectedTextAttribute as CFString, text as CFString) == .success
        else { return false }
        let after: String? = Self.attribute(focused, kAXValueAttribute)
        return after != before && after?.contains(text) == true
    }

    /// Shortcuts that end the person's session or destroy data are never sent for a Bot.
    static func refuseSystemShortcut(key: String, flags: CGEventFlags) throws {
        let command = flags.contains(.maskCommand)
        let blocked =
            (command && flags.contains(.maskShift) && ["q", "delete"].contains(key))
            || (command && flags.contains(.maskControl) && key == "q")
            || (command && flags.contains(.maskAlternate) && key == "escape")
        if blocked {
            throw DesktopError("That shortcut logs out, locks, force-quits or empties the Trash.")
        }
    }

    static func textChunks(_ text: String) throws -> [[UniChar]] {
        guard text.unicodeScalars.allSatisfy({ $0.value >= 0x20 && $0.value != 0x7F }) else {
            throw DesktopError("Use setValue for multiline text, or pressKey for Return and Tab.")
        }
        // Quartz truncates long event strings. Preserve surrogate pairs at each 20-unit boundary.
        var chunks: [[UniChar]] = []
        var chunk: [UniChar] = []
        for scalar in text.unicodeScalars {
            let units = Array(String(scalar).utf16)
            if chunk.count + units.count > 20 {
                chunks.append(chunk)
                chunk = []
            }
            chunk.append(contentsOf: units)
        }
        if !chunk.isEmpty { chunks.append(chunk) }
        return chunks
    }

    struct PointerTarget: Equatable {
        let pid: pid_t
        let window: Int64
        var screenshotApp: NSRunningApplication? = nil
    }

    static func requirePointerScope(appPID: pid_t, target: PointerTarget?) throws {
        guard target?.pid == appPID else {
            throw DesktopError(
                "That point is not in the captured app. Activate it and capture the app again.")
        }
    }

    /// The frontmost app window under a Quartz point, other than möbius's own.
    static func window(at point: CGPoint) -> PointerTarget? {
        let windows =
            CGWindowListCopyWindowInfo(
                [.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
            as? [[String: Any]] ?? []
        let own = ProcessInfo.processInfo.processIdentifier
        for window in windows {
            guard window[kCGWindowLayer as String] as? Int == 0,
                let pid = window[kCGWindowOwnerPID as String] as? pid_t, pid != own,
                let number = window[kCGWindowNumber as String] as? Int64,
                let bounds = (window[kCGWindowBounds as String] as? NSDictionary)
                    .flatMap({ CGRect(dictionaryRepresentation: $0) }),
                bounds.contains(point)
            else { continue }
            return PointerTarget(pid: pid, window: number)
        }
        return nil
    }

    /// Straight to the app when its window is known, so the system pointer never moves.
    static func deliver(_ event: CGEvent, at point: CGPoint, to target: PointerTarget?) {
        guard let target else {
            event.post(tap: .cghidEventTap)
            return
        }
        SkyLight.set(event, field: 40, to: Int64(target.pid))
        for field: UInt32 in [51, 91, 92] { SkyLight.set(event, field: field, to: target.window) }
        SkyLight.setWindowLocation(event, point)
        SkyLight.post(event, to: target.pid, authenticated: false)
    }

    /// One mouse event; with a target, stamped the way WindowServer stamps real input.
    private func mouse(
        _ type: CGEventType, at point: CGPoint, button: CGMouseButton = .left, state: Int = 0,
        phase: Int64? = nil, target: PointerTarget?
    ) throws {
        if let app = target?.screenshotApp { try requireScreenshotApp(app) }
        guard
            let event = CGEvent(
                mouseEventSource: CGEventSource(stateID: .hidSystemState), mouseType: type,
                mouseCursorPosition: point, mouseButton: button)
        else {
            throw DesktopError("Mouse events are unavailable.")
        }
        event.setIntegerValueField(.mouseEventClickState, value: Int64(state))
        if target != nil {
            if let phase { SkyLight.set(event, field: 0, to: phase) }
            SkyLight.set(event, field: 3, to: Int64(button.rawValue))
            SkyLight.set(event, field: 7, to: 3)
            SkyLight.set(event, field: 58, to: Self.gesture)
        }
        Self.deliver(event, at: point, to: target)
        if point.x >= 0 { moveCursor(to: point) }
    }

    /// Groups the events of one action as a single gesture.
    private static var gesture = Int64(0)

    func number(_ request: JSONValue, _ field: String) throws -> Double {
        let value: Double
        switch request[field] {
        case .number(let number): value = number
        case .integer(let number): value = Double(number)
        case .unsignedInteger(let number): value = Double(number)
        case .decimal(let number): value = NSDecimalNumber(decimal: number).doubleValue
        default: throw DesktopError("\(field) must be a finite number.")
        }
        guard value.isFinite else { throw DesktopError("\(field) must be finite.") }
        return value
    }

    private static let keyCodes: [String: Int] = [
        "return": kVK_Return, "tab": kVK_Tab, "space": kVK_Space, "escape": kVK_Escape,
        "delete": kVK_Delete, "forwarddelete": kVK_ForwardDelete,
        "left": kVK_LeftArrow, "right": kVK_RightArrow, "up": kVK_UpArrow, "down": kVK_DownArrow,
        "home": kVK_Home, "end": kVK_End, "pageup": kVK_PageUp, "pagedown": kVK_PageDown,
        "a": kVK_ANSI_A, "b": kVK_ANSI_B, "c": kVK_ANSI_C, "d": kVK_ANSI_D,
        "e": kVK_ANSI_E, "f": kVK_ANSI_F, "g": kVK_ANSI_G, "h": kVK_ANSI_H,
        "i": kVK_ANSI_I, "j": kVK_ANSI_J, "k": kVK_ANSI_K, "l": kVK_ANSI_L,
        "m": kVK_ANSI_M, "n": kVK_ANSI_N, "o": kVK_ANSI_O, "p": kVK_ANSI_P,
        "q": kVK_ANSI_Q, "r": kVK_ANSI_R, "s": kVK_ANSI_S, "t": kVK_ANSI_T,
        "u": kVK_ANSI_U, "v": kVK_ANSI_V, "w": kVK_ANSI_W, "x": kVK_ANSI_X,
        "y": kVK_ANSI_Y, "z": kVK_ANSI_Z,
        "0": kVK_ANSI_0, "1": kVK_ANSI_1, "2": kVK_ANSI_2, "3": kVK_ANSI_3,
        "4": kVK_ANSI_4, "5": kVK_ANSI_5, "6": kVK_ANSI_6, "7": kVK_ANSI_7,
        "8": kVK_ANSI_8, "9": kVK_ANSI_9,
        "f1": kVK_F1, "f2": kVK_F2, "f3": kVK_F3, "f4": kVK_F4, "f5": kVK_F5,
        "f6": kVK_F6, "f7": kVK_F7, "f8": kVK_F8, "f9": kVK_F9, "f10": kVK_F10,
        "f11": kVK_F11, "f12": kVK_F12,
    ]
}
