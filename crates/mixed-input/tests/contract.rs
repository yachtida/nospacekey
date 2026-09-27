//! 契約 fixture 全体の検証。計画書 10 の PR1 完了条件
//! 「意図・曖昧例・空白/記号・キー操作・同一性と fallback がテストで表せること」
//! をここで固定する。件数の下限は corpus の意図的な縮小を検出するためのもの。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use mixed_input::episode::{
    parse_episodes, Category, Episode, ExpectedPlanSpec, FeatureMode, Intent,
};
use mixed_input::validation::{validate_corpus, validate_episode, Violation};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn load_corpus() -> Vec<Episode> {
    let mut files: Vec<PathBuf> = fs::read_dir(fixtures_dir())
        .expect("fixtures ディレクトリを読める")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext == "toml")
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    let mut corpus = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).expect("fixture を読める");
        corpus.extend(
            parse_episodes(&text).unwrap_or_else(|error| panic!("{}: {error}", file.display())),
        );
    }
    corpus
}

fn category_counts(corpus: &[Episode]) -> BTreeMap<Category, usize> {
    let mut counts = BTreeMap::new();
    for episode in corpus {
        *counts.entry(episode.category).or_insert(0) += 1;
    }
    counts
}

/// 区分ごとの fixture 下限。計画書 11 の試験行列に対応する。
fn minimum_count(category: Category) -> usize {
    match category {
        Category::PureJapanese => 30,
        Category::LowercaseMixed => 30,
        Category::MultiMixed => 12,
        Category::CapsSymbol => 15,
        Category::Ambiguous => 25,
        Category::UnknownWords => 12,
        Category::EditEvents => 20,
        Category::Position => 10,
        Category::ExplicitInput => 10,
        Category::AsyncIdentity => 10,
        Category::PartialCommit => 8,
        Category::Persistence => 5,
        Category::Compat => 4,
        Category::Failure => 6,
        Category::Unicode => 7,
    }
}

#[test]
fn corpus_is_valid_and_replays_clean() {
    let corpus = load_corpus();
    assert!(
        corpus.len() >= 200,
        "契約 fixture は約200件必要（実績 {}）",
        corpus.len()
    );
    let violations = validate_corpus(&corpus);
    assert!(violations.is_empty(), "契約違反: {violations:#?}");
}

#[test]
fn categories_cover_the_test_matrix() {
    let corpus = load_corpus();
    let counts = category_counts(&corpus);
    let all = [
        Category::PureJapanese,
        Category::LowercaseMixed,
        Category::MultiMixed,
        Category::CapsSymbol,
        Category::Ambiguous,
        Category::UnknownWords,
        Category::EditEvents,
        Category::Position,
        Category::ExplicitInput,
        Category::AsyncIdentity,
        Category::PartialCommit,
        Category::Persistence,
        Category::Compat,
        Category::Failure,
        Category::Unicode,
    ];
    for category in all {
        let count = counts.get(&category).copied().unwrap_or(0);
        let minimum = minimum_count(category);
        assert!(
            count >= minimum,
            "{:?} が {count} 件（最低 {minimum}）",
            category
        );
    }
}

/// OFF 時基準テスト。機能無効なら既存どおり全体日本語であり、
/// 混在候補も自動適用も現れない（計画書 8.1・12.1）。
#[test]
fn off_mode_baseline_keeps_existing_behavior() {
    let corpus = load_corpus();
    let off: Vec<&Episode> = corpus
        .iter()
        .filter(|episode| episode.mode == FeatureMode::Off)
        .collect();
    assert!(off.len() >= 4, "OFF 時基準の例が少なすぎる: {}", off.len());
    for episode in &off {
        assert!(
            matches!(episode.expected, ExpectedPlanSpec::AllJapanese),
            "{}: OFF なのに混在解釈を期待している",
            episode.id
        );
        let violations = validate_episode(episode);
        assert!(violations.is_empty(), "{}: {violations:#?}", episode.id);
    }
}

/// 曖昧例は、利用者が候補で選ぶまで日本語のまま。自動適用しない契約
/// （計画書 5.3・12.3 の誤検出抑制）を fixture の形で固定する。
#[test]
fn ambiguous_examples_stay_japanese_until_user_picks() {
    let corpus = load_corpus();
    let ambiguous: Vec<&Episode> = corpus
        .iter()
        .filter(|episode| episode.category == Category::Ambiguous)
        .collect();
    assert!(ambiguous.len() >= 25, "曖昧例が少なすぎる");
    for episode in &ambiguous {
        assert_eq!(episode.intent, Intent::Ambiguous, "{}", episode.id);
        assert_eq!(episode.mode, FeatureMode::CandidatesOnly, "{}", episode.id);
        assert!(
            matches!(episode.expected, ExpectedPlanSpec::AllJapanese),
            "{}: 曖昧例なのに混在を自動適用する期待になっている",
            episode.id
        );
        let alternative = episode
            .alternative_literal
            .as_deref()
            .unwrap_or_else(|| panic!("{}: 候補になる英字解釈が無い", episode.id));
        assert!(
            episode.source.contains(alternative),
            "{}: alternative_literal がソースに無い",
            episode.id
        );
    }
}

/// PR1 完了条件: キー操作・同一性・fallback が fixture で表現できていること。
#[test]
fn key_operations_identity_and_fallback_are_expressible() {
    let corpus = load_corpus();
    assert!(
        corpus.iter().any(|episode| episode.source.contains(' ')),
        "空白を含む入力の例が無い"
    );
    let with_symbols = corpus
        .iter()
        .filter(|episode| episode.source.chars().any(|c| ".-/@+".contains(c)))
        .count();
    assert!(
        with_symbols >= 3,
        "記号を含む入力の例が足りない: {with_symbols}"
    );

    let counts = category_counts(&corpus);
    for category in [
        Category::EditEvents,
        Category::Position,
        Category::AsyncIdentity,
        Category::PartialCommit,
        Category::Compat,
        Category::Persistence,
        Category::Failure,
    ] {
        let count = counts.get(&category).copied().unwrap_or(0);
        assert!(
            count > 0,
            "{category:?} の例が無い。キー操作・同一性・fallback を表現できない"
        );
    }
}

/// 位置契約は Unicode scalar。結合文字を含む例が 2 scalar として数わること
/// （書記素でも UTF-16 code unit でもない）。
#[test]
fn unicode_positions_are_scalar_based() {
    let corpus = load_corpus();
    let decomposed = corpus
        .iter()
        .find(|episode| episode.id == "uni-003")
        .expect("uni-003 が無い");
    assert_eq!(
        mixed_input::position::scalar_len(&decomposed.source),
        15,
        "e + 結合アクセントは 2 scalar として数える"
    );
}

fn violations_of(toml_text: &str) -> Vec<Violation> {
    let episodes = parse_episodes(toml_text).expect("fixture を解析できる");
    episodes
        .iter()
        .flat_map(validate_episode)
        .collect()
}

fn has_rule(violations: &[Violation], rule: &str) -> bool {
    violations.iter().any(|violation| violation.rule == rule)
}

/// 検証器の歯が実在すること。壊れた契約を黙って通さない。
#[test]
fn validator_rejects_broken_contracts() {
    // 被覆の欠落（span 列の結合がソースと一致しない）。
    let gap = r#"
[[episode]]
id = "neg-001"
category = "lowercase_mixed"
source = "githubnotukaikata"
expected = [
  { kind = "literal", text = "githug" },
  { kind = "japanese", text = "notukaikata" },
]
events = ["type:githubnotukaikata"]
"#;
    assert!(has_rule(&violations_of(gap), "E_PLAN_CONCAT"));

    // 曖昧例の自動適用。
    let auto_ambiguous = r#"
[[episode]]
id = "neg-002"
category = "ambiguous"
mode = "auto"
intent = "ambiguous"
source = "made"
expected = "all_japanese"
alternative_literal = "made"
events = ["type:made"]
"#;
    assert!(has_rule(
        &violations_of(auto_ambiguous),
        "E_AMBIGUOUS_RULES"
    ));

    // OFF 時の混在期待。
    let off_mixed = r#"
[[episode]]
id = "neg-003"
category = "lowercase_mixed"
mode = "off"
source = "githubnotukaikata"
expected = [
  { kind = "literal", text = "github" },
  { kind = "japanese", text = "notukaikata" },
]
events = ["type:githubnotukaikata"]
"#;
    assert!(has_rule(
        &violations_of(off_mixed),
        "E_MODE_OFF_ALL_JAPANESE"
    ));

    // 打鍵列が宣言ソースを再現しない。
    let replay_gap = r#"
[[episode]]
id = "neg-004"
category = "pure_japanese"
source = "mada"
expected = "all_japanese"
events = ["type:made"]
"#;
    assert!(has_rule(&violations_of(replay_gap), "E_REPLAY_CONSISTENCY"));

    // commit fence: Literal の途中での部分確定。
    let fence = r#"
[[episode]]
id = "neg-005"
category = "partial_commit"
source = "kyouhaPythonwotukau"
expected = [
  { kind = "japanese", text = "kyouha" },
  { kind = "literal", text = "Python" },
  { kind = "japanese", text = "wotukau" },
]
events = ["type:kyouhaPythonwotukau", "commit_prefix:9"]
"#;
    assert!(has_rule(&violations_of(fence), "E_COMMIT_FENCE"));

    // 純日本語区分での英字期待。
    let pure_literal = r#"
[[episode]]
id = "neg-006"
category = "pure_japanese"
source = "made"
expected = [
  { kind = "literal", text = "made" },
]
events = ["type:made"]
"#;
    assert!(has_rule(
        &violations_of(pure_literal),
        "E_PURE_JP_NO_LITERAL"
    ));

    // 利用者の明示変更が期待に反映されていない。
    let explicit_ignored = r#"
[[episode]]
id = "neg-007"
category = "explicit_input"
source = "made"
expected = "all_japanese"
events = ["type:made", "reinterpret:0:4:literal"]
"#;
    assert!(has_rule(
        &violations_of(explicit_ignored),
        "E_EXPLICIT_HONORED"
    ));

    // 部分確定を含む打鍵列が、内容まで宣言ソースと一致しない
    // （総量の一致だけでは見逃していた）。
    let commit_content_mismatch = r#"
[[episode]]
id = "neg-008"
category = "partial_commit"
source = "made"
expected = "all_japanese"
events = ["type:kore", "commit_prefix:2"]
"#;
    assert!(has_rule(
        &violations_of(commit_content_mismatch),
        "E_REPLAY_CONSISTENCY"
    ));

    // 複数の明示変更のうち、最後以外が期待に反映されていない
    // （最後の1件だけ照合しては見逃していた）。
    let first_reinterpret_ignored = r#"
[[episode]]
id = "neg-009"
category = "explicit_input"
source = "madeare"
expected = [
  { kind = "japanese", text = "made" },
  { kind = "literal", text = "are" },
]
events = [
  "type:madeare",
  "reinterpret:0:4:literal",
  "reinterpret:4:7:literal",
]
"#;
    assert!(has_rule(
        &violations_of(first_reinterpret_ignored),
        "E_EXPLICIT_HONORED"
    ));

    // 範囲の前の編集でずれた明示変更を、元の位置のままで照合しない
    // （追従が無いと保持できているのに誤検出する）。
    let shifted_reinterpret_ignored = r#"
[[episode]]
id = "neg-010"
category = "explicit_input"
source = "omade"
expected = "all_japanese"
events = [
  "type:nomade",
  "reinterpret:2:6:literal",
  "move_to:1",
  "delete_backward:1",
]
"#;
    assert!(has_rule(
        &violations_of(shifted_reinterpret_ignored),
        "E_EXPLICIT_HONORED"
    ));

    // 確定済み区間の明示指定は、後からの Escape で消せない。Escape は未確定側
    // だけを解除するので、「made を原文保持で確定した」契約に対する
    // all_japanese 期待は不正であり、受理してはいけなかった。
    let committed_literal_lost_on_escape = r#"
[[episode]]
id = "neg-011"
category = "partial_commit"
source = "madeno"
expected = "all_japanese"
events = [
  "type:madeno",
  "reinterpret:0:4:literal",
  "commit_prefix:4",
  "escape",
]
"#;
    assert!(has_rule(
        &violations_of(committed_literal_lost_on_escape),
        "E_EXPLICIT_HONORED"
    ));

    // commit_all → 新しい入力 → escape でも、前回確定した明示指定は
    // 検証対象に残る。確定済み側を Escape で消せてしまうと通っていた。
    let committed_before_new_input_lost_on_escape = r#"
[[episode]]
id = "neg-012"
category = "partial_commit"
source = "madenoare"
expected = "all_japanese"
events = [
  "type:made",
  "reinterpret:0:4:literal",
  "commit_all",
  "type:noare",
  "escape",
]
"#;
    assert!(has_rule(
        &violations_of(committed_before_new_input_lost_on_escape),
        "E_EXPLICIT_HONORED"
    ));
}

/// 明示変更より前の編集で範囲がずれても、利用者の指定は追従して保持される。
#[test]
fn explicit_range_follows_edits_before_it() {
    let episode_text = r#"
[[episode]]
id = "pos-explicit-001"
category = "explicit_input"
source = "omade"
expected = [
  { kind = "japanese", text = "o" },
  { kind = "literal", text = "made" },
]
events = [
  "type:nomade",
  "reinterpret:2:6:literal",
  "move_to:1",
  "delete_backward:1",
]
"#;
    let violations = violations_of(episode_text);
    assert!(
        violations.is_empty(),
        "範囲の前の編集で明示指定が失われた: {violations:#?}"
    );
}

/// 確定済み区間の明示指定は Escape でも解除されず、期待解釈の照合対象に残る。
/// 部分確定と全確定→入力継続の両方で固定する。
#[test]
fn committed_explicit_choice_survives_escape() {
    let episode_text = r#"
[[episode]]
id = "pc-explicit-001"
category = "partial_commit"
source = "madeno"
expected = [
  { kind = "literal", text = "made" },
  { kind = "japanese", text = "no" },
]
events = [
  "type:madeno",
  "reinterpret:0:4:literal",
  "commit_prefix:4",
  "escape",
]

[[episode]]
id = "pc-explicit-002"
category = "partial_commit"
source = "madenoare"
expected = [
  { kind = "literal", text = "made" },
  { kind = "japanese", text = "noare" },
]
events = [
  "type:made",
  "reinterpret:0:4:literal",
  "commit_all",
  "type:noare",
  "escape",
]
"#;
    let violations = violations_of(episode_text);
    assert!(
        violations.is_empty(),
        "確定済みの明示指定が Escape で失われた: {violations:#?}"
    );
}
