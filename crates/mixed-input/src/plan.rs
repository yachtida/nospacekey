//! 区間解釈の契約。ADR-0007: 解釈は2値ラベルだけ。
//! `Japanese` は既存のかな漢字変換へ渡す区間、`Literal` は元文字を最終出力まで
//! 一字不動で保持する区間を意味する。「英語である」ではない（計画書 2.1）。

use serde::Deserialize;

use crate::position::{SourcePosition, SourceRange};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SegmentKind {
    Japanese,
    Literal,
}

impl SegmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SegmentKind::Japanese => "japanese",
            SegmentKind::Literal => "literal",
        }
    }

    /// fixture の文字列表記を解釈する。serde の untagged では error 文面が
    /// 分かりにくいため、DSL と fixture の両方でこの関数を通す。
    pub fn deserialize_toml_kind(s: &str) -> Result<Self, String> {
        match s {
            "japanese" => Ok(SegmentKind::Japanese),
            "literal" => Ok(SegmentKind::Literal),
            other => Err(format!("未知の span kind: {other}")),
        }
    }
}

/// 解釈の1区間。fixture は text で宣言し、検証時にソース上の範囲へ確定する。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlanSpan {
    pub kind: SegmentKind,
    pub text: String,
    pub range: SourceRange,
}

/// 1解釈。spans は先頭から順にソースを過不足なく被覆する。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct InterpretationPlan {
    pub spans: Vec<PlanSpan>,
}

impl InterpretationPlan {
    /// span text 列を順にソースへ突き合わせて範囲を確定する。
    /// 結合がソースと過不足なく一致しない場合は Err。範囲の被覆契約はここ一箇所で守る。
    pub fn build(source: &str, specs: &[(SegmentKind, String)]) -> Result<Self, String> {
        let mut spans = Vec::new();
        let mut cursor: u32 = 0;
        let mut byte: usize = 0;
        for (index, (kind, text)) in specs.iter().enumerate() {
            if text.is_empty() {
                return Err(format!("span {index} が空"));
            }
            if !source[byte..].starts_with(text.as_str()) {
                return Err(format!(
                    "span {index} ({text}) がソースの {cursor} scalar 目から一致しない"
                ));
            }
            let start = SourcePosition::new(cursor);
            let end = SourcePosition::new(cursor + crate::position::scalar_len(text));
            spans.push(PlanSpan {
                kind: *kind,
                text: text.clone(),
                range: SourceRange { start, end },
            });
            cursor = end.get();
            byte += text.len();
        }
        if byte != source.len() {
            return Err(format!(
                "span 列の結合がソースと一致しない（余り {} byte）",
                source.len() - byte
            ));
        }
        Ok(InterpretationPlan { spans })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_rejects_gap_between_spans() {
        let specs = vec![
            (SegmentKind::Literal, "github".to_string()),
            (SegmentKind::Japanese, "notukaikat".to_string()),
        ];
        assert!(InterpretationPlan::build("githubnotukaikata", &specs).is_err());
    }

    #[test]
    fn build_rejects_empty_span() {
        let specs = vec![(SegmentKind::Japanese, String::new())];
        assert!(InterpretationPlan::build("made", &specs).is_err());
    }

    #[test]
    fn build_accepts_exact_cover() {
        let specs = vec![
            (SegmentKind::Literal, "Python".to_string()),
            (SegmentKind::Japanese, "wotukau".to_string()),
        ];
        let plan = InterpretationPlan::build("Pythonwotukau", &specs).unwrap();
        assert_eq!(plan.spans[0].range.start.get(), 0);
        assert_eq!(plan.spans[0].range.end.get(), 6);
    }
}
