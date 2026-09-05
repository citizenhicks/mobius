import Foundation
import XCTest
@preconcurrency import AVFoundation
@preconcurrency import WebRTC

@MainActor
extension AppModelTests {
    private func voiceModel(recorder: GatewayRequestRecorder = GatewayRequestRecorder()) throws -> AppModel {
        let model = try model { await recorder.record($0) }
        let config = composition()
        var status = providerStatus(for: config.provider)
        status.supportsRealtimeVoice = true
        model.providerStatuses = [status]
        model.providerInstances = [ProviderInstance(
            label: "Work", tint: .blue, configured: true, selection: config.provider,
            modelIds: [], reasoningEfforts: []
        )]
        model.modelChoices = [ModelChoice(
            route: "voice-route", group: "Work", model: config.provider.model,
            reasoningEffort: config.provider.reasoningEffort, contextWindow: nil,
            supportsImageInput: true, supportsRealtimeVoice: true, toolDiscovery: .native
        )]
        model.modelProviders = ["voice-route": config.provider.instance]
        model.botDefaultsSnapshot = VersionedAgentConfig(revision: 1, config: config)
        model.selectedModelRoute = "voice-route"
        model.connectionState = .ready
        return model
    }

    func testRealtimeEligibilityUsesSelectedRouteAndConfiguredInstance() throws {
        let model = try voiceModel()
        XCTAssertTrue(model.selectedRouteSupportsRealtimeVoice)
        model.selectedSessionID = "chat-1"
        XCTAssertTrue(model.selectedRouteSupportsRealtimeVoice)
        model.providerInstances[0].configured = false
        XCTAssertFalse(model.selectedRouteSupportsRealtimeVoice)
        model.providerInstances[0].configured = true
        model.providerStatuses[0].supportsRealtimeVoice = false
        XCTAssertFalse(model.selectedRouteSupportsRealtimeVoice)
        model.providerStatuses[0].supportsRealtimeVoice = true
        model.selectedModelRoute = "unknown"
        XCTAssertFalse(model.selectedRouteSupportsRealtimeVoice)
        model.selectedModelRoute = "voice-route"
        model.modelProviders["voice-route"] = "other-instance"
        XCTAssertFalse(model.selectedRouteSupportsRealtimeVoice)
    }

    func testNewVoiceChatWaitsForWorkspaceBotAndSessionReplay() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try voiceModel(recorder: recorder)
        let first = bot()
        let second = bot(id: "bot-2", handle: "reviewer", name: "Reviewer")
        model.bots = [first, second]
        model.openNewVoiceChat()
        XCTAssertTrue(model.showsWorkspaceBrowser)
        XCTAssertEqual(model.newVoiceChatIntent, .selectingWorkspace)
        XCTAssertNil(model.realtimeVoiceCall)
        model.chooseWorkspace("/srv/project")
        XCTAssertEqual(model.newVoiceChatIntent, .selectingBot)
        model.selectBotForNewChat(second)
        let request = await recorder.firstRequest(after: 0) {
            if case .createSession = $0 { true } else { false }
        }
        guard case .createSession(let requestID, "/srv/project", "bot-2") = try XCTUnwrap(request) else {
            return XCTFail("Expected the selected workspace and Bot")
        }
        XCTAssertEqual(model.newVoiceChatIntent, .openingSession(requestID))
        XCTAssertNil(model.realtimeVoiceCall)
        model.completePendingVoiceChat(requestID: "unrelated-replay")
        XCTAssertEqual(model.newVoiceChatIntent, .openingSession(requestID))
        model.cancelVoiceChatIntent()
        model.completePendingVoiceChat(requestID: requestID)
        XCTAssertNil(model.realtimeVoiceCall)
        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains { if case .startRealtimeVoice = $0 { true } else { false } })
    }

    func testCanceledVoiceStartAndLateAnswerEndOnlyThatCall() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try voiceModel(recorder: recorder)
        model.selectedSessionID = "chat-1"
        model.realtimeVoiceCall = RealtimeVoiceCall(
            requestID: "old", sessionID: "chat-1"
        )
        model.stopRealtimeVoice()
        XCTAssertNil(model.realtimeVoiceCall)
        let ended = await recorder.firstRequest(after: 0) {
            if case .endRealtimeVoice("chat-1", "old") = $0 { true } else { false }
        }
        XCTAssertNotNil(ended)
        model.realtimeVoiceCall = RealtimeVoiceCall(
            requestID: "new", sessionID: "chat-1"
        )
        model.handle(.realtimeVoiceStarted(
            requestID: "old", sessionID: "chat-1", voiceID: "old", answerSDP: "late answer"
        ))
        model.handle(.realtimeVoiceFailed(requestID: "old", sessionID: "chat-1", message: "late error"))
        model.handle(.realtimeVoiceEnded(sessionID: "chat-1", voiceID: "old", reason: nil))
        XCTAssertEqual(model.realtimeVoiceCall?.requestID, "new")
        model.handle(.realtimeVoiceFailed(requestID: "new", sessionID: "chat-1", message: "start failed"))
        XCTAssertNil(model.realtimeVoiceCall)
    }

    func testBackgroundCancelsPendingVoiceAndEndsBeforeDisconnect() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try voiceModel(recorder: recorder)
        model.selectedSessionID = "chat-1"
        model.startRealtimeVoice()
        let call = try XCTUnwrap(model.realtimeVoiceCall)
        let startup = try XCTUnwrap(model.realtimeVoiceTask)
        let oldGeneration = model.connectionGeneration
        model.newVoiceChatIntent = .openingSession("pending-session")
        // Follow the actual scene lifecycle synchronously, before mic permission can run.
        model.setSceneActive(false)
        model.appDidEnterBackground()
        XCTAssertNil(model.realtimeVoiceCall)
        XCTAssertNil(model.realtimeVoiceTask)
        XCTAssertNil(model.newVoiceChatIntent)
        XCTAssertTrue(startup.isCancelled)
        XCTAssertFalse(model.realtimeVoice.isConnected)
        XCTAssertNotEqual(model.connectionGeneration, oldGeneration)
        let ended = await recorder.firstRequest(after: 0) {
            if case .endRealtimeVoice("chat-1", call.requestID) = $0 { true } else { false }
        }
        XCTAssertNotNil(ended)
        let requests = await recorder.requests()
        XCTAssertFalse(requests.contains { if case .startRealtimeVoice = $0 { true } else { false } })
    }

    func testVoiceClosesOnSessionRouteAndBackgroundChanges() throws {
        let model = try voiceModel()
        model.selectedSessionID = "chat-1"
        let call = RealtimeVoiceCall(requestID: "voice", sessionID: "chat-1")
        model.realtimeVoiceCall = call
        model.selectedSessionID = "chat-2"
        XCTAssertNil(model.realtimeVoiceCall)
        model.realtimeVoiceCall = call
        model.selectedModelRoute = "other-route"
        XCTAssertNil(model.realtimeVoiceCall)
        model.realtimeVoiceCall = call
        model.newVoiceChatIntent = .selectingWorkspace
        model.appDidEnterBackground()
        XCTAssertNil(model.realtimeVoiceCall)
        XCTAssertNil(model.newVoiceChatIntent)
        XCTAssertFalse(model.realtimeVoice.isConnected)
    }
}

@MainActor
extension AppModelTests {
    func testNewVoiceChatOpensMicrophoneOnlyAfterCreatedSessionReplay() throws {
        let model = try voiceModel()
        model.openNewVoiceChat()
        model.chooseWorkspace("/srv/mobius")
        guard case .openingSession(let requestID) = model.newVoiceChatIntent else {
            return XCTFail("Expected session creation after workspace and the only Bot were selected")
        }
        model.handle(.sessionOpened(
            requestID: requestID,
            payload: sessionReady(latestSequence: 0, modelRoute: "voice-route")
        ))
        XCTAssertNil(model.realtimeVoiceCall)
        model.handle(.sessionReplayComplete(requestID: requestID, sessionID: "chat-1"))
        XCTAssertEqual(model.realtimeVoiceCall?.sessionID, "chat-1")
        XCTAssertNil(model.newVoiceChatIntent)
        // Cancel before the asynchronous permission request runs.
        model.stopRealtimeVoice()
    }

    func testCancelingWorkspaceSelectionDiscardsVoiceIntent() throws {
        let model = try voiceModel()
        model.openNewVoiceChat()
        model.showsWorkspaceBrowser = false
        XCTAssertNil(model.newVoiceChatIntent)
        model.chooseWorkspace("/srv/mobius")
        XCTAssertNil(model.sessionRequestID)
        XCTAssertNil(model.realtimeVoiceCall)
    }
}


@MainActor
extension AppModelTests {
    func testOldAudioInterruptionCannotEndReplacementVoiceCall() async throws {
        let model = try voiceModel()
        model.selectedSessionID = "chat-1"
        model.startRealtimeVoice()
        // No permission request or capture: exercise only ownership and delegate delivery.
        model.realtimeVoiceTask?.cancel()
        let oldVoice = model.realtimeVoice
        oldVoice.audioSessionDidBeginInterruption(RTCAudioSession.sharedInstance())
        model.stopRealtimeVoice()
        model.startRealtimeVoice()
        model.realtimeVoiceTask?.cancel()
        let newVoice = model.realtimeVoice
        let currentRequestID = model.realtimeVoiceCall?.requestID
        XCTAssertFalse(oldVoice === newVoice)
        await Task.yield()
        XCTAssertEqual(model.realtimeVoiceCall?.requestID, currentRequestID)
        XCTAssertNil(model.toast)

        newVoice.audioSessionDidBeginInterruption(RTCAudioSession.sharedInstance())
        let currentInterruptionHandled = await eventually { model.realtimeVoiceCall == nil }
        XCTAssertTrue(currentInterruptionHandled)
    }

    func testReadAloudStopCancelsDeferredAudioPreparation() async {
        let synthesizer = RecordingSpeechSynthesizer()
        let speaker = MessageSpeaker(synthesizer: synthesizer)
        let preparing = expectation(description: "Old speech is waiting for dictation")
        let resumed = expectation(description: "Old preparation finished")
        let spoken = expectation(description: "Current speech delivered")
        var resumePreparation: CheckedContinuation<Void, Never>?
        speaker.speak("Old speech") {
            await withCheckedContinuation { continuation in
                resumePreparation = continuation
                preparing.fulfill()
            }
            resumed.fulfill()
        }
        await fulfillment(of: [preparing], timeout: 1)
        speaker.stop()
        synthesizer.onSpeak = { spoken.fulfill() }
        speaker.speak("Current speech") {}
        resumePreparation?.resume()
        await fulfillment(of: [resumed, spoken], timeout: 1)
        XCTAssertEqual(synthesizer.spoken, ["Current speech"])
        speaker.stop()
    }
}

private final class RecordingSpeechSynthesizer: AVSpeechSynthesizer {
    var spoken: [String] = []
    var onSpeak: (() -> Void)?

    override func speak(_ utterance: AVSpeechUtterance) {
        MainActor.preconditionIsolated()
        spoken.append(utterance.speechString)
        onSpeak?()
    }

    override func stopSpeaking(at boundary: AVSpeechBoundary) -> Bool { true }
}
