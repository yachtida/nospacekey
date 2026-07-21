//! フロント⇔settings 変換・検証・鍵3態の純ロジック。tauri 非依存（単体テスト対象）。
//!
//! 鍵の扱い（旧 nwg 版 main.rs から移植）:
//! - 表示: 平文は渡さない。設定済みなら KEY_PLACEHOLDER、未設定なら空文字。
//! - 保存: プレースホルダのまま=未変更→既存 blob 維持 / 空=明示削除 /
//!   その他=新規入力→encrypt 成功時のみ上書き（失敗時は既存 blob 維持）。

use serde::{Deserialize, Serialize};

/// 鍵フィールドの「設定済み」プレースホルダ。これが入力欄の値のまま適用されたら
/// 「変更なし」とみなし、既存の DPAPI blob を保持する（鍵を消さない）。
pub const KEY_PLACEHOLDER: &str = "(設定済み — 変更する場合のみ入力)";

/// LLM タイムアウトの妥当範囲（ms）。0 は即時タイムアウトで無意味、極端に大きい値は誤入力なので弾く。
pub const TIMEOUT_MS_RANGE: std::ops::RangeInclusive<u32> = 1..=600_000;

/// フィールド単位の検証エラー。field はフロントの data-error-for と一致させる。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

/// フロントとやり取りする形。settings::Settings と鍵の扱いだけが違う
/// （api_key_dpapi の代わりに表示用テキスト api_key_input を持つ）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsDto {
    pub llm_enabled: bool,
    pub api_key_input: String,
    pub endpoint: String,
    pub model: String,
    pub prompt: String,
    pub timeout_ms: u32,
    pub zenzai_enabled: bool,
    pub weight_path: String,
    pub live_enabled: bool,
    pub default_direct: bool,
    pub learning_enabled: bool,
    /// 品質ループ③: 誤変換フィードバック記録（feedback.jsonl）。既定 false=opt-in。
    pub feedback_enabled: bool,
    /// かな入力モードで数字を既定で全角確定するか（既定 true）。
    pub number_full_width: bool,
    /// かな入力モードで句読点を既定で全角確定するか（既定 true）。
    pub punctuation_full_width: bool,
    /// かな入力モードで記号を既定で全角確定するか（既定 false）。
    pub symbol_full_width: bool,
    /// 読みモニタ（ライブ変換中の生読み常時表示、既定 true）。
    pub reading_monitor_enabled: bool,
    /// 読みモニタ: 自動確定をまたいで読みを累積表示する（既定 true）。
    pub reading_monitor_accumulate: bool,
    /// 読みモニタ: 窓の表示上限（全角文字数換算、既定 34。apply 時に 10..=100 へクランプ）。
    pub reading_monitor_max_chars: u32,
    /// 一時的なかなモードを有効にするか（既定 true）。
    pub ephemeral_enabled: bool,
    /// 一時的なかなモードの旧トリガキー設定（"f8"|"f9"|"f10"、既定 "f8"）。
    /// UI には露出しない読み取り専用の移行フィールド: トリガキーの変更は keymap.ephemeral に
    /// 一本化した（キー設定ページ）。この値は keymap.ephemeral 不在時の既定の解決
    /// （TIP の default_chords / キー設定ページの既定表示）にだけ使われ、素通しで保存される。
    pub ephemeral_trigger: String,
    /// Shift+英字の挙動（"compose"=英語未確定モード / "commit"=大文字直接確定、既定 "compose"）。
    pub shift_latin_mode: String,
    pub keymap: settings::keymap::KeymapSettings,
    pub appearance: settings::Appearance,
}

/// Settings → フロント表示用 DTO。鍵はマスク（平文もblobも渡さない）。
pub fn to_dto(s: &settings::Settings) -> SettingsDto {
    SettingsDto {
        llm_enabled: s.llm.enabled,
        api_key_input: if s.llm.api_key_dpapi.is_empty() {
            String::new()
        } else {
            KEY_PLACEHOLDER.to_string()
        },
        endpoint: s.llm.endpoint.clone(),
        model: s.llm.model.clone(),
        prompt: s.llm.prompt.clone(),
        timeout_ms: s.llm.timeout_ms,
        zenzai_enabled: s.zenzai.enabled,
        weight_path: s.zenzai.weight_path.clone(),
        live_enabled: s.live_conversion.enabled,
        default_direct: s.default_direct,
        learning_enabled: s.learning.enabled,
        feedback_enabled: s.feedback.enabled,
        number_full_width: s.number.full_width,
        punctuation_full_width: s.punctuation.full_width,
        symbol_full_width: s.symbol.full_width,
        reading_monitor_enabled: s.reading_monitor.enabled,
        reading_monitor_accumulate: s.reading_monitor.accumulate,
        reading_monitor_max_chars: s.reading_monitor.max_chars,
        ephemeral_enabled: s.ephemeral.enabled,
        ephemeral_trigger: s.ephemeral.trigger.clone(),
        shift_latin_mode: s.shift_latin.mode.clone(),
        keymap: s.keymap.clone(),
        appearance: s.appearance.clone(),
    }
}

/// `#RRGGBB`（# + 6 桁16進、3桁短縮不可）のみ許可。settings::parse_hex_color と同じ制約。
fn is_valid_hex(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// DTO 全体を検証してフィールド単位のエラーを返す。空 Vec = 妥当。
pub fn validate(dto: &SettingsDto) -> Vec<FieldError> {
    let mut errs = Vec::new();
    if !TIMEOUT_MS_RANGE.contains(&dto.timeout_ms) {
        errs.push(FieldError {
            field: "timeout_ms".into(),
            message: format!(
                "タイムアウトは {}〜{} ms の範囲で入力してください。",
                TIMEOUT_MS_RANGE.start(),
                TIMEOUT_MS_RANGE.end()
            ),
        });
    }
    // 列挙値はラジオUI由来だが、defense-in-depth で検証する（TIP 側は未知値を握り潰すため
    // 黙って既定になる事故を防ぐ）。
    let a = &dto.appearance;
    if !["auto", "light", "dark", "custom"].contains(&a.theme.as_str()) {
        errs.push(FieldError {
            field: "appearance.theme".into(),
            message: format!("不正なテーマ値です: {:?}", a.theme),
        });
    }
    if !["acrylic", "opaque"].contains(&a.backdrop.as_str()) {
        errs.push(FieldError {
            field: "appearance.backdrop".into(),
            message: format!("不正な背景値です: {:?}", a.backdrop),
        });
    }
    if !["round", "square"].contains(&a.corner.as_str()) {
        errs.push(FieldError {
            field: "appearance.corner".into(),
            message: format!("不正な角丸値です: {:?}", a.corner),
        });
    }
    if !["f8", "f9", "f10"].contains(&dto.ephemeral_trigger.as_str()) {
        errs.push(FieldError {
            field: "ephemeral_trigger".into(),
            message: format!("不正なトリガキーです: {:?}", dto.ephemeral_trigger),
        });
    }
    if !["compose", "commit"].contains(&dto.shift_latin_mode.as_str()) {
        errs.push(FieldError {
            field: "shift_latin_mode".into(),
            message: format!("不正な Shift+英字設定です: {:?}", dto.shift_latin_mode),
        });
    }
    // UI は 6..=24 の number 入力だが、NaN→0 化や手編集 JSON に備え広めの範囲で防御する。
    if !a.font_point.is_finite() || !(4.0..=32.0).contains(&a.font_point) {
        errs.push(FieldError {
            field: "appearance.font_point".into(),
            message: "フォントサイズは 4〜32 pt の範囲で入力してください。".into(),
        });
    }
    for (pal_name, pal) in [
        ("palette_light", &a.palette_light),
        ("palette_dark", &a.palette_dark),
    ] {
        let fields: [(&str, &str); 7] = [
            ("bg", &pal.bg),
            ("text", &pal.text),
            ("index", &pal.index),
            ("sel_bg", &pal.sel_bg),
            ("sel_text", &pal.sel_text),
            ("sel_index", &pal.sel_index),
            ("border", &pal.border),
        ];
        for (key, value) in fields {
            if !is_valid_hex(value) {
                errs.push(FieldError {
                    field: format!("{pal_name}.{key}"),
                    message: format!("#RRGGBB 形式（#+16進6桁）で入力してください: {value:?}"),
                });
            }
        }
    }
    // keymap: 個別バインドの妥当性(共有パーサ)と、文脈グループ内の衝突。
    for f in settings::keymap::ALL_FUNCS {
        if let Some(v) = dto.keymap.get(f) {
            if let Err(message) = settings::keymap::validate_binding(f, v) {
                errs.push(FieldError {
                    field: format!("keymap.{}", f.settings_field()),
                    message,
                });
            }
        }
    }
    for c in settings::keymap::find_conflicts(
        &dto.keymap,
        &dto.ephemeral_trigger,
        dto.ephemeral_enabled,
        dto.feedback_enabled,
        true, // typo_correct.enabled は DTO に無い(GUI 未露出)ため常に参加させる(安全側)
        settings::llm_effective(dto.llm_enabled), // 凍結中は llm_convert を衝突判定から外す
    ) {
        errs.push(FieldError {
            field: format!("keymap.{}", c.a.settings_field()),
            message: format!(
                "「{}」と同じキー({})に割り当てられています",
                c.b.label_ja(),
                settings::keymap::format_chord(&c.chord)
            ),
        });
        errs.push(FieldError {
            field: format!("keymap.{}", c.b.settings_field()),
            message: format!(
                "「{}」と同じキー({})に割り当てられています",
                c.a.label_ja(),
                settings::keymap::format_chord(&c.chord)
            ),
        });
    }
    errs
}

/// DTO を検証し、prev（ディスク上の現行 Settings）に重ねて保存用 Settings を作る。
/// version と「未変更の鍵 blob」は prev から引き継ぐ。encrypt は注入（テストで差し替え）。
pub fn apply_dto(
    dto: SettingsDto,
    prev: &settings::Settings,
    encrypt: impl Fn(&str) -> Option<String>,
) -> Result<settings::Settings, Vec<FieldError>> {
    let errs = validate(&dto);
    if !errs.is_empty() {
        return Err(errs);
    }
    let mut s = prev.clone();
    s.llm.enabled = dto.llm_enabled;
    s.llm.endpoint = dto.endpoint;
    s.llm.model = dto.model;
    s.llm.prompt = dto.prompt;
    s.llm.timeout_ms = dto.timeout_ms;

    let key = dto.api_key_input.trim();
    if key == KEY_PLACEHOLDER {
        // 未変更: prev の blob を維持（clone 済み）。
    } else if key.is_empty() {
        // 明示削除（フロントで確認済みの前提）。
        s.llm.api_key_dpapi = String::new();
    } else if let Some(blob) = encrypt(key) {
        // 新規入力: 暗号化成功時のみ上書き（失敗時は既存 blob 維持 — 旧実装踏襲）。
        s.llm.api_key_dpapi = blob;
    }

    s.zenzai.enabled = dto.zenzai_enabled;
    s.zenzai.weight_path = dto.weight_path;
    s.live_conversion.enabled = dto.live_enabled;
    s.default_direct = dto.default_direct;
    s.learning.enabled = dto.learning_enabled;
    s.feedback.enabled = dto.feedback_enabled;
    s.number.full_width = dto.number_full_width;
    s.punctuation.full_width = dto.punctuation_full_width;
    s.symbol.full_width = dto.symbol_full_width;
    s.reading_monitor.enabled = dto.reading_monitor_enabled;
    s.reading_monitor.accumulate = dto.reading_monitor_accumulate;
    // 範囲外はエラーでなくクランプ(spec 決定)。正規化点は settings::effective_max_chars。
    s.reading_monitor.max_chars = dto.reading_monitor_max_chars;
    s.reading_monitor.max_chars = s.reading_monitor.effective_max_chars();
    s.ephemeral.enabled = dto.ephemeral_enabled;
    s.ephemeral.trigger = dto.ephemeral_trigger; // validate 済み（未知値は上で Err 済み）
    s.shift_latin.mode = dto.shift_latin_mode; // validate 済み（同上）
    s.keymap = dto.keymap;
    s.appearance = dto.appearance;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_dto() -> SettingsDto {
        to_dto(&settings::Settings::default())
    }

    fn prev_with_key() -> settings::Settings {
        let mut s = settings::Settings::default();
        s.llm.api_key_dpapi = "EXISTING_BLOB".into();
        s
    }

    #[test]
    fn to_dto_masks_key() {
        assert_eq!(to_dto(&settings::Settings::default()).api_key_input, "");
        assert_eq!(to_dto(&prev_with_key()).api_key_input, KEY_PLACEHOLDER);
    }

    #[test]
    fn hex_validation() {
        assert!(is_valid_hex("#FAFAFA"));
        assert!(is_valid_hex("#0078d7"));
        assert!(!is_valid_hex("FAFAFA")); // # なし
        assert!(!is_valid_hex("#FFF")); // 3桁短縮
        assert!(!is_valid_hex("#GGGGGG")); // 16進でない
        assert!(!is_valid_hex("#FFFFFFF")); // 7桁
        assert!(!is_valid_hex(""));
    }

    #[test]
    fn validate_default_is_clean() {
        assert!(validate(&base_dto()).is_empty());
    }

    #[test]
    fn validate_rejects_bad_timeout_and_palette() {
        let mut dto = base_dto();
        dto.timeout_ms = 0;
        dto.appearance.palette_light.bg = "red".into();
        let errs = validate(&dto);
        assert!(errs.iter().any(|e| e.field == "timeout_ms"));
        assert!(errs.iter().any(|e| e.field == "palette_light.bg"));
        assert_eq!(errs.len(), 2);
    }

    #[test]
    fn validate_rejects_bad_font_point() {
        let mut dto = base_dto();
        dto.appearance.font_point = 0.0;
        assert!(validate(&dto)
            .iter()
            .any(|e| e.field == "appearance.font_point"));
        dto.appearance.font_point = f32::NAN;
        assert!(validate(&dto)
            .iter()
            .any(|e| e.field == "appearance.font_point"));
        dto.appearance.font_point = 10.5;
        assert!(validate(&dto).is_empty());
    }

    #[test]
    fn validate_rejects_unknown_enums() {
        let mut dto = base_dto();
        dto.appearance.theme = "sepia".into();
        dto.appearance.backdrop = "glass".into();
        dto.appearance.corner = "bevel".into();
        let fields: Vec<_> = validate(&dto).into_iter().map(|e| e.field).collect();
        assert_eq!(
            fields,
            vec![
                "appearance.theme",
                "appearance.backdrop",
                "appearance.corner"
            ]
        );
    }

    #[test]
    fn key_placeholder_keeps_existing_blob() {
        let mut dto = base_dto();
        dto.api_key_input = KEY_PLACEHOLDER.to_string();
        let s = apply_dto(dto, &prev_with_key(), |_| {
            panic!("encrypt must not be called")
        })
        .unwrap();
        assert_eq!(s.llm.api_key_dpapi, "EXISTING_BLOB");
    }

    #[test]
    fn empty_key_clears_blob() {
        let mut dto = base_dto();
        dto.api_key_input = "   ".to_string(); // trim で空
        let s = apply_dto(dto, &prev_with_key(), |_| {
            panic!("encrypt must not be called")
        })
        .unwrap();
        assert_eq!(s.llm.api_key_dpapi, "");
    }

    #[test]
    fn new_key_encrypts_and_overwrites() {
        let mut dto = base_dto();
        dto.api_key_input = "sk-new".to_string();
        let s = apply_dto(dto, &prev_with_key(), |p| Some(format!("ENC({p})"))).unwrap();
        assert_eq!(s.llm.api_key_dpapi, "ENC(sk-new)");
    }

    #[test]
    fn encrypt_failure_keeps_existing_blob() {
        let mut dto = base_dto();
        dto.api_key_input = "sk-new".to_string();
        let s = apply_dto(dto, &prev_with_key(), |_| None).unwrap();
        assert_eq!(s.llm.api_key_dpapi, "EXISTING_BLOB");
    }

    #[test]
    fn apply_preserves_version_and_maps_fields() {
        let mut prev = settings::Settings::default();
        prev.version = 7;
        let mut dto = base_dto();
        dto.llm_enabled = true;
        dto.endpoint = "https://example.invalid/v1".into();
        dto.timeout_ms = 250;
        dto.zenzai_enabled = false;
        dto.weight_path = r"C:\models\w.gguf".into();
        dto.live_enabled = false;
        dto.default_direct = true;
        dto.appearance.theme = "custom".into();
        dto.appearance.palette_light.bg = "#112233".into();
        let s = apply_dto(dto, &prev, |_| None).unwrap();
        assert_eq!(s.version, 7);
        assert!(s.llm.enabled);
        assert_eq!(s.llm.endpoint, "https://example.invalid/v1");
        assert_eq!(s.llm.timeout_ms, 250);
        assert!(!s.zenzai.enabled);
        assert_eq!(s.zenzai.weight_path, r"C:\models\w.gguf");
        assert!(!s.live_conversion.enabled);
        assert!(s.default_direct);
        assert_eq!(s.appearance.theme, "custom");
        assert_eq!(s.appearance.palette_light.bg, "#112233");
    }

    #[test]
    fn learning_enabled_roundtrips_between_dto_and_settings() {
        // Settings → DTO
        let mut s = settings::Settings::default();
        assert!(to_dto(&s).learning_enabled, "既定 ON が DTO に写る");
        s.learning.enabled = false;
        assert!(!to_dto(&s).learning_enabled);
        // DTO → Settings（apply_dto の既存シグネチャに合わせる — encrypt は成功スタブ）
        let mut dto = to_dto(&settings::Settings::default());
        dto.learning_enabled = false;
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert!(!applied.learning.enabled, "DTO の OFF が Settings に写る");
    }

    #[test]
    fn number_full_width_roundtrips_between_dto_and_settings() {
        // Settings → DTO（既定 全角 が写る）
        let mut s = settings::Settings::default();
        assert!(to_dto(&s).number_full_width, "既定 全角 が DTO に写る");
        s.number.full_width = false;
        assert!(!to_dto(&s).number_full_width);
        // DTO → Settings（apply_dto、encrypt は成功スタブ）
        let mut dto = to_dto(&settings::Settings::default());
        dto.number_full_width = false;
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert!(!applied.number.full_width, "DTO の OFF が Settings に写る");
    }

    #[test]
    fn punctuation_full_width_roundtrips_between_dto_and_settings() {
        // Settings → DTO（既定 全角 が写る）
        let mut s = settings::Settings::default();
        assert!(to_dto(&s).punctuation_full_width, "既定 全角 が DTO に写る");
        s.punctuation.full_width = false;
        assert!(!to_dto(&s).punctuation_full_width);
        // DTO → Settings（apply_dto、encrypt は成功スタブ）
        let mut dto = to_dto(&settings::Settings::default());
        dto.punctuation_full_width = false;
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert!(!applied.punctuation.full_width, "DTO の OFF が Settings に写る");
    }

    #[test]
    fn symbol_full_width_roundtrips_between_dto_and_settings() {
        // Settings → DTO（既定 半角 が写る — number/punctuation と逆）
        let mut s = settings::Settings::default();
        assert!(!to_dto(&s).symbol_full_width, "既定 半角 が DTO に写る");
        s.symbol.full_width = true;
        assert!(to_dto(&s).symbol_full_width);
        // DTO → Settings（apply_dto、encrypt は成功スタブ）
        let mut dto = to_dto(&settings::Settings::default());
        dto.symbol_full_width = true;
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert!(applied.symbol.full_width, "DTO の ON が Settings に写る");
    }

    #[test]
    fn reading_monitor_roundtrips_between_dto_and_settings() {
        // Settings → DTO（既定 ON が写る）
        let mut s = settings::Settings::default();
        assert!(to_dto(&s).reading_monitor_enabled, "既定 ON が DTO に写る");
        s.reading_monitor.enabled = false;
        assert!(!to_dto(&s).reading_monitor_enabled);
        // DTO → Settings（apply_dto、encrypt は成功スタブ）
        let mut dto = to_dto(&settings::Settings::default());
        dto.reading_monitor_enabled = false;
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert!(!applied.reading_monitor.enabled, "DTO の OFF が Settings に写る");
    }

    #[test]
    fn reading_monitor_accumulate_roundtrips_between_dto_and_settings() {
        let mut s = settings::Settings::default();
        assert!(to_dto(&s).reading_monitor_accumulate, "既定 ON が DTO に写る");
        s.reading_monitor.accumulate = false;
        assert!(!to_dto(&s).reading_monitor_accumulate);
        let mut dto = to_dto(&settings::Settings::default());
        dto.reading_monitor_accumulate = false;
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert!(!applied.reading_monitor.accumulate, "DTO の OFF が Settings に写る");
    }

    #[test]
    fn reading_monitor_max_chars_roundtrips_and_clamps_on_apply() {
        let mut s = settings::Settings::default();
        assert_eq!(to_dto(&s).reading_monitor_max_chars, 34);
        s.reading_monitor.max_chars = 50;
        assert_eq!(to_dto(&s).reading_monitor_max_chars, 50);
        // apply はクランプして保存(空欄→0 で来ても 10 に正規化 — app.js は NaN を 0 に落とす)。
        let mut dto = to_dto(&settings::Settings::default());
        dto.reading_monitor_max_chars = 0;
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert_eq!(applied.reading_monitor.max_chars, 10);
        let mut dto = to_dto(&settings::Settings::default());
        dto.reading_monitor_max_chars = 42;
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert_eq!(applied.reading_monitor.max_chars, 42);
    }

    #[test]
    fn shift_latin_mode_roundtrips_between_dto_and_settings() {
        // Settings → DTO（既定 compose が写る）
        let mut s = settings::Settings::default();
        assert_eq!(to_dto(&s).shift_latin_mode, "compose", "既定 compose が DTO に写る");
        s.shift_latin.mode = "commit".into();
        assert_eq!(to_dto(&s).shift_latin_mode, "commit");
        // DTO → Settings（apply_dto、encrypt は成功スタブ）
        let mut dto = to_dto(&settings::Settings::default());
        dto.shift_latin_mode = "commit".into();
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert_eq!(applied.shift_latin.mode, "commit", "DTO の commit が Settings に写る");
    }

    #[test]
    fn validate_rejects_unknown_shift_latin_mode() {
        let mut dto = base_dto();
        dto.shift_latin_mode = "banana".into();
        let fields: Vec<_> = validate(&dto).into_iter().map(|e| e.field).collect();
        assert_eq!(fields, vec!["shift_latin_mode"]);
    }

    #[test]
    fn feedback_enabled_roundtrips_between_dto_and_settings() {
        // Settings → DTO（既定 OFF=opt-in が DTO に写る）
        let mut s = settings::Settings::default();
        assert!(!to_dto(&s).feedback_enabled, "既定 OFF が DTO に写る");
        s.feedback.enabled = true;
        assert!(to_dto(&s).feedback_enabled);
        // DTO → Settings（learning トグルと同じパターン）
        let mut dto = to_dto(&settings::Settings::default());
        dto.feedback_enabled = true;
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert!(applied.feedback.enabled, "DTO の ON が Settings に写る");
    }

    #[test]
    fn ephemeral_settings_roundtrip_and_validate() {
        let mut s = settings::Settings::default();
        s.ephemeral.enabled = false;
        s.ephemeral.trigger = "f9".into();
        let dto = to_dto(&s);
        assert!(!dto.ephemeral_enabled);
        assert_eq!(dto.ephemeral_trigger, "f9");
        let back = apply_dto(dto.clone(), &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert_eq!(back.ephemeral.trigger, "f9");
        // 未知 trigger は validate が拒否する（apply_dto も Err で拒否する）。
        let mut bad = dto.clone();
        bad.ephemeral_trigger = "ctrl_z".into();
        assert!(validate(&bad).iter().any(|e| e.field == "ephemeral_trigger"));
        assert!(apply_dto(bad, &settings::Settings::default(), |v| Some(v.to_string())).is_err());
    }

    #[test]
    fn apply_rejects_invalid_without_touching_key() {
        let mut dto = base_dto();
        dto.timeout_ms = 0;
        dto.api_key_input = "sk-new".into();
        let errs = apply_dto(dto, &prev_with_key(), |_| {
            panic!("encrypt must not run on invalid dto")
        })
        .unwrap_err();
        assert!(!errs.is_empty());
    }

    #[test]
    fn keymap_roundtrips_between_dto_and_settings() {
        let mut s = settings::Settings::default();
        s.keymap.commit_undo = Some("Ctrl+KeyZ".into());
        s.keymap.typo_correct = Some("none".into());
        let dto = to_dto(&s);
        assert_eq!(dto.keymap.commit_undo.as_deref(), Some("Ctrl+KeyZ"));
        let back = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert_eq!(back.keymap.commit_undo.as_deref(), Some("Ctrl+KeyZ"));
        assert_eq!(back.keymap.typo_correct.as_deref(), Some("none"));
        assert_eq!(back.keymap.mode_toggle, None);
    }

    #[test]
    fn validate_accepts_space_chords_and_rejects_bare_space() {
        // 一時かな/モードトグルへの Space 系割り当て(2026-07-18 要望)が DTO 経由でも通る。
        let mut dto = base_dto();
        dto.keymap.ephemeral = Some("Shift+Space".into());
        dto.keymap.mode_toggle = Some("Ctrl+Space".into());
        assert!(validate(&dto).is_empty());
        // Space 単独は拒否(フィールド名付きで報告)。
        let mut dto = base_dto();
        dto.keymap.ephemeral = Some("Space".into());
        assert!(validate(&dto).iter().any(|e| e.field == "keymap.ephemeral"));
    }

    #[test]
    fn validate_rejects_bad_binding_and_conflict_with_field_names() {
        // 不正チョード(Alt をキーシンク経路へ)はフィールド名 keymap.<field> で報告される。
        let mut dto = base_dto();
        dto.keymap.commit_undo = Some("Alt+KeyZ".into());
        let errs = validate(&dto);
        assert!(errs.iter().any(|e| e.field == "keymap.commit_undo"));
        // 衝突(to_hiragana を既定 F7 の to_katakana に重ねる)は両フィールドに報告される。
        let mut dto = base_dto();
        dto.keymap.to_hiragana = Some("F7".into());
        let errs = validate(&dto);
        assert!(errs.iter().any(|e| e.field == "keymap.to_hiragana" && e.message.contains("カタカナ")));
        assert!(errs.iter().any(|e| e.field == "keymap.to_katakana"));
        // feature off の機能の既定キーは空き地(feedback off で Ctrl+Slash は妥当)。
        let mut dto = base_dto();
        dto.feedback_enabled = false;
        dto.keymap.typo_correct = Some("Ctrl+Slash".into());
        assert!(validate(&dto).is_empty());
        dto.feedback_enabled = true;
        assert!(!validate(&dto).is_empty());
    }

    #[test]
    fn frozen_llm_default_chord_is_free_even_when_enabled() {
        // 凍結中(settings::LLM_CONVERT_FROZEN)は llm_convert が衝突判定に参加しない=
        // 既定 Shift+Tab を他機能へ割当可能(spec 2026-07-21-llm-freeze-design.md)。
        let mut dto = base_dto();
        dto.llm_enabled = true;
        dto.keymap.typo_correct = Some("Shift+Tab".into());
        assert!(validate(&dto).is_empty());
    }
}
