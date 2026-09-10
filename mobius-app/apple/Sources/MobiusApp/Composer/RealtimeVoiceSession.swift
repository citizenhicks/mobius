import Foundation
import Observation
import SwiftUI
@preconcurrency import AVFoundation
@preconcurrency import WebRTC

/// Instantaneous native audio levels, never a transcript or a recording history.
struct RealtimeAudioLevels: Equatable, Sendable {
    var microphone: Double = 0
    var playback: Double = 0

    // Ignore digital silence; a quiet microphone never overrides audible Bot playback.
    var isPlaybackActive: Bool { playback > 0.001 }
    var displayLevel: Double { isPlaybackActive ? playback : microphone }

    mutating func include(type: String, values: [String: NSObject]) {
        guard values["kind"] as? String == "audio",
            let value = (values["audioLevel"] as? NSNumber)?.doubleValue,
            value.isFinite
        else { return }
        let level = min(max(value, 0), 1)
        switch type {
        case "media-source": microphone = max(microphone, level)
        case "inbound-rtp": playback = max(playback, level)
        default: break
        }
    }
}

/// Native media only. The gateway owns authentication, voice control, and conversation history.
@MainActor
@Observable
final class RealtimeVoiceSession: NSObject {
    private(set) var isConnected = false
    private(set) var audioLevels = RealtimeAudioLevels()
    /// How far the current level sits from its own recent average, so the wave reacts to
    /// changes in the voice rather than to loudness alone.
    private(set) var levelFlare = 0.0
    var isMuted = false {
        didSet {
            audioTrack?.isEnabled = !isMuted
            if isMuted { audioLevels.microphone = 0 }
        }
    }
    @ObservationIgnored static let factory: RTCPeerConnectionFactory? = {
        guard RTCInitializeSSL() else { return nil }
        #if os(macOS)
            // AVAudioEngine rebuilds its converters when Bluetooth changes the hardware format.
            return RTCPeerConnectionFactory(
                audioDeviceModuleType: .audioEngine, bypassVoiceProcessing: false,
                encoderFactory: nil, decoderFactory: nil, audioProcessingModule: nil)
        #else
            return RTCPeerConnectionFactory(encoderFactory: nil, decoderFactory: nil)
        #endif
    }()
    @ObservationIgnored var peer: RTCPeerConnection?
    @ObservationIgnored private var audioTrack: RTCAudioTrack?
    @ObservationIgnored private var dataChannel: RTCDataChannel?
    @ObservationIgnored private var failure: ((String) -> Void)?
    @ObservationIgnored private var generation = UUID()
    @ObservationIgnored private var audioIsActive = false
    @ObservationIgnored private var meteringTask: Task<Void, Never>?
    @ObservationIgnored private var disconnectedRecoveryTask: Task<Void, Never>?
    @ObservationIgnored private var levelBaseline = 0.0

    init(onFailure: ((String) -> Void)? = nil) {
        failure = onFailure
        super.init()
    }

    func offer() async throws -> String {
        try Task.checkCancellation()
        let generation = generation
        #if os(macOS)
            let permitted = await AVCaptureDevice.requestAccess(for: .audio)
        #else
            let permitted = await AVAudioApplication.requestRecordPermission()
        #endif
        guard permitted else {
            throw VoiceError.microphonePermission
        }
        try Task.checkCancellation()
        guard self.generation == generation else { throw CancellationError() }
        try activateAudio()
        let configuration = RTCConfiguration()
        configuration.sdpSemantics = .unifiedPlan
        let constraints = RTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: nil)
        guard let factory = Self.factory,
            let peer = factory.peerConnection(
                with: configuration, constraints: constraints, delegate: self
            )
        else { throw VoiceError.connection }
        self.peer = peer
        let track = factory.audioTrack(
            with: factory.audioSource(with: constraints), trackId: "voice"
        )
        audioTrack = track
        track.isEnabled = !isMuted
        peer.add(track, streamIds: ["voice"])
        // Establish SCTP, but all provider events/control stay on the gateway sideband.
        dataChannel = peer.dataChannel(
            forLabel: "oai-events", configuration: RTCDataChannelConfiguration())
        let offer = try await peer.offer(for: constraints)
        try Task.checkCancellation()
        guard self.generation == generation else { throw CancellationError() }
        try await peer.setLocalDescription(offer)
        try Task.checkCancellation()
        guard self.generation == generation else { throw CancellationError() }
        startMetering(peer)
        return offer.sdp
    }

    func accept(answer: String) async throws {
        try Task.checkCancellation()
        guard let peer, !answer.isEmpty, answer.utf8.count <= 256 * 1024 else {
            throw VoiceError.connection
        }
        let generation = generation
        try await peer.setRemoteDescription(RTCSessionDescription(type: .answer, sdp: answer))
        try Task.checkCancellation()
        guard self.generation == generation else { throw CancellationError() }
        try await Task.sleep(for: .seconds(20))
        guard self.generation == generation else { throw CancellationError() }
        if !isConnected { throw VoiceError.connection }
    }

    func close() {
        generation = UUID()
        failure = nil
        meteringTask?.cancel()
        meteringTask = nil
        disconnectedRecoveryTask?.cancel()
        disconnectedRecoveryTask = nil
        audioLevels = RealtimeAudioLevels()
        levelFlare = 0
        levelBaseline = 0
        dataChannel?.close()
        dataChannel = nil
        audioTrack?.isEnabled = false
        audioTrack = nil
        peer?.delegate = nil
        peer?.close()
        peer = nil
        isConnected = false
        isMuted = false
        #if os(iOS)
            guard audioIsActive else { return }
            let audio = RTCAudioSession.sharedInstance()
            audio.remove(self)
            audio.lockForConfiguration()
            defer { audio.unlockForConfiguration() }
            try? audio.setActive(false)
            audioIsActive = false
        #endif
    }

    private func startMetering(_ peer: RTCPeerConnection) {
        let generation = generation
        meteringTask = Task { [weak self] in
            while !Task.isCancelled {
                let report = await peer.statistics()
                guard let self, !Task.isCancelled,
                    self.generation == generation, self.peer === peer
                else { return }
                var levels = RealtimeAudioLevels()
                for statistic in report.statistics.values {
                    levels.include(type: statistic.type, values: statistic.values)
                }
                self.updateAudioLevels(levels)
                // libwebrtc caches each report for 50ms, so this is the fastest poll that
                // returns fresh numbers instead of the previous report again.
                try? await Task.sleep(for: .milliseconds(50))
            }
        }
    }

    func updateAudioLevels(_ levels: RealtimeAudioLevels) {
        audioLevels = levels
        if isMuted { audioLevels.microphone = 0 }
        let level = audioLevels.displayLevel
        levelBaseline += (level - levelBaseline) * 0.12
        // Fast attack, slow release: a flare should read as a swell, not as 20Hz jitter.
        levelFlare = level == 0 ? 0 : max(min(1, abs(level - levelBaseline) * 3), levelFlare * 0.82)
    }

    private func activateAudio() throws {
        #if os(iOS)
            let audio = RTCAudioSession.sharedInstance()
            let configuration = RTCAudioSessionConfiguration.webRTC()
            configuration.category = AVAudioSession.Category.playAndRecord.rawValue
            configuration.mode = AVAudioSession.Mode.voiceChat.rawValue
            configuration.categoryOptions = [.allowBluetoothHFP, .defaultToSpeaker]
            audio.lockForConfiguration()
            defer { audio.unlockForConfiguration() }
            try audio.setConfiguration(configuration, active: true)
            audioIsActive = true
            audio.add(self)
        #endif
    }

    private func scheduleDisconnectedRecovery(for peer: RTCPeerConnection) {
        guard disconnectedRecoveryTask == nil else { return }
        let generation = generation
        disconnectedRecoveryTask = Task { [weak self, weak peer] in
            try? await Task.sleep(for: .seconds(2))
            guard !Task.isCancelled, let self, let peer,
                self.generation == generation,
                self.peer === peer,
                !self.isConnected
            else { return }
            self.disconnectedRecoveryTask = nil
            self.failure?(String(localized: "The voice connection ended."))
        }
    }

    private enum VoiceError: LocalizedError {
        case microphonePermission
        case connection

        var errorDescription: String? {
            switch self {
            case .microphonePermission:
                String(localized: "Allow microphone access in Settings to use voice chat.")
            case .connection: String(localized: "Voice could not connect. Try again.")
            }
        }
    }
}

extension RealtimeVoiceSession: RTCPeerConnectionDelegate {
    nonisolated func peerConnection(
        _ peerConnection: RTCPeerConnection, didChange newState: RTCPeerConnectionState
    ) {
        Task { @MainActor [weak self] in
            guard let self, self.peer === peerConnection else { return }
            self.isConnected = newState == .connected
            switch newState {
            case .connected:
                self.disconnectedRecoveryTask?.cancel()
                self.disconnectedRecoveryTask = nil
            case .disconnected:
                self.scheduleDisconnectedRecovery(for: peerConnection)
            case .failed, .closed:
                self.disconnectedRecoveryTask?.cancel()
                self.disconnectedRecoveryTask = nil
                self.failure?(String(localized: "The voice connection ended."))
            default:
                break
            }
        }
    }

    nonisolated func peerConnection(
        _ peerConnection: RTCPeerConnection, didChange stateChanged: RTCSignalingState
    ) {}
    nonisolated func peerConnection(
        _ peerConnection: RTCPeerConnection, didAdd stream: RTCMediaStream
    ) {}
    nonisolated func peerConnection(
        _ peerConnection: RTCPeerConnection, didRemove stream: RTCMediaStream
    ) {}
    nonisolated func peerConnectionShouldNegotiate(_ peerConnection: RTCPeerConnection) {}
    nonisolated func peerConnection(
        _ peerConnection: RTCPeerConnection, didChange newState: RTCIceConnectionState
    ) {}
    nonisolated func peerConnection(
        _ peerConnection: RTCPeerConnection, didChange newState: RTCIceGatheringState
    ) {}
    nonisolated func peerConnection(
        _ peerConnection: RTCPeerConnection, didGenerate candidate: RTCIceCandidate
    ) {}
    nonisolated func peerConnection(
        _ peerConnection: RTCPeerConnection, didRemove candidates: [RTCIceCandidate]
    ) {}
    nonisolated func peerConnection(
        _ peerConnection: RTCPeerConnection, didOpen dataChannel: RTCDataChannel
    ) {}
}

#if os(iOS)
    extension RealtimeVoiceSession: RTCAudioSessionDelegate {
        nonisolated func audioSessionDidBeginInterruption(_ session: RTCAudioSession) {
            Task { @MainActor [weak self] in
                self?.failure?(String(localized: "Voice was interrupted by another audio session."))
            }
        }
    }
#endif

/// Shared by the iOS composer and the gateway's macOS menu bar.
@Animatable
struct AudioLevelEqualizer: View {
    @AnimatableIgnored @Environment(\.colorScheme) private var colorScheme
    @AnimatableIgnored @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @AnimatableIgnored @Environment(\.scenePhase) private var scenePhase
    var amplitude: Double
    var flare: Double
    @AnimatableIgnored var playbackColor: Color?

    var body: some View {
        TimelineView(
            .animation(
                minimumInterval: 1.0 / 60.0,
                paused: reduceMotion || scenePhase != .active || amplitude == 0
            )
        ) { _ in
            let time =
                reduceMotion || scenePhase != .active ? 0 : ProcessInfo.processInfo.systemUptime
            particles(at: time)
        }
    }

    private func particles(at time: Double) -> some View {
        Canvas(rendersAsynchronously: true) { context, size in
            // A shallow reflection under the line keeps the field from reading as floored.
            let reflection = 0.2
            let ceiling = (size.height - 8) / (1 + reflection)
            let baseline = size.height - 4 - ceiling * reflection
            let step = 4.0
            let columns = 96
            // One mountain that widens outward from the centre as the voice grows.
            // Smaller variance is a sharper peak.
            let variance = 0.02 + 0.12 * amplitude + 0.05 * flare
            for column in 0..<columns {
                let x = Double(column) / Double(columns - 1)
                let centered = x * 2 - 1
                let hump = exp(-centered * centered / (2 * variance))
                // Stable phases and speeds let each needle jitter without frame-to-frame randomness.
                let phase = Double(column) * 2.39996
                let needle =
                    0.15 + 0.85 * pow(abs(sin(time * (2.1 + 0.9 * sin(phase)) + phase)), 1.6)
                let dots = Int(min(1, amplitude * 1.45) * hump * needle * ceiling / step)
                for dot in -Int(Double(dots) * reflection)...dots {
                    let fade = dots == 0 ? 0 : abs(Double(dot)) / Double(dots)
                    let ink =
                        playbackColor
                        ?? Color(
                            white: colorScheme == .dark ? 0.92 - 0.44 * fade : 0.08 + 0.44 * fade)
                    let radius = 0.95 - 0.4 * fade
                    let sway = sin(time * 1.3 + phase) * 1.6 * fade
                    let rect = CGRect(
                        x: x * (size.width - 4) + 2 + sway - radius,
                        y: baseline - Double(dot) * step - radius,
                        width: radius * 2, height: radius * 2
                    )
                    context.fill(
                        Path(ellipseIn: rect),
                        with: .color(ink.opacity((0.35 + 0.65 * (1 - fade)) * (0.3 + 0.7 * hump)))
                    )
                }
            }
        }
    }
}
