import SwiftUI

struct SettingsInfoButton: View {
    @Environment(\.mobiusPalette) private var palette
    @State private var showsDetail = false
    let title: MobiusText
    let detail: MobiusText
    var glyph: MobiusGlyph = .info
    var accessibilityHint: MobiusText = .localized("Shows setting guidance")
    /// Beside a section header or a stacked label, where a full 44pt target would push the
    /// rows under it down and leave that section sitting lower than every other one.
    var compact = false

    init(
        title: LocalizedStringResource,
        detail: LocalizedStringResource,
        glyph: MobiusGlyph = .info,
        accessibilityHint: LocalizedStringResource = "Shows setting guidance",
        compact: Bool = false
    ) {
        self.init(
            title: .localized(title),
            detail: .localized(detail),
            glyph: glyph,
            accessibilityHint: .localized(accessibilityHint),
            compact: compact
        )
    }

    init(
        title: MobiusText,
        detail: MobiusText,
        glyph: MobiusGlyph = .info,
        accessibilityHint: MobiusText = .localized("Shows setting guidance"),
        compact: Bool = false
    ) {
        self.title = title
        self.detail = detail
        self.glyph = glyph
        self.accessibilityHint = accessibilityHint
        self.compact = compact
    }

    var body: some View {
        Button {
            showsDetail = true
        } label: {
            MobiusIcon(glyph, size: MobiusStyle.glyphInline, foreground: palette.muted)
                .frame(
                    minWidth: MobiusStyle.iconButtonSize,
                    minHeight: compact ? MobiusStyle.iconSize : MobiusStyle.iconButtonSize
                )
                .contentShape(Rectangle())
        }
        .buttonStyle(.mobiusPlain)
        .accessibilityLabel(aboutTitle)
        .accessibilityHint(accessibilityHint.text)
        .help(aboutTitle)
        .sensoryFeedback(.selection, trigger: showsDetail)
        .popover(isPresented: $showsDetail) {
            VStack(alignment: .leading, spacing: MobiusSpace.s) {
                title.text
                    .font(MobiusStyle.controlFont.weight(.semibold))
                detail.text
                    .font(MobiusStyle.bodyFont)
                    .foregroundStyle(palette.muted)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(MobiusSpace.l)
            .frame(width: 280, alignment: .leading)
            .presentationCompactAdaptation(.popover)
        }
    }

    private var aboutTitle: Text {
        switch title {
        case .localized(let resource): Text("About \(resource)")
        case .verbatim(let value): Text("About \(value)")
        }
    }
}

struct SettingsStatusAccessory: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    let subject: MobiusText
    let hasChanges: Bool
    let isSaving: Bool
    let saveDisabled: Bool
    let statusLabel: MobiusText
    let statusDetail: MobiusText
    let statusColor: Color
    let saveLabel: MobiusText
    var secondaryActionLabel: MobiusText?
    var secondaryAction: (() -> Void)?
    let save: () -> Void

    init(
        subject: LocalizedStringResource,
        hasChanges: Bool,
        isSaving: Bool,
        saveDisabled: Bool,
        statusLabel: LocalizedStringResource,
        statusDetail: LocalizedStringResource,
        statusColor: Color,
        saveLabel: LocalizedStringResource,
        secondaryActionLabel: LocalizedStringResource? = nil,
        secondaryAction: (() -> Void)? = nil,
        save: @escaping () -> Void
    ) {
        self.init(
            subject: .localized(subject),
            hasChanges: hasChanges,
            isSaving: isSaving,
            saveDisabled: saveDisabled,
            statusLabel: .localized(statusLabel),
            statusDetail: .localized(statusDetail),
            statusColor: statusColor,
            saveLabel: .localized(saveLabel),
            secondaryActionLabel: secondaryActionLabel.map { .localized($0) },
            secondaryAction: secondaryAction,
            save: save
        )
    }

    init(
        subject: MobiusText,
        hasChanges: Bool,
        isSaving: Bool,
        saveDisabled: Bool,
        statusLabel: MobiusText,
        statusDetail: MobiusText,
        statusColor: Color,
        saveLabel: MobiusText,
        secondaryActionLabel: MobiusText? = nil,
        secondaryAction: (() -> Void)? = nil,
        save: @escaping () -> Void
    ) {
        self.subject = subject
        self.hasChanges = hasChanges
        self.isSaving = isSaving
        self.saveDisabled = saveDisabled
        self.statusLabel = statusLabel
        self.statusDetail = statusDetail
        self.statusColor = statusColor
        self.saveLabel = saveLabel
        self.secondaryActionLabel = secondaryActionLabel
        self.secondaryAction = secondaryAction
        self.save = save
    }

    var body: some View {
        HeaderActionGroup {
            if hasChanges {
                saveButton
            }
            statusButton
        }
        .animation(
            reduceMotion ? nil : .spring(response: 0.34, dampingFraction: 0.78),
            value: hasChanges
        )
    }

    private var statusButton: some View {
        SettingsStatusButton(
            subject: subject,
            statusLabel: statusLabel,
            statusDetail: statusDetail,
            statusColor: statusColor,
            secondaryActionLabel: secondaryActionLabel,
            secondaryAction: secondaryAction
        )
        .tint(.primary)
        // Only half a shared surface has to draw the full target; alone, letting the
        // system's glass hug the dot is what keeps it a circle rather than a pill.
        .frame(
            width: hasChanges ? MobiusStyle.iconButtonSize : nil,
            height: hasChanges ? MobiusStyle.iconButtonSize : nil
        )
        .contentShape(Rectangle())
    }

    private var saveButton: some View {
        Button(action: save) {
            Label {
                saveLabel.text
            } icon: {
                Group {
                    if isSaving {
                        MobiusSpinner(size: MobiusStyle.iconSize)
                    } else {
                        MobiusIcon(.saveAll, size: MobiusStyle.iconSize)
                    }
                }
            }
        }
        .labelStyle(.iconOnly)
        .groupedHeaderAction(prominent: true)
        .disabled(saveDisabled)
        .accessibilityLabel(saveLabel.text)
        .help(saveLabel.text)
        .sensoryFeedback(.success, trigger: hasChanges) { was, now in was && !now }
    }
}

struct SettingsStatusButton: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.mobiusPalette) private var palette
    @State private var showsStatus = false
    let subject: MobiusText
    let statusLabel: MobiusText
    let statusDetail: MobiusText
    let statusColor: Color
    var secondaryActionLabel: MobiusText?
    var secondaryAction: (() -> Void)?

    init(
        subject: LocalizedStringResource,
        statusLabel: LocalizedStringResource,
        statusDetail: LocalizedStringResource,
        statusColor: Color,
        secondaryActionLabel: LocalizedStringResource? = nil,
        secondaryAction: (() -> Void)? = nil
    ) {
        self.init(
            subject: .localized(subject),
            statusLabel: .localized(statusLabel),
            statusDetail: .localized(statusDetail),
            statusColor: statusColor,
            secondaryActionLabel: secondaryActionLabel.map { .localized($0) },
            secondaryAction: secondaryAction
        )
    }

    init(
        subject: MobiusText,
        statusLabel: MobiusText,
        statusDetail: MobiusText,
        statusColor: Color,
        secondaryActionLabel: MobiusText? = nil,
        secondaryAction: (() -> Void)? = nil
    ) {
        self.subject = subject
        self.statusLabel = statusLabel
        self.statusDetail = statusDetail
        self.statusColor = statusColor
        self.secondaryActionLabel = secondaryActionLabel
        self.secondaryAction = secondaryAction
    }

    var body: some View {
        Button {
            showsStatus = true
        } label: {
            Circle()
                .fill(statusColor)
                .frame(width: 8, height: 8)
                .symbolEffect(
                    .pulse.byLayer,
                    options: .repeat(.continuous),
                    isActive: !reduceMotion
                )
        }
        .accessibilityLabel(statusAccessibilityLabel)
        .accessibilityValue(statusLabel.text)
        .help(statusHelp)
        .popover(isPresented: $showsStatus) {
            VStack(spacing: MobiusSpace.m) {
                statusLabel.text
                    .font(MobiusStyle.controlFont.weight(.semibold))
                    .foregroundStyle(statusColor)
                statusDetail.text
                    .font(MobiusStyle.bodyFont)
                    .foregroundStyle(palette.muted)
                if let secondaryActionLabel, let secondaryAction {
                    Divider()
                    Button {
                        showsStatus = false
                        secondaryAction()
                    } label: {
                        secondaryActionLabel.text
                    }
                }
            }
            .multilineTextAlignment(.center)
            .padding(MobiusSpace.l)
            .frame(width: 280)
            .presentationCompactAdaptation(.popover)
        }
    }

    private var statusAccessibilityLabel: Text {
        switch subject {
        case .localized(let resource): Text("\(resource) status")
        case .verbatim(let value): Text("\(value) status")
        }
    }

    private var statusHelp: Text {
        switch (subject, statusLabel) {
        case (.localized(let subject), .localized(let status)):
            Text("\(subject): \(status)")
        case (.localized(let subject), .verbatim(let status)):
            Text("\(subject): \(status)")
        case (.verbatim(let subject), .localized(let status)):
            Text("\(subject): \(status)")
        case (.verbatim(let subject), .verbatim(let status)):
            Text("\(subject): \(status)")
        }
    }
}

struct GatewayView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.mobiusPalette) private var palette
    @State private var forgetting: GatewayAccount?

    var body: some View {
        let status = gatewayStatus
        PageScaffold(
            title: "Gateway",
            detail: "Machines paired with this device. Chats run on the selected one.",
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
                        statusColor: status.color
                    )
                    .groupedHeaderAction()
                }
            }
        ) {
            if !model.accounts.isEmpty {
                Section("Active") {
                    // The same control as the chats header, so switching does not
                    // require stepping into a gateway's detail page.
                    Picker("Gateway", selection: Binding(
                        get: { model.selectedAccountID },
                        set: { model.selectAccount($0) }
                    )) {
                        ForEach(model.accounts) { account in
                            Text(verbatim: account.machineName)
                                .lineLimit(1)
                                .truncationMode(.middle)
                                .tag(Optional(account.id))
                        }
                    }
                    .settingsPickerStyle()
                    .sensoryFeedback(.selection, trigger: model.selectedAccountID)
                    LabeledContent("Status") {
                        HStack(spacing: MobiusSpace.s) {
                            Circle()
                                .fill(model.connectionState.tone.color(in: palette))
                                .frame(width: 7, height: 7)
                            Text(model.connectionState.label)
                        }
                        .font(MobiusStyle.controlFont)
                    }
                }
            }

            Section("Paired") {
                if model.accounts.isEmpty {
                    SettingsCaption("No gateway paired on this device.")
                } else {
                    ForEach(model.accounts) { account in
                        pairedRow(account)
                    }
                }
            }

            if !model.hasCloudAccount || model.cloudAccount?.subscribed == false {
                Section("möbius Cloud") {
                    SettingsCaption("Let möbius provision and manage a private gateway for you.")
                    MobiusCloudOfferButton()
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
        switch model.connectionState {
        case .ready:
            let detail: MobiusText
            if let machineName = model.selectedAccount?.machineName {
                detail = .localized("\(model.accounts.count) paired · \(machineName) selected")
            } else {
                detail = .localized("\(model.accounts.count) paired · no gateway selected")
            }
            return (
                .localized(model.connectionState.label),
                detail,
                palette.signal
            )
        case .failed(let message):
            return (.localized("Needs attention"), .verbatim(message), palette.danger)
        default:
            return (
                .localized(model.connectionState.label),
                .localized("Pair a gateway to run chats on it."),
                palette.warning
            )
        }
    }

    private func pairedRow(_ account: GatewayAccount) -> some View {
        SettingsNavigationRow(
            hint: "Shows this gateway's settings",
            open: { model.navigationPath = [.settings(.gateway(account.id))] },
            marks: {
                if account.id == model.selectedAccountID {
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
        if let account = model.accounts.first(where: { $0.id == id }) {
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
                .task(id: model.connectionState.isReady) {
                    guard account.id == model.selectedAccountID,
                          model.connectionState.isReady
                    else { return }
                    if model.gitCredentialAvailable == nil {
                        model.probeGitCredential(githubCredentialTarget)
                    }
                    if model.sshIdentities == nil {
                        model.listSshIdentities()
                    }
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
        let isActive = account.id == model.selectedAccountID
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
                            Circle()
                                .fill(model.connectionState.tone.color(in: palette))
                                .frame(width: 7, height: 7)
                            Text(model.connectionState.label)
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
                    SettingsCaption("Ask this gateway for a short-lived code, then enter it with the same gateway address on the other device.")
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
                    .disabled(!model.connectionState.isReady)
                    .accessibilityLabel("GitHub credentials")
                    .accessibilityValue(Text(gitCredentialSummary))
                    .accessibilityHint(Text(gitCredentialHint))

                    Button {
                        hostCredentialSheet = .ssh
                    } label: {
                        sshCredentialRow
                    }
                    .buttonStyle(.plain)
                    .disabled(!model.connectionState.isReady)
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
        if !model.connectionState.isReady { return "Connect to check this host." }
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
        if !model.connectionState.isReady { return .localized("Connect to check this host.") }
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
                        Text("Sent once to the host's configured Git helper. Möbius does not store or read it back.")
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
    }

    @ViewBuilder
    private var actionButton: some View {
        if model.gitCredentialAvailable == true {
            Button("Done") { dismiss() }
        } else {
            Button {
                let value = token
                token = ""
                model.approveGitCredential(
                    target: githubCredentialTarget,
                    username: username,
                    token: value
                )
            } label: {
                Text(actionTitle)
            }
            .disabled(
                model.isCheckingGitCredential
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
                        Text("Create an Ed25519 key pair on this gateway host. The private key never leaves the host.")
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
                        Text("Creating it does not grant access by itself. The private key stays on the gateway host.")
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
                        .disabled(model.isLoadingSshIdentities)
                    } else if model.sshIdentities?.isEmpty == true {
                        Button {
                            model.generateSshIdentity()
                        } label: {
                            Text(generateActionTitle)
                        }
                        .disabled(model.isGeneratingSshIdentity)
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

struct PageScaffold<HeaderAccessory: View, Content: View>: View {
    let title: MobiusText
    let detail: MobiusText
    let sharesHeaderBackground: Bool
    let headerAccessory: HeaderAccessory
    let content: Content

    init(
        title: LocalizedStringResource,
        detail: LocalizedStringResource,
        sharesHeaderBackground: Bool = false,
        @ViewBuilder headerAccessory: () -> HeaderAccessory,
        @ViewBuilder content: () -> Content
    ) {
        self.init(
            title: .localized(title),
            detail: .localized(detail),
            sharesHeaderBackground: sharesHeaderBackground,
            headerAccessory: headerAccessory,
            content: content
        )
    }

    init(
        title: MobiusText,
        detail: MobiusText,
        sharesHeaderBackground: Bool = false,
        @ViewBuilder headerAccessory: () -> HeaderAccessory,
        @ViewBuilder content: () -> Content
    ) {
        self.title = title
        self.detail = detail
        self.sharesHeaderBackground = sharesHeaderBackground
        self.headerAccessory = headerAccessory()
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Form {
                if !detail.isEmpty {
                    SettingsCaption(detail)
                        .listRowBackground(Color.clear)
                }
                content
            }
            .formStyle(.grouped)
            // One rhythm for every settings page: sections a card's gap apart, and the
            // description sitting under the bar instead of a band of empty canvas.
            .listSectionSpacing(MobiusSpace.l)
            .contentMargins(.top, MobiusSpace.xs, for: .scrollContent)
            .scrollContentBackground(.hidden)
            .scrollDismissesKeyboard(.interactively)
        }
        .navigationTitle(title.text)
        .toolbarTitleDisplayMode(.inline)
        .toolbar {
            if sharesHeaderBackground {
                ToolbarItem(placement: .primaryAction) { headerAccessory }
            } else {
                ToolbarItem(placement: .primaryAction) { headerAccessory }
                    .sharedBackgroundVisibility(.hidden)
            }
        }
        .background(MobiusBackdrop())
    }
}

extension PageScaffold where HeaderAccessory == EmptyView {
    init(
        title: LocalizedStringResource,
        detail: LocalizedStringResource,
        @ViewBuilder content: () -> Content
    ) {
        self.init(
            title: .localized(title),
            detail: .localized(detail),
            sharesHeaderBackground: false,
            headerAccessory: EmptyView.init,
            content: content
        )
    }

    init(
        title: MobiusText,
        detail: MobiusText,
        @ViewBuilder content: () -> Content
    ) {
        self.init(
            title: title,
            detail: detail,
            sharesHeaderBackground: false,
            headerAccessory: EmptyView.init,
            content: content
        )
    }
}

/// Secondary explanation in a form: a note under a control, an empty section, a failure.
/// The page description in `PageScaffold` reads at this step too, so a page stays one voice.
struct SettingsCaption: View {
    @Environment(\.mobiusPalette) private var palette
    let content: MobiusText

    init(_ text: LocalizedStringResource) { content = .localized(text) }

    init(_ text: MobiusText) { content = text }

    init(verbatim text: String) { content = .verbatim(text) }

    var body: some View {
        content.text
            .font(MobiusStyle.captionFont)
            .foregroundStyle(palette.muted)
            .listRowSeparator(.hidden)
    }
}

/// The two lines a settings row reads as: the name, and the muted line under it.
struct SettingsRowLabel<Mark: View>: View {
    @Environment(\.mobiusPalette) private var palette
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize
    let title: MobiusText
    var detail: MobiusText?
    @ViewBuilder let mark: Mark

    init(
        title: LocalizedStringResource,
        detail: LocalizedStringResource? = nil,
        @ViewBuilder mark: () -> Mark
    ) {
        self.init(
            title: .localized(title),
            detail: detail.map { .localized($0) },
            mark: mark
        )
    }

    init(
        title: MobiusText,
        detail: MobiusText? = nil,
        @ViewBuilder mark: () -> Mark
    ) {
        self.title = title
        self.detail = detail
        self.mark = mark()
    }

    var body: some View {
        HStack(spacing: MobiusSpace.s) {
            mark
            VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
                title.text
                    .lineLimit(1)
                    .truncationMode(.middle)
                if let detail, !detail.isEmpty {
                    detail.text
                        .font(MobiusStyle.captionFont)
                        .foregroundStyle(palette.muted)
                        // At accessibility sizes two lines cannot hold a sentence, so the
                        // row grows instead of truncating it.
                        .lineLimit(dynamicTypeSize.isAccessibilitySize ? nil : 2)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .accessibilityElement(children: .combine)
    }
}

extension SettingsRowLabel where Mark == EmptyView {
    init(
        title: LocalizedStringResource,
        detail: LocalizedStringResource? = nil
    ) {
        self.init(
            title: .localized(title),
            detail: detail.map { .localized($0) }
        ) { EmptyView() }
    }

    init(title: MobiusText, detail: MobiusText? = nil) {
        self.init(title: title, detail: detail) { EmptyView() }
    }
}

/// A section's skeleton, standing in for the rows that are about to arrive.
///
/// One row holding all of them rather than a placeholder per row: the shimmer band is
/// masked by the view it is applied to, so per-row placeholders light a single row in the
/// middle instead of sweeping the section the way the chats list does.
struct SettingsLoadingRows<Content: View>: View {
    let label: MobiusText
    @ViewBuilder let content: Content

    init(
        label: LocalizedStringResource,
        @ViewBuilder content: () -> Content
    ) {
        self.init(label: .localized(label), content: content)
    }

    init(
        label: MobiusText,
        @ViewBuilder content: () -> Content
    ) {
        self.label = label
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.m) {
            content
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .mobiusLoadingPlaceholder(label)
    }
}

/// A field whose value outgrows a trailing column: the label sits over it, so a list of
/// model ids, an endpoint, or anything else long keeps the full width of the row — while
/// reading and while being edited.
struct SettingsStackedField<Content: View>: View {
    @Environment(\.mobiusPalette) private var palette
    let title: MobiusText
    var info: MobiusText?
    @ViewBuilder let content: Content

    init(
        title: LocalizedStringResource,
        info: LocalizedStringResource? = nil,
        @ViewBuilder content: () -> Content
    ) {
        self.init(
            title: .localized(title),
            info: info.map { .localized($0) },
            content: content
        )
    }

    init(
        title: MobiusText,
        info: MobiusText? = nil,
        @ViewBuilder content: () -> Content
    ) {
        self.title = title
        self.info = info
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: MobiusSpace.xxs) {
            HStack(spacing: MobiusSpace.xs) {
                title.text
                if let info {
                    SettingsInfoButton(title: title, detail: info, compact: true)
                }
            }
            // Muted however short the value is, and always on its own line: under a label
            // rather than beside one, colour is what tells the two apart.
            content
                .font(MobiusStyle.bodyFont)
                .foregroundStyle(palette.muted)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

/// A settings row that opens a detail page: the label carries the tap, status marks sit
/// before the disclosure every one of these rows ends with.
struct SettingsNavigationRow<Marks: View, Label: View>: View {
    @Environment(\.mobiusPalette) private var palette
    let hint: MobiusText
    let open: () -> Void
    @ViewBuilder let marks: Marks
    @ViewBuilder let label: Label

    init(
        hint: LocalizedStringResource,
        open: @escaping () -> Void,
        @ViewBuilder marks: () -> Marks,
        @ViewBuilder label: () -> Label
    ) {
        self.init(
            hint: .localized(hint),
            open: open,
            marks: marks,
            label: label
        )
    }

    init(
        hint: MobiusText,
        open: @escaping () -> Void,
        @ViewBuilder marks: () -> Marks,
        @ViewBuilder label: () -> Label
    ) {
        self.hint = hint
        self.open = open
        self.marks = marks()
        self.label = label()
    }

    var body: some View {
        HStack(spacing: MobiusSpace.s) {
            Button(action: open) {
                label.contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityHint(hint.text)
            marks
            MobiusIcon(.caretRight, size: MobiusStyle.glyphMark, foreground: palette.muted)
                .accessibilityHidden(true)
        }
    }
}

extension View {
    /// A menu keeps the value on its own row without pushing a destination: the
    /// navigation-link style pushes a blank page from a split view's detail column.
    func settingsPickerStyle() -> some View {
        pickerStyle(.menu)
    }

    /// Trailing-aligned entry like Settings.app.
    func settingsField() -> some View {
        multilineTextAlignment(.trailing)
    }

    func settingsStandaloneRow() -> some View {
        Section {
            frame(maxWidth: .infinity)
                .listRowInsets(EdgeInsets(top: 6, leading: 0, bottom: 6, trailing: 0))
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
        }
    }
}

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

func cacheHit(_ usage: TokenUsage) -> String {
    guard usage.inputTokens > 0 else { return "—" }
    return (Double(usage.cachedInputTokens) / Double(usage.inputTokens))
        .formatted(.percent.precision(.fractionLength(1)))
}
