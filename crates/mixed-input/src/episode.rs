//! fixture 1件分のエピソード契約。
//! TOML はデータとして書き、ここで型付きの列挙へ落とす。判別器（PR4）や
//! TIP 側ワーカー（PR5 以降）が fixture と同じ列挙を共有するのが目的で、
//! fixture 専用の表現を増やさない。

use serde::Deserialize;

use crate::plan::SegmentKind;
use crate::position::SourceRange;

/// 評価・契約の区分（計画書 11 節の試験行列に対応）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Category {
    PureJapanese,
    LowercaseMixed,
    MultiMixed,
    CapsSymbol,
    Ambiguous,
    UnknownWords,
    EditEvents,
    Position,
    ExplicitInput,
    AsyncIdentity,
    PartialCommit,
    Persistence,
    Compat,
    Failure,
    Unicode,
}

impl Category {
    pub fn parse(s: &str) -> Result<Self, String> {
        let category = match s {
            "pure_japanese" => Category::PureJapanese,
            "lowercase_mixed" => Category::LowercaseMixed,
            "multi_mixed" => Category::MultiMixed,
            "caps_symbol" => Category::CapsSymbol,
            "ambiguous" => Category::Ambiguous,
            "unknown_words" => Category::UnknownWords,
            "edit_events" => Category::EditEvents,
            "position" => Category::Position,
            "explicit_input" => Category::ExplicitInput,
            "async_identity" => Category::AsyncIdentity,
            "partial_commit" => Category::PartialCommit,
            "persistence" => Category::Persistence,
            "compat" => Category::Compat,
            "failure" => Category::Failure,
            "unicode" => Category::Unicode,
            other => return Err(format!("未知の category: {other}")),
        };
        Ok(category)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Category::PureJapanese => "pure_japanese",
            Category::LowercaseMixed => "lowercase_mixed",
            Category::MultiMixed => "multi_mixed",
            Category::CapsSymbol => "caps_symbol",
            Category::Ambiguous => "ambiguous",
            Category::UnknownWords => "unknown_words",
            Category::EditEvents => "edit_events",
            Category::Position => "position",
            Category::ExplicitInput => "explicit_input",
            Category::AsyncIdentity => "async_identity",
            Category::PartialCommit => "partial_commit",
            Category::Persistence => "persistence",
            Category::Compat => "compat",
            Category::Failure => "failure",
            Category::Unicode => "unicode",
        }
    }
}

/// 公開モード（計画書 8.1）。既定は Off。閾値やモデル種別を fixture に露出させない。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FeatureMode {
    Off,
    CandidatesOnly,
    Auto,
}

impl FeatureMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "off" => Ok(FeatureMode::Off),
            "candidates" => Ok(FeatureMode::CandidatesOnly),
            "auto" => Ok(FeatureMode::Auto),
            other => Err(format!("未知の mode: {other}")),
        }
    }
}

/// その入力だけを見て解釈が一意に決まるか。決まらない例を無理に決めない（計画書 5.3）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Intent {
    Unambiguous,
    Ambiguous,
}

/// fixture が宣言する期待解釈。宣言ソース全体を被覆する。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExpectedPlanSpec {
    /// 全体を日本語として扱う（既存相当の経路）。
    AllJapanese,
    /// 区間列。text 列の結合がソースと一致することを検証で要求する。
    Spans(Vec<(SegmentKind, String)>),
}

/// エピソード内の1操作。fixture では "type:kyouha" 等の文字列 DSL で書く。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EpisodeEvent {
    /// caret に文字を挿入する。
    Type {
        text: String,
    },
    /// caret 前の scalar を n 削る。PR1 骨格は scalar 単位の素朴な意味で、
    /// 本物のかな削除・未完ローマ字契約は PR2 の CompositionSource が担う。
    DeleteBackward {
        units: u32,
    },
    /// caret を絶対 scalar 位置へ移す。
    MoveTo {
        position: u32,
    },
    SelectCandidate {
        index: u32,
    },
    /// 区間の解釈を利用者が明示変更する。自動推定より強い（計画書 2.1）。
    Reinterpret {
        range: SourceRange,
        as_kind: SegmentKind,
    },
    CommitAll,
    /// 先頭 n scalar を確定する（部分確定）。
    CommitPrefix {
        scalars: u32,
    },
    Escape,
    /// 障害注入・同一性などのシナリオ注記。PR1 では表現の存在だけを契約し、
    /// 効果の実装は各 PR（PR3 障害保持、PR6 同一性）で行う。
    Annotate {
        tag: String,
    },
}

impl EpisodeEvent {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "commit_all" => return Ok(EpisodeEvent::CommitAll),
            "escape" => return Ok(EpisodeEvent::Escape),
            _ => {}
        }
        let (head, rest) = s
            .split_once(':')
            .ok_or_else(|| format!("イベントに区切りが無い: {s}"))?;
        match head {
            "type" => Ok(EpisodeEvent::Type {
                text: rest.to_string(),
            }),
            "delete_backward" => {
                let units = rest
                    .parse::<u32>()
                    .map_err(|_| format!("delete_backward の数が読めない: {s}"))?;
                Ok(EpisodeEvent::DeleteBackward { units })
            }
            "move_to" => {
                let position = rest
                    .parse::<u32>()
                    .map_err(|_| format!("move_to の位置が読めない: {s}"))?;
                Ok(EpisodeEvent::MoveTo { position })
            }
            "select_candidate" => {
                let index = rest
                    .parse::<u32>()
                    .map_err(|_| format!("select_candidate が読めない: {s}"))?;
                Ok(EpisodeEvent::SelectCandidate { index })
            }
            "reinterpret" => {
                let parts: Vec<&str> = rest.split(':').collect();
                if parts.len() != 3 {
                    return Err(format!("reinterpret は <start>:<end>:<kind> で書く: {s}"));
                }
                let start = parts[0]
                    .parse::<u32>()
                    .map_err(|_| format!("reinterpret の開始位置が読めない: {s}"))?;
                let end = parts[1]
                    .parse::<u32>()
                    .map_err(|_| format!("reinterpret の終了位置が読めない: {s}"))?;
                if end <= start {
                    return Err(format!("reinterpret の範囲が空か逆: {s}"));
                }
                let as_kind = SegmentKind::deserialize_toml_kind(parts[2])?;
                Ok(EpisodeEvent::Reinterpret {
                    range: SourceRange::new(start, end),
                    as_kind,
                })
            }
            "commit_prefix" => {
                let scalars = rest
                    .parse::<u32>()
                    .map_err(|_| format!("commit_prefix が読めない: {s}"))?;
                Ok(EpisodeEvent::CommitPrefix { scalars })
            }
            "annotate" => Ok(EpisodeEvent::Annotate {
                tag: rest.to_string(),
            }),
            other => Err(format!("未知のイベント: {other}")),
        }
    }

    pub fn annotation_tag(&self) -> Option<&str> {
        match self {
            EpisodeEvent::Annotate { tag } => Some(tag),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct RawDocument {
    episode: Vec<RawEpisode>,
}

#[derive(Deserialize)]
struct RawEpisode {
    id: String,
    category: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default = "default_intent")]
    intent: String,
    source: String,
    expected: RawExpected,
    #[serde(default)]
    alternative_literal: Option<String>,
    #[serde(default)]
    events: Vec<String>,
    #[serde(default)]
    notes: String,
}

fn default_mode() -> String {
    "auto".to_string()
}

fn default_intent() -> String {
    "unambiguous".to_string()
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawExpected {
    Flag(String),
    Spans(Vec<RawSpan>),
}

#[derive(Deserialize)]
struct RawSpan {
    kind: String,
    text: String,
}

/// fixture ファイル1つ（複数 `[[episode]]` を含み得る）を読む。
pub fn parse_episodes(toml_text: &str) -> Result<Vec<Episode>, String> {
    let raw: RawDocument = toml::from_str(toml_text).map_err(|e| e.to_string())?;
    raw.episode.into_iter().map(convert).collect()
}

/// エピソードの実体。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Episode {
    pub id: String,
    pub category: Category,
    pub mode: FeatureMode,
    pub intent: Intent,
    /// 打鍵列の再生結果として宣言する完全な元ソース（commit 済み接頭辞を含む）。
    pub source: String,
    pub expected: ExpectedPlanSpec,
    /// 曖昧例で「候補として存在すべき英字解釈」の区間文字列。
    pub alternative_literal: Option<String>,
    pub events: Vec<EpisodeEvent>,
    pub notes: String,
}

fn convert(raw: RawEpisode) -> Result<Episode, String> {
    let category = Category::parse(&raw.category)?;
    let mode = FeatureMode::parse(&raw.mode)?;
    let intent = match raw.intent.as_str() {
        "unambiguous" => Intent::Unambiguous,
        "ambiguous" => Intent::Ambiguous,
        other => return Err(format!("{}: 未知の intent: {other}", raw.id)),
    };
    let expected = match raw.expected {
        RawExpected::Flag(flag) => match flag.as_str() {
            "all_japanese" => ExpectedPlanSpec::AllJapanese,
            other => return Err(format!("{}: 未知の expected: {other}", raw.id)),
        },
        RawExpected::Spans(spans) => {
            let mut out = Vec::new();
            for span in spans {
                let kind = SegmentKind::deserialize_toml_kind(&span.kind)
                    .map_err(|e| format!("{}: {e}", raw.id))?;
                out.push((kind, span.text));
            }
            ExpectedPlanSpec::Spans(out)
        }
    };
    let events = raw
        .events
        .iter()
        .map(|e| EpisodeEvent::parse(e))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{}: {e}", raw.id))?;
    Ok(Episode {
        id: raw.id,
        category,
        mode,
        intent,
        source: raw.source,
        expected,
        alternative_literal: raw.alternative_literal,
        events,
        notes: raw.notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_unknown_event() {
        assert!(EpisodeEvent::parse("resurrect:0:4:literal").is_err());
    }

    #[test]
    fn type_event_keeps_colons_in_text() {
        let event = EpisodeEvent::parse("type:src/main.rs").unwrap();
        match event {
            EpisodeEvent::Type { text } => assert_eq!(text, "src/main.rs"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_document_with_multiple_episodes() {
        let doc = r#"
[[episode]]
id = "x-001"
category = "ambiguous"
mode = "candidates"
intent = "ambiguous"
source = "made"
expected = "all_japanese"
alternative_literal = "made"

[[episode]]
id = "x-002"
category = "pure_japanese"
source = "mada"
expected = [{ kind = "literal", text = "mada" }]
"#;
        let episodes = parse_episodes(doc).unwrap();
        assert_eq!(episodes.len(), 2);
        assert_eq!(episodes[0].mode, FeatureMode::CandidatesOnly);
    }
}
