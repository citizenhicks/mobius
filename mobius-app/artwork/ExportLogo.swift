import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

enum ExportFailure: Error {
    case arguments, invalidLayer(String), cannotRender, cannotWrite
}

guard CommandLine.arguments.count == 3 else { throw ExportFailure.arguments }
let icon = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
let output = URL(fileURLWithPath: CommandLine.arguments[2])
let size = 1024
let bounds = CGRect(x: 0, y: 0, width: size, height: size)
guard
    let context = CGContext(
        data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: size * 4,
        space: CGColorSpace(name: CGColorSpace.sRGB)!,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    )
else { throw ExportFailure.cannotRender }
context.clear(bounds)

// Composite the approved default/dark artwork from back to front.
for name in ["Rear", "Body", "Front", "Foreground"] {
    let url = icon.appendingPathComponent("Assets/\(name).png")
    guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
        let layer = CGImageSourceCreateImageAtIndex(source, 0, nil),
        layer.width == size, layer.height == size
    else { throw ExportFailure.invalidLayer(url.path) }
    context.draw(layer, in: bounds)
}

guard let image = context.makeImage(),
    let destination = CGImageDestinationCreateWithURL(
        output as CFURL, UTType.png.identifier as CFString, 1, nil)
else { throw ExportFailure.cannotWrite }
CGImageDestinationAddImage(destination, image, nil)
guard CGImageDestinationFinalize(destination) else { throw ExportFailure.cannotWrite }
print("Wrote \(output.path): \(size) × \(size), transparent sRGB")
