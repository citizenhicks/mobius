import AppKit
import SwiftUI

/// A pinned panel must reveal controls even while another application has focus.
struct VoiceHoverArea: NSViewRepresentable {
    let changed: (Bool) -> Void

    func makeNSView(context: Context) -> TrackingView {
        TrackingView(changed: changed)
    }

    func updateNSView(_ view: TrackingView, context: Context) {
        view.changed = changed
    }

    final class TrackingView: NSView {
        var changed: (Bool) -> Void

        init(changed: @escaping (Bool) -> Void) {
            self.changed = changed
            super.init(frame: .zero)
        }

        required init?(coder: NSCoder) { nil }

        override func updateTrackingAreas() {
            super.updateTrackingAreas()
            for area in trackingAreas { removeTrackingArea(area) }
            addTrackingArea(
                NSTrackingArea(
                    rect: .zero, options: [.mouseEnteredAndExited, .activeAlways, .inVisibleRect],
                    owner: self, userInfo: nil))
        }

        override func mouseEntered(with event: NSEvent) { changed(true) }
        override func mouseExited(with event: NSEvent) { changed(false) }
        override func hitTest(_ point: NSPoint) -> NSView? { nil }
    }
}
