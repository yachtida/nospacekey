//! tauri コマンド層。logic（純関数）と settings crate を繋ぐだけの薄い層に保つ。

use crate::logic::{
    self, DictCmdError, DictLock, EngineStatus, ExportReportDto, FieldError, ImportReportDto, ListReport,
    MutationReport, SettingsDto,
};

#[derive(serde::Serialize)]
pub struct LoadResult {
    pub dto: SettingsDto,
    /// settings.json が存在し JSON として壊れていた（→ load() が隔離して既定に劣化した）。
    pub corrupt_recovered: bool,
}

#[tauri::command]
pub fn get_settings() -> LoadResult {
    // load() は破損ファイルを *.corrupt.* へ隔離して既定に劣化する（無言）。
    // UI で知らせるため、隔離が起きる前に自前で「存在するのに JSON 不正」を検知しておく。
    let corrupt_recovered =
        match settings::settings_path().and_then(|p| std::fs::read_to_string(p).ok()) {
            Some(text) if !text.trim().is_empty() => {
                serde_json::from_str::<serde_json::Value>(&text).is_err()
            }
            _ => false, // 不在・空・読み取り不可は「破損」とは扱わない（load() の方針と同じ）
        };
    let s = settings::load();
    LoadResult {
        dto: logic::to_dto(&s),
        corrupt_recovered,
    }
}

#[tauri::command]
pub fn apply_settings(dto: SettingsDto) -> Result<(), Vec<FieldError>> {
    // prev は適用時点のディスク上の値を読む（起動後に TIP 側で version 等が変わる可能性に備える）。
    let prev = settings::load();
    let s = logic::apply_dto(dto, &prev, settings::dpapi::encrypt)?;
    settings::save(&s).map_err(|e| {
        vec![FieldError {
            field: "_io".into(),
            message: format!("設定を保存できませんでした: {e}"),
        }]
    })
}

#[tauri::command]
pub fn get_default_settings() -> SettingsDto {
    logic::to_dto(&settings::Settings::default())
}

/// 記号個別選択グリッドの1項目(半角/全角プレビュー)。JS に写像表を持たせないための供給源
/// （2026-08-02 spec §3）。
#[derive(serde::Serialize)]
pub struct SymbolCatalogEntry {
    pub half: char,
    pub full: char,
}

/// 記号個別選択の対象29件カタログ（read-only、`get_default_settings` と同列）。
#[tauri::command]
pub fn get_symbol_catalog() -> Vec<SymbolCatalogEntry> {
    settings::symbol::symbol_targets()
        .map(|(half, full)| SymbolCatalogEntry { half, full })
        .collect()
}

#[derive(serde::Serialize)]
pub struct AppInfo {
    pub version: String,
    pub build_hash: String,
    pub settings_path: String,
}

#[tauri::command]
pub fn get_app_info() -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_hash: env!("GIT_HASH").to_string(),
        settings_path: settings::settings_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    }
}

/// Spec2: 学習履歴を消去する。常駐エンジンが居れば ClearLearning IPC（RAM+ディスクを
/// エンジン自身が消去 — mmap 競合なし）、居なければ %LOCALAPPDATA%\nospacekey\memory を
/// 直接削除（エンジン不在なので競合なし）。戻り値は消去経路（"engine"/"files" — UI 表示用）。
/// パイプ名は TIP と同じ per-logon-session 名（ipc::client::stable_pipe_name）。
///
/// `(async)` 必須（I-1）: Tauri v2 の同期 command は main スレッド実行のため、blocking な
/// pipe I/O で UI がフリーズする。さらに素の request()（deadline 無し）はエンジンが
/// warm-up（converterLock 数秒保持）やハング中だと無期限ブロックするので、A8 の
/// `request_within` で 2 秒の deadline を切る。
#[tauri::command(async)]
pub fn clear_learning_history() -> Result<String, String> {
    use std::time::{Duration, Instant};
    let pipe = ipc::client::stable_pipe_name();
    match ipc::client::EngineClient::connect_to(&pipe, Duration::from_millis(250)) {
        Ok(mut c) => {
            let deadline = Instant::now() + Duration::from_millis(2000);
            match c.request_within(&ipc::protocol::Request::ClearLearning, deadline) {
                Ok(ipc::protocol::Response::Ok) => Ok("engine".into()),
                Ok(ipc::protocol::Response::Error { message }) => {
                    Err(format!("エンジンが消去を拒否しました: {message}"))
                }
                Ok(other) => Err(format!("予期しない応答: {other:?}")),
                Err(e) => Err(format!(
                    "エンジンが応答しません（変換中の可能性）。少し待って再試行してください: {e}"
                )),
            }
        }
        Err(_) => {
            // エンジン不在（接続 250ms 失敗）: 標準の学習フォルダを直接削除。
            // 注: NOSPACEKEY_MEMORY_DIR override は開発/テスト用なのでここでは見ない。
            let Some(dir) = std::env::var_os("LOCALAPPDATA")
                .map(|d| std::path::PathBuf::from(d).join("nospacekey").join("memory"))
            else {
                return Err("LOCALAPPDATA が解決できません".into());
            };
            if dir.exists() {
                std::fs::remove_dir_all(&dir)
                    .map_err(|e| format!("学習フォルダを削除できませんでした: {e}"))?;
            }
            Ok("files".into())
        }
    }
}

#[tauri::command]
pub fn open_settings_dir() {
    // explorer /select,<path> でファイルを選択状態でフォルダを開く（ファイル不在でもフォルダは開く）。
    let Some(p) = settings::settings_path() else {
        return;
    };
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", p.display()))
        .spawn();
}

/// `--stop-engine` の終了コード判定（純関数）。sent=接続できて Shutdown を送った、
/// gone=その後 pipe が消えた（＝停止確認）。exit code は診断用（.iss は code に依らず taskkill へ
/// 進む）: 0 = 停止完了 or 元々不在 / 1 = 送ったが 3s 以内に消えない。
fn stop_engine_exit_code(sent: bool, gone: bool) -> i32 {
    if !sent || gone {
        0
    } else {
        1
    }
}

/// アンインストーラ/更新から `NospacekeyConfig.exe --stop-engine` で呼ばれる graceful 停止。
/// 常駐エンジンへ Request::Shutdown を送り（エンジンは学習 flush→応答後 exit）、pipe の消滅を
/// 最大 3s ポーリングして停止を確認する。GUI は出さない（main は Tauri init 前にこれを呼ぶ）。
/// パイプ名は TIP と同じ per-logon-session 名（stable_pipe_name）＝現セッション分のみ止まる。
/// 他ユーザセッションのエンジンや graceful 失敗分は .iss の elevated taskkill が掃討する。
pub fn stop_engine() -> i32 {
    use std::time::{Duration, Instant};
    let pipe = ipc::client::stable_pipe_name();
    let Ok(mut c) = ipc::client::EngineClient::connect_to(&pipe, Duration::from_millis(250)) else {
        // エンジン不在（接続失敗）＝止めるものが無い＝成功。
        return stop_engine_exit_code(false, false);
    };
    // engine は応答を書いてから exit するので、応答（Ok / 読取り時 broken pipe）は問わない —
    // 真の停止判定は下の pipe 消滅ポーリング。deadline は 1s（flush 込みでも余裕）。
    let deadline = Instant::now() + Duration::from_millis(1000);
    let _ = c.request_within(&ipc::protocol::Request::Shutdown, deadline);
    drop(c); // 送信済み接続は用済み。停止判定は新規 connect で行うので c は保持しない。
    // pipe 消滅を最大 3s ポーリング（connect(0ms) が Err になるまで 100ms 間隔）。
    let poll_until = Instant::now() + Duration::from_millis(3000);
    let mut gone = false;
    while Instant::now() < poll_until {
        if ipc::client::EngineClient::connect_to(&pipe, Duration::ZERO).is_err() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    stop_engine_exit_code(true, gone)
}

/// Releases ページ URL を repository から組み立てる純関数。末尾 `.git`／`/` を落として
/// `/releases/latest` を連結する。
fn releases_url(repo: &str) -> String {
    let base = repo
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_end_matches('/');
    format!("{base}/releases/latest")
}

/// 情報画面の「更新を確認」。既定ブラウザで Releases ページを開く（explorer に URL を
/// 渡して委譲＝ open_settings_dir と同型。新規依存・新規 capability 不要）。
#[tauri::command]
pub fn open_releases_page() {
    let _ = std::process::Command::new("explorer.exe")
        .arg(releases_url(env!("CARGO_PKG_REPOSITORY")))
        .spawn();
}

/// Zenzai モデルの帰属表示（作者ページ / ライセンス）を既定ブラウザで開く。
/// URL は allowlist に固定する: フロントから任意 URL を shell へ渡せると、汚染された
/// UI 文字列で任意サイト（ローカル UNC 含む）を開かせる余地が生まれるため。
#[tauri::command]
pub fn open_external_url(url: String) {
    if is_allowed_external_url(&url) {
        let _ = std::process::Command::new("explorer.exe").arg(&url).spawn();
    }
}

/// 開いてよい外部 URL（帰属表示リンク）の allowlist 判定（純関数）。
fn is_allowed_external_url(url: &str) -> bool {
    const ALLOW: &[&str] = &[
        "https://huggingface.co/Miwa-Keita/zenz-v3.1-small-gguf",
        "https://creativecommons.org/licenses/by-sa/4.0/",
    ];
    ALLOW.contains(&url)
}

// ============================================================================
// カスタム辞書 CRUD(Issue #3 spec §5.3)。logic::dict_*_logic を State から借りて
// 呼ぶだけの薄層に保つ(mutation 本体・直列化は logic.rs 側で単体テスト済み)。
// ============================================================================

/// 辞書系 IPC の接続 timeout(spec §4.2)。`clear_learning_history` の 250ms より短縮する —
/// mutation ごとにエンジン不在で待たされると連続登録の体感に響くため、ローカル pipe の
/// 生存確認としては 100ms で十分。
const DICT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);
const DICT_REQUEST_DEADLINE: std::time::Duration = std::time::Duration::from_millis(2000);

/// ReloadDictionary を実際にパイプへ送る唯一の実装。enabled は呼び出し側
/// (`reload_payload` でゲート済み)から渡す — ここで settings を読み直さない
/// (logic 層が enabled をでっち上げない不変条件と対の規律)。
fn send_reload_over_pipe(enabled: bool) -> EngineStatus {
    use std::time::Instant;
    let pipe = ipc::client::stable_pipe_name();
    let Ok(mut c) = ipc::client::EngineClient::connect_to(&pipe, DICT_CONNECT_TIMEOUT) else {
        return EngineStatus::Absent;
    };
    let deadline = Instant::now() + DICT_REQUEST_DEADLINE;
    match c.request_within(&ipc::protocol::Request::ReloadDictionary { enabled }, deadline) {
        Ok(ipc::protocol::Response::Ok) => EngineStatus::Applied,
        Ok(ipc::protocol::Response::Error { .. }) => EngineStatus::Declined,
        Ok(_) => EngineStatus::Declined, // 未知応答は旧エンジンの拒否と同型に畳む
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => EngineStatus::Timeout,
        Err(_) => EngineStatus::Absent, // 送信後の切断等も「反映されなかった」で無害に畳む
    }
}

/// ReloadDictionary を送る全コマンド共通のゲート(spec §4.2)。送る enabled は常にディスクの
/// settings.json から読む(UI の未適用トグル値は送らない)。読み取りが `Loaded`/`Missing`
/// 以外なら `reload_payload` が送信を止める。
fn reload_sender() -> impl Fn() -> EngineStatus {
    let (s, outcome) = settings::load_reporting();
    move || match logic::reload_payload(outcome, s.user_dictionary.enabled) {
        Some(enabled) => send_reload_over_pipe(enabled),
        None => EngineStatus::Absent,
    }
}

fn dict_path_or_err() -> Result<std::path::PathBuf, DictCmdError> {
    settings::user_dictionary::dict_path()
        .ok_or_else(|| DictCmdError::Io { message: "LOCALAPPDATA が解決できません".into() })
}

#[tauri::command(async)]
pub fn dict_list(lock: tauri::State<DictLock>) -> Result<ListReport, DictCmdError> {
    logic::dict_list_logic(&lock, &dict_path_or_err()?)
}

#[tauri::command(async)]
pub fn dict_add(
    lock: tauri::State<DictLock>,
    ruby: String,
    word: String,
    pos: String,
) -> Result<MutationReport, DictCmdError> {
    let path = dict_path_or_err()?;
    logic::dict_add_logic(&lock, &path, &reload_sender(), &ruby, &word, &pos)
}

#[tauri::command(async)]
pub fn dict_update(
    lock: tauri::State<DictLock>,
    old_ruby: String,
    old_word: String,
    ruby: String,
    word: String,
    pos: String,
) -> Result<MutationReport, DictCmdError> {
    let path = dict_path_or_err()?;
    logic::dict_update_logic(&lock, &path, &reload_sender(), &old_ruby, &old_word, &ruby, &word, &pos)
}

#[tauri::command(async)]
pub fn dict_delete(lock: tauri::State<DictLock>, ruby: String, word: String) -> Result<MutationReport, DictCmdError> {
    let path = dict_path_or_err()?;
    logic::dict_delete_logic(&lock, &path, &reload_sender(), &ruby, &word)
}

/// TSV ファイルを選んでインポートする。ダイアログ表示・ファイル読み込みは mutex の外
/// (spec §5.3)。キャンセルは `Ok(None)`。
#[tauri::command(async)]
pub fn dict_import(
    app: tauri::AppHandle,
    lock: tauri::State<DictLock>,
) -> Result<Option<ImportReportDto>, DictCmdError> {
    use tauri_plugin_dialog::DialogExt;
    let Some(picked) = app.dialog().file().add_filter("TSV", &["tsv", "txt"]).blocking_pick_file() else {
        return Ok(None);
    };
    let file_path = picked.into_path().map_err(|e| DictCmdError::Io { message: e.to_string() })?;
    let bytes = std::fs::read(&file_path).map_err(|e| DictCmdError::Io { message: e.to_string() })?;
    let path = dict_path_or_err()?;
    logic::dict_import_logic(&lock, &path, &reload_sender(), &bytes).map(Some)
}

/// TSV ファイルへエクスポートする。保存先ダイアログは mutex の外(spec §5.3)。
/// キャンセルは `Ok(None)`。
#[tauri::command(async)]
pub fn dict_export(
    app: tauri::AppHandle,
    lock: tauri::State<DictLock>,
) -> Result<Option<ExportReportDto>, DictCmdError> {
    use tauri_plugin_dialog::DialogExt;
    let Some(picked) =
        app.dialog().file().add_filter("TSV", &["tsv"]).set_file_name("user_dictionary.tsv").blocking_save_file()
    else {
        return Ok(None);
    };
    let file_path = picked.into_path().map_err(|e| DictCmdError::Io { message: e.to_string() })?;
    let path = dict_path_or_err()?;
    let (tsv, report) = logic::dict_export_logic(&lock, &path)?;
    std::fs::write(&file_path, tsv.as_bytes()).map_err(|e| DictCmdError::Io { message: e.to_string() })?;
    Ok(Some(report))
}

/// settings 適用成功後にフロントが fire-and-forget で呼ぶトグル反映(spec §4.2)。
/// エントリ mutation(§4.1 の起動時 enqueue で救済される)と異なり、トグル適用は engine
/// init〜pipe 作成の短い窓に落ちると次回まで無言で効かないため、接続失敗時のみ
/// 300ms×3 リトライして窓を実質閉塞する。
#[tauri::command(async)]
pub fn dict_sync_engine() -> EngineStatus {
    let send = reload_sender();
    for attempt in 0..3u32 {
        match send() {
            EngineStatus::Absent if attempt + 1 < 3 => {
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            status => return status,
        }
    }
    EngineStatus::Absent
}

#[cfg(test)]
mod tests {
    use super::{get_symbol_catalog, is_allowed_external_url, releases_url, stop_engine_exit_code};

    #[test]
    fn symbol_catalog_returns_29_entries_excluding_dash_comma_period() {
        let catalog = get_symbol_catalog();
        assert_eq!(catalog.len(), 29);
        assert!(catalog.iter().all(|e| !matches!(e.half, '-' | ',' | '.')));
    }

    #[test]
    fn external_url_allowlist_admits_only_attribution_links() {
        assert!(is_allowed_external_url(
            "https://huggingface.co/Miwa-Keita/zenz-v3.1-small-gguf"
        ));
        assert!(is_allowed_external_url(
            "https://creativecommons.org/licenses/by-sa/4.0/"
        ));
        // 近いが別物・任意 URL・UNC は弾く（前方一致ではなく完全一致）。
        assert!(!is_allowed_external_url(
            "https://huggingface.co/Miwa-Keita/zenz-v3.1-small-gguf/evil"
        ));
        assert!(!is_allowed_external_url("https://example.com"));
        assert!(!is_allowed_external_url(r"\\attacker\share"));
        assert!(!is_allowed_external_url(""));
    }

    #[test]
    fn releases_url_appends_latest_and_trims_dot_git() {
        assert_eq!(
            releases_url("https://github.com/o/r"),
            "https://github.com/o/r/releases/latest"
        );
        assert_eq!(
            releases_url("https://github.com/o/r.git"),
            "https://github.com/o/r/releases/latest"
        );
        assert_eq!(
            releases_url("https://github.com/o/r/"),
            "https://github.com/o/r/releases/latest"
        );
    }

    #[test]
    fn stop_engine_exit_code_maps_sent_and_gone() {
        // 不在（接続できず）＝止めるものが無い＝成功（gone は無意味）。
        assert_eq!(stop_engine_exit_code(false, false), 0);
        assert_eq!(stop_engine_exit_code(false, true), 0);
        // 送って pipe が消えた＝停止確認＝成功。
        assert_eq!(stop_engine_exit_code(true, true), 0);
        // 送ったが 3s 以内に pipe が消えない＝診断用の失敗（呼び出し元 .iss は taskkill へ進む）。
        assert_eq!(stop_engine_exit_code(true, false), 1);
    }
}
