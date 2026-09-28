import Foundation

/// Explicitly selected Microsoft conversion candidates. The native API supplies only
/// surfaces, so this store keeps the verified full reading without passing synthetic
/// dictionary elements to AzooKey's learning memory.
final class MicrosoftLearningStore: @unchecked Sendable {
    static let maxEntries = 512

    struct Entry: Codable, Equatable {
        let reading: String
        let surface: String
    }

    private struct FileFormat: Codable {
        let version: Int
        let entries: [Entry]
    }

    private let fileURL: URL?
    private var entries: [Entry] = []
    private var loaded = false
    private var dirty = false
    private var generation: UInt64 = 0

    init(directory: URL?) {
        fileURL = directory?.appendingPathComponent("microsoft-candidates.json")
    }

    func record(reading: String, surface: String) {
        guard let key = Self.validReading(reading), Self.validSurface(surface) else { return }
        loadIfNeeded()
        let entry = Entry(reading: key, surface: surface)
        entries.removeAll { $0 == entry }
        entries.insert(entry, at: 0)
        if entries.count > Self.maxEntries { entries.removeLast(entries.count - Self.maxEntries) }
        dirty = true
        generation &+= 1
    }

    func surfaces(reading: String) -> [String] {
        guard let key = Self.validReading(reading) else { return [] }
        loadIfNeeded()
        return Array(entries.lazy.filter { $0.reading == key }.prefix(16).map(\.surface))
    }

    func clearMemory() {
        entries = []
        loaded = true
        dirty = false
        generation &+= 1
    }

    struct PersistenceSnapshot: @unchecked Sendable {
        fileprivate let generation: UInt64
        fileprivate let url: URL
        fileprivate let data: Data
    }

    func persistenceSnapshot() -> PersistenceSnapshot? {
        guard dirty, let url = fileURL else { return nil }
        let payload = FileFormat(version: 1, entries: entries)
        guard let data = try? JSONEncoder().encode(payload) else { return nil }
        return PersistenceSnapshot(generation: generation, url: url, data: data)
    }

    static func persist(_ snapshot: PersistenceSnapshot) -> Bool {
        (try? snapshot.data.write(to: snapshot.url, options: .atomic)) != nil
    }

    func acknowledgePersistence(_ snapshot: PersistenceSnapshot) {
        if generation == snapshot.generation { dirty = false }
    }

    func flush() {
        guard let snapshot = persistenceSnapshot() else { return }
        if Self.persist(snapshot) { acknowledgePersistence(snapshot) }
    }

    private static func validReading(_ reading: String) -> String? {
        guard let key = CorrectionStore.normalizedKey(reading), key.unicodeScalars.count <= 64 else { return nil }
        return key
    }

    private static func validSurface(_ surface: String) -> Bool {
        !surface.isEmpty && surface.unicodeScalars.count <= 300
    }

    private func loadIfNeeded() {
        guard !loaded else { return }
        loaded = true
        guard let url = fileURL,
              let data = try? Data(contentsOf: url),
              let payload = try? JSONDecoder().decode(FileFormat.self, from: data),
              payload.version == 1 else { return }
        entries = Array(payload.entries.filter {
            Self.validReading($0.reading) == $0.reading && Self.validSurface($0.surface)
        }.prefix(Self.maxEntries))
    }
}
