import Foundation
import Observation

struct MobiusCloudModelCallbacks {
    var resetGatewayDependentState: (@MainActor (Bool, Bool) -> Void)?
    var reconnectRecoveredGateway: (@MainActor () -> Void)?
    var removeGateway: (@MainActor (GatewayAccount) async -> Bool)?
    var cloudPairingStarted: (@MainActor () -> Void)?
    var showToast: (@MainActor (String, ToastTone) -> Void)?
    var presentRemoteNotification: (@MainActor (RemoteNotification, String?, String?) -> Void)?
    var openRemoteNotification: (@MainActor (RemoteNotification) -> Void)?
}

@MainActor
@Observable
final class MobiusCloudModel {
    var cloudSession: MobiusCloudSession?
    var cloudAccount: MobiusCloudAccount?
    var cloudAction: MobiusCloudAction = .idle
    var cloudError: String?
    var cloudIssue: MobiusCloudIssue?
    var isUpdatingCloudDiagnostics = false
    var availableExtensions: [MobiusCloudExtensionCatalogItem] = []
    var extensionCatalogError: String?
    var isLoadingExtensionCatalog = false

    var notificationsEnabled: Bool
    var isUpdatingNotifications = false
    var notificationError: String?
    var pendingRemoteNotification: RemoteNotification?
    var remoteNotificationDeviceToken: String?
    var pushTokenRemovalPending: Bool

    @ObservationIgnored var callbacks = MobiusCloudModelCallbacks()
    @ObservationIgnored let gateway: GatewayConnectionModel
    @ObservationIgnored let cloudClient: MobiusCloudClient
    @ObservationIgnored let cloudPurchases: MobiusCloudPurchases
    @ObservationIgnored let settingsDefaults: UserDefaults
    @ObservationIgnored let remoteNotifications: RemoteNotificationSystem
    @ObservationIgnored let pushInstallationID: UUID
    @ObservationIgnored var cloudPairingContinuation: CheckedContinuation<Void, Error>?
    @ObservationIgnored var cloudPurchaseUpdateTask: Task<Void, Never>?
    @ObservationIgnored var cloudPurchaseTasks: [String: Task<Void, Error>] = [:]
    @ObservationIgnored var cloudAuthenticationRequestTask: Task<MobiusCloudSession, Error>?
    @ObservationIgnored var cloudAuthenticationTask: Task<Void, Never>?
    @ObservationIgnored private(set) var operationGeneration = UUID()
    @ObservationIgnored var remoteNotificationRegistrationTask: Task<Void, Never>?
    @ObservationIgnored var remoteNotificationEventIDs: Set<String> = []
    @ObservationIgnored var remoteNotificationEventOrder: [String] = []
    @ObservationIgnored var notificationKeys: Set<AppNotificationKey> = []
    @ObservationIgnored var notificationKeyOrder: [AppNotificationKey] = []

    init(
        gateway: GatewayConnectionModel,
        settingsDefaults: UserDefaults,
        remoteNotifications: RemoteNotificationSystem,
        cloudClient: MobiusCloudClient,
        cloudPurchases: MobiusCloudPurchases
    ) {
        self.gateway = gateway
        self.settingsDefaults = settingsDefaults
        self.remoteNotifications = remoteNotifications
        self.cloudClient = cloudClient
        self.cloudPurchases = cloudPurchases
        cloudSession = try? cloudClient.loadSession()
        let pushInstallationID =
            settingsDefaults.string(forKey: pushInstallationIDKey)
            .flatMap(UUID.init(uuidString:)) ?? UUID()
        settingsDefaults.set(pushInstallationID.uuidString, forKey: pushInstallationIDKey)
        self.pushInstallationID = pushInstallationID
        notificationsEnabled = settingsDefaults.bool(forKey: notificationsEnabledKey)
        pushTokenRemovalPending = settingsDefaults.bool(forKey: pushTokenRemovalPendingKey)
    }

    isolated deinit {
        cloudPurchaseUpdateTask?.cancel()
        cloudPurchaseTasks.values.forEach { $0.cancel() }
        cloudAuthenticationRequestTask?.cancel()
        cloudAuthenticationTask?.cancel()
        remoteNotificationRegistrationTask?.cancel()
    }

    var hasCloudAccount: Bool { cloudSession != nil }

    var isLoadingCloudAccount: Bool {
        hasCloudAccount && cloudAccount == nil && cloudError == nil
    }

    var cloudGateway: GatewayAccount? {
        guard let userID = cloudSession?.userID else { return nil }
        return gateway.accounts.first { $0.cloudUserID == userID }
    }

    var isSelectedCloudGateway: Bool {
        guard let cloudGateway else { return false }
        return gateway.selectedAccountID == cloudGateway.id
    }

    func appStoreURL() async -> URL? {
        await cloudPurchases.appStoreURL()
    }

    func toast(_ message: LocalizedStringResource, tone: ToastTone = .info) {
        var message = message
        message.locale = locale
        callbacks.showToast?(String(localized: message), tone)
    }

    var locale: Locale {
        gateway.locale
    }

    func localizedString(_ resource: LocalizedStringResource) -> String {
        var resource = resource
        resource.locale = locale
        return String(localized: resource)
    }

    func localizedErrorDescription(_ error: Error) -> String {
        if let error = error as? MobiusCloudError {
            return localizedString(error.localizedDescriptionResource)
        }
        if let error = error as? MobiusCloudPurchaseError,
            let resource = error.localizedDescriptionResource
        {
            return localizedString(resource)
        }
        return error.localizedDescription
    }

    func reportToast(_ message: String, tone: ToastTone) {
        callbacks.showToast?(message, tone)
    }

    func clearCloudAccountState() {
        cloudSession = nil
        cloudAccount = nil
        cloudError = nil
        cloudIssue = nil
        availableExtensions = []
        extensionCatalogError = nil
        isLoadingExtensionCatalog = false
        stopRemoteNotifications(forgetsCloudInstallation: true)
    }

    func clearCloudError() {
        guard cloudIssue != .subscriptionExpired else { return }
        cloudError = nil
        cloudIssue = nil
    }

    func invalidateInFlightOperations() {
        operationGeneration = UUID()
        cloudAuthenticationTask?.cancel()
        cloudPurchaseTasks.values.forEach { $0.cancel() }
        cloudAuthenticationRequestTask?.cancel()
        if cloudPairingContinuation != nil {
            completeCloudPairing(.failure(CancellationError()))
        }
    }

    func scheduleAuthenticationRefresh() {
        cloudAuthenticationTask?.cancel()
        cloudAuthenticationTask = Task { @MainActor [weak self] in
            await self?.cloudAuthenticationDidChange()
        }
    }

    func cancelAuthenticationRefresh() {
        cloudAuthenticationTask?.cancel()
        cloudAuthenticationTask = nil
    }

    @discardableResult
    func clearLocalCloudState() -> Error? {
        do {
            try cloudClient.signOut()
            clearCloudAccountState()
            return nil
        } catch {
            return error
        }
    }
}
