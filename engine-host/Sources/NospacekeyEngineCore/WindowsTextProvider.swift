import Foundation
import NospacekeyWindowsText

enum ConversionEngine: String, Decodable {
    case azookey
    case microsoft
    case hybrid

    static func resolve(environment: [String: String]) -> Self {
        Self(rawValue: environment["NOSPACEKEY_CONVERSION_ENGINE"] ?? "") ?? .azookey
    }
}

struct WindowsTextProvider: Sendable {
    var candidates: @Sendable (_ reading: String, _ prediction: Bool, _ limit: Int) -> [String]

    static let native = WindowsTextProvider { reading, prediction, limit in
        final class Buffer { var values: [String] = [] }
        let buffer = Buffer()
        let input = Array(reading.utf16)
        guard !input.isEmpty, input.count <= 4096 else { return [] }
        let status = input.withUnsafeBufferPointer { input in
            nsk_windows_text_candidates(input.baseAddress, UInt32(input.count), prediction ? 1 : 0,
                UInt32(min(256, max(1, limit))), 250, { context, text, length in
                    guard let context, let text else { return }
                    let buffer = Unmanaged<Buffer>.fromOpaque(context).takeUnretainedValue()
                    buffer.values.append(String(decoding: UnsafeBufferPointer(start: text, count: Int(length)), as: UTF16.self))
                }, Unmanaged.passUnretained(buffer).toOpaque())
        }
        guard status >= 0 else {
            // Candidate text and readings must never enter diagnostic logs.
            engineLog("ev=windows_text_unavailable hresult=\(status)\n")
            return []
        }
        return buffer.values
    }
}
