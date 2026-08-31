import Foundation

let mobiusCloudGatewayDisplayName = "möbius Cloud"

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
}

extension AppModel {
    func observeCloudPurchaseUpdates() {
        guard cloudPurchaseUpdateTask == nil else { return }
        let updates = cloudPurchases.updates()
        cloudPurchaseUpdateTask = Task { @MainActor [weak self] in
            for await purchase in updates {
                guard let self, let requestedSession = cloudSession else { continue }
                var activeSession = requestedSession
                do {
                    _ = try await authoritativeCloudAccount(requestedSession: requestedSession)
                    guard let repairedSession = cloudSession else { continue }
                    activeSession = repairedSession
                    try await acknowledge(purchase)
                    guard cloudSession == activeSession else { continue }
                    _ = try await authoritativeCloudAccount(requestedSession: activeSession)
                    clearCloudError()
                } catch is CancellationError {
                    continue
                } catch {
                    guard cloudSession == activeSession else { continue }
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
        var activeSession = requestedSession
        clearCloudError()

        do {
            let account = try await authoritativeCloudAccount()
            guard let repairedSession = cloudSession else { return }
            activeSession = repairedSession
            _ = try await reconcileActivePurchases(from: account)
        } catch is CancellationError {
            return
        } catch {
            guard cloudSession == activeSession else { return }
            reportCloud(error)
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
        guard cloudAction == .idle else { return false }
        cloudAction = .signingIn
        clearCloudError()
        defer { cloudAction = .idle }

        do {
            cloudSession = try await cloudClient.authenticate(
                authorizationCode: authorizationCode,
                nonce: nonce
            )
            cloudAccount = nil
            cloudAction = .purchasing
            return try await continueCloudSignup()
        } catch MobiusCloudPurchaseError.cancelled {
            return false
        } catch MobiusCloudPurchaseError.pending {
            showToast("Purchase approval is pending.", tone: .info)
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
        cloudAction = .purchasing
        clearCloudError()
        defer { cloudAction = .idle }

        do {
            return try await continueCloudSignup()
        } catch MobiusCloudPurchaseError.cancelled {
            return false
        } catch MobiusCloudPurchaseError.pending {
            showToast("Purchase approval is pending.", tone: .info)
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
        cloudAction = .provisioning
        clearCloudError()
        defer { cloudAction = .idle }

        do {
            let account = try await authoritativeCloudAccount()
            guard account.subscribed else { throw MobiusCloudError.subscriptionRequired }
            return try await provisionCloudGateway()
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
        cloudAction = .restoring
        clearCloudError()
        defer { cloudAction = .idle }

        do {
            let account = try await authoritativeCloudAccount()
            let recoveredAccount = try await reconcileActivePurchases(
                from: account,
                synchronize: true
            )
            guard recoveredAccount.subscribed else {
                throw MobiusCloudError.subscriptionRequired
            }
            return try await provisionCloudGateway()
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

    private func continueCloudSignup() async throws -> Bool {
        let account = try await authoritativeCloudAccount()
        let recoveredAccount = try await reconcileActivePurchases(from: account)
        if recoveredAccount.subscribed {
            return try await provisionCloudGateway()
        }

        let purchase = try await cloudPurchases.purchase(userID: recoveredAccount.userID)
        try await acknowledge(purchase)
        let verifiedAccount = try await authoritativeCloudAccount()
        guard verifiedAccount.subscribed else { throw MobiusCloudError.subscriptionRequired }
        return try await provisionCloudGateway()
    }

    private func reconcileActivePurchases(
        from account: MobiusCloudAccount,
        synchronize: Bool = false
    ) async throws -> MobiusCloudAccount {
        guard let requestedSession = cloudSession else {
            throw MobiusCloudError.authenticationRequired
        }
        let unfinished = try await cloudPurchases.unfinishedPurchases()
        let current = account.subscribed && !synchronize
            ? MobiusCloudPurchaseScan()
            : try await cloudPurchases.currentEntitlements(synchronize: synchronize)
        var seenJWS: Set<String> = []
        let purchases = (unfinished.purchases + current.purchases).filter {
            seenJWS.insert($0.jws).inserted
        }
        var firstError: Error? = unfinished.hasUnverifiedPurchase || current.hasUnverifiedPurchase
            ? MobiusCloudPurchaseError.unavailable
            : nil
        for purchase in purchases {
            guard cloudSession == requestedSession else { throw CancellationError() }
            do {
                try await acknowledge(purchase)
            } catch is CancellationError {
                throw CancellationError()
            } catch {
                guard cloudSession == requestedSession else { throw CancellationError() }
                if firstError == nil { firstError = error }
            }
        }
        let refreshed = purchases.isEmpty
            ? account
            : try await authoritativeCloudAccount(requestedSession: requestedSession)
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
        requestedSession: MobiusCloudSession? = nil
    ) async throws -> MobiusCloudAccount {
        guard let requestedSession = requestedSession ?? cloudSession else {
            throw MobiusCloudError.authenticationRequired
        }
        let account = try await cloudClient.account()
        guard cloudSession == requestedSession else { throw CancellationError() }
        guard account.userID == requestedSession.userID else {
            try cloudClient.invalidateSession(requestedSession)
            throw MobiusCloudError.accountIdentityMismatch
        }
        cloudAccount = account
        return account
    }

    private func provisionCloudGateway() async throws -> Bool {
        cloudAction = .provisioning
        try await provisionAndPairCloudGateway()
        showToast("Your Cloud gateway is connected.", tone: .success)
        return true
    }

    private func provisionAndPairCloudGateway() async throws {
        for attempt in 0..<150 {
            switch try await cloudClient.gatewayStatus() {
            case .waiting:
                guard attempt < 149 else { throw MobiusCloudError.provisioningTimedOut }
                try await Task.sleep(for: .seconds(2))
            case .ready:
                let grant = try await cloudClient.createPairingGrant()
                cloudAction = .connecting
                applyPairingSetup(grant.setup)
                showsPairing = false
                pair()
                pendingPairingAccount?.displayName = mobiusCloudGatewayDisplayName
                pendingPairingAccount?.cloudUserID = cloudSession?.userID
                try await withTaskCancellationHandler {
                    try await withCheckedThrowingContinuation {
                        (continuation: CheckedContinuation<Void, Error>) in
                        cloudPairingContinuation = continuation
                    }
                } onCancel: {
                    Task { @MainActor [weak self] in
                        guard self?.cloudPairingContinuation != nil else { return }
                        self?.resetGatewayState(preservingDrafts: true)
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
            pairingEndpoint = "wss://"
            pairingCode = ""
            pendingPairingAccount = nil
            automaticReconnectBlocked = true
        }
        continuation.resume(with: result)
    }

    func signOutOfCloud() async {
        let cloudGateway = mobiusCloudGateway
        do {
            try cloudClient.signOut()
        } catch {
            showToast(verbatim: localizedErrorDescription(error), tone: .error)
            return
        }
        clearCloudAccountState()
        if let cloudGateway, !(await removeGateway(cloudGateway)) { return }
        showToast("Signed out of möbius Cloud.", tone: .info)
    }

    func clearDataAndGatewayInformation() async {
        guard !isClearingLocalData else { return }
        isClearingLocalData = true
        defer { isClearingLocalData = false }

        cancelReconnect()
        automaticReconnectBlocked = true
        discardComposerDraft()
        _ = resetGatewayState(preservingDrafts: false, preservingSession: false)
        let transcriptIO = transcriptIOTask
        let composerIO = composerDraftIOTask
        await client.disconnect()
        await transcriptIO?.value
        await composerIO?.value

        do {
            try cloudClient.signOut()
            clearCloudAccountState()
            try await store.clearAllData()
        } catch {
            accounts = store.loadAccounts()
            selectedAccountID = store.selectedAccountID() ?? accounts.first?.id
            restoreSessionReadState()
            showsPairing = accounts.isEmpty
            showToast(verbatim: localizedErrorDescription(error), tone: .error)
            return
        }

        accounts = []
        selectedAccountID = nil
        restoreSessionReadState()
        pairingEndpoint = "wss://"
        pairingCode = ""
        pairingError = nil
        destination = .chats
        navigationPath = []
        showsPairing = true
        showToast("Local data and gateway information cleared.", tone: .success)
    }

    func deleteCloudAccount(authorizationCode: String, nonce: String) async -> Bool {
        guard cloudAction == .idle else { return false }
        guard cloudSession != nil else {
            reportCloud(MobiusCloudError.authenticationRequired)
            return false
        }
        let cloudGateway = mobiusCloudGateway
        cloudAction = .deleting
        clearCloudError()
        defer { cloudAction = .idle }

        do {
            try await cloudClient.deleteAccount(
                authorizationCode: authorizationCode,
                nonce: nonce
            )
            clearCloudAccountState()
            if let cloudGateway, !(await removeGateway(cloudGateway)) {
                showToast(
                    "Account deletion started, but this device couldn’t remove its local Cloud gateway.",
                    tone: .error
                )
                return true
            }
            showToast("Your möbius Cloud account is being deleted.", tone: .success)
            return true
        } catch is CancellationError {
            return false
        } catch {
            reportCloud(error)
            return false
        }
    }

    func reportCloud(_ error: Error) {
        if let cloudError = error as? MobiusCloudError {
            switch cloudError {
            case .subscriptionAccountConflict:
                cloudIssue = .subscriptionAccountConflict
            case .accountIdentityMismatch, .authenticationRequired, .sessionExpired, .server(401):
                cloudIssue = nil
                clearCloudAccountState()
            default:
                cloudIssue = nil
            }
        } else {
            cloudIssue = nil
        }
        let message: String
        if let error = error as? MobiusCloudError {
            message = localizedString(error.localizedDescriptionResource)
        } else if let resource = (error as? MobiusCloudPurchaseError)?
            .localizedDescriptionResource {
            message = localizedString(resource)
        } else {
            message = localizedString("Couldn’t connect to möbius Cloud. Try again.")
        }
        cloudError = message
        showToast(verbatim: message, tone: .error)
    }

    private func clearCloudAccountState() {
        cloudSession = nil
        cloudAccount = nil
        cloudError = nil
        cloudIssue = nil
        availableExtensions = []
        extensionCatalogError = nil
        isLoadingExtensionCatalog = false
    }

    private func clearCloudError() {
        cloudError = nil
        cloudIssue = nil
    }
}
