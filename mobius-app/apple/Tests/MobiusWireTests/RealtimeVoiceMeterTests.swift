@testable import Mobius
import SwiftUI
import XCTest

final class RealtimeVoiceMeterTests: XCTestCase {
    func testNativeStatsKeepMicrophoneAndPlaybackSeparate() {
        var levels = Mobius.RealtimeAudioLevels()
        levels.include(type: "media-source", values: ["kind": "audio" as NSString, "audioLevel": 0.36 as NSNumber])
        levels.include(type: "inbound-rtp", values: ["kind": "audio" as NSString, "audioLevel": 0.81 as NSNumber])
        levels.include(type: "media-source", values: ["kind": "video" as NSString, "audioLevel": 1 as NSNumber])
        levels.include(type: "outbound-rtp", values: ["kind": "audio" as NSString, "audioLevel": 1 as NSNumber])
        levels.include(type: "inbound-rtp", values: ["kind": "audio" as NSString, "audioLevel": Double.nan as NSNumber])
        levels.include(type: "media-source", values: ["kind": "audio" as NSString])
        XCTAssertEqual(levels.microphone, 0.36)
        XCTAssertEqual(levels.playback, 0.81)
        XCTAssertTrue(levels.isPlaybackActive)
        XCTAssertEqual(levels.displayLevel, 0.81)
        levels.playback = 0.0001
        XCTAssertFalse(levels.isPlaybackActive)
        XCTAssertEqual(levels.displayLevel, 0.36)
    }

    @MainActor
    func testMutingAndClosingClearOnlyTheOwnedAudioLevels() {
        let voice = Mobius.RealtimeVoiceSession()
        let levels = Mobius.RealtimeAudioLevels(microphone: 0.36, playback: 0.81)
        voice.updateAudioLevels(levels)
        voice.isMuted = true
        XCTAssertEqual(voice.audioLevels.microphone, 0)
        XCTAssertEqual(voice.audioLevels.playback, 0.81)
        voice.updateAudioLevels(levels)
        XCTAssertEqual(voice.audioLevels.microphone, 0)
        voice.isMuted = false
        voice.updateAudioLevels(levels)
        XCTAssertEqual(voice.audioLevels, levels)
        voice.close()
        XCTAssertEqual(voice.audioLevels, Mobius.RealtimeAudioLevels())
        XCTAssertFalse(voice.isMuted)
        XCTAssertNotNil(Mobius.MobiusGlyph.micOff01.menuImage(.primary))
        XCTAssertEqual(Mobius.MobiusSymbol.knownGlyph(for: "voice"), .audioWave01)
    }
}

@MainActor
extension AppModelTests {
    func testVoiceSurfaceHidesComposerDuringStartupAndPreservesDraft() async throws {
        let defaults = try XCTUnwrap(UserDefaults(suiteName: UUID().uuidString))
        let directory = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let model = Mobius.AppModel(
            store: Mobius.GatewayStore(defaults: defaults, transcriptDirectory: directory, draftDirectory: directory),
            settingsDefaults: defaults,
            appLockAuthenticator: Mobius.AppLockAuthenticator(method: { .unavailable }, authenticate: { _ in false }),
            requestSender: { _ in }
        )
        model.bots = [try JSONDecoder().decode(Mobius.BotRecord.self, from: JSONEncoder().encode(bot(tint: .orange)))]
        model.pendingNewChatBotID = "bot-1"
        model.connectionState = .ready
        model.composer = "Preserve this draft"
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene)
        let previous = scene.keyWindow
        previous?.isHidden = true
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(x: 0, y: 0, width: 390, height: 260)
        defer { window.isHidden = true; previous?.isHidden = false; previous?.makeKeyAndVisible() }

        func show(_ scheme: ColorScheme, preview: Mobius.TranscriptPreview? = nil) async -> UIView {
            let content = Group {
                if let preview {
                    Mobius.PreviewTranscriptSheet(preview: preview)
                } else {
                    VStack {
                        if !model.transcript.isEmpty {
                            Mobius.TranscriptRowsView(
                                projection: Mobius.TranscriptProjection(entries: model.transcript),
                                fileSessionID: nil
                            )
                            .padding(.horizontal, Mobius.MobiusSpace.l)
                        }
                        Spacer(minLength: 0)
                        Mobius.ComposerView(showBotSettings: {})
                    }
                }
            }
                .frame(maxHeight: .infinity, alignment: .bottom)
                .background(Mobius.MobiusPalette(scheme).canvas)
                .modifier(Mobius.MobiusTheme())
                .environment(model)
                .environment(\.colorScheme, scheme)
            let host = UIHostingController(rootView: content)
            host.overrideUserInterfaceStyle = scheme == .dark ? .dark : .light
            host.view.backgroundColor = UIColor(Mobius.MobiusPalette(scheme).canvas)
            window.backgroundColor = host.view.backgroundColor
            window.rootViewController = host
            window.makeKeyAndVisible()
            // Let native glass finish switching hosts before recording the visual fixture.
            try? await Task.sleep(for: .milliseconds(300))
            host.view.layoutIfNeeded()
            return host.view
        }
        func inputs(in view: UIView) -> [UIView] {
            (view is UITextField || view is UITextView ? [view] : []) + view.subviews.flatMap { inputs(in: $0) }
        }
        func capture(_ view: UIView, name: String) {
            let image = UIGraphicsImageRenderer(bounds: view.bounds).image { _ in
                view.drawHierarchy(in: view.bounds, afterScreenUpdates: true)
            }
            let attachment = XCTAttachment(image: image)
            attachment.name = name
            attachment.lifetime = .keepAlways
            add(attachment)
        }
        let normal = await show(.light)
        XCTAssertFalse(inputs(in: normal).isEmpty)
        capture(normal, name: "voice-normal-composer")
        // A pending call is sufficient: never request permission or create media.
        model.mountedWidgets = [Mobius.MountedWidget(capability: "test-preview", widget: Mobius.FrontendWidget(
            id: "voice-preview", slot: .composerFooter, text: "Voice conversation", tone: "neutral",
            symbol: "voice", iconOnly: true, progress: nil, content: nil,
            action: .capabilityCommand(capability: "test-preview", command: "show", arguments: "", input: nil, target: nil)
        ))]
        model.realtimeVoiceCall = Mobius.RealtimeVoiceCall(requestID: "pending", sessionID: "chat-1")
        model.realtimeVoice.updateAudioLevels(Mobius.RealtimeAudioLevels(microphone: 0.36, playback: 0))
        let microphone = await show(.light)
        XCTAssertTrue(inputs(in: microphone).isEmpty)
        capture(microphone, name: "voice-light-microphone")
        model.realtimeVoice.updateAudioLevels(Mobius.RealtimeAudioLevels(microphone: 0, playback: 0.81))
        capture(await show(.light), name: "voice-light-bot")
        model.realtimeVoice.updateAudioLevels(Mobius.RealtimeAudioLevels(microphone: 0.36, playback: 0))
        capture(await show(.dark), name: "voice-dark-microphone")
        model.realtimeVoice.updateAudioLevels(Mobius.RealtimeAudioLevels(microphone: 0.36, playback: 0.81))
        capture(await show(.dark), name: "voice-dark-both")
        window.frame.size.height = 500
        model.transcript = [Mobius.TranscriptEntry(
            id: "voice-handoff", text: "Review the launch checklist and fix the remaining issues.",
            kind: .peer, format: "plain_text", pending: false,
            messageMetadata: Mobius.TranscriptMessageMetadata(
                author: .peer(messageID: "handoff", sessionID: "voice-child", handle: "voice agent", symbol: "voice"),
                delivery: .turn
            )
        )]
        capture(await show(.light), name: "voice-handoff-shared-bubble")
        let preview = Mobius.TranscriptPreview(
            id: "voice-child", title: "voice agent", context: "", status: nil, model: nil,
            entries: [
                Mobius.TranscriptEntry(id: "spoken-input", text: "How is the launch looking?", kind: .user, format: "plain_text", pending: false),
                Mobius.TranscriptEntry(id: "spoken-answer", text: "The Bot is checking the remaining items. I'll keep you updated.", kind: .assistant, format: "plain_text", pending: false)
            ], next: nil
        )
        capture(await show(.light, preview: preview), name: "voice-shared-read-only-transcript")
        XCTAssertNotNil(model.realtimeVoiceCall)
        model.stopRealtimeVoice()
        let restored = await show(.light)
        XCTAssertFalse(inputs(in: restored).isEmpty)
        XCTAssertEqual(model.composer, "Preserve this draft")
    }
}
