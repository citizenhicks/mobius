import AVFoundation
import SwiftUI
import UIKit

struct MobiusComposingOrb: View {
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        ComposingOrbVideo(
            url: Bundle.main.url(forResource: "ComposingOrb", withExtension: "mov")!,
            isPlaying: scenePhase == .active
        )
        .allowsHitTesting(false)
    }
}

/// Plays the Blender render with its original material, soft lighting, and transparent background.
private struct ComposingOrbVideo: UIViewRepresentable {
    let url: URL
    let isPlaying: Bool

    func makeUIView(context: Context) -> OrbPlayerView {
        OrbPlayerView(url: url)
    }

    func updateUIView(_ view: OrbPlayerView, context: Context) {
        view.isPlaying = isPlaying
    }

    static func dismantleUIView(_ view: OrbPlayerView, coordinator: ()) {
        view.isPlaying = false
        view.looper.disableLooping()
    }
}

final class OrbPlayerView: UIView {
    override class var layerClass: AnyClass { AVPlayerLayer.self }
    let player = AVQueuePlayer()
    let looper: AVPlayerLooper
    var isPlaying = false { didSet { updatePlayback() } }
    private var audioDisconnected = false

    init(url: URL) {
        looper = AVPlayerLooper(player: player, templateItem: AVPlayerItem(url: url))
        super.init(frame: .zero)
        isOpaque = false
        backgroundColor = .clear
        player.isMuted = true
        player.preventsDisplaySleepDuringVideoPlayback = false
        let playerLayer = layer as! AVPlayerLayer
        playerLayer.player = player
        playerLayer.videoGravity = .resizeAspect
        playerLayer.isOpaque = false
        if #available(iOS 27.0, *) {
            // Muting alone still activates the audio session and can interrupt music.
            player.setDisconnectedFromSystemAudio(true) { [weak self] in
                Task { @MainActor [weak self] in
                    self?.audioDisconnected = true
                    self?.updatePlayback()
                }
            }
        } else {
            audioDisconnected = true
        }
    }

    private func updatePlayback() {
        if isPlaying && audioDisconnected { player.play() } else { player.pause() }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }
}
