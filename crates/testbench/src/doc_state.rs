//! ITextStoreACP の純粋な状態（UTF-16 バッファ＋選択＋合成範囲）。
//! COM から切り離して TDD できるようにする。STA 単一スレッド前提なので
//! 内部可変は Cell/RefCell（Send 不要）。
//!
//! preedit/committed の分離は合成範囲 `comp: Option<(start,end)>` で行う:
//!   - OnStartComposition で comp = Some((caret, caret))
//!   - 合成中の SetText/replace で comp.end を更新（TIP は合成範囲を毎回全置換する）
//!   - OnEndComposition で comp = None（バッファに残るものは確定扱い）
//!
//! キャレット（選択）モデルは実 msctf に忠実化する:
//!   - SetText（`ITfRange::SetText`→`ITextStoreACP::SetText`）はテキストを置換するだけで
//!     キャレット（選択）を動かさない。テキスト変更ぶんの整合（前は不変・後ろはシフト）だけ取る。
//!     したがってキャレットが合成開始位置（先頭）にあれば、合成中ずっと先頭に居座る。これが
//!     「ライブ変換中にカーソルが先頭のまま」実機バグの正体で、TIP が preedit 更新後に
//!     明示 SetSelection（末尾へ）して初めて、ふつうの IME のようにキャレットが末尾追従する。
//!   - InsertTextAtSelection(NOQUERY) は挿入なのでキャレットを挿入末尾へ動かす（劣化 commit 経路）。
//!   - 確定（OnEndComposition）時、TIP が明示 SetSelection していなければ、キャレットは合成開始
//!     位置（アンカー＝comp.start）へ戻る。これが「確定するとカーソルが打ち始めた先頭に戻る」
//!     実機バグの正体で、TIP が確定後に SetSelection すれば末尾に留まる（＝修正）。
//!     `sel_explicit` でこの分岐を表現する。

use std::cell::{Cell, RefCell};

pub struct DocState {
    buf: RefCell<Vec<u16>>,
    acp_start: Cell<i32>,
    acp_end: Cell<i32>,
    comp: Cell<Option<(i32, i32)>>,
    /// 直近のテキスト編集（SetText/Insert）以降に TIP が明示 SetSelection したか。
    /// 確定時にキャレットをアンカーへ戻すか末尾に留めるかを決める。
    sel_explicit: Cell<bool>,
}

impl DocState {
    pub fn new() -> Self {
        DocState {
            buf: RefCell::new(Vec::new()),
            acp_start: Cell::new(0),
            acp_end: Cell::new(0),
            comp: Cell::new(None),
            sel_explicit: Cell::new(false),
        }
    }

    pub fn len(&self) -> i32 { self.buf.borrow().len() as i32 }
    pub fn selection(&self) -> (i32, i32) { (self.acp_start.get(), self.acp_end.get()) }
    /// TIP の明示 SetSelection（ITfContext::SetSelection 経由）。キャレットを指定位置へ
    /// 動かし、「明示指定された」ことを記録する（確定時のアンカー戻しを抑止する）。
    pub fn set_selection(&self, start: i32, end: i32) {
        self.acp_start.set(start);
        self.acp_end.set(end);
        self.sel_explicit.set(true);
    }

    /// SetText 経路（`ITfRange::SetText`→`ITextStoreACP::SetText`）。[start,end) を `text` で
    /// 置換するが、**キャレット（選択）は動かさない**。実 msctf/アプリは SetText では選択を移動
    /// せず、テキスト変更ぶんの整合だけ取る:
    ///   - 変更開始位置 `start` 以前のキャレットは不変、
    ///   - 変更終了位置 `end` 以後のキャレットは増減ぶん（delta）シフト、
    ///   - その中間に居たキャレットは `start` へ畳む。
    /// したがってキャレットが合成開始位置（先頭）にあれば、合成中ずっと先頭に居座る＝
    /// 「ライブ変換中にカーソルが先頭のまま」実機バグの再現。TIP が preedit 更新後に明示
    /// SetSelection（末尾へ）して初めて、ふつうの IME のようにキャレットが末尾追従する。
    /// テキスト編集なので「明示 SetSelection」フラグはここで倒す。
    /// 合成中なら合成範囲の終端を新しい末尾に追従させる（TIP は範囲を毎回全置換するので start は不変）。
    pub fn set_text(&self, start: i32, end: i32, text: &[u16]) {
        // pub API の防御的クランプ（slice() と同じ規約）。実呼び出し元は全て検証済みだが、
        // 範囲外/逆順の引数でも splice が panic しないよう、バッファ長と start<=end へ丸める。
        let len = self.len();
        let start = start.clamp(0, len);
        let end = end.clamp(start, len);
        {
            let mut b = self.buf.borrow_mut();
            let (s, e) = (start as usize, end as usize);
            b.splice(s..e, text.iter().copied());
        }
        let new_end = start + text.len() as i32;
        let delta = new_end - end; // = text.len() - (end - start)
        // 変更区間 [start,end) に「跨って」居るキャレットは start へ畳む防御枝。実際の合成経路
        // ではキャレットは常に comp.start(==start) か区間外なので、この中間枝は通らない（畳み先が
        // start でも new_end でも観測に影響しない）。先頭/末尾居座りの再現に必要なのは前2枝のみ。
        let adjust = |p: i32| if p <= start { p } else if p >= end { p + delta } else { start };
        self.acp_start.set(adjust(self.acp_start.get()));
        self.acp_end.set(adjust(self.acp_end.get()));
        self.sel_explicit.set(false);
        if let Some((cs, _)) = self.comp.get() {
            self.comp.set(Some((cs.min(start), new_end)));
        }
    }

    /// InsertTextAtSelection(NOQUERY) 経路。現在の選択 [start,end) を `text` で置換し、
    /// **キャレットを挿入末尾へ動かす**（挿入のセマンティクス。msctf は返り値 pacpend を末尾と
    /// みなす）。composition が無い劣化 commit 経路（選択位置への直接挿入）で使う。
    /// テキスト編集なので「明示 SetSelection」フラグはここで倒す。
    pub fn insert_at_selection(&self, start: i32, end: i32, text: &[u16]) {
        // pub API の防御的クランプ（slice() と同じ規約。範囲外/逆順でも panic させない）。
        let len = self.len();
        let start = start.clamp(0, len);
        let end = end.clamp(start, len);
        {
            let mut b = self.buf.borrow_mut();
            let (s, e) = (start as usize, end as usize);
            b.splice(s..e, text.iter().copied());
        }
        let new_end = start + text.len() as i32;
        self.acp_start.set(new_end);
        self.acp_end.set(new_end);
        self.sel_explicit.set(false);
        if let Some((cs, _)) = self.comp.get() {
            self.comp.set(Some((cs.min(start), new_end)));
        }
    }

    pub fn start_composition(&self) {
        let caret = self.acp_start.get();
        self.comp.set(Some((caret, caret)));
        self.sel_explicit.set(false);
    }
    /// 合成終了。TIP が（確定文字列を書いたあと）明示 SetSelection していなければ、
    /// 実アプリと同じくキャレットを合成開始位置（アンカー）へ戻す＝確定後カーソルが
    /// 先頭へ戻るバグの再現。明示 SetSelection 済みならその位置（通常は末尾）に留める。
    pub fn end_composition(&self) {
        if !self.sel_explicit.get() {
            if let Some((start, _)) = self.comp.get() {
                self.acp_start.set(start);
                self.acp_end.set(start);
            }
        }
        self.comp.set(None);
    }
    pub fn composing(&self) -> bool { self.comp.get().is_some() }

    pub fn full(&self) -> String { String::from_utf16_lossy(&self.buf.borrow()) }

    /// [start,end) を UTF-16 のコピーで返す（COM 書き出し用、borrow を跨がない）。
    pub fn slice(&self, start: i32, end: i32) -> Vec<u16> {
        let b = self.buf.borrow();
        let s = (start.max(0) as usize).min(b.len());
        let e = (end.max(start) as usize).min(b.len());
        b[s..e].to_vec()
    }

    pub fn preedit(&self) -> String {
        match self.comp.get() {
            Some((s, e)) if e > s => {
                String::from_utf16_lossy(&self.buf.borrow()[s as usize..e as usize])
            }
            _ => String::new(),
        }
    }

    pub fn committed(&self) -> String {
        let b = self.buf.borrow();
        match self.comp.get() {
            Some((s, e)) => {
                debug_assert!(s <= e);
                let mut v = b[..s as usize].to_vec();
                v.extend_from_slice(&b[e as usize..]);
                String::from_utf16_lossy(&v)
            }
            None => String::from_utf16_lossy(&b),
        }
    }

    pub fn reset(&self) {
        self.buf.borrow_mut().clear();
        self.acp_start.set(0);
        self.acp_end.set(0);
        self.comp.set(None);
        self.sel_explicit.set(false);
    }

    /// SP5 再変換テスト用: 文書へ「既に存在する確定済みテキスト」を直接据える（合成にしない）。
    /// 実アプリで半角英数モードのときユーザが直接打ち込んだラテン列を模す。キャレットは末尾へ。
    /// ロックモデルを介さないのは、これが TIP の編集ではなく「アプリの既存内容」のシードだから。
    pub fn seed_committed(&self, text: &[u16]) {
        let mut b = self.buf.borrow_mut();
        b.clear();
        b.extend_from_slice(text);
        let end = b.len() as i32;
        drop(b);
        self.acp_start.set(end);
        self.acp_end.set(end);
        self.comp.set(None);
        self.sel_explicit.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::DocState;

    fn u16s(s: &str) -> Vec<u16> { s.encode_utf16().collect() }

    #[test]
    fn set_text_does_not_move_caret() {
        let d = DocState::new();
        // 実 msctf 準拠: SetText はテキストを置換するだけでキャレット（選択）を動かさない。
        // キャレットが先頭(0)にあれば、SetText 後も先頭のまま。これがライブ変換中に
        // カーソルが先頭へ居座る挙動の正体で、TIP が明示 SetSelection して初めて末尾追従する。
        d.set_text(0, 0, &u16s("にほんご"));
        assert_eq!(d.full(), "にほんご");
        assert_eq!(d.selection(), (0, 0)); // SetText 単独ではキャレットは先頭のまま
    }

    #[test]
    fn preedit_committed_split_via_composition() {
        let d = DocState::new();
        d.start_composition();              // comp 開始 = 現在キャレット(0)
        d.set_text(0, 0, &u16s("にほんご")); // 合成中の SetText
        assert_eq!(d.preedit(), "にほんご");
        assert_eq!(d.committed(), "");
        assert_eq!(d.full(), "にほんご");

        // commit: 合成範囲を "日本語" に置換してから合成終了。
        d.set_text(0, 4, &u16s("日本語"));
        d.end_composition();
        assert_eq!(d.preedit(), "");
        assert_eq!(d.committed(), "日本語");
        assert_eq!(d.full(), "日本語");
    }

    #[test]
    fn commit_without_setselection_reverts_caret_to_anchor() {
        // 確定文字列を書いたあと TIP が SetSelection しない（旧バグ実装）と、
        // キャレットは合成開始位置（アンカー=0）へ戻る＝打ち始めの先頭。
        let d = DocState::new();
        d.start_composition();              // アンカー=0
        d.set_text(0, 0, &u16s("あ"));       // preedit。SetText はキャレットを動かさない→先頭0
        d.set_text(0, 1, &u16s("あ"));       // commit SetText。やはり先頭0のまま
        d.end_composition();                // 明示 SetSelection 無し → caret はアンカー0へ
        assert_eq!(d.selection(), (0, 0));
    }

    #[test]
    fn commit_with_setselection_keeps_caret_at_end() {
        // 確定後に TIP が SetSelection(末尾) すれば、キャレットは末尾に留まる（修正版）。
        let d = DocState::new();
        d.start_composition();
        d.set_text(0, 0, &u16s("あ"));
        d.set_text(0, 1, &u16s("あ"));       // commit SetText。SetText 単独では先頭0のまま
        d.set_selection(1, 1);              // TIP の明示 SetSelection(末尾)→ここで初めて末尾へ
        d.end_composition();                // 明示済み → アンカー戻し抑止
        assert_eq!(d.selection(), (1, 1));
    }

    #[test]
    fn two_words_stay_ordered_only_when_caret_kept_at_end() {
        // item11 の純データ版。1 語確定後にキャレットが末尾に無いと 2 語目が先頭へ入る。
        // 修正版（commit 後 set_selection）: あ→い の順で "あい"。
        let d = DocState::new();
        d.start_composition();
        d.set_text(0, 0, &u16s("あ"));
        d.set_text(0, 1, &u16s("あ"));
        d.set_selection(1, 1);              // 修正: 末尾へ
        d.end_composition();
        let (s, _) = d.selection();
        d.start_composition();              // 2 語目はキャレット位置から
        d.set_text(s, s, &u16s("い"));
        d.set_text(s, s + 1, &u16s("い"));
        d.set_selection(s + 1, s + 1);
        d.end_composition();
        assert_eq!(d.full(), "あい");

        // バグ版（commit 後 set_selection 無し）: 2 語目が先頭へ→"いあ"。
        let d = DocState::new();
        d.start_composition();
        d.set_text(0, 0, &u16s("あ"));
        d.set_text(0, 1, &u16s("あ"));
        d.end_composition();                // 末尾へ動かさない → caret はアンカー0
        let (s, _) = d.selection();
        assert_eq!(s, 0);
        d.start_composition();
        d.set_text(s, s, &u16s("い"));
        d.set_text(s, s + 1, &u16s("い"));
        d.end_composition();
        assert_eq!(d.full(), "いあ");
    }

    #[test]
    fn cancel_leaves_nothing() {
        let d = DocState::new();
        d.start_composition();
        d.set_text(0, 0, &u16s("にほんご"));
        d.set_text(0, 4, &[]);   // 取消: 合成範囲を空に
        d.end_composition();
        assert_eq!(d.full(), "");
        assert_eq!(d.committed(), "");
        assert_eq!(d.preedit(), "");
    }

    #[test]
    fn backspace_shrinks_preedit() {
        let d = DocState::new();
        d.start_composition();
        d.set_text(0, 0, &u16s("にほんご"));
        d.set_text(0, 4, &u16s("にほん")); // 読みが1つ縮む
        assert_eq!(d.preedit(), "にほん");
    }

    #[test]
    fn seed_committed_places_text_with_caret_at_end() {
        let d = DocState::new();
        d.seed_committed(&u16s("React nihongo"));
        assert_eq!(d.full(), "React nihongo");
        assert_eq!(d.committed(), "React nihongo"); // 合成中でない＝全部確定扱い
        assert_eq!(d.preedit(), "");
        assert_eq!(d.selection(), (13, 13)); // キャレットは末尾（ReconvertStart はここから後方を読む）
        assert!(!d.composing());
    }

    #[test]
    fn set_text_clamps_out_of_range_args() {
        // pub API はバッファ外/逆順の範囲でも panic せずクランプする（slice() と同じ防御規約）。
        let d = DocState::new();
        d.set_text(0, 0, &u16s("abc")); // "abc"
        d.set_text(1, 99, &u16s("X"));  // end>len → [1,3) を置換 → "aX"
        assert_eq!(d.full(), "aX");
        d.set_text(5, 2, &u16s("Z"));   // start>len かつ start>end → start=end=len → 末尾挿入 → "aXZ"
        assert_eq!(d.full(), "aXZ");
    }

    #[test]
    fn reset_clears_all() {
        let d = DocState::new();
        d.start_composition();
        d.set_text(0, 0, &u16s("あ"));
        d.reset();
        assert_eq!(d.full(), "");
        assert_eq!(d.preedit(), "");
        assert_eq!(d.selection(), (0, 0));
    }
}
