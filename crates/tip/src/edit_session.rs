//! TSF 編集セッション（ITfEditSession）。
//!
//! TSF では composition / range の書き換えは必ず編集セッションの内側で行う。
//! ここでは 5 種類のセッションを定義する:
//!   - `StartOrUpdatePreedit` : composition を（無ければ）開始し、preedit 文字列を更新して下線属性を付与する。
//!   - `CommitText`           : composition を確定文字列で置換し EndComposition する。
//!   - `EndCompositionOnly`   : 確定文字列の書込み後に終了だけ失敗した composition を再度閉じる。
//!   - `CancelComposition`    : composition を確定せず終了する。
//!   - `ReconvertStart`       : 直前ラテン列（または選択範囲）を読み戻し、その**非空** range を composition 化する。
//!   - `RestoreText`          : composition の range を元ラテンに戻してから閉じる（取消復元）。
//!
//! いずれも `ITfContext::RequestEditSession` から `TF_ES_SYNC | TF_ES_READWRITE` で同期実行される。
//! `composition` は `TextService` と共有される `Rc<RefCell<Option<ITfComposition>>>` で、
//! セッションをまたいで現在の composition を保持する。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use core::mem::ManuallyDrop;

use windows::core::{implement, IUnknown, Interface, Result, BOOL, HRESULT, HSTRING};
use windows::Win32::Foundation::{E_UNEXPECTED, RECT};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VARIANT, VT_UNKNOWN};
use windows::Win32::UI::TextServices::{
    ITfComposition, ITfCompositionSink, ITfContext, ITfContextComposition, ITfEditSession,
    ITfEditSession_Impl, ITfInputScope, ITfInsertAtSelection, ITfProperty, ITfRange, InputScope,
    GUID_PROP_ATTRIBUTE, GUID_PROP_INPUTSCOPE, INSERT_TEXT_AT_SELECTION_FLAGS, TF_AE_NONE,
    TF_ANCHOR_END, TF_ANCHOR_START, TF_DEFAULT_SELECTION, TF_E_DISCONNECTED, TF_E_EMPTYCONTEXT,
    TF_E_INVALIDVIEW, TF_E_LOCKED, TF_E_NOLOCK, TF_E_NOOBJECT, TF_E_NOSERVICE, TF_E_READONLY,
    TF_E_SYNCHRONOUS, TF_IAS_QUERYONLY, TF_SELECTION, TF_SELECTIONSTYLE,
};

use crate::globals::ComObjectGuard;
use crate::input_state::{classify_reconvert_selection, ReconvertKind};

/// `ReconvertStart` の出力。掴んだ対象文字列とその種別を呼び出し側（start_reconvert）へ返す。
#[derive(Default, Clone)]
pub struct ReconvertCapture {
    pub text: String,
    pub kind: ReconvertKind,
}

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
    /// まだ状態を書いていない（RequestEditSession 自体の拒否など）。
    Idle,
    /// EndComposition 成功、または同期 callback が slot を清算済み。
    Closed,
    /// 一時的な lock/synchronous 拒否。呼び出し側が有限回だけ再試行する。
    Retryable,
    /// context/range が終端している。参照を捨てて liveness を優先する。
    Terminal,
}

/// close-only の失敗 HRESULT を有限状態へ畳む。未知の HRESULT は安全側に
/// `Retryable` とし、呼び出し側の上限到達で quarantine される（無限障壁にはならない）。
/// `range_code` は `E_UNEXPECTED` の特別扱いにだけ使う。
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
    if matches!(end_code, TF_E_LOCKED | TF_E_NOLOCK | TF_E_SYNCHRONOUS) {
        return CompositionEndStatus::Retryable;
    }
    CompositionEndStatus::Retryable
}

/// 挿入点 `range` の直前 64 UTF-16 単位を読み、サニタイズ済み左文脈を返す（U9）。
/// ReconvertStart の後方スキャン（ShiftStart(-64)→GetText）と同型。読み取りは best-effort:
/// clone/Shift/GetText いずれの失敗も None（呼び出し側は「必ず上書き」規約でスロットへ書く）。
unsafe fn read_left_context(ec: u32, range: &ITfRange) -> Option<String> {
    let scan = range.Clone().ok()?;
    // QUERYONLY の range は非空選択だと選択範囲そのものを指し得る。左文脈は「挿入開始位置の
    // 左側」なので、まず先頭へ畳んでから後方へ広げる（畳まないと選択テキスト自身を読んでしまう）。
    scan.Collapse(ec, TF_ANCHOR_START).ok()?;
    let mut moved = 0i32;
    scan.ShiftStart(ec, -64, &mut moved, core::ptr::null())
        .ok()?;
    let mut buf = [0u16; 64];
    let mut got = 0u32;
    scan.GetText(ec, 0, &mut buf, &mut got).ok()?;
    crate::input_state::sanitize_left_context(&String::from_utf16_lossy(&buf[..got as usize]))
}

/// composition を開始/更新し preedit を `text` にして下線属性を付与するセッション。
#[implement(ITfEditSession)]
pub struct StartOrUpdatePreedit {
    pub context: ITfContext,
    pub text: HSTRING,
    pub sink: ITfCompositionSink,
    pub da_variant: VARIANT,
    /// 文節ナビゲーション: 選択文節の (UTF-16 開始, UTF-16 長)。Some なら該当区間だけ
    /// `da_target_variant`（太下線）で上書きし、選択文節を視覚化する（MS-IME の変換対象文節）。
    pub target: Option<(usize, usize)>,
    /// 選択文節用の表示属性 atom を内包した VARIANT（`target` が Some のときだけ使う）。
    pub da_target_variant: VARIANT,
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
        unsafe {
            // composition がまだ無ければ、現在の選択位置に空 range を作って開始する。
            if self.composition.borrow().is_none() {
                let cc: ITfContextComposition = self.context.cast()?;
                let ins: ITfInsertAtSelection = self.context.cast()?;
                // TF_IAS_QUERYONLY: テキストは挿入せず、選択位置の range だけ得る。
                let range = ins.InsertTextAtSelection(ec, TF_IAS_QUERYONLY, &[])?;
                // U9: StartComposition の前に挿入点左の周辺テキストを読む（preedit 混入前）。
                // 成否によらず必ず上書き（読めなければ None）。内容はログに出さない（len のみ）。
                let ctx_text = read_left_context(ec, &range);
                let len = ctx_text.as_ref().map_or(0, |s| s.chars().count());
                *self.left_context_out.borrow_mut() = ctx_text;
                crate::text_service::tip_log(&format!("ev=left_context len={len}"));
                let comp = cc.StartComposition(ec, &range, &self.sink)?;
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
            crange.SetText(ec, 0, &self.text)?;

            // 下線の表示属性を range（全体）に適用する（atom を内包した VARIANT を使う）。
            // 末尾へ畳む前に適用すること（畳むと range が空になり下線が乗らない）。
            let prop: ITfProperty = self.context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
            prop.SetValue(ec, &crange, &self.da_variant)?;

            // 文節ナビゲーション: 選択文節の区間だけ太下線属性で上書きする。sub-range は
            // 全体 range の clone を先頭へ畳み、ShiftEnd→ShiftStart の順で切り出す
            // （逆順だと start>end の一瞬が生じ実装依存の失敗を踏む）。属性の上書きは
            // best-effort — 失敗しても preedit 本文は既に立っているので合成は壊さない。
            if let Some((start, len)) = self.target {
                if len > 0 {
                    let apply_target = || -> Result<()> {
                        let sub = crange.Clone()?;
                        sub.Collapse(ec, TF_ANCHOR_START)?;
                        let mut moved = 0i32;
                        sub.ShiftEnd(ec, (start + len) as i32, &mut moved, core::ptr::null())?;
                        sub.ShiftStart(ec, start as i32, &mut moved, core::ptr::null())?;
                        prop.SetValue(ec, &sub, &self.da_target_variant)
                    };
                    let _ = apply_target();
                }
            }

            // preedit 更新後、キャレットを合成文字列の末尾へ移す。これをしないと多くの TSF アプリは
            // 合成開始位置（＝打ち始めた先頭）に選択を残し、ライブ変換中ずっとカーソルが文頭に
            // 居座ってしまう（ふつうの IME は変換済み文字列の末尾にキャレットが付く）。
            // 確定時の `CommitText` と同じ規律: range を末尾へ畳んで SetSelection し、TF_SELECTION.range
            // の ManuallyDrop 自参照は必ず解放する（SetSelection が必要なら内部で AddRef する）。
            crange.Collapse(ec, TF_ANCHOR_END)?;
            let mut sel = TF_SELECTION {
                range: ManuallyDrop::new(Some(crange)),
                style: TF_SELECTIONSTYLE {
                    ase: TF_AE_NONE,
                    fInterimChar: BOOL(0),
                },
            };
            let set = self.context.SetSelection(ec, core::slice::from_ref(&sel));
            ManuallyDrop::drop(&mut sel.range);
            set?;
        }
        Ok(())
    }
}

/// composition を確定文字列 `text` で置換して EndComposition するセッション。
#[implement(ITfEditSession)]
pub struct CommitText {
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
        unsafe {
            let comp = self.composition.borrow().clone();
            match comp {
                Some(comp) => {
                    // composition の range を確定文字列で置換する。
                    let crange = comp.GetRange()?;
                    crange.SetText(ec, 0, &self.text)?;
                    // 巡4 T3: ここまで来たら文書はもう変更済み（SetText 成功）— 以降の
                    // Collapse/SetSelection/EndComposition は best-effort に落とす。同期セッション
                    // では戻り値が phrSession 経由で do_commit の bool に載るため、後段の失敗で
                    // Err を返すと「文書変更済みなのに未挿入扱い」になり呼び出し側の再試行で
                    // 二重挿入する。SetText 成功を以て確定とみなす契約（SampleIME と同型）。
                    // 確定後、キャレットを確定文字列の末尾へ移す。range を末尾へ畳んで
                    // 選択に設定する。これをしないと多くの TSF アプリは合成開始位置
                    // （＝打ち始めた先頭）にキャレットを残し、次の入力が文書先頭へ挿入
                    // されてしまう（Microsoft TSF SampleIME と同じ確定手順）。
                    if crange.Collapse(ec, TF_ANCHOR_END).is_err() {
                        crate::text_service::tip_log("ev=commit_post_settext_failed step=collapse");
                    }
                    let mut sel = TF_SELECTION {
                        range: ManuallyDrop::new(Some(crange)),
                        style: TF_SELECTIONSTYLE {
                            ase: TF_AE_NONE,
                            fInterimChar: BOOL(0),
                        },
                    };
                    let set = self.context.SetSelection(ec, core::slice::from_ref(&sel));
                    // TF_SELECTION.range は ManuallyDrop。SetSelection が必要なら内部で
                    // AddRef するので、ここで自分の参照を必ず解放する（失敗時もリークさせない）。
                    ManuallyDrop::drop(&mut sel.range);
                    if set.is_err() {
                        crate::text_service::tip_log(
                            "ev=commit_post_settext_failed step=setselection",
                        );
                    }
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
                    let range = ins.InsertTextAtSelection(
                        ec,
                        INSERT_TEXT_AT_SELECTION_FLAGS(0),
                        &self.text,
                    )?;
                    // 巡5 GLM I-3: 挿入済み（InsertTextAtSelection 成功）の後段は best-effort —
                    // Some(comp) 枝と同じ契約（成功点は SetText 対 ここは挿入 API、いずれも
                    // 「挿入成功を以て確定とみなす」）。`?` で Err を返すと文書挿入済みなのに
                    // do_commit=false となり、呼び出し側が未挿入と誤認して状態不整合・
                    // 再試行での二重挿入を起こす。
                    if range.Collapse(ec, TF_ANCHOR_END).is_err() {
                        crate::text_service::tip_log(
                            "ev=commit_post_settext_failed step=insert_collapse",
                        );
                    }
                    let mut sel = TF_SELECTION {
                        range: ManuallyDrop::new(Some(range)),
                        style: TF_SELECTIONSTYLE {
                            ase: TF_AE_NONE,
                            fInterimChar: BOOL(0),
                        },
                    };
                    // TF_SELECTION.range は ManuallyDrop。SetSelection が必要なら内部で AddRef
                    // するので、自分の参照は必ず解放する（composition あり枝と同じ規律）。
                    let set = self.context.SetSelection(ec, core::slice::from_ref(&sel));
                    ManuallyDrop::drop(&mut sel.range);
                    if set.is_err() {
                        crate::text_service::tip_log(
                            "ev=commit_post_settext_failed step=insert_setselection",
                        );
                    }
                    *self.composition.borrow_mut() = None;
                    self.end_pending.set(false);
                    *self.end_context.borrow_mut() = None;
                    self.end_status.set(CompositionEndStatus::Closed);
                    self.end_retry_count.set(0);
                }
            }
        }
        Ok(())
    }
}

/// CommitText の SetText 成功後に残った composition を、本文へ触れずに閉じ直すセッション。
/// EndComposition が再び失敗した場合は slot と pending marker を保持し、次の安全な同期
/// edit session または Deactivate で再試行できるようにする。
#[implement(ITfEditSession)]
pub struct EndCompositionOnly {
    pub composition: Rc<RefCell<Option<ITfComposition>>>,
    pub end_pending: Rc<Cell<bool>>,
    pub end_context: Rc<RefCell<Option<ITfContext>>>,
    pub(crate) end_status: Rc<Cell<CompositionEndStatus>>,
    pub(crate) end_retry_count: Rc<Cell<u8>>,
    pub(crate) _guard: ComObjectGuard,
}

impl ITfEditSession_Impl for EndCompositionOnly_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
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

/// composition を確定せずに終了する（取消）セッション。
#[implement(ITfEditSession)]
pub struct CancelComposition {
    pub composition: Rc<RefCell<Option<ITfComposition>>>,
    // C-1: DLL_REF で生存数を数える。
    pub(crate) _guard: ComObjectGuard,
}

impl ITfEditSession_Impl for CancelComposition_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        unsafe {
            let comp = self.composition.borrow().clone();
            if let Some(comp) = comp {
                // 取消なので range の preedit を空にしてから composition を閉じる
                // （これをしないと打ちかけのローマ字/読みが文書に残ってしまう）。
                // GetRange/SetText の失敗は Err で伝播する — preedit を除去できて
                // いないのに Ok を返さない契約。SetText 成功後の失敗でテキストを
                // 巻き戻すことはしない。
                let crange = comp.GetRange()?;
                crange.SetText(ec, 0, &[])?;
                comp.EndComposition(ec)?;
            }
            // 共有スロットは EndComposition が成功した経路でのみ落とす（上の `?`
            // が Err を返したらここへは届かない）。
            *self.composition.borrow_mut() = None;
        }
        Ok(())
    }
}

/// 再変換の起点。直前ラテン列（または選択範囲）を読み戻し、その**非空** range を
/// composition 化するセッション（D9）。
///
/// `StartOrUpdatePreedit` は選択位置に**空** range を作って合成を始めるが、ここでは
/// 既存テキスト（読み＝ラテン列またはかな Surface）の上に直接合成を張る点が新しい。
/// 読み取った文字列と種別を `out`（`ReconvertCapture`）に書き戻し、呼び出し側（Task 6）が
/// 種別に応じてエンジンへ g1 リプレイするか再変換をスキップするかを判断する。
///
/// 動作:
///   1) 既定選択を取得し、その range を所有クローンして TSF の参照を解放する。
///   2) 選択が非空ならそれをそのまま対象 range にする。
///      空（キャレットのみ）なら caret から後方へ最大 64 文字読み、末尾ラテン列の長さだけ
///      開始位置を戻して対象 range を作る（ラテン列が無ければ何もしない）。
///   3) 対象 range の文字列と種別（Latin / Surface / NonKana）を `out` に記録する。
///   4) 再変換可能な種別（Latin または Surface）の場合のみ非空の対象 range で
///      `StartComposition` する。漢字・混在など NonKana の場合は合成を行わず終了し、
///      呼び出し側が `ev=reconvert_skip` として処理する。
///
/// MVP 制約: 後方スキャン/読み取りは 64 UTF-16 単位上限。直前ラテン列が 64 文字を超える場合は
/// 末尾 64 文字だけが対象になる（実用上ありえない長さ。拡張は将来送り）。`out` の text は対象
/// range から読み直すので、クランプが起きても composition と読みは常に一致する。
#[implement(ITfEditSession)]
pub struct ReconvertStart {
    pub context: ITfContext,
    pub sink: ITfCompositionSink,
    pub composition: Rc<RefCell<Option<ITfComposition>>>,
    /// `StartComposition` の実成功だけを caller へ返す one-shot 出力。
    pub started: Rc<Cell<bool>>,
    pub out: Rc<RefCell<ReconvertCapture>>,
    /// U9: 再変換対象の**手前**の左文脈の出力先（TextService.left_context と共有）。
    /// キャレット経路は読み済み text_before の非ラテン prefix を書き、選択経路・早期離脱は
    /// None を書く（**必ず上書き** — 前 composition の文脈を Reconvert 要求へ漏らさない）。
    pub left_context_out: Rc<RefCell<Option<String>>>,
    // C-1: DLL_REF で生存数を数える。
    pub(crate) _guard: ComObjectGuard,
}

impl ITfEditSession_Impl for ReconvertStart_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        unsafe {
            // U9: 上書き規約 — どの経路（早期 return 含む）でも stale 文脈を残さない。
            // キャレット経路だけ後で実文脈に上書きし直す。
            *self.left_context_out.borrow_mut() = None;
            // 1) 既定選択を取得する。TF_SELECTION は ManuallyDrop<Option<ITfRange>> を内包し
            //    Default を導出しないので、range=None で明示構築する。
            let mut sel = [TF_SELECTION {
                range: ManuallyDrop::new(None),
                style: TF_SELECTIONSTYLE {
                    ase: TF_AE_NONE,
                    fInterimChar: BOOL(0),
                },
            }];
            let mut fetched = 0u32;
            self.context
                .GetSelection(ec, TF_DEFAULT_SELECTION, &mut sel, &mut fetched)?;
            if fetched == 0 {
                // 念のため: 取得 0 でも range は None のはずだが、ManuallyDrop 規律として drop する。
                ManuallyDrop::drop(&mut sel[0].range);
                return Ok(());
            }

            // GetSelection は range の所有権を呼び出し側へ渡す（AddRef 済み）。必要分を
            // 所有クローンして TSF 側の参照（ManuallyDrop）を drop で解放する。これを怠ると
            // リーク、二重に扱うと UAF になる（CommitText の SetSelection と同じ規律）。
            let range: Option<ITfRange> = (*sel[0].range).as_ref().cloned();
            ManuallyDrop::drop(&mut sel[0].range);
            let range = match range {
                Some(r) => r,
                None => return Ok(()),
            };

            // 2) 選択の空/非空を一度だけ判定して使い回す（再呼び出しで状態が変わらないように）。
            let is_empty = range.IsEmpty(ec)?.as_bool();

            // 2) 対象 range を決める。実選択はそのまま、キャレットのみなら後方ラテン run。
            let comp_range: ITfRange = if !is_empty {
                range.Clone()?
            } else {
                let scan = range.Clone()?;
                let mut moved = 0i32;
                scan.ShiftStart(ec, -64, &mut moved, core::ptr::null())?;
                let mut buf = [0u16; 64];
                let mut got = 0u32;
                scan.GetText(ec, 0, &mut buf, &mut got)?;
                let text_before = String::from_utf16_lossy(&buf[..got as usize]);
                let span = crate::input_state::latin_run_span(&text_before);
                if span == 0 {
                    return Ok(()); // 対象なし（out は default None のまま）
                }
                // U9: ラテン run の手前が左文脈（latin_run_span は ASCII run のバイト数＝文字数
                // なのでバイトスライスは char 境界安全）。読み済みバッファの再利用で追加読取なし。
                let ctx_text = crate::input_state::sanitize_left_context(
                    &text_before[..text_before.len() - span],
                );
                let ctx_len = ctx_text.as_ref().map_or(0, |s| s.chars().count());
                *self.left_context_out.borrow_mut() = ctx_text;
                // StartOrUpdatePreedit と同じ規律で長さのみログ（VM 受入 item7 の観測点。
                // 内容は出さない — spec §2.5 / 最終レビュー Minor-3）。
                crate::text_service::tip_log(&format!(
                    "ev=left_context len={ctx_len} src=reconvert"
                ));
                let r = range.Clone()?;
                let mut m = 0i32;
                r.ShiftStart(ec, -(span as i32), &mut m, core::ptr::null())?;
                r
            };

            // 3) 対象 range の文字列を読み、種別を決める。
            let mut cbuf = [0u16; 64];
            let mut cgot = 0u32;
            comp_range.GetText(ec, 0, &mut cbuf, &mut cgot)?;
            let text = String::from_utf16_lossy(&cbuf[..cgot as usize]);
            // キャレット後方ラテン run は常に Latin。実選択は内容で分類する。
            // ただし選択が読み取りバッファ(64 UTF-16 単位)を満たす場合は全体を読めていない
            // 可能性がある（GetText は cchMax まで読んで残量を教えない）。truncated な prefix で
            // 分類すると、漢字を含む長い選択を Surface と誤判定して comp_range(選択全体)に合成し、
            // 64 単位より後ろを候補で置換・削除してしまう（do-no-harm 違反・データロス）。
            // 全体を分類・捕捉できないので NonKana として何もしない（合成しない）。
            let kind = if is_empty {
                ReconvertKind::Latin
            } else if cgot as usize >= cbuf.len() {
                ReconvertKind::NonKana
            } else {
                classify_reconvert_selection(&text)
            };
            *self.out.borrow_mut() = ReconvertCapture { text, kind };

            // 4) 再変換可能な種別のときだけ非空 range で StartComposition する。
            //    NonKana/None は合成しない＝選択を一切触らない（do-no-harm）。
            if matches!(kind, ReconvertKind::Latin | ReconvertKind::Surface) {
                let cc: ITfContextComposition = self.context.cast()?;
                let comp = cc.StartComposition(ec, &comp_range, &self.sink)?;
                // StartComposition's own internal reentrancy is before its success return; after
                // that API boundary the signal precedes the local slot assignment with no COM call.
                self.started.set(true);
                *self.composition.borrow_mut() = Some(comp);
            }
        }
        Ok(())
    }
}

/// 確定取消（Ctrl+Backspace）: キャレット（空選択）から確定文字列の**既知長**（`expected` の
/// UTF-16 単位数）だけ ShiftStart で後方へ戻して GetText し、読み取り結果が `expected` に
/// **バイト一致したときだけ** その range を composition 化する新セッション（`ReconvertStart`
/// の骨格＝GetSelection→所有クローン→ShiftStart→GetText→StartComposition／left_context 捕捉／
/// ManuallyDrop 規律 と同型）。
///
/// `ReconvertStart` はキャレット経路が後方**ラテン run 限定**で、選択分類も漢字/混在を
/// NonKana として合成拒否するため、漢字かな交じり（NonKana）を対象とする確定取消には
/// そのまま使えない。ここでは種別分類をせず、**既知長ぴったり読み戻し＋バイト一致**だけを
/// 条件に合成する。
///
/// do-no-harm 規律（`ReconvertStart` :318-329 と同じ）: GetSelection が非空選択なら／読み戻しが
/// 失敗したら／読み取りテキストが `expected` にバイト一致しなければ、**文書を一切書かない**
/// （StartComposition しない・`*self.out = false`）。呼び出し側は `out=false` を text_mismatch
/// として無害離脱する。
///
/// `left_context_out`（U9 上書き規約）: 照合 range とは**別に**その手前を追加 GetText して捕捉する
/// （`read_left_context` と同型）。追加 GetText が失敗しても left_context=None のまま続行し、undo
/// 本体は止めない（M-4）。どの経路（早期 return 含む）でも stale 文脈を残さない。
#[implement(ITfEditSession)]
pub struct CommitUndoStart {
    pub context: ITfContext,
    pub sink: ITfCompositionSink,
    pub composition: Rc<RefCell<Option<ITfComposition>>>,
    /// 照合 range の `StartComposition` 実成功だけを caller へ返す one-shot 出力。
    pub started: Rc<Cell<bool>>,
    /// 照合対象の確定文字列。この UTF-16 単位数だけ ShiftStart で戻し、バイト一致を確認する。
    pub expected: String,
    /// バイト一致して StartComposition したら true。呼び出し側は false を text_mismatch と扱う。
    pub out: Rc<RefCell<bool>>,
    /// 照合 range の**手前**の左文脈の出力先（TextService.left_context と共有）。必ず上書き。
    pub left_context_out: Rc<RefCell<Option<String>>>,
    // C-1: DLL_REF で生存数を数える。
    pub(crate) _guard: ComObjectGuard,
}

impl ITfEditSession_Impl for CommitUndoStart_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        unsafe {
            // U9: 上書き規約 — どの経路（早期 return 含む）でも stale 文脈を残さない。
            *self.left_context_out.borrow_mut() = None;
            *self.out.borrow_mut() = false;

            // 1) 既定選択を取得する（ReconvertStart と同じ ManuallyDrop 規律）。
            let mut sel = [TF_SELECTION {
                range: ManuallyDrop::new(None),
                style: TF_SELECTIONSTYLE {
                    ase: TF_AE_NONE,
                    fInterimChar: BOOL(0),
                },
            }];
            let mut fetched = 0u32;
            self.context
                .GetSelection(ec, TF_DEFAULT_SELECTION, &mut sel, &mut fetched)?;
            if fetched == 0 {
                ManuallyDrop::drop(&mut sel[0].range);
                return Ok(());
            }
            // GetSelection は range の所有権を渡す（AddRef 済み）。所有クローンして TSF 側参照を drop。
            let range: Option<ITfRange> = (*sel[0].range).as_ref().cloned();
            ManuallyDrop::drop(&mut sel[0].range);
            let range = match range {
                Some(r) => r,
                None => return Ok(()),
            };

            // 2) 空選択（キャレット）でなければ何も書かない（do-no-harm）。
            if !range.IsEmpty(ec)?.as_bool() {
                return Ok(());
            }

            // 3) 確定文字列の既知長（UTF-16 単位）だけ後方へ広げて読む。
            //    ShiftStart/GetText の単位＝UTF-16 単位なので encode_utf16().count() で数える。
            let want_len = self.expected.encode_utf16().count();
            // 呼び出し側で tlen≤64 を保証済みだが、多重防御でバッファ長を上限にする。
            if want_len == 0 || want_len > 64 {
                return Ok(());
            }
            let comp_range = range.Clone()?;
            let mut moved = 0i32;
            comp_range.ShiftStart(ec, -(want_len as i32), &mut moved, core::ptr::null())?;
            // 実際に戻れた単位数が足りなければ（文頭近く等）照合は成立しない。何も書かない。
            if moved != -(want_len as i32) {
                return Ok(());
            }
            let mut buf = [0u16; 64];
            let mut got = 0u32;
            comp_range.GetText(ec, 0, &mut buf, &mut got)?;
            let read = String::from_utf16_lossy(&buf[..got as usize]);

            // 4) バイト一致したときだけ合成する。不一致なら文書を一切触らない（do-no-harm）。
            if read != self.expected {
                return Ok(());
            }

            // 5) 照合 range の手前を追加 GetText して左文脈を捕捉する（U9・M-4: 失敗は None 続行）。
            *self.left_context_out.borrow_mut() = read_left_context(ec, &comp_range);

            // 6) 一致 range を composition 化する。
            let cc: ITfContextComposition = self.context.cast()?;
            let comp = cc.StartComposition(ec, &comp_range, &self.sink)?;
            // As above, only reentrancy after the StartComposition success return is observable;
            // signal it before the local composition slot assignment and any later COM callout.
            self.started.set(true);
            *self.composition.borrow_mut() = Some(comp);
            *self.out.borrow_mut() = true;
        }
        Ok(())
    }
}

/// composition の range を `text`（元ラテン）に戻してから閉じるセッション（取消復元・D9）。
///
/// `CancelComposition` は range を空文字で置換してから閉じる＝ユーザの元テキストを消して
/// しまう。再変換の取消では元のラテン列を残したいので、range を `text` に書き戻してから
/// `EndComposition` する。
#[implement(ITfEditSession)]
pub struct RestoreText {
    pub context: ITfContext,
    pub text: HSTRING,
    pub composition: Rc<RefCell<Option<ITfComposition>>>,
    // C-1: DLL_REF で生存数を数える。
    pub(crate) _guard: ComObjectGuard,
}

impl ITfEditSession_Impl for RestoreText_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        unsafe {
            let comp = self.composition.borrow().clone();
            if let Some(comp) = comp {
                // range を元ラテンへ書き戻してから閉じる（&HSTRING は Deref で &[u16] に通る）。
                // 巡4 T4: GetRange/SetText の失敗は Err で伝播する — 同期セッションでは
                // phrSession に DoEditSession の結果が載るため、cancel_reconvert 側の早期
                // return が効き「元テキスト未復元のまま再変換状態を破棄する」事故を防ぐ
                // （旧実装は握り潰して常に Ok で、外側の判定が形骸化していた）。
                let crange = comp.GetRange()?;
                crange.SetText(ec, 0, &self.text)?;
                // 復元後、キャレットを復元文字列の末尾へ移す。`CommitText` と同じ規律:
                // SetText 単独ではキャレットは合成開始位置（=単語の先頭）に残り、
                // EndComposition でアンカー（先頭）へ戻ってしまう。実機 SP5: Esc 復元後に
                // カーソルが単語の手前へ居座り、(a) 体感が悪い・(b) 直前が空白になって
                // 再変換キーが対象（直前ラテン列）を掴めなくなる。range を末尾へ畳んで
                // SetSelection する。失敗しても復元自体は済んでいるので EndComposition は続ける。
                if crange.Collapse(ec, TF_ANCHOR_END).is_ok() {
                    let mut sel = TF_SELECTION {
                        range: ManuallyDrop::new(Some(crange)),
                        style: TF_SELECTIONSTYLE {
                            ase: TF_AE_NONE,
                            fInterimChar: BOOL(0),
                        },
                    };
                    // TF_SELECTION.range は ManuallyDrop。SetSelection が必要なら内部で
                    // AddRef するので、自分の参照は必ず解放する（CommitText と同じ規律）。
                    let _ = self.context.SetSelection(ec, core::slice::from_ref(&sel));
                    ManuallyDrop::drop(&mut sel.range);
                }
                // EndComposition の失敗は Err で伝播する — composition が閉じられて
                // いないのに Ok を返すと、呼び出し側が再変換状態を破棄して TSF 側の
                // composition と共有スロットの状態が食い違う。SetText 成功＝テキストは
                // 復元済みなので、失敗時も復元済みテキストの巻き戻しはしない。
                comp.EndComposition(ec)?;
            }
            // 共有スロットは EndComposition が成功した経路でのみ落とす（上の `?`
            // が Err を返したらここへは届かない）。
            *self.composition.borrow_mut() = None;
        }
        Ok(())
    }
}

/// キャレット（既定選択）のスクリーン矩形を `ITfContextView::GetTextExt` で読む読み取り専用
/// セッション。候補窓/HUD の位置決め専用で、composition・選択・テキストには一切書き込まない
/// （`TF_ES_READ` で要求する）。取得した矩形は `out` に書き戻す。
///
/// `GetTextExt` はアプリのレイアウトが未確定だと `TF_E_NOLAYOUT` を返す。その場合や、選択
/// 取得・view 取得に失敗した場合は `out` を `None` のままにして抜ける＝呼び出し側
/// （`TextService::caret_point`）が直近の有効アンカー（初回は作業領域右下の無害位置）へ
/// フォールバックする（位置取得は best-effort。UIバグ5 の (200,200) 廃止後の契約）。
#[implement(ITfEditSession)]
pub struct QueryCaretRect {
    pub context: ITfContext,
    pub out: Rc<RefCell<Option<RECT>>>,
    // C-1: DLL_REF で生存数を数える。
    pub(crate) _guard: ComObjectGuard,
}

impl ITfEditSession_Impl for QueryCaretRect_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        unsafe {
            // 既定選択（キャレット）の range を取得する。GetSelection は range の所有権を渡す
            // （AddRef 済み）ので所有クローンして TSF 側の参照（ManuallyDrop）を drop で解放する
            // ＝ReconvertStart と同じ規律（怠るとリーク、二重に扱うと UAF）。
            let mut sel = [TF_SELECTION {
                range: ManuallyDrop::new(None),
                style: TF_SELECTIONSTYLE {
                    ase: TF_AE_NONE,
                    fInterimChar: BOOL(0),
                },
            }];
            let mut fetched = 0u32;
            self.context
                .GetSelection(ec, TF_DEFAULT_SELECTION, &mut sel, &mut fetched)?;
            if fetched == 0 {
                ManuallyDrop::drop(&mut sel[0].range);
                return Ok(());
            }
            let range: Option<ITfRange> = (*sel[0].range).as_ref().cloned();
            ManuallyDrop::drop(&mut sel[0].range);
            let Some(range) = range else {
                return Ok(());
            };

            // アクティブビューでキャレット矩形（スクリーン座標）を得る。レイアウト未確定なら
            // GetTextExt は TF_E_NOLAYOUT を返す＝out は None のまま（既定座標へフォールバック）。
            let view = self.context.GetActiveView()?;
            let mut rc = RECT::default();
            let mut clipped = BOOL(0);
            if view.GetTextExt(ec, &range, &mut rc, &mut clipped).is_ok() {
                *self.out.borrow_mut() = Some(rc);
            }
        }
        Ok(())
    }
}

/// 読みモニタ用アンカー矩形（スクリーン座標）を取る読み取り専用セッション。
/// composition **先頭**の矩形が第一候補（ライブ変換の preedit 全置換でキャレット=末尾の
/// X が打鍵ごとに跳ねるため、静止する先頭に窓を置く）。取れなければ同じ ec でキャレット
/// （既定選択）矩形へ落ちる — 独立した2セッションに分けないのは、NOLAYOUT ホストでは
/// 両者が同一 view の GetTextExt で同時に失敗し、2本目が空振りセッションを毎打鍵
/// 1本増やすだけだから（spec 性能C1）。ev ログは出さない（打鍵ごとに走るフック用）。
#[implement(ITfEditSession)]
pub struct QueryMonitorAnchorRect {
    pub context: ITfContext,
    pub composition: Rc<RefCell<Option<ITfComposition>>>,
    pub out: Rc<RefCell<Option<RECT>>>,
    pub(crate) _guard: ComObjectGuard,
}

impl ITfEditSession_Impl for QueryMonitorAnchorRect_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        unsafe {
            let view = self.context.GetActiveView()?;
            // borrow は COM コールアウト前に clone で落とす（CancelComposition と同じ規律 —
            // GetTextExt 中の再入に借用を持ち込まない）。GetRange の戻りも Clone してから
            // Collapse する（composition 本体の range を縮めない）。
            let comp = self.composition.borrow().clone();
            if let Some(comp) = comp {
                if let Ok(range) = comp.GetRange() {
                    if let Ok(start) = range.Clone() {
                        let _ = start.Collapse(ec, TF_ANCHOR_START);
                        let mut rc = RECT::default();
                        let mut clipped = BOOL(0);
                        if view.GetTextExt(ec, &start, &mut rc, &mut clipped).is_ok()
                            && !(rc.left == 0 && rc.top == 0 && rc.right == 0 && rc.bottom == 0)
                        {
                            *self.out.borrow_mut() = Some(rc);
                            return Ok(());
                        }
                    }
                }
            }
            // キャレット矩形（QueryCaretRect と同じ規律 — ManuallyDrop の解放を怠らない）。
            let mut sel = [TF_SELECTION {
                range: ManuallyDrop::new(None),
                style: TF_SELECTIONSTYLE {
                    ase: TF_AE_NONE,
                    fInterimChar: BOOL(0),
                },
            }];
            let mut fetched = 0u32;
            self.context
                .GetSelection(ec, TF_DEFAULT_SELECTION, &mut sel, &mut fetched)?;
            if fetched == 0 {
                ManuallyDrop::drop(&mut sel[0].range);
                return Ok(());
            }
            let range: Option<ITfRange> = (*sel[0].range).as_ref().cloned();
            ManuallyDrop::drop(&mut sel[0].range);
            let Some(range) = range else {
                return Ok(());
            };
            let mut rc = RECT::default();
            let mut clipped = BOOL(0);
            if view.GetTextExt(ec, &range, &mut rc, &mut clipped).is_ok() {
                *self.out.borrow_mut() = Some(rc);
            }
        }
        Ok(())
    }
}

/// UIバグ4: ホストのスクロール・リフロー（`ITfTextLayoutSink::OnLayoutChange`）で
/// 表示中の候補窓・読みモニタをキャレットへ追従させるための再照会セッション。
/// OnLayoutChange の COM コールバック内からは同期 edit session を要求できない
/// （TSF の再入規律。Mozc も非同期化している）ため、`TF_ES_ASYNC | TF_ES_READ` で
/// 投げて本セッションの内側で GetTextExt し直し、結果を
/// `text_service::layout_refresh_apply` へ渡す。矩形の取り方（候補窓=キャレット、
/// 読みモニタ=composition 先頭→キャレットの2段試行）は QueryCaretRect /
/// QueryMonitorAnchorRect と同一規律。
#[implement(ITfEditSession)]
pub struct RefreshAnchorOnLayout {
    pub context: ITfContext,
    pub composition: Rc<RefCell<Option<ITfComposition>>>,
    /// セッション投入時点のレイアウト世代（TextService::layout_sink_gen）。投入後に
    /// Activate/Deactivate/context 貼替が起きていたら、layout_refresh_apply が旧 context
    /// の座標を現在の表示へ適用しない（巡2 E1/E3 — TF_ES_ASYNC セッションは旧 context を
    /// 強参照で保持したまま遅延実行されるため、投入時の世代で結果の新旧を判別する）。
    pub gen: u64,
    // C-1: DLL_REF で生存数を数える。
    pub(crate) _guard: ComObjectGuard,
}

impl ITfEditSession_Impl for RefreshAnchorOnLayout_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        // 巡1レビュー 8c2354e指摘3: 全 early-return 経路で layout_refresh_apply を呼び
        // layout_refresh_pending を必ず解除する。さもないと一度失敗したら以後の
        // OnLayoutChange がすべて pending で無視され追従が恒久的に止まる。
        unsafe {
            // 失敗時も pending 解消のため None,None で apply を呼ぶ（relayout は
            // last_valid_anchor を使うので座標が失われるわけではない）。
            let fail = || crate::text_service::layout_refresh_apply(self.gen, None, None);

            let Ok(view) = self.context.GetActiveView() else {
                fail();
                return Ok(());
            };
            // キャレット矩形（候補窓用）。GetSelection の所有権は ManuallyDrop 規律で解放。
            let mut sel = [TF_SELECTION {
                range: ManuallyDrop::new(None),
                style: TF_SELECTIONSTYLE {
                    ase: TF_AE_NONE,
                    fInterimChar: BOOL(0),
                },
            }];
            let mut fetched = 0u32;
            if self
                .context
                .GetSelection(ec, TF_DEFAULT_SELECTION, &mut sel, &mut fetched)
                .is_err()
            {
                // 失敗時の range 書込は未規定 — 書かれていた場合に備え fetched==0 枝と同じく
                // TSF 側の参照をここで解放する（対称化。事後検証 2026-08-20 の指摘）。
                ManuallyDrop::drop(&mut sel[0].range);
                fail();
                return Ok(());
            }
            if fetched == 0 {
                ManuallyDrop::drop(&mut sel[0].range);
                fail();
                return Ok(());
            }
            let caret_range: Option<ITfRange> = (*sel[0].range).as_ref().cloned();
            ManuallyDrop::drop(&mut sel[0].range);
            let Some(caret_range) = caret_range else {
                fail();
                return Ok(());
            };
            let mut rc = RECT::default();
            let mut clipped = BOOL(0);
            let caret = view
                .GetTextExt(ec, &caret_range, &mut rc, &mut clipped)
                .ok()
                .map(|_| rc);

            // composition 先頭矩形（読みモニタ用）。取れなければキャレット矩形へ落ちる
            // （QueryMonitorAnchorRect の2段試行と同じ）。borrow は clone で落とす。
            let head = {
                let comp = self.composition.borrow().clone();
                comp.and_then(|c| c.GetRange().ok())
                    .and_then(|r| r.Clone().ok())
                    .and_then(|start| {
                        let _ = start.Collapse(ec, TF_ANCHOR_START);
                        let mut hrc = RECT::default();
                        let mut hclipped = BOOL(0);
                        view.GetTextExt(ec, &start, &mut hrc, &mut hclipped)
                            .ok()
                            .filter(|_| {
                                !(hrc.left == 0
                                    && hrc.top == 0
                                    && hrc.right == 0
                                    && hrc.bottom == 0)
                            })
                            .map(|_| hrc)
                    })
            };
            let monitor = head.or(caret);

            let to_anchor =
                |r: Option<RECT>| r.and_then(crate::candidate_window::caret_rect_to_anchor);
            crate::text_service::layout_refresh_apply(
                self.gen,
                to_anchor(caret),
                to_anchor(monitor),
            );
        }
        Ok(())
    }
}

/// Spec2: 文書の InputScope（`GUID_PROP_INPUTSCOPE`）を読み、IS_PASSWORD を含むかを
/// 判定する読み取り専用セッション（`TF_ES_READ` で要求する）。composition・選択・テキストには
/// 一切書き込まない。判定結果は `out` に `Some(bool)` で書き戻す。
///
/// COM 呼出し鎖:
///   `GetAppProperty(GUID_PROP_INPUTSCOPE)` → `ITfReadOnlyProperty`
///   → 文書先頭の空 range（`GetStart(ec)`）で `GetValue(ec, range)` → VT_UNKNOWN の VARIANT
///   → その `punkVal`（IUnknown）を `ITfInputScope` へ QI
///   → `GetInputScopes` で InputScope 配列を得て `scopes_contain_password` で判定。
/// **どの段の失敗も `out=None` のまま**（呼び出し側が false へ倒す＝通常欄を誤って
/// direct 化しない安全側）。GetInputScopes の out 配列は `CoTaskMemFree` で解放する。
#[implement(ITfEditSession)]
pub struct QueryInputScopes {
    pub context: ITfContext,
    pub out: Rc<RefCell<Option<bool>>>,
    // C-1: DLL_REF で生存数を数える。
    pub(crate) _guard: ComObjectGuard,
}

impl ITfEditSession_Impl for QueryInputScopes_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        unsafe {
            // (1) InputScope の app property を引く（read-only property）。
            let Ok(prop) = self.context.GetAppProperty(&GUID_PROP_INPUTSCOPE) else {
                return Ok(());
            };
            // (2) 文書先頭の空 range で property 値（VT_UNKNOWN）を読む。ec が要るのでこの
            //     読み取りセッション内で行う。
            let Ok(range) = self.context.GetStart(ec) else {
                return Ok(());
            };
            let Ok(variant) = prop.GetValue(ec, &range) else {
                return Ok(());
            };
            // (3) VARIANT の punkVal（IUnknown）を取り出し ITfInputScope へ QI する。
            //     variant はスコープ終端で VariantClear され punkVal は解放されるため、cast で
            //     自前の参照（AddRef 済み）を持つ。VT_UNKNOWN 以外の union フィールドを IUnknown と
            //     して読むと不正ポインタになりうるので vt を先に検証する（未設定=VT_EMPTY 等は素通し）。
            if variant.Anonymous.Anonymous.vt != VT_UNKNOWN {
                return Ok(());
            }
            let unk: Option<&IUnknown> = (*variant.Anonymous.Anonymous.Anonymous.punkVal).as_ref();
            let Some(scope) = unk.and_then(|u| u.cast::<ITfInputScope>().ok()) else {
                return Ok(());
            };
            // (4) InputScope 配列を得て password を判定する。out 配列は CoTaskMemFree で解放。
            let mut ptr: *mut InputScope = core::ptr::null_mut();
            let mut n: u32 = 0;
            if scope.GetInputScopes(&mut ptr, &mut n).is_ok() && !ptr.is_null() {
                let scopes: Vec<i32> = core::slice::from_raw_parts(ptr, n as usize)
                    .iter()
                    .map(|s| s.0)
                    .collect();
                CoTaskMemFree(Some(ptr as *const core::ffi::c_void));
                *self.out.borrow_mut() =
                    Some(crate::text_service::scopes_contain_password(&scopes));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod composition_end_state_tests {
    use super::{
        classify_composition_end_error, composition_end_stays_pending, CompositionEndStatus,
    };
    use windows::core::HRESULT;
    use windows::Win32::Foundation::E_UNEXPECTED;
    use windows::Win32::UI::TextServices::{TF_E_DISCONNECTED, TF_E_LOCKED};

    #[test]
    fn failed_end_keeps_retry_only_while_same_composition_is_tracked() {
        assert!(composition_end_stays_pending(false, true));
        assert!(!composition_end_stays_pending(true, true));
        // EndComposition が Err でも同期終了 callback が slot を落としたなら再試行は不要。
        assert!(!composition_end_stays_pending(false, false));
    }

    #[test]
    fn unexpected_end_with_unexpected_range_is_idempotently_closed() {
        assert_eq!(
            classify_composition_end_error(E_UNEXPECTED, Some(E_UNEXPECTED)),
            CompositionEndStatus::Closed,
        );
    }

    #[test]
    fn terminal_and_transient_end_errors_are_distinct() {
        assert_eq!(
            classify_composition_end_error(TF_E_DISCONNECTED, None),
            CompositionEndStatus::Terminal,
        );
        assert_eq!(
            classify_composition_end_error(TF_E_LOCKED, None),
            CompositionEndStatus::Retryable,
        );
        // Unknown HRESULTs are still bounded by the caller's retry budget.
        assert_eq!(
            classify_composition_end_error(HRESULT(0x80004005_u32 as i32), None),
            CompositionEndStatus::Retryable,
        );
    }
}
