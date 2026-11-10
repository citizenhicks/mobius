import Foundation
@preconcurrency import AVFoundation
@preconcurrency import LiveKitWebRTC

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
final class RealtimeVoiceSession: NSObject {
    private(set) var isConnected = false {
        didSet {
            guard isConnected != oldValue else { return }
            // A soft chime as the call starts listening, and another as it ends.
            Cue.play(isConnected ? "VoiceConnect" : "VoiceEnd")
            onConnectionChange?()
        }
    }
    var onConnectionChange: (() -> Void)?
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
    static let factory: LKRTCPeerConnectionFactory? = {
        guard LKRTCInitializeSSL() else { return nil }
        #if os(macOS)
            // AVAudioEngine rebuilds its converters when Bluetooth changes the hardware format.
            return LKRTCPeerConnectionFactory(
                audioDeviceModuleType: .audioEngine, bypassVoiceProcessing: false,
                encoderFactory: nil, decoderFactory: nil, audioProcessingModule: nil)
        #else
            return LKRTCPeerConnectionFactory(encoderFactory: nil, decoderFactory: nil)
        #endif
    }()
    var peer: LKRTCPeerConnection?
    private var audioTrack: LKRTCAudioTrack?
    var dataChannel: LKRTCDataChannel?
    private var failure: ((LocalizedStringResource) -> Void)?
    private let waitForSessionStarted: Bool
    private var transportConnected = false
    private var sessionStarted = false
    private var generation = UUID()
    private var meteringTask: Task<Void, Never>?
    private var disconnectedRecoveryTask: Task<Void, Never>?
    private var levelBaseline = 0.0

    init(
        waitForSessionStarted: Bool = false,
        onFailure: ((LocalizedStringResource) -> Void)? = nil
    ) {
        self.waitForSessionStarted = waitForSessionStarted
        failure = onFailure
        super.init()
    }

    deinit {
        peer?.delegate = nil
        peer?.close()
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
        let configuration = LKRTCConfiguration()
        configuration.sdpSemantics = .unifiedPlan
        let constraints = LKRTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: nil)
        guard let factory = Self.factory,
            let peer = factory.peerConnection(
                with: configuration, constraints: constraints, delegate: self
            )
        else { throw VoiceError.connection }
        self.peer = peer
        // Keep echo/noise removal active without amplifying quiet room noise into speech.
        let audioConstraints = LKRTCMediaConstraints(
            mandatoryConstraints: [
                "googEchoCancellation": "true", "googNoiseSuppression": "true",
                "googAutoGainControl": "false", "googHighpassFilter": "true",
            ], optionalConstraints: nil)
        let track = factory.audioTrack(
            with: factory.audioSource(with: audioConstraints), trackId: "voice"
        )
        audioTrack = track
        track.isEnabled = !isMuted
        peer.add(track, streamIds: ["voice"])
        // Observe provider readiness here; controls and transcripts stay on the gateway sideband.
        dataChannel = peer.dataChannel(
            forLabel: "oai-events", configuration: LKRTCDataChannelConfiguration())
        dataChannel?.delegate = self
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
        guard let peer, !answer.isEmpty else {
            throw VoiceError.connection
        }
        let generation = generation
        try await peer.setRemoteDescription(LKRTCSessionDescription(type: .answer, sdp: answer))
        try Task.checkCancellation()
        guard self.generation == generation else { throw CancellationError() }
        let deadline = ContinuousClock.now.advanced(by: .seconds(20))
        while !isConnected {
            guard self.generation == generation else { throw CancellationError() }
            guard ContinuousClock.now < deadline else { throw VoiceError.connection }
            try await Task.sleep(for: .milliseconds(50))
        }
        try Task.checkCancellation()
        guard self.generation == generation else { throw CancellationError() }
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
        (VoiceMeter.levels, VoiceMeter.flare) = (audioLevels, levelFlare)
        dataChannel?.delegate = nil
        dataChannel?.close()
        dataChannel = nil
        audioTrack?.isEnabled = false
        audioTrack = nil
        peer?.delegate = nil
        peer?.close()
        peer = nil
        transportConnected = false
        sessionStarted = false
        isConnected = false
        isMuted = false
    }

    private func startMetering(_ peer: LKRTCPeerConnection) {
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

    #if DEBUG
        func previewConnection() { isConnected = true }
    #endif

    func updateAudioLevels(_ levels: RealtimeAudioLevels) {
        audioLevels = levels
        if isMuted { audioLevels.microphone = 0 }
        let level = audioLevels.displayLevel
        levelBaseline += (level - levelBaseline) * 0.12
        // Fast attack, slow release: a flare should read as a swell, not as 20Hz jitter.
        levelFlare = level == 0 ? 0 : max(min(1, abs(level - levelBaseline) * 3), levelFlare * 0.82)
        (VoiceMeter.levels, VoiceMeter.flare) = (audioLevels, levelFlare)
    }

    #if os(iOS)
        static func prepareSystemCallAudio() throws {
            let audio = LKRTCAudioSession.sharedInstance()
            let configuration = LKRTCAudioSessionConfiguration.webRTC()
            configuration.category = AVAudioSession.Category.playAndRecord.rawValue
            configuration.mode = AVAudioSession.Mode.voiceChat.rawValue
            configuration.categoryOptions = [.allowBluetoothHFP, .defaultToSpeaker]
            audio.useManualAudio = true
            audio.isAudioEnabled = false
            audio.lockForConfiguration()
            defer { audio.unlockForConfiguration() }
            try audio.setConfiguration(configuration)
        }

        static func systemCallAudioDidActivate(_ session: AVAudioSession) {
            let audio = LKRTCAudioSession.sharedInstance()
            audio.audioSessionDidActivate(session)
            audio.isAudioEnabled = true
        }

        static func systemCallAudioDidDeactivate(_ session: AVAudioSession) {
            let audio = LKRTCAudioSession.sharedInstance()
            audio.isAudioEnabled = false
            audio.audioSessionDidDeactivate(session)
        }
    #endif

    private func scheduleDisconnectedRecovery(for peer: LKRTCPeerConnection) {
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
            self.failure?("The voice connection ended.")
        }
    }

    private func updateConnectionReadiness() {
        isConnected = transportConnected && (!waitForSessionStarted || sessionStarted)
    }

    enum VoiceError: LocalizedError {
        case microphonePermission
        case connection

        var localizedDescriptionResource: LocalizedStringResource {
            switch self {
            case .microphonePermission:
                "Allow microphone access in Settings to use voice chat."
            case .connection: "Voice could not connect. Try again."
            }
        }

        var errorDescription: String? { String(localized: localizedDescriptionResource) }
    }
}

extension RealtimeVoiceSession: LKRTCPeerConnectionDelegate {
    nonisolated func peerConnection(
        _ peerConnection: LKRTCPeerConnection, didChange newState: LKRTCPeerConnectionState
    ) {
        Task { @MainActor [weak self] in
            guard let self, self.peer === peerConnection else { return }
            self.transportConnected = newState == .connected
            if newState == .failed || newState == .closed {
                self.sessionStarted = false
                self.dataChannel?.delegate = nil
                self.dataChannel?.close()
                self.dataChannel = nil
            }
            self.updateConnectionReadiness()
            switch newState {
            case .connected:
                self.disconnectedRecoveryTask?.cancel()
                self.disconnectedRecoveryTask = nil
            case .disconnected:
                self.scheduleDisconnectedRecovery(for: peerConnection)
            case .failed, .closed:
                self.disconnectedRecoveryTask?.cancel()
                self.disconnectedRecoveryTask = nil
                self.failure?("The voice connection ended.")
            default:
                break
            }
        }
    }

    nonisolated func peerConnection(
        _ peerConnection: LKRTCPeerConnection, didChange stateChanged: LKRTCSignalingState
    ) {}
    nonisolated func peerConnection(
        _ peerConnection: LKRTCPeerConnection, didAdd stream: LKRTCMediaStream
    ) {}
    nonisolated func peerConnection(
        _ peerConnection: LKRTCPeerConnection, didRemove stream: LKRTCMediaStream
    ) {}
    nonisolated func peerConnectionShouldNegotiate(_ peerConnection: LKRTCPeerConnection) {}
    nonisolated func peerConnection(
        _ peerConnection: LKRTCPeerConnection, didChange newState: LKRTCIceConnectionState
    ) {}
    nonisolated func peerConnection(
        _ peerConnection: LKRTCPeerConnection, didChange newState: LKRTCIceGatheringState
    ) {}
    nonisolated func peerConnection(
        _ peerConnection: LKRTCPeerConnection, didGenerate candidate: LKRTCIceCandidate
    ) {}
    nonisolated func peerConnection(
        _ peerConnection: LKRTCPeerConnection, didRemove candidates: [LKRTCIceCandidate]
    ) {}
    nonisolated func peerConnection(
        _ peerConnection: LKRTCPeerConnection, didOpen dataChannel: LKRTCDataChannel
    ) {}
}

extension RealtimeVoiceSession: LKRTCDataChannelDelegate {
    nonisolated func dataChannelDidChangeState(_ dataChannel: LKRTCDataChannel) {
        let state = dataChannel.readyState
        Task { @MainActor [weak self] in
            guard let self, self.dataChannel === dataChannel,
                self.waitForSessionStarted, state == .closed
            else { return }
            self.sessionStarted = false
            self.dataChannel?.delegate = nil
            self.dataChannel = nil
            self.updateConnectionReadiness()
            self.failure?("The voice connection ended.")
        }
    }

    nonisolated func dataChannel(
        _ dataChannel: LKRTCDataChannel, didReceiveMessageWith buffer: LKRTCDataBuffer
    ) {
        guard !buffer.isBinary, buffer.data.count <= 256 * 1024 else { return }
        let data = buffer.data
        Task { @MainActor [weak self] in
            guard let self, self.dataChannel === dataChannel, self.peer != nil,
                self.waitForSessionStarted,
                let event = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                event["type"] as? String == "session.started"
            else { return }
            self.sessionStarted = true
            self.updateConnectionReadiness()
        }
    }
}
