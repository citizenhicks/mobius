import Foundation

/// Media lives on the Mac; authentication, provider control and transcripts stay in the gateway.
@MainActor
final class NativeVoice {
    private let emit: (JSONValue) -> Void
    private var media = RealtimeVoiceSession()
    private var task: Task<Void, Never>?
    private var call: Call?

    private struct Call {
        let requestID = UUID().uuidString
        let sessionID: String
        var voiceID: String?
    }

    init(emit: @escaping (JSONValue) -> Void) { self.emit = emit }

    var status: JSONValue {
        .object([
            "type": .string("voice_state"), "active": .bool(call != nil),
            "connected": .bool(media.isConnected), "muted": .bool(media.isMuted),
        ])
    }

    func start(sessionID: String, waitForSessionStarted: Bool) {
        stop()
        let call = Call(sessionID: sessionID)
        self.call = call
        media = RealtimeVoiceSession(waitForSessionStarted: waitForSessionStarted) {
            [weak self] message in
            guard self?.call?.requestID == call.requestID else { return }
            self?.fail(String(localized: message))
        }
        media.onConnectionChange = { [weak self] in
            guard let self, self.call?.requestID == call.requestID else { return }
            emit(status)
        }
        emit(status)
        let media = media
        task = Task { [weak self] in
            do {
                let offer = try await media.offer()
                guard let self, self.call?.requestID == call.requestID else { return }
                send(
                    GatewayRequest(
                        "start_realtime_voice",
                        [
                            "requestId": .string(call.requestID), "sessionId": .string(sessionID),
                            "offerSdp": .string(offer),
                        ]))
                try await Task.sleep(for: .seconds(45))
                guard self.call?.requestID == call.requestID else { return }
                fail("Voice could not connect. Try again.")
            } catch is CancellationError {
            } catch {
                guard self?.call?.requestID == call.requestID else { return }
                self?.fail(error.localizedDescription)
            }
        }
    }

    func stop(notifyGateway: Bool = true) {
        let previous = call
        call = nil
        task?.cancel()
        task = nil
        media.close()
        if previous != nil { emit(status) }
        if let previous, notifyGateway {
            end(sessionID: previous.sessionID, voiceID: previous.voiceID ?? previous.requestID)
        }
    }

    func mute(_ muted: Bool) {
        guard media.isMuted != muted else { return }
        media.isMuted = muted
        emit(status)
    }

    func receive(_ envelope: GatewayEnvelope) throws {
        let body = envelope.body
        let sessionID = try body.requiredString("sessionId")
        switch envelope.type {
        case "realtime_voice_started":
            let requestID = try body.requiredString("requestId")
            let voiceID = try body.requiredString("voiceId")
            guard call?.requestID == requestID, call?.sessionID == sessionID else {
                end(sessionID: sessionID, voiceID: voiceID)
                return
            }
            let answer = try body.requiredString("answerSdp")
            call?.voiceID = voiceID
            task?.cancel()
            let media = media
            task = Task { [weak self] in
                do { try await media.accept(answer: answer) } catch is CancellationError {} catch {
                    guard self?.call?.requestID == requestID else { return }
                    self?.fail(error.localizedDescription)
                }
            }
        case "realtime_voice_failed":
            guard call?.requestID == body["requestId"]?.stringValue, call?.sessionID == sessionID
            else {
                return
            }
            stop(notifyGateway: false)
            emit(.error(try body.requiredString("message")))
        case "realtime_voice_ended":
            guard call?.voiceID == body["voiceId"]?.stringValue, call?.sessionID == sessionID else {
                return
            }
            stop(notifyGateway: false)
            if let reason = body["reason"]?.stringValue, !reason.isEmpty {
                emit(.error(reason))
            }
        default: break
        }
    }

    private func fail(_ message: String) {
        stop()
        emit(.error(message))
    }

    private func send(_ request: GatewayRequest) {
        emit(.object(["type": .string("gateway_request"), "request": request.body]))
    }

    private func end(sessionID: String, voiceID: String) {
        send(
            GatewayRequest(
                "end_realtime_voice",
                [
                    "sessionId": .string(sessionID), "voiceId": .string(voiceID),
                ]))
    }
}

@_cdecl("mobius_native_voice_start")
public func nativeVoiceStart(_ session: UnsafePointer<CChar>, _ waitForSessionStarted: Bool) {
    let session = String(cString: session)
    MainActor.assumeIsolated {
        let bridge = NativeBridge.shared
        guard bridge.isConnected, !session.isEmpty else {
            bridge.fail(DesktopError("Open a connected chat to start voice."))
            return
        }
        bridge.dictation.stop()
        bridge.voice.start(sessionID: session, waitForSessionStarted: waitForSessionStarted)
    }
}

@_cdecl("mobius_native_voice_stop")
public func nativeVoiceStop() { MainActor.assumeIsolated { NativeBridge.shared.voice.stop() } }

@_cdecl("mobius_native_voice_mute")
public func nativeVoiceMute(_ muted: Bool) {
    MainActor.assumeIsolated { NativeBridge.shared.voice.mute(muted) }
}
