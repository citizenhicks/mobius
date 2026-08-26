import Foundation
import XCTest

final class ComposerDictationTests: XCTestCase {
    func testAutomaticDictationKeepsEnglishFrenchAndIncludesCurrentLocale() {
        let languages = ComposerDictation.requestedLocales
            .compactMap { $0.language.languageCode?.identifier }

        XCTAssertEqual(Array(languages.prefix(2)), ["en", "fr"])
        XCTAssertTrue(
            ComposerDictation.requestedLocales.contains {
                $0.identifier(.bcp47) == Locale.current.identifier(.bcp47)
            }
        )
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
