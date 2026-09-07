import Foundation
import XCTest
@preconcurrency import AVFoundation
@preconcurrency import WebRTC
@testable import Mobius

@MainActor
extension AppModelTests {
    private func voiceModel(recorder: GatewayRequestRecorder = GatewayRequestRecorder()) throws
        -> AppModel
    {
        let model = try model { await recorder.record($0) }
        let config = composition()
        var status = providerStatus(for: config.provider)
        status.realtimeVoices = ["marin", "cedar"]
        model.providerStatuses = [status]
        model.providerInstances = [
            ProviderInstance(
                label: "Work", tint: .blue, configured: true, selection: config.provider,
                modelIds: [], reasoningEfforts: []
            )
        ]
        model.modelChoices = [
            ModelChoice(
                route: "voice-route", group: "Work", model: config.provider.model,
                reasoningEffort: config.provider.reasoningEffort, contextWindow: nil,
                supportsImageInput: true, supportsRealtimeVoice: true, toolDiscovery: .native
            )
        ]
        model.modelProviders = ["voice-route": config.provider.instance]
        model.botDefaultsSnapshot = VersionedAgentConfig(revision: 1, config: config)
        model.chat.selectedModelRoute = "voice-route"
        model.gateway.connectionState = .ready
        return model
    }

    func testRealtimeEligibilityUsesSelectedRouteAndConfiguredInstance() throws {
        let model = try voiceModel()
        XCTAssertTrue(model.selectedRouteSupportsRealtimeVoice)
        model.chat.selectedSessionID = "chat-1"
        XCTAssertTrue(model.selectedRouteSupportsRealtimeVoice)
        model.providerInstances[0].configured = false
        XCTAssertFalse(model.selectedRouteSupportsRealtimeVoice)
        model.providerInstances[0].configured = true
        model.providerStatuses[0].realtimeVoices = []
        XCTAssertFalse(model.selectedRouteSupportsRealtimeVoice)
        model.providerStatuses[0].realtimeVoices = ["marin", "cedar"]
        model.chat.selectedModelRoute = "unknown"
        XCTAssertFalse(model.selectedRouteSupportsRealtimeVoice)
        model.chat.selectedModelRoute = "voice-route"
        model.modelProviders["voice-route"] = "other-instance"
        XCTAssertFalse(model.selectedRouteSupportsRealtimeVoice)
    }

    func testBotVoiceSelectionUsesEligibleCatalogAndSurvivesConfigurationCoding() throws {
        let model = try voiceModel()
        var config = composition()
        config.realtimeVoice = "cedar"
        XCTAssertEqual(model.realtimeVoices(for: config), ["marin", "cedar"])
        XCTAssertEqual(
            model.draft(config, selectingModelRoute: "voice-route")?.realtimeVoice, "cedar")

        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let data = try encoder.encode(config)
        XCTAssertEqual(try decoder.decode(AgentComposition.self, from: data), config)

        model.providerStatuses[0].realtimeVoices = ["marin"]
        XCTAssertNil(model.draft(config, selectingModelRoute: "voice-route")?.realtimeVoice)
        model.modelChoices = [
            ModelChoice(
                route: "voice-route", group: "Work", model: config.provider.model,
                reasoningEffort: config.provider.reasoningEffort, contextWindow: nil,
                supportsImageInput: true, toolDiscovery: .native
            )
        ]
        XCTAssertTrue(model.realtimeVoices(for: config).isEmpty)
    }

    func testVoicePillOpensLiveIsolatedTranscriptWithoutEndingCall() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try model { await recorder.record($0) }
        model.chat.selectedSessionID = "chat-1"
        model.gateway.connectionState = .ready
        let call = RealtimeVoiceCall(requestID: "call", sessionID: "chat-1")
        model.chat.realtimeVoiceCall = call
        let widget = MountedWidget(
            capability: "messages",
            widget: FrontendWidget(
                id: "voice", slot: .composerFooter, text: "Voice", tone: "neutral", symbol: "voice",
                iconOnly: true, progress: nil, content: nil,
                action: .capabilityCommand(
                    capability: "messages", command: "voice", arguments: "", input: nil, target: nil
                )
            ))
        model.submitWidget(widget)
        let request = await recorder.firstRequest(after: 0) {
            if case .submit = $0 { true } else { false }
        }
        guard case .submit("chat-1", let submission) = try XCTUnwrap(request) else {
            return XCTFail("Expected the existing capability command flow")
        }
        func preview(_ events: [RenderedEventRecord]) -> RenderedPreview {
            RenderedPreview(
                id: "voice-chat", title: "voice agent", subtitle: "", pageId: "latest",
                update: .replace, events: events, next: nil
            )
        }
        let draft = RenderedEventRecord(
            event: .object(["type": .string("message_delta"), "text": .string("Hello")]),
            blocks: [], submissionId: "spoken-1"
        )
        model.chat.reduce(
            event: AgentEventRecord(
                submissionId: submission.id, msg: .object(["type": .string("frontend")])),
            blocks: [], preview: preview([draft])
        )
        let draftID = try XCTUnwrap(model.chat.presentedPreview?.entries.first?.id)
        XCTAssertEqual(model.chat.presentedPreview?.entries.first?.pending, true)
        XCTAssertEqual(model.chat.presentedPreview?.entries.first?.text, "Hello")
        XCTAssertNil(model.chat.previewWidgetRequestID)
        model.chat.apply(
            RenderedPreview(
                id: "voice-chat", title: "voice agent", subtitle: "", pageId: "earlier",
                update: .prepend,
                events: [
                    RenderedEventRecord(
                        event: testMessageEvent(text: "Earlier discussion"), blocks: [],
                        submissionId: "spoken-0"
                    )
                ], next: nil
            ), selection: nil)

        let finals = [
            RenderedEventRecord(
                event: testMessageEvent(text: "Hello!"), blocks: [], submissionId: "spoken-1"),
            RenderedEventRecord(
                event: testAssistantMessage(
                    turnID: "spoken-reply", modelStepID: "spoken-reply", text: "Hi there!"
                ), blocks: []),
        ]
        model.chat.reduce(
            event: AgentEventRecord(submissionId: nil, msg: .object(["type": .string("frontend")])),
            blocks: [], preview: preview(finals)
        )
        XCTAssertEqual(
            model.chat.presentedPreview?.entries.map(\.text),
            ["Earlier discussion", "Hello!", "Hi there!"])
        XCTAssertEqual(model.chat.presentedPreview?.entries[1].id, draftID)
        XCTAssertNil(model.chat.presentedPreview?.next)
        XCTAssertTrue(try XCTUnwrap(model.chat.presentedPreview).entries.allSatisfy { !$0.pending })
        XCTAssertTrue(model.chat.transcript.isEmpty)
        XCTAssertEqual(model.sessionRunCount, 0)
        XCTAssertEqual(model.chat.realtimeVoiceCall, call)
        model.chat.stopRealtimeVoice(notifyGateway: false)
        XCTAssertEqual(
            model.chat.presentedPreview?.entries.map(\.text),
            ["Earlier discussion", "Hello!", "Hi there!"])
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
        XCTAssertNil(model.chat.realtimeVoiceCall)
        model.chooseWorkspace("/srv/project")
        XCTAssertEqual(model.newVoiceChatIntent, .selectingBot)
        model.selectBotForNewChat(second)
        let request = await recorder.firstRequest(after: 0) {
            if case .createSession = $0 { true } else { false }
        }
        guard case .createSession(let requestID, "/srv/project", "bot-2") = try XCTUnwrap(request)
        else {
            return XCTFail("Expected the selected workspace and Bot")
        }
        XCTAssertEqual(model.newVoiceChatIntent, .openingSession(requestID))
        XCTAssertNil(model.chat.realtimeVoiceCall)
        model.completePendingVoiceChat(requestID: "unrelated-replay")
        XCTAssertEqual(model.newVoiceChatIntent, .openingSession(requestID))
        model.cancelVoiceChatIntent()
        model.completePendingVoiceChat(requestID: requestID)
        XCTAssertNil(model.chat.realtimeVoiceCall)
        let requests = await recorder.requests()
        XCTAssertFalse(
            requests.contains { if case .startRealtimeVoice = $0 { true } else { false } })
    }

    func testOpeningAnExistingChatCancelsPendingVoiceSetup() throws {
        let model = try voiceModel()
        model.chat.sessions = [session(sessionID: "chat-1", state: .idle)]
        model.newVoiceChatIntent = .selectingBot

        model.openChat("chat-1")

        XCTAssertNil(model.newVoiceChatIntent)
    }

    func testCanceledVoiceStartAndLateAnswerEndOnlyThatCall() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try voiceModel(recorder: recorder)
        model.chat.selectedSessionID = "chat-1"
        model.chat.realtimeVoiceCall = RealtimeVoiceCall(
            requestID: "old", sessionID: "chat-1"
        )
        model.chat.stopRealtimeVoice()
        XCTAssertNil(model.chat.realtimeVoiceCall)
        let ended = await recorder.firstRequest(after: 0) {
            if case .endRealtimeVoice("chat-1", "old") = $0 { true } else { false }
        }
        XCTAssertNotNil(ended)
        model.chat.realtimeVoiceCall = RealtimeVoiceCall(
            requestID: "new", sessionID: "chat-1"
        )
        model.gateway.handle(
            .realtimeVoiceStarted(
                requestID: "old", sessionID: "chat-1", voiceID: "old", answerSDP: "late answer"
            ))
        model.gateway.handle(
            .realtimeVoiceFailed(requestID: "old", sessionID: "chat-1", message: "late error"))
        model.gateway.handle(.realtimeVoiceEnded(sessionID: "chat-1", voiceID: "old", reason: nil))
        XCTAssertEqual(model.chat.realtimeVoiceCall?.requestID, "new")
        model.gateway.handle(
            .realtimeVoiceFailed(requestID: "new", sessionID: "chat-1", message: "start failed"))
        XCTAssertNil(model.chat.realtimeVoiceCall)
    }

    func testBackgroundCancelsPendingVoiceAndEndsBeforeDisconnect() async throws {
        let recorder = GatewayRequestRecorder()
        let model = try voiceModel(recorder: recorder)
        model.chat.selectedSessionID = "chat-1"
        model.startRealtimeVoice()
        let call = try XCTUnwrap(model.chat.realtimeVoiceCall)
        let startup = try XCTUnwrap(model.chat.realtimeVoiceTask)
        let oldGeneration = model.gateway.connectionGeneration
        model.newVoiceChatIntent = .openingSession("pending-session")
        model.appDidEnterBackground()
        XCTAssertNil(model.chat.realtimeVoiceCall)
        XCTAssertNil(model.chat.realtimeVoiceTask)
        XCTAssertNil(model.newVoiceChatIntent)
        XCTAssertTrue(startup.isCancelled)
        XCTAssertFalse(model.chat.realtimeVoice.isConnected)
        XCTAssertNotEqual(model.gateway.connectionGeneration, oldGeneration)
        let ended = await recorder.firstRequest(after: 0) {
            if case .endRealtimeVoice("chat-1", call.requestID) = $0 { true } else { false }
        }
        XCTAssertNotNil(ended)
        let requests = await recorder.requests()
        XCTAssertFalse(
            requests.contains { if case .startRealtimeVoice = $0 { true } else { false } })
    }

    func testVoiceClosesOnSessionRouteAndBackgroundChanges() throws {
        let model = try voiceModel()
        model.chat.selectedSessionID = "chat-1"
        let call = RealtimeVoiceCall(requestID: "voice", sessionID: "chat-1")
        model.chat.realtimeVoiceCall = call
        model.chat.selectedSessionID = "chat-2"
        XCTAssertNil(model.chat.realtimeVoiceCall)
        model.chat.realtimeVoiceCall = call
        model.chat.selectedModelRoute = "other-route"
        XCTAssertNil(model.chat.realtimeVoiceCall)
        model.chat.realtimeVoiceCall = call
        model.newVoiceChatIntent = .selectingWorkspace
        model.appDidEnterBackground()
        XCTAssertNil(model.chat.realtimeVoiceCall)
        XCTAssertNil(model.newVoiceChatIntent)
        XCTAssertFalse(model.chat.realtimeVoice.isConnected)
    }
}

@MainActor
extension AppModelTests {
    func testNewVoiceChatOpensMicrophoneOnlyAfterCreatedSessionReplay() throws {
        let model = try voiceModel()
        model.openNewVoiceChat()
        model.chooseWorkspace("/srv/mobius")
        guard case .openingSession(let requestID) = model.newVoiceChatIntent else {
            return XCTFail(
                "Expected session creation after workspace and the only Bot were selected")
        }
        model.gateway.handle(
            .sessionOpened(
                requestID: requestID,
                payload: sessionReady(latestSequence: 0, modelRoute: "voice-route")
            ))
        XCTAssertNil(model.chat.realtimeVoiceCall)
        model.gateway.handle(.sessionReplayComplete(requestID: requestID, sessionID: "chat-1"))
        XCTAssertEqual(model.chat.realtimeVoiceCall?.sessionID, "chat-1")
        XCTAssertNil(model.newVoiceChatIntent)
        // Cancel before the asynchronous permission request runs.
        model.chat.stopRealtimeVoice()
    }

    func testCancelingWorkspaceSelectionDiscardsVoiceIntent() throws {
        let model = try voiceModel()
        model.openNewVoiceChat()
        model.showsWorkspaceBrowser = false
        XCTAssertNil(model.newVoiceChatIntent)
        model.chooseWorkspace("/srv/mobius")
        XCTAssertNil(model.chat.sessionRequestID)
        XCTAssertNil(model.chat.realtimeVoiceCall)
    }
}

@MainActor
extension AppModelTests {
    func testOldAudioInterruptionCannotEndReplacementVoiceCall() async throws {
        let model = try voiceModel()
        model.chat.selectedSessionID = "chat-1"
        model.startRealtimeVoice()
        // No permission request or capture: exercise only ownership and delegate delivery.
        model.chat.realtimeVoiceTask?.cancel()
        let oldVoice = model.chat.realtimeVoice
        oldVoice.audioSessionDidBeginInterruption(RTCAudioSession.sharedInstance())
        model.chat.stopRealtimeVoice()
        model.startRealtimeVoice()
        model.chat.realtimeVoiceTask?.cancel()
        let newVoice = model.chat.realtimeVoice
        let currentRequestID = model.chat.realtimeVoiceCall?.requestID
        XCTAssertFalse(oldVoice === newVoice)
        await Task.yield()
        XCTAssertEqual(model.chat.realtimeVoiceCall?.requestID, currentRequestID)
        XCTAssertNil(model.toast)

        newVoice.audioSessionDidBeginInterruption(RTCAudioSession.sharedInstance())
        let currentInterruptionHandled = await eventually { model.chat.realtimeVoiceCall == nil }
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

    func testReleasingOwnerCancelsSuspendedVoiceTaskAndClosesPeer() async throws {
        weak var releasedModel: AppModel?
        let cancellation = expectation(description: "Voice task canceled")
        let peer: RTCPeerConnection
        do {
            let model = try voiceModel()
            releasedModel = model
            XCTAssertTrue(RTCInitializeSSL())
            let factory = RTCPeerConnectionFactory(encoderFactory: nil, decoderFactory: nil)
            let configuration = RTCConfiguration()
            configuration.sdpSemantics = .unifiedPlan
            let constraints = RTCMediaConstraints(
                mandatoryConstraints: nil, optionalConstraints: nil)
            peer = try XCTUnwrap(
                factory.peerConnection(
                    with: configuration, constraints: constraints,
                    delegate: model.chat.realtimeVoice
                ))
            model.chat.realtimeVoice.peer = peer
            model.chat.realtimeVoiceCall = RealtimeVoiceCall(
                requestID: "voice", sessionID: "chat-1")
            model.chat.realtimeVoiceTask = Task {
                do {
                    try await Task.sleep(for: .seconds(3_600))
                } catch {
                    XCTAssertTrue(error is CancellationError)
                    cancellation.fulfill()
                }
            }
        }

        XCTAssertNil(releasedModel)
        await fulfillment(of: [cancellation], timeout: 1)
        XCTAssertEqual(peer.signalingState, .closed)
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
