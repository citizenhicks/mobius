import Foundation
import XCTest

final class ComposerDictationTests: XCTestCase {
    func testDictationUsesOnlyEnglishFrenchAndGerman() {
        let languages = ComposerDictation.requestedLocales
            .compactMap { $0.language.languageCode?.identifier }

        XCTAssertEqual(languages, ["en", "fr", "de"])
    }

    func testDictationChoosesTheHighestConfidenceTranscript() {
        var english = ComposerDictationTranscript(locale: Locale(identifier: "en_US"))
        english.consume(text: "hello", confidence: 0.35, isFinal: false)
        var french = ComposerDictationTranscript(locale: Locale(identifier: "fr_FR"))
        french.consume(text: "bonjour", confidence: 0.91, isFinal: false)

        XCTAssertEqual(
            ComposerDictationTranscript.bestIndex(in: [english, french], keeping: 0),
            1
        )
    }

    func testAudioMeterMapsSilenceAndSpeechIntoWaveformRange() {
        XCTAssertEqual(ComposerAudioMeter.normalizedLevel(for: [Float](repeating: 0, count: 8)), 0)
        XCTAssertEqual(
            ComposerAudioMeter.normalizedLevel(for: [Float](repeating: 0.1, count: 8)),
            0.6,
            accuracy: 0.001
        )
    }
}

extension ComposerDictationTests {
    @MainActor
    func testConcurrentCancelAndDiscardJoinRecognitionTeardown() async {
        let dictation = ComposerDictation()
        let stopping = expectation(description: "Recognition teardown is suspended")
        var resume: CheckedContinuation<Void, Never>?
        var returned = 0
        // Stand in for the asynchronous SpeechAnalyzer teardown without recording audio.
        dictation.cancellationTask = Task {
            await withCheckedContinuation { continuation in
                resume = continuation
                stopping.fulfill()
            }
        }
        let first = Task { await dictation.cancel(); returned += 1 }
        let second = Task { await dictation.discard(); returned += 1 }
        await fulfillment(of: [stopping], timeout: 1)
        XCTAssertEqual(returned, 0)
        resume?.resume()
        await first.value
        await second.value
        XCTAssertEqual(returned, 2)
    }
}
