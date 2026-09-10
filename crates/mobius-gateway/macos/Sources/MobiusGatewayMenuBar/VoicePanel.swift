import AppKit
import Observation
import SwiftUI

enum VoiceCorner: String, CaseIterable, Identifiable {
    case topLeft, topRight, bottomLeft, bottomRight

    var id: String { rawValue }
    var title: String {
        switch self {
        case .topLeft: "Top left"
        case .topRight: "Top right"
        case .bottomLeft: "Bottom left"
        case .bottomRight: "Bottom right"
        }
    }

    func frame(for size: CGSize, in visibleFrame: CGRect) -> CGRect {
        let bounds = visibleFrame.insetBy(dx: MobiusSpace.l, dy: MobiusSpace.l)
        let size = CGSize(
            width: min(size.width, bounds.width), height: min(size.height, bounds.height))
        let right = self == .topRight || self == .bottomRight
        let top = self == .topLeft || self == .topRight
        return CGRect(
            x: right ? bounds.maxX - size.width : bounds.minX,
            y: top ? bounds.maxY - size.height : bounds.minY,
            width: size.width, height: size.height)
    }
}

@MainActor
@Observable
final class VoicePanelController: NSObject, NSWindowDelegate {
    private(set) var corner: VoiceCorner?
    var isMini = false
    var controlsRequested = false
    let shortcuts: VoiceShortcuts
    @ObservationIgnored private let model: MenuBarModel
    @ObservationIgnored private var panel: VoicePanel?
    @ObservationIgnored private var screen: NSScreen?
    @ObservationIgnored private var shortcutWindow: NSWindow?

    init(model: MenuBarModel, defaults: UserDefaults = .standard) {
        self.model = model
        shortcuts = VoiceShortcuts(defaults: defaults) { action in
            switch action {
            case .call: model.toggleVoice()
            case .microphone: model.toggleMicrophone()
            }
        }
        super.init()
        NotificationCenter.default.addObserver(
            self, selector: #selector(displaysChanged),
            name: NSApplication.didChangeScreenParametersNotification, object: nil)
    }

    func pin(to corner: VoiceCorner) {
        self.corner = corner
        controlsRequested = false
        if panel == nil {
            screen =
                NSScreen.screens.first { $0.frame.contains(NSEvent.mouseLocation) } ?? NSScreen.main
            let panel = VoicePanel(
                contentRect: .zero, styleMask: [.borderless, .nonactivatingPanel],
                backing: .buffered, defer: false)
            panel.title = "möbius Voice"
            panel.level = .floating
            panel.isFloatingPanel = true
            panel.hidesOnDeactivate = false
            panel.becomesKeyOnlyIfNeeded = false
            panel.acceptsMouseMovedEvents = true
            panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
            panel.isOpaque = false
            panel.backgroundColor = .clear
            panel.hasShadow = false
            panel.isReleasedWhenClosed = false
            panel.delegate = self
            self.panel = panel
            panel.contentView = NSHostingView(
                rootView: VoiceMenuView(model: model, presentation: self, isPinned: true))
        }
        if let panel {
            resize(
                to: panel.frame.size == .zero ? CGSize(width: 416, height: 96) : panel.frame.size)
        }
        panel?.orderFrontRegardless()
    }

    func unpin() {
        corner = nil
        isMini = false
        controlsRequested = false
        panel?.close()
        panel = nil
        screen = nil
    }

    func showControls() {
        isMini = false
        controlsRequested = true
        panel?.makeKeyAndOrderFront(nil)
    }

    func showKeyboardShortcuts() {
        if shortcutWindow == nil {
            let window = NSWindow(
                contentRect: .zero, styleMask: [.titled, .closable], backing: .buffered,
                defer: false)
            window.title = "Voice Keyboard Shortcuts"
            window.isReleasedWhenClosed = false
            window.delegate = self
            let content = NSHostingView(rootView: VoiceShortcutSettings(shortcuts: shortcuts))
            window.contentView = content
            window.setContentSize(content.fittingSize)
            window.center()
            shortcutWindow = window
        }
        NSApplication.shared.activate()
        shortcutWindow?.makeKeyAndOrderFront(nil)
    }

    func resize(to size: CGSize) {
        guard let panel, let corner, let screen = screen ?? NSScreen.main else { return }
        let frame = corner.frame(for: size, in: screen.visibleFrame)
        if panel.frame != frame { panel.setFrame(frame, display: true) }
    }

    func windowWillClose(_ notification: Notification) {
        guard notification.object as? NSWindow === panel else {
            shortcuts.cancelRecording()
            return
        }
        corner = nil
        isMini = false
        controlsRequested = false
    }

    func windowDidResignKey(_ notification: Notification) {
        if notification.object as? NSWindow === shortcutWindow { shortcuts.cancelRecording() }
    }

    @objc private func displaysChanged() {
        if let screen, !NSScreen.screens.contains(screen) { self.screen = NSScreen.main }
        if let panel { resize(to: panel.frame.size) }
    }
}

private final class VoicePanel: NSPanel {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { false }
}
