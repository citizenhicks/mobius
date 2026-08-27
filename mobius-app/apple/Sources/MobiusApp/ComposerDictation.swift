@preconcurrency import AVFoundation
import Foundation
import Observation
import Speech

@MainActor
@Observable
final class ComposerDictation {
    private static let waveformSampleCount = 32
    nonisolated static var requestedLocales: [Locale] {
        [
            Locale(identifier: "en_US"),
            Locale(identifier: "fr_FR"),
            Locale.current,
        ]
    }

    enum State: Equatable {
        case idle
        case preparing
        case recording
        case stopping
    }

    private(set) var state = State.idle
    private(set) var audioLevels = Array(repeating: 0.0, count: waveformSampleCount)

    @ObservationIgnored private var audioEngine: AVAudioEngine?
    @ObservationIgnored private var audioContinuation: AsyncStream<AVAudioPCMBuffer>.Continuation?
    @ObservationIgnored private var inputContinuation: AsyncStream<AnalyzerInput>.Continuation?
    @ObservationIgnored private var analyzer: SpeechAnalyzer?
    @ObservationIgnored private var feedTask: Task<Void, Never>?
    @ObservationIgnored private var recognitionTasks: [Task<Void, Never>] = []
    @ObservationIgnored private var workerFailure: ComposerDictationError?
    @ObservationIgnored private var hasAudioTap = false
    @ObservationIgnored private var generation = 0
    @ObservationIgnored private var baseText = ""
    @ObservationIgnored private var separator = ""
    private var transcripts: [ComposerDictationTranscript] = []
    private var selectedTranscriptIndex = 0
    @ObservationIgnored private var updateText: ((String) -> Void)?
    @ObservationIgnored private var reportError: ((String) -> Void)?

    var isActive: Bool { state != .idle }
    var isRecording: Bool { state == .recording }
    var isTransitioning: Bool { state == .preparing || state == .stopping }
    var canToggle: Bool { state == .idle || state == .recording }
    var detectedLanguageCode: String? {
        guard transcripts.indices.contains(selectedTranscriptIndex),
              !transcripts[selectedTranscriptIndex].text.isEmpty,
              let languageCode = transcripts[selectedTranscriptIndex]
                .locale.language.languageCode?.identifier
        else { return nil }
        return languageCode.uppercased()
    }

    func start(
        existingText: String,
        updateText: @escaping (String) -> Void,
        reportError: @escaping (String) -> Void
    ) async throws {
        guard state == .idle else { return }
        state = .preparing
        generation += 1
        let currentGeneration = generation
        baseText = existingText
        separator = existingText.isEmpty || existingText.last?.isWhitespace == true ? "" : " "
        audioLevels = Array(repeating: 0, count: Self.waveformSampleCount)
        transcripts = []
        selectedTranscriptIndex = 0
        self.updateText = updateText
        self.reportError = reportError
        workerFailure = nil

        do {
            guard await AVAudioApplication.requestRecordPermission() else {
                throw ComposerDictationError.microphoneDenied
            }
            try checkGeneration(currentGeneration)

            let locales = await supportedLocales()
            guard !locales.isEmpty else {
                throw ComposerDictationError.unsupportedLanguage
            }
            try checkGeneration(currentGeneration)

            transcripts = locales.map(ComposerDictationTranscript.init(locale:))
            var preset = DictationTranscriber.Preset.progressiveShortDictation
            preset.attributeOptions.insert(.transcriptionConfidence)
            let transcribers = locales.map { locale in
                DictationTranscriber(locale: locale, preset: preset)
            }
            let modules: [any SpeechModule] = transcribers
            if let installation = try await AssetInventory.assetInstallationRequest(
                supporting: modules
            ) {
                try await installation.downloadAndInstall()
            }
            try checkGeneration(currentGeneration)

            guard let analyzerFormat = await SpeechAnalyzer.bestAvailableAudioFormat(
                compatibleWith: modules
            ) else {
                throw ComposerDictationError.audioUnavailable
            }
            try checkGeneration(currentGeneration)

            let analyzer = SpeechAnalyzer(modules: modules)
            let (inputStream, inputContinuation) = AsyncStream<AnalyzerInput>.makeStream()
            self.analyzer = analyzer
            self.inputContinuation = inputContinuation
            recognitionTasks = transcribers.enumerated().map { index, transcriber in
                Task { [weak self] in
                    do {
                        for try await result in transcriber.results {
                            guard !Task.isCancelled, let self else { return }
                            self.consume(result, transcriptIndex: index)
                        }
                    } catch is CancellationError {
                        return
                    } catch {
                        await self?.workerFailed(.transcriptionFailed)
                    }
                }
            }
            try await analyzer.start(inputSequence: inputStream)
            try checkGeneration(currentGeneration)

            let audioSession = AVAudioSession.sharedInstance()
            try audioSession.setCategory(.playAndRecord, mode: .spokenAudio)
            try audioSession.setActive(true, options: .notifyOthersOnDeactivation)

            let engine = AVAudioEngine()
            let inputNode = engine.inputNode
            let inputFormat = inputNode.outputFormat(forBus: 0)
            guard inputFormat.sampleRate > 0, inputFormat.channelCount > 0 else {
                throw ComposerDictationError.audioUnavailable
            }

            let (audioStream, audioContinuation) = AsyncStream<AVAudioPCMBuffer>.makeStream()
            self.audioContinuation = audioContinuation
            inputNode.installTap(
                onBus: 0,
                bufferSize: 2_048,
                format: inputFormat
            ) { buffer, _ in
                audioContinuation.yield(buffer)
            }
            hasAudioTap = true
            audioEngine = engine
            let feed = Task.detached(priority: .userInitiated) { [weak self] in
                let converter = ComposerAudioBufferConverter()
                defer { inputContinuation.finish() }
                do {
                    for await buffer in audioStream {
                        try Task.checkCancellation()
                        let level = ComposerAudioMeter.normalizedLevel(in: buffer)
                        await self?.recordAudioLevel(level)
                        let converted = try converter.convert(buffer, to: analyzerFormat)
                        inputContinuation.yield(AnalyzerInput(buffer: converted))
                    }
                } catch is CancellationError {
                    return
                } catch {
                    await self?.workerFailed(.conversionFailed)
                }
            }
            feedTask = feed
            engine.prepare()
            try engine.start()
            try checkGeneration(currentGeneration)
            state = .recording
        } catch {
            let workerFailure = workerFailure
            await cancel()
            throw workerFailure ?? error
        }
    }

    func stop() async throws {
        guard state != .idle else { return }
        guard state == .recording else {
            await cancel()
            return
        }
        state = .stopping
        generation += 1
        audioEngine?.stop()
        removeAudioTap()
        audioContinuation?.finish()

        do {
            await feedTask?.value
            try checkWorkerFailure()
            inputContinuation?.finish()
            try await analyzer?.finalizeAndFinishThroughEndOfInput()
            for task in recognitionTasks {
                await task.value
            }
            try checkWorkerFailure()
            finish()
        } catch {
            await cancel()
            throw error
        }
    }

    func cancel() async {
        await cancel(keepingFinalizedText: true)
    }

    func discard() async {
        await cancel(keepingFinalizedText: false)
    }

    private func cancel(keepingFinalizedText: Bool) async {
        guard state != .idle else { return }
        state = .stopping
        generation += 1
        updateText?(keepingFinalizedText ? renderedText(includeVolatile: false) : baseText)
        updateText = nil
        audioEngine?.stop()
        removeAudioTap()
        audioContinuation?.finish()
        feedTask?.cancel()
        inputContinuation?.finish()
        await analyzer?.cancelAndFinishNow()
        recognitionTasks.forEach { $0.cancel() }
        finish()
    }

    private func consume(_ result: DictationTranscriber.Result, transcriptIndex: Int) {
        guard transcripts.indices.contains(transcriptIndex) else { return }
        transcripts[transcriptIndex].consume(
            text: String(result.text.characters),
            confidence: result.text.transcriptionConfidence,
            isFinal: result.isFinal
        )
        selectedTranscriptIndex = ComposerDictationTranscript.bestIndex(
            in: transcripts,
            keeping: selectedTranscriptIndex
        )
        updateText?(renderedText(includeVolatile: true))
    }

    private func workerFailed(_ failure: ComposerDictationError) async {
        guard state != .idle else { return }
        workerFailure = failure
        guard state != .stopping else { return }
        let reportError = reportError
        let wasPreparing = state == .preparing
        await cancel()
        if !wasPreparing {
            reportError?(failure.localizedDescription)
        }
    }

    private func renderedText(includeVolatile: Bool) -> String {
        guard transcripts.indices.contains(selectedTranscriptIndex) else { return baseText }
        let transcript = includeVolatile
            ? transcripts[selectedTranscriptIndex].text
            : transcripts[selectedTranscriptIndex].finalizedText
        return transcript.isEmpty ? baseText : baseText + separator + transcript
    }

    private func supportedLocales() async -> [Locale] {
        var locales: [Locale] = []
        for requested in Self.requestedLocales {
            guard let supported = await DictationTranscriber.supportedLocale(
                equivalentTo: requested
            ), !locales.contains(where: {
                $0.identifier(.bcp47) == supported.identifier(.bcp47)
            }) else { continue }
            locales.append(supported)
        }
        return locales
    }

    private func recordAudioLevel(_ level: Double) {
        audioLevels.append(level)
        audioLevels.removeFirst(max(0, audioLevels.count - Self.waveformSampleCount))
    }

    private func checkGeneration(_ expected: Int) throws {
        guard generation == expected else { throw CancellationError() }
    }

    private func checkWorkerFailure() throws {
        if let workerFailure {
            throw workerFailure
        }
    }

    private func removeAudioTap() {
        guard hasAudioTap else { return }
        audioEngine?.inputNode.removeTap(onBus: 0)
        hasAudioTap = false
    }

    private func finish() {
        try? AVAudioSession.sharedInstance().setActive(
            false,
            options: .notifyOthersOnDeactivation
        )
        audioEngine = nil
        audioContinuation = nil
        inputContinuation = nil
        analyzer = nil
        feedTask = nil
        recognitionTasks = []
        hasAudioTap = false
        updateText = nil
        reportError = nil
        state = .idle
    }
}

struct ComposerDictationTranscript {
    let locale: Locale
    private(set) var finalizedText = ""
    private(set) var volatileText = ""
    private var finalizedConfidenceTotal = 0.0
    private var finalizedConfidenceWeight = 0
    private var volatileConfidence: Double?
    private var volatileConfidenceWeight = 0

    init(locale: Locale) {
        self.locale = locale
    }

    var text: String { finalizedText + volatileText }

    private var score: Double {
        guard !text.isEmpty else { return -.infinity }
        let volatileTotal = (volatileConfidence ?? 0) * Double(volatileConfidenceWeight)
        let weight = finalizedConfidenceWeight + volatileConfidenceWeight
        return weight == 0 ? 0 : (finalizedConfidenceTotal + volatileTotal) / Double(weight)
    }

    mutating func consume(text: String, confidence: Double?, isFinal: Bool) {
        let weight = text.count
        if isFinal {
            finalizedText += text
            finalizedConfidenceTotal += (confidence ?? 0) * Double(weight)
            finalizedConfidenceWeight += weight
            volatileText = ""
            volatileConfidence = nil
            volatileConfidenceWeight = 0
        } else {
            volatileText = text
            volatileConfidence = confidence
            volatileConfidenceWeight = weight
        }
    }

    static func bestIndex(
        in transcripts: [ComposerDictationTranscript],
        keeping current: Int
    ) -> Int {
        var best = transcripts.indices.contains(current) ? current : transcripts.startIndex
        for index in transcripts.indices where transcripts[index].score > transcripts[best].score {
            best = index
        }
        return best
    }
}

enum ComposerAudioMeter {
    static func normalizedLevel<S: Collection>(for samples: S) -> Double
    where S.Element == Float {
        guard !samples.isEmpty else { return 0 }
        let meanSquare = samples.reduce(0.0) { $0 + Double($1 * $1) }
            / Double(samples.count)
        guard meanSquare > 0 else { return 0 }
        let decibels = 20 * log10(sqrt(meanSquare))
        return min(1, max(0, (decibels + 50) / 50))
    }

    static func normalizedLevel(in buffer: AVAudioPCMBuffer) -> Double {
        guard buffer.frameLength > 0, let channel = buffer.floatChannelData?.pointee else {
            return 0
        }
        return normalizedLevel(for: UnsafeBufferPointer(
            start: channel,
            count: Int(buffer.frameLength)
        ))
    }
}

private extension AttributedString {
    var transcriptionConfidence: Double? {
        var total = 0.0
        var weight = 0
        for run in runs {
            guard let confidence = run[
                AttributeScopes.SpeechAttributes.ConfidenceAttribute.self
            ] else { continue }
            let count = self[run.range].characters.count
            total += confidence * Double(count)
            weight += count
        }
        return weight == 0 ? nil : total / Double(weight)
    }
}

private enum ComposerDictationError: LocalizedError {
    case microphoneDenied
    case unsupportedLanguage
    case audioUnavailable
    case conversionFailed
    case transcriptionFailed

    var errorDescription: String? {
        switch self {
        case .microphoneDenied:
            "Microphone access is required to dictate a message."
        case .unsupportedLanguage:
            "Dictation is not available for the current language."
        case .audioUnavailable:
            "The microphone is not available for dictation."
        case .conversionFailed:
            "möbius could not process the microphone audio."
        case .transcriptionFailed:
            "Dictation stopped unexpectedly. Please try again."
        }
    }
}

private final class ComposerAudioBufferConverter {
    private var converter: AVAudioConverter?

    func convert(_ buffer: AVAudioPCMBuffer, to format: AVAudioFormat) throws -> AVAudioPCMBuffer {
        guard buffer.format != format else { return buffer }
        if converter?.inputFormat != buffer.format || converter?.outputFormat != format {
            converter = AVAudioConverter(from: buffer.format, to: format)
            converter?.primeMethod = .none
        }
        guard let converter else { throw ComposerDictationError.conversionFailed }

        let ratio = converter.outputFormat.sampleRate / converter.inputFormat.sampleRate
        let capacity = max(
            1,
            AVAudioFrameCount((Double(buffer.frameLength) * ratio).rounded(.up))
        )
        guard let converted = AVAudioPCMBuffer(
            pcmFormat: converter.outputFormat,
            frameCapacity: capacity
        ) else {
            throw ComposerDictationError.conversionFailed
        }

        var conversionError: NSError?
        // AVAudioConverter invokes this block synchronously; neither local escapes the call.
        nonisolated(unsafe) let input = buffer
        nonisolated(unsafe) var suppliedInput = false
        let status = converter.convert(to: converted, error: &conversionError) { _, status in
            guard !suppliedInput else {
                status.pointee = .noDataNow
                return nil
            }
            suppliedInput = true
            status.pointee = .haveData
            return input
        }
        guard status != .error else { throw ComposerDictationError.conversionFailed }
        return converted
    }
}
