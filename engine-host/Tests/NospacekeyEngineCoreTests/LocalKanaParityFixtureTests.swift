import Foundation
import XCTest
import KanaKanjiConverterModuleWithDefaultDictionary

final class LocalKanaParityFixtureTests: XCTestCase {
    func testSharedFixtureMatchesPinnedAzooKeyComposingTextTrajectory() throws {
        let fixture = try String(contentsOf: fixtureURL(), encoding: .utf8)
        let fixtureRevision = try fixtureRevision(fixture)
        XCTAssertEqual(fixtureRevision, try packageResolvedRevision())
        XCTAssertEqual(fixtureRevision, try packageSwiftRevision())

        for fixtureCase in try fixtureCases(fixture) {
            var composing = ComposingText()
            var event = 0
            for operation in fixtureCase.operations {
                switch operation {
                case let .kana(payload):
                    for character in payload {
                        composing.insertAtCursorPosition(String(character), inputStyle: .roman2kana)
                        XCTAssertEqual(composing.convertTarget, fixtureCase.trajectory[event], "fixture=\(fixtureCase.name) event=\(event + 1)")
                        event += 1
                    }
                case let .direct(payload):
                    for character in payload {
                        composing.insertAtCursorPosition(String(character), inputStyle: .direct)
                        XCTAssertEqual(composing.convertTarget, fixtureCase.trajectory[event], "fixture=\(fixtureCase.name) event=\(event + 1)")
                        event += 1
                    }
                case .backspace:
                    composing.deleteBackwardFromCursorPosition(count: 1)
                    XCTAssertEqual(composing.convertTarget, fixtureCase.trajectory[event], "fixture=\(fixtureCase.name) event=\(event + 1)")
                    event += 1
                }
            }
        }
    }

    func testFixtureRejectsMalformedNonCommentRows() {
        for malformed in ["missing\tK:a\n", "empty\tK:a\t\n", "unknown\tQ:a\ta\n", "# metadata\n"] {
            XCTAssertThrowsError(try fixtureCases(malformed), "fixture=\(malformed.debugDescription)")
        }
    }

    /// TIP は MS IME 準拠で Backspace 後の n を再結合可能にするため、この軌道だけ
    /// 仕様が両者で異なる(共有 fixture から外した経緯は fixtures/local-kana-parity.tsv の
    /// コメントと docs/plans/2026-09-01-alive-n-after-backspace-msime.md 参照)。
    /// ここでは上流 AzooKey (KanaKanjiConverter) の凍結挙動を固定して、
    /// 依存パッケージ更新時に意図せず変わっていないかを検出する。
    func testEngineFreezesBackspacedNForAzooKeyParityWhileTipDiverges() {
        var composing = ComposingText()
        for character in "ny" {
            composing.insertAtCursorPosition(String(character), inputStyle: .roman2kana)
        }
        XCTAssertEqual(composing.convertTarget, "ny")
        composing.deleteBackwardFromCursorPosition(count: 1)
        XCTAssertEqual(composing.convertTarget, "n")
        composing.insertAtCursorPosition("a", inputStyle: .roman2kana)
        XCTAssertEqual(composing.convertTarget, "nあ")
    }

    private func fixtureURL() -> URL {
        repositoryURL().appendingPathComponent("fixtures/local-kana-parity.tsv")
    }

    private func packageResolvedURL() -> URL {
        repositoryURL().appendingPathComponent("engine-host/Package.resolved")
    }

    private func packageSwiftURL() -> URL {
        repositoryURL().appendingPathComponent("engine-host/Package.swift")
    }

    private func repositoryURL() -> URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
    }

    private func fixtureRevision(_ fixture: String) throws -> String {
        let prefix = "# AzooKeyKanaKanjiConverter revision "
        let revisions = fixture.split(whereSeparator: \.isNewline).compactMap { line -> String? in
            guard line.hasPrefix(prefix) else { return nil }
            let revision = String(line.dropFirst(prefix.count))
            return revision.isEmpty ? nil : revision
        }
        guard revisions.count == 1 else { throw FixtureError.invalid("fixture revision metadata") }
        return revisions[0]
    }

    private func packageResolvedRevision() throws -> String {
        let data = try Data(contentsOf: packageResolvedURL())
        guard let root = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              let pins = root["pins"] as? [[String: Any]],
              let pin = pins.first(where: { $0["identity"] as? String == "azookeykanakanjiconverter" }),
              let state = pin["state"] as? [String: Any],
              let revision = state["revision"] as? String,
              !revision.isEmpty else {
            throw FixtureError.invalid("Package.resolved AzooKey revision")
        }
        return revision
    }

    private func packageSwiftRevision() throws -> String {
        let package = try String(contentsOf: packageSwiftURL(), encoding: .utf8)
        guard let dependency = package.components(separatedBy: ".package(")
            .first(where: { $0.contains("AzooKeyKanaKanjiConverter") }),
              let revisionStart = dependency.range(of: "revision: \"")?.upperBound,
              let revisionEnd = dependency[revisionStart...].firstIndex(of: "\"") else {
            throw FixtureError.invalid("Package.swift AzooKey revision")
        }
        return String(dependency[revisionStart..<revisionEnd])
    }

    private func fixtureCases(_ fixture: String) throws -> [FixtureCase] {
        var cases = [FixtureCase]()
        for (index, line) in fixture.split(whereSeparator: \.isNewline).enumerated() {
            if line.isEmpty || line.hasPrefix("#") { continue }
            let fields = line.split(separator: "\t", omittingEmptySubsequences: false)
            guard fields.count == 3, fields.allSatisfy({ !$0.isEmpty }) else {
                throw FixtureError.invalid("line \(index + 1) must have three non-empty columns")
            }
            let operations = try fields[1].split(separator: ",").map { operation -> FixtureOperation in
                if operation == "B" { return .backspace }
                let scalars = operation.unicodeScalars
                if scalars.count > 2,
                   scalars.starts(with: ["K", ":"]) {
                    return .kana(String(String.UnicodeScalarView(scalars.dropFirst(2))))
                }
                if scalars.count > 2,
                   scalars.starts(with: ["D", ":"]) {
                    return .direct(String(String.UnicodeScalarView(scalars.dropFirst(2))))
                }
                throw FixtureError.invalid("line \(index + 1) has unknown operation \(operation)")
            }
            let trajectory = fields[2].split(separator: "|", omittingEmptySubsequences: false).map(String.init)
            guard !trajectory.contains("") else {
                throw FixtureError.invalid("line \(index + 1) has an empty trajectory reading")
            }
            let eventCount = operations.reduce(0) { count, operation in count + operation.eventCount }
            guard trajectory.count == eventCount else {
                throw FixtureError.invalid("line \(index + 1) has \(eventCount) events but \(trajectory.count) readings")
            }
            cases.append(FixtureCase(name: String(fields[0]), operations: operations, trajectory: trajectory))
        }
        guard !cases.isEmpty else { throw FixtureError.invalid("fixture has no cases") }
        return cases
    }
}

private struct FixtureCase {
    let name: String
    let operations: [FixtureOperation]
    let trajectory: [String]
}

private enum FixtureOperation {
    case kana(String)
    case direct(String)
    case backspace

    var eventCount: Int {
        switch self {
        case let .kana(payload), let .direct(payload): payload.count
        case .backspace: 1
        }
    }
}

private enum FixtureError: Error {
    case invalid(String)
}
