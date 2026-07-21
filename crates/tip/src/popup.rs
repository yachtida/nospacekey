//! 候補ウィンドウ / モード HUD が共有するポップアップ基盤。
//!
//! 両者は「WS_POPUP|TOPMOST|NOACTIVATE の自前描画窓・GWLP_USERDATA に状態・
//! D2D(DComp) 試行→GDI フォールバックの 2 段バックエンド・デバイスロスト回復・
//! DPI スケール・モニタ内クランプ」という同じ骨格を持つ。ここに一本化し、
//! 各ウィンドウは「サイズ計算＋描画」だけを持つ薄い層にする。
//!
//! 方針は既存 2 窓と同じ: TIP パスでは決して panic しない（unwrap/expect 禁止）、
//! 失敗はすべて劣化動作（描かない/GDI へ落ちる）で吸収する。IME は STA なので
//! GWLP_USERDATA 越しの状態アクセスは単一スレッドに直列化される。

use std::sync::OnceLock;
use std::time::Instant;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_CLASS_ALREADY_EXISTS, HWND, POINT, RECT};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection, IDWriteTextFormat,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS, DWRITE_TRIMMING,
    DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, GetDC, GetDeviceCaps, GetMonitorInfoW, InvalidateRect,
    MonitorFromPoint, ReleaseDC, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DEFAULT_PITCH, FF_DONTCARE, FW_NORMAL, HFONT, LOGPIXELSX, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, OUT_TT_PRECIS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GetWindowLongPtrW, RegisterClassW, SetWindowLongPtrW,
    CS_DROPSHADOW, GWLP_USERDATA, HMENU, WINDOW_EX_STYLE, WNDCLASSW, WNDPROC, WS_EX_NOACTIVATE,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOPMOST, WS_POPUP,
};

use crate::render::SurfaceRenderer;
use crate::text_service::tip_log;
use crate::theme::tokens;

// ============================================================================
// 純粋ヘルパ（GDI 非依存・単体テスト可能）。
// ============================================================================

/// 整数丸めの DPI スケール。`(v*dpi + 48)/96`（+48 で四捨五入相当）。
/// `MulDiv` は無効フィーチャ（Win32_System_WindowsProgramming）依存なので使わない。
pub(crate) fn scale(v: i32, dpi: i32) -> i32 {
    (v * dpi + 48) / 96
}

/// `GetDeviceCaps` の結果を妥当な DPI に丸める。<=0 は 96 にフォールバックし、
/// 異常な大値も [96,480] にクランプして暴走を防ぐ。
pub(crate) fn effective_dpi(raw: i32) -> i32 {
    if raw <= 0 {
        96
    } else {
        raw.clamp(96, 480)
    }
}

/// ポイントサイズ（10 倍値）と縦 DPI から論理フォント高（負値=文字高）を求める。
/// `-MulDiv(pt, dpiY, 72)` を 10.5pt 用に一般化した整数丸め式。例: (105, 96) -> -14。
pub(crate) fn font_height_for_dpi(point_tenths: i32, dpi_y: i32) -> i32 {
    -((point_tenths * dpi_y + 360) / 720)
}

/// ポイント×10 と DPI から DWrite に渡すフォントサイズ（物理 px）を求める。
/// D2D 側は SetDpi(96,96) で px==DIP 扱いのため、pt のままではなく px 換算が必要
/// （pt/72*dpi。GDI の font_height_for_dpi と同じ換算）。
pub(crate) fn font_size_px(point_tenths: i32, dpi: i32) -> f32 {
    point_tenths as f32 / 10.0 * dpi as f32 / 72.0
}

/// 画面（モニタ作業領域 `work`）に収まるよう、ウィンドウ左上 (x, y) を補正する純粋関数。
/// 右/下にはみ出すなら端へ寄せ、その後あらためて左/上を作業領域内へクランプする。
pub(crate) fn fit_to_work_area(x: i32, y: i32, w: i32, h: i32, work: RECT) -> (i32, i32) {
    let nx = (if x + w > work.right { work.right - w } else { x }).max(work.left);
    let ny = (if y + h > work.bottom { work.bottom - h } else { y }).max(work.top);
    (nx, ny)
}

/// キャレット下に出す窓の配置。下端はみ出し時、単純クランプは上へずり上がって
/// 入力中の行を覆うため、キャレット上端 `caret_top` が分かっていて上側に収まるなら
/// キャレット直上（bottom = caret_top）へフリップする。それ以外は従来クランプへ劣化。
pub(crate) fn fit_below_or_flip_above(
    x: i32,
    y: i32,
    caret_top: Option<i32>,
    w: i32,
    h: i32,
    work: RECT,
) -> (i32, i32) {
    if y + h > work.bottom {
        if let Some(top) = caret_top {
            if top - h >= work.top {
                let (nx, _) = fit_to_work_area(x, y, w, h, work);
                return (nx, top - h);
            }
        }
    }
    fit_to_work_area(x, y, w, h, work)
}

// ============================================================================
// GDI/OS ヘルパ。
// ============================================================================

/// テーマのフォントファミリを NUL 終端 UTF-16 にする（CreateFontW / DWrite 共用）。
pub(crate) fn family_utf16z(family: &str) -> Vec<u16> {
    family.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 指定ファミリ・ポイントサイズ・DPI 用の GDI フォントを生成する。失敗時は invalid な HFONT。
/// ファミリは theme.font_family 由来（D2D パスの DWrite と同じ設定値を使い、
/// GDI フォールバック・幅測定とでフォントが食い違わないようにする）。
pub(crate) unsafe fn create_font(family_z: &[u16], point_tenths: i32, dpi: i32) -> HFONT {
    let cheight = font_height_for_dpi(point_tenths, dpi);
    CreateFontW(
        cheight,
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_TT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        (DEFAULT_PITCH.0 as u32) | ((FF_DONTCARE.0 as u32) << 4),
        PCWSTR(family_z.as_ptr()),
    )
}

/// hwnd の横 DPI を読む（描画・メトリクスと単一の DPI 軸で整合させる）。取れなければ 96。
pub(crate) fn window_dpi(hwnd: HWND) -> i32 {
    unsafe {
        let hdc = GetDC(Some(hwnd));
        if hdc.is_invalid() {
            return 96;
        }
        let dpi = effective_dpi(GetDeviceCaps(Some(hdc), LOGPIXELSX));
        let _ = ReleaseDC(Some(hwnd), hdc);
        dpi
    }
}

/// (x, y) を希望位置に、その点が属すモニタの作業領域内へ収めた左上座標を返す。
/// モニタ情報が取れなければ素通し（劣化）。
pub(crate) fn place_on_monitor(x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
    unsafe {
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let hmon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            fit_to_work_area(x, y, w, h, mi.rcWork)
        } else {
            (x, y)
        }
    }
}

/// `place_on_monitor` のキャレットフリップ版（候補窓用）。下端はみ出し時、`caret_top` が
/// あればキャレット直上へフリップして入力行を覆わない。モニタ情報が取れなければ素通し。
pub(crate) fn place_on_monitor_flipped(
    x: i32,
    y: i32,
    caret_top: Option<i32>,
    w: i32,
    h: i32,
) -> (i32, i32) {
    unsafe {
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let hmon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            fit_below_or_flip_above(x, y, caret_top, w, h, mi.rcWork)
        } else {
            (x, y)
        }
    }
}

// ============================================================================
// ウィンドウクラス登録・生成。
// ============================================================================

/// ウィンドウクラスを（未登録なら）登録してアトムを返す。失敗時は None。
/// クラススタイルは共通で `CS_DROPSHADOW`（Apple 風 --shadow-float の視覚語彙。
/// DWM 角丸(Win11)の付随シャドウが無い環境=square 設定/Win10/GDI でも影が出る）。
pub(crate) fn register_class(
    atom_cell: &'static OnceLock<u16>,
    class_name: PCWSTR,
    wnd_proc: WNDPROC,
) -> Option<u16> {
    if let Some(atom) = atom_cell.get() {
        return Some(*atom);
    }
    unsafe {
        let hinstance = GetModuleHandleW(None).ok()?;
        let wc = WNDCLASSW {
            style: CS_DROPSHADOW,
            lpfnWndProc: wnd_proc,
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        let atom = RegisterClassW(&wc);
        if atom == 0 {
            // 既登録による失敗（別インスタンスが先に登録した等）だけはダミーのアトムを
            // 記録して以降の再登録を避ける（CreateWindow はクラス名で引ける）。
            // それ以外の失敗をキャッシュすると一時的な失敗が恒久化し、以後ポップアップが
            // 二度と出なくなるため、キャッシュせず次回の表示で再試行する。
            if GetLastError() == ERROR_CLASS_ALREADY_EXISTS {
                let _ = atom_cell.set(1);
                return atom_cell.get().copied();
            }
            return None;
        }
        let _ = atom_cell.set(atom);
        Some(atom)
    }
}

/// 指定 ex-style でポップアップを CreateWindowExW する。失敗時は None（劣化動作）。
/// 位置は 0,0 の仮置き（実寸・実位置は呼び出し側の relayout で content-fit に直す）。
pub(crate) unsafe fn create_popup(
    class_name: PCWSTR,
    ex_style: WINDOW_EX_STYLE,
    width: i32,
    height: i32,
) -> Option<HWND> {
    let hinstance = GetModuleHandleW(None).ok();
    match CreateWindowExW(
        ex_style,
        class_name,
        w!("nospacekey"),
        // WS_BORDER は外し、枠は各ウィンドウが自前描画する。
        WS_POPUP,
        0,
        0,
        width,
        height,
        None,
        None::<HMENU>,
        hinstance.map(|h| h.into()),
        None,
    ) {
        Ok(hwnd) => Some(hwnd),
        Err(e) => {
            // 診断: AppContainer 等で CreateWindowExW が失敗するケースを可視化。
            tip_log(&format!("ev=popup_create_err hr=0x{:08X}", e.code().0));
            None
        }
    }
}

/// 2 段バックエンド判定つきの生成。まず DComp 前提（WS_EX_NOREDIRECTIONBITMAP 付き）で
/// 生成して D2D レンダラを試す。NOREDIRECTIONBITMAP 窓には GDI 描画が一切映らない
/// （リダイレクションサーフェスが無い）ため「同じ HWND で D2D 失敗→GDI」はできず、
/// 失敗時はフラグ無しで作り直して GDI フォールバックに確定する。判定は初回のみ（以後固定）。
pub(crate) unsafe fn create_backed_popup(
    class_name: PCWSTR,
    width: i32,
    height: i32,
) -> Option<(HWND, Option<SurfaceRenderer>)> {
    let hwnd = create_popup(
        class_name,
        WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
        width,
        height,
    )?;
    let renderer = SurfaceRenderer::new(hwnd, width as u32, height as u32).ok();
    if renderer.is_some() {
        return Some((hwnd, renderer));
    }
    let _ = DestroyWindow(hwnd);
    let hwnd = create_popup(class_name, WS_EX_TOPMOST | WS_EX_NOACTIVATE, width, height)?;
    Some((hwnd, None))
}

// ============================================================================
// HWND ごとの状態（GWLP_USERDATA）と共有バックエンド。
// ============================================================================

/// 各ウィンドウの状態が共有バックエンドへアクセスするためのフック。
/// デバイスロスト回復・リサイズなどの共通処理がこの trait 越しに動く。
pub(crate) trait PopupState {
    fn backend_mut(&mut self) -> &mut Backend;
}

/// D2D/GDI 描画バックエンドと、paint をまたいで保持する GDI/DWrite リソース。
/// ウィンドウ状態（候補列・ラベル等）から描画基盤を分離したもの。Drop で所有フォントを解放する。
pub(crate) struct Backend {
    /// D2D バックエンド。None なら GDI フォールバック。初回生成時に一度だけ決める。
    pub renderer: Option<SurfaceRenderer>,
    /// D2D パス用 DWrite factory（遅延生成、HWND ごと。STA なので共有不要）。
    /// COM ポインタは Sync でないため static OnceLock は不可。
    dwrite: Option<IDWriteFactory>,
    /// paint の end_draw がデバイスロスト（GPU リセット/TDR/RDP/スリープ復帰）を返したら
    /// true。次回表示でこのフラグを読み取り、窓を破棄して D2D レンダラを作り直す。
    /// WndProc は &self を持てないため、paint からは GWLP_USERDATA 側のここへ書くしかない。
    pub renderer_dead: bool,
    /// (HFONT, DPI, ポイント×10, ファミリ)。キーのどれかが変わったら作り直す。GDI パスのみ使用。
    /// DPI だけでなくサイズ・ファミリもキーに含める（settings 変更が既存キャッシュに勝つように）。
    font: Option<(HFONT, i32, i32, Vec<u16>)>,
    /// D2D パス用 IDWriteTextFormat のキャッシュ。(px の to_bits, align, trim, ファミリ) がキー。
    /// CreateTextFormat はフォント名解決を伴うので毎フレーム生成せず使い回す。
    /// ファミリ/サイズが変わったら古いエントリは捨てる（残るのは align/trim 違いの数個だけ）。
    fmt_cache: Vec<(u32, i32, bool, Vec<u16>, IDWriteTextFormat)>,
    /// 出現フェードの開始時刻。フェードイン途中の退場で現在の不透明度を推定し、
    /// 1.0 からではなくそこからフェードアウトする（スナップの瞬き防止）。
    pub fade_in_started: Option<Instant>,
    /// 退場フェード進行中フラグ。hide/dismiss の再入で 1.0 へ跳ね直さないためのガード。
    /// 遅延 hide タイマの発火（実際の SW_HIDE）と出現処理（play_entrance）で解除する。
    pub fading_out: bool,
}

impl Backend {
    pub fn new(renderer: Option<SurfaceRenderer>) -> Self {
        Self {
            renderer,
            dwrite: None,
            renderer_dead: false,
            font: None,
            fmt_cache: Vec::new(),
            fade_in_started: None,
            fading_out: false,
        }
    }

    /// 与えたファミリ・ポイントサイズ・DPI に合う GDI フォントを返す。キーが変わっていれば
    /// 作り直す。取得できなければ None（system font にフォールバック）。
    pub unsafe fn font_for_dpi(
        &mut self,
        family_z: &[u16],
        point_tenths: i32,
        dpi: i32,
    ) -> Option<HFONT> {
        if let Some((hfont, c_dpi, c_pt, c_fam)) = self.font.as_ref() {
            if *c_dpi == dpi && *c_pt == point_tenths && c_fam == family_z && !hfont.is_invalid() {
                return Some(*hfont);
            }
            if !hfont.is_invalid() {
                let _ = DeleteObject((*hfont).into());
            }
            self.font = None;
        }
        let hfont = create_font(family_z, point_tenths, dpi);
        if hfont.is_invalid() {
            None
        } else {
            self.font = Some((hfont, dpi, point_tenths, family_z.to_vec()));
            Some(hfont)
        }
    }

    /// D2D パス用のテキストフォーマットを返す（factory・フォーマットとも遅延生成キャッシュ）。
    /// 垂直中央そろえ・ja-jp 固定。ファミリ/サイズが変わったら古いキャッシュを捨てる。
    /// `trim_ellipsis=false` はトリミング無し（読みモニタの末尾寄せ用 — トリミングは整列と
    /// 無関係に**末尾**を削るため、TRAILING でも最新の読みが…で消える。クリップだけに任せる）。
    pub unsafe fn text_format(
        &mut self,
        family_utf16z: &[u16],
        font_px: f32,
        align: DWRITE_TEXT_ALIGNMENT,
        trim_ellipsis: bool,
    ) -> Option<IDWriteTextFormat> {
        let px_bits = font_px.to_bits();
        if let Some((_, _, _, _, fmt)) = self.fmt_cache.iter().find(|(b, a, t, fam, _)| {
            *b == px_bits && *a == align.0 && *t == trim_ellipsis && fam == family_utf16z
        }) {
            return Some(fmt.clone());
        }
        if self.dwrite.is_none() {
            self.dwrite = DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED).ok();
        }
        let f = self.dwrite.as_ref()?;
        let fmt = f
            .CreateTextFormat(
                PCWSTR(family_utf16z.as_ptr()),
                None::<&IDWriteFontCollection>,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                font_px,
                w!("ja-jp"),
            )
            .ok()?;
        let _ = fmt.SetTextAlignment(align);
        let _ = fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
        // 既定の word-wrap のままにしない: 行 RECT は常に 1 行分の高さで、幅 MAX_W クランプを
        // 超える長文候補が下の行へ折り返して滲む（GDI パスの DT_SINGLELINE|DT_END_ELLIPSIS と
        // 非対称になる）。無折返し＋文字単位の省略記号で 1 行に収める。trimming sign の生成
        // 失敗は無視してよい（NO_WRAP だけでも折返し滲みは消え、はみ出しは DrawText 側の
        // CLIP が抑える）。
        let _ = fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
        if trim_ellipsis {
            if let Ok(sign) = f.CreateEllipsisTrimmingSign(&fmt) {
                let trimming = DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                };
                let _ = fmt.SetTrimming(&trimming, &sign);
            }
        }
        // ファミリ/サイズが変わったら旧エントリは以後ヒットしないので捨てる。
        self.fmt_cache
            .retain(|(b, _, _, fam, _)| *b == px_bits && fam == family_utf16z);
        self.fmt_cache
            .push((px_bits, align.0, trim_ellipsis, family_utf16z.to_vec(), fmt.clone()));
        Some(fmt)
    }

    /// D2D パス用: テキスト列の最大実測幅（物理 px、切り上げ）を DWrite で測る。
    /// 描画（DrawText）と同一エンジン・同一フォーマットなので、GDI 測定とのメトリクス差で
    /// 本文が切れたり余ったりしない。`cap` に達したら打ち切る（幅は MAX_W でクランプされるため）。
    /// factory/format が用意できなければ None（呼び出し側は GDI 測定へ劣化）。
    pub unsafe fn measure_max_width_dwrite(
        &mut self,
        family_utf16z: &[u16],
        font_px: f32,
        texts: &[String],
        cap: i32,
    ) -> Option<i32> {
        let fmt = self.text_format(family_utf16z, font_px, DWRITE_TEXT_ALIGNMENT_LEADING, true)?;
        let f = self.dwrite.as_ref()?;
        let mut max_w = 0i32;
        for t in texts {
            let utf16: Vec<u16> = t.encode_utf16().collect();
            let Ok(layout) = f.CreateTextLayout(&utf16, &fmt, f32::MAX, f32::MAX) else {
                continue;
            };
            let mut m = DWRITE_TEXT_METRICS::default();
            if layout.GetMetrics(&mut m).is_ok() {
                let w = m.widthIncludingTrailingWhitespace.ceil() as i32;
                if w > max_w {
                    max_w = w;
                    if max_w >= cap {
                        break;
                    }
                }
            }
        }
        Some(max_w)
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Some((hfont, _, _, _)) = self.font.take() {
            if !hfont.is_invalid() {
                unsafe {
                    let _ = DeleteObject(hfont.into());
                }
            }
        }
    }
}

/// `GWLP_USERDATA` に格納した状態 `T` を可変借用する。未設定なら None。
/// IME は STA なので、この借用が他と重複しないことは呼び出し文脈で保証される。
pub(crate) unsafe fn state_mut<'a, T>(hwnd: HWND) -> Option<&'a mut T> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut T;
    if ptr.is_null() {
        None
    } else {
        Some(&mut *ptr)
    }
}

/// 状態 `T` を Box で確保して `GWLP_USERDATA` に格納する（WM_NCDESTROY で take_state する）。
pub(crate) unsafe fn install_state<T>(hwnd: HWND, state: Box<T>) {
    let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
}

/// WM_NCDESTROY 用: `GWLP_USERDATA` の状態を回収して所有権ごと返す（呼び出し側で drop）。
/// Backend の Drop が所有フォントを解放する。
pub(crate) unsafe fn take_state<T>(hwnd: HWND) -> Option<Box<T>> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut T;
    if ptr.is_null() {
        return None;
    }
    let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    Some(Box::from_raw(ptr))
}

/// デバイスロスト後の回復: 前回 paint で renderer_dead が立っていたら窓を破棄して
/// `*hwnd` を null に戻す（直後の ensure_hwnd が 2 段バックエンドで作り直す）。
/// NOREDIRECTIONBITMAP 窓は GDI が映らないため SetTarget(None) では復帰できず、
/// 破棄→再生成しかない。DestroyWindow の「前」にフラグを読む（WM_NCDESTROY で state が
/// 消える前に回収）。破棄失敗時は窓を残して次回表示で再試行（panic せず劣化）。
pub(crate) fn recover_if_device_lost<T: PopupState>(hwnd: &mut HWND, log_event: &str) {
    if hwnd.is_invalid() {
        return;
    }
    let dead = unsafe {
        state_mut::<T>(*hwnd)
            .map(|s| s.backend_mut().renderer_dead)
            .unwrap_or(false)
    };
    if !dead {
        return;
    }
    tip_log(log_event);
    unsafe {
        if DestroyWindow(*hwnd).is_ok() {
            *hwnd = HWND(std::ptr::null_mut());
        }
    }
}

/// D2D パスの swapchain を新サイズへ作り直してから InvalidateRect する。
/// resize 失敗はそのフレームの描画をスキップ（panic せず劣化。次回表示で再試行される）。
pub(crate) unsafe fn resize_and_invalidate<T: PopupState>(hwnd: HWND, width: i32, height: i32) {
    let mut skip_paint = false;
    if let Some(state) = state_mut::<T>(hwnd) {
        if let Some(r) = state.backend_mut().renderer.as_mut() {
            if r.resize(width as u32, height as u32).is_err() {
                skip_paint = true;
            }
        }
    }
    if !skip_paint {
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

// ============================================================================
// 出現/退場フェードの共通処理（候補窓・HUD 共有）。
// ============================================================================

/// 退場処理の指示。呼び出し側（hide/dismiss）はこれに従って SetTimer / SW_HIDE する。
pub(crate) enum FadeOut {
    /// 既に退場フェード中（遅延 hide タイマも武装済み）。何もしない。
    AlreadyFading,
    /// フェードを開始した。この ms 後に SW_HIDE する遅延タイマを張ること。
    Fade(u32),
    /// フェード不可（GDI/reduced-motion/不透明度ほぼ 0）。即時 SW_HIDE すること。
    Immediate,
}

/// 出現演出。新規出現（was_visible=false）かつ motion かつ opacity 対応ならフェードイン、
/// それ以外（表示中の内容更新・フェード割り込み・reduced-motion）は 1.0 へスナップして
/// 表示を待たせない。退場フラグはどちらでも解除する（show はフェードアウトを打ち切る）。
pub(crate) fn play_entrance<T: PopupState>(state: &mut T, motion: bool, was_visible: bool) {
    let b = state.backend_mut();
    b.fading_out = false;
    let Some(r) = b.renderer.as_ref() else {
        b.fade_in_started = None;
        return;
    };
    if motion && !was_visible && r.supports_opacity() && r
        .animate_opacity(0.0, 1.0, tokens::MOTION_IN_MS)
        .is_ok()
    {
        b.fade_in_started = Some(Instant::now());
    } else {
        let _ = r.set_opacity(1.0);
        b.fade_in_started = None;
    }
}

/// フェードイン開始からの進行率 progress（elapsed/MOTION_IN_MS）→ 画面上の不透明度。
/// 線形 progress をそのまま返してはならない: 実際の出現アニメは renderer::animate_opacity
/// のイーズアウト多項式 f(x)=2x−x² で進むため、線形推定だと中間で最大 0.25 明るい実値から
/// 暗い推定値へ張り直され、退場開始時に下向きのスナップが見える。
pub(crate) fn entrance_opacity(progress: f64) -> f32 {
    let x = progress.clamp(0.0, 1.0);
    (x * (2.0 - x)) as f32
}

/// 退場フェードを開始する。フェードイン途中なら経過時間から現在の不透明度を推定し、
/// 1.0 からではなくそこから対称に帰る（スナップの瞬き防止。時間も比例で縮める）。
/// 再入（既にフェードアウト中）は AlreadyFading を返し、呼び出し側は何もしない。
pub(crate) fn begin_fade_out<T: PopupState>(state: &mut T, motion: bool) -> FadeOut {
    let b = state.backend_mut();
    if b.fading_out {
        return FadeOut::AlreadyFading;
    }
    let Some(r) = b.renderer.as_ref() else {
        return FadeOut::Immediate;
    };
    if !motion || !r.supports_opacity() {
        return FadeOut::Immediate;
    }
    let from = match b.fade_in_started {
        Some(t0) => entrance_opacity((t0.elapsed().as_secs_f64() * 1000.0) / tokens::MOTION_IN_MS),
        None => 1.0,
    };
    if from <= 0.05 {
        // まだほぼ透明（フェードイン直後の即キャンセル）。フェードする意味がない。
        return FadeOut::Immediate;
    }
    let dur = tokens::MOTION_OUT_MS * from as f64;
    if r.animate_opacity(from, 0.0, dur).is_err() {
        return FadeOut::Immediate;
    }
    b.fading_out = true;
    b.fade_in_started = None;
    FadeOut::Fade(dur as u32 + 40)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_format_is_single_line_with_ellipsis_trimming() {
        // GDI パスは DT_SINGLELINE|DT_END_ELLIPSIS で候補を必ず 1 行に収める。DWrite の
        // 既定は word-wrap 有効のため、幅 MAX_W クランプを超える長文候補が行 RECT の下へ
        // 折り返して次の候補行/フッターに滲む。フォーマット側で無折返し＋省略記号に固定し、
        // 両バックエンドの見た目を対称にする。
        use windows::Win32::Graphics::DirectWrite::{
            DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
        };
        let mut b = Backend::new(None);
        let fam = family_utf16z("Yu Gothic UI");
        let Some(fmt) = (unsafe { b.text_format(&fam, 14.0, DWRITE_TEXT_ALIGNMENT_LEADING, true) })
        else {
            panic!("DWrite factory が使えない環境（このテストは実 DWrite 前提）");
        };
        unsafe {
            assert_eq!(fmt.GetWordWrapping(), DWRITE_WORD_WRAPPING_NO_WRAP);
            let mut trimming = DWRITE_TRIMMING::default();
            let mut sign = None;
            fmt.GetTrimming(&mut trimming, &mut sign).unwrap();
            assert_eq!(trimming.granularity, DWRITE_TRIMMING_GRANULARITY_CHARACTER);
            assert!(sign.is_some(), "省略記号の trimming sign が未設定");
        }
    }

    #[test]
    fn text_format_without_trimming_clips_instead_of_ellipsis() {
        // 読みモニタの末尾寄せ（overflow）用: トリミングは整列と無関係に末尾を削るため、
        // TRAILING+トリミング有りでは最新の読み（末尾）が…で消える。トリミング無し+
        // D2D クリップで頭側が切れる構成が必要。
        use windows::Win32::Graphics::DirectWrite::{
            DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_NONE,
            DWRITE_WORD_WRAPPING_NO_WRAP,
        };
        let mut b = Backend::new(None);
        let fam = family_utf16z("Yu Gothic UI");
        let Some(fmt) =
            (unsafe { b.text_format(&fam, 14.0, DWRITE_TEXT_ALIGNMENT_TRAILING, false) })
        else {
            panic!("DWrite factory が使えない環境（このテストは実 DWrite 前提）");
        };
        unsafe {
            assert_eq!(fmt.GetWordWrapping(), DWRITE_WORD_WRAPPING_NO_WRAP);
            let mut trimming = DWRITE_TRIMMING::default();
            let mut sign = None;
            fmt.GetTrimming(&mut trimming, &mut sign).unwrap();
            assert_eq!(trimming.granularity, DWRITE_TRIMMING_GRANULARITY_NONE);
        }
    }

    #[test]
    fn entrance_opacity_matches_renderer_ease_out_curve() {
        // 出現アニメは renderer::animate_opacity のイーズアウト多項式 f(x)=2x−x² で進む
        // （x=経過/所要）。退場開始時の現在値推定がこれと一致しないと、フェードイン途中の
        // hide で「実際の不透明度→推定値」への下向きスナップが見える（線形推定だと
        // x=0.5 で 0.75→0.5、最大 0.25 の瞬き）。
        assert_eq!(entrance_opacity(0.0), 0.0);
        assert!((entrance_opacity(0.25) - 0.4375).abs() < 1e-6);
        assert!((entrance_opacity(0.5) - 0.75).abs() < 1e-6);
        assert_eq!(entrance_opacity(1.0), 1.0);
        // 完了後・開始前はクランプ（負や 1 超の progress でも不透明度は [0,1]）。
        assert_eq!(entrance_opacity(1.7), 1.0);
        assert_eq!(entrance_opacity(-0.1), 0.0);
    }

    #[test]
    fn scale_rounds_to_nearest() {
        assert_eq!(scale(28, 96), 28); // 等倍
        assert_eq!(scale(28, 192), 56); // 2倍
        assert_eq!(scale(28, 144), 42); // 1.5倍
        assert_eq!(scale(10, 120), 13); // (10*120+48)/96 = 1248/96 = 13
        assert_eq!(scale(1, 96), 1);
    }

    #[test]
    fn effective_dpi_clamps() {
        assert_eq!(effective_dpi(0), 96);
        assert_eq!(effective_dpi(-50), 96);
        assert_eq!(effective_dpi(96), 96);
        assert_eq!(effective_dpi(192), 192);
        assert_eq!(effective_dpi(10), 96); // 下限
        assert_eq!(effective_dpi(100000), 480); // 上限
    }

    #[test]
    fn font_height_matches_minus_muldiv() {
        // 10.5pt @96 -> -14
        assert_eq!(font_height_for_dpi(105, 96), -14);
        // 10.5pt @192 -> -((105*192+360)/720) = -(20520/720) = -28
        assert_eq!(font_height_for_dpi(105, 192), -28);
        // 10.5pt @144 -> -21（丸めが効く境界）。
        assert_eq!(font_height_for_dpi(105, 144), -21);
        // 10.5pt @120 -> -18。
        assert_eq!(font_height_for_dpi(105, 120), -18);
        // HUD の 24.0pt(=240)。24pt @96 -> -32、@192 -> -64。
        assert_eq!(font_height_for_dpi(240, 96), -32);
        assert_eq!(font_height_for_dpi(240, 192), -64);
    }

    #[test]
    fn font_size_px_matches_gdi_conversion() {
        // 10.5pt @96dpi = 14px（font_height_for_dpi(105,96)==-14 と同値）。
        assert!((font_size_px(105, 96) - 14.0).abs() < 1e-3);
        // DPI 200% で倍。
        assert!((font_size_px(105, 192) - 28.0).abs() < 1e-3);
        // 24pt @96dpi = 32px（HUD サイズ）。
        assert!((font_size_px(240, 96) - 32.0).abs() < 1e-3);
    }

    #[test]
    fn fit_to_work_area_keeps_window_on_screen() {
        let work = RECT { left: 0, top: 0, right: 1000, bottom: 800 };
        // 収まる位置はそのまま。
        assert_eq!(fit_to_work_area(100, 100, 200, 300, work), (100, 100));
        // 右はみ出しは右端に寄せる。
        assert_eq!(fit_to_work_area(900, 100, 200, 300, work), (800, 100));
        // 下はみ出しは下端に寄せる。
        assert_eq!(fit_to_work_area(100, 700, 200, 300, work), (100, 500));
        // 両方。
        assert_eq!(fit_to_work_area(900, 700, 200, 300, work), (800, 500));
        // ウィンドウが作業領域より広い場合は左/上端にクランプ（負座標にしない）。
        let small = RECT { left: 0, top: 0, right: 100, bottom: 100 };
        assert_eq!(fit_to_work_area(90, 90, 200, 200, small), (0, 0));
        // 作業領域が原点以外（マルチモニタ）でも左/上端へクランプ。
        let off = RECT { left: 1920, top: 0, right: 2920, bottom: 800 };
        assert_eq!(fit_to_work_area(1800, 100, 200, 300, off), (1920, 100));
    }

    #[test]
    fn flip_above_caret_when_bottom_overflows() {
        let work = RECT { left: 0, top: 0, right: 1000, bottom: 800 };
        // 収まる位置はフリップしない（キャレット直下のまま）。
        assert_eq!(fit_below_or_flip_above(100, 100, Some(80), 200, 300, work), (100, 100));
        // 下端はみ出し＋キャレット上端あり → キャレット直上（bottom=caret_top）へフリップ。
        // クランプなら (100, 500) でキャレット行（y=680..700）を覆うところ。
        assert_eq!(
            fit_below_or_flip_above(100, 700, Some(680), 200, 300, work),
            (100, 380)
        );
        // フリップ時も横ははみ出しクランプが効く。
        assert_eq!(
            fit_below_or_flip_above(900, 700, Some(680), 200, 300, work),
            (800, 380)
        );
        // キャレット上端が不明（既定座標フォールバック等）は従来のクランプへ劣化。
        assert_eq!(fit_below_or_flip_above(100, 700, None, 200, 300, work), (100, 500));
        // 上側にも収まらない（画面が低い/窓が高い）ならクランプへ劣化。
        let short = RECT { left: 0, top: 0, right: 1000, bottom: 320 };
        assert_eq!(
            fit_below_or_flip_above(100, 310, Some(290), 200, 300, short),
            (100, 20)
        );
    }
}
