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
        writeImage(image, to: destination, type: .png)
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
    return downsample(source, maxPixels: maxPixels)
}

private func downsample(_ source: CGImageSource, maxPixels: Int) -> CGImage? {
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

/// Returns the format of the encoded still (1 for JPEG, 2 for PNG), or zero on failure.
@_cdecl("mobius_native_convert_upload_image")
public func nativeConvertUploadImage(
    _ sourcePath: UnsafePointer<CChar>, _ destinationPath: UnsafePointer<CChar>
) -> UInt8 {
    let url = URL(fileURLWithPath: String(cString: sourcePath))
    guard let source = CGImageSourceCreateWithURL(
        url as CFURL, [kCGImageSourceShouldCache: false] as CFDictionary),
        CGImageSourceGetCount(source) == 1,
        let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any],
        let image = downsample(source, maxPixels: 4096)
    else { return 0 }
    // Downsampling can add an alpha channel even to an opaque camera photo.
    let hasAlpha = properties[kCGImagePropertyHasAlpha] as? Bool == true
    let destination = URL(fileURLWithPath: String(cString: destinationPath))
    guard writeImage(image, to: destination, type: hasAlpha ? .png : .jpeg) else { return 0 }
    return hasAlpha ? 2 : 1
}

private func writeImage(_ image: CGImage, to url: URL, type: UTType) -> Bool {
    guard
        let destination = CGImageDestinationCreateWithURL(
            url as CFURL, type.identifier as CFString, 1, nil)
    else { return false }
    let options = [kCGImageDestinationLossyCompressionQuality: 0.9] as CFDictionary
    CGImageDestinationAddImage(destination, image, type == .jpeg ? options : nil)
    return CGImageDestinationFinalize(destination)
}
