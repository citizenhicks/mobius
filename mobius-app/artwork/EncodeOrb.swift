import AVFoundation
import CoreImage
import CoreVideo
import Foundation
import ImageIO
import VideoToolbox

@main
struct EncodeOrb {
    enum Failure: Error { case arguments, invalidFrame(String), cannotEncode, emptyPool }

    static func main() async throws {
        guard (3...4).contains(CommandLine.arguments.count) else { throw Failure.arguments }
        let directory = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
        let output = URL(fileURLWithPath: CommandLine.arguments[2])
        guard let count = Int(CommandLine.arguments.count > 3 ? CommandLine.arguments[3] : "149"),
            count > 0
        else { throw Failure.arguments }
        let colorSpace = CGColorSpace(name: CGColorSpace.sRGB)!
        let context = CIContext(options: [.workingColorSpace: colorSpace])
        let writer = try AVAssetWriter(outputURL: output, fileType: .mov)
        let settings: [String: Any] = [
            AVVideoCodecKey: AVVideoCodecType.hevcWithAlpha.rawValue,
            AVVideoWidthKey: 512,
            AVVideoHeightKey: 512,
            AVVideoCompressionPropertiesKey: [
                AVVideoAverageBitRateKey: 3_000_000,
                AVVideoExpectedSourceFrameRateKey: 24,
                AVVideoMaxKeyFrameIntervalKey: 24,
                AVVideoAllowFrameReorderingKey: false,
                kVTCompressionPropertyKey_TargetQualityForAlpha as String: 0.95,
                kVTCompressionPropertyKey_AlphaChannelMode as String:
                    kVTAlphaChannelMode_PremultipliedAlpha,
            ],
            AVVideoColorPropertiesKey: [
                AVVideoColorPrimariesKey: AVVideoColorPrimaries_ITU_R_709_2,
                AVVideoTransferFunctionKey: AVVideoTransferFunction_IEC_sRGB,
                AVVideoYCbCrMatrixKey: AVVideoYCbCrMatrix_ITU_R_709_2,
            ],
        ]
        guard writer.canApply(outputSettings: settings, forMediaType: .video) else {
            throw Failure.cannotEncode
        }
        let input = AVAssetWriterInput(mediaType: .video, outputSettings: settings)
        let receiver = writer.inputPixelBufferReceiver(
            for: input,
            pixelBufferAttributes: .init(
                pixelFormatType: .init(rawValue: kCVPixelFormatType_32BGRA),
                size: .init(width: 512, height: 512)
            )
        )
        try writer.start()
        writer.startSession(atSourceTime: .zero)
        guard let pool = receiver.pixelBufferPool else { throw Failure.emptyPool }
        for frame in 1...count {
            let url = directory.appendingPathComponent(String(format: "frame-%03d.png", frame))
            guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
                let image = CGImageSourceCreateImageAtIndex(source, 0, nil),
                image.width == 512, image.height == 512
            else { throw Failure.invalidFrame(url.path) }
            let buffer = try pool.makeMutablePixelBuffer()
            buffer.withUnsafeBuffer { pixelBuffer in
                CVBufferSetAttachment(
                    pixelBuffer, kCVImageBufferAlphaChannelModeKey,
                    kCVImageBufferAlphaChannelMode_PremultipliedAlpha, .shouldPropagate)
                context.render(
                    CIImage(cgImage: image), to: pixelBuffer,
                    bounds: CGRect(x: 0, y: 0, width: 512, height: 512), colorSpace: colorSpace)
            }
            try await receiver.append(
                CVReadOnlyPixelBuffer(buffer),
                with: CMTime(value: Int64(frame - 1), timescale: 24))
            if frame.isMultiple(of: 24) { print("Encoded \(frame)/\(count)") }
        }
        writer.endSession(atSourceTime: CMTime(value: Int64(count), timescale: 24))
        receiver.finish()
        await writer.finishWriting()
        guard writer.status == .completed else { throw writer.error ?? Failure.cannotEncode }
        let bytes = try output.resourceValues(forKeys: [.fileSizeKey]).fileSize!
        print("Wrote \(output.path): \(count) frames, \(Double(count) / 24)s, \(bytes) bytes")
    }
}
