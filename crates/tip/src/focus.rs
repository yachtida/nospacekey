//! フォーカス変更（ウィンドウ/アプリ切替）時に、進行中の合成＋エンジンセッションを
//! 放棄リセットすべきかの純ロジック。
//!
//! 背景: 別ウィンドウ（多くは別プロセス）へフォーカスが移ると、ホストは live preedit を
//! 文書へ確定するが `ITfCompositionSink::OnCompositionTerminated` を**呼ばないことがある**。
//! すると TIP 側の合成状態とエンジンセッション（前の読み）が stale に居残り、ユーザが戻って
//! 打鍵すると古い読みへ連結される（例: にほんご のあと aiueo → にほんごあいうえお →
//! 日本語あいうえお。既に確定済みの 日本語 と合わさって 日本語日本語あいうえお）。
//! フォーカス変更を捕捉する sink（`ITfThreadMgrEventSink::OnSetFocus`）から、ここで判定して
//! 放棄リセットを焚く。COM の同一性比較は TextService 側で行い、その結果の bool をここへ渡す。

/// フォーカスが「自分の合成があるドキュメント以外」へ移ったとき、進行中状態を放棄すべきか。
///
/// * `has_active_input` … エンジンセッションまたは合成が生きている（放棄すべき状態がある）。
/// * `focus_is_our_doc` … 新しいフォーカス先が、自分の合成のあるドキュメントと同一。
///   （NULL フォーカス＝アプリがバックグラウンドへ、も「自分でない」として扱う＝false で渡す）
/// * `partial_committing` … 前方一致候補の部分確定の最中。保持しているエンジンセッションを
///   フォーカスの揺れで壊さないため、この間は放棄しない。
pub fn should_abandon_on_focus_change(
    has_active_input: bool,
    focus_is_our_doc: bool,
    partial_committing: bool,
) -> bool {
    has_active_input && !focus_is_our_doc && !partial_committing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandons_when_focus_leaves_our_doc_with_active_input() {
        // 別ウィンドウへフォーカスが移り、進行中の入力がある → 放棄リセットする。
        assert!(should_abandon_on_focus_change(true, false, false));
    }

    #[test]
    fn keeps_state_when_focus_returns_to_our_doc() {
        // 自分のドキュメントへ戻る/留まる → 何もしない（正常な合成を壊さない）。
        assert!(!should_abandon_on_focus_change(true, true, false));
    }

    #[test]
    fn no_abandon_when_idle() {
        // 放棄すべき進行中状態が無ければ、フォーカスがどこへ行っても何もしない。
        assert!(!should_abandon_on_focus_change(false, false, false));
        assert!(!should_abandon_on_focus_change(false, true, false));
    }

    #[test]
    fn never_abandon_during_partial_commit() {
        // 部分確定の保持セッションを、フォーカスの揺れで壊さない。
        assert!(!should_abandon_on_focus_change(true, false, true));
    }
}
