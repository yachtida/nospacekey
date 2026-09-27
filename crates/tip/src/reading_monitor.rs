//! ライブ変換中に生の読み（ひらがな）を本文に近い位置へ常時表示し、辞書予測候補を
//! 上段に同居させる統合パネル（Win32 popup）。
//!
//! ライブ変換は preedit を変換結果（漢字かな交じり）へ全置換するため、「今何を打ったか」が
//! 画面から消える。読みを候補窓/HUD と同じ popup 基盤の小窓で並走表示する
//! （spec: docs/design/2026-07-21-reading-monitor-design.md）。
//!
//! 予測（input prediction）は従来候補窓の preview モードで流用表示していたが、本窓へ
//! 統合した: 読み行を下段（=本文に近い位置）に固定し、候補欄はその上に広がる。入力を
//! 再開してもパネルは閉じず、読み行だけ即時更新し、新しい応答が届けば候補欄の中身だけ
//! 差し替える（閉じ→開きをしない）。読みが進んで古くなった候補（stale）は薄色で選択
//! 対象外を示す（確定は identity 突合で拒否される）。選択モード（Tab）は従来どおり
//! 文節変換の候補窓へ移るため、本窓に選択強調は存在しない。
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
    CreateSolidBrush, DeleteObject, DrawTextW, FillRect, FrameRect, GetDC,
    GetTextExtentPoint32W, InvalidateRect, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, DT_END_ELLIPSIS, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
    TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, GetClientRect, IsWindowVisible, KillTimer, ShowWindow, SW_HIDE,
    SW_SHOWNOACTIVATE, WM_NCDESTROY, WM_PAINT, WM_TIMER,
};

use crate::candidate_window::CaretAnchor;
use crate::popup::{self, font_size_px, scale, Backend, PopupState};
use crate::text_service::tip_log;

const CLASS_NAME: PCWSTR = w!("NospacekeyReadingMonitor");

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
/// パネルの予測候補は 3 行固定。エンジンは 9 件まで返すが、パネルは先頭 3 件の
/// コンパクト表示に留め、全件は Tab で選択モードに入った後の候補窓で選ぶ。
pub(crate) const PANEL_PREVIEW_ROWS: usize = 3;
/// 候補 1 行の高さ（dp）。候補窓の ROW_HEIGHT と同じ見た目。
const ROW_H: i32 = 28;
/// 読み行と候補欄の間の区切り帯（dp）。中央にヘアライン 1px を引く。
const SEP_BAND: i32 = 7;
/// 候補行のガター幅（dp）。1 行目だけ "Tab" を描く（候補窓の番号ガター 22dp に合わせる）。
const GUTTER_W: i32 = 22;
/// パネル幅の下限（dp）。候補が読める最小幅（候補窓の MIN_W と同じ）—「未確定文字列の
/// 表示幅を基本に、読み・候補が読める最小幅を確保する」の下限側。
const MIN_PANEL_W: i32 = 160;

static CLASS_ATOM: OnceLock<u16> = OnceLock::new();

/// パネルの予測候補行。`stale` は読みが進んで旧世代になった応答で、薄色表示・選択不可。
/// 新しい応答が届けば中身だけ差し替わる（パネルは閉じない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PanelCandidate {
    pub text: String,
    pub stale: bool,
}

/// パネルの表示計画（純関数・唯一の真実源）。読み行と候補欄は**独立**した条件で、
/// どちらか片方だけの形態もある:
/// - 読み行: 従来の should_show と同じ（設定ON && composing && ライブ変換ON && 候補窓非表示）。
///   ライブ変換 OFF は preedit に読みがそのまま見えるので出さない。
/// - 候補欄: composing && 候補窓非表示 && 予測候補あり。ライブ変換を要求しない
///   （preedit 中なら読みと無関係に出てよい）。
///
/// 候補窓（文節変換）表示中はどちらも隠す（ユーザ確認済みの決定 — 候補選択中は候補窓に集中）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PanelPlan {
    pub reading_row: bool,
    pub candidate_rows: bool,
}

pub(crate) fn plan_panel(
    reading_enabled: bool,
    prediction_on: bool,
    composing: bool,
    live_enabled: bool,
    candidate_visible: bool,
) -> PanelPlan {
    let blocked = !composing || candidate_visible;
    PanelPlan {
        reading_row: reading_enabled && live_enabled && !blocked,
        candidate_rows: prediction_on && !blocked,
    }
}

impl PanelPlan {
    /// plan を**表示データへ反映**する（読み行 OFF=空文字、候補欄 OFF=空配列）。
    /// 通常更新とレイアウト追従の共通組立（text_service::panel_sync_data 経由の唯一の適用箇所）。
    /// plan を可否判定にだけ使って生データを show_or_update へ渡すと、受け側の
    /// 「text 非空なら読み行を描く」規律が勝ち、読み表示 OFF／ライブ変換 OFF でも
    /// 予測候補が出ている限り読み行まで表示されてしまう。
    pub(crate) fn filter_display(
        self,
        reading: String,
        candidates: Vec<PanelCandidate>,
    ) -> (String, Vec<PanelCandidate>) {
        (
            if self.reading_row {
                reading
            } else {
                String::new()
            },
            if self.candidate_rows {
                candidates
            } else {
                Vec::new()
            },
        )
    }
}

/// 幅上限（物理px）= max_chars × フォントem幅。読み行の中身はひらがな=全角のみで
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

fn trim_to_tail(buf: &mut String, max_chars: usize) {
    let n = buf.chars().count();
    if n > max_chars {
        if let Some((cut, _)) = buf.char_indices().nth(n - max_chars) {
            buf.drain(..cut);
        }
    }
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

/// 枠＋左右パディングぶんの窓幅増分（物理px）。panel_window_size が文字領域幅へ加算する
/// 増分の唯一の出所。max_text_w_px は**文字領域**の上限なので、窓全体の幅を扱う側
/// （show_or_update の幅保持クランプ）は panel_frame_w を加えた上限を使う — 文字領域の
/// 上限で窓幅をクランプすると枠ぶん描画領域が痩せ、overflow 判定（max_text_w_px 基準）と
/// 実際の描画幅が食い違って末尾（=最新の読み）が省略記号なしの右端クリップで消える。
fn panel_frame_w(dpi: i32) -> i32 {
    2 * BORDER + 2 * scale(PAD_H, dpi)
}

/// 幅保持後の窓幅クランプ上限 = 文字領域上限 + 枠・左右パディング（panel_frame_w）。
fn max_window_w(max_text_w: i32, min_w: i32, dpi: i32) -> i32 {
    (max_text_w + panel_frame_w(dpi)).max(min_w)
}

/// 統合パネルの (幅, 高さ)。幅 = max(composition 表示幅, 読み実測, 候補実測) を
/// [scale(MIN_PANEL_W), max_w_px] へクランプ —「未確定文字列の表示幅を基本に、読み・候補が
/// 読める最小幅を確保する」の唯一の計算箇所。はみ出しは描画側の末尾寄せ/省略記号が受ける。
/// 高さ = 枠 + 読み行（上下パディング+フォント）+（候補行があるとき区切り帯 + rows×行高）。
/// `with_reading_row=false`（ライブ変換 OFF で候補欄のみの形態）は読み行帯と区切り帯を省く。
#[allow(clippy::too_many_arguments)]
pub(crate) fn panel_window_size(
    comp_px_w: i32,
    reading_px_w: i32,
    candidate_px_w: i32,
    with_reading_row: bool,
    rows: usize,
    font_px: i32,
    dpi: i32,
    max_w_px: i32,
) -> (i32, i32) {
    let min_w = scale(MIN_PANEL_W, dpi);
    let content = comp_px_w.max(reading_px_w).max(candidate_px_w);
    let clamped = content.clamp(min_w, max_w_px.max(min_w));
    let w = panel_frame_w(dpi) + clamped;
    let mut h = 2 * BORDER;
    if with_reading_row {
        h += scale(PAD_V_TOTAL, dpi) + font_px;
        if rows > 0 {
            h += scale(SEP_BAND, dpi);
        }
    }
    h += rows as i32 * scale(ROW_H, dpi);
    (w, h)
}

/// 同一 composition 中は幅を縮めない（候補の長さ変動で幅が揺れて視線を外すのを防ぐ）。
/// composition が替わったら実績幅を捨てる。純関数。戻り値は呼び出し側で max クランプに
/// 再収めしてから使う（設定/DPI 変更で上限が下がった場合に備える）。
pub(crate) fn held_width(prev: i32, composition_changed: bool, w: i32) -> i32 {
    if composition_changed {
        w
    } else {
        prev.max(w)
    }
}

/// text 色を bg 色と半分ブレンドした「更新待ちの旧候補（stale）」の色。GDI は不透明前提
/// （COLORREF はアルファを捨てる）なのでアルファでなく実ブレンドにし、D2D/GDI の両パスで
/// 同じ見た目にする。
pub(crate) fn dim_text(text: crate::theme::Rgba, bg: crate::theme::Rgba) -> crate::theme::Rgba {
    fn mix(t: u8, b: u8) -> u8 {
        ((t as u32 + b as u32) / 2) as u8
    }
    crate::theme::Rgba {
        r: mix(text.r, bg.r),
        g: mix(text.g, bg.g),
        b: mix(text.b, bg.b),
        a: 255,
    }
}

/// 読み行帯の上端（クライアント座標、下辺からの帯 = パディング+フォント）。区切り線は
/// この直上 SEP_BAND/2、候補行 i の下端は（区切り線位置 − i×ROW_H）。panel_window_size の
/// 高さ式と対応させる帯計算の唯一の出所（paint_gdi / paint_d2d が共有）。
pub(crate) fn reading_band_top(
    client_bottom: i32,
    with_reading_row: bool,
    font_px: i32,
    dpi: i32,
) -> i32 {
    if with_reading_row {
        client_bottom - scale(PAD_V_TOTAL, dpi) - font_px
    } else {
        client_bottom
    }
}

/// 候補欄の下端 y（クライアント座標、paint_gdi / paint_d2d の共有出所）。読み行がある
/// ときは区切り線位置（候補第1行の下端 = 線の上端）、無いとき（ライブ変換 OFF + 候補欄
/// のみ）は**下枠の直上** — 区切り帯は読み行との境界なので、無い形態でまで差し引くと
/// 最上段の候補 top が負になり描画ループの `top < rc.top` で全行描かれない
/// （panel_window_size が読み行なしで SEP_BAND を積まない高さ式と対応）。
pub(crate) fn candidate_rows_bottom(
    client_bottom: i32,
    with_reading_row: bool,
    font_px: i32,
    dpi: i32,
) -> i32 {
    if with_reading_row {
        reading_band_top(client_bottom, true, font_px, dpi) - scale(SEP_BAND, dpi) / 2
    } else {
        client_bottom - BORDER
    }
}

/// HWND ごとの描画状態（GWLP_USERDATA に格納）。
struct MonitorState {
    layout_dpi: i32,
    /// 現在の読み（ひらがな）。打鍵ごとに更新される。空なら読み行を描かない
    /// （ライブ変換 OFF で候補欄のみの形態）。
    text: String,
    /// 予測候補欄（空なら読み行のみの従来形態）。stale 行は薄色で描く。
    candidates: Vec<PanelCandidate>,
    theme: crate::theme::Theme,
    backend: Backend,
    /// 実測幅が上限超過（末尾寄せ描画中）か。show_or_update が設定し paint が読む。
    overflow: bool,
    /// 直近 SetWindowPos したサイズ。同一なら swapchain ResizeBuffers を省く
    /// （renderer.resize に同一サイズ早期リターンが無く、累積 ON では上限到達後も
    /// 毎打鍵フル再構築になるため — spec 性能C2）。
    last_size: (i32, i32),
    /// 同一 composition 中の実績幅（幅縮小抑制 — held_width）。composition 切替で 0 に戻る。
    held_w: i32,
    held_comp: u64,
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
            popup::paint_guarded::<MonitorState>(hwnd, "ReadingMonitor.WM_PAINT", || paint(hwnd));
            LRESULT(0)
        }
        WM_TIMER => unsafe {
            // 退場フェード完了タイマは世代ID判定（KillTimer はキュー済みの WM_TIMER
            // を除去できないため、旧世代の発火を切り捨てる — 候補窓/HUD と同じ仕組み）。
            // 巡3 G6: state 欠落（WM_NCDESTROY 後等）は判定不能だが可視のまま残す理由も
            // 無い — 1438196 構造に倣い隠して抜ける（SW_HIDE は冪等）。
            let Some(s) = monitor_state(hwnd) else {
                if wparam.0 >= 1000 {
                    let _ = KillTimer(Some(hwnd), wparam.0);
                }
                let _ = ShowWindow(hwnd, SW_HIDE);
                return LRESULT(0);
            };
            if wparam.0 >= 1000 {
                let _ = KillTimer(Some(hwnd), wparam.0);
            }
            if !s.backend.is_current_fade_timer(wparam.0) {
                return LRESULT(0);
            }
            s.backend.fade_timer_id = 0;
            s.backend.fading_out = false;
            let _ = ShowWindow(hwnd, SW_HIDE);
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
        let paint_session = popup::PaintSession::begin(hwnd);
        let hdc = paint_session.hdc();
        if hdc.is_invalid() {
            return;
        }
        let state = match monitor_state(hwnd) {
            Some(s) => s,
            None => {
                return;
            }
        };
        let dpi = state.layout_dpi;
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
        let pad = scale(PAD_H, dpi);
        let with_reading = !state.text.is_empty();
        let rows = state.candidates.len();
        let font_px = font_size_px(point_tenths, dpi).ceil() as i32;
        // 帯の割り出しは reading_band_top（panel_window_size と対応する唯一の出所）経由。
        // 候補欄は読み行の上側に広がる（読み行の位置は候補の有無で動かない — 統合パネルの
        // 視線固定要件）。候補行は candidate_rows_bottom（読み行あり=区切り線位置 / なし=
        // 下枠直上）から上へ ROW_H 刻み。区切り線は読み行との境界なので読み行があるとき
        // だけ描く。
        let reading_top = reading_band_top(rc.bottom, with_reading, font_px, dpi);
        if rows > 0 {
            let rows_bottom = candidate_rows_bottom(rc.bottom, with_reading, font_px, dpi);
            let row_h = scale(ROW_H, dpi);
            if with_reading {
                let line = CreateSolidBrush(COLORREF(colors.border.colorref()));
                let _ = FillRect(
                    hdc,
                    &RECT {
                        left: rc.left,
                        top: rows_bottom,
                        right: rc.right,
                        bottom: rows_bottom + 1,
                    },
                    line,
                );
                let _ = DeleteObject(line.into());
            }
            for (i, cand) in state.candidates.iter().enumerate() {
                let bottom = rows_bottom - i as i32 * row_h;
                let top = bottom - row_h;
                if top < rc.top {
                    break;
                }
                let color = if cand.stale {
                    dim_text(colors.text, colors.bg)
                } else {
                    colors.text
                };
                let _ = SetTextColor(hdc, COLORREF(color.colorref()));
                let mut text: Vec<u16> = cand.text.encode_utf16().collect();
                let mut tr = RECT {
                    left: rc.left + pad + scale(GUTTER_W, dpi),
                    top,
                    right: rc.right - pad,
                    bottom,
                };
                let _ = DrawTextW(
                    hdc,
                    &mut text,
                    &mut tr,
                    DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
                // ガターは 1 行目だけ "Tab"（候補窓 preview の preview_index 規約を踏襲）。
                // 選択色は付けない（選べる情報が出ているだけの表示）。
                if i == 0 {
                    let _ = SetTextColor(hdc, COLORREF(colors.index.colorref()));
                    let mut tab: Vec<u16> = "Tab".encode_utf16().collect();
                    let mut gr = RECT {
                        left: rc.left + pad,
                        top,
                        right: rc.left + pad + scale(GUTTER_W, dpi),
                        bottom,
                    };
                    let _ = DrawTextW(
                        hdc,
                        &mut tab,
                        &mut gr,
                        DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
                    );
                }
            }
        }
        if with_reading {
            let _ = SetTextColor(hdc, COLORREF(colors.text.colorref()));
            let mut text: Vec<u16> = state.text.encode_utf16().collect();
            // 左右パディング分を除いた領域へ 1 行描画（候補窓と同じ DT_SINGLELINE|DT_END_ELLIPSIS）。
            let mut tr = RECT {
                left: rc.left + pad,
                top: reading_top,
                right: rc.right - pad,
                bottom: rc.bottom,
            };
            // 上限超過は末尾寄せ: DT_RIGHT + rect クリップで頭側が切れる（DT_END_ELLIPSIS だと
            // 末尾=最新の読みが消える）。
            let flags = if state.overflow {
                DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_NOPREFIX
            } else {
                DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX
            };
            let _ = DrawTextW(hdc, &mut text, &mut tr, flags);
        }
        if let Some(o) = old {
            let _ = SelectObject(hdc, o);
        }

        let bb = CreateSolidBrush(COLORREF(colors.border.colorref()));
        let _ = FrameRect(hdc, &rc, bb);
        let _ = DeleteObject(bb.into());
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

    let paint_session = popup::PaintSession::begin(hwnd);
    let _hdc = paint_session.hdc();
    let Some(state) = monitor_state(hwnd) else {
        return;
    };
    if state.backend.renderer.is_none() {
        return;
    }
    let dpi = state.layout_dpi;
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);

    let family: Vec<u16> = state
        .theme
        .font_family
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let font_px_f = font_size_px(state.theme.font_point_tenths, dpi);
    let font_px = font_px_f.ceil() as i32;
    // 上限超過は末尾寄せ+トリミング無し（トリミングは整列と無関係に末尾を削るため、
    // TRAILING+トリミング有りだと最新の読みが…で消える。頭側は CLIP が切る）。
    let (align, trim) = if state.overflow {
        (DWRITE_TEXT_ALIGNMENT_TRAILING, false)
    } else {
        (DWRITE_TEXT_ALIGNMENT_LEADING, true)
    };
    let Some(fmt) = state.backend.text_format(&family, font_px_f, align, trim) else {
        return;
    };
    // 候補行は常に先頭寄せ（読み行の overflow 規律とは独立）。
    let candidate_fmt = if state.candidates.is_empty() {
        None
    } else {
        state
            .backend
            .text_format(&family, font_px_f, DWRITE_TEXT_ALIGNMENT_LEADING, true)
    };
    // 以降 state への書き込みは end_draw 後にしか無いので、テーマは不変借用で読む。
    let t = &state.theme;

    // TIP パスでは expect/unwrap を使わず else で対の EndPaint を打って return する。
    let Some(renderer) = state.backend.renderer.as_ref() else {
        return;
    };
    let Ok(ctx) = renderer.begin_draw() else {
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

    ctx.Clear(Some(
        &crate::theme::Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        }
        .d2d(),
    ));
    if let Some(b) = brush(t.colors.bg) {
        ctx.FillRectangle(&rectf, &b);
    }

    let pad = scale(PAD_H, dpi) as f32;
    let with_reading = !state.text.is_empty();
    let rows = state.candidates.len();
    // 帯の割り出しは reading_band_top 経由（GDI パスと同一の出所）。候補欄は読み行の
    // 上側に広がる — 読み行の位置は候補の有無で動かない（統合パネルの視線固定要件）。
    // 候補行の下端は candidate_rows_bottom（読み行なしは下枠直上）— GDI パスと共有の
    // 出所。区切り線は読み行との境界なので読み行があるときだけ描く。
    let reading_top = reading_band_top(rc.bottom, with_reading, font_px, dpi) as f32;
    if rows > 0 {
        let row_h = scale(ROW_H, dpi) as f32;
        let rows_bottom = candidate_rows_bottom(rc.bottom, with_reading, font_px, dpi) as f32;
        let gutter_w = scale(GUTTER_W, dpi) as f32;
        if with_reading {
            if let Some(b) = brush(t.colors.border) {
                ctx.FillRectangle(
                    &D2D_RECT_F {
                        left: rectf.left,
                        top: rows_bottom,
                        right: rectf.right,
                        bottom: rows_bottom + 1.0,
                    },
                    &b,
                );
            }
        }
        for (i, cand) in state.candidates.iter().enumerate() {
            let bottom = rows_bottom - i as f32 * row_h;
            let top = bottom - row_h;
            if top < rc.top as f32 {
                break;
            }
            let color = if cand.stale {
                dim_text(t.colors.text, t.colors.bg)
            } else {
                t.colors.text
            };
            let Some(b) = brush(color) else { continue };
            let Some(cf) = candidate_fmt.as_ref() else {
                break;
            };
            let text_utf16: Vec<u16> = cand.text.encode_utf16().collect();
            ctx.DrawText(
                &text_utf16,
                cf,
                &D2D_RECT_F {
                    left: rectf.left + pad + gutter_w,
                    top,
                    right: rectf.right - pad,
                    bottom,
                },
                &b,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
            // ガターは 1 行目だけ "Tab"（preview_index 規約の踏襲）。選択色は付けない。
            if i == 0 {
                if let Some(ib) = brush(t.colors.index) {
                    let tab: Vec<u16> = "Tab".encode_utf16().collect();
                    ctx.DrawText(
                        &tab,
                        cf,
                        &D2D_RECT_F {
                            left: rectf.left + pad,
                            top,
                            right: rectf.left + pad + gutter_w,
                            bottom,
                        },
                        &ib,
                        D2D1_DRAW_TEXT_OPTIONS_CLIP,
                        DWRITE_MEASURING_MODE_NATURAL,
                    );
                }
            }
        }
    }
    if with_reading {
        let text_rect = D2D_RECT_F {
            left: rectf.left + pad,
            top: reading_top,
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
}

/// GDI パス用のテキスト幅実測（物理px、複数文字列の最大値）。フォントが作れない/測れない
/// ときは None（呼び出し側は MIN クランプで劣化）。
unsafe fn measure_texts_gdi(
    hwnd: HWND,
    state: &mut MonitorState,
    dpi: i32,
    texts: &[&str],
) -> Option<i32> {
    let hdc = GetDC(Some(hwnd));
    if hdc.is_invalid() {
        return None;
    }
    let family = popup::family_utf16z(&state.theme.font_family);
    let hfont = state
        .backend
        .font_for_dpi(&family, state.theme.font_point_tenths, dpi);
    let mut best = 0;
    let ok = match hfont {
        Some(f) => {
            let old = SelectObject(hdc, f.into());
            for t in texts {
                let utf16: Vec<u16> = t.encode_utf16().collect();
                let mut size = SIZE::default();
                if GetTextExtentPoint32W(hdc, &utf16, &mut size).as_bool() {
                    best = best.max(size.cx);
                }
            }
            let _ = SelectObject(hdc, old);
            best > 0
        }
        None => false,
    };
    let _ = ReleaseDC(Some(hwnd), hdc);
    ok.then_some(best)
}

/// 読みモニタ本体。`hwnd` は遅延生成（初回 `show_or_update` まで null）。
pub struct ReadingMonitor {
    hwnd: HWND,
}

impl ReadingMonitor {
    /// HWND を持たない空のモニタを構築する（`TextService::new` 用）。
    pub fn empty() -> Self {
        Self {
            hwnd: HWND(std::ptr::null_mut()),
        }
    }

    /// 窓が生成済みで可視か。UIバグ4 のレイアウト追従（OnLayoutChange）で
    /// 「この窓のために再照会するか」の判定に使う。
    pub fn is_visible(&self) -> bool {
        !self.hwnd.is_invalid() && unsafe { IsWindowVisible(self.hwnd) }.as_bool()
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
            let (width, height) =
                panel_window_size(0, 0, 0, true, 0, 14, 96, max_text_w_px(34, 14));
            let Some((hwnd, renderer)) = popup::create_backed_popup(CLASS_NAME, width, height)
            else {
                self.hwnd = HWND(std::ptr::null_mut());
                return;
            };
            self.hwnd = hwnd;
            // 角丸は両パス、アクリルは D2D パスのみ（mode_hud と同じ。Win10=no-op）。
            crate::render::apply_dwm_chrome(
                hwnd,
                theme.rounded,
                theme.acrylic && renderer.is_some(),
            );
            popup::install_state(
                hwnd,
                Box::new(MonitorState {
                    layout_dpi: 96,
                    text: String::new(),
                    candidates: Vec::new(),
                    theme,
                    backend: Backend::new(renderer),
                    overflow: false,
                    last_size: (0, 0),
                    held_w: 0,
                    held_comp: 0,
                }),
            );
        }
    }

    /// 読み `text` と予測候補 `candidates` を composition 先頭アンカーの上側に1窓で
    /// 表示/更新する（統合パネル）。表示条件の判定は呼び出し側（plan_panel が唯一の
    /// 真実源）。`anchor=None`（矩形取得失敗）は表示中なら前回位置保持・非表示なら
    /// 既定座標（plan_anchor）。`comp_width` は未確定文字列の表示幅の実測（物理px）で
    /// パネル幅の基本 — None なら読み・候補の実測だけで幅を決める。`composition_id`
    /// は幅縮小抑制のリセット区切り（同一 composition 中は幅を縮めない）。
    /// HWND 生成失敗は劣化（何もしない）、text 空+候補なしは hide。
    #[allow(clippy::too_many_arguments)]
    pub fn show_or_update(
        &mut self,
        text: &str,
        candidates: &[PanelCandidate],
        anchor: Option<CaretAnchor>,
        comp_width: Option<i32>,
        composition_id: u64,
        max_chars: u32,
        theme: crate::theme::Theme,
    ) {
        let with_reading_row = !text.is_empty();
        let rows = candidates.len();
        if !with_reading_row && rows == 0 {
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
                state.candidates = candidates.to_vec();
                // DWM chrome に効く属性（角丸/アクリル）が変わったときだけ再適用する
                // （色だけの変化は後段の InvalidateRect による再描画で足りる — HUD と同じ）。
                let chrome_changed =
                    state.theme.rounded != theme.rounded || state.theme.acrylic != theme.acrylic;
                state.theme = theme;
                if chrome_changed {
                    let d2d = state.backend.renderer.is_some();
                    crate::render::apply_dwm_chrome(
                        self.hwnd,
                        state.theme.rounded,
                        state.theme.acrylic && d2d,
                    );
                }
            }
        }
        // アンカー計画を先に確定し、DPI は「これから置く／留まる位置」のモニタから読む
        // （候補窓・HUD と同じ理由 — 窓の現位置 DPI では混合DPIのモニタ越えで外枠と
        // グリフの縮尺が1フレーム食い違う、UIバグ2）。Hold は位置不変なので現位置の
        // DPI が正しい。Fallback の座標は harmless_anchor（前景窓照会）で、位置決め時の
        // 再照会を避けるためここで確定させておく。
        enum Target {
            Move(CaretAnchor),
            Hold,
            Harmless(i32, i32),
        }
        // SetWindowPos/resize は WS_VISIBLE を触らないので、ここでの IsWindowVisible は
        // 従来使っていた ShowWindow の戻り値（呼び出し前の可視状態）と同値。
        let was_visible = unsafe { IsWindowVisible(self.hwnd).as_bool() };
        let (target, dpi) = match plan_anchor(anchor, was_visible) {
            // 巡3 P10: DPI 照会点は「窓を置く側」の y（caret_top があればその行）— y=rc.bottom
            // は上モニタ最下行のとき境界点で MonitorFromPoint が下モニタへ帰属し、上側に置く窓を
            // 下モニタの DPI で計算して混合DPIで食い違う（harmless_anchor の -1 退避と同根）。
            AnchorPlan::Move(a) => {
                let ay = a.caret_top.unwrap_or(a.y);
                (Target::Move(a), popup::dpi_for_anchor(a.x, ay))
            }
            AnchorPlan::Hold => (Target::Hold, popup::window_dpi(self.hwnd)),
            // 初回表示でアンカー取得失敗: 主モニタ左上 (200,200) ではなく、HUD と同じ
            // 「作業領域右下の無害位置」へ劣化させる（UIバグ5 — ctx=Some+照会失敗経路の
            // 左上残存と同じ不自然さをこの窓でも潰す）。
            AnchorPlan::Fallback => {
                let (hx, hy) = popup::harmless_anchor();
                (Target::Harmless(hx, hy), popup::dpi_for_anchor(hx, hy))
            }
        };
        unsafe {
            let Some(state) = monitor_state(self.hwnd) else {
                return;
            };
            state.layout_dpi = dpi;
            let font_px_f = font_size_px(state.theme.font_point_tenths, dpi);
            let font_px = font_px_f.ceil() as i32;
            // テキスト幅は描画と同一エンジンで実測（D2D=DWrite / GDI=GetTextExtentPoint32W）。
            // 測れなければ 0 → panel_window_size の MIN クランプで最小幅に劣化。
            let max_w = max_text_w_px(max_chars, font_px);
            let family = popup::family_utf16z(&state.theme.font_family);
            // 実測は &mut state を要する（フォントキャッシュ）ため、測定対象は先に複製して
            // state への借用を切っておく。
            let reading_text = state.text.clone();
            let reading_w = if !with_reading_row {
                0
            } else if state.backend.renderer.is_some() {
                state
                    .backend
                    .measure_max_width_dwrite(
                        &family,
                        font_px_f,
                        std::slice::from_ref(&reading_text),
                        max_w,
                    )
                    .unwrap_or(0)
            } else {
                measure_texts_gdi(self.hwnd, state, dpi, &[reading_text.as_str()]).unwrap_or(0)
            };
            // 候補欄の幅 = 候補本文の最大実測 + "Tab" ガター。stale 行も同幅で測る
            // （応答差し替えで stale→fresh になっても幅は変わらない）。
            let candidate_w = if rows == 0 {
                0
            } else {
                let owned: Vec<String> = state.candidates.iter().map(|c| c.text.clone()).collect();
                let body = if state.backend.renderer.is_some() {
                    state
                        .backend
                        .measure_max_width_dwrite(&family, font_px_f, &owned, max_w)
                        .unwrap_or(0)
                } else {
                    let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
                    measure_texts_gdi(self.hwnd, state, dpi, &refs).unwrap_or(0)
                };
                body + scale(GUTTER_W, dpi)
            };
            let comp_w = comp_width.unwrap_or(0);
            let (mut w, h) = panel_window_size(
                comp_w,
                reading_w,
                candidate_w,
                with_reading_row,
                rows,
                font_px,
                dpi,
                max_w,
            );
            state.overflow = text_overflows(reading_w, max_w);
            // 同一 composition 中は幅を縮めない（held_width）。設定/DPI 変更で上限が
            // 下がった場合に備え、実績幅も最新の上限へ収めておく。上限は**窓幅**基準
            // （文字領域上限 + 枠・左右パディング）— 文字領域上限で窓幅をクランプすると
            // 枠ぶん描画領域が痩せて overflow 判定と描画が食い違う（panel_frame_w 注記）。
            let min_w = scale(MIN_PANEL_W, dpi);
            let comp_changed = state.held_comp != composition_id;
            state.held_comp = composition_id;
            state.held_w = held_width(state.held_w, comp_changed, w)
                .clamp(min_w, max_window_w(max_w, min_w, dpi));
            w = state.held_w;
            // アンカー上側（caret_top の上に GAP 空けて）。caret_top 不明（無害位置劣化）は
            // アンカー位置へそのまま（下側）— そのときは実キャレットも不明なので上下の
            // 使い分けに意味がない。クランプは**アンカーモニタの作業領域**で行う
            // （place_on_monitor だと配置点のモニタに跨いでしまい、縦積み混合DPIで
            // 着地モニタとサイズDPIが不一致する — 巡1検証 G1。候補窓の
            // place_on_monitor_flipped と同じ「アンカー点基準」に統一）。
            match target {
                Target::Move(a) => {
                    let (dx, dy) = match a.caret_top {
                        Some(top) => (a.x, top - h - scale(GAP, dpi)),
                        None => (a.x, a.y),
                    };
                    // クランプはアンカーモニタ（=窓を置く側）の作業領域で行う。fit_to_work_area は
                    // 「窓が作業領域より広い」場合も (right-w).max(left) の二段クランプで
                    // panic しない（i32::clamp の min>max panic を構造的に回避 — 巡2 A1。
                    // popup.rs に幅超過テストあり。max_chars=100 等の合法設定で窓幅が
                    // 作業領域幅を超えうるため、素の clamp は使えない）。
                    match popup::work_area_at(a.x, a.caret_top.unwrap_or(a.y)) {
                        Some(work) => {
                            let (cx, cy) = popup::fit_to_work_area(dx, dy, w, h, work);
                            popup::set_popup_pos(self.hwnd, Some((cx, cy)), w, h);
                        }
                        // モニタ情報が取れないときは配置希望点を素通し（候補窓の
                        // place_on_monitor_flipped 失敗時と同じ劣化）。
                        None => popup::set_popup_pos(self.hwnd, Some((dx, dy)), w, h),
                    }
                }
                Target::Hold => {
                    // 位置は前回のまま、サイズだけ追従（読みは伸縮する）。
                    popup::set_popup_pos(self.hwnd, None, w, h);
                }
                Target::Harmless(hx, hy) => {
                    let (fx, fy) = popup::place_on_monitor(hx, hy, w, h);
                    popup::set_popup_pos(self.hwnd, Some((fx, fy)), w, h);
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
            // 退場フェード中の再表示なら現在の世代タイマを解除して表示を続行する
            // （旧世代のキュー済み発火は WM_TIMER 側の世代ID判定が切り捨てる）。
            if let Some(s) = monitor_state(self.hwnd) {
                s.backend.cancel_fade_timer(self.hwnd);
            }
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            // 出現モーション（新規出現のみ短いフェード。表示中の打鍵更新は 1.0 へスナップ
            // ＝ちらつかない）。候補窓/HUD と共有の popup::play_entrance。
            if let Some(state) = monitor_state(self.hwnd) {
                let motion = state.theme.motion;
                popup::play_entrance(state, motion, was_visible);
            }
            tip_log(&format!(
                "ev=reading_monitor action={} reading_len={} rows={}",
                if was_visible { "update" } else { "show" },
                text.chars().count(),
                rows
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
                    // 世代ID付きで武装。失敗を無視すると fading_out が立ったまま透明な
                    // TOPMOST 窓が残留する（HUD/候補窓と同じ理由）。失敗時は即時 hide へ劣化。
                    let armed = monitor_state(self.hwnd)
                        .map(|s| s.backend.arm_fade_timer(self.hwnd, ms))
                        .unwrap_or(false);
                    if !armed {
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
    #[test]
    fn paint_panic_stays_inside_window_callback() {
        crate::popup::assert_paint_panic_is_contained(Some(super::wnd_proc));
    }

    #[test]
    fn paint_uses_the_layout_dpi() {
        use super::*;
        unsafe {
            popup::register_class(&CLASS_ATOM, CLASS_NAME, Some(wnd_proc)).unwrap();
            let hwnd = popup::create_popup(CLASS_NAME, Default::default(), 50, 50).unwrap();
            popup::install_state(
                hwnd,
                Box::new(MonitorState {
                    layout_dpi: 96,
                    text: String::new(),
                    candidates: Vec::new(),
                    theme: Default::default(),
                    backend: Backend::new(None),
                    overflow: false,
                    last_size: (0, 0),
                    held_w: 0,
                    held_comp: 0,
                }),
            );
            let mut monitor = ReadingMonitor { hwnd };
            for dpi in [192, 96, 288] {
                popup::TEST_ANCHOR_DPI.set(Some(dpi));
                let anchor = CaretAnchor {
                    x: 100,
                    y: 100,
                    caret_top: Some(80),
                };
                monitor.show_or_update(
                    "にほんご",
                    &[],
                    Some(anchor),
                    None,
                    1,
                    34,
                    Default::default(),
                );
                popup::TEST_ANCHOR_DPI.set(None);
                paint_gdi(hwnd);
                assert_eq!(
                    monitor_state(hwnd).unwrap().backend.cached_font_dpi(),
                    Some(dpi)
                );
            }
        }
    }
    use super::*;

    #[test]
    fn plan_panel_maps_reading_and_candidate_rows_independently() {
        // 読み行は従来の should_show と同じ4条件、候補欄はライブ変換を要求しない。
        // どちらも composing && 候補窓非表示が共通の前提。
        let p = |reading_enabled: bool,
                 prediction_on: bool,
                 composing: bool,
                 live: bool,
                 showing: bool| {
            plan_panel(reading_enabled, prediction_on, composing, live, showing)
        };
        // 両方出る（統合パネルの基本形態）。
        assert!(p(true, true, true, true, false).reading_row);
        assert!(p(true, true, true, true, false).candidate_rows);
        // ライブ変換 OFF: 読み行は出ないが候補欄は出る（独立条件）。
        let off = p(true, true, true, false, false);
        assert!(!off.reading_row);
        assert!(off.candidate_rows);
        // 候補が無い: 読み行のみ（従来の読みモニタと同じ）。
        let only = p(true, false, true, true, false);
        assert!(only.reading_row);
        assert!(!only.candidate_rows);
        // 非合成中 / 候補窓表示中は blocked でどちらの行も出ない。
        for (re, po, co, li, sh) in [
            (true, true, false, true, false),
            (true, true, true, true, true),
        ] {
            let plan = p(re, po, co, li, sh);
            assert!(!plan.reading_row && !plan.candidate_rows);
        }
        // 読み設定 OFF は読み行だけが消え、候補欄は出る（独立条件）。
        let no_reading_setting = p(false, true, true, true, false);
        assert!(!no_reading_setting.reading_row);
        assert!(no_reading_setting.candidate_rows);
        // 候補窓表示中は予測候補があっても隠れる（候補窓に集中する — ユーザ確認済み決定）。
        assert!(!p(true, true, true, true, true).candidate_rows);
    }

    #[test]
    fn compose_monitor_text_joins_and_trims_tail_priority() {
        // 通常: 連結のみ。
        assert_eq!(
            compose_monitor_text("きょうは", "てんき", 64),
            "きょうはてんき"
        );
        // 累積 OFF 相当(committed 空)は current と等価 — OFF 時の現状挙動保証。
        assert_eq!(compose_monitor_text("", "てんき", 64), "てんき");
        // 上限超過は末尾優先で頭を落とす。
        let long = "あ".repeat(70);
        let s = compose_monitor_text(&long, "おわり", 64);
        assert_eq!(s.chars().count(), 64);
        assert!(s.ends_with("おわり"));
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
        let a = CaretAnchor {
            x: 10,
            y: 20,
            caret_top: Some(5),
        };
        // 矩形が取れたら移動。
        assert!(matches!(plan_anchor(Some(a), true), AnchorPlan::Move(_)));
        assert!(matches!(plan_anchor(Some(a), false), AnchorPlan::Move(_)));
        // 取れない+表示中 = 前回位置保持(DEFAULT へ跳ねない — spec UX P-3)。
        assert!(matches!(plan_anchor(None, true), AnchorPlan::Hold));
        // 取れない+非表示 = 既定座標で初期配置。
        assert!(matches!(plan_anchor(None, false), AnchorPlan::Fallback));
    }

    #[test]
    fn panel_window_size_literal_at_96dpi() {
        // 期待値は実装と独立に手計算したリテラルで固定する（同じ scale()/定数で再導出すると
        // 符号・定数取り違えを検出できない — candidate_window の window_size テストと同一規律）。
        // 96DPI(scale=1)・フォント14px・読み100px・候補0px・読み行あり・候補0行:
        // 幅 = 2*1 + 2*10 + clamp(max(0,100,0), 160, 480) = 2+20+160 = 182（MIN_PANEL_W 下駄）。
        // 高さ = 2*1 + 12 + 14 = 28（読み行のみ = 従来の読みモニタと同値）。
        assert_eq!(
            panel_window_size(0, 100, 0, true, 0, 14, 96, 480),
            (182, 28)
        );
        // 候補3行: 高さに区切り帯7 + 3*28 を足す = 28 + 7 + 84 = 119。
        // 幅は候補実測 200 + Tab ガター22 分を呼び出し側が candidate_px_w に積んで渡す前提。
        assert_eq!(
            panel_window_size(0, 100, 222, true, 3, 14, 96, 480),
            (2 + 20 + 222, 119)
        );
        // composition 幅が最も広いときはそれが幅になる（未確定文字列の表示幅を基本にする）。
        assert_eq!(
            panel_window_size(300, 100, 50, true, 3, 14, 96, 480).0,
            2 + 20 + 300
        );
        // ライブ変換 OFF 相当（読み行なし）: 高さは枠 + 候補行のみ = 2 + 84 = 86。
        assert_eq!(panel_window_size(0, 0, 222, false, 3, 14, 96, 480).1, 86);
        // 幅上限: 巨大 composition は max_w_px でクランプ。
        assert_eq!(
            panel_window_size(10_000, 0, 0, true, 0, 14, 96, 480).0,
            2 + 20 + 480
        );
    }

    #[test]
    fn panel_window_size_scales_at_192dpi() {
        // 192DPI(scale=2)・フォント28px・読み100px・候補0行:
        // 幅 = 2*1 + 2*20 + clamp(100, 320, 960) = 2+40+320 = 362（MIN_PANEL_W のスケール下駄）。
        // 高さ = 2*1 + 24 + 28 = 54。
        assert_eq!(
            panel_window_size(0, 100, 0, true, 0, 28, 192, 960),
            (362, 54)
        );
        // 候補3行: 54 + 14 + 3*56 = 236。
        assert_eq!(
            panel_window_size(0, 100, 500, true, 3, 28, 192, 960).1,
            54 + 14 + 168
        );
    }

    #[test]
    fn panel_window_size_survives_max_below_min() {
        // min>max 防御: max_w_px が下限未満でも clamp が panic せず下限に落ちる
        // (max_chars=10×小フォントで実在するエッジ)。
        assert_eq!(
            panel_window_size(0, 0, 0, true, 0, 14, 96, 1).0,
            2 + 20 + 160
        );
    }

    #[test]
    fn held_width_never_shrinks_within_a_composition() {
        // 同一 composition: 縮まない（候補が短くなっても幅を保つ）。
        assert_eq!(held_width(300, false, 200), 300);
        // 伸びる場合は追従。
        assert_eq!(held_width(300, false, 400), 400);
        // composition 切替: 実績幅を捨てて新しい幅から始める。
        assert_eq!(held_width(300, true, 200), 200);
    }

    #[test]
    fn panel_plan_filters_the_data_passed_to_the_panel() {
        // plan は可否判定だけでなく show_or_update へ渡すデータそのものを落とす。
        // 受け側は「text 非空なら読み行を描く」ので、plan.reading_row=false でも生の
        // 読みを渡すと読み表示 OFF／ライブ変換 OFF なのに読み行が出てしまう。
        let candidates = vec![PanelCandidate {
            text: "がぞう".into(),
            stale: false,
        }];
        // 読み行 OFF（読み表示設定 OFF / ライブ変換 OFF）でも候補欄は生きる — 読みだけ落とす。
        let (reading, rows) = PanelPlan {
            reading_row: false,
            candidate_rows: true,
        }
        .filter_display("がぞう".to_string(), candidates.clone());
        assert!(reading.is_empty());
        assert_eq!(rows, candidates);
        // 候補欄 OFF でも読み行は生きる（読みモニタ単体の従来形態）。
        let (reading, rows) = PanelPlan {
            reading_row: true,
            candidate_rows: false,
        }
        .filter_display("がぞう".to_string(), candidates.clone());
        assert_eq!(reading, "がぞう");
        assert!(rows.is_empty());
        // blocked（非合成中 / 候補窓表示中）は両方落とす — 呼び出し側は空を見て hide する。
        let (reading, rows) = PanelPlan {
            reading_row: false,
            candidate_rows: false,
        }
        .filter_display("がぞう".to_string(), candidates);
        assert!(reading.is_empty());
        assert!(rows.is_empty());
    }

    #[test]
    fn width_limit_clamps_window_width_so_reading_tail_stays_visible() {
        // レビュー再現条件: 96DPI・フォント14px・max_chars 34 → 文字領域上限 476px。
        // 実測読み 476px は上限ちょうど（overflow=false）。窓幅 = 476+枠・左右余白22 = 498px
        // なのに、これを**文字領域**上限の 476px でクランプすると文字領域が 456px に痩せ、
        // 末尾寄せへ切り替わらないまま最新の読みの末尾が右端で切れていた。
        let max_w = max_text_w_px(34, 14);
        let (w, _) = panel_window_size(0, 476, 0, true, 0, 14, 96, max_w);
        assert_eq!(w, 2 + 20 + 476);
        // show_or_update と同じ幅確定（held_width → クランプ。クランプ上限は窓幅基準）。
        let min_w = scale(MIN_PANEL_W, 96);
        let settled = held_width(0, true, w).clamp(min_w, max_window_w(max_w, min_w, 96));
        assert_eq!(settled, 498);
        // 最終文字領域 >= 実測幅: overflow=false のままで全体が描ける（末尾が消えない）。
        assert!(settled - panel_frame_w(96) >= 476);
        // 上限超過時は文字領域が上限ちょうどになり、overflow=true で末尾寄せへ切り替わる。
        let (w, _) = panel_window_size(0, 520, 0, true, 0, 14, 96, max_w);
        let settled = held_width(0, true, w).clamp(min_w, max_window_w(max_w, min_w, 96));
        assert_eq!(settled - panel_frame_w(96), max_w);
        assert!(text_overflows(520, max_w));
        // 設定変更で上限が下がった（max_chars 半減）場合も、実績幅は新しい**窓幅**上限へ収まる。
        let half = max_window_w(max_w / 2, min_w, 96);
        let settled = held_width(settled, false, settled).clamp(min_w, half);
        assert!(settled <= half);
        assert!(settled - panel_frame_w(96) <= max_w / 2);
    }

    #[test]
    fn candidate_rows_fit_the_client_area_with_and_without_reading_row() {
        // 読み行なしの形態（ライブ変換 OFF + 候補欄）でも区切り帯を差し引くと最上段の
        // 候補 top が負になり、描画ループの `top < rc.top` で全行描かれない（96DPI・候補
        // 1件なら top=-1）。panel_window_size の高さ式と candidate_rows_bottom の対応で、
        // 読み行あり/なし × 候補1〜3件 × 96/192DPI の全組合せで全候補行がクライアント
        // 領域内に収まることを固定する（描画ループと同一の判定で検証する）。
        for (dpi, font_px) in [(96, 14), (192, 28)] {
            for with_reading in [true, false] {
                for rows in [1usize, 2, 3] {
                    let (_, h) = panel_window_size(
                        0,
                        if with_reading { 100 } else { 0 },
                        0,
                        with_reading,
                        rows,
                        font_px,
                        dpi,
                        max_text_w_px(34, font_px),
                    );
                    let row_h = scale(ROW_H, dpi);
                    let rows_bottom = candidate_rows_bottom(h, with_reading, font_px, dpi);
                    for i in 0..rows {
                        let bottom = rows_bottom - i as i32 * row_h;
                        let top = bottom - row_h;
                        assert!(
                            top >= 0,
                            "row {i} clipped: dpi={dpi} reading={with_reading} rows={rows} top={top}"
                        );
                        assert!(bottom <= h);
                    }
                }
            }
        }
    }

    #[test]
    fn dim_text_blends_halfway_to_background() {
        let bg = crate::theme::Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        };
        let text = crate::theme::Rgba {
            r: 10,
            g: 200,
            b: 255,
            a: 255,
        };
        let dimmed = dim_text(text, bg);
        // 50% ブレンド = (t+b)/2。
        assert_eq!(dimmed.r, 5);
        assert_eq!(dimmed.g, 100);
        assert_eq!(dimmed.b, 127);
        // GDI は不透明前提なのでアルファは 255 のまま。
        assert_eq!(dimmed.a, 255);
    }

    #[test]
    fn reading_band_top_places_reading_row_at_the_bottom() {
        // 96DPI・フォント14: 読み行帯の上端 = 下辺 - 12(パディング) - 14(フォント)。
        assert_eq!(reading_band_top(100, true, 14, 96), 74);
        // 読み行なし形態（ライブ変換 OFF + 候補欄のみ）は下辺まで空ける。
        assert_eq!(reading_band_top(100, false, 14, 96), 100);
    }
}
