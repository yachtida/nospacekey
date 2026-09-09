//! Production commit and close-only sessions, shared with the real TSF harness.
use crate::globals::ComObjectGuard;
use core::mem::ManuallyDrop;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;
use windows::core::{implement, IUnknown, Interface, Result, BOOL, HRESULT, HSTRING};
use windows::Win32::Foundation::{E_FAIL, E_UNEXPECTED};
use windows::Win32::UI::TextServices::{
    ITfComposition, ITfContext, ITfEditSession, ITfEditSession_Impl, ITfInsertAtSelection,
    ITfRange, GUID_PROP_ATTRIBUTE, INSERT_TEXT_AT_SELECTION_FLAGS, TF_AE_NONE, TF_ANCHOR_END,
    TF_E_DISCONNECTED, TF_E_EMPTYCONTEXT, TF_E_INVALIDVIEW, TF_E_NOOBJECT, TF_E_NOSERVICE,
    TF_E_READONLY, TF_SELECTION, TF_SELECTIONSTYLE,
};
/// EndComposition の戻り値と、同期 callback 後にも同じ composition が追跡中かから、
/// close-only 再試行を保留すべきか判定する。EndComposition が Err でも、呼出し中の
/// OnCompositionTerminated が slot を既に落としていれば終了済みとして扱う。
pub(crate) fn composition_end_stays_pending(end_ok: bool, still_tracked: bool) -> bool {
    !end_ok && still_tracked
}

/// `EndComposition`/`RequestEditSession` 後の close-only 状態。本文はすでに
/// `SetText` 済みなので、ここで `Retryable` 以外へ遷移したら同じ composition を
/// もう一度編集してはいけない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompositionEndStatus {
    Idle,
    Closed,
    Retryable,
    Terminal,
}

/// close-only の失敗 HRESULT を有限状態へ畳む。
pub(crate) fn classify_composition_end_error(
    end_code: HRESULT,
    range_code: Option<HRESULT>,
) -> CompositionEndStatus {
    if end_code == E_UNEXPECTED && range_code == Some(E_UNEXPECTED) {
        return CompositionEndStatus::Closed;
    }
    if matches!(
        end_code,
        TF_E_DISCONNECTED
            | TF_E_EMPTYCONTEXT
            | TF_E_INVALIDVIEW
            | TF_E_NOOBJECT
            | TF_E_NOSERVICE
            | TF_E_READONLY
    ) {
        return CompositionEndStatus::Terminal;
    }
    CompositionEndStatus::Retryable
}

/// composition を確定文字列 `text` で置換して EndComposition するセッション。
#[implement(ITfEditSession)]
pub struct CommitText {
    pub(crate) on_text_applied: RefCell<Option<Box<dyn FnOnce()>>>,
    #[cfg(feature = "tsf-test-hooks")]
    pub(crate) fail_attributes: Rc<Cell<bool>>,
    pub(crate) caret: Rc<RefCell<Option<ITfRange>>>,
    pub(crate) apply: Rc<crate::preedit_apply::PreeditApply>,
    pub(crate) request: Rc<crate::preedit_apply::PreeditRequest>,
    pub(crate) executed: Cell<bool>,
    pub(crate) deadline: Option<Instant>,
    pub context: ITfContext,
    pub text: HSTRING,
    pub composition: Rc<RefCell<Option<ITfComposition>>>,
    /// SetText は成功したが EndComposition だけが失敗した状態。true の間、後続操作は
    /// 確定文字列を再度 SetText せず `EndCompositionOnly` で close だけを再試行する。
    pub end_pending: Rc<Cell<bool>>,
    /// pending composition を所有する context。次打鍵が別 context へ移っても、元 context の
    /// edit cookie で EndComposition を再試行するため専用に保持する。
    pub end_context: Rc<RefCell<Option<ITfContext>>>,
    /// close-only の戻り値を TextService 側へ伝える。SetText 後の本文を再編集しない
    /// ため、terminal は即 quarantine、transient だけ有限回再試行する。
    pub(crate) end_status: Rc<Cell<CompositionEndStatus>>,
    /// 初回 EndComposition 失敗も retry budget に含める。
    pub(crate) end_retry_count: Rc<Cell<u8>>,
    // C-1: DLL_REF で生存数を数える。
    pub(crate) _guard: ComObjectGuard,
}

impl ITfEditSession_Impl for CommitText_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        // A host can retain or reenter this COM object. One request may write
        // its body at most once, even if the host invokes it again.
        if self.executed.replace(true) {
            return Ok(());
        }
        let composition = self.composition.borrow().clone();
        if !self
            .request
            .state
            .begin(self.before_deadline() && self.apply.is_current(&self.request, &composition))
        {
            return Err(E_FAIL.into());
        }
        let result = self.commit_body(ec);
        self.request
            .state
            .complete(result.as_ref().err().map_or(0, |e| e.code().0));
        result
    }
}

impl CommitText_Impl {
    fn record_text_applied(&self) {
        self.request.state.wrote_text();
        let notify = self.on_text_applied.borrow_mut().take();
        if let Some(notify) = notify { notify(); }
    }
    fn repair_committed_caret(&self, ec: u32, range: &ITfRange) -> Result<()> {
        let range_to_retain = range.clone();
        let context_to_retain = self.context.clone();
        self.validate_owner()?;
        let retired_caret = self.caret.borrow_mut().replace(range_to_retain);
        let retired_context = self.end_context.borrow_mut().replace(context_to_retain);
        self.end_pending.set(true);
        self.end_status.set(CompositionEndStatus::Retryable);
        self.end_retry_count.set(1);
        drop(retired_caret);
        drop(retired_context);
        // The deadline limits starting the body write, not acknowledging an
        // already successful write or retaining its repair barrier.
        repair_presentation(
            ec,
            &self.context,
            range,
            false,
            #[cfg(feature = "tsf-test-hooks")]
            &self.fail_attributes,
            || self.validate_owner(),
        )?;
        let retired = self.caret.borrow_mut().take();
        drop(retired);
        Ok(())
    }
    fn before_deadline(&self) -> bool {
        self.deadline
            .is_none_or(|deadline| Instant::now() < deadline)
    }
    fn validate_before_write(&self) -> Result<()> {
        self.validate_owner()?;
        if !self.before_deadline() {
            self.request.state.begin(false);
            return Err(E_FAIL.into());
        }
        Ok(())
    }
    fn validate_owner(&self) -> Result<()> {
        let composition = self.composition.borrow().clone();
        if self
            .request
            .state
            .begin(self.apply.is_current(&self.request, &composition))
        {
            Ok(())
        } else {
            Err(E_FAIL.into())
        }
    }

    fn commit_body(&self, ec: u32) -> Result<()> {
        unsafe {
            let comp = self.composition.borrow().clone();
            match comp {
                Some(comp) => {
                    // composition の range を確定文字列で置換する。
                    let crange = comp.GetRange()?;
                    self.validate_before_write()?;
                    crange.SetText(ec, 0, &self.text)?;
                    self.record_text_applied();
                    // The body is already committed. Continue closing even if
                    // caret repair fails, but report that repair is incomplete.
                    // The caller acknowledges the recorded body, not phrSession.
                    // 確定後、キャレットを確定文字列の末尾へ移す。range を末尾へ畳んで
                    // 選択に設定する。これをしないと多くの TSF アプリは合成開始位置
                    // （＝打ち始めた先頭）にキャレットを残し、次の入力が文書先頭へ挿入
                    // されてしまう（Microsoft TSF SampleIME と同じ確定手順）。
                    self.repair_committed_caret(ec, &crange)?;
                    // ここからは文書変更済み。EndComposition の同期 callback が現在 slot を
                    // 清算できるよう、呼出し前に pending を立てる。
                    self.end_pending.set(true);
                    *self.end_context.borrow_mut() = Some(self.context.clone());
                    self.end_status.set(CompositionEndStatus::Idle);
                    let end_result = comp.EndComposition(ec);
                    let end_ok = end_result.is_ok();
                    let still_tracked = self.composition.borrow().is_some();
                    if !end_ok && !still_tracked {
                        // 同期 callback が先に slot を落とした場合は、HRESULT に関係なく
                        // 終了済み。後続の late callback は identity check で無害化する。
                        self.end_status.set(CompositionEndStatus::Closed);
                        self.end_pending.set(false);
                        *self.end_context.borrow_mut() = None;
                        self.end_retry_count.set(0);
                    } else if composition_end_stays_pending(end_ok, still_tracked) {
                        // E_UNEXPECTED + GetRange=E_UNEXPECTED は host がすでに閉じた
                        // composition の冪等通知。二重 close をせず closed とみなす。
                        let range_code = if end_result
                            .as_ref()
                            .err()
                            .is_some_and(|err| err.code() == E_UNEXPECTED)
                        {
                            comp.GetRange().err().map(|err| err.code())
                        } else {
                            None
                        };
                        let status = classify_composition_end_error(
                            end_result.as_ref().expect_err("end_result is Err").code(),
                            range_code,
                        );
                        self.end_status.set(status);
                        if status == CompositionEndStatus::Terminal {
                            self.request.state.begin(false);
                        }
                        if matches!(
                            status,
                            CompositionEndStatus::Closed | CompositionEndStatus::Terminal
                        ) {
                            // terminal context は参照を捨てる。SetText はすでに成功して
                            // いるため、ここで EndComposition を再度呼んではならない。
                            *self.composition.borrow_mut() = None;
                            self.end_pending.set(false);
                            *self.end_context.borrow_mut() = None;
                            self.end_retry_count.set(0);
                        } else {
                            // 初回 EndComposition を budget 1 として記録する。
                            self.end_retry_count.set(1);
                            crate::text_service::tip_log(
                                "ev=commit_post_settext_failed step=endcomposition",
                            );
                        }
                    } else {
                        // End=S_OK は常に close 済みとして収束させる。
                        self.end_status.set(CompositionEndStatus::Closed);
                        *self.composition.borrow_mut() = None;
                        self.end_pending.set(false);
                        *self.end_context.borrow_mut() = None;
                        self.end_retry_count.set(0);
                    }
                }
                None => {
                    // composition が無い経路: 選択位置へ直接テキストを挿入する（従来の劣化 commit と
                    // shift_latin の直接確定が使う）。
                    // レビュー M-3: dwFlags は NOQUERY でなく 0（挿入して range も返す —
                    // Microsoft SampleIME の _InsertAtSelection と同型）を使う。NOQUERY だと
                    // 挿入後のキャレット位置がホストの ITextStoreACP 実装依存になり、
                    // 「。」連打の 2 打目が 1 打目の**前**に入るホストがありうる。返り値 range を
                    // 末尾へ畳んで明示 SetSelection し、composition あり枝と同じ規律で
                    // キャレット末尾追従（＝連打順序）を保証する。
                    let ins: ITfInsertAtSelection = self.context.cast()?;
                    self.validate_before_write()?;
                    let range = ins.InsertTextAtSelection(
                        ec,
                        INSERT_TEXT_AT_SELECTION_FLAGS(0),
                        &self.text,
                    )?;
                    self.record_text_applied();
                    // Record decoration failure separately from the body write,
                    // just as for replacement of an existing composition.
                    self.repair_committed_caret(ec, &range)?;
                    *self.composition.borrow_mut() = None;
                    self.end_pending.set(false);
                    *self.end_context.borrow_mut() = None;
                    self.end_status.set(CompositionEndStatus::Closed);
                    self.end_retry_count.set(0);
                }
            }
        }
        if self.end_status.get() != CompositionEndStatus::Closed {
            Err(E_FAIL.into())
        } else {
            Ok(())
        }
    }
}

fn repair_presentation(
    ec: u32,
    context: &ITfContext,
    range: &ITfRange,
    cancel_selection: bool,
    #[cfg(feature = "tsf-test-hooks")] fail_attributes: &Cell<bool>,
    validate: impl Fn() -> Result<()>,
) -> Result<()> {
    unsafe {
        let range = range.Clone()?;
        validate()?;
        let clear = (|| -> Result<()> {
            #[cfg(feature = "tsf-test-hooks")]
            if fail_attributes.replace(false) {
                return Err(E_FAIL.into());
            }
            let property = context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
            validate()?;
            property.Clear(ec, &range)
        })();
        if !cancel_selection {
            clear?;
        }
        validate()?;
        // Explicit cancellation abandons caret movement, while still making
        // one best-effort attempt to remove display attributes.
        if cancel_selection {
            return Ok(());
        }
        range.Collapse(ec, TF_ANCHOR_END)?;
        validate()?;
        let mut selection = TF_SELECTION {
            range: ManuallyDrop::new(Some(range)),
            style: TF_SELECTIONSTYLE {
                ase: TF_AE_NONE,
                fInterimChar: BOOL(0),
            },
        };
        let result = context.SetSelection(ec, core::slice::from_ref(&selection));
        ManuallyDrop::drop(&mut selection.range);
        result?;
        validate()
    }
}

/// CommitText の SetText 成功後に残った composition を、本文へ触れずに閉じ直すセッション。
/// EndComposition が再び失敗した場合は slot と pending marker を保持し、次の安全な同期
/// edit session または Deactivate で再試行できるようにする。
#[implement(ITfEditSession)]
pub struct EndCompositionOnly {
    pub(crate) report: Option<Rc<crate::apply_state::ApplyState>>,
    pub(crate) cancel_selection: bool,
    #[cfg(feature = "tsf-test-hooks")]
    pub(crate) fail_attributes: Rc<Cell<bool>>,
    pub(crate) expected_context: ITfContext,
    pub(crate) expected_composition: Option<ITfComposition>,
    pub(crate) epoch: Rc<Cell<u64>>,
    pub(crate) expected_epoch: u64,
    pub(crate) caret: Rc<RefCell<Option<ITfRange>>>,
    pub(crate) active: Rc<Cell<bool>>,
    pub(crate) executed: Cell<bool>,
    pub composition: Rc<RefCell<Option<ITfComposition>>>,
    pub end_pending: Rc<Cell<bool>>,
    pub end_context: Rc<RefCell<Option<ITfContext>>>,
    pub(crate) end_status: Rc<Cell<CompositionEndStatus>>,
    pub(crate) end_retry_count: Rc<Cell<u8>>,
    pub(crate) _guard: ComObjectGuard,
}

impl ITfEditSession_Impl for EndCompositionOnly_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        if !self.active.get()
            || self.executed.replace(true)
            || self.epoch.get() != self.expected_epoch
        {
            return Err(E_FAIL.into());
        }
        let result = self.repair_and_close(ec);
        if self.epoch.get() == self.expected_epoch {
            if let Some(report) = &self.report {
                let closed = self.end_status.get() == CompositionEndStatus::Closed;
                if self.cancel_selection || self.end_status.get() == CompositionEndStatus::Terminal
                {
                    report.begin(false);
                }
                report.complete(
                    result
                        .as_ref()
                        .err()
                        .map_or(if closed { 0 } else { E_FAIL.0 }, |error| error.code().0),
                );
            }
        }
        result
    }
}

impl EndCompositionOnly_Impl {
    fn repair_and_close(&self, ec: u32) -> Result<()> {
        self.validate_owner(&self.expected_context, &self.expected_composition)?;
        let caret = self.caret.borrow().clone();
        if let Some(caret) = caret {
            let result = repair_presentation(
                ec,
                &self.expected_context,
                &caret,
                self.cancel_selection,
                #[cfg(feature = "tsf-test-hooks")]
                &self.fail_attributes,
                || self.validate_owner(&self.expected_context, &self.expected_composition),
            );
            if let Err(error) = result {
                if self.epoch.get() == self.expected_epoch {
                    self.end_status.set(CompositionEndStatus::Retryable);
                }
                return Err(error);
            }
            let retired = self.caret.borrow_mut().take();
            drop(retired);
        }
        self.validate_owner(&self.expected_context, &self.expected_composition)?;
        unsafe {
            let Some(comp) = self.composition.borrow().clone() else {
                self.end_status.set(CompositionEndStatus::Closed);
                self.end_pending.set(false);
                *self.end_context.borrow_mut() = None;
                self.end_retry_count.set(0);
                return Ok(());
            };
            self.end_status.set(CompositionEndStatus::Idle);
            let result = comp.EndComposition(ec);
            if !self.active.get() || self.epoch.get() != self.expected_epoch {
                return Err(E_FAIL.into());
            }
            let current = self.composition.borrow().clone();
            if current
                .as_ref()
                .is_some_and(|current| !same_interface(current, &comp))
            {
                return Err(E_FAIL.into());
            }
            let still_tracked = self.composition.borrow().is_some();
            if result.is_ok() || !still_tracked {
                self.end_status.set(CompositionEndStatus::Closed);
                *self.composition.borrow_mut() = None;
                self.end_pending.set(false);
                *self.end_context.borrow_mut() = None;
                self.end_retry_count.set(0);
                return Ok(());
            }

            let range_code = if result
                .as_ref()
                .err()
                .is_some_and(|err| err.code() == E_UNEXPECTED)
            {
                comp.GetRange().err().map(|err| err.code())
            } else {
                None
            };
            let status = classify_composition_end_error(
                result.as_ref().expect_err("result is Err").code(),
                range_code,
            );
            self.end_status.set(status);
            if matches!(
                status,
                CompositionEndStatus::Closed | CompositionEndStatus::Terminal
            ) {
                *self.composition.borrow_mut() = None;
                self.end_pending.set(false);
                *self.end_context.borrow_mut() = None;
                self.end_retry_count.set(0);
                return Ok(());
            }
            result
        }
    }
}

impl EndCompositionOnly_Impl {
    fn validate_owner(
        &self,
        context: &ITfContext,
        expected: &Option<ITfComposition>,
    ) -> Result<()> {
        let current_context = self.end_context.borrow().clone();
        let current_composition = self.composition.borrow().clone();
        let context_matches = current_context
            .as_ref()
            .is_some_and(|current| same_interface(current, context));
        let composition_matches = match (current_composition.as_ref(), expected.as_ref()) {
            (Some(a), Some(b)) => same_interface(a, b),
            (None, None) => true,
            _ => false,
        };
        if context_matches
            && composition_matches
            && self.active.get()
            && self.end_pending.get()
            && self.epoch.get() == self.expected_epoch
        {
            Ok(())
        } else {
            Err(E_FAIL.into())
        }
    }
}

fn same_interface<T: Interface>(a: &T, b: &T) -> bool {
    match (a.cast::<IUnknown>(), b.cast::<IUnknown>()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
