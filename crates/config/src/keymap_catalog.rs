//! settings crate のキー語彙を UI 向けに投影する。機能数・既定キー・衝突規則を
//! フロントへ複製しないための読み取り専用境界。

use crate::logic::FieldError;
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeymapCatalogEntry {
    pub function: String,
    pub label: String,
    pub context: String,
    pub state: String,
    pub default_chords: Vec<String>,
    pub effective_chords: Vec<String>,
    pub enabled: bool,
    pub disabled_reason: Option<String>,
    pub alt_allowed: bool,
}

fn feature_state(function: settings::keymap::KeymapFunc, settings: &settings::Settings) -> bool {
    use settings::keymap::KeymapFunc::*;
    match function {
        Ephemeral => settings.ephemeral.enabled,
        Feedback => settings.feedback.enabled,
        TypoCorrect => settings.typo_correct.enabled,
        LlmConvert => settings::llm_effective_enabled(settings),
        _ => true,
    }
}

fn context_label(function: settings::keymap::KeymapFunc) -> &'static str {
    use settings::keymap::FuncGroup::*;
    match function.group() {
        Global => "待機中・入力中",
        Idle => "未入力・確定直後",
        Composing => "入力中・候補表示中",
    }
}

pub fn catalog() -> Vec<KeymapCatalogEntry> {
    let (current, _) = settings::load_reporting_read_only();
    settings::keymap::ALL_FUNCS
        .into_iter()
        .filter(|function| {
            *function != settings::keymap::KeymapFunc::LlmConvert || !settings::LLM_CONVERT_FROZEN
        })
        .map(|function| {
            let defaults = settings::keymap::default_chords(function, &current.ephemeral.trigger)
                .iter()
                .map(settings::keymap::format_chord)
                .collect::<Vec<_>>();
            let (state, effective_chords) =
                match settings::keymap::resolve_binding(current.keymap.get(function)) {
                    settings::keymap::Binding::Default => ("default", defaults.clone()),
                    settings::keymap::Binding::Disabled => ("disabled", Vec::new()),
                    settings::keymap::Binding::Chord(chord) => {
                        ("custom", vec![settings::keymap::format_chord(&chord)])
                    }
                };
            let enabled = feature_state(function, &current);
            KeymapCatalogEntry {
                function: function.settings_field().into(),
                label: function.label_ja().into(),
                context: context_label(function).into(),
                state: state.into(),
                default_chords: defaults,
                effective_chords,
                enabled,
                disabled_reason: (!enabled).then(|| match function {
                    settings::keymap::KeymapFunc::Ephemeral => {
                        "入力・変換で一時かな入力を有効にすると使えます。".into()
                    }
                    settings::keymap::KeymapFunc::Feedback => {
                        "診断・詳細で誤変換記録を有効にすると使えます。".into()
                    }
                    settings::keymap::KeymapFunc::TypoCorrect => {
                        "入力・変換で修正変換を有効にすると使えます。".into()
                    }
                    _ => "現在この機能は利用できません。".into(),
                }),
                alt_allowed: function.alt_allowed(),
            }
        })
        .collect()
}

pub fn validate(function: String, binding: Option<String>) -> Vec<FieldError> {
    let (mut current, outcome) = settings::load_reporting_read_only();
    if !matches!(
        outcome,
        settings::LoadOutcome::Loaded
            | settings::LoadOutcome::Missing
            | settings::LoadOutcome::Empty
    ) {
        return vec![FieldError {
            field: "_io".into(),
            message: format!("現在の設定を検証できません（{outcome:?}）。"),
        }];
    }
    let slot = match function.as_str() {
        "mode_toggle" => &mut current.keymap.mode_toggle,
        "reconvert" => &mut current.keymap.reconvert,
        "feedback" => &mut current.keymap.feedback,
        "ephemeral" => &mut current.keymap.ephemeral,
        "commit_undo" => &mut current.keymap.commit_undo,
        "typo_correct" => &mut current.keymap.typo_correct,
        "to_hiragana" => &mut current.keymap.to_hiragana,
        "to_katakana" => &mut current.keymap.to_katakana,
        "to_hankaku_kana" => &mut current.keymap.to_hankaku_kana,
        "to_zenkaku_eisu" => &mut current.keymap.to_zenkaku_eisu,
        "to_hankaku_eisu" => &mut current.keymap.to_hankaku_eisu,
        "notation_rotate" => &mut current.keymap.notation_rotate,
        "convert" => &mut current.keymap.convert,
        _ => {
            return vec![FieldError {
                field: "key_binding".into(),
                message: "編集できないキー操作です。".into(),
            }]
        }
    };
    *slot = binding;
    crate::logic::validate(&crate::logic::to_dto(&current))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_derived_from_all_functions_and_hides_frozen_llm() {
        let entries = catalog();
        assert_eq!(entries.len(), settings::keymap::ALL_FUNCS.len() - 1);
        assert!(!entries.iter().any(|entry| entry.function == "llm_convert"));
        let mode = entries
            .iter()
            .find(|entry| entry.function == "mode_toggle")
            .unwrap();
        assert!(mode.default_chords.len() > 1);
    }
}
