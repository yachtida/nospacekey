import Foundation
import KanaKanjiConverterModuleWithDefaultDictionary

/// ユーザ辞書(Google日本語入力/MS-IME からのワンショット移行 JSON)のロードと、
/// 組み込み日付テンプレート(きょう/あした/きのう → 実日付候補)。
///
/// パス解決は ZenzaiConfig/LearningSettings と同型: env 優先 → `%LOCALAPPDATA%\nospacekey` 既定。
/// JSON は scripts/convert-user-dict.ps1 が生成する
/// `[{"ruby":"やちだ","word":"谷内田","pos":"人名(姓)"},...]`(pos は IME の原文字列。
/// pos→CID のマップは本型の `cid(for:)` に一元化する)。
///
/// ロード結果は `KanaKanjiConverter.importDynamicUserDictionary` へ渡す。これは**丸ごと置換**
/// (DicdataStoreState.importDynamicUserDictionary が配列を代入するだけ — lib 0.11.2)なので、
/// 呼び出し側(ConversionService.loadUserDictionary)は組み込みテンプレートと結合した全量を
/// 1回で渡す。
public enum UserDictionary {
    struct Entry: Decodable {
        let ruby: String
        let word: String
        let pos: String?
    }

    /// ユーザ辞書 JSON のパスを解決する。
    /// 1. env `NOSPACEKEY_USER_DICT`(非空)— テスト/診断用 override。実在チェックはしない
    ///    (壊れ/不在は load が空配列へ劣化)
    /// 2. `%LOCALAPPDATA%\nospacekey\user_dictionary.json` — **実在する場合のみ**
    ///    (移行を実行していないユーザーが大多数なので、不在は「辞書なし」の正常系)
    public static func resolve(environment: [String: String] = ProcessInfo.processInfo.environment) -> URL? {
        if let p = environment["NOSPACEKEY_USER_DICT"], !p.isEmpty {
            return URL(fileURLWithPath: p)
        }
        guard let base = environment["LOCALAPPDATA"], !base.isEmpty else { return nil }
        let url = URL(fileURLWithPath: base)
            .appendingPathComponent("nospacekey")
            .appendingPathComponent("user_dictionary.json")
        return FileManager.default.fileExists(atPath: url.path) ? url : nil
    }

    /// Google日本語入力/MS-IME の品詞名 → CID。CIDData の名前付き case の範囲でマップし、
    /// 未知の品詞は一般名詞(1285)へフォールバックする。
    /// 注: Google のエクスポートは「姓」「名」が単独の品詞名で現れる。「名詞」「固有名詞」も
    /// 「名」を含むため、単純な contains("名") では誤爆する — 人名系は「人名」を除いた残りで判定。
    static func cid(for pos: String?) -> Int {
        guard let p = pos, !p.isEmpty else { return CIDData.一般名詞.cid }
        if p.contains("人名") {
            if p.contains("姓") { return CIDData.人名姓.cid }
            // 「人名(名)」等 — 「人名」自身の「名」に反応しないよう除去してから判定
            if p.replacingOccurrences(of: "人名", with: "").contains("名") { return CIDData.人名名.cid }
            return CIDData.人名一般.cid
        }
        if p == "姓" { return CIDData.人名姓.cid }
        if p == "名" { return CIDData.人名名.cid }
        if p.contains("組織") { return CIDData.固有名詞組織.cid }
        if p.contains("地名") || p.contains("駅") { return CIDData.地名一般.cid }
        if p.contains("固有") { return CIDData.固有名詞.cid }
        if p.contains("数") { return CIDData.数.cid }
        return CIDData.一般名詞.cid
    }

    /// 移行 JSON を [DicdataElement] へロードする。読めない/壊れた JSON は空配列へ劣化
    /// (黙って壊れない、はログを出す呼び出し側の責務 — loaded=0 が観測される)。
    ///
    /// ruby は**カタカナへ正規化必須**: DicdataStore は動的ユーザ辞書を索く前に読みを
    /// `toKatakana()` してから `$0.ruby == ruby` の完全一致で照合する
    /// (lib DicdataStore.swift:369,873)。ひらがな ruby のままでは永遠にヒットしない。
    public static func load(url: URL) -> [DicdataElement] {
        guard var data = try? Data(contentsOf: url) else { return [] }
        // PS 5.1 の Set-Content -Encoding UTF8 等が付けうる UTF-8 BOM は剥がす(防御)。
        if data.starts(with: [0xEF, 0xBB, 0xBF]) { data.removeFirst(3) }
        guard let entries = try? JSONDecoder().decode([Entry].self, from: data) else { return [] }
        return entries.compactMap { e in
            let ruby = ConversionService.toKatakana(e.ruby.trimmingCharacters(in: .whitespaces))
            let word = e.word.trimmingCharacters(in: .whitespaces)
            guard !ruby.isEmpty, !word.isEmpty else { return nil }
            // value=-5 は仮値(plan Open Risk): 辞書語が上位に出すぎ/出なさすぎなら実機で調整。
            return DicdataElement(word: word, ruby: ruby, cid: cid(for: e.pos),
                                  mid: MIDData.一般.mid, value: -5)
        }
    }

    /// 「きょう/あした/きのう」→ 実日付へ展開されるテンプレートエントリ(3読み×3形式=9件)。
    /// word は DateTemplateLiteral のエクスポート形式(lib TemplateData.swift:178-182)で、
    /// `Candidate.parseTemplate()` が変換結果整形時に無条件で実日付へ展開する
    /// (lib Candidate.swift:190-210、実例 TemplateConversionTests.swift:12-14)。
    /// delta は日数、deltaunit は秒係数(86400=1日) — previewString が
    /// `Date().advanced(by: delta * deltaUnit)` で加算する(lib TemplateData.swift:159)。
    /// 注意: format にスペースを入れないこと(import が " " split でパースする — 同:163)。
    public static func builtinDateTemplates() -> [DicdataElement] {
        let specs: [(ruby: String, delta: Int)] = [("キョウ", 0), ("アシタ", 1), ("キノウ", -1)]
        let formats = ["yyyy年MM月dd日", "yyyy/MM/dd", "MM月dd日"]
        return specs.flatMap { spec in
            formats.map { fmt in
                DicdataElement(
                    word: "<date format=\"\(fmt)\" type=\"western\" language=\"ja_JP\" delta=\"\(spec.delta)\" deltaunit=\"86400\">",
                    ruby: spec.ruby,
                    cid: CIDData.一般名詞.cid,
                    mid: MIDData.一般.mid,
                    // -18 は仮値(plan Open Risk): 通常候補の順位を荒らさない弱さを狙う。
                    value: -18
                )
            }
        }
    }
}
