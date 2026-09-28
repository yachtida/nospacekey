import Foundation
import WinSDK

/// Pinned vendor format (80b8204): validate every node before letting its unchecked
/// binary readers see an inherited store. A successful sample conversion is insufficient.
enum LearningStoreValidation {
    enum Invalid: Error { case format, unavailable }
    static func integer(_ data: Data, _ offset: Int, _ size: Int) throws -> UInt64 {
        guard offset >= 0, size > 0, offset <= data.count - size else { throw Invalid.format }
        return (0..<size).reduce(0) { $0 | UInt64(data[offset + $1]) << (8 * $1) }
    }

    static func validate(_ files: [String: Data]) throws {
        if let data = files["microsoft-candidates.json"] {
            struct MicrosoftCandidates: Decodable { let version: Int; let entries: [MicrosoftLearningStore.Entry] }
            let decoded = try JSONDecoder().decode(MicrosoftCandidates.self, from: data)
            guard decoded.version == 1, decoded.entries.count <= MicrosoftLearningStore.maxEntries,
                  decoded.entries.allSatisfy({ entry in
                      CorrectionStore.normalizedKey(entry.reading) == entry.reading &&
                      entry.reading.unicodeScalars.count <= 64 &&
                      !entry.surface.isEmpty && entry.surface.unicodeScalars.count <= 300
                  }) else { throw Invalid.format }
        }
        if files.count == 1, files["microsoft-candidates.json"] != nil { return }
        guard let metadata = files["memory.memorymetadata"],
              let chars = files["memory.loudschars2"], let louds = files["memory.louds"] else { throw Invalid.format }
        let nodes = Int(try integer(metadata, 0, 4))
        guard (2...1_000_000).contains(nodes), chars.count == nodes,
              chars[0] == 0, chars[1] == 0, louds.count == ((2 * nodes - 1 + 63) / 64) * 8 else { throw Invalid.format }
        // Breadth-first LOUDS has a virtual root "10", followed by each node's
        // children and a zero terminator. All trailing padding bits must be one.
        var bit = 0
        func nextBit() throws -> Bool {
            let word = try integer(louds, (bit / 64) * 8, 8)
            defer { bit += 1 }
            return word & (UInt64(1) << (63 - bit % 64)) != 0
        }
        guard try nextBit(), try !nextBit() else { throw Invalid.format }
        var discovered = 2
        for node in 1..<nodes {
            guard node < discovered else { throw Invalid.format }
            var last: UInt8?
            while try nextBit() {
                guard discovered < nodes, chars[discovered] > 0,
                      last.map({ chars[discovered] > $0 }) ?? true else { throw Invalid.format }
                last = chars[discovered]
                discovered += 1
            }
        }
        guard discovered == nodes else { throw Invalid.format }
        while bit < louds.count * 8 { guard try nextBit() else { throw Invalid.format } }

        var metadataOffset = 4
        var total = 0
        for shard in 0..<((nodes + 2047) / 2048) {
            guard let data = files["memory\(shard).loudstxt3"] else { throw Invalid.format }
            let count = Int(try integer(data, 0, 2))
            guard count == min(2048, nodes - total) else { throw Invalid.format }
            let header = 2 + 4 * count
            var expectedStart = header
            for index in 0..<count {
                let start = Int(try integer(data, 2 + index * 4, 4))
                let end = index + 1 < count ? Int(try integer(data, 6 + index * 4, 4)) : data.count
                guard start == expectedStart, end >= start + 2, end <= data.count else { throw Invalid.format }
                expectedStart = end
                let entries = Int(try integer(data, start, 2))
                let metadataCount = Int(try integer(metadata, metadataOffset, 1))
                // MemoryLayout<MetadataElement>.size is five bytes on Windows x64.
                guard entries == metadataCount, metadataOffset + 1 + entries * 5 <= metadata.count,
                      start + 2 + entries * 10 <= end else { throw Invalid.format }
                metadataOffset += 1 + entries * 5
                for entry in 0..<entries {
                    let offset = start + 2 + entry * 10
                    guard try integer(data, offset, 2) < 1319,
                          try integer(data, offset + 2, 2) < 1319,
                          try integer(data, offset + 4, 2) < 502,
                          Float(bitPattern: UInt32(try integer(data, offset + 6, 4))).isFinite else { throw Invalid.format }
                }
                let strings = data[(start + 2 + entries * 10)..<end]
                guard let text = String(data: strings, encoding: .utf8) else { throw Invalid.format }
                if entries == 0 {
                    guard text.isEmpty else { throw Invalid.format }
                } else {
                    let fields = text.split(separator: "\t", omittingEmptySubsequences: false)
                    guard fields.count == entries + 1, !fields[0].isEmpty,
                          !text.contains("\0") else { throw Invalid.format }
                }
            }
            total += count
        }
        guard metadataOffset == metadata.count else { throw Invalid.format }
        if let data = files["corrections.json"] {
            struct Corrections: Decodable { let version: Int; let entries: [CorrectionStore.Entry] }
            let decoded = try JSONDecoder().decode(Corrections.self, from: data)
            guard decoded.version == 1, decoded.entries.count <= CorrectionStore.maxEntries else { throw Invalid.format }
        }
    }
}

/// Deny writes/deletes while taking a snapshot. The old vendor writes .pause before
/// replacing canonical files; checking it after all leases detects an in-flight save.
final class LearningFileLease {
    private var ownedHandle: HANDLE?
    var handle: HANDLE { ownedHandle! }
    init(_ url: URL, directory: Bool = false, deletable: Bool = false) throws {
        let opened = url.path.withCString(encodedAs: UTF16.self) {
            CreateFileW($0, DWORD(directory ? 0x80 : GENERIC_READ) | (deletable ? DWORD(DELETE) : 0),
                        DWORD(directory ? FILE_SHARE_READ | FILE_SHARE_WRITE : FILE_SHARE_READ),
                        nil, DWORD(OPEN_EXISTING),
                        DWORD(FILE_FLAG_OPEN_REPARSE_POINT | (directory ? FILE_FLAG_BACKUP_SEMANTICS : 0)), nil)
        }
        guard let opened, opened != INVALID_HANDLE_VALUE else { throw LearningStoreValidation.Invalid.unavailable }
        var info = BY_HANDLE_FILE_INFORMATION()
        guard GetFileInformationByHandle(opened, &info),
              info.dwFileAttributes & DWORD(FILE_ATTRIBUTE_REPARSE_POINT) == 0,
              (info.dwFileAttributes & DWORD(FILE_ATTRIBUTE_DIRECTORY) != 0) == directory else {
            CloseHandle(opened)
            throw LearningStoreValidation.Invalid.format
        }
        ownedHandle = opened
    }
    func close() {
        if let handle = ownedHandle { CloseHandle(handle); ownedHandle = nil }
    }
    deinit { close() }
    func delete() throws {
        var info = FILE_DISPOSITION_INFO()
        // FILE_DISPOSITION_INFO contains one BOOLEAN. The SDK macro for DeleteFile
        // obscures its field name in Swift, so initialize that byte directly.
        withUnsafeMutableBytes(of: &info) { $0[0] = 1 }
        guard SetFileInformationByHandle(handle, FileDispositionInfo, &info,
                                        DWORD(MemoryLayout<FILE_DISPOSITION_INFO>.size)) else {
            throw LearningStoreValidation.Invalid.format
        }
        close()
    }
    func move(to destination: URL) throws {
        let name = Array(destination.path.replacingOccurrences(of: "/", with: "\\").utf16)
        guard let offset = MemoryLayout<FILE_RENAME_INFO>.offset(of: \.FileName) else {
            throw LearningStoreValidation.Invalid.format
        }
        let size = max(MemoryLayout<FILE_RENAME_INFO>.size, offset + (name.count + 1) * 2)
        let buffer = UnsafeMutableRawPointer.allocate(byteCount: size, alignment: MemoryLayout<FILE_RENAME_INFO>.alignment)
        defer { buffer.deallocate() }
        buffer.initializeMemory(as: UInt8.self, repeating: 0, count: size)
        let info = buffer.assumingMemoryBound(to: FILE_RENAME_INFO.self)
        // Zeroed ReplaceIfExists/RootDirectory: publish only into an absent destination.
        info.pointee.FileNameLength = DWORD(name.count * 2)
        name.withUnsafeBytes { bytes in
            buffer.advanced(by: offset).copyMemory(from: bytes.baseAddress!, byteCount: bytes.count)
        }
        guard SetFileInformationByHandle(handle, FileRenameInfo, buffer, DWORD(size)) else {
            throw LearningStoreValidation.Invalid.format
        }
    }
    func read() throws -> Data {
        var size = LARGE_INTEGER()
        guard GetFileSizeEx(handle, &size) else { throw LearningStoreValidation.Invalid.unavailable }
        guard size.QuadPart >= 0, size.QuadPart <= 64 * 1024 * 1024 else {
            throw LearningStoreValidation.Invalid.format
        }
        var data = Data(count: Int(size.QuadPart))
        var read: DWORD = 0
        let ok = data.withUnsafeMutableBytes { buffer in
            ReadFile(handle, buffer.baseAddress, DWORD(buffer.count), &read, nil)
        }
        guard ok, Int(read) == data.count else { throw LearningStoreValidation.Invalid.unavailable }
        return data
    }
}

enum LearningMigration {
    static let marker = ".learning-initialized"
    static func stageName(_ version: String) -> String { ".learning-inheritance-" + version }
    enum Outcome: String { case inherited, existing, empty, rejected, unavailable, disabled }
    struct Candidate { let directory: URL; let modified: Date }

    static func isEarlierBuild(_ name: String, than version: String) -> Bool {
        guard name.count <= 80, name.utf8.first.map({ (48...57).contains($0) }) == true,
              name.utf8.allSatisfy({ (48...57).contains($0) || (65...90).contains($0) ||
                  (97...122).contains($0) || $0 == 43 || $0 == 45 || $0 == 46 }) else { return false }
        let candidate = name.split(separator: "+", maxSplits: 1)[0].split(separator: "-", maxSplits: 1)
        let current = version.split(separator: "+", maxSplits: 1)[0].split(separator: "-", maxSplits: 1)
        let coreOrder = String(candidate[0]).compare(String(current[0]), options: .numeric)
        if coreOrder != .orderedSame { return coreOrder == .orderedAscending }
        // A release inherits its prereleases; build metadata alone does not make a version newer.
        if candidate.count != current.count { return candidate.count > current.count }
        return candidate.count == 2 &&
            String(candidate[1]).compare(String(current[1]), options: .numeric) == .orderedAscending
    }

    static func candidates(base: URL, version: String) throws -> [Candidate] {
        let manager = FileManager.default
        let children = try manager.contentsOfDirectory(at: base, includingPropertiesForKeys: nil)
        let roots = [base] + children.filter {
            let name = $0.lastPathComponent
            return isEarlierBuild(name, than: version)
        }
        return try roots.compactMap { directory in
            guard let attributes = try learningPathMetadata(for: directory), attributes.isDirectory,
                  !attributes.isReparsePoint else { return nil }
            let metadata = directory.appendingPathComponent("memory.memorymetadata")
            // Vendor removes canonical metadata between creating .pause and copying .2.
            // Never interpret that window as "no history" or select a different older store.
            let pause = directory.appendingPathComponent(".pause")
            if try learningPathMetadata(for: pause) != nil { throw LearningStoreValidation.Invalid.unavailable }
            let microsoft = directory.appendingPathComponent("microsoft-candidates.json")
            let microsoftItem = try learningPathMetadata(for: microsoft)
            if let microsoftItem {
                guard microsoftItem.isRegularFile, !microsoftItem.isReparsePoint else {
                    throw LearningStoreValidation.Invalid.format
                }
            }
            guard let item = try learningPathMetadata(for: metadata) else {
                if try learningPathMetadata(for: directory.appendingPathComponent("memory.memorymetadata.2")) != nil ||
                    learningPathMetadata(for: pause) != nil { throw LearningStoreValidation.Invalid.unavailable }
                guard microsoftItem != nil else { return nil }
                let modified = try manager.attributesOfItem(atPath: microsoft.path)[.modificationDate] as? Date ?? .distantPast
                return Candidate(directory: directory, modified: modified)
            }
            guard item.isRegularFile, !item.isReparsePoint else { throw LearningStoreValidation.Invalid.format }
            let modified = try manager.attributesOfItem(atPath: metadata.path)[.modificationDate] as? Date ?? .distantPast
            return Candidate(directory: directory, modified: modified)
        }.sorted {
            $0.modified == $1.modified ? $0.directory.path < $1.directory.path : $0.modified > $1.modified
        }
    }

    static func prepare(environment: [String: String], learning: LearningSettings) -> Outcome {
        guard learning.enabled, let target = learning.memoryDir,
              let base = LearningSettings.resolveBaseDir(environment: environment),
              let scope = LearningSettings.coordinationScope(environment: environment),
              target.standardizedFileURL == base.appendingPathComponent(BuildInfo.version).standardizedFileURL else { return .disabled }
        // Reload holds converterLock. Config may hold this gate while waiting for ClearLearning.
        // Do not wait and invert that lock order.
        guard let gate = LearningLifecycleGate(name: LearningSettings.lifecycleMutexName(scope: scope), timeoutMs: 0) else { return .unavailable }
        defer { gate.release() }
        let result = inherit(base: base, version: BuildInfo.version)
        engineLog("ev=learning_migration result=\(result.rawValue)\n")
        return result
    }

    /// Caller holds the same lifecycle gate as startup and history deletion.
    static func inherit(base: URL, version: String) -> Outcome {
        let manager = FileManager.default
        let target = base.appendingPathComponent(version)
        let stage = base.appendingPathComponent(stageName(version))
        do {
            // Keep every ancestor fixed; an ancestor junction must not redirect the snapshot.
            var ancestors: [LearningFileLease] = []
            var current = base.standardizedFileURL
            while true {
                ancestors.append(try LearningFileLease(current, directory: true))
                let parent = current.deletingLastPathComponent()
                if parent.path == current.path || parent.path == "/" { break }
                current = parent
            }
            return try withExtendedLifetime(ancestors) {
                try removeStage(stage)
                if try learningPathMetadata(for: target) == nil {
                    try manager.createDirectory(at: target, withIntermediateDirectories: false)
                }
                let targetLease = try LearningFileLease(target, directory: true, deletable: true)
                let names = try manager.contentsOfDirectory(atPath: target.path)
                if !names.isEmpty { return .existing }
                // Only a fresh, empty destination is eligible. The marker survives ClearLearning,
                // so an intentionally emptied store never imports previously deleted history.
                let files: [String: Data]
                do {
                    guard let snapshot = try inheritanceSnapshot(base: base, version: version) else {
                        try Data().write(to: target.appendingPathComponent(marker), options: .withoutOverwriting)
                        return .empty
                    }
                    files = snapshot
                } catch LearningStoreValidation.Invalid.format {
                    try Data().write(to: target.appendingPathComponent(marker), options: .withoutOverwriting)
                    return .rejected
                } catch { return .unavailable }
                // A stage left by interrupted publication contains only our allowlisted files.
                try manager.createDirectory(at: stage, withIntermediateDirectories: false)
                do {
                    let stageLease = try LearningFileLease(stage, directory: true, deletable: true)
                    try Data().write(to: stage.appendingPathComponent(marker), options: .withoutOverwriting)
                    for (name, data) in files {
                        try data.write(to: stage.appendingPathComponent(name), options: .withoutOverwriting)
                    }
                    // Both operations target the verified handles. No path-based reopen can
                    // follow a replacement junction between writing and publication.
                    try targetLease.delete()
                    try stageLease.move(to: target)
                    return .inherited
                } catch {
                    try? removeStage(stage)
                    throw error
                }
            }
        } catch {
            return .unavailable
        }
    }

    private static func inheritanceSnapshot(base: URL, version: String) throws -> [String: Data]? {
        func attempt() throws -> [String: Data]? {
            guard let source = try candidates(base: base, version: version).first else { return nil }
            return try snapshot(source.directory)
        }
        do { return try attempt() }
        catch LearningStoreValidation.Invalid.format { throw LearningStoreValidation.Invalid.format }
        catch {
            // Include candidate discovery in the retry: canonical files can temporarily
            // disappear while the vendor publishes several files behind .pause.
            Thread.sleep(forTimeInterval: 0.025)
            return try attempt()
        }
    }

    static func snapshot(_ source: URL) throws -> [String: Data] {
        let directoryLease = try LearningFileLease(source, directory: true)
        return try withExtendedLifetime(directoryLease) {
            guard !FileManager.default.fileExists(atPath: source.appendingPathComponent(".pause").path) else {
                throw LearningStoreValidation.Invalid.unavailable
            }
            let metadataURL = source.appendingPathComponent("memory.memorymetadata")
            if try learningPathMetadata(for: metadataURL) == nil {
                let microsoftLease = try LearningFileLease(source.appendingPathComponent("microsoft-candidates.json"))
                let files = ["microsoft-candidates.json": try microsoftLease.read()]
                guard !FileManager.default.fileExists(atPath: source.appendingPathComponent(".pause").path) else {
                    throw LearningStoreValidation.Invalid.unavailable
                }
                do { try LearningStoreValidation.validate(files) }
                catch { throw LearningStoreValidation.Invalid.format }
                return files
            }
            let metadataLease = try LearningFileLease(source.appendingPathComponent("memory.memorymetadata"))
            let metadata = try metadataLease.read()
            let nodes = Int(try LearningStoreValidation.integer(metadata, 0, 4))
            guard (2...1_000_000).contains(nodes) else { throw LearningStoreValidation.Invalid.format }
            var leases = [metadataLease]
            var files = ["memory.memorymetadata": metadata]
            var names = ["memory.louds", "memory.loudschars2"] + (0..<((nodes + 2047) / 2048)).map { "memory\($0).loudstxt3" }
            if FileManager.default.fileExists(atPath: source.appendingPathComponent("corrections.json").path) { names.append("corrections.json") }
            if FileManager.default.fileExists(atPath: source.appendingPathComponent("microsoft-candidates.json").path) { names.append("microsoft-candidates.json") }
            for name in names {
                let lease = try LearningFileLease(source.appendingPathComponent(name))
                leases.append(lease)
                files[name] = try lease.read()
                guard files.values.reduce(0, { $0 + $1.count }) <= 64 * 1024 * 1024 else { throw LearningStoreValidation.Invalid.format }
            }
            return try withExtendedLifetime(leases) {
                guard !FileManager.default.fileExists(atPath: source.appendingPathComponent(".pause").path) else { throw LearningStoreValidation.Invalid.unavailable }
                do { try LearningStoreValidation.validate(files) }
                catch { throw LearningStoreValidation.Invalid.format }
                return files
            }
        }
    }

    static func removeStage(_ stage: URL) throws {
        guard let metadata = try learningPathMetadata(for: stage) else { return }
        guard metadata.isDirectory, !metadata.isReparsePoint else { throw LearningStoreValidation.Invalid.format }
        let stageLease = try LearningFileLease(stage, directory: true, deletable: true)
        let names = try FileManager.default.contentsOfDirectory(atPath: stage.path)
        guard names.isEmpty || names.contains(marker) else { throw LearningStoreValidation.Invalid.format }
        for name in names {
            guard name == marker || ConversionService.isLearningArtifactName(name),
                  let item = try learningPathMetadata(for: stage.appendingPathComponent(name)),
                  item.isRegularFile, !item.isReparsePoint else { throw LearningStoreValidation.Invalid.format }
        }
        // Preflight every handle before deleting anything; DeleteFile disposition never
        // recursively removes a directory substituted for a regular artifact.
        let files = try names.map { try LearningFileLease(stage.appendingPathComponent($0), deletable: true) }
        for file in files { try file.delete() }
        try stageLease.delete()
    }
}
