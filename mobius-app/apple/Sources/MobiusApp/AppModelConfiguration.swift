import Foundation

extension AppModel {
    var botDefaultsDraftModelRoute: String? {
        modelRoute(for: botDefaultsDraft)
    }

    func modelRoute(for draft: AgentComposition?) -> String? {
        guard let provider = draft?.provider else { return nil }
        return modelChoices.first { choice in
            choice.model == provider.model
                && choice.reasoningEffort == provider.reasoningEffort
                && modelProviders[choice.route] == provider.instance
        }?.route
    }

    func selectBotDefaultsDraftModel(_ route: String) {
        botDefaultsDraft = draft(botDefaultsDraft, selectingModelRoute: route)
    }

    var botDraftModelRoute: String? {
        modelRoute(for: botDraft)
    }

    func selectBotDraftModel(_ route: String) {
        botDraft = draft(botDraft, selectingModelRoute: route)
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
        if let voice = draft.realtimeVoice, !realtimeVoices(for: draft).contains(voice) {
            draft.realtimeVoice = nil
        }
        return draft
    }

    func realtimeVoices(for draft: AgentComposition?) -> [String] {
        guard let draft, let route = modelRoute(for: draft),
              modelChoices.first(where: { $0.route == route })?.supportsRealtimeVoice == true
        else { return [] }
        return providerStatus(forInstance: draft.provider.instance)?.realtimeVoices ?? []
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
    func providerTint(for choice: ModelChoice) -> AccentTint {
        guard let instance = modelProviders[choice.route] else { return .appDefault }
        return providerInstances.first { $0.instance == instance }?.tint ?? .appDefault
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
        if let scope = scope.gitScope { refreshGitDiff(scope) }
    }

    func saveBotDefaults() {
        guard !isApplyingConfiguration,
              let draft = botDefaultsDraft,
              let snapshot = botDefaultsSnapshot
        else { return }
        let id = requestID("configure-default")
        botDefaultsApplyState = .applying
        botDefaultsRequestID = id
        submittedBotDefaultsDraft = draft
        transmit(.configureBotDefaults(
            requestID: id,
            expectedRevision: snapshot.revision,
            config: draft
        )) { [weak self] message in
            guard self?.botDefaultsRequestID == id else { return }
            self?.botDefaultsRequestID = nil
            self?.submittedBotDefaultsDraft = nil
            self?.botDefaultsApplyState = .failed(message)
        }
    }

    func beginEditingBot(_ bot: BotRecord) {
        editingBotID = bot.id
        editingBotRevision = bot.config.revision
        botNameDraft = bot.name
        botDescriptionDraft = bot.description
        botTintDraft = bot.tint
        botDraft = bot.config.config
        botApplyState = .idle
    }

    func selectModelForSelectedBot(_ route: String) {
        guard let bot = selectedBot,
              let config = draft(bot.config.config, selectingModelRoute: route),
              config != bot.config.config
        else { return }
        saveSelectedBot(bot, config: config)
    }

    func setSelectedBotSetting(
        _ value: FrontendSettingValue?,
        middleware: String,
        setting: String
    ) {
        guard let bot = selectedBot else { return }
        var config = bot.config.config
        guard config.middleware.settings[middleware]?[setting] != value else { return }
        config.middleware.setSetting(value, middleware: middleware, setting: setting)
        saveSelectedBot(bot, config: config)
    }

    private func saveSelectedBot(_ bot: BotRecord, config: AgentComposition) {
        guard canMutateBot(bot.id) else { return }
        beginEditingBot(bot)
        botDraft = config
        saveBotDraft()
    }

    func createBot(name rawName: String, description rawDescription: String) {
        let name = rawName.trimmingCharacters(in: .whitespacesAndNewlines)
        let description = rawDescription.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canMutateBots, !name.isEmpty, !description.isEmpty else { return }
        let id = requestID("bot-create")
        botMutationRequestID = id
        botMutationSuccessMessage = localizedString("Bot created.")
        transmit(.createBot(
            requestID: id,
            name: name,
            description: description
        )) {
            [weak self] message in
            guard self?.botMutationRequestID == id else { return }
            self?.botMutationRequestID = nil
            self?.botMutationSuccessMessage = nil
            self?.botApplyState = .failed(message)
        }
    }

    func saveBotDraft() {
        let name = botNameDraft.trimmingCharacters(in: .whitespacesAndNewlines)
        let description = botDescriptionDraft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let id = editingBotID,
              canMutateBot(id),
              let expectedRevision = editingBotRevision,
              bots.contains(where: { $0.id == id }),
              let draft = botDraft,
              !name.isEmpty,
              !description.isEmpty
        else { return }
        let requestID = requestID("bot-update")
        botMutationRequestID = requestID
        botMutationSuccessMessage = localizedString("Bot saved.")
        botApplyState = .applying
        transmit(.updateBot(
            requestID: requestID,
            id: id,
            expectedRevision: expectedRevision,
            name: name,
            description: description,
            tint: botTintDraft,
            config: draft
        )) { [weak self] message in
            guard self?.botMutationRequestID == requestID else { return }
            self?.botMutationRequestID = nil
            self?.botMutationSuccessMessage = nil
            self?.botApplyState = .failed(message)
        }
    }

    func deleteBot(_ bot: BotRecord) {
        guard canMutateBots, bot.handle != "mobius" else { return }
        let id = requestID("bot-delete")
        botMutationRequestID = id
        botMutationSuccessMessage = localizedString("Bot deleted.")
        transmit(.deleteBot(
            requestID: id,
            id: bot.id,
            expectedRevision: bot.config.revision
        )) { [weak self] message in
            guard self?.botMutationRequestID == id else { return }
            self?.botMutationRequestID = nil
            self?.botMutationSuccessMessage = nil
            self?.botApplyState = .failed(message)
        }
    }

    func reloadBotDraft() {
        guard let editingBotID,
              let bot = bots.first(where: { $0.id == editingBotID })
        else { return }
        botNameDraft = bot.name
        botDescriptionDraft = bot.description
        botTintDraft = bot.tint
        editingBotRevision = bot.config.revision
        botDraft = bot.config.config
        botApplyState = .idle
        showToast("Bot draft reloaded.", tone: .info)
    }

    func refreshBots() {
        guard connectionState.isReady else { return }
        transmit(.listBots(requestID: requestID("bot-list")))
    }

    func reloadBotDefaultsDraft() {
        botDefaultsDraft = botDefaultsSnapshot?.config
        botDefaultsApplyState = .idle
        showToast("Bot defaults draft reloaded.", tone: .info)
    }

    /// Starts a new setup of `provider` with the identity used by credentials and registration.
    func addProviderInstance(_ provider: String) {
        guard let status = providerStatuses.first(where: { $0.provider == provider }),
              let search = status.webSearch.first,
              let webSearch = HostedWebSearch(rawValue: search.value)
        else { return }
        let selectedModel = status.models.first
        providerLabelDraft = status.label
        providerTintDraft = .appDefault
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
            let message = localizedString(
                "Enter an API key. It will be sent once and never read back."
            )
            providerActionState = .failed(message)
            showToast(verbatim: message, tone: .error)
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
            gitCredentialError = localizedString("Enter an HTTPS Git host or URL.")
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
            gitCredentialError = localizedString(
                "Enter the Git host, username, and access token."
            )
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

    func createRoutine(
        botID: String,
        workspace: String,
        instructions: String,
        schedule: RoutineSchedule,
        endsAt: Int64?
    ) {
        let instructions = instructions.trimmingCharacters(in: .whitespacesAndNewlines)
        guard connectionState.isReady, !botID.isEmpty, !workspace.isEmpty,
              !instructions.isEmpty
        else { return }
        let id = requestID("routine-create")
        routineRequestIDs.insert(id)
        routineError = nil
        transmit(.createRoutine(
            requestID: id,
            botID: botID,
            workspace: workspace,
            instructions: instructions,
            schedule: schedule,
            endsAt: endsAt
        )) { [weak self] message in
            self?.routineRequestIDs.remove(id)
            self?.routineError = message
        }
    }

    func updateRoutine(
        _ routine: Routine,
        botID: String,
        workspace: String,
        instructions: String,
        schedule: RoutineSchedule,
        endsAt: Int64?,
        enabled: Bool
    ) {
        let instructions = instructions.trimmingCharacters(in: .whitespacesAndNewlines)
        guard connectionState.isReady, !botID.isEmpty, !workspace.isEmpty,
              !instructions.isEmpty
        else { return }
        let id = requestID("routine-update")
        routineRequestIDs.insert(id)
        routineError = nil
        transmit(.updateRoutine(
            requestID: id,
            id: routine.id,
            botID: botID,
            workspace: workspace,
            instructions: instructions,
            schedule: schedule,
            endsAt: endsAt,
            enabled: enabled
        )) { [weak self] message in
            self?.routineRequestIDs.remove(id)
            self?.routineError = message
        }
    }

    func deleteRoutine(_ routine: Routine) {
        guard connectionState.isReady else { return }
        let id = requestID("routine-delete")
        routineRequestIDs.insert(id)
        transmit(.deleteRoutine(requestID: id, id: routine.id)) { [weak self] message in
            self?.routineRequestIDs.remove(id)
            self?.routineError = message
        }
    }

    func deleteRoutineRun(_ run: RoutineRun) {
        guard connectionState.isReady, run.status != .running else { return }
        let id = requestID("routine-run-delete")
        routineRequestIDs.insert(id)
        transmit(.deleteRoutineRun(requestID: id, id: run.id)) { [weak self] message in
            self?.routineRequestIDs.remove(id)
            self?.routineError = message
        }
    }

    func runRoutine(_ routine: Routine) {
        guard connectionState.isReady else { return }
        let id = requestID("routine-run")
        routineRequestIDs.insert(id)
        transmit(.runRoutine(requestID: id, id: routine.id)) { [weak self] message in
            self?.routineRequestIDs.remove(id)
            self?.routineError = message
        }
    }

    func refreshRoutines() {
        guard connectionState.isReady else { return }
        transmit(.listRoutines(requestID: requestID("routine-list"), botID: nil))
        transmit(.listRoutineHistory(requestID: requestID("routine-history"), id: nil))
    }

    func presentRoutineRun(_ run: RoutineRun) {
        cancelSessionFileThumbnailDownloads()
        presentedRoutineRun = run
        routineRunPreview = nil
        routineRunPreviewEntries = []
        routineRunPreviewNextBeforeSequence = nil
        routineRunPreviewError = nil
        routineRunPreviewPollingTask?.cancel()
        routineRunPreviewPollingTask = nil
        loadRoutineRunPreview(runID: run.id)
        routineRunPreviewPollingTask = Task { [weak self] in
            // ponytail: poll only while a sheet is open; add push subscriptions if live viewers scale.
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(2))
                guard !Task.isCancelled, let self,
                      self.presentedRoutineRun?.id == run.id
                else { return }
                guard (self.routineRunPreview?.run.status ?? run.status) == .running else { return }
                self.loadRoutineRunPreview(runID: run.id)
            }
        }
    }

    func closeRoutineRunPreview() {
        cancelSessionFileThumbnailDownloads()
        routineRunPreviewPollingTask?.cancel()
        routineRunPreviewPollingTask = nil
        routineRunPreviewRequestID = nil
        routineRunPreviewRequestBeforeSequence = nil
        presentedRoutineRun = nil
        routineRunPreview = nil
        routineRunPreviewEntries = []
        routineRunPreviewNextBeforeSequence = nil
        routineRunPreviewError = nil
        isLoadingRoutineRunPreview = false
    }

    func loadEarlierRoutineRunPreview() {
        guard let runID = presentedRoutineRun?.id,
              let beforeSequence = routineRunPreviewNextBeforeSequence
        else { return }
        loadRoutineRunPreview(runID: runID, beforeSequence: beforeSequence)
    }

    private func loadRoutineRunPreview(runID: String, beforeSequence: UInt64? = nil) {
        guard connectionState.isReady, routineRunPreviewRequestID == nil else { return }
        let id = requestID("routine-preview")
        routineRunPreviewRequestID = id
        routineRunPreviewRequestBeforeSequence = beforeSequence
        isLoadingRoutineRunPreview = routineRunPreview == nil || beforeSequence != nil
        transmit(.getRoutineRunPreview(
            requestID: id,
            id: runID,
            beforeSequence: beforeSequence
        )) { [weak self] message in
            guard let self, self.routineRunPreviewRequestID == id else { return }
            self.routineRunPreviewRequestID = nil
            self.routineRunPreviewRequestBeforeSequence = nil
            self.isLoadingRoutineRunPreview = false
            self.routineRunPreviewError = message
        }
    }

    func setTheme(_ theme: ThemePreference) {
        self.theme = theme
        settingsDefaults.set(theme.rawValue, forKey: "theme")
    }

    func setLanguage(_ language: AppLanguage) {
        self.language = language
        settingsDefaults.set(language.rawValue, forKey: "language")
    }

    func setAccentTint(_ accentTint: AccentTint) {
        self.accentTint = accentTint
        settingsDefaults.set(accentTint.rawValue, forKey: "accent-tint")
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
        cancelVoiceChatIntent()
        let voiceCall = realtimeVoiceCall
        stopRealtimeVoice(notifyGateway: false)
        cancelReconnect()
        reconnectsOnActivation = true
        // Retire this socket before suspension. Cloud resume checks subscription state
        // asynchronously, so its old receive callback must already be out of generation.
        if pendingPairingAccount == nil, !automaticReconnectBlocked {
            connectionGeneration = UUID()
            let generation = connectionGeneration
            eventTask?.cancel()
            eventTask = nil
            flushStreamDeltas()
            restorePendingDrafts()
            if connectionState.isReady || connectionState.isLoading { connectionState = .disconnected }
            Task { [weak self] in
                guard let self, self.connectionGeneration == generation else { return }
                if let voiceCall {
                    try? await self.requestSender(.endRealtimeVoice(
                        sessionID: voiceCall.sessionID,
                        voiceID: voiceCall.voiceID ?? voiceCall.requestID
                    ))
                    guard self.connectionGeneration == generation else { return }
                }
                await self.client.disconnect()
            }
        }
        flushComposerDraft()
        guard appLockEnabled else { return }
        discardFilePresentation(preservingWorkspaceTextDraft: true)
        isAppLocked = true
        appLockError = nil
    }

    func appDidBecomeActive() async {
        appIsInBackground = false
        await unlockApp()
        if selectedGatewayIsMobiusCloud {
            await refreshCloudAccount()
            if reconnectsOnActivation {
                reconnectsOnActivation = false
                if selectedGatewayIsMobiusCloud,
                   cloudIssue != .subscriptionExpired {
                    reconnect()
                }
            }
        }
        await refreshRemoteNotificationRegistration()
    }

    func unlockApp() async {
        guard appLockEnabled, isAppLocked, !isAppLockAuthenticating else { return }
        guard await authenticateForAppLock(
            reason: localizedString("Authenticate to unlock möbius.")
        ) else {
            return
        }
        isAppLocked = appIsInBackground
    }

    private func authenticateForAppLock(reason: String) async -> Bool {
        refreshAppLockAuthenticationMethod()
        guard appLockAuthenticationMethod.isAvailable else {
            appLockError = localizedString(
                "Biometric authentication is unavailable. Update Face ID or Touch ID, then try again."
            )
            return false
        }
        isAppLockAuthenticating = true
        appLockError = nil
        let succeeded = await appLockAuthenticator.authenticate(
            reason: reason,
            cancelTitle: localizedString("Cancel")
        )
        isAppLockAuthenticating = false
        guard succeeded else {
            appLockError = localizedString("Authentication wasn’t completed. Try again.")
            return false
        }
        return true
    }

}
