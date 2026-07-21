//! SP6b: nospacekey の永続設定。TIP と NospacekeyConfig.exe が共有する。COM/GUI 非依存。
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub mod dpapi;
pub mod keymap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmSettings {
    pub enabled: bool,
    #[serde(default)] pub api_key_dpapi: String, // DPAPI blob の base64。空=未設定。
    #[serde(default)] pub endpoint: String,
    pub model: String,
    #[serde(default)] pub prompt: String,
    pub timeout_ms: u32,
}
impl Default for LlmSettings {
    fn default() -> Self {
        Self { enabled: false, api_key_dpapi: String::new(), endpoint: String::new(),
               model: "gpt-4o-mini".into(), prompt: String::new(), timeout_ms: 15000 }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZenzaiSettings { pub enabled: bool, #[serde(default)] pub weight_path: String }
impl Default for ZenzaiSettings { fn default() -> Self { Self { enabled: true, weight_path: String::new() } } }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveSettings { pub enabled: bool }
impl Default for LiveSettings { fn default() -> Self { Self { enabled: true } } }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningSettings { pub enabled: bool }
impl Default for LearningSettings { fn default() -> Self { Self { enabled: true } } }

/// 品質ループ③: 誤変換ワンキー記録（Ctrl+変換 → feedback.jsonl）。**既定 OFF＝opt-in**
/// （NOSPACEKEY_LOG の診断ログとは独立の opt-in — 既定状態で新規に書かれるものはゼロ）。
/// `enabled: false` が既定なので Default は derive（clippy::derivable_impls）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeedbackSettings { pub enabled: bool }

/// かな入力モードで数字を既定で全角確定するか。既定 true（全角）。いつでも設定で切替可能。
/// 候補を明示選択した確定は幅を変えない（既定確定のみ全角化）。LiveSettings と同じく既定が
/// true なので Default は手書き（derive だと false になる）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NumberSettings { pub full_width: bool }
impl Default for NumberSettings { fn default() -> Self { Self { full_width: true } } }

/// かな入力モードの句読点既定幅（true=全角 、。／false=半角 ,.）。既定 true なので
/// Default は手書き（derive だと false になる）。NumberSettings と同じ流儀（設計 §E）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PunctuationSettings { pub full_width: bool }
impl Default for PunctuationSettings { fn default() -> Self { Self { full_width: true } } }

/// かな入力モードの記号既定幅（true=全角 ・「」！？～：；等／false=半角 ASCII）。既定 false。
/// Number/Punctuation と違い既定が false なので Default は derive で足りる。
/// `,` `.`（punctuation の領分）と `-`→ー（長音符=かな）はこのトグルの対象外。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SymbolSettings { pub full_width: bool }

/// 一時的なかなモード（トリガキーで一時的にかな入力へ入り、確定で自動的に半角英数へ戻る）。
/// ターミナル/vim 向けに「日本語モードの抜け忘れ」を防ぐ。既定 ON・トリガは F8。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EphemeralSettings { pub enabled: bool, pub trigger: String }
impl Default for EphemeralSettings {
    fn default() -> Self { Self { enabled: true, trigger: "f8".into() } }
}

/// 修正変換(Tab): 読みのタイポ修復候補を提示する。`learn` は修復候補確定時の
/// 誤読み学習(合成ペア — engine env NOSPACEKEY_TYPO_LEARN)。両方とも既定 ON。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypoCorrectSettings { pub enabled: bool, pub learn: bool }
impl Default for TypoCorrectSettings { fn default() -> Self { Self { enabled: true, learn: true } } }

/// Shift+英字の挙動。"compose"=英語未確定モード(確定まで英字が続く・MS-IME系・既定) /
/// "commit"=大文字を直接確定(Google/ATOK系・e0beaf3 の旧既定)。bool でなく文字列 enum
/// なのは appearance.backdrop/ephemeral.trigger と同じ将来拡張余地のため。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShiftLatinSettings {
    #[serde(default = "default_shift_latin_mode")]
    pub mode: String,
}
fn default_shift_latin_mode() -> String { "compose".into() }
impl Default for ShiftLatinSettings {
    fn default() -> Self { Self { mode: default_shift_latin_mode() } }
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
    fn default() -> Self { Self { enabled: true, accumulate: true, max_chars: 34 } }
}
impl ReadingMonitorSettings {
    /// 10..=100 へクランプ。config の apply と tip の Activate 読みの両方が通る
    /// 唯一の正規化点（境界定数をここ以外に書かない）。
    pub fn effective_max_chars(&self) -> u32 {
        self.max_chars.clamp(10, 100)
    }
}

fn default_true() -> bool { true }
fn default_max_chars() -> u32 { 34 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub version: u32,
    #[serde(default)] pub llm: LlmSettings,
    #[serde(default)] pub zenzai: ZenzaiSettings,
    #[serde(default)] pub live_conversion: LiveSettings,
    /// Spec2: かな漢字変換の学習（確定候補を以後の順位に反映）。既定 ON。
    /// engine env `NOSPACEKEY_LEARNING`（"1"/"0"）へ resolve_env_map が常に注入する。
    #[serde(default)] pub learning: LearningSettings,
    /// SP7: 真なら IME 活性化時に conversion-mode を半角英数(直接入力)へ初期化する
    /// （ターミナル/Vim 向け）。既定 false＝従来どおりひらがな既定。TIP 側の挙動で
    /// engine env には注入しない。フィールド欠落の旧 settings.json は false でロード。
    #[serde(default)] pub default_direct: bool,
    /// A 段: 外観（配色/フォント/角丸/バックドロップ）。欠落は既定 Appearance。
    #[serde(default)] pub appearance: Appearance,
    /// 品質ループ③: 誤変換ワンキー記録（feedback.jsonl）。既定 false=opt-in。
    /// フィールド欠落の旧 settings.json は false でロード（後方互換）。
    #[serde(default)] pub feedback: FeedbackSettings,
    /// かな入力モードの数字既定幅（true=全角）。欠落の旧 settings.json は true でロード。
    #[serde(default)] pub number: NumberSettings,
    /// かな入力モードの句読点既定幅（true=全角）。欠落の旧 settings.json は true でロード。
    #[serde(default)] pub punctuation: PunctuationSettings,
    /// かな入力モードの記号既定幅（false=半角 ASCII）。欠落の旧 settings.json は false でロード。
    #[serde(default)] pub symbol: SymbolSettings,
    /// 一時的なかなモード（トリガキーで一時的にかな入力へ、確定で自動的に半角英数へ戻る）。
    /// 欠落の旧 settings.json は既定（enabled=true, trigger="f8"）でロード。
    #[serde(default)] pub ephemeral: EphemeralSettings,
    /// 修正変換(Tab): 読みのタイポ修復候補。欠落の旧 settings.json は既定
    /// （enabled=true, learn=true）でロード。
    #[serde(default)] pub typo_correct: TypoCorrectSettings,
    /// Shift+英字の挙動（"compose"=英語未確定モード / "commit"=大文字直接確定）。
    /// 欠落の旧 settings.json は "compose" でロード。TIP ローカル設定（engine env 非注入）。
    #[serde(default)] pub shift_latin: ShiftLatinSettings,
    /// 読みモニタ（ライブ変換中の生読み常時表示）。欠落の旧 settings.json は ON でロード。
    #[serde(default)] pub reading_monitor: ReadingMonitorSettings,
    /// configurable keymap: コマンド系 12 機能のキー割り当て(spec 2026-07-16)。
    /// 欠落の旧 settings.json は全機能「既定」でロード。反映は Activate 時 1 回(D7)。
    #[serde(default)] pub keymap: keymap::KeymapSettings,
}
impl Default for Settings {
    fn default() -> Self { Self { version: 2, llm: Default::default(), zenzai: Default::default(), live_conversion: Default::default(), learning: Default::default(), default_direct: false, appearance: Default::default(), feedback: Default::default(), number: Default::default(), punctuation: Default::default(), symbol: Default::default(), ephemeral: EphemeralSettings::default(), typo_correct: Default::default(), shift_latin: Default::default(), reading_monitor: Default::default(), keymap: Default::default() } }
}

/// A 段の外観設定。全フィールド `#[serde(default)]` で後方互換（欠落フィールドは既定へ）。
/// 色は人間が編集できる `#RRGGBB` 文字列で保存する（カスタムテーマ MVP）。
/// パース失敗は Theme 解決層でフィールド単位に既定へフォールバックする（ここでは文字列のまま保持）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Palette {
    #[serde(default)] pub bg: String,
    #[serde(default)] pub text: String,
    #[serde(default)] pub index: String,
    #[serde(default)] pub sel_bg: String,
    #[serde(default)] pub sel_text: String,
    #[serde(default)] pub sel_index: String,
    #[serde(default)] pub border: String,
}

/// 内蔵ライトパレット。Apple 風 UI パターン集（docs/apple-design-ui-patterns.md）の
/// ライトトークン由来: panel #FFFFFF / text #1D1D1F / text-sub #86868B /
/// accent(systemBlue) #0071E3。border は rgba(0,0,0,.12) を白地に合成した #E0E0E0、
/// sel_index はアクセント地に白 70% を合成した淡青（GDI が不透明色しか扱えないため事前合成）。
pub fn default_light_palette() -> Palette {
    Palette {
        bg: "#FFFFFF".into(), text: "#1D1D1F".into(), index: "#86868B".into(),
        sel_bg: "#0071E3".into(), sel_text: "#FFFFFF".into(), sel_index: "#B3D4F7".into(),
        border: "#E0E0E0".into(),
    }
}

/// 内蔵ダークパレット。同トークンのダーク側: panel #2C2C2E / text #F5F5F7 /
/// text-sub #98989D / accent(systemBlue) #0A84FF。border は rgba(255,255,255,.16) を
/// #2C2C2E 地に合成した #4E4E4F、sel_index はアクセント地に白 70% を合成した淡青。
pub fn default_dark_palette() -> Palette {
    Palette {
        bg: "#2C2C2E".into(), text: "#F5F5F7".into(), index: "#98989D".into(),
        sel_bg: "#0A84FF".into(), sel_text: "#FFFFFF".into(), sel_index: "#B6DAFF".into(),
        border: "#4E4E4F".into(),
    }
}

impl Default for Palette {
    fn default() -> Self { default_light_palette() }
}

/// v1 時代の内蔵ライトパレット（既定刷新前の焼き付き値の検出専用。新規利用禁止）。
fn legacy_v1_light_palette() -> Palette {
    Palette {
        bg: "#FAFAFA".into(), text: "#202020".into(), index: "#A0A0A0".into(),
        sel_bg: "#0078D7".into(), sel_text: "#FFFFFF".into(), sel_index: "#C8DCF0".into(),
        border: "#E0E0E0".into(),
    }
}

/// v1 時代の内蔵ダークパレット（同上）。
fn legacy_v1_dark_palette() -> Palette {
    Palette {
        bg: "#2B2B2B".into(), text: "#F0F0F0".into(), index: "#7A7A7A".into(),
        sel_bg: "#0078D7".into(), sel_text: "#FFFFFF".into(), sel_index: "#1E3A5F".into(),
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
    if s.version < 2 {
        if s.appearance.palette_light == legacy_v1_light_palette() {
            s.appearance.palette_light = default_light_palette();
        }
        if s.appearance.palette_dark == legacy_v1_dark_palette() {
            s.appearance.palette_dark = default_dark_palette();
        }
        s.version = 2;
    }
    s
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Appearance {
    /// "auto" | "light" | "dark" | "custom"。
    #[serde(default = "appearance_theme_default")] pub theme: String,
    /// "acrylic" | "opaque"。
    #[serde(default = "appearance_backdrop_default")] pub backdrop: String,
    #[serde(default = "appearance_font_family_default")] pub font_family: String,
    #[serde(default = "appearance_font_point_default")] pub font_point: f32,
    /// "round" | "square"。
    #[serde(default = "appearance_corner_default")] pub corner: String,
    #[serde(default = "default_light_palette")] pub palette_light: Palette,
    #[serde(default = "default_dark_palette")] pub palette_dark: Palette,
}

fn appearance_theme_default() -> String { "auto".into() }
fn appearance_backdrop_default() -> String { "acrylic".into() }
fn appearance_font_family_default() -> String { "Yu Gothic UI".into() }
fn appearance_font_point_default() -> f32 { 10.5 }
fn appearance_corner_default() -> String { "round".into() }

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
    pub fn from_json_str(s: &str) -> Settings { serde_json::from_str(s).map(migrate).unwrap_or_default() }
    pub fn to_json(&self) -> String { serde_json::to_string_pretty(self).unwrap_or_default() }
}

/// %LOCALAPPDATA%\nospacekey\settings.json。無ければ None（呼び元は既定で劣化）。
pub fn settings_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("nospacekey").join("settings.json"))
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
    /// JSON 破損（原本は退避）。既定で劣化。
    Corrupt,
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

/// 読み取り結果とその要因を返す（`load()` の実体）。UU-7: 呼び出し側が失敗要因を診断できる。
pub fn load_reporting() -> (Settings, LoadOutcome) {
    let Some(path) = settings_path() else { return (Settings::default(), LoadOutcome::NoPath); };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return (Settings::default(), classify_read_error(e.kind())),
    };
    // 空（0バイト/空白のみ）ファイルは、書き込み途中で中断された torn write の典型的痕跡で、
    // 保全すべき鍵を含まない。これを「破損」として .corrupt へ退避するのは無駄なクラッタを生むので、
    // ここでは退避せず既定へ劣化する（後続の save() が正規の内容で上書きする）。save() は原子的 rename
    // を使うので、行儀のよい writer 経由なら load() が torn write（途中状態）を観測することはない。
    if text.trim().is_empty() {
        return (Settings::default(), LoadOutcome::Empty);
    }
    match serde_json::from_str::<Settings>(&text) {
        Ok(s) => (migrate(s), LoadOutcome::Loaded),
        Err(_) => {
            // 壊れた設定でも全消去（特に DPAPI 暗号化済み API キー）を黙って起こさないよう、
            // 原本を `*.json.corrupt.<unix秒>.<nanos>` へ退避してから既定へ劣化する。退避しておけば
            // 後続の save() が既定値で上書きしても、ユーザは壊れた原本から鍵を手動復旧できる。
            //
            // 退避名は固定の `*.json.corrupt` ではなく一意化する。固定名だと2度目の破損が
            // 1度目の（まだ復旧可能な）退避を上書き破壊してしまうため。time/chrono 依存は
            // 持たないので SystemTime の UNIX 秒＋nanos＋pid から一意サフィックスを作る。
            // さらに、クロック解像度が粗い/連続リトライで同一 tick に当たっても既存退避を
            // 絶対に再利用しないよう、衝突したら連番を付けて未使用名を確定させる（exists チェック）。
            //
            // 失敗ハンドリング: rename を黙って捨ててはならない。rename が失敗したまま既定を返すと、
            // 後続の save() が壊れた原本（＝暗号化鍵）を既定値で上書きして恒久消失させうる。
            // そこで rename 失敗時は copy で退避を試み（原本は所定位置に残るが鍵は保全される）、
            // 両方失敗した場合のみ既定を返しつつ stderr に明確な警告を出す（load() は決して
            // panic せず Settings を返し続ける＝infallible を維持する）。
            let base = {
                let d = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                format!("json.corrupt.{}.{}.{}", d.as_secs(), d.subsec_nanos(), std::process::id())
            };
            // pid を含むのでプロセス間衝突は無く、ここは逐次実行なので exists() チェックで競合しない。
            // 既存退避（rename/copy いずれの上書きも）を避け、必ず未使用の退避名を選ぶ。
            let mut bak = path.with_extension(&base);
            let mut n = 1u32;
            while bak.exists() {
                bak = path.with_extension(format!("{base}.{n}"));
                n += 1;
            }
            if std::fs::rename(&path, &bak).is_ok() {
                // 原本は一意名で安全に退避済み。既定へ劣化してよい。
                return (Settings::default(), LoadOutcome::Corrupt);
            }
            // rename 失敗（ロック/権限/別ボリューム等）。原本を所定位置に残したまま copy で退避を試みる。
            if std::fs::copy(&path, &bak).is_ok() {
                return (Settings::default(), LoadOutcome::Corrupt);
            }
            // rename も copy も失敗。退避できなかったことを明示し、鍵消失リスクを警告する。
            eprintln!(
                "nospacekey settings: 壊れた settings.json を退避できませんでした（{}）。\
                 後続の save() が暗号化済み API キーを上書きする恐れがあります。\
                 手動で原本をバックアップしてください。",
                path.display()
            );
            (Settings::default(), LoadOutcome::Corrupt)
        }
    }
}

/// 親dir作成＋一時ファイル経由の原子的置換。
/// TIP と NospacekeyConfig.exe が同じ settings.json を共有するため、一時ファイル名は
/// プロセス毎に一意化する（固定名だと2プロセス同時 save で書き込み/rename が競合し
/// 片方が NotFound や破損を起こす）。失敗時は残骸 tmp をベストエフォートで掃除する。
pub fn save(s: &Settings) -> std::io::Result<()> {
    let path = settings_path().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no LOCALAPPDATA"))?;
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
    // シリアライズ失敗時に settings.json を空ファイルで上書きして破壊しないよう、ここで ?
    // で中断する（to_json は unwrap_or_default で "" に落ちるため save では使わない）。
    let json = serde_json::to_string_pretty(s).map_err(std::io::Error::other)?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, &json) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // UU-7: AppContainer/LPAC ホスト（Start/検索の SearchHost 等）が settings.json を読めるよう、
    // 親ディレクトリと当該ファイルに AppContainer read ACE を付与する（best-effort・プロセス1回）。
    // %LOCALAPPDATA% は AppContainer ACE を継承しないため、付与しないと検索窓でだけ設定が既定へ
    // 劣化する（load() の PermissionDenied を握り潰していた症状）。DLL/pipe の ACE と同じ 2 SID・RX。
    ensure_appcontainer_readable(&path);
    Ok(())
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
    let mut put = |k: &str, v: String| { if env_lookup(k).is_none() { out.push((k.to_string(), v)); } };
    if llm_effective_enabled(s) {
        if let Some(key) = api_key_plain { if !key.is_empty() { put("NOSPACEKEY_LLM_API_KEY", key.to_string()); } }
        if !s.llm.endpoint.is_empty() { put("NOSPACEKEY_LLM_ENDPOINT", s.llm.endpoint.clone()); }
        // endpoint/prompt と同じく空なら注入しない（エンジン側が既定 model へフォールバックする）。
        if !s.llm.model.is_empty() { put("NOSPACEKEY_LLM_MODEL", s.llm.model.clone()); }
        if !s.llm.prompt.is_empty() { put("NOSPACEKEY_LLM_PROMPT", s.llm.prompt.clone()); }
        put("NOSPACEKEY_LLM_TIMEOUT_MS", s.llm.timeout_ms.to_string());
    }
    put("NOSPACEKEY_ZENZAI", if s.zenzai.enabled { "on".into() } else { "off".into() });
    if !s.zenzai.weight_path.is_empty() { put("NOSPACEKEY_ZENZAI_WEIGHT", s.zenzai.weight_path.clone()); }
    put("NOSPACEKEY_LEARNING", if s.learning.enabled { "1".into() } else { "0".into() });
    put("NOSPACEKEY_TYPO_LEARN", if s.typo_correct.learn { "1".into() } else { "0".into() });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- UU-7: 読み取り失敗の分類 ----
    #[test]
    fn classify_read_error_distinguishes_permission_denied() {
        use std::io::ErrorKind;
        assert_eq!(classify_read_error(ErrorKind::NotFound), LoadOutcome::Missing);
        // AppContainer/LPAC ホストからの読み取り拒否は独立要因（診断できるように）。
        assert_eq!(classify_read_error(ErrorKind::PermissionDenied), LoadOutcome::PermissionDenied);
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
    fn default_direct_defaults_false() {
        // SP7: 既定はひらがな（従来挙動を保持）。
        assert!(!Settings::default().default_direct);
    }
    #[test]
    fn default_direct_roundtrip() {
        let s = Settings { default_direct: true, ..Default::default() };
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
        // 退避されること（黙って既定で潰されないこと）を確認する。LOCALAPPDATA を触る
        // テストはこれだけなので並行実行でも env 競合しない。
        let base = std::env::temp_dir().join(format!("nospacekey-settings-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::env::set_var("LOCALAPPDATA", &base);

        let mut s = Settings::default();
        s.llm.enabled = true;
        s.llm.api_key_dpapi = "blob".into();
        save(&s).expect("save ok");
        let loaded = load();
        assert!(loaded.llm.enabled);
        assert_eq!(loaded.llm.api_key_dpapi, "blob");

        // 壊れた JSON を書いてから load → 既定に劣化しつつ原本は *.json.corrupt.* へ退避。
        // 退避名は一意化（タイムスタンプ＋pid）されるので固定名ではなく、ディレクトリを走査して
        // `settings.json.corrupt` 始まりのファイルが存在することを確認する。
        let path = settings_path().unwrap();
        let dir = path.parent().unwrap().to_path_buf();
        let count_backups = || {
            std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("settings.json.corrupt")
                })
                .count()
        };

        std::fs::write(&path, "{ broken json ").unwrap();
        let after = load();
        assert!(!after.llm.enabled); // 既定へ劣化
        assert!(count_backups() >= 1); // 原本は一意名で退避済み

        // 2度目の破損 → 2つ目の別退避が生まれ、1度目の退避が上書き破壊されないこと。
        // 退避名は nanos/pid に加え、衝突時は連番でずらして必ず未使用名を選ぶので、
        // 同一クロック tick に2度当たっても 2 件目が確実に増える（再利用・上書きしない）。
        std::fs::write(&path, "{ broken json again ").unwrap();
        let _ = load();
        assert!(count_backups() >= 2); // 1度目の退避は残り、別ファイルとして2件目が増える

        // 空ファイル（torn write の痕跡）は破損退避せず既定へ劣化する（.corrupt を増やさない）。
        let before_empty = count_backups();
        std::fs::write(&path, "").unwrap();
        let after_empty = load();
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
        assert_eq!(parse_hex_color("#FFF"), None);   // 3 桁は非対応
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
        assert!(env.iter().any(|(k, v)| k == "NOSPACEKEY_LEARNING" && v == "1"));
        let mut off = s.clone();
        off.learning.enabled = false;
        let env = resolve_env_map(&off, None, |_| None);
        assert!(env.iter().any(|(k, v)| k == "NOSPACEKEY_LEARNING" && v == "0"));
        // D6: ユーザーが env で明示 override していれば注入しない。
        let env = resolve_env_map(&s, None, |k| (k == "NOSPACEKEY_LEARNING").then(|| "0".into()));
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
        assert!(env.iter().any(|(k, v)| k == "NOSPACEKEY_TYPO_LEARN" && v == "1"));
        let mut off = s.clone();
        off.typo_correct.learn = false;
        let env = resolve_env_map(&off, None, |_| None);
        assert!(env.iter().any(|(k, v)| k == "NOSPACEKEY_TYPO_LEARN" && v == "0"));
        // D6: ユーザーが env で明示 override していれば注入しない。
        let env = resolve_env_map(&s, None, |k| (k == "NOSPACEKEY_TYPO_LEARN").then(|| "0".into()));
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
        assert!(!Settings::from_json_str(&off.to_json()).reading_monitor.accumulate);
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
        assert_eq!(Settings::from_json_str(&m.to_json()).reading_monitor.max_chars, 50);
        // effective_max_chars は 10..=100 へクランプ(手編集 settings.json への防御)。
        m.reading_monitor.max_chars = 9;
        assert_eq!(m.reading_monitor.effective_max_chars(), 10);
        m.reading_monitor.max_chars = 34;
        assert_eq!(m.reading_monitor.effective_max_chars(), 34);
        m.reading_monitor.max_chars = 101;
        assert_eq!(m.reading_monitor.effective_max_chars(), 100);
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
}
