import SwiftUI

/// Keep high-frequency metering out of the view that owns native menus and hover tracking.
struct VoiceWaveform: View {
    let voice: RealtimeVoiceSession
    let playbackColor: Color
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var isVisible = false

    var body: some View {
        AudioLevelEqualizer(
            amplitude: isVisible ? sqrt(voice.audioLevels.displayLevel) : 0,
            flare: voice.levelFlare,
            playbackColor: voice.audioLevels.isPlaybackActive ? playbackColor : nil
        )
        .animation(
            reduceMotion ? nil : .smooth(duration: 0.09),
            value: [voice.audioLevels.displayLevel, voice.levelFlare]
        )
        .onAppear { isVisible = true }
        .onDisappear { isVisible = false }
        .accessibilityHidden(true)
    }
}
