//! SP6a: 候補表示の統合。自前 Win32 窓と UIElement advertise を CandidateUI 裏に隠す。
//! BeginUIElement の *pbShow で「自前描画(TRUE)」「データ公開のみ(FALSE)」を分岐。
//!
//! 配線(text_service)タスクへの注意: `notify` は UIElement の Behavior 経由でしか
//! 呼ばれない。notify クロージャに **この presenter / element の Rc を捕捉させない**こと
//! （Rc 循環＝リーク）。notify は text_service の弱参照 or イベント経路を指すべき。
use crate::candidate_state::CandidateState;
use crate::candidate_uielement::{BehaviorAction, CandidateListUIElement};
use crate::candidate_window::{CandidateUI, CandidateWindow};
use crate::text_service::tip_log;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use windows::core::BOOL;
use windows::Win32::UI::TextServices::{
    ITfCandidateListUIElementBehavior, ITfUIElement, ITfUIElementMgr, TF_CLUIE_COUNT,
    TF_CLUIE_CURRENTPAGE, TF_CLUIE_PAGEINDEX, TF_CLUIE_SELECTION, TF_CLUIE_STRING,
};

/// 自前描画すべきか: advertise 出来ていて pbShow=FALSE のときだけ「描かない」。
/// それ以外(advertise 無し=フォールバック / pbShow=TRUE=デスクトップ)は自前描画する。
pub(crate) fn should_draw_self(advertised: bool, pbshow: bool) -> bool {
    !advertised || pbshow
}

const CLUIE_FULL: u32 = TF_CLUIE_COUNT
    | TF_CLUIE_SELECTION
    | TF_CLUIE_STRING
    | TF_CLUIE_PAGEINDEX
    | TF_CLUIE_CURRENTPAGE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BeginOutcome {
    NotAttempted,
    Advertised,
    Failed,
}

fn effective_selection_after_begin(
    state: &RefCell<CandidateState>,
    candidates: &[String],
    selected: usize,
    outcome: BeginOutcome,
) -> usize {
    if outcome == BeginOutcome::Failed {
        state.borrow_mut().set(candidates.to_vec(), selected);
    }
    let effective = state.borrow().selected();
    effective
}

pub struct CandidatePresenter {
    window: CandidateWindow,
    state: Rc<RefCell<CandidateState>>,
    outbox: Rc<RefCell<Option<BehaviorAction>>>,
    /// ホスト/マウス発の選択移動を text_service へ知らせる共有フラグ（preedit 同期の要求）。
    selection_dirty: Rc<Cell<bool>>,
    updated_flags: Rc<Cell<u32>>,
    notify: Rc<dyn Fn()>,
    ui_mgr: Option<ITfUIElementMgr>,
    element: Option<ITfUIElement>, // advertise した UIElement（EndUIElement まで保持）
    element_id: Option<u32>,
    element_active: Option<Rc<Cell<bool>>>,
    pbshow: bool,
}

impl CandidatePresenter {
    pub fn new(
        state: Rc<RefCell<CandidateState>>,
        outbox: Rc<RefCell<Option<BehaviorAction>>>,
        selection_dirty: Rc<Cell<bool>>,
        notify: Rc<dyn Fn()>,
    ) -> Self {
        Self {
            // 選択の真実源（cand_state）と preedit 同期フラグを窓と共有する。窓側のマウス
            // クリック選択が presenter を介さず cand_state へ直接書けるようにするため。
            window: CandidateWindow::with_state(state.clone(), selection_dirty.clone()),
            state,
            outbox,
            selection_dirty,
            updated_flags: Rc::new(Cell::new(0)),
            notify,
            ui_mgr: None,
            element: None,
            element_id: None,
            element_active: None,
            pbshow: true,
        }
    }

    /// Activate 時に ITfUIElementMgr を渡す（None=取得失敗→フォールバック自前描画）。
    pub fn set_ui_mgr(&mut self, mgr: Option<ITfUIElementMgr>) {
        // mgr が変わるなら、古い element_id を新 mgr に持ち越さない（Deactivate→Activate の取り違え防止）。
        self.invalidate_element();
        self.element = None;
        self.element_id = None;
        self.pbshow = true;
        self.ui_mgr = mgr;
    }

    fn begin_if_needed(&mut self) -> BeginOutcome {
        if self.element_id.is_some() {
            return BeginOutcome::NotAttempted;
        }
        let Some(mgr) = self.ui_mgr.clone() else {
            // SP6a 診断: ホストが ITfUIElementMgr を出さない＝フォールバックで自前描画。
            tip_log("ev=uielement mgr=none advertise=skip draw=self(fallback)");
            return BeginOutcome::NotAttempted;
        };
        // #[implement(ITfCandidateListUIElementBehavior)] は Behavior 派生の COM
        // オブジェクトを生む。ITfUIElement へは Behavior 経由でアップキャストする。
        let active = Rc::new(Cell::new(true));
        self.element_active = Some(active.clone());
        let behavior: ITfCandidateListUIElementBehavior = CandidateListUIElement::new(
            self.state.clone(),
            self.outbox.clone(),
            self.selection_dirty.clone(),
            self.updated_flags.clone(),
            self.notify.clone(),
            active.clone(),
        )
        .into();
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
                    id,
                    self.pbshow,
                    if should_draw_self(true, self.pbshow) {
                        "self"
                    } else {
                        "host"
                    }
                ));
                BeginOutcome::Advertised
            }
            Err(e) => {
                self.invalidate_element();
                // SP6a 診断: advertise 失敗＝フォールバックで自前描画。
                tip_log(&format!(
                    "ev=uielement advertised=false begin_hr=0x{:08X} draw=self(fallback)",
                    e.code().0 as u32
                ));
                BeginOutcome::Failed
            }
        }
    }
    fn invalidate_element(&mut self) {
        if let Some(active) = self.element_active.take() {
            active.set(false);
        }
        self.outbox.borrow_mut().take();
        self.selection_dirty.set(false);
    }
    fn end(&mut self) {
        self.invalidate_element();
        if let (Some(mgr), Some(id)) = (self.ui_mgr.clone(), self.element_id.take()) {
            unsafe {
                let _ = mgr.EndUIElement(id);
            }
        }
        self.element = None;
        self.pbshow = true;
    }
    fn signal_update(&self, flags: u32) {
        if let (Some(mgr), Some(id)) = (self.ui_mgr.as_ref(), self.element_id) {
            self.updated_flags.set(self.updated_flags.get() | flags);
            unsafe {
                let _ = mgr.UpdateUIElement(id);
            }
        }
    }
    fn advertised(&self) -> bool {
        self.element_id.is_some()
    }

    /// Deactivate から呼ぶ。自前描画窓の DirectComposition/D3D リソースをプロセスが
    /// 健全なうちに畳む（理由は `CandidateWindow::destroy` のコメント参照）。
    /// UIElement 側の後始末（hide/end）は呼び出し元の責務のまま変えない。
    pub fn destroy_window(&mut self) {
        self.window.destroy();
    }

    /// 予測プレビューの**公開のみ**を行う（統合パネル移行後の予測経路）。
    /// advertise（BeginUIElement）と候補データの state 設定までを済ませ、自前窓は出さない。
    /// 戻り値 true = 自前描画を担える環境（デスクトップ pbShow=TRUE / mgr 無しフォール
    /// バック）→ 読み+予測の統合パネルが見た目を担う（element は活性のまま維持 —
    /// 従来 show_preview が pbShow=TRUE で自前窓を出すのと同じ契約の履行）。
    /// false = ホスト描画環境（イマーシブ検索等の pbShow=FALSE）→ データ公開済みなので
    /// ホストが候補リストを描く。既存 element の再公開なら CLUIE_FULL で再読み込みを促す。
    /// 先に advertise を確定させるため、初回呼び出しからホスト/自前の判定が正しく分岐する。
    pub fn publish_preview(&mut self, candidates: &[String]) -> bool {
        self.state.borrow_mut().set(candidates.to_vec(), 0);
        let first = self.element_id.is_none();
        let begin = self.begin_if_needed();
        let _ = effective_selection_after_begin(&self.state, candidates, 0, begin);
        self.window.hide();
        if !first {
            self.signal_update(CLUIE_FULL);
        }
        should_draw_self(self.advertised(), self.pbshow)
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
        let begin = self.begin_if_needed();
        let effective_selected =
            effective_selection_after_begin(&self.state, candidates, selected, begin);
        if should_draw_self(self.advertised(), self.pbshow) {
            // Task 7: 表示ごとに解決し直したテーマを自前窓へそのまま渡す（ホスト描画時は不要）。
            self.window
                .show(candidates, effective_selected, anchor, theme);
        } else {
            self.window.hide();
            // 初回 BeginUIElement はホストが全項目を取りに来るので update 不要。
            // 既存 element の再表示(候補入替)なら全項目変化を通知する。
            if !first {
                self.signal_update(CLUIE_FULL);
            }
        }
    }
    fn hide(&mut self) {
        self.window.hide();
        self.end();
    }
    fn selected(&self) -> usize {
        self.state.borrow().selected()
    }
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
    use crate::candidate_uielement::{behavior_abort, behavior_finalize, behavior_set_selection};
    use windows::Win32::Foundation::E_NOTIMPL;
    use windows::Win32::UI::TextServices::{IEnumTfUIElements, ITfUIElementMgr_Impl};

    /// ホスト（ITfUIElementMgr）の検出器。Begin/Update/End の呼出数だけ数える。
    /// 「0 件応答の後始末（hide）が公開済み UIElement を End する」契約の受け側保証用
    /// （input_prediction の 0 件経路が CandidateUI::hide を呼ぶことと対で成立する）。
    struct MgrCounts {
        pbshow: Cell<bool>,
        begins: Cell<u32>,
        updates: Cell<u32>,
        ends: Cell<u32>,
    }

    #[windows::core::implement(ITfUIElementMgr)]
    struct FakeUiElementMgr {
        counts: Rc<MgrCounts>,
    }

    impl ITfUIElementMgr_Impl for FakeUiElementMgr_Impl {
        fn BeginUIElement(
            &self,
            _pelement: windows::core::Ref<'_, ITfUIElement>,
            pbshow: *mut BOOL,
            pdwuielementid: *mut u32,
        ) -> windows::core::Result<()> {
            unsafe {
                *pbshow = self.counts.pbshow.get().into();
                *pdwuielementid = 1;
            }
            self.counts.begins.set(self.counts.begins.get() + 1);
            Ok(())
        }
        fn UpdateUIElement(&self, _dwuielementid: u32) -> windows::core::Result<()> {
            self.counts.updates.set(self.counts.updates.get() + 1);
            Ok(())
        }
        fn EndUIElement(&self, _dwuielementid: u32) -> windows::core::Result<()> {
            self.counts.ends.set(self.counts.ends.get() + 1);
            Ok(())
        }
        fn GetUIElement(&self, _dwuielementid: u32) -> windows::core::Result<ITfUIElement> {
            Err(E_NOTIMPL.into())
        }
        fn EnumUIElements(&self) -> windows::core::Result<IEnumTfUIElements> {
            Err(E_NOTIMPL.into())
        }
    }
    #[test]
    fn route_selection() {
        assert!(should_draw_self(true, true)); // デスクトップ: 自前描画
        assert!(!should_draw_self(true, false)); // イマーシブ: 描かない
        assert!(should_draw_self(false, false)); // mgr 無し: フォールバック自前描画
        assert!(should_draw_self(false, true));
    }

    #[test]
    fn shared_and_window_selection_match_for_each_begin_outcome() {
        let requested = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let state = Rc::new(RefCell::new(CandidateState::default()));
        state.borrow_mut().set(requested.clone(), 0);
        state.borrow_mut().set_selection(2);
        let window_selected =
            effective_selection_after_begin(&state, &requested, 0, BeginOutcome::Failed);
        assert_eq!(state.borrow().selected(), 0);
        assert_eq!(window_selected, 0);

        state.borrow_mut().set_selection(2);
        let window_selected =
            effective_selection_after_begin(&state, &requested, 0, BeginOutcome::Advertised);
        assert_eq!(state.borrow().selected(), 2);
        assert_eq!(window_selected, 2);

        state.borrow_mut().set(requested.clone(), 0);
        let window_selected =
            effective_selection_after_begin(&state, &requested, 0, BeginOutcome::NotAttempted);
        assert_eq!(state.borrow().selected(), 0);
        assert_eq!(window_selected, 0);
    }

    #[test]
    fn invalidation_clears_retired_payload_and_keeps_only_the_successor_payload() {
        let state = Rc::new(RefCell::new(CandidateState::default()));
        state
            .borrow_mut()
            .set(vec!["a".into(), "b".into(), "c".into()], 0);
        let outbox = Rc::new(RefCell::new(None));
        let dirty = Rc::new(Cell::new(false));
        let mut presenter =
            CandidatePresenter::new(state, outbox.clone(), dirty.clone(), Rc::new(|| {}));
        let active = Rc::new(Cell::new(true));
        presenter.element_active = Some(active.clone());
        behavior_set_selection(&active, &presenter.state, &dirty, 2);
        behavior_finalize(&active, &outbox);

        presenter.invalidate_element();
        assert!(!active.get());
        assert_eq!(*outbox.borrow(), None);
        assert!(!dirty.get());

        let successor = Cell::new(true);
        behavior_abort(&successor, &outbox);
        assert_eq!(*outbox.borrow(), Some(BehaviorAction::Abort));
    }

    #[test]
    fn generic_hide_uses_the_same_element_invalidation_path() {
        let state = Rc::new(RefCell::new(CandidateState::default()));
        let outbox = Rc::new(RefCell::new(Some(BehaviorAction::Finalize)));
        let dirty = Rc::new(Cell::new(true));
        let mut presenter =
            CandidatePresenter::new(state, outbox.clone(), dirty.clone(), Rc::new(|| {}));
        let active = Rc::new(Cell::new(true));
        presenter.element_active = Some(active.clone());

        presenter.hide();
        assert!(!active.get());
        assert_eq!(*outbox.borrow(), None);
        assert!(!dirty.get());
    }

    #[test]
    fn publish_then_hide_ends_the_host_element_and_republish_begins_fresh() {
        // 0 件応答の後始末（CandidateUI::hide）は公開済み UIElement を EndUIElement で
        // 終了しなければならない — End を省くとホスト描画環境では TIP 内部の候補が消えて
        // もホスト側に旧候補リストが描き続く（UILess モードの終了通知契約）。
        // 取り下げ後の次回非空応答は新しい Begin で立て直す。
        let counts = Rc::new(MgrCounts {
            pbshow: Cell::new(false), // ホスト描画環境（pbShow=FALSE）を模す。
            begins: Cell::new(0),
            updates: Cell::new(0),
            ends: Cell::new(0),
        });
        let mgr: ITfUIElementMgr = FakeUiElementMgr {
            counts: Rc::clone(&counts),
        }
        .into();
        let mut presenter = CandidatePresenter::new(
            Rc::new(RefCell::new(CandidateState::default())),
            Rc::new(RefCell::new(None)),
            Rc::new(Cell::new(false)),
            Rc::new(|| {}),
        );
        presenter.set_ui_mgr(Some(mgr));
        // ホスト描画環境の公開: publish_preview はデータ公開のみで自前描画しない（false）。
        assert!(!presenter.publish_preview(&["がぞう".to_string()]));
        assert_eq!(counts.begins.get(), 1);
        assert_eq!(counts.updates.get(), 0);
        // 既存 element の再公開は End ではなく CLUIE_FULL で再読み込みを促す。
        assert!(!presenter.publish_preview(&["がぞう".into(), "ぶぶん".into()]));
        assert_eq!(counts.updates.get(), 1);
        // hide（=0 件応答が呼ぶ後始末）は公開済み element を EndUIElement で終了する。
        presenter.hide();
        assert_eq!(counts.ends.get(), 1);
        // 取り下げ後の次回応答は新規 Begin で立て直す（終了済み id を使い回さない）。
        assert!(!presenter.publish_preview(&["がぞう".to_string()]));
        assert_eq!(counts.begins.get(), 2);
        assert_eq!(counts.updates.get(), 1);
    }
}
