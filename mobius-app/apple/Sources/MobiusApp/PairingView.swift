import SwiftUI

struct PairingView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.mobiusPalette) private var palette
    let canCancel: Bool

    var body: some View {
        @Bindable var model = model
        ScrollView {
            VStack(alignment: .leading, spacing: MobiusSpace.xl) {
                HStack(alignment: .top) {
                    SectionHeading(
                        title: "Pair with a gateway",
                        detail: "Use the same address and one-time code on iPad or iPhone."
                    )
                    Spacer()
                    if canCancel {
                        Button("Close", glyph: .x) {
                            model.showsPairing = false
                            dismiss()
                        }
                        .mobiusIconButton()
                        .help("Close")
                    }
                }

                VStack(spacing: MobiusSpace.m) {
                    MobiusCard {
                        VStack(alignment: .leading, spacing: MobiusSpace.l) {
                            VStack(alignment: .leading, spacing: MobiusSpace.s) {
                                Text("Gateway address")
                                    .font(MobiusStyle.controlFont)
                                HStack {
                                    TextField("wss://gateway.example", text: $model.pairingEndpoint)
                                        .textFieldStyle(.roundedBorder)
                                        .textContentType(.URL)
                                        .autocorrectionDisabled()
                                        .controlSize(.large)
                                    PasteButton(payloadType: String.self) { values in
                                        if let value = values.first {
                                            model.applyPairingSetup(value)
                                        }
                                    }
                                    .labelStyle(.iconOnly)
                                    .buttonStyle(.mobiusPlain)
                                    .tint(.primary)
                                    .frame(
                                        width: MobiusStyle.iconButtonSize,
                                        height: MobiusStyle.iconButtonSize
                                    )
                                    .mobiusGlass(in: Circle(), interactive: true, clear: true)
                                    .accessibilityLabel("Paste pairing setup")
                                    .help("Paste pairing setup")
                                }
                            }
                            VStack(alignment: .leading, spacing: MobiusSpace.s) {
                                Text("One-time code")
                                    .font(MobiusStyle.controlFont)
                                SecureField("One-time code", text: $model.pairingCode)
                                    .textFieldStyle(.roundedBorder)
                                    .textInputAutocapitalization(.never)
                                    .autocorrectionDisabled()
                                    .controlSize(.large)
                            }
                        }
                    }

                    Text("Cloud gateways use wss://. tcp:// is accepted only for localhost; direct remote gateways can use tls://.")
                        .font(MobiusStyle.bodyFont)
                        .foregroundStyle(palette.muted)
                        .multilineTextAlignment(.center)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.horizontal, MobiusSpace.s)

                    if let error = model.pairingError {
                        MobiusLabel(
                            verbatim: error,
                            glyph: .warning,
                            iconColor: palette.danger
                        )
                            .foregroundStyle(palette.danger)
                            .multilineTextAlignment(.center)
                    }
                }
                .frame(maxWidth: .infinity)
            }
        }
        .scrollIndicators(.hidden)
        .scrollBounceBehavior(.basedOnSize)
        .scrollDismissesKeyboard(.interactively)
        .safeAreaInset(edge: .bottom) { pairAction }
        .onSubmit { model.pair() }
        .task(id: model.cloudSession?.userID) {
            await model.refreshCloudAccount()
        }
    }

    /// The two ways in, then the wire detail. The protocol line led this stack before, which
    /// put the most technical line on the screen above the decision it belongs under.
    private var pairAction: some View {
        VStack(spacing: MobiusSpace.m) {
            if model.connectionState == .connecting || model.connectionState == .authenticating {
                HStack {
                    MobiusSpinner(size: MobiusStyle.glyphLead, foreground: palette.accent)
                }
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(
                    model.connectionState == .authenticating
                        ? Text("Authenticating with gateway")
                        : Text("Connecting to gateway")
                )
            }
            Button("Pair to self-hosted gateway", action: model.pair)
                .mobiusProminentButton()
                .buttonBorderShape(.capsule)
                .controlSize(.large)
                .buttonSizing(.flexible)
            if !model.hasCloudAccount {
                MobiusCloudOfferButton()
            } else if model.cloudAccount?.subscribed == true, model.mobiusCloudGateway == nil {
                Button("Connect Cloud gateway", glyph: .cloudServer) {
                    Task { _ = await model.connectCloudGateway() }
                }
                .buttonStyle(.mobiusGlass)
                .buttonBorderShape(.capsule)
                .controlSize(.large)
                .buttonSizing(.flexible)
                .tint(palette.accent)
                .disabled(model.cloudAction.isRunning)
            } else if model.cloudAccount == nil {
                Button(model.cloudError == nil ? "Checking Cloud account…" : "Retry Cloud account") {
                    Task { await model.refreshCloudAccount() }
                }
                .buttonStyle(.mobiusGlass)
                .buttonBorderShape(.capsule)
                .controlSize(.large)
                .buttonSizing(.flexible)
                .disabled(model.cloudError == nil)
            } else if model.cloudAccount?.subscribed == false {
                MobiusCloudOfferButton()
            }
            MobiusLabel(
                title: "4-byte framed JSON · protocol v\(gatewayProtocolVersion)",
                glyph: .shieldCheck,
                iconColor: palette.muted
            )
                .font(MobiusStyle.metadataFont)
                .foregroundStyle(palette.muted)
                .padding(.top, MobiusSpace.xxs)
        }
        .frame(maxWidth: .infinity)
        .padding(.top, MobiusSpace.l)
    }
}

extension String {
    var nonEmpty: String? {
        let trimmed = trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}
