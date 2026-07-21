//! ライブ変換中に生の読み（ひらがな）をキャレット上側へ常時表示する読みモニタ（Win32 popup）。
//!
//! ライブ変換は preedit を変換結果（漢字かな交じり）へ全置換するため、「今何を打ったか」が
//! 画面から消える。読みを候補窓/HUD と同じ popup 基盤の小窓で並走表示する
//! （spec: docs/superpowers/specs/2026-07-21-reading-monitor-design.md）。
//!
//! mode_hud との差分は 2 点だけ: 自動消去タイマを持たない（明示 hide まで表示）、
//! テキストが打鍵ごとに更新され幅が文字列に追従する。mode_hud を汎用化せず同型の
//! 別実装にしたのは、HUD が直近でフェード/SetTimer 修正を重ねた直後で条件分岐の追加は
//! 回帰リスクが高く、あ/A HUD と本窓は同時表示があり得て結局 2 インスタンス必要なため
//! （spec の却下案 B）。

use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect,
    GetDC, GetDeviceCaps, GetTextExtentPoint32W, InvalidateRect, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor, DT_END_ELLIPSIS, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
    LOGPIXELSX, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, GetClientRect, IsWindowVisible, KillTimer, SetTimer,
    SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SW_HIDE,
    SW_SHOWNOACTIVATE, WM_NCDESTROY, WM_PAINT, WM_TIMER,
};

use crate::candidate_window::CaretAnchor;
use crate::popup::{self, effective_dpi, font_size_px, scale, Backend, PopupState};
use crate::text_service::tip_log;

const CLASS_NAME: PCWSTR = w!("NospacekeyReadingMonitor");

/// 退場フェード完了後に SW_HIDE する遅延タイマの ID（自動消去タイマは持たない）。
const FADE_TIMER_ID: usize = 1;

/// ヘアライン枠の太さ（px、非スケール）。
const BORDER: i32 = 1;
/// 左右パディング（dp）。
const PAD_H: i32 = 10;
/// 上下パディング合計（dp）。
const PAD_V_TOTAL: i32 = 12;
/// テキスト幅の下限（dp）。上限は固定値でなく設定 max_chars から max_text_w_px で導出する
/// （spec 2026-07-21 max-chars 方式B）。「作業領域の半分」の動的計算はモニタ照会を
/// 増やすだけで、文字数指定+末尾優先で用が足りる（spec 却下案）。
const MIN_TEXT_W: i32 = 24;
/// キャレット上端と窓下端の間隔（dp）。
const GAP: i32 = 6;

static CLASS_ATOM: OnceLock<u16> = OnceLock::new();

/// 表示条件の唯一の真実源（純関数）。
/// ライブ変換 OFF は preedit に読みがそのまま見えるので出さない。候補窓表示中は隠す
/// （ユーザ確認済みの決定 — 候補選択中は候補窓に集中する）。
pub(crate) fn should_show(
    enabled: bool,
    composing: bool,
    live_enabled: bool,
    candidate_visible: bool,
) -> bool {
    enabled && composing && live_enabled && !candidate_visible
}

/// 幅上限（物理px）= max_chars × フォントem幅。読みモニタの中身はひらがな=全角のみで
/// 全角グリフの advance ≒ em（フォントpx）のため「N文字」として実質正確（spec 方式B）。
/// font_px は DPI スケール済みなので追加の scale は不要。
pub(crate) fn max_text_w_px(max_chars: u32, font_px: i32) -> i32 {
    max_chars as i32 * font_px
}

/// 表示合成とバッファの末尾バウンド（文字数）。ASCII 半角（≒em/2）が混ざっても
/// em 幅換算の表示容量を下回らない係数2。末尾優先描画では上限を超えた頭は永遠に
/// 描かれないため、保持する意味がない（無制限だと Enter を押さない長文で実測/UTF-16
/// 変換が O(n) 成長する — spec 性能P1）。
pub(crate) fn display_bound(max_chars: u32) -> usize {
    2 * max_chars as usize
}

/// 自動確定でエンジンが消費した読み（= full から remaining を末尾サフィックスとして
/// 剥がした頭）。サフィックス不成立（engine_insert 失敗の raw フォールバック等）は None —
/// 呼び出し側は追記をスキップして劣化する（欠落は Enter まで恒久だが壊れない）。
pub(crate) fn consumed_reading<'a>(full: &'a str, remaining: &str) -> Option<&'a str> {
    full.strip_suffix(remaining)
}

fn trim_to_tail(buf: &mut String, max_chars: usize) {
    let n = buf.chars().count();
    if n > max_chars {
        if let Some((cut, _)) = buf.char_indices().nth(n - max_chars) {
            buf.drain(..cut);
        }
    }
}

/// 累積バッファへ消費分を追記し末尾 `bound` 文字へ切り詰める（bound は display_bound 由来）。
pub(crate) fn append_committed(buf: &mut String, consumed: &str, bound: usize) {
    buf.push_str(consumed);
    trim_to_tail(buf, bound);
}

/// モニタ表示文字列（累積+現在読み、末尾優先バウンド）。累積 OFF は committed="" で
/// current と等価になる — OFF 経路に別分岐を作らない。
pub(crate) fn compose_monitor_text(committed: &str, current: &str, bound: usize) -> String {
    let mut s = String::with_capacity(committed.len() + current.len());
    s.push_str(committed);
    s.push_str(current);
    trim_to_tail(&mut s, bound);
    s
}

/// 実測テキスト幅が px 上限（max_text_w_px 由来）を超えたか。超えたら末尾寄せへ切り替える。
pub(crate) fn text_overflows(text_px_w: i32, max_w_px: i32) -> bool {
    text_px_w > max_w_px
}

/// アンカー矩形が取れないフレームの位置決め方針。表示中は前回位置を保持する
/// （キャレット矩形も取れない状況で DEFAULT へ跳ねると、成否が交互するホストで
/// ②の目的（静止）が壊れる — spec UX P-3）。
pub(crate) enum AnchorPlan {
    Move(CaretAnchor),
    Hold,
    Fallback,
}

pub(crate) fn plan_anchor(anchor: Option<CaretAnchor>, visible: bool) -> AnchorPlan {
    match anchor {
        Some(a) => AnchorPlan::Move(a),
        None if visible => AnchorPlan::Hold,
        None => AnchorPlan::Fallback,
    }
}

/// 窓の (幅, 高さ)。`text_px_w` は実測テキスト幅（物理px）、`font_px` はフォント高（物理px）、
/// `max_w_px` は px 上限（max_text_w_px 由来）。テキスト幅は [MIN, max_w_px] へクランプ —
/// はみ出しは描画側の末尾寄せクリップが受ける。max 側は `.max(min_w)` で下駄を履かせる —
/// clamp は min>max で panic するため（max_chars=10×小フォントで実在するエッジ）。
pub(crate) fn monitor_window_size(text_px_w: i32, font_px: i32, dpi: i32, max_w_px: i32) -> (i32, i32) {
    let min_w = scale(MIN_TEXT_W, dpi);
    let clamped = text_px_w.clamp(min_w, max_w_px.max(min_w));
    let w = 2 * BORDER + 2 * scale(PAD_H, dpi) + clamped;
    let h = 2 * BORDER + scale(PAD_V_TOTAL, dpi) + font_px;
    (w, h)
}

/// HWND ごとの描画状態（GWLP_USERDATA に格納）。
struct MonitorState {
    /// 現在の読み（ひらがな）。打鍵ごとに更新される。
    text: String,
    theme: crate::theme::Theme,
    backend: Backend,
    /// 実測幅が上限超過（末尾寄せ描画中）か。show_or_update が設定し paint が読む。
    overflow: bool,
    /// 直近 SetWindowPos したサイズ。同一なら swapchain ResizeBuffers を省く
    /// （renderer.resize に同一サイズ早期リターンが無く、累積 ON では上限到達後も
    /// 毎打鍵フル再構築になるため — spec 性能C2）。
    last_size: (i32, i32),
}

impl PopupState for MonitorState {
    fn backend_mut(&mut self) -> &mut Backend {
        &mut self.backend
    }
}

unsafe fn monitor_state<'a>(hwnd: HWND) -> Option<&'a mut MonitorState> {
    popup::state_mut::<MonitorState>(hwnd)
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_TIMER => unsafe {
            if wparam.0 == FADE_TIMER_ID {
                // 退場フェード完了 → フラグを解いて実際に隠す（HUD の HUD_FADE_TIMER_ID と同じ）。
                let _ = KillTimer(Some(hwnd), FADE_TIMER_ID);
                if let Some(s) = monitor_state(hwnd) {
                    s.backend.fading_out = false;
                }
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        },
        WM_NCDESTROY => unsafe {
            // GWLP_USERDATA のボックスを回収して破棄（Backend の Drop が所有フォントを解放）。
            drop(popup::take_state::<MonitorState>(hwnd));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        },
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// WM_PAINT のディスパッチ。バックエンドは初回生成時に固定（mode_hud と同じ 2 段判定）。
fn paint(hwnd: HWND) {
    unsafe {
        let is_d2d = monitor_state(hwnd)
            .map(|s| s.backend.renderer.is_some())
            .unwrap_or(false);
        if is_d2d {
            paint_d2d(hwnd);
        } else {
            paint_gdi(hwnd);
        }
    }
}

fn paint_gdi(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_invalid() {
            return;
        }
        let state = match monitor_state(hwnd) {
            Some(s) => s,
            None => {
                let _ = EndPaint(hwnd, &ps);
                return;
            }
        };
        let dpi = effective_dpi(GetDeviceCaps(Some(hdc), LOGPIXELSX));
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let colors = state.theme.colors;

        let bg = CreateSolidBrush(COLORREF(colors.bg.colorref()));
        let _ = FillRect(hdc, &rc, bg);
        let _ = DeleteObject(bg.into());

        let _ = SetBkMode(hdc, TRANSPARENT);
        let family = popup::family_utf16z(&state.theme.font_family);
        let point_tenths = state.theme.font_point_tenths;
        let hfont = state.backend.font_for_dpi(&family, point_tenths, dpi);
        let old = hfont.map(|f| SelectObject(hdc, f.into()));
        let _ = SetTextColor(hdc, COLORREF(colors.text.colorref()));
        let mut text: Vec<u16> = state.text.encode_utf16().collect();
        // 左右パディング分を除いた領域へ 1 行描画（候補窓と同じ DT_SINGLELINE|DT_END_ELLIPSIS）。
        let pad = scale(PAD_H, dpi);
        let mut tr = RECT { left: rc.left + pad, top: rc.top, right: rc.right - pad, bottom: rc.bottom };
        // 上限超過は末尾寄せ: DT_RIGHT + rect クリップで頭側が切れる（DT_END_ELLIPSIS だと
        // 末尾=最新の読みが消える）。
        let flags = if state.overflow {
            DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_NOPREFIX
        } else {
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX
        };
        let _ = DrawTextW(hdc, &mut text, &mut tr, flags);
        if let Some(o) = old {
            let _ = SelectObject(hdc, o);
        }

        let bb = CreateSolidBrush(COLORREF(colors.border.colorref()));
        let _ = FrameRect(hdc, &rc, bb);
        let _ = DeleteObject(bb.into());

        let _ = EndPaint(hwnd, &ps);
    }
}

/// D2D 描画。SetDpi(96,96) で px==DIP。テキストフォーマットは Backend::text_format が
/// NO_WRAP + 文字単位省略記号を焼き込み済み（候補窓の 1 行固定修正 da11d7d と同一）。
/// 失敗は握り潰して次フレームに委ねる。BeginPaint/EndPaint は全経路で必ず対にする。
unsafe fn paint_d2d(hwnd: HWND) {
    use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
    use windows::Win32::Graphics::Direct2D::D2D1_DRAW_TEXT_OPTIONS_CLIP;
    use windows::Win32::Graphics::DirectWrite::{
        DWRITE_MEASURING_MODE_NATURAL, DWRITE_TEXT_ALIGNMENT_LEADING,
        DWRITE_TEXT_ALIGNMENT_TRAILING,
    };

    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let Some(state) = monitor_state(hwnd) else {
        let _ = EndPaint(hwnd, &ps);
        return;
    };
    if state.backend.renderer.is_none() {
        let _ = EndPaint(hwnd, &ps);
        return;
    }
    let dpi = effective_dpi(GetDeviceCaps(Some(hdc), LOGPIXELSX));
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);

    let family: Vec<u16> = state
        .theme
        .font_family
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let font_px = font_size_px(state.theme.font_point_tenths, dpi);
    // 上限超過は末尾寄せ+トリミング無し（トリミングは整列と無関係に末尾を削るため、
    // TRAILING+トリミング有りだと最新の読みが…で消える。頭側は CLIP が切る）。
    let (align, trim) = if state.overflow {
        (DWRITE_TEXT_ALIGNMENT_TRAILING, false)
    } else {
        (DWRITE_TEXT_ALIGNMENT_LEADING, true)
    };
    let Some(fmt) = state.backend.text_format(&family, font_px, align, trim) else {
        let _ = EndPaint(hwnd, &ps);
        return;
    };
    // 以降 state への書き込みは end_draw 後にしか無いので、テーマは不変借用で読む。
    let t = &state.theme;

    // TIP パスでは expect/unwrap を使わず else で対の EndPaint を打って return する。
    let Some(renderer) = state.backend.renderer.as_ref() else {
        let _ = EndPaint(hwnd, &ps);
        return;
    };
    let Ok(ctx) = renderer.begin_draw() else {
        let _ = EndPaint(hwnd, &ps);
        return;
    };
    ctx.SetDpi(96.0, 96.0);

    let rectf = D2D_RECT_F {
        left: rc.left as f32,
        top: rc.top as f32,
        right: rc.right as f32,
        bottom: rc.bottom as f32,
    };
    let brush = |c: crate::theme::Rgba| ctx.CreateSolidColorBrush(&c.d2d(), None).ok();

    ctx.Clear(Some(&crate::theme::Rgba { r: 0, g: 0, b: 0, a: 0 }.d2d()));
    if let Some(b) = brush(t.colors.bg) {
        ctx.FillRectangle(&rectf, &b);
    }

    let pad = scale(PAD_H, dpi) as f32;
    let text_rect = D2D_RECT_F {
        left: rectf.left + pad,
        top: rectf.top,
        right: rectf.right - pad,
        bottom: rectf.bottom,
    };
    let text_utf16: Vec<u16> = state.text.encode_utf16().collect();
    if let Some(b) = brush(t.colors.text) {
        ctx.DrawText(
            &text_utf16,
            &fmt,
            &text_rect,
            &b,
            D2D1_DRAW_TEXT_OPTIONS_CLIP,
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }

    if let Some(b) = brush(t.colors.border) {
        let inset = D2D_RECT_F {
            left: rc.left as f32 + 0.5,
            top: rc.top as f32 + 0.5,
            right: rc.right as f32 - 0.5,
            bottom: rc.bottom as f32 - 0.5,
        };
        ctx.DrawRectangle(&inset, &b, 1.0, None);
    }

    // end_draw の失敗がデバイスロスト由来なら renderer_dead を立て、次回表示で窓を作り直す。
    let lost = match renderer.end_draw() {
        Ok(()) => false,
        Err(e) => crate::render::is_device_lost(&e),
    };
    if lost {
        state.backend.renderer_dead = true;
    }
    let _ = EndPaint(hwnd, &ps);
}

/// GDI パス用のテキスト幅実測（物理px）。フォントが作れない/測れないときは None（呼び出し側は
/// MIN_TEXT_W クランプで劣化）。
unsafe fn measure_text_gdi(hwnd: HWND, state: &mut MonitorState, dpi: i32) -> Option<i32> {
    let hdc = GetDC(Some(hwnd));
    if hdc.is_invalid() {
        return None;
    }
    let family = popup::family_utf16z(&state.theme.font_family);
    let hfont = state.backend.font_for_dpi(&family, state.theme.font_point_tenths, dpi);
    let mut size = SIZE::default();
    let ok = match hfont {
        Some(f) => {
            let old = SelectObject(hdc, f.into());
            let utf16: Vec<u16> = state.text.encode_utf16().collect();
            let r = GetTextExtentPoint32W(hdc, &utf16, &mut size).as_bool();
            let _ = SelectObject(hdc, old);
            r
        }
        None => false,
    };
    let _ = ReleaseDC(Some(hwnd), hdc);
    ok.then_some(size.cx)
}

/// 読みモニタ本体。`hwnd` は遅延生成（初回 `show_or_update` まで null）。
pub struct ReadingMonitor {
    hwnd: HWND,
}

impl ReadingMonitor {
    /// HWND を持たない空のモニタを構築する（`TextService::new` 用）。
    pub fn empty() -> Self {
        Self { hwnd: HWND(std::ptr::null_mut()) }
    }

    /// Deactivate から呼ぶ（mode_hud::destroy と同じ理由 — プロセス終了時の msctf 後始末に
    /// SurfaceRenderer の drop を持ち込ませない。c000041d の再発防止）。
    pub fn destroy(&mut self) {
        if !self.hwnd.is_invalid() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
            self.hwnd = HWND(std::ptr::null_mut());
            tip_log("ev=reading_monitor action=destroy");
        }
    }

    /// デバイスロスト後の回復。判定・破棄は popup 側の共通処理。
    fn recover_if_device_lost(&mut self) {
        popup::recover_if_device_lost::<MonitorState>(
            &mut self.hwnd,
            "ev=reading_monitor_device_lost_recover",
        );
    }

    /// 必要なら HWND を生成する。失敗したら null のまま（劣化動作）。
    fn ensure_hwnd(&mut self, theme: crate::theme::Theme) {
        if !self.hwnd.is_invalid() {
            return;
        }
        if popup::register_class(&CLASS_ATOM, CLASS_NAME, Some(wnd_proc)).is_none() {
            return;
        }
        unsafe {
            // text=0 は必ず MIN 幅へクランプされるため第4引数の値は初期窓に影響しない。
            let (width, height) = monitor_window_size(0, 14, 96, max_text_w_px(34, 14));
            let Some((hwnd, renderer)) = popup::create_backed_popup(CLASS_NAME, width, height)
            else {
                self.hwnd = HWND(std::ptr::null_mut());
                return;
            };
            self.hwnd = hwnd;
            // 角丸は両パス、アクリルは D2D パスのみ（mode_hud と同じ。Win10=no-op）。
            crate::render::apply_dwm_chrome(hwnd, theme.rounded, theme.acrylic && renderer.is_some());
            popup::install_state(
                hwnd,
                Box::new(MonitorState {
                    text: String::new(),
                    theme,
                    backend: Backend::new(renderer),
                    overflow: false,
                    last_size: (0, 0),
                }),
            );
        }
    }

    /// 読み `text` を composition 先頭アンカーの上側に表示/更新する。表示条件の判定は
    /// 呼び出し側（TextService::update_reading_monitor — should_show が唯一の真実源）。
    /// `anchor=None`（矩形取得失敗）は表示中なら前回位置保持・非表示なら既定座標（plan_anchor）。
    /// HWND 生成失敗は劣化（何もしない）、空文字は hide。
    pub fn show_or_update(
        &mut self,
        text: &str,
        anchor: Option<CaretAnchor>,
        max_chars: u32,
        theme: crate::theme::Theme,
    ) {
        if text.is_empty() {
            self.hide();
            return;
        }
        self.recover_if_device_lost();
        self.ensure_hwnd(theme.clone());
        if self.hwnd.is_invalid() {
            return;
        }
        unsafe {
            if let Some(state) = monitor_state(self.hwnd) {
                state.text = text.to_string();
                // DWM chrome に効く属性（角丸/アクリル）が変わったときだけ再適用する
                // （色だけの変化は後段の InvalidateRect による再描画で足りる — HUD と同じ）。
                let chrome_changed =
                    state.theme.rounded != theme.rounded || state.theme.acrylic != theme.acrylic;
                state.theme = theme;
                if chrome_changed {
                    let d2d = state.backend.renderer.is_some();
                    crate::render::apply_dwm_chrome(
                        self.hwnd, state.theme.rounded, state.theme.acrylic && d2d,
                    );
                }
            }
        }
        let dpi = popup::window_dpi(self.hwnd);
        unsafe {
            let Some(state) = monitor_state(self.hwnd) else { return };
            let font_px_f = font_size_px(state.theme.font_point_tenths, dpi);
            let font_px = font_px_f.ceil() as i32;
            // テキスト幅は描画と同一エンジンで実測（D2D=DWrite / GDI=GetTextExtentPoint32W）。
            // 測れなければ 0 → monitor_window_size の MIN クランプで最小幅に劣化。
            let max_w = max_text_w_px(max_chars, font_px);
            let family = popup::family_utf16z(&state.theme.font_family);
            let text_w = if state.backend.renderer.is_some() {
                state
                    .backend
                    .measure_max_width_dwrite(
                        &family,
                        font_px_f,
                        std::slice::from_ref(&state.text),
                        max_w,
                    )
                    .unwrap_or(0)
            } else {
                measure_text_gdi(self.hwnd, state, dpi).unwrap_or(0)
            };
            let (w, h) = monitor_window_size(text_w, font_px, dpi, max_w);
            state.overflow = text_overflows(text_w, max_w);
            // SetWindowPos/resize は WS_VISIBLE を触らないので、ここでの IsWindowVisible は
            // 従来使っていた ShowWindow の戻り値（呼び出し前の可視状態）と同値。
            let was_visible = IsWindowVisible(self.hwnd).as_bool();
            // アンカー上側（caret_top の上に GAP 空けて）。caret_top 不明（既定座標劣化）は
            // アンカー位置へそのまま（下側）— そのときは実キャレットも不明なので上下の
            // 使い分けに意味がない。place_on_monitor が作業領域内へクランプする
            // （上端はみ出しは上端貼り付き。下へのフリップはしない — spec 位置決め）。
            match plan_anchor(anchor, was_visible) {
                AnchorPlan::Move(a) => {
                    let (dx, dy) = match a.caret_top {
                        Some(top) => (a.x, top - h - scale(GAP, dpi)),
                        None => (a.x, a.y),
                    };
                    let (fx, fy) = popup::place_on_monitor(dx, dy, w, h);
                    let _ =
                        SetWindowPos(self.hwnd, None, fx, fy, w, h, SWP_NOACTIVATE | SWP_NOZORDER);
                }
                AnchorPlan::Hold => {
                    // 位置は前回のまま、サイズだけ追従（読みは伸縮する）。
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        0,
                        0,
                        w,
                        h,
                        SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOMOVE,
                    );
                }
                AnchorPlan::Fallback => {
                    let a = crate::text_service::DEFAULT_CARET_POS;
                    let (fx, fy) = popup::place_on_monitor(a.x, a.y, w, h);
                    let _ =
                        SetWindowPos(self.hwnd, None, fx, fy, w, h, SWP_NOACTIVATE | SWP_NOZORDER);
                }
            }
            // 同一サイズの打鍵更新は swapchain 再構築（ResizeBuffers — 同一サイズでも
            // フル実行される）を省きテキスト再描画のみにする。初回 show は必ず resize
            // （「size_changed ガードが常に偽で初回 flash」の前例は SetWindowPos 後の
            // GetClientRect 比較が原因 — 自前追跡の last_size なら安全）。
            let size_unchanged = monitor_state(self.hwnd)
                .map(|s| s.last_size == (w, h))
                .unwrap_or(false);
            if was_visible && size_unchanged {
                let _ = InvalidateRect(Some(self.hwnd), None, true);
            } else {
                popup::resize_and_invalidate::<MonitorState>(self.hwnd, w, h);
            }
            if let Some(s) = monitor_state(self.hwnd) {
                s.last_size = (w, h);
            }
            // 退場フェード中の再表示なら遅延 hide タイマを解除して表示を続行する。
            let _ = KillTimer(Some(self.hwnd), FADE_TIMER_ID);
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            // 出現モーション（新規出現のみ短いフェード。表示中の打鍵更新は 1.0 へスナップ
            // ＝ちらつかない）。候補窓/HUD と共有の popup::play_entrance。
            if let Some(state) = monitor_state(self.hwnd) {
                let motion = state.theme.motion;
                popup::play_entrance(state, motion, was_visible);
            }
            tip_log(&format!(
                "ev=reading_monitor action={} len={}",
                if was_visible { "update" } else { "show" },
                text.chars().count()
            ));
        }
    }

    /// 退場（フェードできるならフェードして遅延 hide、だめなら即時 hide）。
    /// 非表示中は no-op（ログも出さない — 打鍵ごとの hide 連打でログを埋めない）。
    pub fn hide(&mut self) {
        if self.hwnd.is_invalid() {
            return;
        }
        unsafe {
            if !IsWindowVisible(self.hwnd).as_bool() {
                return;
            }
            let action = monitor_state(self.hwnd)
                .map(|s| {
                    let motion = s.theme.motion;
                    popup::begin_fade_out(s, motion)
                })
                .unwrap_or(popup::FadeOut::Immediate);
            match action {
                popup::FadeOut::AlreadyFading => return,
                popup::FadeOut::Fade(ms) => {
                    // SetTimer 失敗を無視すると fading_out が立ったまま透明な TOPMOST 窓が
                    // 残留する（HUD/候補窓と同じ理由）。失敗時は即時 hide へ劣化する。
                    if SetTimer(Some(self.hwnd), FADE_TIMER_ID, ms, None) == 0 {
                        if let Some(s) = monitor_state(self.hwnd) {
                            s.backend.fading_out = false;
                        }
                        let _ = ShowWindow(self.hwnd, SW_HIDE);
                    }
                }
                popup::FadeOut::Immediate => {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                }
            }
        }
        tip_log("ev=reading_monitor action=hide");
    }
}

impl Drop for ReadingMonitor {
    fn drop(&mut self) {
        self.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_show_requires_all_of_enabled_composing_live_and_no_candidates() {
        // 全条件成立のときだけ表示。
        assert!(should_show(true, true, true, false));
        // 設定 OFF / 非合成中 / ライブ変換 OFF / 候補窓表示中 のいずれかで非表示。
        assert!(!should_show(false, true, true, false));
        assert!(!should_show(true, false, true, false));
        assert!(!should_show(true, true, false, false));
        assert!(!should_show(true, true, true, true));
    }

    #[test]
    fn consumed_reading_strips_remaining_as_suffix() {
        // 自動確定はエンジンが先頭文節の読みを消費する — 残りは全読みの末尾サフィックス。
        assert_eq!(consumed_reading("きょうはてんき", "てんき"), Some("きょうは"));
        // 全消費(remaining が空)は全体が消費分。
        assert_eq!(consumed_reading("きょうは", ""), Some("きょうは"));
        // 濁点・促音・小書きを含む読みでも文字列サフィックスとして剥がせる。
        assert_eq!(consumed_reading("がっこうへいった", "へいった"), Some("がっこう"));
        // サフィックス不成立(raw フォールバック等の不整合)は None — 呼び出し側がスキップ。
        assert_eq!(consumed_reading("kyouha", "てんき"), None);
    }

    #[test]
    fn append_committed_bounds_buffer_to_tail_chars() {
        let mut buf = "あ".repeat(60);
        append_committed(&mut buf, &"い".repeat(10), 64);
        // 末尾 bound(64) にバウンド: 先頭の「あ」が 6 文字落ち、末尾は保たれる。
        assert_eq!(buf.chars().count(), 64);
        assert!(buf.ends_with(&"い".repeat(10)));
        assert!(buf.starts_with("あ"));
    }

    #[test]
    fn compose_monitor_text_joins_and_trims_tail_priority() {
        // 通常: 連結のみ。
        assert_eq!(compose_monitor_text("きょうは", "てんき", 64), "きょうはてんき");
        // 累積 OFF 相当(committed 空)は current と等価 — OFF 時の現状挙動保証。
        assert_eq!(compose_monitor_text("", "てんき", 64), "てんき");
        // 上限超過は末尾優先で頭を落とす。
        let long = "あ".repeat(70);
        let s = compose_monitor_text(&long, "おわり", 64);
        assert_eq!(s.chars().count(), 64);
        assert!(s.ends_with("おわり"));
    }

    #[test]
    fn auto_commit_frame_keeps_display_string_unchanged() {
        // 順序契約(spec 状態C1): 「追記→last_reading 縮小」の後の合成表示が追記前と一致する。
        // これが破れると自動確定フレームだけ読みが縮んで見える「跳ね」になる。
        let full = "きょうはてんきがいい";
        let remaining = "てんきがいい";
        let mut buf = String::from("わたしの");
        let before = compose_monitor_text(&buf, full, 64);
        let consumed = consumed_reading(full, remaining).expect("正常系はサフィックス成立");
        append_committed(&mut buf, consumed, 64);
        let after = compose_monitor_text(&buf, remaining, 64);
        assert_eq!(before, after);
    }

    #[test]
    fn text_overflows_only_beyond_max_w_px() {
        // px 上限ちょうどは切り替えず、超えたら末尾寄せ。
        assert!(!text_overflows(476, 476));
        assert!(text_overflows(477, 476));
    }

    #[test]
    fn max_text_w_px_is_chars_times_em() {
        // 全角1グリフ≒em幅(=フォントpx)。34文字×14px=476px が旧480dpとほぼ同じ見た目。
        assert_eq!(max_text_w_px(34, 14), 476);
        assert_eq!(max_text_w_px(10, 20), 200);
    }

    #[test]
    fn display_bound_is_twice_max_chars() {
        // ASCII 半角(em の約半分)が混ざっても em 幅換算の表示容量を下回らない係数2。
        assert_eq!(display_bound(34), 68);
        assert_eq!(display_bound(10), 20);
    }

    #[test]
    fn plan_anchor_holds_position_when_rect_unavailable_but_visible() {
        let a = CaretAnchor { x: 10, y: 20, caret_top: Some(5) };
        // 矩形が取れたら移動。
        assert!(matches!(plan_anchor(Some(a), true), AnchorPlan::Move(_)));
        assert!(matches!(plan_anchor(Some(a), false), AnchorPlan::Move(_)));
        // 取れない+表示中 = 前回位置保持(DEFAULT へ跳ねない — spec UX P-3)。
        assert!(matches!(plan_anchor(None, true), AnchorPlan::Hold));
        // 取れない+非表示 = 既定座標で初期配置。
        assert!(matches!(plan_anchor(None, false), AnchorPlan::Fallback));
    }

    #[test]
    fn monitor_window_size_scales_and_clamps() {
        // 96DPI・フォント14px・テキスト100px: w=2+20+100=122, h=2+12+14=28。
        assert_eq!(monitor_window_size(100, 14, 96, 480), (122, 28));
        // 192DPI で pad が倍にスケール（テキスト実測幅・px上限は呼び出し側が実DPIで計算）。
        assert_eq!(monitor_window_size(100, 28, 192, 960), (2 + 40 + 100, 2 + 24 + 28));
        // 幅下限: 空文字相当でも最小幅を保つ（96DPI: 24px）。
        assert_eq!(monitor_window_size(0, 14, 96, 480).0, 2 + 20 + 24);
        // 幅上限: 巨大テキストは max_w_px でクランプ。
        assert_eq!(monitor_window_size(10_000, 14, 96, 480).0, 2 + 20 + 480);
        // min>max 防御: max_w_px が下限未満でも clamp が panic せず下限に落ちる
        // (max_chars=10×小フォントで実在するエッジ)。
        assert_eq!(monitor_window_size(0, 14, 96, 1).0, 2 + 20 + 24);
    }
}
