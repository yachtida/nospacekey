//! SP6a: 候補表示の統合。自前 Win32 窓と UIElement advertise を CandidateUI 裏に隠す。
//! BeginUIElement の *pbShow で「自前描画(TRUE)」「データ公開のみ(FALSE)」を分岐。
//!
//! 配線(text_service)タスクへの注意: `notify` は UIElement の Behavior 経由でしか
//! 呼ばれない。notify クロージャに **この presenter / element の Rc を捕捉させない**こと
//! （Rc 循環＝リーク）。notify は text_service の弱参照 or イベント経路を指すべき。
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use windows::core::BOOL;
use windows::Win32::UI::TextServices::{
    ITfCandidateListUIElementBehavior, ITfUIElement, ITfUIElementMgr,
    TF_CLUIE_COUNT, TF_CLUIE_CURRENTPAGE, TF_CLUIE_PAGEINDEX, TF_CLUIE_SELECTION, TF_CLUIE_STRING,
};
use crate::candidate_state::CandidateState;
use crate::candidate_uielement::{BehaviorAction, CandidateListUIElement};
use crate::candidate_window::{CandidateUI, CandidateWindow};
use crate::text_service::tip_log;

/// 自前描画すべきか: advertise 出来ていて pbShow=FALSE のときだけ「描かない」。
/// それ以外(advertise 無し=フォールバック / pbShow=TRUE=デスクトップ)は自前描画する。
pub(crate) fn should_draw_self(advertised: bool, pbshow: bool) -> bool {
    !advertised || pbshow
}

const CLUIE_FULL: u32 = TF_CLUIE_COUNT | TF_CLUIE_SELECTION | TF_CLUIE_STRING | TF_CLUIE_PAGEINDEX | TF_CLUIE_CURRENTPAGE;

pub struct CandidatePresenter {
    window: CandidateWindow,
    state: Rc<RefCell<CandidateState>>,
    outbox: Rc<RefCell<Option<BehaviorAction>>>,
    updated_flags: Rc<Cell<u32>>,
    notify: Rc<dyn Fn()>,
    ui_mgr: Option<ITfUIElementMgr>,
    element: Option<ITfUIElement>, // advertise した UIElement（EndUIElement まで保持）
    element_id: Option<u32>,
    pbshow: bool,
}

impl CandidatePresenter {
    pub fn new(
        state: Rc<RefCell<CandidateState>>,
        outbox: Rc<RefCell<Option<BehaviorAction>>>,
        notify: Rc<dyn Fn()>,
    ) -> Self {
        Self {
            // 選択の真実源（cand_state）を窓と共有する。窓側のマウスクリック選択が
            // presenter を介さず cand_state へ直接書けるようにするため。
            window: CandidateWindow::with_state(state.clone()),
            state, outbox,
            updated_flags: Rc::new(Cell::new(0)),
            notify,
            ui_mgr: None, element: None, element_id: None, pbshow: true,
        }
    }

    /// Activate 時に ITfUIElementMgr を渡す（None=取得失敗→フォールバック自前描画）。
    pub fn set_ui_mgr(&mut self, mgr: Option<ITfUIElementMgr>) {
        // mgr が変わるなら、古い element_id を新 mgr に持ち越さない（Deactivate→Activate の取り違え防止）。
        self.element = None;
        self.element_id = None;
        self.pbshow = true;
        self.ui_mgr = mgr;
    }

    fn begin_if_needed(&mut self) {
        if self.element_id.is_some() { return; }
        let Some(mgr) = self.ui_mgr.clone() else {
            // SP6a 診断: ホストが ITfUIElementMgr を出さない＝フォールバックで自前描画。
            tip_log("ev=uielement mgr=none advertise=skip draw=self(fallback)");
            return;
        };
        // #[implement(ITfCandidateListUIElementBehavior)] は Behavior 派生の COM
        // オブジェクトを生む。ITfUIElement へは Behavior 経由でアップキャストする。
        let behavior: ITfCandidateListUIElementBehavior = CandidateListUIElement::new(
            self.state.clone(), self.outbox.clone(), self.updated_flags.clone(), self.notify.clone(),
        ).into();
        let element: ITfUIElement = behavior.into();
        let mut pbshow = BOOL::from(true);
        let mut id = 0u32;
        // BeginUIElement がホストへ提示。pbShow=FALSE ならホストが描く。
        match unsafe { mgr.BeginUIElement(&element, &mut pbshow, &mut id) } {
            Ok(()) => {
                self.pbshow = pbshow.as_bool();
                self.element = Some(element);
                self.element_id = Some(id);
                // SP6a 診断: advertise 成功。pbShow=TRUE=自前描画(デスクトップ) / FALSE=ホスト描画(イマーシブ)。
                tip_log(&format!(
                    "ev=uielement advertised=true id={} pbshow={} draw={}",
                    id, self.pbshow,
                    if should_draw_self(true, self.pbshow) { "self" } else { "host" }
                ));
            }
            Err(e) => {
                // SP6a 診断: advertise 失敗＝フォールバックで自前描画。
                tip_log(&format!(
                    "ev=uielement advertised=false begin_hr=0x{:08X} draw=self(fallback)",
                    e.code().0 as u32
                ));
            }
        }
    }
    fn end(&mut self) {
        if let (Some(mgr), Some(id)) = (self.ui_mgr.clone(), self.element_id.take()) {
            unsafe { let _ = mgr.EndUIElement(id); }
        }
        self.element = None;
        self.pbshow = true;
    }
    fn signal_update(&self, flags: u32) {
        if let (Some(mgr), Some(id)) = (self.ui_mgr.as_ref(), self.element_id) {
            self.updated_flags.set(self.updated_flags.get() | flags);
            unsafe { let _ = mgr.UpdateUIElement(id); }
        }
    }
    fn advertised(&self) -> bool { self.element_id.is_some() }

    /// Deactivate から呼ぶ。自前描画窓の DirectComposition/D3D リソースをプロセスが
    /// 健全なうちに畳む（理由は `CandidateWindow::destroy` のコメント参照）。
    /// UIElement 側の後始末（hide/end）は呼び出し元の責務のまま変えない。
    pub fn destroy_window(&mut self) {
        self.window.destroy();
    }
}

impl CandidateUI for CandidatePresenter {
    fn show(
        &mut self,
        candidates: &[String],
        selected: usize,
        anchor: crate::candidate_window::CaretAnchor,
        theme: crate::theme::Theme,
    ) {
        self.state.borrow_mut().set(candidates.to_vec(), selected);
        let first = self.element_id.is_none();
        self.begin_if_needed();
        if should_draw_self(self.advertised(), self.pbshow) {
            // Task 7: 表示ごとに解決し直したテーマを自前窓へそのまま渡す（ホスト描画時は不要）。
            self.window.show(candidates, selected, anchor, theme);
        } else {
            self.window.hide();
            // 初回 BeginUIElement はホストが全項目を取りに来るので update 不要。
            // 既存 element の再表示(候補入替)なら全項目変化を通知する。
            if !first { self.signal_update(CLUIE_FULL); }
        }
    }
    fn hide(&mut self) {
        self.window.hide();
        self.end();
    }
    fn selected(&self) -> usize { self.state.borrow().selected() }
    fn move_selection(&mut self, delta: i32) {
        self.state.borrow_mut().move_selection(delta);
        if should_draw_self(self.advertised(), self.pbshow) {
            // 相対 delta を窓へ二重適用せず、cand_state で確定した絶対位置を渡す。
            // マウスクリック（窓側で表示状態を直接更新する）と混在しても乖離しない。
            let sel = self.state.borrow().selected();
            self.window.set_selection(sel);
        } else {
            self.signal_update(TF_CLUIE_SELECTION);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn route_selection() {
        assert!(should_draw_self(true, true));    // デスクトップ: 自前描画
        assert!(!should_draw_self(true, false));  // イマーシブ: 描かない
        assert!(should_draw_self(false, false));  // mgr 無し: フォールバック自前描画
        assert!(should_draw_self(false, true));
    }
}
