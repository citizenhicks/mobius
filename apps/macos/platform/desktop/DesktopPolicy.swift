import AppKit

/// The person's answer when a Bot first uses an app.
enum DesktopAppChoice: String {
    case always
    case once
    case deny
}

/// Which apps a Bot may use. Each app is asked about once (Always, Once, Don't Allow);
/// apps that hold system settings or secrets are never controlled.
extension DesktopRuntime {
    static let neverControlled: Set<String> = [
        "com.apple.systempreferences", "com.apple.SecurityAgent", "com.apple.loginwindow",
        "com.apple.keychainaccess", "com.apple.Passwords", "com.apple.UserNotificationCenter",
    ]
    private static let choicesKey = "desktop-control-apps"

    /// Remembered answers: bundle identifier to "always" or "deny", with the app's name.
    var appChoices: [String: (choice: DesktopAppChoice, name: String)] {
        let stored = defaults.dictionary(forKey: Self.choicesKey) as? [String: [String]] ?? [:]
        return stored.compactMapValues { pair in
            guard pair.count == 2, let choice = DesktopAppChoice(rawValue: pair[0]) else {
                return nil
            }
            return (choice, pair[1])
        }
    }

    /// "Always: Safari, kitty · Not allowed: Mail", or nil before any choice.
    var appChoiceSummary: String? {
        let choices = appChoices.values
        let names = { (choice: DesktopAppChoice) in
            choices.filter { $0.choice == choice }.map(\.name).sorted().joined(separator: ", ")
        }
        let parts = [("Always", names(.always)), ("Not allowed", names(.deny))]
            .filter { !$0.1.isEmpty }.map { "\($0.0): \($0.1)" }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    func forgetAppChoices() {
        defaults.removeObject(forKey: Self.choicesKey)
        onceApps.removeAll()
        clearObservations()
        lastActionTarget = nil
        notifyIfChanged()
    }

    /// The key an app's consent is remembered under.
    static func consentKey(_ app: NSRunningApplication) -> String {
        app.bundleIdentifier ?? "pid:\(app.processIdentifier)"
    }

    /// Recheck retained app identity and consent after asynchronous capture without prompting.
    func requireScreenshotApp(_ app: NSRunningApplication) throws {
        guard !app.isTerminated, app.processIdentifier != ProcessInfo.processInfo.processIdentifier,
            (try? rememberedConsent(bundle: Self.consentKey(app), name: "")) == true
        else {
            throw DesktopError(
                "The app is no longer available or allowed. Use desktop.screenshotApp(pid) again.")
        }
    }

    func authorize(pid: pid_t) async throws {
        guard let app = NSRunningApplication(processIdentifier: pid) else {
            throw DesktopError("Choose a running app from desktop.apps().")
        }
        try await authorize(bundle: Self.consentKey(app), name: app.localizedName ?? "This app")
    }

    /// The person's standing answer, without prompting: throws for apps never controlled or
    /// refused, and returns whether an Always or Once answer already allows the app.
    private func rememberedConsent(bundle: String, name: String) throws -> Bool {
        if Self.neverControlled.contains(bundle) || bundle == Bundle.main.bundleIdentifier {
            throw DesktopError(
                "Bots can’t use \(name): it holds system settings, passwords or möbius itself.")
        }
        switch appChoices[bundle]?.choice {
        case .always: return true
        case .deny: throw DesktopError("The person doesn’t allow Bots to use \(name).")
        case .once, nil: return onceApps.contains(bundle)
        }
    }

    func authorize(bundle: String, name: String) async throws {
        try Task.checkCancellation()
        if try rememberedConsent(bundle: bundle, name: name) { return }
        let choice = await askToUse(name)
        try Task.checkCancellation()
        switch choice {
        case .always:
            var stored = defaults.dictionary(forKey: Self.choicesKey) ?? [:]
            stored[bundle] = [DesktopAppChoice.always.rawValue, name]
            defaults.set(stored, forKey: Self.choicesKey)
            notifyIfChanged()
        case .once:
            onceApps.insert(bundle)
        case .deny:
            var stored = defaults.dictionary(forKey: Self.choicesKey) ?? [:]
            stored[bundle] = [DesktopAppChoice.deny.rawValue, name]
            defaults.set(stored, forKey: Self.choicesKey)
            notifyIfChanged()
            throw DesktopError("The person doesn’t allow Bots to use \(name).")
        }
    }

    static func promptToUse(_ name: String) async -> DesktopAppChoice {
        let alert = NSAlert()
        alert.messageText = "Let a Bot use \(name)?"
        alert.informativeText =
            "It will see \(name) and use its controls with its own pointer. You can change this in Settings → Security."
        alert.addButton(withTitle: "Always Allow")
        alert.addButton(withTitle: "Allow Once")
        alert.addButton(withTitle: "Don’t Allow")
        NSApp.activate()
        switch alert.runModal() {
        case .alertFirstButtonReturn: return .always
        case .alertSecondButtonReturn: return .once
        default: return .deny
        }
    }
}
