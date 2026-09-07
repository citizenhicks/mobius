import Foundation

indirect enum JSONValue: Codable, Equatable, Sendable {
    case object([String: JSONValue])
    case array([JSONValue])
    case string(String)
    case number(Double)
    case integer(Int64)
    case unsignedInteger(UInt64)
    case decimal(Decimal)
    case bool(Bool)
    case null

    static func == (lhs: JSONValue, rhs: JSONValue) -> Bool {
        if let lhsNumber = lhs.numericValue, let rhsNumber = rhs.numericValue {
            return lhsNumber == rhsNumber
        }
        switch (lhs, rhs) {
        case (.object(let lhs), .object(let rhs)): return lhs == rhs
        case (.array(let lhs), .array(let rhs)): return lhs == rhs
        case (.string(let lhs), .string(let rhs)): return lhs == rhs
        case (.number(let lhs), .number(let rhs)): return lhs == rhs
        case (.bool(let lhs), .bool(let rhs)): return lhs == rhs
        case (.null, .null): return true
        default: return false
        }
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let value = try? container.decode(Bool.self) {
            self = .bool(value)
        } else if let value = try? container.decode(Decimal.self) {
            let string = NSDecimalNumber(decimal: value).stringValue
            if let integer = Int64(string) {
                self = .integer(integer)
            } else if let unsignedInteger = UInt64(string) {
                self = .unsignedInteger(unsignedInteger)
            } else {
                self = .decimal(value)
            }
        } else if let value = try? container.decode(Double.self) {
            self = .number(value)
        } else if let value = try? container.decode(String.self) {
            self = .string(value)
        } else if let value = try? container.decode([JSONValue].self) {
            self = .array(value)
        } else {
            let keyed = try decoder.container(keyedBy: DynamicCodingKey.self)
            self = .object(
                try Dictionary(
                    uniqueKeysWithValues: keyed.allKeys.map { key in
                        (key.stringValue, try keyed.decode(JSONValue.self, forKey: key))
                    }))
        }
    }

    func encode(to encoder: Encoder) throws {
        switch self {
        case .object(let value):
            var container = encoder.container(keyedBy: DynamicCodingKey.self)
            for (key, element) in value {
                try container.encode(element, forKey: DynamicCodingKey(key))
            }
        case .array(let value):
            var container = encoder.singleValueContainer()
            try container.encode(value)
        case .string(let value):
            var container = encoder.singleValueContainer()
            try container.encode(value)
        case .number(let value):
            var container = encoder.singleValueContainer()
            try container.encode(value)
        case .integer(let value):
            var container = encoder.singleValueContainer()
            try container.encode(value)
        case .unsignedInteger(let value):
            var container = encoder.singleValueContainer()
            try container.encode(value)
        case .decimal(let value):
            var container = encoder.singleValueContainer()
            try container.encode(value)
        case .bool(let value):
            var container = encoder.singleValueContainer()
            try container.encode(value)
        case .null:
            var container = encoder.singleValueContainer()
            try container.encodeNil()
        }
    }

    subscript(key: String) -> JSONValue? {
        guard case .object(let object) = self else { return nil }
        return object[key]
    }

    var stringValue: String? {
        guard case .string(let value) = self else { return nil }
        return value
    }

    private var numericValue: Decimal? {
        switch self {
        case .number(let value):
            guard value.isFinite else { return nil }
            let decimal = Decimal(value)
            guard NSDecimalNumber(decimal: decimal) != .notANumber else { return nil }
            return decimal
        case .integer(let value):
            return Decimal(value)
        case .unsignedInteger(let value):
            return Decimal(value)
        case .decimal(let value):
            return value
        default:
            return nil
        }
    }

    var intValue: Int? {
        switch self {
        case .number(let value):
            guard value.rounded() == value else { return nil }
            return Int(exactly: value)
        case .integer(let value):
            return Int(exactly: value)
        case .unsignedInteger(let value):
            return Int(exactly: value)
        case .decimal:
            return nil
        default:
            return nil
        }
    }

    var uintValue: UInt64? {
        switch self {
        case .number(let value):
            guard value.rounded() == value else { return nil }
            return UInt64(exactly: value)
        case .integer(let value):
            return UInt64(exactly: value)
        case .unsignedInteger(let value):
            return value
        case .decimal:
            return nil
        default:
            return nil
        }
    }

    var boolValue: Bool? {
        guard case .bool(let value) = self else { return nil }
        return value
    }

    var arrayValue: [JSONValue]? {
        guard case .array(let value) = self else { return nil }
        return value
    }

    var objectValue: [String: JSONValue]? {
        guard case .object(let value) = self else { return nil }
        return value
    }
}

struct DynamicCodingKey: CodingKey, Hashable {
    let stringValue: String
    let intValue: Int?

    init(stringValue: String) {
        self.stringValue = stringValue
        self.intValue = nil
    }

    init(intValue: Int) {
        self.stringValue = String(intValue)
        self.intValue = intValue
    }

    init(_ string: String) {
        self.init(stringValue: string)
    }
}

extension KeyedEncodingContainer where Key == DynamicCodingKey {
    mutating func encode<T: Encodable>(_ value: T, forKey key: String) throws {
        try encode(value, forKey: DynamicCodingKey(key))
    }

    mutating func encodeIfPresent<T: Encodable>(_ value: T?, forKey key: String) throws {
        try encodeIfPresent(value, forKey: DynamicCodingKey(key))
    }
}

extension KeyedDecodingContainer where Key == DynamicCodingKey {
    func decode<T: Decodable>(_ type: T.Type, forKey key: String) throws -> T {
        try decode(type, forKey: DynamicCodingKey(key))
    }

    func decodeIfPresent<T: Decodable>(_ type: T.Type, forKey key: String) throws -> T? {
        try decodeIfPresent(type, forKey: DynamicCodingKey(key))
    }
}
