import AppKit
import ApplicationServices

extension DesktopRuntime {
    /// A compact outline of the app's usable elements: one line each, `[s3:12] button "Send"`,
    /// indented by depth. With `changes`, only what differs from the previous outline of this
    /// app; with `query`, only rows whose text contains it.
    func inspect(_ app: NSRunningApplication, query: String?, changes: Bool) async throws
        -> JSONValue
    {
        let pid = app.processIdentifier
        let root = AXUIElementCreateApplication(pid)
        await enableWebAccessibility(root, pid: pid)
        elements.removeAll()
        snapshotCount += 1
        let snapshot = "s\(snapshotCount)"
        let deadline = ContinuousClock.now.advanced(by: .seconds(2))
        var queue: [(AXUIElement, Int)] = [(root, 0)]
        var rows: [(id: String, text: String, depth: Int)] = []
        var index = 0
        while index < queue.count, rows.count < 300, ContinuousClock.now < deadline {
            await Task.yield()
            try requireControl()
            let (element, depth) = queue[index]
            index += 1
            AXUIElementSetMessagingTimeout(element, 0.05)
            guard let role: String = Self.attribute(element, kAXRoleAttribute) else { continue }
            if let row = Self.row(element, role: role) {
                let id = "\(snapshot):\(rows.count + 1)"
                elements[id] = Element(
                    value: element, pid: pid, role: role, label: Self.elementLabel(element))
                rows.append((id, row, depth))
            }
            // The menu bar is the same in every app and would crowd out the window's content.
            if depth < 14, queue.count < 1500, role != "AXMenuBar",
                let children: [AXUIElement] = Self.attribute(element, kAXChildrenAttribute)
            {
                queue.append(
                    contentsOf: children.prefix(1500 - queue.count).map { ($0, depth + 1) })
            }
        }
        let previous = outlines[pid]
        outlines[pid] = rows.map(\.text)
        var result: [String: JSONValue] = [
            "pid": .integer(Int64(pid)), "name": .string(app.localizedName ?? "Application"),
            "snapshot": .string(snapshot), "truncated": .bool(index < queue.count),
        ]
        if changes, let previous {
            result["changes"] = .string(Self.changes(from: previous, to: rows))
        } else {
            let needle = query?.lowercased()
            result["tree"] = .string(
                rows.filter { needle == nil || $0.text.lowercased().contains(needle!) }
                    .map {
                        String(repeating: "  ", count: min($0.depth, 8)) + "[\($0.id)] \($0.text)"
                    }
                    .joined(separator: "\n"))
        }
        return .object(result)
    }

    /// Added rows with their new IDs, then removed rows, against the previous outline.
    static func changes(from previous: [String], to rows: [(id: String, text: String, depth: Int)])
        -> String
    {
        var remaining = previous.reduce(into: [String: Int]()) { $0[$1, default: 0] += 1 }
        var added: [String] = []
        for row in rows {
            if let count = remaining[row.text], count > 0 {
                remaining[row.text] = count - 1
            } else {
                added.append("+ [\(row.id)] \(row.text)")
            }
        }
        let removed = remaining.flatMap { text, count in
            Array(repeating: "- \(text)", count: count)
        }
        let lines = added + removed.sorted()
        return lines.isEmpty
            ? "No change since the previous inspect." : lines.joined(separator: "\n")
    }

    /// One outline line for an element worth acting on or reading, or nil for structure.
    static func row(_ element: AXUIElement, role: String) -> String? {
        let actions: [String] = {
            var names: CFArray?
            guard AXUIElementCopyActionNames(element, &names) == .success,
                let list = names as? [String]
            else { return [] }
            let ignored: Set<String> = [
                "AXShowMenu", "AXScrollToVisible", "AXShowDefaultUI", "AXShowAlternateUI",
                "AXRaise",
            ]
            return list.filter { !ignored.contains($0) && $0.hasPrefix("AX") }
                .map { String($0.dropFirst(2)).lowercased() }
        }()
        let title: String? = attribute(element, kAXTitleAttribute)
        let description: String? = attribute(element, kAXDescriptionAttribute)
        let label = [title, description].compactMap { $0 }.first { !$0.isEmpty }
        var value: String? = nil
        if role != "AXSecureTextField", let text: String = attribute(element, kAXValueAttribute),
            !text.isEmpty
        {
            value = String(text.prefix(200))
        }
        let textual: Set<String> = [
            "AXStaticText", "AXHeading", "AXTextField", "AXTextArea", "AXWindow", "AXLink",
            "AXCheckBox", "AXRadioButton", "AXPopUpButton", "AXComboBox", "AXMenuItem",
            "AXSecureTextField",
        ]
        guard !actions.isEmpty || (textual.contains(role) && (label != nil || value != nil)) else {
            return nil
        }
        var line = String(role.dropFirst(2)).lowercased()
        if let label { line += " \"\(label.prefix(120))\"" }
        if let value, value != label { line += " = \"\(value)\"" }
        if let enabled: Bool = attribute(element, kAXEnabledAttribute), !enabled {
            line += " disabled"
        }
        if !actions.isEmpty { line += " (\(actions.joined(separator: ", ")))" }
        return line
    }

    /// Chromium and Electron build their web accessibility only when asked.
    private func enableWebAccessibility(_ root: AXUIElement, pid: pid_t) async {
        guard !webAccessible.contains(pid) else { return }
        webAccessible.insert(pid)
        let enabled =
            AXUIElementSetAttributeValue(root, "AXManualAccessibility" as CFString, kCFBooleanTrue)
            == .success
            || AXUIElementSetAttributeValue(
                root, "AXEnhancedUserInterface" as CFString, kCFBooleanTrue) == .success
        if enabled { try? await Task.sleep(for: .milliseconds(300)) }
    }

    func accessibilityAction(_ request: JSONValue, action: String) throws -> JSONValue {
        try fields(request, allowed: action == "press" ? ["elementId"] : ["elementId", "text"])
        let id = try request.requiredString("elementId")
        guard let element = elements[id],
            let app = NSRunningApplication(processIdentifier: element.pid), !app.isTerminated,
            Self.attribute(element.value, kAXRoleAttribute) as String? == element.role,
            Self.elementLabel(element.value) == element.label
        else {
            throw DesktopError("This UI observation is stale. Inspect the app again before acting.")
        }
        let result: AXError
        if action == "set_value" {
            let text = try request.requiredString("text")
            guard text.utf8.count <= 16_000 else {
                throw DesktopError("Text exceeds the desktop input limit.")
            }
            var settable = DarwinBoolean(false)
            guard
                AXUIElementIsAttributeSettable(
                    element.value, kAXValueAttribute as CFString, &settable) == .success,
                settable.boolValue
            else { throw DesktopError("This control does not accept a text value.") }
            moveCursor(to: element.value)
            clearObservations()
            result = AXUIElementSetAttributeValue(
                element.value, kAXValueAttribute as CFString, text as CFTypeRef)
        } else {
            moveCursor(to: element.value)
            clearObservations()
            result = AXUIElementPerformAction(element.value, kAXPressAction as CFString)
        }
        guard result == .success else {
            throw DesktopError(
                "The app reported accessibility error \(result.rawValue). Inspect its state before continuing; the action may have completed."
            )
        }
        lastActionTarget = (app, nil)
        return .bool(true)
    }

    func moveCursor(to element: AXUIElement) {
        AXUIElementSetMessagingTimeout(element, 0.05)
        guard
            let point = Self.elementCenter(
                position: Self.attribute(element, kAXPositionAttribute),
                size: Self.attribute(element, kAXSizeAttribute))
        else { return }
        moveCursor(to: point)
    }

    func moveCursorToFocus(in app: NSRunningApplication) {
        let root = AXUIElementCreateApplication(app.processIdentifier)
        AXUIElementSetMessagingTimeout(root, 0.05)
        if let focused: AXUIElement = Self.attribute(root, kAXFocusedUIElementAttribute) {
            moveCursor(to: focused)
        }
    }

    static func elementCenter(position: AXValue?, size: AXValue?) -> CGPoint? {
        var origin = CGPoint.zero
        var dimensions = CGSize.zero
        guard let position, let size,
            AXValueGetValue(position, .cgPoint, &origin),
            AXValueGetValue(size, .cgSize, &dimensions),
            dimensions.width.isFinite, dimensions.height.isFinite,
            dimensions.width > 0, dimensions.height > 0
        else { return nil }
        let point = CGPoint(
            x: origin.x + dimensions.width / 2, y: origin.y + dimensions.height / 2)
        return point.x.isFinite && point.y.isFinite ? point : nil
    }

    static func attribute<T>(_ element: AXUIElement, _ name: String) -> T? {
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success else {
            return nil
        }
        return value as? T
    }

    static func elementLabel(_ element: AXUIElement) -> String {
        let title: String? = attribute(element, kAXTitleAttribute)
        let description: String? = attribute(element, kAXDescriptionAttribute)
        let identifier: String? = attribute(element, kAXIdentifierAttribute)
        return String(
            [title, description, identifier].compactMap { $0 }.filter { !$0.isEmpty }.joined(
                separator: " · "
            ).prefix(1000))
    }
}
