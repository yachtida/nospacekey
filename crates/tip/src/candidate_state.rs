//! SP6a: 候補リストの純データ。候補列・選択 index。COM 非依存・テスト対象。
//! presenter と UIElement が Rc<RefCell<CandidateState>> で共有する単一の真実源。

#[derive(Default)]
pub struct CandidateState {
    items: Vec<String>,
    selected: usize,
}

impl CandidateState {
    pub fn new() -> Self { Self::default() }

    /// 候補列と初期選択を設定。selected は範囲内へクランプ。
    pub fn set(&mut self, items: Vec<String>, selected: usize) {
        self.selected = if items.is_empty() { 0 } else { selected.min(items.len() - 1) };
        self.items = items;
    }

    pub fn count(&self) -> usize { self.items.len() }
    pub fn selected(&self) -> usize { self.selected }
    pub fn string_at(&self, i: usize) -> Option<String> { self.items.get(i).cloned() }
    pub fn items(&self) -> &[String] { &self.items }

    /// 循環選択（既存 CandidateWindow と同一挙動）。空なら no-op。
    pub fn move_selection(&mut self, delta: i32) {
        if self.items.is_empty() { return; }
        let n = self.items.len() as i32;
        self.selected = (self.selected as i32 + delta).rem_euclid(n) as usize;
    }

    /// ホスト発の直接選択（クランプ）。空なら no-op。
    pub fn set_selection(&mut self, index: usize) {
        if self.items.is_empty() { return; }
        self.selected = index.min(self.items.len() - 1);
    }

    /// 確定する候補の `(index, text)` を解決する。`sel` の候補が無ければ先頭(index 0)へフォールバックし、
    /// **index と text を必ず一致させて**返す（エンジンへ送る確定 index と実際に確定する文字列が
    /// ズレると、エンジンが別候補を prefixComplete して残り読みが壊れる）。空候補なら `None`。
    pub fn resolve_commit(&self, sel: usize) -> Option<(usize, String)> {
        if let Some(t) = self.string_at(sel) {
            Some((sel, t))
        } else {
            self.string_at(0).map(|t| (0, t))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn set_and_read() {
        let mut s = CandidateState::new();
        s.set(vec!["日本語".into(), "二本語".into(), "にほんご".into()], 0);
        assert_eq!(s.count(), 3);
        assert_eq!(s.selected(), 0);
        assert_eq!(s.string_at(1).as_deref(), Some("二本語"));
        assert_eq!(s.string_at(9), None);
    }
    #[test]
    fn move_selection_is_cyclic() {
        let mut s = CandidateState::new();
        s.set(vec!["a".into(), "b".into(), "c".into()], 0);
        s.move_selection(1); assert_eq!(s.selected(), 1);
        s.move_selection(-1); assert_eq!(s.selected(), 0);
        s.move_selection(-1); assert_eq!(s.selected(), 2); // 上端で末尾へ循環
        s.move_selection(1); assert_eq!(s.selected(), 0);  // 下端で先頭へ
    }
    #[test]
    fn set_selection_clamps() {
        let mut s = CandidateState::new();
        s.set(vec!["a".into(), "b".into()], 0);
        s.set_selection(5);
        assert_eq!(s.selected(), 1); // 範囲外はクランプ
    }
    #[test]
    fn empty_is_safe() {
        let mut s = CandidateState::new();
        s.set(vec![], 0);
        assert_eq!(s.count(), 0);
        assert_eq!(s.selected(), 0);
        s.move_selection(1); // パニックしない
        s.set_selection(3);  // パニックしない
        assert_eq!(s.string_at(0), None);
    }
    #[test]
    fn set_clamps_initial_selected() {
        let mut s = CandidateState::new();
        s.set(vec!["a".into(), "b".into()], 9);
        assert_eq!(s.selected(), 1); // 初期 selected も範囲内へクランプ
    }

    #[test]
    fn resolve_commit_returns_index_and_text() {
        let mut s = CandidateState::new();
        s.set(vec!["日本語".into(), "日本".into()], 1);
        assert_eq!(s.resolve_commit(1), Some((1, "日本".into())));
    }

    #[test]
    fn resolve_commit_falls_back_to_zero_in_lockstep() {
        // sel が範囲外 → index 0 と text(0) を**揃えて**返す（エンジンへ送る index と確定文字列のズレ防止）。
        let mut s = CandidateState::new();
        s.set(vec!["日本語".into(), "日本".into()], 0);
        assert_eq!(s.resolve_commit(99), Some((0, "日本語".into())));
    }

    #[test]
    fn resolve_commit_none_when_empty() {
        let s = CandidateState::new();
        assert_eq!(s.resolve_commit(0), None);
    }
}
