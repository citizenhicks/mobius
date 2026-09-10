// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "MobiusGatewayMenuBar",
    platforms: [.macOS(.v26)],
    products: [.executable(name: "MobiusGatewayMenuBar", targets: ["MobiusGatewayMenuBar"])],
    targets: [
        .binaryTarget(
            name: "WebRTC",
            url: "https://github.com/webrtc-sdk/Specs/releases/download/150.7871.01/WebRTC.xcframework.zip",
            checksum: "03815cdf2f6a0ed328c94d74cce8fd1b8d2b6e95e2b37eab66795012fcecfdfa"
        ),
        .executableTarget(
            name: "MobiusGatewayMenuBar",
            dependencies: ["WebRTC"]
        ),
        .testTarget(name: "MobiusGatewayMenuBarTests", dependencies: ["MobiusGatewayMenuBar"]),
    ]
)
