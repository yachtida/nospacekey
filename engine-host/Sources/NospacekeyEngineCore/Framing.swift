import Foundation

enum Framing {
    static func encode<T: Encodable>(_ value: T) throws -> Data {
        let body = try JSONEncoder().encode(value)
        var len = UInt32(body.count).littleEndian
        var out = Data(bytes: &len, count: 4)
        out.append(body)
        return out
    }
    static func decode<T: Decodable>(_ type: T.Type, from body: Data) throws -> T {
        try JSONDecoder().decode(type, from: body)
    }
}
