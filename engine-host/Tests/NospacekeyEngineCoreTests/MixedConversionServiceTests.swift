import XCTest
import WinSDK
@testable import NospacekeyEngineCore

final class MixedConversionServiceTests: XCTestCase {
    private func span(_ kind: String, _ start: UInt32, _ text: String) -> MixedSpan {
        MixedSpan(kind: kind, readingStart: start,
                  readingEnd: start + UInt32(text.unicodeScalars.count), text: text)
    }

    // ---- validate: span 列の契約（完全被覆・既知kind・文字数一致）----

    func testValidateAcceptsContiguousSpansFromZero() {
        XCTAssertTrue(MixedConversionService.validate(spans: [
            span("japanese", 0, "きょうは"),
            span("literal", 4, "Python"),
            span("japanese", 10, "をつかう"),
        ]))
    }

    func testValidateRejectsGapOverlapUnknownKindAndLengthMismatch() {
        // 先頭が 0 でない（欠落）
        XCTAssertFalse(MixedConversionService.validate(spans: [span("japanese", 1, "きょうは")]))
        // 重複（隣接していない）
        XCTAssertFalse(MixedConversionService.validate(spans: [
            span("japanese", 0, "きょうは"), span("literal", 5, "Python"),
        ]))
        // 未知 kind
        XCTAssertFalse(MixedConversionService.validate(spans: [span("english", 0, "abc")]))
        // 文字数が読み幅と不一致（Literal の原文は読みと同じ文字数で載る契約）
        XCTAssertFalse(MixedConversionService.validate(spans: [
            MixedSpan(kind: "literal", readingStart: 0, readingEnd: 4, text: "abc"),
        ]))
        // 空区間
        XCTAssertFalse(MixedConversionService.validate(spans: [
            MixedSpan(kind: "literal", readingStart: 0, readingEnd: 0, text: "abc"),
        ]))
        // 空
        XCTAssertFalse(MixedConversionService.validate(spans: []))
    }

    // ---- convert: 古典変換（weightURL nil = Zenzai 無し）での固定 ----

    func testConvertKeepsLiteralVerbatimAndConvertsJapanese() {
        let svc = ConversionService()
        let spans = [
            span("japanese", 0, "きょうは"),
            span("literal", 4, "Python"),
            span("japanese", 10, "にほんご"),
        ]
        let output = MixedConversionService.convert(service: svc, spans: spans, leftContext: nil, admissionDeadline: nil)
        XCTAssertNotNil(output, "古典変換で日本語区間が変換できる")
        guard let output else { return }
        XCTAssertEqual(output.spans.count, 3)
        XCTAssertEqual(output.spans[1].text, "Python", "Literal は原文を一字不動で保持")
        XCTAssertNil(output.spans[1].candidateToken, "Literal は学習 token を持たない")
        XCTAssertEqual(output.text, output.spans.map(\.text).joined(), "全体は span 表示の連結")
        XCTAssertFalse(output.spans[0].text.isEmpty)
        XCTAssertNotNil(output.spans[0].candidateToken, "Japanese は学習 token を発行")
        XCTAssertNotNil(output.spans[2].candidateToken)
        XCTAssertEqual(output.spans[0].readingStart, 0)
        XCTAssertEqual(output.spans[0].readingEnd, 4)
        XCTAssertEqual(output.spans[1].readingStart, 4)
        XCTAssertEqual(output.spans[1].readingEnd, 10)
        XCTAssertEqual(output.spans[2].readingStart, 10)
    }

    func testConvertRejectsInvalidSpansBeforeTouchingTheConverter() {
        let svc = ConversionService()
        // 検証失敗は変換を呼ばない（nil 即返し）
        XCTAssertNil(MixedConversionService.convert(service: svc, spans: [], leftContext: nil, admissionDeadline: nil))
        XCTAssertNil(MixedConversionService.convert(service: svc, spans: [
            MixedSpan(kind: "literal", readingStart: 0, readingEnd: 9, text: "abc"),
        ], leftContext: nil, admissionDeadline: nil))
    }

    func testLiteralOnlySpansArePureConcatenation() {
        // 日本語区間なし = 変換なしで連結だけ（大文字・記号を保存）。
        let svc = ConversionService()
        let spans = [span("literal", 0, "API"), span("literal", 3, "を"), span("literal", 4, "call")]
        let output = MixedConversionService.convert(service: svc, spans: spans, leftContext: nil, admissionDeadline: nil)
        XCTAssertEqual(output?.text, "APIをcall")
        XCTAssertEqual(output?.spans.map(\.text), ["API", "を", "call"])
    }

    // ---- GPU ワーカー経路（監視付き transport の差し替え。実 GPU 不要）----

    private final class RecordingTransport: GPUWorkerTransport, @unchecked Sendable {
        private let lock = NSLock()
        private(set) var requests: [GPUWorkerRequest] = []
        private var replies: [GPUWorkerTransportReply] = []
        var reply: GPUWorkerTransportReply = .timeout

        func start(generation: UInt64) -> GPUWorkerTransportStartResult {
            .ready(backend: "Vulkan", device: "stub")
        }

        func start(generation: UInt64,
                   configuration: GPUWorkerRuntimeConfiguration?) -> GPUWorkerTransportStartResult {
            .ready(backend: "Vulkan", device: "stub")
        }

        func request(_ request: GPUWorkerRequest, timeout: TimeInterval) -> GPUWorkerTransportReply {
            lock.lock()
            requests.append(request)
            let reply = replies.isEmpty ? self.reply : replies.removeFirst()
            lock.unlock()
            return reply
        }

        func terminate() {}

        func queueReplies(_ replies: [GPUWorkerTransportReply]) {
            lock.lock()
            self.replies.append(contentsOf: replies)
            lock.unlock()
        }
    }

    private func workerService(transport: GPUWorkerTransport) -> ConversionService {
        ConversionService(config: ZenzaiConfig(weightURL: nil, inferenceLimit: 1),
                          processRole: .mainClassicOnly,
                          gpuWorkerSupervisor: GPUWorkerSupervisor(transport: transport))
    }

    /// 十分な期限（GPU 予算 0.9s+余裕 を満たす）。nil は実装の判定で GPU 待機が
    /// 始まらないため、ワーカー経路の試験には必ず期限を与える。
    private func generousDeadline() -> RequestDeadline {
        RequestDeadline(tickMilliseconds: GetTickCount64() + 10_000)
    }

    private func mixedSpans() -> [MixedSpan] {
        [
            span("japanese", 0, "きょうは"),
            span("literal", 4, "Python"),
            span("japanese", 10, "にほんご"),
            span("literal", 14, "v1.2"),
            span("japanese", 18, "をつかう"),
        ]
    }

    func testWorkerCallsAreCappedAtTwoAndRemainingIntervalsStayClassical() {
        // 正常応答するワーカーで日本語3区間を処理する: GPU 待機は予算
        // （mixedZenzaiCallBudget=2）の2区間だけで、3区間目は古典で通る。
        // 正常応答の候補は古典候補と同じ text（単一かな。表面=読み）を返し、
        // rerank の合成で古典候補オブジェクトが再利用される形にする。
        let transport = RecordingTransport()
        transport.queueReplies([
            .response(GPUWorkerResponse(requestID: 1, generation: 1, mainResults: ["あ"], firstClauseResults: ["あ"])),
            .response(GPUWorkerResponse(requestID: 2, generation: 1, mainResults: ["い"], firstClauseResults: ["い"])),
        ])
        let svc = workerService(transport: transport)
        let spans = [
            span("japanese", 0, "あ"),
            span("literal", 1, "Python"),
            span("japanese", 7, "い"),
            span("literal", 8, "v1.2"),
            span("japanese", 12, "う"),
        ]
        let output = MixedConversionService.convert(service: svc, spans: spans,
                                                    leftContext: nil, admissionDeadline: generousDeadline())
        XCTAssertNotNil(output)
        XCTAssertEqual(transport.requests.count, ConversionService.mixedZenzaiCallBudget,
                       "GPU 待機は予算の2区間だけ。3区間目は古典")
        XCTAssertEqual(output?.spans[1].text, "Python")
        XCTAssertEqual(output?.spans[3].text, "v1.2")
        XCTAssertEqual(output?.text, output?.spans.map(\.text).joined())
    }

    func testWorkerFailureKeepsLiteralAndClassicalConversion() {
        // ワーカーが常に失敗（timeout）しても、日本語区間は古典 pool で変換され、
        // Literal は原文を一字不動で保持する（障害時保持 — §8.5）。
        let transport = RecordingTransport()
        let svc = workerService(transport: transport)
        let output = MixedConversionService.convert(service: svc, spans: mixedSpans(),
                                                    leftContext: nil, admissionDeadline: generousDeadline())
        XCTAssertNotNil(output, "古典 pool で変換が成立する")
        guard let output else { return }
        XCTAssertEqual(output.spans[1].text, "Python")
        XCTAssertEqual(output.spans[3].text, "v1.2")
        XCTAssertNil(output.spans[3].candidateToken)
        XCTAssertEqual(output.text, output.spans.map(\.text).joined())
        XCTAssertGreaterThan(transport.requests.count, 0, "期限を与えた試験はワーカー経路を通る")
        XCTAssertLessThanOrEqual(transport.requests.count,
                                 ConversionService.mixedZenzaiCallBudget,
                                 "GPU ワーカー呼出は予算を超えない")
    }

    func testWorkerCrashReplyKeepsLiteralAndClassicalConversion() {
        // クラッシュ応答でも同じく Literal 保持・古典変換が成立する。
        let transport = RecordingTransport()
        transport.reply = .exit
        let svc = workerService(transport: transport)
        let output = MixedConversionService.convert(service: svc, spans: mixedSpans(),
                                                    leftContext: nil, admissionDeadline: generousDeadline())
        XCTAssertNotNil(output)
        XCTAssertEqual(output?.spans[1].text, "Python")
        XCTAssertEqual(output?.spans[3].text, "v1.2")
        XCTAssertGreaterThan(transport.requests.count, 0, "期限を与えた試験はワーカー経路を通る")
    }

    func testTightDeadlineSkipsGpuWaitEntirely() {
        // 要求全体の残り予算がワーカー予算（0.9s+余裕）に満たないときは GPU 待機を
        // 始めず古典で通す（§6.4: 残り時間を各区間へ渡す）。残り 600ms は古典変換
        // （この環境で 3 区間 ≈ 300ms）には足りるが GPU 予算には足りない。
        let transport = RecordingTransport()
        let svc = workerService(transport: transport)
        let deadline = RequestDeadline(tickMilliseconds: GetTickCount64() + 600)
        let output = MixedConversionService.convert(service: svc, spans: mixedSpans(),
                                                    leftContext: nil, admissionDeadline: deadline)
        XCTAssertNotNil(output, "古典変換は成立する")
        XCTAssertEqual(output?.spans[1].text, "Python")
        XCTAssertEqual(transport.requests.count, 0, "残り予算不足では GPU 待機を始めない")
    }
}
