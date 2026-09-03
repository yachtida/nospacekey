import Foundation
import KanaKanjiConverterModuleWithDefaultDictionary
import NospacekeyLlamaRuntimeAdapter

#if os(Windows)
import WinSDK
#endif

/// 学習ディレクトリの一要素（リンク/ジャンクションを通常ファイルとして扱わないための
/// production metadata seam）。Windows では FILE_ATTRIBUTE_REPARSE_POINT、その他の環境では
/// Foundation の symbolic-link 属性を用いる。
struct LearningPathMetadata: Sendable {
    let isDirectory: Bool
    let isRegularFile: Bool
    let isReparsePoint: Bool
}

/// FileManager の metadata を安全側に正規化する。存在しない path は nil（列挙と削除の競合で
/// 先に消えた場合）として扱い、権限その他のエラーは呼び出し側へ伝播する。
func learningPathMetadata(for url: URL) throws -> LearningPathMetadata? {
#if os(Windows)
    // GetFileAttributesW はリンク先を辿らず path 自身の属性を返す。Foundation の
    // attributesOfItem は壊れた reparse point で NotFound になり得るため、先にこれを
    // 見ておく。reparse point は regular file として扱わず、reset/delete を拒否する。
    let windowsAttributes = url.path.withCString(encodedAs: UTF16.self) { pointer in
        GetFileAttributesW(pointer)
    }
    if windowsAttributes != UInt32.max && (windowsAttributes & 0x0000_0400) != 0 {
        return LearningPathMetadata(
            isDirectory: (windowsAttributes & 0x0000_0010) != 0,
            isRegularFile: false,
            isReparsePoint: true)
    }
#endif

    let attributes: [FileAttributeKey: Any]
    do {
        attributes = try FileManager.default.attributesOfItem(atPath: url.path)
    } catch {
        let nsError = error as NSError
        if (nsError.domain == NSCocoaErrorDomain && nsError.code == NSFileNoSuchFileError) ||
            (nsError.domain == NSPOSIXErrorDomain && nsError.code == 2) {
            return nil
        }
        throw error
    }

    let type = attributes[.type] as? FileAttributeType
    let isDirectory = type == .typeDirectory
    let isRegularFile = type == .typeRegular
#if os(Windows)
    // Foundation の attributes は link 先を返すことがあるため、Windows では path 自身の
    // file attributes を確認する。取得不能時も reparse 扱いにして fail-closed にする。
    let isReparsePoint = windowsAttributes == UInt32.max ||
        (windowsAttributes & 0x0000_0400) != 0
    #else
    let resourceValues = try? url.resourceValues(forKeys: [.isSymbolicLinkKey])
    let isReparsePoint = type == .typeSymbolicLink ||
        resourceValues?.isSymbolicLink == true ||
        (try? FileManager.default.destinationOfSymbolicLink(atPath: url.path)) != nil
    #endif
    return LearningPathMetadata(
        isDirectory: isDirectory,
        isRegularFile: isRegularFile,
        isReparsePoint: isReparsePoint)
}

/// `clearLearning()` のファイル操作だけを差し替えるための最小 seam。
/// 本番では FileManager、テストでは列挙/削除の失敗を決定的に注入する。
struct LearningFileSystem: @unchecked Sendable {
    let list: (URL) throws -> [String]
    let remove: (URL) throws -> Void
    /// 本番では nil（ConversionService の vendor converter.resetMemory() を使う）。テストでは
    /// 呼び出しを観測して、unsafe preflight 前に vendor reset が走らないことを固定する。
    let resetMemory: (() -> Void)?
    /// path 自身の metadata。nil の seam は既存の deterministic listing test 用であり、
    /// production の `.live` は必ずこれを設定する。
    let metadata: ((URL) throws -> LearningPathMetadata?)?

    static let live = LearningFileSystem(
        list: { try FileManager.default.contentsOfDirectory(atPath: $0.path) },
        remove: { try FileManager.default.removeItem(at: $0) },
        resetMemory: nil,
        metadata: { try learningPathMetadata(for: $0) }
    )

    init(list: @escaping (URL) throws -> [String],
         remove: @escaping (URL) throws -> Void,
         resetMemory: (() -> Void)? = nil,
         metadata: ((URL) throws -> LearningPathMetadata?)? = nil) {
        self.list = list
        self.remove = remove
        self.resetMemory = resetMemory
        self.metadata = metadata
    }
}

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
/// `corrections`/`recordability`（訂正昇格）も同じ規律（読み書きとも converterLock 下）。
/// `llmClient` は読み(llmConvert/isEcho)/書き(reload) とも handler の serviceLock 下。reload は
/// serviceLock を握る handler から呼ばれ converterLock を **非ブロックで試す**ので、ロック反転は無い。
/// `config` の公開状態読み取り（zenzaiEnabled/inferenceLimit）は専用の短いロックで保護し、
/// reload は converterLock と同じ更新点でそのロックも取得する。status はこの短いスナップショット
/// を読むため、native warm-up を待たない。
/// `zenzaiReady`（cold start ③）は専用 `zenzaiReadyLock` で保護。makeOptions（converterLock 下）→
/// getter の一方向の入れ子しか無く、zenzaiReadyLock 保持中に他のロックは取らない＝反転しない。
/// `activeConverterSession`（bindConverter/endSession）と `firstConvertLogged`
/// （logFirstConvertOnceLocked）は読み書きとも converterLock 下（各メソッドの呼出契約）。
/// カスタム辞書のリロード（spec 2026-08-02-custom-dictionary §4.1）: `desiredDictEnabled` は専用
/// `dictStateLock`（保持中に他のロックを取らない＝反転しない）、`environment` は immutable。
/// 実作業はbounded maintenance lane上で走り、serviceLockを保持しない。
public final class ConversionService: @unchecked Sendable {
    static let defaultSnapshotAutoCommitStateLimit = 64
    struct SnapshotEnhancementKey: Equatable, Sendable {
        let composition: UInt64
        let revision: UInt64
        let configurationGeneration: UInt64
        let connectionGeneration: UInt64
    }

    struct SnapshotAutoCommitProposal: Equatable, Sendable {
        let proposal: UInt64
        let text: String
        let consumedReading: String
        let remaining: String
    }

    private struct SnapshotAutoCommitState {
        var live = LiveConversionState()
        var lastRevision: UInt64?
        var pending: (key: SnapshotEnhancementKey, value: SnapshotAutoCommitProposal, candidate: Candidate)?
    }

    private struct SnapshotAutoCommitStream: Hashable {
        let connection: Int
        let composition: UInt64
    }

    enum SnapshotEnhancementPoll: Sendable {
        case pending
        case unavailable
        case ready(text: String, candidates: [String]?, candidateRemaining: [String]?)
    }

    private struct SnapshotCandidateIdentity: Hashable {
        let text: String
        let consumedReading: Int
    }

    private struct SnapshotEnhancementWork: Sendable {
        let key: SnapshotEnhancementKey
        let baseline: UInt64
        let explicit: Bool
        let reading: String
        let classic: ConversionResult
        let snapshot: GPUWorkerCompositionSnapshot
        let leftContext: String?
        let inferenceLimit: Int
    }

    private struct CompletedSnapshotEnhancement: Sendable {
        let key: SnapshotEnhancementKey
        let baseline: UInt64
        let result: SnapshotEnhancementPoll
    }
    /// The production main host owns only the classic converter.  The worker
    /// role is the sole role allowed to compose Zenzai .on options.  `legacy`
    /// keeps the existing unit-test initializer semantics while the process
    /// entry points pass an explicit role.
    enum ProcessRole: Sendable {
        case legacy
        case mainClassicOnly
        case gpuWorker
    }

    public struct GPUWorkerEvaluation: Sendable {
        public let conversion: ConversionResult?
        public let failure: GPUWorkerFailure?

        public init(conversion: ConversionResult? = nil,
                    failure: GPUWorkerFailure? = nil) {
            self.conversion = conversion
            self.failure = failure
        }
    }

    private let converter = KanaKanjiConverter.withDefaultDictionary()
    private var learningConverter = KanaKanjiConverter.withDefaultDictionary()
    private let recentLearning = RecentLearningOverlay()
    private let learningStateLock = NSLock()
    private var learningGeneration: UInt64 = 0
    private var clearingLearning = false
    private let learningPersistenceForTesting: (@Sendable (Candidate) -> Void)?
    private let learningClearStartedForTesting: (@Sendable () -> Void)?

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
        /// キャッシュ時点の「モデル1位」表層（昇格・修復ブロック挿入で並びが動く**前**の素の
        /// 先頭候補）。訂正記録の除外基準 — 判定を表示リスト添字（index != 0）だけにしないのは、
        /// 昇格発火時は表示 index 0 が昇格候補で「0=モデル正解」の前提が破れ、モデル正解の
        /// 選択が訂正記録され既存訂正を上書き破壊するため（文節スコープの modelTopTexts と
        /// 同じ理由の文レベル版）。
        var cachedModelTop: String? = nil
        /// キャッシュ時点で訂正昇格が実際に起きたか(promoted() 非nil)。un-learn の発火条件 —
        /// cachedModelTop の文字列一致だけでは「昇格が model top を押し下げた窓での選択」と
        /// 「昇格の無い窓(typoConvert の修復ブロック先頭等)で literal 1位を普通に選んだ」が
        /// 区別できず、後者で訂正を誤削除する(第3R敵対レビュー N-1)。記録除外(!= modelTop)は
        /// fail-safe なので流用可だが、削除は fail-destructive なので昇格の実発火を要求する。
        var cachedPromoted: Bool = false
        /// typoConvert が cachedCandidates に積んだ「修復候補ブロック」由来の index 集合。
        /// commit がこの集合に含まれる index を確定するときだけ、全消費＋誤読み合成ペア学習の
        /// 特別経路へ分岐する。
        /// 不変条件: 非nil なのは、直近の typoConvert が積んだ修復ブロックが cachedCandidates に
        /// 載っている間だけ（cacheCandidates が候補と同時に設定/クリアするため、書き込み箇所ごとの
        /// 手動 nil は不要になった）。
        var typoRepairedIndices: Set<Int>? = nil
        /// ライブ変換履歴（自動確定用 — iOS LiveConversionManager の移植）。
        var liveState: LiveConversionState? = nil
        /// 文節ナビゲーション状態（MoveClause で開始）。読みの変更・確定・新しい変換で必ず破棄する
        /// — invalidateCandidateCache / cacheCandidates の両方が nil に落とすため、候補キャッシュと
        /// 同じライフサイクルで stale が構造的に残らない。
        var clauseState: ClauseState? = nil
        /// 作成元の接続 id。所有チェック（UU-2 connectionOwns）と endSession の所有集合除去が使う。
        let connection: Int

        /// 候補キャッシュを破棄する。読みが変わる/確定する全経路で呼ぶ。
        mutating func invalidateCandidateCache() {
            cachedCandidates = nil
            cachedTarget = nil
            cachedModelTop = nil
            cachedPromoted = false
            typoRepairedIndices = nil
            clauseState = nil
        }

        /// 変換結果を候補キャッシュへ載せる（invalidateCandidateCache と対）。repairedIndices は
        /// 修復ブロックを積む typoConvert だけが渡す — 省略時 nil が、convert/liveConvert に古い
        /// 修復 index が残る余地（旧レビューCritical）を構造的に塞ぐ。
        mutating func cacheCandidates(_ candidates: [Candidate], target: String, repairedIndices: Set<Int>? = nil, modelTop: String? = nil, promoted: Bool = false) {
            cachedCandidates = candidates
            cachedTarget = target
            cachedModelTop = modelTop
            cachedPromoted = promoted
            typoRepairedIndices = repairedIndices
            clauseState = nil   // 新しい変換 = 文節ナビゲーションは仕切り直し
        }
    }

    /// 文節ナビゲーション（変換中の←/→）の状態。clauses は各文節の現在表層（確定/学習に使う
    /// 実 Candidate）、candidates は選択文節の候補（SelectClauseCandidate が index で引く）。
    struct ClauseState {
        var clauses: [Candidate]
        var selected: Int
        var candidates: [Candidate] = []
        var candidateIndex: Int = 0
        /// 分解時点の各文節の表層（訂正記録の基準線）。「見えていた表層へ選び直しただけ」を
        /// 訂正にしないための不変スナップショット — clauses は select で書き換わるので使えない。
        var originalTexts: [String] = []
        /// 文節idx → その文節読み単体でのモデル1位表層（clauseCandidatesLocked が計算時に上書き）。
        var modelTopTexts: [Int: String] = [:]
        /// 文節idx → 確定時にその文節を訂正として記録するか。select のたびに上書き＝最後の
        /// 選択が勝つ。判定を表示リスト添字（chosen != 0）にしないのは、文節候補列の 0 は
        /// 「現在表層の挿入位置」や「昇格候補」であり得て commit() spec §2(a) の前提
        /// 「0=モデル正解」が破れるため — 初期位置へ戻しただけの選択やモデル正解の選択が
        /// 訂正記録され、文脈依存語の恒久固定と既存訂正の上書き破壊を起こす（第2R敵対レビュー①）。
        var clauseCorrections: [Int: Bool] = [:]
        /// 種が「文候補窓で 1 位以外を明示選択した候補」だった事実（reading=全文読み, surface=種表層）。
        /// 文節候補に触れないまま確定されたら commit() と同じ文レベル訂正として記録する —
        /// 保存しないと矢印 1 打の有無で学習結果が変わる（第2R敵対レビュー②）。
        var sentenceCorrection: (reading: String, surface: String)? = nil
        /// 種が「文候補窓でモデル1位を明示選択した候補」だった事実。文節候補に触れないまま
        /// 確定されたら commit() のモデル1位選択と同じ un-learn(昇格の拒否)を行う —
        /// sentenceCorrection と対で、矢印 1 打の有無で帰結が変わらないようにする。
        var sentenceUnlearn: (reading: String, surface: String)? = nil
        /// 文節idx → (候補列, 初期選択idx)。同じ文節へ戻る矢印のたびに Zenzai フル推論
        /// （OnKeyDown が同期 IPC で待つ変換1回ぶん）を払わないためのキャッシュ。
        var candidateCache: [Int: ([Candidate], Int)] = [:]
    }

    /// 文節ナビゲーションのビュー（IPC ClauseView の素材）。
    public struct ClauseView {
        public let segments: [String]
        public let selected: Int
        public let candidates: [String]
        public let candidateIndex: Int
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
    /// （同一セッション継続ならリセットしない＝部分確定の左文脈を保つ。Zenzai 実稼働中は audit H2
    /// によりリセット自体をスキップする — bindConverter の注記参照）。
    private var activeConverterSession: Int?
    /// Zenzai の遅延フォールバックが決まった後、次の converter 操作前に classic 用の共有状態を
    /// 一度だけ破棄する予約。遅い requestCandidates の後処理中に stopComposition すると、その要求の
    /// 結果まで壊すため、converterLock 下の次の入口で消費する。
    private var needsClassicReset = false
    /// vendor の classic 経路だけが読む確定・学習文脈の所有セッション。Zenzai 稼働中は
    /// セッション切替で stopComposition を省くため、遅延フォールバック時に「現セッションの
    /// 部分確定を残すべきか／別セッションの文脈を捨てるべきか」を値そのものではなく所有者で判定する。
    private var completedDataSession: Int?
    private var learningDataSession: Int?
    /// stopComposition の実行回数。回帰テストが「予約を消しただけ」の偽修正を見逃さないための観測窓。
    private var compositionResetCount = 0
    /// 接続 id → その接続で作られたセッション id の集合。常駐サーバは複数 TIP クライアントが
    /// それぞれ別接続で同時接続しうる（NamedPipeServer は nMaxInstances=255）ため、切断時に掃除すべき
    /// セッションを接続単位で特定する。TIP が EndSession を送らずパイプを落とす経路（EndSession
    /// タイムアウト劣化・アプリ強制終了。Rust 側 drop_engine は何も送らない）で、孤児セッションが
    /// `sessions` に永久残留するのを防ぐ（cleanupConnection）。session→接続の逆方向は
    /// SessionRecord.connection が持つ（endSession はそれで所有集合から O(1) 除去する）。
    private var connectionSessions: [Int: Set<Int>] = [:]
    private var nextId = 1
    private let workDir: URL
    private let fileSystem: LearningFileSystem
    /// 学習設定が OFF でも clear の対象 root を失わないための解決済み directory。
    /// reload の overrides は ProcessInfo.environment へ戻せないため、現在値を保持する。
    private var learningDirectory: URL?
    /// vendor が直近の requestCandidates で保持した learning config。requestCandidates 前は
    /// unknown なので、OFF→ON reload 直後に stale workDir へ resetMemory しない。
    private var vendorLearningRoot: URL?
    private var vendorLearningEnabled = false
    private var vendorLearningConfigKnown = false
    /// vendor の temporary trie は public API から flush 成否を観測できない。ON→OFF 前に
    /// flush した後は、vendor config が .nothing のままの期間に resetMemory を呼べない。
    private enum VendorTemporaryState: Equatable { case empty, mayContainData, unobservableAfterFlush }
    private var vendorTemporaryState: VendorTemporaryState = .empty
    /// UU-5: 常駐エンジンは起動後も `reload` で設定を差し替えられる（設定アプリの変更を反映）。
    /// `makeOptions` が convert ごとに読むため、`converterLock` 下で差し替えれば次回変換から効く
    /// （converter オブジェクト自体の再構築は不要＝Zenzai は options の weightURL で切替わる）。
    private var config: ZenzaiConfig
    /// Public status/configuration reads must not race reload's converterLock
    /// critical section, and status must remain independent of native work.
    private let configurationLock = NSLock()
    private let processRole: ProcessRole
    private let gpuWorkerSupervisor: GPUWorkerSupervisor?
    private let snapshotEnhancementLock = NSLock()
    private var desiredSnapshotEnhancement: SnapshotEnhancementWork?
    private var latestSnapshotEnhancement: (SnapshotEnhancementKey, UInt64)?
    private var completedSnapshotEnhancement: CompletedSnapshotEnhancement?
    private var snapshotEnhancementRunning = false
    /// Structural evidence for the main process's CPU-Zenzai prohibition.
    /// Incremented immediately before the sole vendor request seam when its
    /// effective options carry Zenzai `.on`.
    private var zenzaiInvocationCounter = ZenzaiInvocationCounter()
    /// GPU-required runtime client. All calls are made under converterLock so a failed
    /// request cannot race a retry or a model reload.
    private let zenzaiRuntime: ZenzaiRuntimeClient
    /// Typed state is kept separately from the converter's legacy zenzStatus string.
    private var _zenzaiRuntimeState: ZenzaiRuntimeState
    private var _zenzaiRuntimeStatus = ZenzaiRuntimeStatus.unconfigured
    /// Status queries must remain responsive while warm-up holds converterLock. Keep a
    /// sanitized snapshot behind its own short lock instead of exposing converter state.
    private let zenzaiRuntimeSnapshotLock = NSLock()
    private var _zenzaiRuntimeSnapshot: ZenzaiRuntimeSnapshot
    private var warmupDecodeAttemptsBaseline: UInt64 = 0
    private var warmUpInFlight = false
    private let warmUpControlLock = NSLock()
    private var warmUpActiveForReload = false
    private var warmUpCancellationRequested = false
    private var warmUpWeightURL: URL?
    private var warmUpRuntimeDirectory: URL?
    /// Spec2: 学習設定。読み(makeOptions/commit)/書き(reload) とも `converterLock` 下（config と同じ規律）。
    private var learning: LearningSettings
    /// 訂正昇格テーブル(spec 2026-07-30-correction-promotion)。読み書きとも converterLock 下
    /// (learning と同じ規律)。reload で learning.memoryDir が変わったら作り直す。
    private var corrections: CorrectionStore
    /// RecordCorrection(文字列しか運ばない)の記録可否照合用: 直近 32 読みの
    /// 「表層 → 記録可(isLearningTarget かつ全被覆)」+ その読みで直近に観測したモデル1位
    /// 表層の集合(昇格前の素の先頭。commit の cachedModelTop と同じ除外基準 — 昇格発火時の
    /// 候補窓は index 1 がモデル1位で、TIP の index != 0 送出だけでは弾けない)。
    /// fail-closed(マップミスは棄却) — 記録漏れは再訂正で済むが、誤記録は既存訂正の
    /// 上書き破壊になるため。modelTops が単一値でなく集合なのは、常駐エンジンは複数接続
    /// 共有で、同一読みの別接続変換がエントリを上書きし得る — ユーザーが見ていた旧1位の
    /// 選択が新1位基準で false-accept されるのを防ぐ(mergedModelTops の注記)。
    /// 容量を 8 にしないのは、別接続の変換が挟まると追い出し→棄却で訂正が無言で失われるため。
    /// 読み書きとも converterLock 下。
    private var recordability: [(reading: String, surfaces: [String: Bool], modelTops: [String])] = []
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
    private var snapshotAutoCommitStates: [SnapshotAutoCommitStream: SnapshotAutoCommitState] = [:]
    private var appliedSnapshotAutoCommitReceipts: [SnapshotAutoCommitStream: (SnapshotEnhancementKey, UInt64)] = [:]
    private var nextSnapshotAutoCommitProposal: UInt64 = 0
    private let snapshotAutoCommitStateLimit: Int
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
    /// Zenzai 推論が重すぎて古典（辞書）変換へフォールバックしたか。true の間 makeOptions が
    /// ZenzaiMode を .off に落とし、変換は古典で即応する。1回でも推論が zenzaiSlowThresholdMs を
    /// 超えると true に張り付き、このエンジンプロセスの生存中は古典固定（ハングするPC環境での
    /// 「ぶっ壊れない」体験を最優先 — Zenzai なしでも精度は十分ある）。
    /// リセットは reload のみ（ユーザーが設定アプリで Zenzai を明示操作した時）。
    /// zenzaiReadyLock と同じ規律（専用 NSLock・他ロック取らない・makeOptions の converterLock 下
    /// 読みと convert の converterLock 下書きを直列化）。
    private let zenzaiTooSlowLock = NSLock()
    private var _zenzaiTooSlow = false
    public private(set) var zenzaiTooSlow: Bool {
        get { zenzaiTooSlowLock.lock(); defer { zenzaiTooSlowLock.unlock() }; return _zenzaiTooSlow }
        set { zenzaiTooSlowLock.lock(); defer { zenzaiTooSlowLock.unlock() }; _zenzaiTooSlow = newValue }
    }
    /// Zenzai 推論の「重い」判定閾値（ms）。1回の推論がこれを超えたら zenzaiTooSlow=true。
    /// **TIP 側 IPC タイムアウトより前に自発的に古典へ落ちる安全裕度**として設定する。
    /// convert/reconvert/typoConvert/moveClause は TIP 側 IPC_TIMEOUT_CONVERT(1200ms) に晒される
    /// ため 800ms（400ms の裕度）。liveConvert は IPC_TIMEOUT_LIVE(400ms) に晒されるため
    /// 300ms（100ms の裕度）— 400ms に届く前にSwift側でフォールバックを決めないと、TIP 側が
    /// タイムアウトして Swift の liveConvert が呼ばれなくなり checkZenzaiTooSlowLocked が発火しない。
    /// Zenzai small(Q5_K_M) の通常推論は短文脈で数百ms未満。長文脈(leftContext最大40文字)では
    /// 伸びうるが、重いPCの恒常ハング防止を優先し、一過性スパイクは初回スキップで吸収する。
    private let zenzaiSlowThresholdMs: Double = 800
    private let zenzaiSlowThresholdLiveMs: Double = 300
    /// cold start ①: プロセス起動後の「初回変換」(convert/liveConvert の先勝ち) を一度だけ計測する
    /// ワンショット。読み書きとも converterLock 下（両呼び出し元が t0〜ms 計測を lock 内で行う）。
    private var firstConvertLogged = false
    /// zenzaiTooSlow 監視の初回スキップカウンタ。初回 cold spike（KVキャッシュがまだ温まっていない
    /// 一時的な遅延）で本来速いPCが誤って古典へ落ちるのを防ぐため、warmUp 完了後の最初の数回の
    /// Zenzai推論をスキップしてから監視を開始する。**実際に Zenzai 推論として実行されたもののみ**が
    /// 消費する（合計回数ベース — op別でなく呼出順非依存。convert/liveConvert/typoConvert(literal)/
    /// reconvert/文節候補のいずれかの実推論から）。古典変換・ウォームアップ待ち・forceClassic・
    /// invalid/nonexistent weight の silent fallback・空入力・マージ/昇格/キャッシュ/自動確定の時間は
    /// 消費しない — 誤消費は Zenzai が一度も走らないまま skip を尽くし、最初の実推論が cold spike
    /// として即 disable される（High）。reload で 0 にリセット（モデルは既にホットなので cold spike
    /// ガードは不要）。
    /// 読み書きとも converterLock 下。
    private var slowWatchSkipsRemaining = 1
    private let slowWatchSkipInitial = 1
    private let slowWatchSkipAfterReload = 0
    /// 外部LLM変換クライアント。echo 判定（`isEcho`）も含めここに一本化する。
    /// UU-5: `reload` で差し替え可能（LLMClient は config を保持するだけなのでモデル再ロード等は不要）。
    private var llmClient: LLMClient
    /// spawn 時の環境変数。カスタム辞書のリロードが `UserDictionary.resolve` をやり直すために
    /// 保持する（immutable なのでロック無しでワーカスレッドから読める）。テストはここに
    /// NOSPACEKEY_USER_DICT / LOCALAPPDATA を注入して辞書ファイルを差し込む。
    private let environment: [String: String]
    /// カスタム辞書のリロード作業を直列化する専用レーン。ハンドラ（serviceLock 下）は
    /// ここへ積むだけで即返り、実際の I/O と converterLock 取得はこのレーンの上で行う
    /// （spec §4.1: ハンドラ内 blocking 取得は warm-up 中に全クライアントの打鍵を凍らせる）。
    /// Session teardown and persistence are deliberately outside the request handler.  This queue
    /// is serial so vendor learning state is finalized in commit order without making EndSession
    /// acknowledgement wait for disk or native cleanup.
    private let maintenance = BackgroundMaintenance()
    /// desiredDictEnabled 専用のロック。保持中に他のロックは取らない（反転しない）。
    private let dictStateLock = NSLock()
    /// 「望ましい辞書状態」。**書くのは init（env から1回）と ReloadDictionary ハンドラだけ**で、
    /// work item は読むだけ — 起動時 enqueue が env 値で書き戻す実装だと、pipe 開通直後に
    /// 届いた `{enabled:false}` を上書きして辞書が勝手に有効へ戻る競合窓ができる（spec §4.1）。
    private var desiredDictEnabled: Bool
    private var dictionaryReloadGeneration: UInt64 = 0
    private let dictionaryRetryDelay: DispatchTimeInterval

    /// 本番用: env から「明示 weight → per-user(%LOCALAPPDATA%) → exe 隣」の3段解決表で
    /// Zenzai 設定を解決する（ZenzaiConfig.resolve と同一の表 — UIバグ8）。
    /// テストからは呼ばないこと（exe 隣のモデル有無で挙動が環境依存になる）。テストは `init(config:)` を使う。
    public convenience init() {
        self.init(productionMain: true)
    }

    /// Production main entry point.  Its converter is permanently classic;
    /// GPU ranking is owned by the injected isolated-worker supervisor.
    convenience init(productionMain: Bool) {
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
                  typoLearn: env["NOSPACEKEY_TYPO_LEARN"] != "0",
                  environment: env,
                  processRole: productionMain ? .mainClassicOnly : .legacy,
                  gpuWorkerSupervisor: productionMain
                    ? GPUWorkerSupervisor(
                        transport: NativeGPUWorkerTransport(),
                        runtimeConfiguration: GPUWorkerRuntimeConfiguration(config: cfg),
                        allowsLazyStart: false) : nil)
        // Plan4: ユーザ辞書(ワンショット移行 JSON)+組み込み日付テンプレートの起動時ロード。
        // ここは runEngineHost の service.startWarmUp() より前(EngineHost.swift:128→136)＝
        // warm-up スレッド起動前の初期化時点なので競合しない(メソッド側でも lock は取る)。
        loadUserDictionary(from: UserDictionary.resolve(environment: env),
                           enabled: UserDictionary.enabled(environment: env))
    }

    /// テスト用: 設定を明示注入する。llmClient は任意注入（既定は未設定＝disabled）。
    /// autoCommit の既定 `.weak` は本番既定（AutoCommitStrength.resolve の未設定時）と同値。
    /// autoCommitMaxReading の既定 25 は本番既定（AutoCommitLengthBackstop.resolve の未設定時）と同値。
    /// environment は辞書リロードの解決に使う env（既定 `[:]` ＝ resolve が nil＝辞書なし）。
    convenience init(config: ZenzaiConfig,
                            learning: LearningSettings = .disabled,
                            llmClient: LLMClient = LLMClient(config: LLMConfig.resolve(environment: [:])),
                            autoCommit: AutoCommitStrength = .weak,
                            autoCommitMaxReading: Int = 25,
                            typoLearn: Bool = true,
                            environment: [String: String] = [:],
                            runtimeClient: ZenzaiRuntimeClient = NativeZenzaiRuntimeClient(),
                            processRole: ProcessRole = .legacy,
                            gpuWorkerSupervisor: GPUWorkerSupervisor? = nil,
                            privateTemporaryDirectory: URL? = nil,
                            snapshotAutoCommitStateLimit: Int = defaultSnapshotAutoCommitStateLimit,
                            dictionaryRetryDelay: DispatchTimeInterval = .milliseconds(100)) {
        self.init(config: config, learning: learning, llmClient: llmClient,
                  autoCommit: autoCommit, autoCommitMaxReading: autoCommitMaxReading,
                  typoLearn: typoLearn, environment: environment,
                  runtimeClient: runtimeClient,
                  fileSystem: .live,
                  processRole: processRole,
                  gpuWorkerSupervisor: gpuWorkerSupervisor,
                  privateTemporaryDirectory: privateTemporaryDirectory,
                  snapshotAutoCommitStateLimit: snapshotAutoCommitStateLimit,
                  dictionaryRetryDelay: dictionaryRetryDelay)
    }

    /// テスト用のファイル操作注入。公開 initializer は本番 seam を露出しない。
    init(config: ZenzaiConfig,
         learning: LearningSettings = .disabled,
         llmClient: LLMClient = LLMClient(config: LLMConfig.resolve(environment: [:])),
         autoCommit: AutoCommitStrength = .weak,
         autoCommitMaxReading: Int = 25,
         typoLearn: Bool = true,
         environment: [String: String] = [:],
         runtimeClient: ZenzaiRuntimeClient = NativeZenzaiRuntimeClient(),
         fileSystem: LearningFileSystem,
         processRole: ProcessRole = .legacy,
         gpuWorkerSupervisor: GPUWorkerSupervisor? = nil,
         privateTemporaryDirectory: URL? = nil,
         snapshotAutoCommitStateLimit: Int = defaultSnapshotAutoCommitStateLimit,
         learningPersistenceForTesting: (@Sendable (Candidate) -> Void)? = nil,
         learningClearStartedForTesting: (@Sendable () -> Void)? = nil,
         dictionaryRetryDelay: DispatchTimeInterval = .milliseconds(100)) {
        self.config = config
        self.processRole = processRole
        self.gpuWorkerSupervisor = gpuWorkerSupervisor
        self.zenzaiRuntime = runtimeClient
        let initialRuntimeState: ZenzaiRuntimeState = config.weightURL == nil
            ? .classic(reason: Self.classicReason(for: config))
            : .classic(reason: .notStarted)
        self._zenzaiRuntimeState = initialRuntimeState
        self._zenzaiRuntimeSnapshot = Self.runtimeSnapshot(
            state: initialRuntimeState,
            status: .unconfigured,
            zenzaiEnabled: config.weightURL != nil)
        self.learning = learning
        self.corrections = CorrectionStore(directory: learning.memoryDir)
        self.llmClient = llmClient
        self.autoCommit = autoCommit
        self.autoCommitMaxReading = autoCommitMaxReading
        self.snapshotAutoCommitStateLimit = max(1, snapshotAutoCommitStateLimit)
        self.learningPersistenceForTesting = learningPersistenceForTesting
        self.learningClearStartedForTesting = learningClearStartedForTesting
        self.dictionaryRetryDelay = dictionaryRetryDelay
        self.typoLearn = typoLearn
        self.environment = environment
        self.desiredDictEnabled = UserDictionary.enabled(environment: environment)
        self.fileSystem = fileSystem
        self.workDir = privateTemporaryDirectory ?? FileManager.default.temporaryDirectory
        self.learningDirectory = learning.memoryDir ?? LearningSettings.resolveDir(environment: environment)
    }

    /// Zenzai が有効か（重みが解決できたか）。
    public var zenzaiEnabled: Bool {
        configurationLock.lock()
        defer { configurationLock.unlock() }
        return config.weightURL != nil
    }

    /// Current engine state. The value is sanitized and contains no model path or input.
    public var zenzaiRuntimeState: ZenzaiRuntimeState {
        if processRole == .mainClassicOnly, zenzaiEnabled, let supervisor = gpuWorkerSupervisor {
            let worker = supervisor.snapshot
            switch worker.state {
            case .preparing: return .warming
            case .gpuActive:
                return .gpuActive(device: worker.device ?? "unknown")
            case .classic:
                return .classic(reason: Self.classicReason(fromWorkerReason: worker.reason))
            case .disabled: return .classic(reason: .userDisabled)
            case .stopped: return .classic(reason: .notStarted)
            }
        }
        converterLock.lock()
        defer { converterLock.unlock() }
        return _zenzaiRuntimeState
    }

    /// Non-blocking sanitized state for the settings UI. The snapshot lock is independent
    /// from converterLock because warm-up may hold the latter for native model work.
    public var zenzaiRuntimeSnapshot: ZenzaiRuntimeSnapshot {
        if processRole == .mainClassicOnly, zenzaiEnabled, let supervisor = gpuWorkerSupervisor {
            let worker = supervisor.snapshot
            let state: ZenzaiRuntimeSnapshot.DisplayState
            switch worker.state {
            case .stopped, .preparing: state = .preparing
            case .gpuActive: state = .gpuActive
            case .classic: state = .classic
            case .disabled: state = .disabled
            }
            return ZenzaiRuntimeSnapshot(
                state: state, backend: worker.backend, device: worker.device,
                reason: worker.reason)
        }
        zenzaiRuntimeSnapshotLock.lock()
        defer { zenzaiRuntimeSnapshotLock.unlock() }
        return _zenzaiRuntimeSnapshot
    }

    /// Sanitized worker-only state for host integrations that need to
    /// distinguish child lifecycle from the legacy runtime status.
    public var gpuWorkerRuntimeSnapshot: GPUWorkerSupervisorSnapshot? {
        gpuWorkerSupervisor?.snapshot
    }

    /// Worker-only handshake/evaluation seam.  It exposes typed native state
    /// but never serializes candidate or composing text data.
    public func gpuWorkerHandshake(generation: UInt64) -> GPUWorkerHandshakeResponse {
        guard processRole == .gpuWorker else {
            return GPUWorkerHandshakeResponse(
                generation: generation, ready: false, failure: .unknown)
        }
        converterLock.lock()
        defer { converterLock.unlock() }
        let status = zenzaiRuntime.status()
        setRuntimeStatusLocked(status)
        guard case .gpuActive = _zenzaiRuntimeState,
              status.state == .gpuActive,
              status.failure == .none,
              status.decodeAttempts > warmupDecodeAttemptsBaseline,
              Self.hasTrustedGPUIdentity(status) else {
            return GPUWorkerHandshakeResponse(
                generation: generation, ready: false,
                backend: status.backend.isEmpty ? nil : status.backend,
                device: status.device.isEmpty ? nil : status.device,
                failure: Self.workerRuntimeFailure(
                    status, requireTrustedGPUIdentity: true))
        }
        return GPUWorkerHandshakeResponse(
            generation: generation, ready: true,
            backend: status.backend.isEmpty ? nil : status.backend,
            device: status.device.isEmpty ? nil : status.device)
    }

    /// Evaluate exactly one worker request after warm-up.  The returned
    /// ConversionResult is local to the child; host wire code copies the
    /// public candidate structure needed for safe display and commit.
    public func evaluateGPUWorker(
        snapshot: GPUWorkerCompositionSnapshot,
        leftContext: String?,
        nBest: Int,
        inferenceLimit: Int
    ) -> GPUWorkerEvaluation {
        guard processRole == .gpuWorker else {
            return GPUWorkerEvaluation(failure: .unavailable)
        }
        guard snapshot.supportsGPUWorker,
              let composing = try? snapshot.makeComposingText(),
              !composing.convertTarget.isEmpty else {
            return GPUWorkerEvaluation(failure: .unsupportedInput)
        }
        converterLock.lock()
        defer { converterLock.unlock() }
        guard inferenceLimit == config.inferenceLimit else {
            return GPUWorkerEvaluation(failure: .protocolMismatch)
        }
        let before = zenzaiRuntime.status()
        setRuntimeStatusLocked(before)
        guard case .gpuActive = _zenzaiRuntimeState,
              before.state == .gpuActive, before.failure == .none,
              before.decodeAttempts >= warmupDecodeAttemptsBaseline,
              Self.hasTrustedGPUIdentity(before) else {
            return GPUWorkerEvaluation(
                failure: Self.workerFailure(before, requireTrustedGPUIdentity: true))
        }
        let options = makeOptions(
            nBest: max(10, nBest), leftSideContext: leftContext,
            forceZenzai: true, noLearning: true)
        let conversion = requestCandidatesLocked(composing, options: options)
        let after = zenzaiRuntime.status()
        setRuntimeStatusLocked(after)
        guard after.state == .gpuActive, after.failure == .none,
              Self.hasTrustedGPUIdentity(after) else {
            return GPUWorkerEvaluation(
                failure: Self.workerFailure(after, requireTrustedGPUIdentity: true))
        }
        guard after.decodeAttempts > before.decodeAttempts else {
            return GPUWorkerEvaluation(failure: .decode)
        }
        return GPUWorkerEvaluation(conversion: conversion)
    }

    /// Alias used by diagnostics and tests at the public boundary.
    public var runtimeState: ZenzaiRuntimeState { zenzaiRuntimeState }

    /// Last status observed from the native seam.
    public var zenzaiRuntimeStatus: ZenzaiRuntimeStatus {
        converterLock.lock()
        defer { converterLock.unlock() }
        return _zenzaiRuntimeStatus
    }

    /// 実効の Zenzai 推論上限（観測/テスト用。config は private のため読み取り口を公開する）。
    public var zenzaiInferenceLimit: Int {
        configurationLock.lock()
        defer { configurationLock.unlock() }
        return config.inferenceLimit
    }

    private static func runtimeSnapshot(
        state: ZenzaiRuntimeState,
        status: ZenzaiRuntimeStatus,
        zenzaiEnabled: Bool
    ) -> ZenzaiRuntimeSnapshot {
        let backend = status.backend.isEmpty ? nil : status.backend
        let device = status.device.isEmpty ? nil : status.device
        switch state {
        case .probing, .warming:
            return ZenzaiRuntimeSnapshot(
                state: .preparing, backend: backend, device: device)
        case .gpuActive:
            return ZenzaiRuntimeSnapshot(
                state: .gpuActive, backend: backend, device: device)
        case .classic(let reason):
            let displayState: ZenzaiRuntimeSnapshot.DisplayState
            if reason == .userDisabled || reason == .cpuUnsupported {
                displayState = .disabled
            } else if reason == .notStarted && zenzaiEnabled {
                displayState = .preparing
            } else {
                displayState = .classic
            }
            return ZenzaiRuntimeSnapshot(
                state: displayState, backend: backend, device: device,
                reason: reason.description)
        }
    }

    private static func classicReason(fromWorkerReason reason: String?) -> ZenzaiClassicReason {
        switch reason {
        case ZenzaiClassicReason.invalidRuntimeDirectory.description: return .invalidRuntimeDirectory
        case ZenzaiClassicReason.backendPathRejected.description: return .backendPathRejected
        case ZenzaiClassicReason.backendUnavailable.description: return .backendUnavailable
        case ZenzaiClassicReason.gpuUnavailable.description: return .gpuUnavailable
        case ZenzaiClassicReason.modelLoadFailed.description: return .modelLoadFailed
        case ZenzaiClassicReason.contextLoadFailed.description: return .contextLoadFailed
        case ZenzaiClassicReason.decodeFailed.description: return .decodeFailed
        case ZenzaiClassicReason.warmupFailed.description: return .warmupFailed
        case ZenzaiClassicReason.tooSlow.description: return .tooSlow
        default: return .unknownRuntimeFailure
        }
    }

    private static func hasTrustedGPUIdentity(_ status: ZenzaiRuntimeStatus) -> Bool {
        let backend = status.backend.trimmingCharacters(in: .whitespacesAndNewlines)
            .lowercased()
        let device = status.device.trimmingCharacters(in: .whitespacesAndNewlines)
        // The shipped runtime is Vulkan-only.  The device name is evidence (and
        // may vary by driver), so require it to be present without hard-coding
        // one Radeon marketing string.
        return backend.contains("vulkan") && !device.isEmpty
    }

    private static func workerRuntimeFailure(
        _ status: ZenzaiRuntimeStatus,
        requireTrustedGPUIdentity: Bool = false
    ) -> GPUWorkerRuntimeFailure {
        if requireTrustedGPUIdentity,
           status.state == .gpuActive,
           status.failure == .none,
           !hasTrustedGPUIdentity(status) {
            return .gpuUnavailable
        }
        switch status.failure {
        case .invalidRuntimeDirectory: return .invalidRuntimeDirectory
        case .backendPathRejected: return .backendPathRejected
        case .backendUnavailable: return .backendUnavailable
        case .gpuUnavailable: return .gpuUnavailable
        case .modelLoad: return .modelLoad
        case .contextLoad: return .contextLoad
        case .decode: return .decode
        case .unknown: return .unknown
        case .none:
            switch status.state {
            case .unconfigured: return .invalidRuntimeDirectory
            case .gpuActive: return .none
            case .failed: return .unknown
            }
        }
    }

    private static func workerFailure(
        _ status: ZenzaiRuntimeStatus,
        requireTrustedGPUIdentity: Bool = false
    ) -> GPUWorkerFailure {
        switch workerRuntimeFailure(
            status, requireTrustedGPUIdentity: requireTrustedGPUIdentity) {
        case .invalidRuntimeDirectory: return .invalidRuntimeDirectory
        case .backendPathRejected: return .backendPathRejected
        case .backendUnavailable: return .backendUnavailable
        case .gpuUnavailable: return .gpuUnavailable
        case .modelLoad: return .modelLoad
        case .contextLoad: return .contextLoad
        case .decode: return .decode
        case .none, .unknown: return .warmup
        }
    }

    /// **converterLock 保持中に呼ぶこと**。UI snapshot は native status の内部 counter を
    /// 越境させず、state/status の更新を同じ観測点へ反映する。
    private func updateRuntimeSnapshotLocked() {
        let snapshot = Self.runtimeSnapshot(
            state: _zenzaiRuntimeState,
            status: _zenzaiRuntimeStatus,
            zenzaiEnabled: config.weightURL != nil)
        zenzaiRuntimeSnapshotLock.lock()
        _zenzaiRuntimeSnapshot = snapshot
        zenzaiRuntimeSnapshotLock.unlock()
    }

    /// **converterLock 保持中に呼ぶこと**。
    private func setRuntimeStateLocked(_ state: ZenzaiRuntimeState) {
        _zenzaiRuntimeState = state
        updateRuntimeSnapshotLocked()
    }

    /// **converterLock 保持中に呼ぶこと**。
    private func setRuntimeStatusLocked(_ status: ZenzaiRuntimeStatus) {
        _zenzaiRuntimeStatus = status
        updateRuntimeSnapshotLocked()
    }

    /// Plan4: ユーザ辞書(ワンショット移行 JSON)+組み込み日付テンプレートを converter へ載せる。
    /// `importDynamicUserDictionary` は**丸ごと置換**（DicdataStoreState が配列を代入するだけ）
    /// なので、テンプレートとインポート辞書を必ず1配列に結合して**1回だけ**呼ぶ。
    /// 本番は convenience init から呼ばれる（startWarmUp 前＝warm-up スレッド起動前）。
    /// テストは `init(config:)` の後に任意の URL（nil=テンプレートのみ）で呼ぶ。
    /// converter を触るので converterLock 下で行う（init 時点では無競合だが、後から呼ばれても
    /// warm-up/変換と直列化される規律を守る）。起動後の辞書更新は ReloadDictionary IPC →
    /// `requestDictionaryReload` で反映する（docs/superpowers/specs/2026-08-02-custom-dictionary-design.md
    /// §4.3 が旧 plan の設計ロック「起動時ロードのみ」を上書きした）。
    /// `enabled=false` は**ファイルを読まずに**テンプレートのみ（評価順序は enabled が先 — 同 §4.1）。
    func loadUserDictionary(from url: URL?, enabled: Bool = true) {
        let templates = UserDictionary.builtinDateTemplates()
        var dicdata = templates
        if enabled, let url {
            let imported = UserDictionary.load(url: url)
            dicdata.append(contentsOf: imported)
            engineLog("ev=user_dict loaded=\(imported.count) templates=\(templates.count)\n")
        }
        converterLock.lock()
        defer { converterLock.unlock() }
        converter.importDynamicUserDictionary(dicdata)
    }

    /// ReloadDictionary IPC の受け口: desired 状態を更新し、リロードを直列キューへ積んで**即返る**。
    /// ここで converterLock を取らないのが本設計の眼目 — ハンドラは serviceLock を保持しており、
    /// warm-up がモデルロード中に握る converterLock を blocking 待ちすると全クライアントの
    /// 打鍵が数秒凍る。ReloadConfig の「非ブロック試行→skip」も採れない（辞書には次回接続で
    /// 再 push する経路が無く、skip した更新が engine 再起動まで失われる）— spec §4.1。
    public func requestDictionaryReload(enabled: Bool) {
        dictStateLock.lock()
        desiredDictEnabled = enabled
        dictionaryReloadGeneration &+= 1
        let generation = dictionaryReloadGeneration
        dictStateLock.unlock()
        enqueueDictionaryReload(generation: generation, attempt: 0)
    }

    /// desired を変えずに再読だけを積む。pipe 作成直後（`NamedPipeServer.onListening`）から呼び、
    /// 「エンジン init の辞書ロード後〜pipe 作成前」に落ちた保存（接続失敗＝不達）を拾い直す。
    /// 各作業がファイルと desired を読み直すので冪等（二重読みは無害）。
    public func enqueueDictionaryReload() {
        dictStateLock.lock()
        dictionaryReloadGeneration &+= 1
        let generation = dictionaryReloadGeneration
        dictStateLock.unlock()
        enqueueDictionaryReload(generation: generation, attempt: 0)
    }

    private func enqueueDictionaryReload(generation: UInt64, attempt: Int) {
        maintenance.submitLatest(label: "dictionary_reload") { [weak self] in
            try self?.runDictionaryReload(generation: generation, attempt: attempt)
        }
    }

    private func runDictionaryReload(generation: UInt64, attempt: Int) throws {
        dictStateLock.lock()
        let current = dictionaryReloadGeneration == generation
        dictStateLock.unlock()
        guard current else { return }
        if dictionaryReloadWork() { return }

        dictStateLock.lock()
        let stillCurrent = dictionaryReloadGeneration == generation
        dictStateLock.unlock()
        guard stillCurrent else { return }
        guard attempt < 2 else {
            engineLog("ev=user_dict retry_giveup generation=\(generation) attempt=\(attempt + 1)\n")
            throw DictionaryReloadError.failed
        }

        engineLog("ev=user_dict retry_scheduled generation=\(generation) attempt=\(attempt + 2) deadline_ms=100\n")
        let accepted = maintenance.submitLatestAfter(
            label: "dictionary_reload", delay: dictionaryRetryDelay
        ) { [weak self] in
            try self?.runDictionaryReload(generation: generation, attempt: attempt + 1)
        }
        if !accepted {
            engineLog("ev=user_dict retry_superseded generation=\(generation) attempt=\(attempt + 2)\n")
        }
    }

    /// 直列キュー上のリロード作業。I/O はロック外、converterLock 内は import だけ
    /// （ロック内でファイル読み＋JSON デコードを行うと数万語辞書で全クライアントの変換が止まる）。
    private func dictionaryReloadWork() -> Bool {
        dictStateLock.lock()
        let enabled = desiredDictEnabled
        dictStateLock.unlock()

        var dicdata = UserDictionary.builtinDateTemplates()
        if enabled {
            // resolve をやり直すのは、起動時に不在だったファイルが GUI 初回登録で生まれるため。
            switch UserDictionary.loadResult(url: UserDictionary.resolve(environment: environment)) {
            case .absent:
                break
            case .loaded(let imported):
                dicdata.append(contentsOf: imported)
                engineLog("ev=user_dict reloaded=\(imported.count)\n")
            case .failed:
                // 一過性の読み失敗で「動いていた辞書の全消滅」を起こさないため import 自体を行わない。
                engineLog("ev=user_dict reload_failed\n")
                return false
            }
        }
        converterLock.lock()
        defer { converterLock.unlock() }
        converter.importDynamicUserDictionary(dicdata)
        // classic 経路は previousInputData 一致時に増分ラティスを使い辞書を索き直さないため、
        // 落とさないとリロード後の同一読み再変換に新語が出ない。Zenzai 実稼働中
        // （!tooSlow — isZenzaiOperationalLocked）は classic キャッシュを使わないので、
        // reset_context スパイクを払わない。tooSlow の古典フォールバック中は classic キャッシュ
        // が経路そのものなので、リセットして新語の可視性を保つ。
        // activeConverterSession には触れないこと（nil を置くと bindConverter のリセットが
        // スキップされ、終了セッションの文脈が次セッションへ漏れる — endSession の注記）。
        if !isZenzaiOperationalLocked { stopCompositionLocked() }
        return true
    }

    /// テスト専用: 直列キューに積まれたリロードの完了を待つ（キューは serial なので sync で足りる）。
    func flushDictionaryQueueForTesting() {
        maintenance.barrier()
    }

    @discardableResult
    func releaseDictionaryRetryForTesting() -> Bool {
        let released = maintenance.releaseDelayedForTesting()
        maintenance.barrier()
        return released
    }

    /// テスト専用: **背景スレッドで** converterLock を保持し、解放クロージャを返す。
    /// 同一スレッド保持にしないのは、壊れた実装（ハンドラ内 blocking lock）で XCTFail ではなく
    /// 自己デッドロック＝テストランナーごとハングになるため。NSLock は非所有スレッドの unlock が
    /// UB なので、解放も取得したスレッド自身が行う（クロージャは semaphore を signal するだけ）。
    /// 戻ったクロージャは unlock 完了まで待つ（テスト終了と ConversionService 解放の追い越し防止）。
    func beginConverterLockHoldForTesting() -> () -> Void {
        let acquired = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let released = DispatchSemaphore(value: 0)
        Thread.detachNewThread { [self] in
            converterLock.lock()
            acquired.signal()
            release.wait()
            converterLock.unlock()
            released.signal()
        }
        acquired.wait()
        return { release.signal(); released.wait() }
    }

    /// UU-5: 常駐エンジンの LLM/Zenzai 設定を差し替える（設定アプリの変更を接続中に反映）。
    /// `overrides` は TIP が push した設定値（LLMConfig.resolve / ZenzaiConfig.resolve が読む env キー）。
    ///
    /// #2: `overrides` は丸ごと置換ではなく **実プロセス env に重ねる**。丸ごと置換すると spawn 時のみ
    /// 効く env が消える — `NOSPACEKEY_LLM_ECHO`（テスト/診断の echo）、および resolve_env_map が注入を
    /// 控えて尊重している D6 の env override（push しないキーは env 側が勝つ。例: 診断 env
    /// `NOSPACEKEY_ZENZAI_INFERENCE_LIMIT` が居るとき TIP は本キーを push せず、この重ね方が env を生かす）。
    ///
    /// #1b: `converterLock` は warm-up がモデルロード中ずっと保持する（cold start ③でもこの構造は維持 —
    /// ロック外ロードは lib の可視性/共有状態の制約で不可。startWarmUp の注記参照）。ここでブロック
    /// 待ちすると handler の serviceLock を握ったまま数秒固まり、全クライアントの全要求を warm-up
    /// 終了まで凍らせ ReloadConfig 自体もタイムアウトする。そこで **非ブロックで試し、取れなければ
    /// skip** する。安全な理由: converterLock が埋まっているのは spawn 直後の warm-up（or 変換中）で、
    /// その間 config は spawn 時 env（=当時の最新 settings）のまま＝まだ変わっていない。busy の
    /// Error を受けた TIP が同一接続で上限付きの遅延再送をする（EngineHost の .error 文言と
    /// text_service.rs schedule_reload_retry 参照 — 再送後も busy なら次回接続で反映）。
    /// LLMClient は config を保持するだけ（モデル再ロード不要）。呼び出しは handler の serviceLock 下で
    /// 直列化されるため llmConvert とは競合しない。
    /// 注: Zenzai を新たに有効化した直後の初回変換はモデルをその場ロードするため一度だけ遅い（warm-up はしない）。
    /// - Returns: 設定を適用できたか。`converterLock` が warm-up/変換中で取れない場合は
    ///   false（busy — 中身はスキップ。巡2 D5: 呼び出し側が応答へ反映して「成功」を
    ///   詐称しないようにする）。
    @discardableResult
    public func reload(
        overrides: [String: String],
        cpuMeetsLlamaBaseline: Bool = ZenzaiConfig.runtimeCPUMeetsLlamaBaseline
    ) -> Bool {
        var env = ProcessInfo.processInfo.environment
        for (k, v) in overrides { env[k] = v }
        let exeDir = (Bundle.main.executableURL ?? URL(fileURLWithPath: CommandLine.arguments[0]))
            .deletingLastPathComponent()
        // cpuMeetsLlamaBaseline はテスト注入用（巡2 D3 — AVX2 非搭載機で resolve が候補探索
        // 前に nil へ短路し、reload 経由のテストが環境依存で失敗するのを防ぐ）。
        let newZenzai = ZenzaiConfig.resolve(
            exeDir: exeDir, environment: env, cpuMeetsLlamaBaseline: cpuMeetsLlamaBaseline)
        let newLLM = LLMConfig.resolve(environment: env)
        let newLearning = LearningSettings.resolve(environment: env)
        var scheduleZenzaiWarmUp = false
        // 通常は非ブロック取得。warm-up中にZenzaiをOFFへ切る場合だけ、warm-upを
        // キャンセルして短時間待ち、OFF設定を取りこぼさない。
        let cancelWarmUp = requestWarmUpCancellationIfConfigurationChanged(
            weightURL: newZenzai.weightURL,
            runtimeDirectory: newZenzai.runtimeDirectory)
        let lockDeadline = cancelWarmUp ? Date().addingTimeInterval(2) : Date()
        if converterLock.lock(before: lockDeadline) {
            defer { converterLock.unlock() }
            // OFF では LearningSettings.memoryDir が nil になるため、明示 directory を失わない。
            // env に新しい directory があればそれを採用し、無ければ直前の clear root を保持する。
            let resolvedEnvironmentDirectory = LearningSettings.resolveDir(environment: env)
            let newLearningDirectory: URL?
            if let dir = newLearning.memoryDir {
                newLearningDirectory = dir
            } else if let explicit = env["NOSPACEKEY_MEMORY_DIR"], !explicit.isEmpty {
                newLearningDirectory = resolvedEnvironmentDirectory
            } else {
                // A test/embedded caller may inject a memoryDir while the process env still has
                // LOCALAPPDATA; turning learning OFF must keep clearing that injected root.
                newLearningDirectory = self.learningDirectory ?? resolvedEnvironmentDirectory
            }
            // Spec2: OFF へ切り替わる前に保留分を保存（.nothing では新規更新が止まり save も skip される
            // ＝保留分が「凍結」され、後で ON に戻すと古い保留分が書かれうる。先に保存して空にしておく。
            // 注: ライブラリの updateConfig(.nothing) は一時トライをクリアしない — LearningMemory.swift:645-650）。
            if self.learning.enabled && !newLearning.enabled {
                if processRole != .mainClassicOnly {
                    flushLearningLocked()
                    if vendorTemporaryState == .mayContainData {
                        vendorTemporaryState = .unobservableAfterFlush
                    }
                }
                enqueueCorrectionPersistenceLocked()
            }
            // audit H2: Zenzai 有効→無効の切替時は一度だけフルリセットする。稼働中に bindConverter が
            // （切替スパイク排除のため）温存してきた classic 分岐の文脈（completedData 等）と zenz の
            // KV/zenzaiCache を、古典モードへ入る前に一掃する（以後の切替リセットは classic 規律に戻る）。
            // 非 nil→別の非 nil（モデル差し替え — フォールバック切替含む）も stopComposition:
            // 依存ライブラリのモデルキャッシュが URL 不一致でその場リロードするため、文脈も一掃する
            // （敵対レビュー巡1 G1-B）。
            // 巡2 D1/D2: 差し替え判定は「両非 nil」に限定する。旧実装の単純 URL 比較は
            // 非 nil→nil（無効化）も真になり、(a) H2 行と二重に stopComposition を発火し、
            // (b) 無効化で cold spike は起きないのに初回 skip を復活させていた（純関数
            // shouldRestoreSkipOnReload の「無効化時は復活させない」意味論との矛盾）。
            let weightSwapped = self.config.weightURL != nil && newZenzai.weightURL != nil
                && self.config.weightURL != newZenzai.weightURL
            let runtimeDirectoryChanged = self.config.weightURL != nil && newZenzai.weightURL != nil &&
                self.config.runtimeDirectory != newZenzai.runtimeDirectory
            let inferenceLimitChanged = self.config.inferenceLimit != newZenzai.inferenceLimit
            let disabledReasonChanged = self.config.weightURL == nil &&
                self.config.disabledReason != newZenzai.disabledReason
            let modelConfigurationChanged = self.config.weightURL != newZenzai.weightURL ||
                runtimeDirectoryChanged || inferenceLimitChanged || disabledReasonChanged
            if self.config.weightURL != nil && newZenzai.weightURL == nil { stopCompositionLocked() }
            if weightSwapped { stopCompositionLocked() }
            // 訂正昇格テーブルは学習 directory と運命共同体: dir が変わったら flush して作り直す。
            if self.learningDirectory != newLearningDirectory {
                enqueueCorrectionPersistenceLocked()
                corrections = CorrectionStore(directory: newLearningDirectory)
            }
            // self.config の差し替え前に、旧 weightURL をキャプチャ（新規有効化判定で self.config が
            // 既に newZenzai に置き換わった後だと old==new で常に false になる — 行451 と同じパターン）。
            let oldWeightURL = self.config.weightURL
            self.learning = newLearning
            self.learningDirectory = newLearningDirectory
            configurationLock.lock()
            self.config = newZenzai
            configurationLock.unlock()
            self.llmClient = LLMClient(config: newLLM)
            self.autoCommit = AutoCommitStrength.resolve(environment: env)
            self.autoCommitMaxReading = AutoCommitLengthBackstop.resolve(environment: env)
            self.typoLearn = env["NOSPACEKEY_TYPO_LEARN"] != "0"
            // A model/runtime change starts a new native generation. Ordinary settings
            // reloads preserve a failure latch so a broken backend is not retried in a loop.
            if modelConfigurationChanged {
                // 遅延フォールバック予約を残したまま Zenzai を再有効化しない。上のモデル無効化/
                // 差し替えで既にリセット済みなら stopCompositionLocked が予約を消しているため二重実行しない。
                if self.needsClassicReset { self.stopCompositionLocked() }
                self.zenzaiTooSlow = false
                self.setRuntimeStatusLocked(.unconfigured)
                if newZenzai.weightURL == nil {
                    if processRole == .mainClassicOnly {
                        gpuWorkerSupervisor?.disable()
                    }
                    self.setClassicLocked(Self.classicReason(for: newZenzai))
                } else {
                    if processRole == .mainClassicOnly {
                        gpuWorkerSupervisor?.modelOrRuntimeChanged(
                            configuration: GPUWorkerRuntimeConfiguration(config: newZenzai))
                    }
                    self.setRuntimeStateLocked(.classic(reason: .notStarted))
                    self.zenzaiReady = false
                    scheduleZenzaiWarmUp = true
                }
                engineLog("ev=zenzai_reset reason=model_or_runtime_change\n")
            } else if self.zenzaiTooSlow {
                // Slow inference is an independent, user-tunable fallback latch. It is
                // reset by the existing reload contract, while native failure latches above
                // remain unchanged unless the model/runtime generation changes.
                self.zenzaiTooSlow = false
                if self.needsClassicReset { self.stopCompositionLocked() }
                engineLog("ev=zenzai_reset reason=reload\n")
            }
            // 初回スキップ: モデルが既にホット（Zenzai 継続/無効→無効/無効化）なら 0 で即監視。
            // ただし Zenzai を新規有効化（weightURL が nil→有効値）した直後はモデルが未ロードで、
            // 初回 convert がインラインモデルロード＋初回推論（KV冷え）で本質的に遅くなる（reload の注記参照）。
            // この cold spike を吸収するため、新規有効化時だけ初回スキップを復活させる。
            // モデル差し替え（非 nil→別の非 nil — G1-B）も新 URL でインラインロードが走るため
            // 同じ cold spike が発生する: weightSwapped も初回スキップの対象に含める（両非nil限定 — D2）。
            let newlyEnabledZenzai = ConversionService.shouldRestoreSkipOnReload(
                old: oldWeightURL, new: newZenzai.weightURL) || weightSwapped
            self.slowWatchSkipsRemaining = newlyEnabledZenzai ? self.slowWatchSkipInitial : self.slowWatchSkipAfterReload
            if scheduleZenzaiWarmUp {
                if processRole == .mainClassicOnly {
                    // The main converter has no native Zenzai lifecycle.  A model/runtime
                    // generation change warms the isolated child instead of reopening the
                    // legacy in-process probe path.
                    Thread.detachNewThread { [weak self] in
                        self?.gpuWorkerSupervisor?.startWarmUp()
                    }
                } else {
                    Thread.detachNewThread { [weak self] in self?.startWarmUp(explicitRetry: true) }
                }
            }
            engineLog("ev=reload_config zenzai=\(newZenzai.weightURL != nil) inference_limit=\(newZenzai.inferenceLimit) llm=\(newLLM.enabled) learning=\(newLearning.enabled) auto_commit=\(self.autoCommit.rawValue) auto_commit_max_reading=\(self.autoCommitMaxReading) typo_learn=\(self.typoLearn)\n")
            return true
        } else {
            // warm-up/変換中。config は現状維持（skip 安全 — 内部状態は壊れない）。
            // TIP が busy の Error を受けて同一接続で上限付きの遅延再送をする
            // （EngineHost の .error 文言と text_service.rs schedule_reload_retry 参照）。
            // 巡2 D5: 「反映されなかった」ことを応答へ伝える（旧実装は busy でも .ok を
            // 返し、TIP 側で成功扱いになる詐称だった）。
            engineLog("ev=reload_config skipped=busy\n")
            return false
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

    private static func classicReason(for config: ZenzaiConfig) -> ZenzaiClassicReason {
        switch config.disabledReason {
        case .userDisabled: return .userDisabled
        case .cpuUnsupported: return .cpuUnsupported
        case .modelMissing, .none: return .modelMissing
        }
    }

    /// isZenzaiOperationalLocked の判定表（純粋関数 — converter/実モデルを参照しないため
    /// truth table として直接検証できる）。実稼働 = ready × !tooSlow × weightURL あり ×
    /// zenzStatus が成功形（"load <url>" ちょうど）の全て。tooSlow を含める理由:
    /// zenzaiTooSlow の古典フォールバック中、変換は classic 分岐（previousInputData/lattice/
    /// completedDataを読む）で走る。reset skip の特典（切替スパイク排除）を受けてよいのは
    /// Zenzai 分岐で走っている間 ＝ !tooSlow の間だけ。
    static func isZenzaiOperational(ready: Bool, tooSlow: Bool, weightURL: URL?, zenzStatus: String) -> Bool {
        guard ready, !tooSlow, let weightURL else { return false }
        return zenzStatus == "load \(weightURL.absoluteString)"
    }

    /// Zenzai が「実際にモデルロード済みで変換に使われている」か。**converterLock 保持中に呼ぶこと**
    /// （config/zenzStatus 読みの規律）。実体は純粋 helper isZenzaiOperational(_:tooSlow:weightURL:
    /// zenzStatus:)（判定表の固定はあちらの truth table テスト）。weightURL と ready ゲートに加え
    /// **zenzaiTooSlow でないこと**と、zenzStatus の成功形（"load <url>" ちょうど — 失敗時は空白＋
    /// エラー説明が付く。KanaKanjiConverter.getModel 0.11.x の形式）で判定する。壊れた重み等で
    /// ロード失敗し古典へサイレント劣化している間は false ＝ bindConverter は従来どおりリセット
    /// する（classic 分岐の文脈漏れ防止が優先）。zenzaiTooSlow の古典フォールバック中も同じ理由で
    /// false ＝ reset 側: その間 classic 分岐が稼働中キャッシュを読むため、skip すると別セッション/
    /// 旧辞書の previousInputData/lattice/completedData が残置される。reload で Zenzai を新規有効化
    /// した直後も、初回の Zenzai 変換が成功するまでは false（安全側）。
    private var isZenzaiOperationalLocked: Bool {
        guard case .gpuActive = _zenzaiRuntimeState else { return false }
        return ConversionService.isZenzaiOperational(
            ready: zenzaiReady, tooSlow: zenzaiTooSlow,
            weightURL: config.weightURL, zenzStatus: converter.zenzStatus)
    }

    /// 監視対象の requestCandidates が**実際に Zenzai 推論として走ったか**: options の .on 要求
    /// （requestedZenzai — makeOptionsWithZenzaiUsage の報告）に加え、対象入力が非空（空入力は
    /// 推論が走らない）で、モデルのロード成功（isZenzaiOperationalLocked — 成功 status=
    /// "load <url>"）を満たす。要求だけでは不十分: invalid/nonexistent weight では upstream の
    /// requestCandidates が古典へ silent fallback するため、要求が真でも実推論は走っていない
    /// ＝false — この間の skip 消費・tooSlow 化は「Zenzai が一度も走らないまま skip を尽くし、
    /// 最初の実推論が cold spike として即 disable」の誤消費（High）になる。
    /// **converterLock 保持中に、監視対象の requestCandidates の直後**に呼ぶこと — zenzStatus は
    /// converter の共有状態で、後段（setCompletedData 等）に遅らせると読みが信用できなくなる。
    private func zenzaiInferenceUsedLocked(requestedZenzai: Bool, input: String) -> Bool {
        requestedZenzai && !input.isEmpty && isZenzaiOperationalLocked
    }

    /// **converterLock 保持中に呼ぶこと**。共有 converter の全合成状態を破棄し、保留中の
    /// classic リセットも同時に消費する。直接 stopComposition を呼ばず必ずここを通す。
    private func stopCompositionLocked() {
        converter.stopComposition()
        needsClassicReset = false
        completedDataSession = nil
        learningDataSession = nil
        compositionResetCount += 1
    }

    /// vendor の classic 文脈と、その所有セッションを同じ lock 区間で更新する。
    private func setCompletedDataLocked(_ candidate: Candidate, session: Int) {
        converter.setCompletedData(candidate)
        completedDataSession = session
        refreshDeferredClassicResetLocked()
    }

    /// updateLearningData は vendor の `lastData` も更新するため、その所有者も追跡する。
    private func updateLearningDataLocked(_ candidate: Candidate, session: Int) {
        converter.updateLearningData(candidate)
        learningDataSession = session
        refreshDeferredClassicResetLocked()
        vendorTemporaryState = .mayContainData
    }

    /// requestCandidates が options を vendor の config へ反映した直後に呼ぶ。空読みでは vendor
    /// 自身が updateIfRequired を早期 return するため root を更新しない。
    private func noteVendorLearningConfigurationLocked(_ options: ConvertRequestOptions,
                                                       input: ComposingText) {
        guard !input.convertTarget.isEmpty else { return }
        vendorLearningRoot = options.memoryDirectoryURL
        vendorLearningEnabled = options.learningType != .nothing
        vendorLearningConfigKnown = true
        // vendor の LearningManager.updateConfig は learningType=.nothing で早期 return
        // するため、temporaryMemory は消えない。OFF/noLearning request を「RAM が空になった」
        // と扱わず、mayContainData / unobservableAfterFlush をそのまま保持する。
    }

    private func setClassicLocked(_ reason: ZenzaiClassicReason) {
        setRuntimeStateLocked(.classic(reason: reason))
        zenzaiReady = true
        engineLog("ev=zenzai_classic reason=\(reason.description)\n")
    }

    /// Start a new probe. No native status/configure call is made for disabled or missing
    /// models, which keeps the classic-only path independent of runtime DLLs.
    @discardableResult
    private func beginRuntimeProbeLocked(explicitRetry: Bool) -> Bool {
        guard let weightURL = config.weightURL else {
            setClassicLocked(Self.classicReason(for: config))
            return false
        }
        var isDirectory = ObjCBool(false)
        guard weightURL.isFileURL,
              FileManager.default.fileExists(atPath: weightURL.path, isDirectory: &isDirectory),
              !isDirectory.boolValue else {
            setClassicLocked(.modelMissing)
            return false
        }
        guard let runtimeDirectory = config.runtimeDirectory else {
            setClassicLocked(.invalidRuntimeDirectory)
            return false
        }
        setRuntimeStateLocked(.probing)
        let status = zenzaiRuntime.configure(
            trustedRuntimeDirectory: runtimeDirectory,
            explicitRetry: explicitRetry)
        setRuntimeStatusLocked(status)
        guard status.state == .gpuActive, status.failure == .none else {
            setClassicLocked(status.classicReason ?? .backendUnavailable)
            return false
        }
        warmupDecodeAttemptsBaseline = status.decodeAttempts
        setRuntimeStateLocked(.warming)
        engineLog("ev=zenzai_probe backend=\(status.backend)\n")
        return true
    }

    private func refreshRuntimeAfterRequestLocked() -> ZenzaiClassicReason? {
        guard config.weightURL != nil else { return nil }
        guard case .classic = _zenzaiRuntimeState else {
            let status = zenzaiRuntime.status()
            setRuntimeStatusLocked(status)
            if let reason = status.classicReason {
                setClassicLocked(reason)
                return reason
            }
            return nil
        }
        return nil
    }

    /// A native failure invalidates the result that requested Zenzai. Re-run the exact
    /// composing input with classic options so the caller still receives a result.
    private func requestCandidatesWithRuntimeFallbackLocked(
        _ input: ComposingText,
        options: ConvertRequestOptions,
        requestedZenzai: Bool,
        classicOptions: ConvertRequestOptions,
        leftContext: String? = nil,
        deadline: GPUWorkerDeadlineTier = .convert
    ) -> ConversionResult {
        if processRole == .mainClassicOnly {
            return requestClassicAndRerankLocked(
                input, leftContext: leftContext, nBest: max(options.N_best, 10), deadline: deadline)
        }
        let result = requestCandidatesLocked(input, options: options)
        guard requestedZenzai, refreshRuntimeAfterRequestLocked() != nil else { return result }
        consumePendingClassicResetLocked()
        return requestCandidatesLocked(input, options: classicOptions)
    }

    /// 全ての requestCandidates をここへ集約し、vendor learning root の観測窓を漏らさない。
    /// **converterLock 保持中に呼ぶこと**。
    private func requestCandidatesLocked(_ input: ComposingText,
                                         options: ConvertRequestOptions) -> ConversionResult {
        var effectiveOptions = options
        if processRole == .mainClassicOnly {
            // The main process never enters the vendor Zenzai path.  This is
            // a second guard in addition to the call-site classic options.
            effectiveOptions.zenzaiMode = .off
        }
        if effectiveOptions.zenzaiMode != .off {
            zenzaiInvocationCounter.recordInvocation()
        }
        let result = converter.requestCandidates(input, options: effectiveOptions)
        if processRole == .mainClassicOnly,
           ProcessInfo.processInfo.environment["NOSPACEKEY_GPU_WORKER_E2E"] == "1" {
            engineLog(
                "ev=zenzai_main_vendor_invocations count=\(zenzaiInvocationCounter.value)\n")
        }
        noteVendorLearningConfigurationLocked(effectiveOptions, input: input)
        return result
    }

    /// Main-process conversion seam: generate the complete classic candidate
    /// pool first, then ask the isolated worker for GPU candidates.  Exact
    /// text matches reuse the classic objects; GPU-only candidates retain
    /// commit structure but are made ineligible for persistent learning.
    private func requestClassicAndRerankLocked(
        _ input: ComposingText,
        leftContext: String?,
        nBest: Int,
        deadline: GPUWorkerDeadlineTier = .convert
    ) -> ConversionResult {
        let poolSize = max(10, nBest)
        let classicOptions = makeOptions(
            nBest: poolSize, leftSideContext: leftContext, forceClassic: true)
        let classic = requestCandidatesLocked(input, options: classicOptions)
        guard processRole == .mainClassicOnly,
              let gpuWorkerSupervisor,
              !input.convertTarget.isEmpty else {
            return classic
        }
        let decision = gpuWorkerSupervisor.rerank(
            classic: classic,
            snapshot: GPUWorkerCompositionSnapshot(input),
            leftContext: leftContext,
            nBest: poolSize,
            inferenceLimit: config.inferenceLimit,
            deadline: deadline.workerBudget)
        if let failure = decision.failure {
            // Only the sanitized category is logged; no input/candidate text.
            engineLog("ev=zenzai_worker_fallback reason=\(failure.rawValue)\n")
        }
        return decision.conversion
    }

    private func requestWarmUpCancellationIfConfigurationChanged(
        weightURL: URL?, runtimeDirectory: URL?
    ) -> Bool {
        warmUpControlLock.lock()
        defer { warmUpControlLock.unlock() }
        guard warmUpActiveForReload,
              warmUpWeightURL != weightURL ||
                (weightURL != nil && warmUpRuntimeDirectory != runtimeDirectory) else { return false }
        warmUpCancellationRequested = true
        return true
    }

    private func takeWarmUpCancellationRequest() -> Bool {
        warmUpControlLock.lock()
        defer { warmUpControlLock.unlock() }
        let requested = warmUpCancellationRequested
        warmUpCancellationRequested = false
        return requested
    }

    private func markWarmUpStarted() {
        warmUpControlLock.lock()
        warmUpActiveForReload = true
        warmUpCancellationRequested = false
        warmUpWeightURL = config.weightURL
        warmUpRuntimeDirectory = config.runtimeDirectory
        warmUpControlLock.unlock()
    }

    private func markWarmUpFinished() {
        warmUpControlLock.lock()
        warmUpActiveForReload = false
        warmUpCancellationRequested = false
        warmUpWeightURL = nil
        warmUpRuntimeDirectory = nil
        warmUpControlLock.unlock()
    }

    /// GPU稼働中に温存したclassic文脈は、classicへ入る境界でだけ破棄する。
    /// **converterLock 保持中に呼ぶこと**。
    private func consumePendingClassicResetLocked() {
        guard needsClassicReset else { return }
        stopCompositionLocked()
    }

    private func refreshDeferredClassicResetLocked() {
        guard isZenzaiOperationalLocked else { return }
        needsClassicReset = Self.requiresClassicReset(
            activeSession: activeConverterSession,
            completedDataSession: completedDataSession,
            learningDataSession: learningDataSession)
    }

    /// 共有 converter を `session` 用に束ねる。classic稼働中は別セッションの文脈を即時破棄する。
    /// **converterLock 保持中に呼ぶこと**（stopComposition/zenzStatus が converter を触るため）。
    ///
    /// audit H2 (2026-07-18): Zenzai 実稼働中（!tooSlow — isZenzaiOperationalLocked）は、このリセットを
    /// **遅延**する。stopComposition は
    /// zenz.endSession()→reset_context()（llama_free＋llama_init_from_model）を誘発し、prevInput が
    /// 空に戻るため、アプリ切替直後の 1 変換に KV 全再プリフィル分のレイテンシが上乗せされていた
    /// （頻度はアプリ切替に比例）。スキップが安全な根拠（upstream 0.11.2 精読）:
    /// - Zenzai 経路（convertToLattice の zenzai 分岐→all_zenzai）は completedData/previousInputData/
    ///   lattice を一切**読まない**（読むのは classic 分岐のみ）＝残しても変換に混入しない。
    /// - llama の KV キャッシュは get_logits が prevInput との共通接頭辞で毎回自己補正する
    ///   （llama_kv_cache_seq_rm）ため reset_context は correctness に不要
    ///   （PROGRESS 2026-07-08 バグ#3 実測の結論とも整合）。
    /// - zenzaiCache（prefix 制約ヒント）は getNewConstraint が新しい読みに対し自己検証し、採用された
    ///   制約も all_zenzai のループが現在の左文脈で zenz 再評価・自己修正する（stale ヒントの最悪影響は
    ///   初回推論の反復増、次の入力でキャッシュは現セッションのものに置き換わる）。
    /// classic 文脈の所有者が切替先と異なる場合はresetを予約し、native失敗またはslow判定でclassicへ
    /// 入る直前に一度だけ消費する。GPU成功中に即時resetするとアプリ切替ごとにcontext再生成が入り、
    /// CPU競合下でLiveConvertの400ms期限を超える。
    /// Zenzai 有効→無効の reload 切替時は reload 側が一度フルリセットして残置状態を一掃する。
    private func bindConverter(to session: Int) {
        let zenzaiOperational = isZenzaiOperationalLocked
        if let active = activeConverterSession, active != session {
            if !Self.shouldResetForSessionSwitch(isZenzaiOperational: zenzaiOperational) {
                // スキップの観測用（従来の ev=llama_reset reason=session_switch 計数と対になる）。
                engineLog("ev=llama_reset_skipped reason=session_switch\n")
            } else {
                // 古典変換は classic 分岐（completedData/previousInputData/lattice）を読むため、
                // Zenzai 非稼働時は従来どおりリセットして文脈漏れを防ぐ。この分岐では zenz は
                // ほぼ常に未ロード（未DL/ロード失敗/ready前）で reset_context は走らない。
                // 例外は reload で有効→無効へ切った後の残置 zenz と、zenzaiTooSlow の古典
                // フォールバック中のロード済み zenz（この間の変換は classic 分岐で稼働中キャッシュを
                // 読むため、reset は文脈一掃として正当）。
                stopCompositionLocked()
            }
        }
        activeConverterSession = session
        if zenzaiOperational {
            refreshDeferredClassicResetLocked()
        } else {
            consumePendingClassicResetLocked()
        }
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
        let (options, requestedZenzai) = makeOptionsWithZenzaiUsage(leftSideContext: leftContext)
        let classicOptions = makeOptions(leftSideContext: leftContext, forceClassic: true)
        let t0 = DispatchTime.now()
        let rawResults = requestCandidatesWithRuntimeFallbackLocked(
            rec.composing, options: options, requestedZenzai: requestedZenzai,
            classicOptions: classicOptions, leftContext: leftContext).mainResults
        // 監視に渡すのは requestCandidates の推論時間のみ。実稼働判定はこの直後で確定させる —
        // 要求（requestedZenzai）だけでは足りず、invalid/nonexistent weight の silent fallback 中は
        // 実推論が走っていない（zenzaiInferenceUsedLocked の注記）。
        // 昇格・記録可否・キャッシュ等の後段は Zenzai の重さではなく、数えると古典/後段が
        // 遅いだけで Zenzai が一度も走らないうちに恒久 disable し得た（旧実装の High）。
        let usedZenzai = zenzaiInferenceUsedLocked(requestedZenzai: requestedZenzai,
                                                   input: rec.composing.convertTarget)
        let inferMs = Double(DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds) / 1_000_000
        noteRecordability(reading: rec.composing.convertTarget, candidates: rawResults)
        let promotedList = promoted(rawResults, composing: rec.composing)
        let mainResults = promotedList ?? rawResults
        if let p = promotedList, p.first?.text != rawResults.first?.text {
            engineLog("ev=correction_promote kind=convert reading_chars=\(rec.composing.convertTarget.count)\n")
        }
        // commit(session:index:) が同じ並びの Candidate を index で引けるようキャッシュする。
        // 返す text 配列は mainResults と 1:1（同順）なので TIP 側 index がそのまま使える。
        // レビューCritical だった「typoConvert 後に読みが変わらないまま convert()/liveConvert() が
        // 呼ばれる経路で前回の修復 index が stale 残留」は、cacheCandidates が repairedIndices を
        // 候補と同時に置き換える（省略時 nil）ことで消えている。
        rec.cacheCandidates(mainResults, target: rec.composing.convertTarget,
                            modelTop: rawResults.first?.text, promoted: promotedList != nil)
        sessions[session] = rec
        let results = mainResults.map { $0.text }
        let ms = Double(DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds) / 1_000_000
        logFirstConvertOnceLocked(ms: ms)
        checkZenzaiTooSlowLocked(ms: inferMs, thresholdMs: zenzaiSlowThresholdMs, usedZenzai: usedZenzai)
        engineLog("ev=infer kind=convert ms=\(String(format: "%.1f", ms)) n=\(results.count) target_chars=\(rec.composing.convertTarget.count) ctx_chars=\(leftContext?.count ?? 0)\n")
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
        // 監視は literal の Zenzai .on 推論のみ: 仮説変換は forceClassic（古典）、マージ/重複除去は
        // 後段処理なので、どちらも時間を数えず skip も消費させない。全区間を数える旧実装は
        // 仮説数と辞書引きの遅さ次第で Zenzai 未実行のまま閾値を超え得た（High）。
        let (literalOptions, literalRequestedZenzai) = makeOptionsWithZenzaiUsage(leftSideContext: leftContext)
        let literalClassicOptions = makeOptions(leftSideContext: leftContext, forceClassic: true)
        let literalT0 = DispatchTime.now()
        let literalResults = requestCandidatesWithRuntimeFallbackLocked(
            rec.composing, options: literalOptions, requestedZenzai: literalRequestedZenzai,
            classicOptions: literalClassicOptions, leftContext: leftContext).mainResults
        // 実稼働判定は literal の requestCandidates 直後に確定（convert と同じ規律 — 後続の
        // forceClassic 仮説変換が converter を触る前に zenzStatus を読む）。
        let literalUsedZenzai = zenzaiInferenceUsedLocked(requestedZenzai: literalRequestedZenzai,
                                                          input: rec.composing.convertTarget)
        let literalInferMs = Double(DispatchTime.now().uptimeNanoseconds &- literalT0.uptimeNanoseconds) / 1_000_000

        var repaired: [Candidate] = []
        for hyp in hyps {
            var hypComposing = ComposingText()
            hypComposing.insertAtCursorPosition(hyp, inputStyle: .roman2kana)
            let hypResults = requestCandidatesLocked(hypComposing, options: makeOptions(nBest: 3, forceClassic: true)).mainResults
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

        rec.cacheCandidates(merged, target: rec.composing.convertTarget, repairedIndices: repairedIndices,
                            modelTop: literalResults.first?.text)
        sessions[session] = rec
        let results = merged.map { $0.text }
        let ms = Double(DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds) / 1_000_000
        checkZenzaiTooSlowLocked(ms: literalInferMs, thresholdMs: zenzaiSlowThresholdMs, usedZenzai: literalUsedZenzai)
        engineLog("ev=infer kind=typo_convert ms=\(String(format: "%.1f", ms)) n=\(results.count) hyps=\(hyps.count) target_chars=\(rec.composing.convertTarget.count)\n")
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
        rec.clauseState = nil // 同上 — 旧読みの文節分解は無効
        // cacheCandidates は意図的に呼ばない: 再変換の確定は TIP 側が `reconverting` ガードで
        // Commit IPC を迂回し resolved_text を直接挿入する契約（key_event_sink.rs）ため、
        // ここで積んだキャッシュは誰も引かない。差し替え前の旧キャッシュが残っていても、
        // commit の stale ガード（cachedTarget != convertTarget）が拒否する。
        sessions[session] = rec
        converterLock.lock()
        defer { converterLock.unlock() }
        bindConverter(to: session)
        let (options, requestedZenzai) = makeOptionsWithZenzaiUsage(leftSideContext: leftContext)
        let classicOptions = makeOptions(leftSideContext: leftContext, forceClassic: true)
        let t0 = DispatchTime.now()
        let mainCands = requestCandidatesWithRuntimeFallbackLocked(
            c, options: options, requestedZenzai: requestedZenzai,
            classicOptions: classicOptions, leftContext: leftContext).mainResults
        // 監視は推論時間のみ — 昇格（promoted）の lookup/合成は後段処理（convert と同型）。
        // 実稼働判定は requestCandidates 直後に確定（silent fallback 除外 — surface が空なら
        // 対象入力も空で推論は走らない）。
        let usedZenzai = zenzaiInferenceUsedLocked(requestedZenzai: requestedZenzai,
                                                   input: c.convertTarget)
        let inferMs = Double(DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds) / 1_000_000
        noteRecordability(reading: c.convertTarget, candidates: mainCands)
        let promotedList = promoted(mainCands, composing: c)
        if let p = promotedList, p.first?.text != mainCands.first?.text {
            engineLog("ev=correction_promote kind=reconvert reading_chars=\(c.convertTarget.count)\n")
        }
        let results = (promotedList ?? mainCands).map { $0.text }
        let ms = Double(DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds) / 1_000_000
        checkZenzaiTooSlowLocked(ms: inferMs, thresholdMs: zenzaiSlowThresholdMs, usedZenzai: usedZenzai)
        engineLog("ev=infer kind=reconvert ms=\(String(format: "%.1f", ms)) n=\(results.count) target_chars=\(c.convertTarget.count) ctx_chars=\(leftContext?.count ?? 0)\n")
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
        let (options, requestedZenzai) = makeOptionsWithZenzaiUsage(nBest: 1, leftSideContext: leftContext)
        let classicOptions = makeOptions(nBest: 1, leftSideContext: leftContext, forceClassic: true)
        let t0 = DispatchTime.now()
        let conversion = requestCandidatesWithRuntimeFallbackLocked(
            rec.composing, options: options, requestedZenzai: requestedZenzai,
            classicOptions: classicOptions, leftContext: leftContext, deadline: .live)
        let results = conversion.mainResults
        // 監視は推論時間のみ: 自動確定（setCompletedData/学習）・昇格・キャッシュの後段は
        // Zenzai の重さではない（convert と同型 — 旧実装はこれら込みで数えていた）。
        // 実稼働判定はこの直後に確定 — 自動確定の prefixComplete で対象入力（読み）が縮む前に、
        // かつ converter を触る後段の前に zenzStatus を読む（zenzaiInferenceUsedLocked の注記）。
        let usedZenzai = zenzaiInferenceUsedLocked(requestedZenzai: requestedZenzai,
                                                   input: rec.composing.convertTarget)
        let inferMs = Double(DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds) / 1_000_000
        let startedFallback = checkZenzaiTooSlowLocked(
            ms: inferMs, thresholdMs: zenzaiSlowThresholdLiveMs, usedZenzai: usedZenzai)
        // 別セッション由来の classic 文脈を一掃する必要がある回では、この応答の候補を
        // 自動確定しない。確定後に次入口の reset で completedData/lastData を消すと、残り読みの
        // afterComplete と bigram 文脈を失うため。表示は返し、次の converter 入口で一度だけ reset する。
        let suppressAutoCommit = startedFallback && needsClassicReset
        if suppressAutoCommit { rec.liveState = nil }
        // cacheCandidates はここでは呼ばない: 昇格(訂正1位)は「自動確定が起きなかった回」
        // だけ cache に載せる必要があり、確定回に呼ぶと短縮後読みで stale 候補を再キャッシュ
        // してしまうため、自動確定判定の後段で条件付きに行う(spec §3(c)1)。

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
        if allowAutoCommit, !suppressAutoCommit,
           let threshold = autoCommit.threshold, !rec.composing.convertTarget.isEmpty {
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
                // isLearningTarget ガードは commit() の注記と同じ(lastData への生タグ流入防止)。
                setCompletedDataLocked(firstClause, session: session)
                if learning.enabled && firstClause.isLearningTarget {
                    updateLearningDataLocked(firstClause, session: session)
                }
                rec.composing.prefixComplete(composingCount: firstClause.composingCount)
                rec.invalidateCandidateCache()   // 読みが縮んだので古い候補 index は無効
                state.didCompleteFirstClause()
                committed = firstClause.text
                engineLog("ev=live_auto_commit reason=\(reason) committed_chars=\(firstClause.text.count) remaining_chars=\(rec.composing.convertTarget.count)\n")
            }
            rec.liveState = state
        }
        // 昇格(spec §3(c)): 自動確定が起きなかった回だけ、cache と表示を訂正1位に差し替える。
        // 確定回に昇格しないのは remainderText の prefix 不変条件(下記)がモデル候補基準のため。
        // liveState の安定判定履歴には昇格候補を入れない(モデル外候補の混入で自動確定が壊れる)。
        var display = candidate
        if committed == nil {
            // 昇格が実際に起きた回(promoted 非nil)だけ表示を差し替える。素通し配列との
            // 内容比較で判定すると、訂正未登録かつ先頭非被覆の回に candidate フォールバックと
            // 食い違って誤発火する(表示の被覆ガード無効化+偽ログ — 最終レビュー N1)。
            // 比較は results.first でなく candidate(表示既定値): 先頭非被覆時の取りこぼし防止。
            // 被覆チェックは不要 — 昇格時の先頭は構成上「被覆する既存候補」か合成候補。
            let promotedList = promoted(results, composing: rec.composing)
            rec.cacheCandidates(promotedList ?? results, target: rec.composing.convertTarget,
                                modelTop: results.first?.text, promoted: promotedList != nil)
            if let top = promotedList?.first, top.text != candidate.text {
                display = top
                engineLog("ev=correction_promote kind=live reading_chars=\(rec.composing.convertTarget.count)\n")
            }
        }
        sessions[session] = rec

        let ms = Double(DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds) / 1_000_000
        logFirstConvertOnceLocked(ms: ms)
        engineLog("ev=infer kind=live ms=\(String(format: "%.1f", ms)) target_chars=\(rec.composing.convertTarget.count) ctx_chars=\(leftContext?.count ?? 0)\n")
        if let committedText = committed {
            // 確定文節は candidate.text の prefix（履歴の安定判定により両者の先頭文節テキストは一致）。
            // 残り表示 = 全体のライブ結果から確定分を落としたもの。空なら読みへ劣化（TIP 側でも防御）。
            let remainderText = String(candidate.text.dropFirst(committedText.count))
            return (remainderText, rec.composing.convertTarget, committedText)
        }
        let top = display.text.isEmpty ? rec.composing.convertTarget : display.text
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

    /// 候補が読み全体を被覆するか(typoConvert/liveConvert の被覆判定と同式)。
    private static func covers(_ cand: Candidate, targetCount: Int) -> Bool {
        cand.data.reduce(0) { $0 + $1.ruby.count } == targetCount
    }

    /// 記録可否マップのキーは CorrectionStore と同一の正規化+かな判定を共有する。
    /// ここを非空判定だけにすると、非かな読み(劣化経路の ASCII 等)がマップを通過して
    /// 「record したのに Store が無言棄却」の偽成功ログを生む。
    private func normalizedRecordKey(_ reading: String) -> String? {
        CorrectionStore.normalizedKey(reading)
    }

    /// 記録可否マップを更新する。**converterLock 保持中に呼ぶこと**。
    /// OR 統合: 同一表層が被覆/非被覆の両形で並ぶ場合、被覆形があれば記録可
    /// (「その読みの正当な全被覆変換」なので安全)。
    private func noteRecordability(reading: String, candidates: [Candidate]) {
        guard let key = normalizedRecordKey(reading) else { return }
        var surfaces: [String: Bool] = [:]
        let count = reading.count   // normalizeKana はスカラ 1:1 写像=正規化前後で同数
        for cand in candidates {
            let ok = cand.isLearningTarget && Self.covers(cand, targetCount: count)
            surfaces[cand.text] = (surfaces[cand.text] ?? false) || ok
        }
        let carried = recordability.first(where: { $0.reading == key })?.modelTops ?? []
        recordability.removeAll { $0.reading == key }
        recordability.insert((reading: key, surfaces: surfaces,
                              modelTops: Self.mergedModelTops(new: candidates.first?.text, carried: carried)),
                             at: 0)
        if recordability.count > 32 { recordability.removeLast(recordability.count - 32) }
    }

    /// 同一読みエントリの上書き時にモデル1位表層を持ち越す(新しい観測が先頭・重複除去・上限4)。
    /// 単一値で差し替えないのは、別接続の同一読み変換が modelTop を変えた直後、ユーザーが
    /// 「1位でよい」のつもりで選んだ旧1位が record 側で false-accept され既存訂正を上書き
    /// 破壊するため(第4R敵対レビュー①)。集合照合は fail-closed 側の誤差(稀に正当な訂正が
    /// 旧1位と衝突して棄却)しか生まない — 棄却は再訂正で回復できる。
    static func mergedModelTops(new: String?, carried: [String]) -> [String] {
        var merged = carried
        if let new {
            merged.removeAll { $0 == new }
            merged.insert(new, at: 0)
        }
        return Array(merged.prefix(4))
    }

    /// RecordCorrection の照合結果。reject 系を1本の Bool に畳まないのは、正常操作
    /// (モデル1位でよい)と無言データ欠落(マップ追い出し)がログで区別できないと、
    /// まさに追いたい後者の事象が隠れるため(第4R敵対レビュー③)。invalidReading を
    /// mapMiss と分けるのは対処が全く違うため — 前者は TIP 側の読み採取バグ/劣化経路
    /// (恒久・要調査)の signature、後者は共有 LRU の容量問題(一過性)(第5R N-4)。
    private enum RecordabilityVerdict { case recordable, invalidReading, mapMiss, modelTop, surfaceUnrecordable }

    /// **converterLock 保持中に呼ぶこと**。マップに読みが無ければ mapMiss(fail-closed —
    /// 記録漏れは無害、誤記録は既存訂正の上書き破壊)。直近に観測したモデル1位表層は
    /// modelTop — モデル正解の選択を「訂正」にしない(commit と同じ基準線)。
    private func recordabilityVerdict(reading: String, surface: String) -> RecordabilityVerdict {
        guard let key = normalizedRecordKey(reading) else { return .invalidReading }
        guard let entry = recordability.first(where: { $0.reading == key }) else { return .mapMiss }
        if entry.modelTops.contains(surface) { return .modelTop }
        return entry.surfaces[surface] == true ? .recordable : .surfaceUnrecordable
    }

    /// 訂正昇格(spec §3): lookup ヒットで同一 text を dedup し、被覆候補を先頭へ
    /// (無ければ合成候補を挿入)。**converterLock 保持中に呼ぶこと**。
    /// 戻り値 nil = 昇格なし(訂正未登録/learning OFF)。Bool を別返しにしないのは、
    /// 呼び出し側が「昇格が起きた時だけ」表示差替え・ログを行う判定に、素通し配列との
    /// 内容比較(先頭非被覆時に誤発火する — 最終レビュー N1)を使わせないため。
    private func promoted(_ results: [Candidate], composing: ComposingText) -> [Candidate]? {
        guard learning.enabled,
              let surface = corrections.lookup(reading: composing.convertTarget) else { return nil }
        let targetCount = composing.convertTarget.count
        // 同名の非被覆候補を残すと候補窓に同文字列が2つ並び、後者の選択が意図しない
        // 部分確定になる — dedup は text 全体で行う。
        var rest = results.filter { $0.text != surface }
        let top: Candidate
        if let existing = results.first(where: { $0.text == surface && Self.covers($0, targetCount: targetCount) }) {
            top = existing   // 元 Candidate を保つ = cid/学習 data も元のまま
        } else {
            top = Candidate(
                text: surface,
                value: (results.first?.value ?? 0) + 1,   // 順序目的のみ(戻り値に vendor の後段読者は居ない)
                composingCount: .inputCount(composing.input.count),   // 全消費確定を保証
                lastMid: MIDData.一般.mid,
                data: [DicdataElement(
                    word: surface,
                    ruby: Self.toKatakana(composing.convertTarget),
                    cid: CIDData.固有名詞.cid,
                    mid: MIDData.一般.mid,
                    value: -5)])
        }
        rest.insert(top, at: 0)
        // ログはここでは出さない: liveConvert 経由だと打鍵ごとに出て ev=infer と同量に
        // 膨れるため、候補窓を開く側(convert/reconvert)と live の表示差替え時だけ出す。
        return rest
    }

    /// RecordCorrection IPC(再変換訂正)の処理。記録可否マップで照合できた表層だけ記録する
    /// (fail-closed)。updateLearningData は呼ばない — lastData を汚し次確定の学習が
    /// 偽 bigram になるため(リセット手段は stopComposition のみで、それは zenz
    /// reset_context のレイテンシスパイクを招く)。学習への反映は、昇格された候補を
    /// 後日通常経路で確定した時に自然に乗る。
    public func recordCorrection(reading: String, surface: String) {
        converterLock.lock()
        defer { converterLock.unlock() }
        guard learning.enabled else { return }
        // モデル1位表層は記録しない(modelTop 棄却)が、un-learn もしない — ここの照合基盤は
        // 共有 32 件マップで、別接続の同一読み変換がエントリを上書きし得る。stale 基準での
        // 削除は fail-destructive(棄却は再訂正で済むが削除は訂正の喪失)なので、un-learn は
        // セッションローカルに昇格発火(cachedPromoted)を確認できる commit/文節種経路に
        // 限定する(第3R敵対レビュー N-4)。同じ上書きは record 側では「旧1位の false-accept」
        // になるため、modelTops を集合で持ち越して棄却する(第4R①)。再変換窓しか使わない
        // ユーザーも、同じ読みを通常変換すれば候補窓から un-learn できる。
        // reading をログへ出すのは既存の ev=infer target= / ev=correction_promote reading= と
        // 同水準の露出(機微は leftContext のみ長さで抑制する方針)。reason だけでは
        // invalid_reading(TIP 採取バグ)の再現材料が残らない。
        switch recordabilityVerdict(reading: reading, surface: surface) {
        case .invalidReading:
            engineLog("ev=correction_record_reject reason=invalid_reading reading_chars=\(reading.count)\n")
        case .mapMiss:
            engineLog("ev=correction_record_reject reason=map_miss reading_chars=\(reading.count)\n")
        case .modelTop:
            engineLog("ev=correction_record_reject reason=model_top reading_chars=\(reading.count)\n")
        case .surfaceUnrecordable:
            engineLog("ev=correction_record_reject reason=surface reading_chars=\(reading.count)\n")
        case .recordable:
            corrections.record(reading: reading, surface: surface)
            enqueueCorrectionPersistenceLocked()
            engineLog("ev=correction_record source=reconvert\n")
        }
    }

    // ---- テスト専用の観測窓(既存 typoRepairedIndices と同じ流儀。
    // 間接観測は辞書データ依存・学習効果との混同で再現性が無いため) ----

    /// テスト専用: 記録可否マップを迂回して直接 record する(昇格側の単体検証用)。
    func recordForTesting(reading: String, surface: String) {
        corrections.record(reading: reading, surface: surface)
    }
    /// テスト専用: CorrectionStore の中身を直接引く(記録の陰性/陽性検証用)。
    func correctionLookupForTesting(reading: String) -> String? {
        converterLock.lock()
        defer { converterLock.unlock() }
        return corrections.lookup(reading: reading)
    }
    /// テスト専用: 昇格テーブルだけ消す(学習効果と昇格効果の分離観測用。学習には触れない)。
    func clearCorrectionsForTesting() {
        corrections.clear()
    }
    /// テスト専用: 記録可否マップへ候補列を直接書く(本番の noteRecordability と同一経路)。
    /// 「同一読みの別接続変換がエントリを上書きする」状況は実変換では決定的に作れない
    /// (classic 変換の1位は文脈を変えても安定)ため、上書き→旧1位照合の e2e はここで固定する。
    /// 他の ForTesting と違い書き込みなので converterLock を取る。
    func noteRecordabilityForTesting(reading: String, candidates: [Candidate]) {
        converterLock.lock()
        defer { converterLock.unlock() }
        noteRecordability(reading: reading, candidates: candidates)
    }
    /// テスト専用: 記録可否マップで「記録可」の表層一覧(fail-closed 照合の陽性ケース選定用)。
    func recordableSurfacesForTesting(reading: String) -> [String] {
        guard let key = normalizedRecordKey(reading) else { return [] }
        return recordability.first { $0.reading == key }
            .map { $0.surfaces.filter(\.value).map(\.key) } ?? []
    }
    /// テスト専用: clauseCandidatesLocked の実行回数(文節ナビの推論回数)。クランプ/キャッシュが
    /// 再推論を抑止していることの直接観測用(間接観測は辞書データ・Zenzai 非決定に依存し再現性が無い)。
    private(set) var clauseInferenceCountForTesting = 0
    /// テスト専用: 現在の文節列の読み(訂正記録の陽性/陰性検証でキーに使う)。
    func clauseReadingsForTesting(session: Int) -> [String]? {
        sessions[session]?.clauseState.map { $0.clauses.map { Self.clauseReading($0) } }
    }
    /// テスト専用: 文節idx→モデル1位表層(訂正記録の除外基準。テスト側で記録可否の期待値を
    /// 実データから計算するために公開する)。
    func clauseModelTopsForTesting(session: Int) -> [Int: String]? {
        sessions[session]?.clauseState.map { $0.modelTopTexts }
    }
    /// テスト専用: 現在の選択文節候補のうち確定時に訂正記録され得る表層(=学習対象。全被覆は
    /// clauseCandidatesLocked が構造的に保証)。共有 recordability マップから引かないのは、
    /// あれが reconvert 専用で文節ナビは書き込まないため(同一読みエントリの破壊防止)。
    func clauseRecordableSurfacesForTesting(session: Int) -> [String]? {
        sessions[session]?.clauseState.map { st in
            st.candidates.filter(\.isLearningTarget).map(\.text)
        }
    }
    /// テスト専用: 候補キャッシュを直接注入する。日付テンプレート候補の parseTemplate 後形
    /// (text=展開済み・data.word=生タグ)は実辞書変換では決定的に作れない — 素の convert に
    /// 頼るとテンプレート候補が nBest に入るかが辞書スコア依存になり永久スキップ化するため。
    func cacheCandidatesForTesting(session: Int, candidates: [Candidate], target: String,
                                   modelTop: String? = nil, promoted: Bool = false) {
        guard var rec = sessions[session] else { return }
        rec.cacheCandidates(candidates, target: target, modelTop: modelTop, promoted: promoted)
        sessions[session] = rec
    }
    /// テスト専用: 変換1位の data 要素数を覗く。「学習の全文1エントリ(単一要素)が1位」という
    /// 再導出テストの前提を自己証明させる(前提が崩れるとテストが空洞化して静かに緑になる)。
    func cachedTopElementCountForTesting(session: Int) -> Int? {
        sessions[session]?.cachedCandidates?.first?.data.count
    }
    /// テスト専用: commit せず「index を確定したら残る読み」を覗く(部分確定候補の探索用)。
    func commitProbeRemaining(session: Int, index: Int) -> String? {
        guard let rec = sessions[session], let cands = rec.cachedCandidates,
              index >= 0, index < cands.count else { return nil }
        var probe = rec.composing
        probe.prefixComplete(composingCount: cands[index].composingCount)
        return probe.convertTarget
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
        setCompletedDataLocked(candidate, session: session)         // nospacekey ネイティブ確定順序（学習は updateLearningData で明示）
        // isLearningTarget=false(日付テンプレート等)は vendor 側で学習本体こそ no-op だが、
        // lastData には data 末尾(生タグ DicdataElement)が無条件に残り、次確定の bigram 左要素
        // として学習メモリへ書かれる(vendor updateLearningData) — 呼び出しごと避ける。
        if learning.enabled && candidate.isLearningTarget {
            updateLearningDataLocked(candidate, session: session)   // Spec2: RAM 学習（ディスクは endSession で）
        }

        if isRepaired {
            // 修正変換(TypoConvert)の修復候補を確定: 読み全体を消費する（残り読みという概念が無い —
            // 仮説は composingCount が literal の input 列と対応しないため prefixComplete は使えない）。
            //
            // 誤読み学習(ADR-0002): (誤読み全体, 修復表記) の合成ペアを学習器へ渡す。次回、通常の
            // convert() でも誤読みのまま修復候補が浮上するようにするための唯一の経路。
            // 予測変換(requireJapanesePrediction)は OFF 固定が前提: 学習辞書に入るこの「実在しない
            // 読み」が他の入力へ漏れる唯一の経路は前方一致の先読みで、予測 OFF の間だけ閉じている。
            // isLearningTarget ガード: 修復ブロックにもテンプレート候補が載り得る(vendor は
            // forceClassic 経路にも parseTemplate を適用する)。素通しすると「誤読み→展開済み
            // 日付」の合成ペアが恒久学習される(第3R敵対レビュー N-2 — commit() 注記の同型)。
            if learning.enabled && typoLearn && candidate.isLearningTarget {
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
                updateLearningDataLocked(synthetic, session: session)
                engineLog("ev=typo_learn ruby_chars=\(rec.composing.convertTarget.count) word_chars=\(candidate.text.count)\n")
            }
            rec.composing = ComposingText()                         // 読み全体を消費（次の入力はまっさらから）
            rec.invalidateCandidateCache()
            rec.liveState = nil
            sessions[session] = rec
            return (candidate.text, "")
        }

        let wholeReading = rec.composing.convertTarget              // prefixComplete が破壊する前に採取
        let modelTop = rec.cachedModelTop                           // invalidateCandidateCache が消す前に採取
        let promotedWindow = rec.cachedPromoted                     // 同上(un-learn の発火条件)
        rec.composing.prefixComplete(composingCount: candidate.composingCount)  // 消費プレフィックスを除去（.composite は再帰処理）
        let remaining = rec.composing.convertTarget                 // prefixComplete 後 == 残り読み
        rec.invalidateCandidateCache()                              // 確定したので候補キャッシュは無効
        rec.liveState = nil                                         // 手動確定で読みが激変＝自動確定履歴は無効
        sessions[session] = rec                                     // 書き戻し必須（生きたセッションを残り読みへ更新）
        // 訂正のみ記録(spec §2(a)): 1位拒否・全消費・非修復(この分岐)・学習対象、が全条件。
        // index==0(モデル正解)を記録しないのは文脈依存語(きかい等)の固定回避(ユーザ決定)。
        // 判定を index だけにしないのは、昇格発火時は表示 index 0 が昇格候補で index 1 が
        // モデル1位になり「0=モデル正解」の前提が破れるため — 表層でも除外する(cachedModelTop、
        // 文節スコープ modelTopTexts (cf6aca3) と同じ基準線の文レベル版)。
        // isLearningTarget=false(日付テンプレート等)を弾くのは、展開済みの古い日付が
        // 恒久1位化するため(vendor が「学習に乗せるな」と明示した候補は昇格にも乗せない)。
        if learning.enabled && index != 0 && candidate.text != modelTop
            && remaining.isEmpty && candidate.isLearningTarget {
            corrections.record(reading: wholeReading, surface: candidate.text)
            enqueueCorrectionPersistenceLocked()
            engineLog("ev=correction_record source=candidate\n")
        }
        // 昇格が押し下げたモデル1位の明示選択は「昇格の拒否」= un-learn。記録除外だけに
        // 留めると、誤登録した訂正の除去手段が ClearLearning(学習ごと全消し)しか無くなる
        // (第2R敵対レビュー②)。削除は再訂正でいつでも回復できる。
        // promotedWindow を要求するのは、text == modelTop だけでは昇格の無い窓(typoConvert は
        // 修復ブロックが先頭で literal 1位が index>0)での普通の選択と区別できず、訂正を
        // 誤削除するため(第3R敵対レビュー N-1 — 削除は fail-destructive なので記録除外の
        // 基準線をそのまま流用できない)。isLearningTarget はテンプレート1位の受容(日付が
        // 欲しかっただけ)を昇格拒否と読まないための保守側。
        if learning.enabled && promotedWindow && index != 0 && candidate.text == modelTop
            && remaining.isEmpty && candidate.isLearningTarget {
            if corrections.remove(reading: wholeReading) {
                enqueueCorrectionPersistenceLocked()
                engineLog("ev=correction_unlearn source=candidate\n")
            }
        }
        return (candidate.text, remaining)
    }

    /// 候補を先頭から文節列へ分解する（LiveConversionState.updateHistories と同じループの汎用化）。
    /// makePrefixClauseCandidate は data の先頭から1文節分を切り出す vendor API。空文節ガードも同じ。
    static func decomposeClauses(_ candidate: Candidate) -> [Candidate] {
        var clauses: [Candidate] = []
        var data = candidate.data[...]
        while !data.isEmpty {
            let clause = Candidate.makePrefixClauseCandidate(data: data)
            if clause.data.isEmpty { break }
            clauses.append(clause)
            data = data.dropFirst(clause.data.count)
        }
        return clauses
    }

    /// 文節の読み（ひらがな）。data の ruby（カタカナ）連結を正規化する。
    private static func clauseReading(_ clause: Candidate) -> String {
        normalizeKana(clause.data.reduce("") { $0 + $1.ruby })
    }

    /// 学習メモリは確定文字列を「読み全体→表層」の1エントリで記憶するため、よく使う文ほど
    /// 変換1位が単一 DicdataElement の全文候補になり、文節分解の材料（要素境界）が存在しない
    /// (v1.2.0 実機受入で発覚 — settle 確定がさらに学習を強化する自己強化ループ)。
    /// 同じ読みを学習抜き・古典で引き直し、**同一表層**の辞書経路候補から要素列だけを
    /// 再導出する。表層が同一なので確定結果は不変。classic 固定は修復仮説（makeOptions の
    /// forceClassic 注記）と同じ理由 — 使い捨て変換に Zenzai の非決定と推論コストを持ち込まない。
    /// **converterLock 保持中に呼ぶこと**。辞書が同一表層を作れなければ nil（呼び元が settle 劣化）。
    private func dictionaryBoundaryClausesLocked(reading: String, text: String) -> [Candidate]? {
        // learningType は requestCandidates が converter の永続状態へ反映する(vendor の
        // updateIfRequired)ため、.nothing のまま戻ると以後の確定学習と flush が無言 no-op
        // になる(reload の学習OFF切替 :409 と同じ罠 — updateConfig(.nothing) は一時トライを
        // 凍結する)。成功パスは直後の clauseCandidatesLocked が通常 options で復元するが、
        // 失敗パス=settle 劣化はそのまま確定に進むため、ここで必ず復元してから戻る。
        // updateLearningConfig の直呼びは LearningConfig のメンバが internal のため不可 —
        // 通常 options のミニ変換(1文字・nBest 1・classic)が唯一の復元手段。
        defer {
            if learning.enabled {
                var restore = ComposingText()
                restore.insertAtCursorPosition("あ", inputStyle: .direct)
                _ = requestCandidatesLocked(restore, options: makeOptions(nBest: 1, forceClassic: true))
            }
        }
        // 直前 convert のラティスは入力方式が違っても**表層一致で** surface 側が再利用される
        // (vendor differenceSuffix は入力と表層を別々に数える)ため、学習の全文ノードが
        // noLearning でも生き残り 1 位を取り返す — 再導出が本番シナリオ(学習済みの文)でこそ
        // 不発になる。1文字ダミーで previousInputData を潰し、本命をゼロから引かせる。
        // stopComposition でのリセットは Zenzai 稼働中の llama スパイク(bindConverter 注記)で不可。
        var flush = ComposingText()
        flush.insertAtCursorPosition("あ", inputStyle: .direct)
        _ = requestCandidatesLocked(flush, options: makeOptions(nBest: 1, forceClassic: true, noLearning: true))
        var c = ComposingText()
        c.insertAtCursorPosition(reading, inputStyle: .direct)
        let results = requestCandidatesLocked(
            c, options: makeOptions(forceClassic: true, noLearning: true)
        ).mainResults
        for cand in results where cand.text == text
            && Self.covers(cand, targetCount: reading.count)
            && cand.text == cand.data.map(\.word).joined() {
            let clauses = Self.decomposeClauses(cand)
            if clauses.count >= 2 { return clauses }
        }
        return nil
    }

    /// 選択文節の変換候補を用意する。**converterLock 保持中に呼ぶこと**。
    /// reconvert と同じ「使い捨て ComposingText に .direct 挿入して丸ごと requestCandidates」
    /// パターンを文節読みへ適用し、**文節読みを全被覆する候補だけ**を残す（部分被覆候補で
    /// 差し替えると文節境界＝残り読みが壊れるため）。現在表層は必ずリストに含め、その位置を
    /// candidateIndex として返す（候補窓の初期選択＝見えている文節）。
    private func clauseCandidatesLocked(state: ClauseState, leftContext: String?)
        -> (list: [Candidate], index: Int, modelTop: String?) {
        clauseInferenceCountForTesting += 1
        let current = state.clauses[state.selected]
        let reading = Self.clauseReading(current)
        var c = ComposingText()
        c.insertAtCursorPosition(reading, inputStyle: .direct)
        // 左文脈 = 文書の左文脈 + 先行文節の表層（連文節スコアの代替。Zenzai の品質レバー）。
        let preceding = state.clauses[..<state.selected].map { $0.text }.joined()
        let ctx = (leftContext ?? "") + preceding
        let (options, requestedZenzai) = makeOptionsWithZenzaiUsage(leftSideContext: ctx.isEmpty ? nil : ctx)
        let classicOptions = makeOptions(leftSideContext: ctx.isEmpty ? nil : ctx, forceClassic: true)
        let ct0 = DispatchTime.now()
        let results = requestCandidatesWithRuntimeFallbackLocked(
            c, options: options, requestedZenzai: requestedZenzai,
            classicOptions: classicOptions, leftContext: ctx.isEmpty ? nil : ctx).mainResults
        // 実稼働判定は requestCandidates 直後に確定（convert と同じ規律 — silent fallback 除外）。
        let usedZenzai = zenzaiInferenceUsedLocked(requestedZenzai: requestedZenzai,
                                                   input: c.convertTarget)
        let cms = Double(DispatchTime.now().uptimeNanoseconds &- ct0.uptimeNanoseconds) / 1_000_000
        checkZenzaiTooSlowLocked(ms: cms, thresholdMs: zenzaiSlowThresholdMs, usedZenzai: usedZenzai)
        // noteRecordability は呼ばない — 共有マップは reconvert→RecordCorrection の照合専用で、
        // 文節読みで書くと同一読みの reconvert エントリ(別接続)を丸ごと差し替え fail-closed
        // 棄却に落とす(第2R敵対レビュー③)。文節スコープの記録可否は select 時の
        // clauseCorrections が判定し、マップを参照しない(commitClauses の注記)。
        var seen = Set<String>()
        var list: [Candidate] = []
        for cand in results where Self.covers(cand, targetCount: reading.count) {
            guard seen.insert(cand.text).inserted else { continue }
            list.append(cand)
        }
        // モデル1位＝被覆候補の先頭（昇格・現在表層挿入で並びが動く前）。訂正記録の除外基準に
        // 使う（モデル正解の選択を「訂正」にしない — 表示リスト添字では取れない情報）。
        let modelTop = list.first?.text
        // 訂正昇格を convert/reconvert/liveConvert と同じ規律で適用する（適用しないと、直前
        // トラックで根治した「学習が効かない」が文節候補だけ再発する）。昇格候補は合成でも
        // ruby=読み全体なので全被覆＝文節境界は壊れない。
        if let promotedList = promoted(list, composing: c) {
            if promotedList.first?.text != list.first?.text {
                engineLog("ev=correction_promote kind=clause reading_chars=\(reading.count)\n")
            }
            list = promotedList
        }
        if let idx = list.firstIndex(where: { $0.text == current.text }) {
            // 現在表層と同名の候補は分解由来の Candidate で差し替える（文全体の lattice 由来の
            // data/composingCount を保つ — 確定学習の素材を候補窓経由の往復で劣化させない）。
            list[idx] = current
            return (list, idx, modelTop)
        }
        list.insert(current, at: 0)
        return (list, 0, modelTop)
    }

    private func makeClauseView(_ state: ClauseState) -> ClauseView {
        ClauseView(
            segments: state.clauses.map { $0.text },
            selected: state.selected,
            candidates: state.candidates.map { $0.text },
            candidateIndex: state.candidateIndex
        )
    }

    /// 文節ナビゲーション（変換中の←/→）。文節状態が無ければ `baseIndex`（直前 convert 系の
    /// 候補添字＝TIP の現在選択）を種に開始し、選択文節を `offset` だけ動かす（端はクランプ）。
    /// 種は baseIndex の候補**そのもの**だけ。非被覆（前方一致・修復候補）なら nil で、
    /// TIP は従来の「確定して畳む」へ劣化する — かつては最初の被覆候補へ乗り換えていたが、
    /// Tab の修復候補や前方一致を選んだ状態の ←/→ が preedit をユーザーの選択と別の変換へ
    /// 黙って差し替えていた（マージ後敵対レビュー①）。分解が 1 文節の種（学習の全文1エントリ・
    /// 訂正昇格の合成候補）は同一表層の辞書境界を再導出して開始を試み、再導出も 1 文節
    /// （真の短文）か辞書に無い表層なら nil。nil は他に **未知セッション / 候補キャッシュ無し /
    /// stale** のとき。
    public func moveClause(session: Int, offset: Int, baseIndex: Int, leftContext: String? = nil) -> ClauseView? {
        // 種拒否は仕様上の劣化(settle)だが、無言 nil だと実機で6経路のどれかを切り分けられない
        // (v1.2.0 受入で実証) — 全経路に reason ログを置く。
        guard var rec = sessions[session] else {
            engineLog("ev=clause_seed_reject reason=no_session session=\(session)\n"); return nil }
        converterLock.lock()
        defer { converterLock.unlock() }
        bindConverter(to: session)
        if rec.clauseState == nil {
            guard let cands = rec.cachedCandidates, !cands.isEmpty else {
                engineLog("ev=clause_seed_reject reason=no_cache\n"); return nil }
            guard rec.cachedTarget == rec.composing.convertTarget else {
                engineLog("ev=clause_seed_reject reason=stale_target cached_chars=\(rec.cachedTarget?.count ?? 0) now_chars=\(rec.composing.convertTarget.count)\n"); return nil }
            guard baseIndex >= 0, baseIndex < cands.count else {
                engineLog("ev=clause_seed_reject reason=index_range base=\(baseIndex) n=\(cands.count)\n"); return nil }
            // 修復候補は縮約仮説の読みを覆う＝literal の読みとは別物。covers は文字数比較なので
            // 同数になる仮説が将来増えると素通りする — index 集合で明示的に弾く。
            guard rec.typoRepairedIndices?.contains(baseIndex) != true else {
                engineLog("ev=clause_seed_reject reason=typo_repaired base=\(baseIndex)\n"); return nil }
            let targetCount = rec.composing.convertTarget.count
            guard Self.covers(cands[baseIndex], targetCount: targetCount) else {
                engineLog("ev=clause_seed_reject reason=not_covering base=\(baseIndex) target=\(targetCount)\n"); return nil }
            // 日付テンプレート候補は parseTemplate が text だけを実日付へ書き換え、data.word は
            // 生タグのまま残る(vendor Candidate.swift の makePrefixClauseCandidate 注記)。分解は
            // data.word から表層を再構成するため、不整合候補を種にすると preedit/確定/学習が
            // 生タグ `<date …>` へ化ける — 種にせず settle 劣化に落とす。
            guard cands[baseIndex].text == cands[baseIndex].data.map(\.word).joined() else {
                engineLog("ev=clause_seed_reject reason=text_data_mismatch text_chars=\(cands[baseIndex].text.count) joined_chars=\(cands[baseIndex].data.map(\.word).joined().count)\n"); return nil }
            var clauses = Self.decomposeClauses(cands[baseIndex])
            // 1 文節は移動先が無い。ただし単一要素の合成候補（学習の全文1エントリ・訂正昇格）は
            // 「境界情報が無い」だけで文としては複数文節であり得るため、辞書境界の再導出を先に
            // 試みる。真の1文節（短い読み）と辞書が再現できない表層だけを settle 劣化に落とす。
            if clauses.count < 2 {
                let elems = cands[baseIndex].data.count
                guard let derived = dictionaryBoundaryClausesLocked(
                    reading: rec.composing.convertTarget, text: cands[baseIndex].text) else {
                    engineLog("ev=clause_seed_reject reason=single_clause elems=\(elems)\n"); return nil }
                engineLog("ev=clause_seed_rederive elems=\(elems) clauses=\(derived.count)\n")
                clauses = derived
            }
            var st = ClauseState(clauses: clauses, selected: 0,
                                 originalTexts: clauses.map { $0.text })
            if baseIndex != 0, cands[baseIndex].text == rec.cachedModelTop {
                // モデル1位を選んだまま文節モードへ入った(昇格発火時は baseIndex 1 がモデル1位)。
                // 触らず確定されたら commit() のモデル1位選択と同じ un-learn — index だけでは
                // 「1位拒否」と区別できないため表層基準(cachedModelTop)で分岐する。
                // sentenceCorrection 側へは落とさない(モデル1位の記録は基準線違反)。
                // 発火条件(cachedPromoted/isLearningTarget)は commit() の un-learn 注記と同じ。
                if rec.cachedPromoted, cands[baseIndex].isLearningTarget {
                    st.sentenceUnlearn = (reading: rec.composing.convertTarget,
                                          surface: cands[baseIndex].text)
                }
            } else if baseIndex != 0, cands[baseIndex].isLearningTarget {
                // commit() の記録条件のうち全消費は covers（上の guard）、非修復は修復 guard で
                // 既に保証済み。ここで事実だけ保存し、記録可否は確定時に判断する。
                st.sentenceCorrection = (reading: rec.composing.convertTarget,
                                         surface: cands[baseIndex].text)
            }
            rec.clauseState = st
        }
        var state = rec.clauseState!
        let moved = min(max(state.selected + offset, 0), state.clauses.count - 1)
        if moved == state.selected, !state.candidates.isEmpty {
            // 端クランプ（押しっぱなしの大半）で再推論・候補窓再構築をしない — OnKeyDown は
            // この応答を同期 IPC で待つため、動かない矢印の変換1回は IME ブロックに直結する。
            return makeClauseView(state)
        }
        state.selected = moved
        if let (cachedList, cachedIndex) = state.candidateCache[moved] {
            state.candidates = cachedList
            state.candidateIndex = cachedIndex
        } else {
            let (candList, candIndex, modelTop) = clauseCandidatesLocked(state: state, leftContext: leftContext)
            state.candidates = candList
            state.candidateIndex = candIndex
            state.modelTopTexts[moved] = modelTop
            state.candidateCache[moved] = (candList, candIndex)
        }
        rec.clauseState = state
        sessions[session] = rec
        engineLog("ev=clause_move sel=\(state.selected) n=\(state.clauses.count) cands=\(state.candidates.count)\n")
        return makeClauseView(state)
    }

    /// 文節ナビゲーション中: 選択文節の表層を候補 `index` へ差し替える。候補は全被覆のみなので
    /// 読み＝文節境界は変わらない。nil は **未知セッション / 文節状態なし / index 範囲外** のとき。
    public func selectClauseCandidate(session: Int, index: Int) -> ClauseView? {
        guard var rec = sessions[session], var state = rec.clauseState,
              index >= 0, index < state.candidates.count else { return nil }
        state.clauses[state.selected] = state.candidates[index]
        state.candidateIndex = index
        // 訂正 = 分解時に見えていた表層でもモデル1位でもない表層への明示変更（判定基準の理由は
        // ClauseState.clauseCorrections の注記）。モデル1位の除外は fail-closed 側に倒す —
        // 記録漏れは無害、誤記録は文脈依存語の恒久固定と既存訂正の上書き破壊。
        let chosenText = state.candidates[index].text
        state.clauseCorrections[state.selected] =
            chosenText != state.originalTexts[state.selected]
            && chosenText != state.modelTopTexts[state.selected]
        // 後続文節の候補キャッシュは左文脈（先行文節表層）ごと無効になるため捨てる。先行文節と
        // 選択文節自身は候補列が変わらない（自身は初期選択だけ差し替える）ので保持する。
        state.candidateCache = state.candidateCache.filter { $0.key <= state.selected }
        state.candidateCache[state.selected] = (state.candidates, index)
        rec.clauseState = state
        sessions[session] = rec
        engineLog("ev=clause_select sel=\(state.selected) index=\(index)\n")
        return makeClauseView(state)
    }

    /// 文節ナビゲーション中の確定。全文節の表層を連結して返し、文節ごとに setCompletedData→学習へ
    /// 乗せる（liveConvert の先頭文節自動確定と同順・全文節版）。1 位以外を明示選択した文節は
    /// 訂正として CorrectionStore へも記録する（commit の spec §2(a) と同じ規律の文節スコープ版）。
    /// 読みは全消費なので reading=""。
    /// composingCount による prefixComplete は使わない — 分解由来の文節（先頭以外）は
    /// composingCount が入力列と対応しない（LiveConversionState の注記と同根）ため、読み全体の
    /// 消費は ComposingText の作り直しで表す。nil は **未知セッション / 文節状態なし** のとき
    /// （TIP は表示中ビューの連結を直確定する劣化へ落ちる）。
    public func commitClauses(session: Int) -> (text: String, reading: String)? {
        guard var rec = sessions[session], let state = rec.clauseState else { return nil }
        converterLock.lock()
        defer { converterLock.unlock() }
        bindConverter(to: session)
        var text = ""
        var recorded = false
        for (i, clause) in state.clauses.enumerated() {
            setCompletedDataLocked(clause, session: session)
            // isLearningTarget ガードは commit() の注記と同じ — 文節候補窓は日付テンプレート
            // 候補(isLearningTarget=false・data.word=生タグ)を選べるため、ここを素通しすると
            // lastData 経由で次文節の bigram 左要素に生タグが乗る(第2R敵対レビュー④)。
            // skip の代償: lastData がテンプレート直前の値のまま残り、次の学習ペアが
            // テンプレートを跨いだ組になる。vendor に lastData クリア API が無く
            // (stopComposition は zenz reset のスパイク)、生タグ恒久化よりは軽微な側に倒す。
            if learning.enabled && clause.isLearningTarget {
                updateLearningDataLocked(clause, session: session)
            }
            text += clause.text
            // commit(spec §2(a)) の文節スコープ版: 判定は select 時に表層基準で済ませてある
            // （clauseCorrections の注記参照）。全消費条件は文節候補が全被覆のみ
            // （clauseCandidatesLocked）なので構造的に満たされる。記録可否マップ照合は使わない —
            // あれは表層が IPC 経由で out-of-band に届く RecordCorrection 用で、ここは選択の
            // 出所（SelectClauseCandidate）がエンジン内で自明。
            if learning.enabled, state.clauseCorrections[i] == true, clause.isLearningTarget {
                corrections.record(reading: Self.clauseReading(clause), surface: clause.text)
                recorded = true
                engineLog("ev=correction_record source=clause\n")
            }
        }
        // 種が文候補窓の 1 位拒否だった場合、文節候補に触れないまま（＝連結が種の表層のまま）
        // 確定されたら commit() が行うはずだった文レベル訂正を落とさない。文節を差し替えて
        // いれば連結が変わるので走らない（文節スコープの記録が引き継ぐ）。
        if learning.enabled, let sc = state.sentenceCorrection, text == sc.surface {
            corrections.record(reading: sc.reading, surface: sc.surface)
            recorded = true
            engineLog("ev=correction_record source=clause_seed\n")
        }
        // モデル1位種のまま（＝連結が種の表層のまま）の確定は commit() のモデル1位選択と同じ
        // un-learn。文節を差し替えていれば連結が変わり走らない（sentenceCorrection と対）。
        if learning.enabled, let su = state.sentenceUnlearn, text == su.surface,
           corrections.remove(reading: su.reading) {
            recorded = true
            engineLog("ev=correction_unlearn source=clause_seed\n")
        }
        if recorded { enqueueCorrectionPersistenceLocked() }
        rec.composing = ComposingText()
        rec.invalidateCandidateCache()   // clauseState もここで消える
        rec.liveState = nil
        sessions[session] = rec
        engineLog("ev=clause_commit n=\(state.clauses.count)\n")
        return (text, "")
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
        // session id は単調増加で再利用されないため、次に別 id が converter を使うとき必ず切替扱いになる
        // （古典モードではリセット、Zenzai 実稼働中は audit H2 のスキップ — bindConverter の注記参照）。
        // 全セッションが消えた場合だけ下で proactively リセットする。
        //
        // 合成が1つも残っていなければ converter の合成状態をリセットする。commit() が
        // setCompletedData で残す completedData（および previousInputData/lattice/zenz セッション）は
        // converter 共有なので、確定でセッションを終えた後も残ると **次の独立セッションの変換へ漏れる**
        // （例: nihongo→日本語 を全確定→次に go を打つと afterComplete 経路で日本語が左文脈に混ざる）。
        // 部分確定はセッションを保持し endSession を呼ばないので、残り読みの変換では completedData が
        // 正しく左文脈として効く（リセットされない）。
        guard sessions.isEmpty else { return }
        let endedSession = session
        maintenance.submit(label: "end_session") { [weak self] in
            guard let self else { return }
            self.converterLock.lock()
            defer { self.converterLock.unlock() }
            // A newer session may have used the shared converter before this deferred job ran.
            // Its bind already performed the required old-session reset; resetting again here
            // would erase the new composition.
            guard self.activeConverterSession == endedSession else { return }
            // Legacy mutable-session callers have no apply-receipt persistence lane. Preserve
            // their durability in the background; production snapshots use learningConverter.
            if self.processRole != .mainClassicOnly { self.flushLearningLocked() }
            if self.config.weightURL != nil { engineLog("ev=llama_reset reason=all_end\n") }
            self.stopCompositionLocked()
            self.activeConverterSession = nil
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
        if let ids = connectionSessions[connection] {
            // ids は Set の値コピー（値意味論）。endSession が内部で connectionSessions[connection] を
            // 変更しても、このループ対象は不変。
            for id in ids { endSession(session: id) }
            connectionSessions[connection] = nil   // 念のため（通常は最後の endSession が nil 済み）
        }
        converterLock.lock()
        snapshotAutoCommitStates = snapshotAutoCommitStates.filter { $0.key.connection != connection }
        appliedSnapshotAutoCommitReceipts = appliedSnapshotAutoCommitReceipts.filter {
            $0.key.connection != connection
        }
        converterLock.unlock()
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
        if processRole == .mainClassicOnly {
            guard zenzaiEnabled else { return }
            gpuWorkerSupervisor?.startWarmUp()
            return
        }
        startWarmUp(explicitRetry: false)
    }

    func flushMaintenanceForTesting() {
        maintenance.flushForTesting()
    }

    func beginMaintenanceHoldForTesting() -> @Sendable () -> Void {
        let started = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        maintenance.submit(label: "test_hold") {
            started.signal()
            release.wait()
        }
        started.wait()
        return { release.signal() }
    }

    var recentLearningCountForTesting: Int { recentLearning.count }

    var snapshotReceiptLedgerCountsForTesting: (pending: Int, applied: Int) {
        converterLock.lock()
        defer { converterLock.unlock() }
        return (snapshotAutoCommitStates.count, appliedSnapshotAutoCommitReceipts.count)
    }

    static func makeSnapshotComposing(_ segments: [SnapshotSegment]) -> ComposingText {
        var composing = ComposingText()
        for segment in segments {
            composing.insertAtCursorPosition(
                segment.text,
                inputStyle: segment.style == "direct" ? .direct : .roman2kana)
        }
        return composing
    }

    func snapshot(_ segments: [SnapshotSegment], explicit: Bool, leftContext: String? = nil,
                  enhancementKey: SnapshotEnhancementKey? = nil, snapshotConnection: Int = 0)
        -> (text: String, reading: String, candidates: [String]?, candidateRemaining: [String]?, baseline: UInt64,
            autoCommit: SnapshotAutoCommitProposal?)
    {
        let composing = Self.makeSnapshotComposing(segments)
        converterLock.lock()
        defer { converterLock.unlock() }
        stopCompositionLocked()
        let options = makeOptions(nBest: explicit ? 10 : 1, leftSideContext: leftContext, forceClassic: true)
        var classic = requestCandidatesLocked(composing, options: options)
        let reading = composing.convertTarget
        let ranked = recentLearning.rank(
            mainResults: classic.mainResults, firstClauseResults: classic.firstClauseResults,
            composing: composing)
        classic.mainResults = ranked.main
        classic.firstClauseResults = ranked.firstClause
        let results = classic.mainResults
        let baseline = Self.snapshotBaselineDigest(explicit: explicit, reading: reading, candidates: results)
        let whole = results.filter {
            $0.data.reduce(0) { $0 + $1.ruby.count } == reading.count
        }.map(\.text)
        let display = whole.first ?? reading
        engineLog("ev=infer kind=\(explicit ? "explicit" : "live")_snapshot target_chars=\(reading.count)\n")
        guard explicit else {
            let liveCandidate = results.first(where: {
                $0.data.reduce(0) { $0 + $1.ruby.count } == reading.count
            }) ?? Candidate(
                text: reading, value: 0,
                composingCount: .inputCount(composing.input.count),
                lastMid: MIDData.一般.mid,
                data: [DicdataElement(
                    ruby: Self.toKatakana(reading), cid: CIDData.一般名詞.cid,
                    mid: MIDData.一般.mid, value: 0)])
            let proposal = snapshotAutoCommitProposalLocked(
                key: enhancementKey, connection: snapshotConnection,
                composing: composing, candidate: liveCandidate,
                firstClauseCandidates: classic.firstClauseResults, reading: reading)
            if let enhancementKey {
                enqueueSnapshotEnhancement(SnapshotEnhancementWork(
                    key: enhancementKey, baseline: baseline, explicit: false, reading: reading,
                    classic: classic, snapshot: GPUWorkerCompositionSnapshot(composing),
                    leftContext: leftContext, inferenceLimit: config.inferenceLimit))
            }
            let proposalDisplay: String
            if let proposal {
                let remainder = display.hasPrefix(proposal.text)
                    ? String(display.dropFirst(proposal.text.count)) : proposal.remaining
                proposalDisplay = remainder.isEmpty ? proposal.remaining : remainder
            } else {
                proposalDisplay = display.isEmpty ? reading : display
            }
            return (proposalDisplay, reading, nil, nil, baseline, proposal)
        }
        let candidates = results.map(\.text)
        let remaining = results.map { candidate in
            let consumed = min(reading.count, candidate.data.reduce(0) { $0 + $1.ruby.count })
            return String(reading.dropFirst(consumed))
        }
        if let enhancementKey {
            enqueueSnapshotEnhancement(SnapshotEnhancementWork(
                key: enhancementKey, baseline: baseline, explicit: true, reading: reading,
                classic: classic, snapshot: GPUWorkerCompositionSnapshot(composing),
                leftContext: leftContext, inferenceLimit: config.inferenceLimit))
        }
        return (display.isEmpty ? reading : display, reading, candidates, remaining, baseline, nil)
    }

    private func snapshotAutoCommitProposalLocked(
        key: SnapshotEnhancementKey?, connection: Int, composing: ComposingText, candidate: Candidate,
        firstClauseCandidates: [Candidate], reading: String
    ) -> SnapshotAutoCommitProposal? {
        guard let key, let threshold = autoCommit.threshold, !reading.isEmpty else { return nil }
        let stream = SnapshotAutoCommitStream(connection: connection, composition: key.composition)
        if snapshotAutoCommitStates[stream] == nil {
            let replaced = snapshotAutoCommitStates.keys.filter {
                $0.connection == connection && $0 != stream
            }
            for key in replaced {
                snapshotAutoCommitStates[key] = nil
                appliedSnapshotAutoCommitReceipts[key] = nil
            }
            if snapshotAutoCommitStates.count >= snapshotAutoCommitStateLimit,
               let oldest = snapshotAutoCommitStates.keys.min(by: {
                   ($0.connection, $0.composition) < ($1.connection, $1.composition)
               }) {
                snapshotAutoCommitStates[oldest] = nil
                appliedSnapshotAutoCommitReceipts[oldest] = nil
            }
        }
        var state = snapshotAutoCommitStates[stream] ?? SnapshotAutoCommitState()
        if let last = state.lastRevision, key.revision < last { return nil }
        if let pending = state.pending {
            if pending.key == key { return pending.value }
            if key.revision <= pending.key.revision { return nil }
            state.pending = nil
        }
        if state.lastRevision == key.revision {
            return nil
        }
        state.lastRevision = key.revision
        state.live.update(candidate: candidate, firstClauseCandidates: firstClauseCandidates)
        var prefix = state.live.candidateForCompleteFirstClause(threshold: threshold)
        if prefix == nil, autoCommitMaxReading > 0, reading.count > autoCommitMaxReading {
            prefix = firstClauseCandidates.first
        }
        guard let prefix else {
            snapshotAutoCommitStates[stream] = state
            return nil
        }
        var remainder = composing
        remainder.prefixComplete(composingCount: prefix.composingCount)
        let remaining = remainder.convertTarget
        guard !remaining.isEmpty, remaining.count < reading.count else {
            snapshotAutoCommitStates[stream] = state
            return nil
        }
        let consumed = String(reading.dropLast(remaining.count))
        nextSnapshotAutoCommitProposal &+= 1
        let proposal = SnapshotAutoCommitProposal(
            proposal: nextSnapshotAutoCommitProposal, text: prefix.text,
            consumedReading: consumed, remaining: remaining)
        state.pending = (key, proposal, prefix)
        snapshotAutoCommitStates[stream] = state
        return proposal
    }

    func applySnapshotAutoCommitReceipt(connection: Int, key: SnapshotEnhancementKey,
                                        proposal: UInt64) -> Bool {
        converterLock.lock()
        let stream = SnapshotAutoCommitStream(connection: connection, composition: key.composition)
        if let applied = appliedSnapshotAutoCommitReceipts[stream], applied.0 == key,
           applied.1 == proposal {
            converterLock.unlock()
            return true
        }
        guard var state = snapshotAutoCommitStates[stream],
              state.pending?.key == key,
              state.pending?.value.proposal == proposal else {
            converterLock.unlock()
            return false
        }
        let candidate = state.pending!.candidate
        state.live.didCompleteFirstClause()
        state.pending = nil
        snapshotAutoCommitStates[stream] = state
        appliedSnapshotAutoCommitReceipts[stream] = (key, proposal)
        if learning.enabled && candidate.isLearningTarget {
            recentLearning.record(candidate)
            enqueueLearningPersistence(candidate: candidate,
                                       options: makeOptions(nBest: 1, forceClassic: true))
        }
        converterLock.unlock()
        return true
    }

    private func enqueueLearningPersistence(candidate: Candidate, options: ConvertRequestOptions) {
        learningStateLock.lock()
        guard !clearingLearning else {
            learningStateLock.unlock()
            return
        }
        let generation = learningGeneration
        learningStateLock.unlock()
        maintenance.submit(label: "learning") { [weak self] in
            guard let self else { return }
            self.learningStateLock.lock()
            let current = !self.clearingLearning && self.learningGeneration == generation
            self.learningStateLock.unlock()
            guard current else { return }
            if let hook = self.learningPersistenceForTesting {
                hook(candidate)
                return
            }
            var composing = ComposingText()
            let reading = candidate.data.map(\.ruby).joined()
            guard !reading.isEmpty else { return }
            composing.insertAtCursorPosition(reading, inputStyle: .direct)
            // Snapshot conversion deliberately has no cross-request composition context. Reset
            // the persistence converter too, or receipts from different apps could form a bigram.
            self.learningConverter.stopComposition()
            _ = self.learningConverter.requestCandidates(composing, options: options)
            self.learningConverter.updateLearningData(candidate)
            self.learningConverter.commitUpdateLearningData()
        }
    }

    /// Capture immutable bytes while holding converterLock, then perform filesystem I/O on the
    /// bounded maintenance lane. A later mutation advances the generation, so completion of an
    /// older write cannot incorrectly clear the newer dirty state.
    private func enqueueCorrectionPersistenceLocked() {
        maintenance.submitLatest(label: "correction") { [weak self] in
            guard let self else { return }
            for _ in 0..<3 {
                self.converterLock.lock()
                let store = self.corrections
                let snapshot = store.persistenceSnapshot()
                self.converterLock.unlock()
                guard let snapshot else { return }
                guard CorrectionStore.persist(snapshot) else { continue }
                self.converterLock.lock()
                store.acknowledgePersistence(snapshot)
                self.converterLock.unlock()
                return
            }
            throw CorrectionPersistenceError.failed
        }
    }

    private enum CorrectionPersistenceError: Error { case failed }
    private enum DictionaryReloadError: Error { case failed }

    func pollSnapshotEnhancement(key: SnapshotEnhancementKey, baseline: UInt64)
        -> SnapshotEnhancementPoll
    {
        snapshotEnhancementLock.lock()
        defer { snapshotEnhancementLock.unlock() }
        guard latestSnapshotEnhancement?.0 == key,
              latestSnapshotEnhancement?.1 == baseline else { return .unavailable }
        guard let completed = completedSnapshotEnhancement,
              completed.key == key, completed.baseline == baseline else { return .pending }
        completedSnapshotEnhancement = nil
        return completed.result
    }

    private func enqueueSnapshotEnhancement(_ work: SnapshotEnhancementWork) {
        guard processRole == .mainClassicOnly, zenzaiEnabled,
              gpuWorkerSupervisor != nil else { return }
        snapshotEnhancementLock.lock()
        latestSnapshotEnhancement = (work.key, work.baseline)
        completedSnapshotEnhancement = nil
        desiredSnapshotEnhancement = work
        guard !snapshotEnhancementRunning else {
            snapshotEnhancementLock.unlock()
            return
        }
        snapshotEnhancementRunning = true
        snapshotEnhancementLock.unlock()
        Thread.detachNewThread { [weak self] in self?.runSnapshotEnhancements() }
    }

    private func runSnapshotEnhancements() {
        while true {
            snapshotEnhancementLock.lock()
            guard let work = desiredSnapshotEnhancement else {
                snapshotEnhancementRunning = false
                snapshotEnhancementLock.unlock()
                return
            }
            desiredSnapshotEnhancement = nil
            snapshotEnhancementLock.unlock()

            let result = evaluateSnapshotEnhancement(work)
            snapshotEnhancementLock.lock()
            if latestSnapshotEnhancement?.0 == work.key,
               latestSnapshotEnhancement?.1 == work.baseline {
                completedSnapshotEnhancement = CompletedSnapshotEnhancement(
                    key: work.key, baseline: work.baseline, result: result)
            }
            snapshotEnhancementLock.unlock()
        }
    }

    private func evaluateSnapshotEnhancement(_ work: SnapshotEnhancementWork)
        -> SnapshotEnhancementPoll
    {
        guard let gpuWorkerSupervisor else { return .unavailable }
        let decision = gpuWorkerSupervisor.rerank(
            classic: work.classic, snapshot: work.snapshot, leftContext: work.leftContext,
            nBest: work.explicit ? 10 : 1, inferenceLimit: work.inferenceLimit,
            deadline: work.explicit ? GPUWorkerDeadlineTier.convert.workerBudget
                                    : GPUWorkerDeadlineTier.live.workerBudget)
        guard decision.usedWorker, decision.failure == nil,
              Self.isSnapshotEnhancement(decision.conversion.mainResults,
                                         of: work.classic.mainResults) else {
            return .unavailable
        }
        let enhanced = decision.conversion.mainResults
        let whole = enhanced.filter { Self.consumedReading(of: $0) == work.reading.count }
        let display = whole.first?.text ?? work.reading
        guard work.explicit else {
            return .ready(text: display.isEmpty ? work.reading : display,
                          candidates: nil, candidateRemaining: nil)
        }
        return .ready(
            text: display.isEmpty ? work.reading : display,
            candidates: enhanced.map(\.text),
            candidateRemaining: enhanced.map {
                String(work.reading.dropFirst(min(work.reading.count, Self.consumedReading(of: $0))))
            })
    }

    static func isSnapshotEnhancement(_ enhanced: [Candidate], of classic: [Candidate]) -> Bool {
        guard !enhanced.isEmpty, enhanced.count == classic.count else { return false }
        var remaining: [SnapshotCandidateIdentity: Int] = [:]
        for candidate in classic {
            let identity = SnapshotCandidateIdentity(
                text: candidate.text, consumedReading: consumedReading(of: candidate))
            remaining[identity, default: 0] += 1
        }
        for candidate in enhanced {
            let identity = SnapshotCandidateIdentity(
                text: candidate.text, consumedReading: consumedReading(of: candidate))
            guard let count = remaining[identity], count > 0 else { return false }
            remaining[identity] = count - 1
        }
        return true
    }

    private static func consumedReading(of candidate: Candidate) -> Int {
        candidate.data.reduce(0) { $0 + $1.ruby.count }
    }

    static func snapshotBaselineDigest(explicit: Bool, reading: String,
                                       candidates: [Candidate]) -> UInt64 {
        var hash: UInt64 = 14_695_981_039_346_656_037
        func append(_ bytes: some Sequence<UInt8>) {
            for byte in bytes { hash = (hash ^ UInt64(byte)) &* 1_099_511_628_211 }
        }
        append([explicit ? 1 : 0])
        for field in [reading] + candidates.flatMap({ [$0.text, String(consumedReading(of: $0))] }) {
            var length = UInt64(field.utf8.count).littleEndian
            withUnsafeBytes(of: &length) { append($0) }
            append(field.utf8)
        }
        return hash
    }

    /// Explicit retry is the only non-model-change operation that clears a runtime latch.
    public func retryZenzai() {
        if processRole == .mainClassicOnly {
            guard zenzaiEnabled else { return }
            gpuWorkerSupervisor?.explicitRetry()
            gpuWorkerSupervisor?.startWarmUp()
            return
        }
        startWarmUp(explicitRetry: true)
    }

    private func startWarmUp(explicitRetry: Bool) {
        converterLock.lock()
        guard !warmUpInFlight else {
            converterLock.unlock()
            return
        }
        if explicitRetry {
            zenzaiTooSlow = false
            setRuntimeStatusLocked(.unconfigured)
            if config.weightURL != nil {
                setRuntimeStateLocked(.classic(reason: .notStarted))
                zenzaiReady = false
            }
        }
        guard zenzaiEnabled else {
            setClassicLocked(Self.classicReason(for: config))
            converterLock.unlock()
            return
        }
        if !explicitRetry,
           case .classic(let reason) = _zenzaiRuntimeState,
           reason != .notStarted {
            converterLock.unlock()
            return
        }
        warmUpInFlight = true
        markWarmUpStarted()
        guard beginRuntimeProbeLocked(explicitRetry: explicitRetry) else {
            warmUpInFlight = false
            markWarmUpFinished()
            converterLock.unlock()
            return
        }
        converterLock.unlock()
        Thread.detachNewThread { [weak self] in self?.warmUp() }
    }

    private func warmUp() {
        let t0 = DispatchTime.now()
        converterLock.lock()
        if takeWarmUpCancellationRequest() {
            warmUpInFlight = false
            markWarmUpFinished()
            converterLock.unlock()
            return
        }
        defer {
            warmUpInFlight = false
            markWarmUpFinished()
            converterLock.unlock()
        }
        var dummy = ComposingText()
        dummy.insertAtCursorPosition("tesuto", inputStyle: .roman2kana)
        let options = makeOptions(forceZenzai: true)
        let classicOptions = makeOptions(forceClassic: true)
        _ = requestCandidatesWithRuntimeFallbackLocked(
            dummy, options: options, requestedZenzai: true,
            classicOptions: classicOptions)
        let status = zenzaiRuntime.status()
        setRuntimeStatusLocked(status)
        guard status.state == .gpuActive, status.failure == .none,
              status.decodeAttempts > warmupDecodeAttemptsBaseline,
              case .warming = _zenzaiRuntimeState else {
            if case .classic = _zenzaiRuntimeState {
                zenzaiReady = true
            } else {
                setClassicLocked(status.classicReason ?? .warmupFailed)
            }
            let ms = Double(DispatchTime.now().uptimeNanoseconds &- t0.uptimeNanoseconds) / 1_000_000
            engineLog("ev=coldstart stage=warmup_failed ms=\(String(format: "%.1f", ms))\n")
            return
        }
        let device = status.device.isEmpty ? "unknown" : status.device
        setRuntimeStateLocked(.gpuActive(device: device))
        zenzaiReady = true
        engineLog("ev=zenzai_gpu_active device=\(device) decode_attempts=\(status.decodeAttempts)\n")
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

    /// Zenzai 推論の重さを監視し、閾値を超えたら zenzaiTooSlow=true で古典へ固定する。
    /// **converterLock 保持中に呼ぶこと**（slowWatchSkipsRemaining の読み書きが lock 内のため）。
    /// usedZenzai: ms が**実際に Zenzai 推論として走った** requestCandidates の計測か
    /// （zenzaiInferenceUsedLocked — .on 要求 × 対象入力非空 × ロード成功。要求だけでは不十分:
    /// invalid/nonexistent weight の silent fallback 中は実推論が走っていない）。
    /// false（古典変換・ウォームアップ待ち・forceClassic・silent fallback・空入力・
    /// マージ/昇格/キャッシュ/自動確定の後段処理）は Zenzai の重さと無関係なので、
    /// **skip の消費も tooSlow 化もしない** — ガードは skip 消費の前に置く。誤消費は
    /// 「Zenzai が一度も走らないまま skip が尽き、最初の実推論が cold spike として即 disable」
    /// を招く（High: 計測範囲を推論のみに限定して解消）。
    /// 初回（slowWatchSkipsRemaining で指定）は cold spike 誤判定防止のためスキップする。
    /// zenzaiTooSlow の setter が zenzaiTooSlowLock を取るが、これは他ロックを取らない独立ロック
    /// （zenzaiReadyLock と同型）なので converterLock 保持下からの呼出は安全。
    /// thresholdMs: op別のTIP側IPCタイムアウトに合わせた閾値（convert系=800ms, liveConvert=300ms）。
    /// 遅延検知で classic へ切り替わったかを返す。reset が必要なのは、vendor 内に残る
    /// completedData/lastData のどちらかが現在とは別のセッション由来のときだけ。同一セッションの
    /// 部分確定文脈まで無条件に消すと、次の classic 変換が afterComplete を使えなくなる。
    @discardableResult
    private func checkZenzaiTooSlowLocked(ms: Double, thresholdMs: Double, usedZenzai: Bool) -> Bool {
        guard usedZenzai, !zenzaiTooSlow else { return false }
        if slowWatchSkipsRemaining > 0 {
            slowWatchSkipsRemaining -= 1
            return false
        }
        if ms > thresholdMs {
            zenzaiTooSlow = true
            // 現在の requestCandidates の結果処理は完了させ、次の converter 入口で一度だけ
            // 別セッション由来の classic 文脈だけを破棄する。同一セッションまたは未確定なら、
            // 現要求が更新した previousInputData/lattice をそのまま classic 継続へ渡す。
            needsClassicReset = Self.requiresClassicReset(
                activeSession: activeConverterSession,
                completedDataSession: completedDataSession,
                learningDataSession: learningDataSession)
            engineLog("ev=zenzai_disabled reason=slow_inference ms=\(String(format: "%.1f", ms)) threshold=\(String(format: "%.0f", thresholdMs))\n")
            return true
        }
        return false
    }

    /// Zenzai→classic 切替時に、classic 専用文脈が別セッション由来かを判定する純関数。
    static func requiresClassicReset(activeSession: Int?, completedDataSession: Int?,
                                     learningDataSession: Int?) -> Bool {
        [completedDataSession, learningDataSession].contains { owner in
            guard let owner else { return false }
            return owner != activeSession
        }
    }

    static func shouldResetForSessionSwitch(isZenzaiOperational: Bool) -> Bool {
        !isZenzaiOperational
    }

    /// reload 時に初回スキップを復活させるべきか（Zenzai 新規有効化）を判定する純関数。
    /// 新規有効化（weightURL が nil→非nil）ではモデルが未ロードで、初回 convert がインライン
    /// モデルロード＋初回推論（KV冷え）で本質的に遅くなる。この cold spike を吸収するためスキップを復活。
    /// テストから直接検証可能（fileExists 制約を受けない）。
    static func shouldRestoreSkipOnReload(old: URL?, new: URL?) -> Bool {
        old == nil && new != nil
    }

    /// テスト専用: slowWatchSkipsRemaining の直接観測（skip 消費の陰性検証用 — 間接観測だと
    /// 「消費したのが誰か」が分からない）。本番呼び出し元と同じ converterLock 下で読む
    /// （checkZenzaiTooSlowLocked の読み書きと直列化）。
    var zenzaiSlowWatchSkipsRemainingForTesting: Int {
        converterLock.lock()
        defer { converterLock.unlock() }
        return slowWatchSkipsRemaining
    }

    /// テスト専用: 遅延フォールバックのリセット予約と、実際の stopComposition 回数を同じ
    /// converterLock 規律で観測する。
    var classicResetStateForTesting: (pending: Bool, count: Int) {
        converterLock.lock()
        defer { converterLock.unlock() }
        return (needsClassicReset, compositionResetCount)
    }

    /// テスト専用: vendor の private な completedData/lastData を直接読めないため、対応する
    /// 所有者メタデータだけを注入し、fallback の境界判定を決定的に検証する。
    func setClassicContextOwnersForTesting(completed: Int?, learning: Int?) {
        converterLock.lock()
        defer { converterLock.unlock() }
        completedDataSession = completed
        learningDataSession = learning
    }

    func setClassicResetPendingForTesting() {
        converterLock.lock()
        defer { converterLock.unlock() }
        needsClassicReset = true
    }

    /// テスト専用: converterLock を取った上で checkZenzaiTooSlowLocked を呼ぶ（本番の convert/liveConvert
    /// が converterLock 内で呼ぶのと同じ規律を再現）。private(set) の zenzaiTooSlow をテストから操作するための口。
    /// usedZenzai に既定値を付けないのは意図的 — 呼び出しごとに「Zenzai 推論の計測」か
    /// 「古典/後段処理の計測」かをテスト側が明示させ、ガード分岐の両側を検証させるため。
    func forceTooSlowForTesting(ms: Double = 1000, thresholdMs: Double = 800, usedZenzai: Bool) {
        converterLock.lock()
        defer { converterLock.unlock() }
        checkZenzaiTooSlowLocked(ms: ms, thresholdMs: thresholdMs, usedZenzai: usedZenzai)
    }

    /// テスト専用: zenzaiReady（warmUp 完了ゲート）を強制設定する。private(set) の zenzaiReady を
    /// テストから操作する口。ダミー weightURL で startWarmUp を呼んでもモデルロード失敗で
    /// zenzaiReady が true にならないため、結合テストで makeOptions の zenzaiTooSlow 分岐を
    /// 分離検証するには直接ゲートを開ける必要がある。
    func setZenzaiReadyForTesting(_ value: Bool) {
        converterLock.lock()
        zenzaiReady = value
        if value, config.weightURL != nil {
            setRuntimeStateLocked(.gpuActive(device: "test-device"))
        } else if !value, config.weightURL != nil {
            setRuntimeStateLocked(.classic(reason: .notStarted))
        }
        converterLock.unlock()
    }

    /// テスト専用: makeOptionsWithZenzaiUsage の実際の決定を呼び、**同一の結果から**
    /// (options.zenzaiMode の実効 on/off, requestedZenzai 報告) の対を曝す。options 構築のみで
    /// converter を呼ばないためモデルロード・推論が走らない（決定的・環境非依存）。
    /// 報告は**要求**（.on を options に載せた）の truth table 検証用 — 要求は実行の保証では
    /// なく、実推論の資格は各経路が requestCandidates 直後に組む usedZenzai
    /// （zenzaiInferenceUsedLocked）で判定する。要求報告が決定表から切り離されて
    /// hardcode・diverge していない事の直接証拠が要る — 実 convert 経路の観測では候補並びが
    /// silent degrade で古典と同値になり検出できない（testConvertFallsBackToClassicWhenTooSlow
    /// の注記）。
    /// 本番呼び出し元と同じ converterLock 下で呼ぶ（config/learning 読みの規律）。
    func makeOptionsZenzaiRequestForTesting(nBest: Int = 10, leftSideContext: String? = nil,
                                            forceZenzai: Bool = false, forceClassic: Bool = false,
                                            noLearning: Bool = false)
        -> (zenzaiOn: Bool, requestedZenzai: Bool) {
        converterLock.lock()
        defer { converterLock.unlock() }
        let (options, requestedZenzai) = makeOptionsWithZenzaiUsage(nBest: nBest, leftSideContext: leftSideContext,
                                                                    forceZenzai: forceZenzai, forceClassic: forceClassic,
                                                                    noLearning: noLearning)
        // 実効値の読み取りは本体と同一式（.off との等値比較 — 呼び出し側で読み方を組み直すと
        // 決定の複製になる）。
        let zenzaiOn = options.zenzaiMode != .off
        return (zenzaiOn, requestedZenzai)
    }

    /// テスト専用（巡2 D9）: 現在解決済みの weightURL。reload のモデル差し替え
    /// （非 nil→別の非 nil）を zenzaiEnabled の真偽だけでは観測できないための読み出し口。
    var zenzaiWeightURLForTesting: URL? {
        configurationLock.lock()
        defer { configurationLock.unlock() }
        return config.weightURL
    }

    /// Structural observation of the vendor Zenzai call seam. Unlike a log
    /// declaration, this value is incremented only when the effective options
    /// passed to `requestCandidates` are actually `.on`.
    var zenzaiVendorInvocationCountForTesting: UInt64 {
        converterLock.lock()
        defer { converterLock.unlock() }
        return zenzaiInvocationCounter.value
    }

    /// graceful 停止（Shutdown IPC → 応答後 exit）の前段: 保留中の学習をディスクへフラッシュする。
    /// flushLearningLocked は private かつ「converterLock 保持中に呼ぶこと」契約なので、ここで
    /// converterLock を取ってから呼ぶ公開ラッパ。呼び出し元 handler は serviceLock を保持しており、
    /// converterLock をその内側で取るのは既存の順序（clearLearning と同型）に従う。
    public func prepareForShutdown() {
        maintenance.barrier()
        converterLock.lock()
        defer { converterLock.unlock() }
        // Snapshot receipts are persisted by the isolated learning converter. Flushing the
        // classic converter as well would replay the same candidate into long-term memory.
        if processRole != .mainClassicOnly { flushLearningLocked() }
        corrections.flush()
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
    /// resetMemory は vendor root が直近 request で確認でき、かつ preflight 済みのときだけ呼ぶ。
    /// OFF→ON reload 直後は vendor がまだ `.nothing + workDir` を保持し得るため、service の
    /// learning.enabled だけを根拠に reset してはいけない。ON→OFF 前の unobservable flush 後も
    /// RAM の安全な消去を確認できないため false を返す。
    private func isFileNotFound(_ error: Error) -> Bool {
        if let cocoa = error as? CocoaError, cocoa.code == .fileNoSuchFile {
            return true
        }
        let nsError = error as NSError
        return (nsError.domain == NSCocoaErrorDomain && nsError.code == NSFileNoSuchFileError) ||
            (nsError.domain == NSPOSIXErrorDomain && nsError.code == 2) // ENOENT
    }

    private struct LearningEntry {
        let name: String
        let url: URL
        let metadata: LearningPathMetadata
    }

    private struct LearningSafetyError: Error, CustomStringConvertible {
        let message: String
        var description: String { message }
    }

    /// vendor LongTermLearningMemory.reset の suffix allowlist。vendor reset を使う場合は、
    /// この判定に入る foreign name も含めて preflight で拒否し、suffix の巻き込みを防ぐ。
    private static func vendorResetLearningName(_ name: String) -> Bool {
        name.hasSuffix(".loudstxt3") || name.hasSuffix(".loudschars2") ||
            name.hasSuffix(".memorymetadata") || name.hasSuffix(".louds") ||
            name.hasSuffix(".loudstxt3.2") || name.hasSuffix(".loudschars2.2") ||
            name.hasSuffix(".memorymetadata.2") || name.hasSuffix(".louds.2") ||
            name.hasSuffix(".pause") || name.hasSuffix("learningMemory.txt")
    }

    /// 現 vendor 0.11.x が生成する名前だけを許可する。memory* の prefix だけでは
    /// `memory.backup` 等の foreign file を消すため、shard 以外は exact に限定する。
    private static func isLearningArtifactName(_ name: String) -> Bool {
        switch name {
        case ".pause", "corrections.json", "learningMemory.txt",
             "memory.louds", "memory.louds.2", "memory.loudschars2", "memory.loudschars2.2",
             "memory.memorymetadata", "memory.memorymetadata.2",
             "memory.loudstxt3", "memory.loudstxt3.2":
            return true
        default:
            break
        }
        for suffix in [".loudstxt3", ".loudstxt3.2"] {
            guard name.hasPrefix("memory"), name.hasSuffix(suffix) else { continue }
            let start = name.index(name.startIndex, offsetBy: "memory".count)
            let end = name.index(name.endIndex, offsetBy: -suffix.count)
            let shard = name[start..<end]
            // Vendor shard IDs are canonical ASCII decimal: memory0, memory1, ... .
            // Character.isNumber would also accept Unicode numerals, and a leading zero
            // creates a foreign name that vendor reset's suffix matcher could remove.
            guard !shard.isEmpty,
                  shard.utf8.allSatisfy({ $0 >= 48 && $0 <= 57 }),
                  shard.count == 1 || shard.first != "0",
                  Int(shard) != nil else { continue }
            return true
        }
        return false
    }

    /// root と各 entry を同じ seam で確認する。metadata seam が nil なのは、既存の
    /// deterministic list/remove test を壊さないための internal test fallback（production
    /// `.live` には必ず metadata がある）。
    private func scanLearningDirectory(_ dir: URL) throws -> [LearningEntry]? {
        if let metadata = fileSystem.metadata {
            guard let root = try metadata(dir) else { return nil }
            guard root.isDirectory, !root.isReparsePoint else {
                throw LearningSafetyError(message: "learning root is not a regular directory")
            }
        }
        let names = try fileSystem.list(dir)
        var result: [LearningEntry] = []
        result.reserveCapacity(names.count)
        for name in names {
            let url = dir.appendingPathComponent(name, isDirectory: false)
            if let metadata = fileSystem.metadata {
                // 列挙直後に消えた entry は NotFound と同じ benign race。その他の metadata
                // error は対象を安全に確定できないため throw する。
                guard let entryMetadata = try metadata(url) else { continue }
                result.append(LearningEntry(name: name, url: url, metadata: entryMetadata))
            } else {
                result.append(LearningEntry(
                    name: name, url: url,
                    metadata: LearningPathMetadata(isDirectory: false, isRegularFile: true,
                                                   isReparsePoint: false)))
            }
        }
        return result
    }

    private func isVendorResetSafe(_ entry: LearningEntry) -> Bool {
        Self.isLearningArtifactName(entry.name) && entry.metadata.isRegularFile &&
            !entry.metadata.isDirectory && !entry.metadata.isReparsePoint
    }

    private func isDirectDeleteSafe(_ entry: LearningEntry) -> Bool {
        entry.metadata.isRegularFile && !entry.metadata.isDirectory && !entry.metadata.isReparsePoint
    }

    private func learningPathsEqual(_ lhs: URL?, _ rhs: URL?) -> Bool {
        guard let lhs, let rhs else { return lhs == nil && rhs == nil }
        let left = lhs.standardizedFileURL.path
        let right = rhs.standardizedFileURL.path
#if os(Windows)
        return left.caseInsensitiveCompare(right) == .orderedSame
#else
        return left == right
#endif
    }

    /// テスト seam があればそこへ委譲し、本番では vendor converter の resetMemory を呼ぶ。
    /// 呼び出し元は必ず scanLearningDirectory の全件 preflight 後であること。
    private func resetVendorMemoryLocked() {
        if let resetMemory = fileSystem.resetMemory {
            resetMemory()
        } else {
            converter.resetMemory()
        }
    }

    enum LearningClearOutcome {
        case cleared
        case failed
        case inProgress
    }

    func clearLearningForIPC() -> LearningClearOutcome {
        learningStateLock.lock()
        guard !clearingLearning else {
            learningStateLock.unlock()
            return .inProgress
        }
        clearingLearning = true
        learningGeneration &+= 1
        learningStateLock.unlock()
        learningClearStartedForTesting?()
        defer {
            learningStateLock.lock()
            clearingLearning = false
            learningStateLock.unlock()
        }
        return clearLearningAccepted() ? .cleared : .failed
    }

    public func clearLearning() -> Bool {
        if case .cleared = clearLearningForIPC() { return true }
        return false
    }

    private func clearLearningAccepted() -> Bool {
        // Establish a generation boundary and drain already-accepted work before deletion. New
        // receipts observe clearingLearning and cannot enqueue into either learning state.
        maintenance.barrier()
        converterLock.lock()
        defer { converterLock.unlock() }

        // RAM の訂正テーブルは disk preflight が失敗しても消す（既存の false/error 契約）。
        // corrections.json 自体は下の allowlist preflight 後に seam 経由で削除する。
        corrections.clearMemory()
        recentLearning.clear()
        snapshotAutoCommitStates.removeAll()
        appliedSnapshotAutoCommitReceipts.removeAll()
        // Dropping the dedicated converter is the only observable way to discard vendor
        // temporary learning after a failed commit; its API does not report commit success.
        learningConverter = KanaKanjiConverter.withDefaultDictionary()

        let dir = learningDirectory
        guard let dir else {
            // root 不明のまま vendor temporary が存在する可能性がある状態で成功を返さない。
            guard vendorTemporaryState == .empty else {
                engineLog("ev=learning_clear clean=false reason=no_root_with_ram\n")
                return false
            }
            engineLog("ev=learning_clear clean=true reason=no_dir\n")
            return true
        }

        // resetMemory を許可できるのは、直近 vendor config が学習 ON で、root が現在の
        // clear root と一致する場合だけ。特に OFF→ON reload 直後は vendor config がまだ
        // .nothing+workDir のままなので、service.learning.enabled だけで reset しない。
        let vendorRootIsWorkDir = learningPathsEqual(vendorLearningRoot, workDir)
        let canResetVendor = learning.enabled && vendorLearningConfigKnown && vendorLearningEnabled &&
            !vendorRootIsWorkDir && learningPathsEqual(vendorLearningRoot, dir)
        if vendorLearningConfigKnown && vendorLearningEnabled && !learningPathsEqual(vendorLearningRoot, dir) {
            engineLog("ev=learning_clear clean=false reason=vendor_root_unknown\n")
            return false
        }
        if !vendorLearningConfigKnown && vendorTemporaryState != .empty {
            engineLog("ev=learning_clear clean=false reason=vendor_config_unknown\n")
            return false
        }
        // `.nothing` request は vendor の temporary trie を消さないため、最後の request が
        // OFF/noLearning のままでは resetMemory を呼べない。unobservable flush 後も同じく、
        // actual root と同期した ON request を再度観測するまで fail-closed にする。
        guard vendorTemporaryState == .empty || canResetVendor else {
            let reason = vendorTemporaryState == .unobservableAfterFlush
                ? "unobservable_flush" : "vendor_ram_unknown"
            engineLog("ev=learning_clear clean=false reason=\(reason)\n")
            return false
        }

        let entries: [LearningEntry]?
        do {
            entries = try scanLearningDirectory(dir)
        } catch {
            // 初回列挙でディレクトリが無いのは「既に消えている」ので成功。
            // それ以外は削除対象を確定できず、成功を偽らない。
            let clean = isFileNotFound(error)
            if clean && vendorTemporaryState != .empty {
                // root metadata の確認後に directory が消えた場合でも、vendor temporary
                // trie の存在は観測不能なまま。resetMemory は path 消失後に呼ばず、RAM
                // clear の成功を偽装しない（再起動後に再試行できる）。
                engineLog("ev=learning_clear clean=false phase=initial_list reason=ram_unobservable\n")
                return false
            }
            engineLog("ev=learning_clear clean=\(clean) phase=initial_list error=\(error)\n")
            return clean
        }

        // metadata seam が root 不在を nil で返すケースも NotFound semantics と同じ。
        if entries == nil {
            if vendorTemporaryState != .empty {
                // root が metadata 段階で消えていても vendor temporary trie の残存は
                // 観測不能。消えた path へ resetMemory を試さず、RAM clear を成功扱いしない。
                engineLog("ev=learning_clear clean=false reason=no_dir_with_ram\n")
                return false
            }
            engineLog("ev=learning_clear clean=true reason=no_dir\n")
            return true
        }
        guard let entries else { return true }

        // vendor reset の suffix 巻き込みと direct remove の unsafe target を、どの削除より
        // 前に全件検証する。foreign.txt のような非 allowlist entry はそのまま保持する。
        for entry in entries {
            if canResetVendor && Self.vendorResetLearningName(entry.name) && !isVendorResetSafe(entry) {
                engineLog("ev=learning_clear clean=false reason=unsafe_vendor_target file=\(entry.name)\n")
                return false
            }
            if Self.isLearningArtifactName(entry.name) && !isDirectDeleteSafe(entry) {
                engineLog("ev=learning_clear clean=false reason=unsafe_target file=\(entry.name)\n")
                return false
            }
        }

        if canResetVendor {
            // root/全 vendor suffix target は上の preflight 済み。resetMemory は temporary trie
            // を必ず空にする一方、disk error は内部で握るため、下の remove/verify も必須。
            resetVendorMemoryLocked()
            vendorTemporaryState = .empty
        }

        var clean = true
        for entry in entries where Self.isLearningArtifactName(entry.name) {
            do {
                try fileSystem.remove(entry.url)
            } catch {
                // 競合で先に消えた場合だけ成功扱い。それ以外は残留の有無に関わらず失敗。
                if !isFileNotFound(error) {
                    clean = false
                    engineLog("ev=learning_clear clean=false phase=remove file=\(entry.name) error=\(error)\n")
                }
            }
        }

        do {
            guard let after = try scanLearningDirectory(dir) else {
                vendorTemporaryState = .empty
                engineLog("ev=learning_clear clean=true reason=no_dir\n")
                return clean
            }
            if after.contains(where: { entry in
                Self.isLearningArtifactName(entry.name) ||
                    (canResetVendor && Self.vendorResetLearningName(entry.name))
            }) {
                clean = false
                engineLog("ev=learning_clear clean=false phase=verify reason=residual\n")
            }
        } catch {
            // 初回列挙とは違い、検証列挙の失敗は真偽を確認できないため失敗。
            clean = false
            engineLog("ev=learning_clear clean=false phase=verify error=\(error)\n")
        }
        if clean && (vendorTemporaryState == .empty || canResetVendor) {
            vendorTemporaryState = .empty
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
    /// `noLearning`: 文節境界の再導出専用。学習メモリの全文1エントリが 1 位を取り返すと
    /// 再導出まで単一要素に戻ってしまうため、そのリクエストだけ学習を外す。
    /// 実体は makeOptionsWithZenzaiUsage — フラグが不要な呼び出し側（forceClassic 仮説・辞書境界の
    /// 再導出・warmUp）向けの薄いラッパで、決定表はここに持たない。
    private func makeOptions(nBest: Int = 10, leftSideContext: String? = nil, forceZenzai: Bool = false, forceClassic: Bool = false, noLearning: Bool = false) -> ConvertRequestOptions {
        makeOptionsWithZenzaiUsage(nBest: nBest, leftSideContext: leftSideContext, forceZenzai: forceZenzai,
                                   forceClassic: forceClassic, noLearning: noLearning).options
    }

    /// makeOptions の実体。options に加え、**このリクエストの zenzaiMode が .on を要求したか**を
    /// 同一の決定（下の分岐）から報告する。監視（checkZenzaiTooSlowLocked）が「Zenzai 推論の
    /// 時間」だけを数えるための口 — mode 判定を呼び出し側で再構築させると決定表が二重化して
    /// すぐ齟齬るので、.on/.off はここでのみ決める。
    /// requestedZenzai は**要求**であって実行の保証ではない: invalid/nonexistent weight では
    /// upstream の requestCandidates が古典へ silent fallback するため、実稼働の判定は各経路が
    /// requestCandidates 直後に zenzaiInferenceUsedLocked（要求 × 入力非空 × 実ロード成功）で行う。
    private func makeOptionsWithZenzaiUsage(nBest: Int = 10, leftSideContext: String? = nil, forceZenzai: Bool = false, forceClassic: Bool = false, noLearning: Bool = false)
        -> (options: ConvertRequestOptions, requestedZenzai: Bool) {
        let classicOnly = forceClassic || processRole == .mainClassicOnly
        // cold start ③: ゲートが開く（zenzaiReady）まで Zenzai を options に載せない＝古典（辞書）変換で即応。
        // forceZenzai は warmUp 専用（ゲートを開ける前のモデル先読みロードに Zenzai ON が要る）。
        // zenzaiTooSlow: 推論が恒常的に重い環境では古典固定（drop_engine 自己増幅ループ＝Space ハング防止）。
        //   forceZenzai（warmUp）は zenzaiTooSlow で止めない — warmUp は起動時1回のモデル先読みで、
        //   ユーザーが Zenzai を意図した以上はロードを尊重し、ロード完了後の convert で重さを判定する。
        let zenzai: ConvertRequestOptions.ZenzaiMode
        if classicOnly {
            zenzai = .off
        } else if forceZenzai, case .warming = _zenzaiRuntimeState {
            zenzai = ConversionService.makeZenzaiMode(config: config, leftSideContext: leftSideContext)
        } else if zenzaiReady && !zenzaiTooSlow,
                  case .gpuActive = _zenzaiRuntimeState {
            zenzai = ConversionService.makeZenzaiMode(config: config, leftSideContext: leftSideContext)
        } else {
            zenzai = .off
        }
        // weightURL 無しでは makeZenzaiMode 自体が .off に落ちるため、実効値は .off との
        // 等値比較で読む（分岐条件の再評価ではなく options の実効値 — 決定の重複にならない）。
        // ZenzaiMode は struct（.on は static ファクトリで enum case ではない）なので case
        // 一致は不可。公開 API が .off/.on の2経路だけ（memberwise init は internal）のため
        // != .off は enabled フラグと完全同値。
        let requestedZenzai = zenzai != .off
        return (.init(
            N_best: nBest,
            requireJapanesePrediction: false,
            requireEnglishPrediction: false,
            keyboardLanguage: .ja_JP,
            fullWidthRomanCandidate: true,   // 数字/英数の全角候補を常時提供（読みは半角 canonical のまま）
            learningType: (learning.enabled && !noLearning) ? .inputAndOutput : .nothing,
            memoryDirectoryURL: learning.memoryDir ?? workDir,
            sharedContainerURL: workDir,
            textReplacer: .withDefaultEmojiDictionary(),
            specialCandidateProviders: nil,
            zenzaiMode: zenzai,
            metadata: .init(versionString: "NospacekeyEngineHost")
        ), requestedZenzai)
    }
}
