/// 修正変換(TypoConvert)の修復仮説生成。ローマ字入力の「同一英字ちょうど2連打」を
/// タイポ(例: shitekudassai の ss)とみなし、1文字へ縮約した仮説を列挙する純関数。
/// converter/セッションに一切触れない（ConversionService.typoConvert が呼び出す）。
enum TypoRepair {
    /// 縮約サイト = 同一英字(a-z、n を除く)がちょうど2連打された区間の開始 index（長さは常に2）。
    /// n の2連打（「ん」の正当な定石）と、3連打以上の run（意図的な連打）はサイトにしない。
    private static func findSites(_ chars: [Character]) -> [Int] {
        var sites: [Int] = []
        var i = 0
        while i < chars.count {
            let c = chars[i]
            var j = i
            while j < chars.count, chars[j] == c { j += 1 }
            let runLength = j - i
            if runLength == 2, c.isASCII, c.isLowercase, c != "n" {
                sites.append(i)
            }
            i = j
        }
        return sites
    }

    /// 0..<n から k 個選ぶ組合せを昇順(combinations 標準順)で列挙する。
    private static func combinations(_ n: Int, _ k: Int) -> [[Int]] {
        guard k > 0 else { return [[]] }
        var result: [[Int]] = []
        func pick(_ start: Int, _ chosen: [Int]) {
            if chosen.count == k { result.append(chosen); return }
            guard start < n else { return }
            for i in start..<n { pick(i + 1, chosen + [i]) }
        }
        pick(0, [])
        return result
    }

    /// 選んだサイトの2文字目を落として1仮説の文字列を組み立てる。
    private static func collapse(_ chars: [Character], sites: [Int], selected: [Int]) -> String {
        let skip = Set(selected.map { sites[$0] + 1 })
        return String(chars.enumerated().compactMap { idx, c in skip.contains(idx) ? nil : c })
    }

    /// 仮説 = 縮約サイトの非空部分集合ごとに該当サイトを縮約した文字列。
    /// 並び = サイズ昇順（単一サイト→2サイト→…）、同サイズ内は combinations の昇順。
    /// 計 `cap` 件でキャップする（サイト数が多いと組合せが爆発するため）。
    private static let cap = 8

    static func hypotheses(roman: String) -> [String] {
        let chars = Array(roman)
        let sites = findSites(chars)
        guard !sites.isEmpty else { return [] }
        var results: [String] = []
        outer: for k in 1...sites.count {
            for combo in combinations(sites.count, k) {
                results.append(collapse(chars, sites: sites, selected: combo))
                if results.count == cap { break outer }
            }
        }
        return results
    }
}
