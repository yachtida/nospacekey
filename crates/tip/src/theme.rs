//! A 段: `settings::Appearance` から候補ウィンドウ/HUD が直接使える「解決済み」テーマを組む。
//!
//! ここは COM 非依存の純ロジック（色解決・α 適用・GDI/D2D 両表現の生成）。ダークモード判定の
//! レジストリ読みは text_service 側で行い、その結果 `is_dark: bool` を受け取る（テスト容易化）。
//!
//! 設計:
//! - light/dark 選択: theme=="dark" or (theme=="auto" && is_dark) で dark、それ以外は light。
//!   "custom" は light スロットを採用（C 段のカスタム編集 UI が light を編集する運用）。
//! - 色文字列は `#RRGGBB`。パース失敗は**フィールド単位**で内蔵既定へフォールバック（起動不能にしない）。
//! - アクリル時は背景 bg だけを半透明にし、前景/アクセント/枠は不透明のまま。
//! - D2D 用の色は premultiplied-alpha（swapchain が premultiplied のため）。

use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;

/// docs/apple-design-ui-patterns.md のトークン表と対応する TIP 側の定数（文書が正）。
/// GUI(style.css) と同じ語彙を GDI/D2D 描画へ持ち込むための単一の置き場。
/// 色は settings の内蔵既定パレット側（同トークン由来）が持つのでここには置かない。
pub mod tokens {
    /// 角丸 3 段階（dp）。--radius-lg / --radius-md / --radius-sm。
    /// カード外形の角丸は DWM(DWMWCP_ROUND) が握るため、コード側で使うのは主に SM
    /// （選択ハイライトのピル）。LG/MD は将来の面（シート状 UI 等）用に表を写しておく。
    #[allow(dead_code)]
    pub const RADIUS_LG: i32 = 12;
    #[allow(dead_code)]
    pub const RADIUS_MD: i32 = 8;
    pub const RADIUS_SM: i32 = 6;
    /// 出現フェードの時間（ms）。--ease-snap 相当のイーズアウトで駆動する。
    pub const MOTION_IN_MS: f64 = 140.0;
    /// 退場フェードの時間（ms）。出現と対称の経路（来た道を戻る）。
    pub const MOTION_OUT_MS: f64 = 120.0;
}

/// 背景 bg に適用するアクリル時のアルファ（0..255）。~0.7。spec の 0.6–0.8 域。
const ACRYLIC_BG_ALPHA: u8 = 179; // 0.70 * 255 ≈ 179

/// 8bit RGBA。GDI(COLORREF)/D2D(premultiplied f32) 両方へ変換できる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba { pub r: u8, pub g: u8, pub b: u8, pub a: u8 }

impl Rgba {
    /// GDI 用 COLORREF（0x00BBGGRR）。α は捨てる（GDI パスは不透明前提）。
    pub fn colorref(&self) -> u32 {
        (self.b as u32) << 16 | (self.g as u32) << 8 | (self.r as u32)
    }
    /// D2D 用 premultiplied-alpha の 0..1 正規化色。
    pub fn d2d(&self) -> D2D1_COLOR_F {
        let a = self.a as f32 / 255.0;
        D2D1_COLOR_F {
            r: (self.r as f32 / 255.0) * a,
            g: (self.g as f32 / 255.0) * a,
            b: (self.b as f32 / 255.0) * a,
            a,
        }
    }
}

/// 解決済みの 7 色。
#[derive(Debug, Clone, Copy)]
pub struct ThemeColors {
    pub bg: Rgba, pub text: Rgba, pub index: Rgba,
    pub sel_bg: Rgba, pub sel_text: Rgba, pub sel_index: Rgba, pub border: Rgba,
}

/// 候補ウィンドウ/HUD が共有する解決済みテーマ。
#[derive(Debug, Clone)]
pub struct Theme {
    pub colors: ThemeColors,
    pub font_family: String,
    /// ポイントサイズ×10（create_font/font_height_for_dpi と同じ表現）。
    pub font_point_tenths: i32,
    pub rounded: bool,
    pub acrylic: bool,
    /// 出現/退場フェードを使ってよいか。resolve は常に true で返し、`AppearanceSource`
    /// が OS の「アニメーション効果」設定（SPI_GETCLIENTAREAANIMATION）で上書きする
    /// （GUI の prefers-reduced-motion 対応と同じ発想。移動系を消し即時表示へ劣化）。
    pub motion: bool,
    /// 解決結果が dark だったか。現状は resolve のテスト検証と将来の B/C 段
    /// （設定 UI プレビュー等）向けの公開情報で、描画パス自体は colors を直接使う。
    #[allow(dead_code)]
    pub is_dark: bool,
}

/// hex をパースし、失敗したら内蔵既定 (r,g,b) へフォールバック。
fn color_or(hex: &str, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    settings::parse_hex_color(hex).unwrap_or(fallback)
}

impl Theme {
    pub fn resolve(app: &settings::Appearance, is_dark: bool) -> Theme {
        let use_dark = app.theme == "dark" || (app.theme == "auto" && is_dark);
        let pal = if use_dark { &app.palette_dark } else { &app.palette_light };
        // 対応する内蔵既定（フィールド単位フォールバック用）。
        let def = if use_dark { settings::default_dark_palette() } else { settings::default_light_palette() };
        let acrylic = app.backdrop == "acrylic";
        let bg_alpha = if acrylic { ACRYLIC_BG_ALPHA } else { 255 };

        let mk = |hex: &str, def_hex: &str, alpha: u8| -> Rgba {
            let drgb = settings::parse_hex_color(def_hex).unwrap_or((0, 0, 0));
            let (r, g, b) = color_or(hex, drgb);
            Rgba { r, g, b, a: alpha }
        };

        let colors = ThemeColors {
            bg: mk(&pal.bg, &def.bg, bg_alpha),
            text: mk(&pal.text, &def.text, 255),
            index: mk(&pal.index, &def.index, 255),
            sel_bg: mk(&pal.sel_bg, &def.sel_bg, 255),
            sel_text: mk(&pal.sel_text, &def.sel_text, 255),
            sel_index: mk(&pal.sel_index, &def.sel_index, 255),
            border: mk(&pal.border, &def.border, 255),
        };

        Theme {
            colors,
            font_family: if app.font_family.trim().is_empty() { "Yu Gothic UI".into() } else { app.font_family.clone() },
            font_point_tenths: (app.font_point * 10.0).round() as i32,
            rounded: app.corner != "square",
            acrylic,
            motion: true,
            is_dark: use_dark,
        }
    }
}

impl Default for Theme {
    /// 既定 Appearance を light で解決した初期値。ウィンドウ構築時のプレースホルダ用
    /// （実際の表示前に show/flash が settings 由来の Theme で必ず上書きするので、
    /// この値がそのまま描画されることはない）。
    fn default() -> Self { Theme::resolve(&settings::Appearance::default(), false) }
}

// ============================================================================
// A 段 (Task 7): 表示ごとの外観再読込＆ダーク再評価。ポップアップは短命なので on-show 判定で
// 十分（常駐監視スレッドは入れない=YAGNI。IME は STA なので余計なスレッドを持たない方が安全）。
// 変換中は候補更新=show が実質打鍵ごとに走るが、per-show のコストは mtime stat 1 回
//（＋SPI/条件付きレジストリ read）に抑え、フルパース IO は mtime 変化時だけにする。
// ============================================================================

use std::time::SystemTime;

/// settings.json の mtime を見て再読込すべきか。初回(cached=None かつ current=Some)か、
/// mtime が変わったときだけ true。ファイル消失(current=None)は前回値維持で false
/// （消失のたびに既定へ戻すとエディタの保存方式によっては一瞬のちらつきになるため）。
pub fn should_reload(current: Option<SystemTime>, cached: Option<SystemTime>) -> bool {
    match (current, cached) {
        (Some(_), None) => true,
        (Some(c), Some(p)) => c != p,
        (None, _) => false,
    }
}

/// settings.json の現在 mtime。ファイルが無い/読めないは None（呼び元は既定で劣化＝panic しない）。
pub fn settings_mtime() -> Option<SystemTime> {
    let path = settings::settings_path()?;
    std::fs::metadata(path).ok()?.modified().ok()
}

/// theme 文字列が OS のダーク判定（レジストリ read）を必要とするか。"auto" のときだけ true。
/// resolve は "light"/"dark"/"custom" では is_dark を無視するので、その場合はレジストリを読まない。
fn needs_registry_dark(theme: &str) -> bool {
    theme == "auto"
}

/// Windows のアプリ配色設定（AppsUseLightTheme）が dark か。==0 で dark。
/// キーが無い/読めない場合は light 扱い（false）で劣化する＝IME 経路では決して panic しない。
pub fn is_dark_from_registry() -> bool {
    windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_u32("AppsUseLightTheme"))
        .map(|v| v == 0)
        .unwrap_or(false)
}

/// Windows の「透明効果」設定（EnableTransparency）が有効か。==0 で無効。
/// GUI の prefers-reduced-transparency 対応に相当し、無効ならアクリルを不透明へ劣化させる。
/// キーが無い/読めない場合は有効扱い（true）＝従来挙動のまま。
pub fn is_transparency_enabled_from_registry() -> bool {
    windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_u32("EnableTransparency"))
        .map(|v| v != 0)
        .unwrap_or(true)
}

/// OS の「アニメーション効果」設定（クライアント領域アニメーション）が有効か。
/// GUI の prefers-reduced-motion 対応に相当し、無効なら出現/退場フェードをスキップする。
/// 読めない場合は有効扱い（true）で劣化する（決して panic しない）。
pub fn os_animations_enabled() -> bool {
    use windows::core::BOOL;
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    let mut enabled = BOOL::from(true);
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut enabled as *mut BOOL as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    ok.map(|()| enabled.as_bool()).unwrap_or(true)
}

/// テーマへ OS のアクセシビリティ設定を合成する純粋関数（テスト可能な中核）。
/// - 透明効果オフ → アクリルを不透明へ（bg の α も 255 へ戻す）。
/// - アニメーションオフ → motion=false（フェードなしの即時表示/非表示）。
pub fn apply_os_accessibility(mut t: Theme, transparency: bool, animations: bool) -> Theme {
    if !transparency && t.acrylic {
        t.acrylic = false;
        t.colors.bg.a = 255;
    }
    t.motion = animations;
    t
}

/// 表示（候補 show / HUD flash）ごとに外観設定を供給する。settings.json の mtime が変化した
/// ときだけ再読込するので、表示のたびに残る IO は mtime stat 1 回だけ
///（変換中の show は実質打鍵ごとに走るため、ここを重くしない）。
pub struct AppearanceSource {
    cached_mtime: Option<SystemTime>,
    cached: settings::Appearance,
}

impl AppearanceSource {
    pub fn new() -> Self {
        // 初期値は内蔵既定。cached_mtime=None なので、ファイルが存在すれば初回の
        // current_appearance() で必ず実ファイルから読み直される。
        Self { cached_mtime: None, cached: settings::Appearance::default() }
    }

    /// mtime を見て必要なら再読込し、現在の Appearance を返す。
    pub fn current_appearance(&mut self) -> settings::Appearance {
        let now = settings_mtime();
        // settings.json が壊れていて `settings::load()` が既定へフォールバックした場合でも、
        // ここで mtime を更新して cached に既定値を保持する。壊れたファイルの mtime が
        // 変わらない限り再読込しない＝表示のたびに同じ壊れたファイルを再パースし続けない
        // （意図的な挙動。次に mtime が変わって初めて再評価する）。
        if should_reload(now, self.cached_mtime) {
            self.cached = settings::load().appearance;
            self.cached_mtime = now;
        }
        self.cached.clone()
    }

    /// レジストリのダーク判定も合わせて解決済み Theme を返す。theme=="auto" はここで
    /// 表示のたびに再評価されるので、OS のライト/ダーク切替に次の表示から追従する。
    pub fn current_theme(&mut self) -> Theme {
        let app = self.current_appearance();
        // レジストリのダーク判定は theme=="auto" のときだけ意味を持つ（resolve は明示的な
        // light/dark/custom では is_dark を無視する）。auto 以外では読みに行かず false を渡す
        // ＝表示のたびのレジストリ read を省く（打鍵経路の無駄 IO を減らす）。
        let is_dark = if needs_registry_dark(&app.theme) {
            is_dark_from_registry()
        } else {
            false
        };
        let t = Theme::resolve(&app, is_dark);
        // OS のアクセシビリティ設定（透明効果/アニメーション）を表示のたびに合成する。
        // どちらも安価な read（レジストリ/SPI）。表示=show は変換中実質打鍵ごとに走るため、
        // 透明効果の read はアクリル時だけに絞る（opaque 設定では読みに行かない）。
        let transparency = if t.acrylic {
            is_transparency_enabled_from_registry()
        } else {
            true
        };
        apply_os_accessibility(t, transparency, os_animations_enabled())
    }
}

impl Default for AppearanceSource {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::Appearance;
    use std::time::{Duration, SystemTime};

    #[test]
    fn should_reload_when_mtime_changes_or_first_time() {
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + Duration::from_secs(10);
        // 初回（cached=None）は現在があるなら reload。
        assert!(should_reload(Some(t0), None));
        // 変わっていれば reload。
        assert!(should_reload(Some(t1), Some(t0)));
        // 同じなら reload しない。
        assert!(!should_reload(Some(t0), Some(t0)));
        // ファイルが消えた（current=None）が cache がある → reload しない（前回値を維持）。
        assert!(!should_reload(None, Some(t0)));
        // 両方 None → reload しない（既定を使い続ける）。
        assert!(!should_reload(None, None));
    }

    #[test]
    fn needs_registry_dark_only_for_auto() {
        // "auto" のときだけレジストリのダーク判定が要る。明示指定は不要。
        assert!(needs_registry_dark("auto"));
        assert!(!needs_registry_dark("light"));
        assert!(!needs_registry_dark("dark"));
        assert!(!needs_registry_dark("custom"));
        assert!(!needs_registry_dark(""));
    }

    #[test]
    fn resolve_light_uses_light_palette_opaque() {
        let mut app = Appearance::default();
        app.theme = "light".into();
        app.backdrop = "opaque".into();
        let t = Theme::resolve(&app, /*is_dark=*/true); // theme=light なので is_dark は無視
        assert_eq!((t.colors.bg.r, t.colors.bg.g, t.colors.bg.b), (0xFF, 0xFF, 0xFF));
        assert_eq!(t.colors.bg.a, 255); // opaque
        assert!(!t.acrylic);
        assert!(t.rounded);
        assert!(!t.is_dark);
    }

    #[test]
    fn resolve_auto_follows_is_dark() {
        let app = Appearance::default(); // theme=auto, acrylic, round
        let dark = Theme::resolve(&app, true);
        assert_eq!((dark.colors.bg.r, dark.colors.bg.g, dark.colors.bg.b), (0x2C, 0x2C, 0x2E));
        assert!(dark.is_dark);
        let light = Theme::resolve(&app, false);
        assert_eq!((light.colors.bg.r, light.colors.bg.g, light.colors.bg.b), (0xFF, 0xFF, 0xFF));
        assert!(!light.is_dark);
    }

    #[test]
    fn acrylic_makes_only_bg_translucent() {
        let app = Appearance::default(); // acrylic
        let t = Theme::resolve(&app, false);
        assert!(t.acrylic);
        assert!(t.colors.bg.a < 255 && t.colors.bg.a > 0); // 背景は半透明
        assert_eq!(t.colors.text.a, 255); // 前景は不透明
        assert_eq!(t.colors.sel_bg.a, 255);
        assert_eq!(t.colors.border.a, 255);
    }

    #[test]
    fn bad_color_string_falls_back_per_field() {
        let mut app = Appearance::default();
        app.theme = "light".into();
        app.palette_light.text = "not-a-color".into(); // 壊れた1フィールドだけ
        app.palette_light.bg = "#010203".into();       // 有効
        let t = Theme::resolve(&app, false);
        // 壊れた text は内蔵 light 既定(#1D1D1F)へフォールバック。
        assert_eq!((t.colors.text.r, t.colors.text.g, t.colors.text.b), (0x1D, 0x1D, 0x1F));
        // 有効な bg はそのまま採用。
        assert_eq!((t.colors.bg.r, t.colors.bg.g, t.colors.bg.b), (0x01, 0x02, 0x03));
    }

    #[test]
    fn colorref_is_bgr_order() {
        // COLORREF は 0x00BBGGRR。#0078D7 → R=0x00 G=0x78 B=0xD7 → 0x00D77800。
        let c = Rgba { r: 0x00, g: 0x78, b: 0xD7, a: 255 };
        assert_eq!(c.colorref(), 0x00D7_7800);
    }

    #[test]
    fn d2d_is_premultiplied_and_normalized() {
        // 不透明白 → (1,1,1,1)。
        let w = Rgba { r: 255, g: 255, b: 255, a: 255 }.d2d();
        assert!((w.r - 1.0).abs() < 1e-3 && (w.a - 1.0).abs() < 1e-3);
        // 半透明(α=128)白 → premultiplied なので r≈a≈0.502。
        let h = Rgba { r: 255, g: 255, b: 255, a: 128 }.d2d();
        assert!((h.a - 128.0/255.0).abs() < 1e-3);
        assert!((h.r - h.a).abs() < 1e-3); // premultiplied: r == a for white
    }

    #[test]
    fn apply_os_accessibility_disables_acrylic_and_motion() {
        let app = Appearance::default(); // acrylic
        let t = Theme::resolve(&app, false);
        assert!(t.acrylic && t.motion && t.colors.bg.a < 255);

        // 透明効果オフ → アクリル解除＋bg 不透明化。アニメーションオフ → motion=false。
        let off = apply_os_accessibility(t.clone(), false, false);
        assert!(!off.acrylic);
        assert_eq!(off.colors.bg.a, 255);
        assert!(!off.motion);

        // 両方オンなら無変化。
        let on = apply_os_accessibility(t.clone(), true, true);
        assert!(on.acrylic && on.motion);
        assert!(on.colors.bg.a < 255);

        // opaque テーマに透明効果オフを合成しても壊れない（no-op）。
        let mut app2 = Appearance::default();
        app2.backdrop = "opaque".into();
        let t2 = apply_os_accessibility(Theme::resolve(&app2, false), false, true);
        assert!(!t2.acrylic);
        assert_eq!(t2.colors.bg.a, 255);
        assert!(t2.motion);
    }

    #[test]
    fn font_point_tenths_rounds_from_f32() {
        let mut app = Appearance::default();
        app.font_point = 10.5;
        assert_eq!(Theme::resolve(&app, false).font_point_tenths, 105);
        app.font_point = 12.0;
        assert_eq!(Theme::resolve(&app, false).font_point_tenths, 120);
    }
}
