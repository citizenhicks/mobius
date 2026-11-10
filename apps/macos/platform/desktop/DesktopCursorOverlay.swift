import AppKit
import SwiftUI

/// The pointer is drawn at 36 points; the panel adds room for its glow on every side.
fileprivate let desktopCursorSize = CGFloat(18)
fileprivate let desktopCursorGlow = CGFloat(8)
fileprivate let desktopCursorPanel = desktopCursorSize + desktopCursorGlow * 2
/// The pointer's tip inside the panel.
fileprivate let desktopCursorTip = CGPoint(
    x: desktopCursorSize * 0.142, y: desktopCursorSize * 0.142)
fileprivate let desktopCursorHotspot = CGPoint(
    x: desktopCursorGlow + desktopCursorTip.x, y: desktopCursorGlow + desktopCursorTip.y)

@MainActor
final class DesktopCursorOverlay {
    private var panel: NSPanel?
    private var accent: UInt32?
    private var hideTask: Task<Void, Never>?
    private var isActive = false
    private(set) var isVisible = false

    var windowID: CGWindowID? {
        guard let number = panel?.windowNumber, number > 0 else { return nil }
        return CGWindowID(number)
    }

    func prepare(accent: UInt32) {
        guard panel == nil || self.accent != accent || !isActive else { return }
        hideTask?.cancel()
        hideTask = nil
        let panel = panel ?? makePanel()
        self.accent = accent
        isActive = true
        updateView()
        if !isVisible { panel.orderOut(nil) }
    }

    private func updateView() {
        guard let panel, let accent else { return }
        panel.contentView = NSHostingView(
            rootView: DesktopCursorView(
                color: Color(
                    red: Double((accent >> 16) & 255) / 255,
                    green: Double((accent >> 8) & 255) / 255, blue: Double(accent & 255) / 255),
                isActive: isActive))
    }

    /// Jumps into place when it appears, then glides from point to point.
    func move(to point: CGPoint) {
        guard let panel else { return }
        let origin = NSPoint(
            x: point.x - desktopCursorHotspot.x,
            y: point.y - desktopCursorPanel + desktopCursorHotspot.y)
        if isVisible, !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion {
            NSAnimationContext.runAnimationGroup { context in
                context.duration = 0.4
                context.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
                panel.animator().setFrameOrigin(origin)
            }
        } else {
            panel.setFrameOrigin(origin)
        }
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

    /// How long the Bot's pointer stays between its actions before it fades.
    static var linger = Duration.seconds(30)

    func settle() {
        guard isVisible else { return }
        isActive = false
        updateView()
        hideTask?.cancel()
        hideTask = Task { [weak self] in
            do { try await Task.sleep(for: Self.linger) } catch { return }
            guard !Task.isCancelled else { return }
            self?.hide()
        }
    }

    private func makePanel() -> NSPanel {
        let panel = NSPanel(
            contentRect: CGRect(
                origin: .zero, size: CGSize(width: desktopCursorPanel, height: desktopCursorPanel)),
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

/// The Bot's pointer: the accent, a pale moonlit rim and a soft glow that breathes while
/// the Bot is acting.
private struct DesktopCursorView: View {
    let color: Color
    let isActive: Bool
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var isBreathing = false

    /// Hugeicons' MousePointer01, filled (resources/hi.mousePointer01.svg).
    private var pointer: some View {
        Image(nsImage: NSImage(named: "hi.mousePointer01")!)
            .renderingMode(.template)
            .resizable()
            .aspectRatio(contentMode: .fit)
    }

    var body: some View {
        let moving = isActive && !reduceMotion
        ZStack {
            pointer.foregroundStyle(color).blur(radius: 5)
                .opacity(moving && isBreathing ? 0.75 : 0.45)
            pointer.foregroundStyle(
                LinearGradient(
                    colors: [color.mix(with: .white, by: 0.35), color],
                    startPoint: .topLeading, endPoint: .bottomTrailing)
            )
            .shadow(color: Color(red: 0.86, green: 0.91, blue: 1).opacity(0.8), radius: 1)
            .shadow(color: Color(red: 0.86, green: 0.91, blue: 1).opacity(0.45), radius: 2.5)
        }
        .frame(width: desktopCursorSize, height: desktopCursorSize)
        .rotationEffect(
            .degrees(reduceMotion ? 0 : (isBreathing ? 7.5 : -7.5)),
            anchor: UnitPoint(x: 0.142, y: 0.142)
        )
        .frame(width: desktopCursorPanel, height: desktopCursorPanel)
        .animation(
            reduceMotion ? nil : .easeInOut(duration: 1.6).repeatForever(autoreverses: true),
            value: isBreathing
        )
        .onAppear { isBreathing = true }
        .accessibilityHidden(true)
    }
}
