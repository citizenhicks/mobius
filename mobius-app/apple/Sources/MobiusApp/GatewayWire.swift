import Foundation
import UIKit

let gatewayProtocolVersion = 68
let maximumGatewayFrameBytes = 50 * 1024 * 1024
let maximumComposerBytes = 1024 * 1024
let maximumWireSessionFileReferences = 16

enum GatewayWireError: LocalizedError, Equatable {
    case invalidEndpoint(LocalizedStringResource)
    case invalidPairingSetup
    case insecureRemoteEndpoint
    case unsupportedVersion(Int)
    case oversizedFrame(Int)
    case invalidFrame(String)
    case disconnected

    var localizedDescriptionResource: LocalizedStringResource {
        switch self {
        case .invalidEndpoint(let message): message
        case .invalidPairingSetup:
            "Use a complete möbius pairing setup from the gateway."
        case .insecureRemoteEndpoint:
            "Plaintext gateway connections are allowed only on this device. Use tls:// or wss:// for remote gateways."
        case .unsupportedVersion(let version): "Gateway protocol version \(version) is not supported."
        case .oversizedFrame(let size): "Gateway frame is too large (\(size) bytes)."
        case .invalidFrame(let message): "Invalid gateway frame: \(message)"
        case .disconnected: "The gateway disconnected."
        }
    }

    var errorDescription: String? { String(localized: localizedDescriptionResource) }
}
struct GatewayPairingSetup: Equatable, Sendable {
    private static let maximumCodeBytes = 512

    let endpoint: GatewayEndpoint
    let code: String

    init(_ rawValue: String) throws {
        let parts = rawValue.split(separator: "|", omittingEmptySubsequences: false)
        guard parts.count == 3, parts[0] == "mobius-pair:v1" else {
            throw GatewayWireError.invalidPairingSetup
        }
        try self.init(endpoint: String(parts[1]), code: String(parts[2]))
    }

    init(endpoint: String, code: String) throws {
        guard !code.isEmpty,
              code.utf8.count <= Self.maximumCodeBytes,
              code.utf8.allSatisfy({ $0 >= 0x21 && $0 <= 0x7e })
        else {
            throw GatewayWireError.invalidPairingSetup
        }
        self.endpoint = try GatewayEndpoint(endpoint)
        self.code = code
    }
}

struct GatewayEndpoint: Hashable, Codable, Sendable {
    let rawValue: String

    init(_ rawValue: String) throws {
        let trimmed = rawValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let components = URLComponents(string: trimmed),
              let scheme = components.scheme?.lowercased(),
              let parsedHost = components.host,
              components.user == nil,
              components.password == nil,
              components.query == nil,
              components.fragment == nil,
              components.path.isEmpty || components.path == "/"
        else {
            throw GatewayWireError.invalidEndpoint(
                "Use tcp://host:port, tls://host:port, or wss://host."
            )
        }
        guard scheme == "tcp" || scheme == "tls" || scheme == "wss" else {
            throw GatewayWireError.invalidEndpoint(
                "The endpoint scheme must be tcp://, tls://, or wss://."
            )
        }
        guard let port = components.port ?? (scheme == "wss" ? 443 : nil),
              (1...65_535).contains(port)
        else {
            throw GatewayWireError.invalidEndpoint(
                "Use tcp://host:port, tls://host:port, or wss://host."
            )
        }
        let host = Self.normalized(host: parsedHost)
        guard !host.isEmpty else {
            throw GatewayWireError.invalidEndpoint(
                "Use tcp://host:port, tls://host:port, or wss://host."
            )
        }
        if scheme == "tcp" && !Self.isLoopback(host) {
            throw GatewayWireError.insecureRemoteEndpoint
        }
        let suffix = scheme == "wss" && port == 443 ? "" : ":\(port)"
        self.rawValue = "\(scheme)://\(Self.formatted(host: host))\(suffix)"
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        try self.init(container.decode(String.self, forKey: .rawValue))
    }

    var usesTLS: Bool { rawValue.hasPrefix("tls://") || usesWebSocket }

    var usesWebSocket: Bool { rawValue.hasPrefix("wss://") }

    var host: String {
        Self.normalized(host: URLComponents(string: rawValue)?.host ?? "")
    }

    var port: UInt16 {
        UInt16(URLComponents(string: rawValue)?.port ?? (usesWebSocket ? 443 : 0))
    }

    var displayName: String { displayName(locale: .current) }

    func displayName(locale: Locale) -> String {
        func resolve(_ resource: LocalizedStringResource) -> String {
            var resource = resource
            resource.locale = locale
            return String(localized: resource)
        }
        if Self.isLoopback(host) {
            return resolve("This device · \(port)")
        }
        let quickSuffix = ".trycloudflare.com"
        if host.hasSuffix(quickSuffix) {
            let words = host.dropLast(quickSuffix.count).split(separator: "-")
            let tunnel = words.count > 1
                ? "\(words[0])…\(words[words.count - 1])"
                : words.first.map(String.init) ?? resolve("Tunnel")
            return resolve("Cloudflare · \(tunnel)")
        }
        return port == 443 ? host : "\(host):\(port)"
    }

    private static func isLoopback(_ host: String) -> Bool {
        host.caseInsensitiveCompare("localhost") == .orderedSame
            || host == "127.0.0.1"
            || host == "::1"
    }

    private static func formatted(host: String) -> String {
        host.contains(":") ? "[\(host)]" : host
    }

    private static func normalized(host: String) -> String {
        guard host.first == "[", host.last == "]" else { return host }
        return String(host.dropFirst().dropLast())
    }
}

struct GatewayAccount: Identifiable, Hashable, Codable, Sendable {
    let id: UUID
    var endpoint: GatewayEndpoint
    var displayName: String
    var machineName: String
    var cloudUserID: UUID?

    init(
        id: UUID = UUID(),
        endpoint: GatewayEndpoint,
        displayName: String? = nil,
        machineName: String? = nil,
        cloudUserID: UUID? = nil
    ) {
        self.id = id
        self.endpoint = endpoint
        self.displayName = displayName ?? endpoint.displayName
        self.machineName = machineName ?? endpoint.displayName
        self.cloudUserID = cloudUserID
    }
}
