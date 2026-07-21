import XCTest
import Foundation
import KanaKanjiConverterModuleWithDefaultDictionary
@testable import NospacekeyEngineCore

/// Spec2 最優先リスクゲート: Windows で「学習ファイルをロード（mmap の可能性）した状態での
/// 上書きフラッシュ」が共有違反で失敗しないかの実証。ライブラリの上書きは removeItem+copyItem、
/// 読みは mappedIfSafe（LearningMemory.swift:68-75 / extension LOUDS.swift:39-47）。
/// このテストが FAIL する場合はフラッシュ設計の見直しが必要（計画の他タスクへ進まない）。
final class LearningSmokeTests: XCTestCase {
    func testFlushWhileMemoryLoadedDoesNotFailOnWindows() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("nospacekey-learn-smoke-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        let converter = KanaKanjiConverter.withDefaultDictionary()
        func options() -> ConvertRequestOptions {
            .init(N_best: 5, requireJapanesePrediction: false, requireEnglishPrediction: false,
                  keyboardLanguage: .ja_JP, learningType: .inputAndOutput,
                  memoryDirectoryURL: dir, sharedContainerURL: dir,
                  textReplacer: .withDefaultEmojiDictionary(), specialCandidateProviders: nil,
                  zenzaiMode: .off, metadata: .init(versionString: "smoke"))
        }
        func composing(_ roman: String) -> ComposingText {
            var c = ComposingText()
            c.insertAtCursorPosition(roman, inputStyle: .roman2kana)
            return c
        }
        // 1周目: 変換→学習→フラッシュ（学習ファイル生成）
        let r1 = converter.requestCandidates(composing("nihongo"), options: options()).mainResults
        XCTAssertFalse(r1.isEmpty)
        converter.updateLearningData(r1[0])
        converter.commitUpdateLearningData()
        let files1 = try FileManager.default.contentsOfDirectory(atPath: dir.path)
        XCTAssertTrue(files1.contains("memory.louds"), "フラッシュで学習ファイルが生成されるはず: \(files1)")
        XCTAssertFalse(files1.contains(".pause"), "1周目フラッシュ後に .pause が残らない")
        // 2周目: 変換（ここで memory.louds をロード=mmap の可能性）→学習→**上書き**フラッシュ
        converter.stopComposition()
        let r2 = converter.requestCandidates(composing("nihongo"), options: options()).mainResults
        XCTAssertFalse(r2.isEmpty)
        converter.updateLearningData(r2[0])
        converter.commitUpdateLearningData()   // ← Windows 共有違反ならここで merge が壊れる
        let files2 = try FileManager.default.contentsOfDirectory(atPath: dir.path)
        XCTAssertFalse(files2.contains(".pause"), "上書きフラッシュ後に .pause が残らない（merge 途中失敗の兆候）")
        XCTAssertTrue(files2.contains("memory.louds"), "上書き後も学習ファイルが存在する: \(files2)")
        // 3周目: 上書き後のファイルが読めて変換が動く（torn file なら異常になる）
        converter.stopComposition()
        let r3 = converter.requestCandidates(composing("nihongo"), options: options()).mainResults
        XCTAssertFalse(r3.isEmpty, "再フラッシュ後の変換が動くはず（学習ファイル破損の疑い）")
    }
}
