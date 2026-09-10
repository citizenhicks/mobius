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

    func observedPoint(_ request: JSONValue, x: String = "x", y: String = "y") throws -> CGPoint {
        let id = try request.requiredString("screenshotId")
        guard let screenshot = screenshots[id],
            screenshot.captured.duration(to: .now) < .seconds(60),
            CGDisplayIsActive(screenshot.displayID) != 0,
            CGDisplayBounds(screenshot.displayID) == screenshot.frame,
            NSWorkspace.shared.frontmostApplication?.processIdentifier == screenshot.frontmostPID
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
        if action == "drag" {
            let destination = try observedPoint(request, x: "toX", y: "toY")
            clearObservations()
            try mouse(.leftMouseDown, at: point)
            defer { try? mouse(.leftMouseUp, at: CGEvent(source: nil)?.location ?? point) }
            for step in 1...10 {
                try await Task.sleep(for: .milliseconds(20))
                try requireControl()
                let fraction = Double(step) / 10
                try mouse(
                    .leftMouseDragged,
                    at: CGPoint(
                        x: point.x + (destination.x - point.x) * fraction,
                        y: point.y + (destination.y - point.y) * fraction))
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
            try mouse(.mouseMoved, at: point)
            event.location = point
            event.post(tap: .cghidEventTap)
        } else if action == "move" {
            try mouse(.mouseMoved, at: point)
        } else {
            guard let buttonName = request["button"]?.stringValue,
                ["left", "right"].contains(buttonName), let clicks = request["clicks"]?.intValue,
                (1...2).contains(clicks)
            else { throw DesktopError("Use a left or right click, with a count of one or two.") }
            let button: CGMouseButton = buttonName == "right" ? .right : .left
            clearObservations()
            for count in 1...clicks {
                try mouse(
                    button == .right ? .rightMouseDown : .leftMouseDown, at: point, button: button,
                    count: count)
                try mouse(
                    button == .right ? .rightMouseUp : .leftMouseUp, at: point, button: button,
                    count: count)
            }
        }
        return .bool(true)
    }

    func keyboardAction(_ request: JSONValue, action: String) async throws -> JSONValue {
        try fields(
            request,
            allowed: action == "type_text" ? ["pid", "text"] : ["pid", "key", "modifiers"])
        let app = try application(request)
        let code: CGKeyCode
        var chunks: [[UniChar]] = [[]]
        var flags: CGEventFlags = []
        if action == "type_text" {
            let text = try request.requiredString("text")
            guard text.utf8.count <= 16_000 else {
                throw DesktopError("Text exceeds the desktop input limit.")
            }
            chunks = try Self.textChunks(text)
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
        }
        for (index, characters) in chunks.enumerated() {
            if index > 0 { try await Task.sleep(for: .milliseconds(5)) }
            try requireControl()
            guard
                NSWorkspace.shared.frontmostApplication?.processIdentifier == app.processIdentifier
            else {
                throw DesktopError(
                    "The selected app is not frontmost. Activate and inspect it before typing.")
            }
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
            for event in events { event.post(tap: .cghidEventTap) }
        }
        return .bool(true)
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

    private func mouse(
        _ type: CGEventType, at point: CGPoint, button: CGMouseButton = .left, count: Int = 1
    ) throws {
        guard
            let event = CGEvent(
                mouseEventSource: nil, mouseType: type, mouseCursorPosition: point,
                mouseButton: button)
        else {
            throw DesktopError("Mouse events are unavailable.")
        }
        event.setIntegerValueField(.mouseEventClickState, value: Int64(count))
        event.post(tap: .cghidEventTap)
    }

    private func number(_ request: JSONValue, _ field: String) throws -> Double {
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
