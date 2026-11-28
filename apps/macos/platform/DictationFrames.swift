import Foundation

/// Coalesces short microphone callbacks and splits large ones before base64 encoding.
/// Both apps send mono PCM16 at 24 kHz, normally in about 100 ms of audio.
struct DictationFrames {
    private var pending = Data()

    mutating func next(from input: inout Data) -> Data? {
        guard !input.isEmpty else { return nil }
        let count = min(input.count, gatewayDictationChunkBytes - pending.count)
        let prefix = input.prefix(count)
        input = input.dropFirst(count)
        if pending.isEmpty && prefix.count >= 4_800 { return prefix }
        pending.append(prefix)
        return pending.count >= 4_800 ? finish() : nil
    }

    mutating func finish() -> Data? {
        guard !pending.isEmpty else { return nil }
        let frame = pending
        pending = Data()
        return frame
    }
}
