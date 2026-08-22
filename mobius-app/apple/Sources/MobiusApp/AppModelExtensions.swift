import Foundation

extension AppModel {
    /// One extension mutation is in flight at a time, and all of them need the gateway.
    var canMutateExtensions: Bool {
        extensionAction == nil && connectionState.isReady
    }

    func installExtension() {
        let source = extensionInstallSource.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !source.isEmpty else { return }
        beginExtensionAction(.installing) { requestID in
            .installExtension(
                requestID: requestID,
                source: source,
                reference: nil,
                subdirectory: nil
            )
        }
    }

    func installExtension(_ item: MobiusCloudExtensionCatalogItem) {
        guard availableExtensions.contains(item) else { return }
        beginExtensionAction(.installing) { requestID in
            .installExtension(
                requestID: requestID,
                source: item.source.url,
                reference: item.source.reference,
                subdirectory: item.source.subdirectory
            )
        }
    }

    func refreshExtensionCatalog() async {
        guard let userID = cloudSession?.userID else {
            availableExtensions = []
            extensionCatalogError = nil
            isLoadingExtensionCatalog = false
            return
        }
        availableExtensions = []
        extensionCatalogError = nil
        isLoadingExtensionCatalog = true
        defer {
            if cloudSession?.userID == userID { isLoadingExtensionCatalog = false }
        }

        do {
            let catalog = try await cloudClient.extensionCatalog()
            guard cloudSession?.userID == userID else { return }
            availableExtensions = catalog
        } catch is CancellationError {
            return
        } catch {
            guard cloudSession?.userID == userID else { return }
            if let error = error as? MobiusCloudError {
                switch error {
                case .authenticationRequired, .sessionExpired, .server(401):
                    reportCloud(error)
                    return
                default:
                    break
                }
            }
            extensionCatalogError = (error as? MobiusCloudError)?.localizedDescription
                ?? "The extension catalog is temporarily unavailable."
        }
    }

    func updateExtension(_ extensionRecord: ExtensionRecord) {
        beginExtensionAction(.updating(extensionRecord.name)) { requestID in
            .updateExtension(requestID: requestID, id: extensionRecord.id)
        }
    }

    func uninstallExtension(_ extensionRecord: ExtensionRecord) {
        beginExtensionAction(.uninstalling(extensionRecord.name)) { requestID in
            .uninstallExtension(requestID: requestID, id: extensionRecord.id)
        }
    }

    func trustHooks(for extensionRecord: ExtensionRecord) {
        guard !extensionRecord.hooks.isEmpty, !extensionRecord.hooksTrusted else { return }
        beginExtensionAction(.trusting(extensionRecord.name)) { requestID in
            .trustExtensionHooks(
                requestID: requestID,
                id: extensionRecord.id,
                expectedDigest: extensionRecord.digest
            )
        }
    }

    func untrustHooks(for extensionRecord: ExtensionRecord) {
        guard !extensionRecord.hooks.isEmpty, extensionRecord.hooksTrusted else { return }
        beginExtensionAction(.untrusting(extensionRecord.name)) { requestID in
            .revokeExtensionHooksTrust(
                requestID: requestID,
                id: extensionRecord.id,
                expectedDigest: extensionRecord.digest
            )
        }
    }

    func completeExtensionAction(requestID: String) {
        guard requestID == extensionRequestID, let action = extensionAction else { return }
        extensionRequestID = nil
        extensionAction = nil
        if action == .installing { extensionInstallSource = "" }
        let outcome = extensionCompletionOutcome(action)
        showToast(outcome.message, tone: outcome.tone)
    }

    func rejectExtensionAction(requestID: String) {
        guard requestID == extensionRequestID else { return }
        extensionRequestID = nil
        extensionAction = nil
    }

    private func beginExtensionAction(
        _ action: ExtensionAction,
        request: (String) -> GatewayRequest
    ) {
        guard extensionRequestID == nil, connectionState.isReady else { return }
        let id = requestID("extension")
        extensionRequestID = id
        extensionAction = action
        transmit(request(id)) { [weak self] _ in
            self?.rejectExtensionAction(requestID: id)
        }
    }

    private func extensionCompletionOutcome(
        _ action: ExtensionAction
    ) -> (message: String, tone: ToastTone) {
        switch action {
        case .installing:
            return ("Extension installed.", .success)
        case .updating(let name):
            return ("\(name) updated.", .success)
        case .uninstalling(let name):
            return ("\(name) uninstalled.", .success)
        case .trusting(let name):
            return ("\(name) hooks trusted.", .success)
        case .untrusting(let name):
            return ("\(name) hooks untrusted.", .success)
        }
    }
}
