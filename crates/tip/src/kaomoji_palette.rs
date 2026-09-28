//! Focus-free kaomoji search. Only the visible eight rows reach the renderer.
use crate::local_kana_composer::{InputStyle, LocalKanaComposer};
use crate::popup::{self, scale, Backend, PopupState};
use settings::user_dictionary::{normalize_key, UserDictEntry};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::SystemTime;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, DrawTextW, FillRect, FrameRect, SelectObject, SetBkMode,
    SetTextColor, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, GetClientRect, ShowWindow, SW_HIDE, SW_SHOWNOACTIVATE,
    WM_LBUTTONDOWN, WM_MOUSEWHEEL, WM_NCDESTROY, WM_PAINT,
};

const ROWS: usize = 8;
const CLASS: PCWSTR = w!("NospacekeyKaomojiPalette");
static CLASS_ATOM: OnceLock<u16> = OnceLock::new();

#[derive(Default)]
pub(crate) struct Search {
    composer: LocalKanaComposer,
    entries: Vec<UserDictEntry>,
    keys: Vec<String>,
    matches: Vec<usize>,
    selected: usize,
    notice: String,
}
impl Search {
    fn set_entries(&mut self, entries: Vec<UserDictEntry>) {
        self.keys = entries.iter().map(|e| normalize_key(&e.ruby)).collect();
        self.entries = entries;
        self.filter();
    }
    fn query(&self) -> String {
        let mut composer = self.composer.clone();
        composer.finalize_pending_n();
        normalize_key(composer.reading())
    }
    fn filter(&mut self) {
        let query = self.query();
        self.matches = self
            .keys
            .iter()
            .enumerate()
            .filter_map(|(i, k)| k.contains(&query).then_some(i))
            .collect();
        self.selected = 0;
    }
    pub fn text(&mut self, ch: char) {
        self.composer
            .push(ch.to_ascii_lowercase(), InputStyle::Kana);
        self.filter();
    }
    pub fn backspace(&mut self) {
        self.composer.backspace();
        self.filter();
    }
    pub fn move_by(&mut self, delta: i32) {
        self.selected = (self.selected as i64 + delta as i64)
            .clamp(0, self.matches.len().saturating_sub(1) as i64) as usize;
    }
    pub fn insertion_failed(&mut self) {
        self.notice = "挿入できませんでした。Enter で再試行、Esc で閉じる".into();
    }
    pub fn word(&self) -> Option<String> {
        self.matches
            .get(self.selected)
            .and_then(|i| self.entries.get(*i))
            .map(|e| e.word.clone())
    }
    fn visible_start(&self) -> usize {
        self.selected / ROWS * ROWS
    }
    fn lines(&self) -> Vec<(String, bool)> {
        let mut out = vec![(
            format!("顔文字・絵文字  読み: {}", self.composer.reading()),
            false,
        )];
        for (i, &idx) in self
            .matches
            .iter()
            .enumerate()
            .skip(self.visible_start())
            .take(ROWS)
        {
            let e = &self.entries[idx];
            out.push((format!("{}   {}", e.word, e.ruby), i == self.selected));
        }
        if self.matches.is_empty() {
            out.push((
                if self.entries.is_empty() {
                    "設定の顔文字・絵文字タブで登録してください"
                } else {
                    "一致する読みがありません"
                }
                .into(),
                false,
            ));
        }
        while out.len() < ROWS + 1 {
            out.push((String::new(), false));
        }
        out.push((
            if self.notice.is_empty() {
                format!(
                    "{} / {} 件   ↑↓ 選択  Enter 挿入  Esc 閉じる",
                    if self.matches.is_empty() {
                        0
                    } else {
                        self.selected + 1
                    },
                    self.matches.len()
                )
            } else {
                self.notice.clone()
            },
            false,
        ));
        out
    }
}

struct WindowState {
    search: Rc<RefCell<Search>>,
    theme: crate::theme::Theme,
    dpi: i32,
    backend: Backend,
}
impl PopupState for WindowState {
    fn backend_mut(&mut self) -> &mut Backend {
        &mut self.backend
    }
}
extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            popup::paint_guarded::<WindowState>(hwnd, "KaomojiPalette.WM_PAINT", || unsafe {
                paint(hwnd)
            });
            LRESULT(0)
        }
        WM_MOUSEWHEEL | WM_LBUTTONDOWN => {
            popup::paint_guarded::<WindowState>(hwnd, "KaomojiPalette.mouse", || unsafe {
                if let Some(s) = popup::state_mut::<WindowState>(hwnd) {
                    let mut search = s.search.borrow_mut();
                    if msg == WM_MOUSEWHEEL {
                        let delta = (wp.0 >> 16) as u16 as i16;
                        search.move_by(if delta > 0 { -3 } else { 3 });
                    } else {
                        let y = (lp.0 >> 16) as u16 as i16 as i32;
                        let row = y / scale(32, s.dpi) - 1;
                        if y >= scale(32, s.dpi) && (0..ROWS as i32).contains(&row) {
                            let idx = search.visible_start() + row as usize;
                            if idx < search.matches.len() {
                                search.selected = idx;
                            }
                        }
                    }
                    drop(search);
                    let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false);
                }
            });
            LRESULT(0)
        }
        WM_NCDESTROY => unsafe {
            drop(popup::take_state::<WindowState>(hwnd));
            DefWindowProcW(hwnd, msg, wp, lp)
        },
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
unsafe fn paint(hwnd: HWND) {
    let guard = popup::PaintSession::begin(hwnd);
    let hdc = guard.hdc();
    let Some(s) = popup::state_mut::<WindowState>(hwnd) else {
        return;
    };
    let lines = s.search.borrow().lines();
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let row = scale(32, s.dpi);
    let pad = scale(10, s.dpi);
    let colors = s.theme.colors;
    let family = popup::family_utf16z(&s.theme.font_family);
    if s.backend.renderer.is_some() {
        use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
        use windows::Win32::Graphics::Direct2D::{
            D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
            D2D1_ROUNDED_RECT,
        };
        use windows::Win32::Graphics::DirectWrite::{
            DWRITE_MEASURING_MODE_NATURAL, DWRITE_TEXT_ALIGNMENT_LEADING,
        };
        let Some(fmt) = s.backend.text_format(
            &family,
            popup::font_size_px(s.theme.font_point_tenths, s.dpi),
            DWRITE_TEXT_ALIGNMENT_LEADING,
            true,
        ) else {
            return;
        };
        let Some(renderer) = s.backend.renderer.as_ref() else {
            return;
        };
        let Ok(ctx) = renderer.begin_draw() else {
            return;
        };
        ctx.SetDpi(96.0, 96.0);
        ctx.Clear(Some(&colors.bg.d2d()));
        for (i, (line, selected)) in lines.iter().enumerate() {
            let rect = D2D_RECT_F {
                left: pad as f32,
                top: (i as i32 * row) as f32,
                right: (rc.right - pad) as f32,
                bottom: ((i as i32 + 1) * row) as f32,
            };
            if *selected {
                if let Ok(b) = ctx.CreateSolidColorBrush(&colors.sel_bg.d2d(), None) {
                    ctx.FillRoundedRectangle(
                        &D2D1_ROUNDED_RECT {
                            rect,
                            radiusX: scale(crate::theme::tokens::RADIUS_SM, s.dpi) as f32,
                            radiusY: scale(crate::theme::tokens::RADIUS_SM, s.dpi) as f32,
                        },
                        &b,
                    );
                }
            }
            let color = if *selected {
                colors.sel_text
            } else {
                colors.text
            };
            if let Ok(b) = ctx.CreateSolidColorBrush(&color.d2d(), None) {
                ctx.DrawText(
                    &line.encode_utf16().collect::<Vec<_>>(),
                    &fmt,
                    &rect,
                    &b,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP | D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
        if let Ok(b) = ctx.CreateSolidColorBrush(&colors.border.d2d(), None) {
            ctx.DrawRectangle(
                &D2D_RECT_F {
                    left: 0.5,
                    top: 0.5,
                    right: rc.right as f32 - 0.5,
                    bottom: rc.bottom as f32 - 0.5,
                },
                &b,
                1.0,
                None,
            );
        }
        if let Err(e) = renderer.end_draw() {
            s.backend.renderer_dead = crate::render::is_device_lost(&e);
        }
        return;
    }
    if hdc.is_invalid() {
        return;
    }
    let bg = CreateSolidBrush(COLORREF(colors.bg.colorref()));
    let _ = FillRect(hdc, &rc, bg);
    let _ = DeleteObject(bg.into());
    let _ = SetBkMode(hdc, TRANSPARENT);
    let old = s
        .backend
        .font_for_dpi(&family, s.theme.font_point_tenths, s.dpi)
        .map(|f| SelectObject(hdc, f.into()));
    for (i, (line, selected)) in lines.iter().enumerate() {
        let mut tr = RECT {
            left: pad,
            top: i as i32 * row,
            right: rc.right - pad,
            bottom: (i as i32 + 1) * row,
        };
        if *selected {
            let b = CreateSolidBrush(COLORREF(colors.sel_bg.colorref()));
            let _ = FillRect(hdc, &tr, b);
            let _ = DeleteObject(b.into());
        }
        let _ = SetTextColor(
            hdc,
            COLORREF(
                if *selected {
                    colors.sel_text
                } else {
                    colors.text
                }
                .colorref(),
            ),
        );
        let _ = DrawTextW(
            hdc,
            &mut line.encode_utf16().collect::<Vec<_>>(),
            &mut tr,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
    }
    if let Some(old) = old {
        let _ = SelectObject(hdc, old);
    }
    let b = CreateSolidBrush(COLORREF(colors.border.colorref()));
    let _ = FrameRect(hdc, &rc, b);
    let _ = DeleteObject(b.into());
}

pub(crate) struct Palette {
    hwnd: HWND,
    pub search: Rc<RefCell<Search>>,
    pub owner: Option<windows::Win32::UI::TextServices::ITfContext>,
    stamp: Option<(SystemTime, u64)>,
}
impl Default for Palette {
    fn default() -> Self {
        Self {
            hwnd: HWND(std::ptr::null_mut()),
            search: Rc::new(RefCell::new(Search::default())),
            owner: None,
            stamp: None,
        }
    }
}
impl Palette {
    pub fn close(&mut self) {
        self.owner = None;
        if !self.hwnd.is_invalid() {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }
    pub fn destroy(&mut self) {
        self.close();
        if !self.hwnd.is_invalid() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
            self.hwnd = HWND(std::ptr::null_mut());
        }
    }
    pub fn open(
        &mut self,
        ctx: &windows::Win32::UI::TextServices::ITfContext,
        x: i32,
        y: i32,
        theme: crate::theme::Theme,
    ) {
        self.search.borrow_mut().composer.clear();
        self.search.borrow_mut().notice.clear();
        if let Some(path) = settings::user_dictionary::kaomoji_path() {
            let stamp = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok().map(|t| (t, m.len())));
            if stamp.is_none() || stamp != self.stamp {
                match settings::user_dictionary::load_from(&path) {
                    Ok(loaded) => {
                        self.search.borrow_mut().set_entries(loaded.entries);
                        self.stamp = stamp;
                        if loaded.corrupt == settings::user_dictionary::DictCorrupt::Quarantined {
                            self.search.borrow_mut().notice =
                                "壊れた辞書を退避しました。設定で再取込してください".into();
                        }
                    }
                    Err(_) => {
                        self.stamp = None;
                        self.search.borrow_mut().set_entries(Vec::new());
                        self.search.borrow_mut().notice =
                            "顔文字辞書を読み込めません。設定で確認してください".into();
                    }
                }
            }
        } else {
            self.search.borrow_mut().set_entries(Vec::new());
        }
        self.search.borrow_mut().filter();
        popup::recover_if_device_lost::<WindowState>(&mut self.hwnd, "ev=kaomoji_device_lost");
        if self.hwnd.is_invalid() {
            if popup::register_class(&CLASS_ATOM, CLASS, Some(wnd_proc)).is_none() {
                return;
            }
            unsafe {
                let Some((hwnd, renderer)) = popup::create_backed_popup(CLASS, 440, 320) else {
                    return;
                };
                self.hwnd = hwnd;
                popup::install_state(
                    hwnd,
                    Box::new(WindowState {
                        search: self.search.clone(),
                        theme: theme.clone(),
                        dpi: 96,
                        backend: Backend::new(renderer),
                    }),
                );
            }
        }
        self.owner = Some(ctx.clone());
        let dpi = popup::dpi_for_anchor(x, y);
        let (width, height) = (scale(440, dpi), scale(32 * (ROWS as i32 + 2), dpi));
        let (x, y) = popup::place_on_monitor(x, y, width, height);
        unsafe {
            if let Some(s) = popup::state_mut::<WindowState>(self.hwnd) {
                s.theme = theme;
                s.dpi = dpi;
                crate::render::apply_dwm_chrome(
                    self.hwnd,
                    s.theme.rounded,
                    s.theme.acrylic && s.backend.renderer.is_some(),
                );
            }
            popup::set_popup_pos(self.hwnd, Some((x, y)), width, height);
            popup::resize_and_invalidate::<WindowState>(self.hwnd, width, height);
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
    }
    pub fn repaint(&self) {
        if self.hwnd.is_invalid() {
            return;
        }
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(self.hwnd), None, false);
        }
    }
}
impl Drop for Palette {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// Both TSF test and delivery use this exact set. Unsupported shortcuts close the palette.
pub(crate) fn claims(vk: u32, command_modifier: bool) -> bool {
    !command_modifier
        && (matches!(
            vk,
            0x08 | 0x0D | 0x1B | 0x20 | 0x21 | 0x22 | 0x23 | 0x24 | 0x25 | 0x26 | 0x27 | 0x28
        ) || ((0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk))
            || (0x60..=0x6F).contains(&vk)
            || matches!(vk,0xBA..=0xC0|0xDB..=0xDF|0xE2))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(ruby: &str, word: &str) -> UserDictEntry {
        UserDictEntry {
            ruby: ruby.into(),
            word: word.into(),
            pos: None,
        }
    }
    #[test]
    fn kana_query_is_local_and_partial() {
        let mut s = Search::default();
        s.set_entries(vec![
            entry("ニコニコ", "(^_^)"),
            entry("なく", "(;_;)"),
            entry("えがお", "🙂"),
        ]);
        for ch in "niko".chars() {
            s.text(ch);
        }
        assert_eq!(s.word().as_deref(), Some("(^_^)"));
        assert_eq!(s.matches.len(), 1);
        s.backspace();
        assert!(s.word().is_some());
    }
    #[test]
    fn pending_n_search_does_not_destroy_subsequent_roman_input() {
        let mut s = Search::default();
        s.set_entries(vec![entry("かん", "A"), entry("かに", "B")]);
        for ch in "kan".chars() {
            s.text(ch);
        }
        assert_eq!(s.word().as_deref(), Some("A"));
        s.text('i');
        assert_eq!(s.word().as_deref(), Some("B"));
    }
    #[test]
    fn paint_panics_stay_inside_window_callback() {
        crate::popup::assert_paint_panic_is_contained(Some(wnd_proc));
    }
    #[test]
    fn navigation_is_virtual_and_clamped() {
        let mut s = Search::default();
        s.set_entries((0..10000).map(|i| entry("かお", &i.to_string())).collect());
        s.move_by(9);
        assert_eq!(s.visible_start(), 8);
        assert_eq!(s.word().as_deref(), Some("9"));
        assert_eq!(s.lines().len(), 10);
        s.move_by(i32::MAX);
        assert_eq!(s.word().as_deref(), Some("9999"));
        s.text('z');
        assert_eq!(s.word(), None);
        s.move_by(-8);
        assert_eq!(s.selected, 0);
    }
    #[test]
    fn shortcuts_and_body_keys_are_not_captured() {
        assert!(!claims(0x43, true));
        assert!(!claims(0x70, false));
        assert!(claims(0x0D, false));
        assert!(claims(0x41, false));
    }
}
