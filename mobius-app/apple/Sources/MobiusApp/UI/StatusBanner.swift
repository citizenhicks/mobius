import SwiftUI

struct StatusBanner: View {
    enum Tone { case neutral, success, warning, error }
    @Environment(\.mobiusPalette) private var palette
    let tone: Tone
    let title: MobiusText
    let detail: MobiusText
    var progress = false
    var action: (MobiusText, @MainActor () -> Void)?

    init(
        tone: Tone,
        title: LocalizedStringResource,
        detail: LocalizedStringResource,
        progress: Bool = false,
        action: (LocalizedStringResource, @MainActor () -> Void)? = nil
    ) {
        self.init(
            tone: tone,
            title: .localized(title),
            detail: .localized(detail),
            progress: progress,
            action: action.map { (.localized($0.0), $0.1) }
        )
    }

    init(
        tone: Tone,
        title: MobiusText,
        detail: MobiusText,
        progress: Bool = false,
        action: (MobiusText, @MainActor () -> Void)? = nil
    ) {
        self.tone = tone
        self.title = title
        self.detail = detail
        self.progress = progress
        self.action = action
    }

    var body: some View {
        HStack(spacing: MobiusSpace.m) {
            if progress { ProgressView().controlSize(.small) }
            else { MobiusIcon(glyph, foreground: color) }
            VStack(alignment: .leading, spacing: MobiusSpace.xs) {
                title.text.font(MobiusStyle.controlFont)
                detail.text.font(MobiusStyle.bodyFont).foregroundStyle(palette.muted)
            }
            Spacer()
            if let action {
                Button(action: action.1) {
                    action.0.text
                }
                    .buttonStyle(.mobiusGlass)
                    .buttonBorderShape(.capsule)
            }
        }
        .padding(MobiusSpace.m)
        .background(color.opacity(0.09), in: MobiusStyle.cardShape)
        .overlay {
            MobiusStyle.cardShape
                .stroke(color.opacity(0.45), lineWidth: MobiusStyle.borderWidth)
        }
    }

    private var color: Color {
        switch tone {
        case .neutral: palette.accent
        case .success: palette.signal
        case .warning: palette.warning
        case .error: palette.danger
        }
    }

    private var glyph: MobiusGlyph {
        switch tone {
        case .neutral: .info
        case .success: .sealCheck
        case .warning: .warning
        case .error: .warningOctagon
        }
    }
}

struct DisabledCapabilityNotice: View {
    let title: MobiusText
    let detail: MobiusText

    init(
        title: LocalizedStringResource,
        detail: LocalizedStringResource
    ) {
        self.init(title: .localized(title), detail: .localized(detail))
    }

    init(title: MobiusText, detail: MobiusText) {
        self.title = title
        self.detail = detail
    }

    var body: some View {
        StatusBanner(tone: .neutral, title: title, detail: detail)
            .settingsStandaloneRow()
    }
}
