import Foundation

/// The live call's levels for the app's equalizer, which polls them at its own frame rate.
@MainActor
enum VoiceMeter {
    static var levels = RealtimeAudioLevels()
    static var flare = 0.0
}

/// Writes the displayed level and its flare to `out[0...1]`; returns whether the Bot speaks.
@_cdecl("mobius_native_voice_levels")
public func nativeVoiceLevels(_ out: UnsafeMutablePointer<Double>) -> Bool {
    let (level, flare, playback) = MainActor.assumeIsolated {
        (VoiceMeter.levels.displayLevel, VoiceMeter.flare, VoiceMeter.levels.isPlaybackActive)
    }
    out[0] = level
    out[1] = flare
    return playback
}
