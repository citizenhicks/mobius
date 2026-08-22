import SwiftUI

struct MobiusCard<Content: View>: View {
    let content: Content

    init(@ViewBuilder content: () -> Content) {
        self.content = content()
    }

    var body: some View {
        content
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(MobiusStyle.cardPadding)
            .glassEffect(.regular, in: MobiusStyle.cardShape)
    }
}

struct MobiusBadge: View {
    @Environment(\.mobiusPalette) private var palette
    let text: String
    var tone = "neutral"
    var glyph: MobiusGlyph?
    var progress: Double?
    var interactive = false

    var body: some View {
        HStack(spacing: MobiusSpace.s) {
            if let progress {
                ZStack {
                    Circle().stroke(palette.line.opacity(0.55), lineWidth: 2)
                    Circle()
                        .trim(from: 0, to: min(max(progress, 0), 1))
                        .stroke(palette.accent, style: StrokeStyle(lineWidth: 2, lineCap: .round))
                        .rotationEffect(.degrees(-90))
                }
                .frame(width: 12, height: 12)
                .accessibilityHidden(true)
            }
            if let glyph {
                MobiusIcon(glyph, size: MobiusStyle.glyphInline, foreground: foreground, gutter: false)
            }
            if !text.isEmpty { Text(text).lineLimit(1) }
        }
        .font(MobiusStyle.badgeFont)
        .foregroundStyle(foreground)
        .padding(.horizontal, MobiusSpace.m)
        .frame(height: MobiusStyle.badgeHeight)
        .mobiusGlass(in: Capsule(), interactive: interactive)
    }

    private var foreground: Color { palette.tone(tone) }
}

/// A menu's current value: the provider's mark, the value itself, and a muted qualifier.
/// No container — the icon buttons it sits beside carry none either, and the hierarchy
/// between the three parts is what separates it from the row.
struct MobiusMenuLabel: View {
    @Environment(\.mobiusPalette) private var palette
    let text: String
    var glyph: MobiusGlyph?
    var detail: String?
    var showsDisclosure = true
    /// The composer sizes its mark up to `glyphLead` to sit level with the icon buttons
    /// beside it; inline in a header the glyph stays on the text's own step.
    var glyphSize = MobiusStyle.glyphInline
    /// Tints only the mark without recolouring the label text.
    var glyphColor: Color?
    /// A settings row reads at body size beside its label; the composer and the file header
    /// carry the badge step so the label sits under the text it belongs to.
    var font = MobiusStyle.badgeFont

    var body: some View {
        HStack(spacing: MobiusSpace.s) {
            if let glyph {
                MobiusIcon(
                    glyph,
                    size: glyphSize,
                    foreground: glyphColor ?? palette.accent,
                    gutter: false
                )
            }
            Text(text)
                .font(font)
                .lineLimit(1)
                .truncationMode(.middle)
            if let detail {
                Text(detail)
                    .font(font)
                    .foregroundStyle(palette.muted)
                    .lineLimit(1)
            }
            if showsDisclosure {
                MobiusIcon(.caretUpDown, size: MobiusStyle.glyphMark, foreground: palette.muted, gutter: false)
            }
        }
        .frame(minHeight: MobiusStyle.controlHeight)
        .contentShape(Rectangle())
    }
}

struct MobiusFeedbackButtonStyle<Base: PrimitiveButtonStyle>: PrimitiveButtonStyle {
    let base: Base

    @ViewBuilder
    func makeBody(configuration: Configuration) -> some View {
        FeedbackButton(configuration: configuration, base: base)
    }

    private struct FeedbackButton: View {
        @State private var feedback = false
        let configuration: PrimitiveButtonStyleConfiguration
        let base: Base

        var body: some View {
            Button(role: configuration.role) {
                feedback.toggle()
                configuration.trigger()
            } label: {
                configuration.label
            }
            .buttonStyle(base)
            .sensoryFeedback(.impact(weight: .light), trigger: feedback)
        }
    }
}

extension PrimitiveButtonStyle where Self == MobiusFeedbackButtonStyle<DefaultButtonStyle> {
    static var mobiusAutomatic: Self { Self(base: DefaultButtonStyle()) }
}

extension PrimitiveButtonStyle where Self == MobiusFeedbackButtonStyle<PlainButtonStyle> {
    static var mobiusPlain: Self { Self(base: PlainButtonStyle()) }
}

extension PrimitiveButtonStyle where Self == MobiusFeedbackButtonStyle<GlassButtonStyle> {
    static var mobiusGlass: Self { Self(base: GlassButtonStyle()) }
}

extension PrimitiveButtonStyle where Self == MobiusFeedbackButtonStyle<GlassProminentButtonStyle> {
    static var mobiusGlassProminent: Self { Self(base: GlassProminentButtonStyle()) }
}

struct MobiusIconButtonStyle: ButtonStyle {
    var prominent = false
    var bare = false

    func makeBody(configuration: Configuration) -> some View {
        IconButton(
            label: configuration.label,
            isPressed: configuration.isPressed,
            prominent: prominent,
            bare: bare
        )
    }

    private struct IconButton: View {
        @Environment(\.mobiusPalette) private var palette
        // A custom style gets no automatic disabled treatment.
        @Environment(\.isEnabled) private var isEnabled
        let label: ButtonStyleConfiguration.Label
        let isPressed: Bool
        let prominent: Bool
        let bare: Bool

        var body: some View {
            let base = label
                .font(MobiusStyle.controlFont)
                .foregroundStyle(foreground)
                .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
                .contentShape(Circle())
            Group {
                if bare {
                    base
                } else {
                    base.mobiusGlass(
                        in: Circle(),
                        interactive: isEnabled,
                        prominent: prominent && isEnabled,
                        clear: !prominent
                    )
                }
            }
            .opacity(isPressed ? 0.72 : 1)
            .sensoryFeedback(.impact(weight: .light), trigger: isPressed) { _, pressed in pressed }
        }

        private var foreground: Color {
            guard isEnabled else { return palette.muted }
            if prominent { return bare ? palette.accent : palette.onAccent }
            return .primary
        }
    }
}

struct MobiusToolbarIconButton: View {
    let glyph: MobiusGlyph
    let label: String
    let action: () -> Void

    var body: some View {
        Button(label, glyph: glyph, action: action)
            .mobiusIconButton()
            .accessibilityLabel(label)
            .help(label)
    }
}

/// Keeps adjacent toolbar actions inside the system's single shared glass surface.
struct HeaderActionGroup<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        HStack(spacing: 0) { content }
            .fixedSize()
    }
}

struct HeaderOptionsMenu<Content: View>: View {
    let label: String
    @ViewBuilder let content: Content

    init(label: String, @ViewBuilder content: () -> Content) {
        self.label = label
        self.content = content()
    }

    var body: some View {
        // No target padded out here: the system's glass hugs the label, and a 44pt square
        // draws as a wide pill rather than the circle a lone action should be. Inside a
        // `HeaderActionGroup` the call site adds `groupedHeaderAction()` for the target it
        // needs to fill its half of the shared surface.
        Menu { content } label: {
            MobiusIcon(.dotsThree, foreground: .primary)
        }
        .labelStyle(.titleAndIcon)
        .menuIndicator(.hidden)
        .accessibilityLabel(label)
        .tint(.primary)
        .help(label)
    }
}

extension View {
    func groupedHeaderAction(prominent: Bool = false) -> some View {
        modifier(GroupedHeaderAction(prominent: prominent))
    }
}

private struct GroupedHeaderAction: ViewModifier {
    @Environment(\.mobiusPalette) private var palette
    let prominent: Bool

    func body(content: Content) -> some View {
        content
            .tint(prominent ? palette.accent : .primary)
            .frame(
                width: MobiusStyle.iconButtonSize,
                height: MobiusStyle.iconButtonSize
            )
            .contentShape(Rectangle())
    }
}

/// A prominent button with a label that stays legible on the accent in both schemes.
private struct MobiusProminentButton: ViewModifier {
    @Environment(\.mobiusPalette) private var palette

    func body(content: Content) -> some View {
        // `.glassProminent` fills from the tint, so it needs the accessible one rather than
        // the global tint `MobiusTheme` sets for switches, pickers, and links.
        content
            .buttonStyle(.mobiusGlassProminent)
            .tint(palette.accentFill)
            .foregroundStyle(palette.onAccent)
    }
}

extension View {
    func mobiusProminentButton() -> some View { modifier(MobiusProminentButton()) }

    func mobiusIconButton() -> some View {
        labelStyle(.iconOnly)
            .tint(.primary)
            .buttonStyle(MobiusIconButtonStyle())
    }

    func mobiusProminentIconButton() -> some View {
        labelStyle(.iconOnly)
            .buttonStyle(MobiusIconButtonStyle(prominent: true))
    }

    /// Lets a row of badges scroll instead of squeezing when it outgrows the width.
    func scrollableRow() -> some View {
        ScrollView(.horizontal) {
            fixedSize(horizontal: true, vertical: false)
        }
        // ponytail: centering can snap as badges arrive; keep one render until native overflow
        // alignment can preserve state without duplicating the controls.
        .defaultScrollAnchor(.center, for: .alignment)
        .scrollIndicators(.hidden)
        .scrollBounceBehavior(.basedOnSize)
    }

    func mobiusGlass<S: Shape>(
        in shape: S,
        interactive: Bool = false,
        prominent: Bool = false,
        clear: Bool = false
    ) -> some View {
        modifier(
            MobiusGlassModifier(
                shape: shape,
                interactive: interactive,
                prominent: prominent,
                clear: clear
            )
        )
    }

}

private struct MobiusGlassModifier<S: Shape>: ViewModifier {
    @Environment(\.mobiusPalette) private var palette
    let shape: S
    let interactive: Bool
    let prominent: Bool
    let clear: Bool

    func body(content: Content) -> some View {
        let glass = prominent ? Glass.regular.tint(palette.accentFill) : clear ? Glass.clear : Glass.regular
        if interactive {
            content.glassEffect(glass.interactive(), in: shape)
        } else {
            content.glassEffect(glass, in: shape)
        }
    }
}

struct SectionHeading: View {
    @Environment(\.mobiusPalette) private var palette
    let title: String
    let detail: String

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            Text(title)
                .font(MobiusStyle.titleFont)
            Text(detail)
                .font(MobiusStyle.bodyFont)
                .foregroundStyle(palette.muted)
        }
    }
}

extension View {
    func mobiusTheme() -> some View { modifier(MobiusTheme()) }
}
