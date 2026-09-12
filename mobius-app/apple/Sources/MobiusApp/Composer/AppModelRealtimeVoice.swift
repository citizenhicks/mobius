import Foundation

enum NewVoiceChatIntent: Equatable {
    case selectingWorkspace
    case selectingBot
    case openingSession(String)
}

struct RealtimeVoiceCall: Equatable {
    let requestID: String
    let sessionID: String
    var voiceID: String?
}

extension AppModel {
    var selectedRouteSupportsRealtimeVoice: Bool {
        guard !selectedChatIsGroup else { return false }
        let route =
            chat.selectedSessionID == nil
            ? modelRoute(for: selectedBot?.config.config ?? botDefaultsSnapshot?.config)
            : chat.selectedModelRoute
        guard let route,
            modelChoices.first(where: { $0.route == route })?.supportsRealtimeVoice == true,
            let instanceID = modelProviders[route],
            let instance = providerInstances.first(where: { $0.instance == instanceID }),
            instance.configured
        else { return false }
        return providerStatus(forInstance: instanceID)?.realtimeVoices.isEmpty == false
    }

    var canStartRealtimeVoice: Bool {
        gateway.connectionState.isReady && selectedRouteSupportsRealtimeVoice
            && (chat.selectedSessionID != nil || !chat.pendingNewChatBotIDs.isEmpty)
    }

    var composerUsesPrimaryVoice: Bool {
        selectedRouteSupportsRealtimeVoice && chat.selectedSessionID == nil
            && chat.composer.isEmpty && !chat.hasComposerContext
    }

    func openNewVoiceChat() {
        guard canCreateSession, selectedRouteSupportsRealtimeVoice else { return }
        openNewSession()
        newVoiceChatIntent = .selectingWorkspace
        openWorkspaceBrowser()
    }

    func cancelVoiceChatIntent() {
        newVoiceChatIntent = nil
    }

    func createPendingVoiceChat() {
        guard newVoiceChatIntent == .selectingBot else { return }
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
        chat.startRealtimeVoice(eligible: selectedRouteSupportsRealtimeVoice)
    }

    func startRealtimeVoice() {
        guard canStartRealtimeVoice else { return }
        guard chat.selectedSessionID != nil else {
            newVoiceChatIntent = .selectingBot
            createPendingVoiceChat()
            return
        }
        chat.startRealtimeVoice(eligible: selectedRouteSupportsRealtimeVoice)
    }

}

extension ChatSessionModel {
    func startRealtimeVoice(eligible: Bool) {
        guard gateway.connectionState.isReady, eligible, realtimeVoiceCall == nil else { return }
        guard let sessionID = selectedSessionID else { return }
        let requestID = UUID().uuidString.lowercased()
        realtimeVoiceCall = RealtimeVoiceCall(
            requestID: requestID, sessionID: sessionID
        )
        let voice = RealtimeVoiceSession { [weak self] message in
            guard self?.realtimeVoiceCall?.requestID == requestID else { return }
            self?.stopRealtimeVoice()
            self?.showToast(verbatim: message, tone: .error)
        }
        realtimeVoice = voice
        messageSpeaker.stop()
        dismissComposerFocus()
        realtimeVoiceTask = Task { [weak self] in
            do {
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
                self.stopRealtimeVoice()
                self.showToast(verbatim: self.localizedErrorDescription(error), tone: .error)
            }
        }
    }

    func stopRealtimeVoice(notifyGateway: Bool = true) {
        let call = realtimeVoiceCall
        realtimeVoiceCall = nil
        realtimeVoiceTask?.cancel()
        realtimeVoiceTask = nil
        realtimeVoice.close()
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
            realtimeVoiceCall?.voiceID = voiceID
            realtimeVoiceTask?.cancel()
            let voice = realtimeVoice
            realtimeVoiceTask = Task { [weak self] in
                guard self?.realtimeVoiceCall?.requestID == requestID else { return }
                do {
                    try await voice.accept(answer: answerSDP)
                } catch {
                    guard let self, self.realtimeVoiceCall?.requestID == requestID else { return }
                    self.stopRealtimeVoice()
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
}
