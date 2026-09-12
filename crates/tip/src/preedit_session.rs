//! Shared production preedit session; the testbench executes this same COM object.
use crate::apply_state::exact_shift;
use crate::globals::ComObjectGuard;
use crate::preedit_apply::{PreeditApply, PreeditRequest};
use core::mem::ManuallyDrop;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use windows::core::{implement, Interface, Result, BOOL};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::TextServices::{
    ITfComposition, ITfCompositionSink, ITfContextComposition, ITfEditSession, ITfEditSession_Impl,
    ITfInsertAtSelection, ITfProperty, ITfRange, GUID_PROP_ATTRIBUTE, TF_AE_NONE, TF_ANCHOR_END,
    TF_ANCHOR_START, TF_IAS_QUERYONLY, TF_SELECTION, TF_SELECTIONSTYLE,
};

#[cfg(feature = "tsf-test-hooks")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreeditFault {
    Attributes,
    ShiftStart,
    ShiftEnd,
}

#[cfg(feature = "tsf-test-hooks")]
#[derive(Default)]
pub(crate) struct PreeditFaults(pub Cell<Option<PreeditFault>>);

#[cfg(feature = "tsf-test-hooks")]
impl PreeditFaults {
    fn take(&self, point: PreeditFault) -> bool {
        if self.0.get() == Some(point) {
            self.0.set(None);
            true
        } else {
            false
        }
    }
}

/// composition を開始/更新し preedit を `text` にして下線属性を付与するセッション。
#[implement(ITfEditSession)]
pub struct StartOrUpdatePreedit {
    #[cfg(feature = "tsf-test-hooks")]
    pub(crate) faults: Rc<PreeditFaults>,
    pub(crate) apply: Rc<PreeditApply>,
    pub(crate) request: Rc<PreeditRequest>,
    pub(crate) on_complete: fn(&PreeditRequest),
    pub capture_left_context: unsafe fn(u32, &ITfRange) -> Option<String>,
    pub sink: ITfCompositionSink,
    pub da_variant: VARIANT,
    /// 文節ナビゲーション: 選択文節の (UTF-16 開始, UTF-16 長)。Some なら該当区間だけ
    /// `da_target_variant`（太下線）で上書きし、選択文節を視覚化する（MS-IME の変換対象文節）。
    /// 選択文節用の表示属性 atom を内包した VARIANT（`target` が Some のときだけ使う）。
    pub da_target_variant: VARIANT,
    pub da_converted_variant: VARIANT,
    pub composition: Rc<RefCell<Option<ITfComposition>>>,
    /// `StartComposition` が実際に成功したことを caller へ伝える one-shot 出力。
    /// RequestEditSession 拒否や StartComposition 前の失敗では立たない。
    pub started: Rc<Cell<bool>>,
    /// U9: composition 新規作成時に読んだ左文脈の出力先（TextService.left_context と共有）。
    /// 取得の成否にかかわらず**必ず上書き**する（失敗=None。前文書の文脈残留を許さない — spec §2.1）。
    pub left_context_out: Rc<RefCell<Option<String>>>,
    // C-1: DLL_REF で生存数を数える（ホストが session を保持中の DLL アンロードによる UAF を防ぐ）。
    pub(crate) _guard: ComObjectGuard,
}

impl ITfEditSession_Impl for StartOrUpdatePreedit_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        let composition = self.composition.borrow().clone();
        if !self
            .request
            .state
            .begin(self.apply.is_current(&self.request, &composition))
        {
            self.request.state.complete(E_FAIL.0);
            (self.on_complete)(&self.request);
            return Err(E_FAIL.into());
        }
        let result = self.apply_preedit(ec);
        self.request
            .state
            .complete(result.as_ref().err().map_or(0, |error| error.code().0));
        (self.on_complete)(&self.request);
        result
    }
}

impl StartOrUpdatePreedit_Impl {
    fn validate_identity(&self) -> Result<()> {
        let composition = self.composition.borrow().clone();
        if self.apply.is_current(&self.request, &composition) {
            Ok(())
        } else {
            self.request.state.begin(false);
            Err(E_FAIL.into())
        }
    }

    fn apply_preedit(&self, ec: u32) -> Result<()> {
        unsafe {
            if let Some(caret) = self.request.caret {
                let offset = caret.0 as usize;
                let units: &[u16] = &self.request.text;
                if offset > units.len() || offset > i32::MAX as usize
                    || (offset > 0 && offset < units.len() && (0xD800..=0xDBFF).contains(&units[offset - 1])
                        && (0xDC00..=0xDFFF).contains(&units[offset])) { return Err(E_FAIL.into()); }
            }
            for (start, len) in self.request.converted.iter().copied().chain(self.request.target) {
                let end = start
                    .checked_add(len)
                    .filter(|end| *end <= self.request.text.len() && *end <= i32::MAX as usize)
                    .ok_or_else(|| windows::core::Error::from(E_FAIL))?;
                let units: &[u16] = &self.request.text;
                for offset in [start, end] {
                    if offset > 0
                        && offset < units.len()
                        && (0xD800..=0xDBFF).contains(&units[offset - 1])
                        && (0xDC00..=0xDFFF).contains(&units[offset])
                    {
                        return Err(E_FAIL.into());
                    }
                }
            }
            // composition がまだ無ければ、現在の選択位置に空 range を作って開始する。
            if self.composition.borrow().is_none() {
                let cc: ITfContextComposition = self.request.context.cast()?;
                let ins: ITfInsertAtSelection = self.request.context.cast()?;
                // TF_IAS_QUERYONLY: テキストは挿入せず、選択位置の range だけ得る。
                let range = ins.InsertTextAtSelection(ec, TF_IAS_QUERYONLY, &[])?;
                // U9: StartComposition の前に挿入点左の周辺テキストを読む（preedit 混入前）。
                // 成否によらず必ず上書き（読めなければ None）。内容はログに出さない（len のみ）。
                let ctx_text = (self.capture_left_context)(ec, &range);
                let len = ctx_text.as_ref().map_or(0, |s| s.chars().count());
                self.validate_identity()?;
                *self.left_context_out.borrow_mut() = ctx_text;
                crate::text_service::tip_log(&format!("ev=left_context len={len}"));
                self.validate_identity()?;
                let comp = cc.StartComposition(ec, &range, &self.sink)?;
                if self.validate_identity().is_err() {
                    let _ = comp.EndComposition(ec);
                    return Err(E_FAIL.into());
                }
                *self.request.composition.borrow_mut() = Some(comp.clone());
                // The API-internal reentrancy of StartComposition itself returns before success
                // is observable here and cannot be signalled.  Once it returns, set the shared
                // flag immediately; the following slot assignment has no COM callout.
                self.started.set(true);
                *self.composition.borrow_mut() = Some(comp);
            }

            // composition の range を取り出し、preedit を text で置換する。
            let comp = self
                .composition
                .borrow()
                .clone()
                .expect("composition was just set above");
            let crange = comp.GetRange()?;
            self.validate_identity()?;
            if !self.request.state.text_applied() {
                // Read the actual range so host-side edits cannot make a cached
                // equal body suppress a required write. One extra unit detects truncation.
                let mut current = vec![0u16; self.request.text.len() + 1];
                let mut fetched = 0;
                let unchanged = crange.GetText(ec, 0, &mut current, &mut fetched).is_ok()
                    && fetched as usize == self.request.text.len()
                    && current[..fetched as usize] == *self.request.text;
                self.validate_identity()?;
                // Chromium 系ホストは本文不変の属性のみ更新では下線スパンを再描画しない
                // （文節移動で選択文節の太下線が画面に残る）。装飾が前回適用から変わる
                // 場合は同一本文でも SetText を流して文書変化を通知する。同一装飾の
                // 再適用（修復再試行）は書き換えない — 書き込み回数の契約を保つため。
                let decorated = self.request.target.is_some() || !self.request.converted.is_empty();
                if !unchanged || (decorated && !self.request.decoration_repeat) {
                    crange.SetText(ec, 0, &self.request.text)?;
                }
                self.request.state.wrote_text();
            }
            self.validate_identity()?;

            // 下線の表示属性を range（全体）に適用する（atom を内包した VARIANT を使う）。
            // 末尾へ畳む前に適用すること（畳むと range が空になり下線が乗らない）。
            let prop: ITfProperty = self.request.context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
            self.validate_identity()?;
            #[cfg(feature = "tsf-test-hooks")]
            if self.faults.take(PreeditFault::Attributes) {
                return Err(E_FAIL.into());
            }
            prop.SetValue(ec, &crange, &self.da_variant)?;

            for &(start, len) in &self.request.converted {
                let sub = crange.Clone()?;
                sub.Collapse(ec, TF_ANCHOR_START)?;
                let mut moved = 0;
                sub.ShiftEnd(ec, (start + len) as i32, &mut moved, core::ptr::null())?;
                if !exact_shift((start + len) as i32, moved) { return Err(E_FAIL.into()); }
                sub.ShiftStart(ec, start as i32, &mut moved, core::ptr::null())?;
                if !exact_shift(start as i32, moved) { return Err(E_FAIL.into()); }
                self.validate_identity()?;
                prop.SetValue(ec, &sub, &self.da_converted_variant)?;
            }

            // 文節ナビゲーション: 選択文節の区間だけ太下線属性で上書きする。sub-range は
            // 全体 range の clone を先頭へ畳み、ShiftEnd→ShiftStart の順で切り出す
            // （逆順だと start>end の一瞬が生じ実装依存の失敗を踏む）。属性の上書きは
            // Failure leaves the text applied and schedules decoration-only repair.
            if let Some((start, len)) = self.request.target {
                if len > 0 {
                    let apply_target = || -> Result<()> {
                        let sub = crange.Clone()?;
                        sub.Collapse(ec, TF_ANCHOR_START)?;
                        let mut moved = 0i32;
                        sub.ShiftEnd(ec, (start + len) as i32, &mut moved, core::ptr::null())?;
                        #[cfg(feature = "tsf-test-hooks")]
                        if self.faults.take(PreeditFault::ShiftEnd) {
                            moved -= 1;
                        }
                        if !exact_shift((start + len) as i32, moved) {
                            return Err(E_FAIL.into());
                        }
                        sub.ShiftStart(ec, start as i32, &mut moved, core::ptr::null())?;
                        #[cfg(feature = "tsf-test-hooks")]
                        if self.faults.take(PreeditFault::ShiftStart) {
                            moved -= 1;
                        }
                        if !exact_shift(start as i32, moved) {
                            return Err(E_FAIL.into());
                        }
                        self.validate_identity()?;
                        prop.SetValue(ec, &sub, &self.da_target_variant)
                    };
                    apply_target()?;
                }
            }

            // preedit 更新後、キャレットを合成文字列の末尾へ移す。これをしないと多くの TSF アプリは
            // 合成開始位置（＝打ち始めた先頭）に選択を残し、ライブ変換中ずっとカーソルが文頭に
            // 居座ってしまう（ふつうの IME は変換済み文字列の末尾にキャレットが付く）。
            // 確定時の `CommitText` と同じ規律: range を末尾へ畳んで SetSelection し、TF_SELECTION.range
            // の ManuallyDrop 自参照は必ず解放する（SetSelection が必要なら内部で AddRef する）。
            if let Some(caret) = self.request.caret {
                crange.Collapse(ec, TF_ANCHOR_START)?;
                let mut moved = 0;
                crange.ShiftEnd(ec, caret.0 as i32, &mut moved, core::ptr::null())?;
                if !exact_shift(caret.0 as i32, moved) { return Err(E_FAIL.into()); }
                self.validate_identity()?;
            }
            crange.Collapse(ec, TF_ANCHOR_END)?;
            let mut sel = TF_SELECTION {
                range: ManuallyDrop::new(Some(crange)),
                style: TF_SELECTIONSTYLE {
                    ase: TF_AE_NONE,
                    fInterimChar: BOOL(0),
                },
            };
            let set = self.validate_identity().and_then(|()| {
                self.request
                    .context
                    .SetSelection(ec, core::slice::from_ref(&sel))
            });
            ManuallyDrop::drop(&mut sel.range);
            set?;
            self.validate_identity()?;
        }
        Ok(())
    }
}
