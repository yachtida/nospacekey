//! プロセス全体で共有するグローバル状態。
//! 永続ID（CLSID/PROFILE/表示属性GUID/LANGID）は `ids` クレートに移設し、
//! ここから再エクスポートして既存の `crate::globals::*` 参照を不変に保つ。

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use windows::core::GUID;
use windows::Win32::Foundation::HMODULE;

// 永続IDの唯一の真実源は `ids`。tip と testbench が同一値を参照する。
pub use ids::{CLSID_NOSPACEKEY, GUID_DISPLAY_ATTRIBUTE, LANGID_JA, PROFILE_NOSPACEKEY};

/// SP5: モードトグル（ひらがな⇄半角英数）の preserved key 識別 GUID。
pub const GUID_PRESERVEDKEY_MODE_TOGGLE: GUID = GUID::from_u128(0xc6963839_c572_45cb_a66d_25a4a4d704ba);
/// SP5: 再変換（直前ラテン列を変換）の preserved key 識別 GUID。
pub const GUID_PRESERVEDKEY_RECONVERT: GUID = GUID::from_u128(0x4006ec1a_2ef6_4f61_9822_174d550783cc);
/// US(ANSI)配列向け: モードトグル（Alt+;）の preserved key 識別 GUID。一度確定したら変更しない。
/// （Alt+` は OS がシステム IME on/off に予約し TIP へ届かないため Alt+; を採用。2026-06-25 実機確認）
pub const GUID_PRESERVEDKEY_MODE_TOGGLE_US: GUID = GUID::from_u128(0x87a01c5a_b74f_47af_98a8_d5c588d6f747);
/// US(ANSI)配列向け: 再変換（Alt+/）の preserved key 識別 GUID。一度確定したら変更しない。
pub const GUID_PRESERVEDKEY_RECONVERT_US: GUID = GUID::from_u128(0x23822a23_3ca1_401e_9b1c_4bfa1e6f0efb);
/// 品質ループ③: 誤変換フィードバック記録（Ctrl+変換 0x1C, JIS）の preserved key 識別 GUID。
/// 既存4登録と衝突しない新 GUID。一度確定したら変更しない。
pub const GUID_PRESERVEDKEY_FEEDBACK: GUID = GUID::from_u128(0x5b8ce2d1_7f3a_4d68_9a41_c1d20f6e3b57);
/// 品質ループ③: 誤変換フィードバック記録（Ctrl+/ 0xBF, US）の preserved key 識別 GUID。
pub const GUID_PRESERVEDKEY_FEEDBACK_US: GUID = GUID::from_u128(0x9e4f1c3b_2a6d_4e90_b7c8_51aa0d2f8e64);

/// SP6a: 候補リスト UIElement の識別 GUID（ITfUIElement::GetGUID が返す純粋な識別子。登録不要）。
/// 一度確定したら変更しないこと（SP5 preserved-key GUID と同じ規律）。
pub const GUID_UIELEMENT_CANDIDATELIST: GUID = GUID::from_u128(0x7d6f6f0a_6b3a_4e2b_9b4a_2f9e3b1c5d80);

/// DllCanUnloadNow が参照する DLL の参照カウント。
pub static DLL_REF: AtomicI32 = AtomicI32::new(0);

/// 全 `#[implement]` COM オブジェクトの生存数を `DLL_REF` で数える RAII ガード。
///
/// 各 COM オブジェクト（`TextService`・`ClassFactory`・各 edit session・`AttrEnum`/
/// `UnderlineInfo`・`CandidateListUIElement` 等）はこのガードを 1 つフィールドとして保持し、
/// 生成で +1 / Drop で -1 する。`DllCanUnloadNow` はこのカウントが 0 のときだけ `S_OK` を返す。
///
/// これを怠ると、最後の `TextService` が drop された後でもホストが他の独立寿命 COM オブジェクト
/// （edit session・UIElement・表示属性列挙子など）を保持している間に `DllCanUnloadNow` が `S_OK`
/// を返し、ホストが DLL を解放 → 生存オブジェクトの次の vtable 呼び出しで use-after-free になる。
///
/// COM オブジェクト以外に `power.rs` のプリウォームワーカも DLL 生存保持にこのガードを使う
/// （裏ワーカ稼働中に DLL がアンロードされるとワーカコードが宙に浮く AV を防ぐ。実装は共用）。
pub(crate) struct ComObjectGuard;

impl ComObjectGuard {
    pub(crate) fn new() -> Self {
        DLL_REF.fetch_add(1, Ordering::SeqCst);
        ComObjectGuard
    }
}

impl Drop for ComObjectGuard {
    fn drop(&mut self) {
        DLL_REF.fetch_sub(1, Ordering::SeqCst);
    }
}
/// DllMain で受け取る自分自身のモジュールハンドル（DLLパス取得に使う）。
/// `HMODULE` は内部に `*mut c_void` を1つ持つので、生ポインタを `AtomicPtr` で持てば
/// `static mut`（2024 edition の `static_mut_refs` が嫌う無同期共有）を避けられる。
/// 書き込みは DllMain(PROCESS_ATTACH) の1度だけ、読みは `hinst()` 経由。
static HINST: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// DllMain(PROCESS_ATTACH) で受け取ったモジュールハンドルを保存する。
pub(crate) fn set_hinst(h: HMODULE) {
    HINST.store(h.0, Ordering::SeqCst);
}

/// 保存済みのモジュールハンドルを取り出す（未設定なら null の HMODULE）。
pub(crate) fn hinst() -> HMODULE {
    HMODULE(HINST.load(Ordering::SeqCst))
}

/// この DLL（HINST）のフルパスを取得する。MAX_PATH(260) 固定バッファだと長いパスで
/// 切り詰められ、不正なパス（誤った InprocServer32 値・存在しない兄弟 exe）を黙って
/// 返してしまうため、戻り値がバッファ長に達したら（＝切り詰めの可能性）倍々で取り直す。
/// 取得不能（n==0）や拡張パス上限超過なら None。
pub(crate) fn module_file_path() -> Option<String> {
    use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
    let mut size = 260usize;
    loop {
        let mut buf = vec![0u16; size];
        // SAFETY: hinst() は DllMain(PROCESS_ATTACH) で set_hinst 済みのハンドルを
        // SeqCst で読み出す（未設定でも null の HMODULE で GetModuleFileNameW は実行ファイル
        // 自身を返すだけで未定義動作にはならない）。buf は size 要素を確保済み。
        let n = unsafe { GetModuleFileNameW(Some(hinst()), &mut buf) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(String::from_utf16_lossy(&buf[..n]));
        }
        // n == buf.len(): 切り詰めの可能性 → 拡張して取り直す（拡張パス上限 32767 を超えたら諦める）。
        if size >= 0x8000 {
            return None;
        }
        size *= 2;
    }
}
