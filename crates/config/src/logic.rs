//! フロント⇔settings 変換・検証・鍵3態の純ロジック。tauri 非依存（単体テスト対象）。
//!
//! 鍵の扱い（旧 nwg 版 main.rs から移植）:
//! - 表示: 平文は渡さない。設定済みなら KEY_PLACEHOLDER、未設定なら空文字。
//! - 保存: プレースホルダのまま=未変更→既存 blob 維持 / 空=明示削除 /
//!   その他=新規入力→encrypt 成功時のみ上書き（失敗時は既存 blob 維持）。

use serde::{Deserialize, Serialize};
use std::path::Path;

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
    /// Zenzai 推論上限（1〜10、既定1）。範囲外は validate が弾く（timeout_ms と同パターン）。
    pub zenzai_inference_limit: u32,
    pub live_enabled: bool,
    /// ローカルインライン予測（既定 OFF）。
    pub inline_prediction_enabled: bool,
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
    /// マスタートグル ON 時に実際に全角化する記号の部分集合（1文字ずつの文字列、既定 全29）。
    pub symbol_full_width_chars: Vec<String>,
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
    /// カスタム辞書(ユーザー辞書)を有効にするか(既定 true)。
    pub user_dictionary_enabled: bool,
    /// アップデート通知に pre-release(beta) を含めるか（既定 false=安定版のみ）。
    pub update_include_beta: bool,
    /// GitHub Releases の自動確認（既定 false=opt-in）。
    pub update_automatic_check: bool,
    /// 初回案内を閉じたか（自動確認を勝手に有効化しない）。
    pub update_automatic_check_prompt_dismissed: bool,
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
        zenzai_inference_limit: s.zenzai.inference_limit,
        live_enabled: s.live_conversion.enabled,
        inline_prediction_enabled: s.inline_prediction.enabled,
        default_direct: s.default_direct,
        learning_enabled: s.learning.enabled,
        feedback_enabled: s.feedback.enabled,
        number_full_width: s.number.full_width,
        punctuation_full_width: s.punctuation.full_width,
        symbol_full_width: s.symbol.full_width,
        symbol_full_width_chars: s
            .symbol
            .full_width_chars
            .iter()
            .map(|c| c.to_string())
            .collect(),
        reading_monitor_enabled: s.reading_monitor.enabled,
        reading_monitor_accumulate: s.reading_monitor.accumulate,
        reading_monitor_max_chars: s.reading_monitor.max_chars,
        ephemeral_enabled: s.ephemeral.enabled,
        ephemeral_trigger: s.ephemeral.trigger.clone(),
        shift_latin_mode: s.shift_latin.mode.clone(),
        user_dictionary_enabled: s.user_dictionary.enabled,
        update_include_beta: s.update.include_beta,
        update_automatic_check: s.update.automatic_check,
        update_automatic_check_prompt_dismissed: s.update.automatic_check_prompt_dismissed,
        keymap: s.keymap.clone(),
        appearance: s.appearance.clone(),
    }
}

/// `#RRGGBB`（# + 6 桁16進、3桁短縮不可）のみ許可。settings::parse_hex_color と同じ制約。
fn is_valid_hex(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// DTO の `Vec<String>` を `BTreeSet<char>` へ正規化する。1文字の要素のみ採用し、
/// 複数文字/空文字は黙って捨てる（settings::symbol::de_symbol_chars の要素検証と同じ規則）。
///
/// Why not FieldError: 上の `validate` が列挙値に FieldError を返すのは、ラジオ/テキスト
/// 入力のようにユーザーが訂正可能な入力を想定しているため。このフィールドはチェックボックス
/// UI からしか来ず、UI が複数文字/空文字を生成することは構造的に無いので、
/// defense-in-depth の黙殺で足りる（手編集 JSON からの不正入力は settings 側
/// `de_symbol_chars` が別途フィールド内で防御する）。
fn normalize_symbol_chars(items: Vec<String>) -> std::collections::BTreeSet<char> {
    items
        .into_iter()
        .filter_map(|s| {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            }
        })
        .collect()
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
    if !settings::ZENZAI_INFERENCE_LIMIT_RANGE.contains(&dto.zenzai_inference_limit) {
        errs.push(FieldError {
            field: "zenzai_inference_limit".into(),
            message: format!(
                "推論上限は {}〜{} の範囲で入力してください。",
                settings::ZENZAI_INFERENCE_LIMIT_RANGE.start(),
                settings::ZENZAI_INFERENCE_LIMIT_RANGE.end()
            ),
        });
    }
    // 巡2 D8: 明示 weight_path は絶対パス必須。相対パスは設定UIと TIP/engine の
    // カレントディレクトリが異なるため別ファイルを指し、「UI は導入済み表示・エンジンは
    // 古典へ劣化」の解離を起こす（UIバグ8と同型）。ここで機械的に弾いて UI に通知する。
    if !dto.weight_path.is_empty() && !std::path::Path::new(&dto.weight_path).is_absolute() {
        errs.push(FieldError {
            field: "weight_path".into(),
            message: "モデルのパスは絶対パスで指定してください。".into(),
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
    s.zenzai.inference_limit = dto.zenzai_inference_limit; // validate 済み（範囲内）
    s.live_conversion.enabled = dto.live_enabled;
    s.inline_prediction.enabled = dto.inline_prediction_enabled;
    s.default_direct = dto.default_direct;
    s.learning.enabled = dto.learning_enabled;
    s.feedback.enabled = dto.feedback_enabled;
    s.number.full_width = dto.number_full_width;
    s.punctuation.full_width = dto.punctuation_full_width;
    s.symbol.full_width = dto.symbol_full_width;
    s.symbol.full_width_chars = normalize_symbol_chars(dto.symbol_full_width_chars);
    s.reading_monitor.enabled = dto.reading_monitor_enabled;
    s.reading_monitor.accumulate = dto.reading_monitor_accumulate;
    // 範囲外はエラーでなくクランプ(spec 決定)。正規化点は settings::effective_max_chars。
    s.reading_monitor.max_chars = dto.reading_monitor_max_chars;
    s.reading_monitor.max_chars = s.reading_monitor.effective_max_chars();
    s.ephemeral.enabled = dto.ephemeral_enabled;
    s.ephemeral.trigger = dto.ephemeral_trigger; // validate 済み（未知値は上で Err 済み）
    s.shift_latin.mode = dto.shift_latin_mode; // validate 済み（同上）
    s.user_dictionary.enabled = dto.user_dictionary_enabled;
    s.update.include_beta = dto.update_include_beta;
    s.update.automatic_check = dto.update_automatic_check;
    s.update.automatic_check_prompt_dismissed =
        dto.update_automatic_check_prompt_dismissed || dto.update_automatic_check;
    s.keymap = dto.keymap;
    s.appearance = dto.appearance;
    Ok(s)
}

// ============================================================================
// カスタム辞書 CRUD(Issue #3 spec §5.3)。settings の dirty/適用フローとは独立で、
// 確定した瞬間に load→mutate→save→ReloadDictionary 送信まで進める。
// ============================================================================

/// 辞書ファイル操作の直列化 mutex。`State` へ直接埋めず引数注入にする — mutex 込みの
/// 不変条件(ロストアップデート無し)を単体テストから直接固定する場所を確保するため
/// (commands.rs は `State` から借りて渡すだけの薄層)。
pub struct DictLock(pub std::sync::Mutex<()>);

/// settings.json へのアクセスを直列化する mutex。用途は2つ:
/// (1) 適用(apply_settings)とモデルDL終端(download_zenzai_model の設定書き戻し)の
///     read-modify-save が並行で last-writer-wins するのを防ぐ（I/O の async 化で
///     コマンド並行性が増えたため）。
/// (2) 辞書系コマンドの reload_sender / dict_sync_engine は settings.json を読んでから
///     ReloadDictionary をパイプ送信するまで**書き込みをしないまま保持する**（ms 級の
///     逆転で読んだ直後の値が古くなり、mutation の送信が旧値を送り返すのを防ぐ —
///     commands.rs 巡4 B3。ロック順は Dict→Settings の一方向のみ）。
/// 「書き込み区間だけ」の保護ではない点に注意（保持区間はパイプ送信を含む）。
pub struct SettingsLock(pub std::sync::Mutex<()>);

/// ReloadDictionary の伝播結果(spec §4.2)。保存成功とは独立の付帯情報として返す
/// (mutation はファイル保存が成功した時点で成功であり、エンジン都合で Err にしない)。
#[derive(Serialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum EngineStatus {
    Applied,
    Declined,
    Absent,
    Timeout,
}

/// UI 一覧行。ruby/pos は生値を必ず含める(`dict_update`/`dict_delete` の対象特定は
/// 正準化前の生 ruby で行うため — 表示用の正準値だけでは非かな読みの移行産エントリを
/// 削除できなくなる)。
#[derive(Serialize)]
pub struct DictEntryDto {
    pub ruby: String,
    pub word: String,
    pub pos: Option<String>,
    pub pos_display: &'static str,
}

#[derive(Serialize)]
pub struct ListReport {
    pub entries: Vec<DictEntryDto>,
    pub deduped: usize,
    pub corrupt: String,
}

// Debug は本体機能に不要だが、テストの `.unwrap_err()`(Ok 側の型に境界が掛かる)が要求する。
#[derive(Serialize, Debug)]
pub struct MutationReport {
    pub engine: EngineStatus,
}

#[derive(Serialize)]
pub struct ImportReportDto {
    pub added: usize,
    pub skipped_dup: usize,
    pub skipped_invalid: usize,
    pub encoding_hint: bool,
    pub engine: EngineStatus,
}

#[derive(Serialize)]
pub struct ExportReportDto {
    pub written: usize,
    pub skipped_control: usize,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "kind")]
pub enum DictCmdError {
    NotFound,
    Duplicate,
    Invalid { field: String },
    Unreadable,
    QuarantineFailed,
    Io { message: String },
}

/// `validate_entry` はどちらのフィールドが失敗したかを返さない。常に妥当なダミー word
/// (ruby 側の判定は word に依存しない)で ruby 単体を再検証し、切り分ける。
fn invalid_field(ruby: &str) -> &'static str {
    if settings::user_dictionary::validate_entry(ruby, "x").is_err() {
        "ruby"
    } else {
        "word"
    }
}

/// mutation 側の load。読取 I/O エラー・隔離失敗のどちらも拒否する(空続行させると
/// 後続 save が原本を上書きして恒久消失する — settings::user_dictionary::DictLoadError の doc 参照)。
fn load_locked(path: &Path) -> Result<settings::user_dictionary::LoadedDict, DictCmdError> {
    settings::user_dictionary::load_from(path).map_err(|e| match e {
        settings::user_dictionary::DictLoadError::Unreadable => DictCmdError::Unreadable,
        settings::user_dictionary::DictLoadError::QuarantineFailed => {
            DictCmdError::QuarantineFailed
        }
    })
}

fn save_and_send(
    path: &Path,
    entries: &[settings::user_dictionary::UserDictEntry],
    send: &dyn Fn() -> EngineStatus,
) -> Result<MutationReport, DictCmdError> {
    settings::user_dictionary::save_to(path, entries).map_err(|e| DictCmdError::Io {
        message: e.to_string(),
    })?;
    Ok(MutationReport { engine: send() })
}

/// §5.1/§6.2 共通のソートキー `(normalize_key(ruby), word)` の辞書式昇順で DTO 化する。
fn to_sorted_dtos(mut entries: Vec<settings::user_dictionary::UserDictEntry>) -> Vec<DictEntryDto> {
    entries.sort_by_cached_key(|e| {
        (
            settings::user_dictionary::normalize_key(&e.ruby),
            e.word.clone(),
        )
    });
    entries
        .into_iter()
        .map(|e| {
            let pos_display = settings::user_dictionary::canonical_pos(e.pos.as_deref());
            DictEntryDto {
                ruby: e.ruby,
                word: e.word,
                pos: e.pos,
                pos_display,
            }
        })
        .collect()
}

/// 読むだけ(書き戻さない)。**それでもロックを取る** — `load_from` は破損検知時に隔離
/// rename を行うため、ロック無しだと mutation が保存した直後の健全ファイルを、破損バイト列
/// を読んで止まっていた並行 dict_list が `.corrupt` へ持ち去る競合が成立する。
pub fn dict_list_logic(lock: &DictLock, path: &Path) -> Result<ListReport, DictCmdError> {
    let _guard = lock.0.lock().unwrap();
    match settings::user_dictionary::load_from(path) {
        Ok(loaded) => Ok(ListReport {
            deduped: loaded.deduped,
            corrupt: match loaded.corrupt {
                settings::user_dictionary::DictCorrupt::None => "none",
                settings::user_dictionary::DictCorrupt::Quarantined => "quarantined",
            }
            .to_string(),
            entries: to_sorted_dtos(loaded.entries),
        }),
        Err(settings::user_dictionary::DictLoadError::Unreadable) => Err(DictCmdError::Unreadable),
        // 隔離不能=mutation 拒否状態だが、閲覧自体は落とさず「壊れている」ことだけ伝える。
        Err(settings::user_dictionary::DictLoadError::QuarantineFailed) => Ok(ListReport {
            entries: Vec::new(),
            deduped: 0,
            corrupt: "quarantine_failed".to_string(),
        }),
    }
}

pub fn dict_add_logic(
    lock: &DictLock,
    path: &Path,
    send: &dyn Fn() -> EngineStatus,
    ruby: &str,
    word: &str,
    pos: &str,
) -> Result<MutationReport, DictCmdError> {
    settings::user_dictionary::validate_entry(ruby, word).map_err(|_| DictCmdError::Invalid {
        field: invalid_field(ruby).to_string(),
    })?;
    let _guard = lock.0.lock().unwrap();
    let mut loaded = load_locked(path)?;
    let entry = settings::user_dictionary::UserDictEntry {
        ruby: ruby.to_string(),
        word: word.to_string(),
        pos: Some(pos.to_string()),
    };
    let key = settings::user_dictionary::entry_key(&entry);
    if loaded
        .entries
        .iter()
        .any(|e| settings::user_dictionary::entry_key(e) == key)
    {
        return Err(DictCmdError::Duplicate);
    }
    loaded.entries.push(entry);
    save_and_send(path, &loaded.entries, send)
}

#[allow(clippy::too_many_arguments)] // spec§5.3 の固定インターフェース(old_*/新値を並べる形)
pub fn dict_update_logic(
    lock: &DictLock,
    path: &Path,
    send: &dyn Fn() -> EngineStatus,
    old_ruby: &str,
    old_word: &str,
    ruby: &str,
    word: &str,
    pos: &str,
) -> Result<MutationReport, DictCmdError> {
    settings::user_dictionary::validate_entry(ruby, word).map_err(|_| DictCmdError::Invalid {
        field: invalid_field(ruby).to_string(),
    })?;
    let _guard = lock.0.lock().unwrap();
    let mut loaded = load_locked(path)?;
    // 対象特定はキー組一致のみ(validate を通さない — 移行産の非かな読みエントリも編集可能にする)。
    let old_key = settings::user_dictionary::entry_key(&settings::user_dictionary::UserDictEntry {
        ruby: old_ruby.to_string(),
        word: old_word.to_string(),
        pos: None,
    });
    let idx = loaded
        .entries
        .iter()
        .position(|e| settings::user_dictionary::entry_key(e) == old_key)
        .ok_or(DictCmdError::NotFound)?;
    let new_entry = settings::user_dictionary::UserDictEntry {
        ruby: ruby.to_string(),
        word: word.to_string(),
        pos: Some(pos.to_string()),
    };
    let new_key = settings::user_dictionary::entry_key(&new_entry);
    // 自己除外(§3.2): 編集対象自身とのキー一致は重複としない。
    if loaded
        .entries
        .iter()
        .enumerate()
        .any(|(i, e)| i != idx && settings::user_dictionary::entry_key(e) == new_key)
    {
        return Err(DictCmdError::Duplicate);
    }
    loaded.entries[idx] = new_entry;
    save_and_send(path, &loaded.entries, send)
}

pub fn dict_delete_logic(
    lock: &DictLock,
    path: &Path,
    send: &dyn Fn() -> EngineStatus,
    ruby: &str,
    word: &str,
) -> Result<MutationReport, DictCmdError> {
    let _guard = lock.0.lock().unwrap();
    let mut loaded = load_locked(path)?;
    // delete も対象特定は validate を通さない(delete_works_on_invalid_legacy_entry)。
    let key = settings::user_dictionary::entry_key(&settings::user_dictionary::UserDictEntry {
        ruby: ruby.to_string(),
        word: word.to_string(),
        pos: None,
    });
    let idx = loaded
        .entries
        .iter()
        .position(|e| settings::user_dictionary::entry_key(e) == key)
        .ok_or(DictCmdError::NotFound)?;
    loaded.entries.remove(idx);
    save_and_send(path, &loaded.entries, send)
}

pub fn dict_import_logic(
    lock: &DictLock,
    path: &Path,
    send: &dyn Fn() -> EngineStatus,
    bytes: &[u8],
) -> Result<ImportReportDto, DictCmdError> {
    // エンコーディング判別+パースは mutex の外(spec §5.3 — ダイアログ同様、重い処理で
    // 辞書タブ全体を無期限に無反応にしない)。
    let parsed = settings::user_dictionary::parse_tsv(bytes);
    let _guard = lock.0.lock().unwrap();
    let mut loaded = load_locked(path)?;
    let report = settings::user_dictionary::merge_imported(
        &mut loaded.entries,
        parsed.rows,
        parsed.had_replacement,
    );
    settings::user_dictionary::save_to(path, &loaded.entries).map_err(|e| DictCmdError::Io {
        message: e.to_string(),
    })?;
    Ok(ImportReportDto {
        added: report.added,
        skipped_dup: report.skipped_dup,
        skipped_invalid: report.skipped_invalid,
        encoding_hint: report.encoding_hint,
        engine: send(),
    })
}

/// tsv は UTF-8 BOM 無しで書く(呼び出し元がファイルへ書き出す)。lock は `dict_list_logic`
/// と同じ理由(`load_from` の隔離 rename)。
pub fn dict_export_logic(
    lock: &DictLock,
    path: &Path,
) -> Result<(String, ExportReportDto), DictCmdError> {
    let _guard = lock.0.lock().unwrap();
    let loaded = load_locked(path)?;
    let out = settings::user_dictionary::to_google_tsv(&loaded.entries);
    Ok((
        out.tsv,
        ExportReportDto {
            written: out.written,
            skipped_control: out.skipped_control,
        },
    ))
}

/// spec §4.2: settings 読み取りが `Loaded`/`Missing` 以外(共有違反等の I/O エラー)なら
/// ReloadDictionary を送らない。素の `settings::load()` は読み失敗を既定値(enabled=true)へ
/// 劣化させるため、これを使うと settings.json の原子 rename と読みが競合した瞬間に
/// OFF にした辞書がエンジン側だけ再有効化されてしまう。
pub fn reload_payload(outcome: settings::LoadOutcome, enabled: bool) -> Option<bool> {
    use settings::LoadOutcome::*;
    match outcome {
        Loaded | Missing => Some(enabled),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

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
    fn zenzai_inference_limit_roundtrips_and_validates() {
        let mut dto = base_dto();
        assert_eq!(dto.zenzai_inference_limit, 1, "既定は 1");
        dto.zenzai_inference_limit = 7;
        let s = apply_dto(dto.clone(), &settings::Settings::default(), |_| None).unwrap();
        assert_eq!(s.zenzai.inference_limit, 7);
        assert_eq!(to_dto(&s).zenzai_inference_limit, 7);
        // 範囲外（0=空欄の NaN→0 化含む / 11）はフィールドエラー。
        for bad in [0u32, 11] {
            dto.zenzai_inference_limit = bad;
            let errs =
                apply_dto(dto.clone(), &settings::Settings::default(), |_| None).unwrap_err();
            assert!(
                errs.iter().any(|e| e.field == "zenzai_inference_limit"),
                "{bad} は弾く: {errs:?}"
            );
        }
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
        let prev = settings::Settings {
            version: 7,
            ..Default::default()
        };
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
        assert!(
            !applied.punctuation.full_width,
            "DTO の OFF が Settings に写る"
        );
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
    fn symbol_full_width_chars_roundtrips_default_29_and_partial_subset() {
        // Settings → DTO（既定 全29 が BTreeSet 順の文字列配列で写る）
        let s = settings::Settings::default();
        let dto = to_dto(&s);
        assert_eq!(dto.symbol_full_width_chars.len(), 29);
        assert!(dto
            .symbol_full_width_chars
            .iter()
            .all(|c| c.chars().count() == 1));
        // DTO → Settings: 部分集合(Issue #1 の例 — `/` `@` を外す)
        let mut dto = to_dto(&settings::Settings::default());
        dto.symbol_full_width_chars.retain(|c| c != "/" && c != "@");
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert!(!applied.symbol.full_width_chars.contains(&'/'));
        assert!(!applied.symbol.full_width_chars.contains(&'@'));
        assert!(applied.symbol.full_width_chars.contains(&'!'));
        assert_eq!(applied.symbol.full_width_chars.len(), 27);
    }

    #[test]
    fn symbol_full_width_chars_to_dto_reflects_saved_non_default_subset() {
        // Settings → DTO を「非既定の保存値」で固定する。既定入力のテストだけだと、
        // to_dto が保存値を無視して常に既定29を返す変異が全緑で通る（起動のたび全チェックに
        // 戻り、次の適用で部分選択が黙って消える = Issue #1 の機能価値の消失）。
        // 他の bool 往復テストが「Settings 側を非既定に変えて to_dto を assert」で
        // 揃えている規約（spec §6「既存の bool 往復テストを集合込みへ拡張」）の集合版。
        let mut s = settings::Settings::default();
        s.symbol.full_width_chars.remove(&'/');
        s.symbol.full_width_chars.remove(&'@');
        let dto = to_dto(&s);
        assert_eq!(dto.symbol_full_width_chars.len(), 27);
        assert!(!dto.symbol_full_width_chars.contains(&"/".to_string()));
        assert!(!dto.symbol_full_width_chars.contains(&"@".to_string()));
        assert!(dto.symbol_full_width_chars.contains(&"!".to_string()));
    }

    #[test]
    fn symbol_full_width_chars_empty_roundtrips_as_all_deselected() {
        let mut dto = to_dto(&settings::Settings::default());
        dto.symbol_full_width_chars.clear();
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("空集合も妥当な DTO");
        assert!(applied.symbol.full_width_chars.is_empty());
    }

    #[test]
    fn symbol_full_width_chars_silently_drops_multi_char_and_empty_elements_without_field_error() {
        // チェックボックス UI は不正値を生成し得ないため、apply は FieldError でなく黙殺で正規化する。
        let mut dto = to_dto(&settings::Settings::default());
        dto.symbol_full_width_chars = vec!["!".into(), "ab".into(), "".into(), "?".into()];
        assert!(
            validate(&dto).is_empty(),
            "不正要素があっても FieldError は出さない"
        );
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert_eq!(applied.symbol.full_width_chars, BTreeSet::from(['!', '?']));
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
        assert!(
            !applied.reading_monitor.enabled,
            "DTO の OFF が Settings に写る"
        );
    }

    #[test]
    fn reading_monitor_accumulate_roundtrips_between_dto_and_settings() {
        let mut s = settings::Settings::default();
        assert!(
            to_dto(&s).reading_monitor_accumulate,
            "既定 ON が DTO に写る"
        );
        s.reading_monitor.accumulate = false;
        assert!(!to_dto(&s).reading_monitor_accumulate);
        let mut dto = to_dto(&settings::Settings::default());
        dto.reading_monitor_accumulate = false;
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert!(
            !applied.reading_monitor.accumulate,
            "DTO の OFF が Settings に写る"
        );
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
        assert_eq!(
            to_dto(&s).shift_latin_mode,
            "compose",
            "既定 compose が DTO に写る"
        );
        s.shift_latin.mode = "commit".into();
        assert_eq!(to_dto(&s).shift_latin_mode, "commit");
        // DTO → Settings（apply_dto、encrypt は成功スタブ）
        let mut dto = to_dto(&settings::Settings::default());
        dto.shift_latin_mode = "commit".into();
        let applied = apply_dto(dto, &settings::Settings::default(), |v| Some(v.to_string()))
            .expect("妥当な DTO は適用できる");
        assert_eq!(
            applied.shift_latin.mode, "commit",
            "DTO の commit が Settings に写る"
        );
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
    fn inline_prediction_defaults_off_and_roundtrips_between_dto_and_settings() {
        let defaults = settings::Settings::default();
        assert!(!to_dto(&defaults).inline_prediction_enabled);
        let mut dto = to_dto(&defaults);
        dto.inline_prediction_enabled = true;
        let applied = apply_dto(dto, &defaults, |value| Some(value.to_string())).unwrap();
        assert!(applied.inline_prediction.enabled);
    }

    #[test]
    fn ephemeral_settings_roundtrip_and_validate() {
        let mut s = settings::Settings::default();
        s.ephemeral.enabled = false;
        s.ephemeral.trigger = "f9".into();
        let dto = to_dto(&s);
        assert!(!dto.ephemeral_enabled);
        assert_eq!(dto.ephemeral_trigger, "f9");
        let back = apply_dto(dto.clone(), &settings::Settings::default(), |v| {
            Some(v.to_string())
        })
        .expect("妥当な DTO は適用できる");
        assert_eq!(back.ephemeral.trigger, "f9");
        // 未知 trigger は validate が拒否する（apply_dto も Err で拒否する）。
        let mut bad = dto.clone();
        bad.ephemeral_trigger = "ctrl_z".into();
        assert!(validate(&bad)
            .iter()
            .any(|e| e.field == "ephemeral_trigger"));
        assert!(apply_dto(bad, &settings::Settings::default(), |v| Some(v.to_string())).is_err());
    }

    #[test]
    fn dto_roundtrips_user_dictionary_enabled() {
        let mut s = settings::Settings::default();
        s.user_dictionary.enabled = false;
        let dto = to_dto(&s);
        assert!(!dto.user_dictionary_enabled);
        let s2 = apply_dto(dto, &s, |_| None).unwrap();
        assert!(!s2.user_dictionary.enabled);
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
        assert!(errs
            .iter()
            .any(|e| e.field == "keymap.to_hiragana" && e.message.contains("カタカナ")));
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

    // ---- カスタム辞書 CRUD(spec §8 config 層) ----

    fn no_send() -> EngineStatus {
        EngineStatus::Absent
    }

    /// テストごとに専用 dir(共有すると並列実行で他テストの残骸を拾って偽 RED/GREEN になる)。
    fn temp_dict_path(case: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nsk-cfg-dict-{}-{case}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("d.json")
    }

    #[test]
    fn concurrent_adds_all_survive() {
        // 2件×1回だとスレッド起動オーバーヘッド>臨界区間でロック無しでも偶然通る。
        // 各スレッド50件で衝突確率を実質1へ上げ、直列化の不変条件を決定的に固定する。
        let lock = std::sync::Arc::new(DictLock(std::sync::Mutex::new(())));
        let path = std::sync::Arc::new(temp_dict_path("conc"));
        let hs: Vec<_> = ["あ", "い"]
            .map(|prefix| {
                let (lock, path, prefix) = (lock.clone(), path.clone(), prefix.to_string());
                std::thread::spawn(move || {
                    for i in 0..50 {
                        let ruby = format!(
                            "{}{}",
                            prefix,
                            "かきくけこ".chars().cycle().take(i + 1).collect::<String>()
                        );
                        dict_add_logic(&lock, &path, &no_send, &ruby, &format!("W{i}"), "名詞")
                            .unwrap();
                    }
                })
            })
            .into_iter()
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(dict_list_logic(&lock, &path).unwrap().entries.len(), 100); // ロストアップデートなし
    }

    #[test]
    fn reload_payload_suppresses_send_on_read_failure() {
        use settings::LoadOutcome::*;
        assert_eq!(reload_payload(Loaded, false), Some(false));
        assert_eq!(reload_payload(Missing, true), Some(true));
        assert_eq!(reload_payload(PermissionDenied, true), None); // 既定trueへの劣化でOFFを潰さない
        assert_eq!(reload_payload(IoError, false), None);
    }

    #[test]
    fn list_and_export_share_sorted_order() {
        let lock = DictLock(std::sync::Mutex::new(()));
        let path = temp_dict_path("sort");
        for (r, w) in [
            ("アップル", "Apple2"),
            ("あっぷる", "Apple1"),
            ("いぬ", "犬"),
        ] {
            dict_add_logic(&lock, &path, &no_send, r, w, "名詞").unwrap();
        }
        let names: Vec<_> = dict_list_logic(&lock, &path)
            .unwrap()
            .entries
            .iter()
            .map(|e| e.word.clone())
            .collect();
        assert_eq!(names, ["Apple1", "Apple2", "犬"]); // (normalize_key, word) 辞書式(かな種混在OK)
                                                       // export も同一順(テスト名どおり両方を固定 — list だけだと to_google_tsv 未ソートで緑)
        let (tsv, _) = dict_export_logic(&lock, &path).unwrap();
        let words: Vec<_> = tsv
            .lines()
            .map(|l| l.split('\t').nth(1).unwrap().to_string())
            .collect();
        assert_eq!(words, ["Apple1", "Apple2", "犬"]);
    }

    #[test]
    fn update_pos_only_succeeds_and_notfound_rejects() {
        let lock = DictLock(std::sync::Mutex::new(()));
        let path = temp_dict_path("upd");
        dict_add_logic(&lock, &path, &no_send, "やちだ", "谷内田", "名詞").unwrap();
        dict_update_logic(
            &lock,
            &path,
            &no_send,
            "やちだ",
            "谷内田",
            "やちだ",
            "谷内田",
            "姓",
        )
        .unwrap(); // 自己除外
        assert_eq!(
            dict_update_logic(&lock, &path, &no_send, "ない", "無い", "あ", "a", "名詞")
                .unwrap_err(),
            DictCmdError::NotFound
        ); // is_err() だけだと別エラーでも緑=UI分岐が偽緑
        assert_eq!(
            dict_delete_logic(&lock, &path, &no_send, "ない", "無い").unwrap_err(),
            DictCmdError::NotFound
        ); // phantom insert 禁止
    }

    #[test]
    fn delete_after_dup_file_leaves_no_remnant() {
        // 重複入りファイルへ delete → 保存後に同一キー残骸 0 件(dedup済みリスト経由の固定 — spec§8)
        let path = temp_dict_path("dupdel");
        std::fs::write(
            &path,
            r#"[{"ruby":"あっぷる","word":"Apple"},{"ruby":"アップル","word":"Apple"}]"#,
        )
        .unwrap();
        let lock = DictLock(std::sync::Mutex::new(()));
        dict_delete_logic(&lock, &path, &no_send, "あっぷる", "Apple").unwrap();
        assert_eq!(dict_list_logic(&lock, &path).unwrap().entries.len(), 0);
    }

    #[test]
    fn nfd_add_is_rejected_as_dup_via_key_pair() {
        let lock = DictLock(std::sync::Mutex::new(()));
        let path = temp_dict_path("nfd");
        dict_add_logic(&lock, &path, &no_send, "か\u{3099}っこう", "学校", "名詞").unwrap();
        assert_eq!(
            dict_add_logic(&lock, &path, &no_send, "がっこう", "学校", "名詞").unwrap_err(),
            DictCmdError::Duplicate
        ); // キー組経由(is_err だと別エラーでも緑)
           // word 側: ワ行濁点
        dict_add_logic(&lock, &path, &no_send, "いすず", "いすヷ", "名詞").unwrap();
        assert_eq!(
            dict_add_logic(&lock, &path, &no_send, "いすず", "いすワ\u{3099}", "名詞").unwrap_err(),
            DictCmdError::Duplicate
        );
    }

    #[test]
    fn delete_works_on_invalid_legacy_entry() {
        // spec§3.2: 移行産の検証不合格エントリは削除可能(対象特定に validate を通さない)
        let path = temp_dict_path("legacy");
        std::fs::write(&path, r#"[{"ruby":"kanji漢字","word":"x"}]"#).unwrap();
        let lock = DictLock(std::sync::Mutex::new(()));
        dict_delete_logic(&lock, &path, &no_send, "kanji漢字", "x").unwrap();
        assert_eq!(dict_list_logic(&lock, &path).unwrap().entries.len(), 0);
    }

    #[test]
    fn corrupt_file_add_does_not_destroy_original() {
        // 破損JSONを置く→dict_add→.corrupt.* が存在し原本は上書きされていない(隔離後の空+1件保存)
        let path = temp_dict_path("addcorrupt");
        std::fs::write(&path, br#"[{"ruby":"x""#).unwrap();
        let lock = DictLock(std::sync::Mutex::new(()));
        dict_add_logic(&lock, &path, &no_send, "あ", "亜", "名詞").unwrap();
        assert!(path.parent().unwrap().read_dir().unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".corrupt.")));
        let entries = dict_list_logic(&lock, &path).unwrap().entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].word, "亜");
    }

    #[test]
    fn dict_list_does_not_write_file() {
        // 重複入りファイル→dict_list→ファイル内容が不変(閲覧はファイルを書かない — spec§3.2)
        let path = temp_dict_path("listnowrite");
        let json = r#"[{"ruby":"あっぷる","word":"Apple"},{"ruby":"アップル","word":"Apple"}]"#;
        std::fs::write(&path, json).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let lock = DictLock(std::sync::Mutex::new(()));
        let report = dict_list_logic(&lock, &path).unwrap();
        assert_eq!(report.entries.len(), 1); // 返り値は dedup 済みだが
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before); // ファイルは書いていない
    }

    #[test]
    fn dict_list_blocks_while_lock_held() {
        // 第3巡 N1(list/export もロックを取る)の決定的な固定。確率的な並行破損シナリオは
        // 窓が µs 規模でロック無しでも緑になり得るため、「保持中は返らない」を直接見る。
        let lock = std::sync::Arc::new(DictLock(std::sync::Mutex::new(())));
        let path = std::sync::Arc::new(temp_dict_path("listlock"));
        let guard = lock.0.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (l2, p2) = (lock.clone(), path.clone());
        std::thread::spawn(move || {
            let _ = dict_list_logic(&l2, &p2);
            tx.send(()).unwrap();
        });
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err()); // 保持中は返らない
        drop(guard);
        assert!(rx.recv_timeout(std::time::Duration::from_secs(2)).is_ok()); // 解放後に返る
    }

    #[test]
    fn unreadable_mutation_refuses_and_preserves_file() {
        // dir を辞書パスに指定(=読取 I/O エラー)→ dict_add_logic が Err(Unreadable) を返し、
        // 隔離も保存も起きない(一過性 I/O 失敗で「空+1件」上書きしない — F-1 の mutation 側)
        let dir =
            std::env::temp_dir().join(format!("nsk-cfg-dict-{}-unreadable", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = DictLock(std::sync::Mutex::new(()));
        assert_eq!(
            dict_add_logic(&lock, &dir, &no_send, "あ", "亜", "名詞").unwrap_err(),
            DictCmdError::Unreadable
        );
        assert!(dir.exists()); // ディレクトリのまま(何も書かれていない)
    }

    #[test]
    fn invalid_input_is_rejected_with_field() {
        // dict_add_logic("kanji漢字", "x", "名詞") → Err(Invalid{field:"ruby"})
        // (validate 経路の観測 — バリアントが一度も使われない実装の検出)
        let lock = DictLock(std::sync::Mutex::new(()));
        let path = temp_dict_path("invalid");
        assert_eq!(
            dict_add_logic(&lock, &path, &no_send, "kanji漢字", "x", "名詞").unwrap_err(),
            DictCmdError::Invalid {
                field: "ruby".to_string()
            }
        );
    }
}
