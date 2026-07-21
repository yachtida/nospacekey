//! 自前 ITextStoreACP（最小部分集合 + 全 26 メソッド）と composition sink。
//! DocState を Rc 共有し、ホストは Rc<DocState> 経由で committed()/preedit() を読む。

use std::rc::Rc;
use std::cell::{Cell, RefCell};

use windows::core::{implement, Interface, Ref, Result, Error, BOOL, GUID, HRESULT, IUnknown, PCWSTR, PWSTR};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, POINT, RECT, HWND};
use windows::Win32::System::Com::{IDataObject, FORMATETC};
use windows::Win32::UI::TextServices::{
    ITextStoreACP, ITextStoreACP_Impl, ITextStoreACPSink,
    ITfContextOwnerCompositionSink, ITfContextOwnerCompositionSink_Impl,
    ITfCompositionView, ITfRange,
    TS_SELECTION_ACP, TS_SELECTIONSTYLE, TS_STATUS, TS_TEXTCHANGE, TS_RUNINFO, TS_ATTRVAL,
    TS_RT_PLAIN, TS_E_NOLOCK, TS_E_INVALIDPOS, TEXT_STORE_LOCK_FLAGS,
};

use crate::doc_state::DocState;

const TS_IAS_QUERYONLY_U: u32 = 2; // ITextStoreACP::InsertTextAtSelection dwflags
// NOQUERY は QUERYONLY ビットが立っていない経路（else）で扱う。値の記録用に保持。
#[allow(dead_code)]
const TS_IAS_NOQUERY_U: u32 = 1;

/// 診断: TIP と同じ `%TEMP%\nospacekey-tip.log` へ時系列で追記する（TIP の `[pid N] ev=` と
/// 交互に並ぶので、合成開始/終了・ロック・レイアウト要求の前後関係を1ファイルで追える）。
/// 「初打鍵で最初の合成だけ msctf が即終了する」既存バグの真因特定用。
pub(crate) fn hlog(msg: &str) {
    use std::io::Write;
    if let Some(dir) = std::env::var_os("TEMP") {
        let path = std::path::Path::new(&dir).join("nospacekey-tip.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "[harness] {msg}");
        }
    }
}

/// ロック状態と advise された sink を DocState に併設する補助。
pub struct StoreState {
    pub doc: DocState,
    sink: RefCell<Option<ITextStoreACPSink>>,
    locked: Cell<bool>,
}

impl StoreState {
    fn new() -> Self {
        StoreState { doc: DocState::new(), sink: RefCell::new(None), locked: Cell::new(false) }
    }
    // テスト専用: ロック保持を擬似する。
    #[cfg(test)]
    pub fn force_locked(&self, v: bool) { self.locked.set(v); }
    // ホスト/テストからの委譲（DocState の getter を素通し）。
    pub fn full(&self) -> String { self.doc.full() }
    pub fn preedit(&self) -> String { self.doc.preedit() }
    pub fn committed(&self) -> String { self.doc.committed() }
    pub fn selection(&self) -> (i32, i32) { self.doc.selection() }
    /// 合成中か（warm_up が「合成が生き残った」ことを判定するのに使う）。
    pub fn composing(&self) -> bool { self.doc.composing() }
    pub fn start_composition(&self) { self.doc.start_composition() }
    pub fn reset(&self) { self.doc.reset() }
    /// SP5 再変換テスト用: 既存確定テキストをシードする（item13 が "React nihongo" を据える）。
    pub fn seed_committed(&self, s: &str) {
        let u: Vec<u16> = s.encode_utf16().collect();
        self.doc.seed_committed(&u);
    }
    /// SP5 step-6 テスト用: 非空選択を据える（DocState::set_selection 素通し、UTF-16 オフセット）。
    pub fn set_selection(&self, start: i32, end: i32) { self.doc.set_selection(start, end); }
}

#[implement(ITextStoreACP, ITfContextOwnerCompositionSink)]
pub struct HarnessTextStore {
    st: Rc<StoreState>,
}

impl HarnessTextStore {
    /// COM インタフェースと、ホストが getter を読むための共有状態を返す。
    pub fn create() -> (ITextStoreACP, Rc<StoreState>) {
        let st = Rc::new(StoreState::new());
        let store: ITextStoreACP = HarnessTextStore { st: st.clone() }.into();
        (store, st)
    }
}

impl ITextStoreACP_Impl for HarnessTextStore_Impl {
    // ---- 実体: advise / lock ----
    fn AdviseSink(&self, _riid: *const GUID, punk: Ref<IUnknown>, _dwmask: u32) -> Result<()> {
        let unk = punk.ok()?;
        *self.st.sink.borrow_mut() = Some(unk.cast::<ITextStoreACPSink>()?);
        Ok(())
    }
    fn UnadviseSink(&self, _punk: Ref<IUnknown>) -> Result<()> {
        *self.st.sink.borrow_mut() = None;
        Ok(())
    }
    /// 同期ロックモデルの核心。即座に OnLockGranted を再入し、S_OK を返す。
    fn RequestLock(&self, dwlockflags: u32) -> Result<HRESULT> {
        let sink = self.st.sink.borrow().clone().ok_or_else(|| Error::from(E_INVALIDARG))?;
        hlog(&format!("RequestLock flags={dwlockflags:#x} >>>"));
        self.st.locked.set(true);
        let hr = unsafe { sink.OnLockGranted(TEXT_STORE_LOCK_FLAGS(dwlockflags)) };
        self.st.locked.set(false);
        hlog(&format!("RequestLock flags={dwlockflags:#x} <<< ok={}", hr.is_ok()));
        hr?; // 編集セッションのエラーを伝播
        Ok(HRESULT(0)) // S_OK（TS_S_ASYNC は禁止）
    }
    fn GetStatus(&self) -> Result<TS_STATUS> {
        Ok(TS_STATUS { dwDynamicFlags: 0, dwStaticFlags: 0 })
    }
    fn QueryInsert(&self, _acpteststart: i32, _acptestend: i32, cch: u32,
                   pacpresultstart: *mut i32, pacpresultend: *mut i32) -> Result<()> {
        let (s, _) = self.st.doc.selection();
        unsafe { *pacpresultstart = s; *pacpresultend = s + cch as i32; }
        Ok(())
    }
    // ---- 実体: selection ----
    fn GetSelection(&self, _ulindex: u32, ulcount: u32,
                    pselection: *mut TS_SELECTION_ACP, pcfetched: *mut u32) -> Result<()> {
        if ulcount == 0 { unsafe { *pcfetched = 0; } return Ok(()); }
        let (s, e) = self.st.doc.selection();
        unsafe {
            *pselection = TS_SELECTION_ACP { acpStart: s, acpEnd: e, style: TS_SELECTIONSTYLE::default() };
            *pcfetched = 1;
        }
        Ok(())
    }
    fn SetSelection(&self, ulcount: u32, pselection: *const TS_SELECTION_ACP) -> Result<()> {
        if ulcount >= 1 {
            let sel = unsafe { *pselection };
            self.st.doc.set_selection(sel.acpStart, sel.acpEnd);
        }
        Ok(())
    }
    // ---- 実体: text read ----
    fn GetText(&self, acpstart: i32, acpend: i32, pchplain: PWSTR, cchplainreq: u32,
               pcchplainret: *mut u32, prgruninfo: *mut TS_RUNINFO, cruninforeq: u32,
               pcruninforet: *mut u32, pacpnext: *mut i32) -> Result<()> {
        let len = self.st.doc.len();
        // 範囲外位置はクランプせずエラーにする（実ホスト同様）。TIP の範囲外読みを黙って
        // 隠さずハーネスで検出するため。acpend<0 は「末尾まで」の慣習なので許す（SetText と同規律）。
        if acpstart < 0 || acpstart > len || (acpend >= 0 && (acpend < acpstart || acpend > len)) {
            return Err(Error::from(TS_E_INVALIDPOS));
        }
        let start = acpstart;
        let end = if acpend < 0 { len } else { acpend };
        let want = (end - start) as u32;
        let n = want.min(cchplainreq);
        // バッファをローカルにコピーしてから書き出す（borrow を跨がない）。
        let slice: Vec<u16> = self.st.doc.slice(start, start + n as i32);
        unsafe {
            if !pchplain.is_null() && n > 0 {
                std::ptr::copy_nonoverlapping(slice.as_ptr(), pchplain.0, n as usize);
            }
            if !pcchplainret.is_null() { *pcchplainret = n; }
            // 返した文字数が 0 なら run は 0 個（空 range で長さ 0 の run を 1 個返すのは TSF 契約違反）。
            if n > 0 && cruninforeq >= 1 && !prgruninfo.is_null() {
                *prgruninfo = TS_RUNINFO { uCount: n, r#type: TS_RT_PLAIN };
                if !pcruninforet.is_null() { *pcruninforet = 1; }
            } else if !pcruninforet.is_null() { *pcruninforet = 0; }
            if !pacpnext.is_null() { *pacpnext = start + n as i32; }
        }
        Ok(())
    }
    // ---- 実体: text write ----
    fn SetText(&self, _dwflags: u32, acpstart: i32, acpend: i32, pchtext: &PCWSTR, cch: u32)
        -> Result<TS_TEXTCHANGE> {
        if !self.st.locked.get() { return Err(Error::from(TS_E_NOLOCK)); }
        let len = self.st.doc.len();
        if acpstart < 0 || acpstart > len || acpend < acpstart || acpend > len {
            return Err(Error::from(TS_E_INVALIDPOS));
        }
        // cch==0 で pchtext が null の場合、from_raw_parts はゼロ長でも非null要求があり UB。空スライスで回避。
        let new: &[u16] = if cch == 0 { &[] } else { unsafe { std::slice::from_raw_parts(pchtext.0, cch as usize) } };
        // SetText 経路: テキストだけ置換しキャレットは動かさない（実 msctf 準拠）。
        self.st.doc.set_text(acpstart, acpend, new);
        Ok(TS_TEXTCHANGE { acpStart: acpstart, acpOldEnd: acpend, acpNewEnd: acpstart + cch as i32 })
    }
    // ---- 実体: insert at selection ----
    fn InsertTextAtSelection(&self, dwflags: u32, pchtext: &PCWSTR, cch: u32,
                             pacpstart: *mut i32, pacpend: *mut i32, pchange: *mut TS_TEXTCHANGE)
        -> Result<()> {
        let (sel_start, sel_end) = self.st.doc.selection();
        if dwflags & TS_IAS_QUERYONLY_U != 0 {
            // 変異せず、置換される範囲（＝選択）を返すだけ。
            unsafe {
                if !pacpstart.is_null() { *pacpstart = sel_start; }
                if !pacpend.is_null() { *pacpend = sel_end; }
            }
            return Ok(());
        }
        if !self.st.locked.get() { return Err(Error::from(TS_E_NOLOCK)); }
        // cch==0 で pchtext が null の場合、from_raw_parts はゼロ長でも非null要求があり UB。空スライスで回避。
        let new: &[u16] = if cch == 0 { &[] } else { unsafe { std::slice::from_raw_parts(pchtext.0, cch as usize) } };
        // InsertTextAtSelection(NOQUERY) 経路: 挿入なのでキャレットを挿入末尾へ動かす。
        self.st.doc.insert_at_selection(sel_start, sel_end, new);
        let new_end = sel_start + cch as i32;
        unsafe {
            if !pacpstart.is_null() { *pacpstart = sel_start; }
            if !pacpend.is_null() { *pacpend = new_end; }
            // 変異した場合は *pchange を必ず埋める。
            if !pchange.is_null() {
                *pchange = TS_TEXTCHANGE { acpStart: sel_start, acpOldEnd: sel_end, acpNewEnd: new_end };
            }
        }
        Ok(())
    }
    fn GetEndACP(&self) -> Result<i32> { Ok(self.st.doc.len()) }

    // ---- stub（正しい HRESULT / 無害値）----
    fn GetActiveView(&self) -> Result<u32> { Ok(0) }
    fn GetFormattedText(&self, _: i32, _: i32) -> Result<IDataObject> { Err(Error::from(E_NOTIMPL)) }
    fn GetEmbedded(&self, _: i32, _: *const GUID, _: *const GUID) -> Result<IUnknown> { Err(Error::from(E_NOTIMPL)) }
    fn QueryInsertEmbedded(&self, _: *const GUID, _: *const FORMATETC) -> Result<BOOL> { Ok(BOOL(0)) }
    fn InsertEmbedded(&self, _: u32, _: i32, _: i32, _: Ref<IDataObject>) -> Result<TS_TEXTCHANGE> { Err(Error::from(E_NOTIMPL)) }
    fn InsertEmbeddedAtSelection(&self, _: u32, _: Ref<IDataObject>, _: *mut i32, _: *mut i32, _: *mut TS_TEXTCHANGE) -> Result<()> { Err(Error::from(E_NOTIMPL)) }
    fn RequestSupportedAttrs(&self, _: u32, _: u32, _: *const GUID) -> Result<()> { Ok(()) }
    fn RequestAttrsAtPosition(&self, _: i32, _: u32, _: *const GUID, _: u32) -> Result<()> { Ok(()) }
    fn RequestAttrsTransitioningAtPosition(&self, _: i32, _: u32, _: *const GUID, _: u32) -> Result<()> { Ok(()) }
    fn FindNextAttrTransition(&self, _: i32, _: i32, _: u32, _: *const GUID, _: u32,
                              pacpnext: *mut i32, pffound: *mut BOOL, plfoundoffset: *mut i32) -> Result<()> {
        unsafe {
            if !pacpnext.is_null() { *pacpnext = 0; }
            if !pffound.is_null() { *pffound = BOOL(0); }
            if !plfoundoffset.is_null() { *plfoundoffset = 0; }
        }
        Ok(())
    }
    fn RetrieveRequestedAttrs(&self, _: u32, _: *mut TS_ATTRVAL, pcfetched: *mut u32) -> Result<()> {
        unsafe { if !pcfetched.is_null() { *pcfetched = 0; } } Ok(())
    }
    // レイアウト系: 実窓が無いので NOLAYOUT（TIP の caret_point は固定で未使用）。
    fn GetACPFromPoint(&self, _: u32, _: *const POINT, _: u32) -> Result<i32> { Err(Error::from(TS_E_INVALIDPOS)) }
    // レイアウト: 実窓は無いが、NOLAYOUT を返すと msctf が（初打鍵のエンジン起動遅延と
    // 相まって）最初の合成を即終了させてしまう（実 Notepad は妥当なレイアウトが返るので無傷）。
    // ヘッドレスでも実アプリ同様に妥当な矩形を返して合成を生かす（描画はしない）。1 文字 10px 相当。
    fn GetTextExt(&self, _vcview: u32, acpstart: i32, acpend: i32, prc: *mut RECT, pfclipped: *mut BOOL) -> Result<()> {
        // 診断: msctf が「初打鍵の合成」前後でレイアウトを要求しているか（NOLAYOUT 即終了説の検証）。
        hlog(&format!("GetTextExt acp=[{acpstart},{acpend})"));
        let left = 100 + acpstart.max(0) * 10;
        let right = 100 + (acpend.max(acpstart) + 1) * 10;
        unsafe {
            if !prc.is_null() { *prc = RECT { left, top: 100, right, bottom: 120 }; }
            if !pfclipped.is_null() { *pfclipped = BOOL(0); }
        }
        Ok(())
    }
    fn GetScreenExt(&self, _: u32) -> Result<RECT> {
        hlog("GetScreenExt");
        Ok(RECT { left: 0, top: 0, right: 1920, bottom: 1080 })
    }
    fn GetWnd(&self, _: u32) -> Result<HWND> { Ok(HWND(std::ptr::null_mut())) }
}

impl ITfContextOwnerCompositionSink_Impl for HarnessTextStore_Impl {
    fn OnStartComposition(&self, _pcomposition: Ref<ITfCompositionView>) -> Result<BOOL> {
        // 合成開始 = 現在キャレットに合成範囲を立てる。範囲は以降の SetText が追従。
        hlog(&format!("OnStartComposition buf={} sel={:?}", self.st.doc.len(), self.st.doc.selection()));
        self.st.doc.start_composition();
        Ok(BOOL(1)) // pfOk = TRUE: 合成を許可
    }
    fn OnUpdateComposition(&self, _pcomposition: Ref<ITfCompositionView>, _prangenew: Ref<ITfRange>) -> Result<()> {
        hlog("OnUpdateComposition");
        Ok(()) // 範囲追従は replace() 側で行うので no-op
    }
    fn OnEndComposition(&self, _pcomposition: Ref<ITfCompositionView>) -> Result<()> {
        hlog(&format!("OnEndComposition buf={} sel={:?}", self.st.doc.len(), self.st.doc.selection()));
        self.st.doc.end_composition();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::HarnessTextStore;
    use windows::Win32::UI::TextServices::{
        ITextStoreACP, TS_SELECTION_ACP, TS_SELECTIONSTYLE, TS_TEXTCHANGE, TS_IAS_QUERYONLY, TS_IAS_NOQUERY,
    };

    // windows-0.62 の ITextStoreACP コンシューマ側ラッパは pchtext を &[u16]
    // として受け取り cch を .len() から導出する（NUL 終端しない）。よって計画の
    // u16z（NUL 付き）ではなく非終端の u16s を使う。挙動・アサーションは同一。
    fn u16s(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn insert_query_then_settext_roundtrip() {
        let (store, st) = HarnessTextStore::create();
        let acp: ITextStoreACP = store;
        st.force_locked(true); // テスト用にロック保持を擬似（実機は RequestLock 経由）

        // QUERYONLY: 現在選択 [0,0) を返し、変異しない。
        let mut a = 0i32; let mut b = 0i32; let mut ch = TS_TEXTCHANGE::default();
        let q: Vec<u16> = Vec::new(); // cch = 0
        unsafe {
            acp.InsertTextAtSelection(TS_IAS_QUERYONLY, &q, &mut a, &mut b, &mut ch).unwrap();
        }
        assert_eq!((a, b), (0, 0));
        assert_eq!(st.full(), "");

        // SetText で合成範囲に "にほんご" を書く（合成は start_composition 済み想定）。
        st.start_composition();
        let t = u16s("にほんご"); // cch = 4
        let tc = unsafe { acp.SetText(0, 0, 0, &t).unwrap() };
        assert_eq!(tc.acpNewEnd, 4);
        assert_eq!(st.preedit(), "にほんご");

        // 実 msctf 準拠: SetText は合成範囲のテキストを置換するだけでキャレットを動かさない。
        // 合成開始位置(0)のままなので GetSelection は [0,0)。ライブ変換中ここが先頭に居座るのが
        // 実機バグで、TIP が StartOrUpdatePreedit で明示 SetSelection(末尾) して初めて末尾追従する。
        let mut sel = [TS_SELECTION_ACP::default()];
        let mut fetched = 0u32;
        unsafe { acp.GetSelection(0, &mut sel, &mut fetched).unwrap(); }
        assert_eq!(fetched, 1);
        assert_eq!((sel[0].acpStart, sel[0].acpEnd), (0, 0));

        // TIP の修正（StartOrUpdatePreedit の明示 SetSelection 末尾）を模す: SetSelection([4,4))
        // で初めてキャレットが preedit 末尾へ動く＝ふつうの IME 挙動。
        let want_end = [TS_SELECTION_ACP { acpStart: 4, acpEnd: 4, style: TS_SELECTIONSTYLE::default() }];
        unsafe { acp.SetSelection(&want_end).unwrap(); }
        let mut sel2 = [TS_SELECTION_ACP::default()];
        let mut fetched2 = 0u32;
        unsafe { acp.GetSelection(0, &mut sel2, &mut fetched2).unwrap(); }
        assert_eq!((sel2[0].acpStart, sel2[0].acpEnd), (4, 4));
    }

    #[test]
    fn insert_noquery_mutates_and_reports_change() {
        let (store, st) = HarnessTextStore::create();
        let acp: ITextStoreACP = store;
        st.force_locked(true);
        let mut a = 0i32; let mut b = 0i32; let mut ch = TS_TEXTCHANGE::default();
        let t = u16s("あ"); // cch = 1
        unsafe {
            acp.InsertTextAtSelection(TS_IAS_NOQUERY, &t, &mut a, &mut b, &mut ch).unwrap();
        }
        assert_eq!(st.full(), "あ");
        // 変異経路では *pchange を必ず埋める。
        assert_eq!((ch.acpStart, ch.acpOldEnd, ch.acpNewEnd), (0, 0, 1));
        // InsertTextAtSelection(NOQUERY) は挿入なのでキャレットを挿入末尾へ動かす（SetText と違う）。
        assert_eq!(st.selection(), (1, 1));
    }
}
