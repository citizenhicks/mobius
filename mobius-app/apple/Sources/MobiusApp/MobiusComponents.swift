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
    private let text: MobiusText
    var tone = "neutral"
    var glyph: MobiusGlyph?
    var glyphColor: Color?
    var progress: Double?
    var interactive = false
    var selected = false

    init(
        text: MobiusText,
        tone: String = "neutral",
        glyph: MobiusGlyph? = nil,
        glyphColor: Color? = nil,
        progress: Double? = nil,
        interactive: Bool = false,
        selected: Bool = false
    ) {
        self.text = text
        self.tone = tone
        self.glyph = glyph
        self.glyphColor = glyphColor
        self.progress = progress
        self.interactive = interactive
        self.selected = selected
    }

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
                MobiusIcon(
                    glyph,
                    size: MobiusStyle.glyphInline,
                    foreground: glyphColor ?? foreground,
                    gutter: false
                )
            }
            if !text.isEmpty { text.text.lineLimit(1) }
        }
        .font(MobiusStyle.badgeFont)
        .foregroundStyle(foreground)
        .padding(.horizontal, MobiusSpace.m)
        .frame(height: MobiusStyle.badgeHeight)
        .mobiusGlass(in: Capsule(), interactive: interactive, prominent: selected)
    }

    private var foreground: Color { selected ? palette.onAccent : palette.tone(tone) }
}

/// A menu's current value: the provider's mark, the value itself, and a muted qualifier.
/// No container — the icon buttons it sits beside carry none either, and the hierarchy
/// between the three parts is what separates it from the row.
struct MobiusMenuLabel: View {
    @Environment(\.mobiusPalette) private var palette
    private let label: MobiusText
    var glyph: MobiusGlyph?
    private var detail: MobiusText?
    var showsDisclosure = true
    /// The composer sizes its mark up to `glyphLead` to sit level with the icon buttons
    /// beside it; inline in a header the glyph stays on the text's own step.
    var glyphSize = MobiusStyle.glyphInline
    /// Tints only the mark without recolouring the label text.
    var glyphColor: Color?
    /// A settings row reads at body size beside its label; the composer and the file header
    /// carry the badge step so the label sits under the text it belongs to.
    var font = MobiusStyle.badgeFont

    init(
        text: MobiusText,
        glyph: MobiusGlyph? = nil,
        detail: MobiusText? = nil,
        showsDisclosure: Bool = true,
        glyphSize: CGFloat = MobiusStyle.glyphInline,
        glyphColor: Color? = nil,
        font: Font = MobiusStyle.badgeFont
    ) {
        label = text
        self.glyph = glyph
        self.detail = detail
        self.showsDisclosure = showsDisclosure
        self.glyphSize = glyphSize
        self.glyphColor = glyphColor
        self.font = font
    }

    init(
        text: LocalizedStringResource,
        glyph: MobiusGlyph? = nil,
        detail: LocalizedStringResource? = nil,
        showsDisclosure: Bool = true,
        glyphSize: CGFloat = MobiusStyle.glyphInline,
        glyphColor: Color? = nil,
        font: Font = MobiusStyle.badgeFont
    ) {
        self.label = .localized(text)
        self.glyph = glyph
        self.detail = detail.map(MobiusText.localized)
        self.showsDisclosure = showsDisclosure
        self.glyphSize = glyphSize
        self.glyphColor = glyphColor
        self.font = font
    }

    init(
        verbatim text: String,
        glyph: MobiusGlyph? = nil,
        detail: String? = nil,
        showsDisclosure: Bool = true,
        glyphSize: CGFloat = MobiusStyle.glyphInline,
        glyphColor: Color? = nil,
        font: Font = MobiusStyle.badgeFont
    ) {
        self.label = .verbatim(text)
        self.glyph = glyph
        self.detail = detail.map(MobiusText.verbatim)
        self.showsDisclosure = showsDisclosure
        self.glyphSize = glyphSize
        self.glyphColor = glyphColor
        self.font = font
    }

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
            label.text
                .font(font)
                .lineLimit(1)
                .truncationMode(.middle)
            if let detail {
                detail.text
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
                .contentShape(Rectangle())
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
    let label: LocalizedStringResource
    let action: () -> Void

    var body: some View {
        // Bare: the system's toolbar glass hugs the glyph into the same circle every other
        // lone header action is. Drawing our own circle on top leaves two stacked surfaces
        // — a lighter blob inside the system's wider pill.
        Button(action: action) {
            MobiusIcon(glyph, foreground: .primary)
        }
        .tint(.primary)
        .accessibilityLabel(Text(label))
        .help(Text(label))
    }
}

struct MobiusSwipeAction: View {
    @Environment(\.mobiusPalette) private var palette
    private let title: MobiusText
    let glyph: MobiusGlyph
    var tone = "neutral"
    var isEnabled = true
    let action: () -> Void

    init(
        title: LocalizedStringResource,
        glyph: MobiusGlyph,
        tone: String = "neutral",
        isEnabled: Bool = true,
        action: @escaping () -> Void
    ) {
        self.title = .localized(title)
        self.glyph = glyph
        self.tone = tone
        self.isEnabled = isEnabled
        self.action = action
    }

    init(
        verbatim title: String,
        glyph: MobiusGlyph,
        tone: String = "neutral",
        isEnabled: Bool = true,
        action: @escaping () -> Void
    ) {
        self.title = .verbatim(title)
        self.glyph = glyph
        self.tone = tone
        self.isEnabled = isEnabled
        self.action = action
    }

    var body: some View {
        // No destructive role: it makes the list animate the row away on tap, which tears
        // down any confirmation the action wanted to show. The tone already reads as red.
        Button(action: action) {
            MobiusIcon(glyph, foreground: palette.tone(tone))
        }
        .tint(palette.panel)
        .disabled(!isEnabled)
        .accessibilityLabel(title.text)
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
    let label: LocalizedStringResource
    @ViewBuilder let content: Content

    init(label: LocalizedStringResource, @ViewBuilder content: () -> Content) {
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
        .accessibilityLabel(Text(label))
        .tint(.primary)
        .help(Text(label))
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

    func promptCard() -> some View {
        padding(.horizontal, MobiusSpace.l)
            .padding(.vertical, MobiusSpace.m)
            .mobiusGlass(in: MobiusStyle.cardShape, interactive: true)
            .listRowInsets(EdgeInsets(top: 4, leading: 0, bottom: 4, trailing: 0))
            .listRowBackground(Color.clear)
            .listRowSeparator(.hidden)
    }

    func mobiusSwipeActions<Actions: View>(
        @ViewBuilder actions: () -> Actions
    ) -> some View {
        swipeActions(edge: .trailing, allowsFullSwipe: false, content: actions)
    }

    /// Keeps sheet sizing and drag behavior consistent without overriding system material.
    func mobiusSheet(
        detents: Set<PresentationDetent> = [.medium, .large],
        selection: Binding<PresentationDetent>? = nil
    ) -> some View {
        modifier(MobiusSheetModifier(detents: detents, selection: selection))
    }
}

private struct MobiusSheetModifier: ViewModifier {
    let detents: Set<PresentationDetent>
    let selection: Binding<PresentationDetent>?

    func body(content: Content) -> some View {
        Group {
            if let selection {
                content.presentationDetents(detents, selection: selection)
            } else {
                content.presentationDetents(detents)
            }
        }
        .presentationDragIndicator(.visible)
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
    let title: LocalizedStringResource
    let detail: LocalizedStringResource

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
