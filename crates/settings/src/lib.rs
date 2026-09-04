//! SP6b: nospacekey の永続設定。TIP と NospacekeyConfig.exe が共有する。COM/GUI 非依存。
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub mod dpapi;
pub mod keymap;
pub mod symbol;
pub mod user_dictionary;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmSettings {
    pub enabled: bool,
    #[serde(default)]
    pub api_key_dpapi: String, // DPAPI blob の base64。空=未設定。
    #[serde(default)]
    pub endpoint: String,
    pub model: String,
    #[serde(default)]
    pub prompt: String,
    pub timeout_ms: u32,
}
impl Default for LlmSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key_dpapi: String::new(),
            endpoint: String::new(),
            model: "gpt-4o-mini".into(),
            prompt: String::new(),
            timeout_ms: 15000,
        }
    }
}

/// LLM変換(外部API)の開発凍結フラグ(2026-07-21)。当面実装予定がないため UI/機能を閉じる。
/// 再開時はこれを false へ(ゲート4箇所は実効判定経由で自動復帰。UI とテストの復元は
/// docs/superpowers/specs/2026-07-21-llm-freeze-design.md の「再開手順」)。
pub const LLM_CONVERT_FROZEN: bool = true;

/// 凍結を考慮した実効有効判定(bool 版)。Settings を持たない層(config の DTO 検証)は
/// こちらを契約の入口として使ってよい。
pub fn llm_effective(enabled: bool) -> bool {
    enabled && !LLM_CONVERT_FROZEN
}

/// 凍結を考慮した実効有効判定。llm 機能の有効/無効を見る側は `s.llm.enabled` を直読みせず
/// 必ずこれ(Settings を持たない層は `llm_effective`)を通す。生値 `s.llm.enabled` を読んで
/// よいのは永続化と DTO ラウンドトリップ(config の to_dto/apply_dto)だけ=保存値温存のため。
pub fn llm_effective_enabled(s: &Settings) -> bool {
    llm_effective(s.llm.enabled)
}

/// Zenzai 推論上限の妥当範囲。クランプ(effective_inference_limit)と config の validate が
/// 共有する唯一の境界定義（範囲の二重定義を作らない）。
pub const ZENZAI_INFERENCE_LIMIT_RANGE: std::ops::RangeInclusive<u32> = 1..=10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZenzaiSettings {
    pub enabled: bool,
    #[serde(default)]
    pub weight_path: String,
    /// Zenzai の候補再ランキング推論回数上限。engine env NOSPACEKEY_ZENZAI_INFERENCE_LIMIT と対。
    #[serde(default = "default_zenzai_inference_limit")]
    pub inference_limit: u32,
}
fn default_zenzai_inference_limit() -> u32 {
    1
}
impl Default for ZenzaiSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            weight_path: String::new(),
            inference_limit: 1,
        }
    }
}
impl ZenzaiSettings {
    /// 手編集 settings.json の異常値（0/10000 等）を吸収する唯一の正規化点
    /// （reading_monitor の effective_max_chars と同じ方針）。
    pub fn effective_inference_limit(&self) -> u32 {
        self.inference_limit.clamp(
            *ZENZAI_INFERENCE_LIMIT_RANGE.start(),
            *ZENZAI_INFERENCE_LIMIT_RANGE.end(),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveSettings {
    pub enabled: bool,
}
impl Default for LiveSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// 完全ローカルのインライン予測。モデル取得前に勝手に有効化しない opt-in 機能。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InlinePredictionSettings {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningSettings {
    pub enabled: bool,
}
impl Default for LearningSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// 品質ループ③: 誤変換ワンキー記録（Ctrl+変換 → feedback.jsonl）。**既定 OFF＝opt-in**
/// （NOSPACEKEY_LOG の診断ログとは独立の opt-in — 既定状態で新規に書かれるものはゼロ）。
/// `enabled: false` が既定なので Default は derive（clippy::derivable_impls）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeedbackSettings {
    pub enabled: bool,
}

/// かな入力モードで数字を既定で全角確定するか。既定 true（全角）。いつでも設定で切替可能。
/// 候補を明示選択した確定は幅を変えない（既定確定のみ全角化）。LiveSettings と同じく既定が
/// true なので Default は手書き（derive だと false になる）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NumberSettings {
    pub full_width: bool,
}
impl Default for NumberSettings {
    fn default() -> Self {
        Self { full_width: true }
    }
}

/// かな入力モードの句読点既定幅（true=全角 、。／false=半角 ,.）。既定 true なので
/// Default は手書き（derive だと false になる）。NumberSettings と同じ流儀（設計 §E）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PunctuationSettings {
    pub full_width: bool,
}
impl Default for PunctuationSettings {
    fn default() -> Self {
        Self { full_width: true }
    }
}

/// かな入力モードの記号既定幅（true=全角 ・「」！？～：；等／false=半角 ASCII）。既定 false。
/// `,` `.`（punctuation の領分）と `-`→ー（長音符=かな）はこのトグルの対象外。
/// `full_width_chars`: マスタートグル ON 時に実際に全角化する記号の部分集合（Issue #1 —
/// 2026-08-02 spec）。既定は `symbol::symbol_targets()` の全29記号。
/// **Default は手書き**（derive を外した）: derive のままだと `full_width_chars` が
/// 空集合になり、`symbol` オブジェクトごと欠落した JSON・新規インストール・破損
/// フォールバック・「既定に戻す」の全てで機能が黙って死ぬ（spec §3）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolSettings {
    pub full_width: bool,
    #[serde(
        default = "symbol::default_full_width_chars",
        deserialize_with = "symbol::de_symbol_chars"
    )]
    pub full_width_chars: BTreeSet<char>,
}
impl Default for SymbolSettings {
    fn default() -> Self {
        Self {
            full_width: false,
            full_width_chars: symbol::default_full_width_chars(),
        }
    }
}
impl SymbolSettings {
    /// `full_width_chars` と `symbol_targets()` の積（対象外文字はここで落ちる）。
    /// `Cell<SymbolCharSet>` としてキャッシュへ載せるための実効集合（spec §2）。
    pub fn effective_chars(&self) -> symbol::SymbolCharSet {
        symbol::SymbolCharSet::from(&self.full_width_chars)
            & symbol::SymbolCharSet::from(&symbol::default_full_width_chars())
    }
    /// overlay 実効値（= `full_width` かつ実効集合が非空）。素の `full_width_chars.is_empty()`
    /// を使わないのは、`["-"]` のような「非空だが実効ゼロ」の集合で gate だけ食い続ける
    /// 不整合を防ぐため（spec §2）。TIP の gate/OnKeyDown 系はこの値をキャッシュへ格納するだけにし、
    /// 判定ロジックを Activate に書かない。
    pub fn symbol_overlay(&self) -> bool {
        self.full_width && !self.effective_chars().is_empty()
    }
}

/// 一時的なかなモード（トリガキーで一時的にかな入力へ入り、確定で自動的に半角英数へ戻る）。
/// ターミナル/vim 向けに「日本語モードの抜け忘れ」を防ぐ。既定 ON・トリガは F8。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EphemeralSettings {
    pub enabled: bool,
    pub trigger: String,
}
impl Default for EphemeralSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: "f8".into(),
        }
    }
}

/// 修正変換(Tab): 読みのタイポ修復候補を提示する。`learn` は修復候補確定時の
/// 誤読み学習(合成ペア — engine env NOSPACEKEY_TYPO_LEARN)。両方とも既定 ON。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypoCorrectSettings {
    pub enabled: bool,
    pub learn: bool,
}
impl Default for TypoCorrectSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            learn: true,
        }
    }
}

/// Shift+英字の挙動。"compose"=英語未確定モード(確定まで英字が続く・MS-IME系・既定) /
/// "commit"=大文字を直接確定(Google/ATOK系・e0beaf3 の旧既定)。bool でなく文字列 enum
/// なのは appearance.backdrop/ephemeral.trigger と同じ将来拡張余地のため。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShiftLatinSettings {
    #[serde(default = "default_shift_latin_mode")]
    pub mode: String,
}
fn default_shift_latin_mode() -> String {
    "compose".into()
}
impl Default for ShiftLatinSettings {
    fn default() -> Self {
        Self {
            mode: default_shift_latin_mode(),
        }
    }
}

/// 読みモニタ: ライブ変換中に生の読み(ひらがな)をキャレット上側の小窓で常時表示する。
/// 既定 ON なので Default は手書き（derive だと false — LiveSettings と同じ流儀）。
/// accumulate: 自動確定(live_auto)をまたいで読みを累積表示する（Enter まで保持）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadingMonitorSettings {
    pub enabled: bool,
    /// 旧 settings.json（フィールド欠落）は ON でロード — serde default は struct Default
    /// でなくフィールド単位で効かせる必要がある。
    #[serde(default = "default_true")]
    pub accumulate: bool,
    /// 窓の表示上限（全角文字数換算）。範囲外は effective_max_chars がクランプ。
    #[serde(default = "default_max_chars")]
    pub max_chars: u32,
}
impl Default for ReadingMonitorSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            accumulate: true,
            max_chars: 34,
        }
    }
}
impl ReadingMonitorSettings {
    /// 10..=100 へクランプ。config の apply と tip の Activate 読みの両方が通る
    /// 唯一の正規化点（境界定数をここ以外に書かない）。
    pub fn effective_max_chars(&self) -> u32 {
        self.max_chars.clamp(10, 100)
    }
}

/// カスタム辞書(ユーザー辞書)の有効/無効。既定 ON。LearningSettings と同じく既定が true
/// なので Default は手書き(derive だと false — 既存ユーザの辞書が黙って無効化される)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserDictionarySettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
}
impl Default for UserDictionarySettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_true() -> bool {
    true
}
fn default_max_chars() -> u32 {
    34
}

/// 設定アプリのアップデート確認設定。TIP は読まない（設定アプリのみが使うローカル設定）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateSettings {
    /// GitHub Releases の自動確認。既定 false（opt-in）。
    #[serde(default)]
    pub automatic_check: bool,
    /// pre-release(beta) をアップデート通知に含めるか。既定 false=安定版のみ。
    #[serde(default)]
    pub include_beta: bool,
    /// 初回案内を閉じたか。自動確認を勝手に有効化しないため設定として保存する。
    #[serde(default)]
    pub automatic_check_prompt_dismissed: bool,
}

const SETTINGS_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub version: u32,
    #[serde(default)]
    pub llm: LlmSettings,
    #[serde(default)]
    pub zenzai: ZenzaiSettings,
    #[serde(default)]
    pub live_conversion: LiveSettings,
    /// 明示確定後のローカル続き予測。欠落する旧 settings.json は OFF。
    #[serde(default)]
    pub inline_prediction: InlinePredictionSettings,
    /// Spec2: かな漢字変換の学習（確定候補を以後の順位に反映）。既定 ON。
    /// engine env `NOSPACEKEY_LEARNING`（"1"/"0"）へ resolve_env_map が常に注入する。
    #[serde(default)]
    pub learning: LearningSettings,
    /// SP7: 真なら新しいアプリ（TIP インスタンス初回 Activate）で conversion-mode を
    /// 半角英数へ初期化する。既定 false＝従来どおりひらがな既定。ワンショットなので
    /// 無変換後のひらがなは維持する。TIP 側の挙動で engine env には注入しない。
    /// フィールド欠落の旧 settings.json は false でロード。
    #[serde(default)]
    pub default_direct: bool,
    /// A 段: 外観（配色/フォント/角丸/バックドロップ）。欠落は既定 Appearance。
    #[serde(default)]
    pub appearance: Appearance,
    /// 品質ループ③: 誤変換ワンキー記録（feedback.jsonl）。既定 false=opt-in。
    /// フィールド欠落の旧 settings.json は false でロード（後方互換）。
    #[serde(default)]
    pub feedback: FeedbackSettings,
    /// かな入力モードの数字既定幅（true=全角）。欠落の旧 settings.json は true でロード。
    #[serde(default)]
    pub number: NumberSettings,
    /// かな入力モードの句読点既定幅（true=全角）。欠落の旧 settings.json は true でロード。
    #[serde(default)]
    pub punctuation: PunctuationSettings,
    /// かな入力モードの記号既定幅（false=半角 ASCII）。欠落の旧 settings.json は false でロード。
    #[serde(default)]
    pub symbol: SymbolSettings,
    /// 一時的なかなモード（トリガキーで一時的にかな入力へ、確定で自動的に半角英数へ戻る）。
    /// 欠落の旧 settings.json は既定（enabled=true, trigger="f8"）でロード。
    #[serde(default)]
    pub ephemeral: EphemeralSettings,
    /// 修正変換(Tab): 読みのタイポ修復候補。欠落の旧 settings.json は既定
    /// （enabled=true, learn=true）でロード。
    #[serde(default)]
    pub typo_correct: TypoCorrectSettings,
    /// Shift+英字の挙動（"compose"=英語未確定モード / "commit"=大文字直接確定）。
    /// 欠落の旧 settings.json は "compose" でロード。TIP ローカル設定（engine env 非注入）。
    #[serde(default)]
    pub shift_latin: ShiftLatinSettings,
    /// 読みモニタ（ライブ変換中の生読み常時表示）。欠落の旧 settings.json は ON でロード。
    #[serde(default)]
    pub reading_monitor: ReadingMonitorSettings,
    /// configurable keymap: コマンド系 12 機能のキー割り当て(spec 2026-07-16)。
    /// 欠落の旧 settings.json は全機能「既定」でロード。反映は Activate 時 1 回(D7)。
    #[serde(default)]
    pub keymap: keymap::KeymapSettings,
    /// カスタム辞書(ユーザー辞書)。既定 ON。欠落の旧 settings.json は ON でロード(後方互換)。
    #[serde(default)]
    pub user_dictionary: UserDictionarySettings,
    /// アップデート確認（beta を含めるか）。欠落の旧 settings.json は既定（安定版のみ）でロード。
    #[serde(default)]
    pub update: UpdateSettings,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_SCHEMA_VERSION,
            llm: Default::default(),
            zenzai: Default::default(),
            live_conversion: Default::default(),
            inline_prediction: Default::default(),
            learning: Default::default(),
            default_direct: false,
            appearance: Default::default(),
            feedback: Default::default(),
            number: Default::default(),
            punctuation: Default::default(),
            symbol: Default::default(),
            ephemeral: EphemeralSettings::default(),
            typo_correct: Default::default(),
            shift_latin: Default::default(),
            reading_monitor: Default::default(),
            keymap: Default::default(),
            user_dictionary: Default::default(),
            update: Default::default(),
        }
    }
}

/// A 段の外観設定。全フィールド `#[serde(default)]` で後方互換（欠落フィールドは既定へ）。
/// 色は人間が編集できる `#RRGGBB` 文字列で保存する（カスタムテーマ MVP）。
/// パース失敗は Theme 解決層でフィールド単位に既定へフォールバックする（ここでは文字列のまま保持）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Palette {
    #[serde(default)]
    pub bg: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub index: String,
    #[serde(default)]
    pub sel_bg: String,
    #[serde(default)]
    pub sel_text: String,
    #[serde(default)]
    pub sel_index: String,
    #[serde(default)]
    pub border: String,
}

/// 内蔵ライトパレット。Apple 風 UI パターン集（docs/apple-design-ui-patterns.md）の
/// ライトトークン由来: panel #FFFFFF / text #1D1D1F / text-sub #86868B /
/// accent(systemBlue) #0071E3。border は rgba(0,0,0,.12) を白地に合成した #E0E0E0、
/// sel_index はアクセント地に白 70% を合成した淡青（GDI が不透明色しか扱えないため事前合成）。
pub fn default_light_palette() -> Palette {
    Palette {
        bg: "#FFFFFF".into(),
        text: "#1D1D1F".into(),
        index: "#86868B".into(),
        sel_bg: "#0071E3".into(),
        sel_text: "#FFFFFF".into(),
        sel_index: "#B3D4F7".into(),
        border: "#E0E0E0".into(),
    }
}

/// 内蔵ダークパレット。同トークンのダーク側: panel #2C2C2E / text #F5F5F7 /
/// text-sub #98989D / accent(systemBlue) #0A84FF。border は rgba(255,255,255,.16) を
/// #2C2C2E 地に合成した #4E4E4F、sel_index はアクセント地に白 70% を合成した淡青。
pub fn default_dark_palette() -> Palette {
    Palette {
        bg: "#2C2C2E".into(),
        text: "#F5F5F7".into(),
        index: "#98989D".into(),
        sel_bg: "#0A84FF".into(),
        sel_text: "#FFFFFF".into(),
        sel_index: "#B6DAFF".into(),
        border: "#4E4E4F".into(),
    }
}

impl Default for Palette {
    fn default() -> Self {
        default_light_palette()
    }
}

/// v1 時代の内蔵ライトパレット（既定刷新前の焼き付き値の検出専用。新規利用禁止）。
fn legacy_v1_light_palette() -> Palette {
    Palette {
        bg: "#FAFAFA".into(),
        text: "#202020".into(),
        index: "#A0A0A0".into(),
        sel_bg: "#0078D7".into(),
        sel_text: "#FFFFFF".into(),
        sel_index: "#C8DCF0".into(),
        border: "#E0E0E0".into(),
    }
}

/// v1 時代の内蔵ダークパレット（同上）。
fn legacy_v1_dark_palette() -> Palette {
    Palette {
        bg: "#2B2B2B".into(),
        text: "#F0F0F0".into(),
        index: "#7A7A7A".into(),
        sel_bg: "#0078D7".into(),
        sel_text: "#FFFFFF".into(),
        sel_index: "#1E3A5F".into(),
        border: "#3C3C3C".into(),
    }
}

/// v1→v2 スキーマ移行。パース直後に必ず通す（load_reporting / from_json_str の両経路）。
///
/// `#[serde(default)]` に任せられない理由: save() は Settings 全体をフルシリアライズする
/// ため、設定アプリで一度でも保存した settings.json には旧内蔵既定色が「具体値」で
/// 焼き付いており、フィールド欠落時にしか効かない serde default では新既定へ上がらない。
/// 旧内蔵既定と 7 色完全一致のパレットだけを「カスタマイズしていない」とみなして
/// 引き上げる（1 色でも違えば意図的カスタム＝丸ごと温存）。
fn migrate(mut s: Settings) -> Settings {
    if s.version < SETTINGS_SCHEMA_VERSION {
        if s.appearance.palette_light == legacy_v1_light_palette() {
            s.appearance.palette_light = default_light_palette();
        }
        if s.appearance.palette_dark == legacy_v1_dark_palette() {
            s.appearance.palette_dark = default_dark_palette();
        }
        s.version = SETTINGS_SCHEMA_VERSION;
    }
    s
}

/// 巡3 Z3: 手書き settings.json 由来の相対 weight_path を正規化する choke point。
/// 設定UIの validate 絶対パス必須（適用経由のみ）を迂回した値は、Config/TIP/engine の
/// CWD 差で「UI は導入済み表示・エンジンは古典劣化」の解離を起こす（UIバグ8 と同型）。
/// 非空かつ非絶対のパスは空へ畳み、ZenzaiConfig.resolve の 3 段表（per-user→exe 隣）へ
/// フォールバックさせる — 次回 save() でファイルも自己修復される。全消費者
/// （UI/TIP/DL の read-modify-save）が load を通るため、この 1 点で全迂回を閉じられる。
fn normalize_loaded(mut s: Settings) -> Settings {
    if !s.zenzai.weight_path.is_empty()
        && !std::path::Path::new(&s.zenzai.weight_path).is_absolute()
    {
        s.zenzai.weight_path.clear();
    }
    s
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Appearance {
    /// "auto" | "light" | "dark" | "custom"。
    #[serde(default = "appearance_theme_default")]
    pub theme: String,
    /// "acrylic" | "opaque"。
    #[serde(default = "appearance_backdrop_default")]
    pub backdrop: String,
    #[serde(default = "appearance_font_family_default")]
    pub font_family: String,
    #[serde(default = "appearance_font_point_default")]
    pub font_point: f32,
    /// "round" | "square"。
    #[serde(default = "appearance_corner_default")]
    pub corner: String,
    #[serde(default = "default_light_palette")]
    pub palette_light: Palette,
    #[serde(default = "default_dark_palette")]
    pub palette_dark: Palette,
}

fn appearance_theme_default() -> String {
    "auto".into()
}
fn appearance_backdrop_default() -> String {
    "acrylic".into()
}
fn appearance_font_family_default() -> String {
    "Yu Gothic UI".into()
}
fn appearance_font_point_default() -> f32 {
    10.5
}
fn appearance_corner_default() -> String {
    "round".into()
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: appearance_theme_default(),
            backdrop: appearance_backdrop_default(),
            font_family: appearance_font_family_default(),
            font_point: appearance_font_point_default(),
            corner: appearance_corner_default(),
            palette_light: default_light_palette(),
            palette_dark: default_dark_palette(),
        }
    }
}

/// `#RRGGBB`（先頭 `#`＋6 桁 16 進、大小問わず）を (R,G,B) へ。それ以外は None。
/// パース失敗は呼び出し側（Theme 解決）でフィールド単位に既定へフォールバックする前提で、
/// ここでは決して panic せず None を返すだけにする。
pub fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

impl Settings {
    pub fn from_json_str(s: &str) -> Settings {
        // 巡4 J6: migrate と対の第2パース経路でも normalize_loaded を通す（lib の migrate
        // doc が「load_reporting / from_json_str の両経路」と契約する正規化 choke point）。
        serde_json::from_str(s)
            .map(|s| normalize_loaded(migrate(s)))
            .unwrap_or_default()
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

/// %LOCALAPPDATA%\nospacekey\settings.json。無しおよび空文字なら None（呼び元は既定で劣化）。
/// 空文字を通すとカレントディレクトリ基準の相対パスになり、プロセス毎に別ファイルを
/// 指しうる（巡2 D7 — download.rs の DL 先と同じ規律）。
pub fn settings_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join("nospacekey").join("settings.json"))
}

/// Corruption recovery uses a durable append-only ledger.  A successful
/// quarantine creates one empty `pending` file and Config acknowledges it by
/// creating the matching empty `ack` file.  Neither side removes, renames, or
/// overwrites ledger entries, so a crash before acknowledgement is retried on
/// the next launch.  The zero-byte files intentionally remain as a
/// non-destructive ledger; their names contain no user data and the scan only
/// considers valid tokens. The parent is the per-user settings directory, so
/// scanning it fully avoids starving a fresh entry behind old ledger files.
const CORRUPT_RECOVERY_PENDING_PREFIX: &str = "settings.json.corrupt-recovered.";
const CORRUPT_RECOVERY_PENDING_SUFFIX: &str = ".pending";
const CORRUPT_RECOVERY_ACK_SUFFIX: &str = ".ack";
const CORRUPT_RECOVERY_TOKEN_MAX_LEN: usize = 64;

fn corrupt_recovery_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TOKEN: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let serial = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}{:09}{}{}",
        now.as_secs(),
        now.subsec_nanos(),
        std::process::id(),
        serial
    )
}

fn marker_name(prefix: &str, token: &str, suffix: &str) -> String {
    format!("{prefix}{token}{suffix}")
}

fn valid_corrupt_recovery_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= CORRUPT_RECOVERY_TOKEN_MAX_LEN
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn pending_token_from_name(name: &str) -> Option<&str> {
    let token = name
        .strip_prefix(CORRUPT_RECOVERY_PENDING_PREFIX)?
        .strip_suffix(CORRUPT_RECOVERY_PENDING_SUFFIX)?;
    valid_corrupt_recovery_token(token).then_some(token)
}

fn ack_path_for_pending(pending: &Path, token: &str) -> PathBuf {
    pending.with_file_name(marker_name(
        CORRUPT_RECOVERY_PENDING_PREFIX,
        token,
        CORRUPT_RECOVERY_ACK_SUFFIX,
    ))
}

fn marker_entry_is_regular(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

fn create_new_ledger_file(path: &Path) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        // If an attacker pre-creates a reparse point at this exact ledger
        // name, create_new must not follow it or write through to its target.
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    options.open(path).map(|_| ())
}

fn create_corrupt_recovery_pending(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    // A timestamp + pid + process-local serial is bounded, ASCII, and avoids
    // path scanning.  A create_new collision is retried with a fresh serial;
    // every successful quarantine gets its own durable ledger entry.
    for _ in 0..4 {
        let name = marker_name(
            CORRUPT_RECOVERY_PENDING_PREFIX,
            &corrupt_recovery_token(),
            CORRUPT_RECOVERY_PENDING_SUFFIX,
        );
        match create_new_ledger_file(&parent.join(name)) {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return,
        }
    }
}

/// Read-only check for an unacknowledged, valid pending ledger entry. Malformed
/// names and non-regular entries are ignored; valid entries are scanned even
/// when the append-only ledger has accumulated older acknowledgements.
pub fn has_pending_corrupt_recovery_notice() -> bool {
    let Some(path) = settings_path() else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return false;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(token) = pending_token_from_name(&name) else {
            continue;
        };
        if !marker_entry_is_regular(&entry_path) {
            continue;
        }
        let ack = ack_path_for_pending(&entry_path, token);
        if !marker_entry_is_regular(&ack) {
            return true;
        }
    }
    false
}

/// Acknowledge every valid pending ledger entry by creating, never replacing,
/// its matching `.ack`.  Existing files and races are intentionally benign.
pub fn acknowledge_corrupt_recovery_notices() {
    let Some(path) = settings_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(token) = pending_token_from_name(&name) else {
            continue;
        };
        if !marker_entry_is_regular(&entry_path) {
            continue;
        }
        let ack = ack_path_for_pending(&entry_path, token);
        if marker_entry_is_regular(&ack) {
            continue;
        }
        let _ = create_new_ledger_file(&ack);
    }
}

/// UU-7: settings.json をロードした結果の要因。`load()` は Settings しか返さず失敗を握り潰すため、
/// 「検索窓でだけ設定が効かない」＝AppContainer/LPAC ホストからの権限拒否を診断できなかった。
/// `load_reporting()` がこの要因を返し、TIP の Activate が tip_log に残せるようにする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOutcome {
    /// 正常に読めてパースできた。
    Loaded,
    /// ファイルが無い（初回起動）。既定で劣化。
    Missing,
    /// 読み取りが権限で拒否された（AppContainer/LPAC ホストから読めない疑い — UU-7）。既定で劣化。
    PermissionDenied,
    /// その他の I/O エラー。既定で劣化。
    IoError,
    /// LOCALAPPDATA 未設定でパスが解決できない。既定で劣化。
    NoPath,
    /// 空/空白のみ（torn write 痕跡）。既定で劣化。
    Empty,
    /// JSON 破損（read-only loader が検出、mutation loader では退避成功済み）。既定で劣化。
    Corrupt,
    /// JSON 破損を検出したが rename/copy のどちらでも原本を退避できなかった。
    /// 原本を既定値で上書きしないため、mutation は拒否する。
    CorruptQuarantineFailed,
    /// この実行ファイルより新しいスキーマ。既知フィールドは読み取れるが、未知フィールドを
    /// 失う全体保存を防ぐため mutation は拒否する。
    UnsupportedVersion,
}

/// UU-7: `std::fs` の read エラー種別を `LoadOutcome` へ分類する純関数（テスト可能）。
/// PermissionDenied を独立させ、AppContainer からの読み取り拒否を診断可能にする。
pub fn classify_read_error(kind: std::io::ErrorKind) -> LoadOutcome {
    match kind {
        std::io::ErrorKind::NotFound => LoadOutcome::Missing,
        std::io::ErrorKind::PermissionDenied => LoadOutcome::PermissionDenied,
        _ => LoadOutcome::IoError,
    }
}

pub fn load() -> Settings {
    load_reporting().0
}

/// 読み取り結果とその要因を返す。既存の reporting 呼び出しは破損原本の退避まで行い、
/// `Corrupt`/`CorruptQuarantineFailed` で成否を区別する。
pub fn load_reporting() -> (Settings, LoadOutcome) {
    let Some(path) = settings_path() else {
        return (Settings::default(), LoadOutcome::NoPath);
    };
    let (settings, outcome) = load_for_mutation_from_with(
        &path,
        |path| std::fs::read_to_string(path),
        |from, to| std::fs::rename(from, to).map(|_| ()),
        |from, to| std::fs::copy(from, to),
    );
    if outcome == LoadOutcome::Corrupt {
        // Corrupt means quarantine succeeded in the mutation loader.  Ledger
        // creation is only a best-effort notification handoff and must never
        // affect the already-completed quarantine result.
        create_corrupt_recovery_pending(&path);
    }
    (settings, outcome)
}

/// Read `settings.json` and report its outcome without mutating any file.
///
/// This read-only API resolves [`settings_path`], reads once, and parses only.
/// It never creates, renames, copies, removes, or otherwise writes the settings
/// file, including when syntax or typed corruption produces `Corrupt`.
/// Consumers that must preserve the settings path (for example, the background
/// update checker) must use this API instead of [`load_reporting`].
pub fn load_reporting_read_only() -> (Settings, LoadOutcome) {
    let Some(path) = settings_path() else {
        return (Settings::default(), LoadOutcome::NoPath);
    };
    load_reporting_from_with(&path, |path| std::fs::read_to_string(path))
}

/// Read-modify-save 用の loader。読み取り失敗や quarantine 失敗を既定値へ畳まず、
/// `Err` で返すため、呼び出し側は原本を既定値で上書きできない。
/// `Empty` は従来どおり torn-write 痕跡として既定値での mutation を許可する。
pub fn load_for_mutation() -> Result<Settings, LoadOutcome> {
    let Some(path) = settings_path() else {
        return Err(LoadOutcome::NoPath);
    };
    let (settings, outcome) = load_for_mutation_from_with(
        &path,
        |path| std::fs::read_to_string(path),
        |from, to| std::fs::rename(from, to).map(|_| ()),
        |from, to| std::fs::copy(from, to),
    );
    if outcome == LoadOutcome::Corrupt {
        // The public mutation path also completes the TIP -> Config handoff.
        // Ledger creation is best effort and must not change the quarantine
        // result or put the preserved original at risk.
        create_corrupt_recovery_pending(&path);
    }
    match outcome {
        LoadOutcome::Loaded | LoadOutcome::Missing | LoadOutcome::Empty | LoadOutcome::Corrupt => {
            Ok(settings)
        }
        _ => Err(outcome),
    }
}

/// read-only loader の注入 seam。テストと診断用に、読み込みだけを行い破損原本を保持する。
pub(crate) fn load_reporting_from_with<R>(path: &Path, read: R) -> (Settings, LoadOutcome)
where
    R: Fn(&Path) -> std::io::Result<String>,
{
    let text = match read(path) {
        Ok(t) => t,
        Err(error) => return (Settings::default(), classify_read_error(error.kind())),
    };
    parse_settings_text(&text)
}

/// mutation loader の注入 seam。rename/copy を別々に受け取り、copy fallback の欠落と
/// 両方失敗時の原本保持をテスト可能にする（辞書 loader と同じ設計）。
pub(crate) fn load_for_mutation_from_with<R, M, C>(
    path: &Path,
    read: R,
    rename: M,
    copy: C,
) -> (Settings, LoadOutcome)
where
    R: Fn(&Path) -> std::io::Result<String>,
    M: Fn(&Path, &Path) -> std::io::Result<()>,
    C: Fn(&Path, &Path) -> std::io::Result<u64>,
{
    let (settings, outcome) = load_reporting_from_with(path, read);
    if outcome != LoadOutcome::Corrupt {
        return (settings, outcome);
    }
    let destination = quarantine_dest_path(path);
    if rename(path, &destination).is_ok() || copy(path, &destination).is_ok() {
        return (Settings::default(), LoadOutcome::Corrupt);
    }
    eprintln!(
        "nospacekey settings: 壊れた settings.json を退避できませんでした（{}）。\
         後続の save() が暗号化済み API キーを上書きする恐れがあります。\
         手動で原本をバックアップしてください。",
        path.display()
    );
    (Settings::default(), LoadOutcome::CorruptQuarantineFailed)
}

fn read_compatible_future_settings(text: &str) -> Settings {
    let Ok(serde_json::Value::Object(future)) = serde_json::from_str::<serde_json::Value>(text)
    else {
        return Settings::default();
    };
    let Ok(serde_json::Value::Object(mut accepted)) = serde_json::to_value(Settings::default())
    else {
        return Settings::default();
    };
    for (field, value) in future {
        if !accepted.contains_key(&field) {
            continue;
        }
        let mut candidate = accepted.clone();
        candidate.insert(field, value);
        if serde_json::from_value::<Settings>(serde_json::Value::Object(candidate.clone())).is_ok()
        {
            accepted = candidate;
        }
    }
    serde_json::from_value::<Settings>(serde_json::Value::Object(accepted))
        .map(|settings| normalize_loaded(migrate(settings)))
        .unwrap_or_default()
}

fn parse_settings_text(text: &str) -> (Settings, LoadOutcome) {
    if text.trim().is_empty() {
        return (Settings::default(), LoadOutcome::Empty);
    }
    #[derive(Deserialize)]
    struct VersionProbe {
        version: u64,
    }
    // A future schema may legitimately change the type of a field known to this binary.
    // Detect its version before typed deserialization so that incompatibility cannot be
    // mistaken for corruption and trigger quarantine.
    if serde_json::from_str::<VersionProbe>(text)
        .is_ok_and(|probe| probe.version > u64::from(SETTINGS_SCHEMA_VERSION))
    {
        return (
            read_compatible_future_settings(text),
            LoadOutcome::UnsupportedVersion,
        );
    }
    match serde_json::from_str::<Settings>(text) {
        Ok(s) => (normalize_loaded(migrate(s)), LoadOutcome::Loaded),
        Err(_) => (Settings::default(), LoadOutcome::Corrupt),
    }
}

fn quarantine_dest_path(path: &Path) -> PathBuf {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let base = format!(
        "json.corrupt.{}.{}.{}",
        d.as_secs(),
        d.subsec_nanos(),
        std::process::id()
    );
    let mut destination = path.with_extension(&base);
    let mut suffix = 1u32;
    while destination.exists() {
        destination = path.with_extension(format!("{base}.{suffix}"));
        suffix += 1;
    }
    destination
}

/// 親dir作成＋一時ファイル経由の原子的置換。
/// TIP と NospacekeyConfig.exe が同じ settings.json を共有するため、一時ファイル名は
/// プロセス毎に一意化する（固定名だと2プロセス同時 save で書き込み/rename が競合し
/// 片方が NotFound や破損を起こす）。失敗時は残骸 tmp をベストエフォートで掃除する。
pub fn save(s: &Settings) -> std::io::Result<()> {
    let path = settings_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no LOCALAPPDATA"))?;
    // シリアライズ失敗時に settings.json を空ファイルで上書きして破壊しないよう、ここで ?
    // で中断する（to_json は unwrap_or_default で "" に落ちるため save では使わない）。
    let json = serde_json::to_string_pretty(s).map_err(std::io::Error::other)?;
    save_atomic(&path, &json)
}

/// `save()` から抽出した汎用の原子保存ヘルパ（辞書ファイル保存でも使う想定）。
/// 親 dir 作成→一時ファイルへ書き→rename（短リトライ付き）→AppContainer read ACE 付与、まで行う。
pub(crate) fn save_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 元の save() は settings.json 専用に with_extension(".json" 前提) で組んでいたが、
    // save_atomic は拡張子を問わず呼ばれる汎用ヘルパなので、拡張子の有無に依存しない
    // 文字列連結（`{path}.tmp.{pid}`）にする。
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp = PathBuf::from(tmp_name);
    if let Err(e) = std::fs::write(&tmp, content) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // std::fs::rename は AsRef<Path> 総称のため直接渡すと HRTB 推論に失敗する。クロージャで包む。
    if let Err(e) = rename_with_retry(&tmp, path, |f, t| std::fs::rename(f, t)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // UU-7: AppContainer/LPAC ホスト（Start/検索の SearchHost 等）が settings.json を読めるよう、
    // 親ディレクトリと当該ファイルに AppContainer read ACE を付与する（best-effort・プロセス1回）。
    // %LOCALAPPDATA% は AppContainer ACE を継承しないため、付与しないと検索窓でだけ設定が既定へ
    // 劣化する（load() の PermissionDenied を握り潰していた症状）。DLL/pipe の ACE と同じ 2 SID・RX。
    ensure_appcontainer_readable(path);
    Ok(())
}

/// rename を短リトライする（spec §3.1: 5ms間隔×4回）。エンジンが settings.json/辞書ファイルを
/// 読んでいる最中の rename は Windows では sharing violation（PermissionDenied）で瞬間的に
/// 失敗し得るため、リトライなしだと保存が失敗扱いになる。rename 実装を注入可能にしてあるのは、
/// 実 OS のファイルロックを再現せずにリトライ回数だけを単体テストするため。
fn rename_with_retry(
    from: &Path,
    to: &Path,
    rename: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    const RETRIES: u32 = 4;
    const DELAY_MS: u64 = 5;
    let mut last_err = None;
    for attempt in 0..RETRIES {
        match rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < RETRIES {
                    std::thread::sleep(std::time::Duration::from_millis(DELAY_MS));
                }
            }
        }
    }
    Err(last_err.expect("RETRIES > 0"))
}

/// UU-7: AppContainer(`ALL APPLICATION PACKAGES`=S-1-15-2-1)/LPAC(`ALL RESTRICTED APPLICATION
/// PACKAGES`=S-1-15-2-2) の 2 SID へ read+execute を付与する icacls 引数を組み立てる純関数
/// （テスト可能）。`inheritable=true` はディレクトリ向け（(OI)(CI) で以後の原子 rename により
/// 作られる settings.json も RX を継承）、false はファイル向け（現存ファイルを直接読めるように）。
pub fn icacls_grant_args(target: &str, inheritable: bool) -> Vec<String> {
    let spec = if inheritable { "(OI)(CI)(RX)" } else { "(RX)" };
    vec![
        target.to_string(),
        "/grant".to_string(),
        format!("*S-1-15-2-1:{spec}"),
        format!("*S-1-15-2-2:{spec}"),
        "/Q".to_string(),
    ]
}

/// UU-7: settings.json とその親 dir に AppContainer read ACE を付与する（プロセス1回・best-effort）。
/// dir は継承付きで付与し、以後の save が作る一時ファイル（→原子 rename）へ RX を継承させる。
/// 現存 settings.json は継承前に作られているので直接も付与する。失敗（icacls 不在/権限不足）は
/// 無視し、save() の成否には一切影響させない（設定は書けているので ACE 付与失敗で止めない）。
#[cfg(windows)]
fn ensure_appcontainer_readable(file: &std::path::Path) {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if let Some(dir) = file.parent() {
            run_icacls(&icacls_grant_args(&dir.to_string_lossy(), true));
        }
        run_icacls(&icacls_grant_args(&file.to_string_lossy(), false));
    });
}

#[cfg(windows)]
fn run_icacls(args: &[String]) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000; // コンソール窓を出さない（切替時のフラッシュ防止）。
    let _ = std::process::Command::new("icacls")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// 非 Windows（ホストでの単体テスト等）では no-op。
#[cfg(not(windows))]
fn ensure_appcontainer_readable(_file: &std::path::Path) {}

/// engine へ注入する NOSPACEKEY_* env を作る。api_key_plain=DPAPI復号後の鍵。
/// env_lookup が Some を返すキーは「既にプロセス env にある」とみなし注入しない（env override 尊重 = D6）。
pub fn resolve_env_map(
    s: &Settings,
    api_key_plain: Option<&str>,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut put = |k: &str, v: String| {
        if env_lookup(k).is_none() {
            out.push((k.to_string(), v));
        }
    };
    if llm_effective_enabled(s) {
        if let Some(key) = api_key_plain {
            if !key.is_empty() {
                put("NOSPACEKEY_LLM_API_KEY", key.to_string());
            }
        }
        if !s.llm.endpoint.is_empty() {
            put("NOSPACEKEY_LLM_ENDPOINT", s.llm.endpoint.clone());
        }
        // endpoint/prompt と同じく空なら注入しない（エンジン側が既定 model へフォールバックする）。
        if !s.llm.model.is_empty() {
            put("NOSPACEKEY_LLM_MODEL", s.llm.model.clone());
        }
        if !s.llm.prompt.is_empty() {
            put("NOSPACEKEY_LLM_PROMPT", s.llm.prompt.clone());
        }
        put("NOSPACEKEY_LLM_TIMEOUT_MS", s.llm.timeout_ms.to_string());
    }
    put(
        "NOSPACEKEY_ZENZAI",
        if s.zenzai.enabled {
            "on".into()
        } else {
            "off".into()
        },
    );
    if !s.zenzai.weight_path.is_empty() {
        put("NOSPACEKEY_ZENZAI_WEIGHT", s.zenzai.weight_path.clone());
    }
    put(
        "NOSPACEKEY_ZENZAI_INFERENCE_LIMIT",
        s.zenzai.effective_inference_limit().to_string(),
    );
    put(
        "NOSPACEKEY_LEARNING",
        if s.learning.enabled {
            "1".into()
        } else {
            "0".into()
        },
    );
    put(
        "NOSPACEKEY_TYPO_LEARN",
        if s.typo_correct.learn {
            "1".into()
        } else {
            "0".into()
        },
    );
    put(
        "NOSPACEKEY_USER_DICT_ENABLED",
        if s.user_dictionary.enabled {
            "1".into()
        } else {
            "0".into()
        },
    );
    put(
        "NOSPACEKEY_INLINE_PREDICTION",
        if s.inline_prediction.enabled {
            "1".into()
        } else {
            "0".into()
        },
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static LOCALAPPDATA_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn localappdata_test_lock() -> MutexGuard<'static, ()> {
        LOCALAPPDATA_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct LocalAppDataGuard {
        previous: Option<OsString>,
    }

    impl LocalAppDataGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("LOCALAPPDATA");
            std::env::set_var("LOCALAPPDATA", path);
            Self { previous }
        }
    }

    impl Drop for LocalAppDataGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("LOCALAPPDATA", value),
                None => std::env::remove_var("LOCALAPPDATA"),
            }
        }
    }

    // ---- 巡4 J6: normalize_loaded（相対 weight_path の切り捨て）----

    #[test]
    fn from_json_str_normalizes_relative_weight_path() {
        // 手書き settings.json 相当 — 相対パスは CWD 分裂の元なので空へ畳み、
        // 3段解決表（per-user→exe 隣）へフォールバックさせる。version は Settings の
        // 必須フィールド（無いとパース全体が失敗し既定へ落ちる）。
        let json =
            r#"{"version": 2, "zenzai": {"enabled": true, "weight_path": "models\\w.gguf"}}"#;
        let s = Settings::from_json_str(json);
        assert_eq!(s.zenzai.weight_path, "");
    }

    #[test]
    fn from_json_str_keeps_absolute_weight_path() {
        let json =
            r#"{"version": 2, "zenzai": {"enabled": true, "weight_path": "C:\\models\\w.gguf"}}"#;
        let s = Settings::from_json_str(json);
        assert_eq!(s.zenzai.weight_path, r"C:\models\w.gguf");
    }

    // ---- save_atomic: rename 短リトライ ----
    #[test]
    fn rename_with_retry_retries_transient_failure() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let r = rename_with_retry(Path::new("a"), Path::new("b"), |_f, _t| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "sharing violation",
                ))
            } else {
                Ok(())
            }
        });
        assert!(r.is_ok());
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn rename_with_retry_gives_up_after_retries() {
        let r = rename_with_retry(Path::new("a"), Path::new("b"), |_f, _t| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "sv",
            ))
        });
        assert!(r.is_err());
    }

    #[test]
    fn save_atomic_roundtrip_via_tempdir() {
        let dir = std::env::temp_dir().join(format!("nsk-sa-{}", std::process::id()));
        let path = dir.join("x.json");
        save_atomic(&path, "[1,2]").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[1,2]");
        save_atomic(&path, "[3]").unwrap(); // 上書き(rename REPLACE_EXISTING)
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[3]");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- UU-7: 読み取り失敗の分類 ----
    #[test]
    fn classify_read_error_distinguishes_permission_denied() {
        use std::io::ErrorKind;
        assert_eq!(
            classify_read_error(ErrorKind::NotFound),
            LoadOutcome::Missing
        );
        // AppContainer/LPAC ホストからの読み取り拒否は独立要因（診断できるように）。
        assert_eq!(
            classify_read_error(ErrorKind::PermissionDenied),
            LoadOutcome::PermissionDenied
        );
        assert_eq!(classify_read_error(ErrorKind::Other), LoadOutcome::IoError);
    }

    // ---- UU-7: AppContainer read ACE 付与の icacls 引数 ----
    #[test]
    fn icacls_grant_args_builds_expected_for_dir_and_file() {
        // ディレクトリ: 継承付き（(OI)(CI)）で 2 SID に RX。
        assert_eq!(
            icacls_grant_args(r"C:\x\nospacekey", true),
            vec![
                r"C:\x\nospacekey".to_string(),
                "/grant".to_string(),
                "*S-1-15-2-1:(OI)(CI)(RX)".to_string(),
                "*S-1-15-2-2:(OI)(CI)(RX)".to_string(),
                "/Q".to_string(),
            ]
        );
        // ファイル: 継承なし（現存ファイルを直接読めるように）RX のみ。
        assert_eq!(
            icacls_grant_args(r"C:\x\nospacekey\settings.json", false),
            vec![
                r"C:\x\nospacekey\settings.json".to_string(),
                "/grant".to_string(),
                "*S-1-15-2-1:(RX)".to_string(),
                "*S-1-15-2-2:(RX)".to_string(),
                "/Q".to_string(),
            ]
        );
    }

    #[test]
    fn default_is_llm_off_zenzai_on_live_on() {
        let s = Settings::default();
        assert!(!s.llm.enabled);
        assert_eq!(s.llm.model, "gpt-4o-mini");
        assert!(s.zenzai.enabled);
        assert!(s.live_conversion.enabled);
        assert_eq!(s.version, 2);
    }
    #[test]
    fn zenzai_inference_limit_defaults_to_1_and_clamps() {
        assert_eq!(Settings::default().zenzai.inference_limit, 1);
        // 旧 settings.json（フィールド欠落）も 1 でロード（後方互換）。
        let s = Settings::from_json_str(r#"{"version":2,"zenzai":{"enabled":true}}"#);
        assert_eq!(s.zenzai.inference_limit, 1);
        // 手編集の異常値はクランプ（唯一の正規化点）。
        let mut z = ZenzaiSettings::default();
        z.inference_limit = 0;
        assert_eq!(z.effective_inference_limit(), 1);
        z.inference_limit = 11;
        assert_eq!(z.effective_inference_limit(), 10);
        z.inference_limit = 5;
        assert_eq!(z.effective_inference_limit(), 5);
    }
    #[test]
    fn env_map_emits_zenzai_inference_limit_and_respects_env_override() {
        let mut s = Settings::default();
        s.zenzai.inference_limit = 7;
        let map = resolve_env_map(&s, None, |_| None);
        let get = |k: &str| map.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
        assert_eq!(get("NOSPACEKEY_ZENZAI_INFERENCE_LIMIT"), Some("7".into()));
        // 範囲外はクランプ後の値を注入する。
        s.zenzai.inference_limit = 0;
        let map = resolve_env_map(&s, None, |_| None);
        let get = |k: &str| map.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
        assert_eq!(get("NOSPACEKEY_ZENZAI_INFERENCE_LIMIT"), Some("1".into()));
        // D6: 実プロセス env に既にあるキーは注入しない。
        let map = resolve_env_map(&s, None, |k| {
            (k == "NOSPACEKEY_ZENZAI_INFERENCE_LIMIT").then(|| "5".to_string())
        });
        assert!(map
            .iter()
            .all(|(k, _)| k != "NOSPACEKEY_ZENZAI_INFERENCE_LIMIT"));
    }
    #[test]
    fn default_direct_defaults_false() {
        // SP7: 既定はひらがな（従来挙動を保持）。
        assert!(!Settings::default().default_direct);
    }
    #[test]
    fn default_direct_roundtrip() {
        let s = Settings {
            default_direct: true,
            ..Default::default()
        };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(back.default_direct);
    }
    #[test]
    fn missing_default_direct_field_loads_false() {
        // 旧 settings.json（default_direct フィールドなし）でも false でロードできる（後方互換）。
        let s = Settings::from_json_str(r#"{"version":1}"#);
        assert!(!s.default_direct);
    }
    #[test]
    fn old_update_settings_default_to_opt_in_and_prompt_visible() {
        let s = Settings::from_json_str(r#"{"version":2,"update":{"include_beta":true}}"#);
        assert!(!s.update.automatic_check);
        assert!(s.update.include_beta);
        assert!(!s.update.automatic_check_prompt_dismissed);
    }
    #[test]
    fn update_settings_roundtrip_preserves_opt_in_fields() {
        let mut s = Settings::default();
        s.update.automatic_check = true;
        s.update.automatic_check_prompt_dismissed = true;
        let loaded = Settings::from_json_str(&s.to_json());
        assert!(loaded.update.automatic_check);
        assert!(loaded.update.automatic_check_prompt_dismissed);
    }
    #[test]
    fn json_roundtrip() {
        let mut s = Settings::default();
        s.llm.enabled = true;
        s.llm.endpoint = "https://api.example.com/v1/chat/completions".into();
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert!(back.llm.enabled);
        assert_eq!(back.llm.endpoint, s.llm.endpoint);
    }
    #[test]
    fn corrupt_json_falls_back_to_default() {
        let s = Settings::from_json_str("{ this is not json ");
        assert!(!s.llm.enabled);
    }

    #[test]
    fn read_only_reporting_seam_does_not_quarantine_corrupt_settings() {
        let dir = std::env::temp_dir().join(format!(
            "nospacekey-settings-read-only-{}",
            std::process::id()
        ));
        let path = dir.join("settings.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"{"version":"2"}"#).unwrap();

        let (loaded, outcome) = load_reporting_from_with(&path, |_| std::fs::read_to_string(&path));
        assert_eq!(outcome, LoadOutcome::Corrupt);
        assert!(!loaded.llm.enabled);
        assert!(path.exists());
        assert!(std::fs::read_dir(&dir).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("corrupt")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_only_reporting_production_api_preserves_corrupt_file() {
        let _lock = localappdata_test_lock();
        let base = std::env::temp_dir().join(format!(
            "nospacekey-settings-read-only-production-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let _env = LocalAppDataGuard::set(&base);
        let path = settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        for original in [br#"{ broken"#.as_slice(), br#"{"version":"2"}"#.as_slice()] {
            std::fs::write(&path, original).unwrap();
            let before = std::fs::read(&path).unwrap();
            let (loaded, outcome) = load_reporting_read_only();
            assert_eq!(outcome, LoadOutcome::Corrupt);
            assert!(!loaded.llm.enabled);
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".corrupt.")));
            assert!(!has_pending_corrupt_recovery_notice());
        }

        let mut expected = Settings::default();
        expected.update.automatic_check = true;
        std::fs::write(&path, expected.to_json()).unwrap();
        let (loaded, outcome) = load_reporting_read_only();
        assert_eq!(outcome, LoadOutcome::Loaded);
        assert!(loaded.update.automatic_check);

        std::fs::remove_file(&path).unwrap();
        let (_, outcome) = load_reporting_read_only();
        assert_eq!(outcome, LoadOutcome::Missing);

        // A directory at the file path exercises a real non-NotFound read
        // error (the exact platform mapping is IoError or PermissionDenied).
        std::fs::create_dir_all(&path).unwrap();
        let (_, outcome) = load_reporting_read_only();
        assert!(matches!(
            outcome,
            LoadOutcome::IoError | LoadOutcome::PermissionDenied
        ));

        std::env::remove_var("LOCALAPPDATA");
        let (_, outcome) = load_reporting_read_only();
        assert_eq!(outcome, LoadOutcome::NoPath);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn corrupt_recovery_ledger_is_at_least_once_and_acknowledged() {
        let _lock = localappdata_test_lock();
        let base = std::env::temp_dir().join(format!(
            "nospacekey-settings-corrupt-marker-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let _env = LocalAppDataGuard::set(&base);
        let path = settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for original in ["{ broken", r#"{"version":"2"}"#] {
            std::fs::write(&path, original).unwrap();
            let (_, outcome) = load_reporting();
            assert_eq!(outcome, LoadOutcome::Corrupt);
            // Simulate a crash before Config acknowledges: the durable pending
            // entry remains visible on the next process/startup.
            assert!(has_pending_corrupt_recovery_notice());
        }
        std::thread::scope(|scope| {
            scope.spawn(acknowledge_corrupt_recovery_notices);
            scope.spawn(acknowledge_corrupt_recovery_notices);
        });
        assert!(!has_pending_corrupt_recovery_notice());
        // A second consumer is harmless and never removes the ledger entries.
        acknowledge_corrupt_recovery_notices();
        assert!(!has_pending_corrupt_recovery_notice());
        // The quarantine remains user-recoverable; only the dedicated ledger
        // acknowledgement is added by Config.
        assert!(std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".corrupt.")));
        assert!(std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(CORRUPT_RECOVERY_PENDING_SUFFIX)));
        assert!(std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(CORRUPT_RECOVERY_ACK_SUFFIX)));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn pending_notice_scan_reaches_fresh_entry_after_old_ledger_entries() {
        let _lock = localappdata_test_lock();
        let base = std::env::temp_dir().join(format!(
            "nospacekey-settings-corrupt-ledger-scan-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let _env = LocalAppDataGuard::set(&base);
        let path = settings_path().unwrap();
        let parent = path.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();

        // More than the old 256-entry cap of malformed entries and already
        // acknowledged ledger entries must not hide a fresh pending notice.
        for index in 0..300 {
            std::fs::write(
                parent.join(format!("a-invalid-ledger-entry-{index:03}")),
                "",
            )
            .unwrap();
            let token = format!("old{index:03}");
            let pending = path.with_file_name(marker_name(
                CORRUPT_RECOVERY_PENDING_PREFIX,
                &token,
                CORRUPT_RECOVERY_PENDING_SUFFIX,
            ));
            std::fs::write(&pending, "").unwrap();
            std::fs::write(ack_path_for_pending(&pending, &token), "").unwrap();
        }
        let fresh_token = "zzzz-fresh";
        let fresh = path.with_file_name(marker_name(
            CORRUPT_RECOVERY_PENDING_PREFIX,
            fresh_token,
            CORRUPT_RECOVERY_PENDING_SUFFIX,
        ));
        std::fs::write(fresh, "").unwrap();

        assert!(has_pending_corrupt_recovery_notice());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn public_mutation_loader_creates_pending_for_syntax_and_typed_corruption() {
        let _lock = localappdata_test_lock();
        let base = std::env::temp_dir().join(format!(
            "nospacekey-settings-corrupt-public-loader-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let _env = LocalAppDataGuard::set(&base);
        let path = settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        for original in ["{ broken", r#"{"version":"2"}"#] {
            std::fs::write(&path, original).unwrap();
            let loaded = load_for_mutation().expect("corrupt settings are quarantined");
            assert!(!loaded.llm.enabled);
            assert!(!path.exists());
            assert!(has_pending_corrupt_recovery_notice());
            acknowledge_corrupt_recovery_notices();
            assert!(!has_pending_corrupt_recovery_notice());
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn corrupt_recovery_ledger_never_overwrites_precreated_entries() {
        let _lock = localappdata_test_lock();
        let base = std::env::temp_dir().join(format!(
            "nospacekey-settings-corrupt-ledger-link-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let _env = LocalAppDataGuard::set(&base);
        let path = settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let token = "precreated";
        let pending = path.with_file_name(marker_name(
            CORRUPT_RECOVERY_PENDING_PREFIX,
            token,
            CORRUPT_RECOVERY_PENDING_SUFFIX,
        ));
        std::fs::write(&pending, "pending").unwrap();
        let ack = ack_path_for_pending(&pending, token);

        #[cfg(unix)]
        {
            let target = base.join("target");
            std::fs::write(&target, "keep").unwrap();
            std::os::unix::fs::symlink(&target, &ack).unwrap();
            acknowledge_corrupt_recovery_notices();
            assert!(ack.symlink_metadata().unwrap().file_type().is_symlink());
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&ack, "keep").unwrap();
            acknowledge_corrupt_recovery_notices();
            assert_eq!(std::fs::read_to_string(&ack).unwrap(), "keep");
        }
        // A pre-created symlink is not treated as acknowledgement (and remains
        // retryable); on Windows the regular pre-created file is already an
        // acknowledgement, but it still must not be overwritten.
        #[cfg(unix)]
        assert!(has_pending_corrupt_recovery_notice());
        #[cfg(not(unix))]
        assert!(!has_pending_corrupt_recovery_notice());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn loaded_missing_empty_and_read_only_corruption_do_not_create_pending_notice() {
        let _lock = localappdata_test_lock();
        let base = std::env::temp_dir().join(format!(
            "nospacekey-settings-corrupt-ledger-clean-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let _env = LocalAppDataGuard::set(&base);
        let path = settings_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        let valid = Settings::default().to_json();
        std::fs::write(&path, valid).unwrap();
        assert_eq!(load_reporting().1, LoadOutcome::Loaded);
        assert!(!has_pending_corrupt_recovery_notice());
        std::fs::remove_file(&path).unwrap();
        assert_eq!(load_reporting().1, LoadOutcome::Missing);
        assert!(!has_pending_corrupt_recovery_notice());
        std::fs::write(&path, " \n").unwrap();
        assert_eq!(load_reporting().1, LoadOutcome::Empty);
        assert!(!has_pending_corrupt_recovery_notice());
        std::fs::write(&path, "{ broken").unwrap();
        assert_eq!(load_reporting_read_only().1, LoadOutcome::Corrupt);
        assert!(!has_pending_corrupt_recovery_notice());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn syntax_and_typed_corruption_share_recovery_outcome() {
        for text in ["{ broken", r#"{"version":"2"}"#] {
            let (settings, outcome) = parse_settings_text(text);
            assert_eq!(outcome, LoadOutcome::Corrupt);
            assert!(!settings.llm.enabled);
        }
    }

    #[test]
    fn mutation_loader_refuses_unreadable_settings_before_save() {
        use std::cell::Cell;
        let path = PathBuf::from(r"C:\settings.json");
        let rename_called = Cell::new(false);
        let copy_called = Cell::new(false);
        let result = load_for_mutation_from_with(
            &path,
            |_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            |_from, _to| {
                rename_called.set(true);
                Ok(())
            },
            |_from, _to| {
                copy_called.set(true);
                Ok(1)
            },
        );
        assert_eq!(result.1, LoadOutcome::PermissionDenied);
        assert!(!result.0.llm.enabled);
        assert!(!rename_called.get());
        assert!(!copy_called.get());
    }

    #[test]
    fn newer_schema_is_readable_but_never_opened_for_mutation() {
        let future = r#"{"version":3,"live_conversion":{"enabled":false},"future_setting":true}"#;
        let (read_only, outcome) = parse_settings_text(future);
        assert_eq!(outcome, LoadOutcome::UnsupportedVersion);
        assert!(!read_only.live_conversion.enabled);

        let path = PathBuf::from(r"C:\settings.json");
        let mutation = load_for_mutation_from_with(
            &path,
            |_| Ok(future.to_string()),
            |_from, _to| panic!("a newer schema is not corrupt and must not be quarantined"),
            |_from, _to| panic!("a newer schema is not corrupt and must not be copied"),
        );
        assert_eq!(mutation.1, LoadOutcome::UnsupportedVersion);
        assert!(!mutation.0.live_conversion.enabled);

        let _lock = localappdata_test_lock();
        let base = std::env::temp_dir().join(format!(
            "nospacekey-settings-future-schema-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let _env = LocalAppDataGuard::set(&base);
        let settings_path = settings_path().unwrap();
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(&settings_path, future).unwrap();
        assert!(matches!(
            load_for_mutation(),
            Err(LoadOutcome::UnsupportedVersion)
        ));
        assert_eq!(std::fs::read_to_string(&settings_path).unwrap(), future);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn newer_schema_with_changed_known_shape_is_not_quarantined() {
        let future = r#"{"version":3,"default_direct":true,"live_conversion":"future-shape"}"#;
        let (readable, outcome) = parse_settings_text(future);
        assert_eq!(outcome, LoadOutcome::UnsupportedVersion);
        assert!(readable.default_direct);

        let _lock = localappdata_test_lock();
        let base = std::env::temp_dir().join(format!(
            "nospacekey-settings-future-shape-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let _env = LocalAppDataGuard::set(&base);
        let settings_path = settings_path().unwrap();
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(&settings_path, future).unwrap();
        assert!(matches!(
            load_for_mutation(),
            Err(LoadOutcome::UnsupportedVersion)
        ));
        assert_eq!(std::fs::read_to_string(&settings_path).unwrap(), future);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn mutation_loader_allows_empty_and_quarantine_success() {
        let path = PathBuf::from(r"C:\settings.json");
        let empty = load_for_mutation_from_with(
            &path,
            |_| Ok("  \n".to_string()),
            |_from, _to| Ok(()),
            |_from, _to| Ok(1),
        );
        assert_eq!(empty.1, LoadOutcome::Empty);
        assert!(!empty.0.llm.enabled);

        let corrupt = load_for_mutation_from_with(
            &path,
            |_| Ok("not json".to_string()),
            |_from, _to| Ok(()),
            |_from, _to| Ok(1),
        );
        assert_eq!(corrupt.1, LoadOutcome::Corrupt);
        assert!(!corrupt.0.llm.enabled);
    }

    #[test]
    fn mutation_loader_preserves_corrupt_original_when_quarantine_fails() {
        let dir = std::env::temp_dir().join(format!(
            "nospacekey-settings-quarantine-failure-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let original = "not json";
        std::fs::write(&path, original).unwrap();
        let result = load_for_mutation_from_with(
            &path,
            |path| std::fs::read_to_string(path),
            |_from, _to| Err(std::io::Error::other("locked")),
            |_from, _to| Err(std::io::Error::other("locked")),
        );
        assert_eq!(result.1, LoadOutcome::CorruptQuarantineFailed);
        assert!(!result.0.llm.enabled);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn llm_effective_is_false_while_frozen_even_when_enabled() {
        // 凍結契約(docs/superpowers/specs/2026-07-21-llm-freeze-design.md):
        // settings 直編集で enabled=true でも実効は無効。再開時は LLM_CONVERT_FROZEN=false で復帰。
        assert!(LLM_CONVERT_FROZEN);
        assert!(!llm_effective(true));
        assert!(!llm_effective(false));
        let mut s = Settings::default();
        s.llm.enabled = true;
        assert!(!llm_effective_enabled(&s));
    }
    #[test]
    fn frozen_env_map_omits_llm_keys_even_when_enabled() {
        // 凍結中(LLM_CONVERT_FROZEN)は enabled=true+鍵ありでも NOSPACEKEY_LLM_* を一切注入しない
        // (平文キーを engine env へ流さない)。LLM 以外のキーは不変。env override 尊重の一般機構は
        // NOSPACEKEY_LEARNING/NOSPACEKEY_TYPO_LEARN のテストが被覆。凍結前の注入期待
        // (env_map_skips_keys_already_in_env_and_emits_from_settings / env_map_skips_empty_model)は
        // 再開時に spec の再開手順で復元する。
        let mut s = Settings::default();
        s.llm.enabled = true;
        s.llm.endpoint = "https://e".into();
        s.zenzai.enabled = false;
        let map = resolve_env_map(&s, Some("sk-test"), |_| None);
        assert!(map.iter().all(|(k, _)| !k.starts_with("NOSPACEKEY_LLM_")));
        let get = |k: &str| map.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
        assert_eq!(get("NOSPACEKEY_ZENZAI").as_deref(), Some("off"));
    }
    #[test]
    fn save_then_load_roundtrip_and_corrupt_is_backed_up() {
        // LOCALAPPDATA を一意な temp dir に向けて save→load を往復し、壊れた原本が
        // reporting seam は原本を保持し、通常の load() は従来どおり退避することを確認する。
        let _lock = localappdata_test_lock();
        let base =
            std::env::temp_dir().join(format!("nospacekey-settings-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _env = LocalAppDataGuard::set(&base);

        let mut s = Settings::default();
        s.llm.enabled = true;
        s.llm.api_key_dpapi = "blob".into();
        save(&s).expect("save ok");
        let loaded = load();
        assert!(loaded.llm.enabled);
        assert_eq!(loaded.llm.api_key_dpapi, "blob");

        // 壊れた JSON を load すると、従来どおり原本は一意名へ退避される。
        let path = settings_path().unwrap();
        let dir = path.parent().unwrap().to_path_buf();
        let count_backups = || {
            std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("settings.json.corrupt.")
                        && !e
                            .file_name()
                            .to_string_lossy()
                            .ends_with(CORRUPT_RECOVERY_PENDING_SUFFIX)
                        && !e
                            .file_name()
                            .to_string_lossy()
                            .ends_with(CORRUPT_RECOVERY_ACK_SUFFIX)
                })
                .count()
        };

        std::fs::write(&path, "{ broken json ").unwrap();
        let after = load();
        assert!(!after.llm.enabled); // 既定へ劣化
        assert!(count_backups() >= 1); // load_reporting が原本を退避

        // 2度目の破損 → 2つ目の別退避が生まれ、1度目の退避が上書き破壊されないこと。
        // 退避名は nanos/pid に加え、衝突時は連番でずらして必ず未使用名を選ぶので、
        // 同一クロック tick に2度当たっても 2 件目が確実に増える（再利用・上書きしない）。
        std::fs::write(&path, "{ broken json again ").unwrap();
        let _ = load();
        assert!(count_backups() >= 2); // 1度目の退避は残り、別ファイルとして2件目が増える

        // 空ファイル（torn write の痕跡）は破損退避せず既定へ劣化する（.corrupt を増やさない）。
        let before_empty = count_backups();
        std::fs::write(&path, "").unwrap();
        let after_empty = load_for_mutation().expect("empty settings may be mutated");
        assert!(!after_empty.llm.enabled); // 既定へ劣化
        assert_eq!(count_backups(), before_empty); // 空ファイルは退避しない

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parse_hex_color_parses_and_rejects() {
        assert_eq!(parse_hex_color("#FAFAFA"), Some((0xFA, 0xFA, 0xFA)));
        assert_eq!(parse_hex_color("#0078D7"), Some((0x00, 0x78, 0xD7)));
        assert_eq!(parse_hex_color("#ffffff"), Some((0xFF, 0xFF, 0xFF))); // 小文字可
        assert_eq!(parse_hex_color("FAFAFA"), None); // # 無し
        assert_eq!(parse_hex_color("#FFF"), None); // 3 桁は非対応
        assert_eq!(parse_hex_color("#GGGGGG"), None); // 非 16 進
        assert_eq!(parse_hex_color(""), None);
    }

    #[test]
    fn appearance_defaults_are_auto_acrylic_round() {
        let a = Appearance::default();
        assert_eq!(a.theme, "auto");
        assert_eq!(a.backdrop, "acrylic");
        assert_eq!(a.corner, "round");
        assert_eq!(a.font_family, "Yu Gothic UI");
        assert!((a.font_point - 10.5).abs() < 1e-6);
        // 既定 light パレットは Apple 風トークン由来の値。
        assert_eq!(a.palette_light.bg, "#FFFFFF");
        assert_eq!(a.palette_light.sel_bg, "#0071E3");
    }

    #[test]
    fn settings_without_appearance_loads_defaults() {
        // 旧 settings.json（appearance フィールドなし）でも既定 Appearance でロードできる。
        let s = Settings::from_json_str(r#"{"version":1}"#);
        assert_eq!(s.appearance.theme, "auto");
        assert_eq!(s.appearance.palette_dark.bg, default_dark_palette().bg);
    }

    #[test]
    fn feedback_settings_default_disabled_and_roundtrips() {
        // opt-in: 既定 false。フィールド欠落の旧 settings.json でも false でロード（後方互換）。
        assert!(!Settings::default().feedback.enabled);
        let s: Settings = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert!(!s.feedback.enabled);
        // ON がラウンドトリップする。
        let mut s = Settings::default();
        s.feedback.enabled = true;
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(back.feedback.enabled);
    }

    #[test]
    fn learning_defaults_to_enabled_and_resolves_env() {
        // 既定 ON（settings.json 欠落フィールドは true でロード — 後方互換）。
        let s = Settings::default();
        assert!(s.learning.enabled);
        let js = r#"{"version":1}"#; // learning フィールド無しの旧 settings.json
        assert!(Settings::from_json_str(js).learning.enabled);

        // resolve_env_map: 常に NOSPACEKEY_LEARNING を注入（NOSPACEKEY_ZENZAI と同じ「常時 put」）。
        let env = resolve_env_map(&s, None, |_| None);
        assert!(env
            .iter()
            .any(|(k, v)| k == "NOSPACEKEY_LEARNING" && v == "1"));
        let mut off = s.clone();
        off.learning.enabled = false;
        let env = resolve_env_map(&off, None, |_| None);
        assert!(env
            .iter()
            .any(|(k, v)| k == "NOSPACEKEY_LEARNING" && v == "0"));
        // D6: ユーザーが env で明示 override していれば注入しない。
        let env = resolve_env_map(&s, None, |k| {
            (k == "NOSPACEKEY_LEARNING").then(|| "0".into())
        });
        assert!(!env.iter().any(|(k, _)| k == "NOSPACEKEY_LEARNING"));
    }

    #[test]
    fn number_defaults_to_full_width_and_roundtrips() {
        // 既定は全角（ユーザーの「普通は全角」に一致）。
        let s = Settings::default();
        assert!(s.number.full_width, "既定は全角");
        // フィールド欠落の旧 settings.json も全角へ（後方互換）。
        let js = r#"{"version":1}"#;
        assert!(Settings::from_json_str(js).number.full_width);
        // roundtrip（false も往復する）。
        let mut half = s.clone();
        half.number.full_width = false;
        let back = Settings::from_json_str(&half.to_json());
        assert!(!back.number.full_width);
    }

    #[test]
    fn punctuation_defaults_to_full_width_and_roundtrips() {
        // 既定は全角（ユーザーの「普通は全角」に一致）。
        let s = Settings::default();
        assert!(s.punctuation.full_width, "既定は全角");
        // フィールド欠落の旧 settings.json も全角へ（後方互換）。
        let js = r#"{"version":1}"#;
        assert!(Settings::from_json_str(js).punctuation.full_width);
        // roundtrip（false も往復する）。
        let mut half = s.clone();
        half.punctuation.full_width = false;
        let back = Settings::from_json_str(&half.to_json());
        assert!(!back.punctuation.full_width);
    }

    #[test]
    fn symbol_defaults_to_half_width_and_roundtrips() {
        // 既定は半角（記号は ASCII のまま。number/punctuation と逆 — 2026-07-16 spec）。
        let s = Settings::default();
        assert!(!s.symbol.full_width, "既定は半角");
        // フィールド欠落の旧 settings.json も半角へ（後方互換）。
        let js = r#"{"version":1}"#;
        assert!(!Settings::from_json_str(js).symbol.full_width);
        // roundtrip（true も往復する）。
        let mut full = s.clone();
        full.symbol.full_width = true;
        let back = Settings::from_json_str(&full.to_json());
        assert!(back.symbol.full_width);
    }

    // ---- Issue #1: symbol_full_width 対象記号の個別選択（2026-08-02 spec）----

    #[test]
    fn symbol_object_entirely_missing_loads_default_29_via_handwritten_default() {
        // symbol オブジェクトごと欠落した旧 JSON は SymbolSettings::default()（手書き）を通る。
        let s = Settings::from_json_str(r#"{"version":2}"#);
        assert!(!s.symbol.full_width);
        assert_eq!(
            s.symbol.full_width_chars,
            symbol::default_full_width_chars()
        );
        assert_eq!(s.symbol.full_width_chars.len(), 29);
    }

    #[test]
    fn full_width_chars_field_missing_loads_default_29_via_field_default() {
        // symbol はあるが full_width_chars フィールドだけ無い旧 JSON は serde のフィールド
        // default（`default = "symbol::default_full_width_chars"`）を通る。
        let s = Settings::from_json_str(r#"{"version":2,"symbol":{"full_width":true}}"#);
        assert!(s.symbol.full_width);
        assert_eq!(
            s.symbol.full_width_chars,
            symbol::default_full_width_chars()
        );
    }

    #[test]
    fn partial_symbol_subset_saves_and_restores() {
        let mut s = Settings::default();
        s.symbol.full_width = true;
        s.symbol.full_width_chars = BTreeSet::from(['!', '?']);
        let back = Settings::from_json_str(&s.to_json());
        assert_eq!(back.symbol.full_width_chars, BTreeSet::from(['!', '?']));
    }

    #[test]
    fn empty_symbol_set_saves_and_restores_distinct_from_missing_field() {
        // 空集合（全解除）は serde 上「欠落」と区別される — 欠落は既定29へ、明示空は空のまま。
        let mut s = Settings::default();
        s.symbol.full_width_chars = BTreeSet::new();
        let back = Settings::from_json_str(&s.to_json());
        assert!(back.symbol.full_width_chars.is_empty());
    }

    #[test]
    fn invalid_array_elements_are_skipped_and_valid_ones_kept() {
        let js = r#"{"version":2,"symbol":{"full_width":true,"full_width_chars":["!","ab",42,"?"]},"number":{"full_width":false}}"#;
        let s = Settings::from_json_str(js);
        assert_eq!(s.symbol.full_width_chars, BTreeSet::from(['!', '?']));
        assert!(s.symbol.full_width);
        assert!(
            !s.number.full_width,
            "不正要素があっても兄弟設定は壊れない（spec §6）"
        );
    }

    #[test]
    fn invalid_container_falls_back_to_default_29_without_breaking_sibling_settings() {
        // 不正コンテナ（配列でない値）は既定29へフォールバックし、blast radius はこのフィールド
        // に限定される — 兄弟設定（number 等）や symbol.full_width 自体は壊れない。
        for bad in [r#""!?""#, "null", "{}"] {
            let js = format!(
                r#"{{"version":2,"symbol":{{"full_width":true,"full_width_chars":{bad}}},"number":{{"full_width":false}}}}"#
            );
            let s = Settings::from_json_str(&js);
            assert_eq!(
                s.symbol.full_width_chars,
                symbol::default_full_width_chars(),
                "container {bad:?}"
            );
            assert!(s.symbol.full_width);
            assert!(!s.number.full_width);
        }
    }

    #[test]
    fn out_of_scope_char_is_preserved_in_set_but_ineffective() {
        // `-`（対象外）だけの1文字要素は捨てずに保持してよい（将来対象が広がった場合に活きる）。
        let s = Settings::from_json_str(
            r#"{"version":2,"symbol":{"full_width":true,"full_width_chars":["-"]}}"#,
        );
        assert!(s.symbol.full_width_chars.contains(&'-'));
        assert!(s.symbol.effective_chars().is_empty());
        assert!(!s.symbol.symbol_overlay());
    }

    #[test]
    fn effective_chars_and_symbol_overlay_four_cases() {
        // 全29 → overlay=true
        let mut all = Settings::default();
        all.symbol.full_width = true;
        assert!(!all.symbol.effective_chars().is_empty());
        assert!(all.symbol.symbol_overlay());

        // 空集合 → overlay=false
        let mut empty = Settings::default();
        empty.symbol.full_width = true;
        empty.symbol.full_width_chars = BTreeSet::new();
        assert!(empty.symbol.effective_chars().is_empty());
        assert!(!empty.symbol.symbol_overlay());

        // 非空だが実効ゼロ（対象外文字のみ）→ overlay=false
        let mut ineffective = Settings::default();
        ineffective.symbol.full_width = true;
        ineffective.symbol.full_width_chars = BTreeSet::from(['-']);
        assert!(!ineffective.symbol.full_width_chars.is_empty());
        assert!(ineffective.symbol.effective_chars().is_empty());
        assert!(!ineffective.symbol.symbol_overlay());

        // full_width=false → 実効集合が非空でも常に false
        let off = Settings::default();
        assert!(!off.symbol.effective_chars().is_empty());
        assert!(!off.symbol.symbol_overlay());
    }

    #[test]
    fn shift_latin_defaults_to_compose_and_roundtrips() {
        // 既定は compose（英語未確定モード=MS-IME系。変更要望の起点がこの挙動への期待だった）。
        let s = Settings::default();
        assert_eq!(s.shift_latin.mode, "compose");
        // フィールド欠落の旧 settings.json も compose へ（後方互換）。
        let js = r#"{"version":1}"#;
        assert_eq!(Settings::from_json_str(js).shift_latin.mode, "compose");
        // roundtrip（commit も往復する）。
        let mut commit = s.clone();
        commit.shift_latin.mode = "commit".into();
        let back = Settings::from_json_str(&commit.to_json());
        assert_eq!(back.shift_latin.mode, "commit");
    }

    #[test]
    fn ephemeral_defaults_and_old_json_compat() {
        assert!(Settings::default().ephemeral.enabled);
        assert_eq!(Settings::default().ephemeral.trigger, "f8");
        // ephemeral フィールドを欠く旧 JSON も既定で埋まる（#[serde(default)]）。
        // version は #[serde(default)] が無い必須フィールドなので他の後方互換テストと
        // 同じく明示する（欠くと version 必須で from_str 自体が失敗する）。
        let old = r#"{"version":1,"number":{"full_width":true}}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert!(s.ephemeral.enabled);
        assert_eq!(s.ephemeral.trigger, "f8");
    }

    #[test]
    fn typo_correct_defaults_to_enabled_and_resolves_env() {
        // 既定 ON（settings.json 欠落フィールドは true/true でロード — 後方互換）。
        let s = Settings::default();
        assert!(s.typo_correct.enabled);
        assert!(s.typo_correct.learn);
        let js = r#"{"version":1}"#; // typo_correct フィールド無しの旧 settings.json
        let loaded = Settings::from_json_str(js);
        assert!(loaded.typo_correct.enabled);
        assert!(loaded.typo_correct.learn);

        // resolve_env_map: 常に NOSPACEKEY_TYPO_LEARN を注入（NOSPACEKEY_LEARNING と同じ「常時 put」）。
        let env = resolve_env_map(&s, None, |_| None);
        assert!(env
            .iter()
            .any(|(k, v)| k == "NOSPACEKEY_TYPO_LEARN" && v == "1"));
        let mut off = s.clone();
        off.typo_correct.learn = false;
        let env = resolve_env_map(&off, None, |_| None);
        assert!(env
            .iter()
            .any(|(k, v)| k == "NOSPACEKEY_TYPO_LEARN" && v == "0"));
        // D6: ユーザーが env で明示 override していれば注入しない。
        let env = resolve_env_map(&s, None, |k| {
            (k == "NOSPACEKEY_TYPO_LEARN").then(|| "0".into())
        });
        assert!(!env.iter().any(|(k, _)| k == "NOSPACEKEY_TYPO_LEARN"));
    }

    #[test]
    fn appearance_roundtrips_through_json() {
        let mut s = Settings::default();
        s.appearance.theme = "dark".into();
        s.appearance.palette_light.text = "#123456".into();
        let back = Settings::from_json_str(&serde_json::to_string(&s).unwrap());
        assert_eq!(back.appearance.theme, "dark");
        assert_eq!(back.appearance.palette_light.text, "#123456");
    }

    #[test]
    fn keymap_defaults_to_all_none_and_old_json_loads() {
        // 旧 settings.json(keymap フィールドなし)は全機能「既定」でロード(後方互換)。
        let s = Settings::from_json_str(r#"{"version":1}"#);
        for f in keymap::ALL_FUNCS {
            assert_eq!(*s.keymap.get(f), None, "{f:?} は既定のはず");
        }
    }

    // ---- v1→v2 移行: 既定パレット刷新（Apple 風トークン化）の引き上げ ----
    // #[serde(default)] はフィールド欠落時しか効かず、設定アプリで一度でも保存した
    // settings.json には旧内蔵既定色が具体値で焼き付いている。移行が無いと、色を
    // カスタマイズしていないユーザーに新既定が未来永劫反映されない。

    const V1_BUILTIN_LIGHT: &str = r##"{"bg":"#FAFAFA","text":"#202020","index":"#A0A0A0","sel_bg":"#0078D7","sel_text":"#FFFFFF","sel_index":"#C8DCF0","border":"#E0E0E0"}"##;
    const V1_BUILTIN_DARK: &str = r##"{"bg":"#2B2B2B","text":"#F0F0F0","index":"#7A7A7A","sel_bg":"#0078D7","sel_text":"#FFFFFF","sel_index":"#1E3A5F","border":"#3C3C3C"}"##;

    fn json_with_palettes(version: u32, light: &str, dark: &str) -> String {
        format!(
            r#"{{"version":{version},"appearance":{{"theme":"auto","palette_light":{light},"palette_dark":{dark}}}}}"#
        )
    }

    #[test]
    fn v1_builtin_default_palettes_migrate_to_new_defaults() {
        let s = Settings::from_json_str(&json_with_palettes(1, V1_BUILTIN_LIGHT, V1_BUILTIN_DARK));
        assert_eq!(s.appearance.palette_light, default_light_palette());
        assert_eq!(s.appearance.palette_dark, default_dark_palette());
        assert_eq!(s.version, 2);
    }

    #[test]
    fn v1_customized_palette_survives_migration() {
        // light は 1 色でも変えていれば意図的カスタム＝丸ごと温存。dark は旧既定のまま＝引き上げ。
        let custom_light = V1_BUILTIN_LIGHT.replace("#FAFAFA", "#123456");
        let s = Settings::from_json_str(&json_with_palettes(1, &custom_light, V1_BUILTIN_DARK));
        assert_eq!(s.appearance.palette_light.bg, "#123456");
        assert_eq!(s.appearance.palette_light.text, "#202020");
        assert_eq!(s.appearance.palette_dark, default_dark_palette());
        assert_eq!(s.version, 2);
    }

    #[test]
    fn v2_palettes_matching_old_defaults_are_left_alone() {
        // 移行済み(v2)で旧既定と同じ色を選び直した場合はユーザーの選択＝二度と触らない。
        let s = Settings::from_json_str(&json_with_palettes(2, V1_BUILTIN_LIGHT, V1_BUILTIN_DARK));
        assert_eq!(s.appearance.palette_light.bg, "#FAFAFA");
        assert_eq!(s.appearance.palette_dark.bg, "#2B2B2B");
        assert_eq!(s.version, 2);
    }

    #[test]
    fn v1_without_appearance_migrates_to_v2_with_new_defaults() {
        // appearance フィールド欠落の旧 settings.json は serde default で既に新既定。
        // version だけ 2 へ引き上げ、以後の保存で移行済みと分かるようにする。
        let s = Settings::from_json_str(r#"{"version":1}"#);
        assert_eq!(s.appearance.palette_light, default_light_palette());
        assert_eq!(s.version, 2);
    }

    #[test]
    fn reading_monitor_defaults_to_enabled_and_roundtrips() {
        // 既定 ON（ライブ変換の読み可視化は標準体験。欠落フィールドの旧 settings.json も ON でロード）。
        let s = Settings::default();
        assert!(s.reading_monitor.enabled, "既定は ON");
        let js = r#"{"version":1}"#; // reading_monitor フィールド無しの旧 settings.json
        assert!(Settings::from_json_str(js).reading_monitor.enabled);
        // roundtrip（OFF も往復する）。
        let mut off = s.clone();
        off.reading_monitor.enabled = false;
        let back = Settings::from_json_str(&off.to_json());
        assert!(!back.reading_monitor.enabled);
    }

    #[test]
    fn reading_monitor_accumulate_defaults_to_on_and_roundtrips() {
        // 自動確定をまたぐ読み累積は読みモニタの標準体験(spec 2026-07-21 cache-and-anchor)。
        let s = Settings::default();
        assert!(s.reading_monitor.accumulate, "既定は ON");
        // accumulate フィールド無しの旧 settings.json も ON でロード(後方互換)。
        let js = r#"{"version":1,"reading_monitor":{"enabled":true}}"#;
        assert!(Settings::from_json_str(js).reading_monitor.accumulate);
        // OFF も往復する。
        let mut off = s.clone();
        off.reading_monitor.accumulate = false;
        assert!(
            !Settings::from_json_str(&off.to_json())
                .reading_monitor
                .accumulate
        );
    }

    #[test]
    fn reading_monitor_max_chars_defaults_and_clamps() {
        // 既定34 = 従来の固定幅480dp(全角34文字相当)の見た目を保存(spec 決定事項)。
        let s = Settings::default();
        assert_eq!(s.reading_monitor.max_chars, 34);
        // フィールド欠落の旧 settings.json も 34 でロード。
        let js = r#"{"version":1,"reading_monitor":{"enabled":true}}"#;
        assert_eq!(Settings::from_json_str(js).reading_monitor.max_chars, 34);
        // roundtrip。
        let mut m = s.clone();
        m.reading_monitor.max_chars = 50;
        assert_eq!(
            Settings::from_json_str(&m.to_json())
                .reading_monitor
                .max_chars,
            50
        );
        // effective_max_chars は 10..=100 へクランプ(手編集 settings.json への防御)。
        m.reading_monitor.max_chars = 9;
        assert_eq!(m.reading_monitor.effective_max_chars(), 10);
        m.reading_monitor.max_chars = 34;
        assert_eq!(m.reading_monitor.effective_max_chars(), 34);
        m.reading_monitor.max_chars = 101;
        assert_eq!(m.reading_monitor.effective_max_chars(), 100);
    }

    #[test]
    fn user_dictionary_defaults_enabled_on_missing_field() {
        let s: Settings = serde_json::from_str(r#"{"version":2}"#).unwrap();
        assert!(s.user_dictionary.enabled); // 旧 settings.json で辞書が死なない
        assert!(UserDictionarySettings::default().enabled); // derive(Default)化の退行検出
    }

    #[test]
    fn env_map_injects_user_dict_enabled() {
        let mut s = Settings::default();
        s.user_dictionary.enabled = false;
        let m = resolve_env_map(&s, None, |_| None);
        assert!(m
            .iter()
            .any(|(k, v)| k == "NOSPACEKEY_USER_DICT_ENABLED" && v == "0"));
    }

    #[test]
    fn keymap_roundtrips_explicit_none_and_chord() {
        let mut s = Settings::default();
        s.keymap.commit_undo = Some("none".into());
        s.keymap.to_katakana = Some("F11".into());
        let back = Settings::from_json_str(&s.to_json());
        assert_eq!(back.keymap.commit_undo.as_deref(), Some("none"));
        assert_eq!(back.keymap.to_katakana.as_deref(), Some("F11"));
        assert_eq!(back.keymap.mode_toggle, None);
        // 3状態を JSON 上で区別できる: None は null として明示的に書かれる
        // (skip_serializing_if を使わない — 設定アプリの dirty 判定が null/欠落で食い違わないように)。
        assert!(s.to_json().contains(r#""mode_toggle": null"#));
    }

    #[test]
    fn inline_prediction_defaults_off_and_roundtrips() {
        let old = Settings::from_json_str(r#"{"version":2}"#);
        assert!(!old.inline_prediction.enabled);
        let mut enabled = Settings::default();
        enabled.inline_prediction.enabled = true;
        assert!(
            Settings::from_json_str(&enabled.to_json())
                .inline_prediction
                .enabled
        );
        let env = resolve_env_map(&enabled, None, |_| None);
        assert!(env
            .iter()
            .any(|(key, value)| { key == "NOSPACEKEY_INLINE_PREDICTION" && value == "1" }));
    }
}
