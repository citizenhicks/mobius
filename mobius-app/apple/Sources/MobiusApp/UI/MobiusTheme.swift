import SwiftUI

enum MobiusSymbol {
    private static let placeholder = MobiusGlyph.question

    static func glyph(for symbol: String) -> MobiusGlyph {
        vocabulary[symbol] ?? placeholder
    }

    /// Nil where `glyph(for:)` would return the placeholder. Beside a label the placeholder
    /// reads as a broken glyph rather than a neutral one, so a caller can drop it instead.
    static func knownGlyph(for symbol: String) -> MobiusGlyph? {
        vocabulary[symbol]
    }

    /// Semantic protocol tokens plus provider artwork known by this client.
    private static let vocabulary: [String: MobiusGlyph] = [
        "agent": .aiScan,
        "brain": .brain,
        "branch": .gitBranch,
        "chat": .chatCircle,
        "chat_gpt": .chatGpt,
        "claude": .claude,
        "deepseek": .deepseek,
        "delete": .trash,
        "edit": .pencilSimple,
        "kimi": .kimiAi,
        "moon": .moon,
        "promote": .arrowCircleUp,
        "queue": .queue01,
        "route": .path,
        "search": .magnifyingGlass,
        "shield": .shield02,
        "shield_alert": .shieldAlert,
        "shield_check": .shieldCheck,
        "shield_off": .shieldOff,
        "sparkle": .sparkle,
        "storage": .hardDrives,
        "steer": .arrowUpRight01,
        "task": .checkCircle,
        "voice": .audioWave01,
    ]
}

struct MobiusTheme: ViewModifier {
    @Environment(AppModel.self) private var model
    @Environment(\.colorScheme) private var colorScheme

    func body(content: Content) -> some View {
        let palette = MobiusPalette(
            colorScheme,
            lightsOut: model.theme == .lightsOut,
            accentTint: model.accentTint
        )
        content
            .environment(\.mobiusPalette, palette)
            .foregroundStyle(.primary)
            .tint(palette.accent)
            .font(MobiusStyle.bodyFont)
            .buttonStyle(.mobiusAutomatic)
    }
}

struct MobiusBackdrop: View {
    @Environment(\.mobiusPalette) private var palette

    var body: some View {
        palette.canvas
            .ignoresSafeArea()
            .accessibilityHidden(true)
    }
}
