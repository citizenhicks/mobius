import AppKit
import SwiftUI
@preconcurrency import WebRTC

@main
struct GatewayMenuBarApp: App {
    @NSApplicationDelegateAdaptor(GatewayMenuBarDelegate.self) private var delegate

    var body: some Scene {
        MenuBarExtra {
            if delegate.voicePanel.corner == nil {
                VoiceMenuView(model: delegate.model, presentation: delegate.voicePanel)
            } else {
                VStack(alignment: .leading, spacing: MobiusSpace.m) {
                    Button("Keyboard shortcuts…") { delegate.voicePanel.showKeyboardShortcuts() }
                    Button("Unpin voice window") { delegate.voicePanel.unpin() }
                }
                .padding(MobiusSpace.l)
            }
        } label: {
            HStack(spacing: 3) {
                Image(nsImage: Self.logo)
                if let symbol = delegate.symbol {
                    VoiceIcon(symbol)
                }
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("möbius Gateway")
            .accessibilityValue(delegate.model.status)
        }
        .menuBarExtraStyle(.window)
    }

    static let logo: NSImage = {
        let url = Bundle.main.url(forResource: "MobiusLogo", withExtension: "svg")!
        let image = NSImage(contentsOf: url)!
        image.size = NSSize(width: 22, height: 22)
        image.isTemplate = true
        return image
    }()
}

@MainActor
final class GatewayMenuBarDelegate: NSObject, NSApplicationDelegate {
    let model = MenuBarModel()
    lazy var voicePanel = VoicePanelController(model: model)
    private var terminating = false
    private let audioLogger = RTCCallbackLogger()

    var symbol: String? {
        if model.approval != nil { return "shieldAlert" }
        if model.voiceCall != nil { return model.voice.isMuted ? "micOff01" : "audioWave01" }
        return nil
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        _ = voicePanel
        audioLogger.severity = .verbose
        audioLogger.start { @Sendable message in
            guard message.contains("audio_engine_device.mm") else { return }
            FileHandle.standardError.write(Data(message.utf8))
        }
        model.connect()
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard !terminating else { return .terminateLater }
        terminating = true
        Task {
            await model.shutdown()
            sender.reply(toApplicationShouldTerminate: true)
        }
        return .terminateLater
    }
}
