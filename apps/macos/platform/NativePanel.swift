import AppKit
import Quartz

@MainActor
final class NativePanel {
    private var preview: QLPreviewView?
    private var previewLocation: String?
    private var activeID: String?
    private var computerOwner: NSGlassEffectView?
    let terminals: NativeTerminals
    let browsers = NativeBrowsers()
    private var clickMonitor: Any?

    init(emit: @escaping (JSONValue) -> Void, terminals: NativeTerminals? = nil) {
        self.terminals = terminals ?? NativeTerminals(emit: emit)
    }

    /// A click on GPUI's own view takes the keyboard back from a preview or terminal,
    /// which keep first responder otherwise.
    private func installClickMonitor() {
        guard clickMonitor == nil else { return }
        clickMonitor = NSEvent.addLocalMonitorForEvents(matching: .leftMouseDown) { event in
            MainActor.assumeIsolated { Self.focusClickedApp(event) }
            return event
        }
    }

    static func focusClickedApp(_ event: NSEvent) {
        guard let window = event.window, let content = window.contentView,
            let app = appView(in: window), window.firstResponder !== app,
            let hit = content.hitTest(content.convert(event.locationInWindow, from: nil)),
            hit === app || hit.isDescendant(of: app)
        else { return }
        window.makeFirstResponder(app)
    }

    /// Shows a terminal tab; its shell starts the first time.
    func showTerminal(_ tabID: String, launch: TerminalLaunch, frame: CGRect, in host: NSView) {
        let frame = CGRect(
            x: frame.minX,
            y: host.isFlipped ? frame.minY : host.bounds.height - frame.maxY,
            width: frame.width, height: frame.height)
        guard frame.width > 0, frame.height > 0 else {
            hide()
            return
        }
        if activeID != tabID { hide() }
        activeID = tabID
        installClickMonitor()
        terminals.show(tabID, launch: launch, frame: frame, in: host)
    }

    /// Shows a browser tab's page over `frame`.
    func showBrowser(_ tabID: String, frame: CGRect, in host: NSView) {
        let frame = CGRect(
            x: frame.minX,
            y: host.isFlipped ? frame.minY : host.bounds.height - frame.maxY,
            width: frame.width, height: frame.height)
        guard frame.width > 0, frame.height > 0 else {
            hide()
            return
        }
        if activeID != tabID { hide() }
        activeID = tabID
        installClickMonitor()
        browsers.show(tabID, frame: frame, in: host)
    }

    /// Shows the local file at `location` through QuickLook over `frame`.
    func show(_ tabID: String, location: String, frame: CGRect, in host: NSView) throws {
        let frame = CGRect(
            x: frame.minX,
            y: host.isFlipped ? frame.minY : host.bounds.height - frame.maxY,
            width: frame.width, height: frame.height)
        guard frame.width > 0, frame.height > 0 else {
            hide()
            return
        }
        if activeID != tabID { hide() }
        installClickMonitor()
        guard location.hasPrefix("/"), FileManager.default.fileExists(atPath: location) else {
            throw DesktopError("Choose an existing file to preview.")
        }
        guard let quickLook = preview ?? QLPreviewView(frame: frame, style: .normal) else {
            throw DesktopError("The file preview could not be opened.")
        }
        preview = quickLook
        if previewLocation != location {
            quickLook.previewItem = URL(fileURLWithPath: location) as NSURL
            previewLocation = location
        }
        activeID = tabID
        if quickLook.superview !== host {
            quickLook.removeFromSuperview()
            host.addSubview(quickLook)
        }
        quickLook.frame = frame
        quickLook.isHidden = false
    }

    /// Starts QuickLook on `location` behind the scenes, so the preview is drawn by the time
    /// the pane settles and `show` reveals it.
    func preload(_ location: String, in host: NSView) {
        guard previewLocation != location, location.hasPrefix("/"),
            let quickLook = preview ?? QLPreviewView(frame: host.bounds, style: .normal)
        else { return }
        preview = quickLook
        quickLook.isHidden = true
        if quickLook.superview !== host {
            quickLook.removeFromSuperview()
            host.addSubview(quickLook)
        }
        quickLook.previewItem = URL(fileURLWithPath: location) as NSURL
        previewLocation = location
    }

    func hide() {
        computerOwner?.isHidden = true
        if let preview {
            releaseFocus(from: preview)
            preview.isHidden = true
        }
        terminals.hide()
        browsers.hide()
    }

    func close(_ tabID: String) {
        terminals.close(tabID)
        browsers.close(tabID)
        if activeID == tabID {
            computerOwner?.isHidden = true
            activeID = nil
            if let preview { releaseFocus(from: preview) }
            preview?.isHidden = true
            preview?.previewItem = nil
            previewLocation = nil
        }
    }

    func reset() {
        computerOwner?.removeFromSuperview()
        computerOwner = nil
        terminals.reset()
        browsers.reset()
        if let preview { releaseFocus(from: preview) }
        preview?.removeFromSuperview()
        preview = nil
        previewLocation = nil
        activeID = nil
    }

    /// Native glass stays above Chromium; the app owns the label and control state.
    func showComputerOwner(
        _ text: String, owner: Bool, tint: NSColor, foreground: NSColor,
        frame: CGRect, in host: NSView
    ) {
        let glass = computerOwner ?? NSGlassEffectView()
        let content = glass.contentView ?? NSView()
        let label = content.subviews.first as? NSTextField ?? NSTextField(labelWithString: text)
        if label.superview == nil {
            label.translatesAutoresizingMaskIntoConstraints = false
            label.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
            content.addSubview(label)
            NSLayoutConstraint.activate([
                label.centerXAnchor.constraint(equalTo: content.centerXAnchor),
                label.centerYAnchor.constraint(equalTo: content.centerYAnchor),
                label.leadingAnchor.constraint(
                    greaterThanOrEqualTo: content.leadingAnchor, constant: 12),
                label.trailingAnchor.constraint(
                    lessThanOrEqualTo: content.trailingAnchor, constant: -12),
            ])
        }
        label.stringValue = text
        label.font = .systemFont(ofSize: 12)
        label.textColor = foreground
        label.alignment = .center
        label.lineBreakMode = .byTruncatingTail
        let height = frame.height
        let width = min(frame.width, label.intrinsicContentSize.width + 24)
        guard width > 0, height > 0 else { glass.isHidden = true; return }
        glass.frame = CGRect(
            x: frame.maxX - width,
            y: host.isFlipped ? frame.minY : host.bounds.height - frame.maxY,
            width: width, height: height)
        glass.cornerRadius = height / 2
        glass.style = .regular
        glass.tintColor = owner ? tint : nil
        content.frame = CGRect(x: 0, y: 0, width: width, height: height)
        glass.contentView = content
        if glass.superview !== host || host.subviews.last !== glass {
            host.addSubview(glass, positioned: .above, relativeTo: nil)
        }
        glass.isHidden = false
        computerOwner = glass
    }
}

@_cdecl("mobius_native_computer_owner")
public func nativeComputerOwner(
    _ text: UnsafePointer<CChar>, _ owner: Bool, _ tint: UInt32, _ foreground: UInt32,
    _ x: Double, _ y: Double, _ width: Double, _ height: Double
) {
    let text = String(cString: text)
    func color(_ rgb: UInt32) -> NSColor {
        NSColor(
            srgbRed: Double((rgb >> 16) & 255) / 255,
            green: Double((rgb >> 8) & 255) / 255, blue: Double(rgb & 255) / 255, alpha: 1)
    }
    showInHost(x, y, width, height) { frame, host in
        NativeBridge.shared.panel.showComputerOwner(
            text, owner: owner, tint: color(tint), foreground: color(foreground),
            frame: frame, in: host)
    }
}

@_cdecl("mobius_native_panel")
public func nativePanel(
    _ tabID: UnsafePointer<CChar>, _ location: UnsafePointer<CChar>,
    _ x: Double, _ y: Double, _ width: Double, _ height: Double
) {
    let tabID = String(cString: tabID)
    let location = String(cString: location)
    showInHost(x, y, width, height) { frame, host in
        try NativeBridge.shared.panel.show(tabID, location: location, frame: frame, in: host)
    }
}

@_cdecl("mobius_native_preload_panel")
public func nativePreloadPanel(_ location: UnsafePointer<CChar>) {
    let location = String(cString: location)
    MainActor.assumeIsolated {
        guard let host = hostView() else { return }
        NativeBridge.shared.panel.preload(location, in: host)
    }
}

/// Runs `show` on the main thread with a frame in the host view; a thrown error becomes a
/// toast.
func showInHost(
    _ x: Double, _ y: Double, _ width: Double, _ height: Double,
    _ show: @MainActor (CGRect, NSView) throws -> Void
) {
    MainActor.assumeIsolated {
        guard let host = hostView() else { return }
        do {
            try show(CGRect(x: x, y: y, width: width, height: height), host)
        } catch { NativeBridge.shared.fail(error) }
    }
}

/// The window hosting native panels: GPUI's, preferring the main or key one. An inactive
/// app has neither, and its first visible window may be the menu bar item's.
@MainActor
func hostView() -> NSView? {
    let app = NSApplication.shared
    let windows = [app.mainWindow, app.keyWindow].compactMap { $0 } + app.windows
    return windows.first { appView(in: $0) != nil }?.contentView
}

@_cdecl("mobius_native_close_panel")
public func nativeClosePanel(_ tabID: UnsafePointer<CChar>) {
    let tabID = String(cString: tabID)
    MainActor.assumeIsolated { NativeBridge.shared.panel.close(tabID) }
}

@_cdecl("mobius_native_reset_panels")
public func nativeResetPanels() { MainActor.assumeIsolated { NativeBridge.shared.panel.reset() } }

@_cdecl("mobius_native_hide_panel")
public func nativeHidePanel() { MainActor.assumeIsolated { NativeBridge.shared.panel.hide() } }

/// GPUI's view: a subview of the content view, beside the native panels.
@MainActor
func appView(in window: NSWindow) -> NSView? {
    window.contentView?.subviews.first { $0.className == "GPUIView" }
}

/// Hands the keyboard back to GPUI when `view`, or anything inside it, holds it. GPUI's view
/// never takes first responder from a click, so text fields stay deaf until this runs.
@MainActor
func releaseFocus(from view: NSView) {
    guard let window = view.window, let responder = window.firstResponder as? NSView,
        responder === view || responder.isDescendant(of: view), let app = appView(in: window)
    else { return }
    window.makeFirstResponder(app)
}
