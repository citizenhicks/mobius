import Foundation
import ImageIO
import SwiftUI
import UniformTypeIdentifiers

/// Compile with MobiusPalette.swift so the app and exported artwork share their colors.
@main
struct ExportAccentIcons {
    enum Failure: Error {
        case arguments, invalidIcon, invalidLayer(String), cannotRender(String), cannotWrite(String)
    }

    @MainActor
    static func main() throws {
        guard CommandLine.arguments.count == 3 else { throw Failure.arguments }
        let source = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
        let output = URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
        guard
            let icon = try JSONSerialization.jsonObject(
                with: Data(contentsOf: source.appendingPathComponent("icon.json")))
                as? [String: Any],
            let fill = icon["fill"] as? [String: Any],
            fill["linear-gradient"] is [String]
        else { throw Failure.invalidIcon }

        let layers = try ["Rear", "Body", "Front", "Foreground"].map { name in
            let url = source.appendingPathComponent("Assets/\(name).png")
            guard let imageSource = CGImageSourceCreateWithURL(url as CFURL, nil),
                let image = CGImageSourceCreateImageAtIndex(imageSource, 0, nil),
                image.width == 1024, image.height == 1024
            else { throw Failure.invalidLayer(url.path) }
            let mono = try Data(
                contentsOf: source.appendingPathComponent("Assets/\(name)-Mono.png"))
            return (name, image, mono)
        }

        for tint in AccentTint.allCases where tint != .appDefault {
            let destination = output.appendingPathComponent("AppIcon-\(tint.rawValue).icon")
            let assets = destination.appendingPathComponent("Assets", isDirectory: true)
            try FileManager.default.createDirectory(at: assets, withIntermediateDirectories: true)

            for (name, image, mono) in layers {
                let renderer = ImageRenderer(
                    content: Image(decorative: image, scale: 1).colorMultiply(tint.artworkTint))
                renderer.scale = 1
                guard let rendered = renderer.cgImage,
                    rendered.width == image.width, rendered.height == image.height
                else { throw Failure.cannotRender(name) }
                let url = assets.appendingPathComponent("\(name).png")
                guard
                    let png = CGImageDestinationCreateWithURL(
                        url as CFURL, UTType.png.identifier as CFString, 1, nil)
                else { throw Failure.cannotWrite(url.path) }
                CGImageDestinationAddImage(png, rendered, nil)
                guard CGImageDestinationFinalize(png) else { throw Failure.cannotWrite(url.path) }
                try mono.write(
                    to: assets.appendingPathComponent("\(name)-Mono.png"), options: .atomic)
            }

            // Use the same subtle Nord surface tint as the app; retain the icon's geometry.
            let palette = MobiusPalette(.dark, accentTint: tint)
            var tintedFill = fill
            tintedFill["linear-gradient"] = [palette.raised, palette.panel].map { color in
                let value = color.resolve(in: EnvironmentValues())
                return "srgb:\(value.red),\(value.green),\(value.blue),\(value.opacity)"
            }
            var tintedIcon = icon
            tintedIcon["fill"] = tintedFill
            let metadata = try JSONSerialization.data(
                withJSONObject: tintedIcon, options: [.prettyPrinted, .sortedKeys])
            try metadata.write(
                to: destination.appendingPathComponent("icon.json"), options: .atomic)
            print("Wrote \(destination.path)")
        }
    }
}
