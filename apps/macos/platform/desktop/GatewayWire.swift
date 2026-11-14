import Foundation

let gatewayProtocolVersion = 103

struct GatewayRequest: Encodable, Sendable {
    let body: JSONValue

    init(_ type: String, _ fields: [String: JSONValue] = [:]) {
        var fields = fields
        fields["type"] = .string(type)
        fields["version"] = .integer(Int64(gatewayProtocolVersion))
        body = .object(fields)
    }

    func encode(to encoder: Encoder) throws { try body.encode(to: encoder) }
}

struct GatewayEnvelope: Decodable, Sendable {
    let type: String
    let body: JSONValue

    init(from decoder: Decoder) throws {
        body = try JSONValue(from: decoder)
        guard body["version"]?.intValue == gatewayProtocolVersion else {
            throw DesktopError("Update the app and gateway together.")
        }
        type = try body.requiredString("type")
    }
}

extension JSONValue {
    func requiredString(_ key: String) throws -> String {
        guard let value = self[key]?.stringValue, !value.isEmpty else {
            throw DesktopError("The gateway response is missing \(key).")
        }
        return value
    }

    func decode<T: Decodable>(_ type: T.Type) throws -> T {
        try JSONDecoder().decode(type, from: JSONEncoder().encode(self))
    }
}
