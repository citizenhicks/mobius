import Foundation

extension AppModel {
    func selectModel(_ route: String) {
        guard let sessionID = selectedSessionID, route != selectedModelRoute else { return }
        transmit(.submit(
            sessionID: sessionID,
            submission: Submission(id: requestID("model"), op: .setModel(route: route))
        ))
    }

    var agentDraftModelRoute: String? {
        modelRoute(for: agentDraft)
    }

    var defaultAgentDraftModelRoute: String? {
        modelRoute(for: defaultAgentDraft)
    }

    private func modelRoute(for draft: AgentComposition?) -> String? {
        guard let provider = draft?.provider else { return nil }
        return modelChoices.first { choice in
            choice.model == provider.model
                && choice.reasoningEffort == provider.reasoningEffort
                && modelProviders[choice.route] == provider.instance
        }?.route
    }

    func selectAgentDraftModel(_ route: String) {
        agentDraft = draft(agentDraft, selectingModelRoute: route)
    }

    func selectDefaultAgentDraftModel(_ route: String) {
        defaultAgentDraft = draft(defaultAgentDraft, selectingModelRoute: route)
    }

    func draft(
        _ currentDraft: AgentComposition?,
        selectingModelRoute route: String
    ) -> AgentComposition? {
        guard let choice = modelChoices.first(where: { $0.route == route }),
              let instance = modelProviders[choice.route],
              var provider = providerInstances
                  .first(where: { $0.instance == instance })?
                  .selection,
              var draft = currentDraft
        else { return currentDraft }
        provider.model = choice.model
        provider.reasoningEffort = choice.reasoningEffort
        draft.provider = provider
        return draft
    }

    func modelLabel(for choice: ModelChoice) -> String {
        modelLabel(provider: modelProviders[choice.route], modelID: choice.model)
    }

    func modelLabel(provider: String?, modelID: String) -> String {
        if let instance = provider,
           let model = providerStatus(forInstance: instance)?
            .models.first(where: { $0.id == modelID }) {
            return model.label
        }
        guard let separator = modelID.lastIndex(of: "/") else { return modelID }
        let shortID = String(modelID[modelID.index(after: separator)...])
        guard !shortID.isEmpty else { return modelID }
        return providerStatuses.lazy
            .compactMap { status in status.models.first { $0.id == shortID }?.label }
            .first ?? modelID
    }

    func modelGroupLabel(for choice: ModelChoice) -> String {
        let label = modelLabel(for: choice)
        guard label != choice.model, choice.group.hasSuffix(choice.model) else {
            return choice.group
        }
        return "\(choice.group.dropLast(choice.model.count))\(label)"
    }

    /// The user-facing name of one setup, so two setups of a provider stay distinguishable.
    func providerLabel(for instance: String) -> String {
        if providerDraft?.instance == instance {
            let label = providerLabelDraft.trimmingCharacters(in: .whitespacesAndNewlines)
            if !label.isEmpty { return label }
        }
        return providerInstances.first { $0.instance == instance }?.label
            ?? providerStatuses.first { $0.provider == instance }?.label
            ?? instance
    }

    func providerLabel(for choice: ModelChoice) -> String {
        guard let instance = modelProviders[choice.route] else { return choice.group }
        return providerLabel(for: instance)
    }

    func providerSymbol(for choice: ModelChoice) -> String? {
        providerStatus(for: choice)?.symbol
    }

    /// The accent of the setup behind one route, so two setups of a provider differ.
    func providerTint(for choice: ModelChoice) -> ProviderTint {
        guard let instance = modelProviders[choice.route] else { return .blue }
        return providerInstances.first { $0.instance == instance }?.tint ?? .blue
    }

    private func providerStatus(for choice: ModelChoice) -> ProviderStatus? {
        guard let instance = modelProviders[choice.route] else { return nil }
        return providerStatus(forInstance: instance)
    }

    /// The definition backing one configured setup.
    func providerStatus(forInstance instance: String) -> ProviderStatus? {
        guard let entry = providerInstances.first(where: { $0.instance == instance }) else {
            return nil
        }
        return providerStatuses.first { $0.provider == entry.provider }
    }

    func distinctModels(in choices: [ModelChoice]) -> [ModelChoice] {
        var seen = Set<String>()
        return choices.filter { choice in
            let instance = modelProviders[choice.route] ?? choice.route
            return seen.insert("\(instance)\u{0}\(choice.model)").inserted
        }
    }

    func modelChoices(
        matching selected: ModelChoice,
        in choices: [ModelChoice]
    ) -> [ModelChoice] {
        choices.filter { sameModel($0, selected) }
    }

    func sameModel(_ lhs: ModelChoice, _ rhs: ModelChoice) -> Bool {
        guard let lhsInstance = modelProviders[lhs.route],
              let rhsInstance = modelProviders[rhs.route]
        else { return lhs.route == rhs.route }
        return lhsInstance == rhsInstance && lhs.model == rhs.model
    }

    func interrupt() {
        guard let sessionID = selectedSessionID, let activeTurnID else { return }
        transmit(.submit(
            sessionID: sessionID,
            submission: Submission(
                id: requestID("interrupt"),
                op: .interrupt(turnID: activeTurnID)
            )
        ))
    }

    func resolveApproval(_ decision: ReviewDecision) {
        guard let sessionID = selectedSessionID,
              let approval = pendingApproval,
              approvalRequestID == nil
        else { return }
        let id = requestID("approval")
        approvalRequestID = id
        transmit(.submit(
            sessionID: sessionID,
            submission: Submission(
                id: id,
                op: .execApproval(id: approval.id, decision: decision)
            )
        )) { [weak self] _ in
            guard self?.approvalRequestID == id else { return }
            self?.approvalRequestID = nil
        }
    }

    func showFiles(_ tab: FilesInspectorTab? = nil) {
        if let tab { filesInspectorTab = tab }
        showsInspector = true
        refreshFiles(for: filesInspectorTab)
    }

    func showFiles(_ scope: ModifiedFilesScope) {
        filesInspectorTab = .modified
        modifiedFilesScope = scope
        showsInspector = true
        refreshModifiedFiles(scope)
    }

    func refreshFiles(for tab: FilesInspectorTab) {
        switch tab {
        case .modified: refreshModifiedFiles(modifiedFilesScope)
        case .allFiles: refreshWorkspaceFiles()
        case .chatFiles: refreshSessionFiles()
        }
    }

    func refreshModifiedFiles(_ scope: ModifiedFilesScope) {
        switch scope {
        case .lastTurn: break
        case .unstaged: refreshGitDiff()
        case .staged: refreshStagedGitDiff()
        case .committed: refreshCommittedGitDiff()
        }
    }

    func changeAgentForCurrentChat() {
        applyAgentConfiguration(agentDraft, to: .session)
    }

    func saveAgentAsDefault() {
        applyAgentConfiguration(defaultAgentDraft, to: .defaultAgent)
    }

    func setAgentSettingForCurrentChat(
        _ value: FrontendSettingValue?,
        middleware: String,
        setting: String
    ) {
        guard !isApplyingConfiguration,
              let snapshot = agentSnapshot,
              var draft = agentDraft
        else { return }
        guard draft == snapshot.config else {
            showToast(
                "Apply or reload pending agent edits before changing this setting.",
                tone: .warning
            )
            return
        }
        guard draft.middleware.settings[middleware]?[setting] != value else { return }
        draft.middleware.setSetting(value, middleware: middleware, setting: setting)
        agentDraft = draft
        applyAgentConfiguration(draft, to: .session)
    }

    func applyAgentConfiguration(
        _ draft: AgentComposition?,
        to target: ConfigurationTarget
    ) {
        guard !isApplyingConfiguration, let draft else { return }
        let id = requestID("configure")
        switch target {
        case .session:
            guard let sessionID = selectedSessionID, let snapshot = agentSnapshot else {
                chatAgentApplyState = .idle
                return
            }
            chatAgentApplyState = .applying
            configRequestID = id
            transmit(.configureSession(
                requestID: id,
                sessionID: sessionID,
                expectedRevision: snapshot.revision,
                config: draft
            )) { [weak self] message in
                guard self?.configRequestID == id else { return }
                self?.configRequestID = nil
                self?.chatAgentApplyState = .failed(message)
            }
        case .defaultAgent:
            guard let snapshot = defaultAgentSnapshot else {
                defaultAgentApplyState = .failed(
                    "The gateway has no default agent configuration."
                )
                return
            }
            defaultAgentApplyState = .applying
            defaultConfigRequestID = id
            submittedDefaultAgentDraft = draft
            transmit(.configureDefaultAgent(
                requestID: id,
                expectedRevision: snapshot.revision,
                config: draft
            )) { [weak self] message in
                guard self?.defaultConfigRequestID == id else { return }
                self?.defaultConfigRequestID = nil
                self?.submittedDefaultAgentDraft = nil
                self?.defaultAgentApplyState = .failed(message)
            }
        }
    }

    func reloadAgentDraft() {
        agentDraft = agentSnapshot?.config
        chatAgentApplyState = .idle
        showToast("Agent draft reloaded.", tone: .info)
    }

    func reloadDefaultAgentDraft() {
        defaultAgentDraft = defaultAgentSnapshot?.config
        defaultAgentApplyState = .idle
        showToast("Default agent draft reloaded.", tone: .info)
    }

    /// Starts a new setup of `provider` with the identity used by credentials and registration.
    func addProviderInstance(_ provider: String) {
        guard let status = providerStatuses.first(where: { $0.provider == provider }),
              let search = status.webSearch.first,
              let webSearch = HostedWebSearch(rawValue: search.value)
        else { return }
        let selectedModel = status.models.first
        providerLabelDraft = status.label
        providerTintDraft = .blue
        providerDraft = ProviderConfig(
            instance: UUID().uuidString.lowercased(),
            provider: status.provider,
            model: selectedModel?.id ?? "",
            baseUrl: status.defaultBaseUrl,
            reasoningEffort: selectedModel?.defaultReasoning,
            webSearch: webSearch
        )
        providerModelIDsText = ""
        providerReasoningEffortsText = ""
        providerAPIKey = ""
        providerActionState = .idle
    }

    /// Loads an existing setup for editing. Every field, including the key, is replaceable.
    func editProviderInstance(_ instance: ProviderInstance) {
        providerLabelDraft = instance.label
        providerTintDraft = instance.tint
        providerDraft = instance.selection
        providerModelIDsText = instance.modelIds.joined(separator: ", ")
        providerReasoningEffortsText = instance.reasoningEfforts.joined(separator: ", ")
        providerAPIKey = ""
        providerActionState = .idle
    }

    var providerModelIDs: [String] {
        commaSeparatedValues(providerModelIDsText)
    }

    var providerReasoningEfforts: [String] {
        commaSeparatedValues(providerReasoningEffortsText)
    }

    private func commaSeparatedValues(_ text: String) -> [String] {
        text
            .split(separator: ",")
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
            .reduce(into: []) { values, value in
                if !values.contains(value) { values.append(value) }
            }
    }

    func updateProviderModelIDs(_ value: String) {
        providerModelIDsText = value
        guard let first = providerModelIDs.first else { return }
        providerDraft?.model = first
        providerDraft?.reasoningEffort = providerReasoningEfforts.first
    }

    func updateProviderReasoningEfforts(_ value: String) {
        providerReasoningEffortsText = value
        providerDraft?.reasoningEffort = providerReasoningEfforts.first
    }

    func saveProviderCredential() {
        let key = providerAPIKey
        guard var config = providerDraft, !key.isEmpty else {
            let message = "Enter an API key. It will be sent once and never read back."
            providerActionState = .failed(message)
            showToast(message, tone: .error)
            return
        }
        config.endpointAuth = .providerDefault
        providerDraft = config
        let id = requestID("credential")
        let normalizedKey = key.trimmingCharacters(in: .whitespacesAndNewlines)
        pendingProviderCredential = (
            requestID: id,
            instance: config.instance,
            provider: config.provider,
            credentialHint: normalizedKey.count >= 4 ? String(normalizedKey.suffix(4)) : nil
        )
        providerActionState = .savingCredential(config.instance)
        let request: GatewayRequest
        if let baseURL = config.baseUrl {
            request = .setProviderEndpointCredential(
                requestID: id,
                instance: config.instance,
                provider: config.provider,
                baseURL: baseURL,
                apiKey: key
            )
        } else {
            request = .setProviderCredential(
                requestID: id,
                instance: config.instance,
                provider: config.provider,
                apiKey: key
            )
        }
        transmit(request) { [weak self] message in
            guard let self, self.pendingProviderCredential?.requestID == id else { return }
            self.pendingProviderCredential = nil
            self.providerActionState = .failed(message)
        }
    }

    func registerProvider() {
        guard var config = providerDraft,
              let status = providerStatuses.first(where: { $0.provider == config.provider })
        else { return }
        let modelIDs = status.modelIdsConfigurable ? providerModelIDs : []
        let reasoningEfforts = status.modelIdsConfigurable ? providerReasoningEfforts : []
        if status.modelIdsConfigurable {
            guard let first = modelIDs.first else { return }
            config.model = first
            config.reasoningEffort = reasoningEfforts.first
        }
        let id = requestID("provider")
        providerRegistrationRequestID = id
        transmit(.registerProvider(
            requestID: id,
            config: config,
            label: providerLabelDraft.trimmingCharacters(in: .whitespacesAndNewlines),
            tint: providerTintDraft,
            modelIds: modelIDs,
            reasoningEfforts: reasoningEfforts
        )) { [weak self] message in
            guard self?.providerRegistrationRequestID == id else { return }
            self?.providerRegistrationRequestID = nil
            self?.providerActionState = .failed(message)
        }
    }

    func removeProvider(_ instance: String) {
        guard !isApplyingConfiguration,
              connectionState.isReady,
              providerInstances.contains(where: { $0.instance == instance })
        else { return }
        let id = requestID("provider-remove")
        pendingProviderRemoval = (requestID: id, instance: instance)
        providerActionState = .idle
        transmit(.removeProvider(requestID: id, instance: instance)) { [weak self] message in
            guard self?.pendingProviderRemoval?.requestID == id else { return }
            self?.pendingProviderRemoval = nil
            self?.providerActionState = .failed(message)
        }
    }

    func startProviderLogin() {
        guard let provider = providerDraft?.provider else { return }
        let id = requestID("login")
        providerLoginRequestID = id
        providerActionState = .startingLogin(provider)
        transmit(.startProviderLogin(requestID: id, provider: provider)) { [weak self] message in
            self?.providerActionState = .failed(message)
        }
    }

    func createPairingCode() {
        let id = requestID("pairing-code")
        pairingCodeRequestID = id
        pairingCodeExpiryTask?.cancel()
        pairingCodeExpiryTask = nil
        pairingCodeInfo = nil
        transmit(.createPairingCode(requestID: id)) { [weak self] _ in
            self?.pairingCodeRequestID = nil
        }
    }

    func probeGitCredential(_ target: String) {
        guard connectionState.isReady, gitCredentialRequestID == nil else { return }
        let target = target.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !target.isEmpty else {
            gitCredentialError = "Enter an HTTPS Git host or URL."
            return
        }
        let id = requestID("git-credential")
        gitCredentialAvailable = nil
        gitCredentialUsername = nil
        gitCredentialError = nil
        gitCredentialRequestID = id
        isApprovingGitCredential = false
        isCheckingGitCredential = true
        transmit(.probeGitCredential(requestID: id, target: target)) { [weak self] message in
            guard self?.gitCredentialRequestID == id else { return }
            self?.gitCredentialRequestID = nil
            self?.isCheckingGitCredential = false
            self?.gitCredentialError = message
        }
    }

    func approveGitCredential(target: String, username: String, token: String) {
        guard connectionState.isReady, gitCredentialRequestID == nil else { return }
        let target = target.trimmingCharacters(in: .whitespacesAndNewlines)
        let username = username.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !target.isEmpty, !username.isEmpty, !token.isEmpty else {
            gitCredentialError = "Enter the Git host, username, and access token."
            return
        }
        let id = requestID("git-credential")
        gitCredentialError = nil
        gitCredentialRequestID = id
        isApprovingGitCredential = true
        isCheckingGitCredential = true
        transmit(.approveGitCredential(
            requestID: id,
            target: target,
            username: username,
            token: token
        )) { [weak self] message in
            guard self?.gitCredentialRequestID == id else { return }
            self?.gitCredentialRequestID = nil
            self?.isApprovingGitCredential = false
            self?.isCheckingGitCredential = false
            self?.gitCredentialError = message
        }
    }

    func listSshIdentities() {
        guard connectionState.isReady, sshIdentityRequestID == nil else { return }
        let id = requestID("ssh-list")
        sshIdentityRequestID = id
        sshIdentityError = nil
        isLoadingSshIdentities = true
        transmit(.listSshIdentities(requestID: id)) { [weak self] message in
            guard self?.sshIdentityRequestID == id else { return }
            self?.sshIdentityRequestID = nil
            self?.isLoadingSshIdentities = false
            self?.sshIdentityError = message
        }
    }

    func generateSshIdentity() {
        guard connectionState.isReady,
              sshIdentityRequestID == nil,
              sshIdentities?.isEmpty == true
        else { return }
        let id = requestID("ssh-generate")
        sshIdentityRequestID = id
        sshIdentityError = nil
        isGeneratingSshIdentity = true
        transmit(.generateSshIdentity(requestID: id)) { [weak self] message in
            guard self?.sshIdentityRequestID == id else { return }
            self?.sshIdentityRequestID = nil
            self?.isGeneratingSshIdentity = false
            self?.sshIdentityError = message
        }
    }

    func startCronSetup() {
        guard canStartCronSetup, let sessionID = selectedSessionID else { return }
        let task = cronTaskDraft.trimmingCharacters(in: .whitespacesAndNewlines)
        let id = requestID("cron-setup")
        cronRequestIDs.insert(id)
        cronError = nil
        openChat(sessionID)
        transmit(.startCronSetup(
            requestID: id,
            sessionID: sessionID,
            task: task.isEmpty ? nil : task
        )) { [weak self] message in
            self?.cronRequestIDs.remove(id)
            self?.cronError = message
        }
    }

    func rescheduleCron(_ task: CronTask, schedule: String) {
        guard isSchedulingEnabled, let sessionID = selectedSessionID else { return }
        let value = schedule.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !value.isEmpty else { return }
        let request = requestID("cron-reschedule")
        cronRequestIDs.insert(request)
        transmit(.rescheduleCron(
            requestID: request,
            sessionID: sessionID,
            id: task.id,
            schedule: value
        )) { [weak self] message in
            self?.cronRequestIDs.remove(request)
            self?.cronError = message
        }
    }

    func deleteCron(_ task: CronTask) {
        guard isSchedulingEnabled, let sessionID = selectedSessionID else { return }
        let request = requestID("cron-delete")
        cronRequestIDs.insert(request)
        transmit(.deleteCron(requestID: request, sessionID: sessionID, id: task.id)) { [weak self] message in
            self?.cronRequestIDs.remove(request)
            self?.cronError = message
        }
    }

    func runCron(_ task: CronTask) {
        guard isSchedulingEnabled, let sessionID = selectedSessionID else { return }
        let request = requestID("cron-run")
        cronRequestIDs.insert(request)
        transmit(.runCron(requestID: request, sessionID: sessionID, id: task.id)) { [weak self] message in
            self?.cronRequestIDs.remove(request)
            self?.cronError = message
        }
    }

    func refreshCron() {
        guard let sessionID = selectedSessionID else { return }
        transmit(.listCron(requestID: requestID("cron-list"), sessionID: sessionID))
        transmit(.listCronHistory(
            requestID: requestID("cron-history"),
            sessionID: sessionID,
            id: nil
        ))
    }

    func setTheme(_ theme: ThemePreference) {
        self.theme = theme
        settingsDefaults.set(theme.rawValue, forKey: "theme")
    }

    func refreshAppLockAuthenticationMethod() {
        appLockAuthenticationMethod = appLockAuthenticator.method
    }

    func setAppLockEnabled(_ enabled: Bool) async {
        guard enabled != appLockEnabled, !isAppLockAuthenticating else { return }
        guard enabled else {
            appLockEnabled = false
            isAppLocked = false
            appLockError = nil
            settingsDefaults.set(false, forKey: appLockEnabledKey)
            return
        }
        guard await authenticateForAppLock(
            reason: "Authenticate to enable app lock in möbius."
        ) else { return }
        appLockEnabled = true
        isAppLocked = appIsInBackground
        settingsDefaults.set(true, forKey: appLockEnabledKey)
    }

    func appDidEnterBackground() {
        appIsInBackground = true
        cancelReconnect()
        reconnectsOnActivation = true
        flushComposerDraft()
        guard appLockEnabled else { return }
        discardFilePresentation(preservingWorkspaceTextDraft: true)
        isAppLocked = true
        appLockError = nil
    }

    func appDidBecomeActive() async {
        appIsInBackground = false
        await unlockApp()
    }

    func unlockApp() async {
        guard appLockEnabled, isAppLocked, !isAppLockAuthenticating else { return }
        guard await authenticateForAppLock(reason: "Authenticate to unlock möbius.") else {
            return
        }
        isAppLocked = appIsInBackground
    }

    private func authenticateForAppLock(reason: String) async -> Bool {
        refreshAppLockAuthenticationMethod()
        guard appLockAuthenticationMethod.isAvailable else {
            appLockError = "Biometric authentication is unavailable. Update Face ID or Touch ID, then try again."
            return false
        }
        isAppLockAuthenticating = true
        appLockError = nil
        let succeeded = await appLockAuthenticator.authenticate(reason: reason)
        isAppLockAuthenticating = false
        guard succeeded else {
            appLockError = "Authentication wasn’t completed. Try again."
            return false
        }
        return true
    }

}
