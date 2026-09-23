import Foundation
import Observation

extension AppModel {
    var selectedBotModelRoute: String? {
        guard let bot = selectedBot else { return nil }
        let config =
            botMutationRequestID != nil && editingBotID == bot.id
            ? botDraft : bot.config.config
        return modelRoute(for: config)
    }

    var botDefaultsDraftModelRoute: String? {
        modelRoute(for: botDefaultsDraft)
    }

    func modelRoute(for draft: AgentComposition?) -> String? {
        modelChoice(for: draft)?.route
    }

    func modelChoice(for draft: AgentComposition?) -> ModelChoice? {
        guard let provider = draft?.provider else { return nil }
        let choices = modelChoices.filter { choice in
            choice.model == provider.model
                && modelProviders[choice.route] == provider.instance
        }
        guard let first = choices.first else { return nil }
        if let effort = provider.reasoningEffort {
            return choices.first { $0.reasoningEffort == effort }
        }
        let route = modelRoute(selecting: first, preserving: nil, in: choices)
        return choices.first { $0.route == route }
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
            var provider =
                providerInstances
                .first(where: { $0.instance == instance })?
                .selection,
            var draft = currentDraft
        else { return currentDraft }
        provider.model = choice.model
        provider.reasoningEffort = choice.reasoningEffort
        draft.provider = provider
        draft.middleware.reconcile(
            features: middlewareFeatures,
            model: choice
        )
        if let voice = draft.realtimeVoice, !realtimeVoices(for: draft).contains(voice) {
            draft.realtimeVoice = nil
        }
        return draft
    }

    func realtimeVoices(for draft: AgentComposition?) -> [String] {
        guard let draft, modelChoice(for: draft)?.supports(.realtimeVoice) == true
        else { return [] }
        return providerStatus(forInstance: draft.provider.instance)?.realtimeVoices ?? []
    }

    func modelLabel(for choice: ModelChoice) -> String {
        modelLabel(provider: modelProviders[choice.route], modelID: choice.model)
    }

    func modelLabel(provider: String?, modelID: String) -> String {
        if let instance = provider,
            let model = providerStatus(forInstance: instance)?
                .models.first(where: { $0.id == modelID })
        {
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

    func reasoningLabel(for choice: ModelChoice) -> String {
        choice.reasoningEffort ?? localizedString("Default")
    }

    func reasoningFraction(for choice: ModelChoice) -> Double {
        guard choice.reasoningEffort != nil else { return 0 }
        let levels = modelChoices(matching: choice, in: modelChoices)
        guard let index = levels.firstIndex(where: { $0.route == choice.route }) else { return 0 }
        return levels.count == 1 ? 1 : Double(index) / Double(levels.count - 1)
    }

    func sameModel(_ lhs: ModelChoice, _ rhs: ModelChoice) -> Bool {
        guard let lhsInstance = modelProviders[lhs.route],
            let rhsInstance = modelProviders[rhs.route]
        else { return lhs.route == rhs.route }
        return lhsInstance == rhsInstance && lhs.model == rhs.model
    }

    func modelRoute(
        selecting choice: ModelChoice, preserving current: ModelChoice?, in choices: [ModelChoice]
    ) -> String {
        let levels = modelChoices(matching: choice, in: choices)
        let defaultEffort = providerStatus(for: choice)?.models
            .first { $0.id == choice.model }?.defaultReasoning
        return levels.first { $0.reasoningEffort == current?.reasoningEffort }?.route
            ?? levels.first { $0.reasoningEffort == defaultEffort }?.route ?? choice.route
    }

    var chatModelLabel: String? {
        modelChoices.first { $0.route == chat.selectedModelRoute }.map { modelLabel(for: $0) }
    }

    func setTheme(_ theme: ThemePreference) {
        self.theme = theme
        settingsDefaults.set(theme.rawValue, forKey: "theme")
    }

    func setSimplifiedChatUI(_ enabled: Bool) {
        simplifiedChatUI = enabled
        settingsDefaults.set(enabled, forKey: "simplified-chat-ui")
    }

    func composerWidgets(in slot: FrontendSlot) -> [MountedWidget] {
        chat.widgets(in: slot).filter {
            chat.selectedSessionID == nil || !simplifiedChatUI || $0.widget.symbol == "voice"
        }
    }

    func setLanguage(_ language: AppLanguage) {
        self.language = language
        settingsDefaults.set(language.rawValue, forKey: "language")
    }

    func setAccentTint(_ accentTint: AccentTint) async {
        guard !isChangingAppIcon else { return }
        self.accentTint = accentTint
        settingsDefaults.set(accentTint.rawValue, forKey: "accent-tint")
        appIconError = nil
        let iconName = accentTint == .appDefault ? nil : "AppIcon-\(accentTint.rawValue)"
        guard appIconSystem.alternateIconName() != iconName else { return }
        guard appIconSystem.supportsAlternateIcons() else {
            appIconError = localizedString(
                "The app color changed, but changing the Home Screen icon is unavailable."
            )
            return
        }
        isChangingAppIcon = true
        defer { isChangingAppIcon = false }
        do {
            try await appIconSystem.setAlternateIconName(iconName)
        } catch {
            appIconError = localizedString(
                "The app color changed, but the Home Screen icon could not be changed: \(localizedErrorDescription(error))"
            )
        }
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
        guard
            await authenticateForAppLock(
                reason: "Authenticate to enable app lock in möbius."
            )
        else { return }
        appLockEnabled = true
        isAppLocked = appIsInBackground
        settingsDefaults.set(true, forKey: appLockEnabledKey)
    }

    func appDidEnterBackground(preservingVoiceCall: Bool = true) {
        appIsInBackground = true
        refreshAppLockPreference()
        appLockAuthenticationGeneration = UUID()
        startupTask?.cancel()
        appActivationTask?.cancel()
        appActivationTask = nil
        eventCentreRefreshTask?.cancel()
        eventCentreRefreshTask = nil
        cloud.cancelAuthenticationRefresh()
        messageSpeaker.stop()
        cancelVoiceChatIntent()
        let voiceCall = chat.realtimeVoiceCall
        if !preservingVoiceCall { chat.stopRealtimeVoice(notifyGateway: false) }
        let keepsVoiceCall = preservingVoiceCall && voiceCall != nil
        gateway.setAppInBackground(true)
        gateway.setSceneActive(false)
        chat.flushStreamDeltas()
        chat.restorePendingDrafts()
        if !gateway.hasPendingPairing, !keepsVoiceCall {
            gateway.shutdown(
                endVoiceRequest: voiceCall.map {
                    .endRealtimeVoice(
                        sessionID: $0.sessionID, voiceID: $0.voiceID ?? $0.requestID
                    )
                },
                state: gateway.connectionState.isReady || gateway.connectionState.isLoading
                    ? .disconnected : nil
            )
        }
        chat.flushComposerDraft()
        guard appLockEnabled else { return }
        discardFilePresentation(preservingWorkspaceTextDraft: true)
        isAppLocked = true
        appLockError = nil
    }

    func appDidBecomeActive() async {
        guard !Task.isCancelled else { return }
        let wasStarted = startedAccountID != nil
        refreshAppLockPreference()
        appIsInBackground = false
        gateway.setAppInBackground(false)
        if startedAccountID == nil, gateway.connectionState == .disconnected {
            await start()
        }
        guard !Task.isCancelled, !appIsInBackground else { return }
        await unlockApp()
        guard !Task.isCancelled, !appIsInBackground else { return }
        if selectedGatewayIsMobiusCloud {
            await cloud.refreshCloudAccount()
            guard !Task.isCancelled, !appIsInBackground else { return }
        }
        if chat.realtimeVoiceCall != nil {
            gateway.setSceneActive(true, reconnectWhenActive: false)
        } else if !gateway.automaticReconnectBlocked,
            (!selectedGatewayIsMobiusCloud || cloud.cloudIssue != .subscriptionExpired),
            wasStarted || gateway.reconnectsOnActivation
        {
            reconnect()
        } else {
            setSceneActive(true)
        }
        await cloud.refreshRemoteNotificationRegistration()
    }

    private func refreshAppLockPreference() {
        appLockEnabled = settingsDefaults.bool(forKey: appLockEnabledKey)
        if appLockEnabled {
            if appIsInBackground { isAppLocked = true }
        } else {
            isAppLocked = false
            appLockError = nil
        }
    }

    func beginAppActivation() {
        startEventCentreRefresh()
        guard appActivationTask == nil else { return }
        appActivationTask = Task<Void, Never> { [weak self] in
            guard let self else { return }
            await self.appDidBecomeActive()
            if !Task.isCancelled { self.appActivationTask = nil }
        }
    }

    func unlockApp() async {
        guard appLockEnabled, isAppLocked else { return }
        while isAppLockAuthenticating {
            for await authenticating in Observations({ self.isAppLockAuthenticating }) {
                if !authenticating { break }
            }
            guard !Task.isCancelled else { return }
        }
        guard !Task.isCancelled, appLockEnabled, isAppLocked else { return }
        guard
            await authenticateForAppLock(
                reason: localizedString("Authenticate to unlock möbius.")
            )
        else {
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
        let generation = appLockAuthenticationGeneration
        let succeeded = await appLockAuthenticator.authenticate(
            reason: reason,
            cancelTitle: localizedString("Cancel")
        )
        isAppLockAuthenticating = false
        guard !Task.isCancelled, appLockAuthenticationGeneration == generation else { return false }
        guard succeeded else {
            appLockError = localizedString("Authentication wasn’t completed. Try again.")
            return false
        }
        return true
    }
}
