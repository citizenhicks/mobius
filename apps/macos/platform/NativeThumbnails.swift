import AVFoundation
import Foundation
import ImageIO
import UniformTypeIdentifiers

/// ChatSessionFiles.swift's bound: larger images are not decoded for a tile.
private let maximumThumbnailSourceBytes = 48 * 1024 * 1024

/// Writes a PNG thumbnail of the image or video at `source` to `destination`, at most
/// `maxPixels` on its longer side, and stores its pixel size in `size[0]` and `size[1]`.
/// Synchronous and main-actor free, so Rust calls it from a background thread.
@_cdecl("mobius_native_thumbnail")
public func nativeThumbnail(
    _ source: UnsafePointer<CChar>,
    _ destination: UnsafePointer<CChar>,
    _ maxPixels: UInt32,
    _ size: UnsafeMutablePointer<UInt32>
) -> Bool {
    let source = URL(fileURLWithPath: String(cString: source))
    let destination = URL(fileURLWithPath: String(cString: destination))
    guard let image = thumbnailImage(of: source, maxPixels: Int(maxPixels)),
        writePNG(image, to: destination)
    else { return false }
    size[0] = UInt32(image.width)
    size[1] = UInt32(image.height)
    return true
}

func thumbnailImage(of url: URL, maxPixels: Int) -> CGImage? {
    guard maxPixels > 0 else { return nil }
    let type = UTType(filenameExtension: url.pathExtension)
    if type?.conforms(to: .audiovisualContent) == true {
        return videoThumbnail(of: url, maxPixels: maxPixels)
    }
    guard let bytes = try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize,
        bytes <= maximumThumbnailSourceBytes,
        let source = CGImageSourceCreateWithURL(
            url as CFURL, [kCGImageSourceShouldCache: false] as CFDictionary)
    else { return nil }
    // The same downsampling as ChatSessionModel.downsampledFileThumbnail, orientation applied.
    let options: [CFString: Any] = [
        kCGImageSourceCreateThumbnailFromImageAlways: true,
        kCGImageSourceCreateThumbnailWithTransform: true,
        kCGImageSourceThumbnailMaxPixelSize: maxPixels,
        kCGImageSourceShouldCacheImmediately: true,
    ]
    return CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary)
}

private final class GeneratedImage: @unchecked Sendable {
    var image: CGImage?
}

/// The first frame, as SwiftUI's videoFileThumbnail takes it.
private func videoThumbnail(of url: URL, maxPixels: Int) -> CGImage? {
    let generator = AVAssetImageGenerator(asset: AVURLAsset(url: url))
    generator.appliesPreferredTrackTransform = true
    generator.maximumSize = CGSize(width: maxPixels, height: maxPixels)
    let result = GeneratedImage()
    let done = DispatchSemaphore(value: 0)
    generator.generateCGImageAsynchronously(for: .zero) { image, _, _ in
        result.image = image
        done.signal()
    }
    guard done.wait(timeout: .now() + 10) == .success else {
        generator.cancelAllCGImageGeneration()
        return nil
    }
    return result.image
}

private func writePNG(_ image: CGImage, to url: URL) -> Bool {
    guard
        let destination = CGImageDestinationCreateWithURL(
            url as CFURL, UTType.png.identifier as CFString, 1, nil)
    else { return false }
    CGImageDestinationAddImage(destination, image, nil)
    return CGImageDestinationFinalize(destination)
}
