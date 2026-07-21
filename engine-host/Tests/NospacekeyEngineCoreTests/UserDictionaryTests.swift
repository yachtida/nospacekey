import XCTest
@testable import NospacekeyEngineCore
import KanaKanjiConverterModuleWithDefaultDictionary

final class UserDictionaryTests: XCTestCase {
    private func writeTempJson(_ s: String) throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("ud-test-\(UUID().uuidString).json")
        try Data(s.utf8).write(to: url)
        return url
    }

    func testLoadParsesJsonAndMapsPos() throws {
        let json = #"[{"ruby":"やちだ","word":"谷内田","pos":"人名(姓)"},{"ruby":"ほげ","word":"ホゲ株式会社","pos":"組織"},{"ruby":"ふが","word":"fuga","pos":"謎の品詞"}]"#
        let url = try writeTempJson(json)
        defer { try? FileManager.default.removeItem(at: url) }
        let dic = UserDictionary.load(url: url)
        XCTAssertEqual(dic.count, 3)
        // ruby はカタカナ正規化必須(DicdataStore は読みを toKatakana してから完全一致で索く)
        XCTAssertEqual(dic[0].ruby, "ヤチダ")
        XCTAssertEqual(dic[0].word, "谷内田")
        XCTAssertEqual(dic[0].lcid, CIDData.人名姓.cid)
        XCTAssertEqual(dic[1].lcid, CIDData.固有名詞組織.cid)
        XCTAssertEqual(dic[2].lcid, CIDData.一般名詞.cid)   // 未知品詞はフォールバック
        XCTAssertTrue(dic.allSatisfy { $0.mid == MIDData.一般.mid })
    }

    /// pos 欠落(convert-user-dict.ps1 は品詞列が無い行で pos キー自体を省く)→ 一般名詞。
    func testLoadTreatsMissingPosAsCommonNoun() throws {
        let url = try writeTempJson(#"[{"ruby":"ふが","word":"fuga"}]"#)
        defer { try? FileManager.default.removeItem(at: url) }
        let dic = UserDictionary.load(url: url)
        XCTAssertEqual(dic.count, 1)
        XCTAssertEqual(dic[0].lcid, CIDData.一般名詞.cid)
    }

    /// 壊れた JSON・不在ファイルは空配列へ劣化(起動を止めない)。
    func testLoadDegradesToEmptyOnBrokenJson() throws {
        let broken = try writeTempJson(#"[{"ruby": "や"#)
        defer { try? FileManager.default.removeItem(at: broken) }
        XCTAssertEqual(UserDictionary.load(url: broken).count, 0)
        let missing = FileManager.default.temporaryDirectory
            .appendingPathComponent("ud-missing-\(UUID().uuidString).json")
        XCTAssertEqual(UserDictionary.load(url: missing).count, 0)
    }

    /// UTF-8 BOM 付き JSON(PS 5.1 の Set-Content -Encoding UTF8 が付ける)も読める。
    func testLoadStripsUtf8Bom() throws {
        var data = Data([0xEF, 0xBB, 0xBF])
        data.append(Data(#"[{"ruby":"てすと","word":"テスト"}]"#.utf8))
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("ud-bom-\(UUID().uuidString).json")
        try data.write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }
        let dic = UserDictionary.load(url: url)
        XCTAssertEqual(dic.count, 1)
        XCTAssertEqual(dic[0].ruby, "テスト")
    }

    /// 空 ruby/word のエントリは捨てる(DicdataStore に空読みを入れない)。
    func testLoadSkipsEmptyRubyOrWord() throws {
        let url = try writeTempJson(#"[{"ruby":"","word":"x"},{"ruby":"あ","word":""},{"ruby":"あ","word":"亜"}]"#)
        defer { try? FileManager.default.removeItem(at: url) }
        let dic = UserDictionary.load(url: url)
        XCTAssertEqual(dic.count, 1)
        XCTAssertEqual(dic[0].word, "亜")
    }

    /// Google/MS-IME の品詞名 → CID マップ。「姓」「名」は単独でも現れる。
    /// 「名詞」「固有名詞」の「名」に誤爆しないこと。
    func testCidMapping() {
        XCTAssertEqual(UserDictionary.cid(for: "人名(姓)"), CIDData.人名姓.cid)
        XCTAssertEqual(UserDictionary.cid(for: "人名(名)"), CIDData.人名名.cid)
        XCTAssertEqual(UserDictionary.cid(for: "人名"), CIDData.人名一般.cid)
        XCTAssertEqual(UserDictionary.cid(for: "姓"), CIDData.人名姓.cid)
        XCTAssertEqual(UserDictionary.cid(for: "名"), CIDData.人名名.cid)
        XCTAssertEqual(UserDictionary.cid(for: "組織"), CIDData.固有名詞組織.cid)
        XCTAssertEqual(UserDictionary.cid(for: "地名"), CIDData.地名一般.cid)
        XCTAssertEqual(UserDictionary.cid(for: "駅"), CIDData.地名一般.cid)
        XCTAssertEqual(UserDictionary.cid(for: "固有名詞"), CIDData.固有名詞.cid)
        XCTAssertEqual(UserDictionary.cid(for: "数"), CIDData.数.cid)
        XCTAssertEqual(UserDictionary.cid(for: "名詞"), CIDData.一般名詞.cid)
        XCTAssertEqual(UserDictionary.cid(for: nil), CIDData.一般名詞.cid)
        XCTAssertEqual(UserDictionary.cid(for: ""), CIDData.一般名詞.cid)
    }

    func testResolvePrefersEnvOverride() {
        let url = UserDictionary.resolve(environment: [
            "NOSPACEKEY_USER_DICT": "/tmp/somewhere/dict.json",
            "LOCALAPPDATA": "/tmp/lad",
        ])
        // override は実在チェックなしで採用(壊れ/不在は load が空へ劣化)
        XCTAssertEqual(url?.lastPathComponent, "dict.json")
    }

    func testResolveUsesLocalAppDataOnlyIfFileExists() throws {
        let base = FileManager.default.temporaryDirectory
            .appendingPathComponent("ud-lad-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: base) }
        // 不在 → nil(移行していないユーザーの正常系)
        XCTAssertNil(UserDictionary.resolve(environment: ["LOCALAPPDATA": base.path]))
        // 実在 → 採用
        let dir = base.appendingPathComponent("nospacekey")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try Data("[]".utf8).write(to: dir.appendingPathComponent("user_dictionary.json"))
        let url = UserDictionary.resolve(environment: ["LOCALAPPDATA": base.path])
        XCTAssertEqual(url?.lastPathComponent, "user_dictionary.json")
    }

    func testResolveReturnsNilWithoutAnyEnv() {
        XCTAssertNil(UserDictionary.resolve(environment: [:]))
    }

    func testBuiltinDateTemplatesCoverKyou() {
        let t = UserDictionary.builtinDateTemplates()
        XCTAssertEqual(t.count, 9)   // 3読み × 3形式
        XCTAssertTrue(t.contains { $0.ruby == "キョウ" })
        XCTAssertTrue(t.contains { $0.ruby == "アシタ" })
        XCTAssertTrue(t.contains { $0.ruby == "キノウ" })
        // DateTemplateLiteral のエクスポート形式(Candidate.parseTemplate が展開する)
        XCTAssertTrue(t.allSatisfy { $0.word.hasPrefix("<date format=\"") && $0.word.hasSuffix("\">") })
        // import は " " split でパースするため format にスペースを含めない
        XCTAssertTrue(t.allSatisfy { $0.word.components(separatedBy: " ").count == 6 })
        // きょう=delta 0 / あした=+1日 / きのう=-1日(deltaunit=86400秒)
        XCTAssertTrue(t.filter { $0.ruby == "アシタ" }.allSatisfy { $0.word.contains("delta=\"1\" deltaunit=\"86400\"") })
        XCTAssertTrue(t.filter { $0.ruby == "キノウ" }.allSatisfy { $0.word.contains("delta=\"-1\" deltaunit=\"86400\"") })
    }
}
