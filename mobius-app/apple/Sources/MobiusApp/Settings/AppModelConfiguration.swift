import Foundation
import Observation

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
            var provider =
                providerInstances
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

    func sameModel(_ lhs: ModelChoice, _ rhs: ModelChoice) -> Bool {
        guard let lhsInstance = modelProviders[lhs.route],
            let rhsInstance = modelProviders[rhs.route]
        else { return lhs.route == rhs.route }
        return lhsInstance == rhsInstance && lhs.model == rhs.model
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
        guard
            await authenticateForAppLock(
                reason: "Authenticate to enable app lock in möbius."
            )
        else { return }
        appLockEnabled = true
        isAppLocked = appIsInBackground
        settingsDefaults.set(true, forKey: appLockEnabledKey)
    }

    func appDidEnterBackground() {
        appIsInBackground = true
        appLockAuthenticationGeneration = UUID()
        startupTask?.cancel()
        appActivationTask?.cancel()
        appActivationTask = nil
        cloud.cancelAuthenticationRefresh()
        messageSpeaker.stop()
        Task { await dictation.cancel() }
        cancelVoiceChatIntent()
        let voiceCall = chat.realtimeVoiceCall
        chat.stopRealtimeVoice(notifyGateway: false)
        gateway.setAppInBackground(true)
        gateway.setSceneActive(false)
        chat.flushStreamDeltas()
        chat.restorePendingDrafts()
        let endVoiceRequest = voiceCall.map {
            GatewayRequest.endRealtimeVoice(
                sessionID: $0.sessionID,
                voiceID: $0.voiceID ?? $0.requestID
            )
        }
        if !gateway.hasPendingPairing {
            gateway.shutdown(
                endVoiceRequest: endVoiceRequest,
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
            if gateway.reconnectsOnActivation {
                if selectedGatewayIsMobiusCloud,
                    cloud.cloudIssue != .subscriptionExpired
                {
                    reconnect()
                }
            }
        } else {
            setSceneActive(true)
        }
        await cloud.refreshRemoteNotificationRegistration()
    }

    func beginAppActivation() {
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
