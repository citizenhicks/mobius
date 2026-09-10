import AppKit
import ApplicationServices

extension DesktopRuntime {
    func inspect(_ app: NSRunningApplication) async throws -> JSONValue {
        elements.removeAll()
        let root = AXUIElementCreateApplication(app.processIdentifier)
        let deadline = ContinuousClock.now.advanced(by: .seconds(2))
        var queue: [(AXUIElement, Int, String?)] = [(root, 0, nil)]
        var records: [JSONValue] = []
        var index = 0
        while index < queue.count, records.count < 300, ContinuousClock.now < deadline {
            await Task.yield()
            try requireControl()
            let (element, depth, parent) = queue[index]
            index += 1
            AXUIElementSetMessagingTimeout(element, 0.05)
            guard let role: String = Self.attribute(element, kAXRoleAttribute) else { continue }
            let label = Self.elementLabel(element)
            let id = UUID().uuidString
            elements[id] = Element(
                value: element, pid: app.processIdentifier, role: role, label: label)
            var record: [String: JSONValue] = [
                "elementId": .string(id), "role": .string(role), "label": .string(label),
                "depth": .integer(Int64(depth)), "parentId": parent.map(JSONValue.string) ?? .null,
            ]
            if role != "AXSecureTextField",
                let value: String = Self.attribute(element, kAXValueAttribute)
            {
                record["value"] = .string(String(value.prefix(2000)))
            }
            if let enabled: Bool = Self.attribute(element, kAXEnabledAttribute) {
                record["enabled"] = .bool(enabled)
            }
            var actions: CFArray?
            if AXUIElementCopyActionNames(element, &actions) == .success,
                let names = actions as? [String]
            {
                record["actions"] = .array(names.prefix(16).map(JSONValue.string))
            }
            records.append(.object(record))
            if depth < 12, queue.count < 600,
                let children: [AXUIElement] = Self.attribute(element, kAXChildrenAttribute)
            {
                queue.append(
                    contentsOf: children.prefix(600 - queue.count).map { ($0, depth + 1, id) })
            }
        }
        return .object([
            "pid": .integer(Int64(app.processIdentifier)),
            "name": .string(app.localizedName ?? "Application"),
            "elements": .array(records), "truncated": .bool(index < queue.count),
            "note": .string(
                "Element IDs belong to this observation. Inspect again after an action changes the UI."
            ),
        ])
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
            clearObservations()
            result = AXUIElementSetAttributeValue(
                element.value, kAXValueAttribute as CFString, text as CFTypeRef)
        } else {
            clearObservations()
            result = AXUIElementPerformAction(element.value, kAXPressAction as CFString)
        }
        guard result == .success else {
            throw DesktopError(
                "The app reported accessibility error \(result.rawValue). Inspect its state before continuing; the action may have completed."
            )
        }
        return .bool(true)
    }

    static func attribute<T>(_ element: AXUIElement, _ name: String) -> T? {
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success else {
            return nil
        }
        return value as? T
    }

    private static func elementLabel(_ element: AXUIElement) -> String {
        let title: String? = attribute(element, kAXTitleAttribute)
        let description: String? = attribute(element, kAXDescriptionAttribute)
        let identifier: String? = attribute(element, kAXIdentifierAttribute)
        return String(
            [title, description, identifier].compactMap { $0 }.filter { !$0.isEmpty }.joined(
                separator: " · "
            ).prefix(1000))
    }
}
