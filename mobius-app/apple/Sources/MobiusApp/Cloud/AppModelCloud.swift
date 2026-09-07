import Foundation

let cloudGatewayDisplayName = "möbius Cloud"

enum MobiusCloudAction: Equatable {
    case idle
    case signingIn
    case purchasing
    case restoring
    case provisioning
    case connecting
    case deleting

    var isRunning: Bool { self != .idle }
}

enum MobiusCloudIssue: Equatable {
    case subscriptionAccountConflict
    case subscriptionExpired
}

extension MobiusCloudModel {
    func observeCloudPurchaseUpdates() {
        guard cloudPurchaseUpdateTask == nil else { return }
        let updates = cloudPurchases.updates()
        cloudPurchaseUpdateTask = Task { @MainActor [weak self] in
            for await purchase in updates {
                guard let self, let requestedSession = cloudSession else { continue }
                let generation = operationGeneration
                var activeSession = requestedSession
                do {
                    _ = try await authoritativeCloudAccount(
                        requestedSession: requestedSession,
                        generation: generation
                    )
                    guard operationGeneration == generation,
                        let repairedSession = cloudSession
                    else { continue }
                    activeSession = repairedSession
                    try await acknowledge(purchase)
                    guard operationGeneration == generation,
                        cloudSession == activeSession
                    else { continue }
                    let account = try await authoritativeCloudAccount(
                        requestedSession: activeSession,
                        generation: generation
                    )
                    guard operationGeneration == generation,
                        cloudSession == activeSession
                    else { continue }
                    if applyCloudSubscriptionState(account),
                        operationGeneration == generation,
                        cloudSession == activeSession
                    {
                        callbacks.reconnectRecoveredGateway?()
                    }
                    clearCloudError()
                } catch is CancellationError {
                    continue
                } catch {
                    guard operationGeneration == generation,
                        cloudSession == activeSession
                    else { continue }
                    reportCloud(error)
                }
            }
        }
    }

    func refreshCloudAccount() async {
        guard let requestedSession = cloudSession else {
            cloudAccount = nil
            return
        }
        let generation = operationGeneration
        var activeSession = requestedSession
        clearCloudError()

        do {
            let account = try await authoritativeCloudAccount()
            guard operationGeneration == generation,
                let repairedSession = cloudSession
            else { return }
            activeSession = repairedSession
            let reconciled = try await reconcileActivePurchases(
                from: account,
                generation: generation
            )
            guard operationGeneration == generation else { return }
            if applyCloudSubscriptionState(reconciled) {
                callbacks.reconnectRecoveredGateway?()
            }
        } catch is CancellationError {
            if cloudAccount?.userID != cloudSession?.userID {
                cloudAccount = nil
            }
            return
        } catch {
            guard cloudSession == activeSession else { return }
            guard operationGeneration == generation,
                cloudIssue != .subscriptionExpired
            else { return }
            reportCloud(error)
        }
    }

    func handleCloudSubscriptionExpired() {
        let wasAlreadyExpired = cloudIssue == .subscriptionExpired
        if let account = cloudAccount, account.subscribed {
            cloudAccount = MobiusCloudAccount(
                userID: account.userID,
                email: account.email,
                subscribed: false,
                sharesDiagnostics: account.sharesDiagnostics
            )
        }
        let message = localizedString(
            MobiusCloudError.subscriptionRequired.localizedDescriptionResource)
        cloudIssue = .subscriptionExpired
        cloudError = message

        if isSelectedCloudGateway,
            !wasAlreadyExpired
                || !gateway.automaticReconnectBlocked
                || gateway.connectionState != .failed(message)
        {
            gateway.blockAutomaticReconnect()
            gateway.reset(preservingDrafts: true)
            callbacks.resetGatewayDependentState?(true, true)
            gateway.shutdown(state: .failed(message))
        }
        if !wasAlreadyExpired {
            reportToast(message, tone: .warning)
        }
    }

    func setCloudSharesDiagnostics(_ sharesDiagnostics: Bool) async {
        guard let userID = cloudSession?.userID,
            let account = cloudAccount,
            account.sharesDiagnostics != sharesDiagnostics,
            !isUpdatingCloudDiagnostics
        else { return }
        isUpdatingCloudDiagnostics = true
        clearCloudError()
        defer { isUpdatingCloudDiagnostics = false }

        do {
            try await cloudClient.updateSharesDiagnostics(sharesDiagnostics)
            guard cloudSession?.userID == userID, let account = cloudAccount else { return }
            cloudAccount = MobiusCloudAccount(
                userID: account.userID,
                email: account.email,
                subscribed: account.subscribed,
                sharesDiagnostics: sharesDiagnostics,
                subscriptionStartedAt: account.subscriptionStartedAt,
                luna: account.luna
            )
        } catch is CancellationError {
            return
        } catch {
            guard cloudSession?.userID == userID else { return }
            reportCloud(error)
        }
    }

    func signInAndPurchaseCloud(
        authorizationCode: String,
        nonce: String
    ) async -> Bool {
        guard cloudAction == .idle, cloudAuthenticationRequestTask == nil else { return false }
        let generation = operationGeneration
        cloudAction = .signingIn
        clearCloudError()
        defer { cloudAction = .idle }

        do {
            let task = Task { @MainActor [cloudClient] in
                try await cloudClient.authenticate(
                    authorizationCode: authorizationCode,
                    nonce: nonce
                )
            }
            cloudAuthenticationRequestTask = task
            defer { cloudAuthenticationRequestTask = nil }
            let session = try await withTaskCancellationHandler {
                try await task.value
            } onCancel: {
                task.cancel()
            }
            guard operationGeneration == generation else { return false }
            cloudSession = session
            cloudAccount = nil
            cloudAction = .purchasing
            return try await continueCloudSignup(generation: generation)
        } catch MobiusCloudPurchaseError.cancelled {
            return false
        } catch MobiusCloudPurchaseError.pending {
            toast("Purchase approval is pending.", tone: .info)
            return false
        } catch is CancellationError {
            return false
        } catch {
            reportCloud(error)
            return false
        }
    }

    func purchaseCloud() async -> Bool {
        guard cloudAction == .idle else { return false }
        guard cloudIssue != .subscriptionAccountConflict else { return false }
        guard cloudSession != nil else {
            reportCloud(MobiusCloudError.authenticationRequired)
            return false
        }
        let generation = operationGeneration
        cloudAction = .purchasing
        clearCloudError()
        defer { cloudAction = .idle }

        do {
            return try await continueCloudSignup(generation: generation)
        } catch MobiusCloudPurchaseError.cancelled {
            return false
        } catch MobiusCloudPurchaseError.pending {
            toast("Purchase approval is pending.", tone: .info)
            return false
        } catch is CancellationError {
            return false
        } catch {
            reportCloud(error)
            return false
        }
    }

    func connectCloudGateway() async -> Bool {
        guard cloudAction == .idle else { return false }
        guard cloudSession != nil else {
            reportCloud(MobiusCloudError.authenticationRequired)
            return false
        }
        let generation = operationGeneration
        cloudAction = .provisioning
        clearCloudError()
        defer { cloudAction = .idle }

        do {
            let account = try await authoritativeCloudAccount(generation: generation)
            guard operationGeneration == generation else { throw CancellationError() }
            guard account.subscribed else { throw MobiusCloudError.subscriptionRequired }
            applyCloudSubscriptionState(account)
            return try await provisionCloudGateway(generation: generation)
        } catch is CancellationError {
            return false
        } catch {
            reportCloud(error)
            return false
        }
    }

    func restoreCloudPurchases() async -> Bool {
        guard cloudAction == .idle else { return false }
        guard cloudSession != nil else {
            reportCloud(MobiusCloudError.authenticationRequired)
            return false
        }
        let generation = operationGeneration
        cloudAction = .restoring
        clearCloudError()
        defer { cloudAction = .idle }

        do {
            let account = try await authoritativeCloudAccount(generation: generation)
            let recoveredAccount = try await reconcileActivePurchases(
                from: account,
                synchronize: true,
                generation: generation
            )
            guard operationGeneration == generation else { throw CancellationError() }
            guard recoveredAccount.subscribed else {
                throw MobiusCloudError.subscriptionRequired
            }
            applyCloudSubscriptionState(recoveredAccount)
            return try await provisionCloudGateway(generation: generation)
        } catch is CancellationError {
            return false
        } catch {
            reportCloud(error)
            return false
        }
    }

    func reportCloudSignInFailure() {
        reportCloud(MobiusCloudError.invalidAuthorization)
    }

    func cloudProductDisplayPrice() async throws -> String {
        try await cloudPurchases.displayPrice()
    }

    func manageCloudSubscription() async {
        do {
            try await cloudPurchases.manage()
            await refreshCloudAccount()
        } catch is CancellationError {
            return
        } catch {
            reportCloud(error)
        }
    }

    private func continueCloudSignup(generation: UUID) async throws -> Bool {
        let account = try await authoritativeCloudAccount(generation: generation)
        guard operationGeneration == generation else { throw CancellationError() }
        let recoveredAccount = try await reconcileActivePurchases(
            from: account,
            generation: generation
        )
        guard operationGeneration == generation else { throw CancellationError() }
        if recoveredAccount.subscribed {
            applyCloudSubscriptionState(recoveredAccount)
            return try await provisionCloudGateway(generation: generation)
        }

        let purchase = try await cloudPurchases.purchase(userID: recoveredAccount.userID)
        try await acknowledge(purchase)
        let verifiedAccount = try await authoritativeCloudAccount(generation: generation)
        guard operationGeneration == generation else { throw CancellationError() }
        guard verifiedAccount.subscribed else { throw MobiusCloudError.subscriptionRequired }
        applyCloudSubscriptionState(verifiedAccount)
        return try await provisionCloudGateway(generation: generation)
    }

    private func reconcileActivePurchases(
        from account: MobiusCloudAccount,
        synchronize: Bool = false,
        generation: UUID
    ) async throws -> MobiusCloudAccount {
        guard let requestedSession = cloudSession else {
            throw MobiusCloudError.authenticationRequired
        }
        let unfinished = try await cloudPurchases.unfinishedPurchases()
        let current =
            account.subscribed && !synchronize
            ? MobiusCloudPurchaseScan()
            : try await cloudPurchases.currentEntitlements(synchronize: synchronize)
        guard cloudSession == requestedSession,
            operationGeneration == generation
        else { throw CancellationError() }
        var seenJWS: Set<String> = []
        let purchases = (unfinished.purchases + current.purchases).filter {
            seenJWS.insert($0.jws).inserted
        }
        var firstError: Error? =
            unfinished.hasUnverifiedPurchase || current.hasUnverifiedPurchase
            ? MobiusCloudPurchaseError.unavailable
            : nil
        for purchase in purchases {
            guard cloudSession == requestedSession,
                operationGeneration == generation
            else { throw CancellationError() }
            do {
                try await acknowledge(purchase)
                guard cloudSession == requestedSession,
                    operationGeneration == generation
                else { throw CancellationError() }
            } catch is CancellationError {
                throw CancellationError()
            } catch {
                guard cloudSession == requestedSession else { throw CancellationError() }
                if firstError == nil { firstError = error }
            }
        }
        let refreshed =
            purchases.isEmpty
            ? account
            : try await authoritativeCloudAccount(
                requestedSession: requestedSession,
                generation: generation
            )
        guard cloudSession == requestedSession,
            operationGeneration == generation
        else { throw CancellationError() }
        if refreshed.subscribed { return refreshed }
        if let firstError { throw firstError }
        return refreshed
    }

    private func acknowledge(_ purchase: MobiusCloudPurchase) async throws {
        guard let requestedSession = cloudSession else {
            throw MobiusCloudError.authenticationRequired
        }
        let taskKey = "\(requestedSession.credentialID):\(purchase.jws)"
        if let task = cloudPurchaseTasks[taskKey] {
            return try await task.value
        }
        let task = Task { @MainActor [weak self, cloudClient] in
            guard self?.cloudSession == requestedSession else { throw CancellationError() }
            try await cloudClient.submitSubscription(
                jws: purchase.jws,
                appTransactionJWS: purchase.appTransactionJWS
            )
            guard self?.cloudSession == requestedSession else { throw CancellationError() }
            try await purchase.finish()
        }
        cloudPurchaseTasks[taskKey] = task
        defer { cloudPurchaseTasks[taskKey] = nil }
        try await task.value
    }

    private func authoritativeCloudAccount(
        requestedSession: MobiusCloudSession? = nil,
        generation: UUID? = nil
    ) async throws -> MobiusCloudAccount {
        guard let requestedSession = requestedSession ?? cloudSession else {
            throw MobiusCloudError.authenticationRequired
        }
        let requestGeneration = generation ?? operationGeneration
        let account = try await cloudClient.account()
        guard cloudSession == requestedSession,
            operationGeneration == requestGeneration
        else { throw CancellationError() }
        guard account.userID == requestedSession.userID else {
            try cloudClient.invalidateSession(requestedSession)
            throw MobiusCloudError.accountIdentityMismatch
        }
        cloudAccount = account
        return account
    }

    @discardableResult
    private func applyCloudSubscriptionState(_ account: MobiusCloudAccount) -> Bool {
        let recovered = account.subscribed && cloudIssue == .subscriptionExpired
        if account.subscribed {
            if recovered {
                cloudIssue = nil
                cloudError = nil
                if isSelectedCloudGateway { gateway.allowAutomaticReconnect() }
            }
        } else if cloudGateway != nil || cloudIssue == .subscriptionExpired {
            handleCloudSubscriptionExpired()
        }
        return recovered
    }

    private func provisionCloudGateway(generation: UUID) async throws -> Bool {
        cloudAction = .provisioning
        try await provisionAndPairCloudGateway(generation: generation)
        toast("Your Cloud gateway is connected.", tone: .success)
        return true
    }

    private func provisionAndPairCloudGateway(generation: UUID) async throws {
        for attempt in 0..<150 {
            guard operationGeneration == generation else { throw CancellationError() }
            switch try await cloudClient.gatewayStatus() {
            case .waiting:
                guard attempt < 149 else { throw MobiusCloudError.provisioningTimedOut }
                try await Task.sleep(for: .seconds(2))
            case .ready:
                let grant = try await cloudClient.createPairingGrant()
                guard operationGeneration == generation else { throw CancellationError() }
                cloudAction = .connecting
                gateway.applyPairingSetup(grant.setup)
                gateway.pair()
                callbacks.cloudPairingStarted?()
                gateway.updatePendingPairingAccount(
                    displayName: cloudGatewayDisplayName,
                    cloudUserID: cloudSession?.userID
                )
                try await withTaskCancellationHandler {
                    try await withCheckedThrowingContinuation {
                        (continuation: CheckedContinuation<Void, Error>) in
                        cloudPairingContinuation = continuation
                    }
                } onCancel: {
                    Task { @MainActor [weak self] in
                        guard let self, self.cloudPairingContinuation != nil else { return }
                        self.completeCloudPairing(.failure(CancellationError()))
                        self.gateway.reset(preservingDrafts: true)
                        self.callbacks.resetGatewayDependentState?(true, false)
                    }
                }
                return
            case .expired:
                throw MobiusCloudError.subscriptionRequired
            case .error:
                throw MobiusCloudError.provisioningFailed
            }
        }
    }

    func completeCloudPairing(_ result: Result<Void, Error>) {
        guard let continuation = cloudPairingContinuation else { return }
        cloudPairingContinuation = nil
        if case .failure = result {
            gateway.cancelPendingPairing()
        }
        continuation.resume(with: result)
    }

    func signOutOfCloud() async {
        let selectedCloudGateway = cloudGateway
        let requestedSession = cloudSession
        var pushRemovalFailed = false
        do {
            try await unregisterRemoteNotificationsForCloudSignOut()
        } catch {
            pushRemovalFailed = true
        }
        guard cloudSession == requestedSession else { return }
        if let error = clearLocalCloudState() {
            // Authentication survived; allow replacement without forgetting failed cleanup.
            settingsDefaults.removeObject(forKey: pushTokenRemovalCredentialIDKey)
            await refreshRemoteNotificationRegistration()
            reportToast(localizedErrorDescription(error), tone: .error)
            return
        }
        if pushRemovalFailed {
            pushTokenRemovalPending = true
            settingsDefaults.set(true, forKey: pushTokenRemovalPendingKey)
        }
        let removedGateway =
            if let selectedCloudGateway {
                await callbacks.removeGateway?(selectedCloudGateway) ?? false
            } else {
                true
            }
        guard removedGateway else { return }
        if pushRemovalFailed {
            toast(
                "Signed out of möbius Cloud. Notifications will be updated when you sign in again.",
                tone: .warning
            )
        } else {
            toast("Signed out of möbius Cloud.", tone: .info)
        }
    }

    func deleteCloudAccount(authorizationCode: String, nonce: String) async -> Bool {
        guard cloudAction == .idle else { return false }
        guard let requestedSession = cloudSession else {
            reportCloud(MobiusCloudError.authenticationRequired)
            return false
        }
        let selectedCloudGateway = cloudGateway
        cloudAction = .deleting
        clearCloudError()
        defer { cloudAction = .idle }

        do {
            let cleanupError = try await cloudClient.deleteAccount(
                authorizationCode: authorizationCode,
                nonce: nonce
            )
            guard cloudSession == requestedSession else { return true }
            clearCloudAccountState()
            let gatewayRemoved =
                if let selectedCloudGateway {
                    await callbacks.removeGateway?(selectedCloudGateway) ?? false
                } else {
                    true
                }
            guard cloudSession == nil || cloudSession == requestedSession else { return true }
            switch (cleanupError != nil, gatewayRemoved) {
            case (false, true):
                toast("Your möbius Cloud account deletion has started.", tone: .success)
            case (true, true):
                toast(
                    "Your möbius Cloud account deletion has started, but this device couldn’t forget the saved sign-in.",
                    tone: .error
                )
            case (false, false):
                toast(
                    "Your möbius Cloud account deletion has started, but this device couldn’t remove its local Cloud gateway.",
                    tone: .error
                )
            case (true, false):
                toast(
                    "Your möbius Cloud account deletion has started, but this device couldn’t forget the saved sign-in or remove its local Cloud gateway.",
                    tone: .error
                )
            }
            return true
        } catch is CancellationError {
            return false
        } catch {
            guard cloudSession == requestedSession else { return false }
            reportCloud(error)
            return false
        }
    }

    func reportCloud(_ error: Error) {
        let preservesExpiredIssue = cloudIssue == .subscriptionExpired
        if error is MobiusCloudAuthenticationCleanupError {
            cloudIssue = nil
            clearCloudAccountState()
        } else if let cloudError = error as? MobiusCloudError {
            switch cloudError {
            case .subscriptionAccountConflict:
                cloudIssue = .subscriptionAccountConflict
            case .accountIdentityMismatch, .authenticationRequired, .sessionExpired, .server(401):
                cloudIssue = nil
                clearCloudAccountState()
            default:
                if !preservesExpiredIssue { cloudIssue = nil }
            }
        } else if !preservesExpiredIssue {
            cloudIssue = nil
        }
        let message: String
        if let error = error as? MobiusCloudError {
            message = localizedString(error.localizedDescriptionResource)
        } else if error is MobiusCloudAuthenticationCleanupError {
            message = localizedString(
                "Your Cloud sign-in expired, but this device could not forget the saved sign-in."
            )
        } else if let resource = (error as? MobiusCloudPurchaseError)?
            .localizedDescriptionResource
        {
            message = localizedString(resource)
        } else {
            message = localizedString("Couldn’t connect to möbius Cloud. Try again.")
        }
        if cloudIssue != .subscriptionExpired { cloudError = message }
        reportToast(message, tone: .error)
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
            extensionCatalogError =
                (error as? MobiusCloudError).map {
                    localizedString($0.localizedDescriptionResource)
                } ?? localizedString("The extension catalog is temporarily unavailable.")
        }
    }

}
