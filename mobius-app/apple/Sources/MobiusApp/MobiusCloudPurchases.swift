import Foundation
import StoreKit
import UIKit

@MainActor
struct MobiusCloudPurchase {
    let jws: String
    let appTransactionJWS: String
    private let finishAction: @MainActor () async throws -> Void

    init(
        jws: String,
        appTransactionJWS: String,
        finish: @escaping @MainActor () async throws -> Void = {}
    ) {
        self.jws = jws
        self.appTransactionJWS = appTransactionJWS
        finishAction = finish
    }

    func finish() async throws {
        try await finishAction()
    }
}

struct MobiusCloudPurchaseScan {
    var purchases: [MobiusCloudPurchase] = []
    var hasUnverifiedPurchase = false
}

enum MobiusCloudPurchaseError: LocalizedError {
    case cancelled
    case pending
    case unavailable

    var errorDescription: String? {
        switch self {
        case .cancelled: nil
        case .pending: "Purchase approval is pending."
        case .unavailable: "The App Store purchase could not be completed."
        }
    }
}

@MainActor
struct MobiusCloudPurchases {
    private let loadDisplayPrice: @MainActor () async throws -> String
    private let loadUnfinishedPurchases: @MainActor () async throws -> MobiusCloudPurchaseScan
    private let loadCurrentEntitlements: @MainActor (Bool) async throws -> MobiusCloudPurchaseScan
    private let requestPurchase: @MainActor (UUID) async throws -> MobiusCloudPurchase
    private let loadUpdates: @MainActor () -> AsyncStream<MobiusCloudPurchase>
    private let showSubscriptionManagement: @MainActor () async throws -> Void

    init(
        displayPrice: @escaping @MainActor () async throws -> String,
        unfinishedPurchases: @escaping @MainActor () async throws -> MobiusCloudPurchaseScan,
        currentEntitlements: @escaping @MainActor (Bool) async throws -> MobiusCloudPurchaseScan,
        purchase: @escaping @MainActor (UUID) async throws -> MobiusCloudPurchase,
        updates: @escaping @MainActor () -> AsyncStream<MobiusCloudPurchase> = {
            AsyncStream { $0.finish() }
        },
        manage: @escaping @MainActor () async throws -> Void = {}
    ) {
        loadDisplayPrice = displayPrice
        loadUnfinishedPurchases = unfinishedPurchases
        loadCurrentEntitlements = currentEntitlements
        requestPurchase = purchase
        loadUpdates = updates
        showSubscriptionManagement = manage
    }

    static func live() -> Self {
        let bridge = StoreKitCloudBridge()
        return Self(
            displayPrice: bridge.displayPrice,
            unfinishedPurchases: bridge.unfinishedPurchases,
            currentEntitlements: bridge.currentEntitlements(synchronize:),
            purchase: bridge.purchase(userID:),
            updates: { bridge.updates },
            manage: bridge.showSubscriptionManagement
        )
    }

    func displayPrice() async throws -> String {
        try await loadDisplayPrice()
    }

    func unfinishedPurchases() async throws -> MobiusCloudPurchaseScan {
        try await loadUnfinishedPurchases()
    }

    func currentEntitlements(synchronize: Bool = false) async throws -> MobiusCloudPurchaseScan {
        try await loadCurrentEntitlements(synchronize)
    }

    func purchase(userID: UUID) async throws -> MobiusCloudPurchase {
        try await requestPurchase(userID)
    }

    func updates() -> AsyncStream<MobiusCloudPurchase> {
        loadUpdates()
    }

    func manage() async throws {
        try await showSubscriptionManagement()
    }
}

@MainActor
private final class StoreKitCloudBridge {
    let updates: AsyncStream<MobiusCloudPurchase>

    private let updateContinuation: AsyncStream<MobiusCloudPurchase>.Continuation
    private var updateTask: Task<Void, Never>?

    init() {
        let stream = AsyncStream.makeStream(
            of: MobiusCloudPurchase.self,
            bufferingPolicy: .unbounded
        )
        updates = stream.stream
        updateContinuation = stream.continuation

        updateTask = Task { [continuation = stream.continuation] in
            for await verification in Transaction.updates {
                guard !Task.isCancelled else { break }
                guard let purchase = try? await Self.purchase(from: verification) else { continue }
                continuation.yield(purchase)
            }
            continuation.finish()
        }
    }

    deinit {
        updateTask?.cancel()
        updateContinuation.finish()
    }

    func displayPrice() async throws -> String {
        try await product().displayPrice
    }

    func unfinishedPurchases() async -> MobiusCloudPurchaseScan {
        var scan = MobiusCloudPurchaseScan()
        var seenJWS: Set<String> = []
        for await verification in Transaction.unfinished {
            await Self.append(verification, to: &scan, seenJWS: &seenJWS)
        }
        return scan
    }

    func currentEntitlements(synchronize: Bool) async throws -> MobiusCloudPurchaseScan {
        if synchronize {
            try await AppStore.sync()
        }

        var scan = MobiusCloudPurchaseScan()
        var seenJWS: Set<String> = []
        for await verification in Transaction.currentEntitlements {
            await Self.append(verification, to: &scan, seenJWS: &seenJWS)
        }
        return scan
    }

    func purchase(userID: UUID) async throws -> MobiusCloudPurchase {
        let result: Product.PurchaseResult
        do {
            result = try await product().purchase(options: [.appAccountToken(userID)])
        } catch is CancellationError {
            throw CancellationError()
        } catch {
            throw MobiusCloudPurchaseError.unavailable
        }

        switch result {
        case .success(let verification):
            guard let purchase = try await Self.purchase(from: verification) else {
                throw MobiusCloudPurchaseError.unavailable
            }
            return purchase
        case .pending:
            throw MobiusCloudPurchaseError.pending
        case .userCancelled:
            throw MobiusCloudPurchaseError.cancelled
        @unknown default:
            throw MobiusCloudPurchaseError.unavailable
        }
    }

    func showSubscriptionManagement() async throws {
        let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
        guard let scene = scenes.first(where: { $0.activationState == .foregroundActive })
            ?? scenes.first
        else { throw MobiusCloudPurchaseError.unavailable }
        try await AppStore.showManageSubscriptions(in: scene)
    }

    private func product() async throws -> Product {
        guard let product = try await Product.products(for: [mobiusCloudMonthlyProductID])
            .first(where: { $0.id == mobiusCloudMonthlyProductID && $0.type == .autoRenewable })
        else { throw MobiusCloudPurchaseError.unavailable }
        return product
    }

    private static func append(
        _ verification: VerificationResult<Transaction>,
        to scan: inout MobiusCloudPurchaseScan,
        seenJWS: inout Set<String>
    ) async {
        do {
            guard let purchase = try await purchase(from: verification),
                  seenJWS.insert(purchase.jws).inserted
            else { return }
            scan.purchases.append(purchase)
        } catch {
            scan.hasUnverifiedPurchase = true
        }
    }

    private static func purchase(
        from verification: VerificationResult<Transaction>
    ) async throws -> MobiusCloudPurchase? {
        switch verification {
        case .verified(let transaction):
            guard transaction.productID == mobiusCloudMonthlyProductID else { return nil }
            let jws = verification.jwsRepresentation
            let appTransactionJWS = try await verifiedAppTransactionJWS()
            return MobiusCloudPurchase(jws: jws, appTransactionJWS: appTransactionJWS) {
                await transaction.finish()
            }
        case .unverified(let transaction, _):
            guard transaction.productID == mobiusCloudMonthlyProductID else { return nil }
            throw MobiusCloudPurchaseError.unavailable
        }
    }

    private static func verifiedAppTransactionJWS() async throws -> String {
        let verification = try await AppTransaction.shared
        guard case .verified = verification else {
            throw MobiusCloudPurchaseError.unavailable
        }
        return verification.jwsRepresentation
    }
}
