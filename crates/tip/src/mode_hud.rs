//! モード切替時に あ/A をキャレット近傍へ一瞬表示する自前描画 HUD（Win32 popup）。
//!
//! Win11 は言語バーを廃止し、トレイの Input Indicator にカスタム IME のモードアイコンを
//! 出すには互換カテゴリ登録＋HICON が要る（テキストの langbar 項目は出ない）。確実に
//! 視認させるため、mozc 同様にモード切替時に あ/A を caret 近傍へ一瞬出して自動で消す。
//!
//! `candidate_window` の Win32 popup パターン（WS_POPUP|TOPMOST|NOACTIVATE・GWLP_USERDATA に
//! 状態・WM_PAINT 自前描画・遅延生成・Drop 破棄・DPI/モニタ対応）を踏襲する。自動消去は
//! ウィンドウ付き WM_TIMER で自己完結（text_service のタイマ配線に依存しない）。

use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect,
    GetDeviceCaps, SelectObject, SetBkMode, SetTextColor, DT_CENTER, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, HFONT, LOGPIXELSX, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, GetClientRect, IsWindowVisible, KillTimer, SetTimer,
    SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE,
    WM_NCDESTROY, WM_PAINT, WM_TIMER,
};

use crate::langbar::mode_label_ephemeral;
use crate::popup::{self, effective_dpi, font_size_px, scale, Backend, PopupState};

const CLASS_NAME: PCWSTR = w!("NospacekeyModeHud");

/// HUD カードの一辺（dp、96DPI 基準）。あ/A 1 文字＋余白に十分な正方形。
const HUD_SIDE: i32 = 48;
/// HUD フォントのポイントサイズ（10 倍値、24.0pt=240）。候補窓の 10.5pt より大きい。
const HUD_FONT_POINT_TENTHS: i32 = 240;
/// 自動消去までの時間（ms）。
const HUD_DURATION_MS: u32 = 1200;
/// 自動消去タイマの ID（ウィンドウ付きタイマ）。
const HUD_TIMER_ID: usize = 1;
/// 退場フェード完了後に SW_HIDE する遅延タイマの ID。
const HUD_FADE_TIMER_ID: usize = 2;
/// 自前ヘアライン枠の太さ（px、非スケール）。
const BORDER: i32 = 1;

static CLASS_ATOM: OnceLock<u16> = OnceLock::new();

/// HUD カードの (幅, 高さ) を DPI スケールで求める純関数。正方形。
pub(crate) fn hud_window_size(dpi: i32) -> (i32, i32) {
    let side = 2 * BORDER + scale(HUD_SIDE, dpi);
    (side, side)
}

/// HWND ごとの描画状態（GWLP_USERDATA に格納）。表示文字・テーマ・描画バックエンドを持つ。
struct HudState {
    /// 現在の表示文字（"あ" or "A"。mode_label の &'static を持つので確保不要）。
    label: &'static str,
    /// A 段: 共有テーマ。flash ごとに更新される。
    theme: crate::theme::Theme,
    /// 共有描画バックエンド（D2D/GDI・DWrite・GDI フォント・デバイスロストフラグ）。
    backend: Backend,
}

impl PopupState for HudState {
    fn backend_mut(&mut self) -> &mut Backend {
        &mut self.backend
    }
}

impl HudState {
    /// HUD は候補窓のフォントサイズではなく大きな固定サイズ（24pt=240）を使う。
    /// ファミリは候補窓と同じく theme から（GDI パスにも settings のフォントが効く）。
    unsafe fn font_for_dpi(&mut self, dpi: i32) -> Option<HFONT> {
        let family = popup::family_utf16z(&self.theme.font_family);
        self.backend.font_for_dpi(&family, HUD_FONT_POINT_TENTHS, dpi)
    }
}

unsafe fn hud_state<'a>(hwnd: HWND) -> Option<&'a mut HudState> {
    popup::state_mut::<HudState>(hwnd)
}

/// 表示タイマ満了時の退場。モーションが使えるならフェードして遅延 hide、だめなら即時 hide。
/// フェードイン途中なら推定不透明度から帰り、既にフェードアウト中なら何もしない
/// （判定は候補窓と共有の popup::begin_fade_out）。
unsafe fn begin_dismiss(hwnd: HWND) {
    let action = if IsWindowVisible(hwnd).as_bool() {
        hud_state(hwnd)
            .map(|s| {
                let motion = s.theme.motion;
                popup::begin_fade_out(s, motion)
            })
            .unwrap_or(popup::FadeOut::Immediate)
    } else {
        popup::FadeOut::Immediate
    };
    match action {
        popup::FadeOut::AlreadyFading => {}
        popup::FadeOut::Fade(ms) => {
            // SetTimer 失敗を無視すると fading_out が立ったまま透明な TOPMOST 窓が
            // 残留する（候補窓 hide と同じ理由）。失敗時は即時 hide へ劣化する。
            if SetTimer(Some(hwnd), HUD_FADE_TIMER_ID, ms, None) == 0 {
                if let Some(s) = hud_state(hwnd) {
                    s.backend.fading_out = false;
                }
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
        popup::FadeOut::Immediate => {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_TIMER => unsafe {
            if wparam.0 == HUD_TIMER_ID {
                // 表示時間満了 → 退場（フェードできるなら来た道＝フェードで帰る）。
                let _ = KillTimer(Some(hwnd), HUD_TIMER_ID);
                begin_dismiss(hwnd);
            } else if wparam.0 == HUD_FADE_TIMER_ID {
                // 退場フェード完了 → フラグを解いて実際に隠す（次回退場の再入ガード解除）。
                let _ = KillTimer(Some(hwnd), HUD_FADE_TIMER_ID);
                if let Some(s) = hud_state(hwnd) {
                    s.backend.fading_out = false;
                }
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        },
        WM_NCDESTROY => unsafe {
            // GWLP_USERDATA のボックスを回収して破棄（Backend の Drop が所有フォントを解放）。
            drop(popup::take_state::<HudState>(hwnd));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        },
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// WM_PAINT のディスパッチ。renderer が Some（D2D バックエンド）なら D2D 描画、
/// None（GDI フォールバック）なら従来の GDI 本体を呼ぶ。バックエンドは初回生成時に固定。
fn paint(hwnd: HWND) {
    unsafe {
        let is_d2d = hud_state(hwnd)
            .map(|s| s.backend.renderer.is_some())
            .unwrap_or(false);
        if is_d2d {
            paint_d2d(hwnd);
        } else {
            paint_gdi(hwnd);
        }
    }
}

/// WM_PAINT の GDI 本体。色は theme.colors.* から取る（ハードコード const は使わない）。
fn paint_gdi(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_invalid() {
            return;
        }
        let state = match hud_state(hwnd) {
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

        // カード地。
        let bg = CreateSolidBrush(COLORREF(colors.bg.colorref()));
        let _ = FillRect(hdc, &rc, bg);
        let _ = DeleteObject(bg.into());

        // 文字（中央寄せの あ/A）。
        let _ = SetBkMode(hdc, TRANSPARENT);
        let hfont = state.font_for_dpi(dpi);
        let old = hfont.map(|f| SelectObject(hdc, f.into()));
        let _ = SetTextColor(hdc, COLORREF(colors.text.colorref()));
        let mut text: Vec<u16> = state.label.encode_utf16().collect();
        let mut tr = rc;
        let _ = DrawTextW(hdc, &mut text, &mut tr, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        if let Some(o) = old {
            let _ = SelectObject(hdc, o);
        }

        // ヘアライン枠。
        let bb = CreateSolidBrush(COLORREF(colors.border.colorref()));
        let _ = FrameRect(hdc, &rc, bb);
        let _ = DeleteObject(bb.into());

        let _ = EndPaint(hwnd, &ps);
    }
}

/// D2D バックエンドでの描画。SetDpi(96,96) にしてレイアウト値をそのまま DIP として使う。
/// 失敗は握り潰して次フレームに委ねる（panic しない）。BeginPaint/EndPaint は全経路で必ず対にする。
unsafe fn paint_d2d(hwnd: HWND) {
    use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
    use windows::Win32::Graphics::Direct2D::D2D1_DRAW_TEXT_OPTIONS_NONE;
    use windows::Win32::Graphics::DirectWrite::{
        DWRITE_MEASURING_MODE_NATURAL, DWRITE_TEXT_ALIGNMENT_CENTER,
    };

    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let Some(state) = hud_state(hwnd) else {
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

    // テキストフォーマットは Backend 経由（DWrite factory は遅延生成キャッシュ）。
    // HUD は候補窓のフォントサイズではなく、大きな固定サイズ（24pt=240）を使う。
    let family: Vec<u16> = state
        .theme
        .font_family
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let font_px = font_size_px(HUD_FONT_POINT_TENTHS, dpi);
    let Some(fmt) = state.backend.text_format(&family, font_px, DWRITE_TEXT_ALIGNMENT_CENTER, true)
    else {
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

    let label_utf16: Vec<u16> = state.label.encode_utf16().collect();
    if let Some(b) = brush(t.colors.text) {
        ctx.DrawText(
            &label_utf16,
            &fmt,
            &rectf,
            &b,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
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

    // end_draw の失敗がデバイスロスト由来なら renderer_dead を立て、次回 flash で窓を作り直す。
    // renderer（state.backend への不変借用）の最後の使用は end_draw なので、NLL により
    // その後は同じ `state` 借用から可変で書ける（GWLP_USERDATA を二重借用しない）。
    let lost = match renderer.end_draw() {
        Ok(()) => false,
        Err(e) => crate::render::is_device_lost(&e),
    };
    if lost {
        state.backend.renderer_dead = true;
    }
    let _ = EndPaint(hwnd, &ps);
}

/// 自前描画のモード HUD。`hwnd` は遅延生成（初回 `flash` まで null）。
pub struct ModeHud {
    hwnd: HWND,
}

impl ModeHud {
    /// HWND を持たない空の HUD を構築する（`TextService::new` 用）。
    pub fn empty() -> Self {
        Self { hwnd: HWND(std::ptr::null_mut()) }
    }

    /// Deactivate から呼ぶ。プロセス終了時の msctf 後始末（LdrShutdownProcess 中の
    /// IUnknown::Release）で DestroyWindow されると、その WM_NCDESTROY で
    /// SurfaceRenderer（dcomp/d3d11）が drop され、プロセス終了中の dcomp 操作が
    /// dxgi の例外を起こして STATUS_FATAL_USER_CALLBACK_EXCEPTION (c000041d) で
    /// ホストごと落ちる。プロセスが健全な Deactivate 時点で畳んでおけば、
    /// 終了時には hwnd が null で Drop は no-op になる。
    pub fn destroy(&mut self) {
        if !self.hwnd.is_invalid() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
            self.hwnd = HWND(std::ptr::null_mut());
        }
    }

    /// デバイスロスト後の回復。判定・破棄は popup 側の共通処理（候補窓と同じ理由:
    /// NOREDIRECTIONBITMAP 窓は GDI が映らないため破棄→再生成しかない）。
    fn recover_if_device_lost(&mut self) {
        popup::recover_if_device_lost::<HudState>(&mut self.hwnd, "ev=hud_device_lost_recover");
    }

    /// 必要なら HWND を生成する。失敗したら null のまま（劣化動作）。
    /// 2 段バックエンド判定（D2D 試行→GDI 確定）は popup 側の共通処理。判定は初回のみ。
    fn ensure_hwnd(&mut self, theme: crate::theme::Theme) {
        if !self.hwnd.is_invalid() {
            return;
        }
        if popup::register_class(&CLASS_ATOM, CLASS_NAME, Some(wnd_proc)).is_none() {
            return;
        }
        unsafe {
            let (width, height) = hud_window_size(96);

            let Some((hwnd, renderer)) = popup::create_backed_popup(CLASS_NAME, width, height)
            else {
                self.hwnd = HWND(std::ptr::null_mut());
                return;
            };
            self.hwnd = hwnd;

            // 角丸は両パス、アクリルバックドロップは D2D パスのみ。失敗は握り潰す（Win10=no-op）。
            crate::render::apply_dwm_chrome(hwnd, theme.rounded, theme.acrylic && renderer.is_some());

            popup::install_state(
                hwnd,
                Box::new(HudState {
                    label: mode_label_ephemeral(false, false),
                    theme,
                    backend: Backend::new(renderer),
                }),
            );
        }
    }

    /// モード（is_direct）を (x, y) 付近に一瞬表示し、自動消去タイマを (再)武装する。
    /// 連打でもタイマが再 arm されるので出続ける。HWND 生成失敗時は劣化（何もしない）。
    /// `ephemeral`: ephemeral かなモード中（F8 等の一時トリガ中）なら「あ˙」で区別する。
    /// `theme` は候補窓と共有する解決済みテーマ（Task 7: 呼び出し元が flash ごとに
    /// settings/ダークモードを再評価して渡す）。
    pub fn flash(&mut self, is_direct: bool, ephemeral: bool, x: i32, y: i32, theme: crate::theme::Theme) {
        // デバイスロスト（前回 paint で検知）していたら、まず窓を破棄して null に戻す。
        // ensure_hwnd の前に行い、再生成される窓で最新テーマ・chrome を適用しなおす。
        self.recover_if_device_lost();
        self.ensure_hwnd(theme.clone());
        if self.hwnd.is_invalid() {
            return;
        }
        let label = mode_label_ephemeral(is_direct, ephemeral);
        unsafe {
            if let Some(state) = hud_state(self.hwnd) {
                state.label = label;
                // Task 7: DWM chrome に効く属性（角丸/アクリル）が変わったときだけ再適用する
                // （色だけの変化は後段の InvalidateRect による再描画で足りる）。
                let chrome_changed = state.theme.rounded != theme.rounded
                    || state.theme.acrylic != theme.acrylic;
                // 既存 HWND の場合も毎回テーマを更新する（settings 変更が次回 flash から反映）。
                state.theme = theme;
                if chrome_changed {
                    // アクリルは D2D バックエンドのときだけ有効（ensure_hwnd の初回適用と同条件）。
                    let d2d = state.backend.renderer.is_some();
                    crate::render::apply_dwm_chrome(
                        self.hwnd, state.theme.rounded, state.theme.acrylic && d2d,
                    );
                }
            }
        }
        let dpi = popup::window_dpi(self.hwnd);
        let (w, h) = hud_window_size(dpi);
        let (fx, fy) = popup::place_on_monitor(x, y, w, h);
        unsafe {
            let _ = SetWindowPos(self.hwnd, None, fx, fy, w, h, SWP_NOACTIVATE | SWP_NOZORDER);
            // SetWindowPos で決めた client size へ D2D の swapchain を毎回作り直す。
            // ResizeBuffers は同寸なら安価・冪等なので無条件で呼んで問題ない。以前はここに
            // size_changed ガードを置いていたが、その比較対象が SetWindowPos "後" の
            // GetClientRect（＝既に新サイズ）と (w,h) だったため常に偽になり、初回 flash で
            // リサイズが一度も走らず backbuffer が生成時サイズ（hud_window_size(96)=50x50）に
            // 据え置かれた。結果 150% DPI で 74x74 の窓に対し 50x50 バックバッファへ 74 空間で
            // レイアウト＋DXGI_SCALING_STRETCH され、グリフが拡大・クリップされていた。
            // 候補窓 relayout_and_repaint と同じく無条件 resize にしてこの真因を潰す。
            popup::resize_and_invalidate::<HudState>(self.hwnd, w, h);
            // 退場フェード中の再 flash なら遅延 hide タイマを解除して表示を続行する。
            let _ = KillTimer(Some(self.hwnd), HUD_FADE_TIMER_ID);
            let was_visible = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE).as_bool();
            // 出現モーション（新規出現のみ短いフェード。表示中の連打・フェード割り込みは
            // 1.0 へスナップ）。候補窓と共有の popup::play_entrance。
            if let Some(state) = hud_state(self.hwnd) {
                let motion = state.theme.motion;
                popup::play_entrance(state, motion, was_visible);
            }
            // 自動消去タイマを (再)武装。既存があれば止めてから張り直す（連打で出続ける）。
            // 武装に失敗したまま表示し続けると TOPMOST の HUD が永久に残る。一瞬も
            // 出せないことより残留の方が害が大きいので、失敗時は即座に退場させる。
            let _ = KillTimer(Some(self.hwnd), HUD_TIMER_ID);
            if SetTimer(Some(self.hwnd), HUD_TIMER_ID, HUD_DURATION_MS, None) == 0 {
                begin_dismiss(self.hwnd);
            }
        }
    }
}

impl Drop for ModeHud {
    fn drop(&mut self) {
        self.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::hud_window_size;

    #[test]
    fn hud_window_size_is_square_and_dpi_scaled() {
        // HUD_SIDE=48, BORDER=1。96DPI: 2*1 + scale(48,96)=2+48 = 50（正方形）。
        assert_eq!(hud_window_size(96), (50, 50));
        // 192DPI: 2 + scale(48,192)=2 + (48*192+48)/96 = 2 + 9264/96 = 2+96 = 98。
        assert_eq!(hud_window_size(192), (98, 98));
    }
}
