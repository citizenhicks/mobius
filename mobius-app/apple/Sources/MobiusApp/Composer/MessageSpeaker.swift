import Foundation
import SwiftStreamingMarkdown
@preconcurrency import AVFoundation

@MainActor
final class MessageSpeaker {
    private let synthesizer: AVSpeechSynthesizer
    private var speechTask: Task<Void, Never>?

    init(synthesizer: AVSpeechSynthesizer = AVSpeechSynthesizer()) {
        self.synthesizer = synthesizer
        synthesizer.usesApplicationAudioSession = false
    }

    func speak(_ markdown: String, after prepareAudio: @escaping @MainActor () async -> Void) {
        speechTask?.cancel()
        _ = synthesizer.stopSpeaking(at: .immediate)
        speechTask = Task { [weak self] in
            guard !Task.isCancelled else { return }
            await prepareAudio()
            guard !Task.isCancelled else { return }
            let text = await markdown.markdownToPlainText()
            guard let self,
                  !Task.isCancelled,
                  !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            else { return }
            synthesizer.speak(AVSpeechUtterance(string: text))
        }
    }

    func stop() {
        speechTask?.cancel()
        speechTask = nil
        _ = synthesizer.stopSpeaking(at: .immediate)
    }
}
