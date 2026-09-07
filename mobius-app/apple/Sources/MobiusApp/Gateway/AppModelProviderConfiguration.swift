import Foundation
import Observation

extension AppModel {
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
        if pendingProviderLogin == nil { providerActionState = .idle }
    }

    /// Loads an existing setup for editing. Every field, including the key, is replaceable.
    func editProviderInstance(_ instance: ProviderInstance) {
        providerLabelDraft = instance.label
        providerTintDraft = instance.tint
        providerDraft = instance.selection
        providerModelIDsText = instance.modelIds.joined(separator: ", ")
        providerReasoningEffortsText = instance.reasoningEfforts.joined(separator: ", ")
        providerAPIKey = ""
        if pendingProviderLogin == nil { providerActionState = .idle }
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
        gateway.transmit(request) { [weak self] message in
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
        gateway.transmit(.registerProvider(
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
              gateway.connectionState.isReady,
              providerInstances.contains(where: { $0.instance == instance })
        else { return }
        let id = requestID("provider-remove")
        pendingProviderRemoval = (requestID: id, instance: instance)
        providerActionState = .idle
        gateway.transmit(.removeProvider(requestID: id, instance: instance)) { [weak self] message in
            guard self?.pendingProviderRemoval?.requestID == id else { return }
            self?.pendingProviderRemoval = nil
            self?.providerActionState = .failed(message)
        }
    }

    func startProviderLogin() {
        guard gateway.connectionState.isReady,
              pendingProviderLogin == nil,
              let provider = providerDraft?.provider
        else { return }
        pendingProviderLogin = (requestID("login"), provider)
        providerActionState = .startingLogin(provider)
        resumeProviderLogin()
    }

    func resumeProviderLogin() {
        guard gateway.connectionState.isReady, let login = pendingProviderLogin else { return }
        // The same request resumes its gateway-owned attempt, including a missed result.
        // Keep the identity if sending fails: the gateway may already have received it.
        gateway.transmit(.startProviderLogin(requestID: login.requestID, provider: login.provider))
    }

    func createPairingCode() {
        let id = requestID("pairing-code")
        pairingCodeRequestID = id
        pairingCodeExpiryTask?.cancel()
        pairingCodeExpiryTask = nil
        pairingCodeInfo = nil
        gateway.transmit(.createPairingCode(requestID: id)) { [weak self] _ in
            self?.pairingCodeRequestID = nil
        }
    }

    func probeGitCredential(_ target: String) {
        guard gateway.connectionState.isReady, gitCredentialRequestID == nil else { return }
        let target = target.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !target.isEmpty else {
            gitCredentialError = localizedString("Enter an HTTPS Git host or URL.")
            return
        }
        let id = requestID("git-credential")
        gitCredentialError = nil
        gitCredentialRequestID = id
        isApprovingGitCredential = false
        isCheckingGitCredential = true
        gateway.transmit(.probeGitCredential(requestID: id, target: target)) { [weak self] message in
            guard self?.gitCredentialRequestID == id else { return }
            self?.gitCredentialRequestID = nil
            self?.isCheckingGitCredential = false
            self?.gitCredentialError = message
        }
    }

    func approveGitCredential(target: String, username: String, token: String) {
        guard gateway.connectionState.isReady, gitCredentialRequestID == nil else { return }
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
        gateway.transmit(.approveGitCredential(
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
        guard gateway.connectionState.isReady, sshIdentityRequestID == nil else { return }
        let id = requestID("ssh-list")
        sshIdentityRequestID = id
        sshIdentityError = nil
        isLoadingSshIdentities = true
        gateway.transmit(.listSshIdentities(requestID: id)) { [weak self] message in
            guard self?.sshIdentityRequestID == id else { return }
            self?.sshIdentityRequestID = nil
            self?.isLoadingSshIdentities = false
            self?.sshIdentityError = message
        }
    }

    func generateSshIdentity() {
        guard gateway.connectionState.isReady,
              sshIdentityRequestID == nil,
              sshIdentities?.isEmpty == true
        else { return }
        let id = requestID("ssh-generate")
        sshIdentityRequestID = id
        sshIdentityError = nil
        isGeneratingSshIdentity = true
        gateway.transmit(.generateSshIdentity(requestID: id)) { [weak self] message in
            guard self?.sshIdentityRequestID == id else { return }
            self?.sshIdentityRequestID = nil
            self?.isGeneratingSshIdentity = false
            self?.sshIdentityError = message
        }
    }
}
