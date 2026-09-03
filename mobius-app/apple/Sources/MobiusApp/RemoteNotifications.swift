import Foundation
import UIKit
import UserNotifications

let notificationsEnabledKey = "notifications-enabled"
let pushInstallationIDKey = "push-installation-id"
let pushTokenRemovalPendingKey = "push-token-removal-pending"

enum APNsEnvironment: String, Encodable, Sendable {
    case sandbox
    case production

    static var current: Self {
        if let value = Bundle.main.object(forInfoDictionaryKey: "MobiusAPNsEnvironment")
            as? String {
            switch value {
            case "development", "sandbox": return .sandbox
            case "production": return .production
            default: break
            }
        }
        #if DEBUG
        return .sandbox
        #else
        return .production
        #endif
    }
}

enum RemoteNotificationAuthorization: Equatable {
    case notDetermined
    case denied
    case authorized
}

@MainActor
struct RemoteNotificationSystem {
    let authorization: () async -> RemoteNotificationAuthorization
    let requestAuthorization: () async throws -> Bool
    let register: () -> Void
    let unregister: () -> Void
    let removeAll: () -> Void
    let openSettings: () -> Void

    static func live() -> Self {
        Self(
            authorization: {
                switch await UNUserNotificationCenter.current().notificationSettings()
                    .authorizationStatus {
                case .notDetermined: .notDetermined
                case .denied: .denied
                case .authorized, .provisional, .ephemeral: .authorized
                @unknown default: .denied
                }
            },
            requestAuthorization: {
                try await UNUserNotificationCenter.current().requestAuthorization(
                    options: [.alert, .sound]
                )
            },
            register: { UIApplication.shared.registerForRemoteNotifications() },
            unregister: { UIApplication.shared.unregisterForRemoteNotifications() },
            removeAll: {
                let center = UNUserNotificationCenter.current()
                center.removeAllDeliveredNotifications()
                center.removeAllPendingNotificationRequests()
            },
            openSettings: {
                guard let url = URL(string: UIApplication.openNotificationSettingsURLString)
                else { return }
                UIApplication.shared.open(url)
            }
        )
    }
}

enum SessionNotificationKind: String, Hashable {
    case awaitingApproval = "session.awaiting_approval"
    case completed = "session.completed"
    case aborted = "session.aborted"
    case failed = "session.failed"
}

struct RemoteSessionNotification: Equatable {
    let eventID: String
    let kind: SessionNotificationKind
    let sessionID: String
    let runCount: UInt64?
    let approvalRequestID: String?

    init(
        eventID: String,
        kind: SessionNotificationKind,
        sessionID: String,
        runCount: UInt64? = nil,
        approvalRequestID: String? = nil
    ) {
        self.eventID = eventID
        self.kind = kind
        self.sessionID = sessionID
        self.runCount = runCount
        self.approvalRequestID = approvalRequestID
    }

    init?(userInfo: [AnyHashable: Any]) {
        guard let eventID = Self.identifier(userInfo["eventId"]),
              let rawKind = userInfo["kind"] as? String,
              let kind = SessionNotificationKind(rawValue: rawKind),
              let sessionID = Self.identifier(userInfo["sessionId"])
        else { return nil }
        let runCount = Self.unsignedInteger(userInfo["runCount"])
        let approvalRequestID = Self.optionalIdentifier(userInfo["approvalRequestId"])
        let hasRequiredCursor = switch kind {
        case .awaitingApproval: approvalRequestID != nil
        case .completed, .aborted, .failed: runCount != nil
        }
        guard hasRequiredCursor else { return nil }
        self.init(
            eventID: eventID,
            kind: kind,
            sessionID: sessionID,
            runCount: runCount,
            approvalRequestID: approvalRequestID
        )
    }

    private static func identifier(_ value: Any?) -> String? {
        guard let value = value as? String,
              !value.isEmpty,
              value.utf8.count <= 256,
              !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
        else { return nil }
        return value
    }

    private static func optionalIdentifier(_ value: Any?) -> String? {
        guard value != nil else { return nil }
        return identifier(value)
    }

    private static func unsignedInteger(_ value: Any?) -> UInt64? {
        guard let number = value as? NSNumber,
              CFGetTypeID(number) != CFBooleanGetTypeID(),
              number.int64Value >= 0
        else { return nil }
        return number.uint64Value
    }
}

enum SessionNotificationKey: Hashable {
    case approval(requestID: String)
    case terminal(kind: SessionNotificationKind, sessionID: String, runCount: UInt64)
}

@MainActor
final class MobiusAppDelegate: NSObject, UIApplicationDelegate,
    @preconcurrency UNUserNotificationCenterDelegate {
    private weak var model: AppModel?
    private var pendingDeviceToken: Data?
    private var pendingForegroundNotification: (
        notification: RemoteSessionNotification,
        agentName: String,
        detail: String
    )?
    private var pendingNotificationResponse: RemoteSessionNotification?

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        UNUserNotificationCenter.current().delegate = self
        return true
    }

    func attach(_ model: AppModel) {
        self.model = model
        if let pendingDeviceToken {
            self.pendingDeviceToken = nil
            model.receivedRemoteNotificationDeviceToken(pendingDeviceToken)
        }
        if let pendingForegroundNotification {
            self.pendingForegroundNotification = nil
            model.receivedForegroundRemoteNotification(
                pendingForegroundNotification.notification,
                agentName: pendingForegroundNotification.agentName,
                detail: pendingForegroundNotification.detail
            )
        }
        if let pendingNotificationResponse {
            self.pendingNotificationResponse = nil
            model.openRemoteNotification(pendingNotificationResponse)
        }
    }

    func application(
        _ application: UIApplication,
        didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data
    ) {
        guard let model else {
            pendingDeviceToken = deviceToken
            return
        }
        model.receivedRemoteNotificationDeviceToken(deviceToken)
    }

    func application(
        _ application: UIApplication,
        didFailToRegisterForRemoteNotificationsWithError error: Error
    ) {
        model?.remoteNotificationRegistrationFailed()
    }

    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        let content = notification.request.content
        guard let event = RemoteSessionNotification(
            userInfo: content.userInfo
        ) else { return [] }
        guard let model else {
            pendingForegroundNotification = (event, content.title, content.body)
            return []
        }
        model.receivedForegroundRemoteNotification(
            event,
            agentName: content.title,
            detail: content.body
        )
        return []
    }

    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        guard let event = RemoteSessionNotification(
            userInfo: response.notification.request.content.userInfo
        ) else { return }
        guard let model else {
            pendingNotificationResponse = event
            return
        }
        model.openRemoteNotification(event)
    }
}

extension AppModel {
    func setNotificationsEnabled(_ enabled: Bool) async {
        guard enabled != notificationsEnabled, !isUpdatingNotifications else { return }
        notificationsEnabled = enabled
        settingsDefaults.set(enabled, forKey: notificationsEnabledKey)
        notificationError = nil
        isUpdatingNotifications = true
        defer { isUpdatingNotifications = false }

        if enabled {
            pushTokenRemovalPending = false
            settingsDefaults.set(false, forKey: pushTokenRemovalPendingKey)
            await refreshRemoteNotificationRegistration(opensSettingsWhenDenied: true)
        } else {
            pushTokenRemovalPending = true
            settingsDefaults.set(true, forKey: pushTokenRemovalPendingKey)
            stopRemoteNotifications()
            await removeCloudPushInstallation(reportsErrors: true)
        }
    }

    func cloudAuthenticationDidChange() async {
        await refreshRemoteNotificationRegistration()
    }

    func refreshRemoteNotificationRegistration(
        opensSettingsWhenDenied: Bool = false
    ) async {
        guard notificationsEnabled else {
            stopRemoteNotifications()
            if pushTokenRemovalPending {
                await removeCloudPushInstallation(reportsErrors: false)
            }
            return
        }
        guard cloudSession != nil else {
            stopRemoteNotifications(forgetsCloudInstallation: true)
            return
        }

        switch await remoteNotifications.authorization() {
        case .notDetermined:
            do {
                guard try await remoteNotifications.requestAuthorization() else {
                    notificationError = localizedString("Notifications are off in Settings.")
                    if opensSettingsWhenDenied { remoteNotifications.openSettings() }
                    return
                }
                remoteNotifications.register()
            } catch {
                notificationError = localizedString(
                    "Notifications couldn’t be enabled. Try again."
                )
            }
        case .denied:
            notificationError = localizedString("Notifications are off in Settings.")
            if opensSettingsWhenDenied { remoteNotifications.openSettings() }
        case .authorized:
            notificationError = nil
            remoteNotifications.register()
        }
    }

    func receivedRemoteNotificationDeviceToken(_ deviceToken: Data) {
        let token = deviceToken.map { String(format: "%02x", $0) }.joined()
        guard !token.isEmpty else { return }
        remoteNotificationDeviceToken = token
        guard notificationsEnabled, !pushTokenRemovalPending else { return }
        let previousRegistration = remoteNotificationRegistrationTask
        remoteNotificationRegistrationTask = Task { [weak self] in
            await previousRegistration?.value
            await self?.registerCloudPushToken(token)
        }
    }

    func remoteNotificationRegistrationFailed() {
        guard notificationsEnabled else { return }
        notificationError = localizedString("Notifications couldn’t be enabled. Try again.")
    }

    func receivedForegroundRemoteNotification(
        _ notification: RemoteSessionNotification,
        agentName: String? = nil,
        detail: String? = nil
    ) {
        guard notificationsEnabled,
              cloudSession != nil,
              rememberRemoteNotification(notification.eventID),
              !catalogAlreadyIncludes(notification)
        else { return }
        presentSessionNotification(
            notification.kind,
            sessionID: notification.sessionID,
            runCount: notification.runCount,
            approvalRequestID: notification.approvalRequestID,
            agentName: agentName,
            detail: notification.kind == .completed ? detail : nil
        )
    }

    func openRemoteNotification(_ notification: RemoteSessionNotification) {
        guard notificationsEnabled, cloudSession != nil else { return }
        _ = rememberRemoteNotification(notification.eventID)
        pendingRemoteNotification = notification
        _ = openPendingRemoteNotification()
    }

    @discardableResult
    func openPendingRemoteNotification() -> Bool {
        guard let notification = pendingRemoteNotification,
              let cloudGateway = mobiusCloudGateway
        else { return false }
        guard selectedAccountID == cloudGateway.id else {
            connect(to: cloudGateway)
            return false
        }
        guard connectionState.isReady,
              canOpenSession || selectedSessionID == notification.sessionID
        else { return false }
        if notification.kind == .awaitingApproval,
           let requestID = notification.approvalRequestID,
           let approval = backgroundApprovals.first(where: {
               $0.sessionId == notification.sessionID && $0.requestId == requestID
           }) {
            pendingRemoteNotification = nil
            prepareToOpenNotifiedSession()
            resumeBotSession(botID: approval.botId, sessionID: approval.sessionId)
            return true
        }
        guard sessions.contains(where: { $0.sessionId == notification.sessionID }) else {
            return false
        }
        pendingRemoteNotification = nil
        prepareToOpenNotifiedSession()
        openChat(notification.sessionID)
        return true
    }

    func openNotifiedSession(_ sessionID: String) {
        prepareToOpenNotifiedSession()
        if let approval = backgroundApproval(forSessionID: sessionID) {
            resumeBotSession(botID: approval.botId, sessionID: approval.sessionId)
        } else if sessions.contains(where: { $0.sessionId == sessionID }) {
            openChat(sessionID)
        }
    }

    func presentSessionNotification(
        _ kind: SessionNotificationKind,
        sessionID: String,
        runCount: UInt64? = nil,
        approvalRequestID: String? = nil,
        agentName: String? = nil,
        detail: String? = nil,
        canRefineCompletion: Bool = false
    ) {
        let completionPreview = detail.map {
            $0.split(whereSeparator: { $0.isWhitespace }).joined(separator: " ")
        }.flatMap { $0.isEmpty ? nil : $0 }
        let remoteAgentName = agentName.map {
            $0.split(whereSeparator: { $0.isWhitespace }).joined(separator: " ")
        }.flatMap { $0.isEmpty ? nil : $0 }
        let botName = bot(forSessionID: sessionID)?.name
            ?? remoteAgentName
            ?? localizedString("Bot")
        let finished = localizedString("Finished.")
        let completionMessage = "\(botName): \(completionPreview ?? finished)"
        let refinesCompletedNotification = canRefineCompletion
            && kind == .completed
            && completionPreview != nil
            && toast?.tone == .success
            && toast?.sessionID == sessionID
            && toast?.message != completionMessage
        if let key = sessionNotificationKey(
            kind: kind,
            sessionID: sessionID,
            runCount: runCount,
            approvalRequestID: approvalRequestID
        ), !rememberSessionNotification(key), !refinesCompletedNotification {
            return
        }
        let isHiddenApproval = kind == .awaitingApproval
            && !sessions.contains(where: { $0.sessionId == sessionID })
        let title = backgroundApproval(forSessionID: sessionID) != nil || isHiddenApproval
            ? botName
            : sessionTitle(sessionID)
        let isActiveChat = selectedSessionID == sessionID && isChatVisible
        switch kind {
        case .awaitingApproval:
            showToast("\(title) needs approval.", tone: .warning, sessionID: sessionID)
        case .completed:
            guard !isActiveChat else { return }
            showToast(
                verbatim: completionMessage,
                tone: .success,
                sessionID: sessionID
            )
        case .aborted:
            guard !isActiveChat else { return }
            if let detail {
                showToast(
                    "\(title) stopped: \(detail).",
                    tone: .warning,
                    sessionID: sessionID
                )
            } else {
                showToast("\(title) stopped.", tone: .warning, sessionID: sessionID)
            }
        case .failed:
            if let detail {
                showToast(
                    "\(title) failed: \(detail).",
                    tone: .error,
                    sessionID: sessionID
                )
            } else {
                showToast("\(title) failed.", tone: .error, sessionID: sessionID)
            }
        }
    }

    func unregisterRemoteNotificationsForCloudSignOut() async throws {
        guard cloudSession != nil else { return }
        pushTokenRemovalPending = true
        settingsDefaults.set(true, forKey: pushTokenRemovalPendingKey)
        await remoteNotificationRegistrationTask?.value
        remoteNotificationRegistrationTask = nil
        try await cloudClient.unregisterPushToken(installationID: pushInstallationID)
    }

    func stopRemoteNotifications(forgetsCloudInstallation: Bool = false) {
        if forgetsCloudInstallation {
            pushTokenRemovalPending = false
            settingsDefaults.set(false, forKey: pushTokenRemovalPendingKey)
        }
        remoteNotifications.unregister()
        remoteNotifications.removeAll()
        remoteNotificationDeviceToken = nil
        pendingRemoteNotification = nil
        remoteNotificationEventIDs.removeAll()
        remoteNotificationEventOrder.removeAll()
        sessionNotificationKeys.removeAll()
        sessionNotificationKeyOrder.removeAll()
        notificationError = nil
    }

    private func registerCloudPushToken(_ token: String) async {
        guard notificationsEnabled,
              !pushTokenRemovalPending,
              let requestedSession = cloudSession
        else { return }
        do {
            try await cloudClient.registerPushToken(
                installationID: pushInstallationID,
                token: token,
                environment: .current
            )
            guard cloudSession == requestedSession else { return }
            guard notificationsEnabled else {
                pushTokenRemovalPending = true
                settingsDefaults.set(true, forKey: pushTokenRemovalPendingKey)
                await removeCloudPushInstallation(reportsErrors: false)
                return
            }
            guard !pushTokenRemovalPending else { return }
            pushTokenRemovalPending = false
            settingsDefaults.set(false, forKey: pushTokenRemovalPendingKey)
            notificationError = nil
        } catch is CancellationError {
            return
        } catch {
            guard notificationsEnabled,
                  !pushTokenRemovalPending,
                  cloudSession == requestedSession
            else { return }
            notificationError = localizedString(
                "möbius Cloud couldn’t update notifications. Try again."
            )
        }
    }

    private func removeCloudPushInstallation(reportsErrors: Bool) async {
        guard let requestedSession = cloudSession else { return }
        do {
            try await cloudClient.unregisterPushToken(installationID: pushInstallationID)
            guard cloudSession == requestedSession else { return }
            pushTokenRemovalPending = false
            settingsDefaults.set(false, forKey: pushTokenRemovalPendingKey)
            notificationError = nil
        } catch is CancellationError {
            return
        } catch {
            guard cloudSession == requestedSession else { return }
            if reportsErrors {
                notificationError = localizedString(
                    "möbius Cloud couldn’t update notifications. Try again."
                )
            }
        }
    }

    private func catalogAlreadyIncludes(_ notification: RemoteSessionNotification) -> Bool {
        switch notification.kind {
        case .awaitingApproval:
            guard let requestID = notification.approvalRequestID else { return false }
            if let approval = backgroundApproval(forSessionID: notification.sessionID) {
                return approval.requestId == requestID
            }
            guard let session = sessions.first(where: {
                $0.sessionId == notification.sessionID
            }) else { return false }
            return session.activity.state == .awaitingApproval
                && session.activity.approvalRequestId == requestID
        case .completed, .aborted, .failed:
            guard let session = sessions.first(where: {
                $0.sessionId == notification.sessionID
            }), let runCount = notification.runCount else { return false }
            return session.executionStats.runCount >= runCount
        }
    }

    private func rememberRemoteNotification(_ eventID: String) -> Bool {
        guard remoteNotificationEventIDs.insert(eventID).inserted else { return false }
        remoteNotificationEventOrder.append(eventID)
        if remoteNotificationEventOrder.count > 64 {
            remoteNotificationEventIDs.remove(remoteNotificationEventOrder.removeFirst())
        }
        return true
    }

    private func sessionNotificationKey(
        kind: SessionNotificationKind,
        sessionID: String,
        runCount: UInt64?,
        approvalRequestID: String?
    ) -> SessionNotificationKey? {
        switch kind {
        case .awaitingApproval:
            approvalRequestID.map { .approval(requestID: $0) }
        case .completed, .aborted, .failed:
            runCount.map { .terminal(kind: kind, sessionID: sessionID, runCount: $0) }
        }
    }

    private func rememberSessionNotification(_ key: SessionNotificationKey) -> Bool {
        guard sessionNotificationKeys.insert(key).inserted else { return false }
        sessionNotificationKeyOrder.append(key)
        if sessionNotificationKeyOrder.count > 64 {
            sessionNotificationKeys.remove(sessionNotificationKeyOrder.removeFirst())
        }
        return true
    }

    private func prepareToOpenNotifiedSession() {
        showsInspector = false
        showsPairing = false
        showsWorkspaceBrowser = false
    }
}
