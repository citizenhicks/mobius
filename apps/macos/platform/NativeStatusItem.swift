import AppKit

/// The menu bar item that keeps möbius reachable while it runs in the background: closing
/// the window hides the app and its Dock icon instead of quitting, so the gateway
/// connection, running chats and notifications carry on.
@MainActor
final class NativeStatusItem: NSObject {
    static let shared = NativeStatusItem()
    private var item: NSStatusItem?
    private var usage: [NSMenuItem] = []

    /// Installs the item once, drawing `icon` (SVG) as a template image.
    func install(icon: Data) {
        guard item == nil else { return }
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        if let image = NSImage(data: icon) {
            image.isTemplate = true
            image.size = NSSize(width: 18, height: 18)
            item.button?.image = image
        }
        item.button?.setAccessibilityLabel("möbius")
        let menu = NSMenu()
        menu.addItem(entry("Open möbius", #selector(open)))
        menu.addItem(entry("New Chat", #selector(newChat)))
        menu.addItem(.separator())
        menu.addItem(entry("Quit möbius", #selector(quit), key: "q"))
        item.menu = menu
        self.item = item
    }

    private func entry(_ title: String, _ action: Selector, key: String = "") -> NSMenuItem {
        let entry = NSMenuItem(title: title, action: action, keyEquivalent: key)
        entry.target = self
        return entry
    }

    /// Replaces the read-only usage lines above the menu's actions.
    func setUsage(_ lines: [String]) {
        guard let menu = item?.menu else { return }
        for entry in usage { menu.removeItem(entry) }
        usage = lines.map { line in
            let entry = NSMenuItem(title: line, action: nil, keyEquivalent: "")
            entry.isEnabled = false
            return entry
        }
        if !usage.isEmpty { usage.append(.separator()) }
        for (index, entry) in usage.enumerated() { menu.insertItem(entry, at: index) }
    }

    @objc private func open() { show() }

    @objc private func newChat() {
        show()
        NativeBridge.shared.emit(.object(["type": .string("status_new_chat")]))
    }

    @objc private func quit() { NSApp.terminate(nil) }

    /// Brings the app back: its Dock icon, its window (reopened when it was closed), focus.
    func show() {
        restore()
        if !NSApp.windows.contains(where: { $0.isVisible }) {
            _ = NSApp.delegate?.applicationShouldHandleReopen?(NSApp, hasVisibleWindows: false)
        }
        NSApp.activate()
    }

    /// Brings back the Dock icon and the hidden window without asking for a reopen.
    func restore() {
        NSApp.setActivationPolicy(.regular)
        NSApp.unhide(nil)
    }

    /// Keeps running in the menu bar only: the window hides and the Dock icon goes.
    func background() {
        NSApp.hide(nil)
        NSApp.setActivationPolicy(.accessory)
    }
}

@_cdecl("mobius_native_status_item")
public func nativeStatusItem(_ icon: UnsafePointer<UInt8>, _ length: Int) {
    let data = Data(bytes: icon, count: length)
    MainActor.assumeIsolated { NativeStatusItem.shared.install(icon: data) }
}

@_cdecl("mobius_native_status_usage")
public func nativeStatusUsage(_ json: UnsafePointer<CChar>) {
    let lines =
        (try? JSONDecoder().decode([String].self, from: Data(String(cString: json).utf8))) ?? []
    MainActor.assumeIsolated { NativeStatusItem.shared.setUsage(lines) }
}

@_cdecl("mobius_native_background")
public func nativeBackground() {
    MainActor.assumeIsolated { NativeStatusItem.shared.background() }
}

@_cdecl("mobius_native_reopen")
public func nativeReopen() {
    MainActor.assumeIsolated { NativeStatusItem.shared.show() }
}

/// Hands a second launch to the möbius already running for this user: macOS reopens its
/// window, and this process exits before it starts a gateway. Returns whether one took over.
@_cdecl("mobius_native_restore")
public func nativeRestore() {
    MainActor.assumeIsolated { NativeStatusItem.shared.restore() }
}

@_cdecl("mobius_native_hand_off_launch")
public func nativeHandOffLaunch() -> Bool {
    guard let identifier = Bundle.main.bundleIdentifier else { return false }
    let current = ProcessInfo.processInfo.processIdentifier
    guard
        let running = NSRunningApplication.runningApplications(
            withBundleIdentifier: identifier
        ).first(where: { $0.processIdentifier != current }),
        let bundle = running.bundleURL
    else { return false }
    let opened = DispatchSemaphore(value: 0)
    NSWorkspace.shared.openApplication(at: bundle, configuration: NSWorkspace.OpenConfiguration()) {
        _, _ in opened.signal()
    }
    _ = opened.wait(timeout: .now() + 2)
    return true
}
