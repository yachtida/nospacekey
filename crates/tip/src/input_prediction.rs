//! Non-modal reading completions; selection joins the existing clause/receipt path.
//!
//! 予測の見た目は読み+予測の統合パネル（reading_monitor）が担う。入力を再開しても
//! ready（候補データ）を取り下げない — 読み行は即時更新し、古い候補は stale（identity
//! 不一致）として薄色・選択不可表示に切り替わり、新しい応答が届けば中身だけ差し替わる。
//! イマーシブホスト（UIElement ホスト描画環境）では従来どおりデータ公開のみでホストに
//! 候補を描かせる（host_draws）。
use crate::candidate_window::CandidateUI;
use crate::reading_monitor::{PanelCandidate, PANEL_PREVIEW_ROWS};
use crate::text_service::com_identity_eq;
use crate::text_service::TextService_Impl;
use ipc::clause::{
    ClauseCandidate, ClauseCandidatesRequest, ClauseId, ClauseRequestKey, ReadingPosition,
    SnapshotIdentity,
};
use ipc::client::EngineLearningIdentity;
use ipc::protocol::{Request, Response};
use std::time::{Duration, Instant};
use windows::Win32::UI::TextServices::ITfContext;

#[derive(Default)]
pub(crate) struct InputPredictions {
    pending: Option<(ClauseCandidatesRequest, Instant)>,
    owner: Option<ITfContext>,
    requested: Option<SnapshotIdentity>,
    dismissed: Option<SnapshotIdentity>,
    ready: Option<Ready>,
    pub selecting: bool,
    /// 候補の見た目をホスト（ITfUIElement ホスト描画環境）が担うとき true。統合パネルは
    /// この間候補欄を描かない（二重表示防止）。accept 時に publish_preview の結果で設定。
    host_draws: bool,
}
struct Ready {
    request: ClauseCandidatesRequest,
    candidates: Vec<ClauseCandidate>,
    learning: Option<EngineLearningIdentity>,
}
impl InputPredictions {
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn visible(&self) -> bool {
        self.ready.is_some()
    }
    /// 候補の見た目をホスト（ITfUIElement ホスト描画環境）が担っているか。accept 時に
    /// publish_preview の結果で設定される。
    pub(crate) fn host_draws(&self) -> bool {
        self.host_draws
    }
    /// 候補が**最新**（identity 一致）か。Tab での選択モード開始は fresh に限る — 読みが
    /// 進んで stale になった候補はパネルに薄色で残るが確定対象からは外れる（Tab は素通し）。
    /// identity 突合は accepts と同じ4フィールド一括（configuration/connection 世代も含む）。
    fn fresh(&self, identity: SnapshotIdentity) -> bool {
        self.ready
            .as_ref()
            .is_some_and(|ready| ready.request.key.identity == identity)
    }
    /// 最新の検索が本当に 0 件だったときの取り下げ。ready を落とし、候補欄が減ったかを
    /// 返す（accept 側は true のときだけパネルを再描画する）。
    fn withdraw(&mut self) -> bool {
        self.ready.take().is_some()
    }
    /// 統合パネルの候補欄（先頭 PANEL_PREVIEW_ROWS 件、stale 判定付き）。ホスト描画環境
    /// （host_draws）は候補欄を描かないので空を返す。accepts と同じ突合で fresh か否かを
    /// 行ごとに刻む（表示は残しつつ選択対象外を示す）。
    pub(crate) fn panel_candidates(&self, current: SnapshotIdentity) -> Vec<PanelCandidate> {
        if self.host_draws {
            return Vec::new();
        }
        match &self.ready {
            Some(ready) => ready
                .candidates
                .iter()
                .take(PANEL_PREVIEW_ROWS)
                .map(|candidate| PanelCandidate {
                    text: candidate.surface.clone(),
                    stale: ready.request.key.identity != current,
                })
                .collect(),
            None => Vec::new(),
        }
    }
    fn expire(&mut self, now: Instant, current: SnapshotIdentity) -> bool {
        if self
            .pending
            .as_ref()
            .is_some_and(|(_, deadline)| now >= *deadline)
        {
            return self
                .pending
                .take()
                .is_some_and(|(request, _)| request.key.identity != current);
        }
        false
    }
    fn accepts(
        &self,
        request: &ClauseCandidatesRequest,
        identity: SnapshotIdentity,
        reading: &str,
    ) -> bool {
        !self.selecting
            && self.dismissed != Some(identity)
            && request.key.identity == identity
            && request.reading == reading
    }
}

impl TextService_Impl {
    pub(crate) fn retain_input_prediction_context(&self, context: Option<&ITfContext>) {
        let owner = self.input_predictions.borrow().owner.clone();
        let changed = owner
            .as_ref()
            .is_some_and(|owner| context.is_none_or(|current| !com_identity_eq(current, owner)));
        if changed {
            if self.input_predictions.borrow().selecting {
                self.reset_abandoned_composition();
                return;
            }
            self.hide_input_prediction_preview(true);
            self.clear_input_predictions();
            // 入力先が変わったので統合パネルも畳む（新しい context の preedit フックが
            // 再構成する。ここでは update_reading_monitor を呼べない — 呼び出し時点で
            // context の切替途中のため）。
            self.reading_monitor.borrow_mut().hide();
        }
    }
    fn prediction_owner_matches(&self, context: &ITfContext) -> bool {
        let owner = self.input_predictions.borrow().owner.clone();
        owner
            .as_ref()
            .is_some_and(|owner| com_identity_eq(owner, context))
    }
    fn prediction_context_is_focused(&self, context: &ITfContext) -> bool {
        let manager = self.thread_mgr.borrow().clone();
        manager
            .and_then(|manager| unsafe { manager.GetFocus().ok() })
            .and_then(|document| unsafe { document.GetTop().ok() })
            .is_some_and(|top| com_identity_eq(&top, context))
    }

    pub(crate) fn prediction_identity(&self) -> SnapshotIdentity {
        self.state.borrow().clause_identity(
            self.configuration_generation.get(),
            self.background_input.connection_generation(),
        )
    }
    pub(crate) fn clear_input_predictions(&self) {
        let visible = self.input_predictions.borrow().visible();
        let dismissed = self.input_predictions.borrow().dismissed;
        *self.input_predictions.borrow_mut() = InputPredictions {
            dismissed,
            ..Default::default()
        };
        if visible {
            self.candidate_ui.borrow_mut().hide();
            // 統合パネルの候補欄も畳む（clear 経路は composition 終了/放棄＝読み行も
            // 隠れてよい。次の preedit フックで plan_panel が再構成する）。
            self.reading_monitor.borrow_mut().hide();
        }
    }
    pub(crate) fn hide_input_prediction_preview(&self, dismiss: bool) {
        let identity = self.prediction_identity();
        let visible = {
            let mut state = self.input_predictions.borrow_mut();
            if dismiss {
                state.dismissed = Some(identity);
            }
            state.ready.take().is_some()
        };
        if visible {
            self.candidate_ui.borrow_mut().hide();
        }
    }
    pub(crate) fn submit_input_predictions(&self, context: &ITfContext) {
        if !self.input_prediction_enabled.get()
            || self.local_clauses.borrow().is_some()
            || self.explicit_snapshot_pending.get()
            || self.showing.get()
            || self.reconverting.get()
            || (self.is_direct_mode() && !self.ephemeral_kana.get())
            || self.is_password_context(context)
        {
            return;
        }
        if !self.prediction_context_is_focused(context) {
            return;
        }
        let identity = self.prediction_identity();
        if self.acknowledged_configuration_generation.get() != identity.configuration_generation
            || self.acknowledged_snapshot_connection_generation.get()
                != identity.connection_generation
        {
            return;
        }
        let reading = {
            let input = self.state.borrow();
            if !input.composing || input.notation_fixed.is_some() {
                return;
            }
            ipc::clause::normalize_reading(input.canonical_reading())
        };
        if reading.is_empty() {
            self.hide_input_prediction_preview(false);
            return;
        }
        let mut state = self.input_predictions.borrow_mut();
        if state.pending.is_some()
            || state.selecting
            || state.requested == Some(identity)
            || state.dismissed == Some(identity)
        {
            return;
        }
        let Some(id) = self.background_input.next_clause_request_id() else {
            return;
        };
        let request = ClauseCandidatesRequest {
            key: ClauseRequestKey {
                identity,
                baseline: 0,
                conversion_revision: 0,
                clause_id: ClauseId(1),
                request_id: id,
            },
            reading: reading.clone(),
            reading_start: ReadingPosition(0),
            reading_end: ReadingPosition(reading.chars().count() as u32),
            preceding_surfaces: vec![],
            include_prefix_candidates: false,
        };
        let deadline = Instant::now() + Duration::from_millis(400);
        state.requested = Some(identity);
        if self
            .clause_worker
            .submit(Request::InputPredictions(request.clone()), deadline)
        {
            state.owner = Some(context.clone());
            state.pending = Some((request, deadline));
            drop(state);
            self.arm_clause_poll();
        }
    }
    pub(crate) fn expire_input_predictions(&self) {
        let identity = self.prediction_identity();
        let latest_needs_request = self
            .input_predictions
            .borrow_mut()
            .expire(Instant::now(), identity);
        if latest_needs_request {
            self.arm_debounce();
        }
    }
    pub(crate) fn accept_input_predictions(&self, reply: crate::clause_worker::ClauseReply) {
        if self.kaomoji_palette.borrow().owner.is_some() { return; }
        let Response::InputPredictionsResult { key, candidates } = reply.response else {
            return;
        };
        let request = {
            let mut state = self.input_predictions.borrow_mut();
            if !state
                .pending
                .as_ref()
                .is_some_and(|(request, _)| request.key == key && key == reply.key)
            {
                return;
            }
            state.pending.take().map(|(request, _)| request).unwrap()
        };
        let identity = self.prediction_identity();
        let reading = ipc::clause::normalize_reading(self.state.borrow().canonical_reading());
        if !self
            .input_predictions
            .borrow()
            .accepts(&request, identity, &reading)
        {
            self.arm_debounce();
            return;
        }
        if !self.input_prediction_enabled.get()
            || !self.state.borrow().composing
            || self.local_clauses.borrow().is_some()
            || self.explicit_snapshot_pending.get()
            || self.showing.get()
        {
            return;
        }
        let Some(context) = self.current_context.borrow().clone() else {
            return;
        };
        let owner_matches = self.prediction_owner_matches(&context);
        if !owner_matches || !self.prediction_context_is_focused(&context) {
            return;
        }
        if self.is_password_context(&context)
            || (self.is_direct_mode() && !self.ephemeral_kana.get())
        {
            return;
        }
        if candidates.is_empty() {
            // 最新の検索が本当に 0 件: 旧候補を明示的に取り下げる（黙って放置すると読みと
            // 無関係な候補が表示に残る）。パネルは候補欄だけ畳み、読み行は維持する。この
            // 経路は accepts 通過後＝応答が fresh なことが確定済み。同 identity の再要求は
            // requested 抑止で起きないため、取り下げたまま安定する。
            let had_candidates = self.input_predictions.borrow_mut().withdraw();
            if had_candidates {
                // 内部データ（ready）と外部公開（UIElement）は別々に畳む。publish_preview で
                // Begin した要素を End しないと、ホスト描画環境では TIP 内部の候補が消えて
                // もホスト側に旧候補リストが描き続く（表示終了の通知は EndUIElement — UILess
                // モードの契約）。Esc 経路の hide_input_prediction_preview と同じ後始末。
                // 自前描画環境では要素は見た目を持たないが、次回応答で新しく Begin し直す
                // のでここで終えてよい。
                self.candidate_ui.borrow_mut().hide();
            }
            if had_candidates && !self.input_predictions.borrow().host_draws {
                self.update_reading_monitor(&context);
            }
            return;
        }
        if request.validate_candidates(&candidates).is_err() {
            return;
        }
        let values = candidates
            .iter()
            .map(|c| c.surface.clone())
            .collect::<Vec<_>>();
        // 公開（advertise+データ設定）を先に済ませてホスト描画環境かを確定させてから、
        // 見た目を統合パネル / ホストに振り分ける。自前描画環境では窓を出さないので
        // caret_point は不要 — パネル位置は composition 先頭基準に一本化。
        let host_draws = !self.candidate_ui.borrow_mut().publish_preview(&values);
        {
            let mut state = self.input_predictions.borrow_mut();
            state.ready = Some(Ready {
                request,
                candidates,
                learning: reply.learning_identity.filter(|identity| {
                    !self
                        .receipt_outbox
                        .borrow()
                        .as_ref()
                        .is_some_and(|outbox| outbox.rejects_identity(identity))
                }),
            });
            state.host_draws = host_draws;
        }
        if host_draws {
            // イマーシブホスト: 候補は UIElement でホストが描く（現行契約）。読みモニタは
            // 従来どおり隠す（ホストの候補 UI との重なりを避ける）。
            self.reading_monitor.borrow_mut().hide();
        } else {
            // 自前描画: 統合パネルへ（読み行 + 候補欄）。閉じ→開きはしない。
            self.update_reading_monitor(&context);
        }
        crate::text_service::tip_log("ev=input_predictions shown=true");
    }
    pub(crate) fn input_prediction_claims(
        &self,
        vk: u32,
        action: crate::keymap::KeyAction,
        modified: bool,
    ) -> bool {
        if modified || action != crate::keymap::KeyAction::None {
            return false;
        }
        match vk {
            // Tab は最新（fresh）の候補のときだけ食う。読みが進んで stale になった候補は
            // 確定対象から外れている — 選択せず Tab をアプリへ素通しする。選択モード中は
            // 候補送りの Tab（文節変換経路）を食い続ける。
            0x09 => self.input_prediction_fresh() || self.input_predictions.borrow().selecting,
            // Esc は stale でも候補欄を畳む（読み行はパネルに残る）。
            0x1B => {
                self.input_predictions.borrow().visible()
                    || self.input_predictions.borrow().selecting
            }
            _ => false,
        }
    }

    /// 候補が最新（読み/世代が打鍵時点と一致）か。選択モード開始の可否判定。
    pub(crate) fn input_prediction_fresh(&self) -> bool {
        let identity = self.prediction_identity();
        self.input_predictions.borrow().fresh(identity)
    }
    pub(crate) fn enter_input_prediction_selection(
        &self,
        context: &ITfContext,
        index: usize,
    ) -> bool {
        let owner_matches = self.prediction_owner_matches(context);
        if !owner_matches || !self.prediction_context_is_focused(context) {
            self.clear_input_predictions();
            return false;
        }
        let identity = self.prediction_identity();
        let reading = ipc::clause::normalize_reading(self.state.borrow().canonical_reading());
        let ready = self.input_predictions.borrow_mut().ready.take();
        let Some(ready) = ready else {
            return false;
        };
        if !self
            .input_predictions
            .borrow()
            .accepts(&ready.request, identity, &reading)
        {
            // stale や dismiss 済みの候補では選択に入らない。ready は既に take 済みなので
            // 統合パネルの候補欄もここで畳む（Tab はこの経路では素通しされない — claims が
            // fresh のときだけ食うため、通常は stale 中にここへ来ない防御経路）。
            self.candidate_ui.borrow_mut().hide();
            self.update_reading_monitor(context);
            return false;
        }
        let Some(mut model) = crate::clause_conversion::ClauseConversion::from_predictions(
            identity,
            reading,
            ready.candidates,
            index,
        ) else {
            return false;
        };
        model.learning_identity = ready.learning;
        *self.local_clauses.borrow_mut() = Some(model);
        self.input_predictions.borrow_mut().selecting = true;
        self.state.borrow_mut().invalidate_live_snapshot();
        self.state.borrow_mut().invalidate_live_display();
        self.disarm_debounce();
        self.render_local_edit(context);
        true
    }
    pub(crate) fn handle_input_prediction_key(
        &self,
        context: &ITfContext,
        vk: u32,
        action: crate::keymap::KeyAction,
        modified: bool,
    ) -> bool {
        let selecting = self.input_predictions.borrow().selecting;
        // direct（半角英数）モードでは予測を扱わない。旧フォールスルーが担っていた
        // 「モード切替後の最初の打鍵で取り下げ」をここで代行する — 切替直後は preedit
        // フックが走らず update 経由で候補欄が消えないため、打鍵時点で明示的に畳む
        // （submit/accept 側の direct ゲートは再表示を防ぐ）。ephemeral（一時かな）中は
        // 予測 OK — submit/accept と同じ条件。
        if !selecting
            && self.is_direct_mode()
            && !self.ephemeral_kana.get()
            && self.input_predictions.borrow().visible()
        {
            self.hide_input_prediction_preview(false);
            self.update_reading_monitor(context);
        }
        if !modified && action == crate::keymap::KeyAction::None {
            if vk == 0x09 && selecting {
                if let Some(model) = self.local_clauses.borrow_mut().as_mut() {
                    model.advance_candidate(1);
                }
                self.render_local_edit(context);
                return true;
            }
            if vk == 0x09 && self.enter_input_prediction_selection(context, 0) {
                return true;
            }
            if selecting && matches!(vk, 0x1B | 0x08) {
                let reading = self.state.borrow().canonical_reading().to_owned();
                let Some(mut model) = crate::clause_conversion::ClauseConversion::from_reading(
                    self.prediction_identity(),
                    reading,
                ) else {
                    return true;
                };
                model.mode = crate::clause_conversion::OperationMode::Editing;
                *self.local_clauses.borrow_mut() = Some(model);
                self.input_predictions.borrow_mut().selecting = false;
                self.hide_input_prediction_preview(true);
                self.candidate_ui.borrow_mut().hide();
                self.render_local_edit(context);
                return true;
            } else if vk == 0x1B && self.input_predictions.borrow().visible() {
                // Esc は候補欄だけ畳む。読み行はパネルに残り、dismissed を記録するので
                // 同じ読みの間は勝手に再表示されない（読みを延長した identity では再表示可）。
                self.hide_input_prediction_preview(true);
                self.update_reading_monitor(context);
                return true;
            }
        }
        // 通常キーでは候補を取り下げない（入力再開で予測を消さない）。読み行は preedit
        // フックで即時更新され、旧候補は identity 突合で stale 表示に切り替わる。新しい
        // 応答が届けば候補欄の中身だけ差し替わる。選択（Tab）は fresh な候補に限るので、
        // stale 中の Tab は enter_input_prediction_selection が拒否して素通しする。
        false
    }
    pub(crate) fn input_prediction_mouse(
        &self,
        context: &ITfContext,
        action: Option<crate::candidate_uielement::BehaviorAction>,
        sync: bool,
    ) -> bool {
        if !self.input_predictions.borrow().visible() {
            return false;
        }
        match action {
            Some(crate::candidate_uielement::BehaviorAction::Abort) => {
                self.hide_input_prediction_preview(true)
            }
            Some(crate::candidate_uielement::BehaviorAction::Finalize) => {
                let index = self.cand_state.borrow().selected();
                if self.enter_input_prediction_selection(context, index) {
                    self.queue_local_clause_commit(context);
                }
            }
            _ if sync => { /* A hover/selection notification never changes the composition. */ }
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_prediction_retries_cannot_rearm_live_conversion_during_space_wait() {
        use crate::input_module::{InputEvent, KeyEvent, ReplayMode, TextStyle};
        for explicit_pending in [false, true] {
            for expired in [false, true] {
                let service = windows::core::ComObject::new(crate::text_service::TextService::new());
                service.state.borrow_mut().handle(InputEvent::Key(KeyEvent::Text {
                    ch: 'が', style: TextStyle::Kana, replay: ReplayMode::Full, original: None,
                }));
                let request = ClauseCandidatesRequest {
                    key: ClauseRequestKey {
                        identity: service.prediction_identity(), baseline: 0,
                        conversion_revision: 0, clause_id: ClauseId(1), request_id: 1,
                    },
                    reading: "が".into(), reading_start: ReadingPosition(0),
                    reading_end: ReadingPosition(1), preceding_surfaces: vec![],
                    include_prefix_candidates: false,
                };
                service.state.borrow_mut().handle(InputEvent::Key(KeyEvent::Text {
                    ch: 'ぞ', style: TextStyle::Kana, replay: ReplayMode::Full, original: None,
                }));
                service.input_predictions.borrow_mut().pending = Some((request.clone(), Instant::now()));
                service.explicit_snapshot_pending.set(explicit_pending);
                if expired {
                    service.expire_input_predictions();
                } else {
                    service.accept_input_predictions(crate::clause_worker::ClauseReply {
                        key: request.key,
                        response: Response::InputPredictionsResult { key: request.key, candidates: vec![] },
                        learning_identity: None, rebaseline: None,
                    });
                }
                let rearmed = service.debounce_timer.get() != 0;
                service.disarm_debounce();
                assert!(!service.input_predictions.borrow().pending());
                assert_eq!(rearmed, !explicit_pending, "expired={expired}, explicit={explicit_pending}");
            }
        }
    }

    #[test]
    fn preview_rejects_old_reading_focus_connection_config_and_selection() {
        let identity = SnapshotIdentity {
            composition: 1,
            revision: 2,
            configuration_generation: 3,
            connection_generation: 4,
        };
        let request = ClauseCandidatesRequest {
            key: ClauseRequestKey {
                identity,
                baseline: 0,
                conversion_revision: 0,
                clause_id: ClauseId(1),
                request_id: 1,
            },
            reading: "がぞ".into(),
            reading_start: ReadingPosition(0),
            reading_end: ReadingPosition(2),
            preceding_surfaces: vec![],
            include_prefix_candidates: false,
        };
        let mut state = InputPredictions::default();
        assert!(state.accepts(&request, identity, "がぞ"));
        assert!(!state.accepts(&request, identity, "がぞう"));
        for changed in [
            SnapshotIdentity {
                composition: 2,
                ..identity
            },
            SnapshotIdentity {
                revision: 3,
                ..identity
            },
            SnapshotIdentity {
                configuration_generation: 4,
                ..identity
            },
            SnapshotIdentity {
                connection_generation: 5,
                ..identity
            },
        ] {
            assert!(!state.accepts(&request, changed, "がぞ"));
        }
        let now = Instant::now();
        state.pending = Some((request.clone(), now));
        assert!(state.expire(
            now,
            SnapshotIdentity {
                revision: 3,
                ..identity
            }
        ));
        assert!(!state.pending());
        state.pending = Some((request.clone(), now));
        assert!(
            !state.expire(now, identity),
            "unchanged reading must not retry indefinitely"
        );
        assert!(!state.pending());
        state.dismissed = Some(identity);
        assert!(!state.accepts(&request, identity, "がぞ"));
        state.dismissed = None;
        state.selecting = true;
        assert!(!state.accepts(&request, identity, "がぞ"));
    }

    fn candidate(surface: &str) -> ClauseCandidate {
        ClauseCandidate {
            surface: surface.into(),
            token: surface.into(),
            reading_start: ReadingPosition(0),
            reading_end: ReadingPosition(3),
        }
    }

    fn ready_request(identity: SnapshotIdentity) -> ClauseCandidatesRequest {
        ClauseCandidatesRequest {
            key: ClauseRequestKey {
                identity,
                baseline: 0,
                conversion_revision: 0,
                clause_id: ClauseId(1),
                request_id: 1,
            },
            reading: "がぞう".into(),
            reading_start: ReadingPosition(0),
            reading_end: ReadingPosition(3),
            preceding_surfaces: vec![],
            include_prefix_candidates: false,
        }
    }

    #[test]
    fn panel_candidates_truncate_mark_stale_and_vanish_for_host_drawing() {
        let identity = SnapshotIdentity {
            composition: 1,
            revision: 2,
            configuration_generation: 3,
            connection_generation: 4,
        };
        let advanced = SnapshotIdentity {
            revision: 9,
            ..identity
        };
        let mut state = InputPredictions {
            ready: Some(Ready {
                request: ready_request(identity),
                candidates: (0..9).map(|i| candidate(&format!("ほ{i}"))).collect(),
                learning: None,
            }),
            ..Default::default()
        };
        // パネルは先頭 3 件のコンパクト表示（全件は Tab 後の候補窓で選ぶ）。
        let fresh_rows = state.panel_candidates(identity);
        assert_eq!(fresh_rows.len(), PANEL_PREVIEW_ROWS);
        assert_eq!(fresh_rows[0].text, "ほ0");
        // 最新（identity 一致）なら stale は付かない。
        assert!(!fresh_rows.iter().any(|row| row.stale));
        // 読みが進んだ（identity 不一致）旧候補は stale 表示に切り替わる。パネルは閉じない
        // （入力再開でも取り下げない — 見た目だけ選択対象外を示す）。
        let stale_rows = state.panel_candidates(advanced);
        assert_eq!(stale_rows.len(), PANEL_PREVIEW_ROWS);
        assert!(stale_rows.iter().all(|row| row.stale));
        // Tab（選択モード開始）は fresh のときだけ。stale 中は食わず素通しする。
        assert!(state.fresh(identity));
        assert!(!state.fresh(advanced));
        // ホスト描画環境（イマーシブ）ではパネルは候補欄を描かない（二重表示防止）。
        state.host_draws = true;
        assert!(state.panel_candidates(identity).is_empty());
    }

    #[test]
    fn empty_latest_result_withdraws_the_candidates() {
        let identity = SnapshotIdentity {
            composition: 1,
            revision: 2,
            configuration_generation: 3,
            connection_generation: 4,
        };
        let mut state = InputPredictions {
            ready: Some(Ready {
                request: ready_request(identity),
                candidates: vec![candidate("がぞう")],
                learning: None,
            }),
            ..Default::default()
        };
        // 0 件の最新応答は旧候補を明示的に取り下げる（黙って放置しない）。
        assert!(state.withdraw());
        assert!(!state.visible());
        // 取り下げ済みなら何も減らない（accept 側は再描画しない）。
        assert!(!state.withdraw());
    }

    #[test]
    fn tab_claims_only_fresh_candidates_and_esc_claims_any_visible() {
        use crate::keymap::KeyAction;
        let service = windows::core::ComObject::new(crate::text_service::TextService::new());
        let identity = service.prediction_identity();
        service.input_predictions.borrow_mut().ready = Some(Ready {
            request: ready_request(identity),
            candidates: vec![candidate("がぞう")],
            learning: None,
        });
        // fresh: Tab も Esc も食う。
        assert!(service.input_prediction_claims(0x09, KeyAction::None, false));
        assert!(service.input_prediction_claims(0x1B, KeyAction::None, false));
        // 読みが進んで stale になった（打鍵で identity が変わった）：Tab は素通し、
        // Esc は候補欄を畳むために食う。
        use crate::input_module::{InputEvent, KeyEvent, ReplayMode, TextStyle};
        service
            .state
            .borrow_mut()
            .handle(InputEvent::Key(KeyEvent::Text {
                ch: 'ぞ',
                style: TextStyle::Kana,
                replay: ReplayMode::Full,
                original: None,
            }));
        assert!(!service.input_prediction_claims(0x09, KeyAction::None, false));
        assert!(service.input_prediction_claims(0x1B, KeyAction::None, false));
        // 修飾付き・keymap 割当済みの Tab/Esc は食わない。
        assert!(!service.input_prediction_claims(0x09, KeyAction::None, true));
        assert!(!service.input_prediction_claims(0x09, KeyAction::Convert, false));
    }
}
