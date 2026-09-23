import AVFoundation
@preconcurrency import CallKit
import Foundation

enum NewVoiceChatIntent: Equatable {
    case selectingBot
    case openingSession(String)
}

struct RealtimeVoiceCall: Equatable {
    let requestID: String
    let sessionID: String
    var systemID = UUID()
    var voiceID: String?
}

extension AppModel {
    var selectedRouteSupportsRealtimeVoice: Bool {
        let route =
            chat.selectedSessionID == nil
            ? modelRoute(for: selectedBot?.config.config ?? botDefaultsSnapshot?.config)
            : chat.selectedModelRoute
        return supportsRealtimeVoice(route)
    }

    var newChatRouteSupportsRealtimeVoice: Bool {
        supportsRealtimeVoice(
            modelRoute(for: newChatBot?.config.config ?? botDefaultsSnapshot?.config))
    }

    private func supportsRealtimeVoice(_ route: String?) -> Bool {
        guard let route,
            modelChoices.first(where: { $0.route == route })?.supports(.realtimeVoice) == true,
            let instanceID = modelProviders[route],
            let instance = providerInstances.first(where: { $0.instance == instanceID }),
            instance.configured
        else { return false }
        return providerStatus(forInstance: instanceID)?.realtimeVoices.isEmpty == false
    }

    var canStartRealtimeVoice: Bool {
        gateway.connectionState.isReady && selectedRouteSupportsRealtimeVoice
            && (chat.selectedSessionID != nil || chat.pendingNewChatBotID != nil)
    }

    func openNewVoiceChat() {
        guard canCreateSession, newChatRouteSupportsRealtimeVoice else { return }
        openNewSession()
        newVoiceChatIntent = .selectingBot
        createPendingVoiceChat()
    }

    func cancelVoiceChatIntent() {
        newVoiceChatIntent = nil
    }

    func createPendingVoiceChat() {
        guard newVoiceChatIntent == .selectingBot else { return }
        guard chat.pendingNewChatBotID != nil else { return }
        guard selectedRouteSupportsRealtimeVoice else {
            cancelVoiceChatIntent()
            showToast("Voice is not available for this Bot's provider.", tone: .warning)
            return
        }
        if let requestID = createPendingSession() {
            newVoiceChatIntent = .openingSession(requestID)
        }
    }

    func completePendingVoiceChat(requestID: String?) {
        guard let requestID, newVoiceChatIntent == .openingSession(requestID) else { return }
        cancelVoiceChatIntent()
        chat.startRealtimeVoice(
            eligible: selectedRouteSupportsRealtimeVoice,
            label: selectedBot?.name ?? "möbius"
        )
    }

    func startRealtimeVoice() {
        guard canStartRealtimeVoice else { return }
        guard chat.selectedSessionID != nil else {
            newVoiceChatIntent = .selectingBot
            createPendingVoiceChat()
            return
        }
        chat.startRealtimeVoice(
            eligible: selectedRouteSupportsRealtimeVoice,
            label: selectedBot?.name ?? "möbius"
        )
    }

}

extension ChatSessionModel {
    func startRealtimeVoice(eligible: Bool, label: String = "möbius") {
        guard gateway.connectionState.isReady, eligible, realtimeVoiceCall == nil else { return }
        guard let sessionID = selectedSessionID else { return }
        let requestID = gatewayRequestID("voice")
        let call = RealtimeVoiceCall(
            requestID: requestID, sessionID: sessionID
        )
        realtimeVoiceCall = call
        let voice = RealtimeVoiceSession { [weak self] message in
            guard self?.realtimeVoiceCall?.requestID == requestID else { return }
            self?.stopRealtimeVoice(systemReason: .failed)
            self?.showToast(message, tone: .error)
        }
        realtimeVoice = voice
        messageSpeaker.stop()
        dismissComposerFocus()
        realtimeVoiceTask = Task { [weak self] in
            do {
                await self?.dictation.stopAndReleaseAudioSession()
                guard self?.realtimeVoiceCall?.requestID == requestID else { return }
                try await RealtimeCallProvider.shared.start(
                    id: call.systemID,
                    handle: label,
                    end: { [weak self] in
                        guard self?.realtimeVoiceCall?.systemID == call.systemID else { return }
                        self?.stopRealtimeVoice(reportSystemCall: false)
                    },
                    mute: { [weak self] muted in
                        guard self?.realtimeVoiceCall?.systemID == call.systemID,
                            self?.realtimeVoice === voice
                        else { return }
                        voice.isMuted = muted
                    }
                )
                guard self?.realtimeVoiceCall?.requestID == requestID else { return }
                let offer = try await voice.offer()
                guard self?.realtimeVoiceCall?.requestID == requestID else { return }
                try await self?.gateway.send(
                    .startRealtimeVoice(
                        requestID: requestID, sessionID: sessionID, offerSDP: offer
                    ))
                // A canceled request may still receive an answer; the response handler ends it.
                try await Task.sleep(for: .seconds(45))
                guard let self,
                    self.realtimeVoiceCall?.requestID == requestID,
                    self.realtimeVoiceCall?.voiceID == nil
                else { return }
                self.stopRealtimeVoice()
                self.showToast("Voice could not connect. Try again.", tone: .error)
            } catch is CancellationError {
                return
            } catch {
                guard let self, self.realtimeVoiceCall?.requestID == requestID else { return }
                self.stopRealtimeVoice(systemReason: .failed)
                self.showToast(verbatim: self.localizedErrorDescription(error), tone: .error)
            }
        }
    }

    func stopRealtimeVoice(
        notifyGateway: Bool = true,
        systemReason: CXCallEndedReason = .remoteEnded,
        reportSystemCall: Bool = true
    ) {
        let call = realtimeVoiceCall
        realtimeVoiceCall = nil
        realtimeVoiceTask?.cancel()
        realtimeVoiceTask = nil
        realtimeVoice.close()
        if reportSystemCall, let call {
            RealtimeCallProvider.shared.end(id: call.systemID, reason: systemReason)
        }
        if notifyGateway, let call {
            gateway.transmit(
                .endRealtimeVoice(
                    sessionID: call.sessionID, voiceID: call.voiceID ?? call.requestID
                ))
        }
    }

    func handleRealtimeVoiceEnvelope(_ envelope: GatewayEnvelope, eligible: Bool) {
        switch envelope {
        case .realtimeVoiceStarted(let requestID, let sessionID, let voiceID, let answerSDP):
            guard realtimeVoiceCall?.requestID == requestID,
                realtimeVoiceCall?.sessionID == sessionID,
                selectedSessionID == sessionID,
                eligible
            else {
                gateway.transmit(.endRealtimeVoice(sessionID: sessionID, voiceID: voiceID))
                return
            }
            startVoiceChatTitle(sessionID: sessionID, requestID: requestID)
            realtimeVoiceCall?.voiceID = voiceID
            RealtimeCallProvider.shared.connected(id: realtimeVoiceCall?.systemID)
            realtimeVoiceTask?.cancel()
            let voice = realtimeVoice
            realtimeVoiceTask = Task { [weak self] in
                guard self?.realtimeVoiceCall?.requestID == requestID else { return }
                do {
                    try await voice.accept(answer: answerSDP)
                } catch {
                    guard let self, self.realtimeVoiceCall?.requestID == requestID else { return }
                    self.stopRealtimeVoice(systemReason: .failed)
                    self.showToast(verbatim: self.localizedErrorDescription(error), tone: .error)
                }
            }
        case .realtimeVoiceEnded(let sessionID, let voiceID, let reason):
            guard realtimeVoiceCall?.sessionID == sessionID,
                realtimeVoiceCall?.voiceID == voiceID
            else { return }
            stopRealtimeVoice(notifyGateway: false)
            if let reason { showToast(verbatim: reason, tone: .warning) }
        case .realtimeVoiceFailed(let requestID, let sessionID, let message):
            guard realtimeVoiceCall?.requestID == requestID,
                realtimeVoiceCall?.sessionID == sessionID
            else { return }
            stopRealtimeVoice(notifyGateway: false)
            showToast(verbatim: message, tone: .error)
        default:
            break
        }
    }

    func speakMessage(_ markdown: String) {
        stopRealtimeVoice()
        messageSpeaker.speak(markdown)
    }

    func setRealtimeVoiceMuted(_ muted: Bool) {
        guard let call = realtimeVoiceCall else { return }
        Task { [weak self] in
            do {
                try await RealtimeCallProvider.shared.setMuted(id: call.systemID, muted: muted)
            } catch {
                guard let self, self.realtimeVoiceCall?.systemID == call.systemID else { return }
                self.showToast("Microphone control failed. Try again.", tone: .error)
            }
        }
    }
}

@MainActor
private final class RealtimeCallProvider: NSObject, @preconcurrency CXProviderDelegate {
    static let shared = RealtimeCallProvider()

    private struct ActiveCall {
        let id: UUID
        let end: @MainActor () -> Void
        let mute: @MainActor (Bool) -> Void
    }

    private let provider: CXProvider
    private let controller = CXCallController()
    private var activeCall: ActiveCall?

    private override init() {
        let configuration = CXProviderConfiguration()
        configuration.maximumCallGroups = 1
        configuration.maximumCallsPerCallGroup = 1
        configuration.includesCallsInRecents = false
        configuration.supportedHandleTypes = [.generic]
        provider = CXProvider(configuration: configuration)
        super.init()
        provider.setDelegate(self, queue: nil)
    }

    func start(
        id: UUID,
        handle: String,
        end: @escaping @MainActor () -> Void,
        mute: @escaping @MainActor (Bool) -> Void
    ) async throws {
        guard activeCall == nil else { throw RealtimeVoiceSession.VoiceError.connection }
        try RealtimeVoiceSession.prepareSystemCallAudio()
        activeCall = ActiveCall(id: id, end: end, mute: mute)
        let action = CXStartCallAction(
            call: id,
            handle: CXHandle(type: .generic, value: handle)
        )
        do {
            try await controller.request(CXTransaction(action: action))
        } catch {
            if activeCall?.id == id { activeCall = nil }
            throw error
        }
    }

    func connected(id: UUID?) {
        guard let id, activeCall?.id == id else { return }
        provider.reportOutgoingCall(with: id, connectedAt: nil)
    }

    func end(id: UUID, reason: CXCallEndedReason) {
        guard activeCall?.id == id else { return }
        activeCall = nil
        provider.reportCall(with: id, endedAt: nil, reason: reason)
    }

    func setMuted(id: UUID, muted: Bool) async throws {
        guard activeCall?.id == id else { throw RealtimeVoiceSession.VoiceError.connection }
        try await controller.request(
            CXTransaction(action: CXSetMutedCallAction(call: id, muted: muted))
        )
    }

    func providerDidReset(_ provider: CXProvider) {
        let call = activeCall
        activeCall = nil
        call?.end()
    }

    func provider(_ provider: CXProvider, perform action: CXStartCallAction) {
        guard activeCall?.id == action.callUUID else {
            action.fail()
            return
        }
        provider.reportOutgoingCall(with: action.callUUID, startedConnectingAt: nil)
        action.fulfill()
    }

    func provider(_ provider: CXProvider, perform action: CXEndCallAction) {
        guard activeCall?.id == action.callUUID else {
            action.fail()
            return
        }
        let call = activeCall
        activeCall = nil
        action.fulfill()
        call?.end()
    }

    func provider(_ provider: CXProvider, perform action: CXSetMutedCallAction) {
        guard let call = activeCall, call.id == action.callUUID else {
            action.fail()
            return
        }
        call.mute(action.isMuted)
        action.fulfill()
    }

    func provider(_ provider: CXProvider, didActivate audioSession: AVAudioSession) {
        RealtimeVoiceSession.systemCallAudioDidActivate(audioSession)
    }

    func provider(_ provider: CXProvider, didDeactivate audioSession: AVAudioSession) {
        RealtimeVoiceSession.systemCallAudioDidDeactivate(audioSession)
    }
}
