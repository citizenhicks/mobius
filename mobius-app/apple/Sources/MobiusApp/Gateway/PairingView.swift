import SwiftUI

struct PairingView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    enum Setup { case manual, cloud }
    @State private var setup: Setup?
    let canCancel: Bool
    @ScaledMetric(relativeTo: .caption) private var statusHeight = 80.0

    init(canCancel: Bool, initialSetup: Setup? = nil) {
        self.canCancel = canCancel
        _setup = State(initialValue: initialSetup)
    }

    var body: some View {
        ScrollView {
            VStack(spacing: MobiusSpace.xl) {
                header
                SetupArtwork(scene: .gateway)
                    .frame(height: setup == nil ? 248 : 156)
                VStack(spacing: MobiusSpace.xl) {
                    if setup == .cloud {
                        MobiusCloudOfferContent {
                            model.showsPairing = false
                            dismiss()
                        }
                    } else {
                        VStack(spacing: MobiusSpace.s) {
                            Text("Connect a gateway")
                                .font(.title.weight(.semibold))
                                .accessibilityAddTraits(.isHeader)
                            UserManualCaption(
                                detail: .localized(
                                    "Choose where your Bots run and their work stays."),
                                section: "gateway"
                            )
                            .foregroundStyle(palette.muted)
                        }
                        .multilineTextAlignment(.center)

                        if setup == .manual {
                            manualSetup
                        } else {
                            VStack(spacing: MobiusSpace.m) {
                                MobiusCloudOfferButton { setup = .cloud }
                                Button("Use your own gateway") { setup = .manual }
                                    .buttonStyle(.mobiusGlass)
                                    .buttonBorderShape(.capsule)
                                    .controlSize(.large)
                                    .buttonSizing(.flexible)
                                    .tint(palette.panel)
                                    .foregroundStyle(.primary)
                            }

                        }
                    }
                }
                .padding(MobiusSpace.xl)
                .frame(maxWidth: .infinity)
                .background(palette.raised, in: .rect(cornerRadius: 28))
                .overlay {
                    RoundedRectangle(cornerRadius: 28)
                        .strokeBorder(palette.line.opacity(0.6), lineWidth: MobiusStyle.borderWidth)
                }
                .shadow(color: palette.shadow.opacity(0.05), radius: 20, y: 12)
            }
            .frame(maxWidth: 460)
            .frame(maxWidth: .infinity)
            .padding(.bottom, MobiusSpace.xl)
        }
        .interactiveDismissDisabled(model.cloud.cloudAction.isRunning)
        .scrollIndicators(.hidden)
        .scrollBounceBehavior(.basedOnSize)
        .scrollDismissesKeyboard(.interactively)
        .animation(reduceMotion ? nil : .smooth(duration: 0.35), value: setup)
        .onChange(of: hasPairingDetails, initial: true) { _, hasDetails in
            if hasDetails && setup != .cloud && !model.cloud.cloudAction.isRunning {
                setup = .manual
            }
        }
        .task(id: model.cloud.cloudSession?.userID) {
            await model.cloud.refreshCloudAccount()
        }
    }

    private var hasPairingDetails: Bool {
        !model.gateway.pairingCode.isEmpty
            || model.gateway.pairingEndpoint != "wss://"
            || model.gateway.pairingError != nil
    }

    private var isConnecting: Bool {
        setup == .manual
            && model.gateway.pairingConnectionState?.isConnecting == true
    }

    private var manualSetup: some View {
        @Bindable var gateway = model.gateway
        return VStack(alignment: .leading, spacing: MobiusSpace.l) {
            VStack(alignment: .leading, spacing: MobiusSpace.s) {
                Text("Gateway address").font(MobiusStyle.captionFont)
                TextField("wss://gateway.example", text: $gateway.pairingEndpoint)
                    .textContentType(.URL)
                    .keyboardType(.URL)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .padding(MobiusSpace.m)
                    .background(palette.recessed, in: MobiusStyle.controlShape)
            }
            VStack(alignment: .leading, spacing: MobiusSpace.s) {
                Text("One-time code").font(MobiusStyle.captionFont)
                SecureField("One-time code", text: $gateway.pairingCode)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .padding(MobiusSpace.m)
                    .background(palette.recessed, in: MobiusStyle.controlShape)
            }
            Button {
                if let value = UIPasteboard.general.string { model.applyPairingSetup(value) }
            } label: {
                Text("Paste").frame(maxWidth: .infinity)
            }
            .buttonStyle(.mobiusGlass)
            .buttonBorderShape(.capsule)
            .controlSize(.large)
            .buttonSizing(.flexible)
            .tint(palette.panel)
            .foregroundStyle(.primary)
            .accessibilityLabel("Paste pairing setup")
            .help("Paste pairing setup")
            .frame(minHeight: MobiusStyle.rowTouch)

            Button("Connect", action: model.pair)
                .mobiusProminentButton()
                .buttonBorderShape(.capsule)
                .controlSize(.large)
                .buttonSizing(.flexible)
                .disabled(isConnecting)
        }
        .onSubmit { if !isConnecting { model.pair() } }
    }

    private var header: some View {
        ScrollView {
            Group {
                if let error = setup == .cloud ? model.cloud.cloudError : model.gateway.pairingError
                {
                    Text(verbatim: error).foregroundStyle(palette.danger)
                } else if isConnecting {
                    HStack(spacing: MobiusSpace.s) {
                        MobiusSpinner(size: MobiusStyle.glyphInline, foreground: palette.accent)
                        Text(
                            model.gateway.pairingConnectionState == .authenticating
                                ? "Authenticating with gateway" : "Connecting to gateway")
                    }
                }
            }
            .font(MobiusStyle.captionFont)
            .multilineTextAlignment(.center)
            .frame(maxWidth: .infinity)
        }
        .defaultScrollAnchor(.center, for: .alignment)
        .scrollBounceBehavior(.basedOnSize)
        .padding(.horizontal, MobiusStyle.rowTouch + MobiusSpace.s)
        .frame(height: statusHeight)
        .overlay(alignment: .topLeading) {
            if setup != nil {
                Button {
                    setup = nil
                } label: {
                    MobiusIcon(.caretRight, gutter: false)
                        .rotationEffect(.degrees(180))
                }
                .mobiusIconButton()
                .accessibilityLabel("Other connection options")
                .help("Other connection options")
                .disabled(model.cloud.cloudAction.isRunning || isConnecting)
            }
        }
        .overlay(alignment: .topTrailing) {
            if canCancel {
                Button("Close", glyph: .x) {
                    model.showsPairing = false
                    dismiss()
                }
                .mobiusIconButton()
                .help("Close")
                .disabled(model.cloud.cloudAction.isRunning)
            }
        }
    }

}

extension String {
    var nonEmpty: String? {
        let trimmed = trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}
