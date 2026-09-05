import Foundation
import Observation
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
              value.isFinite else { return }
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
    var isMuted = false {
        didSet {
            audioTrack?.isEnabled = !isMuted
            if isMuted { audioLevels.microphone = 0 }
        }
    }
    @ObservationIgnored private static let factory: RTCPeerConnectionFactory? = {
        guard RTCInitializeSSL() else { return nil }
        return RTCPeerConnectionFactory(encoderFactory: nil, decoderFactory: nil)
    }()
    @ObservationIgnored private var peer: RTCPeerConnection?
    @ObservationIgnored private var audioTrack: RTCAudioTrack?
    @ObservationIgnored private var dataChannel: RTCDataChannel?
    @ObservationIgnored private var failure: ((String) -> Void)?
    @ObservationIgnored private var generation = UUID()
    @ObservationIgnored private var audioIsActive = false
    @ObservationIgnored private var meteringTask: Task<Void, Never>?

    init(onFailure: ((String) -> Void)? = nil) {
        failure = onFailure
        super.init()
    }

    func offer() async throws -> String {
        try Task.checkCancellation()
        let generation = generation
        guard await AVAudioApplication.requestRecordPermission() else {
            throw VoiceError.microphonePermission
        }
        try Task.checkCancellation()
        guard self.generation == generation else { throw CancellationError() }
        try activateAudio()
        let configuration = RTCConfiguration()
        configuration.sdpSemantics = .unifiedPlan
        let constraints = RTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: nil)
        guard let factory = Self.factory, let peer = factory.peerConnection(
            with: configuration, constraints: constraints, delegate: self
        ) else { throw VoiceError.connection }
        self.peer = peer
        let track = factory.audioTrack(
            with: factory.audioSource(with: constraints), trackId: "voice"
        )
        audioTrack = track
        track.isEnabled = !isMuted
        peer.add(track, streamIds: ["voice"])
        // Establish SCTP, but all provider events/control stay on the gateway sideband.
        dataChannel = peer.dataChannel(forLabel: "oai-events", configuration: RTCDataChannelConfiguration())
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
        audioLevels = RealtimeAudioLevels()
        dataChannel?.close()
        dataChannel = nil
        audioTrack?.isEnabled = false
        audioTrack = nil
        peer?.delegate = nil
        peer?.close()
        peer = nil
        isConnected = false
        isMuted = false
        guard audioIsActive else { return }
        let audio = RTCAudioSession.sharedInstance()
        audio.remove(self)
        audio.lockForConfiguration()
        defer { audio.unlockForConfiguration() }
        try? audio.setActive(false)
        audioIsActive = false
    }

    private func startMetering(_ peer: RTCPeerConnection) {
        let generation = generation
        meteringTask = Task { [weak self] in
            while !Task.isCancelled {
                let report = await peer.statistics()
                guard let self, !Task.isCancelled,
                      self.generation == generation, self.peer === peer else { return }
                var levels = RealtimeAudioLevels()
                for statistic in report.statistics.values {
                    levels.include(type: statistic.type, values: statistic.values)
                }
                self.updateAudioLevels(levels)
                try? await Task.sleep(for: .milliseconds(100))
            }
        }
    }

    func updateAudioLevels(_ levels: RealtimeAudioLevels) {
        audioLevels = levels
        if isMuted { audioLevels.microphone = 0 }
    }

    private func activateAudio() throws {
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
    }

    private enum VoiceError: LocalizedError {
        case microphonePermission
        case connection

        var errorDescription: String? {
            switch self {
            case .microphonePermission: String(localized: "Allow microphone access in Settings to use voice chat.")
            case .connection: String(localized: "Voice could not connect. Try again.")
            }
        }
    }
}

extension RealtimeVoiceSession: RTCPeerConnectionDelegate, RTCAudioSessionDelegate {
    nonisolated func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCPeerConnectionState) {
        Task { @MainActor [weak self] in
            guard let self, self.peer === peerConnection else { return }
            self.isConnected = newState == .connected
            if newState == .failed || newState == .disconnected || newState == .closed {
                self.failure?(String(localized: "The voice connection ended."))
            }
        }
    }

    nonisolated func audioSessionDidBeginInterruption(_ session: RTCAudioSession) {
        Task { @MainActor [weak self] in
            self?.failure?(String(localized: "Voice was interrupted by another audio session."))
        }
    }

    nonisolated func peerConnection(_ peerConnection: RTCPeerConnection, didChange stateChanged: RTCSignalingState) {}
    nonisolated func peerConnection(_ peerConnection: RTCPeerConnection, didAdd stream: RTCMediaStream) {}
    nonisolated func peerConnection(_ peerConnection: RTCPeerConnection, didRemove stream: RTCMediaStream) {}
    nonisolated func peerConnectionShouldNegotiate(_ peerConnection: RTCPeerConnection) {}
    nonisolated func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceConnectionState) {}
    nonisolated func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceGatheringState) {}
    nonisolated func peerConnection(_ peerConnection: RTCPeerConnection, didGenerate candidate: RTCIceCandidate) {}
    nonisolated func peerConnection(_ peerConnection: RTCPeerConnection, didRemove candidates: [RTCIceCandidate]) {}
    nonisolated func peerConnection(_ peerConnection: RTCPeerConnection, didOpen dataChannel: RTCDataChannel) {}
}
