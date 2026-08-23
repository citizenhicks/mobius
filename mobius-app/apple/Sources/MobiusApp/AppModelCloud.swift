import Foundation
import StoreKit

let mobiusCloudGatewayDisplayName = "möbius Cloud"

func isMobiusCloudSpriteEndpoint(_ endpoint: GatewayEndpoint) -> Bool {
    endpoint.host.lowercased().hasSuffix(".sprites.app")
}

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

extension AppModel {
    func refreshCloudAccount() async {
        guard let userID = cloudSession?.userID else {
            cloudAccount = nil
            return
        }
        cloudError = nil

        do {
            var account = try await cloudClient.account()
            if let verification = try await currentCloudEntitlement() {
                let transaction = try cloudTransaction(from: verification)
                if !account.subscribed {
                    try await cloudClient.submitSubscription(
                        signedTransaction: verification.jwsRepresentation
                    )
                    await transaction.finish()
                }
                account = MobiusCloudAccount(
                    email: account.email,
                    subscribed: true,
                    sharesDiagnostics: account.sharesDiagnostics,
                    subscriptionStartedAt: transaction.originalPurchaseDate,
                    luna: account.luna
                )
            }
            guard cloudSession?.userID == userID else { return }
            cloudAccount = account
        } catch is CancellationError {
            return
        } catch {
            guard cloudSession?.userID == userID else { return }
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
        cloudError = nil
        defer { isUpdatingCloudDiagnostics = false }

        do {
            try await cloudClient.updateSharesDiagnostics(sharesDiagnostics)
            guard cloudSession?.userID == userID, let account = cloudAccount else { return }
            cloudAccount = MobiusCloudAccount(
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
        nonce: String,
        product: Product?
    ) async -> Bool {
        guard cloudAction == .idle else { return false }
        cloudAction = .signingIn
        cloudError = nil
        defer { cloudAction = .idle }

        do {
            cloudSession = try await cloudClient.authenticate(
                authorizationCode: authorizationCode,
                nonce: nonce
            )
            cloudAccount = nil
            cloudAction = .purchasing
            return try await continueCloudSignup(with: product)
        } catch is CancellationError {
            return false
        } catch {
            reportCloud(error)
            return false
        }
    }

    func purchaseCloud(_ product: Product) async -> Bool {
        guard cloudAction == .idle else { return false }
        guard cloudSession != nil else {
            reportCloud(MobiusCloudError.authenticationRequired)
            return false
        }
        cloudAction = .purchasing
        cloudError = nil
        defer { cloudAction = .idle }

        do {
            return try await continueCloudSignup(with: product)
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
        cloudError = nil
        defer { cloudAction = .idle }

        do {
            let account = try await cloudClient.account()
            cloudAccount = MobiusCloudAccount(
                email: account.email,
                subscribed: account.subscribed,
                sharesDiagnostics: account.sharesDiagnostics,
                subscriptionStartedAt: cloudAccount?.subscriptionStartedAt,
                luna: account.luna
            )
            guard account.subscribed else { throw MobiusCloudError.subscriptionRequired }
            return try await provisionCloudGateway()
        } catch is CancellationError {
            return false
        } catch {
            reportCloud(error)
            return false
        }
    }

    func restoreCloudPurchases(
        synchronize: () async throws -> Void = { try await AppStore.sync() }
    ) async -> Bool {
        guard cloudAction == .idle else { return false }
        guard cloudSession != nil else {
            reportCloud(MobiusCloudError.authenticationRequired)
            return false
        }
        cloudAction = .restoring
        cloudError = nil
        defer { cloudAction = .idle }

        do {
            try await synchronize()
            guard try await continueCloudSignup(with: nil) else {
                throw MobiusCloudError.invalidSignedTransaction
            }
            return true
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

    private func continueCloudSignup(with product: Product?) async throws -> Bool {
        guard let userID = cloudSession?.userID else {
            throw MobiusCloudError.authenticationRequired
        }

        let account = try await cloudClient.account()
        cloudAccount = MobiusCloudAccount(
            email: account.email,
            subscribed: account.subscribed,
            sharesDiagnostics: account.sharesDiagnostics,
            subscriptionStartedAt: cloudAccount?.subscriptionStartedAt,
            luna: account.luna
        )
        if account.subscribed {
            return try await provisionCloudGateway()
        }

        if let verification = try await currentCloudEntitlement() {
            try await acknowledge(verification)
            return try await provisionCloudGateway()
        }

        guard let product else { return false }
        guard product.id == mobiusCloudMonthlyProductID else {
            throw MobiusCloudError.invalidSignedTransaction
        }

        switch try await product.purchase(options: [.appAccountToken(userID)]) {
        case .success(let verification):
            try await acknowledge(verification)
            return try await provisionCloudGateway()
        case .pending:
            showToast("Purchase approval is pending.", tone: .info)
            return false
        case .userCancelled:
            return false
        @unknown default:
            throw MobiusCloudError.unverifiedTransaction
        }
    }

    private func acknowledge(_ verification: VerificationResult<Transaction>) async throws {
        let transaction = try cloudTransaction(from: verification)

        try await cloudClient.submitSubscription(
            signedTransaction: verification.jwsRepresentation
        )
        await transaction.finish()
        cloudAccount = MobiusCloudAccount(
            email: cloudAccount?.email,
            subscribed: true,
            sharesDiagnostics: cloudAccount?.sharesDiagnostics ?? false,
            subscriptionStartedAt: transaction.originalPurchaseDate,
            luna: cloudAccount?.luna
        )
    }

    private func currentCloudEntitlement() async throws -> VerificationResult<Transaction>? {
        var sawUnverifiedTransaction = false
        for await verification in Transaction.currentEntitlements(
            for: mobiusCloudMonthlyProductID
        ) {
            switch verification {
            case .verified:
                _ = try cloudTransaction(from: verification)
                return verification
            case .unverified:
                sawUnverifiedTransaction = true
            }
        }
        if sawUnverifiedTransaction { throw MobiusCloudError.unverifiedTransaction }
        return nil
    }

    private func cloudTransaction(
        from verification: VerificationResult<Transaction>
    ) throws -> Transaction {
        guard case .verified(let transaction) = verification,
              let session = cloudSession,
              transaction.productID == mobiusCloudMonthlyProductID,
              transaction.appAccountToken == session.userID,
              transaction.revocationDate == nil
        else { throw MobiusCloudError.unverifiedTransaction }
        return transaction
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
            showToast(error.localizedDescription, tone: .error)
            return
        }
        clearCloudAccountState()
        if let cloudGateway, !(await removeGateway(cloudGateway)) { return }
        showToast("Signed out of möbius Cloud.", tone: .info)
    }

    func deleteCloudAccount(authorizationCode: String, nonce: String) async -> Bool {
        guard cloudAction == .idle else { return false }
        guard cloudSession != nil else {
            reportCloud(MobiusCloudError.authenticationRequired)
            return false
        }
        let cloudGateway = mobiusCloudGateway
        cloudAction = .deleting
        cloudError = nil
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
            case .authenticationRequired, .sessionExpired, .server(401):
                clearCloudAccountState()
            default:
                break
            }
        }
        let message = (error as? MobiusCloudError)?.localizedDescription
            ?? "Couldn’t connect to möbius Cloud. Try again."
        cloudError = message
        showToast(message, tone: .error)
    }

    private func clearCloudAccountState() {
        cloudSession = nil
        cloudAccount = nil
        cloudError = nil
        availableExtensions = []
        extensionCatalogError = nil
        isLoadingExtensionCatalog = false
    }
}
