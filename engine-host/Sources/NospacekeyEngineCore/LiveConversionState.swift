import Foundation
import KanaKanjiConverterModuleWithDefaultDictionary

/// 自動確定の速さ。iOS nospacekey の `AutomaticCompletionStrengthKey`
/// （NospacekeyCore/Sources/NospacekeyUtils/KeyboardSetting/AutoCompletionStrengthSetting.swift）の移植。
/// threshold は「先頭文節の候補が何回のライブ変換更新で不変なら確定してよいか」。
/// iOS の既定値は `.weak`（=16）なので Windows でも同じ既定にする。
public enum AutoCommitStrength: String, Equatable, Sendable {
    case disabled
    case `weak`
    case normal
    case strong
    case ultrastrong

    /// nil = 無効（iOS の `.disabled: .max` 相当。比較を避けるため Optional にする）。
    public var threshold: Int? {
        switch self {
        case .disabled: return nil
        case .weak: return 16
        case .normal: return 13
        case .strong: return 10
        case .ultrastrong: return 6
        }
    }

    /// env `NOSPACEKEY_AUTO_COMMIT` から解決する。未設定・未知値は iOS 既定と同じ `.weak`。
    /// 明示的に切りたいときだけ `NOSPACEKEY_AUTO_COMMIT=disabled`。
    public static func resolve(environment: [String: String]) -> AutoCommitStrength {
        guard let raw = environment["NOSPACEKEY_AUTO_COMMIT"]?.lowercased(), !raw.isEmpty else {
            return .weak
        }
        return AutoCommitStrength(rawValue: raw) ?? .weak
    }
}

/// 読み長バックストップ（死のループ対策）。裸助詞境界の長文では先頭文節が安定せず
/// `AutoCommitStrength` の履歴判定だけでは自動確定が発火しないことがある（実機実測 2026-07-08）。
/// 読み（convertTarget）が一定長を超えたら、文節安定を待たず先頭文節を強制確定して読みの
/// 頭打ちを保証する安全弁。値は「この長さを超えたら強制確定してよい」しきい値。
public enum AutoCommitLengthBackstop {
    /// env `NOSPACEKEY_AUTO_COMMIT_MAX_READING` から解決する。未設定・パース不能は既定 25
    /// （実測根拠: 読み26-30文字の400ms超過は0/63、31+で21/87 — 25で危険域手前に頭打ち）。
    /// 0 以下は無効（バックストップ OFF）。
    public static func resolve(environment: [String: String]) -> Int {
        guard let raw = environment["NOSPACEKEY_AUTO_COMMIT_MAX_READING"], let value = Int(raw) else {
            return 25
        }
        return value
    }
}

/// iOS nospacekey の `LiveConversionManager`（Keyboard/Display/LiveConversionManager.swift）のうち
/// 「自動確定」に必要な履歴管理の移植。ライブ変換の更新ごとに、採用候補を文節列に分解して
/// 文節 index ごとの履歴 `headClauseCandidateHistories` に積む。先頭文節の候補テキストが直近
/// threshold 回変動していなければ、その文節を確定してよい（candidateForCompleteFirstClause）。
///
/// セッション（ComposingText）ごとに 1 つ持つ値型。ConversionService が serviceLock 下で触るため
/// 同期は不要。iOS 版とのずれは以下のみ:
/// - `@KeyboardSetting` ではなく threshold を引数注入（Windows は env / 既定 weak）
/// - `adjustCandidate`（末尾 1 かな文節のひらがな化＝表示の微調整）は未移植
public struct LiveConversionState {
    /// 文節 index ごとの候補履歴。iOS の headClauseCandidateHistories と同型。
    private var headClauseCandidateHistories: [[Candidate]] = []
    /// 直近のライブ変換で使った候補（差分判定用）。iOS の lastUsedCandidate 相当。
    private var lastUsedCandidate: Candidate?

    public init() {}

    /// ライブ変換の更新を反映する。iOS の setLastUsedCandidate(_:firstClauseCandidates:) の移植。
    /// 読み（ruby 長）が増えた更新は履歴へ追加、減った更新は各履歴の末尾を 1 つ落とし、
    /// 同長（置換）は落としてから追加する。
    public mutating func update(candidate: Candidate, firstClauseCandidates: [Candidate]) {
        let diff: Int
        if let lastUsedCandidate {
            let lastLength = lastUsedCandidate.data.reduce(0) { $0 + $1.ruby.count }
            let newLength = candidate.data.reduce(0) { $0 + $1.ruby.count }
            diff = newLength - lastLength
        } else {
            diff = 1
        }
        self.lastUsedCandidate = candidate
        if diff > 0 {
            self.updateHistories(newCandidate: candidate, firstClauseCandidates: firstClauseCandidates)
        } else if diff < 0 {
            // 削除の場合には最後尾のログを1つ落とす（iOS と同一）。
            for i in self.headClauseCandidateHistories.indices {
                _ = self.headClauseCandidateHistories[i].popLast()
            }
        } else {
            // 置換の場合には更新を追加で入れる（iOS と同一）。
            for i in self.headClauseCandidateHistories.indices {
                _ = self.headClauseCandidateHistories[i].popLast()
            }
            self.updateHistories(newCandidate: candidate, firstClauseCandidates: firstClauseCandidates)
        }
    }

    /// iOS の updateHistories の移植。候補を先頭から文節へ分解し、文節 index ごとの履歴へ積む。
    /// 先頭文節はローマ字入力向けに firstClauseResults の composingCount で補正する（iOS と同一）。
    private mutating func updateHistories(newCandidate: Candidate, firstClauseCandidates: [Candidate]) {
        var data = newCandidate.data[...]
        var count = 0
        while !data.isEmpty {
            var clause = Candidate.makePrefixClauseCandidate(data: data)
            if count == 0, let first = firstClauseCandidates.first(where: { $0.text == clause.text }) {
                clause.composingCount = first.composingCount
            }
            // 防御: 空文節（先頭要素が即文節境界）で dropFirst(0) の無限ループに入らない
            // （iOS には無いガードだが、入っても壊れない安全側の差分）。
            if clause.data.isEmpty {
                break
            }
            if self.headClauseCandidateHistories.count <= count {
                self.headClauseCandidateHistories.append([clause])
            } else {
                self.headClauseCandidateHistories[count].append(clause)
            }
            data = data.dropFirst(clause.data.count)
            count += 1
        }
    }

    /// 最初の文節を確定して良い場合 Candidate を返す。iOS の candidateForCompleteFirstClause の移植。
    /// - warning: 結果を得た場合、必ずその Candidate で確定処理（prefixComplete）を行うこと（iOS と同じ契約）。
    public func candidateForCompleteFirstClause(threshold: Int) -> Candidate? {
        guard let history = headClauseCandidateHistories.first else {
            return nil
        }
        if history.count < threshold {
            return nil
        }
        // 過去十分な回数変動がなければ、prefix を確定して良い（iOS と同一の判定）。
        let texts = Set(history.suffix(threshold).map { $0.text })
        if texts.count == 1 {
            return history.last
        }
        return nil
    }

    /// 先頭文節の確定後に呼ぶ。iOS の updateAfterFirstClauseCompletion の移植
    /// （lastUsedCandidate を破棄し、確定済み文節の履歴を落として次文節を先頭へ繰り上げる）。
    public mutating func didCompleteFirstClause() {
        self.lastUsedCandidate = nil
        if !headClauseCandidateHistories.isEmpty {
            headClauseCandidateHistories.removeFirst()
        }
    }
}
