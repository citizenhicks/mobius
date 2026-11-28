import Foundation

/// One transcription request. Incoming results must match this recording's ID.
@MainActor
final class RemoteDictation {
    let id = UUID().uuidString.lowercased()
    let route: String
    let language: String?
    private let emit: (JSONValue) -> Void
    private var frames = DictationFrames()
    private var started = false

    init(route: String, language: String?, emit: @escaping (JSONValue) -> Void) {
        self.route = route
        self.language = language
        self.emit = emit
    }

    func start() {
        send("start_dictation", selection)
        started = true
    }

    func append(_ samples: Data) {
        var input = samples
        while let frame = frames.next(from: &input) { sendAudio(frame) }
    }

    func finish() {
        if let frame = frames.finish() { sendAudio(frame) }
        send("finish_dictation")
    }

    func cancel() {
        if started { send("cancel_dictation") }
    }

    private var selection: [String: JSONValue] {
        var fields: [String: JSONValue] = ["route": .string(route)]
        if let language { fields["language"] = .string(language) }
        return fields
    }

    private func sendAudio(_ frame: Data) {
        send("dictation_audio", ["audio": .string(frame.base64EncodedString())])
    }

    private func send(_ type: String, _ fields: [String: JSONValue] = [:]) {
        var fields = fields
        fields["requestId"] = .string(id)
        emit(.object(["type": .string("gateway_request"), "request": GatewayRequest(type, fields).body]))
    }

}
