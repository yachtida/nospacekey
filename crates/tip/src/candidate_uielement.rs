//! SP6a: 候補リストを TSF UI Element として公開する COM オブジェクト。
//! 候補データは Rc<RefCell<CandidateState>> を presenter と共有して読む。
//! Behavior(マウス/タッチ発)は outbox に要求を書き、notify で text_service へ知らせる。
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use windows::core::{implement, BOOL, BSTR, GUID, Result};
use windows::Win32::Foundation::{E_NOTIMPL, LPARAM, WPARAM};
use windows::Win32::UI::TextServices::{
    ITfCandidateListUIElement_Impl, ITfCandidateListUIElementBehavior,
    ITfCandidateListUIElementBehavior_Impl, ITfDocumentMgr,
    ITfIntegratableCandidateListUIElement, ITfIntegratableCandidateListUIElement_Impl,
    ITfUIElement_Impl, TfIntegratableCandidateListSelectionStyle,
    GUID_INTEGRATIONSTYLE_SEARCHBOX, STYLE_ACTIVE_SELECTION,
};
use crate::candidate_state::CandidateState;
use crate::globals::{ComObjectGuard, GUID_UIELEMENT_CANDIDATELIST};
use crate::text_service::tip_log;

/// ホスト(マウス/タッチ)発の候補操作。text_service が drain して既存 commit/cancel 経路で実行する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BehaviorAction { Finalize, Abort }

// --- COM 非依存のテスト可能ロジック ---
pub(crate) fn behavior_set_selection(state: &Rc<RefCell<CandidateState>>, index: u32) {
    state.borrow_mut().set_selection(index as usize);
}
pub(crate) fn behavior_finalize(outbox: &Rc<RefCell<Option<BehaviorAction>>>) {
    *outbox.borrow_mut() = Some(BehaviorAction::Finalize);
}
pub(crate) fn behavior_abort(outbox: &Rc<RefCell<Option<BehaviorAction>>>) {
    *outbox.borrow_mut() = Some(BehaviorAction::Abort);
}

#[implement(ITfCandidateListUIElementBehavior, ITfIntegratableCandidateListUIElement)]
pub struct CandidateListUIElement {
    state: Rc<RefCell<CandidateState>>,
    outbox: Rc<RefCell<Option<BehaviorAction>>>,
    /// presenter と共有する更新フラグ。presenter が UpdateUIElement 前に立て、
    /// ホストの GetUpdatedFlags で read-and-clear する。
    updated_flags: Rc<Cell<u32>>,
    notify: Rc<dyn Fn()>,
    shown: Cell<bool>,
    // C-1: DLL_REF で生存数を数える。ホストが UIElement を保持中に DLL がアンロード
    // されると Behavior 呼び出しで UAF になるため、生存中はアンロードを防ぐ。
    _guard: ComObjectGuard,
}

impl CandidateListUIElement {
    pub fn new(
        state: Rc<RefCell<CandidateState>>,
        outbox: Rc<RefCell<Option<BehaviorAction>>>,
        updated_flags: Rc<Cell<u32>>,
        notify: Rc<dyn Fn()>,
    ) -> Self {
        Self { state, outbox, updated_flags, notify, shown: Cell::new(false), _guard: ComObjectGuard::new() }
    }
}

impl ITfUIElement_Impl for CandidateListUIElement_Impl {
    fn GetDescription(&self) -> Result<BSTR> { Ok(BSTR::from("nospacekey candidate list")) }
    fn GetGUID(&self) -> Result<GUID> { Ok(GUID_UIELEMENT_CANDIDATELIST) }
    fn Show(&self, bshow: BOOL) -> Result<()> { self.shown.set(bshow.as_bool()); Ok(()) }
    fn IsShown(&self) -> Result<BOOL> { Ok(self.shown.get().into()) }
}

impl ITfCandidateListUIElement_Impl for CandidateListUIElement_Impl {
    fn GetUpdatedFlags(&self) -> Result<u32> { Ok(self.updated_flags.replace(0)) }
    fn GetDocumentMgr(&self) -> Result<ITfDocumentMgr> { Err(E_NOTIMPL.into()) }
    fn GetCount(&self) -> Result<u32> {
        let n = self.state.borrow().count() as u32;
        // 診断: ホストが UI-less データ経路を実際に引いているか（=インライン描画する気か）を確認。
        tip_log(&format!("ev=uielement_getcount n={n}"));
        Ok(n)
    }
    fn GetSelection(&self) -> Result<u32> { Ok(self.state.borrow().selected() as u32) }
    fn GetString(&self, uindex: u32) -> Result<BSTR> {
        Ok(self.state.borrow().string_at(uindex as usize).map(BSTR::from).unwrap_or_default())
    }
    fn GetPageIndex(&self, pindex: *mut u32, usize: u32, pupagecnt: *mut u32) -> Result<()> {
        // MVP: 単一ページ。pindex 非 null なら先頭ページ開始 index=0 を 1 件書く。
        unsafe {
            if !pupagecnt.is_null() { *pupagecnt = 1; }
            if !pindex.is_null() && usize >= 1 { *pindex = 0; }
        }
        Ok(())
    }
    fn SetPageIndex(&self, _pindex: *const u32, _upagecnt: u32) -> Result<()> { Ok(()) }
    fn GetCurrentPage(&self) -> Result<u32> { Ok(0) }
}

impl ITfCandidateListUIElementBehavior_Impl for CandidateListUIElement_Impl {
    fn SetSelection(&self, nindex: u32) -> Result<()> {
        behavior_set_selection(&self.state, nindex);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.notify)()));
        Ok(())
    }
    fn Finalize(&self) -> Result<()> {
        behavior_finalize(&self.outbox);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.notify)()));
        Ok(())
    }
    fn Abort(&self) -> Result<()> {
        behavior_abort(&self.outbox);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.notify)()));
        Ok(())
    }
}

// 検索ボックス等のイマーシブホストは自前ウィンドウ（ZBID_DESKTOP バンド）を描けない
// （フライアウトは上位バンドで合成され未署名 TIP は SetWindowBand 不可）。MS の契約では
// この統合インタフェースを実装した TIP の候補リストをホストがインライン候補 UI として描く。
// 実装の有無でホストが pbShow=FALSE（host 描画）へ転じることを期待する。
impl ITfIntegratableCandidateListUIElement_Impl for CandidateListUIElement_Impl {
    /// ホストが統合スタイル（検索ボックス等）を通知。受理して以後 UI-less データ経路で描かせる。
    fn SetIntegrationStyle(&self, guidintegrationstyle: &GUID) -> Result<()> {
        let searchbox = *guidintegrationstyle == GUID_INTEGRATIONSTYLE_SEARCHBOX;
        tip_log(&format!("ev=integ_style searchbox={searchbox}"));
        Ok(())
    }
    /// 選択移動が即インライン反映される「アクティブ選択」。
    fn GetSelectionStyle(&self) -> Result<TfIntegratableCandidateListSelectionStyle> {
        Ok(STYLE_ACTIVE_SELECTION)
    }
    /// 統合時のキーはまず TIP 通常経路（ITfKeyEventSink）で処理させる＝ここでは食わない。
    /// 実機でナビゲーションが壊れるようなら eaten 化＋SetSelection 連携へ拡張する。
    fn OnKeyDown(&self, wparam: WPARAM, _lparam: LPARAM) -> Result<BOOL> {
        tip_log(&format!("ev=integ_onkeydown vk={}", wparam.0 as u32));
        Ok(false.into())
    }
    /// 候補番号（1,2,3…）の表示を許可。
    fn ShowCandidateNumbers(&self) -> Result<BOOL> { Ok(true.into()) }
    /// 既存 Finalize 経路へ委譲（現在選択を確定）。
    fn FinalizeExactCompositionString(&self) -> Result<()> {
        tip_log("ev=integ_finalize");
        behavior_finalize(&self.outbox);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.notify)()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Rc<RefCell<CandidateState>>, Rc<RefCell<Option<BehaviorAction>>>) {
        let st = Rc::new(RefCell::new(CandidateState::new()));
        st.borrow_mut().set(vec!["a".into(), "b".into(), "c".into()], 0);
        (st, Rc::new(RefCell::new(None)))
    }
    #[test]
    fn set_selection_updates_state_no_outbox() {
        let (st, ob) = fixture();
        behavior_set_selection(&st, 2);
        assert_eq!(st.borrow().selected(), 2);
        assert_eq!(*ob.borrow(), None);
    }
    #[test]
    fn finalize_and_abort_post_outbox() {
        let (_st, ob) = fixture();
        behavior_finalize(&ob); assert_eq!(*ob.borrow(), Some(BehaviorAction::Finalize));
        behavior_abort(&ob);    assert_eq!(*ob.borrow(), Some(BehaviorAction::Abort));
    }
}
