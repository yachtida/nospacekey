import XCTest
import Foundation
import WinSDK
@testable import NospacekeyEngineCore
import KanaKanjiConverterModuleWithDefaultDictionary

/// スレッドを跨いで1値を受け渡す最小の箱（Swift 6 の Sendable 検査のため。
/// 生の var キャプチャは背景スレッドのクロージャで弾かれる）。
private final class Box<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var stored: T
    init(_ v: T) { stored = v }
    var value: T {
        get { lock.lock(); defer { lock.unlock() }; return stored }
        set { lock.lock(); stored = newValue; lock.unlock() }
    }
}

/// カスタム辞書のリロード適用機構（desired 状態+直列キュー+onListening+3値ロード）。
/// spec: docs/superpowers/specs/2026-08-02-custom-dictionary-design.md §4.1
final class UserDictionaryReloadTests: XCTestCase {
    /// environment は辞書ファイル/有効トグルを注入する唯一の入口（env 直読テスト禁止の既存契約）。
    private func makeService(environment: [String: String] = [:],
                             dictionaryRetryDelay: DispatchTimeInterval = .milliseconds(100)) -> ConversionService {
        ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                          environment: environment,
                          dictionaryRetryDelay: dictionaryRetryDelay)
    }

    /// 読み（かな）を direct 挿入して変換した候補列。reconvert と同じ「かなを丸ごと入れる」経路。
    private func candidates(_ svc: ConversionService, reading: String) -> [String] {
        let sid = svc.startSession()
        defer { svc.endSession(session: sid) }
        _ = svc.insert(session: sid, text: reading, style: "direct")
        return svc.convert(session: sid) ?? []
    }

    private func convertContains(_ svc: ConversionService, reading: String, word: String) -> Bool {
        candidates(svc, reading: reading).contains(word)
    }

    /// 組み込み日付テンプレートが生きているか（辞書 OFF でもテンプレートは残る、の観測）。
    /// 23:59:59 跨ぎ防御で変換の前後両方の日付を許す（既存 testDateTemplateExpandsToTodayInCandidates と同型）。
    private func containsToday(_ svc: ConversionService) -> Bool {
        let f = DateFormatter()
        f.dateFormat = "yyyy年MM月dd日"
        f.locale = Locale(identifier: "ja_JP")
        f.calendar = Calendar(identifier: .gregorian)
        let before = f.string(from: Date())
        let cands = candidates(svc, reading: "きょう")
        return cands.contains(before) || cands.contains(f.string(from: Date()))
    }

    private func reload(_ svc: ConversionService, enabled: Bool) {
        svc.requestDictionaryReload(enabled: enabled)
        svc.flushDictionaryQueueForTesting()
    }

    /// 観測語は**素の辞書から出ない**組み合わせにする。「やちだ→谷内田」のような実在語だと
    /// 「辞書 OFF で消える」「置換で旧語が消える」の陰性アサートが素の変換候補で常に真になり空虚化する。
    private let probeReading = "ぬるぽ"
    private let probeWord = "零号機"
    private let probeJson = #"[{"ruby":"ぬるぽ","word":"零号機","pos":"名詞"}]"#
    private let swappedWord = "壱号機"
    private let swappedJson = #"[{"ruby":"ぬるぽ","word":"壱号機","pos":"名詞"}]"#

    func testEnabledEnvPureFunction() {
        XCTAssertFalse(UserDictionary.enabled(environment: ["NOSPACEKEY_USER_DICT_ENABLED": "0"]))
        XCTAssertTrue(UserDictionary.enabled(environment: ["NOSPACEKEY_USER_DICT_ENABLED": "1"]))
        XCTAssertTrue(UserDictionary.enabled(environment: [:]))          // 未設定=有効
        XCTAssertTrue(UserDictionary.enabled(environment: ["NOSPACEKEY_USER_DICT_ENABLED": ""]))
    }

    /// リロードは丸ごと置換（importDynamicUserDictionary の意味論）— 旧ファイルの語は消える。
    func testReloadSwapsDictionaryWholesale() throws {
        let url = try writeTempJson(probeJson)
        defer { try? FileManager.default.removeItem(at: url) }
        let svc = makeService(environment: ["NOSPACEKEY_USER_DICT": url.path])
        reload(svc, enabled: true)
        XCTAssertTrue(convertContains(svc, reading: probeReading, word: probeWord))

        try Data(swappedJson.utf8).write(to: url)
        reload(svc, enabled: true)
        XCTAssertTrue(convertContains(svc, reading: probeReading, word: swappedWord))
        XCTAssertFalse(convertContains(svc, reading: probeReading, word: probeWord),
                       "差し替え前のエントリが残っている（丸ごと置換になっていない）")
    }

    /// enabled=false は辞書語を落とすが、組み込み日付テンプレートは残す。true へ戻すと復活する。
    func testDisabledSkipsFileAndReenableRestores() throws {
        let url = try writeTempJson(probeJson)
        defer { try? FileManager.default.removeItem(at: url) }
        let svc = makeService(environment: ["NOSPACEKEY_USER_DICT": url.path])
        reload(svc, enabled: true)
        XCTAssertTrue(convertContains(svc, reading: probeReading, word: probeWord))

        reload(svc, enabled: false)
        XCTAssertFalse(convertContains(svc, reading: probeReading, word: probeWord))
        XCTAssertTrue(containsToday(svc), "OFF で日付テンプレートまで落ちている")

        reload(svc, enabled: true)
        XCTAssertTrue(convertContains(svc, reading: probeReading, word: probeWord))
    }

    /// 評価順序は enabled が先（spec §4.1）: 読み失敗の「現状維持」短絡が OFF を食わない。
    func testDisabledWinsOverReadFailure() throws {
        let url = try writeTempJson(probeJson)
        let svc = makeService(environment: ["NOSPACEKEY_USER_DICT": url.path])
        reload(svc, enabled: true)
        XCTAssertTrue(convertContains(svc, reading: probeReading, word: probeWord))

        try FileManager.default.removeItem(at: url)   // 以後どの読みも失敗する
        reload(svc, enabled: false)
        XCTAssertFalse(convertContains(svc, reading: probeReading, word: probeWord),
                       "読み失敗の現状維持が OFF を握りつぶした")
    }

    /// 読み・デコード失敗は現状維持（空への全置換をしない — spec §4.1 の3値）。
    func testReadFailureKeepsCurrentDictionary() throws {
        let url = try writeTempJson(probeJson)
        let svc = makeService(environment: ["NOSPACEKEY_USER_DICT": url.path])
        reload(svc, enabled: true)
        XCTAssertTrue(convertContains(svc, reading: probeReading, word: probeWord))

        try FileManager.default.removeItem(at: url)
        reload(svc, enabled: true)
        XCTAssertTrue(convertContains(svc, reading: probeReading, word: probeWord),
                      "一過性の読み失敗で辞書が全消滅した")
    }

    func testFailedReloadRetriesLaterAndKeepsOnlyTheLatestDesiredState() throws {
        let url = try writeTempJson(probeJson)
        defer { try? FileManager.default.removeItem(at: url) }
        let svc = makeService(environment: ["NOSPACEKEY_USER_DICT": url.path],
                              dictionaryRetryDelay: .seconds(30))
        reload(svc, enabled: true)
        XCTAssertTrue(convertContains(svc, reading: probeReading, word: probeWord))

        try Data("{".utf8).write(to: url)
        svc.requestDictionaryReload(enabled: true)
        svc.flushDictionaryQueueForTesting()
        XCTAssertTrue(convertContains(svc, reading: probeReading, word: probeWord),
                      "待機中の再試行より前に常駐辞書を失っている")

        try Data(swappedJson.utf8).write(to: url)
        XCTAssertTrue(svc.releaseDictionaryRetryForTesting())
        XCTAssertTrue(convertContains(svc, reading: probeReading, word: swappedWord))

        try Data("{".utf8).write(to: url)
        svc.requestDictionaryReload(enabled: true)
        svc.flushDictionaryQueueForTesting()
        svc.requestDictionaryReload(enabled: false)
        svc.flushDictionaryQueueForTesting()
        XCTAssertFalse(svc.releaseDictionaryRetryForTesting(),
                       "新しいリロードが旧世代の待機中再試行を置換していない")
        XCTAssertFalse(convertContains(svc, reading: probeReading, word: swappedWord))
    }

    /// GUI 初回登録シナリオ: 起動時に %LOCALAPPDATA%\nospacekey\user_dictionary.json が
    /// 不在（resolve=nil）でも、後から作られたファイルをリロードが拾う（resolve のやり直し）。
    /// NOSPACEKEY_USER_DICT ではなく LOCALAPPDATA を注入するのは、override 経路は実在チェックを
    /// しないため URL をキャッシュする壊れた実装でも緑になる（偽緑）ため。
    func testResolveRetriesSoFirstRegistrationWorks() throws {
        let base = FileManager.default.temporaryDirectory
            .appendingPathComponent("ud-lad-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: base, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: base) }
        let svc = makeService(environment: ["LOCALAPPDATA": base.path])
        reload(svc, enabled: true)
        XCTAssertFalse(convertContains(svc, reading: probeReading, word: probeWord))

        let dir = base.appendingPathComponent("nospacekey")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try Data(probeJson.utf8).write(to: dir.appendingPathComponent("user_dictionary.json"))
        reload(svc, enabled: true)
        XCTAssertTrue(convertContains(svc, reading: probeReading, word: probeWord),
                      "起動時不在だったファイルが拾われない（resolve をやり直していない）")
    }

    /// classic 増分ラティスの回帰: 同一セッションで同一読みを再変換しても新語が出る
    /// （previousInputData 一致で辞書を索き直さない経路を stopComposition が落とす）。
    func testSameSessionSameReadingGetsNewWordAfterReload() throws {
        let url = try writeTempJson("[]")
        defer { try? FileManager.default.removeItem(at: url) }
        let svc = makeService(environment: ["NOSPACEKEY_USER_DICT": url.path])
        reload(svc, enabled: true)
        let sid = svc.startSession()
        defer { svc.endSession(session: sid) }
        _ = svc.insert(session: sid, text: probeReading, style: "direct")
        XCTAssertFalse((svc.convert(session: sid) ?? []).contains(probeWord))

        try Data(probeJson.utf8).write(to: url)
        reload(svc, enabled: true)
        XCTAssertTrue((svc.convert(session: sid) ?? []).contains(probeWord),
                      "同一セッション・同一読みの再変換に新語が出ない（増分ラティスが残っている）")
    }

    /// NFD かな（か+U+3099）の ruby が NFC 入力「がっこう」でヒットする
    /// （Swift String == の正準等価。Rust 側 normalize_key の合成規約はこの前提に立つ — spec §3.2）。
    func testNfdRubyHitsWithNfcInput() throws {
        let url = try writeTempJson("[{\"ruby\":\"か\u{3099}っこう\",\"word\":\"楽校\"}]")
        defer { try? FileManager.default.removeItem(at: url) }
        let svc = makeService(environment: ["NOSPACEKEY_USER_DICT": url.path])
        reload(svc, enabled: true)
        XCTAssertTrue(convertContains(svc, reading: "がっこう", word: "楽校"))
    }

    /// spec §3.4 の分類表（Rust canonical_pos テストと同じ表）を Swift 側で固定する。
    func testCanonicalEightPosResolveToExpectedCids() {
        let table: [(String?, Int)] = [
            (nil, CIDData.一般名詞.cid), ("名詞", CIDData.一般名詞.cid),
            ("人名", CIDData.人名一般.cid), ("姓", CIDData.人名姓.cid), ("名", CIDData.人名名.cid),
            ("固有名詞", CIDData.固有名詞.cid), ("組織", CIDData.固有名詞組織.cid),
            ("地名", CIDData.地名一般.cid), ("数", CIDData.数.cid),
        ]
        for (pos, want) in table {
            XCTAssertEqual(UserDictionary.cid(for: pos), want, "pos=\(pos ?? "nil")")
        }
    }

    /// desired を書くのは init と ReloadDictionary ハンドラだけ（spec §4.1 の不変条件）。
    /// 起動時 enqueue（onListening）が env 値で desired を書き戻すと、pipe 開通直後に届いた
    /// OFF が有効へ戻る競合窓ができる。
    func testStartupEnqueueDoesNotOverwriteDesired() throws {
        let url = try writeTempJson(probeJson)
        defer { try? FileManager.default.removeItem(at: url) }
        let svc = makeService(environment: ["NOSPACEKEY_USER_DICT": url.path,
                                            "NOSPACEKEY_USER_DICT_ENABLED": "1"])
        svc.requestDictionaryReload(enabled: false)   // OFF を指示
        svc.enqueueDictionaryReload()                 // 起動時 enqueue 相当（desired を書かない）
        svc.flushDictionaryQueueForTesting()
        XCTAssertFalse(convertContains(svc, reading: probeReading, word: probeWord),
                       "起動時 enqueue が desired を env 値へ書き戻した")
    }

    // ---- onListening（spawn 窓の閉塞 — spec §4.1）----

    func testOnceGuardFiresExactlyOnce() {
        let once = OnceFlag()
        var count = 0
        once.fireOnce { count += 1 }
        once.fireOnce { count += 1 }
        XCTAssertEqual(count, 1)
    }

    /// onListening は初回 pipe 作成後・接続受付前に呼ばれる（enqueue の時刻ではなく
    /// 再読の時刻が pipe 作成より後であることが spawn 窓閉塞の条件）。
    /// pipe 名は UUID 付きの一意名 — 実稼働の \\.\pipe\nospacekey-engine と衝突させない。
    func testOnListeningFiresAfterPipeCreationBeforeAccept() {
        let pipeName = #"\\.\pipe\nospacekey-onlistening-"# + UUID().uuidString
        let server = NamedPipeServer(pipeName: pipeName)
        let fired = DispatchSemaphore(value: 0)
        let returned = DispatchSemaphore(value: 0)
        let pipeReachable = Box(false)
        Thread.detachNewThread {
            server.run(handler: { _, _ in (Data(), false) },
                       oneShot: true,
                       onListening: {
                           pipeReachable.value = pipeName.withCString(encodedAs: UTF16.self) { p -> Bool in
                               WaitNamedPipeW(p, 0) != false
                           }
                           fired.signal()
                       })
            returned.signal()
        }
        XCTAssertEqual(fired.wait(timeout: .now() + 5), .success, "onListening が呼ばれない")
        XCTAssertTrue(pipeReachable.value, "onListening が pipe 作成より前に呼ばれている")

        // ダミークライアントで接続し、切断して oneShot の run を返させる（サーバを残さない）。
        let client: HANDLE = pipeName.withCString(encodedAs: UTF16.self) { p in
            // WinSDK の GENERIC_READ は DWORD、GENERIC_WRITE は Int32 なので型を揃えてから OR する。
            CreateFileW(p, GENERIC_READ | DWORD(GENERIC_WRITE), 0, nil,
                        DWORD(OPEN_EXISTING), 0, nil)
        }
        XCTAssertNotEqual(client, INVALID_HANDLE_VALUE, "onListening 後のクライアント接続が失敗した")
        CloseHandle(client)
        XCTAssertEqual(returned.wait(timeout: .now() + 5), .success, "oneShot の run が戻らない")
    }
}
