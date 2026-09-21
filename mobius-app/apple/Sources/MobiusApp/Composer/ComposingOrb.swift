import AVFoundation
import SwiftUI
import UIKit

struct MobiusComposingOrb: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        Group {
            if !reduceMotion,
                let url = Bundle.main.url(forResource: "ComposingOrb", withExtension: "mov")
            {
                ComposingOrbVideo(url: url, isPlaying: scenePhase == .active)
            } else {
                Image("MobiusLogo")
                    .resizable()
                    .scaledToFit()
            }
        }
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
    private let poster = UIImageView(image: UIImage(named: "MobiusLogo"))
    private var readyObservation: NSKeyValueObservation?

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
        poster.contentMode = .scaleAspectFit
        poster.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        addSubview(poster)
        readyObservation = playerLayer.observe(\.isReadyForDisplay, options: [.initial, .new]) {
            [weak self] _, _ in
            Task { @MainActor [weak self] in
                guard let self else { return }
                poster.isHidden = (layer as! AVPlayerLayer).isReadyForDisplay
            }
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }
}
