import SwiftUI

struct AppLockView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @ScaledMetric(relativeTo: .largeTitle) private var iconSize: CGFloat = 72

    var body: some View {
        ZStack {
            MobiusBackdrop()
            Button {
                Task { await model.unlockApp() }
            } label: {
                MobiusIcon(
                    model.appLockError == nil
                        ? model.appLockAuthenticationMethod.glyph
                        : .warningOctagon,
                    size: iconSize,
                    foreground: model.appLockError == nil ? palette.accent : palette.danger
                )
                .frame(width: 128, height: 128)
                .contentShape(Circle())
            }
            .buttonStyle(.mobiusPlain)
            .disabled(model.isAppLockAuthenticating)
            .opacity(model.isAppLockAuthenticating ? 0.45 : 1)
            .accessibilityLabel(
                model.appLockError == nil
                    ? model.appLockAuthenticationMethod.unlockTitle
                    : "Try Again"
            )
            .accessibilityValue(
                appLockAccessibilityValue
            )
        }
    }

    private var appLockAccessibilityValue: Text {
        if model.isAppLockAuthenticating { return Text("Authenticating") }
        if let error = model.appLockError { return Text(verbatim: error) }
        return Text("möbius is locked")
    }
}

struct AppToastOverlay: View {
    @Environment(AppModel.self) private var model
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        ZStack {
            if let toast = model.toast {
                AppToastView(toast: toast, dismiss: dismiss)
                    .transition(
                        reduceMotion
                            ? .opacity
                            : .move(edge: .top).combined(with: .opacity)
                    )
            }
        }
        .frame(maxWidth: 520)
        .padding(.horizontal, MobiusSpace.l)
        .padding(.top, MobiusSpace.m)
        .allowsHitTesting(model.toast != nil)
        .animation(toastAnimation, value: model.toast?.id)
    }

    private var toastAnimation: Animation {
        reduceMotion ? .easeOut(duration: 0.12) : .smooth(duration: 0.28)
    }

    private func dismiss() {
        withAnimation(toastAnimation) { model.dismissToast() }
    }
}

private struct AppToastView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    let toast: AppToast
    let dismiss: () -> Void

    var body: some View {
        HStack(spacing: MobiusSpace.m) {
            if let target = toast.target {
                Button {
                    model.openNotificationTarget(target)
                    dismiss()
                } label: {
                    toastMessage
                }
                .buttonStyle(.mobiusPlain)
                .accessibilityLabel(accessibilityLabel)
                .accessibilityHint("Opens this chat")
            } else {
                toastMessage
                    .accessibilityElement(children: .ignore)
                    .accessibilityLabel(accessibilityLabel)
            }

            Button(action: dismiss) {
                MobiusIcon(.x, size: MobiusStyle.glyphInline, foreground: palette.muted)
                    .frame(width: MobiusStyle.iconButtonSize, height: MobiusStyle.iconButtonSize)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.mobiusPlain)
            .accessibilityLabel("Dismiss notification")
        }
        .padding(.leading, MobiusSpace.l)
        .padding(.trailing, MobiusSpace.s)
        .padding(.vertical, MobiusSpace.m)
        .mobiusGlass(in: MobiusStyle.cardShape, interactive: true)
        .shadow(color: palette.shadow.opacity(0.20), radius: 18, y: 8)
        .gesture(
            DragGesture(minimumDistance: 20)
                .onEnded { value in
                    guard value.predictedEndTranslation.height < -40 else { return }
                    dismiss()
                }
        )
    }

    private var toastMessage: some View {
        HStack(alignment: .top, spacing: MobiusSpace.m) {
            toastIcon
            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                Text(toast.tone.title)
                    .font(MobiusStyle.controlFont.weight(.semibold))
                    .foregroundStyle(toast.tone.color(in: palette))
                Text(toast.message)
                    .font(MobiusStyle.bodyFont)
                    .foregroundStyle(.primary)
                    .lineLimit(toast.tone == .success && toast.target != nil ? 1 : nil)
                    .truncationMode(.tail)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
    }

    @ViewBuilder
    private var toastIcon: some View {
        if let bot = model.bot(for: toast.target) {
            ZStack {
                Circle().fill(bot.tint.color.opacity(0.16))
                MobiusIcon(
                    .aiScan,
                    size: MobiusStyle.glyphLead,
                    foreground: bot.tint.color,
                    gutter: false
                )
            }
            .frame(width: MobiusStyle.controlHeight, height: MobiusStyle.controlHeight)
            .accessibilityHidden(true)
        } else {
            MobiusIcon(
                toast.tone.glyph,
                size: MobiusStyle.glyphLead,
                foreground: toast.tone.color(in: palette)
            )
        }
    }

    private var accessibilityLabel: Text {
        Text("\(toast.tone.title): \(model.accessibilityMessage(for: toast))")
    }
}

extension ToastTone {
    var title: LocalizedStringResource {
        switch self {
        case .info: "Notice"
        case .success: "Done"
        case .warning: "Attention"
        case .error: "Error"
        }
    }

    var glyph: MobiusGlyph {
        switch self {
        case .info: .info
        case .success: .checkCircle
        case .warning: .warning
        case .error: .xCircle
        }
    }

    func color(in palette: MobiusPalette) -> Color {
        switch self {
        case .info: palette.accent
        case .success: palette.signal
        case .warning: palette.warning
        case .error: palette.danger
        }
    }
}
