import AVFoundation
@testable import Mobius
import XCTest

final class ComposingOrbTests: XCTestCase {
    func testBundledBlenderLoopIsPlayableAndRetainsTransparency() async throws {
        let url = try XCTUnwrap(Bundle.main.url(forResource: "ComposingOrb", withExtension: "mov"))
        let asset = AVURLAsset(url: url)
        let playable = try await asset.load(.isPlayable)
        let duration = try await asset.load(.duration)
        let alphaTracks = try await asset.loadTracks(withMediaCharacteristic: .containsAlphaChannel)
        let track = try XCTUnwrap(alphaTracks.first)
        let size = try await track.load(.naturalSize)

        XCTAssertTrue(playable)
        XCTAssertEqual(CMTimeGetSeconds(duration), 149.0 / 24.0, accuracy: 0.001)
        XCTAssertEqual(size, CGSize(width: 512, height: 512))
    }
}
