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
        "route": .path,
        "search": .magnifyingGlass,
        "security_review": .aiSecurity02,
        "shield": .shield02,
        "shield_alert": .shieldAlert,
        "shield_check": .shieldCheck,
        "shield_off": .shieldOff,
        "sparkle": .sparkle,
        "storage": .hardDrives,
        "task": .checkCircle,
    ]
}

struct MobiusPalette: Sendable {
    let canvas: Color
    /// Base surface behind the compact drawer and embedded document views.
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
    /// Label colour for anything filled with `accentFill`.
    let onAccent: Color
    let sidebarScrim: Color

    // Keep the Nord surface steps distinct: chat bubbles, tool details, and diff rows rely
    // on this hierarchy instead of carrying one-off borders and backgrounds.
    init(_ scheme: ColorScheme, lightsOut: Bool = false) {
        onAccent = .nord6
        if scheme == .dark || lightsOut {
            canvas = lightsOut ? .black : Color(red: 0.141, green: 0.161, blue: 0.200)
            recessed = Color(red: 0.094, green: 0.106, blue: 0.133)
            panel = .nord0
            raised = .nord1
            line = .nord3
            accent = .nord10
            // 4.84:1 against onAccent, and the dark backdrop only deepens it under glass.
            accentFill = Color(red: 0.298, green: 0.416, blue: 0.557)
            accentSoft = Color(red: 0.227, green: 0.278, blue: 0.349)
            signal = .nord14
            warning = .nord13
            danger = .nord11
            muted = Color(red: 0.541, green: 0.588, blue: 0.671)
            sidebarScrim = .nord3
        } else {
            canvas = .nord6
            recessed = .nord5
            panel = .nord5
            raised = Color(red: 0.965, green: 0.973, blue: 0.984)
            line = .nord4
            accent = .nord10
            // 6.15:1 against onAccent: the light backdrop lightens the tint under glass,
            // so the extra headroom is what keeps the composited result above 4.5:1.
            accentFill = Color(red: 0.239, green: 0.353, blue: 0.494)
            accentSoft = Color(red: 0.831, green: 0.871, blue: 0.918)
            signal = Color(red: 0.353, green: 0.482, blue: 0.243)
            warning = Color(red: 0.565, green: 0.435, blue: 0.153)
            danger = Color(red: 0.639, green: 0.263, blue: 0.310)
            muted = .nord3
            sidebarScrim = .nord6
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
        let palette = MobiusPalette(colorScheme, lightsOut: model.theme == .lightsOut)
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

/// A user-chosen accent telling two setups of one provider apart. The gateway sends
/// hues; this app maps them onto its own Nord palette.
enum ProviderTint: String, Codable, CaseIterable, Identifiable, Sendable {
    case blue
    case teal
    case green
    case yellow
    case orange
    case red
    case purple

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
}
