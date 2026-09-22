import AppKit
import SwiftUI

fileprivate let desktopCursorSize = CGFloat(36)
fileprivate let desktopCursorHotspot = CGPoint(x: 5.11, y: 5.12)

@MainActor
final class DesktopCursorOverlay {
    private var panel: NSPanel?
    private var tint: String?
    private var hideTask: Task<Void, Never>?
    private var isActive = false
    private(set) var isVisible = false

    var windowID: CGWindowID? {
        guard let number = panel?.windowNumber, number > 0 else { return nil }
        return CGWindowID(number)
    }

    func prepare(tint: String?) {
        guard panel == nil || self.tint != tint || !isActive else { return }
        hideTask?.cancel()
        hideTask = nil
        let panel = panel ?? makePanel()
        self.tint = tint
        isActive = true
        updateView()
        if !isVisible { panel.orderOut(nil) }
    }

    private func updateView() {
        guard let panel else { return }
        panel.contentView = NSHostingView(
            rootView: DesktopCursorView(
                color: (AccentTint(rawValue: tint ?? "") ?? .appDefault).color,
                isActive: isActive))
    }

    func move(to point: CGPoint) {
        guard let panel else { return }
        panel.setFrameOrigin(
            NSPoint(
                x: point.x - desktopCursorHotspot.x,
                y: point.y - desktopCursorSize + desktopCursorHotspot.y))
        isVisible = true
        panel.orderFrontRegardless()
    }

    func hide() {
        hideTask?.cancel()
        hideTask = nil
        isActive = false
        isVisible = false
        panel?.orderOut(nil)
    }

    func settle() {
        guard isVisible else { return }
        isActive = false
        updateView()
        hideTask?.cancel()
        hideTask = Task { [weak self] in
            do { try await Task.sleep(for: .milliseconds(500)) } catch { return }
            guard !Task.isCancelled else { return }
            self?.hide()
        }
    }

    private func makePanel() -> NSPanel {
        let panel = NSPanel(
            contentRect: CGRect(
                origin: .zero, size: CGSize(width: desktopCursorSize, height: desktopCursorSize)),
            styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: false)
        panel.level = NSWindow.Level(rawValue: NSWindow.Level.popUpMenu.rawValue + 1)
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = false
        panel.ignoresMouseEvents = true
        panel.hidesOnDeactivate = false
        panel.sharingType = .none
        self.panel = panel
        return panel
    }
}

private struct DesktopCursorView: View {
    let color: Color
    let isActive: Bool
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var isWiggling = false

    var body: some View {
        VoiceIcon("mousePointer01", size: desktopCursorSize)
            .foregroundStyle(color)
            .rotationEffect(
                .degrees(!isActive || reduceMotion ? 0 : (isWiggling ? 2 : -2)),
                anchor: UnitPoint(
                    x: desktopCursorHotspot.x / desktopCursorSize,
                    y: desktopCursorHotspot.y / desktopCursorSize)
            )
            .animation(
                !isActive || reduceMotion
                    ? nil : .easeInOut(duration: 0.55).repeatForever(autoreverses: true),
                value: isWiggling
            )
            .onAppear { isWiggling = true }
            .accessibilityHidden(true)
    }
}
