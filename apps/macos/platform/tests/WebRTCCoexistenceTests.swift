import CoreVideo
import Darwin
import Foundation
@preconcurrency import LiveKitWebRTC
import ObjectiveC
import Synchronization
import Testing

@testable import MobiusNative

/// Opt in with the actual bundled CEF executable; no microphone, screen, or network is used.
@Suite(.enabled(if: ProcessInfo.processInfo.environment["MOBIUS_CEF_FRAMEWORK"] != nil))
@MainActor
struct WebRTCCoexistenceTests {
    @Test func chromiumCoexistsWithNativeAudioVideoAndSDP() async throws {
        let path = try #require(ProcessInfo.processInfo.environment["MOBIUS_CEF_FRAMEWORK"])
        // Objective-C framework classes remain registered for the lifetime of the process.
        _ = try #require(dlopen(path, RTLD_NOW | RTLD_GLOBAL))
        for name in [
            "SckHelper", "RTCDispatcher", "RTCEncodedImage", "RTCVideoCapturer",
            "RTCVideoCodecInfo", "RTCVideoEncoderCodecSupport", "RTCVideoEncoderQpThresholds",
            "RTCVideoEncoderSettings", "RTCVideoFrame",
        ] {
            let chromium: AnyClass = try #require(NSClassFromString(name))
            let native: AnyClass = try #require(NSClassFromString("LK" + name))
            let chromiumImage = String(cString: try #require(class_getImageName(chromium)))
            let nativeImage = String(cString: try #require(class_getImageName(native)))
            #expect(chromiumImage.contains("Chromium Embedded Framework.framework"))
            #expect(nativeImage.contains("LiveKitWebRTC.framework"))
        }

        // This is the same AVAudioEngine factory used by RealtimeVoiceSession.
        let factory = try #require(RealtimeVoiceSession.factory)
        let delegate = RealtimeVoiceSession()
        let configuration = LKRTCConfiguration()
        configuration.sdpSemantics = .unifiedPlan
        let constraints = LKRTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: nil)
        let peer = try #require(
            factory.peerConnection(
                with: configuration, constraints: constraints, delegate: delegate))
        defer { peer.close() }
        let audio = factory.audioTrack(
            with: factory.audioSource(with: constraints), trackId: "test-audio")
        audio.isEnabled = false
        peer.add(audio, streamIds: ["test"])
        let channel = try #require(
            peer.dataChannel(
                forLabel: "test-events", configuration: LKRTCDataChannelConfiguration()))
        defer { channel.close() }

        let source = factory.videoSource()
        let video = factory.videoTrack(with: source, trackId: "test-video")
        let renderer = SyntheticFrameRenderer()
        video.add(renderer)
        defer { video.remove(renderer) }
        var pixelBuffer: CVPixelBuffer?
        #expect(
            CVPixelBufferCreate(nil, 32, 24, kCVPixelFormatType_32BGRA, nil, &pixelBuffer)
                == kCVReturnSuccess)
        let buffer = try #require(pixelBuffer)
        CVPixelBufferLockBaseAddress(buffer, [])
        memset(CVPixelBufferGetBaseAddress(buffer), 0, CVPixelBufferGetDataSize(buffer))
        CVPixelBufferUnlockBaseAddress(buffer, [])
        let frame = LKRTCVideoFrame(
            buffer: LKRTCCVPixelBuffer(pixelBuffer: buffer), rotation: ._0,
            timeStampNs: 1_000_000_000)
        source.capturer(LKRTCVideoCapturer(delegate: source), didCapture: frame)
        for _ in 0..<100 where !renderer.receivedFrame {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(renderer.receivedFrame)

        let offer = try await peer.offer(for: constraints)
        #expect(offer.sdp.contains("m=audio"))
        #expect(offer.sdp.contains("m=application"))
        // Do not set descriptions: this checks SDP/data-channel creation without ICE or media capture.
    }
}

private final class SyntheticFrameRenderer: NSObject, LKRTCVideoRenderer {
    private let received = Mutex(false)
    var receivedFrame: Bool { received.withLock { $0 } }
    func setSize(_ size: CGSize) {}
    func renderFrame(_ frame: LKRTCVideoFrame?) {
        guard let frame, frame.width == 32, frame.height == 24 else { return }
        received.withLock { $0 = true }
    }
}
