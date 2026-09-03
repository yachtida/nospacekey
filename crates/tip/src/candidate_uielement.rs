//! SP6a: 候補リストを TSF UI Element として公開する COM オブジェクト。
//! 候補データは Rc<RefCell<CandidateState>> を presenter と共有して読む。
//! Behavior(マウス/タッチ発)は outbox に要求を書き、notify で text_service へ知らせる。
use crate::candidate_state::{request_selection, CandidateState};
use crate::globals::{ComObjectGuard, GUID_UIELEMENT_CANDIDATELIST};
use crate::text_service::tip_log;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use windows::core::{implement, Result, BOOL, BSTR, GUID};
use windows::Win32::Foundation::{E_NOTIMPL, LPARAM, WPARAM};
use windows::Win32::UI::TextServices::{
    ITfCandidateListUIElementBehavior, ITfCandidateListUIElementBehavior_Impl,
    ITfCandidateListUIElement_Impl, ITfDocumentMgr, ITfIntegratableCandidateListUIElement,
    ITfIntegratableCandidateListUIElement_Impl, ITfUIElement_Impl,
    TfIntegratableCandidateListSelectionStyle, GUID_INTEGRATIONSTYLE_SEARCHBOX,
    STYLE_ACTIVE_SELECTION,
};

/// ホスト(マウス/タッチ)発の候補操作。text_service が drain して既存 commit/cancel 経路で実行する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BehaviorAction {
    Finalize,
    Abort,
}

// --- COM 非依存のテスト可能ロジック ---
/// 選択要求は outbox ではなく専用フラグに立てる — outbox は Option 1 枠しか無く、
/// 保留中の Finalize を上書きするとホスト発の確定が黙って消える。
pub(crate) fn behavior_set_selection(
    active: &Cell<bool>,
    state: &Rc<RefCell<CandidateState>>,
    selection_dirty: &Rc<Cell<bool>>,
    index: u32,
) {
    if !active.get() {
        return;
    }
    request_selection(state, selection_dirty, index as usize);
}
pub(crate) fn behavior_finalize(active: &Cell<bool>, outbox: &Rc<RefCell<Option<BehaviorAction>>>) {
    if !active.get() {
        return;
    }
    *outbox.borrow_mut() = Some(BehaviorAction::Finalize);
}
pub(crate) fn behavior_abort(active: &Cell<bool>, outbox: &Rc<RefCell<Option<BehaviorAction>>>) {
    if !active.get() {
        return;
    }
    *outbox.borrow_mut() = Some(BehaviorAction::Abort);
}

#[implement(
    ITfCandidateListUIElementBehavior,
    ITfIntegratableCandidateListUIElement
)]
pub struct CandidateListUIElement {
    state: Rc<RefCell<CandidateState>>,
    outbox: Rc<RefCell<Option<BehaviorAction>>>,
    /// text_service と共有する選択同期フラグ。SetSelection が立て、drain が preedit へ反映する。
    selection_dirty: Rc<Cell<bool>>,
    /// presenter と共有する更新フラグ。presenter が UpdateUIElement 前に立て、
    /// ホストの GetUpdatedFlags で read-and-clear する。
    updated_flags: Rc<Cell<u32>>,
    notify: Rc<dyn Fn()>,
    active: Rc<Cell<bool>>,
    shown: Cell<bool>,
    // C-1: DLL_REF で生存数を数える。ホストが UIElement を保持中に DLL がアンロード
    // されると Behavior 呼び出しで UAF になるため、生存中はアンロードを防ぐ。
    _guard: ComObjectGuard,
}

impl CandidateListUIElement {
    pub fn new(
        state: Rc<RefCell<CandidateState>>,
        outbox: Rc<RefCell<Option<BehaviorAction>>>,
        selection_dirty: Rc<Cell<bool>>,
        updated_flags: Rc<Cell<u32>>,
        notify: Rc<dyn Fn()>,
        active: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            state,
            outbox,
            selection_dirty,
            updated_flags,
            notify,
            active,
            shown: Cell::new(false),
            _guard: ComObjectGuard::new(),
        }
    }
}

impl ITfUIElement_Impl for CandidateListUIElement_Impl {
    fn GetDescription(&self) -> Result<BSTR> {
        Ok(BSTR::from("nospacekey candidate list"))
    }
    fn GetGUID(&self) -> Result<GUID> {
        Ok(GUID_UIELEMENT_CANDIDATELIST)
    }
    fn Show(&self, bshow: BOOL) -> Result<()> {
        self.shown.set(bshow.as_bool());
        Ok(())
    }
    fn IsShown(&self) -> Result<BOOL> {
        Ok(self.shown.get().into())
    }
}

impl ITfCandidateListUIElement_Impl for CandidateListUIElement_Impl {
    fn GetUpdatedFlags(&self) -> Result<u32> {
        Ok(self.updated_flags.replace(0))
    }
    fn GetDocumentMgr(&self) -> Result<ITfDocumentMgr> {
        Err(E_NOTIMPL.into())
    }
    fn GetCount(&self) -> Result<u32> {
        let n = self.state.borrow().count() as u32;
        // 診断: ホストが UI-less データ経路を実際に引いているか（=インライン描画する気か）を確認。
        tip_log(&format!("ev=uielement_getcount n={n}"));
        Ok(n)
    }
    fn GetSelection(&self) -> Result<u32> {
        Ok(self.state.borrow().selected() as u32)
    }
    fn GetString(&self, uindex: u32) -> Result<BSTR> {
        Ok(self
            .state
            .borrow()
            .string_at(uindex as usize)
            .map(BSTR::from)
            .unwrap_or_default())
    }
    fn GetPageIndex(&self, pindex: *mut u32, usize: u32, pupagecnt: *mut u32) -> Result<()> {
        // MVP: 単一ページ。pindex 非 null なら先頭ページ開始 index=0 を 1 件書く。
        unsafe {
            if !pupagecnt.is_null() {
                *pupagecnt = 1;
            }
            if !pindex.is_null() && usize >= 1 {
                *pindex = 0;
            }
        }
        Ok(())
    }
    fn SetPageIndex(&self, _pindex: *const u32, _upagecnt: u32) -> Result<()> {
        Ok(())
    }
    fn GetCurrentPage(&self) -> Result<u32> {
        Ok(0)
    }
}

impl ITfCandidateListUIElementBehavior_Impl for CandidateListUIElement_Impl {
    fn SetSelection(&self, nindex: u32) -> Result<()> {
        behavior_set_selection(&self.active, &self.state, &self.selection_dirty, nindex);
        if !self.active.get() {
            return Ok(());
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.notify)()));
        Ok(())
    }
    fn Finalize(&self) -> Result<()> {
        behavior_finalize(&self.active, &self.outbox);
        if !self.active.get() {
            return Ok(());
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.notify)()));
        Ok(())
    }
    fn Abort(&self) -> Result<()> {
        behavior_abort(&self.active, &self.outbox);
        if !self.active.get() {
            return Ok(());
        }
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
    fn ShowCandidateNumbers(&self) -> Result<BOOL> {
        Ok(true.into())
    }
    /// 既存 Finalize 経路へ委譲（現在選択を確定）。
    fn FinalizeExactCompositionString(&self) -> Result<()> {
        tip_log("ev=integ_finalize");
        behavior_finalize(&self.active, &self.outbox);
        if !self.active.get() {
            return Ok(());
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.notify)()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    type Fixture = (
        Rc<RefCell<CandidateState>>,
        Rc<RefCell<Option<BehaviorAction>>>,
        Rc<Cell<bool>>,
    );
    fn fixture() -> Fixture {
        let st = Rc::new(RefCell::new(CandidateState::new()));
        st.borrow_mut()
            .set(vec!["a".into(), "b".into(), "c".into()], 0);
        (st, Rc::new(RefCell::new(None)), Rc::new(Cell::new(false)))
    }
    #[test]
    fn set_selection_updates_state_and_requests_preedit_sync_without_touching_outbox() {
        let (st, ob, dirty) = fixture();
        behavior_set_selection(&Cell::new(true), &st, &dirty, 2);
        assert_eq!(st.borrow().selected(), 2);
        assert!(dirty.get(), "選択移動は preedit 同期を要求する");
        assert_eq!(
            *ob.borrow(),
            None,
            "確定/取消スロットは選択移動で汚されない"
        );
    }
    #[test]
    fn set_selection_does_not_displace_a_pending_finalize() {
        let (st, ob, dirty) = fixture();
        let active = Cell::new(true);
        behavior_finalize(&active, &ob);
        behavior_set_selection(&active, &st, &dirty, 1);
        assert_eq!(
            *ob.borrow(),
            Some(BehaviorAction::Finalize),
            "保留中の確定要求は残る"
        );
        assert!(dirty.get());
    }
    #[test]
    fn finalize_and_abort_post_outbox() {
        let (_st, ob, _dirty) = fixture();
        let active = Cell::new(true);
        behavior_finalize(&active, &ob);
        assert_eq!(*ob.borrow(), Some(BehaviorAction::Finalize));
        behavior_abort(&active, &ob);
        assert_eq!(*ob.borrow(), Some(BehaviorAction::Abort));
    }

    #[test]
    fn invalidated_element_cannot_mutate_the_shared_state_used_by_its_successor() {
        let (state, outbox, dirty) = fixture();
        let old = Cell::new(false);
        let current = Cell::new(true);

        behavior_set_selection(&old, &state, &dirty, 2);
        behavior_finalize(&old, &outbox);
        behavior_abort(&old, &outbox);
        assert_eq!(state.borrow().selected(), 0);
        assert!(!dirty.get());
        assert_eq!(*outbox.borrow(), None);

        behavior_set_selection(&current, &state, &dirty, 2);
        behavior_finalize(&current, &outbox);
        assert_eq!(state.borrow().selected(), 2);
        assert!(dirty.get());
        assert_eq!(*outbox.borrow(), Some(BehaviorAction::Finalize));
    }
}
