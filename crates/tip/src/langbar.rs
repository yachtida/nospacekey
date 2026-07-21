//! SP5/US: ひらがな/半角英数モードを言語バー（システムの IME 表示領域）に「あ/A」で表示する
//! `ITfLangBarItemButton`。nospacekey は従来モードインジケータを一切持たず、無変換/Alt+; で内部
//! conversion-mode は切り替わるのに UI に何も出なかった（ユーザは現在モードが分からなかった）。
//!
//! 現在モード(`is_direct`)とシステムの更新 sink を TextService と `Rc` で共有する。モード切替時に
//! TextService が `is_direct` を更新し、`sink.OnUpdate(TF_LBI_TEXT)` を呼ぶと、システムが `GetText`
//! を再取得して表示（あ/A）を更新する。COM 非依存の `mode_label` は単体テストする。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windows::core::{implement, BOOL, BSTR, GUID, IUnknown, Interface, Ref, Result};
use windows::Win32::Foundation::{E_NOINTERFACE, E_NOTIMPL, POINT, RECT};
use windows::Win32::UI::TextServices::{
    ITfLangBarItemButton, ITfLangBarItemButton_Impl, ITfLangBarItemSink, ITfLangBarItem_Impl,
    ITfMenu, ITfSource, ITfSource_Impl, TfLBIClick, GUID_LBI_INPUTMODE, TF_LANGBARITEMINFO,
    TF_LBI_CLK_RIGHT, TF_LBI_STYLE_BTN_BUTTON,
};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::UI::WindowsAndMessaging::HICON;

use ids::CLSID_NOSPACEKEY;

use crate::globals::ComObjectGuard;

/// 旧・独自 guidItem（現在は未使用）。「一度確定したら変更しない」という判断が過去にあった
/// 経緯を残すため const 自体は残し、実際の登録は well-known `GUID_LBI_INPUTMODE` を使う
/// （B段でタスクバー Input Indicator 統合のため置換。互換リスクは表示識別のみで
/// registration 内に閉じることをレビュー済み）。
#[allow(dead_code)]
pub const GUID_LANGBARITEM_MODE: GUID = GUID::from_u128(0x744eb6a5_d1ea_4b68_97ca_ae2f1a6a1792);

/// langbar 項目のスタイルフラグ。Mozc の GUID_LBI_INPUTMODE ボタンに倣い非メニューの
/// BTN_BUTTON にする。BTN_MENU にすると Win11 のタスクバー入力インジケータでは InitMenu が
/// 呼ばれず右クリックが無反応になる（インジケータは右クリックを OnClick に配送するため）。
/// メニュー表示は OnClick 側で自前ポップアップとして実装する。InitMenu は旧来の言語バー
/// ツールバー経路でのみ使われる。
pub(crate) fn langbar_item_style() -> u32 {
    TF_LBI_STYLE_BTN_BUTTON
}

/// 右クリックメニュー項目 ID。`AddMenuItem` の uid と `OnMenuSelect` の wid で一致させる。
/// 0 は「選択なし」等に使われうるので 1 始まり。ID 重複は menu_tests で機械的に防ぐ。
pub(crate) const MENU_ID_SETTINGS: u32 = 1;
pub(crate) const MENU_ID_TOGGLE_MODE: u32 = 2;

/// メニュー「切替」用トグルコールバックの共有ハンドル。TextService と ModeLangBarItem で
/// Rc 共有し、Activate 期間だけ Some になる（sink と同じ共有パターン）。型を1箇所に固めて
/// 両側の宣言を一致させる。
pub(crate) type ModeToggleHandle = Rc<RefCell<Option<Box<dyn Fn()>>>>;

/// conversion-mode から言語バーに出すモードラベルを返す純関数。
/// direct(半角英数)=「A」, native(ひらがな)=「あ」。ephemeral 非対応の旧 API。
/// `mode_label_ephemeral(is_direct, false)` へ委譲する（非回帰）。
pub fn mode_label(is_direct: bool) -> &'static str {
    mode_label_ephemeral(is_direct, false)
}

/// conversion-mode と ephemeral かなフラグから言語バー/HUD/トレイに出すモードラベルを返す純関数。
/// direct(半角英数)=「A」, 永続かな=「あ」, ephemeral かな（F8 等の一時トリガ中）=「あ˙」。
/// ephemeral は direct のときは無視する（direct 中は ephemeral 状態自体が存在しない）。
pub fn mode_label_ephemeral(is_direct: bool, ephemeral: bool) -> &'static str {
    if is_direct {
        "A"
    } else if ephemeral {
        "あ˙"
    } else {
        "あ"
    }
}

/// 言語バーへ あ/A を出すモードインジケータ。`is_direct`/`sink` を TextService と共有する。
#[implement(ITfLangBarItemButton, ITfSource)]
pub struct ModeLangBarItem {
    /// 現在モード（true=半角英数=A / false=ひらがな=あ）。TextService がトグル時に更新する。
    is_direct: Rc<Cell<bool>>,
    /// ephemeral かなモード中（F8 等の一時トリガ中）かどうか。TextService が
    /// `langbar_is_direct` と並行して更新する。direct=true のときは無視される
    /// （`mode_label_ephemeral` 参照）。
    ephemeral: Rc<Cell<bool>>,
    /// システムが `ITfSource::AdviseSink` で渡してくる更新 sink。TextService が共有参照で
    /// 読み、モード切替時に `OnUpdate` を呼んで表示を再取得させる。
    sink: Rc<RefCell<Option<ITfLangBarItemSink>>>,
    /// メニュー「切替」選択時に呼ぶモード切替コールバック。TextService が Activate で
    /// 自身の COM 参照を捕まえた closure を格納し、Deactivate で None に戻す（sink と同じ共有
    /// パターン）。なぜ closure 注入か: 正規トグル経路 `TextService::toggle_conversion_mode`
    /// は compartment（実変換モード）まで追従させる必要があり、langbar 側で is_direct の Cell を
    /// 直接反転すると実モードと食い違うため、切替の実体は TextService に委譲する。
    on_toggle: ModeToggleHandle,
    _guard: ComObjectGuard,
}

impl ModeLangBarItem {
    pub fn new(
        is_direct: Rc<Cell<bool>>,
        ephemeral: Rc<Cell<bool>>,
        sink: Rc<RefCell<Option<ITfLangBarItemSink>>>,
        on_toggle: ModeToggleHandle,
    ) -> Self {
        Self { is_direct, ephemeral, sink, on_toggle, _guard: ComObjectGuard::new() }
    }
}

impl ITfLangBarItem_Impl for ModeLangBarItem_Impl {
    fn GetInfo(&self, pinfo: *mut TF_LANGBARITEMINFO) -> Result<()> {
        let mut desc = [0u16; 32];
        let name: Vec<u16> = "nospacekey".encode_utf16().collect();
        let n = name.len().min(desc.len() - 1); // NUL 終端ぶん 1 残す
        desc[..n].copy_from_slice(&name[..n]);
        let info = TF_LANGBARITEMINFO {
            clsidService: CLSID_NOSPACEKEY,
            guidItem: GUID_LBI_INPUTMODE,
            dwStyle: langbar_item_style(),
            ulSort: 0,
            szDescription: desc,
        };
        unsafe {
            pinfo.write(info);
        }
        Ok(())
    }

    fn GetStatus(&self) -> Result<u32> {
        Ok(0)
    }

    fn Show(&self, _fshow: BOOL) -> Result<()> {
        Ok(())
    }

    fn GetTooltipString(&self) -> Result<BSTR> {
        Ok(BSTR::from("nospacekey 入力モード"))
    }
}

impl ModeLangBarItem_Impl {
    /// メニュー項目 ID を受けて実処理を行う共有ロジック。旧来の言語バーツールバー経路
    /// （InitMenu→OnMenuSelect）と Win11 タスクバーインジケータ経路（OnClick 右クリックの
    /// 自前ポップアップ）の両方から同じ wid を渡して呼ばれる。分岐を1箇所に固めて二重実装を防ぐ。
    fn handle_menu_command(&self, wid: u32) {
        match wid {
            MENU_ID_SETTINGS => {
                // DLL と同ディレクトリの NospacekeyConfig.exe を起動する。パス解決/起動いずれの
                // 失敗も無視（メニュー選択で panic させない）。
                if let Some(dll_path) = crate::globals::module_file_path() {
                    if let Some(exe) = crate::config_launch::config_exe_path(&dll_path) {
                        unsafe {
                            let _ = crate::config_launch::launch_config_app(&exe);
                        }
                    }
                }
            }
            MENU_ID_TOGGLE_MODE => {
                // 正規トグル経路（TextService::toggle_conversion_mode(None)）へ委譲する。
                // is_direct の Cell を直接反転してはならない — compartment（実変換モード）が
                // 追従せず、キーボードトグルと状態が食い違う。closure は TextService::Activate が
                // 格納し Deactivate で None に戻す。langbar 表示の OnUpdate は
                // toggle_conversion_mode → update_langbar_mode が行うので、ここでは呼ばない
                // （二重更新にしない）。
                if let Some(toggle) = self.on_toggle.borrow().as_ref() {
                    toggle();
                }
            }
            // 未知の wid は無視する（将来のメニュー拡張や誤配送で panic させない）。
            _ => {}
        }
    }
}

impl ITfLangBarItemButton_Impl for ModeLangBarItem_Impl {
    // Win11 のタスクバー入力インジケータは右クリックを（InitMenu ではなく）OnClick に
    // TF_LBI_CLK_RIGHT で配送する。そのため Mozc と同様、右クリック時にここで自前の
    // ポップアップメニューを組み立てて表示し、選択結果を InitMenu/OnMenuSelect と同じ
    // handle_menu_command に流す。左クリックは従来どおり無反応（モード切替は無変換/Alt+; のキー）。
    fn OnClick(&self, click: TfLBIClick, pt: &POINT, _prcarea: *const RECT) -> Result<()> {
        crate::text_service::tip_log(&format!(
            "ev=langbar_onclick click={} x={} y={}",
            click.0, pt.x, pt.y
        ));
        if click == TF_LBI_CLK_RIGHT {
            use windows::core::w;
            use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
            use windows::Win32::UI::WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, DestroyMenu, TrackPopupMenu, MF_STRING,
                TPM_NONOTIFY, TPM_RETURNCMD,
            };
            unsafe {
                // 生成失敗はメニューを出さないだけ（TIP 経路では panic 厳禁）。
                let Ok(hmenu) = CreatePopupMenu() else { return Ok(()) };
                // ラベルは InitMenu と一致させる。AppendMenuW 失敗も握りつぶす（項目が欠けるだけ）。
                let _ = AppendMenuW(hmenu, MF_STRING, MENU_ID_SETTINGS as usize, w!("設定"));
                let _ = AppendMenuW(
                    hmenu,
                    MF_STRING,
                    MENU_ID_TOGGLE_MODE as usize,
                    w!("ひらがな/半角英数 切替"),
                );
                // TPM_RETURNCMD: TrackPopupMenu の戻り BOOL の中身が選択コマンド ID になる
                // （0 は「選択なし/キャンセル」）。TPM_NONOTIFY で親への WM_COMMAND 送出を抑止。
                let owner = GetFocus();
                let cmd = TrackPopupMenu(
                    hmenu,
                    TPM_RETURNCMD | TPM_NONOTIFY,
                    pt.x,
                    pt.y,
                    None,
                    owner,
                    None,
                );
                // DestroyMenu は全経路で必ず呼ぶ（GDI/メニューハンドルのリーク防止）。
                let _ = DestroyMenu(hmenu);
                let wid = cmd.0 as u32;
                if wid != 0 {
                    self.handle_menu_command(wid);
                }
            }
        }
        Ok(())
    }
    fn InitMenu(&self, pmenu: Ref<ITfMenu>) -> Result<()> {
        crate::text_service::tip_log("ev=langbar_initmenu");
        let Some(menu) = pmenu.as_ref() else { return Ok(()) };
        // AddMenuItem(0.62) の pch は &[u16] で cch=slice.len()。NUL 終端を含めると余分な文字が
        // 表示されるので、終端なしの UTF-16 をそのまま渡す。
        let settings: Vec<u16> = "設定".encode_utf16().collect();
        let toggle: Vec<u16> = "ひらがな/半角英数 切替".encode_utf16().collect();
        // ppmenu(サブメニュー) は使わないので null。失敗しても致命でない（メニューが1項目欠ける
        // だけ）ので握りつぶす。TIP 経路では panic 厳禁。
        unsafe {
            let _ = menu.AddMenuItem(
                MENU_ID_SETTINGS,
                0,
                HBITMAP::default(),
                HBITMAP::default(),
                &settings,
                core::ptr::null_mut(),
            );
            let _ = menu.AddMenuItem(
                MENU_ID_TOGGLE_MODE,
                0,
                HBITMAP::default(),
                HBITMAP::default(),
                &toggle,
                core::ptr::null_mut(),
            );
        }
        Ok(())
    }
    fn OnMenuSelect(&self, wid: u32) -> Result<()> {
        // 旧来の言語バーツールバー経路。OnClick と同じ handle_menu_command に委譲する
        // （実処理を1箇所に固めて二重実装を防ぐ）。
        self.handle_menu_command(wid);
        Ok(())
    }
    // TSF 契約: GetIcon が返した HICON は呼び出し側（システム）が DestroyIcon する。
    // このためキャッシュしたハンドルを返してはならない（破棄後のダングリング返却になる）。
    // 呼び出しは OnUpdate(TF_LBI_ICON) 契機のみで低頻度なので毎回生成でよい。
    fn GetIcon(&self) -> Result<HICON> {
        // SAFETY: GetDpiForSystem は引数なしの純粋な問い合わせ。render_mode_icon は GDI
        // オブジェクトを自スコープ内で完結させ、成功時のみ所有権付きの HICON を返す。
        // STA スレッド（TSF 呼び出し文脈）から呼ばれる。
        let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem() } as i32;
        match unsafe {
            crate::langbar_icon::render_mode_icon(self.is_direct.get(), self.ephemeral.get(), dpi)
        } {
            Some(hicon) => Ok(hicon),
            None => Err(E_NOTIMPL.into()), // 生成失敗時はシステム既定表示に劣化
        }
    }
    fn GetText(&self) -> Result<BSTR> {
        Ok(BSTR::from(mode_label_ephemeral(self.is_direct.get(), self.ephemeral.get())))
    }
}

impl ITfSource_Impl for ModeLangBarItem_Impl {
    fn AdviseSink(&self, riid: *const GUID, punk: Ref<IUnknown>) -> Result<u32> {
        // システムは ITfLangBarItemSink を advise してくる。それ以外は受けない。
        if unsafe { *riid } == ITfLangBarItemSink::IID {
            let sink: ITfLangBarItemSink = punk.ok()?.cast()?;
            *self.sink.borrow_mut() = Some(sink);
            Ok(1) // 単一 sink なので固定 cookie。
        } else {
            Err(E_NOINTERFACE.into())
        }
    }

    fn UnadviseSink(&self, _dwcookie: u32) -> Result<()> {
        *self.sink.borrow_mut() = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{mode_label, mode_label_ephemeral};

    #[test]
    fn mode_label_maps_direct_and_native() {
        assert_eq!(mode_label(true), "A"); // 半角英数
        assert_eq!(mode_label(false), "あ"); // ひらがな
    }

    #[test]
    fn ephemeral_kana_shows_dotted_marker() {
        assert_eq!(mode_label_ephemeral(false, false), "あ");
        assert_eq!(mode_label_ephemeral(true, false), "A");
        assert_eq!(mode_label_ephemeral(false, true), "あ˙");
        assert_eq!(mode_label(false), "あ");
        assert_eq!(mode_label(true), "A");
    }
}

#[cfg(test)]
mod style_tests {
    use super::*;

    #[test]
    fn style_is_button() {
        // BTN_BUTTON でなければならない。BTN_MENU にすると Win11 のタスクバー入力
        // インジケータは右クリックを OnClick に配送するため InitMenu が呼ばれず、
        // 右クリックメニューが無反応になる（Mozc の GUID_LBI_INPUTMODE と同じ設計）。
        assert_eq!(langbar_item_style(), TF_LBI_STYLE_BTN_BUTTON);
    }
}

#[cfg(test)]
mod menu_tests {
    use super::*;

    #[test]
    fn menu_ids_are_distinct() {
        // ID 重複という凡ミス（両項目が同じ wid になり切替が設定を起動する等）を機械的に防ぐ。
        assert_ne!(MENU_ID_SETTINGS, MENU_ID_TOGGLE_MODE);
    }
}
