import XCTest
@testable import NospacekeyEngineCore

/// SP3 ライブ変換: liveConvert(session:) は N_best=1 で「先頭候補(text)」と「現在の読み(reading)」を返す。
/// 古典モード（weightURL=nil）で検証し、Zenzai 実モデル無しでも走る。
final class LiveConvertTests: XCTestCase {
    private final class Latencies: @unchecked Sendable {
        private let lock = NSLock()
        private var values: [Double] = []
        func store(_ newValues: [Double]) { lock.lock(); values = newValues; lock.unlock() }
        func load() -> [Double] { lock.lock(); defer { lock.unlock() }; return values }
    }

    private func firstSnapshotProposal(_ svc: ConversionService, composition: UInt64 = 500)
        -> (ConversionService.SnapshotEnhancementKey, ConversionService.SnapshotAutoCommitProposal)? {
        var raw = ""
        for (offset, ch) in "watashiha,gakkouheikimasu".enumerated() {
            raw.append(ch)
            let key = ConversionService.SnapshotEnhancementKey(
                composition: composition, revision: UInt64(offset + 1),
                configurationGeneration: 2, connectionGeneration: 5)
            let result = svc.snapshot([SnapshotSegment(text: raw, style: nil)], explicit: false,
                                      enhancementKey: key, snapshotConnection: 1)
            if let proposal = result.autoCommit { return (key, proposal) }
        }
        return nil
    }

    func testSuccessfulReceiptQueuesLearningOnceWithoutBlockingClassicOnPersistence() {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-async-learning-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let started = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        final class Counter: @unchecked Sendable { let lock = NSLock(); var value = 0 }
        let count = Counter()
        let svc = ConversionService(
            config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: true, memoryDir: dir),
            autoCommit: .ultrastrong, autoCommitMaxReading: 8,
            fileSystem: .live,
            learningPersistenceForTesting: { _ in
                count.lock.lock(); count.value += 1; count.lock.unlock()
                started.signal()
                release.wait()
            })
        guard let (key, proposal) = firstSnapshotProposal(svc) else {
            return XCTFail("representative input must propose an auto commit")
        }
        XCTAssertTrue(svc.applySnapshotAutoCommitReceipt(
            connection: 1, key: key, proposal: proposal.proposal))
        XCTAssertEqual(started.wait(timeout: .now() + 1), .success)

        let classicFinished = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            _ = svc.snapshot([SnapshotSegment(text: "nihongo", style: nil)], explicit: true)
            classicFinished.signal()
        }
        XCTAssertEqual(classicFinished.wait(timeout: .now() + .seconds(2)), .success,
                       "classic must finish before blocked persistence is released")
        XCTAssertTrue(svc.applySnapshotAutoCommitReceipt(
            connection: 1, key: key, proposal: proposal.proposal),
            "a replayed successful receipt stays idempotent")
        release.signal()
        svc.flushMaintenanceForTesting()
        count.lock.lock(); let persisted = count.value; count.lock.unlock()
        XCTAssertEqual(persisted, 1)
    }

    func testSuccessfulReceiptRanksTheNextSnapshotBeforeBackgroundPersistenceRuns() {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-immediate-receipt-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let service = ConversionService(
            config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: true, memoryDir: dir),
            autoCommit: .ultrastrong, autoCommitMaxReading: 8,
            fileSystem: .live, learningPersistenceForTesting: { _ in })
        guard let (key, proposal) = firstSnapshotProposal(service, composition: 505) else {
            return XCTFail("representative input must propose an auto commit")
        }
        let releaseMaintenance = service.beginMaintenanceHoldForTesting()
        defer {
            releaseMaintenance()
            service.flushMaintenanceForTesting()
        }

        XCTAssertTrue(service.applySnapshotAutoCommitReceipt(
            connection: 1, key: key, proposal: proposal.proposal))
        let next = service.snapshot(
            [SnapshotSegment(text: proposal.consumedReading + proposal.remaining, style: "direct")],
            explicit: true)
        XCTAssertTrue(next.candidates?.first?.hasPrefix(proposal.text) == true,
                      "the accepted prefix must rank its containing whole before persistence runs")
    }

#if !DEBUG
    func testReleaseSnapshotP99DoesNotRegressWhileLearningPersistenceIsBlocked() {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-p99-learning-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let persistenceStarted = DispatchSemaphore(value: 0)
        let releasePersistence = DispatchSemaphore(value: 0)
        let service = ConversionService(
            config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: true, memoryDir: dir),
            autoCommit: .ultrastrong, autoCommitMaxReading: 8,
            fileSystem: .live,
            learningPersistenceForTesting: { _ in
                persistenceStarted.signal()
                releasePersistence.wait()
            })
        for _ in 0..<32 {
            _ = service.snapshot([SnapshotSegment(text: "nihongo", style: nil)], explicit: true)
        }
        let baseline = Self.snapshotLatencies(service, count: 256)
        guard let (key, proposal) = firstSnapshotProposal(service, composition: 503) else {
            return XCTFail("representative input must propose an auto commit")
        }
        XCTAssertTrue(service.applySnapshotAutoCommitReceipt(
            connection: 1, key: key, proposal: proposal.proposal))
        XCTAssertEqual(persistenceStarted.wait(timeout: .now() + 2), .success)

        let blocked = Latencies()
        let completed = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            blocked.store(Self.snapshotLatencies(service, count: 256))
            completed.signal()
        }
        guard completed.wait(timeout: .now() + 90) == .success else {
            releasePersistence.signal()
            service.flushMaintenanceForTesting()
            return XCTFail("snapshot batch waited for blocked learning persistence")
        }
        releasePersistence.signal()
        service.flushMaintenanceForTesting()

        let baselineP99 = Self.percentile99(baseline)
        let blockedP99 = Self.percentile99(blocked.load())
        print("snapshot_p99 baseline_ms=\(baselineP99) blocked_ms=\(blockedP99) delta_ms=\(blockedP99 - baselineP99)")
        XCTAssertTrue(Self.snapshotP99IsWithinBudget(baseline: baselineP99, blocked: blockedP99),
                      "blocked learning added \(blockedP99 - baselineP99)ms to snapshot p99")
    }
#endif

    func testSnapshotP99BudgetRejectsAContinuousTwoHundredMillisecondRegression() {
        XCTAssertFalse(Self.snapshotP99IsWithinBudget(baseline: 10, blocked: 210))
        XCTAssertTrue(Self.snapshotP99IsWithinBudget(baseline: 10, blocked: 30))
    }

    private static func snapshotLatencies(_ service: ConversionService, count: Int) -> [Double] {
        (0..<count).map { _ in
            let start = DispatchTime.now().uptimeNanoseconds
            _ = service.snapshot([SnapshotSegment(text: "nihongo", style: nil)], explicit: true)
            return Double(DispatchTime.now().uptimeNanoseconds - start) / 1_000_000
        }
    }

    private static func percentile99(_ values: [Double]) -> Double {
        let sorted = values.sorted()
        let index = max(0, Int(ceil(Double(sorted.count) * 0.99)) - 1)
        return sorted[index]
    }

    private static func snapshotP99IsWithinBudget(baseline: Double, blocked: Double) -> Bool {
        blocked - baseline <= max(20, baseline * 0.5)
    }

    func testClearLearningDropsProcessLocalReceiptLearning() {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-clear-receipt-learning-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let svc = ConversionService(
            config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: true, memoryDir: dir),
            autoCommit: .ultrastrong, autoCommitMaxReading: 8, fileSystem: .live,
            processRole: .mainClassicOnly,
            learningPersistenceForTesting: { _ in })
        guard let (key, proposal) = firstSnapshotProposal(svc, composition: 502) else {
            return XCTFail("representative input must propose an auto commit")
        }
        XCTAssertTrue(svc.applySnapshotAutoCommitReceipt(
            connection: 1, key: key, proposal: proposal.proposal))
        svc.flushMaintenanceForTesting()
        XCTAssertEqual(svc.recentLearningCountForTesting, 1)
        XCTAssertTrue(svc.clearLearning())
        XCTAssertEqual(svc.recentLearningCountForTesting, 0)
    }

    func testClearLearningRejectsAnAppliedReceiptReplayAndBoundsLedgersAcrossUniqueStreams() {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-clear-pending-receipt-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let service = ConversionService(
            config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: true, memoryDir: dir),
            autoCommit: .ultrastrong, autoCommitMaxReading: 8, fileSystem: .live)
        guard let (key, proposal) = firstSnapshotProposal(service, composition: 504) else {
            return XCTFail("representative input must propose an auto commit")
        }
        XCTAssertTrue(service.applySnapshotAutoCommitReceipt(
            connection: 1, key: key, proposal: proposal.proposal))
        XCTAssertEqual(service.snapshotReceiptLedgerCountsForTesting.applied, 1)
        XCTAssertTrue(service.clearLearning())
        XCTAssertEqual(service.snapshotReceiptLedgerCountsForTesting.pending, 0)
        XCTAssertEqual(service.snapshotReceiptLedgerCountsForTesting.applied, 0)
        XCTAssertFalse(service.applySnapshotAutoCommitReceipt(
            connection: 1, key: key, proposal: proposal.proposal),
            "an applied receipt from before ClearLearning must not become valid again")

        for cycle in 0..<3 {
            guard let (nextKey, nextProposal) = firstSnapshotProposal(
                service, composition: UInt64(600 + cycle)) else {
                return XCTFail("unique stream must propose an auto commit")
            }
            XCTAssertTrue(service.applySnapshotAutoCommitReceipt(
                connection: 1, key: nextKey, proposal: nextProposal.proposal))
            XCTAssertTrue(service.clearLearning())
            XCTAssertEqual(service.snapshotReceiptLedgerCountsForTesting.pending, 0)
            XCTAssertEqual(service.snapshotReceiptLedgerCountsForTesting.applied, 0,
                           "repeated clears must not leave an unbounded replay ledger")
        }
    }

    func testRejectedReceiptNeverQueuesLearning() {
        let invoked = DispatchSemaphore(value: 0)
        let svc = ConversionService(
            config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
            learning: LearningSettings(enabled: true, memoryDir: FileManager.default.temporaryDirectory),
            autoCommit: .ultrastrong, autoCommitMaxReading: 8, fileSystem: .live,
            learningPersistenceForTesting: { _ in invoked.signal() })
        guard let (key, proposal) = firstSnapshotProposal(svc, composition: 501) else {
            return XCTFail("representative input must propose an auto commit")
        }
        let stale = ConversionService.SnapshotEnhancementKey(
            composition: key.composition, revision: key.revision + 1,
            configurationGeneration: key.configurationGeneration,
            connectionGeneration: key.connectionGeneration)
        XCTAssertFalse(svc.applySnapshotAutoCommitReceipt(
            connection: 1, key: stale, proposal: proposal.proposal))
        XCTAssertEqual(invoked.wait(timeout: .now() + .milliseconds(50)), .timedOut)
    }
    private func makeService(autoCommit: AutoCommitStrength = .weak, autoCommitMaxReading: Int = 25,
                             snapshotAutoCommitStateLimit: Int = ConversionService.defaultSnapshotAutoCommitStateLimit)
        -> ConversionService {
        ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1), autoCommit: autoCommit,
                           autoCommitMaxReading: autoCommitMaxReading,
                           snapshotAutoCommitStateLimit: snapshotAutoCommitStateLimit)
    }

    func testLiveConvertReturnsTopCandidateAndReading() {
        let svc = makeService()
        let sid = svc.startSession()
        _ = svc.insert(session: sid, text: "nihongo")
        guard let r = svc.liveConvert(session: sid) else { return XCTFail("known session must not return nil") }
        XCTAssertEqual(r.reading, "にほんご")
        XCTAssertFalse(r.text.isEmpty, "live text should be non-empty")
        XCTAssertNil(r.committed, "allowAutoCommit を渡さない既定では読みを消費しない")
    }

    // ---- 自動確定（iOS nospacekey の先頭文節自動確定の移植） ----

    /// 1文字ずつ挿入しながら liveConvert(allowAutoCommit:true) を繰り返す打鍵シミュレーション。
    /// 各更新で「確定なしなら読み不変 / 確定ありなら読みが必ず縮む」の不変条件を検証し、
    /// 確定された文節列を返す。
    private func typeAndCollectAutoCommits(
        _ svc: ConversionService, session: Int, romaji: String,
        file: StaticString = #filePath, line: UInt = #line
    ) -> [String] {
        var committed: [String] = []
        for ch in romaji {
            guard let readingBefore = svc.insert(session: session, text: String(ch)) else {
                XCTFail("insert failed", file: file, line: line); return committed
            }
            guard let r = svc.liveConvert(session: session, allowAutoCommit: true) else {
                XCTFail("liveConvert failed", file: file, line: line); return committed
            }
            if let prefix = r.committed {
                XCTAssertFalse(prefix.isEmpty, "確定文節は非空", file: file, line: line)
                XCTAssertLessThan(r.reading.count, readingBefore.count,
                                  "自動確定後は残り読みが必ず縮む", file: file, line: line)
                committed.append(prefix)
            } else {
                XCTAssertEqual(r.reading, readingBefore,
                               "自動確定なしなら読みは不変", file: file, line: line)
            }
        }
        return committed
    }

    /// ultrastrong(=6): 打鍵していくと、先頭文節が直近6更新で安定した時点で自動確定が発火し、
    /// 残り読みで合成が継続する（iOS の自動確定と同じ挙動）。
    ///
    /// 入力は読点入りの文にする。裸の助詞境界（わたしは|がっこうへ 等）は辞書の複合エントリ
    /// （「は学校|ハガッコウ」「へ行き|ヘイキ」のような文節境界をまたぐ融合要素）が先頭文節に
    /// 吸着し続けて安定しないため、句読点のような硬い境界がないと発火が入力依存になる
    /// （iOS 本家 LiveConversionManager + 同一辞書でも同じ。実測: 2026-07-08）。
    func testAutoCommitFiresOnStableFirstClause() {
        let svc = makeService(autoCommit: .ultrastrong)
        let sid = svc.startSession()
        // わたしは、がっこうへいきます — 読点が文節境界を固定し「私は」が early に安定する。
        let committed = typeAndCollectAutoCommits(svc, session: sid, romaji: "watashiha,gakkouheikimasu")
        XCTAssertFalse(committed.isEmpty, "先頭文節が安定したら自動確定が発火する（ultrastrong=6）")
        // 確定後もセッションは生きており、残り読みへの追記が継続できる。
        XCTAssertNotNil(svc.insert(session: sid, text: "a"))
    }

    func testSnapshotAutoCommitProposesWithoutConsumingUntilReceipt() {
        let svc = makeService(autoCommit: .ultrastrong, autoCommitMaxReading: 8)
        var raw = ""
        var proposal: ConversionService.SnapshotAutoCommitProposal?
        var proposalKey: ConversionService.SnapshotEnhancementKey?
        for (offset, ch) in "watashiha,gakkouheikimasu".enumerated() {
            raw.append(ch)
            let key = ConversionService.SnapshotEnhancementKey(
                composition: 91, revision: UInt64(offset + 1),
                configurationGeneration: 2, connectionGeneration: 5)
            let result = svc.snapshot(
                [SnapshotSegment(text: raw, style: nil)], explicit: false,
                enhancementKey: key, snapshotConnection: 1)
            if let value = result.autoCommit {
                proposal = value
                proposalKey = key
                XCTAssertEqual(result.reading, value.consumedReading + value.remaining)
                break
            }
        }
        guard let proposal, let proposalKey else {
            return XCTFail("representative live input must produce a proposal")
        }
        XCTAssertFalse(proposal.text.isEmpty)
        XCTAssertTrue(svc.applySnapshotAutoCommitReceipt(
            connection: 1, key: proposalKey, proposal: proposal.proposal))
        XCTAssertTrue(svc.applySnapshotAutoCommitReceipt(
            connection: 1, key: proposalKey, proposal: proposal.proposal),
            "a retried receipt is acknowledged idempotently")
    }

    func testNewerRevisionSupersedesAnUnreceiptedSnapshotAutoCommitProposal() {
        let svc = makeService(autoCommit: .ultrastrong, autoCommitMaxReading: 8)
        var raw = ""
        var proposal: ConversionService.SnapshotAutoCommitProposal?
        var proposalKey: ConversionService.SnapshotEnhancementKey?
        var revision: UInt64 = 0
        for ch in "watashiha,gakkouheikimasu" {
            raw.append(ch)
            revision += 1
            let key = ConversionService.SnapshotEnhancementKey(
                composition: 92, revision: revision,
                configurationGeneration: 2, connectionGeneration: 5)
            let result = svc.snapshot(
                [SnapshotSegment(text: raw, style: nil)], explicit: false,
                enhancementKey: key, snapshotConnection: 1)
            if let value = result.autoCommit {
                proposal = value
                proposalKey = key
                break
            }
        }
        guard let proposal, let proposalKey else {
            return XCTFail("representative live input must produce a proposal")
        }

        var replacement: ConversionService.SnapshotAutoCommitProposal?
        var replacementKey: ConversionService.SnapshotEnhancementKey?
        for ch in "xy" {
            raw.append(ch)
            revision += 1
            let key = ConversionService.SnapshotEnhancementKey(
                composition: 92, revision: revision,
                configurationGeneration: 2, connectionGeneration: 5)
            let newer = svc.snapshot(
                [SnapshotSegment(text: raw, style: nil)], explicit: false,
                enhancementKey: key, snapshotConnection: 1)
            if let value = newer.autoCommit {
                replacement = value
                replacementKey = key
                break
            }
        }
        guard let replacement, let replacementKey else {
            return XCTFail("new revisions must resume history and proposal evaluation")
        }
        XCTAssertFalse(svc.applySnapshotAutoCommitReceipt(
            connection: 1, key: proposalKey, proposal: proposal.proposal))
        XCTAssertTrue(svc.applySnapshotAutoCommitReceipt(
            connection: 1, key: replacementKey, proposal: replacement.proposal))
        XCTAssertTrue(svc.applySnapshotAutoCommitReceipt(
            connection: 1, key: replacementKey, proposal: replacement.proposal))
    }

    func testSnapshotAutoCommitStateHasAHardBoundForAbandonedPendingStreams() {
        XCTAssertEqual(ConversionService.defaultSnapshotAutoCommitStateLimit, 64)
        let svc = makeService(
            autoCommit: .ultrastrong, autoCommitMaxReading: 8,
            snapshotAutoCommitStateLimit: 2)
        var proposals: [(Int, ConversionService.SnapshotEnhancementKey,
                         ConversionService.SnapshotAutoCommitProposal)] = []
        for connection in 1...3 {
            var raw = ""
            for (offset, ch) in "watashiha,gakkouheikimasu".enumerated() {
                raw.append(ch)
                let key = ConversionService.SnapshotEnhancementKey(
                    composition: UInt64(connection), revision: UInt64(offset + 1),
                    configurationGeneration: 2, connectionGeneration: 5)
                let result = svc.snapshot(
                    [SnapshotSegment(text: raw, style: nil)], explicit: false,
                    enhancementKey: key, snapshotConnection: connection)
                if let proposal = result.autoCommit {
                    proposals.append((connection, key, proposal))
                    break
                }
            }
        }
        XCTAssertEqual(proposals.count, 3)
        let first = proposals.first!
        let latest = proposals.last!
        XCTAssertFalse(svc.applySnapshotAutoCommitReceipt(
            connection: first.0, key: first.1, proposal: first.2.proposal),
            "the deterministic oldest stream must be evicted at the hard limit")
        XCTAssertTrue(svc.applySnapshotAutoCommitReceipt(
            connection: latest.0, key: latest.1, proposal: latest.2.proposal))
        XCTAssertTrue(svc.applySnapshotAutoCommitReceipt(
            connection: latest.0, key: latest.1, proposal: latest.2.proposal),
            "a retained receipt remains idempotent")
    }

    func testNewCompositionOnSameSnapshotConnectionReplacesAbandonedPendingStream() {
        let svc = makeService(autoCommit: .ultrastrong, autoCommitMaxReading: 8)
        var proposals: [(ConversionService.SnapshotEnhancementKey,
                         ConversionService.SnapshotAutoCommitProposal)] = []
        for composition: UInt64 in [101, 102] {
            var raw = ""
            for (offset, ch) in "watashiha,gakkouheikimasu".enumerated() {
                raw.append(ch)
                let key = ConversionService.SnapshotEnhancementKey(
                    composition: composition, revision: UInt64(offset + 1),
                    configurationGeneration: 2, connectionGeneration: 5)
                let result = svc.snapshot(
                    [SnapshotSegment(text: raw, style: nil)], explicit: false,
                    enhancementKey: key, snapshotConnection: 7)
                if let proposal = result.autoCommit {
                    proposals.append((key, proposal))
                    break
                }
            }
        }
        XCTAssertEqual(proposals.count, 2)
        XCTAssertFalse(svc.applySnapshotAutoCommitReceipt(
            connection: 7, key: proposals[0].0, proposal: proposals[0].1.proposal))
        XCTAssertTrue(svc.applySnapshotAutoCommitReceipt(
            connection: 7, key: proposals[1].0, proposal: proposals[1].1.proposal))
    }

    /// disabled: どれだけ打鍵しても自動確定は発火しない。
    func testAutoCommitDisabledNeverCommits() {
        let svc = makeService(autoCommit: .disabled)
        let sid = svc.startSession()
        let committed = typeAndCollectAutoCommits(svc, session: sid, romaji: "watashihagakkouheikimasu")
        XCTAssertTrue(committed.isEmpty, "disabled では読みを消費しない")
    }

    /// allowAutoCommit=false（Enter 直前の LiveConvert 等）は、強度設定にかかわらず読みを消費しない。
    /// エンジンが勝手に prefixComplete すると直後の Commit{0} が残り読みしか確定できなくなるため。
    func testAllowFlagFalseNeverConsumesReading() {
        let svc = makeService(autoCommit: .ultrastrong)
        let sid = svc.startSession()
        for ch in "watashihagakkouheikimasu" {
            let readingBefore = svc.insert(session: sid, text: String(ch))
            guard let r = svc.liveConvert(session: sid) else { return XCTFail("liveConvert failed") }
            XCTAssertNil(r.committed)
            XCTAssertEqual(r.reading, readingBefore)
        }
    }

    // ---- 読み長バックストップ（死のループ対策） ----

    /// 句読点なし・裸助詞境界のみの長文（先頭文節が安定しないため通常判定では発火しない —
    /// testAutoCommitFiresOnStableFirstClause のコメント参照）でも、maxReading を小さく設定すれば
    /// 読み長超過で強制確定が発火し、読みが頭打ちになる。
    func testLengthBackstopFiresWhenStableJudgmentNever() {
        let svc = makeService(autoCommit: .ultrastrong, autoCommitMaxReading: 8)
        let sid = svc.startSession()
        let committed = typeAndCollectAutoCommits(svc, session: sid, romaji: "watashihagakkouheikimasunanode")
        XCTAssertFalse(committed.isEmpty, "読み長バックストップが安全弁として発火する")
        // 確定後もセッションは生きており、残り読みへの追記が継続できる。
        XCTAssertNotNil(svc.insert(session: sid, text: "a"))
    }

    /// disabled のときはバックストップも従属して発火しない（既存の明示オプトアウトを尊重）。
    func testLengthBackstopDoesNotFireWhenAutoCommitDisabled() {
        let svc = makeService(autoCommit: .disabled, autoCommitMaxReading: 8)
        let sid = svc.startSession()
        let committed = typeAndCollectAutoCommits(svc, session: sid, romaji: "watashihagakkouheikimasunanode")
        XCTAssertTrue(committed.isEmpty, "disabled では読み長バックストップも無効")
    }

    /// maxReading=0（無効設定）では、通常判定が発火しない入力でも強制確定は起きない。
    func testLengthBackstopDisabledWhenMaxReadingIsZero() {
        let svc = makeService(autoCommit: .ultrastrong, autoCommitMaxReading: 0)
        let sid = svc.startSession()
        let committed = typeAndCollectAutoCommits(svc, session: sid, romaji: "watashihagakkouheikimasunanode")
        XCTAssertTrue(committed.isEmpty, "maxReading<=0 はバックストップ OFF")
    }

    // ---- AutoCommitStrength.resolve（env 解決） ----

    func testAutoCommitStrengthResolveDefaultsToWeakLikeIOS() {
        // iOS の AutomaticCompletionStrengthKey.defaultValue = .weak と同値。
        XCTAssertEqual(AutoCommitStrength.resolve(environment: [:]), .weak)
        XCTAssertEqual(AutoCommitStrength.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT": ""]), .weak)
        XCTAssertEqual(AutoCommitStrength.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT": "unknown"]), .weak)
    }

    func testAutoCommitStrengthResolveParsesValues() {
        XCTAssertEqual(AutoCommitStrength.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT": "disabled"]), .disabled)
        XCTAssertEqual(AutoCommitStrength.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT": "ULTRASTRONG"]), .ultrastrong)
        XCTAssertEqual(AutoCommitStrength.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT": "normal"]), .normal)
    }

    func testAutoCommitThresholdsMatchIOS() {
        // iOS AutoCompletionStrengthSetting.swift の threshold と一致（weak16/normal13/strong10/ultra6）。
        XCTAssertNil(AutoCommitStrength.disabled.threshold)
        XCTAssertEqual(AutoCommitStrength.weak.threshold, 16)
        XCTAssertEqual(AutoCommitStrength.normal.threshold, 13)
        XCTAssertEqual(AutoCommitStrength.strong.threshold, 10)
        XCTAssertEqual(AutoCommitStrength.ultrastrong.threshold, 6)
    }

    // ---- AutoCommitLengthBackstop.resolve（env 解決） ----

    func testAutoCommitLengthBackstopResolveDefaultsTo25() {
        XCTAssertEqual(AutoCommitLengthBackstop.resolve(environment: [:]), 25)
        XCTAssertEqual(AutoCommitLengthBackstop.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT_MAX_READING": "abc"]), 25)
    }

    func testAutoCommitLengthBackstopResolveParsesValue() {
        XCTAssertEqual(AutoCommitLengthBackstop.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT_MAX_READING": "12"]), 12)
    }

    func testAutoCommitLengthBackstopResolveNonPositiveMeansDisabled() {
        XCTAssertEqual(AutoCommitLengthBackstop.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT_MAX_READING": "0"]), 0)
        XCTAssertEqual(AutoCommitLengthBackstop.resolve(environment: ["NOSPACEKEY_AUTO_COMMIT_MAX_READING": "-5"]), -5)
    }
}
