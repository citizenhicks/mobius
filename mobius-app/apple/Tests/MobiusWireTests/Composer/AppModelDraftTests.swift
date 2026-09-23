import Foundation
@testable import Mobius
@preconcurrency import AVFoundation
import XCTest
import SwiftUI
import AudioToolbox

@MainActor
extension AppModelTests {
    func testDictationCanActivateTheRecordingSession() async throws {
        let session = AVAudioSession.sharedInstance()
        let previousCategory = session.category
        let previousMode = session.mode
        let previousOptions = session.categoryOptions
        let previousHaptics = session.allowHapticsAndSystemSoundsDuringRecording
        defer {
            try? session.setAllowHapticsAndSystemSoundsDuringRecording(previousHaptics)
            try? session.setCategory(
                previousCategory, mode: previousMode, options: previousOptions)
        }
        try await ComposerDictation.setAudioSessionActive(true)
        XCTAssertEqual(session.category, .record)
        XCTAssertEqual(session.mode, .measurement)
        XCTAssertTrue(session.allowHapticsAndSystemSoundsDuringRecording)
        XCTAssertFalse(session.categoryOptions.contains(.duckOthers))
        try await ComposerDictation.setAudioSessionActive(false)
    }

    func testDictationKeepsHistoricalPeaksThroughSilence() throws {
        let format = try XCTUnwrap(AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 1))
        let buffer = try XCTUnwrap(AVAudioPCMBuffer(pcmFormat: format, frameCapacity: 1_024))
        buffer.frameLength = buffer.frameCapacity
        let samples = try XCTUnwrap(buffer.floatChannelData)[0]
        for index in 0..<Int(buffer.frameLength) { samples[index] = 0 }
        XCTAssertEqual(ComposerDictation.recordingLevel(in: buffer), 0)
        for index in 0..<Int(buffer.frameLength) { samples[index] = 0.01 }
        let level = ComposerDictation.recordingLevel(in: buffer)
        XCTAssertEqual(level, 0.4, accuracy: 0.001)
        for index in 0..<Int(buffer.frameLength) { samples[index] = 1 }
        XCTAssertEqual(ComposerDictation.recordingLevel(in: buffer), 1)

        let dictation = ComposerDictation()
        dictation.recordAudioLevel(level, duration: 0.05)
        XCTAssertTrue(dictation.audioLevels.isEmpty)
        dictation.recordAudioLevel(0, duration: 0.05)
        XCTAssertEqual(dictation.audioLevels, [level])
        dictation.recordAudioLevel(0, duration: 0.1)
        XCTAssertEqual(
            dictation.audioLevels, [level, 0], "Silence must not flatten recorded speech")

        func image(_ levels: [Double]) throws -> CGImage {
            let renderer = ImageRenderer(
                content: DictationWaveform(levels: levels).frame(width: 280, height: 44))
            renderer.scale = 1
            return try XCTUnwrap(renderer.cgImage)
        }
        let first = try image([1, 0])
        let next = try image([1, 0, 0])
        func peakX(_ image: CGImage) throws -> Int? {
            let context = try XCTUnwrap(
                CGContext(
                    data: nil, width: 280, height: 44, bitsPerComponent: 8, bytesPerRow: 280 * 4,
                    space: CGColorSpaceCreateDeviceRGB(),
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue))
            context.draw(image, in: CGRect(x: 0, y: 0, width: 280, height: 44))
            let bytes = try XCTUnwrap(context.data).assumingMemoryBound(to: UInt8.self)
            return (0..<280).first { bytes[(10 * 280 + $0) * 4 + 3] > 0 }
        }
        XCTAssertEqual(
            try XCTUnwrap(peakX(first)) - XCTUnwrap(peakX(next)), 7,
            "New silence shifts an existing peak left by exactly one bar")
        for _ in 0..<150 { dictation.recordAudioLevel(0, duration: 0.1) }
        XCTAssertEqual(dictation.audioLevels.count, 128)
        XCTAssertTrue(dictation.audioLevels.allSatisfy { $0 == 0 })
        dictation.stop()
        XCTAssertTrue(dictation.audioLevels.isEmpty)
    }

    func testSilentDictationDotsHaveContinuousSpacing() throws {
        let renderer = ImageRenderer(
            content:
                DictationWaveform(levels: []).frame(width: 280, height: 44).background(.black))
        renderer.scale = 1
        let image = try XCTUnwrap(renderer.cgImage)
        let context = try XCTUnwrap(
            CGContext(
                data: nil, width: 280, height: 44,
                bitsPerComponent: 8, bytesPerRow: 280 * 4, space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue))
        context.draw(image, in: CGRect(x: 0, y: 0, width: 280, height: 44))
        let bytes = try XCTUnwrap(context.data).assumingMemoryBound(to: UInt8.self)
        var starts: [Int] = []
        var wasDot = false
        for x in 0..<280 {
            let offset = (22 * 280 + x) * 4
            let isDot = Int(bytes[offset]) + Int(bytes[offset + 1]) + Int(bytes[offset + 2]) > 2
            if isDot && !wasDot { starts.append(x) }
            wasDot = isDot
        }
        XCTAssertGreaterThan(starts.count, 30)
        for (left, right) in zip(starts, starts.dropFirst()) {
            XCTAssertEqual(right - left, 7, "No gap where faded dots meet the active bars")
        }
    }

    func testDictationCuesAreBundledPlayableSystemSounds() throws {
        for name in ["DictationStart", "DictationStop"] {
            let url = try XCTUnwrap(Bundle.main.url(forResource: name, withExtension: "wav"))
            let audio = try AVAudioFile(forReading: url)
            XCTAssertGreaterThan(audio.length, 0)
            var sound: SystemSoundID = 0
            XCTAssertEqual(
                AudioServicesCreateSystemSoundID(url as CFURL, &sound), kAudioServicesNoError)
            XCTAssertEqual(AudioServicesDisposeSystemSoundID(sound), kAudioServicesNoError)
        }
    }

    func testIdleDictationDoesNotCreateAnAudioEngine() {
        var engineCreations = 0
        func makeEngine() -> AVAudioEngine {
            engineCreations += 1
            return AVAudioEngine()
        }
        let dictation = ComposerDictation(engine: makeEngine())
        XCTAssertEqual(engineCreations, 0)
        dictation.stop()
        dictation.stopIfDraftChanged("Typed without dictation")
        dictation.stop()
        XCTAssertEqual(engineCreations, 0)
        XCTAssertFalse(dictation.isActive)
    }

    func testDictationPreservesTheDraftBoundary() {
        for original in ["Hello", "Hello ", ""] {
            var draft = DictationDraft(text: original)
            let prefix = original.isEmpty ? "" : "Hello "
            XCTAssertEqual(draft.update("wor", currentText: original), prefix + "wor")
            XCTAssertEqual(draft.update("world", currentText: prefix + "wor"), prefix + "world")
            XCTAssertNil(draft.update("world again", currentText: "Manually edited"))
            XCTAssertNil(draft.update("world again", currentText: ""))
            XCTAssertEqual(draft.text, prefix + "world")
            XCTAssertEqual(draft.originalText(ifCurrentText: prefix + "world"), original)
            XCTAssertNil(draft.originalText(ifCurrentText: "Manually edited"))
        }
    }

    func testDictationOnlyOffersEnglishAndFrench() {
        XCTAssertTrue(ComposerDictation.supports(Locale(identifier: "en-US")))
        XCTAssertTrue(ComposerDictation.supports(Locale(identifier: "fr-FR")))
        XCTAssertFalse(ComposerDictation.supports(Locale(identifier: "de-DE")))
    }

    func testDictationAuthorizationAcceptsBackgroundCallbacks() async {
        let authorization = await dictationAuthorization { completion in
            DispatchQueue.global().async {
                dispatchPrecondition(condition: .notOnQueue(.main))
                completion(.denied)
            }
        }
        MainActor.assertIsolated()
        XCTAssertEqual(authorization, .denied)
    }

    func testActiveTurnPrimaryActionStopsUntilAQueuedMessageCanSubmit() throws {
        let model = try model(requestSender: { _ in })
        model.gateway.connectionState = .ready
        model.chat.selectedSessionID = "chat-1"
        model.chat.activeTurnID = "turn-1"

        XCTAssertTrue(model.composerShowsInterruptAction)

        model.chat.composer = "Continue with this"
        XCTAssertFalse(model.composerShowsInterruptAction)
        XCTAssertEqual(model.localizedString(model.composerSendLabel), "Send as Steer")
        XCTAssertEqual(model.composerSendGlyph, .arrowUpRight01)
        XCTAssertEqual(model.composerAlternateDelivery, .queue)
    }

    func testRailVoiceSlotBecomesTheSharedSendActionWithoutReplacingAnActiveCall() throws {
        let model = try model(requestSender: { _ in })
        model.gateway.connectionState = .ready
        model.chat.selectedSessionID = "chat-1"
        XCTAssertFalse(model.composerRailShowsSendAction)

        model.chat.composerIsCompact = false
        XCTAssertTrue(model.composerRailShowsSendAction)
        model.chat.composerIsCompact = true

        model.chat.composer = "Continue with this"
        XCTAssertTrue(model.composerRailShowsSendAction)
        XCTAssertEqual(model.composerSendGlyph, .arrowUp02)

        model.chat.composer = ""
        model.chat.activeTurnID = "turn-1"
        XCTAssertTrue(model.composerRailShowsSendAction)
        XCTAssertTrue(model.composerShowsInterruptAction)

        model.chat.realtimeVoiceCall = RealtimeVoiceCall(requestID: "voice", sessionID: "chat-1")
        XCTAssertFalse(model.composerRailShowsSendAction)

        model.chat.realtimeVoiceCall = nil
        model.chat.activeTurnID = nil
        model.chat.composer = "Draft"
        for state in [ConnectionState.connecting, .authenticating, .loading] {
            model.gateway.connectionState = state
            XCTAssertTrue(model.composerRailShowsSendAction)
            XCTAssertTrue(model.gateway.connectionState.isLoading)
            XCTAssertFalse(model.canSubmitComposer)
        }
        model.gateway.connectionState = .ready
        XCTAssertFalse(model.gateway.connectionState.isLoading)
        XCTAssertTrue(model.canSubmitComposer)
    }

    func testSwitchingSessionsFlushesAndRestoresTextDrafts() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: root)
        }
        let store = GatewayStore(
            defaults: defaults,
            transcriptDirectory: root.appendingPathComponent("Transcripts", isDirectory: true),
            draftDirectory: root.appendingPathComponent("Drafts", isDirectory: true)
        )
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        let firstReply = MessageReply(
            target: MessageTarget(checkpointSequence: 3, batchItemCount: 1),
            text: "First original"
        )
        let secondReply = MessageReply(
            target: MessageTarget(checkpointSequence: 5, batchItemCount: 2),
            text: "Second original"
        )
        await store.saveComposerDraft(
            ComposerDraft(text: "Draft two", reply: secondReply),
            accountID: account.id,
            sessionID: "chat-2"
        )
        let recorder = GatewayRequestRecorder()
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            settingsDefaults: defaults,
            requestSender: { request in await recorder.record(request) }
        )
        model.bots = [bot()]
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready

        var requestCount = await recorder.requestCount()
        model.chat.openSession("chat-1")
        let firstRequest = await recorder.firstRequest(after: requestCount) { request in
            guard case .openSession(_, "chat-1", _) = request else { return false }
            return true
        }
        guard case .openSession(let firstID, "chat-1", _) = try XCTUnwrap(firstRequest)
        else { return XCTFail("Expected first session open") }
        model.chat.composer = "Typed while opening"
        model.gateway.handle(
            .sessionOpened(
                requestID: firstID,
                payload: sessionReady(latestSequence: 1, sessionID: "chat-1")
            ))
        model.gateway.handle(.sessionReplayComplete(requestID: firstID, sessionID: "chat-1"))
        let firstSessionReady = await eventually { model.canCreateSession }
        XCTAssertTrue(firstSessionReady)
        XCTAssertEqual(model.chat.composer, "Typed while opening")
        model.chat.composer = "Draft one"
        model.chat.composerReply = firstReply

        requestCount = await recorder.requestCount()
        model.chat.openSession("chat-2")
        let secondRequest = await recorder.firstRequest(after: requestCount) { request in
            guard case .openSession(_, "chat-2", _) = request else { return false }
            return true
        }
        let secondOpen = try XCTUnwrap(secondRequest)
        guard case .openSession(let secondID, _, _) = secondOpen else {
            return XCTFail("Expected second session open")
        }
        model.gateway.handle(
            .sessionOpened(
                requestID: secondID,
                payload: sessionReady(latestSequence: 1, sessionID: "chat-2")
            ))
        model.gateway.handle(.sessionReplayComplete(requestID: secondID, sessionID: "chat-2"))
        let secondSessionReady = await eventually {
            model.canCreateSession
                && model.chat.composer == "Draft two"
                && model.chat.composerReply == secondReply
        }
        XCTAssertTrue(secondSessionReady)

        let firstDraftSaved = await eventually {
            await store.loadComposerDraft(
                accountID: account.id,
                sessionID: "chat-1"
            ) == ComposerDraft(text: "Draft one", reply: firstReply)
        }
        XCTAssertTrue(firstDraftSaved)
        let firstDraft = await store.loadComposerDraft(
            accountID: account.id,
            sessionID: "chat-1"
        )
        XCTAssertEqual(firstDraft, ComposerDraft(text: "Draft one", reply: firstReply))
        XCTAssertEqual(model.chat.composer, "Draft two")
        XCTAssertEqual(model.chat.composerReply, secondReply)

        requestCount = await recorder.requestCount()
        model.sendMessage()
        model.chat.composer = "Next draft"
        let submitRequest = await recorder.firstRequest(after: requestCount) { request in
            if case .submit = request { return true }
            return false
        }
        let submittedDraft = await store.loadComposerDraft(
            accountID: account.id,
            sessionID: "chat-2"
        )
        XCTAssertEqual(submittedDraft, ComposerDraft(text: "Draft two", reply: secondReply))
        guard case .submit(_, let submission) = try XCTUnwrap(submitRequest) else {
            return XCTFail("Expected submitted draft")
        }
        model.gateway.handle(.accepted(requestID: submission.id))
        let nextDraftSaved = await eventually {
            await store.loadComposerDraft(
                accountID: account.id,
                sessionID: "chat-2"
            ) == ComposerDraft(text: "Next draft")
        }
        XCTAssertTrue(nextDraftSaved)
        let nextDraft = await store.loadComposerDraft(
            accountID: account.id,
            sessionID: "chat-2"
        )
        XCTAssertEqual(nextDraft, ComposerDraft(text: "Next draft"))

        requestCount = await recorder.requestCount()
        model.deleteSession(session(sessionID: "chat-2", state: .idle))
        let deleteRequest = await recorder.firstRequest(after: requestCount) { request in
            guard case .deleteSessions(_, let ids) = request else { return false }
            return ids == ["chat-2"]
        }
        guard case .deleteSessions(let deleteID, let ids) = try XCTUnwrap(deleteRequest),
            ids == ["chat-2"]
        else { return XCTFail("Expected session delete") }
        model.gateway.handle(.accepted(requestID: deleteID))
        model.gateway.handle(.sessions(requestID: deleteID, sessions: []))
        let deletedDraftRemoved = await eventually {
            await store.loadComposerDraft(
                accountID: account.id,
                sessionID: "chat-2"
            ).isEmpty
        }
        XCTAssertTrue(deletedDraftRemoved)
        let deletedDraft = await store.loadComposerDraft(
            accountID: account.id,
            sessionID: "chat-2"
        )
        XCTAssertTrue(model.chat.composer.isEmpty)
        XCTAssertTrue(deletedDraft.isEmpty)
    }

    func testComposerDraftsAreDurableScopedAndBounded() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        let transcriptDirectory = root.appendingPathComponent("Transcripts", isDirectory: true)
        let draftDirectory = root.appendingPathComponent("Drafts", isDirectory: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: root)
        }
        let firstAccount = GatewayAccount(
            endpoint: try GatewayEndpoint("tcp://localhost:9191")
        )
        let secondAccount = GatewayAccount(
            endpoint: try GatewayEndpoint("tcp://localhost:9192")
        )
        var store = GatewayStore(
            defaults: defaults,
            transcriptDirectory: transcriptDirectory,
            draftDirectory: draftDirectory
        )
        await store.saveComposerDraft(
            ComposerDraft(text: "First account"),
            accountID: firstAccount.id,
            sessionID: "chat-1"
        )
        await store.saveComposerDraft(
            ComposerDraft(text: "Second account"),
            accountID: secondAccount.id,
            sessionID: "chat-1"
        )

        store = GatewayStore(
            defaults: defaults,
            transcriptDirectory: transcriptDirectory,
            draftDirectory: draftDirectory
        )
        let restoredFirst = await store.loadComposerDraft(
            accountID: firstAccount.id,
            sessionID: "chat-1"
        )
        let restoredSecond = await store.loadComposerDraft(
            accountID: secondAccount.id,
            sessionID: "chat-1"
        )
        XCTAssertEqual(restoredFirst, ComposerDraft(text: "First account"))
        XCTAssertEqual(restoredSecond, ComposerDraft(text: "Second account"))

        await store.saveComposerDraft(
            .empty,
            accountID: firstAccount.id,
            sessionID: "chat-1"
        )
        await store.saveComposerDraft(
            ComposerDraft(text: "Existing"),
            accountID: firstAccount.id,
            sessionID: "oversized"
        )
        await store.saveComposerDraft(
            ComposerDraft(text: String(repeating: "x", count: maximumComposerBytes + 1)),
            accountID: firstAccount.id,
            sessionID: "oversized"
        )
        let removedEmpty = await store.loadComposerDraft(
            accountID: firstAccount.id,
            sessionID: "chat-1"
        )
        let removedOversized = await store.loadComposerDraft(
            accountID: firstAccount.id,
            sessionID: "oversized"
        )
        XCTAssertEqual(removedEmpty, .empty)
        XCTAssertEqual(removedOversized, .empty)

        await store.saveComposerDraft(
            ComposerDraft(text: "Will corrupt"),
            accountID: firstAccount.id,
            sessionID: "corrupt"
        )
        let corruptFilename = Data("corrupt".utf8).base64EncodedString()
        let corruptURL =
            draftDirectory
            .appendingPathComponent(firstAccount.id.uuidString, isDirectory: true)
            .appendingPathComponent(corruptFilename)
            .appendingPathExtension("txt")
        try Data([0xFF]).write(to: corruptURL, options: .atomic)
        let corrupt = await store.loadComposerDraft(
            accountID: firstAccount.id,
            sessionID: "corrupt"
        )
        XCTAssertEqual(corrupt, .empty)
        XCTAssertFalse(FileManager.default.fileExists(atPath: corruptURL.path))

        await store.saveComposerDraft(
            ComposerDraft(text: "Remove with account"),
            accountID: firstAccount.id,
            sessionID: "chat-2"
        )
        try await store.remove(firstAccount)
        let removedAccountDraft = await store.loadComposerDraft(
            accountID: firstAccount.id,
            sessionID: "chat-2"
        )
        let preservedAccountDraft = await store.loadComposerDraft(
            accountID: secondAccount.id,
            sessionID: "chat-1"
        )
        XCTAssertEqual(removedAccountDraft, .empty)
        XCTAssertEqual(preservedAccountDraft, ComposerDraft(text: "Second account"))
    }

    func testUnavailableCachedCursorRetriesTheOpenWithoutIt() async throws {
        let suiteName = UUID().uuidString
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer {
            defaults.removePersistentDomain(forName: suiteName)
            try? FileManager.default.removeItem(at: directory)
        }
        let recorder = GatewayRequestRecorder()
        let store = GatewayStore(defaults: defaults, transcriptDirectory: directory)
        let account = GatewayAccount(endpoint: try GatewayEndpoint("tcp://localhost:9191"))
        await store.saveTranscript(
            accountID: account.id,
            sessionID: "chat-1",
            sequence: 7,
            transcript: [
                TranscriptEntry(
                    id: "answer-1",
                    text: "Cached",
                    kind: .assistant,
                    format: "plain_text",
                    pending: false
                )
            ],
            currentUsage: TokenUsage(),
            lastUsage: TokenUsage()
        )
        let model = AppModel(
            client: GatewayClient(),
            store: store,
            requestSender: { request in await recorder.record(request) }
        )
        model.bots = [bot()]
        model.gateway.accounts = [account]
        model.gateway.selectedAccountID = account.id
        model.gateway.connectionState = .ready

        model.chat.openSession("chat-1")
        let firstRequest = await recorder.firstRequest(after: 0) { request in
            guard case .openSession(_, "chat-1", 7) = request else {
                return false
            }
            return true
        }
        let first = try XCTUnwrap(firstRequest)
        guard case .openSession(let requestID, _, 7) = first else {
            return XCTFail("Expected the cached cursor")
        }
        let requestCount = await recorder.requestCount()
        model.gateway.handle(
            .rejected(
                GatewayRejection(
                    requestId: requestID,
                    code: "replay_unavailable",
                    message: "Reload",
                    fatal: false
                )))
        let retryRequest = await recorder.firstRequest(after: requestCount) { request in
            guard case .openSession(_, "chat-1", nil) = request else { return false }
            return true
        }
        _ = try XCTUnwrap(retryRequest)

        let opens = await recorder.requests().compactMap { request -> (String, UInt64?)? in
            guard case .openSession(_, let sessionID, let cursor) = request else { return nil }
            return (sessionID, cursor)
        }
        XCTAssertEqual(opens.count, 2)
        XCTAssertEqual(opens.last?.0, "chat-1")
        XCTAssertNil(opens.last?.1)
        let removed = await eventually {
            await store.loadTranscript(accountID: account.id, sessionID: "chat-1") == nil
        }
        XCTAssertTrue(removed)
    }

}
