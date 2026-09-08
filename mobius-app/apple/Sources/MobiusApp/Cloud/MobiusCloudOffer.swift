import AuthenticationServices
import SwiftUI

struct MobiusCloudOfferButton: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette

    let action: () -> Void

    // A centred label and no chevron: with a leading glyph, a spacer and a caret this read
    // as a list row that happened to be capsule-shaped. The accent tint marks it as the
    // other path rather than a second copy of the pairing button.
    var body: some View {
        let title: LocalizedStringResource =
            model.cloud.cloudAccount?.subscribed == true
            ? "Connect Cloud gateway"
            : model.cloud.hasCloudAccount
                ? "Subscribe to möbius Cloud"
                : "Connect to möbius Cloud"
        let hint: LocalizedStringResource =
            model.cloud.cloudAccount?.subscribed == true
            ? "Connects this device to your managed Cloud gateway"
            : "Explains the managed möbius Cloud subscription"
        Button(action: action) {
            Label {
                Text(title)
                    // Glass takes a tint from its own material, not from the button's, so
                    // the accent has to be carried by the label for it to read at all.
                    .foregroundStyle(palette.accent)
            } icon: {
                // The product's own mark, drawn full-colour: the logo is artwork rather
                // than a template glyph, so it keeps its own colours beside accent text.
                Image("MobiusLogo")
                    .resizable()
                    .scaledToFit()
                    .frame(width: 20, height: 20)
                    .accessibilityHidden(true)
            }
            .font(MobiusStyle.controlFont)
        }
        .buttonStyle(.mobiusGlass)
        .tint(palette.accent)
        .buttonBorderShape(.capsule)
        .controlSize(.large)
        .buttonSizing(.flexible)
        .accessibilityHint(Text(hint))
    }
}

struct MobiusCloudOfferContent: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var productDisplayPrice: String?
    @State private var productLoadFailed = false
    @State private var stageIsSlow = false
    let onConnected: () -> Void

    var body: some View {
        VStack(spacing: MobiusSpace.xl) {
            hero
            if let setupStage {
                setupSteps(current: setupStage)
            } else {
                controlNote
            }
            signupBoundary
            if setupStage == nil {
                DisclosureGroup("What’s included") { offerDetails }
                    .font(MobiusStyle.captionFont)
                    .tint(palette.accent)
            }
        }
        .animation(reduceMotion ? nil : .smooth(duration: 0.28), value: setupStage)
        .task { await loadProduct() }
        .task { await model.cloud.refreshCloudAccount() }
    }

    /// The offer hero becomes setup status while a Cloud action is running.
    private var hero: some View {
        let running = setupStage != nil
        let title: LocalizedStringResource =
            running
            ? "Setting up your möbius Cloud."
            : "Your private gateway, managed by möbius."
        let detail: LocalizedStringResource =
            running
            ? "Keep this screen open. Nothing here needs your attention until it finishes."
            : "Skip server setup without giving up control. We provision, secure, and maintain a gateway scoped to your account."
        return VStack(spacing: MobiusSpace.s) {
            Text(title)
                .font(.title.weight(.semibold))
                .accessibilityAddTraits(.isHeader)
                .fixedSize(horizontal: false, vertical: true)
            Text(detail)
                .font(MobiusStyle.captionFont)
                .foregroundStyle(palette.muted)
                .fixedSize(horizontal: false, vertical: true)
        }
        .multilineTextAlignment(.center)
    }

    /// The stage the flow is on, or nil when nothing is running.
    private var setupStage: CloudSetupStage? {
        switch model.cloud.cloudAction {
        case .idle, .deleting: nil
        case .signingIn: .signIn
        case .purchasing, .restoring: .subscription
        case .provisioning: .gateway
        case .connecting: .connect
        }
    }

    private func setupSteps(current: CloudSetupStage) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(CloudSetupStage.allCases) { stage in
                if stage != .signIn {
                    Divider().padding(.leading, MobiusStyle.glyphGutter + MobiusSpace.m)
                }
                CloudSetupRow(stage: stage, current: current, slow: stageIsSlow)
            }
        }
        // Explain unusually slow provisioning without declaring failure.
        .task(id: current) {
            stageIsSlow = false
            guard current == .gateway else { return }
            if (try? await Task.sleep(for: .seconds(30))) != nil { stageIsSlow = true }
        }
    }

    private var offerDetails: some View {
        VStack(alignment: .leading, spacing: 0) {
            CloudBenefit(
                glyph: .sparkle,
                title: "The open-source gateway, hosted for you",
                detail:
                    "Run the same generic möbius gateway in a private, persistent workspace."
            )
            Divider().padding(.leading, MobiusStyle.glyphGutter + MobiusSpace.m)
            CloudBenefit(
                glyph: .setup01,
                title: "Fast, modular harness",
                detail:
                    "Choose the providers, tools, and capabilities you want while möbius keeps the runtime lean."
            )
            Divider().padding(.leading, MobiusStyle.glyphGutter + MobiusSpace.m)
            CloudBenefit(
                glyph: .key,
                title: "Bring your own keys",
                detail:
                    "Connect your own model provider account without storing its API key in möbius Cloud or the gateway filesystem."
            )
            Divider().padding(.leading, MobiusStyle.glyphGutter + MobiusSpace.m)
            CloudBenefit(
                glyph: .shieldCheck,
                title: "Encrypted and user-scoped",
                detail:
                    "Your gateway, credentials, and cloud data stay isolated to your account."
            )
        }
    }

    private var controlNote: some View {
        VStack(spacing: MobiusSpace.s) {
            billingDescription
                .font(MobiusStyle.controlFont)
                .foregroundStyle(.primary)
                .fixedSize(horizontal: false, vertical: true)
            Text("Manage your subscription from the möbius app or App Store.")
                .font(MobiusStyle.captionFont)
                .foregroundStyle(palette.muted)
                .fixedSize(horizontal: false, vertical: true)
        }
        .multilineTextAlignment(.center)
    }

    /// Keep Apple's branded authorization control and the existing purchase boundaries.
    private var signupBoundary: some View {
        VStack(spacing: MobiusSpace.m) {
            if model.cloud.cloudAction.isRunning {
                EmptyView()
            } else if model.cloud.cloudAccount?.subscribed == true {
                Button("Connect gateway") {
                    Task {
                        if await model.cloud.connectCloudGateway() { onConnected() }
                    }
                }
                .mobiusProminentButton()
                .controlSize(.large)
                .frame(maxWidth: .infinity)
            } else if !model.cloud.hasCloudAccount {
                MobiusCloudAppleAuthorizationButton(label: .continue) {
                    authorizationCode, nonce in
                    Task {
                        if await model.cloud.signInAndPurchaseCloud(
                            authorizationCode: authorizationCode,
                            nonce: nonce
                        ) {
                            onConnected()
                        }
                    }
                } onFailure: {
                    model.cloud.reportCloudSignInFailure()
                }
            } else if model.cloud.hasCloudAccount, model.cloud.cloudAccount == nil {
                if model.cloud.cloudError == nil {
                    waitingButton("Checking subscription…")
                } else {
                    Button("Retry subscription check") {
                        Task { await model.cloud.refreshCloudAccount() }
                    }
                    .buttonStyle(.bordered)
                    .controlSize(.large)
                    .frame(maxWidth: .infinity)
                }
            } else if model.cloud.cloudIssue == .subscriptionAccountConflict {
                VStack(spacing: MobiusSpace.s) {
                    Button("Manage App Store subscription") {
                        Task { await model.cloud.manageCloudSubscription() }
                    }
                    .mobiusProminentButton()
                    .controlSize(.large)
                    Button("Sign out of Cloud") {
                        Task { await model.cloud.signOutOfCloud() }
                    }
                    .buttonStyle(.bordered)
                    .controlSize(.large)
                }
                .frame(maxWidth: .infinity)
            } else if productDisplayPrice != nil {
                Button("Subscribe") {
                    Task {
                        if await model.cloud.purchaseCloud() { onConnected() }
                    }
                }
                .mobiusProminentButton()
                .controlSize(.large)
                .frame(maxWidth: .infinity)
            } else if productLoadFailed {
                VStack(spacing: MobiusSpace.s) {
                    Text("The App Store price could not be loaded.")
                        .font(MobiusStyle.captionFont)
                        .foregroundStyle(palette.muted)
                    Button("Retry App Store") {
                        Task { await loadProduct() }
                    }
                    .buttonStyle(.bordered)
                    .controlSize(.large)
                }
                .frame(maxWidth: .infinity, minHeight: 50)
            } else {
                waitingButton("Connecting to the App Store…")
            }
        }
        .buttonBorderShape(.capsule)
        .buttonSizing(.flexible)
        .frame(maxWidth: .infinity)
    }

    private func waitingButton(_ title: LocalizedStringResource) -> some View {
        Button {
        } label: {
            HStack(spacing: MobiusSpace.s) {
                MobiusSpinner(size: MobiusStyle.glyphInline)
                Text(title)
            }
            .frame(maxWidth: .infinity)
        }
        .buttonStyle(.bordered)
        .controlSize(.large)
        .disabled(true)
        .frame(maxWidth: .infinity)
    }

    private var billingDescription: Text {
        guard let productDisplayPrice else {
            return Text(
                "Billed monthly. \(Text("Price shown at purchase.").foregroundStyle(palette.muted))"
            )
        }
        return Text(
            "\(productDisplayPrice) a month. \(Text("Cancel anytime.").foregroundStyle(palette.muted))"
        )
    }

    private func loadProduct() async {
        guard productDisplayPrice == nil else { return }
        productLoadFailed = false
        do {
            productDisplayPrice = try await model.cloud.cloudProductDisplayPrice()
        } catch {
            productLoadFailed = true
        }
    }

}

struct MobiusCloudAppleAuthorizationButton: View {
    let label: SignInWithAppleButton.Label
    let onAuthorization: @MainActor (String, String) -> Void
    let onFailure: @MainActor () -> Void
    @Environment(\.colorScheme) private var colorScheme
    @State private var nonce: MobiusCloudAppleNonce?

    var body: some View {
        SignInWithAppleButton(label) { request in
            do {
                let nonce = try MobiusCloudAppleNonce.make()
                self.nonce = nonce
                request.requestedScopes = [.email]
                request.nonce = nonce.requestValue
            } catch {
                nonce = nil
                onFailure()
            }
        } onCompletion: { result in
            switch result {
            case .failure(let error):
                nonce = nil
                if let authorizationError = error as? ASAuthorizationError,
                    authorizationError.code == .canceled
                {
                    return
                }
                onFailure()
            case .success(let authorization):
                guard let nonce,
                    let credential = authorization.credential as? ASAuthorizationAppleIDCredential,
                    let data = credential.authorizationCode,
                    let authorizationCode = String(data: data, encoding: .utf8)
                else {
                    self.nonce = nil
                    onFailure()
                    return
                }
                self.nonce = nil
                onAuthorization(authorizationCode, nonce.rawValue)
            }
        }
        .signInWithAppleButtonStyle(colorScheme == .dark ? .white : .black)
        .frame(maxWidth: .infinity, minHeight: 50, maxHeight: 50)
        .clipShape(.capsule)
    }
}

/// Ordered stages shown while Cloud setup is running.
private enum CloudSetupStage: Int, CaseIterable, Identifiable {
    case signIn
    case subscription
    case gateway
    case connect

    var id: Int { rawValue }

    var title: LocalizedStringResource {
        switch self {
        case .signIn: "Account"
        case .subscription: "Subscription"
        case .gateway: "Private gateway"
        case .connect: "Connection"
        }
    }

    func detail(slow: Bool) -> LocalizedStringResource {
        switch self {
        case .signIn: "Verifying your Apple account."
        case .subscription: "Confirming your App Store purchase."
        case .gateway:
            slow
                ? "Still provisioning. This one is taking longer than usual; the screen moves on by itself as soon as the gateway answers."
                : "möbius is provisioning a gateway for your account. This usually takes about fifteen seconds."
        case .connect: "Pairing this device with your gateway."
        }
    }
}

private struct CloudSetupRow: View {
    @Environment(\.mobiusPalette) private var palette
    let stage: CloudSetupStage
    let current: CloudSetupStage
    var slow = false

    var body: some View {
        HStack(alignment: .top, spacing: MobiusSpace.m) {
            mark
                .frame(width: MobiusStyle.glyphLead, height: MobiusStyle.glyphLead)
            VStack(alignment: .leading, spacing: MobiusSpace.xs) {
                Text(stage.title)
                    .font(MobiusStyle.controlFont)
                    .foregroundStyle(isPending ? palette.muted : .primary)
                if stage == current {
                    Text(stage.detail(slow: slow))
                        .font(MobiusStyle.bodyFont)
                        .foregroundStyle(palette.muted)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
        .padding(.vertical, MobiusSpace.m)
        .accessibilityElement(children: .combine)
        .accessibilityValue(Text(accessibilityStatus))
    }

    private var isPending: Bool { stage.rawValue > current.rawValue }

    @ViewBuilder
    private var mark: some View {
        if stage.rawValue < current.rawValue {
            MobiusIcon(
                .checkCircle,
                size: MobiusStyle.glyphLead,
                foreground: palette.signal,
                gutter: false
            )
        } else if stage == current {
            MobiusSpinner(size: MobiusStyle.glyphLead)
        } else {
            Circle().strokeBorder(palette.line, lineWidth: MobiusStyle.borderWidth)
        }
    }

    private var accessibilityStatus: LocalizedStringResource {
        if isPending { return "Waiting" }
        return stage == current ? "In progress" : "Done"
    }
}

private struct CloudBenefit: View {
    @Environment(\.mobiusPalette) private var palette
    let glyph: MobiusGlyph
    let title: LocalizedStringResource
    let detail: LocalizedStringResource

    var body: some View {
        HStack(alignment: .top, spacing: MobiusSpace.m) {
            MobiusIcon(glyph, size: MobiusStyle.glyphLead, foreground: .primary)
            VStack(alignment: .leading, spacing: MobiusSpace.xs) {
                Text(title)
                    .font(MobiusStyle.controlFont)
                Text(detail)
                    .font(MobiusStyle.bodyFont)
                    .foregroundStyle(palette.muted)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.vertical, MobiusSpace.m)
    }
}
