import AuthenticationServices
import SwiftUI

struct MobiusCloudOfferButton: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var showsOffer = false

    // A centred label and no chevron: with a leading glyph, a spacer and a caret this read
    // as a list row that happened to be capsule-shaped. The accent tint marks it as the
    // other path rather than a second copy of the pairing button.
    var body: some View {
        Button {
            showsOffer = true
        } label: {
            Label {
                Text(model.hasCloudAccount ? "Subscribe to möbius Cloud" : "Connect to möbius Cloud")
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
        .sheet(isPresented: $showsOffer) {
            MobiusCloudOfferSheet()
                .presentationDragIndicator(.visible)
        }
        .accessibilityHint("Explains the managed möbius Cloud subscription")
    }
}

private struct MobiusCloudOfferSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var productDisplayPrice: String?
    @State private var productLoadFailed = false
    @State private var stageIsSlow = false

    var body: some View {
        NavigationStack {
            ZStack {
                MobiusBackdrop()
                ScrollView {
                    VStack(alignment: .leading, spacing: MobiusSpace.xl) {
                        hero
                        if let setupStage {
                            setupSteps(current: setupStage)
                        } else {
                            offerDetails
                            controlNote
                        }
                    }
                    .animation(reduceMotion ? nil : .smooth(duration: 0.28), value: setupStage)
                    .frame(maxWidth: 680, alignment: .leading)
                    .padding(.horizontal, MobiusSpace.l)
                    .padding(.top, MobiusSpace.l)
                    .padding(.bottom, MobiusSpace.xl)
                    .frame(maxWidth: .infinity)
                }
                .scrollIndicators(.hidden)
            }
            .navigationTitle("möbius Cloud")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Close") { dismiss() }
                        .disabled(model.cloudAction.isRunning)
                }
            }
            .safeAreaInset(edge: .bottom) { signupBoundary }
        }
        .interactiveDismissDisabled(model.cloudAction.isRunning)
        .task { await loadProduct() }
        .task { await model.refreshCloudAccount() }
    }

    /// The offer hero becomes setup status while a Cloud action is running.
    private var hero: some View {
        let running = setupStage != nil
        return VStack(alignment: .leading, spacing: MobiusSpace.l) {
            // The app's own mark, not a stock globe.
            MobiusComposingOrb()
                .frame(width: 64, height: 64)
                .frame(maxWidth: .infinity)
                .accessibilityHidden(true)
            Text(
                running
                    ? "Setting up your möbius Cloud."
                    : "Your private gateway, managed by möbius."
            )
                .font(.largeTitle.weight(.bold))
                .fixedSize(horizontal: false, vertical: true)
            Text(
                running
                    ? "Keep this screen open. Nothing here needs your attention until it finishes."
                    : "Skip server setup without giving up control. We provision, secure, and maintain a gateway scoped to your account."
            )
                .font(MobiusStyle.bodyFont)
                .foregroundStyle(palette.muted)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// The stage the flow is on, or nil when nothing is running.
    private var setupStage: CloudSetupStage? {
        switch model.cloudAction {
        case .idle, .deleting: nil
        case .signingIn: .signIn
        case .purchasing, .restoring: .subscription
        case .provisioning: .gateway
        case .connecting: .connect
        }
    }

    private func setupSteps(current: CloudSetupStage) -> some View {
        MobiusCard {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(CloudSetupStage.allCases) { stage in
                    if stage != .signIn {
                        Divider().padding(.leading, MobiusStyle.glyphGutter + MobiusSpace.m)
                    }
                    CloudSetupRow(stage: stage, current: current, slow: stageIsSlow)
                }
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
        MobiusCard {
            VStack(alignment: .leading, spacing: 0) {
                CloudBenefit(
                    glyph: .sparkle,
                    title: "The open-source gateway, hosted for you",
                    detail: "Run the same generic möbius gateway in a private, persistent workspace."
                )
                Divider().padding(.leading, MobiusStyle.glyphGutter + MobiusSpace.m)
                CloudBenefit(
                    glyph: .setup01,
                    title: "Fast, modular harness",
                    detail: "Choose the providers, tools, and capabilities you want while möbius keeps the runtime lean."
                )
                Divider().padding(.leading, MobiusStyle.glyphGutter + MobiusSpace.m)
                CloudBenefit(
                    glyph: .key,
                    title: "Bring your own keys",
                    detail: "Connect your own model provider account without storing its API key in möbius Cloud or the gateway filesystem."
                )
                Divider().padding(.leading, MobiusStyle.glyphGutter + MobiusSpace.m)
                CloudBenefit(
                    glyph: .shieldCheck,
                    title: "Encrypted and user-scoped",
                    detail: "Your gateway, credentials, and cloud data stay isolated to your account."
                )
            }
        }
    }

    private var controlNote: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.s) {
            Text("You stay in control")
                .font(MobiusStyle.titleFont)
            Text("Manage your subscription from the möbius app or App Store.")
                .font(MobiusStyle.bodyFont)
                .foregroundStyle(palette.muted)
                .fixedSize(horizontal: false, vertical: true)
            billingDescription
                .font(MobiusStyle.controlFont)
                .foregroundStyle(.primary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// Sign in with Apple is a branded control: Apple's guidelines allow black, white, or
    /// outlined only, so it cannot wear the app's accent. White is the variant meant for a
    /// dark background. The fade underneath is not a bar — it only keeps the last line of
    /// text from colliding with the capsule as the page scrolls past it.
    private var signupBoundary: some View {
        VStack(spacing: MobiusSpace.m) {
            if let cloudError = model.cloudError {
                Text(cloudError)
                    .font(MobiusStyle.captionFont)
                    .foregroundStyle(palette.danger)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if model.cloudAction.isRunning {
                EmptyView()
            } else if model.cloudAccount?.subscribed == true {
                Button("Connect gateway") {
                    Task {
                        if await model.connectCloudGateway() { dismiss() }
                    }
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.extraLarge)
                .frame(maxWidth: .infinity)
            } else if !model.hasCloudAccount {
                MobiusCloudAppleAuthorizationButton(label: .continue) {
                    authorizationCode, nonce in
                    Task {
                        if await model.signInAndPurchaseCloud(
                            authorizationCode: authorizationCode,
                            nonce: nonce
                        ) {
                            dismiss()
                        }
                    }
                } onFailure: {
                    model.reportCloudSignInFailure()
                }
            } else if model.hasCloudAccount, model.cloudAccount == nil {
                if model.cloudError == nil {
                    waitingButton("Checking subscription…")
                } else {
                    Button("Retry subscription check") {
                        Task { await model.refreshCloudAccount() }
                    }
                    .buttonStyle(.bordered)
                    .controlSize(.extraLarge)
                    .frame(maxWidth: .infinity)
                }
            } else if model.cloudIssue == .subscriptionAccountConflict {
                VStack(spacing: MobiusSpace.s) {
                    Button("Manage App Store subscription") {
                        Task { await model.manageCloudSubscription() }
                    }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.extraLarge)
                    Button("Sign out of Cloud") {
                        Task { await model.signOutOfCloud() }
                    }
                    .buttonStyle(.bordered)
                    .controlSize(.large)
                }
                .frame(maxWidth: .infinity)
            } else if productDisplayPrice != nil {
                Button("Subscribe") {
                    Task {
                        if await model.purchaseCloud() { dismiss() }
                    }
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.extraLarge)
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
        .frame(maxWidth: 680)
        .padding(.horizontal, MobiusSpace.l)
        .padding(.top, MobiusSpace.l)
        .padding(.bottom, MobiusSpace.s)
        .frame(maxWidth: .infinity)
        .background {
            LinearGradient(
                colors: [palette.canvas.opacity(0), palette.canvas],
                startPoint: .top,
                endPoint: .bottom
            )
            .ignoresSafeArea()
            .allowsHitTesting(false)
        }
    }

    private func waitingButton(_ title: String) -> some View {
        Button {} label: {
            HStack(spacing: MobiusSpace.s) {
                MobiusSpinner(size: MobiusStyle.glyphInline)
                Text(title)
            }
            .frame(maxWidth: .infinity)
        }
        .buttonStyle(.bordered)
        .controlSize(.extraLarge)
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
            productDisplayPrice = try await model.cloudProductDisplayPrice()
        } catch {
            productLoadFailed = true
        }
    }

}

struct MobiusCloudAppleAuthorizationButton: View {
    let label: SignInWithAppleButton.Label
    let onAuthorization: @MainActor (String, String) -> Void
    let onFailure: @MainActor () -> Void
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
                   authorizationError.code == .canceled {
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
        .signInWithAppleButtonStyle(.white)
        .frame(maxWidth: .infinity, minHeight: 50, maxHeight: 50)
    }
}

/// Ordered stages shown while Cloud setup is running.
private enum CloudSetupStage: Int, CaseIterable, Identifiable {
    case signIn
    case subscription
    case gateway
    case connect

    var id: Int { rawValue }

    var title: String {
        switch self {
        case .signIn: "Account"
        case .subscription: "Subscription"
        case .gateway: "Private gateway"
        case .connect: "Connection"
        }
    }

    func detail(slow: Bool) -> String {
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
        .accessibilityValue(accessibilityStatus)
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

    private var accessibilityStatus: String {
        if isPending { return "Waiting" }
        return stage == current ? "In progress" : "Done"
    }
}

private struct CloudBenefit: View {
    @Environment(\.mobiusPalette) private var palette
    let glyph: MobiusGlyph
    let title: String
    let detail: String

    var body: some View {
        HStack(alignment: .top, spacing: MobiusSpace.m) {
            MobiusIcon(glyph, size: MobiusStyle.glyphLead, foreground: palette.accent)
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
