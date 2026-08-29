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
        "agent": .robot,
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
        "queue": .workflowSquare01,
        "route": .path,
        "search": .magnifyingGlass,
        "security_review": .aiSecurity02,
        "shield": .shield02,
        "shield_alert": .shieldAlert,
        "shield_check": .shieldCheck,
        "shield_off": .shieldOff,
        "sparkle": .sparkle,
        "storage": .hardDrives,
        "steer": .workflowSquare03,
        "task": .checkCircle,
    ]
}

struct MobiusPalette: Sendable {
    let canvas: Color
    /// Base surface behind sidebar views and the compact drawer.
    let recessed: Color
    let panel: Color
    let raised: Color
    let line: Color
    /// Strokes, rings, and marks drawn *in* the accent. Not a background for text.
    let accent: Color
    /// Fill behind `onAccent` labels, darker than `accent` so the pair clears WCAG AA.
    ///
    /// Glass composites its tint with whatever sits behind it, so a fill that only just
    /// clears on paper drifts under one: the light scheme lightens the result and loses
    /// contrast, the dark scheme darkens it and gains. Light therefore carries the extra
    /// headroom, the same way `signal`, `warning`, and `danger` are already darkened there.
    let accentFill: Color
    let accentSoft: Color
    let signal: Color
    let warning: Color
    let danger: Color
    let muted: Color
    /// Base shadow ink. Call sites own geometry and opacity, not its hue.
    let shadow: Color
    /// Label colour for anything filled with `accentFill`.
    let onAccent: Color
    let onDanger: Color
    let onMedia: Color
    let sidebarScrim: Color

    // Keep the Nord surface steps distinct: chat bubbles, tool details, and diff rows rely
    // on this hierarchy instead of carrying one-off borders and backgrounds.
    init(
        _ scheme: ColorScheme,
        lightsOut: Bool = false,
        accentTint: AccentTint = .appDefault
    ) {
        let isDark = scheme == .dark || lightsOut
        let hue = accentTint.color
        let surfaceTintAmount = accentTint == .appDefault ? 0.0 : 0.2
        let surfaceHue = hue.mix(
            with: isDark ? .black : .white,
            by: isDark ? 0.65 : 0.75,
            in: .device
        )
        let panelColor: Color
        let defaultAccentFill: Color
        let defaultAccentSoft: Color
        func surface(_ base: Color) -> Color {
            guard surfaceTintAmount > 0 else { return base }
            return base.mix(with: surfaceHue, by: surfaceTintAmount, in: .device)
        }

        onAccent = .nord6
        onDanger = .white
        onMedia = .white
        shadow = .black
        accent = hue.mix(
            with: isDark ? .white : .black,
            by: 0.55,
            in: .device
        )
        if isDark {
            canvas = lightsOut
                ? .black
                : surface(Color(red: 0.141, green: 0.161, blue: 0.200))
            recessed = lightsOut
                ? .black
                : surface(Color(red: 0.094, green: 0.106, blue: 0.133))
            panelColor = surface(.nord0)
            panel = panelColor
            raised = surface(.nord1)
            line = surface(.nord3)
            // 4.84:1 against onAccent, and the dark backdrop only deepens it under glass.
            defaultAccentFill = Color(red: 0.298, green: 0.416, blue: 0.557)
            defaultAccentSoft = Color(red: 0.227, green: 0.278, blue: 0.349)
            signal = .nord14
            warning = .nord13
            danger = .nord11
            muted = Color(red: 0.541, green: 0.588, blue: 0.671)
            sidebarScrim = lightsOut ? .nord3 : surface(.nord3)
        } else {
            canvas = surface(.nord6)
            recessed = surface(.nord5)
            panelColor = surface(.nord5)
            panel = panelColor
            raised = surface(Color(red: 0.965, green: 0.973, blue: 0.984))
            line = surface(.nord4)
            // 6.15:1 against onAccent: the light backdrop lightens the tint under glass,
            // so the extra headroom is what keeps the composited result above 4.5:1.
            defaultAccentFill = Color(red: 0.239, green: 0.353, blue: 0.494)
            defaultAccentSoft = Color(red: 0.831, green: 0.871, blue: 0.918)
            signal = Color(red: 0.353, green: 0.482, blue: 0.243)
            warning = Color(red: 0.565, green: 0.435, blue: 0.153)
            danger = Color(red: 0.639, green: 0.263, blue: 0.310)
            muted = .nord3
            sidebarScrim = surface(.nord6)
        }

        if accentTint == .appDefault {
            accentFill = defaultAccentFill
            accentSoft = defaultAccentSoft
        } else {
            // Bright swatches need extra black headroom under glass, especially in light mode.
            accentFill = hue.mix(
                with: .black,
                by: isDark ? 0.5 : 0.6,
                in: .device
            )
            accentSoft = panelColor.mix(
                with: hue,
                by: isDark ? 0.08 : 0.12,
                in: .device
            )
        }
    }

    func tone(_ tone: String) -> Color {
        switch tone {
        case "success": signal
        case "warning": warning
        case "error": danger
        default: muted
        }
    }

    static func composingOrbInk(white: Double, scheme: ColorScheme) -> Color {
        let white = min(1, max(0, white))
        return Color(white: scheme == .dark ? 1 - white : white)
    }
}

private extension Color {
    static let nord0 = Color(red: 46.0 / 255.0, green: 52.0 / 255.0, blue: 64.0 / 255.0)
    static let nord1 = Color(red: 0.231, green: 0.259, blue: 0.322)
    static let nord3 = Color(red: 76.0 / 255.0, green: 86.0 / 255.0, blue: 106.0 / 255.0)
    static let nord4 = Color(red: 216.0 / 255.0, green: 222.0 / 255.0, blue: 233.0 / 255.0)
    static let nord5 = Color(red: 0.898, green: 0.914, blue: 0.941)
    static let nord6 = Color(red: 236.0 / 255.0, green: 239.0 / 255.0, blue: 244.0 / 255.0)
    static let nord10 = Color(red: 0.369, green: 0.506, blue: 0.675)
    static let nord11 = Color(red: 0.749, green: 0.380, blue: 0.416)
    static let nord13 = Color(red: 0.922, green: 0.796, blue: 0.545)
    static let nord14 = Color(red: 0.639, green: 0.745, blue: 0.549)
    static let nord7 = Color(red: 0.561, green: 0.737, blue: 0.733)
    static let nord12 = Color(red: 0.816, green: 0.529, blue: 0.439)
    static let nord15 = Color(red: 0.706, green: 0.557, blue: 0.678)
}

extension EnvironmentValues {
    @Entry var mobiusPalette = MobiusPalette(.dark)
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

/// Nord hues shared by provider identities and the user-selected app accent.
enum AccentTint: String, Codable, CaseIterable, Identifiable, Sendable {
    case blue
    case teal
    case green
    case yellow
    case orange
    case red
    case purple

    static let appDefault: Self = .blue

    var id: String { rawValue }

    var color: Color {
        switch self {
        case .blue: .nord10
        case .teal: .nord7
        case .green: .nord14
        case .yellow: .nord13
        case .orange: .nord12
        case .red: .nord11
        case .purple: .nord15
        }
    }

    var label: String { rawValue.capitalized }

    var localizedLabel: LocalizedStringResource {
        switch self {
        case .blue: "Blue"
        case .teal: "Teal"
        case .green: "Green"
        case .yellow: "Yellow"
        case .orange: "Orange"
        case .red: "Red"
        case .purple: "Purple"
        }
    }
}
