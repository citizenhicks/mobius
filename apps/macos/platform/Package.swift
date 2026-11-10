// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "MobiusNative",
    platforms: [.macOS(.v26)],
    products: [.library(name: "MobiusNative", type: .dynamic, targets: ["MobiusNative"])],
    dependencies: [
        .package(url: "https://github.com/sparkle-project/Sparkle", exact: "2.10.0"),
        .package(url: "https://github.com/migueldeicaza/SwiftTerm", exact: "1.99.1"),
    ],
    targets: [
        .binaryTarget(
            name: "LiveKitWebRTC",
            url:
                "https://github.com/livekit/webrtc-xcframework/releases/download/150.7871.03/LiveKitWebRTC.xcframework.zip",
            checksum: "5727a03c32ed902da570b699e3983e3fb4bfe853ab4da045af70912a7db44557"
        ),
        .target(
            name: "MobiusNative",
            dependencies: [
                "LiveKitWebRTC", .product(name: "Sparkle", package: "Sparkle"),
                .product(name: "SwiftTerm", package: "SwiftTerm"),
            ], path: ".",
            exclude: ["tests", "resources", "Info.plist", "Mobius.entitlements"],
            sources: [
                "NativeBridge.swift", "NativeBrowser.swift", "NativeDictation.swift",
                "NativePanel.swift",
                "NativeTitles.swift", "NativeRecaps.swift", "OnDeviceWriters.swift", "NativeVoice.swift", "NativePaths.swift", "NativeCloud.swift",
                "NativeThumbnails.swift",
                "NativeCloudClient.swift", "NativeUpdates.swift", "NativeTerminal.swift", "NativeStatusItem.swift", "desktop",
            ]),
        .testTarget(name: "MobiusNativeTests", dependencies: ["MobiusNative"], path: "tests"),
    ]
)
