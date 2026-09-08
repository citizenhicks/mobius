import SwiftUI

struct PairingView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    private enum Setup { case manual, cloud }
    @State private var setup: Setup?
    let canCancel: Bool

    var body: some View {
        ScrollView {
            VStack(spacing: MobiusSpace.xl) {
                HStack {
                    Text(verbatim: "möbius").font(.title3.weight(.semibold))
                    Spacer()
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
                SetupArtwork(scene: .gateway)
                    .frame(height: setup == nil ? 248 : 156)
                VStack(spacing: MobiusSpace.xl) {
                    if setup == .cloud {
                        MobiusCloudOfferContent {
                            model.showsPairing = false
                            dismiss()
                        }
                        otherConnectionOptions
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
                            if let error = model.cloud.cloudError {
                                Text(verbatim: error)
                                    .font(MobiusStyle.captionFont)
                                    .foregroundStyle(palette.danger)
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
            if hasDetails && !model.cloud.cloudAction.isRunning { setup = .manual }
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
        model.gateway.connectionState == .connecting
            || model.gateway.connectionState == .authenticating
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

            if let error = gateway.pairingError {
                MobiusLabel(verbatim: error, glyph: .warning, iconColor: palette.danger)
                    .foregroundStyle(palette.danger)
                    .font(MobiusStyle.captionFont)
            }
            if isConnecting {
                HStack(spacing: MobiusSpace.s) {
                    MobiusSpinner(size: MobiusStyle.glyphLead, foreground: palette.accent)
                    Text(
                        gateway.connectionState == .authenticating
                            ? "Authenticating with gateway" : "Connecting to gateway"
                    )
                    .font(MobiusStyle.captionFont)
                }
            }
            Button("Connect", action: model.pair)
                .mobiusProminentButton()
                .buttonBorderShape(.capsule)
                .controlSize(.large)
                .buttonSizing(.flexible)
                .disabled(isConnecting)
            otherConnectionOptions
        }
        .onSubmit { if !isConnecting { model.pair() } }
    }

    private var otherConnectionOptions: some View {
        Button("Other connection options") { setup = nil }
            .buttonStyle(.mobiusPlain)
            .font(MobiusStyle.captionFont)
            .frame(maxWidth: .infinity, minHeight: MobiusStyle.rowTouch)
            .disabled(model.cloud.cloudAction.isRunning)
    }

}

extension String {
    var nonEmpty: String? {
        let trimmed = trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}
