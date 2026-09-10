import Foundation

/// Ordered, immutable tool observations shared by live events and replay.
enum ContentPart: Codable, Hashable, Sendable {
    case text(String)
    case image(SessionFileReference, width: Int, height: Int, detail: String)
    case file(SessionFileReference)

    init(from decoder: Decoder) throws {
        try self.init(json: JSONValue(from: decoder))
    }

    init(json: JSONValue) throws {
        switch json["type"]?.stringValue {
        case "input_text":
            guard let text = json["text"]?.stringValue else {
                throw GatewayWireError.invalidFrame("observation has invalid text")
            }
            self = .text(text)
        case "input_image":
            guard let image = json["image"], let file = image["file"],
                let width = image["width"]?.intValue, width > 0,
                let height = image["height"]?.intValue, height > 0,
                let detail = image["detail"]?.stringValue,
                ["auto", "low", "high"].contains(detail)
            else { throw GatewayWireError.invalidFrame("observation has invalid image") }
            self = .image(
                try SessionFileReference(json: file), width: width, height: height, detail: detail)
        case "file":
            guard let file = json["file"] else {
                throw GatewayWireError.invalidFrame("observation has invalid file")
            }
            self = .file(try SessionFileReference(json: file))
        default:
            throw GatewayWireError.invalidFrame("unknown observation type")
        }
    }

    private enum CodingKeys: String, CodingKey {
        case type, text, image, file, width, height, detail
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .text(let text):
            try container.encode("input_text", forKey: .type)
            try container.encode(text, forKey: .text)
        case .file(let file):
            try container.encode("file", forKey: .type)
            try container.encode(file, forKey: .file)
        case .image(let file, let width, let height, let detail):
            try container.encode("input_image", forKey: .type)
            var image = container.nestedContainer(keyedBy: CodingKeys.self, forKey: .image)
            try image.encode(file, forKey: .file)
            try image.encode(width, forKey: .width)
            try image.encode(height, forKey: .height)
            try image.encode(detail, forKey: .detail)
        }
    }
}
