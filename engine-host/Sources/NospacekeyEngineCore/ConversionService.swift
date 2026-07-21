import Foundation
import KanaKanjiConverterModuleWithDefaultDictionary

/// KanaKanjiConverter をラップし、セッションごとに ComposingText を保持する変換サービス。
/// COM/パイプ非依存（ユニットテスト対象）。Zenzai は config で切替える。
///
/// `@unchecked Sendable`: 背景 warm-up スレッド（`startWarmUp`）と EngineHost の @Sendable
/// リクエストハンドラ・切断処理（onDisconnect → cleanupConnection。パイプ接続スレッド上）が
/// `self` を捕捉するため必要。
/// 安全性の根拠 — warm-up スレッドが触るのは `converter`（`converterLock` で直列化）と
/// ローカルの dummy、`zenzaiReady`（専用ロック）のみ。`sessions`（SessionRecord）/`nextId`/
/// `connectionSessions` はリクエストハンドラ（および切断時の cleanupConnection）
/// からのみ触るが、常駐モードでは複数クライアントからのリクエストが並行するため、
/// これらへのアクセスは呼び出し元 EngineHost.serviceLock で直列化される。
/// `workDir` は immutable。UU-5 で可変化した `config`/`llmClient` は次のように保護される:
/// `config` は読み(makeOptions)/書き(reload) とも `converterLock` 下（warm-up スレッドとも直列化）。
/// `learning`/`autoCommit` も `config` と同じ規律（読み書きとも converterLock 下）。
/// `llmClient` は読み(llmConvert/isEcho)/書き(reload) とも handler の serviceLock 下。reload は
/// serviceLock を握る handler から呼ばれ converterLock を **非ブロックで試す**ので、ロック反転は無い。
/// （`zenzaiEnabled` は起動時/テストのみ config を無ロックで読むが、その時点で並行 reload は無い。）
/// `zenzaiReady`（cold start ③）は専用 `zenzaiReadyLock` で保護。makeOptions（converterLock 下）→
/// getter の一方向の入れ子しか無く、zenzaiReadyLock 保持中に他のロックは取らない＝反転しない。
/// `activeConverterSession`（bindConverter/endSession）と `firstConvertLogged`
/// （logFirstConvertOnceLocked）は読み書きとも converterLock 下（各メソッドの呼出契約）。
public final class ConversionService: @unchecked Sendable {
    private let converter = KanaKanjiConverter.withDefaultDictionary()

    /// 1セッションの全状態（合成テキスト・候補キャッシュ・ライブ変換履歴・所有接続）。
    /// 並列 Dictionary 6本（sessions/cachedCandidates/cachedTarget/typoRepairedIndices/
    /// liveState/sessionConnection）に分けない理由: その構造では各メソッドが必要な部分集合を
    /// 手で同期することになり、更新漏れがそのまま stale バグになる（実例: typoRepairedIndices の
    /// 手動 nil 忘れ — 旧レビューCritical）。アクセスは従来どおり呼び出し元 serviceLock で直列化。
    struct SessionRecord {
        var composing: ComposingText
        /// 直近の convert 系が返した候補の [Candidate]（commit が index で引く）。
        /// 候補ごとの composingCount（消費読み）を保持するため text だけでなく Candidate を丸ごと持つ。
        var cachedCandidates: [Candidate]? = nil
        /// キャッシュ時点の convertTarget（読みが変わったら stale としてキャッシュを使わない）。
        var cachedTarget: String? = nil
        /// typoConvert が cachedCandidates に積んだ「修復候補ブロック」由来の index 集合。
        /// commit がこの集合に含まれる index を確定するときだけ、全消費＋誤読み合成ペア学習の
        /// 特別経路へ分岐する。
        /// 不変条件: 非nil なのは、直近の typoConvert が積んだ修復ブロックが cachedCandidates に
        /// 載っている間だけ（cacheCandidates が候補と同時に設定/クリアするため、書き込み箇所ごとの
        /// 手動 nil は不要になった）。
        var typoRepairedIndices: Set<Int>? = nil
        /// ライブ変換履歴（自動確定用 — iOS LiveConversionManager の移植）。
        var liveState: LiveConversionState? = nil
        /// 作成元の接続 id。所有チェック（UU-2 connectionOwns）と endSession の所有集合除去が使う。
        let connection: Int

        /// 候補キャッシュを破棄する。読みが変わる/確定する全経路で呼ぶ。
        mutating func invalidateCandidateCache() {
            cachedCandidates = nil
            cachedTarget = nil
            typoRepairedIndices = nil
        }

        /// 変換結果を候補キャッシュへ載せる（invalidateCandidateCache と対）。repairedIndices は
        /// 修復ブロックを積む typoConvert だけが渡す — 省略時 nil が、convert/liveConvert に古い
        /// 修復 index が残る余地（旧レビューCritical）を構造的に塞ぐ。
        mutating func cacheCandidates(_ candidates: [Candidate], target: String, repairedIndices: Set<Int>? = nil) {
            cachedCandidates = candidates
            cachedTarget = target
            typoRepairedIndices = repairedIndices
        }
    }

    private var sessions: [Int: SessionRecord] = [:]

    /// テスト専用の観測窓（読み取りのみ）。実体は SessionRecord.typoRepairedIndices。
    /// private でなく internal にしているのはテスト専用（不変条件を「stale index が commit を
    /// 誤分類する」という間接観測に頼ると、部分被覆候補が低 index に来ない実辞書データに
    /// 依存し再現性が無い＝直接検査する）。
    var typoRepairedIndices: [Int: Set<Int>] {
        sessions.compactMapValues { $0.typoRepairedIndices }
    }
    /// 共有 converter を現在「合成中」として使っているセッション。別セッションが converter を
    /// 使う直前にリセットし、completedData/previousInputData 等の文脈が別セッションへ漏れるのを防ぐ
    /// （同一セッション継続ならリセットしない＝部分確定の左文脈を保つ）。
    private var activeConverterSession: Int?
    /// 接続 id → その接続で作られたセッション id の集合。常駐サーバは複数 TIP クライアントが
    /// それぞれ別接続で同時接続しうる（NamedPipeServer は nMaxInstances=255）ため、切断時に掃除すべき
    /// セッションを接続単位で特定する。TIP が EndSession を送らずパイプを落とす経路（EndSession
    /// タイムアウト劣化・アプリ強制終了。Rust 側 drop_engine は何も送らない）で、孤児セッションが
    /// `sessions` に永久残留するのを防ぐ（cleanupConnection）。session→接続の逆方向は
    /// SessionRecord.connection が持つ（endSession はそれで所有集合から O(1) 除去する）。
    private var connectionSessions: [Int: Set<Int>] = [:]
    private var nextId = 1
    private let workDir = FileManager.default.temporaryDirectory
    /// UU-5: 常駐エンジンは起動後も `reload` で設定を差し替えられる（設定アプリの変更を反映）。
    /// `makeOptions` が convert ごとに読むため、`converterLock` 下で差し替えれば次回変換から効く
    /// （converter オブジェクト自体の再構築は不要＝Zenzai は options の weightURL で切替わる）。
    private var config: ZenzaiConfig
    /// Spec2: 学習設定。読み(makeOptions/commit)/書き(reload) とも `converterLock` 下（config と同じ規律）。
    private var learning: LearningSettings
    /// 修正変換(TypoConvert)の誤読み学習トグル。ADR-0002: 誤読み(実在しない読み)を学習辞書へ
    /// 恒久追加する副作用があるため、学習本体(learning.enabled)と独立に切れる必要がある。
    /// `LearningSettings.swift` は変更しない方針のため、ここで env から直接解決する（読み書きとも
    /// `converterLock` 下＝learning と同じ規律）。
    private var typoLearn: Bool
    /// 自動確定の速さ（iOS の「自動確定の速さ」設定の移植）。読み(liveConvert)/書き(reload) とも
    /// `converterLock` 下（config と同じ規律）。
    private var autoCommit: AutoCommitStrength
    /// 読み長バックストップ（死のループ対策）: 読みがこの長さを超えたら文節安定を待たず
    /// 先頭文節を強制確定する。0 以下で無効。読み(liveConvert)/書き(reload) とも `converterLock` 下。
    private var autoCommitMaxReading: Int
    /// converter（およびモデル）への全アクセスを直列化する。背景 warm-up（別スレッド）と
    /// convert（リクエストループ）の競合を防ぎ、warm-up がロック保持中に届いた変換はロード完了を
    /// 自然に待つ（ロック取得**前**の要求だけが zenzaiReady ゲート閉で古典に落ちて即応する —
    /// startWarmUp の限界注記参照）。
    /// insert/backspace は ComposingText のみ操作し converter を触らないのでロック不要・即応。
    private let converterLock = NSLock()
    /// cold start ③: Zenzai を options に載せてよいか。false の間 makeOptions が ZenzaiMode を
    /// .off に落とし、変換は古典（辞書）で即応する。warmUp（モデル先読み）完了後に true
    /// （Zenzai 無効設定なら startWarmUp が同期で即 true — weightURL が無ければ makeZenzaiMode 側で
    /// .off に落ちるため無害）。
    /// 専用 NSLock ゲート — makeOptions（converterLock 下の読み）と warmUp スレッドの書きを
    /// 直列化する。zenzaiReadyLock 保持中に他のロックは取らない（クラスコメントのロック順参照）。
    private let zenzaiReadyLock = NSLock()
    private var _zenzaiReady = false
    public private(set) var zenzaiReady: Bool {
        get { zenzaiReadyLock.lock(); defer { zenzaiReadyLock.unlock() }; return _zenzaiReady }
        set { zenzaiReadyLock.lock(); defer { zenzaiReadyLock.unlock() }; _zenzaiReady = newValue }
    }
    /// cold start ①: プロセス起動後の「初回変換」(convert/liveConvert の先勝ち) を一度だけ計測する
    /// ワンショット。読み書きとも converterLock 下（両呼び出し元が t0〜ms 計測を lock 内で行う）。
    private var firstConvertLogged = false
    /// 外部LLM変換クライアント。echo 判定（`isEcho`）も含めここに一本化する。
    /// UU-5: `reload` で差し替え可能（LLMClient は config を保持するだけなのでモデル再ロード等は不要）。
    private var llmClient: LLMClient

    /// 本番用: env と exe 隣の既定パスから Zenzai 設定を解決する。
    /// テストからは呼ばないこと（exe 隣のモデル有無で挙動が環境依存になる）。テストは `init(config:)` を使う。
    public convenience init() {
        let exeDir = (Bundle.main.executableURL ?? URL(fileURLWithPath: CommandLine.arguments[0]))
            .deletingLastPathComponent()
        let env = ProcessInfo.processInfo.environment
        let cfg = ZenzaiConfig.resolve(exeDir: exeDir, environment: env)
        let learning = LearningSettings.resolve(environment: env)
        if env["NOSPACEKEY_LEARNING"] == "1" && !learning.enabled {
            engineLog("ev=learning_degraded reason=dir_unavailable\n")  // 黙って壊れない（spec §1）
        }
        self.init(config: cfg, learning: learning,
                  llmClient: LLMClient(config: LLMConfig.resolve(environment: env)),
                  autoCommit: AutoCommitStrength.resolve(environment: env),
                  autoCommitMaxReading: AutoCommitLengthBackstop.resolve(environment: env),
                  typoLearn: env["NOSPACEKEY_TYPO_LEARN"] != "0")
        // Plan4: ユーザ辞書(ワンショット移行 JSON)+組み込み日付テンプレートの起動時ロード。
        // ここは runEngineHost の service.startWarmUp() より前(EngineHost.swift:128→136)＝
        // warm-up スレッド起動前の初期化時点なので競合しない(メソッド側でも lock は取る)。
        loadUserDictionary(from: UserDictionary.resolve(environment: env))
    }

    /// テスト用: 設定を明示注入する。llmClient は任意注入（既定は未設定＝disabled）。
    /// autoCommit の既定 `.weak` は本番既定（AutoCommitStrength.resolve の未設定時）と同値。
    /// autoCommitMaxReading の既定 25 は本番既定（AutoCommitLengthBackstop.resolve の未設定時）と同値。
    public init(config: ZenzaiConfig,
                learning: LearningSettings = .disabled,
                llmClient: LLMClient = LLMClient(config: LLMConfig.resolve(environment: [:])),
                autoCommit: AutoCommitStrength = .weak,
                autoCommitMaxReading: Int = 25,
                typoLearn: Bool = true) {
        self.config = config
        self.learning = learning
        self.llmClient = llmClient
        self.autoCommit = autoCommit
        self.autoCommitMaxReading = autoCommitMaxReading
        self.typoLearn = typoLearn
    }

    /// Zenzai が有効か（重みが解決できたか）。
    public var zenzaiEnabled: Bool { config.weightURL != nil }

    /// Plan4: ユーザ辞書(ワンショット移行 JSON)+組み込み日付テンプレートを converter へ載せる。
    /// `importDynamicUserDictionary` は**丸ごと置換**（DicdataStoreState が配列を代入するだけ）
    /// なので、テンプレートとインポート辞書を必ず1配列に結合して**1回だけ**呼ぶ。
    /// 本番は convenience init から呼ばれる（startWarmUp 前＝warm-up スレッド起動前）。
    /// テストは `init(config:)` の後に任意の URL（nil=テンプレートのみ）で呼ぶ。
    /// converter を触るので converterLock 下で行う（init 時点では無競合だが、後から呼ばれても
    /// warm-up/変換と直列化される規律を守る）。辞書更新の反映は engine 再起動（ReloadConfig
    /// 経路には載せない — plan の設計ロック）。
    func loadUserDictionary(from url: URL?) {
        let templates = UserDictionary.builtinDateTemplates()
        var dicdata = templates
        if let url {
            let imported = UserDictionary.load(url: url)
            dicdata.append(contentsOf: imported)
            engineLog("ev=user_dict loaded=\(imported.count) templates=\(templates.count)\n")
        }
        converterLock.lock()
        defer { converterLock.unlock() }
        converter.importDynamicUserDictionary(dicdata)
    }

    /// UU-5: 常駐エンジンの LLM/Zenzai 設定を差し替える（設定アプリの変更を接続中に反映）。
    /// `overrides` は TIP が push した設定値（LLMConfig.resolve / ZenzaiConfig.resolve が読む env キー）。
    ///
    /// #2: `overrides` は丸ごと置換ではなく **実プロセス env に重ねる**。丸ごと置換すると spawn 時のみ
    /// 効く env が消える — `NOSPACEKEY_LLM_ECHO`（テスト/診断の echo）、`NOSPACEKEY_ZENZAI_INFERENCE_LIMIT`、
    /// および resolve_env_map が注入を控えて尊重している D6 の env override（push しないキーは env 側が勝つ）。
    ///
    /// #1b: `converterLock` は warm-up がモデルロード中ずっと保持する（cold start ③でもこの構造は維持 —
    /// ロック外ロードは lib の可視性/共有状態の制約で不可。startWarmUp の注記参照）。ここでブロック
    /// 待ちすると handler の serviceLock を握ったまま数秒固まり、全クライアントの全要求を warm-up
    /// 終了まで凍らせ ReloadConfig 自体もタイムアウトする。そこで **非ブロックで試し、取れなければ
    /// skip** する。安全な理由: converterLock が埋まっているのは spawn 直後の warm-up（or 変換中）で、
    /// その間 config は spawn 時 env（=当時の最新 settings）のまま＝まだ変わっていない。設定変更は
    /// 次回接続で反映される。
    /// LLMClient は config を保持するだけ（モデル再ロード不要）。呼び出しは handler の serviceLock 下で
    /// 直列化されるため llmConvert とは競合しない。
    /// 注: Zenzai を新たに有効化した直後の初回変換はモデルをその場ロードするため一度だけ遅い（warm-up はしない）。
    public func reload(overrides: [String: String]) {
        var env = ProcessInfo.processInfo.environment
        for (k, v) in overrides { env[k] = v }
        let exeDir = (Bundle.main.executableURL ?? URL(fileURLWithPath: CommandLine.arguments[0]))
            .deletingLastPathComponent()
        let newZenzai = ZenzaiConfig.resolve(exeDir: exeDir, environment: env)
        let newLLM = LLMConfig.resolve(environment: env)
        let newLearning = LearningSettings.resolve(environment: env)
        // 非ブロック取得（NSLock.lock(before: 現在時刻) は空いていれば true / 埋まっていれば即 false）。
        if converterLock.lock(before: Date()) {
            defer { converterLock.unlock() }
            // Spec2: OFF へ切り替わる前に保留分を保存（.nothing では新規更新が止まり save も skip される
            // ＝保留分が「凍結」され、後で ON に戻すと古い保留分が書かれうる。先に保存して空にしておく。
            // 注: ライブラリの updateConfig(.nothing) は一時トライをクリアしない — LearningMemory.swift:645-650）。
            if self.learning.enabled && !newLearning.enabled { flushLearningLocked() }
            self.learning = newLearning
            self.config = newZenzai
            self.llmClient = LLMClient(config: newLLM)
            self.autoCommit = AutoCommitStrength.resolve(environment: env)
            self.autoCommitMaxReading = AutoCommitLengthBackstop.resolve(environment: env)
            self.typoLearn = env["NOSPACEKEY_TYPO_LEARN"] != "0"
            engineLog("ev=reload_config zenzai=\(newZenzai.weightURL != nil) llm=\(newLLM.enabled) learning=\(newLearning.enabled) auto_commit=\(self.autoCommit.rawValue) auto_commit_max_reading=\(self.autoCommitMaxReading) typo_learn=\(self.typoLearn)\n")
        } else {
            // warm-up/変換中。config は最新のまま（skip 安全）。次回接続で反映。
            engineLog("ev=reload_config skipped=busy\n")
        }
    }

    /// 新規セッションを確保し、空の ComposingText を登録して id を返す。
    /// `connection` は作成元の接続 id。切断時に cleanupConnection がこの接続のセッションを掃除する。
    /// 既定 0 は接続の概念を持たない呼び出し（テスト / oneShot）向け。
    public func startSession(connection: Int = 0) -> Int {
        let id = nextId
        nextId += 1
        sessions[id] = SessionRecord(composing: ComposingText(), connection: connection)
        connectionSessions[connection, default: []].insert(id)
        return id
    }

    /// `session` が `connection` の作成物かどうか（所有権チェック — UU-2）。
    /// 未知セッションは false。呼び出し側（EngineHost のハンドラ）は非所有を未知セッションと
    /// 同じ "no session" へ正規化する（応答形で所有情報を漏らさない）。
    public func connectionOwns(session: Int, connection: Int) -> Bool {
        sessions[session]?.connection == connection
    }

    /// カーソル位置に text を挿入し、現在の読み（convertTarget）を返す。
    /// 戻り値が nil なのは **未知セッションのときだけ**（既知セッションは空読み "" でも非nil）。
    /// style: "direct"=リテラル挿入（Shift英語モード）。enum にしないのは wire の文字列を
    /// ここ1箇所で解釈し、未知値を roman2kana へ安全に劣化させるため。
    public func insert(session: Int, text: String, style: String? = nil) -> String? {
        guard var rec = sessions[session] else { return nil }
        rec.composing.insertAtCursorPosition(text, inputStyle: style == "direct" ? .direct : .roman2kana)
        rec.invalidateCandidateCache()   // 読みが変わったので古い候補 index は無効
        sessions[session] = rec
        return rec.composing.convertTarget
    }

    /// カーソル位置から1文字削除し、現在の読みを返す。
    /// 戻り値が nil なのは **未知セッションのときだけ**（既知セッションは空読み "" でも非nil）。
    public func backspace(session: Int) -> String? {
        guard var rec = sessions[session] else { return nil }
        rec.composing.deleteBackwardFromCursorPosition(count: 1)
        rec.invalidateCandidateCache()   // 読みが変わったので古い候補 index は無効
        sessions[session] = rec
        return rec.composing.convertTarget
    }

    /// 共有 converter を `session` 用に束ねる。直前に別セッションが使っていたら、その完了文脈
    /// （completedData/previousInputData/lattice）をリセットしてからにする（セッション間の漏れ防止）。
    /// **converterLock 保持中に呼ぶこと**（stopComposition が converter を触るため）。
    private func bindConverter(to session: Int) {
        if let active = activeConverterSession, active != session {
            // バグ#3 実測用: セッション切替の stopComposition は zenz.endSession()→reset_context()
            // （llama_free＋llama_init_from_model）を誘発する。ここでは計数のみ — 修正は別トラック。
            // M-2: Zenzai 無効（weightURL 無し＝zenz 不在で reset は no-op）では出さない（過大計上防止）。
            // config 読みは converterLock 下（本メソッドの呼出契約）＝規律どおり。
            if config.weightURL != nil { engineLog("ev=llama_reset reason=session_switch\n") }
            converter.stopComposition()
        }
        activeConverterSession = session
    }

    /// 現在の読みを変換し、変換候補のテキスト配列を返す。
    /// converterLock で warm-up と直列化（初回はモデルロード完了まで待つ）。
    /// 戻り値が nil なのは **未知セッションのときだけ**（既知セッションは空配列でも非nil）。
    /// `leftContext`: U9 — Zenzai の左文脈（ドキュメント本文＝機微データ。ログには文字数のみ出す）。
    public func convert(session: Int, leftContext: String? = nil) -> [String]? {
        guard var rec = sessions[session] else { return nil }
        converterLock.lock()
        defer { converterLock.unlock() }
        bindConverter(to: session)
        let t0 = DispatchTime.now()
        let mainResults = converter.requestCandidates(rec.composing, options: makeOptions(leftSideContext: leftContext)).mainResults
        // commit(session:index:) が同じ並びの Candidate を index で引けるようキャッシュする。
        // 返す text 配列は mainResults と 1:1（同順）なので TIP 側 index がそのまま使える。
        // レビューCritical だった「typoConvert 後に読みが変わらないまま convert()/liveConvert() が
        // 呼ばれる経路で前回の修復 index が stale 残留」は、cacheCandidates が repairedIndices を
        // 候補と同時に置き換える（省略時 nil）ことで消えている。
        rec.cacheCandidates(mainResults, target: rec.composing.convertTarget)
        sessions[session] = rec
        let results = mainResults.map { $0.text }
        let ms = Double(DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds) / 1_000_000
        logFirstConvertOnceLocked(ms: ms)
        engineLog("ev=infer kind=convert ms=\(String(format: "%.1f", ms)) n=\(results.count) target=\(rec.composing.convertTarget) ctx=\(leftContext?.count ?? 0)\n")
        return results
    }

    /// 修正変換(TypoConvert): ローマ字入力の「同一英字ちょうど2連打」を1文字へ縮約した仮説を
    /// 列挙し、各仮説の古典変換候補を先頭に、通常(literal)変換候補を後続に連結した候補リストを返す。
    /// 修復パターンが無ければ convert(session:leftContext:) と同じ（上位互換・キャッシュ意味論ごと委譲）。
    /// 戻り値が nil なのは **未知セッションのときだけ**（既知セッションは空でも非nil）。
    public func typoConvert(session: Int, leftContext: String? = nil) -> [String]? {
        guard var rec = sessions[session] else { return nil }
        // ローマ字列は input の .character piece を連結して得る。.character 以外の piece
        // （direct 入力/reconvert 由来等）が混ざっていたら仮説なし扱い（roman2kana 前提が崩れるため）。
        var roman = ""
        var hasNonCharacterPiece = false
        for element in rec.composing.input {
            if case .character(let ch) = element.piece {
                roman.append(ch)
            } else {
                hasNonCharacterPiece = true
            }
        }
        let hyps = hasNonCharacterPiece ? [] : TypoRepair.hypotheses(roman: roman)
        guard !hyps.isEmpty else {
            // 前回 typoConvert の修復 index が残っていてもここで手動 nil はしない:
            // 委譲先 convert の cacheCandidates（repairedIndices 省略=nil）が候補ごと必ず上書きする。
            return convert(session: session, leftContext: leftContext)
        }

        converterLock.lock()
        defer { converterLock.unlock() }
        bindConverter(to: session)
        let t0 = DispatchTime.now()

        // literal（そのまま）変換を convert() と同じ options で先に実行する。仮説変換（使い捨て
        // ComposingText）は converter の増分キャッシュを汚す（reconvert と同じ許容済みパターン）ため、
        // 汚染の影響を literal 側に及ぼさないよう順序を固定する。
        let literalResults = converter.requestCandidates(rec.composing, options: makeOptions(leftSideContext: leftContext)).mainResults

        var repaired: [Candidate] = []
        for hyp in hyps {
            var hypComposing = ComposingText()
            hypComposing.insertAtCursorPosition(hyp, inputStyle: .roman2kana)
            let hypResults = converter.requestCandidates(hypComposing, options: makeOptions(nBest: 3, forceClassic: true)).mainResults
            let covering = hypResults.filter { cand in
                cand.data.reduce(0) { $0 + $1.ruby.count } == hypComposing.convertTarget.count
            }
            repaired.append(contentsOf: covering.prefix(3))
        }
        if repaired.count > 9 { repaired = Array(repaired.prefix(9)) }

        // マージ: 修復ブロック→literal の順で連結し、text で重複除去(先勝ち)。
        var seen = Set<String>()
        var merged: [Candidate] = []
        var repairedIndices = Set<Int>()
        for cand in repaired {
            guard seen.insert(cand.text).inserted else { continue }
            repairedIndices.insert(merged.count)
            merged.append(cand)
        }
        for cand in literalResults {
            guard seen.insert(cand.text).inserted else { continue }
            merged.append(cand)
        }

        rec.cacheCandidates(merged, target: rec.composing.convertTarget, repairedIndices: repairedIndices)
        sessions[session] = rec
        let results = merged.map { $0.text }
        let ms = Double(DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds) / 1_000_000
        engineLog("ev=infer kind=typo_convert ms=\(String(format: "%.1f", ms)) n=\(results.count) hyps=\(hyps.count) target=\(rec.composing.convertTarget)\n")
        return results
    }

    /// 選択かな表層を「読み」として与え変換候補を返す（SP5 step-6）。
    /// surface は .direct で挿入する（roman2kana は使わない）。カタカナはひらがな読みへ正規化する。
    /// 戻り値が nil なのは **未知セッションのときだけ**（空候補でも非nil）。
    public func reconvert(session: Int, surface: String, leftContext: String? = nil) -> [String]? {
        guard var rec = sessions[session] else { return nil }
        var c = ComposingText()
        c.insertAtCursorPosition(Self.normalizeKana(surface), inputStyle: .direct)
        rec.composing = c
        rec.liveState = nil   // 合成内容を丸ごと差し替えたので自動確定履歴は無効
        // cacheCandidates は意図的に呼ばない: 再変換の確定は TIP 側が `reconverting` ガードで
        // Commit IPC を迂回し resolved_text を直接挿入する契約（key_event_sink.rs）ため、
        // ここで積んだキャッシュは誰も引かない。差し替え前の旧キャッシュが残っていても、
        // commit の stale ガード（cachedTarget != convertTarget）が拒否する。
        sessions[session] = rec
        converterLock.lock()
        defer { converterLock.unlock() }
        let t0 = DispatchTime.now()
        let results = converter.requestCandidates(c, options: makeOptions(leftSideContext: leftContext)).mainResults.map { $0.text }
        let ms = Double(DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds) / 1_000_000
        engineLog("ev=infer kind=reconvert ms=\(String(format: "%.1f", ms)) n=\(results.count) target=\(c.convertTarget) ctx=\(leftContext?.count ?? 0)\n")
        return results
    }

    /// カタカナ（U+30A1…U+30F6）をひらがなへ寄せる。長音符 ー(U+30FC)・ひらがなはそのまま。
    /// nospacekey の読み辞書はひらがな ruby で索かれるため、カタカナ選択を読みに正規化する。
    static func normalizeKana(_ s: String) -> String {
        String(String.UnicodeScalarView(s.unicodeScalars.map { sc in
            if (0x30A1...0x30F6).contains(sc.value), let h = Unicode.Scalar(sc.value - 0x60) { return h }
            return sc
        }))
    }

    /// ライブ変換用: N_best=1 で「先頭1候補(text)」と「現在の読み(reading)」を返す。
    /// converterLock で warm-up と直列化（Zenzai は inferenceLimit が小）。
    /// 戻り値が nil なのは **未知セッションのときだけ**（既知セッションは空でも非nil）。
    ///
    /// `allowAutoCommit`: iOS nospacekey の「自動確定」の移植。true のとき、ライブ変換の更新履歴を
    /// セッションごとに積み（LiveConversionState）、先頭文節の候補が直近 threshold 回
    /// （AutoCommitStrength、既定 weak=16 — iOS 既定と同値）変動していなければ、その文節を
    /// iOS の InputManager.complete(candidate:) と同順で確定する
    /// （setCompletedData → 学習 → ComposingText.prefixComplete → 履歴繰り上げ）。
    /// 確定が起きた場合、戻り値は committed=確定文節、text=残り読みのライブ結果、reading=残り読み。
    /// 呼び出し側（TIP）は committed をアプリへ挿入し、残りで composition を継続する。
    /// false（既定）は従来どおり読みを消費しない（Enter 直前の LiveConvert 等が該当）。
    public func liveConvert(session: Int, leftContext: String? = nil, allowAutoCommit: Bool = false)
        -> (text: String, reading: String, committed: String?)?
    {
        guard var rec = sessions[session] else { return nil }
        converterLock.lock()
        defer { converterLock.unlock() }
        bindConverter(to: session)
        let t0 = DispatchTime.now()
        let conversion = converter.requestCandidates(rec.composing, options: makeOptions(nBest: 1, leftSideContext: leftContext))
        let results = conversion.mainResults
        // Spec2: ライブ確定（TIP の Enter）が Commit{index:0} で学習に乗れるよう、convert() と
        // 同じ規約で候補をキャッシュする（commit は cachedTarget の stale ガード込みでこれを引く）。
        rec.cacheCandidates(results, target: rec.composing.convertTarget)

        // iOS LiveConversionManager.updateWithNewResults と同じ候補選択: 読み全体を被覆する候補を
        // 使い、無ければ読みそのままのダミー候補（ひらがな表示）に落とす。従来の results.first と
        // ほぼ常に一致する（N_best=1 の先頭候補は通常全読みを被覆する）が、被覆しない候補で
        // 誤った prefix を確定しないための iOS 由来のガード。
        let candidate: Candidate
        if let covering = results.first(where: { cand in
            cand.data.reduce(0) { $0 + $1.ruby.count } == rec.composing.convertTarget.count
        }) {
            candidate = covering
        } else {
            candidate = Candidate(
                text: rec.composing.convertTarget,
                value: 0,
                composingCount: .inputCount(rec.composing.input.count),
                lastMid: MIDData.一般.mid,
                data: [DicdataElement(ruby: Self.toKatakana(rec.composing.convertTarget), cid: CIDData.一般名詞.cid, mid: MIDData.一般.mid, value: 0)]
            )
        }

        var committed: String? = nil
        if allowAutoCommit, let threshold = autoCommit.threshold, !rec.composing.convertTarget.isEmpty {
            var state = rec.liveState ?? LiveConversionState()
            state.update(candidate: candidate, firstClauseCandidates: conversion.firstClauseResults)
            var commitCandidate = state.candidateForCompleteFirstClause(threshold: threshold)
            var reason = "stable"
            // 死のループ対策: 先頭文節が安定せず（裸助詞境界の長文等）通常判定が発火しないまま
            // 読みが伸び続ける場合、読み長がしきい値を超えたら文節安定を待たず強制確定する。
            // firstClauseResults は requestCandidates が返した「その回の最良先頭文節候補」で、
            // composingCount が読み全体を超えることは無い（先頭文節は必ず読みの prefix）。
            if commitCandidate == nil,
               autoCommitMaxReading > 0, rec.composing.convertTarget.count > autoCommitMaxReading,
               let forced = conversion.firstClauseResults.first, !forced.text.isEmpty
            {
                commitCandidate = forced
                reason = "length"
            }
            if let firstClause = commitCandidate, !firstClause.text.isEmpty {
                // iOS InputManager.complete(candidate:) の確定順序（先頭文節のみ版）。
                converter.setCompletedData(firstClause)
                if learning.enabled { converter.updateLearningData(firstClause) }
                rec.composing.prefixComplete(composingCount: firstClause.composingCount)
                rec.invalidateCandidateCache()   // 読みが縮んだので古い候補 index は無効
                state.didCompleteFirstClause()
                committed = firstClause.text
                engineLog("ev=live_auto_commit reason=\(reason) committed=\(firstClause.text) remaining=\(rec.composing.convertTarget)\n")
            }
            rec.liveState = state
        }
        sessions[session] = rec

        let ms = Double(DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds) / 1_000_000
        logFirstConvertOnceLocked(ms: ms)
        engineLog("ev=infer kind=live ms=\(String(format: "%.1f", ms)) target=\(rec.composing.convertTarget) ctx=\(leftContext?.count ?? 0)\n")
        if let committedText = committed {
            // 確定文節は candidate.text の prefix（履歴の安定判定により両者の先頭文節テキストは一致）。
            // 残り表示 = 全体のライブ結果から確定分を落としたもの。空なら読みへ劣化（TIP 側でも防御）。
            let remainderText = String(candidate.text.dropFirst(committedText.count))
            return (remainderText, rec.composing.convertTarget, committedText)
        }
        let top = candidate.text.isEmpty ? rec.composing.convertTarget : candidate.text
        return (top, rec.composing.convertTarget, nil)
    }

    /// ひらがな（U+3041…U+3096）をカタカナへ寄せる（normalizeKana の逆方向）。
    /// ダミー候補の ruby 用（iOS の toKatakana 相当）。
    static func toKatakana(_ s: String) -> String {
        String(String.UnicodeScalarView(s.unicodeScalars.map { sc in
            if (0x3041...0x3096).contains(sc.value), let k = Unicode.Scalar(sc.value + 0x60) { return k }
            return sc
        }))
    }

    /// 選択候補(index)をネイティブ部分確定する。戻り: (text=確定候補表層, reading=残り読み)。
    /// reading は prefixComplete 後の convertTarget（消費されなかった読み。全消費なら ""）。
    /// nil は **未知セッション / 候補キャッシュ無し(convert前) / index 範囲外 / stale(読み変化)** のとき
    /// （いずれも TIP 側で従来どおりの全確定へ degrade する）。
    ///
    /// 直近 convert() がキャッシュした [Candidate] から index 番を引き、その `composingCount` だけ
    /// ComposingText を `prefixComplete` で前進させて **書き戻す**。Zenzai は非決定的なので
    /// requestCandidates を再実行せず必ずキャッシュを使う（再実行すると並びが変わり index がずれる）。
    public func commit(session: Int, index: Int) -> (text: String, reading: String)? {
        guard var rec = sessions[session] else { return nil }
        guard let cands = rec.cachedCandidates, index >= 0, index < cands.count else { return nil }
        // convert 後に読みが変わっていたら（insert/backspace）古い index は使わない。
        if let t = rec.cachedTarget, t != rec.composing.convertTarget { return nil }
        let candidate = cands[index]
        let isRepaired = rec.typoRepairedIndices?.contains(index) == true
        converterLock.lock()
        defer { converterLock.unlock() }
        bindConverter(to: session)                                  // 別セッションの文脈をこの確定に混ぜない
        converter.setCompletedData(candidate)                       // nospacekey ネイティブ確定順序（学習は updateLearningData で明示）
        if learning.enabled { converter.updateLearningData(candidate) } // Spec2: RAM 学習（ディスクは endSession で）

        if isRepaired {
            // 修正変換(TypoConvert)の修復候補を確定: 読み全体を消費する（残り読みという概念が無い —
            // 仮説は composingCount が literal の input 列と対応しないため prefixComplete は使えない）。
            //
            // 誤読み学習(ADR-0002): (誤読み全体, 修復表記) の合成ペアを学習器へ渡す。次回、通常の
            // convert() でも誤読みのまま修復候補が浮上するようにするための唯一の経路。
            // 予測変換(requireJapanesePrediction)は OFF 固定が前提: 学習辞書に入るこの「実在しない
            // 読み」が他の入力へ漏れる唯一の経路は前方一致の先読みで、予測 OFF の間だけ閉じている。
            if learning.enabled && typoLearn {
                let synthetic = Candidate(
                    text: candidate.text,
                    value: candidate.value,
                    composingCount: .inputCount(rec.composing.input.count),
                    lastMid: candidate.lastMid,
                    data: [DicdataElement(
                        word: candidate.text,
                        ruby: Self.toKatakana(rec.composing.convertTarget),
                        lcid: candidate.data.first?.lcid ?? CIDData.一般名詞.cid,
                        rcid: candidate.data.last?.rcid ?? CIDData.一般名詞.cid,
                        mid: candidate.lastMid,
                        value: candidate.value)])
                converter.updateLearningData(synthetic)
                engineLog("ev=typo_learn ruby=\(rec.composing.convertTarget) word=\(candidate.text)\n")
            }
            rec.composing = ComposingText()                         // 読み全体を消費（次の入力はまっさらから）
            rec.invalidateCandidateCache()
            rec.liveState = nil
            sessions[session] = rec
            return (candidate.text, "")
        }

        rec.composing.prefixComplete(composingCount: candidate.composingCount)  // 消費プレフィックスを除去（.composite は再帰処理）
        let remaining = rec.composing.convertTarget                 // prefixComplete 後 == 残り読み
        rec.invalidateCandidateCache()                              // 確定したので候補キャッシュは無効
        rec.liveState = nil                                         // 手動確定で読みが激変＝自動確定履歴は無効
        sessions[session] = rec                                     // 書き戻し必須（生きたセッションを残り読みへ更新）
        return (candidate.text, remaining)
    }

    /// 外部LLM変換: 現在の読み(convertTarget)を LLMClient へ。converter は触らない（lock不要）。
    /// echo モード（テスト用）は HTTP を呼ばず "LLM:"+reading を返す（leftContext の有無に関わらず）。
    public func llmConvert(session: Int, leftContext: String? = nil) -> Result<String, LLMError> {
        guard let rec = sessions[session] else { return .failure(LLMError(message: "no session")) }
        let reading = rec.composing.convertTarget
        if reading.isEmpty { return .failure(LLMError(message: "empty reading")) }
        if llmClient.isEcho { return .success("LLM:" + reading) }
        return llmClient.convert(reading: reading, leftContext: leftContext)
    }

    /// セッションを破棄する。
    public func endSession(session: Int) {
        // レコード除去で候補キャッシュ/ライブ状態も一緒に消える。所有マッピングからも除去する
        // （cleanupConnection が二重に触らないように、かつ接続の生存中に接続所有集合が
        // 肥大しないように）。record.connection で O(1)。
        if let rec = sessions.removeValue(forKey: session) {
            connectionSessions[rec.connection]?.remove(session)
            if connectionSessions[rec.connection]?.isEmpty == true { connectionSessions[rec.connection] = nil }
        }
        // 注意: ここで activeConverterSession を nil にしてはいけない。nil にすると次の残存セッションの
        // bindConverter が「アクティブ無し」と見なしてリセットをスキップし、終えたセッションの
        // completedData/previousInputData を引き継いでしまう。終えた id を保持したままにすれば、
        // session id は単調増加で再利用されないため、次に別 id が converter を使うとき必ずリセットされる。
        // 全セッションが消えた場合だけ下で proactively リセットする。
        //
        // 合成が1つも残っていなければ converter の合成状態をリセットする。commit() が
        // setCompletedData で残す completedData（および previousInputData/lattice/zenz セッション）は
        // converter 共有なので、確定でセッションを終えた後も残ると **次の独立セッションの変換へ漏れる**
        // （例: nihongo→日本語 を全確定→次に go を打つと afterComplete 経路で日本語が左文脈に混ざる）。
        // 部分確定はセッションを保持し endSession を呼ばないので、残り読みの変換では completedData が
        // 正しく左文脈として効く（リセットされない）。
        if sessions.isEmpty {
            converterLock.lock()
            defer { converterLock.unlock() }
            flushLearningLocked()          // Spec2: 全確定・切断の終息点でディスクへ保存
            // バグ#3 実測用: 全セッション空時の stopComposition も llama の reset_context を誘発する
            // （bindConverter の session_switch と対）。計数のみ — 修正は別トラック。
            // M-2: Zenzai 無効（zenz 不在で reset は no-op）では出さない。config 読みは converterLock 下。
            if config.weightURL != nil { engineLog("ev=llama_reset reason=all_end\n") }
            converter.stopComposition()
            activeConverterSession = nil
        }
    }

    /// 接続 `connection` で作られた全セッションを endSession 相当で掃除する（パイプ切断時に呼ぶ）。
    /// TIP が EndSession を送らずパイプを落とした場合（EndSession タイムアウト劣化・アプリ強制終了。
    /// Rust 側 drop_engine は EndSession を送らない）に、孤児セッションが sessions へ
    /// 永久残留するのを防ぐ。個々は endSession と同じ経路で片付けるので、この接続のセッションを全て
    /// 消した結果 `sessions` が空になれば endSession と同様に proactive な stopComposition() が走り、
    /// 放棄された合成の completedData/previousInputData が後続の別セッションへ左文脈として漏れるのを防ぐ。
    /// 他接続のセッションは触らない（複数クライアント常駐でも当該接続分だけを掃除する）。
    public func cleanupConnection(_ connection: Int) {
        guard let ids = connectionSessions[connection] else { return }
        // ids は Set の値コピー（値意味論）。endSession が内部で connectionSessions[connection] を
        // 変更しても、このループ対象は不変。
        for id in ids { endSession(session: id) }
        connectionSessions[connection] = nil   // 念のため（通常は最後の endSession が nil 済み）
    }

    /// cold start ③: 背景スレッドで converterLock を握ってダミー変換し、llama モデルを先読みする。
    /// runEngineHost が listening 前に呼ぶが、detach して即 return するので listening は塞がない。
    /// Zenzai ゲート（zenzaiReady）は warmUp **完了後**に開く — warmUp スレッドがロックを取る**前**に
    /// 届いた変換要求は、ゲート閉により Zenzai のインラインモデルロード（数秒＝IPC タイムアウト超過）を
    /// 踏まず、古典（辞書）変換で即応できる（Task2 の Activate プリスポーンで spawn 直後に打鍵が来る
    /// ケースの実利）。Zenzai 無効設定ならロードするものが無いので即開ける。
    ///
    /// 既知の限界（正直な注記）: warmUp が converterLock を保持している間（モデルロード中）に届いた
    /// 変換要求は、古典変換も同じ共有 converter（同じロック）を使うため即応できずロード完了を待つ。
    /// ロードをロック外へ出す案は lib 0.11.2 では不成立 — getModel/Zenz/ZenzContext は package 可視で
    /// 単体呼び出しできず、公開の predictNextCharacter 経由でも getModel が converter 共有状態
    /// （zenz/zenzStatus）を書き、stopComposition（bindConverter/endSession）の zenz 読みとロック無しで
    /// 競合する（data race）。完全な古典即応には upstream の public preload API（ロック外ロード）か
    /// 「busy 応答」プロトコルが必要（follow-up）。
    public func startWarmUp() {
        guard zenzaiEnabled else {
            zenzaiReady = true
            return
        }
        Thread.detachNewThread { [weak self] in self?.warmUp() }
    }

    private func warmUp() {
        let t0 = DispatchTime.now()
        do {
            converterLock.lock()
            defer { converterLock.unlock() }
            var dummy = ComposingText()
            dummy.insertAtCursorPosition("tesuto", inputStyle: .roman2kana)
            // ゲート（zenzaiReady）はまだ閉なので、forceZenzai で Zenzai ON の options を組んで
            // モデルロードを誘発する（これが warm-up の眼目 — ゲート越しだと古典に落ちてしまう）。
            _ = converter.requestCandidates(dummy, options: makeOptions(forceZenzai: true))
            // ロックを放す前にゲートを開ける: このロックを待っていた変換要求は、起きた時点で必ず
            // Zenzai になる（converterLock 保持中の zenzaiReadyLock 取得は makeOptions と同順＝反転しない）。
            zenzaiReady = true
        }
        // M-1: stage=warmup は実所要（モデルロード込み）をスレッド内で測って完了時に出す
        // （呼び出し側で startWarmUp を測ると detach の即 return で常に ~0ms になる）。
        let ms = Double(DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds) / 1_000_000
        engineLog("ev=coldstart stage=warmup ms=\(String(format: "%.1f", ms))\n")
    }

    /// cold start ①: プロセス起動後の初回変換の所要をワンショットで出す。**converterLock 保持中に呼ぶこと**
    /// （convert/liveConvert の計測区間が lock 内のため、フラグも同じ規律で直列化される）。
    private func logFirstConvertOnceLocked(ms: Double) {
        guard !firstConvertLogged else { return }
        firstConvertLogged = true
        engineLog("ev=coldstart stage=first_convert ms=\(String(format: "%.1f", ms))\n")
    }

    /// graceful 停止（Shutdown IPC → 応答後 exit）の前段: 保留中の学習をディスクへフラッシュする。
    /// flushLearningLocked は private かつ「converterLock 保持中に呼ぶこと」契約なので、ここで
    /// converterLock を取ってから呼ぶ公開ラッパ。呼び出し元 handler は serviceLock を保持しており、
    /// converterLock をその内側で取るのは既存の順序（clearLearning と同型）に従う。
    public func prepareForShutdown() {
        converterLock.lock()
        defer { converterLock.unlock() }
        flushLearningLocked()
    }

    /// 保留中の学習をディスクへフラッシュする。**converterLock 保持中に呼ぶこと**。
    /// commitUpdateLearningData は throw しない（失敗はライブラリ内で握られ、一時トライは
    /// 成功時のみクリアされる＝失敗分は次の契機で自然に再試行）。観測は所要 ms ログのみ。
    private func flushLearningLocked() {
        guard learning.enabled else { return }
        let t0 = DispatchTime.now()
        converter.commitUpdateLearningData()
        let ms = Double(DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds) / 1_000_000
        engineLog("ev=learning_flush ms=\(String(format: "%.1f", ms))\n")
    }

    /// 学習履歴を消去する（RAM の一時トライ＋ディスクの学習ファイル）。ClearLearning IPC から呼ばれる。
    /// 戻り値 = ディスクの学習ファイルを消し切れたか。false（mmap ロック等で残存）は呼び出し側で
    /// Error 応答にする — 「Ok なのに次の変換で学習が復活する」事故を防ぐ（I-4）。
    /// resetMemory は **学習 ON のときだけ** 呼ぶ: OFF 中はライブラリの memoryURL が %TEMP% ルート
    /// （workDir）を指しており、reset がそこを suffix 掃除してしまう（I-5）。OFF 中の一時トライは
    /// 常に空（更新は enabled ゲート済み＋toggle-off 時に flush 成功でライブラリがクリア）なので、
    /// OFF 中は dir 直削除だけで足りる。
    public func clearLearning() -> Bool {
        converterLock.lock()
        defer { converterLock.unlock() }
        if learning.enabled {
            converter.resetMemory()   // RAM+ディスク（memoryDir 配下）を即消去＋LOUDSキャッシュ解放
        }
        let dir = learning.memoryDir
            ?? LearningSettings.resolveDir(environment: ProcessInfo.processInfo.environment)
        var clean = true
        if let dir, let files = try? FileManager.default.contentsOfDirectory(atPath: dir.path) {
            for f in files where f.hasPrefix("memory") || f == ".pause" {
                try? FileManager.default.removeItem(at: dir.appendingPathComponent(f))
            }
            // 消し切れたか検証（mmap 共有違反等で残ると、次の変換の遅延ロードで学習が戻る）。
            if let after = try? FileManager.default.contentsOfDirectory(atPath: dir.path) {
                clean = !after.contains { $0.hasPrefix("memory") || $0 == ".pause" }
            }
        }
        engineLog("ev=learning_clear clean=\(clean)\n")
        return clean
    }

    /// U9: ZenzaiMode 構築を切り出す（テスト容易化のため static・ZenzaiConfig を直接受ける）。
    /// leftSideContext は Zenzai v3 の左文脈（変換品質の最大レバー）。nil は従来どおり `.v3(.init())`。
    /// weightURL が無ければ Zenzai 自体を使わない（`.off`）。
    /// maxLeftSideContextLength は指定しない（ライブラリ既定 40 に任せる）。
    static func makeZenzaiMode(config: ZenzaiConfig, leftSideContext: String?) -> ConvertRequestOptions.ZenzaiMode {
        guard let weight = config.weightURL else { return .off }
        return .on(
            weight: weight,
            inferenceLimit: config.inferenceLimit,
            personalizationMode: nil,
            versionDependentMode: .v3(.init(leftSideContext: leftSideContext))
        )
    }

    /// `forceClassic`: 修正変換(TypoConvert)の仮説変換専用。使い捨て ComposingText を Zenzai に
    /// 通すと非決定的な上に高コストなので、修復仮説は常に古典（辞書）変換に固定する。
    private func makeOptions(nBest: Int = 10, leftSideContext: String? = nil, forceZenzai: Bool = false, forceClassic: Bool = false) -> ConvertRequestOptions {
        // cold start ③: ゲートが開く（zenzaiReady）まで Zenzai を options に載せない＝古典（辞書）変換で即応。
        // forceZenzai は warmUp 専用（ゲートを開ける前のモデル先読みロードに Zenzai ON が要る）。
        let zenzai: ConvertRequestOptions.ZenzaiMode
        if forceClassic {
            zenzai = .off
        } else if zenzaiReady || forceZenzai {
            zenzai = ConversionService.makeZenzaiMode(config: config, leftSideContext: leftSideContext)
        } else {
            zenzai = .off
        }
        return .init(
            N_best: nBest,
            requireJapanesePrediction: false,
            requireEnglishPrediction: false,
            keyboardLanguage: .ja_JP,
            fullWidthRomanCandidate: true,   // 数字/英数の全角候補を常時提供（読みは半角 canonical のまま）
            learningType: learning.enabled ? .inputAndOutput : .nothing,
            memoryDirectoryURL: learning.memoryDir ?? workDir,
            sharedContainerURL: workDir,
            textReplacer: .withDefaultEmojiDictionary(),
            specialCandidateProviders: nil,
            zenzaiMode: zenzai,
            metadata: .init(versionString: "NospacekeyEngineHost")
        )
    }
}
