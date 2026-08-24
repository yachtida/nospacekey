//! ITfKeyEventSink 実装（打鍵フロー本体）。
//!
//! VK コード（`wparam.0 as u32`）を見て、A–Z / Space / Enter / 数字 / Esc / Backspace を
//! 処理する。実処理はすべて `TextService_Impl` のヘルパ（`run_preedit`/`do_commit`/…）と
//! エンジン IPC ヘルパ（`engine_insert`/`engine_convert`/…）に委譲する。
//!
//! 設計上の唯一の真実は `OnKeyDown`。`OnTestKeyDown` は `will_handle` で同じ述語を反復し、
//! 「このキーを実際に食うか」を返すだけにする。

use windows::core::{Ref, Result, BOOL, GUID};
use windows::Win32::Foundation::{FALSE, LPARAM, TRUE, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, GetKeyboardState, ToUnicode};
use windows::Win32::UI::TextServices::{ITfContext, ITfKeyEventSink_Impl};

use crate::candidate_window::{visible_range, CandidateUI, MAX_VISIBLE_ROWS};
use crate::input_state::{
    needs_session_reseed, plan_commit, plan_live_enter, should_widen_digits, to_hankaku_ascii,
    to_hankaku_kana, to_kana_reading_char, to_katakana, to_zenkaku_ascii, to_zenkaku_digits,
    CommitPlan, InsertStyle, LiveEnterPlan,
};
use crate::text_service::{
    tip_log, PendingEndKeySignature, PendingEndTestDecision, TextService_Impl,
};
use settings::symbol::zenkaku_symbol;

/// 仮想キーコード。
const VK_BACK: u32 = 0x08;
const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;
// 変換キー(0x1C)/Space(0x20) は will_handle 固定キーでなくなり Convert/Reconvert アクション経由で
// 食う（resolve_action が唯一の source）ため、VK 定数は撤去した（未使用 warning 回避）。
const VK_PRIOR: u32 = 0x21; // PageUp
const VK_NEXT: u32 = 0x22; // PageDown
const VK_END: u32 = 0x23;
const VK_HOME: u32 = 0x24;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_RIGHT: u32 = 0x27;
const VK_DOWN: u32 = 0x28;
const VK_DELETE: u32 = 0x2E;
const VK_1: u32 = 0x31;
const VK_9: u32 = 0x39;
const VK_A: u32 = 0x41;
const VK_Z: u32 = 0x5A;
const VK_OEM_102: u32 = 0xE2;
const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12; // Alt
                           // 確定取消（Ctrl+Backspace）: 「純粋な修飾キー単体」の判定に使う左右個別 VK 群
                           // （is_pure_modifier_vk 専用 — 0x10-0x12 は上の総称 VK と重複するので再掲しない）。
const VK_LSHIFT: u32 = 0xA0;
const VK_RSHIFT: u32 = 0xA1;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
const VK_LMENU: u32 = 0xA4;
const VK_RMENU: u32 = 0xA5;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;

/// COM 境界を越える panic は UB。COM の鍵イベント入口をこれで包み、panic を捕捉して
/// パススルー(FALSE)へ握り潰す（candidate_uielement の notify 保護と同じ思想。L-4）。
fn catch_com(site: &str, inner: impl FnOnce() -> Result<BOOL>) -> Result<BOOL> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(inner)) {
        Ok(r) => r,
        Err(_) => {
            tip_log(&format!("ev=panic site={site}"));
            Ok(FALSE)
        }
    }
}

/// テキスト入力になる VK（0/記号/テンキー）。A–Z と数字1–9は別扱いなので含めない。
/// composition 中はこれらを食ってエンジンへ送る（記号/数字を変換に含める）。
fn is_text_vk(vk: u32) -> bool {
    matches!(vk,
        0x30 |          // '0'（1–9 は VK_1..=VK_9 で個別処理）
        0x60..=0x69 |   // テンキー 0–9
        0x6A..=0x6F |   // テンキー * + , - . /
        0xBA..=0xC0 |   // OEM_1..OEM_3: ; = , - . / `（レイアウト依存）
        0xDB..=0xDF |   // OEM_4..OEM_7 + OEM_8: [ \ ] ' 等
        VK_OEM_102      // VK_OEM_102: < > |（レイアウト依存）
    )
}

/// OEM 記号 VK。native モードでは idle でも食い、全角へ写して composition を開始する
/// （2026-08-03 — 旧・打鍵作法 Task3 の直接確定を廃止。eaten 契約は不変）。
/// 数字/テンキー（0x30/0x60-0x6F）は**含めない** — テンキーの `.` や数字は idle 通常入力のまま
/// （zenkaku_symbol も英数字を対象外にするのと対）。`is_text_vk` の OEM 部分集合。
fn is_oem_symbol_vk(vk: u32) -> bool {
    matches!(vk,
        0xBA..=0xC0 |   // OEM_1..OEM_3: ; = , - . / `（レイアウト依存）
        0xDB..=0xDF |   // OEM_4..OEM_7 + OEM_8: [ \ ] ' 等
        VK_OEM_102      // VK_OEM_102: < > |（レイアウト依存）
    )
}

/// メイン行 0-9 のみ。`is_digit_vk` と違いテンキー(0x60-0x69)を含めない —
/// Shift+テンキーは記号を生まない(NumLock 系で別 VK になる)ため。
fn is_main_row_digit_vk(vk: u32) -> bool {
    matches!(vk, 0x30..=0x39)
}

/// 「記号打鍵」の単一の真実源。gated と OnKeyDown の両方がこれを見る(eaten 一致 = item19)。
/// OEM キーはトグル非依存(表外も食い切って ASCII 確定する従来契約)。数字行は
/// 「Shift かつ 記号 overlay ON」のときだけ — OFF を含めないのは既定 OFF で現行経路と
/// 完全同一(新規に食う打鍵ゼロ)を保証するため(2026-07-16 spec §3)。
/// `symbol_overlay` は素のトグルでなく overlay 実効値。集合の個別内容は引かない —
/// ここは VK しか受けず ToUnicode を呼べないため文字を知り得ない(2026-08-02 spec §4)。
fn is_symbol_keystroke(vk: u32, shift: bool, symbol_overlay: bool) -> bool {
    is_oem_symbol_vk(vk) || (symbol_overlay && shift && is_main_row_digit_vk(vk))
}

/// 数字 VK（メイン行 0-9 とテンキー 0-9）。かなモードでは idle でも食って composition を開始する
/// （②数字を読みへ）。テンキー演算子（0x6A-0x6F）は含めない。
fn is_digit_vk(vk: u32) -> bool {
    matches!(vk, 0x30..=0x39 | 0x60..=0x69)
}

/// Shift が押されているか（数字キーの「候補選択 vs 記号入力」判定用）。
fn shift_down() -> bool {
    unsafe { (GetKeyState(VK_SHIFT) as u16 & 0x8000) != 0 }
}

/// A–Z 打鍵の経路（打鍵作法 Task5 改 + Shift英語モード）。
/// `DirectCommit`=一時直接入力（shift_latin=commit: 素の大文字 ASCII を composition を張らず
/// 直接確定 — Google/ATOK の「Shift で一時的に直接入力」）、`Latin`=英語未確定モード
/// （shift_latin=compose: エンジン読みへ direct 挿入 — MS-IME の「確定まで英字が続く」）、
/// `Kana`=従来のかな合成（小文字ローマ字をエンジンへ）。
#[derive(Debug, PartialEq)]
pub enum AzRoute {
    DirectCommit(char),
    Latin(char),
    Kana(char),
}

/// A–Z 打鍵の経路決定の純関数（COM 非依存 — testbench に Shift 注入が無いため単体テストで担保）。
/// `key_char` は key_to_char（ToUnicode）の結果を外から渡す。
/// - compose（shift_latin=compose）: Shift 押下で英語モード開始、latin_mode 中は無修飾でも
///   英語継続（Shift なし=小文字。CapsLock 等は key_char=ToUnicode の実文字が真実）。
/// - 非 compose（shift_latin=commit）+ Shift: 一時直接入力へ。実文字（大文字。ToUnicode が
///   取れなければ大文字化）を直接確定する。idle でも composition 中でも同じ
///   （呼び出し側が開いている合成を先に settle して畳む）。
/// - 無修飾かつ非 latin_mode: 従来のかな経路（AltGr 等の非英字レイアウト文字は尊重、
///   素の A–Z は小文字へ正規化。CapsLock で大文字が来ても shift 無しなら小文字正規化）。
pub fn resolve_az_char(
    vk: u32,
    shift: bool,
    key_char: Option<char>,
    compose: bool,
    latin_mode: bool,
) -> AzRoute {
    let lower = (b'a' + (vk - VK_A) as u8) as char;
    if compose && (shift || latin_mode) {
        return AzRoute::Latin(key_char.unwrap_or(if shift {
            lower.to_ascii_uppercase()
        } else {
            lower
        }));
    }
    if shift {
        return AzRoute::DirectCommit(key_char.unwrap_or(lower.to_ascii_uppercase()));
    }
    match key_char {
        Some(c) if !c.is_ascii_alphabetic() => AzRoute::Kana(c),
        _ => AzRoute::Kana(lower),
    }
}

/// Ctrl/Alt の押下から「コマンド修飾（＝アプリのアクセラレータ）か」を判定する純粋関数。
/// Ctrl か Alt の **一方だけ** なら Ctrl+C/V/X/A/Z… や Alt+メニュー等のショートカットなので、
/// IME は食わずアプリへ通す。両方押下（AltGr = Ctrl+Alt）と無修飾は通常の文字入力として扱う。
fn is_cmd_modifier(ctrl: bool, alt: bool) -> bool {
    ctrl != alt
}

/// 現在のキーボード状態から `is_cmd_modifier` を評価する（GetKeyState を読む impure ラッパ）。
fn cmd_modifier_down() -> bool {
    let ctrl = unsafe { (GetKeyState(VK_CONTROL) as u16 & 0x8000) != 0 };
    let alt = unsafe { (GetKeyState(VK_MENU) as u16 & 0x8000) != 0 };
    is_cmd_modifier(ctrl, alt)
}

/// 現在の Ctrl/Shift/Alt 押下状態（resolve_action への入力）。
fn mods_now() -> (bool, bool, bool) {
    unsafe {
        (
            (GetKeyState(VK_CONTROL) as u16 & 0x8000) != 0,
            (GetKeyState(VK_SHIFT) as u16 & 0x8000) != 0,
            (GetKeyState(VK_MENU) as u16 & 0x8000) != 0,
        )
    }
}

/// TestKeyDown/KeyDown の physical-pair 署名に使う修飾キー mask。両入口で同じ OS 状態を
/// 読むが、署名の一部として保存して同じ VK の別打鍵を予約 replay しない。
pub(crate) fn current_modifier_mask() -> u8 {
    let (ctrl, shift, alt) = mods_now();
    (ctrl as u8) | ((shift as u8) << 1) | ((alt as u8) << 2)
}

/// 確定取消（Ctrl+Backspace）: この `source`（remember_last_commit/commit_and_reset に渡る
/// ev=commit の source ラベル）の確定が undo_armed を武装してよいかを判定する純粋関数。
/// 対象は「読みを使い切って composition を畳んだ全確定」の candidate/live/clause のみ
/// （設計ロック⑤）。candidate_prefix/live_prefix は composition が残るためガードで自然に除外され、
/// live_auto は全消費時のみ commit_and_reset を通るが「iOS 由来の自動確定」を undo 対象にしない
/// 方針（M-2）で source ゲート側から明示的に除外する。mode_toggle/navigate（settle 系）は
/// 対象外（C-1: settle は候補確定を経由すると source="candidate"/"clause" になるため、armed を
/// 残さないよう settle_active_input/on_preserved_key_impl 側で disarm_undo を必ず呼ぶ）。
/// clause（文節ナビゲーションの Enter 確定）は candidate と同じ「明示選択の全確定」なので武装する。
pub fn arms_undo(source: &str) -> bool {
    matches!(source, "candidate" | "live" | "clause")
}

/// 巡13(round13): 空 text の確定（空 BS cancel 拒否巻き戻し中の Enter=cancel 代わり）は
/// 確定書類（remember_last_commit / undo 武装）を残さない — ゲートの純関数化（単体テスト固定）。
/// cancel 成功経路も書類を残さず対称。
pub fn commit_keeps_records(text: &str) -> bool {
    !text.is_empty()
}

/// A successful SetText may leave the TSF composition open even after the
/// logical input state has been reset.  Every path that must settle before a
/// mode boundary or direct insertion has to count that close-only retry as
/// active input too.
fn input_needs_settle(composing: bool, showing: bool, end_pending: bool) -> bool {
    composing || showing || end_pending
}

/// 確定取消の armed 状態機械: このキーが「純粋な修飾キー単体」（Shift/Ctrl/Alt/Win の
/// 左右いずれか）かを判定する純粋関数。Ctrl+Backspace は打鍵として Ctrl 押下→Backspace 押下の
/// 順で到達するため、Ctrl 単体の押下（＝OnKeyDown への到達）で armed を解除してしまうと
/// Ctrl+Backspace 自体が成立しなくなる。これらのキーの単体到達だけは disarm の対象外とする。
pub fn is_pure_modifier_vk(vk: u32) -> bool {
    matches!(vk as i32, VK_SHIFT | VK_CONTROL | VK_MENU)
        || matches!(
            vk,
            VK_LSHIFT
                | VK_RSHIFT
                | VK_LCONTROL
                | VK_RCONTROL
                | VK_LMENU
                | VK_RMENU
                | VK_LWIN
                | VK_RWIN
        )
}

/// 押された物理キー＋現在のキーボード状態(Shift等)＋レイアウトから入力文字を求める。
/// 印字可能な1文字なら Some、制御文字/デッドキー/合字なら None。
/// 打鍵が自動リピート(押しっぱなし)か — lparam bit30。文節モード中の候補/文節操作
/// (←/→/↑/↓/Space/Tab/数字)は OnKeyDown 内の同期 IPC を伴うため、リピート連打はキー間隔より
/// 遅い応答待ちの積み上げ＝IME ブロックとタイムアウト→接続断に直結する。リピートは IPC を
/// 発行せず食い切り、離して押し直せば bit30=0 で通る(敵対レビュー③の一般化)。
fn is_autorepeat(lparam: LPARAM) -> bool {
    (lparam.0 & (1 << 30)) != 0
}

fn key_to_char(vk: u32, lparam: LPARAM) -> Option<char> {
    let scancode = ((lparam.0 >> 16) & 0xFF) as u32;
    let mut state = [0u8; 256];
    unsafe {
        if GetKeyboardState(&mut state).is_err() {
            return None;
        }
    }
    let mut buf = [0u16; 8];
    // wFlags の bit2(=4) は「キーボード状態を変更しない」(Win10 1607+)。0 を渡すと
    // 文字を覗き見るだけのこの呼び出しがカーネルのデッドキー状態を消費してしまい、
    // ホスト/他アプリの合字（^+e=ê 等）が壊れる。覗き見用途なので必ず立てる。
    const TU_NO_KEYSTATE_CHANGE: u32 = 1 << 2;
    let n = unsafe { ToUnicode(vk, scancode, Some(&state), &mut buf, TU_NO_KEYSTATE_CHANGE) };
    if n == 1 {
        let ch = char::from_u32(buf[0] as u32)?;
        if ch.is_control() {
            None
        } else {
            Some(ch)
        }
    } else {
        None
    }
}

/// 現在の状態でこの VK を IME が食う（TRUE を返す）かどうかの述語。
/// `direct`=true は半角英数(直接入力)モード（SP5 boiled-egg）。`OnTestKeyDown`/`OnKeyDown` で共有。
pub fn will_handle(
    vk: u32,
    composing: bool,
    showing: bool,
    cmd_modifier: bool,
    direct: bool,
) -> bool {
    // Ctrl/Alt 併用キー（Ctrl+C/V/X/A/Z…, Alt+メニュー）はアプリのアクセラレータ。
    // どの VK でも食わずアプリへ通す。AltGr(=Ctrl+Alt 同時)・無修飾は通常入力として下の match へ。
    if cmd_modifier {
        return false;
    }
    // 半角英数(直接入力): ラテンは本文へ流す＝食わない（A–Z/記号/数字をパススルー）。
    // 再変換 composition の候補表示中(showing)だけ候補キーを食う。トグルキー(0x1D)は
    // PreserveKey(OnPreservedKey)で処理するが、再変換キー(0x1C)は PreserveKey 登録が OS に
    // 拒否される（ev=preservekey reconvert_ok=false）ため通常キー経路(OnKeyDown)で食う（SP5 D2/D5）。
    if direct {
        return match vk {
            // Space/変換 の食う判定は resolve_action(Convert/Reconvert)へ一本化した（ここには無い）。
            VK_BACK => showing,
            VK_RETURN | VK_ESCAPE | VK_UP | VK_DOWN => showing,
            // UU-6: Home/End/PageUp/PageDown/Delete も候補表示中は食う（選択候補を確定して
            // 畳む）。非表示 direct では本文操作なので食わない（下の showing 判定で false）。
            VK_HOME | VK_END | VK_PRIOR | VK_NEXT | VK_DELETE => showing,
            VK_1..=VK_9 => showing,
            _ => false,
        };
    }
    match vk {
        // A–Z は常に食う（composition を開始/継続する）。
        VK_A..=VK_Z => true,
        // それ以外は composition 中か候補表示中のときだけ食う。Space/変換 は resolve_action
        // (Convert)へ移したのでここには無い（Convert 束縛の無効化・リバインドを効かせるため）。
        VK_RETURN | VK_ESCAPE | VK_BACK => composing || showing,
        VK_1..=VK_9 => composing || showing,
        // ↑/↓ は候補ウィンドウが出ているときだけ食う（選択移動）。
        // それ以外（idle / composition のみ）は素通しし、アプリのキャレット移動を邪魔しない。
        VK_UP | VK_DOWN => showing,
        // UU-6: Home/End/PageUp/PageDown/Delete は composition 中/候補表示中だけ食う。
        // 未処理だと合成中にキャレットだけ移動し preedit が別位置へ取り残される（合成崩れ）。
        // 一般的な日本語IMEに倣い、食って開いている入力を確定（settle）してから畳む。
        // idle では素通し（本文のキャレット移動/前方削除はアプリに任せる）。
        // ←→ も食う条件は同じ（composing||showing）だが処理が違う: 候補表示中は文節移動
        // （MS-IME の文節ナビゲーション。劣化時は settle）、composition のみは「確定して畳む」
        // （打鍵作法 Task2 — InputState にカーソル概念が無く読み内移動は実装できないため）。
        VK_HOME | VK_END | VK_PRIOR | VK_NEXT | VK_DELETE | VK_LEFT | VK_RIGHT => {
            composing || showing
        }
        // OEM 記号: idle でも食う（全角化して composition を開始する — 2026-08-03、旧 Task3 の
        // 直接確定を廃止）。composition 中は従来どおり食ってエンジンへ送る（このアームは常に
        // true なので包含）。VK 単位で宣言する理由: will_handle は COM 非依存純関数で ToUnicode を
        // 呼べないため、文字（zenkaku_symbol の表の有無）では判定できない。表に無い文字も
        // OnKeyDown 側が食い切って読みへ積み、Test/実の eaten を一致させる
        // （is_text_vk アームより前に置くこと）。
        vk if is_oem_symbol_vk(vk) => true,
        // 0/記号/テンキー: composition 中だけ食ってエンジンへ送る（idle は素通し）。
        vk if is_text_vk(vk) => composing,
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackspaceRoute {
    /// 再変換/確定取消の候補を元本文へ復元して閉じる。
    CancelReconvert,
    /// 候補窓を閉じるだけの防御的な退避。本文には触れない。
    HideStaleCandidates,
    /// 通常の composing Backspace 経路。
    Compose,
    /// idle の本文 Backspace へ渡す。
    Pass,
}

/// Backspace の実処理経路。再変換と確定取消は共有尾部で
/// `showing=true/reconverting=true` になる一方、InputState は composing にならない。
/// `showing && !composing` の非再変換状態も、候補を残したまま本文へ流さず閉じる。
fn backspace_route(composing: bool, showing: bool, reconverting: bool) -> BackspaceRoute {
    if showing && !composing {
        if reconverting {
            BackspaceRoute::CancelReconvert
        } else {
            BackspaceRoute::HideStaleCandidates
        }
    } else if composing {
        BackspaceRoute::Compose
    } else {
        BackspaceRoute::Pass
    }
}

/// ephemeral かつ idle で「かなモードが食わない＝素通しするキー」なら ephemeral を抜けて direct へ
/// 戻す（=true）。ephemeral は常に native なので `direct=false` 固定で評価する。
/// Why not(素の `will_handle` を使う): `will_handle_gated` だけが持つ overlay ——「かなモードは idle の
/// 無修飾数字も食って composition を開始する」(②) と記号トグル ON の Shift+数字行 —— を見落とし、
/// **かなが食うキーで ephemeral を抜ける**。数字始まりで実害が出た: abort で compartment が direct
/// へ落ちた直後、同じ打鍵がかな数字アームまで進んで composition を開き、以後 direct 枝（composing を
/// 見ない）が全キーを素通しさせて preedit へ届かなくなる（モード切替でしか畳めない）。
/// Why not(`action == None` を併記する): gated は先頭で `action != None` を無条件に食うと宣言する
/// ので、その否定に既に含まれる。Ctrl+BS(CommitUndo) が will_handle の Ctrl ゲートで「素通しキー」と
/// 誤分類され dispatch 前に abort される穴も、gated 経由なら carve-out がそのまま効く。
#[allow(clippy::too_many_arguments)]
pub fn ephemeral_idle_abort(
    vk: u32,
    cmd_modifier: bool,
    shift: bool,
    symbol_overlay: bool,
    ephemeral: bool,
    composing: bool,
    showing: bool,
    action: crate::keymap::KeyAction,
) -> bool {
    ephemeral
        && !composing
        && !showing
        && !will_handle_gated(
            vk,
            composing,
            showing,
            cmd_modifier,
            false,
            shift,
            symbol_overlay,
            action,
        )
}

/// 設定 settings.shift_latin.mode → compose か。"commit" 以外は compose 扱い —
/// 手編集 JSON の未知値で黙って直接確定(旧挙動)へ倒すより、既定挙動へ倒すほうが驚きが小さい。
pub fn shift_latin_is_compose(mode: &str) -> bool {
    mode != "commit"
}

/// `will_handle`（固定キーの真実）に keymap 由来の action（コマンド機能）と、記号設定由来の
/// Shift+数字行 overlay（symbol_overlay）を重ねた最終述語。action はどれも「その機能の文脈
/// ゲート＋feature flag＋チョード一致」を resolve_action が織り込み済みなので、ここでは正の
/// carve-out として食うだけでよい。
/// OnTestKeyDown / OnKeyDown の両入口はこの単一関数を共有する（＝「食うか」の唯一の真実）。
#[allow(clippy::too_many_arguments)]
pub fn will_handle_gated(
    vk: u32,
    composing: bool,
    showing: bool,
    cmd_modifier: bool,
    direct: bool,
    shift: bool,
    symbol_overlay: bool,
    action: crate::keymap::KeyAction,
) -> bool {
    // keymap の action は cmd_modifier 早期 return より前の carve-out（Ctrl 併用チョードが
    // cmd_modifier ゲートに殺されないため — 旧 undo/ephemeral carve-out の一般化）。
    if action != crate::keymap::KeyAction::None {
        return true;
    }
    // 記号 overlay ON のときだけ Shift+数字行を記号として食う(読みへ畳み込み。idle=合成開始)。
    // 下の showing 取消より前に置く理由: 目的が候補選択でなく記号入力で、OEM 記号は今日
    // すでに showing 中も食って確定している(そのパターンへ合流)。OFF なら不発=現行同一経路
    //（2026-07-16 spec §3a。旧 !/@ が US 配列で表に到達できなかった穴を塞ぐゲート側の半分）。
    // 集合の個別内容は見ない — 全解除は overlay=false へ縮退してトグル OFF と完全同一になる
    //（部分集合時に集合外記号を「食って半角のまま確定」する差は既知の限界。2026-08-02 spec §4）。
    if !direct && !cmd_modifier && symbol_overlay && shift && is_main_row_digit_vk(vk) {
        return true;
    }
    // Minor 2: 数字キーの候補選択は Shift 無しのときだけ（実処理 OnKeyDown の
    // VK_1..=VK_9 アームが `!shift_down()` を要求するのに合わせる）。composition 中は
    // Shift 有無に依らず記号としてエンジンへ送る＝食う。候補表示のみ(showing && !composing)で
    // Shift+数字を押した場合だけ「食わない」＝記号として本文へ流す。will_handle は Shift を
    // 見ないので、このケースだけ gate 側で eat を取り消して Test/実の判定を一致させる。
    if (VK_1..=VK_9).contains(&vk) && shift && showing && !composing {
        return false;
    }
    // ②: かなモードでは無修飾の数字キーを idle でも食って composition を開始する（従来どおり）。
    if !direct && !cmd_modifier && !shift && is_digit_vk(vk) {
        return true;
    }
    will_handle(vk, composing, showing, cmd_modifier, direct)
}

/// Bug 3: LLM 変換待機(AwaitingLlm)まで含めた最終 eaten 判定。`OnTestKeyDown` はこれで
/// 実処理 `on_key_down_impl` と一致させる（Bug A の鏡像＝Test/実の eaten 判定を揃える）。
/// 実処理の優先順位を保つ: cmd 修飾は最優先でパススルー（待機より先）、待機中は cmd 修飾
/// 以外の全キーを食って無視（preedit ロック）、それ以外は通常の gate。
// 引数は「vk＋4 文脈＋shift＋awaiting＋symbol_overlay＋action」で意味のある最小集合（action へ
// 集約済み。symbol_overlay は記号設定の Shift+数字行 overlay 用でキーマップ非依存）。
#[allow(clippy::too_many_arguments)]
pub fn will_handle_awaiting(
    vk: u32,
    composing: bool,
    showing: bool,
    cmd_modifier: bool,
    direct: bool,
    shift: bool,
    awaiting_llm: bool,
    symbol_overlay: bool,
    action: crate::keymap::KeyAction,
) -> bool {
    // keymap action の carve-out は cmd_modifier 早期 return より**前**（I-1 と同じ理由の一般化:
    // Ctrl 併用チョードは cmd_modifier=true として届くため、後段では殺されて Test/実が食い違う）。
    if action != crate::keymap::KeyAction::None {
        return true;
    }
    if cmd_modifier {
        return false;
    }
    if awaiting_llm {
        return true;
    }
    will_handle_gated(
        vk,
        composing,
        showing,
        cmd_modifier,
        direct,
        shift,
        symbol_overlay,
        action,
    )
}

/// Bug 4: 候補窓の可視ページ内で数字キー(0 始まりの `digit`)が指す **絶対** index を返す。
/// ページ先頭は `visible_range` の start（＝2ページ目以降はオフセットが乗る）。可視ページの
/// 行数を超える数字は `None`（no-op＝誤った候補を選ばない）。`count == 0` も `None`。
fn page_candidate_index(selected: usize, count: usize, digit: usize) -> Option<usize> {
    let (start, end) = visible_range(selected, count, MAX_VISIBLE_ROWS);
    let abs = start + digit;
    if abs < end {
        Some(abs)
    } else {
        None
    }
}

/// `ev=commit` の構造化フィールド（sel/cand_n/rlen/tlen/mode）を組み立てる純関数（品質ループ②）。
/// - `sel`: 確定した候補の**絶対** index（候補確定時のみ `Some` — 数字キー確定はページ補正済みの
///   実確定 index）。ライブ確定・直接確定は `None` → `-1` を出す。
/// - `cand_n`: 確定時点の候補総数（候補確定時のみ非0。ライブ確定は 0）。
/// - `reading`: 読み（かな）。`text`: 確定文字列。長さはバイトでなく **chars 数**。
/// - `direct`: 半角英数(直接入力)モードか（`is_direct_mode()` を渡す）。
pub fn commit_fields(
    sel: Option<usize>,
    cand_n: usize,
    reading: &str,
    text: &str,
    direct: bool,
) -> String {
    let sel_i: i64 = sel.map(|s| s as i64).unwrap_or(-1);
    format!(
        "sel={sel_i} cand_n={cand_n} rlen={} tlen={} mode={}",
        reading.chars().count(),
        text.chars().count(),
        if direct { "direct" } else { "native" }
    )
}

/// preserved key の GUID を「どのアクションか」へ分類する純関数（COM 不要でテスト可能）。
/// JIS キー（無変換/変換）と US キー（Alt+`/Alt+/）の両 GUID を同一アクションへ束ねる。
#[derive(PartialEq, Debug)]
pub enum PreservedAction {
    ToggleMode,
    Reconvert,
    /// 品質ループ③: 誤変換ワンキー記録（Ctrl+変換 / Ctrl+/ → feedback.jsonl）。
    Feedback,
    None,
}

pub fn classify_preserved_key(guid: &GUID) -> PreservedAction {
    use crate::globals::{
        GUID_PRESERVEDKEY_FEEDBACK, GUID_PRESERVEDKEY_FEEDBACK_US, GUID_PRESERVEDKEY_MODE_TOGGLE,
        GUID_PRESERVEDKEY_MODE_TOGGLE_HZ, GUID_PRESERVEDKEY_MODE_TOGGLE_US,
        GUID_PRESERVEDKEY_RECONVERT, GUID_PRESERVEDKEY_RECONVERT_US,
    };
    if *guid == GUID_PRESERVEDKEY_MODE_TOGGLE
        || *guid == GUID_PRESERVEDKEY_MODE_TOGGLE_US
        || *guid == GUID_PRESERVEDKEY_MODE_TOGGLE_HZ
    {
        PreservedAction::ToggleMode
    } else if *guid == GUID_PRESERVEDKEY_RECONVERT || *guid == GUID_PRESERVEDKEY_RECONVERT_US {
        PreservedAction::Reconvert
    } else if *guid == GUID_PRESERVEDKEY_FEEDBACK || *guid == GUID_PRESERVEDKEY_FEEDBACK_US {
        PreservedAction::Feedback
    } else {
        PreservedAction::None
    }
}

impl ITfKeyEventSink_Impl for TextService_Impl {
    fn OnSetFocus(&self, _fforeground: BOOL) -> Result<()> {
        Ok(())
    }

    fn OnTestKeyDown(
        &self,
        _pic: Ref<'_, ITfContext>,
        wparam: WPARAM,
        _lparam: LPARAM,
    ) -> Result<BOOL> {
        catch_com("OnTestKeyDown", || {
            self.on_test_key_down_impl(_pic, wparam, _lparam)
        })
    }

    fn OnKeyDown(&self, pic: Ref<'_, ITfContext>, wparam: WPARAM, lparam: LPARAM) -> Result<BOOL> {
        // UU-4: guarded で包み、show()/move_selection() 等が presenter 経由でホストへ同期
        // コールアウト中にホストが Behavior で再入しても、借用衝突 panic ではなく保留→安全点
        // flush で処理させる（確定ロスト防止）。
        catch_com("OnKeyDown", || {
            self.guarded(|| self.on_key_down_impl(pic, wparam, lparam))
        })
    }

    fn OnTestKeyUp(
        &self,
        _pic: Ref<'_, ITfContext>,
        _wparam: WPARAM,
        _lparam: LPARAM,
    ) -> Result<BOOL> {
        Ok(FALSE)
    }

    fn OnKeyUp(&self, _pic: Ref<'_, ITfContext>, _wparam: WPARAM, _lparam: LPARAM) -> Result<BOOL> {
        Ok(FALSE)
    }

    fn OnPreservedKey(&self, pic: Ref<'_, ITfContext>, rguid: *const GUID) -> Result<BOOL> {
        // UU-4: settle→commit→候補 show でホスト再入しうるので guarded で包む（OnKeyDown と同様）。
        catch_com("OnPreservedKey", || {
            self.guarded(|| self.on_preserved_key_impl(pic, rguid))
        })
    }
}

// ---- COM 鍵イベント入口の実体（catch_com で包んで panic を FFI 越えさせない。L-4）----
impl TextService_Impl {
    /// キーイベント両入口（OnTestKeyDown/OnKeyDown）共通の役割解決。状態読みだけで副作用なし。
    fn resolve_action_now(&self, vk: u32) -> crate::keymap::KeyAction {
        let (ctrl, shift, alt) = mods_now();
        let composing = self.state.borrow().composing;
        let ai = crate::keymap::ActionInput {
            vk,
            ctrl,
            shift,
            alt,
            composing,
            showing: self.showing.get(),
            direct: self.is_direct_mode(),
            undo_armed: self.undo_armed.get(),
            ephemeral_enabled: self.ephemeral_enabled.get(),
            typo_enabled: self.typo_enabled.get(),
            llm_enabled: self.llm_enabled.get(),
        };
        crate::keymap::resolve_action(&self.keymap.get(), &ai)
    }

    fn on_test_key_down_impl(
        &self,
        pic: Ref<'_, ITfContext>,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Result<BOOL> {
        // A new StartComposition may have re-entered this key path through a COM callout before
        // its caller returned.  Consume that shared one-shot before password/reservation gates so
        // an old pending-end pair cannot be taken by the reentrant key.
        self.consume_started_composition();
        let raw_vk = wparam.0 as u32;
        let vk = crate::keymap::normalize_vk(raw_vk);
        // Context identity/password gate must precede reservation.  A failed/secret context
        // invalidates any old slot so a later same-VK event cannot replay it.
        let ctx: ITfContext = match pic.ok() {
            Ok(c) => c.clone(),
            Err(_) => {
                self.invalidate_pending_end_test_reservation();
                return Ok(FALSE);
            }
        };
        if self.is_password_context(&ctx) {
            self.invalidate_pending_end_test_reservation();
            return Ok(FALSE);
        }
        let signature = match PendingEndKeySignature::from_context(
            &ctx,
            raw_vk,
            vk,
            lparam.0,
            current_modifier_mask(),
        ) {
            Some(signature) => signature,
            None => {
                self.invalidate_pending_end_test_reservation();
                return Ok(FALSE);
            }
        };
        // Test→Key の間に callback/timer が close を完了しても、正確な physical pair だけを
        // 一度維持する。occupied slot への同じ/別 Test は Busy(FALSE) とし、通常述語へ落とさない。
        match self.pending_end_test_decision(signature) {
            PendingEndTestDecision::Reserve => return Ok(TRUE),
            PendingEndTestDecision::Busy => return Ok(FALSE),
            PendingEndTestDecision::Normal => {}
        }
        // keymap 役割解決は password gate の直後・disarm 判定より前に 1 回計算し、両入口で共有する。
        let action = self.resolve_action_now(vk);
        // 確定取消（Ctrl+Backspace）: M-5 — 純粋修飾キー単体でも CommitUndo でもない打鍵は
        // 投機的に非武装化する。素通しキー（direct の A–Z、idle の矢印等）は行儀よいホストでは
        // OnKeyDown が呼ばれない（TestKeyDown=FALSE で終わる）ため、ここで検知しないと
        // 「確定後に打鍵した」が OnKeyDown だけでは漏れる。早めの非武装化は安全側（undo が
        // 効かなくなるだけ）。
        if !is_pure_modifier_vk(vk) && action != crate::keymap::KeyAction::CommitUndo {
            self.disarm_undo();
        }
        let composing = self.state.borrow().composing;
        let showing = self.showing.get();
        // ephemeral かな: idle で「かなが食わない（素通しする）キー」が来たら、food 判定より
        // 前に direct へ復帰しておく（投機的 exit。OnKeyDown を呼ばない行儀よいホストでもここで
        // 検知しないと、TestKeyDown=FALSE のまま押し忘れの言語モード居残りになる。exit は冪等）。
        if ephemeral_idle_abort(
            vk,
            cmd_modifier_down(),
            shift_down(),
            self.symbol_overlay.get(),
            self.ephemeral_kana.get(),
            composing,
            showing,
            action,
        ) {
            self.exit_ephemeral_to_direct(Some(&ctx));
        }
        // direct は exit の**後**に読む。exit は compartment を direct へ倒すので、先に読むと
        // 「もう native ではない」のに native として食う判定を出し、実処理と eaten が食い違う。
        let direct = self.is_direct_mode();
        // Bug 3: 待機ロック(AwaitingLlm)まで含めて実処理と同じ述語で判定する。TestKeyDown を
        // 先に呼ぶ行儀よいホストでも、待機中は実処理が全キーを食うのと eaten 判定を一致させる。
        let handled = will_handle_awaiting(
            vk,
            composing,
            showing,
            cmd_modifier_down(),
            direct,
            shift_down(),
            self.state.borrow().awaiting_llm(),
            self.symbol_overlay.get(),
            action,
        );
        // 診断: A–Z 打鍵時に is_direct が実際に何を読むかを残す（toggle で direct にしたのに
        // 入力がひらがなになる件の切り分け。direct=false なら compartment が direct を保持していない）。
        if (VK_A..=VK_Z).contains(&vk) {
            tip_log(&format!(
                "ev=keytest vk={vk:#04x} direct={direct} composing={composing} showing={showing} handled={handled}"
            ));
        }
        Ok(if handled { TRUE } else { FALSE })
    }

    fn on_key_down_impl(
        &self,
        pic: Ref<'_, ITfContext>,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Result<BOOL> {
        // Keep the same preflight ordering as OnTestKeyDown.  This must precede context/password
        // checks and reservation consumption because this entry can be delivered without Test.
        self.consume_started_composition();
        let raw_vk = wparam.0 as u32;
        let vk = crate::keymap::normalize_vk(raw_vk);
        // 半角/全角の実配送 VK/修飾の実機診断(spec §3)。正規化前の生値を残す。
        if matches!(raw_vk, 0x19 | 0xF3 | 0xF4) {
            let (c, s, a) = mods_now();
            tip_log(&format!(
                "ev=vk_raw vk={raw_vk:#04x} ctrl={c} shift={s} alt={a}"
            ));
        }
        // Context identity/password gate must precede reservation consumption.  A vanished or
        // password context invalidates the old pair and any close-only barrier.
        let ctx: ITfContext = match pic.ok() {
            Ok(c) => c.clone(),
            Err(_) => {
                self.invalidate_pending_end_test_reservation();
                return Ok(FALSE);
            }
        };
        if self.is_password_context(&ctx) {
            self.invalidate_pending_end_test_reservation();
            if self.composition_end_pending.get() {
                self.abandon_pending_composition_end("keydown_password_context");
            }
            return Ok(FALSE);
        }
        let signature = match PendingEndKeySignature::from_context(
            &ctx,
            raw_vk,
            vk,
            lparam.0,
            current_modifier_mask(),
        ) {
            Some(signature) => signature,
            None => {
                self.invalidate_pending_end_test_reservation();
                return Ok(FALSE);
            }
        };
        // keymap 役割解決は cmd_modifier ゲートより前に 1 回計算する（両入口で同じ値を共有し、
        // 「食うか」と実処理の一致を保つ）。armed Ctrl+BS や Ctrl 併用チョードは cmd_modifier=true
        // として届くため、carve-out（action != None）で cmd ゲートを通す必要がある。composing/showing/
        // direct は resolve_action_now が ctx 未使用の純粋な状態参照として内部で読む。
        let action = self.resolve_action_now(vk);
        // Consume the slot for every KeyDown.  Only exact signature+pair generation gets the
        // one-shot reservation; mismatch/stale is discarded so a future same-VK event cannot eat it.
        let pending_test_reserved = self.take_pending_end_test(signature);
        let pending_at_entry = self.composition_end_pending.get();
        // Ctrl/Alt 併用キー（Ctrl+C/V 等のアクセラレータ）は食わずアプリへ通す。
        // 本来 OnTestKeyDown が FALSE を返せば OnKeyDown は呼ばれないが、同じ判定をここにも
        // 置いて防御する（リファクタ耐性・意図の明示）。keymap action（confirm undo/ephemeral/
        // Ctrl 併用チョード）だけはこのゲートを通す（実処理へ進ませる — carve-out invariant）。
        if cmd_modifier_down()
            && action == crate::keymap::KeyAction::None
            && !pending_test_reserved
            && !pending_at_entry
        {
            return Ok(FALSE);
        }
        // A7: スリープ復帰の世代カウンタをキースレッドで刈り取る。
        self.poll_power_events();

        // pending close は direct-mode gate より先に回収する。retry 成否にかかわらず、この
        // 1打鍵は TRUE で食う（失敗時は再押下で再試行、成功時は次打鍵から通常判定）。これにより
        // direct A-Z が FALSE で素通りし続け、古い composition が回収不能になる状態を作らない。
        if pending_test_reserved || pending_at_entry {
            if self.composition_end_pending.get() && !self.finish_pending_composition(&ctx) {
                tip_log("ev=keydown skip=pending_end");
                return Ok(TRUE);
            }
            if self.state.borrow().composing {
                // ユーザ打鍵は timer 上限後にも与えられる新しい回復機会。回数を戻して
                // 張り直しを試し、成功時は残り読みの live debounce も再開する。
                self.partial_preedit_redraw_retries.set(0);
                if self.redraw_partial_preedit_if_needed(&ctx) {
                    self.arm_debounce();
                } else {
                    self.arm_partial_preedit_redraw_retry();
                }
            } else if !self.showing.get() {
                self.exit_ephemeral_to_direct(Some(&ctx));
            }
            return Ok(TRUE);
        }

        // Spec2: パスワード欄（IS_PASSWORD）は IME 丸ごと無効（完全 direct 化・全キー素通し）。
        // 秘匿入力を composition/候補窓/学習/診断ログのどこにも乗せない。direct ゲートと同じく
        // TestKeyDown/KeyDown の両方に置く（TestKeyDown を経ないホスト対策 — SP5 バグA の教訓）。
        // パスワード欄内で composition は始まらない（全キー素通しのため）。合成中に外から
        // パスワード欄へフォーカス移動した場合の旧 composition はホストの
        // OnCompositionTerminated / フォーカス遷移 settle が既存機構で畳む。
        // ephemeral かなモード開始トリガ: direct+idle でトリガキー（既定 F8）が来たら
        // compartment を NATIVE へ切替えて ephemeral かなへ入る。パスワード欄より後（欄内で
        // トリガキーを食わない）・direct 早期 return より前（トリガキー自体は素通しでなく消費する）。
        // enter の副作用は OnKeyDown 側でのみ行う（OnTestKeyDown は食う判定の一致のみ）。
        // action==Ephemeral 自体は Ctrl ゲートより前で計算済み（上記）— ここでは消費のみ、再計算しない。
        if action == crate::keymap::KeyAction::Ephemeral {
            self.enter_ephemeral_kana(Some(&ctx));
            return Ok(TRUE); // トリガキー自体は文字を出さず消費
        }

        // 確定取消（Ctrl+Backspace）: M-5 — 純粋修飾キー単体でも CommitUndo でもない打鍵は
        // 非武装化する（disarm 例外は CommitUndo のみ。無修飾 Backspace 単体は disarm 対象）。
        if !is_pure_modifier_vk(vk) && action != crate::keymap::KeyAction::CommitUndo {
            self.disarm_undo();
        }

        // LLM 変換待機中: 入力をロックする。Esc は待機を中断して読み preedit へ戻す。
        // bump_llm_seq だけでは phase が AwaitingLlm のまま残り、応答が来ないエンジンでは
        // 永久フリーズになるため、abort_llm で待機解除まで行う（タイムアウトと同経路）。
        if self.state.borrow().awaiting_llm() {
            if vk == VK_ESCAPE {
                self.abort_llm("user_escape");
                tip_log("ev=llm_cancel_requested");
            }
            return Ok(TRUE); // 待機中は他キーを食って無視（preedit を保護）
        }

        // 確定取消（Ctrl+Backspace）: armed 中の Ctrl+Backspace はここで実処理へ進む
        // （password gate と awaiting gate の両方を通過した後 — 秘匿欄や LLM 待機中には
        // 割り込ませない）。
        if action == crate::keymap::KeyAction::CommitUndo {
            self.start_commit_undo(&ctx);
            return Ok(TRUE);
        }

        // SP5 実機バグの真因修正（direct モード gate）。
        // 一部のホスト（IMM32/CUAS ブリッジや、ITfKeystrokeMgr::KeyDown を TestKeyDown 無しで
        // 呼ぶアプリ）は OnTestKeyDown を経ずに OnKeyDown を直接呼ぶ。direct(半角英数)モードの
        // 「A–Z を食わず本文へ流す」判定は will_handle（＝OnTestKeyDown 専用）にしか無かったため、
        // OnTestKeyDown 非経由のホストでは gate が素通りし、direct でも OnKeyDown の A–Z アームが
        // 常に input_char へ流して かな化していた（実機ログ: ev=keytest がゼロなのに ev=commit が
        // 出る＝OnTestKeyDown 非経由で OnKeyDown が走っていた）。
        // native は各アームが自前で showing/composing を見てパススルーするので無変更でよい（動作中の
        // 経路に触れない最小修正）。direct のときだけ will_handle を再評価し、食わないキー（A–Z 等）を
        // 即パススルー(Ok(FALSE))する。A–Z は ev=keydown を出す（OnTestKeyDown 非経由でも必ず出る、
        // direct の実値も晒す実機確認用の診断）。
        let composing = self.state.borrow().composing;
        let showing = self.showing.get();
        // ephemeral かな: idle で「かなが食わない（素通しする）キー」が来たら、この打鍵を
        // 消費する前に direct へ復帰しておく（キーは素通しのまま — 押し忘れの言語モード居残り防止）。
        if ephemeral_idle_abort(
            vk,
            cmd_modifier_down(),
            shift_down(),
            self.symbol_overlay.get(),
            self.ephemeral_kana.get(),
            composing,
            showing,
            action,
        ) {
            self.exit_ephemeral_to_direct(Some(&ctx));
        }
        // direct は exit の**後**に読む。先に読むと、abort で compartment が direct になったのに
        // 古い native の値で下の direct ゲートが不発になり、各 vk アームが「direct は gated で弾かれ
        // ここへ来ない」という前提のまま composition を開く（compartment と composition のねじれ）。
        let direct = self.is_direct_mode();
        if (VK_A..=VK_Z).contains(&vk) {
            tip_log(&format!(
                "ev=keydown vk={vk:#04x} direct={direct} composing={composing} showing={showing}"
            ));
        }
        if direct
            && !will_handle_gated(
                vk,
                composing,
                showing,
                cmd_modifier_down(),
                true,
                shift_down(),
                self.symbol_overlay.get(),
                action,
            )
        {
            return Ok(FALSE);
        }

        // keymap コマンドのディスパッチ（文脈ゲートは resolve_action が判定済み — 相互排他の
        // KeyAction で「どの機能に当たったか」だけを見る。VK 直参照でないのはリマップ対応のため）。
        // CommitUndo/Ephemeral は上流（:594/:619）で return 済みなのでここには来ない。
        match action {
            crate::keymap::KeyAction::Typo => {
                // 文節モード中の Tab は候補送り(trigger_typo_convert が showing で move_candidate)
                // ＝SelectClauseCandidate の同期 IPC。自動リピートは Convert と同じ理由で食い切る。
                if showing && self.clause_nav.borrow().is_some() && is_autorepeat(lparam) {
                    return Ok(TRUE);
                }
                self.trigger_typo_convert(&ctx);
                return Ok(TRUE);
            }
            crate::keymap::KeyAction::Llm => {
                self.start_llm_convert(&ctx);
                return Ok(TRUE);
            }
            crate::keymap::KeyAction::Notation(kind) => return self.apply_notation(&ctx, vk, kind),
            crate::keymap::KeyAction::NotationRotate => {
                return self.dispatch_notation_rotate(&ctx, vk)
            }
            crate::keymap::KeyAction::Convert => {
                // 文節モード中の変換キー(Space)は候補送り＝SelectClauseCandidate の同期 IPC。
                // 自動リピートは ←/→ と同じ理由で IPC を発行せず食い切る(is_autorepeat 注記)。
                if showing && self.clause_nav.borrow().is_some() && is_autorepeat(lparam) {
                    return Ok(TRUE);
                }
                self.trigger_convert(&ctx);
                return Ok(TRUE);
            }
            // Reconvert はフォールバック無し（対象が無ければ start_reconvert が no-op で食う）。
            crate::keymap::KeyAction::Reconvert => {
                self.start_reconvert(&ctx);
                return Ok(TRUE);
            }
            crate::keymap::KeyAction::ModeToggle => {
                self.dispatch_mode_toggle(Some(&ctx));
                return Ok(TRUE);
            }
            _ => {}
        }

        match vk {
            // ---- A–Z: 文字入力 ----
            VK_A..=VK_Z => {
                // 無修飾の英字は小文字ローマ字へ正規化してエンジンへ（かな経路）。AltGr 等で
                // レイアウトが非英字（アクセント文字/記号）を生む場合だけその文字を尊重する。
                // Shift 押下は設定で二流儀: compose=英語未確定モード（エンジン読みへ direct 挿入
                // ＝かな合成と同一未確定に継ぎ足し、確定まで英字が続く — MS-IME 系）/
                // commit=一時直接入力（大文字を composition を張らず直接確定。開いている合成は
                // commit_char_direct が先に settle する — Google/ATOK 系・打鍵作法 Task5 改）。
                // 経路決定は resolve_az_char（純関数）が唯一の真実。
                let compose = self.shift_latin_compose.get();
                let latin_mode = self.state.borrow().latin_mode();
                match resolve_az_char(
                    vk,
                    shift_down(),
                    key_to_char(vk, lparam),
                    compose,
                    latin_mode,
                ) {
                    AzRoute::DirectCommit(ch) => self.commit_char_direct(&ctx, ch),
                    AzRoute::Latin(ch) => self.input_char(&ctx, ch, InsertStyle::Direct),
                    AzRoute::Kana(ch) => self.input_char(&ctx, ch, InsertStyle::Kana),
                }
            }

            // Space の henkan は KeyAction::Convert（上流の dispatch match）へ移設した。

            // ---- ↑/↓: 候補表示中は選択を移動（↓=次 / ↑=前、端で循環）----
            VK_UP | VK_DOWN => {
                if self.showing.get() {
                    // 文節モード中の↑/↓は選択同期の SelectClauseCandidate IPC を伴う —
                    // 自動リピートは IPC を発行せず食い切る(is_autorepeat 注記。通常候補窓の
                    // ↑/↓はローカル反映のみなので従来どおりリピートを通す)。
                    if self.clause_nav.borrow().is_some() && is_autorepeat(lparam) {
                        return Ok(TRUE);
                    }
                    self.move_candidate(&ctx, if vk == VK_DOWN { 1 } else { -1 });
                    Ok(TRUE)
                } else {
                    // 候補が出ていなければ食わない（アプリのキャレット移動に任せる）。
                    Ok(FALSE)
                }
            }

            // ---- Enter: 確定 ----
            VK_RETURN => {
                let composing = self.state.borrow().composing;
                if !composing && !self.showing.get() {
                    return Ok(FALSE);
                }
                let showing = self.showing.get();
                // 文節ナビゲーション中の Enter は全文節の確定。cand_state の index は
                // 「選択文節の候補」の添字なので、通常の Commit{index}（全文候補キャッシュ）へ
                // 流すと別候補を確定してしまう — 専用の CommitClauses 経路へ。
                if showing && self.clause_nav.borrow().is_some() {
                    self.commit_clauses(&ctx);
                    return Ok(TRUE);
                }
                // 確定文字列の唯一の真実源は cand_state（drain_behavior と同じ読み元）。
                // 選択 index も cand_state から読み、空なら先頭へフォールバック（index と文字列は一致）。
                let cand_pick = if showing {
                    let st = self.cand_state.borrow();
                    st.resolve_commit(st.selected())
                } else {
                    None
                };
                if let Some((index, text)) = cand_pick {
                    // 候補確定: 前方一致候補ならエンジンが残り読みを返すので部分確定して継続する。
                    self.commit_candidate(&ctx, index, &text);
                } else {
                    // 候補非表示: ライブ変換結果（無ければ読み）を確定する。
                    // Spec2: エンジンが生きていれば Commit(0) に通して学習に乗せる
                    // （liveConvert が先頭候補をキャッシュ済み）。劣化時は従来どおり文字列直確定
                    // （学習されないだけで確定は必ず成功）。前方一致候補なら candidate_prefix と
                    // 同じ部分確定継続（source=live_prefix）— 従来は残り読みが暗黙に捨てられていた。
                    // ライブ変換 OFF / F6-F10 表記固定中は engine のライブ変換を参照しない
                    // （None=劣化枝と同じ DirectCommit へ）— 表示中の live_text をそのまま確定する。
                    let live = if self.should_consult_live_engine() {
                        let seq = self.state.borrow_mut().bump_live_seq();
                        // auto_commit=false: この直後の Commit{0} が全読みを確定する前提のため、
                        // エンジンに読みを消費させてはいけない（protocol.rs の LiveConvert 参照）。
                        self.engine_live_convert(seq, false).map(|(t, _, _)| t)
                    } else {
                        None
                    };
                    let live_text = self.live_text.borrow().clone();
                    let last_reading = self.last_reading.borrow().clone();
                    // Why not(ライブ変換 OFF 専用の source を作る): `arms_undo` は
                    // `matches!(source, "candidate" | "live")` の**列挙**なので、別名にすると
                    // Ctrl+Backspace の確定取消が OFF のときだけ静かに武装しなくなる
                    // （`should_widen_digits` は逆に除外列挙なので別名でも幅は変わらない）。
                    match plan_live_enter(live, &live_text, &last_reading) {
                        LiveEnterPlan::EngineCommit { text } => {
                            let plan = plan_commit(self.engine_commit(0), &text);
                            self.apply_commit_plan(&ctx, plan, "live", "live_prefix", None);
                        }
                        LiveEnterPlan::DirectCommit { text } => {
                            // 巡5 GLM M-1: 成否を受ける（後続処理は無いが FullReset 枝の
                            // drop_engine 自己修復と対称に — 直確定はエンジン消費が無いので
                            // drop 不要、ログのみで再試行に任せる）。
                            let _ = self.commit_and_reset(&ctx, &text, "live", None);
                        }
                    }
                }
                Ok(TRUE)
            }

            // ---- 数字 1–9: 候補窓表示中(Shift無し)は候補選択 / それ以外は記号・数字としてエンジンへ ----
            VK_1..=VK_9 => {
                if self.showing.get() && !shift_down() {
                    // Bug 4: 数字キー n(1..9) は「表示中ページの n 行目」を選ぶ。候補が MAX_VISIBLE_ROWS
                    // を超えて 2ページ目以降を表示している場合は、絶対 index ではなくページ先頭を
                    // 加えた絶対 index を確定する（従来は絶対 index 固定で常に1ページ目を誤確定していた）。
                    // 確定文字列の唯一の真実源は cand_state（M-3）。borrow はこのブロック内で完結させる。
                    // 文節ナビゲーション中は確定でなく「選択文節の候補選択」（MS-IME と同じ —
                    // 確定は Enter の CommitClauses）。move_candidate 経由で選択＝preedit＝エンジン
                    // 状態が一点で同期する。
                    if self.clause_nav.borrow().is_some() {
                        // 自動リピートは IPC を発行せず食い切る(is_autorepeat 注記)。
                        if is_autorepeat(lparam) {
                            return Ok(TRUE);
                        }
                        let delta = {
                            let st = self.cand_state.borrow();
                            let digit = (vk - VK_1) as usize;
                            page_candidate_index(st.selected(), st.count(), digit)
                                .map(|abs| abs as i32 - st.selected() as i32)
                        };
                        if let Some(delta) = delta {
                            self.move_candidate(&ctx, delta);
                        }
                        // 可視ページ外の数字は no-op（通常経路と同じく食い切る）。
                        return Ok(TRUE);
                    }
                    let picked = {
                        let st = self.cand_state.borrow();
                        let digit = (vk - VK_1) as usize; // 0 始まりのページ内行
                        page_candidate_index(st.selected(), st.count(), digit)
                            .and_then(|abs| st.resolve_commit(abs))
                    };
                    match picked {
                        Some((index, text)) => {
                            tip_log(&format!("ev=candidate_move sel={index}"));
                            // 候補確定: 前方一致候補なら部分確定して残り読みを継続する。
                            self.commit_candidate(&ctx, index, &text);
                            Ok(TRUE)
                        }
                        // 可視ページの行数を超える数字は no-op（誤った候補を選ばない）。候補表示中は
                        // will_handle が「食う」と宣言するので、reading へ差し込まず食い切る（TRUE）。
                        None => Ok(TRUE),
                    }
                } else if shift_down() {
                    if self.state.borrow().latin_mode() {
                        // 英語モード: Shift+数字は生 ASCII の記号('!' 等)を direct へ。記号トグルより
                        // 英語モードが優先(モード中は記号も半角 ASCII という仕様)。
                        match key_to_char(vk, lparam) {
                            Some(ch) => self.input_char(&ctx, ch, InsertStyle::Direct),
                            None => Ok(TRUE),
                        }
                    } else if self.symbol_overlay.get() {
                        // 記号 overlay ON: Shift+1..9 は記号(！＠＃…)。読みへ畳み込む(idle は合成開始)。
                        // gated 側の同条件オーバーレイと対(eaten 一致)。旧仕様では !/@ が
                        // zenkaku_symbol の表にあるのに VK がここへ届かず死にエントリだった。
                        self.symbol_keydown(&ctx, vk, lparam)
                    } else if self.state.borrow().composing {
                        // OFF+composition: 従来どおり raw 記号('!' 等)を読みへ(gated が食うと宣言済み)。
                        match key_to_char(vk, lparam) {
                            Some(ch) => self.input_char(&ctx, ch, InsertStyle::Kana),
                            None => Ok(TRUE),
                        }
                    } else {
                        // OFF+idle: gated=false でここへ来ないはずだが、TestKeyDown を経ず KeyDown を
                        // 直叩きするホスト(US配列バグAで実在)では届く。'!' で composition を始めず
                        // 素通しする(text_vk アームの 0x30 ガードと対)。
                        Ok(FALSE)
                    }
                } else {
                    // ②: composing 継続 or かなモード idle 開始（native 無修飾。direct は gated で
                    // 弾かれ OnKeyDown へ来ない）。英語モード中は生 ASCII を direct へ
                    // （「iPhone7」の 7 を半角のまま継ぎ足す — 数字全角設定より英語モード優先）。
                    match key_to_char(vk, lparam) {
                        Some(ch) => {
                            let style = if self.state.borrow().latin_mode() {
                                InsertStyle::Direct
                            } else {
                                InsertStyle::Kana
                            };
                            self.input_char(&ctx, ch, style)
                        }
                        // L-3: 印字不能（デッドキー/合字/未対応）は食って無視（stray char 漏れ防止）。
                        None => Ok(TRUE),
                    }
                }
            }

            // ---- Esc: 再変換取消 / 候補を閉じる / composition を取消 ----
            VK_ESCAPE => {
                if self.reconverting.get() {
                    // 再変換中の Esc: 元ラテンを復元して終了。
                    self.cancel_reconvert(&ctx);
                    Ok(TRUE)
                } else if self.showing.get() {
                    self.candidate_ui.borrow_mut().hide();
                    self.showing.set(false);
                    // 文節ナビゲーションも解除（エンジン側の文節状態は次の Convert/Insert が
                    // cacheCandidates/invalidate で必ず捨てるので IPC は送らない）。
                    self.clear_clause_nav();
                    tip_log("ev=candidates_hidden");
                    self.restore_live_preedit(&ctx);
                    // 候補窓を閉じて composition は継続 → 読みモニタを再表示する
                    // （ユーザ決定: 候補窓と排他、閉じたら戻す）。restore_live_preedit の
                    // run_preedit も同じフックを持つが、素材が空で描き戻さなかったときに
                    // モニタが出ないままになるのでここは畳まない。
                    self.update_reading_monitor(&ctx);
                    Ok(TRUE)
                } else if self.state.borrow().composing {
                    self.disarm_debounce();
                    // 巡4 T4: セッション拒否時は TIP 側状態を畳まない（composition が文書に
                    // 残るため「Esc が効かなかった」扱いにする）。do_cancel 内の left_context
                    // 清算は実行済み。
                    if !self.do_cancel(&ctx) {
                        tip_log("ev=cancel_rejected source=escape");
                        return Ok(TRUE);
                    }
                    self.reading_monitor.borrow_mut().hide();
                    self.state.borrow_mut().on_escape();
                    self.engine_end_session();
                    // ephemeral かな: composition を破棄した以上、開始状態へ戻す＝direct へ復帰する。
                    self.exit_ephemeral_to_direct(Some(&ctx));
                    self.live_text.borrow_mut().clear();
                    *self.current_context.borrow_mut() = None;
                    Ok(TRUE)
                } else {
                    Ok(FALSE)
                }
            }

            // ---- Backspace: 1 文字削り、読みを表示してデバウンス再変換 ----
            VK_BACK => {
                let composing = self.state.borrow().composing;
                match backspace_route(composing, self.showing.get(), self.reconverting.get()) {
                    BackspaceRoute::CancelReconvert => {
                        // RestoreText の成否にかかわらずこの打鍵は消費する。拒否時は
                        // cancel_reconvert が候補/原文/ラッチを保持するため、再押下で再試行できる。
                        let _ = self.cancel_reconvert(&ctx);
                        return Ok(TRUE);
                    }
                    BackspaceRoute::HideStaleCandidates => {
                        // showing&&!composing&&!reconverting は通常生成されないが、候補を残したまま
                        // 本文 Backspace へ流すと OnTestKeyDown と実処理が分岐する。本文を変更せず、
                        // stale な候補表示だけを閉じて消費する。
                        self.candidate_ui.borrow_mut().hide();
                        self.reading_monitor.borrow_mut().hide();
                        self.showing.set(false);
                        self.clear_clause_nav();
                        *self.current_context.borrow_mut() = None;
                        self.live_text.borrow_mut().clear();
                        return Ok(TRUE);
                    }
                    BackspaceRoute::Pass => return Ok(FALSE),
                    BackspaceRoute::Compose => {}
                }
                if self.showing.get() {
                    self.candidate_ui.borrow_mut().hide();
                    self.showing.set(false);
                    self.clear_clause_nav();
                }
                self.state.borrow_mut().on_backspace();
                // mark_good は Some アーム内限定 — match 後の共通行に置くと劣化出力まで
                // 良好素材として記録され「エンジン由来の表示」という前提が崩れる。
                let reading = match self.engine_backspace() {
                    Some(r) => {
                        self.state.borrow_mut().mark_good(&r);
                        *self.last_reading.borrow_mut() = r.clone();
                        r
                    }
                    None => {
                        let degraded = self.state.borrow_mut().degraded_reading();
                        *self.last_reading.borrow_mut() = degraded.clone();
                        degraded
                    }
                };
                if reading.is_empty() {
                    self.disarm_debounce();
                    // 巡4 T4: 拒否時は composition が文書に残るため状態を畳まない（再 Backspace
                    // の再試行に任せる）。
                    if !self.do_cancel(&ctx) {
                        tip_log("ev=cancel_rejected source=backspace_empty");
                        // 巡10(round10): on_backspace() が最終文字削除で composing=false に
                        // 済ませているので巻き戻す — このままだと文書に composition が残るのに
                        // will_handle の composing gate(Esc/BS/Return)を素通りし、composition
                        // を閉じる手段が消えて次の Backspace が本文を削りかねない。
                        // 巡11(round11): live_text も空にする — 残したまま戻すと Enter/settle の
                        // plan_live_enter が残骸を確定素材に選び「cancel は落ちたのに commit は
                        // 通る」で消したはずの文字が本文に入り得る。空なら Enter は空確定
                        // （SetText 成功時に限り composition を畳る cancel 代わり — 拒否時は
                        // cancel と同様に中間状態のまま再試行）、Tab(LLM)/F6-F10 は素材空ガード
                        // で no-op になり、中間状態は Esc/BS/Enter の再試行に集中する。
                        // Space(Convert)/Tab(Typo) は空ガード対象外 — 同期 IPC のみで、
                        // 応答が空配列なら TIP 側の no-op 枝(`Some(cands) if !cands.is_empty()`
                        // の外)で収まる。非空を返す実測は無いが「契約」として固定しても
                        // いない(エンジン側に空読みの早期 return は無い)。LLM(awaiting
                        // ロックで再試行が死ぬ)と Notation(preedit 潰し)は実害ゆえガード済み。
                        self.state
                            .borrow_mut()
                            .resume_composing_after_cancel_reject();
                        self.live_text.borrow_mut().clear();
                        return Ok(TRUE);
                    }
                    self.reading_monitor.borrow_mut().hide();
                    self.state.borrow_mut().reset();
                    self.live_text.borrow_mut().clear();
                    *self.current_context.borrow_mut() = None;
                } else {
                    *self.current_context.borrow_mut() = Some(ctx.clone());
                    *self.live_text.borrow_mut() = reading.clone();
                    self.run_preedit(&ctx, &self.widen_display_text(&reading));
                    self.arm_debounce();
                }
                Ok(TRUE)
            }

            // 変換キー(0x1C) は KeyAction::Reconvert（direct+idle）/ Convert（composing・showing）
            // として上流の dispatch match が処理する（reconvert_fallback 特例は撤去）。

            // ---- 0 / 記号 / テンキー: composition 中はエンジンへ送る（記号/数字を変換に含める）----
            // idle の OEM 記号も全角へ写して composition を開始する（2026-08-03 — 旧 Task3 の
            // 直接確定を廃止。native のみ — direct は冒頭の will_handle_gated ゲートで到達しない）。
            vk if is_text_vk(vk) => {
                // 記号打鍵（OEM は常時・数字行 0x30 は Shift かつ 記号 overlay ON）は composition/idle
                // とも symbol_keydown へ。判定は gated と同じ述語（eaten 一致 = item19）。
                let sym = is_symbol_keystroke(vk, shift_down(), self.symbol_overlay.get());
                if self.state.borrow().composing {
                    if self.state.borrow().latin_mode() {
                        // 英語モード: 記号/数字/テンキーも生 ASCII のまま direct へ（symbol_keydown の
                        // 全角化・`-`→ー の to_kana_reading_char は適用しない — 「user_name」を
                        // 半角のまま継ぎ足す。英語モードが幅設定より優先という仕様）。
                        match key_to_char(vk, lparam) {
                            Some(ch) => self.input_char(&ctx, ch, InsertStyle::Direct),
                            None => Ok(TRUE),
                        }
                    } else if sym {
                        self.symbol_keydown(&ctx, vk, lparam)
                    } else {
                        match key_to_char(vk, lparam) {
                            // 英字＝ローマ字合成、テンキー `-`→ー（to_kana_reading_char 維持）、テンキー `.` は literal。
                            Some(ch) => {
                                self.input_char(&ctx, to_kana_reading_char(ch), InsertStyle::Kana)
                            }
                            // L-3: 合成中の印字不能キー（デッドキー/合字）は食って無視（stray char 漏れ防止）。
                            None => Ok(TRUE),
                        }
                    }
                } else if sym {
                    self.symbol_keydown(&ctx, vk, lparam)
                } else if is_digit_vk(vk) {
                    // 0x30/テンキー数字の idle: Shift はここに来ない契約(gated=false)。TestKeyDown を
                    // 経ず KeyDown を直叩きするホスト(US配列バグAで実在)の保険 — 素通しし、')' 等で
                    // composition を始めない(下の VK_1..=VK_9 アームの同ガードと対)。
                    if shift_down() {
                        return Ok(FALSE);
                    }
                    // ②: かなモード idle の 0/テンキー数字は composition を開始（gated が食うと宣言済み）。
                    // idle=非合成なので英語モードはあり得ない（latin_mode は composing が前提）。
                    match key_to_char(vk, lparam) {
                        Some(ch) => self.input_char(&ctx, ch, InsertStyle::Kana),
                        None => Ok(TRUE),
                    }
                } else {
                    Ok(FALSE) // テンキー演算子（* + - . /）等は idle 素通し（従来どおり）
                }
            }

            // ---- ←/→: 候補表示中は文節移動（MS-IME の文節ナビゲーション）----
            // 変換候補の表示中に選択文節を左右へ動かし、候補窓を「選択文節の候補」へ差し替える。
            // 劣化は 2 段: 文節モードに**入れない**（旧エンジン非対応/被覆候補無し/再変換中）は
            // 従来どおり「確定して畳む」、**確立後**の失敗（一時的な IPC タイムアウト等）は
            // no-op で食い切る — settle へ落とすと移動のつもりの矢印 1 打が合成の全確定になる
            // （マージ後敵対レビュー④。両者は move_clause=false からは区別できない）。
            // composition のみ（候補窓なし）も従来どおり settle — 読み内カーソル移動は据え置き。
            // eaten 契約（composing||showing で食い切る）は不変（will_handle_gated と一致）。
            VK_LEFT | VK_RIGHT => {
                if self.showing.get() && !self.reconverting.get() {
                    let in_nav = self.clause_nav.borrow().is_some();
                    // 自動リピート中の文節モードは IPC を発行せず食い切る(is_autorepeat 注記。
                    // 敵対レビュー③ — ↑/↓/Space/数字のガードと同じ規律)。
                    if in_nav && is_autorepeat(lparam) {
                        return Ok(TRUE);
                    }
                    if self.move_clause(&ctx, if vk == VK_RIGHT { 1 } else { -1 }) {
                        return Ok(TRUE);
                    }
                    if in_nav {
                        return Ok(TRUE);
                    }
                }
                if self.state.borrow().composing || self.showing.get() {
                    // 巡5 GLM M-2: settle の成否は受けるが navigate は再試行可能 — 拒否時も
                    // TRUE で食いてユーザの再押下に任せる（モードトグルと違い状態を壊さない）。
                    let _ = self.settle_active_input(Some(&ctx), "navigate");
                    Ok(TRUE)
                } else {
                    Ok(FALSE)
                }
            }

            // ---- Home/End/PageUp/PageDown/Delete: 合成/候補表示中は確定して畳む（UU-6）----
            // 未処理だと合成中にキャレットだけ動いて preedit が別位置へ取り残される（合成崩れ）。
            // 開いている入力を settle（候補表示中は選択候補、composition のみはライブ変換結果）で
            // 確定してから畳む＝MS-IME/Google 日本語入力に倣った「確定してから移動」。キー自体は
            // 食い切る（Test/実の eaten 判定を一致させる）。idle では上の gate に到達せず
            // will_handle=false で OnKeyDown 自体が呼ばれない（呼ばれてもここで FALSE を返す）。
            VK_HOME | VK_END | VK_PRIOR | VK_NEXT | VK_DELETE => {
                if self.state.borrow().composing || self.showing.get() {
                    // 巡5 GLM M-2: 矢印と同じ（成否を受けるが TRUE で食く）。
                    let _ = self.settle_active_input(Some(&ctx), "navigate");
                    Ok(TRUE)
                } else {
                    Ok(FALSE)
                }
            }

            // ---- それ以外: パススルー ----
            _ => Ok(FALSE),
        }
    }

    /// 読みの表記変換（旧 F6-F10 アーム本体）。`vk` は診断ログ用の実押下キー。
    /// KeyAction::Notation が composing を織り込むため、旧アームの composing チェックは呼び出し側で不要。
    /// レビュー I-1: 候補ウィンドウが出ていれば閉じる（input_char と同じ後片付け。MS-IME も
    /// 候補表示中の F7 は窓を閉じて表記変換する）。閉じないと Enter/Space/数字キーが showing 枝で
    /// stale 候補リストを操作し、画面表示と違う文字列を確定してしまう。
    /// レビュー M-2（既知の限界）: raw は「打鍵の生入力」だが、部分確定後の reseed では残り読みの
    /// **かな**になる。その状態の全角/半角英数はかなベース表示になる（元ローマ字は復元不能で許容）。
    fn apply_notation(
        &self,
        ctx: &ITfContext,
        vk: u32,
        kind: crate::keymap::Notation,
    ) -> Result<BOOL> {
        use crate::keymap::Notation;
        if self.showing.get() {
            self.candidate_ui.borrow_mut().hide();
            self.showing.set(false);
            self.clear_clause_nav();
        }
        self.disarm_debounce();
        let reading = self.last_reading.borrow().clone();
        let raw = self.state.borrow().raw.clone();
        let shown = match kind {
            Notation::Hiragana => reading,
            Notation::Katakana => to_katakana(&reading),
            Notation::HankakuKana => to_hankaku_kana(&reading),
            // どちらも raw をそのまま素材にしない: 句読点/記号は合成時に全角へ畳み込まれて
            // raw に積まれる(symbol_keydown)ため、まず逆写像で打鍵の半角へ戻す。全角英数は
            // その上で機械全角化(「、。」→",."→"，．")。戻さないと 、。 が「英数記号」表記に
            // ならず素通りする(F10 側と同型の非対称 — 敵対レビュー M-1)。
            Notation::ZenkakuEisu => to_zenkaku_ascii(&to_hankaku_ascii(&raw)),
            Notation::HankakuEisu => to_hankaku_ascii(&raw),
        };
        // 巡11(round11): 素材が空(空 Backspace の cancel 拒否巻き戻し中は reading/raw とも空)
        // なら何もしない — 空文字の run_preedit が文書に残る composition を潰してしまう。
        // notation_fixed も書かない(次の入力で表記指定が誤発火しない)。
        if shown.is_empty() {
            tip_log("ev=notation_skip_empty_text");
            return Ok(TRUE);
        }
        {
            let mut st = self.state.borrow_mut();
            st.notation_fixed = Some(kind);
            st.mark_good(&shown);
        }
        // 表示だけ全角化する（input_char と同じ規律）。live_text/mark_good を半角 canonical の
        // まま置くのは、それが劣化確定・リプレイでエンジンへ戻る素材だから。
        // widen は notation_fixed を書いた**後**で呼ぶ — 先だと F10/半角カナが半角指定として
        // 効かず、機能名と真逆に数字が全角化される。
        *self.live_text.borrow_mut() = shown.clone();
        *self.current_context.borrow_mut() = Some(ctx.clone());
        self.run_preedit(ctx, &self.widen_display_text(&shown));
        tip_log(&format!("ev=notation vk={vk:#04x} text={shown}"));
        Ok(TRUE)
    }

    /// NotationRotate(無変換連打)のディスパッチ。現在表記(notation_fixed)から次を導出して
    /// apply_notation へ。OnKeyDown の KeyAction::NotationRotate と OnPreservedKey の
    /// ToggleMode 委譲(受理配送ホスト)の両経路が共有し、二重実装のズレを防ぐ(spec §6.3)。
    fn dispatch_notation_rotate(&self, ctx: &ITfContext, vk: u32) -> Result<BOOL> {
        let next = crate::keymap::next_notation(self.state.borrow().notation_fixed);
        self.apply_notation(ctx, vk, next)
    }

    fn on_preserved_key_impl(&self, pic: Ref<'_, ITfContext>, rguid: *const GUID) -> Result<BOOL> {
        // A7: スリープ復帰の世代カウンタをキースレッドで刈り取る（classify_preserved_key より前）。
        self.poll_power_events();
        let guid = unsafe { *rguid };
        let ctx = pic.ok().ok().cloned();
        // JIS キー（無変換/変換）と US キー（Alt+`/Alt+/）の両 GUID を同一アクションへ束ねる。
        let action = classify_preserved_key(&guid);
        // C-1: トグル/再変換/feedback 鍵は OnPreservedKey 経由（OnKeyDown を通らない）ため、
        // ここが preserved key 経路唯一の disarm 点。awaiting_llm/password の早期 return より前で
        // 解除する（トグル等がガードで握り潰されても armed だけは必ず落ちる）。
        if action != PreservedAction::None {
            self.disarm_undo();
        }
        // Bug 3: LLM 変換待機(AwaitingLlm)中はモードトグル/再変換も抑止する。待機中に
        // モードや composition を触ると入力ロック（preedit 保護）が破れる（OnKeyDown が
        // 待機中に全キーを食うのと対）。preserved key はホストが既に消費しているので、
        // 我々の登録キー(ToggleMode/Reconvert)は TRUE で握り潰す（FALSE はデッドキー化）。
        if action != PreservedAction::None && self.state.borrow().awaiting_llm() {
            tip_log("ev=preservedkey skip=awaiting_llm");
            return Ok(TRUE);
        }
        // Spec2: パスワード欄では preserved key（モードトグル/再変換）も握り潰す。特に direct
        // モード中の再変換キーは start_reconvert が確定済み文字列（=入力したパスワード）を掴んで
        // engine/候補窓/診断ログへ載せてしまう — TestKeyDown/KeyDown の両ゲートと同じ秘匿不変条件。
        // preserved key はホストが既に消費しているので FALSE はデッドキー化するだけ。TRUE で食い切る。
        if action != PreservedAction::None {
            if let Some(ctx) = &ctx {
                if self.is_password_context(ctx) {
                    tip_log("ev=preservedkey skip=password");
                    return Ok(TRUE);
                }
            }
        }
        match action {
            PreservedAction::ToggleMode => {
                // spec §6.3: bare 0x1D を受理・配送するホストでは OnPreservedKey が composing 中も
                // 先取りして NotationRotate が沈黙する。リゾルバと同じ優先順を preserved 経路にも
                // 敷く: **配送キーが実際に bare 0x1D のときだけ** resolve_action(0x1D 固定)を諮る。
                // GUID_PRESERVEDKEY_MODE_TOGGLE は既定登録(bare 0x1D)とカスタムチョード
                // (build_preserved_regs の Chord 枝)の両方が共有するため、GUID だけでは配送キーを
                // 特定できない。mode_toggle が bare 0x1D を登録するとき(既定 or それ自身が bare 0x1D)
                // のみ この GUID = bare 0x1D と確定する(このガードが無いと、mode_toggle を bare F13 等へ
                // リバインドした際に 0x1D 固定の resolve が composing で NotationRotate を返し、モード
                // 切替でなくローテーションが誤発火。明示 "NonConvert" 指定なら OnKeyDown=rotate と一致させる)。
                // resolve_action_now は composing/!direct/束縛(rotate="none" 等)を全部織り込むので
                // 条件の二重実装をしない。ctx=None は apply_notation 不能 → 従来の toggle へ。
                let mode_toggle_delivers_bare_1d = match self.keymap.get().mode_toggle {
                    settings::keymap::Binding::Default => true,
                    settings::keymap::Binding::Chord(c) => {
                        c.vk == 0x1D && !c.ctrl && !c.shift && !c.alt
                    }
                    settings::keymap::Binding::Disabled => false,
                };
                if guid == crate::globals::GUID_PRESERVEDKEY_MODE_TOGGLE
                    && mode_toggle_delivers_bare_1d
                {
                    if let Some(c) = &ctx {
                        if self.resolve_action_now(0x1D) == crate::keymap::KeyAction::NotationRotate
                        {
                            return self.dispatch_notation_rotate(c, 0x1D);
                        }
                    }
                }
                // bare 無変換(0x1D) の OnKeyDown 救済（KeyAction::ModeToggle）と同じ順序を共有する。
                self.dispatch_mode_toggle(ctx.as_ref());
                Ok(TRUE)
            }
            PreservedAction::Reconvert => {
                // preserved key はホスト側で既に食われている。ここで FALSE を返してもアプリには
                // 届かず（=デッドキー）になるだけなので、再変換キーは常に TRUE で食い切る。
                if self.is_direct_mode() {
                    // 半角英数モード: 直前ラテン列を掴んで再変換（SP5）。
                    if let Some(ctx) = &ctx {
                        self.start_reconvert(ctx);
                    }
                } else {
                    // ひらがな（native）モード: VK_CONVERT を Space と同じ henkan として扱う。
                    // 打ちかけの composition を変換し候補窓を出す（idle なら trigger_convert は no-op）。
                    if let Some(ctx) = &ctx {
                        self.trigger_convert(ctx);
                    }
                }
                // idle でも TRUE。preserved key は host が食っているので FALSE はデッドキーになる。
                Ok(TRUE)
            }
            PreservedAction::Feedback => {
                // 品質ループ③: 直前確定の誤変換ワンキー記録（opt-in・バッファ消費は
                // record_feedback 内）。パスワード欄/LLM 待機は上の共通ガードで握り潰し済み。
                // preserved key はホストが既に食っているので常に TRUE（FALSE はデッドキー化）。
                self.record_feedback();
                Ok(TRUE)
            }
            PreservedAction::None => Ok(FALSE),
        }
    }
}

// ---- 打鍵→入力/確定の共有ヘルパ（A–Z と 記号/数字 で共通化）----
impl TextService_Impl {
    /// モード切替の前に、開いている入力を確定して畳む（MS-IME/Google 日本語入力と同じ
    /// 「モード切替は暗黙確定」挙動 — UU-3）。開いた composition がモード境界をまたぐと、
    /// direct 側では will_handle が showing しか見ないため Enter/Esc/BS が素通しになり
    /// preedit を閉じる手段がなくなる。実体は `settle_active_input`。
    /// 巡5 GLM I-2: 戻り値は settle の成否 — 呼び出し側は false ならトグルを中止すること
    /// （拒否時 composition が残ったまま direct へ落ちると上記の「閉じる手段がない」状態を
    /// 自ら作ることになる）。
    pub(crate) fn settle_before_mode_toggle(&self, ctx: Option<&ITfContext>) -> bool {
        self.settle_active_input(ctx, "mode_toggle")
    }

    /// モード切替の共有ディスパッチ。OnKeyDown の `KeyAction::ModeToggle`（bare 無変換 0x1D の
    /// OnKeyDown 救済）と OnPreservedKey の `ToggleMode` 枝の両方が呼ぶ。順序厳守:
    /// (a) 進行中の再変換を畳む → (b) settle → (c) toggle → (d) ephemeral 解除。
    /// (a)/(b) が拒否されたらトグル中止で抜ける（文書が未復元/未確定のまま境界をまたがせない）。
    /// `ctx=None`（preserved key で ctx が取れない）でも再変換ラッチだけは必ず落とす。
    pub(crate) fn dispatch_mode_toggle(&self, ctx: Option<&ITfContext>) {
        // SP5: 進行中の再変換があればモード切替の前に畳む。reconverting ラッチと開いた
        // composition がモード境界をまたいで残ると、切替後の入力が壊れる
        // （Esc が誤って RestoreText に流れる／composition の取り違え）。
        if self.reconverting.get() {
            match ctx {
                // cancel_reconvert=false は RestoreText が走っていない＝文書未復元で、ラッチも
                // composition も畳まれていない。best-effort で先へ進むと未復元の再変換
                // composition を settle が確定してしまい、reconverting ラッチを跨いだモード切替
                // になる（SP5 の取り違えの再発）。settle 拒否（下）と同じ規律で中止し、
                // キーは TRUE で食いた呼び出し元の再押下に任せる。
                Some(ctx) => {
                    if !self.cancel_reconvert(ctx) {
                        tip_log("ev=mode_toggle skip=reconvert_cancel_rejected");
                        return;
                    }
                }
                None => {
                    self.reconverting.set(false);
                    self.reconvert_original.borrow_mut().clear();
                    self.reconvert_reading.borrow_mut().clear();
                }
            }
        }
        // UU-3: 進行中の合成/候補もモード切替の前に確定して畳む（再変換の畳み込みと対）。
        // 巡5 GLM I-2: settle 拒否（文書に composition が残る）ならトグルしない — direct へ
        // 落ちると preedit を閉じる手段が消える。キーは TRUE で食いて再試行に任せる。
        if !self.settle_before_mode_toggle(ctx) {
            tip_log("ev=mode_toggle skip=settle_rejected");
            return;
        }
        let toggled = self.toggle_conversion_mode(ctx);
        // ephemeral かな: ユーザが明示的にモードトグルした＝永続かなへの昇格。
        // toggle 自身が NATIVE を反転せず通常かなへ昇格する（exit は呼ばない）。
        // 成功時だけ flag を落として、以降の commit_and_reset 等が direct へ戻さないようにする。
        // SetValue 失敗時は復帰要求を維持し、別の exit 経路で再試行する。
        if toggled {
            self.ephemeral_kana.set(false);
        }
    }

    /// 開いている入力（候補表示 or composition）を確定して畳む共通処理。モード切替(UU-3)と
    /// ナビゲーションキー Home/End/PageUp/PageDown/Delete(UU-6) で共有する。`source` は
    /// `ev=commit` の source ラベル（"mode_toggle" / "navigate"）。
    /// - 候補表示中: 選択中の候補を確定（Enter と同じ経路。前方一致候補は部分確定で
    ///   composition が残るので、続く composing 枝が残り読みも全確定して境界をまたがせない）。
    /// - composition のみ: ライブ変換結果（無ければ読み）を全確定（Enter の候補非表示枝と同一）。
    /// - idle（合成も候補表示も無い）: no-op。
    /// - ctx が無い: 確定先の文書が無いので放棄リセットで一括に畳む（取り残し防止）。
    ///   I-2(トグル中止)の例外 — この枝は常に true を返し、文書の composition を残した
    ///   まま後続へ進め得る(回収はホスト側の composition 終了に任せる)。
    ///   巡4 T3(d): 戻り値は確定の成否（idle/放棄は true=後続を進めてよい）。呼び出し側は
    ///   false なら後続処理（Shift 直打ち等）を止めること — composition が文書に残るため。
    pub(crate) fn settle_active_input(&self, ctx: Option<&ITfContext>, source: &str) -> bool {
        let composing = self.state.borrow().composing;
        let showing = self.showing.get();
        let end_pending = self.composition_end_pending.get();
        if !input_needs_settle(composing, showing, end_pending) {
            // C-1: idle でも settle が呼ばれた以上、直前確定への武装は残さない
            // （「Space→無変換トグル」直後の direct モード誤発火を塞ぐ）。
            self.disarm_undo();
            return true;
        }

        // SetText 成功後の EndComposition だけが保留された状態は、InputState 上は idle でも
        // 文書には composition が残る。mode toggle / Shift 直打ちより先に、保存済み owner
        // context 上で本文へ触れない close-only を完了させる。ctx=None の preserved-key
        // 経路でも owner context があれば回収できる。拒否時は境界を越えず再押下に任せる。
        if end_pending {
            let retry_ctx = self
                .composition_end_context
                .borrow()
                .clone()
                .or_else(|| ctx.cloned());
            let Some(retry_ctx) = retry_ctx else {
                self.abandon_pending_composition_end("settle_context_missing");
                tip_log(&format!(
                    "ev=settle source={source} quarantine=pending_context_missing"
                ));
                self.disarm_undo();
                return true;
            };
            if !self.finish_pending_composition(&retry_ctx) {
                self.disarm_undo();
                return false;
            }
            // A pending-only state has now become genuinely idle.  Do not run a second
            // CommitText session with empty logical input.
            if !composing && !showing {
                // commit_and_reset は pending 中の ephemeral exit を延期する。close が本当に
                // 完了したこの安全点で direct 復帰を再開する。
                self.exit_ephemeral_to_direct(Some(&retry_ctx));
                self.disarm_undo();
                return !input_needs_settle(
                    self.state.borrow().composing,
                    self.showing.get(),
                    self.composition_end_pending.get(),
                );
            }
        }
        let Some(ctx) = ctx else {
            self.reset_abandoned_composition();
            self.disarm_undo();
            return true;
        };
        tip_log(&format!("ev=settle source={source}"));
        // 巡4 T3(d) + 巡5 GLM I-1 + 巡10(round10): 戻り値は「settle 後に合成/候補が
        // 残っていないか」の最終状態判定 — clause/cand/部分確定の全枝で do_commit 拒否なら
        // composing が文書に残るため false になり、呼び出し側の後続処理を止める。個別枝の
        // 戻り値伝播よりも「最終状態で判定」の方が抜けを作らない。試みの有無(attempted)の
        // ゲートは「確定を一度も試みていないのに composing/showing が残る」不変条件破れを
        // 成功扱いにする抜けだったので撤去(idle は上方で早期 return 済み)。
        // 文節ナビゲーション中の settle は全文節の確定（VK_RETURN の文節枝と同じ理由 —
        // cand_state の index は選択文節の候補添字で、Commit{index} へは流せない）。
        if self.showing.get() && self.clause_nav.borrow().is_some() {
            self.commit_clauses(ctx);
            self.disarm_undo();
            return !input_needs_settle(
                self.state.borrow().composing,
                self.showing.get(),
                self.composition_end_pending.get(),
            );
        }
        // 候補表示中: 選択中の候補を確定（VK_RETURN の候補枝と同じ読み元・同じ経路）。
        let cand_pick = if self.showing.get() {
            let st = self.cand_state.borrow();
            st.resolve_commit(st.selected())
        } else {
            None
        };
        if let Some((index, text)) = cand_pick {
            self.commit_candidate(ctx, index, &text);
        }
        // 候補確定が部分確定だった場合・候補非表示の場合とも、composition が残っていれば
        // VK_RETURN の候補非表示枝と同一の「ライブ変換結果（無ければ読み）」で全確定する。
        // 巡10(round10): 候補 FullReset 拒否(drop_engine 済み)でここに来た場合、
        // engine_live_convert は None なので plan_live_enter は live_text(最後のライブ変換
        // 結果。ライブ OFF 時は読み)→空なら読み、の順で素材を拾う — 選択中の候補ではなく
        // 表示済み文字列が確定され得るが、preedit を閉じる手段を失う(確定を試みない)
        // よりは良い(稀経路: 同一コンテキストで1発目拒否・2発目成功)。
        if self.state.borrow().composing {
            // ライブ変換 OFF / 表記固定中は engine のライブ変換を参照しない
            // （Enter の VK_RETURN 枝と同じ規律）— 表示中の live_text をそのまま確定して畳む。
            let live = if self.should_consult_live_engine() {
                let seq = self.state.borrow_mut().bump_live_seq();
                // auto_commit=false: settle は続けて commit_and_reset で全確定するため
                // （エンジンに読みを消費させると確定文字列から prefix が欠ける）。
                self.engine_live_convert(seq, false).map(|(t, _, _)| t)
            } else {
                None
            };
            let live_text = self.live_text.borrow().clone();
            let last_reading = self.last_reading.borrow().clone();
            // Why not(Enter と同じく EngineCommit を engine_commit(0) へ通す): settle は
            // モードトグル/カーソル移動に付随する暗黙確定。2 度目のエンジン往復は失敗経路を
            // 増やすだけなので、素材の優先順位だけを Enter と共有し（`plan_live_enter`）
            // 確定は常に直確定にする。成否は戻り値で呼び出し側が判定する（巡5 GLM I-1）。
            let text = match plan_live_enter(live, &live_text, &last_reading) {
                LiveEnterPlan::EngineCommit { text } | LiveEnterPlan::DirectCommit { text } => text,
            };
            let _ = self.commit_and_reset(ctx, &text, source, None);
        }
        // C-1: settle は候補確定（source="candidate" 経由で commit_candidate→commit_and_reset が
        // armed を立てうる）を含め、armed を残さない。settle_active_input を通った確定は
        // 「Space→無変換トグル」等の設計ロック対象外イベントの一部なので、末尾で必ず解除する。
        self.disarm_undo();
        // SetText は成功して logical state が idle でも、EndComposition の close-only が
        // 新たに pending になった場合は settle 未完了。fresh な三状態で判定し、mode toggle
        // や Shift 直打ちを物理 composition の終了前に進めない。
        !input_needs_settle(
            self.state.borrow().composing,
            self.showing.get(),
            self.composition_end_pending.get(),
        )
    }

    /// 変換（henkan）本体。Space と native モードの VK_CONVERT(再変換キー) で共有する。
    /// 候補表示中なら選択を1つ進め、composition 中ならエンジンで変換して候補窓を出す。
    /// idle（composition でも showing でもない）なら何もしない。
    /// borrow は短命に取る（`composing` を読むだけで即解放し、ヘルパ呼び出しをまたがない）。
    pub(crate) fn trigger_convert(&self, ctx: &ITfContext) {
        if self.showing.get() {
            // 次の候補へ（↓ と同じ動き — preedit の描き直しも move_candidate が担う）。
            self.move_candidate(ctx, 1);
            return;
        }
        let composing = self.state.borrow().composing;
        if composing {
            self.disarm_debounce();
            match self.engine_convert() {
                Some(cands) if !cands.is_empty() => {
                    // 確定文字列の唯一の真実源は cand_state（show() が set する）。別途持たない。
                    self.showing.set(true);
                    // 候補窓を開いた時点で preedit を選択候補へ揃える（show_reconvert_candidates と
                    // 同じ形）。候補確定は幅を変えない契約なので、ここは widen を通さない生の候補。
                    // これが無いと、数字を全角表示した preedit のまま半角候補を確定して
                    // 「見えている幅と確定の幅が違う」が候補経路で再発する。
                    self.run_preedit(ctx, &cands[0]);
                    let anchor = self.caret_point(ctx);
                    // Task 7: 表示ごとに settings/ダークモードを再評価した Theme を渡す。
                    let theme = self.appearance.borrow_mut().current_theme();
                    self.candidate_ui
                        .borrow_mut()
                        .show(&cands, 0, anchor, theme);
                    self.reading_monitor.borrow_mut().hide();
                    let list = cands.join("|");
                    tip_log(&format!(
                        "ev=candidates_shown n={} sel=0 list={}",
                        cands.len(),
                        list
                    ));
                }
                // エンジン失敗/空: preedit はそのまま（ハングさせない）。
                _ => {}
            }
        }
    }

    /// 修正変換（Tab）本体。読みのタイポ修復候補を要求し候補窓に出す。showing 中なら次の候補へ
    /// 進めるだけ（move_candidate と同じ動き — trigger_convert と対称）。idle は何もしない。
    pub(crate) fn trigger_typo_convert(&self, ctx: &ITfContext) {
        if self.showing.get() {
            self.move_candidate(ctx, 1);
            return;
        }
        let composing = self.state.borrow().composing;
        if composing {
            self.disarm_debounce();
            match self.engine_typo_convert() {
                Some(cands) if !cands.is_empty() => {
                    self.showing.set(true);
                    self.run_preedit(ctx, &cands[0]); // trigger_convert と同じ理由（幅の一致）
                    let anchor = self.caret_point(ctx);
                    let theme = self.appearance.borrow_mut().current_theme();
                    self.candidate_ui
                        .borrow_mut()
                        .show(&cands, 0, anchor, theme);
                    self.reading_monitor.borrow_mut().hide();
                    let list = cands.join("|");
                    tip_log(&format!(
                        "ev=typo_candidates_shown n={} sel=0 list={}",
                        cands.len(),
                        list
                    ));
                }
                // エンジン失敗/空: preedit はそのまま（ハングさせない）。
                _ => {}
            }
        }
    }

    /// Shift 押下時の一時直接入力（打鍵作法 Task5 改）。素の（大文字）ASCII 文字 `ch` を
    /// composition を張らず直接確定する（Google/ATOK の「Shift で一時的に直接入力」）。
    /// 開いている合成/候補があれば先に settle して畳む（MS-IME/Google の「Shift で暗黙確定して
    /// から直接入力」）。読みの無い直接確定なので undo 武装はしない
    ///（remember_last_commit を通さない）。commit は composition の無い do_commit 枝
    /// （InsertTextAtSelection＋末尾 SetSelection）で 1 発挿入し、連続 Shift 打鍵の順序も
    /// キャレット末尾追従で保たれる。
    pub(crate) fn commit_char_direct(&self, ctx: &ITfContext, ch: char) -> Result<BOOL> {
        if input_needs_settle(
            self.state.borrow().composing,
            self.showing.get(),
            self.composition_end_pending.get(),
        ) {
            // 巡4 T3(d) + 巡10(round10): settle の確定が拒否されたら直打ちを挿入しない —
            // composition が文書に残っているので、このまま CommitText を流すと合成中文字列を
            // 1 文字で置換して消す。キーは TRUE で食いて再押下の再試行に任せる(モードトグルと同じ)。
            // Why not(FALSE でホストの自前挿入に救済): TestKeyDown は A–Z を常に TRUE と
            // 宣言するため FALSE 返しは Test/実の eaten 食い違いとなり、Test を信じた
            // ホストで打鍵が失われる(item19 の教訓)。自前挿入が動いても composition
            // 残存への挿入は preedit を破壊し得る。
            if !self.settle_active_input(Some(ctx), "shift_latin") {
                tip_log("ev=shift_latin skip=settle_rejected");
                return Ok(TRUE);
            }
        }
        let text = ch.to_string();
        let fields = commit_fields(None, 0, "", &text, self.is_direct_mode());
        tip_log(&format!(
            "ev=commit text={text} source=shift_latin {fields}"
        ));
        // 巡3 P3: 挿入拒否は文字が挿入されないだけ（状態を畳むものが無い）— FALSE で返して
        // ホストの自前挿入に救わせる（旧実装は Ok(TRUE) で打鍵が握りつぶされていた）。
        // 巡11(round11): settle 拒否枝だけ TRUE で食べるのは composition 残存の差 — この枝は
        // settle 成功後(=composition 無し)なので FALSE による自前挿入の救済が安全に働く。
        // Test を信じて FALSE を無視するホストでは打鍵が失われ得る(item19)が、P3 の実測
        // (TRUE での握りつぶし)とトレードオフの設計判断としてここは FALSE を維持する。
        if !self.do_commit(ctx, &text) {
            tip_log("ev=commit_rejected source=shift_latin");
            return Ok(FALSE);
        }
        Ok(TRUE)
    }

    /// 記号打鍵の実処理。全角へ写して読みへ畳み込む — idle なら composition を開始する。
    /// text_vk アーム(OEM 記号)と VK_1..=VK_9 アーム(Shift+数字行、記号トグル ON 時)が共有する
    /// — 呼び出し条件は is_symbol_keystroke(eaten 一致 = item19 のため gated と同一述語)。
    ///
    /// Why not(idle=全角直接確定 — 旧・打鍵作法 Task3): 句読点から打ち始めると composition が
    /// 張られず、続く打鍵と同一 composition で変換できない・表記変換(F10 等)も一切効かない
    /// (実機報告 2026-08-03)。idle 数字が合成を開始するのとも非対称だった。MS-IME/Google と
    /// 同じく idle でも合成開始へ揃える。表に無い文字(トグル OFF の '/' 等)も同様に半角のまま
    /// 読みへ積んで合成開始する — 句読点だけ特別扱いすると「どの記号が合成に入るか」が
    /// 設定依存で割れ、続く打鍵との同一 composition 変換(この修正の動機)も記号種で欠ける
    /// (ユーザー判断 2026-08-03: 記号全般を合成開始に)。
    ///
    /// 必ず食い切る: 表に無い文字(トグル OFF の '=' や '\' 等)を FALSE で素通すと
    /// Test/実の eaten が食い違い、OnTestKeyDown=TRUE を信じたホストで打鍵が失われる
    /// (item19 の教訓)。よって写せない文字はその文字のまま読みへ積む。
    fn symbol_keydown(&self, ctx: &ITfContext, vk: u32, lparam: LPARAM) -> Result<BOOL> {
        let punct = self.punctuation_full_width.get();
        let symbol = self.symbol_overlay.get();
        let chars = self.symbol_chars.get();
        match key_to_char(vk, lparam) {
            // 全角へ写して読みへ畳み込む（`-` と全記号を同仕様に — 設計 §C）。
            // テンキー記号は is_symbol_keystroke=false でここに来ない（`.` は literal、`-` はー化）。
            Some(ch) => {
                let folded = zenkaku_symbol(ch, punct, symbol, chars).unwrap_or(ch);
                // 呼び出し側が英語モードを先に分岐させるので、ここは常にかな読みへの畳み込み。
                self.input_char(ctx, folded, InsertStyle::Kana)
            }
            // L-3: 印字不能キー（デッドキー/合字）は食って無視（stray char 漏れ防止）。
            None => Ok(TRUE),
        }
    }

    /// 1文字 `ch` を入力としてエンジンへ送り、読みを即 preedit 表示してデバウンス変換を仕込む。
    /// A–Z でも記号/数字でも共通。候補窓が出ていれば閉じてライブ入力へ戻す。
    pub(crate) fn input_char(
        &self,
        ctx: &ITfContext,
        ch: char,
        style: InsertStyle,
    ) -> Result<BOOL> {
        if self.showing.get() {
            self.candidate_ui.borrow_mut().hide();
            self.showing.set(false);
            self.clear_clause_nav();
        }
        // 喪失判定は ensure_engine より**前**に行う: drop_engine 後の再接続は
        // ensure_engine 内の start_and_store が StartSession まで済ませて engine_session を
        // 非0にするため、ensure_session は「新規作成」を検知できず false を返す
        // （この経路が fac6315 の盲点 — item24 ヘッドレス再現で崩壊が残ることを実測済み）。
        // 「session==0 かつ raw 非空」は commit/cancel/放棄/Deactivate 後にはあり得ない
        // 組合せ（それらは必ず raw を clear する — needs_session_reseed の doc 参照）ので、
        // 合成途中のエンジン喪失と同値。
        let lost_mid_composition =
            needs_session_reseed(self.engine_session.get(), &self.state.borrow().raw);
        self.ensure_engine();
        let session_created = self.ensure_session();
        match style {
            InsertStyle::Direct => self.state.borrow_mut().on_char_latin(ch),
            InsertStyle::Kana => self.state.borrow_mut().on_char(ch),
        };
        *self.current_context.borrow_mut() = Some(ctx.clone());
        // セッションを今作った（＝engine 側の読みが空）なら raw 全体を送り直す。
        // composition 継続中の drop_engine（ライブ変換タイムアウト等）からの復帰打鍵で、
        // 新セッションに新規1文字だけを入れると preedit が積み上げた読みごと 1 文字に
        // 置き換わる（22 文字打鍵→23 文字目で全部消えるデータロス）。raw は打鍵の全履歴
        // （部分確定後は残り読みのかな）を保持しているので、replay で読みが完全復元される。
        // 新規 composition では raw == ch 1 文字なので従来とワイヤ等価。
        // リプレイは raw をかな部/英語部で style 分割して順送する（split_replay）。複数区間の
        // 途中失敗は最終 Insert の結果だけを見る — 部分成功を巻き戻す経路は無く、None 劣化
        // （raw 表示）は単発失敗時と同じ挙動に収束するため。
        let segments: Vec<(String, InsertStyle)> = if session_created || lost_mid_composition {
            let st = self.state.borrow();
            if lost_mid_composition {
                // 実機受入で「復旧が発火した」ことを確認するための診断（NOSPACEKEY_LOG ゲート内）。
                // len はリプレイ payload の長さ＝喪失していた raw + 今回の打鍵1文字（M-3）。
                tip_log(&format!("ev=session_reseed len={}", st.raw.chars().count()));
            }
            crate::input_state::split_replay(&st.raw, st.latin_from)
        } else {
            vec![(ch.to_string(), style)]
        };
        let mut inserted = None;
        for (seg, seg_style) in &segments {
            inserted = self.engine_insert(seg, *seg_style);
        }
        let reading = match inserted {
            Some(r) => {
                self.state.borrow_mut().mark_good(&r);
                *self.last_reading.borrow_mut() = r.clone();
                r
            }
            None => {
                let degraded = self.state.borrow_mut().degraded_reading();
                *self.last_reading.borrow_mut() = degraded.clone();
                degraded
            }
        };
        *self.live_text.borrow_mut() = reading.clone();
        // 表示だけ全角化する。live_text/last_reading/raw は半角 canonical のまま置く —
        // これらは劣化フォールバックや確定取消のリプレイでエンジンへ戻る素材だから。
        self.run_preedit(ctx, &self.widen_display_text(&reading));
        self.arm_debounce();
        Ok(TRUE)
    }

    /// 品質ループ③: 直前確定バッファ（誤変換ワンキー記録の対象）を保存する。commit サイトが
    /// **状態クリア前に** ev=commit と同じ採取材料で呼ぶ。かな変換系の確定
    /// （commit_and_reset / apply_commit_plan / apply_live_auto_commit）のみが対象で、
    /// shift_latin の直接確定は**意図的に対象外**（読みが無く「誤変換」の概念が成立しない）。
    /// idle 記号は 2026-08-03 の仕様変更（直接確定→合成開始）で通常の commit 経路に乗り、
    /// 対象**内**になった — 「。」だけの確定も undo/feedback の対象になるのは、記号 composition
    /// を通常の合成と区別しない一様性の帰結（旧・対象外の理由づけは直接確定経路ごと消滅）。
    fn remember_last_commit(
        &self,
        reading: &str,
        text: &str,
        source: &str,
        sel: Option<usize>,
        cand_n: usize,
    ) {
        // F-5 改定（確定取消）: opt-in（settings.feedback.enabled）に加えて、この確定が undo
        // 武装対象（arms_undo(source)）なら保存する。常時保存にはしない — mode_toggle/navigate/
        // prefix 系確定は既定ユーザで従来どおり保持ゼロ（I-2）。非武装化した時点で feedback も
        // 無効なら disarm_undo がクリアする（メモリに確定文字列を無期限保持しない）。
        if !self.feedback_enabled.get() && !arms_undo(source) {
            return;
        }
        *self.last_commit.borrow_mut() = Some(crate::text_service::LastCommit {
            ts_ms: crate::text_service::epoch_ms(),
            reading: reading.to_string(),
            text: text.to_string(),
            source: source.to_string(),
            sel: sel.map(|s| s as i32).unwrap_or(-1),
            cand_n,
        });
    }

    /// preedit 表示文字列を、既定確定と同じ規則で全角化する（設計 2026-07-09 §4「表示（preedit）整合」）。
    /// **表示のみ**の変換で、変換/候補生成に使う内部の読み（半角 canonical）は書き換えない
    /// — 読みを全角にするとエンジンの両幅候補生成が片方向で潰れ、半角候補が選べなくなるため。
    /// 呼ぶのは「既定確定の素材をそのまま映している preedit」だけ。候補プレビュー
    /// （再変換/確定取消/変換の `run_preedit(cands[0])`）には掛けない — 候補は選んだ幅が正だから。
    /// Why not(LLM 経路にも掛ける): `LLM_CONVERT_FROZEN` で到達不能なため意図的に対象外。
    /// 凍結を解除するときは `restore_pre_llm` の復元 preedit も包むこと（幅が戻ってしまう）。
    /// 既知の受容: `latin`/`notation_fixed` は composition 単位のフラグなので、合成途中で英語モードへ
    /// 入ると preedit の数字が全角→半角へ反転して見える。確定も同じ判定を通るので表示＝確定は保たれる。
    pub(crate) fn widen_display_text(&self, text: &str) -> String {
        self.widen_commit_text(text, "live")
    }

    /// ④: 既定確定の確定文字列を、数字全角設定に従って全角化する（候補選択 source は不変）。
    fn widen_commit_text(&self, text: &str, source: &str) -> String {
        // borrow はこのブロックで閉じる。下の is_direct_mode() は COM 往復なので、
        // 借用を跨がせると再入時に BorrowMutError で落ちる。
        let (latin, notation_fixed) = {
            let st = self.state.borrow();
            (st.latin_mode(), st.notation_fixed)
        };
        if should_widen_digits(
            self.number_full_width.get(),
            self.is_direct_mode(),
            latin,
            notation_fixed,
            source,
        ) {
            to_zenkaku_digits(text)
        } else {
            text.to_string()
        }
    }

    /// `text` を確定し、composition/候補/状態/タイマを片付ける（Enter・数字選択 共通）。
    /// `sel` は候補確定時の実確定 index（品質ループ②/③ — ライブ/settle 確定は None）。
    /// 巡4 T3: 戻り値は確定の成否。false では文書未挿入のまま composition が残るため、
    /// 呼び出し側は後続処理（settle 後の直打ち・訂正通知）を止めること。
    pub(crate) fn commit_and_reset(
        &self,
        ctx: &ITfContext,
        text: &str,
        source: &str,
        sel: Option<usize>,
    ) -> bool {
        self.disarm_debounce();
        // ④: 既定確定（候補選択でない）はかなモード全角設定に従い数字を全角化する。
        // 以降のログ/remember/do_commit はすべて widened を使う（shadowing）。
        let widened = self.widen_commit_text(text, source);
        let text = widened.as_str();
        // 品質ループ②: 構造化フィールドはクリア**前**に採取する（reading/候補数はこの後の
        // reset/hide で消える）。cand_n は候補確定時のみ意味を持つ（ライブ確定は 0）。
        let cand_n = if sel.is_some() {
            self.cand_state.borrow().count()
        } else {
            0
        };
        let reading = self.last_reading.borrow().clone();
        let fields = commit_fields(sel, cand_n, &reading, text, self.is_direct_mode());
        tip_log(&format!("ev=commit text={text} source={source} {fields}"));
        // 巡3 P3 + 巡4 T3: CommitText セッションが拒否された（TF_E_LOCKED 等）ら状態を畳まない —
        // 文書へは何も書かれていないのに composition/候補/セッションを破棄すると確定文字が
        // 消失する。remember_last_commit / undo 武装も do_commit の**後**に実行する
        // （先行更新だと文書に存在しない「幽霊確定」が Ctrl+変換の記録に混入する）。
        if !self.do_commit(ctx, text) {
            tip_log("ev=commit_rejected source=do_commit");
            return false;
        }
        // 品質ループ③: 誤変換ワンキー記録用の直前確定バッファ（同じ採取材料を流用）。
        // 巡12(round12): 空 text（空 BS cancel 拒否巻き戻し中の Enter=cancel 代わり）は
        // 確定書類を残さない — remember_last_commit の空レコード(feedback への混入)と
        // arms_undo による空確定武装(直後の Ctrl+Backspace が text_mismatch disarm まで
        // 食われる)を防ぐ。cancel 成功経路も書類を残さず対称。
        let keep_records = commit_keeps_records(text);
        if keep_records {
            self.remember_last_commit(&reading, text, source, sel, cand_n);
        }
        // 確定取消: 全消費して composition を畳む確定（candidate/live）だけ武装する。
        // apply_commit_plan の PartialReseed / apply_live_auto_commit の部分確定枝は
        // commit_and_reset を経由しない（=ここを通らないので自然に武装しない）。
        if keep_records && arms_undo(source) {
            self.undo_armed.set(true);
        }
        self.engine_end_session();
        self.state.borrow_mut().reset();
        self.reconverting.set(false);
        self.live_text.borrow_mut().clear();
        // pending EndComposition の所有 context は専用 slot が保持する。通常の current_context は
        // 確定済み状態へ持ち越さず、次の入力文書と混同しない。
        *self.current_context.borrow_mut() = None;
        self.candidate_ui.borrow_mut().hide();
        self.reading_monitor.borrow_mut().hide();
        self.showing.set(false);
        self.clear_clause_nav();
        // U9: 合成終了 — 次 composition の再捕捉まで前文書の左文脈を残さない（stale 残留防止）。
        *self.left_context.borrow_mut() = None;
        // 読みキャッシュ: 合成終了の全経路+PartialReseed でクリア（U9 左文脈と同じ規律。
        // engine_end_session へはフックしない — timeout→drop_engine の劣化経路は合成継続中で、
        // そこで消すと表示が不連続になる）。
        self.monitor_committed_reading.borrow_mut().clear();
        // ephemeral かな: composition を物理的にも畳めたときだけ direct へ復帰する。
        // SetText 済みでも EndComposition が pending なら marker/mode を維持し、キー入口の
        // close-only 障壁または OnCompositionTerminated が回収してから復帰する。
        // PartialReseed/live_auto の部分確定枝はここを通らない＝composition 継続で ephemeral 維持。
        if self.composition_end_pending.get() {
            tip_log("ev=ephemeral_exit deferred=pending_end");
        } else {
            self.exit_ephemeral_to_direct(Some(ctx));
        }
        true
    }

    /// 候補(index)を確定する。`resolved_text` は cand_state で解決済みの確定文字列（index と一致）。
    /// 前方一致候補ならエンジンが残り読みを返すので **部分確定**し、残り読みで composition を継続して
    /// エンジンセッションを保持する（前方一致候補のデータロス対策）。全消費・エンジン失敗・再変換中は
    /// 従来どおりの **全確定**（`commit_and_reset`）でバイト等価。
    pub(crate) fn commit_candidate(&self, ctx: &ITfContext, index: usize, resolved_text: &str) {
        self.disarm_debounce();
        // 再変換中の確定は対象外（g1 リプレイ由来の別セッション）。従来確定へフォールバック。
        if self.reconverting.get() {
            let reading = self.reconvert_reading.borrow().clone();
            // 巡4 T3(e): 確定拒否時は訂正通知（学習記録）を飛ばさない — 文書に存在しない
            // 確定に対する記録が学習データを汚染する。
            if self.commit_and_reset(ctx, resolved_text, "candidate", Some(index))
                && crate::text_service::should_record_correction(index, &reading)
            {
                // 訂正通知は確定完了後(ユーザ可視の確定を待たせない)。確定契約(Commit IPC 迂回・
                // 直接挿入)は不変で、これは記録専用の別 op。resolved_text を widen 前の値で
                // 記録できるのは should_widen_digits が source=="candidate" を除外している前提
                // (除外を外すなら widen 後の文字列を送ること — 確定本文と記録表層の一致が契約)。
                self.engine_record_correction(&reading, resolved_text);
            }
            return;
        }
        let plan = plan_commit(self.engine_commit(index), resolved_text);
        self.apply_commit_plan(ctx, plan, "candidate", "candidate_prefix", Some(index));
    }

    /// 文節ナビゲーション中の確定（Enter / settle / ホスト Finalize）。エンジンの CommitClauses が
    /// 文節ごとに学習して全文節の連結を返す。劣化時（旧エンジン/接続断）は表示中ビューの連結を
    /// 直確定する — 学習に乗らないだけで、表示＝確定の一致は保たれる。常に全確定
    /// （文節候補は全被覆のみ＝残り読みは構造的に無い）なので commit_and_reset へ。
    /// source は settle 発でも "clause" 固定 — settle の候補確定が source="candidate" になるのと
    /// 同じ規律（幅の契約 should_widen_digits と undo 武装 arms_undo が source 列挙で判定するため、
    /// 呼び出し契機で名前が揺れると両方が静かに壊れる）。
    pub(crate) fn commit_clauses(&self, ctx: &ITfContext) {
        let source = "clause";
        let fallback = self
            .clause_nav
            .borrow()
            .as_ref()
            .map(|n| n.segments.concat())
            .unwrap_or_default();
        let text = self
            .engine_commit_clauses()
            .map(|(t, _)| t)
            .unwrap_or(fallback);
        if text.is_empty() {
            // 防御: ビュー無しで呼ばれた（呼び出し側の clause_nav ガードが破れた）場合のみ。
            // 空文字の確定は composition を只の空置換で畳むだけなので何もしない方が安全。
            tip_log("ev=clause_commit skip=empty");
            return;
        }
        tip_log(&format!("ev=clause_commit text={text}"));
        // 巡4 T3(a): clauses はエンジンの学習往復(CommitClauses)が済んだ後 — 挿入拒否時に
        // エンジン側だけ文脈が進んだまま残すと次入力とズレるので、接続を作り直して
        // needs_session_reseed の全リプレイで自己修復させる（partial/live_auto と同じ規律）。
        if !self.commit_and_reset(ctx, &text, source, None) {
            self.drop_engine();
        }
    }

    /// plan_commit の結果を composition へ適用する（候補確定とライブ確定で共有 — Spec2）。
    /// `full_source`/`prefix_source` は ev=commit の source ラベル
    /// （"candidate"/"candidate_prefix" と "live"/"live_prefix"）。
    /// `sel` は候補確定時の実確定 index（品質ループ② — ライブ確定は None）。
    fn apply_commit_plan(
        &self,
        ctx: &ITfContext,
        plan: CommitPlan,
        full_source: &str,
        prefix_source: &str,
        sel: Option<usize>,
    ) {
        match plan {
            CommitPlan::PartialReseed { prefix, remaining } => {
                // ④: 部分確定の prefix も既定確定なら数字を全角化（candidate_prefix は不変）。
                let prefix = self.widen_commit_text(&prefix, prefix_source);
                // 品質ループ②: クリア前に採取（last_reading はこの後 remaining へ上書きされる）。
                let cand_n = if sel.is_some() {
                    self.cand_state.borrow().count()
                } else {
                    0
                };
                let reading = self.last_reading.borrow().clone();
                let fields = commit_fields(sel, cand_n, &reading, &prefix, self.is_direct_mode());
                tip_log(&format!(
                    "ev=commit text={prefix} source={prefix_source} remaining={remaining} {fields}"
                ));
                // do_commit の合成終了が（ホスト依存で）OnCompositionTerminated を誘発しても
                // エンジンセッションを畳まないようガードする。残り読みのセッションは保持する。
                self.partial_committing.set(true);
                // 巡3 P3: prefix 挿入が拒否されたら reseed しない — 文書へ書かれていない
                // prefix を確定済み扱いで残り読みへ進むと文字が消失する。composition は
                // CommitText 未実行で生きており、ユーザの再選択に任せる。
                if !self.do_commit(ctx, &prefix) {
                    self.partial_committing.set(false);
                    tip_log("ev=commit_rejected source=partial");
                    // 巡4 T3(a): エンジン側は既に prefix を消費済み — このまま既存セッションを
                    // 使い続けると次入力が読み欠落する。drop_engine して次打鍵の
                    // needs_session_reseed 全リプレイで自己修復させる。
                    self.drop_engine();
                    return;
                }
                // 巡5 GLM I-4: remember_last_commit は挿入成功後に — 拒否時 return の後ろへ
                // 移動（文書に存在しない prefix の学習記録混入を防ぐ。commit_and_reset と対称）。
                // 品質ループ③: 部分確定も直前確定として記録対象（reading は消費前の全読み）。
                self.remember_last_commit(&reading, &prefix, prefix_source, sel, cand_n);
                // エンジンセッションは保持（engine_end_session を呼ばない）。残り読みで継続する。
                self.state
                    .borrow_mut()
                    .reseed_after_partial_commit(&remaining);
                self.reconverting.set(false);
                *self.last_reading.borrow_mut() = remaining.clone();
                self.monitor_committed_reading.borrow_mut().clear();
                *self.live_text.borrow_mut() = remaining.clone();
                *self.current_context.borrow_mut() = Some(ctx.clone());
                self.cand_state.borrow_mut().set(Vec::new(), 0); // 古い(全読み)候補を破棄
                self.candidate_ui.borrow_mut().hide();
                self.showing.set(false);
                self.clear_clause_nav();
                // 残り読みで新しい composition を張る。旧 composition の close がまだ pending
                // なら、終了 callback / 次打鍵 / debounce のいずれかの安全点まで再描画を保持する。
                self.partial_preedit_redraw_pending.set(true);
                self.partial_preedit_redraw_retries.set(0);
                let redrawn = self.redraw_partial_preedit_if_needed(ctx);
                self.partial_committing.set(false); // 張り替え完了。以降の app 都合終了は通常処理。
                if redrawn {
                    self.arm_debounce(); // 残り読みのライブ変換を再開
                } else {
                    self.arm_partial_preedit_redraw_retry();
                }
            }
            CommitPlan::FullReset { text } => {
                // 全消費 or エンジン失敗: 従来どおり全確定（engine_end_session も呼ばれる）。
                // 巡4 T3(a): 挿入拒否時はエンジン側の読み消費済みの可能性（候補経由）—
                // drop_engine して次打鍵の needs_session_reseed 全リプレイで自己修復させる。
                if !self.commit_and_reset(ctx, &text, full_source, sel) {
                    self.drop_engine();
                }
            }
        }
    }

    /// ライブ変換の自動確定（iOS nospacekey の先頭文節自動確定の再現）を composition へ適用する。
    /// エンジンは LiveConvert{auto_commit:true} の応答時点で既に先頭文節分の読みを消費済み
    /// （ComposingText.prefixComplete 実行済み）なので、ここでは engine_commit を呼ばず、
    /// TIP 側の確定挿入と残り読みへの reseed だけを行う（apply_commit_plan::PartialReseed と
    /// 同じ規律。違いはエンジン側の状態遷移が済んでいることだけ）。
    /// `prefix` = 確定する先頭文節、`text` = 残り読みのライブ変換結果、`reading` = 残り読み。
    /// reading が空（全消費 — 稀だが iOS でも起きる正当ケース）なら全確定と同じ片付けに落とす。
    pub(crate) fn apply_live_auto_commit(
        &self,
        ctx: &ITfContext,
        prefix: &str,
        text: &str,
        reading: &str,
    ) {
        if reading.is_empty() {
            // 全消費: エンジン側の読みは空。従来の全確定と同じ片付け（セッションも畳む）。
            // 巡4 T3(a): 挿入拒否時はエンジン側が消費済み — drop_engine で自己修復に落とす。
            if !self.commit_and_reset(ctx, prefix, "live_auto", None) {
                self.drop_engine();
            }
            return;
        }
        // ④: 部分自動確定の prefix も既定確定なので数字を全角化（source="live_auto"）。
        let prefix = self.widen_commit_text(prefix, "live_auto");
        let prefix = prefix.as_str();
        // 品質ループ②: 自動確定は候補選択でない（sel=-1 cand_n=0）。rlen はこの時点の
        // last_reading（=消費前の全読み。この後 remaining へ上書きされる）。
        let full_reading = self.last_reading.borrow().clone();
        let fields = commit_fields(None, 0, &full_reading, prefix, self.is_direct_mode());
        tip_log(&format!(
            "ev=commit text={prefix} source=live_auto remaining={reading} {fields}"
        ));
        // do_commit の合成終了が（ホスト依存で）OnCompositionTerminated を誘発しても
        // エンジンセッションを畳まないようガードする（apply_commit_plan と同じ）。
        self.partial_committing.set(true);
        // 巡3 P3: 挿入拒否時は reseed しない（エンジン側の読み消費との整合は既に失われるが、
        // 文書未挿入の prefix を確定扱いで進めると文字消失になる — composition を残す方が救える）。
        if !self.do_commit(ctx, prefix) {
            self.partial_committing.set(false);
            tip_log("ev=commit_rejected source=live_auto");
            // 巡4 T3(a): エンジン側は消費済み — partial 枝と同じ drop_engine 自己修復。
            self.drop_engine();
            return;
        }
        // 巡5 GLM I-4: remember_last_commit は挿入成功後に（部分確定枝と対称）。
        // 品質ループ③: ライブ自動確定も直前確定として記録対象。
        self.remember_last_commit(&full_reading, prefix, "live_auto", None, 0);
        // エンジンセッションは保持（読みは消費済みで残り読みと同期している）。
        self.state.borrow_mut().reseed_after_partial_commit(reading);
        self.reconverting.set(false); // 部分確定で composition を張り替えた（apply_commit_plan と同じ）
                                      // 読みキャッシュ: 追記は last_reading を remaining へ縮める行と run_preedit（表示更新）
                                      // より前が契約 — 後に置くと自動確定フレームだけ表示が consumed ぶん縮んで戻る
                                      // 「跳ね」になる（spec 順序契約）。サフィックス不成立は追記スキップ（欠落は Enter まで
                                      // 恒久だが壊れない）。skip ログは発生観測用（通常入力で出ないことが受入条件）。
        if self.reading_monitor_accumulate.get() {
            match crate::reading_monitor::consumed_reading(&full_reading, reading) {
                Some(consumed) => crate::reading_monitor::append_committed(
                    &mut self.monitor_committed_reading.borrow_mut(),
                    consumed,
                    crate::reading_monitor::display_bound(self.reading_monitor_max_chars.get()),
                ),
                None => tip_log("ev=reading_monitor accumulate=skip"),
            }
        }
        *self.last_reading.borrow_mut() = reading.to_string();
        let display = if text.is_empty() { reading } else { text };
        // 直前の reseed_after_partial_commit が残り読みで記録済みだが、表示は
        // ライブ変換結果でありうる — より良い表示素材で上書きする。
        self.state.borrow_mut().mark_good(display);
        *self.live_text.borrow_mut() = display.to_string();
        // 残りの読み/ライブ結果で新しい composition を張る。旧 composition の close-only が
        // 拒否された場合も残り読みを不可視のまま放置せず、PartialReseed と同じ bounded retry
        // へ送る。mark_good/live_text は半角のまま（degraded_reading の読みを汚染しない）。
        self.partial_preedit_redraw_pending.set(true);
        self.partial_preedit_redraw_retries.set(0);
        let redrawn = self.redraw_partial_preedit_if_needed(ctx);
        self.partial_committing.set(false); // 張り替え完了。以降の app 都合終了は通常処理。
        if redrawn {
            // この関数自体が単発 debounce callback から来るため、成功後も残り読みの
            // live conversion を継続する次タイマを明示的に張る。
            self.arm_debounce();
        } else {
            self.arm_partial_preedit_redraw_retry();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        backspace_route, commit_keeps_records, ephemeral_idle_abort, is_cmd_modifier, will_handle,
        will_handle_awaiting, will_handle_gated,
    };
    use crate::keymap::{resolve_action, ActionInput, KeyAction, Keymap};

    // 既存テストは全て native(ひらがな)モード＝第5引数 direct=false。
    // will_handle_gated/awaiting の action 引数は KeyAction を直接渡す（Convert/Typo/Llm/Undo/
    // Ephemeral/Notation/Reconvert を resolve_action が相互排他に解決する。ここでは outcome を直接編む）。

    /// テスト用: 既定 keymap でこの (vk, 文脈) の KeyAction を解決する（実処理と同じ source）。
    fn act(vk: u32, composing: bool, showing: bool, direct: bool) -> KeyAction {
        resolve_action(
            &Keymap::default(),
            &ActionInput {
                vk,
                ctrl: false,
                shift: false,
                alt: false,
                composing,
                showing,
                direct,
                undo_armed: false,
                ephemeral_enabled: true,
                typo_enabled: true,
                llm_enabled: true,
            },
        )
    }

    #[test]
    fn az_always_handled() {
        assert!(will_handle(0x41, false, false, false, false)); // 'A'
        assert!(will_handle(0x5A, false, false, false, false)); // 'Z'
    }

    #[test]
    fn autorepeat_is_bit30_of_lparam() {
        // 文節モード中の同期 IPC を伴うキー(←/→/↑/↓/Space/数字)は自動リピートで積み上がると
        // タイムアウト→接続断になるため、bit30 での判定を全アームで共有する。
        use super::is_autorepeat;
        use windows::Win32::Foundation::LPARAM;
        assert!(!is_autorepeat(LPARAM(0)));
        assert!(!is_autorepeat(LPARAM(1)));
        assert!(is_autorepeat(LPARAM(1 << 30)));
        assert!(is_autorepeat(LPARAM((1 << 30) | 1)));
        assert!(!is_autorepeat(LPARAM(1 << 29)));
    }

    #[test]
    fn space_is_no_longer_a_fixed_key_and_is_eaten_via_convert() {
        // Space は will_handle の固定キーから外れ、Convert アクション(henkan)でのみ食う（keymap 化）。
        // 「composing||showing×Convert 束縛一致」の判定は resolve_action へ移った（keymap.rs でテスト）。
        assert!(!will_handle(0x20, true, false, false, false)); // will_handle 単独ではもう食わない
        assert!(!will_handle(0x20, false, true, false, false));
        // native の Space は composing/showing で Convert へ解決し、gated が食う。idle は素通し。
        assert_eq!(act(0x20, true, false, false), KeyAction::Convert);
        assert_eq!(act(0x20, false, true, false), KeyAction::Convert);
        assert_eq!(act(0x20, false, false, false), KeyAction::None);
        assert!(will_handle_gated(
            0x20,
            true,
            false,
            false,
            false,
            false,
            false,
            KeyAction::Convert
        ));
        assert!(will_handle_gated(
            0x20,
            false,
            true,
            false,
            false,
            false,
            false,
            KeyAction::Convert
        ));
        assert!(!will_handle_gated(
            0x20,
            false,
            false,
            false,
            false,
            false,
            false,
            KeyAction::None
        ));
    }

    #[test]
    fn enter_digits_esc_backspace_only_when_active() {
        for vk in [0x0D, 0x1B, 0x08, 0x31, 0x39] {
            assert!(
                !will_handle(vk, false, false, false, false),
                "vk {:#x} should pass when idle",
                vk
            );
            assert!(
                will_handle(vk, true, false, false, false),
                "vk {:#x} should be handled when composing",
                vk
            );
        }
    }

    #[test]
    fn arrows_only_when_showing() {
        // ↑(0x26)/↓(0x28) は候補表示中だけ食う。idle / composition のみ では素通し
        // （アプリのキャレット移動を邪魔しない）。
        for vk in [0x26u32, 0x28] {
            assert!(
                !will_handle(vk, false, false, false, false),
                "vk {vk:#x} idle should pass"
            );
            assert!(
                !will_handle(vk, true, false, false, false),
                "vk {vk:#x} composing-no-candidates should pass"
            );
            assert!(
                will_handle(vk, false, true, false, false),
                "vk {vk:#x} showing should be handled"
            );
        }
    }

    #[test]
    fn tab_is_no_longer_a_fixed_key_and_is_eaten_via_hot() {
        // Tab は固定キーの真実(will_handle)から外れ、typo/llm hot でのみ食う（keymap 化）。
        // 「flag×composing×チョード一致」の判定は resolve_action へ移った（keymap.rs でテスト）。
        assert!(!will_handle(0x09, true, false, false, false)); // will_handle 単独ではもう食わない
        assert!(!will_handle(0x09, false, false, false, false));
        // typo action が立てば composition 中は食う。無ければ composing でも素通し。
        assert!(will_handle_gated(
            0x09,
            true,
            false,
            false,
            false,
            false,
            false,
            KeyAction::Typo
        ));
        assert!(!will_handle_gated(
            0x09,
            true,
            false,
            false,
            false,
            false,
            false,
            KeyAction::None
        ));
    }

    // ---- Tab 二毛作の feature flag/文脈ゲートは resolve_action が織り込む（keymap.rs でテスト）。
    //      gated 層では typo/llm action が立てば食い、立たなければ素通しする写像だけを固定する。----
    #[test]
    fn tab_typo_and_llm_actions_are_eaten_by_gated() {
        // Typo（無 Shift Tab の修正変換）→ 食う。
        assert!(will_handle_gated(
            0x09,
            true,
            false,
            false,
            false,
            false,
            false,
            KeyAction::Typo
        ));
        // Llm（Shift+Tab の外部LLM変換）→ 食う。
        assert!(will_handle_gated(
            0x09,
            true,
            false,
            false,
            false,
            true,
            false,
            KeyAction::Llm
        ));
        // action が無ければ idle でも composing でも direct でも素通し。
        assert!(!will_handle_gated(
            0x09,
            false,
            false,
            false,
            false,
            false,
            false,
            KeyAction::None
        ));
        assert!(!will_handle_gated(
            0x09,
            true,
            false,
            false,
            true,
            false,
            false,
            KeyAction::None
        ));
    }

    #[test]
    fn actionless_keys_fall_back_to_will_handle_fixed_keys() {
        // action=None なら gated は will_handle の固定キー真実そのまま（'A' は常に食う）。
        assert!(will_handle_gated(
            0x41,
            false,
            false,
            false,
            false,
            false,
            false,
            KeyAction::None
        ));
        // Space は will_handle 固定キーから外れたので action=None では composing でも食わない
        //（henkan は Convert アクション carve-out 経由 — space テストが担保）。
        assert!(!will_handle_gated(
            0x20,
            true,
            false,
            false,
            false,
            false,
            false,
            KeyAction::None
        ));
        assert!(!will_handle_gated(
            0x20,
            false,
            false,
            false,
            false,
            false,
            false,
            KeyAction::None
        ));
        // Convert アクションが立てば composing で食う。
        assert!(will_handle_gated(
            0x20,
            true,
            false,
            false,
            false,
            false,
            false,
            KeyAction::Convert
        ));
    }

    // ---- Minor 2: Shift+数字の Test/実一致 ----
    #[test]
    fn shift_digit_matches_real_handler_condition() {
        // 候補表示のみ(showing && !composing)で Shift+数字は「食わない」＝記号として本文へ
        // （実処理 OnKeyDown の VK_1..=VK_9 アームが `!shift_down()` を要求するのに一致）。
        assert!(!will_handle_gated(
            0x31,
            false,
            true,
            false,
            false,
            /*shift=*/ true,
            false,
            KeyAction::None
        )); // Shift+'1', showing only
            // Shift 無しなら従来どおり食う（候補選択）。
        assert!(will_handle_gated(
            0x31,
            false,
            true,
            false,
            false,
            false,
            false,
            KeyAction::None
        ));
        // composition 中は Shift 有無に依らず食う（記号としてエンジンへ）。
        assert!(will_handle_gated(
            0x31,
            true,
            true,
            false,
            false,
            true,
            false,
            KeyAction::None
        ));
        assert!(will_handle_gated(
            0x31,
            true,
            false,
            false,
            false,
            true,
            false,
            KeyAction::None
        ));
        // 数字以外は shift に左右されない（'A' は常に食う）。
        assert!(will_handle_gated(
            0x41,
            false,
            false,
            false,
            false,
            true,
            false,
            KeyAction::None
        ));
    }

    // ---- ②: かなモード idle の無修飾数字は composition を開始する ----
    #[test]
    fn native_idle_unshifted_digit_is_eaten_to_start_composition() {
        // gated(vk, composing=false, showing=false, cmd=false, direct=false, shift=false, action=None)
        for vk in [0x30u32, 0x31, 0x39, 0x60, 0x69] {
            assert!(
                will_handle_gated(
                    vk,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    KeyAction::None
                ),
                "vk {vk:#x} native idle unshifted -> eaten"
            );
        }
        // direct モードでは従来どおり素通し。
        for vk in [0x31u32, 0x39] {
            assert!(
                !will_handle_gated(vk, false, false, false, true, false, false, KeyAction::None),
                "vk {vk:#x} direct idle -> pass through"
            );
        }
        // Shift+数字 idle は記号入力なので食わない（従来どおり）。
        assert!(!will_handle_gated(
            0x31,
            false,
            false,
            false,
            false,
            /*shift=*/ true,
            false,
            KeyAction::None
        ));
        // Ctrl+数字（cmd 修飾）はアプリのアクセラレータ＝食わない。
        assert!(!will_handle_gated(
            0x31,
            false,
            false,
            true,
            false,
            false,
            false,
            KeyAction::None
        ));
        // テンキー演算子（0x6D='-'）は数字でないので idle では食わない（従来どおり）。
        assert!(!will_handle_gated(
            0x6D,
            false,
            false,
            false,
            false,
            false,
            false,
            KeyAction::None
        ));
    }

    // ---- Bug 3: awaiting_llm の Test/実一致（Bug A の鏡像）----
    #[test]
    fn awaiting_llm_eats_all_non_modifier_keys() {
        use super::will_handle_awaiting;
        // 待機中は cmd 修飾以外の全キーを食う（実処理が待機中に return Ok(TRUE) するのと一致）。
        // will_handle 単独では食わないキー（F1=0x70, ←=0x25）でも待機中は食う。
        for vk in [0x70u32, 0x25, 0x1B /*Esc*/, 0x41 /*A*/] {
            assert!(
                will_handle_awaiting(
                    vk,
                    false,
                    false,
                    false,
                    false,
                    false,
                    /*awaiting=*/ true,
                    false,
                    KeyAction::None
                ),
                "vk {vk:#x} must be eaten while awaiting llm"
            );
        }
        // cmd 修飾は待機中でも最優先でパススルー（実処理が awaiting より先に cmd を弾くのと一致）。
        assert!(!will_handle_awaiting(
            0x41,
            false,
            false,
            true,
            false,
            false,
            true,
            false,
            KeyAction::None
        ));
        // 非待機なら通常の gate に一致する（F1 は食わない、'A' は食う）。
        assert!(!will_handle_awaiting(
            0x70,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            KeyAction::None
        ));
        assert!(will_handle_awaiting(
            0x41,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            KeyAction::None
        ));
    }

    // ---- Bug 4: 数字キーのページオフセット選択 ----
    #[test]
    fn page_candidate_index_maps_digit_to_visible_page() {
        use super::page_candidate_index;
        // 1ページに収まる（9件以下）: digit==絶対 index。
        assert_eq!(page_candidate_index(0, 3, 0), Some(0)); // '1' → 0
        assert_eq!(page_candidate_index(0, 3, 2), Some(2)); // '3' → 2
                                                            // 可視行数を超える数字は no-op。
        assert_eq!(page_candidate_index(0, 3, 3), None); // '4' with 3 cands → None
        assert_eq!(page_candidate_index(1, 3, 8), None); // '9' → None（誤選択しない）
                                                         // 2ページ目（selected=9, 総 20 件）: ページ先頭 9 が加算される。
        assert_eq!(page_candidate_index(9, 20, 0), Some(9)); // '1' → 9（表示上の "10"）
        assert_eq!(page_candidate_index(9, 20, 8), Some(17)); // '9' → 17
                                                              // 末尾の半端ページ（selected=18, 総 20 件, ページ [18,20)）: 2 行しか無い。
        assert_eq!(page_candidate_index(18, 20, 0), Some(18)); // '1' → 18
        assert_eq!(page_candidate_index(18, 20, 1), Some(19)); // '2' → 19
        assert_eq!(page_candidate_index(18, 20, 2), None); // '3' → None（ページ外）
                                                           // 空候補は常に None。
        assert_eq!(page_candidate_index(0, 0, 0), None);
    }

    #[test]
    fn other_keys_pass() {
        assert!(!will_handle(0x70, true, true, false, false)); // F1 はパススルー
    }

    // ---- 品質ループ②: ev=commit の構造化フィールド ----
    #[test]
    fn commit_fields_format_is_stable() {
        use super::commit_fields;
        // 候補確定: sel=絶対index, cand_n=候補総数, rlen/tlen は chars 数（バイトでない）。
        let f = commit_fields(Some(2), 9, "にほんご", "日本語", false);
        assert_eq!(f, "sel=2 cand_n=9 rlen=4 tlen=3 mode=native");
        // ライブ確定（候補選択なし）: sel=-1 cand_n=0。
        let f = commit_fields(None, 0, "", "あ", false);
        assert_eq!(f, "sel=-1 cand_n=0 rlen=0 tlen=1 mode=native");
        // direct モード（再変換確定）: mode=direct。
        let f = commit_fields(Some(0), 3, "にほんご", "日本語", true);
        assert_eq!(f, "sel=0 cand_n=3 rlen=4 tlen=3 mode=direct");
    }

    // ---- 打鍵作法 Task2: composing 中の ←→ は食って確定畳み（意図的な仕様変更）----
    // 旧仕様「←→は常にパススルー」の assert は本テストへ反転統合した（キャレット逃げ防止。
    // UU-6 Home/End と同じ settle_active_input 経路）。
    #[test]
    fn arrows_mid_composition_are_eaten_to_settle() {
        // composing 中の ←→ は食って確定して畳む（UU-6 Home/End と同じ作法）。
        assert!(will_handle(0x25, true, false, false, false)); // ← composing
        assert!(will_handle(0x27, true, false, false, false)); // → composing
                                                               // 候補表示中も食う（Home/End と同じく settle が候補確定まで面倒を見る）。
        assert!(will_handle(0x25, false, true, false, false));
        assert!(will_handle(0x27, false, true, false, false));
        // idle は従来どおり素通し（アプリのキャレット移動を壊さない）。
        assert!(!will_handle(0x25, false, false, false, false));
        assert!(!will_handle(0x27, false, false, false, false));
        // Ctrl+←（単語ジャンプ等のアクセラレータ）は composing 中でも素通し。
        assert!(!will_handle(0x25, true, false, true, false));
        // direct(半角英数) は挙動不変: 非表示は素通し（本文のキャレット移動はアプリに任せる）。
        assert!(!will_handle(0x25, false, false, false, true));
        assert!(!will_handle(0x27, false, false, false, true));
    }

    // ---- UU-6: 合成中の Home/End/PageUp/PageDown/Delete は食って確定→畳む ----
    #[test]
    fn navigation_keys_eaten_only_when_active_native() {
        // Home(0x24)/End(0x23)/PageUp(0x21)/PageDown(0x22)/Delete(0x2E) は
        // composition 中/候補表示中だけ食う（settle で確定して畳む）。idle は素通し
        // （本文のキャレット移動/前方削除をアプリに任せる＝旧挙動不変）。
        for vk in [0x24u32, 0x23, 0x21, 0x22, 0x2E] {
            assert!(
                !will_handle(vk, false, false, false, false),
                "vk {vk:#x} idle native should pass"
            );
            assert!(
                will_handle(vk, true, false, false, false),
                "vk {vk:#x} composing native should be eaten"
            );
            assert!(
                will_handle(vk, false, true, false, false),
                "vk {vk:#x} showing native should be eaten"
            );
        }
    }

    #[test]
    fn navigation_keys_direct_mode_only_when_showing() {
        // direct(半角英数): 候補表示中(reconvert)だけ食う。非表示なら本文操作なので素通し。
        for vk in [0x24u32, 0x23, 0x21, 0x22, 0x2E] {
            assert!(
                will_handle(vk, false, true, false, true),
                "vk {vk:#x} direct showing should be eaten"
            );
            assert!(
                !will_handle(vk, false, false, false, true),
                "vk {vk:#x} direct not-showing should pass"
            );
        }
    }

    #[test]
    fn navigation_keys_cmd_modifier_passes_through() {
        // Ctrl+Home / Ctrl+End 等（ドキュメント先頭/末尾ジャンプのアクセラレータ）は
        // composition 中でも食わずアプリへ通す（cmd 修飾は最優先パススルー）。
        for vk in [0x24u32, 0x23, 0x2E] {
            assert!(
                !will_handle(vk, true, true, true, false),
                "vk {vk:#x} with cmd modifier must pass through"
            );
        }
    }

    #[test]
    fn navigation_keys_test_matches_real_via_gated() {
        // OnTestKeyDown が使う gated 述語でも合成中は食う（Test/実の eaten 判定を一致）。
        for vk in [0x24u32, 0x23, 0x21, 0x22, 0x2E] {
            assert!(
                will_handle_gated(vk, true, false, false, false, false, false, KeyAction::None),
                "vk {vk:#x} composing must be eaten by gated predicate"
            );
            assert!(
                !will_handle_gated(
                    vk,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    KeyAction::None
                ),
                "vk {vk:#x} idle must pass through gated predicate"
            );
        }
    }

    #[test]
    fn text_keys_handled_only_when_composing() {
        // 0(0x30) とテンキーは composition 中だけ食う（エンジンへ送る）。idle では素通し
        // （単独の数字・テンキー記号は通常入力。打鍵作法 Task3 でも対象外 — テンキー混同防止）。
        for vk in [0x30u32, 0x60, 0x6E /*Numpad .*/] {
            assert!(
                will_handle(vk, true, false, false, false),
                "vk {vk:#x} composing -> handled (text)"
            );
            assert!(
                !will_handle(vk, false, false, false, false),
                "vk {vk:#x} idle -> pass through"
            );
        }
    }

    // ---- 打鍵作法 Task5 改: A–Z 経路決定（Shift 起点の一時直接入力）----
    // testbench に Shift 注入 API が無いため、経路決定を純関数 resolve_az_char に切り出して
    // ここで担保する（実機 SP で Shift+英字の end-to-end を確認 — VM 受入送り）。
    #[test]
    fn resolve_az_shift_routes_to_direct_commit() {
        use super::{resolve_az_char, AzRoute};
        // shift_latin=commit(compose=false): Shift+C は一時直接入力（大文字をそのまま直接確定へ）。
        // idle でも composition 中でも同じ（呼び出し側 commit_char_direct が先に settle して畳む）。
        assert_eq!(
            resolve_az_char(0x43, true, Some('C'), false, false),
            AzRoute::DirectCommit('C')
        );
        // ToUnicode が取れないホストでも shift なら大文字へフォールバック。
        assert_eq!(
            resolve_az_char(0x43, true, None, false, false),
            AzRoute::DirectCommit('C')
        );
        assert_eq!(
            resolve_az_char(0x41, true, Some('A'), false, false),
            AzRoute::DirectCommit('A')
        );
        // CapsLock 併用で shift+英字が小文字を返すレイアウトはその文字を尊重（key_char が真実）。
        assert_eq!(
            resolve_az_char(0x41, true, Some('a'), false, false),
            AzRoute::DirectCommit('a')
        );
    }

    #[test]
    fn pending_physical_composition_is_not_logical_idle_for_settle() {
        use super::input_needs_settle;

        assert!(!input_needs_settle(false, false, false));
        assert!(input_needs_settle(true, false, false));
        assert!(input_needs_settle(false, true, false));
        assert!(input_needs_settle(false, false, true));
    }

    #[test]
    fn resolve_az_keeps_kana_route_unchanged() {
        use super::{resolve_az_char, AzRoute};
        // 無修飾はかな経路（小文字へ正規化）— 従来挙動。設定・英語モードに依らない。
        assert_eq!(
            resolve_az_char(0x41, false, Some('a'), false, false),
            AzRoute::Kana('a')
        );
        assert_eq!(
            resolve_az_char(0x41, false, None, false, false),
            AzRoute::Kana('a')
        );
        // AltGr 等の非英字レイアウト文字は従来どおり尊重（かな経路のまま）。
        assert_eq!(
            resolve_az_char(0x41, false, Some('á'), false, false),
            AzRoute::Kana('á')
        );
        // CapsLock（shift 無しで大文字が来る）は従来どおり小文字正規化のかな経路。
        assert_eq!(
            resolve_az_char(0x43, false, Some('C'), false, false),
            AzRoute::Kana('c')
        );
    }

    // ---- Shift英語モード(shift_latin=compose): A–Z 経路決定 ----

    #[test]
    fn resolve_az_compose_shift_enters_latin() {
        use super::{resolve_az_char, AzRoute};
        // compose 設定では Shift+英字は直接確定でなく英語未確定モードへ（MS-IME 系）。
        assert_eq!(
            resolve_az_char(0x41, true, Some('A'), true, false),
            AzRoute::Latin('A')
        );
        // ToUnicode が取れなくても shift なら大文字へフォールバック。
        assert_eq!(
            resolve_az_char(0x41, true, None, true, false),
            AzRoute::Latin('A')
        );
    }

    #[test]
    fn resolve_az_compose_latin_mode_continues_unshifted_as_lowercase() {
        use super::{resolve_az_char, AzRoute};
        // 英語モード中は無修飾でも英語継続（Shift なし=小文字 — 依頼仕様の核）。
        assert_eq!(
            resolve_az_char(0x41, false, Some('a'), true, true),
            AzRoute::Latin('a')
        );
        assert_eq!(
            resolve_az_char(0x42, false, None, true, true),
            AzRoute::Latin('b')
        );
        // モード中の Shift は大文字。
        assert_eq!(
            resolve_az_char(0x41, true, Some('A'), true, true),
            AzRoute::Latin('A')
        );
    }

    #[test]
    fn resolve_az_compose_without_shift_or_mode_stays_kana() {
        use super::{resolve_az_char, AzRoute};
        // compose 設定でも Shift 無し・非英語モードなら従来のかな経路。
        assert_eq!(
            resolve_az_char(0x41, false, Some('a'), true, false),
            AzRoute::Kana('a')
        );
    }

    // ---- 打鍵作法 Task4: F6-F10 の表記変換は notation hot で食う（keymap 化）----
    #[test]
    fn f6_to_f10_notation_action_is_eaten_by_gated() {
        use crate::keymap::Notation;
        // F6-F10 は will_handle の固定キーから外れ、Notation アクションでのみ食う。composing×非 direct×
        // チョード一致の判定は resolve_action が持つ（keymap.rs でテスト）。
        for vk in 0x75u32..=0x79 {
            assert!(
                !will_handle(vk, true, false, false, false),
                "vk {vk:#x} will_handle 単独ではもう食わない"
            );
        }
        // Notation アクションが立てば gated は食う。無ければ composing でも素通し。
        assert!(will_handle_gated(
            0x76,
            true,
            false,
            false,
            false,
            false,
            false,
            KeyAction::Notation(Notation::Katakana)
        ));
        assert!(!will_handle_gated(
            0x76,
            true,
            false,
            false,
            false,
            false,
            false,
            KeyAction::None
        ));
    }

    // ---- OEM 記号は idle でも食う（2026-08-03: 全角化して合成開始。旧 Task3 は直接確定）----
    // 旧仕様「OEM 記号は idle 素通し」の assert は text_keys_handled_only_when_composing から
    // 本テストへ反転分離した。eaten 契約は直接確定→合成開始の変更を跨いで不変。
    #[test]
    fn symbol_vks_handled_even_when_idle_in_native() {
        // OEM 記号 VK は idle でも食う（全角化して composition を開始するため）。native のみ。
        assert!(will_handle(0xBE, false, false, false, false)); // VK_OEM_PERIOD idle
        assert!(will_handle(0xBC, false, false, false, false)); // VK_OEM_COMMA idle
                                                                // composition 中も従来どおり食う（エンジンへ送る経路は OnKeyDown 側で分岐）。
        for vk in [0xBAu32, 0xBC, 0xBD, 0xBE, 0xBF, 0xC0, 0xDB, 0xDE] {
            assert!(
                will_handle(vk, true, false, false, false),
                "vk {vk:#x} composing -> handled"
            );
            assert!(
                will_handle(vk, false, false, false, false),
                "vk {vk:#x} idle -> handled (合成開始)"
            );
        }
        // Ctrl/Alt 併用はアプリのアクセラレータとして常に素通し。
        assert!(!will_handle(0xBE, false, false, true, false));
        // direct モードでは従来どおり素通し（既存 direct 分岐が先に効く）。
        assert!(!will_handle(0xBE, false, false, false, true));
        assert!(!will_handle(0xBC, false, false, false, true));
    }

    #[test]
    fn cmd_modifier_always_passes_through() {
        // Ctrl/Alt 併用キー（Ctrl+C/V/X/A/Z/S 等）は composition / 候補表示中でも食わない。
        // これが今回のバグ（Ctrl+C 等が IME に食われアプリへ届かない）の回帰テスト。
        for vk in [
            0x43u32, /*C*/
            0x56,    /*V*/
            0x58,    /*X*/
            0x41,    /*A*/
            0x5A,    /*Z*/
            0x53,    /*S*/
            0x59,    /*Y*/
        ] {
            assert!(
                !will_handle(vk, true, true, true, false),
                "vk {vk:#x} with cmd modifier must pass through to the app"
            );
        }
        // 無修飾なら従来どおり A–Z は食う（回帰しないこと）。
        assert!(will_handle(0x43, false, false, false, false));
    }

    // ---- SP5: 半角英数(直接入力)モード ----

    #[test]
    fn direct_mode_passes_text_keys_but_eats_candidates_when_showing() {
        // direct(半角英数): A–Z/数字/記号は食わない（本文へ流す）。
        assert!(!will_handle(0x41, false, false, false, true)); // 'A' direct → pass
        assert!(!will_handle(0x30, true, false, false, true)); // '0' direct → pass
                                                               // direct: 候補表示中(showing)は候補キー(Enter/Esc/↑↓/数字)を食う。
        assert!(will_handle(0x0D, false, true, false, true)); // Enter, showing
        assert!(will_handle(0x1B, false, true, false, true)); // Esc, showing
        assert!(will_handle(0x28, false, true, false, true)); // ↓, showing
        assert!(will_handle(0x31, false, true, false, true)); // '1', showing
                                                              // Space は will_handle 固定キーから外れ、direct+showing は Convert アクションで候補送り。
        assert_eq!(act(0x20, false, true, true), KeyAction::Convert);
        assert!(will_handle_gated(
            0x20,
            false,
            true,
            false,
            true,
            false,
            false,
            KeyAction::Convert
        ));
        // direct: 非表示なら候補キーも食わない（全部本文へ）。Space は Convert も立たず素通し。
        assert!(!will_handle(0x20, false, false, false, true)); // Space, not showing → pass (will_handle)
        assert_eq!(act(0x20, false, false, true), KeyAction::None);
    }

    #[test]
    fn backspace_reconvert_candidate_is_eaten_in_native_and_direct() {
        use super::{BackspaceRoute, VK_BACK};

        // 再変換/確定取消の共有尾部は composing=false でも候補を表示するため、
        // OnTestKeyDown の native/direct 判定はいずれも TRUE でなければならない。
        assert!(will_handle(VK_BACK, false, true, false, false));
        assert!(will_handle(VK_BACK, false, true, false, true));
        assert_eq!(
            backspace_route(false, true, true),
            BackspaceRoute::CancelReconvert
        );
    }

    #[test]
    fn backspace_reconvert_and_commit_undo_share_cancel_route() {
        use super::{BackspaceRoute, VK_BACK};

        // start_reconvert と start_commit_undo は同じ show_reconvert_candidates 尾部を使う。
        for source in ["reconvert", "commit_undo"] {
            for direct in [false, true] {
                assert!(
                    will_handle_awaiting(
                        VK_BACK,
                        false,
                        true,
                        false,
                        direct,
                        false,
                        false,
                        false,
                        KeyAction::None,
                    ),
                    "{source} direct={direct} OnTestKeyDown must eat Backspace",
                );
                assert_eq!(
                    backspace_route(false, true, true),
                    BackspaceRoute::CancelReconvert,
                    "{source} direct={direct} OnKeyDown must cancel through RestoreText",
                );
            }
        }
    }

    #[test]
    fn backspace_route_keeps_composing_behavior_and_hides_only_stale_candidates() {
        use super::BackspaceRoute;

        assert_eq!(backspace_route(true, false, false), BackspaceRoute::Compose);
        assert_eq!(backspace_route(true, true, true), BackspaceRoute::Compose);
        assert_eq!(
            backspace_route(false, true, false),
            BackspaceRoute::HideStaleCandidates
        );
        assert_eq!(backspace_route(false, false, true), BackspaceRoute::Pass);
    }

    #[test]
    fn native_mode_unchanged() {
        // native(ひらがな)は従来どおり（第5引数 false）。
        assert!(will_handle(0x41, false, false, false, false)); // 'A' native → 食う
                                                                // Space は will_handle 固定キーから外れ、Convert アクション経由で食う（下記 space テスト）。
                                                                // native composing の Space が食われることは gated+Convert で担保する。
        assert!(will_handle_gated(
            0x20,
            true,
            false,
            false,
            false,
            false,
            false,
            KeyAction::Convert
        ));
        assert!(!will_handle(0x09, false, false, false, false)); // Tab idle → pass
    }

    #[test]
    fn direct_convert_key_is_eaten_via_reconvert_or_convert_action() {
        // SP5 item13: 半角英数(直接入力)の 変換キー(0x1C) は will_handle 固定アームから外れ、
        // resolve_action が direct+idle→Reconvert / direct+composing・showing→Convert を返す
        //（reconvert_fallback 特例は撤去。ephemeral フォールバック無し）。
        assert!(!will_handle(0x1C, false, false, false, true)); // will_handle 単独ではもう食わない
        assert_eq!(act(0x1C, false, false, true), KeyAction::Reconvert); // direct idle → 再変換
        assert_eq!(act(0x1C, true, false, true), KeyAction::Convert); // direct composing → 候補送り(henkan)
        assert_eq!(act(0x1C, false, true, true), KeyAction::Convert); // direct showing → 候補送り
                                                                      // gate ではその action が立てば食う。
        assert!(will_handle_gated(
            0x1C,
            false,
            false,
            false,
            true,
            false,
            false,
            KeyAction::Reconvert
        ));
        assert!(will_handle_gated(
            0x1C,
            true,
            false,
            false,
            true,
            false,
            false,
            KeyAction::Convert
        ));
        assert!(will_handle_gated(
            0x1C,
            false,
            true,
            false,
            true,
            false,
            false,
            KeyAction::Convert
        ));
        // action が無ければ（リバインド/無効化済み）direct の 変換キーは素通し。
        assert!(!will_handle_gated(
            0x1C,
            false,
            false,
            false,
            true,
            false,
            false,
            KeyAction::None
        ));
    }

    #[test]
    fn native_mode_convert_key_acts_like_space() {
        // native(ひらがな): 変換キー(0x1C) は Space と同じ henkan。will_handle 固定キーから外れ、
        // Convert アクションで composing/showing だけ食う。idle は素通し。
        assert!(!will_handle(0x1C, false, false, false, false)); // will_handle 単独ではもう食わない
        assert_eq!(act(0x1C, false, false, false), KeyAction::None); // native idle → 素通し
        assert_eq!(act(0x1C, true, false, false), KeyAction::Convert); // composing → Convert
        assert_eq!(act(0x1C, false, true, false), KeyAction::Convert); // showing → Convert
        assert!(will_handle_gated(
            0x1C,
            true,
            false,
            false,
            false,
            false,
            false,
            KeyAction::Convert
        ));
        assert!(will_handle_gated(
            0x1C,
            false,
            true,
            false,
            false,
            false,
            false,
            KeyAction::Convert
        ));
    }

    #[test]
    fn is_cmd_modifier_is_xor() {
        assert!(is_cmd_modifier(true, false)); // Ctrl のみ → アクセラレータ
        assert!(is_cmd_modifier(false, true)); // Alt のみ → アクセラレータ
        assert!(!is_cmd_modifier(true, true)); // AltGr(Ctrl+Alt) → 通常入力
        assert!(!is_cmd_modifier(false, false)); // 無修飾 → 通常入力
    }

    #[test]
    fn oem_symbol_vk_covers_main_row_not_numpad() {
        use super::{is_oem_symbol_vk, is_text_vk};
        // composition 記号ルーティングの不変条件（設計 §C・レビュー F1）: メイン行の記号キーだけ
        // is_oem_symbol_vk=true で「全角へ写して読みへ畳み込む」枝に入り、テンキーの記号/長音は
        // is_oem_symbol_vk=false で to_kana_reading_char 枝（`-`→ー・`.` は literal）へ落ちる。
        // この境界が崩れるとテンキー `-` のー化やテンキー `.` の literal が壊れる（環境非依存で固定）。
        assert!(is_oem_symbol_vk(0xBD)); // メイン行 '-'（OEM_MINUS）→ 畳み込み枝
        assert!(is_oem_symbol_vk(0xBE)); // メイン行 '.'（OEM_PERIOD）
        assert!(is_oem_symbol_vk(0xBF)); // メイン行 '/'（OEM_2）
        assert!(!is_oem_symbol_vk(0x6D)); // テンキー '-'（VK_SUBTRACT）→ to_kana_reading_char 枝でー
        assert!(!is_oem_symbol_vk(0x6E)); // テンキー '.'（VK_DECIMAL）→ literal のまま
                                          // いずれも is_text_vk=true（composition 枝に入る前提）。
        assert!(is_text_vk(0xBD) && is_text_vk(0x6D) && is_text_vk(0x6E));
    }

    #[test]
    fn oem_102_is_text_and_symbol_without_changing_direct_or_ephemeral_modes() {
        use super::{
            ephemeral_idle_abort, is_oem_symbol_vk, is_symbol_keystroke, is_text_vk, VK_OEM_102,
        };

        assert!(is_text_vk(VK_OEM_102));
        assert!(is_oem_symbol_vk(VK_OEM_102));
        assert!(is_symbol_keystroke(VK_OEM_102, false, false));

        // native は idle で合成開始し、composition 中も食い続ける。
        assert!(will_handle(VK_OEM_102, false, false, false, false));
        assert!(will_handle(VK_OEM_102, true, false, false, false));
        // direct/Cmd 修飾では本文・アプリへ渡す。
        assert!(!will_handle(VK_OEM_102, false, false, false, true));
        assert!(!will_handle(VK_OEM_102, true, false, false, true));
        assert!(!will_handle(VK_OEM_102, false, false, true, false));
        // native ephemeral 中も、このキーでは direct へ脱出しない。
        assert!(!ephemeral_idle_abort(
            VK_OEM_102,
            false,
            false,
            false,
            true,
            false,
            false,
            KeyAction::None,
        ));
    }

    #[test]
    fn main_row_digit_vk_excludes_numpad() {
        use super::{is_main_row_digit_vk, is_symbol_keystroke};
        // Shift+テンキーは記号を生まない(NumLock 系の別 VK になる)ので、記号打鍵述語は
        // メイン行(0x30-0x39)限定。この境界が崩れるとテンキー入力が記号化されて壊れる。
        assert!(
            is_main_row_digit_vk(0x30) && is_main_row_digit_vk(0x31) && is_main_row_digit_vk(0x39)
        );
        assert!(
            !is_main_row_digit_vk(0x60)
                && !is_main_row_digit_vk(0x69)
                && !is_main_row_digit_vk(0x6D)
        );
        // OEM 記号キーはトグル/Shift 非依存(従来契約=常に記号打鍵。表外も食い切って ASCII 確定)。
        assert!(is_symbol_keystroke(0xBF, false, false)); // '/'
        assert!(is_symbol_keystroke(0xC0, true, false)); // Shift+` = '~'
                                                         // 数字行は「Shift かつ 記号 overlay ON」のときだけ記号打鍵。OFF を含めないのは
                                                         // 既定 OFF で現行経路と完全同一(新規に食う打鍵ゼロ)を保証するため。
        assert!(is_symbol_keystroke(0x31, true, true)); // Shift+1 = '!' (ON)
        assert!(!is_symbol_keystroke(0x31, true, false)); // OFF なら記号扱いしない
        assert!(!is_symbol_keystroke(0x31, false, true)); // 無 Shift は数字
        assert!(!is_symbol_keystroke(0x62, true, true)); // テンキー2は対象外
    }

    #[test]
    fn gated_eats_shifted_digit_row_only_when_symbol_overlay() {
        // (vk, composing, showing, cmd, direct, shift, symbol_overlay, action) — action は記号 overlay に無関係。
        let none = KeyAction::None;
        assert!(
            will_handle_gated(0x31, false, false, false, false, true, true, none),
            "ON: idle の Shift+1 を食って！で合成を開始する"
        );
        assert!(
            will_handle_gated(0x30, false, false, false, false, true, true, none),
            "ON: Shift+0=) も対象(0x30 を含む)"
        );
        assert!(
            !will_handle_gated(0x31, false, false, false, false, true, false, none),
            "OFF: 現行と完全同一(食わない)"
        );
        assert!(
            !will_handle_gated(0x31, false, false, false, true, true, true, none),
            "direct は対象外"
        );
        assert!(
            !will_handle_gated(0x31, false, false, true, false, true, true, none),
            "cmd 修飾は対象外"
        );
        assert!(will_handle_gated(0x31, false, true, false, false, true, true, none),
            "ON: showing 中も記号として食う(Shift+数字の候補選択取消より前=OEM 記号の showing 中挙動と同型)");
        assert!(
            !will_handle_gated(0x31, false, true, false, false, true, false, none),
            "OFF: showing 中 Shift+数字は従来どおり素通し"
        );
        assert!(
            !will_handle_gated(0x62, false, false, false, false, true, true, none),
            "テンキーは対象外"
        );
    }

    #[test]
    fn classify_preserved_key_routes_jis_and_us_keys() {
        use super::{classify_preserved_key, PreservedAction};
        use crate::globals::{
            GUID_DISPLAY_ATTRIBUTE, GUID_PRESERVEDKEY_FEEDBACK, GUID_PRESERVEDKEY_FEEDBACK_US,
            GUID_PRESERVEDKEY_MODE_TOGGLE, GUID_PRESERVEDKEY_MODE_TOGGLE_US,
            GUID_PRESERVEDKEY_RECONVERT, GUID_PRESERVEDKEY_RECONVERT_US,
        };
        assert_eq!(
            classify_preserved_key(&GUID_PRESERVEDKEY_MODE_TOGGLE),
            PreservedAction::ToggleMode
        );
        assert_eq!(
            classify_preserved_key(&GUID_PRESERVEDKEY_MODE_TOGGLE_US),
            PreservedAction::ToggleMode
        );
        assert_eq!(
            classify_preserved_key(&crate::globals::GUID_PRESERVEDKEY_MODE_TOGGLE_HZ),
            PreservedAction::ToggleMode
        );
        assert_eq!(
            classify_preserved_key(&GUID_PRESERVEDKEY_RECONVERT),
            PreservedAction::Reconvert
        );
        assert_eq!(
            classify_preserved_key(&GUID_PRESERVEDKEY_RECONVERT_US),
            PreservedAction::Reconvert
        );
        // 品質ループ③: 誤変換フィードバック記録（Ctrl+変換 / Ctrl+/）の両 GUID。
        assert_eq!(
            classify_preserved_key(&GUID_PRESERVEDKEY_FEEDBACK),
            PreservedAction::Feedback
        );
        assert_eq!(
            classify_preserved_key(&GUID_PRESERVEDKEY_FEEDBACK_US),
            PreservedAction::Feedback
        );
        // 未知の GUID（表示属性 GUID を流用）は None。
        assert_eq!(
            classify_preserved_key(&GUID_DISPLAY_ATTRIBUTE),
            PreservedAction::None
        );
    }

    // ---- M-3: 確定文字列の唯一の真実源は cand_state ----
    // Enter / 数字選択 / マウス(drain_behavior) の3経路が cand_state から同じ文字列を
    // 読むことを単体で固定する（field self.candidates 廃止後の回帰ガード）。
    use crate::candidate_state::CandidateState;

    /// 数字 1–9 選択が解決する確定文字列を、実処理(VK_1..=VK_9 アーム)と同じ規則で再現するヘルパ。
    /// Bug 4: 可視ページ先頭を加えた絶対 index の候補を確定する。可視ページ外の数字は None（no-op）。
    fn digit_commit_text(st: &CandidateState, vk: u32) -> Option<String> {
        let digit = (vk - 0x31) as usize; // VK_1=0x31, 0 始まりのページ内行
        super::page_candidate_index(st.selected(), st.count(), digit)
            .and_then(|abs| st.string_at(abs))
    }

    #[test]
    fn enter_commits_selected_from_cand_state() {
        // Enter は string_at(selected()) を確定する（選択を動かした後もここが真実源）。
        let mut st = CandidateState::new();
        st.set(vec!["日本".into(), "二本".into(), "二本".into()], 0);
        assert_eq!(st.string_at(st.selected()).as_deref(), Some("日本"));
        st.move_selection(1);
        assert_eq!(st.string_at(st.selected()).as_deref(), Some("二本"));
    }

    #[test]
    fn digit_select_picks_indexed_candidate() {
        let mut st = CandidateState::new();
        st.set(vec!["一".into(), "二".into(), "三".into()], 0);
        // '1'→0 番目, '3'→2 番目。
        assert_eq!(digit_commit_text(&st, 0x31).as_deref(), Some("一"));
        assert_eq!(digit_commit_text(&st, 0x33).as_deref(), Some("三"));
        // Bug 4: 可視ページ外の番号（'9' で候補3件）は no-op（None）＝誤選択しない。
        st.set_selection(1);
        assert_eq!(digit_commit_text(&st, 0x39), None);
    }

    #[test]
    fn digit_select_uses_page_offset_on_second_page() {
        // Bug 4 回帰: 10件以上で 2ページ目（selected を 9 に置く＝ページ [9,18)）を表示中に
        // '1' を押すと絶対 index 9（1ページ目ではなく現在ページの先頭）を確定する。
        let mut st = CandidateState::new();
        let items: Vec<String> = (0..20).map(|i| format!("c{i}")).collect();
        st.set(items, 0);
        st.set_selection(9); // 2ページ目へ
        assert_eq!(digit_commit_text(&st, 0x31).as_deref(), Some("c9")); // '1' → 先頭 c9
        assert_eq!(digit_commit_text(&st, 0x39).as_deref(), Some("c17")); // '9' → c17
    }

    #[test]
    fn empty_cand_state_commits_nothing() {
        let st = CandidateState::new();
        assert_eq!(
            st.string_at(st.selected()).or_else(|| st.string_at(0)),
            None
        );
        assert_eq!(digit_commit_text(&st, 0x31), None);
    }

    // ---- 確定取消（Ctrl+Backspace）: armed 状態機械 ----
    use super::{arms_undo, is_pure_modifier_vk};

    #[test]
    fn empty_commit_keeps_no_records() {
        // 巡13(round13): 空 text の確定(巻き戻し中間状態の Enter=cancel 代わり)は
        // remember_last_commit/undo 武装の対象外。非空は従来どおり書類を残す。
        assert!(!commit_keeps_records(""));
        assert!(commit_keeps_records("あ"));
    }
    #[test]
    fn undo_arms_only_on_full_commit_sources() {
        assert!(arms_undo("candidate"));
        assert!(arms_undo("live"));
        assert!(!arms_undo("candidate_prefix")); // 部分確定（composition 継続）
        assert!(!arms_undo("live_prefix"));
        assert!(!arms_undo("live_auto"));
        assert!(!arms_undo("mode_toggle")); // settle 系は対象外（設計ロック）
        assert!(!arms_undo("navigate"));
    }

    #[test]
    fn pure_modifier_keys_do_not_disarm() {
        for vk in [
            0x10u32, 0x11, 0x12, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C,
        ] {
            assert!(is_pure_modifier_vk(vk)); // Ctrl 押下自体で非武装化しない（Ctrl→BS の順押しを守る）
        }
        assert!(!is_pure_modifier_vk(0x08)); // Backspace
        assert!(!is_pure_modifier_vk(0x41)); // 'A'
    }

    // ---- 確定取消（Ctrl+Backspace）: キー配線 ----
    #[test]
    fn ctrl_backspace_is_eaten_only_when_undo_armed() {
        // CommitUndo（armed Ctrl+BS）は cmd_modifier=true として届くが carve-out で食う。
        assert!(will_handle_gated(
            0x08,
            false,
            false,
            true,
            false,
            false,
            false,
            KeyAction::CommitUndo
        )); // armed: 食う
        assert!(!will_handle_gated(
            0x08,
            false,
            false,
            true,
            false,
            false,
            false,
            KeyAction::None
        )); // 非武装: アプリの単語削除へ素通し
        assert!(!will_handle_gated(
            0x41,
            false,
            false,
            true,
            false,
            false,
            false,
            KeyAction::None
        )); // Ctrl+A は従来どおり素通し
    }

    #[test]
    fn undo_action_survives_awaiting_entrypoint() {
        // I-1: OnTestKeyDown は will_handle_awaiting 経由。CommitUndo の carve-out が冒頭の
        // cmd_modifier 早期 return より前にないと armed Ctrl+BS が殺される。
        assert!(will_handle_awaiting(
            0x08,
            false,
            false,
            true,
            false,
            false,
            /*awaiting=*/ false,
            false,
            KeyAction::CommitUndo
        )); // armed Ctrl+BS × awaiting=false → true
        assert!(!will_handle_awaiting(
            0x08,
            false,
            false,
            true,
            false,
            false,
            false,
            false,
            KeyAction::None
        )); // 非武装は従来どおり素通し
    }

    // ---- ephemeral かなモード: idle-abort（トリガ発火は keymap.rs の resolve_action テストが後継）----
    #[test]
    fn ephemeral_idle_abort_only_for_passthrough_actionless_keys_when_ephemeral() {
        // abort は「かな(native)が食わない」＝ !will_handle_gated(direct=false) と同義。
        // 素通しキー（矢印・nav idle）は abort=true（direct へ戻して素通し）。
        // 引数: (vk, cmd, shift, symbol_overlay, ephemeral, composing, showing, action)
        assert!(ephemeral_idle_abort(
            0x26,
            false,
            false,
            false,
            true,
            false,
            false,
            act(0x26, false, false, false)
        )); // ↑ → 素通し
        assert!(ephemeral_idle_abort(
            0x0D,
            false,
            false,
            false,
            true,
            false,
            false,
            act(0x0D, false, false, false)
        )); // Enter idle → 素通し
        assert!(ephemeral_idle_abort(
            0x1B,
            false,
            false,
            false,
            true,
            false,
            false,
            act(0x1B, false, false, false)
        )); // Esc idle → 素通し
            // 回帰ガード: romaji A–Z はかな(native)モードが食う（will_handle 真）ので abort=false。
            // F8 直後の1文字目で ephemeral を誤って抜けないこと。
        assert!(!ephemeral_idle_abort(
            0x41,
            false,
            false,
            false,
            true,
            false,
            false,
            act(0x41, false, false, false)
        )); // 'A' → 合成継続
            // アクションを持つキーは abort 対象外（CommitUndo/Ephemeral dispatch を殺さない）。
            // Ctrl+BS(armed) は will_handle の Ctrl ゲートで素通し扱いだが action==CommitUndo で守られる。
        assert!(!ephemeral_idle_abort(
            0x08,
            /*cmd=*/ true,
            false,
            false,
            true,
            false,
            false,
            KeyAction::CommitUndo
        ));
        assert!(!ephemeral_idle_abort(
            0x77,
            false,
            false,
            false,
            true,
            false,
            false,
            KeyAction::Ephemeral
        ));
        // 非武装 Ctrl+BS（action==None）は will_handle が食う（BS composing/showing のみだが idle は
        // 素通し）— idle なので abort=true（アプリの単語削除へ素通し）。
        assert!(ephemeral_idle_abort(
            0x08,
            false,
            false,
            false,
            true,
            false,
            false,
            KeyAction::None
        ));
        // ephemeral でなければ常に false。
        assert!(!ephemeral_idle_abort(
            0x26,
            false,
            false,
            false,
            false,
            false,
            false,
            KeyAction::None
        ));
        // composition 中/候補中は abort しない（畳む経路が別にある）。
        assert!(!ephemeral_idle_abort(
            0x26,
            false,
            false,
            false,
            true,
            true,
            false,
            KeyAction::None
        ));
        assert!(!ephemeral_idle_abort(
            0x26,
            false,
            false,
            false,
            true,
            false,
            true,
            KeyAction::None
        ));
    }

    #[test]
    fn ephemeral_survives_idle_digit_because_kana_eats_it_to_start_composition() {
        // かなモードは idle の無修飾数字を食って composition を開始する(will_handle_gated ②)。
        // よって ephemeral 直後の1打鍵目が数字でも ephemeral を抜けてはならない。
        for vk in [0x30u32, 0x31, 0x39, 0x60, 0x69] {
            assert!(
                !ephemeral_idle_abort(
                    vk,
                    false,
                    false,
                    false,
                    true,
                    false,
                    false,
                    act(vk, false, false, false)
                ),
                "vk {vk:#x}: かなが食う数字キーで ephemeral を抜けてはならない"
            );
        }
        // 記号トグル ON の Shift+数字行も「かなが食う」側（全角記号で合成を開始する）。
        // 既定キーマップで数字は未束縛なので action は None（act ヘルパは shift を取らないため直接渡す）。
        assert!(!ephemeral_idle_abort(
            0x31,
            false,
            true,
            true,
            true,
            false,
            false,
            KeyAction::None
        ));
        // トグル OFF の Shift+数字はかなが食わない＝素通しなので従来どおり abort する。
        assert!(ephemeral_idle_abort(
            0x31,
            false,
            true,
            false,
            true,
            false,
            false,
            KeyAction::None
        ));
    }

    #[test]
    fn ephemeral_action_is_eaten_at_both_entrypoints() {
        // will_handle_gated / will_handle_awaiting の先頭 carve-out（action != None）。
        // direct+idle で F8 は will_handle 上は false（素通し）だが Ephemeral アクションなら食う。
        assert!(will_handle_gated(
            0x77,
            false,
            false,
            false,
            true,
            false,
            false,
            KeyAction::Ephemeral
        ));
        assert!(!will_handle_gated(
            0x77,
            false,
            false,
            false,
            true,
            false,
            false,
            KeyAction::None
        ));
        // Ctrl 併用チョードでも殺されない（cmd_modifier=true でも action != None 優先）。
        assert!(will_handle_awaiting(
            0x4A,
            false,
            false,
            true,
            false,
            false,
            false,
            false,
            KeyAction::Ephemeral
        ));
        assert!(!will_handle_awaiting(
            0x4A,
            false,
            false,
            true,
            false,
            false,
            false,
            false,
            KeyAction::None
        ));
    }

    /// 既定 keymap のとき、リファクタ後の最終述語(will_handle_awaiting)が旧実装と
    /// 全キー×全文脈で一致する。残る方向差分は「修飾併用の厳格化」— チョードは修飾
    /// 完全一致だが、旧実装は shift と AltGr(Ctrl+Alt 同時=cmd_modifier では落ちない)を
    /// 見ていなかった。その場合は必ず 旧=食う/新=食わない の方向に限る。
    /// 本設計は逆向きの新規 eaten を意図的に導入する(spec §8 差分 f/g): 無変換(0x1D) の
    /// ModeToggle 全文脈救済、Space の direct+composing henkan(モード非依存 Convert)、および
    /// 再変換候補表示中の direct Backspace(本文復元取消)。これらは legacy baseline 側を新側へ
    /// 合わせて更新することで方向不変条件を保つ(下記アーム内コメント)。
    /// さらに半角/全角(0x19/0xF3/0xF4→正準 0xF3)の全文脈 eaten を導入する(2026-07-23 spec §8-1)。
    /// new 側は入口と同じく normalize_vk を通し、legacy 基準側は 3 VK とも eaten に更新した。
    #[test]
    fn default_keymap_is_behavior_equivalent_to_legacy_predicates() {
        use super::{is_digit_vk, is_main_row_digit_vk, is_oem_symbol_vk, is_text_vk};
        // ---- 旧実装の忠実な複製(削除したアームを含む)＋意図的な新規 eaten の baseline 更新 ----
        // ベースラインはマージ後のマスタ側最終述語(d9e8a32): 旧 will_handle は
        // Tab(0x09)/F6-F10(0x75..=0x79)/direct VK_CONVERT(0x1C) を食い、旧 gated は
        // 記号設定(symbol_overlay)の Shift+数字行 overlay を持っていた。
        // これに本設計の意図的差分 f/g(無変換 0x1D 救済・direct+composing Space)と、
        // 再変換候補中の direct Backspace を足して更新する。
        fn legacy_will_handle(
            vk: u32,
            composing: bool,
            showing: bool,
            cmd_modifier: bool,
            direct: bool,
        ) -> bool {
            if cmd_modifier {
                return false;
            }
            if direct {
                return match vk {
                    0x1C => true, // VK_CONVERT: direct 再変換(Reconvert)/composing・showing は Convert
                    // 意図的な新規 eaten（バグでなく無変換救済）: bare 無変換(0x1D) を ModeToggle が
                    // OnKeyDown で全文脈救済する（finding#3）。baseline を新側に合わせて更新。
                    0x1D => true,
                    // 意図的な新規 eaten(半角/全角 ModeToggle・spec §8-1): 0x19/0xF4 は
                    // 入口 normalize で 0xF3 に畳まれた上で全文脈 eaten。
                    0x19 | 0xF3 | 0xF4 => true,
                    // 意図的な新規 eaten（モード非依存 Convert）: direct+composing の Space も henkan
                    // ゲート下に入る（旧 direct は Space を showing のみ食っていた）。0x20 を切り出す。
                    0x20 => composing || showing,
                    // 意図的な新規 eaten（再変換取消）: 候補表示中の Backspace は RestoreText へ送る。
                    0x08 => showing,
                    0x0D | 0x1B | 0x26 | 0x28 => showing,
                    0x24 | 0x23 | 0x21 | 0x22 | 0x2E => showing,
                    0x31..=0x39 => showing,
                    _ => false,
                };
            }
            match vk {
                0x41..=0x5A => true,
                0x09 => composing, // VK_TAB
                0x20 | 0x0D | 0x1B | 0x08 => composing || showing,
                0x1C => composing || showing,
                // 意図的な新規 eaten（バグでなく無変換救済）: bare 無変換(0x1D) ModeToggle 救済（全文脈）。
                0x1D => true,
                // 意図的な新規 eaten(半角/全角 ModeToggle・spec §8-1。normalize 帰結で 3 VK とも)。
                0x19 | 0xF3 | 0xF4 => true,
                0x31..=0x39 => composing || showing,
                0x26 | 0x28 => showing,
                0x24 | 0x23 | 0x21 | 0x22 | 0x2E | 0x25 | 0x27 => composing || showing,
                0x75..=0x79 => composing, // VK_F6..=VK_F10
                vk if is_oem_symbol_vk(vk) => true,
                vk if is_text_vk(vk) => composing,
                _ => false,
            }
        }
        #[allow(clippy::too_many_arguments)]
        fn legacy_awaiting(
            vk: u32,
            composing: bool,
            showing: bool,
            cmd_modifier: bool,
            direct: bool,
            llm_enabled: bool,
            typo_enabled: bool,
            shift: bool,
            awaiting: bool,
            undo_hot: bool,
            ephemeral_hot: bool,
            symbol_overlay: bool,
        ) -> bool {
            if ephemeral_hot {
                return true;
            }
            if undo_hot {
                return true;
            }
            if cmd_modifier {
                return false;
            }
            if awaiting {
                return true;
            }
            // 旧 will_handle_gated (master d9e8a32)
            if vk == 0x09 && !(if shift { llm_enabled } else { typo_enabled }) {
                return false;
            }
            // 記号 overlay: Shift+数字行を食う(showing veto より前)。
            if !direct && !cmd_modifier && symbol_overlay && shift && is_main_row_digit_vk(vk) {
                return true;
            }
            if (0x31..=0x39).contains(&vk) && shift && showing && !composing {
                return false;
            }
            if !direct && !cmd_modifier && !shift && is_digit_vk(vk) {
                return true;
            }
            legacy_will_handle(vk, composing, showing, cmd_modifier, direct)
        }

        let km = crate::keymap::Keymap::default();
        let bools = [false, true];
        for vk in 0u32..=0xFF {
            for composing in bools {
                for showing in bools {
                    for direct in bools {
                        for ctrl in bools {
                            for shift in bools {
                                for alt in bools {
                                    for armed in bools {
                                        for awaiting in bools {
                                            for typo_en in bools {
                                                for llm_en in bools {
                                                    for symbol in bools {
                                                        for eph_en in bools {
                                                            let cmd = ctrl != alt;
                                                            // new 側は実入口(on_test/on_key_down)と同じく normalize_vk を通す。legacy 側は
                                                            // 生 vk のまま — だから legacy arm に 0x19/0xF3/0xF4 の 3 VK を明記してある。
                                                            let nvk =
                                                                crate::keymap::normalize_vk(vk);
                                                            let action =
                                                                crate::keymap::resolve_action(
                                                                    &km,
                                                                    &crate::keymap::ActionInput {
                                                                        vk: nvk,
                                                                        ctrl,
                                                                        shift,
                                                                        alt,
                                                                        composing,
                                                                        showing,
                                                                        direct,
                                                                        undo_armed: armed,
                                                                        ephemeral_enabled: eph_en,
                                                                        typo_enabled: typo_en,
                                                                        llm_enabled: llm_en,
                                                                    },
                                                                );
                                                            let new = will_handle_awaiting(
                                                                nvk, composing, showing, cmd,
                                                                direct, shift, awaiting, symbol,
                                                                action,
                                                            );
                                                            let legacy_undo =
                                                                armed && vk == 0x08 && ctrl && !alt;
                                                            let legacy_eph = eph_en
                                                                && direct
                                                                && !composing
                                                                && !showing
                                                                && vk == 0x77
                                                                && !cmd;
                                                            let old = legacy_awaiting(
                                                                vk,
                                                                composing,
                                                                showing,
                                                                cmd,
                                                                direct,
                                                                llm_en,
                                                                typo_en,
                                                                shift,
                                                                awaiting,
                                                                legacy_undo,
                                                                legacy_eph,
                                                                symbol,
                                                            );
                                                            if new != old {
                                                                // 厳格化された修飾 = shift、または AltGr(ctrl&&alt は cmd_modifier を
                                                                // 抜けるが既定チョードは無修飾なので新実装は食わない)。
                                                                assert!(
                        old && !new && (shift || (ctrl && alt)),
                        "許容外の差分: vk={vk:#04x} composing={composing} showing={showing} \
                         direct={direct} ctrl={ctrl} shift={shift} alt={alt} armed={armed} \
                         awaiting={awaiting} typo={typo_en} llm={llm_en} symbol={symbol} \
                         eph_en={eph_en} old={old} new={new}"
                    );
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn shift_latin_is_compose_defaults_safe() {
        use super::shift_latin_is_compose;
        assert!(shift_latin_is_compose("compose"));
        assert!(!shift_latin_is_compose("commit"));
        // 未知値は既定(compose)へ劣化 — 手編集 JSON で黙って旧挙動(直接確定)に化けない。
        assert!(shift_latin_is_compose("unknown"));
    }
}
