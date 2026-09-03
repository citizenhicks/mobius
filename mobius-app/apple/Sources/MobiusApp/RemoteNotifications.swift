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

enum RemoteNotification: Equatable {
    case session(
        eventID: String,
        kind: SessionNotificationKind,
        sessionID: String,
        runCount: UInt64?,
        approvalRequestID: String?
    )
    case swarmAttention(eventID: String, swarmID: String, messageID: String)

    var eventID: String {
        switch self {
        case .session(let eventID, _, _, _, _), .swarmAttention(let eventID, _, _): eventID
        }
    }

    init?(userInfo: [AnyHashable: Any]) {
        guard let eventID = Self.identifier(userInfo["eventId"]),
              let rawKind = userInfo["kind"] as? String
        else { return nil }
        if rawKind == "swarm.attention" {
            guard let swarmID = Self.identifier(userInfo["swarmId"]),
                  let messageID = Self.identifier(userInfo["messageId"])
            else { return nil }
            self = .swarmAttention(eventID: eventID, swarmID: swarmID, messageID: messageID)
            return
        }
        guard let kind = SessionNotificationKind(rawValue: rawKind),
              let sessionID = Self.identifier(userInfo["sessionId"])
        else { return nil }
        let runCount = Self.unsignedInteger(userInfo["runCount"])
        let approvalRequestID = Self.optionalIdentifier(userInfo["approvalRequestId"])
        let hasRequiredCursor = switch kind {
        case .awaitingApproval: approvalRequestID != nil
        case .completed, .aborted, .failed: runCount != nil
        }
        guard hasRequiredCursor else { return nil }
        self = .session(
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

enum AppNotificationKey: Hashable {
    case approval(requestID: String)
    case terminal(kind: SessionNotificationKind, sessionID: String, runCount: UInt64)
    case swarmAttention(messageID: String)
}

@MainActor
final class MobiusAppDelegate: NSObject, UIApplicationDelegate,
    @preconcurrency UNUserNotificationCenterDelegate {
    private weak var model: AppModel?
    private var pendingDeviceToken: Data?
    private var pendingForegroundNotification: (
        notification: RemoteNotification,
        agentName: String,
        detail: String
    )?
    private var pendingNotificationResponse: RemoteNotification?

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
        guard let event = RemoteNotification(
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
        guard let event = RemoteNotification(
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
        _ notification: RemoteNotification,
        agentName: String? = nil,
        detail: String? = nil
    ) {
        guard notificationsEnabled,
              cloudSession != nil,
              rememberRemoteNotification(notification.eventID),
              !catalogAlreadyIncludes(notification)
        else { return }
        switch notification {
        case .session(_, let kind, let sessionID, let runCount, let approvalRequestID):
            presentSessionNotification(
                kind,
                sessionID: sessionID,
                runCount: runCount,
                approvalRequestID: approvalRequestID,
                agentName: agentName,
                detail: kind == .completed ? detail : nil
            )
        case .swarmAttention(_, let swarmID, let messageID):
            presentSwarmAttention(
                swarmID: swarmID,
                messageID: messageID,
                agentName: agentName,
                text: detail
            )
        }
    }

    func openRemoteNotification(_ notification: RemoteNotification) {
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
        guard connectionState.isReady else { return false }
        switch notification {
        case .swarmAttention(_, let swarmID, _):
            guard swarms.contains(where: { $0.id == swarmID }) else { return false }
            pendingRemoteNotification = nil
            prepareToOpenNotification()
            openSwarmChat(swarmID)
            return true
        case .session(_, let kind, let sessionID, _, let requestID):
            guard canOpenSession || selectedSessionID == sessionID else { return false }
            if kind == .awaitingApproval,
               let requestID,
               let approval = backgroundApprovals.first(where: {
                   $0.sessionId == sessionID && $0.requestId == requestID
               }) {
                pendingRemoteNotification = nil
                prepareToOpenNotification()
                resumeBotSession(botID: approval.botId, sessionID: approval.sessionId)
                return true
            }
            guard sessions.contains(where: { $0.sessionId == sessionID }) else { return false }
            pendingRemoteNotification = nil
            prepareToOpenNotification()
            openChat(sessionID)
            return true
        }
    }

    func openNotificationTarget(_ target: AppNotificationTarget) {
        prepareToOpenNotification()
        switch target {
        case .session(let sessionID):
            if let approval = backgroundApproval(forSessionID: sessionID) {
                resumeBotSession(botID: approval.botId, sessionID: approval.sessionId)
            } else if sessions.contains(where: { $0.sessionId == sessionID }) {
                openChat(sessionID)
            }
        case .swarm(let swarmID, _):
            openSwarmChat(swarmID)
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
            && toast?.target == .session(sessionID)
            && toast?.message != completionMessage
        if let key = sessionNotificationKey(
            kind: kind,
            sessionID: sessionID,
            runCount: runCount,
            approvalRequestID: approvalRequestID
        ), !rememberNotification(key), !refinesCompletedNotification {
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
            showToast("\(title) needs approval.", tone: .warning, target: .session(sessionID))
        case .completed:
            guard !isActiveChat else { return }
            showToast(
                verbatim: completionMessage,
                tone: .success,
                target: .session(sessionID)
            )
        case .aborted:
            guard !isActiveChat else { return }
            if let detail {
                showToast(
                    "\(title) stopped: \(detail).",
                    tone: .warning,
                    target: .session(sessionID)
                )
            } else {
                showToast("\(title) stopped.", tone: .warning, target: .session(sessionID))
            }
        case .failed:
            if let detail {
                showToast(
                    "\(title) failed: \(detail).",
                    tone: .error,
                    target: .session(sessionID)
                )
            } else {
                showToast("\(title) failed.", tone: .error, target: .session(sessionID))
            }
        }
    }

    func presentSwarmAttention(_ attention: SwarmAttention) {
        presentSwarmAttention(
            swarmID: attention.swarmId,
            messageID: attention.messageId,
            agentName: bots.first { $0.id == attention.botId }?.name,
            text: attention.text
        )
    }

    private func presentSwarmAttention(
        swarmID: String,
        messageID: String,
        agentName: String?,
        text: String?
    ) {
        guard rememberNotification(.swarmAttention(messageID: messageID)) else { return }
        let name = agentName.map(collapsedNotificationText)
            .flatMap { $0.isEmpty ? nil : $0 }
            ?? localizedString("Bot")
        let detail = text.map(collapsedNotificationText)
            .flatMap { $0.isEmpty ? nil : $0 }
            ?? localizedString("Needs attention")
        showToast(
            verbatim: "\(name): \(detail)",
            tone: .warning,
            target: .swarm(swarmID: swarmID, messageID: messageID)
        )
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
        notificationKeys.removeAll()
        notificationKeyOrder.removeAll()
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

    private func catalogAlreadyIncludes(_ notification: RemoteNotification) -> Bool {
        switch notification {
        case .swarmAttention(_, _, let messageID):
            return swarmAttentions.contains { $0.messageId == messageID }
        case .session(_, .awaitingApproval, let sessionID, _, let requestID):
            guard let requestID else { return false }
            if let approval = backgroundApproval(forSessionID: sessionID) {
                return approval.requestId == requestID
            }
            guard let session = sessions.first(where: {
                $0.sessionId == sessionID
            }) else { return false }
            return session.activity.state == .awaitingApproval
                && session.activity.approvalRequestId == requestID
        case .session(_, .completed, let sessionID, let runCount, _),
             .session(_, .aborted, let sessionID, let runCount, _),
             .session(_, .failed, let sessionID, let runCount, _):
            guard let session = sessions.first(where: {
                $0.sessionId == sessionID
            }), let runCount else { return false }
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
    ) -> AppNotificationKey? {
        switch kind {
        case .awaitingApproval:
            approvalRequestID.map { .approval(requestID: $0) }
        case .completed, .aborted, .failed:
            runCount.map { .terminal(kind: kind, sessionID: sessionID, runCount: $0) }
        }
    }

    private func rememberNotification(_ key: AppNotificationKey) -> Bool {
        guard notificationKeys.insert(key).inserted else { return false }
        notificationKeyOrder.append(key)
        if notificationKeyOrder.count > 64 {
            notificationKeys.remove(notificationKeyOrder.removeFirst())
        }
        return true
    }

    private func prepareToOpenNotification() {
        showsInspector = false
        showsPairing = false
        showsWorkspaceBrowser = false
    }

    private func collapsedNotificationText(_ text: String) -> String {
        text.split(whereSeparator: { $0.isWhitespace }).joined(separator: " ")
    }
}
