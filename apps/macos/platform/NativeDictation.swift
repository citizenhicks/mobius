import AVFoundation
import AppKit
import Carbon
import Foundation
@preconcurrency import Speech

/// Short cues bundled in the app's Resources, matching iOS: dictation start and stop,
/// and a voice call starting to listen and ending. An unbundled build stays silent.
enum Cue {
    @MainActor private static var playing: [NSSound] = []

    @MainActor static func play(_ name: String) {
        guard let url = Bundle.main.url(forResource: name, withExtension: "wav"),
            let sound = NSSound(contentsOf: url, byReference: true)
        else { return }
        // NSSound stops when released, so it is held until the next cue.
        playing = [sound]
        sound.play()
    }
}

/// On-device dictation in the user's language. No audio leaves the Mac.
@MainActor
final class NativeDictation {
    /// One engine for the app's lifetime: releasing an engine while CoreAudio still has a
    /// queued IO-unit property callback for it crashes (AVAudioIOUnit::IOUnitPropertyListener).
    private lazy var engine = AVAudioEngine()
    private var tapped = false
    /// Whether the current recording produced any text.
    private var heard = false
    /// The dictation model for the user's language as last read, for the Settings status:
    /// 0 unsupported, 1 not downloaded, 2 downloading, 3 installed.
    private(set) static var model: UInt8 = 0
    private var analyzer: SpeechAnalyzer?
    private var input: AsyncStream<AnalyzerInput>.Continuation?
    private var transcription: Task<Void, Never>?
    private var silence: Task<Void, Never>?
    private var generation = UUID()
    private var inputObserver: NSObjectProtocol?
    private let emit: (JSONValue) -> Void

    init(emit: @escaping (JSONValue) -> Void) { self.emit = emit }

    func start() {
        reset()
        let generation = UUID()
        self.generation = generation
        emit(.object(["type": .string("dictation_state"), "active": .bool(true)]))
        Task {
            guard self.generation == generation else { return }
            // macOS kills a process that asks for these permissions without usage
            // descriptions, which is what an unbundled `cargo run` binary is.
            let keys = ["NSMicrophoneUsageDescription", "NSSpeechRecognitionUsageDescription"]
            guard keys.allSatisfy({ Bundle.main.object(forInfoDictionaryKey: $0) != nil }) else {
                fail("Dictation needs the bundled app. Launch möbius.app instead.")
                return
            }
            let status = await Self.speechAuthorization()
            guard self.generation == generation else { return }
            guard status == .authorized else {
                fail("Allow Speech Recognition to dictate messages.")
                return
            }
            let canRecord = await AVCaptureDevice.requestAccess(for: .audio)
            guard self.generation == generation else { return }
            guard canRecord else {
                fail("Allow microphone access to dictate messages.")
                return
            }
            do { try await record(generation: generation) } catch {
                guard self.generation == generation else { return }
                fail(error.localizedDescription)
            }
        }
    }

    /// Nonisolated so TCC's reply, which arrives off the main queue, isn't asserted to be
    /// on the main actor (that assertion aborts the process).
    nonisolated private static func speechAuthorization() async
        -> SFSpeechRecognizerAuthorizationStatus
    {
        await withCheckedContinuation { continuation in
            SFSpeechRecognizer.requestAuthorization { continuation.resume(returning: $0) }
        }
    }

    private func record(generation: UUID) async throws {
        guard let transcriber = await Self.transcriber() else {
            throw DesktopError("On-device dictation doesn’t support your language yet.")
        }
        if let installation = try await AssetInventory.assetInstallationRequest(
            supporting: [transcriber])
        {
            Self.model = 2
            defer { Self.refreshModel() }
            do { try await installation.downloadAndInstall() } catch {
                throw DesktopError("The dictation model couldn’t download. Try again.")
            }
        }
        guard
            let format = await SpeechAnalyzer.bestAvailableAudioFormat(
                compatibleWith: [transcriber])
        else { throw DesktopError("On-device dictation is unavailable on this Mac.") }
        let analyzer = SpeechAnalyzer(modules: [transcriber])
        try await analyzer.prepareToAnalyze(in: format)
        guard self.generation == generation else { return }
        let (sequence, input) = AsyncStream.makeStream(of: AnalyzerInput.self)
        self.analyzer = analyzer
        self.input = input
        try startEngine(analyzing: format, into: input)
        // AirPods connecting, or any other input switch, stops the engine and changes the
        // input's format; recording carries on from the new input.
        inputObserver = NotificationCenter.default.addObserver(
            forName: .AVAudioEngineConfigurationChange, object: engine, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.generation == generation, self.tapped else { return }
                do { try self.startEngine(analyzing: format, into: input) } catch {
                    self.fail(error.localizedDescription)
                }
            }
        }
        try await analyzer.start(inputSequence: sequence)
        guard self.generation == generation else { return }
        Cue.play("DictationStart")
        endAfterSilence(seconds: 10, generation: generation)
        transcription = Task { [weak self] in
            var finalized = ""
            do {
                for try await result in transcriber.results {
                    guard let self, self.generation == generation else { return }
                    let text = String(result.text.characters)
                    let spoken = (finalized + text).trimmingCharacters(in: .whitespaces)
                    if result.isFinal { finalized += text }
                    heard = heard || !spoken.isEmpty
                    emit(
                        .object([
                            "type": .string("dictation"),
                            "text": .string(spoken),
                            "is_final": .bool(false),
                        ]))
                    if tapped { endAfterSilence(seconds: 3, generation: generation) }
                }
            } catch {
                guard let self, self.generation == generation else { return }
                fail(error.localizedDescription)
            }
        }
    }

    static func refreshModel() {
        Task {
            guard let transcriber = await transcriber() else {
                model = 0
                return
            }
            model =
                switch await AssetInventory.status(forModules: [transcriber]) {
                case .unsupported: 0
                case .supported: 1
                case .downloading: 2
                case .installed: 3
                @unknown default: 0
                }
        }
    }

    /// A transcriber for the keyboard layout's language, else the system's, or nil where
    /// dictation doesn't support it.
    private static func transcriber() async -> DictationTranscriber? {
        await DictationTranscriber.supportedLocale(equivalentTo: keyboardLocale ?? .current).map {
            DictationTranscriber(locale: $0, preset: .progressiveLongDictation)
        }
    }

    /// The selected keyboard layout's language, the way switching layouts switches typing.
    private static var keyboardLocale: Locale? {
        let source = TISCopyCurrentKeyboardInputSource().takeRetainedValue()
        guard let languages = TISGetInputSourceProperty(source, kTISPropertyInputSourceLanguages)
        else { return nil }
        let list = Unmanaged<CFArray>.fromOpaque(languages).takeUnretainedValue() as? [String]
        return list?.first.map(Locale.init(identifier:))
    }

    /// Ends the recording once nothing new has been heard for a while.
    private func endAfterSilence(seconds: Double, generation: UUID) {
        silence?.cancel()
        silence = Task { [weak self] in
            try? await Task.sleep(for: .seconds(seconds))
            guard !Task.isCancelled, let self, self.generation == generation else { return }
            if heard { finish() } else { fail("No speech detected.") }
        }
    }

    /// Taps the engine's current input; a converter adapts each input format to the
    /// analyzer's, so an input switch needs only a fresh tap.
    private func startEngine(
        analyzing format: AVAudioFormat, into input: AsyncStream<AnalyzerInput>.Continuation
    ) throws {
        if engine.isRunning { engine.stop() }
        if tapped { engine.inputNode.removeTap(onBus: 0) }
        tapped = false
        try Self.installTap(on: engine.inputNode, analyzing: format, into: input)
        tapped = true
        engine.prepare()
        try engine.start()
    }

    /// The tap runs on the audio render thread, so its closure must not be main-actor code.
    nonisolated private static func installTap(
        on node: AVAudioInputNode, analyzing analyzerFormat: AVAudioFormat,
        into input: AsyncStream<AnalyzerInput>.Continuation
    ) throws {
        let format = node.outputFormat(forBus: 0)
        guard format.sampleRate > 0, format.channelCount > 0,
            let converter = AVAudioConverter(from: format, to: analyzerFormat)
        else { throw DesktopError("No microphone input is available.") }
        node.installTap(onBus: 0, bufferSize: 1024, format: format) { buffer, _ in
            DictationMeter.shared.record(buffer)
            if let converted = convert(buffer, with: converter) {
                input.yield(AnalyzerInput(buffer: converted))
            }
        }
    }

    nonisolated private static func convert(
        _ buffer: AVAudioPCMBuffer, with converter: AVAudioConverter
    ) -> AVAudioPCMBuffer? {
        let ratio = converter.outputFormat.sampleRate / converter.inputFormat.sampleRate
        let capacity = AVAudioFrameCount((Double(buffer.frameLength) * ratio).rounded(.up))
        guard
            let output = AVAudioPCMBuffer(
                pcmFormat: converter.outputFormat, frameCapacity: capacity)
        else { return nil }
        var supplied = false
        let status = converter.convert(to: output, error: nil) { _, inputStatus in
            inputStatus.pointee = supplied ? .noDataNow : .haveData
            defer { supplied = true }
            return supplied ? nil : buffer
        }
        return status == .error || output.frameLength == 0 ? nil : output
    }

    /// Stops listening but lets the analyzer deliver the words it already heard; the inactive
    /// state follows once they are in, or after a short deadline so nothing hangs.
    func finish() {
        guard tapped, let analyzer, let transcription else { return stop() }
        let generation = generation
        Cue.play("DictationStop")
        releaseMicrophone()
        Task { [weak self] in
            let deadline = Task {
                try await Task.sleep(for: .seconds(3))
                await analyzer.cancelAndFinishNow()
            }
            try? await analyzer.finalizeAndFinishThroughEndOfInput()
            deadline.cancel()
            await transcription.value
            guard let self, self.generation == generation else { return }
            stop()
        }
    }

    /// Stops at once and drops anything not yet transcribed.
    func stop() {
        if tapped { Cue.play("DictationStop") }
        reset()
        emit(.object(["type": .string("dictation_state"), "active": .bool(false)]))
    }

    private func releaseMicrophone() {
        if let inputObserver { NotificationCenter.default.removeObserver(inputObserver) }
        inputObserver = nil
        DictationMeter.shared.reset()
        silence?.cancel()
        silence = nil
        if tapped {
            engine.stop()
            engine.inputNode.removeTap(onBus: 0)
            tapped = false
        }
        input?.finish()
        input = nil
    }

    private func reset() {
        generation = UUID()
        heard = false
        releaseMicrophone()
        transcription?.cancel()
        transcription = nil
        if let analyzer { Task { await analyzer.cancelAndFinishNow() } }
        analyzer = nil
    }

    private func fail(_ message: String) {
        stop()
        emit(.error(message))
    }
}

/// Permission checks never request access; starting a recording owns that user interaction.
/// Bits 4–5 carry the dictation model's last-read status (NativeDictation.model); each call
/// refreshes it for the next.
@_cdecl("mobius_native_audio_status")
public func nativeAudioStatus() -> UInt8 {
    MainActor.assumeIsolated {
        let microphone: UInt8
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .notDetermined: microphone = 0
        case .denied: microphone = 1
        case .restricted: microphone = 2
        case .authorized: microphone = 3
        @unknown default: microphone = 0
        }
        let speech: UInt8
        switch SFSpeechRecognizer.authorizationStatus() {
        case .notDetermined: speech = 0
        case .denied: speech = 1
        case .restricted: speech = 2
        case .authorized: speech = 3
        @unknown default: speech = 0
        }
        NativeDictation.refreshModel()
        return microphone | (speech << 2) | (NativeDictation.model << 4)
    }
}

/// The recording's loudness for the composer's waveform: one peak per 100 ms, the last 128
/// (DictationController.recordAudioLevel). The tap writes from the audio thread.
final class DictationMeter: @unchecked Sendable {
    static let shared = DictationMeter()
    private let lock = NSLock()
    private var levels: [Double] = []
    private var recorded = 0
    private var peak = 0.0
    private var duration = 0.0

    func reset() {
        lock.withLock {
            levels.removeAll()
            recorded = 0
            peak = 0
            duration = 0
        }
    }

    func record(_ buffer: AVAudioPCMBuffer) {
        let level = Self.level(in: buffer)
        let seconds = Double(buffer.frameLength) / buffer.format.sampleRate
        lock.withLock {
            peak = max(peak, level)
            duration += seconds
            guard duration >= 0.1 else { return }
            levels.append(peak)
            recorded += 1
            if levels.count > 128 { levels.removeFirst(levels.count - 128) }
            duration = duration.truncatingRemainder(dividingBy: 0.1)
            peak = 0
        }
    }

    /// The newest levels and how many samples the recording has made in all.
    var history: (levels: [Double], recorded: Int) { lock.withLock { (levels, recorded) } }

    /// Speech loudness in decibels mapped to 0...1; linear amplitude hides normal speech.
    static func level(in buffer: AVAudioPCMBuffer) -> Double {
        guard let samples = buffer.floatChannelData?.pointee, buffer.frameLength > 0 else {
            return 0
        }
        let count = Int(buffer.frameLength)
        let power = UnsafeBufferPointer(start: samples, count: count).reduce(0.0) {
            $0 + Double($1 * $1)
        }
        guard power.isFinite, power > 0 else { return 0 }
        let decibels = 10 * log10(power / Double(count))
        return min(1, max(0, (decibels + 60) / 50))
    }
}

/// Copies the newest dictation levels, oldest first, into `out` and the recording's
/// sample total into `recorded`; returns how many levels it copied.
@_cdecl("mobius_native_dictation_levels")
public func nativeDictationLevels(
    _ out: UnsafeMutablePointer<Double>, _ capacity: Int, _ recorded: UnsafeMutablePointer<Int>
) -> Int {
    let history = DictationMeter.shared.history
    let levels = history.levels.suffix(max(0, capacity))
    for (index, level) in levels.enumerated() { out[index] = level }
    recorded.pointee = history.recorded
    return levels.count
}
