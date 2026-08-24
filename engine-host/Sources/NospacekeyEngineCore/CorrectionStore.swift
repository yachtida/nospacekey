import Foundation

/// 「正規化読み(ひらがな) → ユーザが訂正として選んだ表層」の 1:1 テーブル(最新勝ち・LRU)。
/// Zenzai 有効時に学習が最終ランキングへ勝てない構造(spec 2026-07-30-correction-promotion 背景)
/// への後段対策で、候補列確定後の先頭昇格(ConversionService)が読む。
/// lock を持たない: 直列化は呼び出し側(ConversionService が converterLock 下で触る —
/// クラス doc のロック規律一覧参照。ForTesting 系観測窓のみ無ロック)。
/// vendor の学習メモリに相乗りしない理由: 学習 value は Zenzai 経路で順位から捨てられるため、
/// 「必ず 1 位」の保証はエンジン側の独立テーブルでしか作れない。
final class CorrectionStore {
    static let maxEntries = 1000

    struct Entry: Codable, Equatable {
        let reading: String
        let surface: String
    }
    private struct FileFormat: Codable {
        let version: Int
        let entries: [Entry]
    }

    /// LRU 順(先頭が最新)。lookup では並び替えない — 参照で寿命が伸びると
    /// 誤登録が永久に消えなくなる。訂正(record)だけが寿命を更新する。
    private var entries: [Entry] = []
    private var dirty = false
    private var loaded = false
    private let fileURL: URL?

    /// テスト専用の観測窓(LRU 順序の直接検査用 — 間接観測は追い出し境界に依存し脆い)。
    var entriesForTesting: [Entry] { loadIfNeeded(); return entries }

    init(directory: URL?) {
        self.fileURL = directory?.appendingPathComponent("corrections.json")
    }

    /// 記録可の条件: 正規化後が非空かつ「ひらがな/ー」のみ。
    /// 非かな読み(ASCII・漢字混在)は昇格キーとして意味を成さないので無言に棄却する。
    /// internal なのは ConversionService の記録可否マップがキー判定を共有するため
    /// (判定が二重定義だと片方だけ緩んで偽成功ログの温床になる)。
    static func normalizedKey(_ reading: String) -> String? {
        let key = ConversionService.normalizeKana(reading)
        guard !key.isEmpty else { return nil }
        let isKana = key.unicodeScalars.allSatisfy { u in
            (0x3041...0x3096).contains(u.value) || u.value == 0x30FC
        }
        return isKana ? key : nil
    }

    func record(reading: String, surface: String) {
        guard let key = Self.normalizedKey(reading), !surface.isEmpty else { return }
        loadIfNeeded()
        entries.removeAll { $0.reading == key }
        entries.insert(Entry(reading: key, surface: surface), at: 0)
        if entries.count > Self.maxEntries { entries.removeLast(entries.count - Self.maxEntries) }
        dirty = true
    }

    /// un-learn: モデル1位の明示選択(=昇格の拒否)で呼ぶ。record と同じ正規化キーで消す。
    /// これが無いと誤登録した訂正の除去手段が ClearLearning(学習ごと全消し)しか無い。
    /// 戻り値 false は不在(flush 不要の判定に使う)。
    @discardableResult
    func remove(reading: String) -> Bool {
        guard let key = Self.normalizedKey(reading) else { return false }
        loadIfNeeded()
        let before = entries.count
        entries.removeAll { $0.reading == key }
        guard entries.count != before else { return false }
        dirty = true
        return true
    }

    func lookup(reading: String) -> String? {
        guard let key = Self.normalizedKey(reading) else { return nil }
        loadIfNeeded()
        return entries.first { $0.reading == key }?.surface
    }

    /// RAM だけを消す。disk は ConversionService の learning-root preflight を通してから
    /// seam 経由で消すため、clearLearning の partial deletion を防ぐ。
    func clearMemory() {
        entries = []
        dirty = false
        loaded = true
    }

    /// 単体利用時の clear。CorrectionStore 自身でも regular non-reparse file を確認し、
    /// corrections.json が directory/symlink/junction のときは fail-closed（void API のため
    /// no-op）にする。ConversionService は clearMemory()+共通 preflight を使用する。
    func clear() {
        clearMemory()
        guard let url = fileURL else { return }
        let metadata: LearningPathMetadata?
        do {
            metadata = try learningPathMetadata(for: url)
        } catch {
            return
        }
        guard let metadata, metadata.isRegularFile,
              !metadata.isDirectory, !metadata.isReparsePoint else { return }
        try? FileManager.default.removeItem(at: url)
    }

    /// dirty 時のみ atomic write。非 throw(失敗は次の flush 契機で自然再試行)。
    /// replaceItemAt を使わないのは置換対象の存在が前提の API で、corrections.json 未作成の
    /// 初回 flush が必ず throw するため。`.atomic` は内部で temp+rename を行い対象不在でも成功する。
    func flush() {
        guard dirty, let url = fileURL else { return }
        let payload = FileFormat(version: 1, entries: entries)
        guard let data = try? JSONEncoder().encode(payload) else { return }
        if (try? data.write(to: url, options: .atomic)) != nil {
            dirty = false
        }
    }

    private func loadIfNeeded() {
        guard !loaded else { return }
        loaded = true
        guard let url = fileURL,
              let data = try? Data(contentsOf: url),
              let payload = try? JSONDecoder().decode(FileFormat.self, from: data),
              payload.version == 1 else { return }   // 破損・版不一致は空で開始(非 throw)
        entries = Array(payload.entries.prefix(Self.maxEntries))
    }
}
