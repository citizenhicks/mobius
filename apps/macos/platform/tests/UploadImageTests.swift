import Foundation
import ImageIO
import Testing
import UniformTypeIdentifiers

@testable import MobiusNative

@Test func uploadEncoderWritesDecodableStillsAndRejectsMultipleFrames() throws {
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
    defer { try? FileManager.default.removeItem(at: directory) }
    let avif = try #require(UTType(filenameExtension: "avif"))
    let cases: [(UTType, Int, UTType?)] = [
        (.heic, 1, .jpeg), (avif, 1, .jpeg), (.bmp, 1, .jpeg), (.tiff, 1, .png), (.tiff, 2, nil),
    ]
    for (type, frames, expected) in cases {
        let input = directory.appendingPathComponent(UUID().uuidString)
        let output = directory.appendingPathComponent(UUID().uuidString)
        let encoder = try #require(
            CGImageDestinationCreateWithURL(
                input as CFURL, type.identifier as CFString, frames, nil))
        let alpha: CGImageAlphaInfo = type == .tiff ? .premultipliedLast : .noneSkipLast
        let context = try #require(
            CGContext(
                data: nil, width: type == .heic ? 5000 : 64, height: 64, bitsPerComponent: 8,
                bytesPerRow: 0,
                space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: alpha.rawValue))
        let image = try #require(context.makeImage())
        for _ in 0..<frames { CGImageDestinationAddImage(encoder, image, nil) }
        try #require(CGImageDestinationFinalize(encoder))
        let original = try Data(contentsOf: input)
        let result = nativeConvertUploadImage(input.path, output.path)
        if let expected {
            #expect(result == (expected == .png ? 2 : 1))
            let encoded = try #require(CGImageSourceCreateWithURL(output as CFURL, nil))
            #expect(CGImageSourceGetType(encoded) as String? == expected.identifier)
            let pixels = try #require(CGImageSourceCreateImageAtIndex(encoded, 0, nil))
            if type == .heic { #expect(pixels.width == 4096) }
        } else {
            #expect(result == 0)
            #expect(!FileManager.default.fileExists(atPath: output.path))
        }
        #expect(try Data(contentsOf: input) == original)
    }
}
