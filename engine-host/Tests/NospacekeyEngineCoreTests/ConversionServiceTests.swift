import XCTest
@testable import NospacekeyEngineCore

final class ConversionServiceTests: XCTestCase {
    func testInsertReturnsHiraganaReading() {
        let svc = ConversionService()
        let sid = svc.startSession()
        let reading = svc.insert(session: sid, text: "nihongo")
        XCTAssertEqual(reading, "にほんご")
    }

    // ---- Shift英語モード: style="direct" はリテラル挿入（roman2kana を通さない）----

    func testInsertDirectStyleKeepsLiteralAscii() {
        let svc = ConversionService()
        let sid = svc.startSession()
        let reading = svc.insert(session: sid, text: "Abc", style: "direct")
        XCTAssertEqual(reading, "Abc")
    }

    func testInsertMixedRomanThenDirectAppendsToSameReading() {
        // かな合成と同一の未確定へ継ぎ足す（MS-IME 互換の核）: roman2kana 部と direct 部が
        // 1つの ComposingText に共存し、読みが「きょうA」になる。
        let svc = ConversionService()
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "kyou")
        let reading = svc.insert(session: sid, text: "A", style: "direct")
        XCTAssertEqual(reading, "きょうA")
    }

    func testConvertReturnsKanjiCandidate() {
        let svc = ConversionService()
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "nihongo")
        let candidates = svc.convert(session: sid)
        XCTAssertTrue(candidates?.contains("日本語") ?? false, "expected 日本語 in \(String(describing: candidates))")
    }

    func testDigitsYieldFullWidthCandidate() {
        // classic 固定（Zenzai 無し）で決定的に。読み "123" に全角 "１２３" 候補が出る。
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "123")
        let candidates = svc.convert(session: sid) ?? []
        XCTAssertTrue(candidates.contains("１２３"), "expected 全角 １２３ in \(candidates)")
    }

    func testKanaConversionNotPollutedByFullWidthOption() {
        // 純かな読みは全角オプションの影響を受けない（回帰なし）。
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "nihongo")
        let candidates = svc.convert(session: sid) ?? []
        XCTAssertTrue(candidates.contains("日本語"), "expected 日本語 still present in \(candidates)")
    }

    func testBackspaceShortensReading() {
        let svc = ConversionService()
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "ka")
        _ = svc.insert(session: sid, text: "ki")
        let r = svc.backspace(session: sid)
        XCTAssertEqual(r, "か")
    }

    /// M-6: 未知セッションは「空の正当な結果」と区別して nil（呼び出し側で .error("no session")）。
    func testUnknownSessionReturnsNil() {
        let svc = ConversionService()
        let unknown = 99999
        XCTAssertNil(svc.insert(session: unknown, text: "a"))
        XCTAssertNil(svc.backspace(session: unknown))
        XCTAssertNil(svc.convert(session: unknown))
        XCTAssertNil(svc.liveConvert(session: unknown))
    }

    /// UU-5: reload で Zenzai 設定が差し替わる（設定アプリの変更を常駐エンジンへ反映）。
    /// 実在ファイルを weight に指定 → 有効化 / off → 無効化 を zenzaiEnabled で観測する。
    func testReloadSwapsZenzaiConfig() throws {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        XCTAssertFalse(svc.zenzaiEnabled, "初期は無効")
        let tmp = FileManager.default.temporaryDirectory
            .appendingPathComponent("uu5-weight-\(UUID().uuidString).gguf")
        try Data("x".utf8).write(to: tmp)
        defer { try? FileManager.default.removeItem(at: tmp) }
        svc.reload(overrides: ["NOSPACEKEY_ZENZAI": "on", "NOSPACEKEY_ZENZAI_WEIGHT": tmp.path])
        XCTAssertTrue(svc.zenzaiEnabled, "reload で weight が解決でき有効化されるはず")
        svc.reload(overrides: ["NOSPACEKEY_ZENZAI": "off"])
        XCTAssertFalse(svc.zenzaiEnabled, "off で無効化されるはず")
    }

    /// cold start ③: zenzaiReady ゲートが閉じている間（本番では listening 後〜warmUp 完了前 =
    /// warmUp スレッドが converterLock を取る前に届いた要求）、convert は Zenzai off（古典）で
    /// 即応し結果は非空。weight にはダミー実在ファイルを使う — ゲート閉の間は makeOptions が
    /// .off に落とすため、壊れた weight のロード（getModel）自体を踏まずに古典候補が返る。
    /// ゲートは warmUp 完了後に開く — 実モデルでの完了遷移は背景スレッド完了待ちになるため
    /// ZenzaiConversionTests（実モデル必須・skip 付き）で検証し、ここでは Zenzai 無効設定の
    /// 同期経路（startWarmUp が即開ける）で開閉遷移を決定的に検証する。
    func testConvertRespondsClassicWhileWarmingUp() throws {
        // (1) ゲート閉のまま変換 = warm-up 未完了中に届いた要求の状態。古典で即応・非空。
        let tmp = FileManager.default.temporaryDirectory
            .appendingPathComponent("coldstart-weight-\(UUID().uuidString).gguf")
        try Data("x".utf8).write(to: tmp)
        defer { try? FileManager.default.removeItem(at: tmp) }
        let svc = ConversionService(config: ZenzaiConfig(weightURL: tmp, inferenceLimit: 1))
        XCTAssertTrue(svc.zenzaiEnabled, "weight が解決できている前提")
        XCTAssertFalse(svc.zenzaiReady, "warmUp 完了前は Zenzai ゲートが閉じている")
        let sid = svc.startSession()
        for ch in "nihongo" { _ = svc.insert(session: sid, text: String(ch)) }
        let candidates = svc.convert(session: sid)   // ゲート閉 → 古典（辞書）変換で即応
        XCTAssertTrue(candidates?.contains("日本語") ?? false,
                      "expected 日本語 (classic) in \(String(describing: candidates))")
        XCTAssertFalse(svc.zenzaiReady, "ゲートを開くのは warmUp 完了時のみ（変換では開かない）")

        // (2) ゲート開の遷移: Zenzai 無効設定では startWarmUp が同期で即開ける（背景スレッド無し）。
        let svcOff = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        XCTAssertFalse(svcOff.zenzaiReady, "startWarmUp 前はゲートが閉じている")
        svcOff.startWarmUp()
        XCTAssertTrue(svcOff.zenzaiReady, "Zenzai 無効設定なら startWarmUp が同期でゲートを開く")
    }

    /// M-6: 既知セッションで composing が空でも nil ではなく空読み ""（空-but-valid を回帰させない）。
    func testKnownEmptySessionReturnsEmptyNotNil() {
        let svc = ConversionService()
        let sid = svc.startSession()
        XCTAssertEqual(svc.insert(session: sid, text: ""), "")
        XCTAssertEqual(svc.backspace(session: sid), "")
        XCTAssertEqual(svc.convert(session: sid), [])
    }

    /// TIP は1打鍵ごとに1文字だけ Insert する（key_event_sink.rs OnKeyDown）。
    /// 結合テストや上の各テストは語まるごと Insert なので、この"1文字ずつ"経路は未検証だった。
    /// 実機で「nihongo がローマ字のまま」になった症状の切り分け用。
    func testPerCharInsertBuildsReadingLikeTip() {
        let svc = ConversionService()
        let sid = svc.startSession()
        var last: String?
        for ch in "nihongo" {
            last = svc.insert(session: sid, text: String(ch))
        }
        XCTAssertEqual(last, "にほんご", "per-char insert should accumulate to にほんご, got \(String(describing: last))")
        let candidates = svc.convert(session: sid)
        XCTAssertTrue(candidates?.contains("日本語") ?? false, "expected 日本語 in \(String(describing: candidates))")
    }

    // --- 部分確定 commit(session:index:) （前方一致候補のデータロス対策） ---
    // 全て classic(weightURL: nil) で決定的に動く。候補は text 一致で引く（順位変化に強い）。

    private func classicNihongoSession() -> (ConversionService, Int, [String]) {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let sid = svc.startSession()
        for ch in "nihongo" { _ = svc.insert(session: sid, text: String(ch)) }
        let cands = svc.convert(session: sid) ?? []
        return (svc, sid, cands)
    }

    /// 前方一致候補「日本」(にほん) を確定すると、確定 text=日本・残り読み=ご を返す（ご を捨てない）。
    func testCommitPrefixCandidateReturnsRemainingReading() {
        let (svc, sid, cands) = classicNihongoSession()
        let idx = cands.firstIndex(of: "日本")
        XCTAssertNotNil(idx, "classic候補に 日本 が無い: \(cands)")
        let r = svc.commit(session: sid, index: idx!)
        XCTAssertNotNil(r, "commit が nil")
        XCTAssertEqual(r?.text, "日本")
        XCTAssertEqual(r?.reading, "ご", "残り読みは ご でなければならない")
    }

    /// 別の前方一致候補「に」を確定すると残り読み=ほんご。
    func testCommitShortPrefixLeavesLongerRemainder() {
        let (svc, sid, cands) = classicNihongoSession()
        let idx = cands.firstIndex(of: "に")
        XCTAssertNotNil(idx, "classic候補に に が無い: \(cands)")
        let r = svc.commit(session: sid, index: idx!)
        XCTAssertEqual(r?.text, "に")
        XCTAssertEqual(r?.reading, "ほんご")
    }

    /// 部分確定後もセッションは生存し、読みは残り(ご)に縮む（prefixComplete の書き戻し）。
    func testCommitPrefixKeepsSessionAliveWithRemainingReading() {
        let (svc, sid, cands) = classicNihongoSession()
        let r = svc.commit(session: sid, index: cands.firstIndex(of: "日本")!)
        XCTAssertEqual(r?.reading, "ご")
        let c2 = svc.convert(session: sid)
        XCTAssertNotNil(c2, "部分確定後もセッションは生存していなければならない")
        XCTAssertFalse(c2?.contains("日本語") ?? true, "読みが残り(ご)に縮んでいない（古い にほんご のまま）")
        XCTAssertTrue(c2?.contains("ご") ?? false, "残り読み ご が変換候補に現れるはず")
    }

    /// 全消費候補「日本語」(にほんご) を確定すると残り読み="" （従来どおりの全確定）。
    func testCommitFullCandidateClearsRemainder() {
        let (svc, sid, cands) = classicNihongoSession()
        let r = svc.commit(session: sid, index: cands.firstIndex(of: "日本語")!)
        XCTAssertEqual(r?.text, "日本語")
        XCTAssertEqual(r?.reading, "", "全消費候補は残り読み空")
    }

    /// 全確定→endSession 後、次の独立セッションの変換が前確定の文脈に汚染されない（I-1 回帰）。
    /// commit() の setCompletedData が残す completedData を endSession の stopComposition で消すこと。
    func testFullCommitDoesNotLeakContextIntoNextSession() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        // 基準: 冷えた converter で "go"(ご) を変換。
        let cold = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let cs = cold.startSession()
        for ch in "go" { _ = cold.insert(session: cs, text: String(ch)) }
        let baseline = cold.convert(session: cs) ?? []

        // 同一 converter で nihongo を全確定→endSession→新セッションで "go" を変換。
        let s1 = svc.startSession()
        for ch in "nihongo" { _ = svc.insert(session: s1, text: String(ch)) }
        let c1 = svc.convert(session: s1) ?? []
        _ = svc.commit(session: s1, index: c1.firstIndex(of: "日本語")!) // 全消費
        svc.endSession(session: s1)
        let s2 = svc.startSession()
        for ch in "go" { _ = svc.insert(session: s2, text: String(ch)) }
        let after = svc.convert(session: s2) ?? []

        XCTAssertEqual(after, baseline, "前セッションの確定文脈が次セッションの変換へ漏れている")
    }

    /// セッションA を開いたまま（endSession せず）commit し、別セッションB で変換しても、
    /// B が A の完了文脈に汚染されない（P2-1: 共有 converter のセッション間漏れ防止）。
    func testOverlappingSessionsDoNotShareConverterContext() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let cold = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let cs = cold.startSession()
        for ch in "go" { _ = cold.insert(session: cs, text: String(ch)) }
        let baseline = cold.convert(session: cs) ?? []

        let a = svc.startSession()
        for ch in "nihongo" { _ = svc.insert(session: a, text: String(ch)) }
        let ca = svc.convert(session: a) ?? []
        _ = svc.commit(session: a, index: ca.firstIndex(of: "日本語")!) // A の completedData を残す
        // A は endSession しない（開いたまま）。別セッション B で変換する。
        let b = svc.startSession()
        for ch in "go" { _ = svc.insert(session: b, text: String(ch)) }
        let after = svc.convert(session: b) ?? []

        XCTAssertEqual(after, baseline, "別セッションBの変換がセッションAの確定文脈に汚染されている")
    }

    /// アクティブ converter セッションA を endSession しても（別セッションB が残存）、B の変換が
    /// A の確定文脈に汚染されない（P2-1 follow-up: endSession で activeConverterSession を早まって
    /// nil にすると bindConverter がリセットを取りこぼす、その回帰ガード）。
    func testEndingActiveSessionDoesNotLeakIntoRemainingSession() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let cold = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let cs = cold.startSession()
        for ch in "go" { _ = cold.insert(session: cs, text: String(ch)) }
        let baseline = cold.convert(session: cs) ?? []

        let b = svc.startSession()  // 先に開くが未使用
        let a = svc.startSession()
        for ch in "nihongo" { _ = svc.insert(session: a, text: String(ch)) }
        let ca = svc.convert(session: a) ?? []                     // activeConverterSession = a
        _ = svc.commit(session: a, index: ca.firstIndex(of: "日本語")!) // A の completedData を残す
        svc.endSession(session: a)                                 // A 終了（B は残存）
        for ch in "go" { _ = svc.insert(session: b, text: String(ch)) }
        let after = svc.convert(session: b) ?? []                  // B: A の文脈に汚染されてはいけない

        XCTAssertEqual(after, baseline, "終了したアクティブセッションの文脈が残存セッションへ漏れている")
    }

    func testCommitUnknownSessionReturnsNil() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        XCTAssertNil(svc.commit(session: 99999, index: 0))
    }

    /// convert していない（候補キャッシュ無し）セッションの commit は nil。
    func testCommitWithoutConvertReturnsNil() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let sid = svc.startSession()
        for ch in "nihongo" { _ = svc.insert(session: sid, text: String(ch)) }
        XCTAssertNil(svc.commit(session: sid, index: 0), "convert前のキャッシュ無し commit は nil")
    }

    func testCommitIndexOutOfRangeReturnsNil() {
        let (svc, sid, _) = classicNihongoSession()
        XCTAssertNil(svc.commit(session: sid, index: 99999))
    }

    /// convert 後に insert で読みが変わったら、古い index の commit は拒否（stale ガード）。
    func testCommitStaleAfterInsertReturnsNil() {
        let (svc, sid, cands) = classicNihongoSession()
        let idx = cands.firstIndex(of: "日本")!
        _ = svc.insert(session: sid, text: "u")  // 読みが変化→キャッシュ無効化
        XCTAssertNil(svc.commit(session: sid, index: idx), "insert 後の stale commit は nil")
    }

    func testZenzaiDisabledWithExplicitClassicConfig() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        XCTAssertFalse(svc.zenzaiEnabled)
    }

    func testClassicConversionWorksWithExplicitClassicConfig() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let sid = svc.startSession()
        for ch in "nihongo" { _ = svc.insert(session: sid, text: String(ch)) }
        XCTAssertTrue(svc.convert(session: sid)?.contains("日本語") ?? false)
    }

    // --- Bug 2: 切断時のセッション掃除（cleanupConnection）。TIP が EndSession を送らずパイプを
    // 落とす経路（タイムアウト劣化・アプリ強制終了）で孤児セッションが残らないことを検証する。 ---

    /// (a) cleanupConnection はその接続で作られたセッションと候補キャッシュを掃除する。
    func testCleanupConnectionRemovesSessionsAndCache() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let sid = svc.startSession(connection: 1)
        for ch in "nihongo" { _ = svc.insert(session: sid, text: String(ch)) }
        _ = svc.convert(session: sid)   // 候補キャッシュを作る
        svc.cleanupConnection(1)
        // セッションは消えている（未知セッション扱いで nil）。
        XCTAssertNil(svc.insert(session: sid, text: "a"), "cleanup 後もセッションが残っている")
        XCTAssertNil(svc.convert(session: sid), "cleanup 後もセッションが残っている")
        // 候補キャッシュも消えている（未知セッションなので commit も nil）。
        XCTAssertNil(svc.commit(session: sid, index: 0), "cleanup 後も候補キャッシュが残っている")
    }

    /// (b) 最後のセッションを cleanupConnection で消すと endSession 同様 stopComposition が走り、
    /// 放棄された合成の確定文脈が次の独立セッションへ漏れない（EndSession 未送の切断でも文脈リセット）。
    func testCleanupConnectionResetsConverterContextWhenLastSession() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        // 基準: 冷えた converter で "go"(ご) を変換。
        let cold = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let cs = cold.startSession()
        for ch in "go" { _ = cold.insert(session: cs, text: String(ch)) }
        let baseline = cold.convert(session: cs) ?? []

        // 同一 converter で nihongo を全確定（completedData を残す）→ EndSession を送らず切断掃除。
        let s1 = svc.startSession(connection: 1)
        for ch in "nihongo" { _ = svc.insert(session: s1, text: String(ch)) }
        let c1 = svc.convert(session: s1) ?? []
        _ = svc.commit(session: s1, index: c1.firstIndex(of: "日本語")!)  // 全消費
        svc.cleanupConnection(1)                                          // 切断掃除（endSession 相当）
        let s2 = svc.startSession(connection: 2)
        for ch in "go" { _ = svc.insert(session: s2, text: String(ch)) }
        let after = svc.convert(session: s2) ?? []

        XCTAssertEqual(after, baseline, "切断掃除で stopComposition が走らず前セッションの確定文脈が漏れている")
    }

    /// (c) cleanupConnection は当該接続のセッションだけを消し、他接続のセッションは生存する。
    /// また掃除後（再接続相当）の新接続で作ったセッションは正常に動く。
    func testCleanupConnectionLeavesOtherConnectionsAndReconnectWorks() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let a = svc.startSession(connection: 1)
        let b = svc.startSession(connection: 2)
        for ch in "nihongo" { _ = svc.insert(session: b, text: String(ch)) }
        svc.cleanupConnection(1)   // 接続1のみ切断
        // 接続1の a は消えた。
        XCTAssertNil(svc.insert(session: a, text: "x"), "接続1のセッションが残っている")
        // 無関係の接続2の b は生存し、変換できる。
        XCTAssertTrue(svc.convert(session: b)?.contains("日本語") ?? false, "無関係の接続2のセッションまで消えた")
        // 再接続相当: 新接続で作ったセッションは正常に動く。
        let c = svc.startSession(connection: 3)
        for ch in "nihongo" { _ = svc.insert(session: c, text: String(ch)) }
        XCTAssertTrue(svc.convert(session: c)?.contains("日本語") ?? false, "再接続後の新セッションが動かない")
    }

    func testConnectionOwnsTracksCreatorAndEndSession() {
        let service = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        let sid = service.startSession(connection: 7)
        // 作成元接続だけが所有者。
        XCTAssertTrue(service.connectionOwns(session: sid, connection: 7))
        XCTAssertFalse(service.connectionOwns(session: sid, connection: 8))
        // 未知 session は誰の所有でもない。
        XCTAssertFalse(service.connectionOwns(session: sid + 999, connection: 7))
        // 終了後は所有も消える。
        service.endSession(session: sid)
        XCTAssertFalse(service.connectionOwns(session: sid, connection: 7))
    }

    /// Plan4: ユーザ辞書 JSON の語が変換候補に出る（起動時ロードの統合）。
    /// 本番は convenience init が resolve() の URL で loadUserDictionary を呼ぶ。テストは
    /// 実ユーザーの %LOCALAPPDATA% に依存しないよう init(config:)+明示 URL で同じ経路を通す。
    func testUserDictionaryEntriesAppearInCandidates() throws {
        let json = #"[{"ruby":"やちだ","word":"谷内田","pos":"人名(姓)"}]"#
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("ud-svc-\(UUID().uuidString).json")
        try Data(json.utf8).write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        svc.loadUserDictionary(from: url)
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "yachida")
        let candidates = svc.convert(session: sid)
        XCTAssertTrue(candidates?.contains("谷内田") ?? false,
                      "expected 谷内田 in \(String(describing: candidates))")
    }

    /// Plan4: 「きょう」で日付テンプレートが**実日付へ展開済み**で候補に出る
    /// （word が "<date ...>" のまま候補に漏れない — Candidate.parseTemplate() の統合確認）。
    func testDateTemplateExpandsToTodayInCandidates() {
        let svc = ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1))
        svc.loadUserDictionary(from: nil)   // 組み込みテンプレートのみ
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "kyou")
        let formatter = DateFormatter()
        formatter.dateFormat = "yyyy年MM月dd日"
        formatter.locale = Locale(identifier: "ja_JP")
        formatter.calendar = Calendar(identifier: .gregorian)
        // 23:59:59 跨ぎ防御: テンプレート展開は convert 実行時刻の日付になるため、変換の
        // 前後両方で日付を取り「どちらかに一致」で判定する（跨がなければ両者は同一）。
        let todayBefore = formatter.string(from: Date())
        let candidates = svc.convert(session: sid) ?? []
        let todayAfter = formatter.string(from: Date())
        XCTAssertTrue(candidates.contains(todayBefore) || candidates.contains(todayAfter),
                      "expected \(todayBefore)/\(todayAfter) in \(candidates)（出ない場合は value=-18 の調整余地 — plan Open Risk）")
        XCTAssertFalse(candidates.contains { $0.hasPrefix("<date") },
                       "テンプレートリテラルが未展開のまま漏れた: \(candidates)")
    }
}
