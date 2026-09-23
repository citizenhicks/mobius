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
        if isPlaying {
            view.player.play()
        } else {
            view.player.pause()
        }
    }

    static func dismantleUIView(_ view: OrbPlayerView, coordinator: ()) {
        view.player.pause()
        view.looper.disableLooping()
    }
}

private final class OrbPlayerView: UIView {
    override class var layerClass: AnyClass { AVPlayerLayer.self }
    let player = AVQueuePlayer()
    let looper: AVPlayerLooper

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
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }
}
