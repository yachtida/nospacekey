//! TSF ホスト: STA に nospacekey プロファイルを自スレッド適用し、本物の VK を注入する。
//! 窓無し/ポンプ要否は未確定なので、防御的に message-only 窓＋pump を持つ。

use std::rc::Rc;

use windows::core::{w, Interface};
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Input::KeyboardAndMouse::HKL;
use windows::Win32::UI::TextServices::{
    ITfCandidateListUIElement, ITfCandidateListUIElementBehavior, ITfCompartmentMgr, ITfContext,
    ITfDocumentMgr, ITfInputProcessorProfileMgr, ITfInputProcessorProfiles, ITfKeystrokeMgr,
    ITfSource, ITfThreadMgr, ITfUIElementMgr,
    CLSID_TF_InputProcessorProfiles, CLSID_TF_ThreadMgr,
    GUID_COMPARTMENT_KEYBOARD_DISABLED, GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION,
    GUID_TFCAT_TIP_KEYBOARD,
    TF_INPUTPROCESSORPROFILE, TF_IPPMF_DONTCARECURRENTINPUTLANGUAGE, TF_IPPMF_ENABLEPROFILE,
    TF_IPP_FLAG_ACTIVE, TF_IPP_FLAG_ENABLED, TF_PROFILETYPE_INPUTPROCESSOR,
};
use windows::Win32::UI::TextServices::ITfUIElementSink;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DispatchMessageW, PeekMessageW, SetForegroundWindow, ShowWindow,
    TranslateMessage, MSG, PM_REMOVE, SW_SHOW, WINDOW_EX_STYLE, WS_POPUP,
};

use ids::{CLSID_NOSPACEKEY, LANGID_JA, PROFILE_NOSPACEKEY};

use crate::text_store::{HarnessTextStore, StoreState};
use crate::uielement_sink::{SinkLog, UiElementSink};

thread_local! {
    // --own-desktop: enter_own_desktop が成功したら true。ensure_foreground_window が
    // SetForegroundWindow(入力デスクトップでは無意味/失敗しうる)に加えてスレッドキューの
    // アクティブ/フォーカス状態を明示的に立てる分岐に使う。
    static OWN_DESKTOP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 子プロセス側マーカー: これが立っていれば既にゲート専用デスクトップ上で動いている。
pub const OWN_DESKTOP_ENV: &str = "NOSPACEKEY_TB_ON_GATE_DESKTOP";

/// 子プロセス側: 専用デスクトップ上での起動を認識し、前面補助(ensure_foreground_window の
/// SetActiveWindow/SetFocus 分岐)を有効化する。
pub fn mark_own_desktop() {
    OWN_DESKTOP.with(|c| c.set(true));
    eprintln!("DIAG own-desktop: running on winsta0\\nospacekey-gate (child)");
}

/// --own-desktop 親側: ゲート専用デスクトップ("nospacekey-gate")を作成し、**プロセスごと**
/// そのデスクトップで自分自身を再起動して終了コードを転送する。
/// 前面(foreground)はデスクトップごとに独立した資源なので、ユーザーが入力デスクトップを
/// 操作していても、専用デスクトップ上のゲートは自分の前面を取れる
/// (headless-gate-foreground-trap の回避)。SetThreadDesktop でスレッドだけ移す方式は
/// ActivateProfile→TIP Activate までは通るがキー配送(fForeground key sink)が通らなかった
/// ため、STARTUPINFO.lpDesktop による正攻法(UAC セキュアデスクトップ等と同型)を使う。
/// デスクトップハンドルは親が子の終了まで保持する(閉じると子の窓が無効になる)。
pub fn respawn_on_gate_desktop(args: &[String]) -> i32 {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    use windows::Win32::Foundation::{SetHandleInformation, HANDLE_FLAGS, HANDLE_FLAG_INHERIT};
    use windows::Win32::System::StationsAndDesktops::{CreateDesktopW, DESKTOP_CONTROL_FLAGS};
    use windows::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, WaitForSingleObject, INFINITE, PROCESS_CREATION_FLAGS,
        PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
    };
    unsafe {
        // GENERIC_ALL(0x10000000): 窓生成・切替・読み書きの全権。既存同名デスクトップは開かれる。
        let _desktop = match CreateDesktopW(
            w!("nospacekey-gate"), None, None, DESKTOP_CONTROL_FLAGS(0), 0x1000_0000, None,
        ) {
            Ok(d) => d,
            Err(e) => { eprintln!("own-desktop FAIL: CreateDesktopW {e:?}"); return 2; }
        };

        // 自分自身を同引数(--own-desktop 除去済み)で再起動。子は env マーカーで認識する。
        std::env::set_var(OWN_DESKTOP_ENV, "1");
        let exe = std::env::current_exe().expect("current_exe");
        let mut cmdline: Vec<u16> = Vec::new();
        cmdline.extend(format!("\"{}\"", exe.display()).encode_utf16());
        for a in args.iter().skip(1) {
            cmdline.extend(format!(" \"{a}\"").encode_utf16());
        }
        cmdline.push(0);

        // stdout/stderr(呼び出し側のリダイレクト先ファイル)を子へ継承する。
        let hout = GetStdHandle(STD_OUTPUT_HANDLE).unwrap_or(HANDLE::default());
        let herr = GetStdHandle(STD_ERROR_HANDLE).unwrap_or(HANDLE::default());
        let _ = SetHandleInformation(hout, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(HANDLE_FLAG_INHERIT.0));
        let _ = SetHandleInformation(herr, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(HANDLE_FLAG_INHERIT.0));

        let mut desktop_name: Vec<u16> = "nospacekey-gate\0".encode_utf16().collect();

        // CTF インフラ: フレッシュなデスクトップには ctfmon(Cicero ローダ)が居らず、msctf の
        // キー配送テーブルが成立しない疑いがある(activate は通るが KeyDown が素通しになる実測)。
        // UAC セキュアデスクトップ等と同様に、デスクトップごとの ctfmon を先に起動しておく。
        // 子ゲート終了後に必ず殺す(専用デスクトップごと片付ける)。
        let mut ctfmon_pi = PROCESS_INFORMATION::default();
        {
            let ctf_si = STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOW>() as u32,
                lpDesktop: PWSTR(desktop_name.as_mut_ptr()),
                ..Default::default()
            };
            let mut ctf_cmd: Vec<u16> =
                "C:\\Windows\\System32\\ctfmon.exe\0".encode_utf16().collect();
            match CreateProcessW(
                None, Some(PWSTR(ctf_cmd.as_mut_ptr())), None, None, false,
                PROCESS_CREATION_FLAGS(0), None, None, &ctf_si, &mut ctfmon_pi,
            ) {
                Ok(()) => {
                    eprintln!("DIAG own-desktop: ctfmon pid={} on gate desktop", ctfmon_pi.dwProcessId);
                    // Cicero 初期化の猶予(経験的に短くてよい)。
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(e) => eprintln!("DIAG own-desktop: ctfmon spawn failed {e:?} (continuing)"),
            }
            let _ = ctf_si;
        }

        let mut si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop_name.as_mut_ptr()),
            dwFlags: STARTF_USESTDHANDLES,
            hStdOutput: hout,
            hStdError: herr,
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        if let Err(e) = CreateProcessW(
            None, Some(PWSTR(cmdline.as_mut_ptr())), None, None, true,
            PROCESS_CREATION_FLAGS(0), None, None, &mut si, &mut pi,
        ) {
            eprintln!("own-desktop FAIL: CreateProcessW {e:?}");
            return 2;
        }
        eprintln!("DIAG own-desktop: child pid={} spawned on winsta0\\nospacekey-gate", pi.dwProcessId);
        let w = WaitForSingleObject(pi.hProcess, INFINITE);
        let mut code: u32 = 2;
        if w == WAIT_OBJECT_0 {
            let _ = GetExitCodeProcess(pi.hProcess, &mut code);
        }
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
        // ゲート用デスクトップの後片付け: 起動していれば ctfmon を終了する。
        if !ctfmon_pi.hProcess.is_invalid() {
            use windows::Win32::System::Threading::TerminateProcess;
            let _ = TerminateProcess(ctfmon_pi.hProcess, 0);
            let _ = CloseHandle(ctfmon_pi.hThread);
            let _ = CloseHandle(ctfmon_pi.hProcess);
        }
        code as i32
    }
}

/// COM(STA) アパートメントの RAII ガード。
/// TsfHost より先に宣言し、後に Drop されることで、全 COM 解放後に CoUninitialize する。
pub struct ComSta;

impl ComSta {
    /// STA で COM を初期化する。呼び出し側は host より先にこれを束縛すること。
    pub fn init() -> windows::core::Result<ComSta> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?; }
        Ok(ComSta)
    }
}

impl Drop for ComSta {
    fn drop(&mut self) {
        unsafe { CoUninitialize(); }
    }
}

/// 起動済みの TSF ホスト一式。Drop で DeactivateProfile/UnadviseSink/Pop/Deactivate する
/// （前面窓はプロセス共有なので破棄せず、ensure_foreground_window が次 host へ持ち回る）。
/// COM(STA) の初期化/終了は本体では行わない（呼び出し側の ComSta ガードが担う）。
pub struct TsfHost {
    thread_mgr: ITfThreadMgr,
    profiles: ITfInputProcessorProfileMgr,
    ksm: ITfKeystrokeMgr,
    doc_mgr: ITfDocumentMgr,
    _ctx: ITfContext,
    /// ThreadMgr::Activate が返したクライアント id。compartment SetValue に要る（item13）。
    tid: u32,
    pub store: Rc<StoreState>,
    activated: bool,
    /// SP6a item14 用: TIP の UIElement advertise（Begin/Update/End）を観測する sink のログ。
    ui_log: Rc<SinkLog>,
    /// AdviseSink が返した cookie（Drop で UnadviseSink に渡す）。
    ui_cookie: u32,
    /// advertise された UIElement を id で引いて候補データ/Behavior を読むためのマネージャ。
    ui_mgr: ITfUIElementMgr,
}

/// 非ブロッキングなメッセージポンプ（スレッドの全メッセージを drain）。
fn pump() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

thread_local! {
    // プロセス（=単一 STA スレッド）で前面窓を 1 枚だけ持ち回る。シナリオ毎に窓を作り直すと
    // 連続の SetForegroundWindow が Windows の前面ロックで弾かれ、数シナリオ後に TIP の
    // キーストロークシンク(fForeground=true)が実効せずキー不達になる（item5+ が落ちる現象）。
    static FOREGROUND_HWND: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

/// 前面窓を一度だけ生成し、以降は同じ窓を前面化して返す（プロセス終了まで破棄しない）。
fn ensure_foreground_window() -> windows::core::Result<HWND> {
    let cached = FOREGROUND_HWND.with(|c| c.get());
    let hwnd = if cached != 0 {
        HWND(cached as *mut core::ffi::c_void)
    } else {
        let h = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0), w!("STATIC"), w!("nospacekey-testbench"),
                WS_POPUP, 0, 0, 1, 1, None, None, None, None,
            )?
        };
        unsafe { let _ = ShowWindow(h, SW_SHOW); }
        FOREGROUND_HWND.with(|c| c.set(h.0 as isize));
        h
    };
    let fg_ok = unsafe { SetForegroundWindow(hwnd).as_bool() };
    // --own-desktop: 専用デスクトップは入力デスクトップでないため SetForegroundWindow が
    // 失敗/無意味になりうる。msctf のフォーカス追跡はスレッドキューのアクティブ/フォーカス
    // 状態を見るので、キュー内状態を明示的に立てて「前面フォーカスを持つ窓」を成立させる。
    if OWN_DESKTOP.with(|c| c.get()) {
        use windows::Win32::UI::Input::KeyboardAndMouse::{SetActiveWindow, SetFocus};
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        unsafe {
            let _ = SetActiveWindow(hwnd);
            let _ = SetFocus(Some(hwnd));
            // DIAG: 前面状態の実測(own-desktop の関門切り分け用)。
            let fg = GetForegroundWindow();
            eprintln!(
                "DIAG own-desktop: SetForegroundWindow ok={fg_ok} GetForegroundWindow={:?} self={:?} is_self={}",
                fg, hwnd, fg == hwnd
            );
        }
    }
    Ok(hwnd)
}

impl TsfHost {
    /// ThreadMgr→doc/context(自 store)→ActivateProfile→SetFocus。
    /// 事前に COM(STA) が初期化済みであること（呼び出し側の ComSta ガード）。
    pub fn start() -> windows::core::Result<TsfHost> {
        unsafe {
            let thread_mgr: ITfThreadMgr =
                CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?;
            let tid = thread_mgr.Activate()?;

            // msctf が TIP を活性化するには「前面フォーカスを持つ実ウィンドウ」を持つスレッド
            // である必要がある（TIP の AdviseKeyEventSink(fForeground=true) も前面を要求）。
            // 窓はプロセスで 1 枚を前面維持して再利用する（毎回作り直すと前面ロックで失敗する）。
            ensure_foreground_window()?;
            pump();

            // doc/context を自前 store で生成。
            let (store_if, store) = HarnessTextStore::create();
            let doc_mgr: ITfDocumentMgr = thread_mgr.CreateDocumentMgr()?;
            let mut ctx: Option<ITfContext> = None;
            let mut ec: u32 = 0;
            doc_mgr.CreateContext(tid, 0, &store_if, &mut ctx, &mut ec)?;
            let ctx = ctx.expect("CreateContext で ITfContext が None");
            doc_mgr.Push(&ctx)?;
            pump();

            // 登録済み nospacekey プロファイルを自スレッドへ適用（Activate→AdviseKeyEventSink）。
            // スレッドの現在入力言語を ja に切替えてから、現在入力言語に依存しない形で profile を
            // ENABLE+活性化する。dwflags=0／langid 不一致だと ActivateProfile が S_OK を返しても
            // 実際には TIP を活性化せず（Activate が呼ばれず）no-op になりうる。
            let ipp: ITfInputProcessorProfiles =
                CoCreateInstance(&CLSID_TF_InputProcessorProfiles, None, CLSCTX_INPROC_SERVER)?;
            let change_lang_result = ipp.ChangeCurrentLanguage(LANGID_JA);
            // DIAG: ChangeCurrentLanguage の戻り値 HRESULT（診断用、通常経路の分岐には使わない）。
            if let Err(e) = &change_lang_result {
                eprintln!("DIAG start: ChangeCurrentLanguage hr={:#010x} {}", e.code().0 as u32, e.message());
            } else {
                eprintln!("DIAG start: ChangeCurrentLanguage hr=S_OK");
            }
            let profiles: ITfInputProcessorProfileMgr = ipp.cast()?;
            let activate_result = profiles.ActivateProfile(
                TF_PROFILETYPE_INPUTPROCESSOR, LANGID_JA,
                &CLSID_NOSPACEKEY, &PROFILE_NOSPACEKEY, HKL::default(),
                TF_IPPMF_ENABLEPROFILE | TF_IPPMF_DONTCARECURRENTINPUTLANGUAGE,
            );
            // DIAG: ActivateProfile の戻り値 HRESULT（成功時も出力。現状は `?` で捨てていた）。
            match &activate_result {
                Ok(()) => eprintln!("DIAG start: ActivateProfile hr=S_OK"),
                Err(e) => eprintln!("DIAG start: ActivateProfile hr={:#010x} {}", e.code().0 as u32, e.message()),
            }
            activate_result?;
            pump();

            // DIAG: ActivateProfile 後の実際の活性状態を確認する。
            diag_check_activation_state(&profiles, &ipp);

            // 自 doc へフォーカス。前面窓＋このフォーカスで msctf が TIP を正規活性化する。
            thread_mgr.SetFocus(&doc_mgr)?;
            pump();

            let ksm: ITfKeystrokeMgr = thread_mgr.cast()?;

            // SP6a item14: TIP の UIElement advertise を観測する sink を AdviseSink する。
            // ITfUIElementMgr / ITfSource は同じ実 ThreadMgr の cast で得る（実 msctf が
            // BeginUIElement→sink へ配送し、GetUIElement で element を引ける）。
            // SetFocus 後（活性化後）に advise するので、初回フォーカスの advert は逃すが、
            // item14 は advise 後に変換を駆動して候補 advert を観測するので問題ない。
            let ui_mgr: ITfUIElementMgr = thread_mgr.cast()?;
            let source: ITfSource = thread_mgr.cast()?;
            let ui_log = Rc::new(SinkLog::default());
            let sink: ITfUIElementSink = UiElementSink { log: ui_log.clone() }.into();
            let ui_cookie = source.AdviseSink(&ITfUIElementSink::IID, &sink)?;

            Ok(TsfHost {
                thread_mgr, profiles, ksm, doc_mgr, _ctx: ctx, tid, store, activated: true,
                ui_log, ui_cookie, ui_mgr,
            })
        }
    }

    /// 本物の VK を 1 つ注入。TestKeyDown→KeyDown の順で、pfEaten を返す。
    pub fn feed_key(&self, vk: u32) -> bool {
        let w = WPARAM(vk as usize);
        let l = LPARAM(0x0001_0001); // repeat=1, scancode 相当
        crate::text_store::hlog(&format!("=== feed_key vk={vk:#x} KeyDown >>>"));
        let eaten = unsafe {
            if self.ksm.TestKeyDown(w, l).unwrap_or(FALSE).as_bool() {
                self.ksm.KeyDown(w, l).unwrap_or(FALSE).as_bool()
            } else {
                false
            }
        };
        crate::text_store::hlog(&format!("--- feed_key vk={vk:#x} KeyDown done eaten={eaten}, pump >>>"));
        pump(); // 候補窓/フォーカス post を drain
        crate::text_store::hlog(&format!("=== feed_key vk={vk:#x} pump done"));
        eaten
    }

    /// `vk` を Ctrl 押下状態で注入する（確定取消 Ctrl+Backspace 用。item30/31）。
    ///
    /// TIP 側の undo_hot 判定（key_event_sink.rs `undo_hot_now`）は `GetKeyState(VK_CONTROL)` を
    /// 読む実装なので、`SetKeyboardState` で VK_CONTROL(0x11) の下位ビットを立ててから
    /// `feed_key` と同じ TestKeyDown→KeyDown 経路で注入し、終了後に元の keyboard state へ復元する。
    /// 設計ロック 未確認(a): SetKeyboardState が GetKeyState に反映されない環境では undo_hot が
    /// 立たず item30/31 が偽 FAIL しうる（その場合は keybd_event 代替へ切替 — ヘッドレスでは
    /// 実効性を検証できず、実機/VM 受入で確認する）。
    pub fn feed_key_with_ctrl(&self, vk: u32) -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState};
        const VK_CONTROL: usize = 0x11;
        let mut state = [0u8; 256];
        let saved = unsafe { GetKeyboardState(&mut state) };
        let mut ctrl_down = state;
        ctrl_down[VK_CONTROL] |= 0x80;
        let set_ok = unsafe { SetKeyboardState(&ctrl_down) }.is_ok();
        crate::text_store::hlog(&format!(
            "=== feed_key_with_ctrl vk={vk:#x} saved={} set_ok={set_ok} >>>", saved.is_ok()
        ));
        let eaten = self.feed_key(vk);
        // 元の keyboard state へ復元（GetKeyboardState が失敗していたら全ゼロを書かず何もしない）。
        if saved.is_ok() {
            let _ = unsafe { SetKeyboardState(&state) };
        }
        crate::text_store::hlog(&format!("=== feed_key_with_ctrl vk={vk:#x} eaten={eaten} restored"));
        eaten
    }

    /// `vk` を Shift 押下状態で注入する（外部LLM変換 Shift+Tab 用。item12）。
    ///
    /// TIP 側の Tab 分岐（key_event_sink.rs `VK_TAB`）は `shift_down()`（`GetKeyState(VK_SHIFT)`）
    /// で Tab（修正変換）/Shift+Tab（外部LLM変換）を振り分ける実装なので、`feed_key_with_ctrl`
    /// と同じ `SetKeyboardState` 方式で VK_SHIFT(0x10) の下位ビットを立ててから注入する。
    /// 設計ロック 未確認(a)（feed_key_with_ctrl と同一の注記）: SetKeyboardState が GetKeyState に
    /// 反映されない環境では偽 FAIL しうる（その場合 keybd_event 代替 — 実機/VM 受入で確認する）。
    pub fn feed_key_with_shift(&self, vk: u32) -> bool {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState};
        const VK_SHIFT: usize = 0x10;
        let mut state = [0u8; 256];
        let saved = unsafe { GetKeyboardState(&mut state) };
        let mut shift_down = state;
        shift_down[VK_SHIFT] |= 0x80;
        let set_ok = unsafe { SetKeyboardState(&shift_down) }.is_ok();
        crate::text_store::hlog(&format!(
            "=== feed_key_with_shift vk={vk:#x} saved={} set_ok={set_ok} >>>", saved.is_ok()
        ));
        let eaten = self.feed_key(vk);
        // 元の keyboard state へ復元（GetKeyboardState が失敗していたら全ゼロを書かず何もしない）。
        if saved.is_ok() {
            let _ = unsafe { SetKeyboardState(&state) };
        }
        crate::text_store::hlog(&format!("=== feed_key_with_shift vk={vk:#x} eaten={eaten} restored"));
        eaten
    }

    /// 実機の「ホストが OnTestKeyDown を呼ばず OnKeyDown を直接呼ぶ」経路を忠実に模す（SP5 item19）。
    ///
    /// `ITfKeystrokeMgr::KeyDown` だけを叩き、`TestKeyDown` は呼ばない。msctf は OnKeyDown を
    /// 直接 TIP へ配送し OnTestKeyDown は呼ばれない（＝実機ログで ev=keytest が出ないのと一致）。
    /// 通常の `feed_key` は TestKeyDown が先に gate するため direct バグを再現できない（direct では
    /// TestKeyDown が false を返し KeyDown へ進まない）。direct の OnKeyDown gate 欠落バグを
    /// ヘッドレスで再現するための注入経路。pfEaten を返す。
    pub fn feed_key_keydown_only(&self, vk: u32) -> bool {
        let w = WPARAM(vk as usize);
        let l = LPARAM(0x0001_0001);
        crate::text_store::hlog(&format!("=== feed_key_keydown_only vk={vk:#x} KeyDown(no-test) >>>"));
        let eaten = unsafe { self.ksm.KeyDown(w, l).unwrap_or(FALSE).as_bool() };
        crate::text_store::hlog(&format!("--- feed_key_keydown_only vk={vk:#x} done eaten={eaten}, pump >>>"));
        pump();
        eaten
    }

    /// SP5 item13 用: conversion-mode compartment を半角英数(直接入力)へ設定する。
    ///
    /// TIP の `is_direct_mode()` は同じ実 `ITfThreadMgr` を
    /// `ITfCompartmentMgr` に cast → `GetCompartment(GUID_..._CONVERSION)` →
    /// `GetValue()` で読む。ここで NATIVE ビット(0x1)を落とした 0 を `SetValue` すれば
    /// `is_direct(0)==true` になり、変換キー(0x1C)→OnKeyDown(VK_CONVERT)→再変換が発火する。
    /// （PreserveKey(0x1C) は OS に拒否されるため OnPreservedKey 経由ではない。）
    /// 戻り値=書き込み成功（cast/GetCompartment/SetValue が全て Ok）。
    pub fn set_direct_mode(&self) -> bool {
        unsafe {
            let cm: ITfCompartmentMgr = match self.thread_mgr.cast() { Ok(c) => c, Err(_) => return false };
            let comp = match cm.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION) {
                Ok(c) => c, Err(_) => return false,
            };
            // NATIVE ビットを落とした 0（VT_I4）= 直接入力。TIP は VariantToInt32 で読む。
            let v = VARIANT::from(0i32);
            comp.SetValue(self.tid, &v).is_ok()
        }
    }

    /// item14 用: conversion-mode compartment をネイティブ入力(ひらがな)へ戻す。
    ///
    /// item13 は `set_direct_mode` で 0(直接入力)を書いたまま終わる。この compartment は
    /// 同一の実 `ITfThreadMgr` 上＝プロセス共有なので、scenario ごとに host を作り直しても残る。
    /// 次に走る item14 はネイティブ前提（"nihongo"+Space で候補）だが、直接入力のままだと TIP が
    /// キーを食わず候補が出ない。NATIVE ビット(0x1)を立てた値を書いて `is_direct(値)==false` に戻す。
    /// 戻り値=書き込み成功（cast/GetCompartment/SetValue が全て Ok）。
    pub fn set_native_mode(&self) -> bool {
        unsafe {
            let cm: ITfCompartmentMgr = match self.thread_mgr.cast() { Ok(c) => c, Err(_) => return false };
            let comp = match cm.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION) {
                Ok(c) => c, Err(_) => return false,
            };
            // NATIVE ビット(0x1)を立てた値 = ひらがな（ネイティブ入力）。is_direct(1)==false。
            let v = VARIANT::from(0x1i32);
            comp.SetValue(self.tid, &v).is_ok()
        }
    }

    /// item29 用: コンテキスト単位の compartment `GUID_COMPARTMENT_KEYBOARD_DISABLED` を書く。
    ///
    /// Chromium/Edge はパスワード欄（TEXT_INPUT_TYPE_PASSWORD）専用の ITfContext に
    /// この compartment を 1 (VT_I4) で立てて「IME 無効」を通知する
    /// （ui/base/ime/win/tsf_bridge.cc InitializeDisabledContext）。InputScope の方は
    /// IS_PASSWORD でなく IS_PRIVATE になるため、TIP は compartment を見ない限り
    /// パスワード欄を検知できない（実機発見バグ #1）。
    /// conversion-mode（thread 単位・set_direct_mode）と違い cast 元は context（self._ctx）。
    /// 戻り値=書き込み成功（cast/GetCompartment/SetValue が全て Ok）。
    pub fn set_context_keyboard_disabled(&self, disabled: bool) -> bool {
        unsafe {
            let cm: ITfCompartmentMgr = match self._ctx.cast() { Ok(c) => c, Err(_) => return false };
            let comp = match cm.GetCompartment(&GUID_COMPARTMENT_KEYBOARD_DISABLED) {
                Ok(c) => c, Err(_) => return false,
            };
            let v = VARIANT::from(if disabled { 1i32 } else { 0i32 });
            comp.SetValue(self.tid, &v).is_ok()
        }
    }

    /// item18 用: 別ウィンドウ/ドキュメントへのフォーカス喪失と復帰を模す。
    ///
    /// 2 つ目の空 `ITfDocumentMgr` を作って SetFocus → 元の doc へ戻す。実 msctf がスレッド
    /// マネージャのフォーカス変更として `ITfThreadMgrEventSink::OnSetFocus` を TIP へ配送する
    /// （実機の「別ウィンドウをクリック」相当。実機ではホストが live preedit を確定しつつ
    /// `OnCompositionTerminated` を呼ばないことがあり、エンジンの読みが居残る＝本バグ）。
    pub fn lose_and_regain_focus(&self) -> windows::core::Result<()> {
        unsafe {
            let other: ITfDocumentMgr = self.thread_mgr.CreateDocumentMgr()?;
            self.thread_mgr.SetFocus(&other)?;
            pump();
            // ユーザがクリックで元のウィンドウへ戻る。
            self.thread_mgr.SetFocus(&self.doc_mgr)?;
            pump();
        }
        Ok(())
    }

    /// 実機特有の罠への best-effort 防御: verify-harness が TIP を regsvr32 でシステム登録すると、
    /// 別アプリ（explorer / シェル等）が nospacekey を IME として活性化し前面/フォーカスを奪うことがある
    /// （nospacekey-tip.log に harness 以外の `[pid N] Activate` が混ざる＝item13 RUN2 で観測した pid 59912）。
    /// すると msctf がこのプロセスの進行中合成を terminate し、以降このスレッドへのキー配送が止まる
    /// （TestKeyDown が direct モードでも false を返す）。自分のドキュメントへ SetFocus し直して
    /// フォーカス/配送を取り戻す。クリーン VM では奪うアプリが居ないので実質 no-op。
    pub fn reclaim_focus(&self) {
        unsafe {
            let _ = self.thread_mgr.SetFocus(&self.doc_mgr);
        }
        pump();
    }

    // ===== SP6a item14: UIElement advertise の観測アクセサ =====

    /// 観測した advertise イベント（Begin/Update/End）のログ。
    pub fn ui_log(&self) -> &SinkLog { &self.ui_log }

    /// BeginUIElement の *pbShow をどう書き換えるか設定する。
    /// Some(false)=イマーシブ模擬（ホストが描く）/ Some(true)=デスクトップ / None=変更しない。
    pub fn force_pbshow(&self, v: Option<bool>) { self.ui_log.force_pbshow.set(v); }

    /// 最後に begun した UIElement の候補文字列を実 msctf 経由で読み戻す。
    /// GetUIElement→ITfCandidateListUIElement::GetCount/GetString。advert が無い/cast 失敗なら空。
    pub fn candidate_strings(&self) -> Vec<String> {
        let ids = self.ui_log.begun.borrow();
        let Some(&id) = ids.last() else { return vec![]; };
        let Ok(el) = (unsafe { self.ui_mgr.GetUIElement(id) }) else { return vec![]; };
        let Ok(cl) = el.cast::<ITfCandidateListUIElement>() else { return vec![]; };
        let n = unsafe { cl.GetCount() }.unwrap_or(0);
        (0..n)
            .map(|i| unsafe { cl.GetString(i) }.map(|b| b.to_string()).unwrap_or_default())
            .collect()
    }

    /// 最後に begun した UIElement の現在選択 index を実 msctf 経由で読み戻す（取れなければ 0）。
    pub fn candidate_selection(&self) -> u32 {
        let ids = self.ui_log.begun.borrow();
        let Some(&id) = ids.last() else { return 0; };
        let Ok(el) = (unsafe { self.ui_mgr.GetUIElement(id) }) else { return 0; };
        let Ok(cl) = el.cast::<ITfCandidateListUIElement>() else { return 0; };
        unsafe { cl.GetSelection() }.unwrap_or(0)
    }

    /// Behavior 経由でホスト選択＋確定を模擬（VM 実走時）。
    /// 最後に begun した UIElement を ITfCandidateListUIElementBehavior へ cast し
    /// SetSelection(k)→Finalize を呼ぶ。戻り値=cast＋呼び出しに到達できたか。
    pub fn behavior_select_and_finalize(&self, k: u32) -> bool {
        let ids = self.ui_log.begun.borrow();
        let Some(&id) = ids.last() else { return false; };
        let Ok(el) = (unsafe { self.ui_mgr.GetUIElement(id) }) else { return false; };
        let Ok(beh) = el.cast::<ITfCandidateListUIElementBehavior>() else { return false; };
        unsafe { let _ = beh.SetSelection(k); let _ = beh.Finalize(); }
        true
    }

    /// 測定打鍵に入る前にエンジンと合成を「温める」。
    ///
    /// 各シナリオは新しい TsfHost（=新規アクティベーション＋エンジン re-spawn）で走る。
    /// msctf はアクティベーション後の **最初の合成（=エンジンを同期 spawn する打鍵）** を
    /// KeyDown 復帰後のメッセージポンプ中に即終了させる（nospacekey-tip.log で確認できる、
    /// 実 Notepad には無いヘッドレス固有のアーティファクト）。即終了されると打ちかけの 1 文字目が
    /// 確定文字として文書に残り（孤児）、以降の合成がその前に挿入されて committed が壊れる。
    ///
    /// ここで測定前にダミー打鍵で「合成が生き残る」状態まで進め、Esc で取消＋エンジンセッションを
    /// 終了して綺麗な状態へ戻す。これにより測定打鍵は warm なエンジン＋落ち着いた文脈で始まり、
    /// 即終了による孤児が出ない。残骸（孤児文字）は呼び元の store.reset() が消す。
    pub fn warm_up(&self) {
        crate::text_store::hlog("=== warm_up >>>");
        // 最初の合成は spawn ブロックで即終了されるので、合成が生き残る（doc が composing）まで 'a' を打つ。
        for _ in 0..4 {
            let _ = self.feed_key(0x41); // 'a'
            if self.store.composing() { break; }
        }
        // Esc: 生き残った合成を取消し、エンジンセッションを終了して読みをクリアする。
        let _ = self.feed_key(0x1B); // VK_ESCAPE
        crate::text_store::hlog("=== warm_up <<<");
    }

    /// デバウンス変換タイマ（UI スレッド WM_TIMER, DEBOUNCE_MS≈30ms）を発火させる。
    ///
    /// SP3 は毎打鍵では reading を即 preedit に出し、入力が落ち着いたら（デバウンス）
    /// ライブ変換して preedit を漢字へ全置換する（arm_debounce → debounce_timer_proc →
    /// on_debounce_convert → engine_live_convert → run_preedit）。ヘッドレスでは feed_key が
    /// 即 pump するためタイマが発火せず preedit が reading のまま残る（実ユーザは打鍵間で
    /// 30ms 以上空くので実機では発火する）。実ユーザの「打ち終えて少し待つ」を模し、
    /// >DEBOUNCE_MS 待ってから pump してタイマ（=ライブ変換）を処理する。
    pub fn settle_debounce(&self) {
        std::thread::sleep(std::time::Duration::from_millis(60));
        pump();
    }

    /// 外部LLM変換（Tab）のワーカ IPC ＋ 結果ポーリングタイマ（WM_TIMER, LLM_POLL_MS≈50ms）を
    /// 落ち着かせる（item12 用）。
    ///
    /// Tab→start_llm_convert は別スレッドへ EngineClient を move して LlmConvert を投げ、
    /// UI スレッドは SetTimer(50ms) の WM_TIMER ポーリングで結果スロットを見る（llm_poll_proc）。
    /// ヘッドレスでは feed_key の即 pump 内ではタイマがまだ発火しておらず、かつワーカも
    /// 走り終えていない可能性がある。実ユーザの「Tab 後に少し待つ」を模し、デバウンス（60ms）
    /// より十分長く待ちつつ pump を繰り返して、ワーカ完了＋WM_TIMER 発火→反映を確実にする。
    /// echo モードでもワーカはプロセス spawn 済み engine への 1 往復 IPC なので余裕を見る。
    pub fn settle_llm(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(20));
            pump(); // 溜まった WM_TIMER を配送し llm_poll_proc を発火させる
            // preedit が「変換中…」から実結果へ置換されたら完了。
            if !self.store.preedit().contains('…') { break; }
            if std::time::Instant::now() >= deadline { break; }
        }
        // 念のためもう一度 pump（最後の置換 SetText を確実に store へ反映）。
        pump();
    }

    /// プロファイルを解除する（item9 用）。以降の feed_key は eaten=false になるはず。
    pub fn deactivate(&mut self) -> windows::core::Result<()> {
        if self.activated {
            unsafe {
                self.profiles.DeactivateProfile(
                    TF_PROFILETYPE_INPUTPROCESSOR, LANGID_JA,
                    &CLSID_NOSPACEKEY, &PROFILE_NOSPACEKEY, HKL::default(), 0,
                )?;
            }
            self.activated = false;
            pump();
        }
        Ok(())
    }
}

impl Drop for TsfHost {
    fn drop(&mut self) {
        let _ = self.deactivate();
        // SP6a item14: UIElement sink を解除（Deactivate より先）。cast 失敗は無視。
        if let Ok(source) = self.thread_mgr.cast::<ITfSource>() {
            unsafe { let _ = source.UnadviseSink(self.ui_cookie); }
        }
        unsafe {
            let _ = self.doc_mgr.Pop(windows::Win32::UI::TextServices::TF_POPF_ALL);
            let _ = self.thread_mgr.Deactivate();
            // 前面窓はプロセス共有なので破棄しない（ensure_foreground_window が次 host へ持ち回る）。
        }
    }
}

/// DIAG専用: ActivateProfile 直後の実際の活性状態を stderr に出す（診断計装、通常経路の分岐には使わない）。
///
/// - GetActiveProfile(GUID_TFCAT_TIP_KEYBOARD) で現在アクティブなキーボードプロファイルの
///   CLSID/guidProfile/langid/dwFlags を出し、nospacekey かどうか判定する。
/// - IsEnabledLanguageProfile(CLSID_NOSPACEKEY, ja) で nospacekey プロファイルの enabled 状態を見る。
/// - GetCurrentLanguage() でスレッドの現在入力言語を見る。
/// - EnumProfiles(ja) で列挙できる全プロファイルの CLSID/dwFlags(ACTIVE/ENABLED) を出す。
unsafe fn diag_check_activation_state(
    profile_mgr: &ITfInputProcessorProfileMgr,
    ipp: &ITfInputProcessorProfiles,
) {
    // GetActiveProfile: 現在アクティブなキーボードカテゴリのプロファイル。
    let mut active = TF_INPUTPROCESSORPROFILE::default();
    match profile_mgr.GetActiveProfile(&GUID_TFCAT_TIP_KEYBOARD, &mut active) {
        Ok(()) => {
            let is_nospacekey = active.clsid == CLSID_NOSPACEKEY;
            eprintln!(
                "DIAG activation: GetActiveProfile OK clsid={:?} guidProfile={:?} langid={:#06x} dwFlags={:#x} is_nospacekey={}",
                active.clsid, active.guidProfile, active.langid, active.dwFlags, is_nospacekey,
            );
        }
        Err(e) => eprintln!(
            "DIAG activation: GetActiveProfile FAIL hr={:#010x} {}",
            e.code().0 as u32, e.message(),
        ),
    }

    // IsEnabledLanguageProfile: nospacekey プロファイル自体が enabled とマークされているか。
    match ipp.IsEnabledLanguageProfile(&CLSID_NOSPACEKEY, LANGID_JA, &PROFILE_NOSPACEKEY) {
        Ok(enabled) => eprintln!("DIAG activation: IsEnabledLanguageProfile(nospacekey, ja) = {}", enabled.as_bool()),
        Err(e) => eprintln!(
            "DIAG activation: IsEnabledLanguageProfile FAIL hr={:#010x} {}",
            e.code().0 as u32, e.message(),
        ),
    }

    // GetCurrentLanguage: スレッドの現在入力言語。
    match ipp.GetCurrentLanguage() {
        Ok(langid) => eprintln!("DIAG activation: GetCurrentLanguage = {langid:#06x}"),
        Err(e) => eprintln!(
            "DIAG activation: GetCurrentLanguage FAIL hr={:#010x} {}",
            e.code().0 as u32, e.message(),
        ),
    }

    // EnumProfiles: ja に登録されている全プロファイルを列挙。
    match profile_mgr.EnumProfiles(LANGID_JA) {
        Ok(en) => {
            let mut buf = [TF_INPUTPROCESSORPROFILE::default()];
            let mut idx = 0usize;
            loop {
                let mut fetched: u32 = 0;
                match en.Next(&mut buf, &mut fetched) {
                    Ok(()) if fetched > 0 => {
                        let p = &buf[0];
                        eprintln!(
                            "DIAG activation: EnumProfiles[{idx}] clsid={:?} guidProfile={:?} dwFlags={:#x} active={} enabled={}",
                            p.clsid, p.guidProfile, p.dwFlags,
                            p.dwFlags & TF_IPP_FLAG_ACTIVE != 0,
                            p.dwFlags & TF_IPP_FLAG_ENABLED != 0,
                        );
                        idx += 1;
                    }
                    _ => break,
                }
            }
            eprintln!("DIAG activation: EnumProfiles total={idx}");
        }
        Err(e) => eprintln!(
            "DIAG activation: EnumProfiles FAIL hr={:#010x} {}",
            e.code().0 as u32, e.message(),
        ),
    }
}

/// Stage 0 スパイク本体。1 往復（key a → preedit 非空）を実証し、終了コードを返す。
pub fn stage0_spike() -> i32 {
    let _com = match ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("STAGE0 FAIL: ComSta::init error {e:?}"); return 2; }
    };
    let host = match TsfHost::start() {
        Ok(h) => h,
        Err(e) => { eprintln!("STAGE0 FAIL: TsfHost::start error {e:?}"); return 2; }
    };
    // VK_A = 0x41。OnKeyDown→run_preedit→SetText で store.preedit() が埋まるはず。
    let eaten = host.feed_key(0x41);
    let preedit = host.store.preedit();
    println!("STAGE0 eaten={eaten} preedit={preedit:?} full={:?}", host.store.full());
    if eaten && !preedit.is_empty() {
        println!("STAGE0 PASS");
        0
    } else {
        eprintln!("STAGE0 FAIL: eaten={eaten} preedit_empty={}", preedit.is_empty());
        1
    }
}

/// 活性化が成立しない原因の切り分け診断。nospacekey COM 直接生成の可否・start 経路・
/// feed 後の preedit・TIP がログを書いたか、を stderr に出す（要 regsvr32 登録済み）。
pub fn diag() -> i32 {
    let _com = match ComSta::init() {
        Ok(c) => c,
        Err(e) => { eprintln!("DIAG ComSta fail: {e:?}"); return 2; }
    };
    let tip_log = std::path::Path::new(&std::env::var("TEMP").unwrap_or_default()).join("nospacekey-tip.log");
    let _ = std::fs::remove_file(&tip_log); // この実行で TIP が書くか判定するため先に消す。

    // (1) nospacekey COM サーバをこのプロセスで直接生成できるか（=DLL がここでロード可能か）。
    let probe: windows::core::Result<windows::core::IUnknown> =
        unsafe { CoCreateInstance(&CLSID_NOSPACEKEY, None, CLSCTX_INPROC_SERVER) };
    match probe {
        Ok(_) => eprintln!("DIAG (1) CoCreateInstance(CLSID_NOSPACEKEY): OK — DLL はこのプロセスでロード可能"),
        Err(e) => eprintln!("DIAG (1) CoCreateInstance(CLSID_NOSPACEKEY): FAIL hr={:#010x} {}", e.code().0 as u32, e.message()),
    }

    // (2)(3) 通常の start() 経路 → feed 'a'。
    match TsfHost::start() {
        Ok(host) => {
            eprintln!("DIAG (2) TsfHost::start: OK");
            let eaten = host.feed_key(0x41);
            eprintln!("DIAG (3) feed 'a': eaten={eaten} preedit={:?} full={:?}", host.store.preedit(), host.store.full());
        }
        Err(e) => eprintln!("DIAG (2) TsfHost::start: FAIL {e:?}"),
    }

    // (4) TIP は何か書いたか（活性化＝Activate が走れば ev=activate を書くはず）。
    match std::fs::metadata(&tip_log) {
        Ok(m) => eprintln!("DIAG (4) tip.log: 存在 size={}b — TIP は活性化してログ書込み", m.len()),
        Err(_) => eprintln!("DIAG (4) tip.log: 無し — TIP は一度も活性化していない"),
    }
    0
}
