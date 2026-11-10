import AppKit

/// The view a browser tab's Chromium page draws into; it keeps the page filling it.
final class BrowserContainer: NSView {
    override var isFlipped: Bool { true }

    override func didAddSubview(_ subview: NSView) {
        super.didAddSubview(subview)
        subview.frame = bounds
    }

    override func resizeSubviews(withOldSize oldSize: NSSize) {
        for view in subviews { view.frame = bounds }
    }
}

/// Containers for browser tabs, kept until their tab closes. Chromium owns what is inside.
@MainActor
final class NativeBrowsers {
    private var containers: [String: BrowserContainer] = [:]
    private var keyMonitor: Any?

    /// The tab's container, added hidden to `host` the first time it is asked for.
    func container(_ tabID: String, in host: NSView) -> NSView {
        if let container = containers[tabID] { return container }
        let container = BrowserContainer(frame: host.bounds)
        container.isHidden = true
        host.addSubview(container)
        containers[tabID] = container
        installKeyMonitor()
        return container
    }

    func show(_ tabID: String, frame: CGRect, in host: NSView) {
        let view = container(tabID, in: host)
        if view.superview !== host {
            view.removeFromSuperview()
            host.addSubview(view)
        }
        view.frame = frame
        view.isHidden = false
    }

    /// Gives the page the keyboard, as a click on it would.
    func focus(_ tabID: String) {
        guard let container = containers[tabID], !container.isHidden,
            let page = container.subviews.first
        else { return }
        container.window?.makeFirstResponder(page)
    }

    func hide() {
        for container in containers.values where !container.isHidden {
            releaseFocus(from: container)
            container.isHidden = true
        }
    }

    func close(_ tabID: String) {
        guard let container = containers.removeValue(forKey: tabID) else { return }
        releaseFocus(from: container)
        container.removeFromSuperview()
        if containers.isEmpty { removeKeyMonitor() }
    }

    func reset() {
        for tabID in Array(containers.keys) { close(tabID) }
    }

    /// Typing reaches a focused page directly, so GPUI's key equivalents never take Escape,
    /// arrows or Control keys from it; ⌘ shortcuts still reach the menu and the page.
    private func installKeyMonitor() {
        guard keyMonitor == nil else { return }
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self else { return event }
            return self.route(event)
        }
    }

    private func removeKeyMonitor() {
        if let keyMonitor { NSEvent.removeMonitor(keyMonitor) }
        keyMonitor = nil
    }

    func route(_ event: NSEvent) -> NSEvent? {
        guard let responder = event.window?.firstResponder as? NSView,
            !responder.isHiddenOrHasHiddenAncestor,
            containers.values.contains(where: { responder.isDescendant(of: $0) }),
            !event.modifierFlags.contains(.command)
        else { return event }
        responder.keyDown(with: event)
        return nil
    }
}

@_cdecl("mobius_native_browser_container")
public func nativeBrowserContainer(_ tabID: UnsafePointer<CChar>) -> UnsafeMutableRawPointer? {
    let tabID = String(cString: tabID)
    // The address crosses the isolation boundary; the view stays owned by its container list.
    let address: UInt? = MainActor.assumeIsolated {
        guard let host = hostView() else { return nil }
        let container = NativeBridge.shared.panel.browsers.container(tabID, in: host)
        return UInt(bitPattern: Unmanaged.passUnretained(container).toOpaque())
    }
    return address.flatMap { UnsafeMutableRawPointer(bitPattern: $0) }
}

@_cdecl("mobius_native_browser")
public func nativeBrowser(
    _ tabID: UnsafePointer<CChar>, _ x: Double, _ y: Double, _ width: Double, _ height: Double
) {
    let tabID = String(cString: tabID)
    showInHost(x, y, width, height) { frame, host in
        NativeBridge.shared.panel.showBrowser(tabID, frame: frame, in: host)
    }
}

@_cdecl("mobius_native_focus_app")
public func nativeFocusApp() {
    MainActor.assumeIsolated {
        guard let window = hostView()?.window, let app = appView(in: window) else { return }
        window.makeFirstResponder(app)
    }
}

@_cdecl("mobius_native_focus_browser")
public func nativeFocusBrowser(_ tabID: UnsafePointer<CChar>) {
    let tabID = String(cString: tabID)
    MainActor.assumeIsolated { NativeBridge.shared.panel.browsers.focus(tabID) }
}
