//! 自前描画の候補ウィンドウ（Win32 popup）。
//!
//! TSF の候補 UI（ITfCandidateListUIElement 等）は使わず、`WS_POPUP` のトップレベル
//! popup を自分で `CreateWindowExW` し、`WM_PAINT` で各候補を 1 行ずつ描画する。
//!
//! 見た目は Apple 風 UI パターン集（docs/apple-design-ui-patterns.md）の視覚語彙に揃えた
//! フラットなカード（既定色は settings の内蔵パレット＝同文書のトークン由来）:
//! - 白（light: #FFFFFF / dark: #2C2C2E）のカード地に、`FrameRect` で自前の 1px
//!   ヘアライン枠を描く（`WS_BORDER` は外し、枠色を完全に制御する）。
//! - 各行は 28dp（96DPI 基準）の高さで、左に右寄せの番号ガター（text-sub 灰）、その右に
//!   候補本文（#1D1D1F）を 2 カラムで描く。`DrawTextW(DT_SINGLELINE|DT_VCENTER)` により
//!   行高/DPI に依らず縦中央そろえになる。
//! - 選択行は唯一の手がかりとしてアクセント青（systemBlue #0071E3）で塗り（D2D パスは
//!   `--radius-sm` 相当の角丸ピル）、本文は白・番号は淡青で描く。
//! - ウィンドウクラスに `CS_DROPSHADOW` を付け、浮遊面（`--shadow-float` 相当）の影を出す。
//! - 複数ページ（総件数 > MAX_VISIBLE_ROWS）ではヘアライン区切り＋「n / 総数」のフッターを出す。
//! - 行のマウスクリックで候補を選択できる（WM_LBUTTONDOWN。表示状態と cand_state の両方を
//!   更新し、プリエディットには触れない＝矢印キー選択と同じ副作用範囲）。
//! - D2D パスでは出現/退場を短いフェード（DComp 駆動・theme.motion）で対称に演出する。
//!
//! HWND ライフサイクル・バックエンド判定・デバイスロスト回復などの共通骨格は
//! `popup.rs`（モード HUD と共有）に置き、この module はレイアウト計算と描画だけを持つ。
//! - フォントは theme.font_family（既定 Yu Gothic UI）・theme の pt を
//!   `CreateFontW(CLEARTYPE_QUALITY, DEFAULT_CHARSET)` で生成し（CJK を綺麗にアンチエイリアス）、
//!   HWND ごとの `WindowState` に (DPI, pt, ファミリ) キーでキャッシュする。
//! - 横幅は各候補の実測幅に chrome（枠・パディング・番号ガター・間隔）を加えた総幅を、
//!   [min,max] にクランプして決める。測定は描画と同じエンジンで行う（D2D パスは DWrite、
//!   GDI フォールバックは `GetTextExtentPoint32W`）— エンジン差で本文が切れないように。
//! - すべての画素メトリクスは `GetDeviceCaps(hdc, LOGPIXELSX)` から得た DPI で
//!   整数丸めスケールする（`MulDiv` は無効フィーチャ依存なので使わない）。
//!
//! ウィンドウクラスは初回だけ `RegisterClassW` し、アトムを `OnceLock` に保持する。
//! `TextService::new()` 時点では HWND は null（未生成）でよく、最初の `show` で遅延生成する。
//!
//! 描画は WndProc（`&self` を持てない）から行うため、候補リスト・選択位置・フォントは
//! HWND ごとの `WindowState` にまとめ、`GWLP_USERDATA` に格納して `WM_PAINT` で読み戻す。
//! thread-global なシングルトンを使わないので、同一スレッドに複数ウィンドウが同居しても
//! 互いの状態を壊さない。IME は STA（単一スレッド）なのでこのアクセスは直列化される。
//! `WindowState`（フォント含む）は `WM_NCDESTROY` で回収・破棄する。表示位置は呼び出し側の
//! (x, y) を基準に、モニタの作業領域内へクランプして使う。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use crate::candidate_state::CandidateState;
use crate::popup::{self, Backend, PopupState};
use crate::text_service::tip_log;
use crate::theme::tokens;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, GetDC,
    GetDeviceCaps, GetTextExtentPoint32W, InvalidateRect, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
    HFONT, LOGPIXELSX, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, GetClientRect, IsWindowVisible, KillTimer, SetTimer,
    SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE,
    WM_LBUTTONDOWN, WM_NCDESTROY, WM_PAINT, WM_TIMER,
};

// ポップアップ共有基盤（popup.rs）の純ヘルパ。旧来この module にあったものを移設した。
pub(crate) use crate::popup::{effective_dpi, font_size_px, scale};

/// 候補窓のアンカー。`(x, y)` は窓左上の希望位置（キャレット左下）。`caret_top` は
/// キャレット矩形の上端で、画面下端はみ出し時にキャレット直上へフリップする基準。
/// 既定座標フォールバック等で不明なら None（従来のクランプ配置へ劣化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaretAnchor {
    pub x: i32,
    pub y: i32,
    pub caret_top: Option<i32>,
}

/// 候補 UI の最小インタフェース。`TextService` 側はこれ越しに使う（テスト/差し替え容易化）。
pub trait CandidateUI {
    /// `theme` は呼び出しごとに settings 由来で解決済みのもの（Task 7: 表示のたびに
    /// mtime/ダークモードを再評価するため、構築時固定ではなく show ごとに受け取る）。
    fn show(
        &mut self,
        candidates: &[String],
        selected: usize,
        anchor: CaretAnchor,
        theme: crate::theme::Theme,
    );
    fn hide(&mut self);
    fn selected(&self) -> usize;
    fn move_selection(&mut self, delta: i32);
}

const CLASS_NAME: PCWSTR = w!("NospacekeyCandidateWindow");

// --- 96 DPI 基準のデザインメトリクス（実行時に DPI でスケールする） ---
/// 1 行の高さ（dp）。Win11 風にゆったりめ。
const ROW_HEIGHT: i32 = 28;
/// カード内側の左右パディング（dp）。
const PADDING_X: i32 = 12;
/// カード上下のパディング（dp）。
const PADDING_Y: i32 = 4;
/// 番号ガターの幅（dp）。番号はこの中で右寄せ。描かれる番号はページ内相対の 1–9
/// （Bug 4: 数字キーと一致させる）で常に 1 桁なので、固定幅で足りる。
const NUMBER_COL_W: i32 = 22;
/// 番号ガターと本文の間隔（dp）。
const GAP: i32 = 8;
/// 内容に合わせた横幅のクランプ下限/上限（dp）。
const MIN_W: i32 = 160;
const MAX_W: i32 = 420;
/// 一度に表示する最大行数。これを超えるとページングし、選択を含むページのみ描画する。
/// ページングが無いと数十候補でウィンドウが画面高を越えてしまうため上限を設ける。
/// Bug 4: 数字キー選択もこのページ単位で解釈するため key_event_sink から参照する。
pub(crate) const MAX_VISIBLE_ROWS: usize = 9;
/// ページングが要る（総件数 > MAX_VISIBLE_ROWS）ときだけ出すフッター（選択位置
/// インジケータ「n / 総数」）の高さ（dp）。上辺にヘアライン区切りを 1px 引く。
const FOOTER_H: i32 = 18;
/// 自前ヘアライン枠の太さ（px、crisp に保つため非スケール）。
const BORDER: i32 = 1;
/// 退場フェード完了後に SW_HIDE する遅延タイマの ID。
const HIDE_TIMER_ID: usize = 1;

/// 登録済みウィンドウクラスのアトム（プロセス内で一度だけ登録）。
static CLASS_ATOM: OnceLock<u16> = OnceLock::new();

// ============================================================================
// 純粋ヘルパ（GDI 非依存・単体テスト可能）。すべてのレイアウト計算の唯一の出所。
// DPI スケール等の共通ヘルパは popup.rs に移設済み（上の再輸出参照）。
// ============================================================================

/// ウィンドウ全体の横幅を [min,max] にクランプする（chrome 込みの総幅を縛る）。
fn clamp_width(content_w: i32, min_w: i32, max_w: i32) -> i32 {
    content_w.clamp(min_w, max_w)
}

/// ページングが要る（＝フッターを出す）か。総件数が 1 ページに収まらないときだけ。
fn has_footer(total_count: usize) -> bool {
    total_count > MAX_VISIBLE_ROWS
}

/// フッター（区切り 1px ＋ FOOTER_H）ぶんの追加高。フッター無しなら 0。
fn footer_extra_h(total_count: usize, dpi: i32) -> i32 {
    if has_footer(total_count) {
        1 + scale(FOOTER_H, dpi)
    } else {
        0
    }
}

/// 可視行数・総件数・内容幅（テキスト実測幅）・DPI から、ウィンドウ全体の (幅, 高さ) を求める。
/// 高さは可視行数（ページング後）で決める。番号ガターはページ内相対番号（常に 1 桁）用の
/// 固定幅。横幅は chrome（枠・パディング・番号ガター・間隔）＋実測内容幅の総和を
/// [MIN_W,MAX_W]（DPI スケール後）にクランプした値。
/// 複数ページのとき（総件数 > 1 ページ）はフッター（選択位置インジケータ）の高さが加わる。
fn window_size(visible_rows: usize, total_count: usize, content_w: i32, dpi: i32) -> (i32, i32) {
    let rows = visible_rows.max(1) as i32;
    let inner_w = scale(NUMBER_COL_W, dpi) + scale(GAP, dpi) + content_w;
    let raw_w = 2 * BORDER + 2 * scale(PADDING_X, dpi) + inner_w;
    let width = clamp_width(raw_w, scale(MIN_W, dpi), scale(MAX_W, dpi));
    let height = 2 * BORDER + 2 * scale(PADDING_Y, dpi) + rows * scale(ROW_HEIGHT, dpi)
        + footer_extra_h(total_count, dpi);
    (width, height)
}

/// クライアント座標 y が可視行のどれに当たるか（0 始まりのページ内行番号）。
/// 行領域の外（上パディング・フッター等）は None。マウス選択のヒットテスト用の純粋関数。
fn row_at_y(y: i32, dpi: i32, visible_rows: usize) -> Option<usize> {
    let top0 = row_top(0, dpi);
    if y < top0 || visible_rows == 0 {
        return None;
    }
    let rh = scale(ROW_HEIGHT, dpi).max(1);
    let idx = ((y - top0) / rh) as usize;
    (idx < visible_rows).then_some(idx)
}

/// フッターの (区切り線 RECT, ラベル RECT)。クライアント矩形の下端から積む。
/// `has_footer` が真のときだけ呼ぶ（幾何は呼び出し側の描画分岐と対で使う）。
fn footer_rects(client: RECT, dpi: i32) -> (RECT, RECT) {
    let pad_x = scale(PADDING_X, dpi);
    let label = RECT {
        left: BORDER + pad_x,
        top: client.bottom - BORDER - scale(FOOTER_H, dpi),
        right: client.right - BORDER - pad_x,
        bottom: client.bottom - BORDER,
    };
    let sep = RECT {
        left: BORDER,
        top: label.top - 1,
        right: client.right - BORDER,
        bottom: label.top,
    };
    (sep, label)
}

/// フッターのラベル（「n / 総数」。n は 1 始まりの絶対選択位置）。
fn footer_label(selected: usize, total: usize) -> String {
    format!("{} / {}", selected + 1, total)
}

/// 行 i（0 始まり）の上端 y 座標。
fn row_top(i: usize, dpi: i32) -> i32 {
    BORDER + scale(PADDING_Y, dpi) + (i as i32) * scale(ROW_HEIGHT, dpi)
}

/// 行 i の (行 RECT, 番号 RECT, 本文 RECT) を求める純粋ジオメトリ。
/// 行 RECT は枠の内側いっぱい（選択塗り用）。番号は右寄せ、本文は左寄せ列。
fn column_rects(i: usize, client_right: i32, dpi: i32) -> (RECT, RECT, RECT) {
    let top = row_top(i, dpi);
    let bottom = top + scale(ROW_HEIGHT, dpi);
    let pad_x = scale(PADDING_X, dpi);
    let num_w = scale(NUMBER_COL_W, dpi);
    let gap = scale(GAP, dpi);

    let row = RECT {
        left: BORDER,
        top,
        right: client_right - BORDER,
        bottom,
    };
    let number = RECT {
        left: BORDER + pad_x,
        top,
        right: BORDER + pad_x + num_w,
        bottom,
    };
    let text = RECT {
        left: number.right + gap,
        top,
        right: client_right - BORDER - pad_x,
        bottom,
    };
    (row, number, text)
}

/// 選択位置を delta だけ動かす（剰余で循環）。`move_selection` の算術を切り出したもの。
fn next_selection(current: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let n = len as i32;
    (current as i32 + delta).rem_euclid(n) as usize
}

/// `show` 時の選択クランプ。空なら 0、それ以外は末尾に丸める。
fn clamp_selection(selected: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        selected.min(len - 1)
    }
}

/// 番号ガターのラベル（1 始まり）。インライン format! からラベル生成を分離。
fn format_index(i: usize) -> String {
    (i + 1).to_string()
}

/// 選択 `selected` を含む可視ページの行範囲 [start, end) を返す純粋関数。
/// `len <= max_visible` なら全件 (0, len)。超える場合は `max_visible` 件単位のページに分け、
/// 選択が属するページを返す（選択は常に可視になる）。`len == 0` は (0, 0)。
pub(crate) fn visible_range(selected: usize, len: usize, max_visible: usize) -> (usize, usize) {
    if len == 0 || max_visible == 0 {
        return (0, 0);
    }
    if len <= max_visible {
        return (0, len);
    }
    let sel = selected.min(len - 1);
    let start = (sel / max_visible) * max_visible;
    let end = (start + max_visible).min(len);
    (start, end)
}

/// `ITfContextView::GetTextExt` のキャレット矩形（スクリーン座標）から、候補窓/HUD を
/// 出すアンカーを返す純粋関数。文字を覆わないようキャレット直下＝左下 (left, bottom) に
/// 出し、画面下端フリップ用にキャレット上端 top も保持する。全 0 の退化矩形
/// （GetTextExt が未書込／レイアウト未確定で既定 RECT のまま）は `None` を返し、
/// 呼び出し側（`caret_point`）が既定座標へフォールバックする。
pub(crate) fn caret_rect_to_anchor(rc: RECT) -> Option<CaretAnchor> {
    if rc.left == 0 && rc.top == 0 && rc.right == 0 && rc.bottom == 0 {
        return None;
    }
    Some(CaretAnchor { x: rc.left, y: rc.bottom, caret_top: Some(rc.top) })
}

// ============================================================================
// HWND ごとの描画状態（GWLP_USERDATA に格納）。
//
// 候補/選択/フォントを HWND 単位で持ち、WndProc から GWLP_USERDATA 越しに読む。
// thread-global なシングルトンを使わないので、同一スレッドに複数ウィンドウが同居しても
// 互いの状態を壊さない。IME は STA なのでこのアクセスは単一スレッドに直列化される。
// ============================================================================

/// HWND ごとの描画状態。`ensure_hwnd` で確保して `GWLP_USERDATA` に格納し、
/// `WM_NCDESTROY` で回収・破棄する（所有フォントは Backend の Drop が解放）。
struct WindowState {
    /// 表示中の候補列。外側 `CandidateWindow` と Rc で共有する（STA なので Rc で足りる）。
    /// clone は Rc のポインタ複製だけ＝選択移動のたびに全 String を複製しない。
    candidates: Rc<Vec<String>>,
    selected: usize,
    /// A 段: 共有テーマ（色/フォント/角丸/アクリル/モーション）。show ごとに更新される。
    theme: crate::theme::Theme,
    /// マウス選択の書き込み先（presenter/UIElement と共有する選択の真実源）。
    /// テスト用の `empty()` 経路では None（クリックは表示側のみ更新）。
    shared: Option<Rc<RefCell<CandidateState>>>,
    /// 共有描画バックエンド（D2D/GDI・DWrite・GDI フォント・デバイスロストフラグ）。
    backend: Backend,
}

impl PopupState for WindowState {
    fn backend_mut(&mut self) -> &mut Backend {
        &mut self.backend
    }
}

impl WindowState {
    /// 与えた DPI に合う GDI フォント。ファミリ・ポイントサイズはハードコード const ではなく
    /// theme から取る（settings のフォント変更を GDI パス・幅測定にも反映）。
    unsafe fn font_for_dpi(&mut self, dpi: i32) -> Option<HFONT> {
        let family = popup::family_utf16z(&self.theme.font_family);
        self.backend.font_for_dpi(&family, self.theme.font_point_tenths, dpi)
    }
}

/// `GWLP_USERDATA` に格納した `WindowState` を可変借用する。未設定なら None。
unsafe fn window_state<'a>(hwnd: HWND) -> Option<&'a mut WindowState> {
    popup::state_mut::<WindowState>(hwnd)
}

/// マウスクリックによる候補選択。クリック行（可視ページ内）を絶対 index へ解決し、
/// 表示状態と共有の選択真実源（cand_state）の両方へ書き込む。プリエディットには触れない
/// ＝矢印キー選択（move_candidate）と同じ副作用範囲なので、確定時に整合する。
unsafe fn on_click(hwnd: HWND, y: i32) {
    let dpi = popup::window_dpi(hwnd);
    let Some(state) = window_state(hwnd) else {
        return;
    };
    let count = state.candidates.len();
    let (start, end) = visible_range(state.selected, count, MAX_VISIBLE_ROWS);
    let Some(row) = row_at_y(y, dpi, end - start) else {
        return;
    };
    let abs = start + row;
    if abs == state.selected {
        return;
    }
    state.selected = abs;
    if let Some(shared) = &state.shared {
        shared.borrow_mut().set_selection(abs);
    }
    tip_log(&format!("ev=candidate_click sel={abs}"));
    let _ = InvalidateRect(Some(hwnd), None, true);
}

/// 候補ウィンドウのウィンドウプロシージャ。WM_PAINT を描画し、WM_LBUTTONDOWN で候補を選択、
/// WM_TIMER（退場フェード完了）で実際に隠し、WM_NCDESTROY で状態を回収する。
extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => unsafe {
            // クライアント座標の y（HIWORD、符号付き）で行をヒットテストする。
            let y = ((lparam.0 as u32 >> 16) & 0xFFFF) as u16 as i16 as i32;
            on_click(hwnd, y);
            LRESULT(0)
        },
        WM_TIMER => unsafe {
            if wparam.0 == HIDE_TIMER_ID {
                let _ = KillTimer(Some(hwnd), HIDE_TIMER_ID);
                // 退場フェード完了。フラグを解いてから実際に隠す（次回 hide の再入ガード解除）。
                if let Some(s) = window_state(hwnd) {
                    s.backend.fading_out = false;
                }
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        },
        WM_NCDESTROY => unsafe {
            // GWLP_USERDATA のボックスを回収して破棄（Backend の Drop が所有フォントを解放）。
            drop(popup::take_state::<WindowState>(hwnd));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        },
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// WM_PAINT のディスパッチ。renderer が Some（D2D バックエンド）なら D2D 描画、
/// None（GDI フォールバック）なら従来の GDI 本体を呼ぶ。バックエンドは初回生成時に固定。
fn paint(hwnd: HWND) {
    unsafe {
        let is_d2d = window_state(hwnd)
            .map(|s| s.backend.renderer.is_some())
            .unwrap_or(false);
        if is_d2d {
            paint_d2d(hwnd);
        } else {
            paint_gdi(hwnd);
        }
    }
}

/// WM_PAINT の GDI 本体。HWND ごとの状態を読み、カード地→各行→ヘアライン枠の順に描く。
/// 色はハードコード const ではなく `state.theme.colors.*` から取る（GDI パスも theme 準拠）。
fn paint_gdi(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        if hdc.is_invalid() {
            return;
        }
        // 状態が無ければ地塗りもせず終える（古い候補は描かない）。BeginPaint 済みなので EndPaint は必須。
        let state = match window_state(hwnd) {
            Some(s) => s,
            None => {
                let _ = EndPaint(hwnd, &ps);
                return;
            }
        };

        // メトリクスもフォントも単一の DPI 軸（横 DPI）で統一する。こうすると異方性 DPI
        // （LOGPIXELSX != LOGPIXELSY）でも内容幅と chrome の DPI 軸が食い違わない。
        let dpi = effective_dpi(GetDeviceCaps(Some(hdc), LOGPIXELSX));

        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);

        // theme から色を取る（GDI パスは不透明前提。α は colorref() が捨てる）。
        let colors = state.theme.colors;

        // カード地を塗る（theme.bg）。
        let bg = CreateSolidBrush(COLORREF(colors.bg.colorref()));
        let _ = FillRect(hdc, &rc, bg);
        let _ = DeleteObject(bg.into());

        // テキストは透過描画（アンチエイリアスの縁が地/アクセントに馴染むように）。
        let _ = SetBkMode(hdc, TRANSPARENT);

        // フォントを選択（取得できれば）。古いオブジェクトは後で必ず戻す。
        let hfont = state.font_for_dpi(dpi);
        let old_obj = hfont.map(|f| SelectObject(hdc, f.into()));

        let sel_brush = CreateSolidBrush(COLORREF(colors.sel_bg.colorref()));

        let text_fmt = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_LEFT | DT_END_ELLIPSIS;
        let num_fmt = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_RIGHT;

        let count = state.candidates.len();
        let selected = state.selected;
        // 画面より高くならないよう、選択を含む 1 ページ（最大 MAX_VISIBLE_ROWS 行）だけ描く。
        // 行 RECT は可視行（0 始まり）で配置し、番号はページ内相対（1 始まり）で描く。
        // Bug 4: 数字キー 1–9 は表示中ページ内の候補を選ぶ（key_event_sink の page_candidate_index）
        // ため、ガターの番号もページ内 1–9 に揃える（2ページ目に「10..18」を描くとキーと食い違う）。
        let (start, end) = visible_range(selected, count, MAX_VISIBLE_ROWS);
        for (row_idx, abs_i) in (start..end).enumerate() {
            let cand = &state.candidates[abs_i];
            let (row, number, text) = column_rects(row_idx, rc.right, dpi);
            let is_sel = abs_i == selected;

            if is_sel {
                // 唯一の選択手がかり: アクセント青のベタ塗り。
                let _ = FillRect(hdc, &row, sel_brush);
            }

            // 番号（ガター内で右寄せ、ページ内相対の 1 始まり＝数字キーと一致）。
            let mut num: Vec<u16> = format_index(abs_i - start).encode_utf16().collect();
            let mut num_rect = number;
            let idx_color = if is_sel { colors.sel_index } else { colors.index };
            let _ = SetTextColor(hdc, COLORREF(idx_color.colorref()));
            let _ = DrawTextW(hdc, &mut num, &mut num_rect, num_fmt);

            // 本文（左寄せ、はみ出しは省略記号）。
            let mut body: Vec<u16> = cand.encode_utf16().collect();
            let mut text_rect = text;
            let body_color = if is_sel { colors.sel_text } else { colors.text };
            let _ = SetTextColor(hdc, COLORREF(body_color.colorref()));
            let _ = DrawTextW(hdc, &mut body, &mut text_rect, text_fmt);
        }

        let _ = DeleteObject(sel_brush.into());

        // 複数ページ時のフッター: ヘアライン区切り＋「n / 総数」を右寄せ（text-sub 色）。
        if has_footer(count) {
            let (sep, label) = footer_rects(rc, dpi);
            let sep_brush = CreateSolidBrush(COLORREF(colors.border.colorref()));
            let _ = FillRect(hdc, &sep, sep_brush);
            let _ = DeleteObject(sep_brush.into());
            let mut lab: Vec<u16> = footer_label(selected, count).encode_utf16().collect();
            let mut lab_rect = label;
            let _ = SetTextColor(hdc, COLORREF(colors.index.colorref()));
            let _ = DrawTextW(hdc, &mut lab, &mut lab_rect, num_fmt);
        }

        // 自前の 1px ヘアライン枠（theme.border）。
        let border_brush = CreateSolidBrush(COLORREF(colors.border.colorref()));
        let _ = FrameRect(hdc, &rc, border_brush);
        let _ = DeleteObject(border_brush.into());

        // フォントを元に戻す（HFONT 自体は WindowState が保持するので破棄しない）。
        if let Some(old) = old_obj {
            let _ = SelectObject(hdc, old);
        }

        let _ = EndPaint(hwnd, &ps);
    }
}

/// D2D バックエンドでの描画。SetDpi(96,96) にしてレイアウト helper の物理 px 値をそのまま
/// DIP として使う。失敗（デバイスロスト/ブラシ生成失敗等）は握り潰して次フレームに委ねる
/// （panic しない）。update region を validate しないと WM_PAINT が無限再送されるため、
/// D2D でも BeginPaint/EndPaint は全経路で必ず対で呼ぶ。
unsafe fn paint_d2d(hwnd: HWND) {
    use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
    use windows::Win32::Graphics::Direct2D::{
        D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_ROUNDED_RECT,
    };
    use windows::Win32::Graphics::DirectWrite::{
        DWRITE_MEASURING_MODE_NATURAL, DWRITE_TEXT_ALIGNMENT_LEADING,
        DWRITE_TEXT_ALIGNMENT_TRAILING,
    };

    // update region の validate（無限 WM_PAINT 防止）。hdc は DPI 取得にだけ使う。
    // BeginPaint 済みなので、以降のどの early-out でも EndPaint は必須（全経路で対にする）。
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let Some(state) = window_state(hwnd) else {
        let _ = EndPaint(hwnd, &ps);
        return;
    };
    // renderer が無ければ paint() が GDI へ振り分けるはずだが、防御的に対にして return。
    if state.backend.renderer.is_none() {
        let _ = EndPaint(hwnd, &ps);
        return;
    }
    let dpi = effective_dpi(GetDeviceCaps(Some(hdc), LOGPIXELSX));
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);

    // テキストフォーマットは Backend 経由（DWrite factory は遅延生成キャッシュ、
    // フォーマット自体は毎フレーム生成で十分軽い）。
    let family: Vec<u16> = state
        .theme
        .font_family
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let font_px = font_size_px(state.theme.font_point_tenths, dpi);
    let (Some(fmt_body), Some(fmt_num)) = (
        state.backend.text_format(&family, font_px, DWRITE_TEXT_ALIGNMENT_LEADING, true),
        state.backend.text_format(&family, font_px, DWRITE_TEXT_ALIGNMENT_TRAILING, true),
    ) else {
        let _ = EndPaint(hwnd, &ps);
        return;
    };
    // 以降 state への書き込みは end_draw 後にしか無いので、テーマは不変借用で読む。
    let t = &state.theme;

    // renderer は上で Some を確認済みだが、TIP パスでは expect/unwrap を使わず else で
    // 対の EndPaint を打って return する（防御的・panic 皆無）。
    let Some(renderer) = state.backend.renderer.as_ref() else {
        let _ = EndPaint(hwnd, &ps);
        return;
    };
    // begin_draw に成功したら、この関数のどの経路でも end_draw を必ず呼ぶ
    // （begin_draw without end_draw は D2D context を begun 状態に残す）。
    let Ok(ctx) = renderer.begin_draw() else {
        let _ = EndPaint(hwnd, &ps);
        return;
    };
    ctx.SetDpi(96.0, 96.0); // px==DIP 扱い（レイアウト helper は物理 px を返す）

    let rectf = |r: &RECT| D2D_RECT_F {
        left: r.left as f32,
        top: r.top as f32,
        right: r.right as f32,
        bottom: r.bottom as f32,
    };
    // 色ごとのソリッドブラシ。失敗はそのフレームの当該描画をスキップ（panic しない）。
    let brush = |c: crate::theme::Rgba| ctx.CreateSolidColorBrush(&c.d2d(), None).ok();

    // 透明クリア → bg を全面に（アクリル時はバックドロップが透ける。opaque 時は α=255）。
    ctx.Clear(Some(&crate::theme::Rgba { r: 0, g: 0, b: 0, a: 0 }.d2d()));
    if let Some(b) = brush(t.colors.bg) {
        ctx.FillRectangle(&rectf(&rc), &b);
    }

    let count = state.candidates.len();
    let selected = state.selected;
    let (start, end) = visible_range(selected, count, MAX_VISIBLE_ROWS);
    let sel_brush = brush(t.colors.sel_bg);
    let text_brush = brush(t.colors.text);
    let index_brush = brush(t.colors.index);
    let sel_text_brush = brush(t.colors.sel_text);
    let sel_index_brush = brush(t.colors.sel_index);

    for (row_idx, abs_i) in (start..end).enumerate() {
        let (row, number, text) = column_rects(row_idx, rc.right, dpi);
        let is_sel = abs_i == selected;
        if is_sel {
            if let Some(b) = sel_brush.as_ref() {
                // Apple 風: 選択ハイライトは --radius-sm 相当の角丸ピルで塗る。
                let rr = D2D1_ROUNDED_RECT {
                    rect: rectf(&row),
                    radiusX: scale(tokens::RADIUS_SM, dpi) as f32,
                    radiusY: scale(tokens::RADIUS_SM, dpi) as f32,
                };
                ctx.FillRoundedRectangle(&rr, b);
            }
        }
        let num_utf16: Vec<u16> = format_index(abs_i - start).encode_utf16().collect();
        let body_utf16: Vec<u16> = state.candidates[abs_i].encode_utf16().collect();
        let nb = if is_sel { sel_index_brush.as_ref() } else { index_brush.as_ref() };
        let tb = if is_sel { sel_text_brush.as_ref() } else { text_brush.as_ref() };
        if let Some(b) = nb {
            // 0.62 の（RenderTarget 継承の）DrawText は 6 引数。
            ctx.DrawText(
                &num_utf16,
                &fmt_num,
                &rectf(&number),
                b,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
        if let Some(b) = tb {
            ctx.DrawText(
                &body_utf16,
                &fmt_body,
                &rectf(&text),
                b,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    // 複数ページ時のフッター: ヘアライン区切り＋「n / 総数」を右寄せ（text-sub 色）。
    if has_footer(count) {
        let (sep, label) = footer_rects(rc, dpi);
        if let Some(b) = brush(t.colors.border) {
            ctx.FillRectangle(&rectf(&sep), &b);
        }
        if let Some(b) = brush(t.colors.index) {
            let lab: Vec<u16> = footer_label(selected, count).encode_utf16().collect();
            ctx.DrawText(
                &lab,
                &fmt_num,
                &rectf(&label),
                &b,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    // 1px ヘアライン枠（D2D の stroke は座標中心なので 0.5px 内側に寄せる）。
    if let Some(b) = brush(t.colors.border) {
        let inset = D2D_RECT_F {
            left: rc.left as f32 + 0.5,
            top: rc.top as f32 + 0.5,
            right: rc.right as f32 - 0.5,
            bottom: rc.bottom as f32 - 0.5,
        };
        ctx.DrawRectangle(&inset, &b, 1.0, None);
    }

    // end_draw の失敗がデバイスロスト由来なら renderer_dead を立て、次回 show で窓を作り直す。
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

/// 自前描画の候補ウィンドウ。`hwnd` は遅延生成（初回 `show` まで null）。
pub struct CandidateWindow {
    hwnd: HWND,
    /// 表示中の候補列。`WindowState` と Rc で共有（sync はポインタ複製のみ）。
    candidates: Rc<Vec<String>>,
    selected: usize,
    /// A 段: この窓が使う解決済みテーマ。show ごとに settings 由来の値へ更新され、
    /// sync_state で WindowState へ複製する（Task 7: 表示ごとの再読込）。
    theme: crate::theme::Theme,
    /// マウス選択の書き込み先（presenter と共有する選択の真実源）。ensure_hwnd で
    /// WindowState へ複製し、WndProc のクリックハンドラが参照する。
    shared: Option<Rc<RefCell<CandidateState>>>,
}

impl CandidateWindow {
    /// HWND を持たない空のウィンドウを構築する（テスト・共有状態なし経路用）。
    pub fn empty() -> Self {
        Self {
            hwnd: HWND(std::ptr::null_mut()),
            candidates: Rc::new(Vec::new()),
            selected: 0,
            // プレースホルダ。初回 show が settings 由来の Theme を渡して必ず上書きする。
            theme: crate::theme::Theme::default(),
            shared: None,
        }
    }

    /// 選択の真実源（cand_state）を共有するウィンドウを構築する（presenter 用）。
    /// マウスクリックによる選択がこの状態へ直接書き込まれる。
    pub fn with_state(shared: Rc<RefCell<CandidateState>>) -> Self {
        let mut w = Self::empty();
        w.shared = Some(shared);
        w
    }

    /// デバイスロスト後の回復。判定・破棄は popup 側の共通処理
    /// （NOREDIRECTIONBITMAP 窓は GDI 復帰できないため破棄→再生成しかない）。
    fn recover_if_device_lost(&mut self) {
        popup::recover_if_device_lost::<WindowState>(&mut self.hwnd, "ev=candwin_device_lost_recover");
    }

    /// 必要なら HWND を生成する。生成に失敗したら null のまま（劣化動作）。
    fn ensure_hwnd(&mut self) {
        if !self.hwnd.is_invalid() {
            return;
        }
        if popup::register_class(&CLASS_ATOM, CLASS_NAME, Some(wnd_proc)).is_none() {
            return;
        }
        unsafe {
            // 仮の初期サイズ。実寸は直後の relayout_and_repaint で content-fit に直す。
            let (width, height) = window_size(3, 3, scale(MIN_W, 96), 96);

            // 2 段バックエンド判定（D2D 試行→GDI 確定）は popup 側の共通処理。
            let Some((hwnd, renderer)) = popup::create_backed_popup(CLASS_NAME, width, height)
            else {
                self.hwnd = HWND(std::ptr::null_mut());
                return;
            };
            self.hwnd = hwnd;

            let theme = self.theme.clone();
            // 角丸は両パス、アクリルバックドロップは D2D パスのみ（GDI は不透明背景で
            // バックドロップが無効なため）。失敗は握り潰す（Win10=no-op）。
            crate::render::apply_dwm_chrome(hwnd, theme.rounded, theme.acrylic && renderer.is_some());

            // HWND ごとの描画状態を確保し、GWLP_USERDATA に格納する
            // （WM_NCDESTROY で回収・破棄する）。
            popup::install_state(
                hwnd,
                Box::new(WindowState {
                    candidates: Rc::new(Vec::new()),
                    selected: 0,
                    theme,
                    shared: self.shared.clone(),
                    backend: Backend::new(renderer),
                }),
            );
        }
    }

    /// HWND ごとの描画状態を現在の候補/選択/テーマに同期する（WM_PAINT 用）。フォントは
    /// 状態側が保持。候補列は Rc の複製（ポインタのみ）で、String の複製は起きない。
    fn sync_state(&self) {
        if self.hwnd.is_invalid() {
            return;
        }
        unsafe {
            if let Some(state) = window_state(self.hwnd) {
                state.candidates = Rc::clone(&self.candidates);
                state.selected = self.selected;
                // Task 7: 表示ごとに更新されるテーマも WM_PAINT 側の真実源へ複製する
                // （これを怠ると色変更が InvalidateRect 後の再描画に反映されない）。
                state.theme = self.theme.clone();
            }
        }
    }

    /// 選択位置だけを WM_PAINT 側へ同期する（矢印キーの選択移動用）。候補列・テーマは
    /// 変わっていないので複製しない — 選択移動は打鍵経路で最も頻繁に走るため軽く保つ。
    fn sync_selection(&self) {
        if self.hwnd.is_invalid() {
            return;
        }
        unsafe {
            if let Some(state) = window_state(self.hwnd) {
                state.selected = self.selected;
            }
        }
    }

    /// 候補本文の最大実測幅を測る。描画と同じエンジンで測るのが原則:
    /// D2D バックエンドなら DWrite（DrawText と同一フォーマット）、GDI フォールバックなら
    /// GetTextExtentPoint32W（描画と同一 HDC+フォント）。エンジンをまたいで測ると
    /// メトリクス差で本文が切れる/余る。幅はどちらも物理 px。総幅は MAX_W でクランプ
    /// されるため、その相当値 `cap` に達したら打ち切る（数百候補を全部測らない）。
    /// 取得できない場合は 0（→ window_size 側で MIN_W にクランプ）。
    fn measure_content_width(&self, dpi: i32) -> i32 {
        if self.hwnd.is_invalid() || self.candidates.is_empty() {
            return 0;
        }
        let cap = scale(MAX_W, dpi);
        unsafe {
            // D2D パス: DWrite で測る（フォーマットは Backend のキャッシュを使う）。
            if let Some(state) = window_state(self.hwnd) {
                if state.backend.renderer.is_some() {
                    let family = popup::family_utf16z(&state.theme.font_family);
                    let font_px = font_size_px(state.theme.font_point_tenths, dpi);
                    if let Some(w) = state.backend.measure_max_width_dwrite(
                        &family,
                        font_px,
                        &self.candidates,
                        cap,
                    ) {
                        return w;
                    }
                    // DWrite が用意できなければ GDI 測定へ劣化（描画は D2D のままなので
                    // 誤差は出得るが、幅 0 で MIN_W に潰れるよりよい）。
                }
            }
            let hdc = GetDC(Some(self.hwnd));
            if hdc.is_invalid() {
                return 0;
            }
            let hfont = window_state(self.hwnd).and_then(|s| s.font_for_dpi(dpi));
            let old_obj = hfont.map(|f| SelectObject(hdc, f.into()));

            let mut max_w = 0;
            for cand in self.candidates.iter() {
                let utf16: Vec<u16> = cand.encode_utf16().collect();
                let mut size = SIZE::default();
                if GetTextExtentPoint32W(hdc, &utf16, &mut size).as_bool() && size.cx > max_w {
                    max_w = size.cx;
                    if max_w >= cap {
                        break;
                    }
                }
            }

            if let Some(old) = old_obj {
                let _ = SelectObject(hdc, old);
            }
            let _ = ReleaseDC(Some(self.hwnd), hdc);
            max_w
        }
    }

    /// 候補数/内容に応じてウィンドウをアンカー位置に移動・リサイズして再描画する。
    fn relayout_and_repaint(&self, anchor: CaretAnchor) {
        if self.hwnd.is_invalid() {
            return;
        }
        // DPI は hwnd の HDC から（paint/measure と同一軸）。
        let dpi = popup::window_dpi(self.hwnd);

        let content_w = self.measure_content_width(dpi);
        // 高さはページ単位（最大 MAX_VISIBLE_ROWS 行）で固定する。こうすると move_selection で
        // ページが変わっても高さが揺れない。
        let visible_rows = self.candidates.len().min(MAX_VISIBLE_ROWS);
        let (width, height) = window_size(visible_rows, self.candidates.len(), content_w, dpi);

        // 希望位置がモニタの作業領域を越える場合は画面内へ収める。下端はみ出しは
        // キャレット直上へフリップし、入力中の行を覆わない（caret_top 不明ならクランプ）。
        let (fx, fy) =
            popup::place_on_monitor_flipped(anchor.x, anchor.y, anchor.caret_top, width, height);
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                fx,
                fy,
                width,
                height,
                SWP_NOACTIVATE | SWP_NOZORDER,
            );
            // D2D パスの swapchain 作り直し＋InvalidateRect（SetWindowPos の後、描画の前）。
            popup::resize_and_invalidate::<WindowState>(self.hwnd, width, height);
        }
    }
}

impl CandidateWindow {
    /// 選択を絶対 index で設定して再描画する（クランプ）。presenter は矢印/Space の移動も
    /// cand_state で新しい絶対位置を計算してからこれを呼ぶ。相対 delta の二重適用をやめる
    /// ことで、マウスクリック（WndProc 側で表示状態を直接更新する）と経路が競合しても
    /// 表示と真実源が乖離しない。
    pub fn set_selection(&mut self, index: usize) {
        if self.candidates.is_empty() {
            return;
        }
        self.selected = clamp_selection(index, self.candidates.len());
        // 候補列・テーマは不変なので選択 index だけ同期する（全 String の複製を避ける）。
        self.sync_selection();
        if !self.hwnd.is_invalid() {
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, true);
            }
        }
    }

    /// Deactivate から呼ぶ。プロセス終了時の msctf 後始末（LdrShutdownProcess 中の
    /// IUnknown::Release）で DestroyWindow されると、その WM_NCDESTROY で
    /// SurfaceRenderer（dcomp/d3d11）が drop され、プロセス終了中の dcomp 操作が
    /// dxgi の例外を起こして STATUS_FATAL_USER_CALLBACK_EXCEPTION (c000041d) で
    /// ホストごと落ちる。プロセスが健全な Deactivate 時点で畳んでおけば、
    /// 終了時には hwnd が null で Drop は no-op になる。
    pub fn destroy(&mut self) {
        // ウィンドウを破棄する。DestroyWindow は WM_NCDESTROY を同期ディスパッチし、その中で
        // GWLP_USERDATA の WindowState（所有フォント含む）を Box::from_raw で回収・破棄する。
        // これにより以降の WM_PAINT・孤児トップモスト窓・HWND/HFONT リークをまとめて防ぐ。
        // STA なので drop は生成スレッド上で走り、DestroyWindow のスレッド制約も満たす。
        if !self.hwnd.is_invalid() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
            self.hwnd = HWND(std::ptr::null_mut());
        }
    }
}

impl Drop for CandidateWindow {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl CandidateUI for CandidateWindow {
    fn show(
        &mut self,
        candidates: &[String],
        selected: usize,
        anchor: CaretAnchor,
        theme: crate::theme::Theme,
    ) {
        // Task 7: 生成済み HWND で DWM chrome に効く属性（角丸/アクリル）が変わったかを、
        // self.theme を新値で上書きする前に判定しておく。色だけの変化は sync_state での
        // WindowState.theme 更新＋relayout_and_repaint の InvalidateRect で反映されるので、
        // chrome の再適用（DWM 呼び出し）は属性が実際に変わったときに限る。
        // デバイスロスト（前回 paint で検知）していたら、まず窓を破棄して null に戻す。
        // これを chrome_changed 判定・ensure_hwnd の前に行い、再生成される窓で最新テーマを
        // 適用しなおす（破棄→null なら chrome_changed は下で false 扱い、ensure_hwnd が全適用）。
        self.recover_if_device_lost();
        let chrome_changed = !self.hwnd.is_invalid()
            && (theme.rounded != self.theme.rounded || theme.acrylic != self.theme.acrylic);
        self.theme = theme;
        self.candidates = Rc::new(candidates.to_vec());
        self.selected = clamp_selection(selected, self.candidates.len());
        self.ensure_hwnd();
        // 診断: 自前描画が選ばれた場合、窓が実際に生成できたか／どこに置くかを記録する
        // （イマーシブ検索面では生成・表示されてもバンドが下で不可視になるため、座標と
        //  hwnd_ok を残して「生成失敗」と「生成したが見えない」を切り分ける）。
        tip_log(&format!(
            "ev=candwin hwnd_ok={} x={} y={} n={}",
            !self.hwnd.is_invalid(), anchor.x, anchor.y, self.candidates.len()
        ));
        if self.hwnd.is_invalid() {
            return;
        }
        if chrome_changed {
            unsafe {
                // アクリルは D2D バックエンドのときだけ有効（GDI は不透明背景でバックドロップが
                // 映らないため）。ensure_hwnd の初回適用と同じ条件で再適用する。
                let d2d = window_state(self.hwnd)
                    .map(|s| s.backend.renderer.is_some())
                    .unwrap_or(false);
                crate::render::apply_dwm_chrome(self.hwnd, self.theme.rounded, self.theme.acrylic && d2d);
            }
        }
        // WM_PAINT 用に HWND ごとの描画状態（候補・選択・テーマ）を更新する。
        self.sync_state();
        // 先に content-fit のサイズ・位置へ合わせてから可視化する。順序を逆にすると
        // 初期サイズ（ensure_hwnd の仮値）や前回サイズで一瞬表示されてからジャンプする
        // ちらつきが出る。SW_SHOWNOACTIVATE で WM_PAINT が走るのは確定後なので、
        // 最終ジオメトリで一度だけ描かれる。
        self.relayout_and_repaint(anchor);
        unsafe {
            // 退場フェード中の再表示なら、遅延 hide タイマを解除して表示を続行する。
            let _ = KillTimer(Some(self.hwnd), HIDE_TIMER_ID);
            let was_visible = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE).as_bool();
            // 出現モーション（Apple 風: 新規出現のみ短いフェード。既に見えている窓の
            // 内容更新やフェード割り込みは 1.0 へスナップ＝表示を待たせない）。DComp が
            // コンポジタ側で駆動するのでタイマ・打鍵経路の負荷はない。
            if let Some(state) = window_state(self.hwnd) {
                let motion = state.theme.motion;
                popup::play_entrance(state, motion, was_visible);
            }
        }
    }

    fn hide(&mut self) {
        if !self.hwnd.is_invalid() {
            unsafe {
                // 退場モーション: 出現と対称のフェードで消え、完了後に SW_HIDE する。
                // 使えない環境・reduced-motion では即時 hide。フェードイン途中の退場は
                // 推定不透明度から帰り、既にフェードアウト中なら何もしない（再入ガード）
                // — begin_fade_out が全部判定する。
                let action = if IsWindowVisible(self.hwnd).as_bool() {
                    window_state(self.hwnd)
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
                        // SetTimer 失敗（USER タイマ資源枯渇等。稀）を無視してはならない:
                        // fading_out が立ったまま WM_TIMER が永遠に来ず、この窓は
                        // WS_EX_TRANSPARENT を持たないため「透明だがクリックを吸う」
                        // ゴースト窓が次の show() まで残る。失敗時は即時 hide へ劣化する。
                        if SetTimer(Some(self.hwnd), HIDE_TIMER_ID, ms, None) == 0 {
                            if let Some(s) = window_state(self.hwnd) {
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
        }
        // 描画ミラーもクリアして真実の源（cand_state 側）と乖離させない。これをしないと、
        // 非表示後に走った無関係な WM_PAINT が破棄済みの古い候補リストを描き得る。
        // フェード中は再描画をスケジュールしないので、最終フレームが残ったまま消えていく。
        self.candidates = Rc::new(Vec::new());
        self.selected = 0;
        self.sync_state();
    }

    /// 表示側の選択位置。マウスクリック（WndProc が WindowState を直接更新する）を
    /// 取りこぼさないよう、HWND があれば WindowState 側を正とする（外側ミラーは
    /// クリックでは更新されない）。
    fn selected(&self) -> usize {
        if !self.hwnd.is_invalid() {
            if let Some(state) = unsafe { window_state(self.hwnd) } {
                return state.selected;
            }
        }
        self.selected
    }

    fn move_selection(&mut self, delta: i32) {
        if self.candidates.is_empty() {
            return;
        }
        // 現在位置は WindowState 側（クリック選択を含む）から読む。
        let current = CandidateUI::selected(self);
        self.set_selection(next_selection(current, delta, self.candidates.len()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_width_bounds() {
        assert_eq!(clamp_width(50, 160, 420), 160);
        assert_eq!(clamp_width(300, 160, 420), 300);
        assert_eq!(clamp_width(999, 160, 420), 420);
    }

    #[test]
    fn caret_rect_to_anchor_uses_bottom_left_and_rejects_degenerate() {
        // GetTextExt のキャレット矩形（スクリーン座標）。候補窓/HUD は文字を覆わないよう
        // キャレット直下＝左下 (left, bottom) に出し、フリップ用に top も保持する。
        assert_eq!(
            caret_rect_to_anchor(RECT { left: 100, top: 40, right: 102, bottom: 60 }),
            Some(CaretAnchor { x: 100, y: 60, caret_top: Some(40) })
        );
        // マルチモニタの負座標（左/上のモニタ）でも左下をそのまま返す。
        assert_eq!(
            caret_rect_to_anchor(RECT { left: -1920, top: 10, right: -1918, bottom: 34 }),
            Some(CaretAnchor { x: -1920, y: 34, caret_top: Some(10) })
        );
        // 全 0 の退化矩形（GetTextExt 未書込/レイアウト未確定の既定 RECT）は None
        // ＝呼び出し側が既定座標へフォールバックする。
        assert_eq!(caret_rect_to_anchor(RECT { left: 0, top: 0, right: 0, bottom: 0 }), None);
    }

    #[test]
    fn window_size_literal_at_96dpi() {
        // 96DPI では scale は恒等。BORDER=1, PADDING_X=12, PADDING_Y=4, ROW_HEIGHT=28,
        // NUMBER_COL_W=22, GAP=8。期待値は実装と独立に手計算したリテラルで固定する
        // （同じ scale()/定数で再導出すると符号・定数取り違えを検出できないため）。

        // 3 行・3 件・内容幅 0: inner=22+8+0=30, raw=2*1+2*12+30=56 → MIN_W=160 にクランプ。
        // 高さ=2*1+2*4+3*28=94。
        assert_eq!(window_size(3, 3, 0, 96), (160, 94));

        // 0 行・0 件: rows.max(1) で 1 行ぶんの高さ=2+8+28=38、幅は MIN_W=160。
        assert_eq!(window_size(0, 0, 0, 96), (160, 38));

        // 3 行・3 件・内容幅 200: inner=22+8+200=230, raw=2+24+230=256（[160,420]内）→ 幅 256。
        assert_eq!(window_size(3, 3, 200, 96), (256, 94));

        // 内容幅が広いと MAX_W=420 にクランプ。高さは 1 行=38。
        assert_eq!(window_size(1, 1, 9999, 96), (420, 38));

        // 総件数 12 でもガターは固定幅（描かれる番号はページ内相対 1–9 で常に 1 桁）:
        // inner=22+8+0=30, raw=2+24+30=56 → MIN_W=160 にクランプ（幅）。高さは可視 3 行 94
        // に加え、複数ページなのでフッター（区切り 1px + FOOTER_H=18）=19 が乗る → 113。
        assert_eq!(window_size(3, 12, 0, 96), (160, 113));
    }

    #[test]
    fn window_size_height_uses_visible_rows_not_total() {
        // 可視行数で高さが決まり、総件数（番号ガター幅）とは独立
        // （複数ページのフッターぶんは加算される）。
        // 可視 9 行・総 100 件 @96: 高さ=2+8+9*28+（1+18）=281。
        let (_w, h) = window_size(9, 100, 0, 96);
        assert_eq!(
            h,
            2 * BORDER + 2 * scale(PADDING_Y, 96) + 9 * scale(ROW_HEIGHT, 96) + 1
                + scale(FOOTER_H, 96)
        );
        assert_eq!(h, 281);
    }

    #[test]
    fn footer_only_when_paged() {
        // 1 ページに収まる（<=MAX_VISIBLE_ROWS）ならフッター無し＝従来と同じ高さ。
        assert!(!has_footer(MAX_VISIBLE_ROWS));
        assert!(has_footer(MAX_VISIBLE_ROWS + 1));
        assert_eq!(footer_extra_h(9, 96), 0);
        assert_eq!(footer_extra_h(10, 96), 1 + scale(FOOTER_H, 96));
        // 9 件と 10 件で高さがフッターぶんだけ違う（可視行数は同じ 9）。
        let (_a, h9) = window_size(9, 9, 0, 96);
        let (_b, h10) = window_size(9, 10, 0, 96);
        assert_eq!(h10 - h9, 1 + scale(FOOTER_H, 96));
    }

    #[test]
    fn footer_rects_stack_from_bottom_and_label_is_one_based() {
        let client = RECT { left: 0, top: 0, right: 300, bottom: 113 };
        let (sep, label) = footer_rects(client, 96);
        // ラベルはクライアント下端から BORDER の内側に FOOTER_H。区切りはその直上 1px。
        assert_eq!(label.bottom, 113 - BORDER);
        assert_eq!(label.top, 113 - BORDER - scale(FOOTER_H, 96));
        assert_eq!(sep.bottom, label.top);
        assert_eq!(sep.top, label.top - 1);
        // 左右は枠＋パディングの内側（区切りは枠の内側いっぱい）。
        assert_eq!(label.left, BORDER + scale(PADDING_X, 96));
        assert_eq!(sep.left, BORDER);
        assert_eq!(sep.right, 300 - BORDER);
        // ラベルは 1 始まり表示。
        assert_eq!(footer_label(0, 12), "1 / 12");
        assert_eq!(footer_label(11, 12), "12 / 12");
    }

    #[test]
    fn row_at_y_hits_rows_and_rejects_chrome() {
        let dpi = 96;
        let top0 = row_top(0, dpi); // BORDER+PADDING_Y = 5
        let rh = scale(ROW_HEIGHT, dpi); // 28
        // 上パディングより上は None。
        assert_eq!(row_at_y(top0 - 1, dpi, 3), None);
        // 各行の先頭/末尾 px が正しい行に落ちる。
        assert_eq!(row_at_y(top0, dpi, 3), Some(0));
        assert_eq!(row_at_y(top0 + rh - 1, dpi, 3), Some(0));
        assert_eq!(row_at_y(top0 + rh, dpi, 3), Some(1));
        assert_eq!(row_at_y(top0 + 3 * rh - 1, dpi, 3), Some(2));
        // 可視行数を超えた領域（フッター等）は None。
        assert_eq!(row_at_y(top0 + 3 * rh, dpi, 3), None);
        // 可視 0 行は常に None。
        assert_eq!(row_at_y(top0, dpi, 0), None);
    }

    #[test]
    fn geometry_scales_at_192dpi() {
        // 192DPI で scale が恒等でなくなる経路を実値で検証する。
        // scale(28,192)=(28*192+48)/96=5424/96=56.5→56（行高）。
        assert_eq!(scale(ROW_HEIGHT, 192), 56);
        assert_eq!(row_top(1, 192) - row_top(0, 192), 56);

        // scale(4,192)=(4*192+48)/96=816/96=8.5→8。2 行・2 件・内容 0 の高さ=2*1+2*8+2*56=130。
        let (_w, h) = window_size(2, 2, 0, 192);
        assert_eq!(h, 130);

        // 列は番号→本文の順で重ならず、枠内に収まる（HiDPI でも）。
        let (row, number, text) = column_rects(0, 400, 192);
        assert!(number.left >= row.left);
        assert!(number.right <= text.left);
        assert!(text.right <= row.right);
        // テキスト列の幅が負にならない（MIN_W 相当の狭幅でも反転しない）。
        let (_r, _n, t_narrow) = column_rects(0, scale(MIN_W, 192), 192);
        assert!(t_narrow.right >= t_narrow.left);
    }

    #[test]
    fn visible_range_pages_around_selection() {
        // 件数が上限以下なら全件。
        assert_eq!(visible_range(0, 0, 9), (0, 0));
        assert_eq!(visible_range(0, 5, 9), (0, 5));
        assert_eq!(visible_range(4, 9, 9), (0, 9));
        // 上限超過は max_visible 単位のページ。選択が属するページを返す。
        assert_eq!(visible_range(0, 20, 9), (0, 9));
        assert_eq!(visible_range(8, 20, 9), (0, 9));
        assert_eq!(visible_range(9, 20, 9), (9, 18));
        assert_eq!(visible_range(17, 20, 9), (9, 18));
        assert_eq!(visible_range(18, 20, 9), (18, 20)); // 末尾の半端ページ
                                                        // 選択は常に [start, end) に含まれる。
        let (s, e) = visible_range(13, 20, 9);
        assert!(s <= 13 && 13 < e);
        // max_visible 0 は (0,0)。
        assert_eq!(visible_range(0, 5, 0), (0, 0));
    }

    #[test]
    fn row_top_advances_by_row_height() {
        let r0 = row_top(0, 96);
        let r1 = row_top(1, 96);
        assert_eq!(r0, BORDER + scale(PADDING_Y, 96));
        assert_eq!(r1 - r0, scale(ROW_HEIGHT, 96));
    }

    #[test]
    fn column_rects_are_ordered_and_inset() {
        let client_right = 240;
        let (row, number, text) = column_rects(0, client_right, 96);
        // 行は枠の内側いっぱい。
        assert_eq!(row.left, BORDER);
        assert_eq!(row.right, client_right - BORDER);
        // 番号→本文の順で左から並び、重ならない。
        assert!(number.left >= row.left);
        assert!(number.right <= text.left);
        assert!(text.right <= row.right);
        // 縦範囲は一致。
        assert_eq!(row.top, number.top);
        assert_eq!(row.bottom, text.bottom);
    }

    #[test]
    fn gutter_width_is_fixed_regardless_of_total_count() {
        // 描かれる番号はページ内相対（1–9）なので、総件数が増えてもガター＝総幅は不変。
        // 内容幅 200 はクランプ域外（raw=2+24+22+8+200=256）なのでガター差がそのまま出る。
        let (w3, _) = window_size(3, 3, 200, 96);
        let (w120, _) = window_size(3, 120, 200, 96);
        assert_eq!(w3, 256);
        assert_eq!(w3, w120);
        // 本文開始位置も総件数に依らない。
        let (_r, _n, t) = column_rects(0, 400, 96);
        assert_eq!(t.left, BORDER + scale(PADDING_X, 96) + scale(NUMBER_COL_W, 96) + scale(GAP, 96));
    }

    #[test]
    fn next_selection_wraps() {
        assert_eq!(next_selection(0, 1, 3), 1);
        assert_eq!(next_selection(2, 1, 3), 0); // 末尾→先頭
        assert_eq!(next_selection(0, -1, 3), 2); // 先頭→末尾
        assert_eq!(next_selection(1, 5, 3), 0); // (1+5)%3
        assert_eq!(next_selection(0, 1, 0), 0); // 空は 0
    }

    #[test]
    fn clamp_selection_bounds() {
        assert_eq!(clamp_selection(0, 0), 0);
        assert_eq!(clamp_selection(5, 0), 0);
        assert_eq!(clamp_selection(5, 3), 2);
        assert_eq!(clamp_selection(1, 3), 1);
    }

    #[test]
    fn format_index_is_one_based() {
        assert_eq!(format_index(0), "1");
        assert_eq!(format_index(9), "10");
    }
}
