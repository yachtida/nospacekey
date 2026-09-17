//! 差分保存 UI の境界。設定の永続値・編集 draft・実動作を混同しないため、
//! この層は「現在の snapshot に型付き change を適用して保存する」ことだけを担う。

use crate::logic::{self, FieldError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

const OPERATION_HISTORY_LIMIT: usize = 64;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsAccess {
    Writable,
    ReadOnly,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsLoadState {
    Loaded,
    Missing,
    PermissionDenied,
    IoError,
    NoPath,
    Empty,
    CorruptRecovered,
    CorruptQuarantineFailed,
    UnsupportedVersion,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsNotice {
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicSettings {
    pub zenzai_enabled: bool,
    pub weight_path: String,
    pub zenzai_inference_limit: u32,
    pub live_enabled: bool,
    pub live_search_width: u32,
    pub inline_prediction_enabled: bool,
    pub default_direct: bool,
    pub learning_enabled: bool,
    pub feedback_enabled: bool,
    pub number_full_width: bool,
    pub punctuation_full_width: bool,
    pub symbol_full_width: bool,
    pub symbol_full_width_chars: Vec<String>,
    pub reading_monitor_enabled: bool,
    pub reading_monitor_accumulate: bool,
    pub reading_monitor_max_chars: u32,
    pub ephemeral_enabled: bool,
    pub ephemeral_trigger: String,
    pub typo_correct_enabled: bool,
    pub typo_correct_learn: bool,
    pub shift_latin_mode: String,
    pub user_dictionary_enabled: bool,
    pub update_include_beta: bool,
    pub update_automatic_check: bool,
    pub update_automatic_check_prompt_dismissed: bool,
    pub keymap: settings::keymap::KeymapSettings,
    pub appearance: settings::Appearance,
}

impl From<&settings::Settings> for PublicSettings {
    fn from(settings: &settings::Settings) -> Self {
        let dto = logic::to_dto(settings);
        Self {
            zenzai_enabled: dto.zenzai_enabled,
            weight_path: dto.weight_path,
            zenzai_inference_limit: dto.zenzai_inference_limit,
            live_enabled: dto.live_enabled,
            live_search_width: dto.live_search_width,
            inline_prediction_enabled: dto.inline_prediction_enabled,
            default_direct: dto.default_direct,
            learning_enabled: dto.learning_enabled,
            feedback_enabled: dto.feedback_enabled,
            number_full_width: dto.number_full_width,
            punctuation_full_width: dto.punctuation_full_width,
            symbol_full_width: dto.symbol_full_width,
            symbol_full_width_chars: dto.symbol_full_width_chars,
            reading_monitor_enabled: dto.reading_monitor_enabled,
            reading_monitor_accumulate: dto.reading_monitor_accumulate,
            reading_monitor_max_chars: dto.reading_monitor_max_chars,
            ephemeral_enabled: dto.ephemeral_enabled,
            ephemeral_trigger: dto.ephemeral_trigger,
            typo_correct_enabled: dto.typo_correct_enabled,
            typo_correct_learn: dto.typo_correct_learn,
            shift_latin_mode: dto.shift_latin_mode,
            user_dictionary_enabled: dto.user_dictionary_enabled,
            update_include_beta: dto.update_include_beta,
            update_automatic_check: dto.update_automatic_check,
            update_automatic_check_prompt_dismissed: dto.update_automatic_check_prompt_dismissed,
            keymap: dto.keymap,
            appearance: dto.appearance,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsSnapshot {
    pub revision: String,
    pub values: PublicSettings,
    pub access: SettingsAccess,
    pub load_state: SettingsLoadState,
    pub notices: Vec<SettingsNotice>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyBindingChange {
    pub function: String,
    pub binding: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
pub enum SettingChange {
    DefaultDirect(bool),
    LiveEnabled(bool),
    LiveSearchWidth(u32),
    EphemeralEnabled(bool),
    EphemeralLegacyTrigger(String),
    ShiftLatinMode(String),
    NumberFullWidth(bool),
    PunctuationFullWidth(bool),
    SymbolFullWidth(bool),
    SymbolFullWidthChars(Vec<String>),
    TypoCorrectEnabled(bool),
    TypoCorrectLearn(bool),
    KeyBinding(KeyBindingChange),
    AppearanceTheme(String),
    AppearanceFontFamily(String),
    AppearanceFontPoint(f32),
    AppearanceBackdrop(String),
    AppearanceCorner(String),
    AppearancePalettes {
        light: settings::Palette,
        dark: settings::Palette,
    },
    ReadingMonitorEnabled(bool),
    ReadingMonitorAccumulate(bool),
    ReadingMonitorMaxChars(u32),
    UserDictionaryEnabled(bool),
    LearningEnabled(bool),
    ZenzaiEnabled(bool),
    WeightPath(String),
    ZenzaiInferenceLimit(u32),
    InlinePredictionEnabled(bool),
    UpdateIncludeBeta(bool),
    FeedbackEnabled(bool),
}

impl SettingChange {
    fn target(&self) -> String {
        match self {
            Self::KeyBinding(change) => format!("keymap.{}", change.function),
            Self::AppearancePalettes { .. } => "appearance.palettes".into(),
            other => serde_json::to_value(other)
                .ok()
                .and_then(|value| value.get("field")?.as_str().map(str::to_owned))
                .unwrap_or_else(|| "settings".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatchRequest {
    pub operation_id: String,
    pub base_revision: String,
    pub changes: Vec<SettingChange>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectStatus {
    pub target: String,
    pub state: String,
    pub condition: String,
    pub observed: String,
    pub message: String,
    pub checked_at: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SettingsPatchResult {
    Saved {
        operation_id: String,
        snapshot: SettingsSnapshot,
        effects: Vec<EffectStatus>,
    },
    Conflict {
        operation_id: String,
        snapshot: SettingsSnapshot,
    },
    Rejected {
        operation_id: String,
        errors: Vec<FieldError>,
        snapshot: Option<SettingsSnapshot>,
    },
}

#[derive(Debug, Clone)]
struct CompletedOperation {
    id: String,
    fingerprint: String,
    result: SettingsPatchResult,
}

#[derive(Default)]
pub struct SettingsService {
    completed: std::sync::Mutex<VecDeque<CompletedOperation>>,
}

impl SettingsService {
    fn previous(&self, request: &SettingsPatchRequest) -> Option<SettingsPatchResult> {
        let fingerprint = request_fingerprint(request);
        self.completed.lock().ok().and_then(|completed| {
            completed
                .iter()
                .find(|item| item.id == request.operation_id)
                .map(|item| {
                    if item.fingerprint == fingerprint {
                        item.result.clone()
                    } else {
                        SettingsPatchResult::Rejected {
                            operation_id: request.operation_id.clone(),
                            errors: vec![FieldError {
                                field: "_operation".into(),
                                message: "同じ操作IDで異なる変更は保存できません。".into(),
                            }],
                            snapshot: None,
                        }
                    }
                })
        })
    }

    fn remember(&self, request: &SettingsPatchRequest, result: SettingsPatchResult) {
        if let Ok(mut completed) = self.completed.lock() {
            completed.push_back(CompletedOperation {
                id: request.operation_id.clone(),
                fingerprint: request_fingerprint(request),
                result,
            });
            while completed.len() > OPERATION_HISTORY_LIMIT {
                completed.pop_front();
            }
        }
    }

    pub fn operation_status(&self, operation_id: &str) -> Option<SettingsPatchResult> {
        self.completed.lock().ok().and_then(|completed| {
            completed
                .iter()
                .find(|item| item.id == operation_id)
                .map(|item| item.result.clone())
        })
    }
}

fn request_fingerprint(request: &SettingsPatchRequest) -> String {
    let bytes = serde_json::to_vec(request).unwrap_or_default();
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn current_revision() -> String {
    let Some(path) = settings::settings_path() else {
        return "unavailable:no_path".into();
    };
    match std::fs::read(path) {
        Ok(bytes) => format!("sha256:{:x}", Sha256::digest(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing".into(),
        Err(error) => format!("unavailable:{:?}", error.kind()).to_lowercase(),
    }
}

fn project_load_outcome(outcome: settings::LoadOutcome) -> (SettingsAccess, SettingsLoadState) {
    use settings::LoadOutcome::*;
    match outcome {
        Loaded => (SettingsAccess::Writable, SettingsLoadState::Loaded),
        Missing => (SettingsAccess::Writable, SettingsLoadState::Missing),
        Empty => (SettingsAccess::Writable, SettingsLoadState::Empty),
        Corrupt => (
            SettingsAccess::Writable,
            SettingsLoadState::CorruptRecovered,
        ),
        PermissionDenied => (
            SettingsAccess::ReadOnly,
            SettingsLoadState::PermissionDenied,
        ),
        IoError => (SettingsAccess::ReadOnly, SettingsLoadState::IoError),
        NoPath => (SettingsAccess::ReadOnly, SettingsLoadState::NoPath),
        CorruptQuarantineFailed => (
            SettingsAccess::ReadOnly,
            SettingsLoadState::CorruptQuarantineFailed,
        ),
        UnsupportedVersion => (
            SettingsAccess::ReadOnly,
            SettingsLoadState::UnsupportedVersion,
        ),
    }
}

pub fn settings_snapshot() -> SettingsSnapshot {
    let (settings, outcome) = settings::load_reporting();
    with_pending_recovery_notice(snapshot_from(settings, outcome))
}

fn snapshot_read_only() -> SettingsSnapshot {
    let (settings, outcome) = settings::load_reporting_read_only();
    with_pending_recovery_notice(snapshot_from(settings, outcome))
}

fn with_pending_recovery_notice(mut snapshot: SettingsSnapshot) -> SettingsSnapshot {
    if settings::has_pending_corrupt_recovery_notice() {
        let location = settings::latest_corrupt_backup_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "退避先を確認できません".into());
        snapshot
            .notices
            .retain(|notice| notice.kind != "corrupt_recovered");
        snapshot.notices.push(SettingsNotice {
            kind: "corrupt_recovered".into(),
            message: format!(
                "壊れた設定ファイルを退避し、既定値で復旧しました。退避先: {location}"
            ),
        });
    }
    snapshot
}

fn snapshot_from(settings: settings::Settings, outcome: settings::LoadOutcome) -> SettingsSnapshot {
    let (access, load_state) = project_load_outcome(outcome);
    let notices = match outcome {
        settings::LoadOutcome::Corrupt => Vec::new(),
        settings::LoadOutcome::UnsupportedVersion => vec![SettingsNotice {
            kind: "error".into(),
            message: "新しい形式の設定です。この版では読み取り専用です。".into(),
        }],
        settings::LoadOutcome::PermissionDenied => vec![SettingsNotice {
            kind: "error".into(),
            message: "設定ファイルへのアクセスが拒否されました。".into(),
        }],
        settings::LoadOutcome::IoError
        | settings::LoadOutcome::NoPath
        | settings::LoadOutcome::CorruptQuarantineFailed => vec![SettingsNotice {
            kind: "error".into(),
            message: "設定を安全に読み書きできません。診断・詳細を確認してください。".into(),
        }],
        _ => Vec::new(),
    };
    SettingsSnapshot {
        revision: current_revision(),
        values: PublicSettings::from(&settings),
        access,
        load_state,
        notices,
    }
}

fn keymap_slot<'a>(
    keymap: &'a mut settings::keymap::KeymapSettings,
    function: &str,
) -> Option<&'a mut Option<String>> {
    match function {
        "mode_toggle" => Some(&mut keymap.mode_toggle),
        "reconvert" => Some(&mut keymap.reconvert),
        "feedback" => Some(&mut keymap.feedback),
        "ephemeral" => Some(&mut keymap.ephemeral),
        "commit_undo" => Some(&mut keymap.commit_undo),
        "typo_correct" => Some(&mut keymap.typo_correct),
        "to_hiragana" => Some(&mut keymap.to_hiragana),
        "to_katakana" => Some(&mut keymap.to_katakana),
        "to_hankaku_kana" => Some(&mut keymap.to_hankaku_kana),
        "to_zenkaku_eisu" => Some(&mut keymap.to_zenkaku_eisu),
        "to_hankaku_eisu" => Some(&mut keymap.to_hankaku_eisu),
        "notation_rotate" => Some(&mut keymap.notation_rotate),
        "convert" => Some(&mut keymap.convert),
        _ => None,
    }
}

fn apply_changes(
    mut settings: settings::Settings,
    changes: &[SettingChange],
) -> Result<settings::Settings, Vec<FieldError>> {
    let mut errors = Vec::new();
    for change in changes {
        match change {
            SettingChange::DefaultDirect(value) => settings.default_direct = *value,
            SettingChange::LiveEnabled(value) => settings.live_conversion.enabled = *value,
            SettingChange::LiveSearchWidth(value) => settings.live_conversion.search_width = *value,
            SettingChange::EphemeralEnabled(value) => settings.ephemeral.enabled = *value,
            SettingChange::EphemeralLegacyTrigger(value) => {
                settings.ephemeral.trigger = value.clone()
            }
            SettingChange::ShiftLatinMode(value) => settings.shift_latin.mode = value.clone(),
            SettingChange::NumberFullWidth(value) => settings.number.full_width = *value,
            SettingChange::PunctuationFullWidth(value) => settings.punctuation.full_width = *value,
            SettingChange::SymbolFullWidth(value) => settings.symbol.full_width = *value,
            SettingChange::SymbolFullWidthChars(value) => {
                settings.symbol.full_width_chars = value
                    .iter()
                    .filter_map(|value| {
                        let mut chars = value.chars();
                        match (chars.next(), chars.next()) {
                            (Some(character), None) => Some(character),
                            _ => None,
                        }
                    })
                    .collect();
            }
            SettingChange::TypoCorrectEnabled(value) => settings.typo_correct.enabled = *value,
            SettingChange::TypoCorrectLearn(value) => settings.typo_correct.learn = *value,
            SettingChange::KeyBinding(change) => {
                if let Some(slot) = keymap_slot(&mut settings.keymap, &change.function) {
                    *slot = change.binding.clone();
                } else {
                    errors.push(FieldError {
                        field: "key_binding".into(),
                        message: format!("不明なキー操作です: {}", change.function),
                    });
                }
            }
            SettingChange::AppearanceTheme(value) => settings.appearance.theme = value.clone(),
            SettingChange::AppearanceFontFamily(value) => {
                settings.appearance.font_family = value.clone()
            }
            SettingChange::AppearanceFontPoint(value) => settings.appearance.font_point = *value,
            SettingChange::AppearanceBackdrop(value) => {
                settings.appearance.backdrop = value.clone()
            }
            SettingChange::AppearanceCorner(value) => settings.appearance.corner = value.clone(),
            SettingChange::AppearancePalettes { light, dark } => {
                settings.appearance.palette_light = light.clone();
                settings.appearance.palette_dark = dark.clone();
            }
            SettingChange::ReadingMonitorEnabled(value) => {
                settings.reading_monitor.enabled = *value
            }
            SettingChange::ReadingMonitorAccumulate(value) => {
                settings.reading_monitor.accumulate = *value
            }
            SettingChange::ReadingMonitorMaxChars(value) => {
                settings.reading_monitor.max_chars = *value
            }
            SettingChange::UserDictionaryEnabled(value) => {
                settings.user_dictionary.enabled = *value
            }
            SettingChange::LearningEnabled(value) => settings.learning.enabled = *value,
            SettingChange::ZenzaiEnabled(value) => settings.zenzai.enabled = *value,
            SettingChange::WeightPath(value) => settings.zenzai.weight_path = value.clone(),
            SettingChange::ZenzaiInferenceLimit(value) => settings.zenzai.inference_limit = *value,
            SettingChange::InlinePredictionEnabled(value) => {
                settings.inline_prediction.enabled = *value
            }
            SettingChange::UpdateIncludeBeta(value) => settings.update.include_beta = *value,
            SettingChange::FeedbackEnabled(value) => settings.feedback.enabled = *value,
        }
    }
    errors.extend(logic::validate(&logic::to_dto(&settings)));
    if errors.is_empty() {
        settings.reading_monitor.max_chars = settings.reading_monitor.effective_max_chars();
        Ok(settings)
    } else {
        Err(errors)
    }
}

pub fn settings_patch(
    service: &SettingsService,
    lock: &logic::SettingsLock,
    request: SettingsPatchRequest,
) -> SettingsPatchResult {
    if let Some(previous) = service.previous(&request) {
        return previous;
    }
    let _guard = match lock.0.lock() {
        Ok(guard) => guard,
        Err(_) => {
            return SettingsPatchResult::Rejected {
                operation_id: request.operation_id,
                errors: vec![FieldError {
                    field: "_io".into(),
                    message: "設定ロックを取得できないため、変更を保存できません。".into(),
                }],
                snapshot: None,
            };
        }
    };
    let current_revision = current_revision();
    if current_revision != request.base_revision {
        let result = SettingsPatchResult::Conflict {
            operation_id: request.operation_id.clone(),
            snapshot: snapshot_read_only(),
        };
        service.remember(&request, result.clone());
        return result;
    }
    let current = match settings::load_for_mutation() {
        Ok(settings) => settings,
        Err(outcome) => {
            let result = SettingsPatchResult::Rejected {
                operation_id: request.operation_id.clone(),
                errors: vec![FieldError {
                    field: "_io".into(),
                    message: format!("設定を安全に変更できません（{outcome:?}）。"),
                }],
                snapshot: Some(snapshot_read_only()),
            };
            service.remember(&request, result.clone());
            return result;
        }
    };
    if request.changes.iter().any(|change| {
        matches!(change, SettingChange::InlinePredictionEnabled(true))
            && !current.inline_prediction.enabled
            && !crate::prediction_download::local_model_is_ready()
    }) {
        let result = SettingsPatchResult::Rejected {
            operation_id: request.operation_id.clone(),
            errors: vec![FieldError {
                field: "inline_prediction_enabled".into(),
                message: "インライン予測を有効にするには、先にモデルを導入してください。".into(),
            }],
            snapshot: Some(snapshot_read_only()),
        };
        service.remember(&request, result.clone());
        return result;
    }
    let next = match apply_changes(current, &request.changes) {
        Ok(settings) => settings,
        Err(errors) => {
            let result = SettingsPatchResult::Rejected {
                operation_id: request.operation_id.clone(),
                errors,
                snapshot: Some(snapshot_read_only()),
            };
            service.remember(&request, result.clone());
            return result;
        }
    };
    if let Err(error) = persist_settings(&next) {
        let result = SettingsPatchResult::Rejected {
            operation_id: request.operation_id.clone(),
            errors: vec![FieldError {
                field: "_io".into(),
                message: format!("設定を保存できませんでした: {error}"),
            }],
            snapshot: Some(snapshot_read_only()),
        };
        service.remember(&request, result.clone());
        return result;
    }
    for change in &request.changes {
        match change {
            SettingChange::ZenzaiEnabled(_) => crate::download::invalidate_activation_intent(),
            SettingChange::InlinePredictionEnabled(_) => {
                crate::prediction_download::invalidate_activation_intent()
            }
            _ => {}
        }
    }
    let checked_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let effects = request
        .changes
        .iter()
        .map(|change| {
            let target = change.target();
            let condition = if target == "update_include_beta" {
                "次回の更新確認"
            } else if target == "live_search_width" {
                "入力先を開き直した後"
            } else if target.starts_with("appearance_") {
                "候補または読み表示の次回描画"
            } else {
                "次回の入力またはエンジン接続"
            };
            EffectStatus {
                target,
                state: "not_observed".into(),
                condition: condition.into(),
                observed: "保存のみ確認済み。実動作は未確認です。".into(),
                message: format!("保存しました。反映: {condition}（実動作は未確認）"),
                checked_at,
            }
        })
        .collect();
    let result = SettingsPatchResult::Saved {
        operation_id: request.operation_id.clone(),
        snapshot: snapshot_read_only(),
        effects,
    };
    service.remember(&request, result.clone());
    result
}

/// Single persistence seam for Config-owned settings writes. Long-running operations keep
/// their resource-specific ordering, but delegate the final atomic settings commit here.
pub(crate) fn persist_settings(settings: &settings::Settings) -> std::io::Result<()> {
    settings::save(settings)
}

pub fn default_public_settings() -> PublicSettings {
    PublicSettings::from(&settings::Settings::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_search_width_patch_roundtrips_and_rejects_other_widths() {
        for width in [10, 1] {
            let original = settings::Settings::default();
            let request: SettingChange = serde_json::from_value(serde_json::json!({
                "field": "live_search_width", "value": width,
            })).unwrap();
            let changed = apply_changes(original.clone(), &[request]).unwrap();
            let restored: settings::Settings = serde_json::from_str(
                &serde_json::to_string(&changed).unwrap()).unwrap();
            let public = serde_json::to_value(PublicSettings::from(&restored)).unwrap();
            assert_eq!(public["liveSearchWidth"], width);
            assert_eq!(restored.zenzai.inference_limit, original.zenzai.inference_limit);
            assert_eq!(restored.live_conversion.enabled, original.live_conversion.enabled);
        }
        for width in [0, 2, 9, 11, u32::MAX] {
            let errors = apply_changes(settings::Settings::default(),
                &[SettingChange::LiveSearchWidth(width)]).unwrap_err();
            assert!(errors.iter().any(|error| error.field == "live_search_width"));
        }
    }

    #[test]
    fn patch_changes_only_named_fields_and_exposes_hidden_typo_settings() {
        let mut original = settings::Settings::default();
        original.llm.api_key_dpapi = "secret-blob".into();
        original.appearance.theme = "dark".into();
        let changed = apply_changes(
            original.clone(),
            &[
                SettingChange::NumberFullWidth(false),
                SettingChange::TypoCorrectEnabled(false),
            ],
        )
        .unwrap();
        assert!(!changed.number.full_width);
        assert!(!changed.typo_correct.enabled);
        assert_eq!(changed.appearance.theme, original.appearance.theme);
        assert_eq!(changed.llm.api_key_dpapi, "secret-blob");
        let public = PublicSettings::from(&changed);
        assert!(!public.typo_correct_enabled);
    }

    #[test]
    fn patch_revalidates_key_conflicts_after_feature_enablement() {
        let mut original = settings::Settings::default();
        original.feedback.enabled = false;
        original.keymap.typo_correct = Some("Ctrl+Slash".into());
        let errors = apply_changes(original, &[SettingChange::FeedbackEnabled(true)]).unwrap_err();
        assert!(errors
            .iter()
            .any(|error| error.field.starts_with("keymap.")));
    }

    #[test]
    fn frozen_llm_binding_is_not_a_writable_public_setting() {
        let original = settings::Settings::default();
        let errors = apply_changes(
            original,
            &[SettingChange::KeyBinding(KeyBindingChange {
                function: "llm_convert".into(),
                binding: None,
            })],
        )
        .unwrap_err();
        assert_eq!(errors[0].field, "key_binding");
    }

    #[test]
    fn operation_id_replay_is_idempotent_and_mismatch_is_rejected() {
        let service = SettingsService::default();
        let first = SettingsPatchRequest {
            operation_id: "op-1".into(),
            base_revision: "missing".into(),
            changes: vec![SettingChange::DefaultDirect(true)],
        };
        let result = SettingsPatchResult::Rejected {
            operation_id: "op-1".into(),
            errors: Vec::new(),
            snapshot: None,
        };
        service.remember(&first, result);
        assert!(service.previous(&first).is_some());
        let mut different = first;
        different.changes = vec![SettingChange::DefaultDirect(false)];
        match service.previous(&different).unwrap() {
            SettingsPatchResult::Rejected { errors, .. } => {
                assert_eq!(errors[0].field, "_operation")
            }
            _ => panic!("mismatched operation id must be rejected"),
        }
    }

    #[test]
    fn typescript_boundary_uses_camel_case_requests_and_tagged_changes() {
        let request: SettingsPatchRequest = serde_json::from_value(serde_json::json!({
            "operationId": "op-ts",
            "baseRevision": "missing",
            "changes": [
                {"field": "default_direct", "value": true},
                {"field": "key_binding", "value": {"function": "ephemeral", "binding": "F9"}}
            ]
        }))
        .unwrap();
        assert_eq!(request.operation_id, "op-ts");
        assert_eq!(request.changes[0], SettingChange::DefaultDirect(true));
        assert_eq!(
            request.changes[1],
            SettingChange::KeyBinding(KeyBindingChange {
                function: "ephemeral".into(),
                binding: Some("F9".into()),
            })
        );

        let result = SettingsPatchResult::Saved {
            operation_id: "op-ts".into(),
            snapshot: snapshot_from(
                settings::Settings::default(),
                settings::LoadOutcome::Missing,
            ),
            effects: Vec::new(),
        };
        let json = serde_json::to_value(result).unwrap();
        assert_eq!(json["operationId"], "op-ts");
        assert!(json["snapshot"]["values"]["defaultDirect"].is_boolean());
        assert!(json.get("operation_id").is_none());
    }
}
