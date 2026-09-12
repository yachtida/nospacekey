//! 設定画面からの Zenzai モデルダウンロード。
//!
//! 原典（HuggingFace: Miwa-Keita/zenz-v3.1-small-gguf, CC-BY-SA-4.0）から直接取得し、
//! `%LOCALAPPDATA%\nospacekey\models\` に保存する。自前ホストしない＝本アプリは CC-BY-SA-4.0 の
//! 再配布者にならない（取得を仲介するだけ）。
//!
//! モデル解決はエンジン側 `ZenzaiConfig.resolve`（engine-host）と同一の3段表
//! 「明示 weight_path → per-user → exeDir\models」で行う（UIバグ8。両側を同時に触ること）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::Emitter;

/// Zenzai 既定モデルのファイル名。engine 側 `ZenzaiConfig.defaultWeightFileName` と一致させる。
const MODEL_FILENAME: &str = "ggml-model-Q5_K_M.gguf";

/// 原典の直リンク。GGUF は LFS 実体なので 302 で CDN へ飛ぶ（reqwest が既定でリダイレクト追従）。
const MODEL_URL: &str =
    "https://huggingface.co/Miwa-Keita/zenz-v3.1-small-gguf/resolve/main/ggml-model-Q5_K_M.gguf";

/// 既知 good の SHA256（実機コピーで検証済みの zenz-v3.1-small Q5_K_M）。
/// 上流がファイルを差し替えたらここを更新する（不一致は下の verify で明快に失敗する）。
const MODEL_SHA256: &str = "4de930c06bef8c263aa1aa40684af206db4ce1b96375b3b8ed0ea508e0b14f6c";

/// 進捗イベント名（フロントの listen と一致させる）。
const PROGRESS_EVENT: &str = "zenzai-download-progress";

/// 同時ダウンロードの排他フラグ。
static DOWNLOADING: AtomicBool = AtomicBool::new(false);
/// キャンセル要求フラグ（`cancel_zenzai_download` が立て、DL 処理の各チェックポイントが
/// 見る — 受信ループの各チャンク・send()/受信の Err 到達時・rename 前の再判定）。
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

/// `DOWNLOADING` を必ず戻すガード。early return / `?` / panic のいずれでも解除する
/// （さもないと一度失敗すると以後ずっと「既にダウンロード中」で締め出される）。
struct DownloadGuard;
impl Drop for DownloadGuard {
    fn drop(&mut self) {
        DOWNLOADING.store(false, Ordering::SeqCst);
    }
}

/// rename 完了前の `.part` を best-effort で掃除する。rename 後に disarm することで、
/// 後続の settings save 失敗時にも完成済みの本名ファイルを誤って削除しない。
struct PartCleanup {
    path: PathBuf,
    committed: bool,
}

impl PartCleanup {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    fn disarm(&mut self) {
        self.committed = true;
    }
}

impl Drop for PartCleanup {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// ============================================================================
// 純関数（単体テスト対象）
// ============================================================================

/// per-user のモデル保存先 `%LOCALAPPDATA%\nospacekey\models\<file>`。
pub fn user_model_path(localappdata: &Path) -> PathBuf {
    localappdata
        .join("nospacekey")
        .join("models")
        .join(MODEL_FILENAME)
}

/// SHA256 hex を大小無視で比較する。
pub fn sha256_hex_matches(actual: &str, expected: &str) -> bool {
    actual.eq_ignore_ascii_case(expected)
}

/// 進捗率 0..=100。total 不明（Content-Length 無し）や 0 のときは None。
pub fn progress_percent(received: u64, total: Option<u64>) -> Option<u8> {
    match total {
        Some(t) if t > 0 => Some(((received.min(t) * 100) / t) as u8),
        _ => None,
    }
}

/// モデルの実在探索。エンジンの解決順（weight_path → per-user → exeDir\models）に対応し、
/// 最初に実在した場所を `(path, source)` で返す。`source` は UI 表示用のラベル。
/// エンジン側 `ZenzaiConfig.resolve`（engine-host）も同一の解決表を持つ — この対応が
/// 崊れると UI の「導入済み」表示とエンジンの実解決が食い違う（UIバグ8。両側を同時に触ること）。
pub fn detect_model(
    weight_path: &str,
    user_models_file: &Path,
    exe_models_file: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Option<(PathBuf, &'static str)> {
    if !weight_path.is_empty() {
        let p = Path::new(weight_path);
        if exists(p) {
            return Some((p.to_path_buf(), "weight_path"));
        }
    }
    if exists(user_models_file) {
        return Some((user_models_file.to_path_buf(), "user"));
    }
    if exists(exe_models_file) {
        return Some((exe_models_file.to_path_buf(), "install"));
    }
    None
}

// ============================================================================
// tauri コマンド
// ============================================================================

/// UI へ返すモデル導入状況。
#[derive(serde::Serialize)]
pub struct ModelStatus {
    pub installed: bool,
    pub path: String,
    /// "weight_path" | "user" | "install" | ""（UI 表示・診断用）。
    pub source: String,
}

/// 進捗イベントのペイロード。
#[derive(Clone, serde::Serialize)]
struct Progress {
    received: u64,
    total: Option<u64>,
    percent: Option<u8>,
}

/// %LOCALAPPDATA%\nospacekey\models\<file>（無ければ空 PathBuf）。
/// 空文字で定義された LOCALAPPDATA も「無い」扱い — Swift 側 ZenzaiConfig.resolve の
/// `!localAppData.isEmpty` と対にし、相対パス候補で UI/エンジン解決表が分岐するのを防ぐ（巡1 G3-A）。
fn user_model_path_from_env() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .filter(|d| !d.is_empty())
        .map(|d| user_model_path(Path::new(&d)))
        .unwrap_or_default()
}

/// 設定アプリの exe と同じ場所の `models\<file>`（インストール同梱/管理者手動配置分）。
fn exe_model_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("models").join(MODEL_FILENAME)))
        .unwrap_or_default()
}

/// モデルが（どの解決経路であれ）実在するかを返す。UI の「導入済み/未導入」表示に使う。
// (async): settings::load + ファイル存在確認の I/O をメインスレッドから外す（UIバグ9）。
#[tauri::command(async)]
pub fn zenzai_model_status() -> ModelStatus {
    let weight_path = settings::load().zenzai.weight_path;
    match detect_model(
        &weight_path,
        &user_model_path_from_env(),
        &exe_model_path(),
        |p| p.exists(),
    ) {
        Some((path, source)) => ModelStatus {
            installed: true,
            path: path.display().to_string(),
            source: source.into(),
        },
        None => ModelStatus {
            installed: false,
            path: String::new(),
            source: String::new(),
        },
    }
}

/// 進行中ダウンロードのキャンセルを要求する（DL 処理の各チェックポイント — 次チャンク、
/// read_timeout 発の send()/受信 Err、受信ループ後・rename 前の再判定 — で気づいて
/// 中断・掃除する）。
#[tauri::command]
pub fn cancel_zenzai_download() {
    CANCEL_REQUESTED.store(true, Ordering::SeqCst);
}

/// Zenzai モデルを原典からダウンロードし per-user 領域へ配置、設定を更新してエンジンを再起動する。
/// 進捗は `PROGRESS_EVENT` イベントで通知。戻り値の文字列は UI 表示用のメッセージ。
#[tauri::command]
pub async fn download_zenzai_model(
    app: tauri::AppHandle,
    lock: tauri::State<'_, crate::logic::SettingsLock>,
) -> Result<String, String> {
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    use std::io::Write;

    // 排他: 既に走っていれば弾く。ガードで DOWNLOADING を必ず戻す。
    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        return Err("既にダウンロード中です。".into());
    }
    let _guard = DownloadGuard;
    CANCEL_REQUESTED.store(false, Ordering::SeqCst);

    let localappdata = std::env::var_os("LOCALAPPDATA")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "LOCALAPPDATA が解決できません。".to_string())?;
    // 空文字も「無い」扱い（巡2 D7 — 検出経路 user_model_path_from_env と Swift 側
    // ZenzaiConfig.resolve の !localAppData.isEmpty と対にし、DL 先がカレントディレクトリ
    // 基準の相対パスへ落ちて UI/エンジンの解決表が分岐するのを防ぐ）。
    let dest = user_model_path(&localappdata);
    let dir = dest
        .parent()
        .ok_or_else(|| "保存先パスが不正です。".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("保存先フォルダを作成できません: {e}"))?;
    // 完成前のファイルを本名で観測させない（＝中断された半端ファイルをエンジンに掴ませない）。
    let part = dir.join(format!("{MODEL_FILENAME}.part"));
    let mut part_cleanup = PartCleanup::new(part.clone());

    // UA 明示（無 UA を弾く CDN があるため）。TLS は native-tls（schannel）。
    // read_timeout は「バイトが来ない状態が続いたら諦める」上限。全体 timeout にしない理由:
    // 70MB を遅回線で落とすと正当でも長時間かかり、全体 timeout だと途中で殺してしまうため。
    // これが無いと接続が張れたまま無通信でストールした際、受信ループに入れず（＝キャンセルも
    // 効かず）guard も落ちずに永久ハングする（レビュー指摘）。
    let client = reqwest::Client::builder()
        .user_agent(concat!("nospacekey-config/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("HTTP クライアントの初期化に失敗: {e}"))?;
    let resp = match client.get(MODEL_URL).send().await {
        Ok(c) => c,
        Err(e) => {
            // 巡6(同型拡張): ヘッダ待ちのストールも read_timeout の Err として現れる —
            // 受信ループ(巡5 M-2)と同じく、キャンセルが立っているならキャンセル扱いにする。
            // この時点では .part 未作成なので scrub は不要。
            if CANCEL_REQUESTED.load(Ordering::SeqCst) {
                return Err("キャンセルしました。".into());
            }
            return Err(format!(
                "接続に失敗しました（ネットワークを確認してください）: {e}"
            ));
        }
    };
    if !resp.status().is_success() {
        return Err(format!(
            "ダウンロードに失敗しました（HTTP {}）。",
            resp.status()
        ));
    }
    let total = resp.content_length();

    let mut file =
        std::fs::File::create(&part).map_err(|e| format!("一時ファイルを作成できません: {e}"))?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    // 進捗イベントの間引き: チャンクは 8-16KB 刻みで来るので毎回 emit すると 70MB で数千発
    // webview へ飛んで UI がジャンクする。整数%が変わったとき（total 既知）か 1MB 毎（不明）だけ出す。
    let mut last_emit_pct: Option<u8> = None;
    let mut last_emit_bytes: u64 = 0;
    let mut stream = resp.bytes_stream();

    while let Some(item) = stream.next().await {
        if CANCEL_REQUESTED.load(Ordering::SeqCst) {
            return Err("キャンセルしました。".into());
        }
        let chunk = match item {
            Ok(c) => c,
            Err(e) => {
                // 巡5 M-2: ストール中のキャンセルは read_timeout の Err として現れる —
                // キャンセルが立っているなら「失敗」でなくキャンセル扱いにする。
                if CANCEL_REQUESTED.load(Ordering::SeqCst) {
                    return Err("キャンセルしました。".into());
                }
                return Err(format!("受信中にエラーが発生しました: {e}"));
            }
        };
        if let Err(e) = file.write_all(&chunk) {
            return Err(format!(
                "書き込みに失敗しました（ディスクの空き容量を確認してください）: {e}"
            ));
        }
        hasher.update(&chunk);
        received += chunk.len() as u64;
        let pct = progress_percent(received, total);
        let should_emit = match pct {
            Some(p) => Some(p) != last_emit_pct,
            None => received.saturating_sub(last_emit_bytes) >= 1_048_576,
        };
        if should_emit {
            last_emit_pct = pct;
            last_emit_bytes = received;
            let _ = app.emit(
                PROGRESS_EVENT,
                Progress {
                    received,
                    total,
                    percent: pct,
                },
            );
        }
    }
    let _ = file.flush();
    drop(file);

    // 巡3 Q9: 受信ループ後（ハッシュ計算・エンジン停止・rename の前）にキャンセルを再判定
    // する — 最終チャンク直後の数秒窓で押されたキャンセルが無視され、DL が成立してしまう
    // のを防ぐ。part を削除してキャンセル扱いで抜ける。
    if CANCEL_REQUESTED.load(Ordering::SeqCst) {
        let _ = std::fs::remove_file(&part);
        return Err("キャンセルしました。".into());
    }

    // 整合性チェック（既知 good の SHA256 と照合）。不一致は破棄して明快に失敗。
    let actual = hex::encode(hasher.finalize());
    if !sha256_hex_matches(&actual, MODEL_SHA256) {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "整合性チェックに失敗しました（上流ファイルが変わった可能性があります）。\n期待 {MODEL_SHA256}\n実際 {actual}"
        ));
    }

    // 反映のためのエンジン停止を rename の *前* に行う。再ダウンロード時、dest は稼働中エンジンが
    // mmap 保持しているファイルで、Windows では mmap 中のファイルへ rename すると共有違反で失敗する
    // （＝再DLが毎回失敗する）。graceful 停止（学習 flush 済み）で mmap を解放させてから置き換える。
    // 停止後は次の打鍵で新 settings により再 spawn され新モデルを読む。stop_engine はブロッキング
    // （最大 3s ポーリング）なので blocking スレッドへ逃がす。
    let stop_code = match tauri::async_runtime::spawn_blocking(crate::commands::stop_engine).await {
        Ok(code) => code,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(format!("エンジン停止処理を完了できませんでした: {e}"));
        }
    };
    if stop_code != 0 {
        let _ = std::fs::remove_file(&part);
        return Err("エンジンの停止を確認できませんでした。少し待って再試行してください。".into());
    }

    // connect失敗だけで code=0 になる起動窓と、停止確認直後のTIPによる再spawnを同時に塞ぐ。
    // Clear Learning の直接削除と同じ singleton object を新規作成できた場合だけ配置へ進み、
    // rename + settings保存が終わるまで保持する。既存/作成失敗は旧engineとの混在を避けて中止。
    let pipe = ipc::client::stable_pipe_name();
    let _engine_absence_lease = match crate::commands::EngineAbsenceLease::acquire(&pipe) {
        Ok(lease) => lease,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(format!("エンジンの停止を確認できませんでした: {e}"));
        }
    };

    // 巡4 B2 + 巡5 M-3: stop_engine 待ち（最悪約4s）の直後・rename 前にキャンセルを再判定 —
    // この窓で押されたキャンセルが無視され DL が成立して settings まで更新されるのを防ぐ。
    // 注: rename 後〜settings 保存の数十ms にはまだ窓が残る（保存後は DL 完了が確定するため
    // 「完全閉塞」ではなく主要窓の閉塞と位置づける）。
    if CANCEL_REQUESTED.load(Ordering::SeqCst) {
        let _ = std::fs::remove_file(&part);
        return Err("キャンセルしました。".into());
    }

    // 設定を更新する前に安全な mutation loader を通す。読み取り拒否/I/O 失敗時に
    // 既定値を保存して既存設定（特に DPAPI blob）を消さない。
    // read-modify-save は SettingsLock で適用(apply_settings)と直列化する。
    let _guard = lock
        .0
        .lock()
        .map_err(|_| "設定ロックを取得できませんでした".to_string())?;
    if CANCEL_REQUESTED.load(Ordering::SeqCst) {
        return Err("キャンセルしました。".into());
    }
    let mut s = settings::load_for_mutation()
        .map_err(crate::commands::settings_mutation_error_for_download)?;
    if CANCEL_REQUESTED.load(Ordering::SeqCst) {
        return Err("キャンセルしました。".into());
    }
    s.zenzai.weight_path = dest.to_string_lossy().to_string();
    s.zenzai.enabled = true;

    // 本名へ原子的に置き換え（同一ボリューム内 rename）。エンジン停止済みなので dest はロックされない。
    if let Err(e) = std::fs::rename(&part, &dest) {
        return Err(format!("モデルの配置に失敗しました: {e}"));
    }
    part_cleanup.disarm();
    settings::save(&s).map_err(|e| format!("設定の保存に失敗しました: {e}"))?;

    Ok("モデルを導入し、Zenzai を有効化しました（次の入力から反映されます）。".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_compare_is_case_insensitive() {
        assert!(sha256_hex_matches("ABCDef", "abcdEF"));
        assert!(sha256_hex_matches(
            MODEL_SHA256,
            &MODEL_SHA256.to_uppercase()
        ));
        assert!(!sha256_hex_matches("abcd", "abce"));
    }

    #[test]
    fn progress_percent_handles_unknown_and_caps() {
        assert_eq!(progress_percent(0, Some(100)), Some(0));
        assert_eq!(progress_percent(50, Some(100)), Some(50));
        assert_eq!(progress_percent(100, Some(100)), Some(100));
        // 実測が Content-Length を超えても 100 で頭打ち（負にも overflow にもしない）。
        assert_eq!(progress_percent(150, Some(100)), Some(100));
        // total 不明・0 は割れないので None。
        assert_eq!(progress_percent(10, None), None);
        assert_eq!(progress_percent(10, Some(0)), None);
    }

    #[test]
    fn user_model_path_layout() {
        let p = user_model_path(Path::new(r"C:\Users\x\AppData\Local"));
        assert!(p.ends_with(
            Path::new("nospacekey")
                .join("models")
                .join("ggml-model-Q5_K_M.gguf")
        ));
    }

    #[test]
    fn detect_model_prefers_weight_path_then_user_then_install() {
        let user = Path::new(r"C:\u\models\m.gguf");
        let exe = Path::new(r"C:\pf\models\m.gguf");
        let wp = r"C:\w\x.gguf";

        // weight_path が実在すれば最優先。
        let got = detect_model(wp, user, exe, |p| p == Path::new(wp));
        assert_eq!(got.unwrap().1, "weight_path");

        // weight_path 空 → per-user を見る。
        let got = detect_model("", user, exe, |p| p == user);
        assert_eq!(got.unwrap().1, "user");

        // per-user も無ければ install（exeDir\models）。
        let got = detect_model("", user, exe, |p| p == exe);
        assert_eq!(got.unwrap().1, "install");

        // どこにも無ければ None。
        assert!(detect_model("", user, exe, |_| false).is_none());

        // weight_path が設定されていても実在しなければスキップして per-user へ落ちる。
        let got = detect_model(wp, user, exe, |p| p == user);
        assert_eq!(got.unwrap().1, "user");
    }

    #[test]
    fn part_cleanup_removes_uncommitted_part_but_not_committed_file() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("model.part");
        std::fs::write(&part, b"partial").unwrap();
        {
            let _cleanup = PartCleanup::new(part.clone());
        }
        assert!(!part.exists());

        std::fs::write(&part, b"complete").unwrap();
        {
            let mut cleanup = PartCleanup::new(part.clone());
            cleanup.disarm();
        }
        assert_eq!(std::fs::read(&part).unwrap(), b"complete");
    }
}
