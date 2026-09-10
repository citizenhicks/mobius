import AppKit
import SwiftUI

struct VoiceIcon: View {
    let name: String
    var size: CGFloat = MobiusStyle.glyphInline

    init(_ name: String, size: CGFloat = MobiusStyle.glyphInline) {
        self.name = name
        self.size = size
    }

    var body: some View {
        Image(nsImage: NSImage(named: "hi.\(name)")!)
            .renderingMode(.template)
            .resizable()
            .aspectRatio(contentMode: .fit)
            .frame(width: size, height: size)
            .accessibilityHidden(true)
    }

    // Native menus extract an image; render its SwiftUI tint and appearance before that handoff.
    static func menuImage(
        _ name: String, color: Color, size: CGFloat = MobiusStyle.glyphInline, scheme: ColorScheme
    ) -> Image {
        let renderer = ImageRenderer(
            content: VoiceIcon(name, size: size).foregroundStyle(color)
                .environment(\.colorScheme, scheme)
        )
        renderer.scale = 2
        return Image(
            nsImage: NSImage(cgImage: renderer.cgImage!, size: NSSize(width: size, height: size))
        ).renderingMode(.original)
    }
}

extension VoiceBot {
    var color: Color {
        (AccentTint(rawValue: tint) ?? .appDefault).color
    }
}
