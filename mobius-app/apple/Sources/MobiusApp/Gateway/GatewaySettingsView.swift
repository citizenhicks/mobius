import SwiftUI

struct GatewayView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var forgetting: GatewayAccount?

    var body: some View {
        let status = gatewayStatus
        PageScaffold(
            title: "Gateway",
            detail: "Machines paired with this device. Chats run on the selected one.",
            manualSection: "gateway",
            sharesHeaderBackground: true,
            headerAccessory: {
                HeaderActionGroup {
                    Button {
                        model.showsPairing = true
                    } label: {
                        MobiusIcon(.plus, gutter: false)
                    }
                    .groupedHeaderAction(prominent: true)
                    .accessibilityLabel("Pair gateway")
                    .accessibilityHint("Opens pairing with a self-hosted gateway")
                    .help("Pair gateway")
                    SettingsStatusButton(
                        subject: .localized("Gateway"),
                        statusLabel: status.label,
                        statusDetail: status.detail,
                        statusColor: status.color,
                        isLoading: model.gateway.connectionState.isLoading
                    )
                    .groupedHeaderAction()
                }
            }
        ) {
            Section("Paired") {
                if model.gateway.accounts.isEmpty {
                    SettingsCaption("No gateway paired on this device.")
                } else {
                    ForEach(model.gateway.accounts) { account in
                        pairedRow(account)
                    }
                }
            }

            if !model.cloud.hasCloudAccount || model.cloud.cloudAccount?.subscribed == false {
                Section("möbius Cloud") {
                    SettingsCaption("Let möbius provision and manage a private gateway for you.")
                    MobiusCloudOfferButton { model.showsCloudOffer = true }
                }
            }
        }
        .alert(
            "Forget this gateway?",
            isPresented: Binding(
                get: { forgetting != nil },
                set: { if !$0 { forgetting = nil } }
            )
        ) {
            Button("Forget gateway", role: .destructive) {
                forgetting.map(model.forgetGateway)
                forgetting = nil
            }
            Button("Cancel", role: .cancel) { forgetting = nil }
        } message: {
            Text("You will need to pair with this gateway again.")
        }
    }

    private var gatewayStatus: (label: MobiusText, detail: MobiusText, color: Color) {
        switch model.gateway.connectionState {
        case .ready:
            let detail: MobiusText
            if let machineName = model.gateway.selectedAccount?.machineName {
                detail = .localized(
                    "\(model.gateway.accounts.count) paired · \(machineName) selected")
            } else {
                detail = .localized("\(model.gateway.accounts.count) paired · no gateway selected")
            }
            return (
                .localized(model.gateway.connectionState.label),
                detail,
                palette.signal
            )
        case .failed(let message):
            return (.localized("Needs attention"), .verbatim(message), palette.danger)
        default:
            return (
                .localized(model.gateway.connectionState.label),
                .localized("Pair a gateway to run chats on it."),
                model.gateway.connectionState.tone.color(in: palette)
            )
        }
    }

    private func pairedRow(_ account: GatewayAccount) -> some View {
        SettingsNavigationRow(
            hint: "Shows this gateway's settings",
            open: { model.navigationPath = [.settings(.gateway(account.id))] },
            marks: {
                if account.id == model.gateway.selectedAccountID {
                    MobiusIcon(.check, size: MobiusStyle.glyphMark, foreground: palette.signal)
                        .accessibilityLabel("Selected")
                }
            }
        ) {
            SettingsRowLabel(title: .verbatim(account.machineName))
        }
        .swipeActions(edge: .trailing) {
            Button {
                forgetting = account
            } label: {
                MobiusIcon(.trash, foreground: palette.danger)
            }
            .tint(palette.panel)
            .accessibilityLabel("Forget \(account.machineName)")
        }
    }
}

private let githubCredentialTarget = "https://github.com"

private enum HostCredentialSheet: Hashable, Identifiable {
    case git
    case ssh

    var id: Self { self }
}

struct GatewayDetailView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.dismiss) private var dismiss
    @State private var confirmsForget = false
    @State private var showsRename = false
    @State private var hostCredentialSheet: HostCredentialSheet?
    @State private var renameDraft = ""
    let id: UUID

    var body: some View {
        @Bindable var model = model
        if let account = model.gateway.accounts.first(where: { $0.id == id }) {
            detail(account)
                .toolbarRole(.editor)
                .alert("Forget this gateway?", isPresented: $confirmsForget) {
                    Button("Forget gateway", role: .destructive) {
                        model.forgetGateway(account)
                        dismiss()
                    }
                    Button("Cancel", role: .cancel) {}
                } message: {
                    Text("You will need to pair with this gateway again.")
                }
                .alert("Rename gateway", isPresented: $showsRename) {
                    TextField("Gateway name", text: $renameDraft)
                    Button("Cancel", role: .cancel) {}
                    Button("Rename") { model.renameGateway(account, to: renameDraft) }
                        .disabled(
                            renameDraft
                                .trimmingCharacters(in: .whitespacesAndNewlines)
                                .isEmpty
                        )
                }
                .sheet(item: $hostCredentialSheet) { sheet in
                    switch sheet {
                    case .git:
                        GitCredentialSheet()
                    case .ssh:
                        SshCredentialSheet()
                    }
                }
                .task(id: model.gateway.connectionState.isReady) {
                    guard account.id == model.gateway.selectedAccountID,
                        model.gateway.connectionState.isReady
                    else { return }
                    model.probeGitCredential(githubCredentialTarget)
                    model.listSshIdentities()
                }
        } else {
            MobiusUnavailable(
                title: "Gateway unavailable",
                glyph: AppDestination.gateway.glyph,
                detail: "It is no longer paired on this device."
            )
            .navigationTitle("Gateway")
            .toolbarRole(.editor)
            .background(MobiusBackdrop())
        }
    }

    private func detail(_ account: GatewayAccount) -> some View {
        let isActive = account.id == model.gateway.selectedAccountID
        return PageScaffold(
            title: .verbatim(account.machineName),
            detail: .verbatim(""),
            sharesHeaderBackground: true,
            headerAccessory: {
                HeaderOptionsMenu(label: "Gateway actions") {
                    if isActive {
                        Button(action: model.reconnect) {
                            MobiusLabel(title: "Reconnect", glyph: .arrowClockwise)
                        }
                    }
                    Button {
                        renameDraft = account.displayName
                        showsRename = true
                    } label: {
                        MobiusLabel(title: "Rename gateway", glyph: .pencilSimple)
                    }
                    Button(role: .destructive) {
                        confirmsForget = true
                    } label: {
                        MobiusLabel(title: "Forget gateway", glyph: .trash)
                    }
                }
            }
        ) {
            Section("Connection") {
                if isActive {
                    LabeledContent("Status") {
                        HStack(spacing: MobiusSpace.s) {
                            MobiusStatusIndicator(
                                color: model.gateway.connectionState.tone.color(in: palette),
                                isLoading: model.gateway.connectionState.isLoading
                            )
                            Text(model.gateway.connectionState.label)
                        }
                        .font(MobiusStyle.controlFont)
                    }
                }
                HStack(spacing: MobiusSpace.m) {
                    Text("Endpoint")
                    Spacer(minLength: MobiusSpace.s)
                    Text(verbatim: account.endpoint.rawValue)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .frame(maxWidth: .infinity, alignment: .trailing)
                        .textSelection(.enabled)
                }
                LabeledContent("Transport") { Text(transportName(account)) }
                LabeledContent("Name") { Text(verbatim: account.displayName) }
                LabeledContent("Wire protocol") {
                    Text(verbatim: "v\(gatewayProtocolVersion)")
                }
            }

            if isActive {
                Section("Pair another device") {
                    SettingsCaption(
                        "Ask this gateway for a short-lived code, then enter it with the same gateway address on the other device."
                    )
                    if let pairing = model.pairingCodeInfo {
                        Text(verbatim: pairing.code)
                            .font(MobiusStyle.codeFont)
                            .tracking(3)
                            .textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .center)
                        LabeledContent("Expires") {
                            Text(pairing.expiresAt, style: .relative)
                        }
                        .foregroundStyle(palette.muted)
                    }
                }

                MobiusActionRow {
                    if let pairing = model.pairingCodeInfo {
                        ShareLink("Copy or share", item: pairing.code)
                    } else {
                        Button(
                            "Create one-time code",
                            glyph: .key,
                            action: model.createPairingCode
                        )
                        .mobiusProminentButton()
                    }
                }
                .settingsStandaloneRow()

                Section("Host credentials") {
                    Button {
                        hostCredentialSheet = .git
                    } label: {
                        gitCredentialRow
                    }
                    .buttonStyle(.plain)
                    .disabled(!model.gateway.connectionState.isReady)
                    .accessibilityLabel("GitHub credentials")
                    .accessibilityValue(Text(gitCredentialSummary))
                    .accessibilityHint(Text(gitCredentialHint))

                    Button {
                        hostCredentialSheet = .ssh
                    } label: {
                        sshCredentialRow
                    }
                    .buttonStyle(.plain)
                    .disabled(!model.gateway.connectionState.isReady)
                    .accessibilityLabel("SSH identities")
                    .accessibilityValue(sshCredentialSummary.text)
                    .accessibilityHint(Text(sshCredentialHint))
                }
            }
        }
    }

    private var gitCredentialRow: some View {
        HStack(spacing: MobiusSpace.m) {
            SettingsRowLabel(title: "GitHub", detail: gitCredentialSummary) {
                MobiusIcon(.gitBranch, size: MobiusStyle.glyphInline)
            }
            if model.isCheckingGitCredential {
                MobiusSpinner(size: MobiusStyle.glyphMark)
                    .accessibilityHidden(true)
            } else {
                MobiusIcon(
                    model.gitCredentialAvailable == true ? .checkCircle : .caretRight,
                    size: MobiusStyle.glyphMark,
                    foreground: model.gitCredentialAvailable == true
                        ? palette.signal
                        : palette.muted
                )
                .accessibilityHidden(true)
            }
        }
        .contentShape(Rectangle())
    }

    private var gitCredentialSummary: LocalizedStringResource {
        if !model.gateway.connectionState.isReady { return "Connect to check this host." }
        if model.isCheckingGitCredential { return "Checking this host…" }
        if model.gitCredentialAvailable == true { return "Credential found on this host." }
        if model.gitCredentialAvailable == false { return "No credential found. Set up GitHub." }
        return "Couldn’t check this host."
    }

    private var gitCredentialHint: LocalizedStringResource {
        model.gitCredentialAvailable == true
            ? "Shows credential details"
            : "Adds a GitHub HTTPS credential to this gateway host"
    }

    private var sshCredentialRow: some View {
        HStack(spacing: MobiusSpace.m) {
            SettingsRowLabel(
                title: .localized("SSH"),
                detail: sshCredentialSummary
            ) {
                MobiusIcon(.fingerprint, size: MobiusStyle.glyphInline)
            }
            if model.isLoadingSshIdentities || model.isGeneratingSshIdentity {
                MobiusSpinner(size: MobiusStyle.glyphMark)
                    .accessibilityHidden(true)
            } else {
                MobiusIcon(
                    model.sshIdentities?.isEmpty == false ? .checkCircle : .caretRight,
                    size: MobiusStyle.glyphMark,
                    foreground: model.sshIdentities?.isEmpty == false
                        ? palette.signal
                        : palette.muted
                )
                .accessibilityHidden(true)
            }
        }
    }

    private var sshCredentialSummary: MobiusText {
        if !model.gateway.connectionState.isReady {
            return .localized("Connect to check this host.")
        }
        if model.isLoadingSshIdentities { return .localized("Checking this host…") }
        if model.isGeneratingSshIdentity {
            return .localized("Generating an Ed25519 key on this host…")
        }
        if let error = model.sshIdentityError { return .verbatim(error) }
        guard let identities = model.sshIdentities else {
            return .localized("Couldn’t check this host.")
        }
        if identities.isEmpty { return .localized("No public identities found.") }
        if identities.count == 1 { return .localized("1 public identity found.") }
        return .localized("\(identities.count) public identities found.")
    }

    private var sshCredentialHint: LocalizedStringResource {
        model.sshIdentities?.isEmpty == false
            ? "Shows public identity details"
            : "Creates an SSH identity on this gateway host"
    }

    private func transportName(_ account: GatewayAccount) -> LocalizedStringResource {
        if account.endpoint.usesWebSocket { return "WebSocket TLS" }
        return account.endpoint.usesTLS ? "TLS" : "Loopback TCP"
    }
}

private struct GitCredentialSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.mobiusPalette) private var palette
    @State private var username = ""
    @State private var token = ""

    var body: some View {
        NavigationStack {
            Form {
                Section("GitHub") {
                    LabeledContent("Host") { Text(verbatim: "github.com") }
                    if model.gitCredentialAvailable == true {
                        LabeledContent("Status") { Text("Available") }
                        if let username = model.gitCredentialUsername {
                            LabeledContent("Username") { Text(verbatim: username) }
                        }
                    }
                }

                if model.gitCredentialAvailable != true {
                    Section {
                        TextField("GitHub username", text: $username)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        SecureField("Personal access token", text: $token)
                            .textContentType(.password)
                            .privacySensitive()
                    } header: {
                        Text("Credential")
                    } footer: {
                        Text(
                            "Sent once to the host's configured Git helper. Möbius does not store or read it back."
                        )
                    }
                }

                if let error = model.gitCredentialError {
                    Text(verbatim: error)
                        .font(MobiusStyle.captionFont)
                        .foregroundStyle(palette.danger)
                }
            }
            .scrollContentBackground(.hidden)
            .navigationTitle("GitHub credentials")
            .toolbarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    actionButton
                }
            }
        }
        .mobiusSheet()
        .onChange(of: model.gitCredentialAvailable) { _, available in
            if available == true { token = "" }
        }
    }

    @ViewBuilder
    private var actionButton: some View {
        if model.gitCredentialAvailable == true {
            Button("Done") { dismiss() }
        } else {
            Button {
                model.approveGitCredential(
                    target: githubCredentialTarget,
                    username: username,
                    token: token
                )
            } label: {
                Text(actionTitle)
            }
            .disabled(
                !model.gateway.connectionState.isReady
                    || model.isCheckingGitCredential
                    || username.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                    || token.isEmpty
            )
        }
    }

    private var actionTitle: LocalizedStringResource {
        model.isCheckingGitCredential ? "Saving…" : "Save"
    }
}

private struct SshCredentialSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.mobiusPalette) private var palette

    var body: some View {
        NavigationStack {
            Form {
                Section("SSH") {
                    LabeledContent("Status") { Text(status) }
                }

                if let identities = model.sshIdentities, !identities.isEmpty {
                    Section("Public identities") {
                        ForEach(identities) { identity in
                            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                                LabeledContent("Label") { Text(verbatim: identity.label) }
                                LabeledContent("Algorithm") {
                                    Text(verbatim: identity.algorithm)
                                }
                                Text(verbatim: identity.fingerprint)
                                    .font(MobiusStyle.metadataFont)
                                    .foregroundStyle(palette.muted)
                                    .lineLimit(1)
                                    .truncationMode(.middle)
                                    .textSelection(.enabled)
                            }
                            .accessibilityElement(children: .combine)
                            .accessibilityLabel(Text(verbatim: identity.label))
                            .accessibilityValue(
                                "\(identity.algorithm), \(identity.fingerprint)"
                            )
                        }
                    }
                } else {
                    Section {
                        Text(
                            "Create an Ed25519 key pair on this gateway host. The private key never leaves the host."
                        )
                    } footer: {
                        Text("After creation, add the public key to GitHub or another SSH remote.")
                    }
                }

                if let result = model.generatedSshIdentity {
                    Section {
                        Text(verbatim: result.publicKey)
                            .font(MobiusStyle.metadataFont)
                            .foregroundStyle(palette.muted)
                            .fixedSize(horizontal: false, vertical: true)
                            .textSelection(.enabled)
                    } header: {
                        Text("Public key")
                    } footer: {
                        Text(
                            "Creating it does not grant access by itself. The private key stays on the gateway host."
                        )
                    }

                    MobiusActionRow {
                        ShareLink("Copy or share", item: result.publicKey)
                    }
                    .settingsStandaloneRow()
                }

                if let error = model.sshIdentityError {
                    Text(verbatim: error)
                        .font(MobiusStyle.captionFont)
                        .foregroundStyle(palette.danger)
                }
            }
            .scrollContentBackground(.hidden)
            .navigationTitle("SSH credentials")
            .toolbarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: dismiss.callAsFunction)
                }
                ToolbarItem(placement: .confirmationAction) {
                    if model.sshIdentities == nil {
                        Button {
                            model.listSshIdentities()
                        } label: {
                            Text(checkActionTitle)
                        }
                        .disabled(
                            !model.gateway.connectionState.isReady || model.isLoadingSshIdentities)
                    } else if model.sshIdentities?.isEmpty == true {
                        Button {
                            model.generateSshIdentity()
                        } label: {
                            Text(generateActionTitle)
                        }
                        .disabled(
                            !model.gateway.connectionState.isReady || model.isGeneratingSshIdentity)
                    } else {
                        Button("Done", action: dismiss.callAsFunction)
                    }
                }
            }
        }
        .mobiusSheet()
        .onDisappear {
            model.generatedSshIdentity = nil
        }
    }

    private var status: LocalizedStringResource {
        if model.isLoadingSshIdentities { return "Checking…" }
        if model.sshIdentityError != nil { return "Couldn’t check" }
        guard let identities = model.sshIdentities else { return "Unknown" }
        return identities.isEmpty ? "Not configured" : "Available"
    }

    private var checkActionTitle: LocalizedStringResource {
        model.isLoadingSshIdentities ? "Checking…" : "Retry"
    }

    private var generateActionTitle: LocalizedStringResource {
        model.isGeneratingSshIdentity ? "Generating…" : "Generate"
    }
}
