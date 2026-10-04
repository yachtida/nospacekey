import Foundation

extension ConversionService {
    /// 自動変換だけで使う一文字かなの表示。合成後の読みで判定し、未完ローマ字は含めない。
    /// NFCで文字数を判定するが、IPCの読み座標を保つため返す文字列の正準合成は変更しない。
    static func automaticSingleKana(_ reading: String) -> String? {
        let hiragana = normalizeKana(reading)
        let composed = hiragana.precomposedStringWithCanonicalMapping
        guard composed.unicodeScalars.count == 1,
              let scalar = composed.unicodeScalars.first,
              (0x3041...0x3096).contains(scalar.value) else { return nil }
        return hiragana
    }
}
