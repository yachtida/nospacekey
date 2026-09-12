//! Fixed, opt-in LLM-jp artifact pair used by inline prediction.

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::Emitter;

pub(crate) const MODEL_FILENAME: &str = "llm-jp-3-150m-q8_0-c060ca9.gguf";
pub(crate) const MODEL_SHA256: &str =
    "191f2fdf41a6f64f00ec6b4fcc39ec6164bb13b41d3609ac8f5b2b6149a23a6d";
pub(crate) const MODEL_LEN: u64 = 164_257_184;
pub(crate) const TOKENIZER_FILENAME: &str = "tokenizer.json";
pub(crate) const TOKENIZER_SHA256: &str =
    "955dc1fa623fab38cc92a3f4ee172423ae6d73201c4207569bfdf5626bc733f0";
pub(crate) const TOKENIZER_LEN: u64 = 6_416_433;
const VERIFIED_FILENAME: &str = "VERIFIED";
const VERIFIED_CONTENT: &str = concat!(
    "schema=1\n",
    "model_sha256=191f2fdf41a6f64f00ec6b4fcc39ec6164bb13b41d3609ac8f5b2b6149a23a6d\n",
    "tokenizer_sha256=955dc1fa623fab38cc92a3f4ee172423ae6d73201c4207569bfdf5626bc733f0\n",
);

// The quantized artifact is produced by the pinned llama.cpp revision documented in NOTICE.
// Publishing this release asset is a release prerequisite; the app never accepts a moving URL.
const MODEL_URL: &str = concat!(
    "https://github.com/yachtida/nospacekey/releases/download/inline-prediction-model-v1/",
    "llm-jp-3-150m-q8_0-c060ca9.gguf"
);
const TOKENIZER_URL: &str = concat!(
    "https://huggingface.co/llm-jp/llm-jp-3-150m/resolve/",
    "b112feef602fff752e4dac4c30af6a2c2fa41c7a/tokenizer.json"
);
const PROGRESS_EVENT: &str = "prediction-download-progress";

static DOWNLOADING: AtomicBool = AtomicBool::new(false);
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

struct DownloadGuard;
impl Drop for DownloadGuard {
    fn drop(&mut self) {
        DOWNLOADING.store(false, Ordering::SeqCst);
    }
}

#[derive(Clone, serde::Serialize)]
struct Progress {
    file: &'static str,
    received: u64,
    total: Option<u64>,
    percent: Option<u8>,
}

#[derive(serde::Serialize)]
pub struct PredictionModelStatus {
    pub state: &'static str,
    pub path: String,
}

pub(crate) fn prediction_model_dir(localappdata: &Path) -> PathBuf {
    localappdata
        .join("Nospacekey")
        .join("models")
        .join("inline-prediction")
}

fn progress_percent(received: u64, total: Option<u64>) -> Option<u8> {
    match total {
        Some(total) if total > 0 => Some(((received.min(total) * 100) / total) as u8),
        _ => None,
    }
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn artifact_valid(path: &Path, expected_len: u64, expected_hash: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.len() == expected_len)
        && sha256_file(path).is_ok_and(|actual| actual.eq_ignore_ascii_case(expected_hash))
}

fn local_model_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(|value| prediction_model_dir(Path::new(&value)))
}

pub(crate) fn local_model_is_ready() -> bool {
    local_model_dir().is_some_and(|dir| inspect_model_dir(&dir) == "ready")
}

fn inspect_model_dir(dir: &Path) -> &'static str {
    let model = dir.join(MODEL_FILENAME);
    let tokenizer = dir.join(TOKENIZER_FILENAME);
    if !model.exists() && !tokenizer.exists() {
        return "missing";
    }
    if artifact_valid(&model, MODEL_LEN, MODEL_SHA256)
        && artifact_valid(&tokenizer, TOKENIZER_LEN, TOKENIZER_SHA256)
        && std::fs::read_to_string(dir.join(VERIFIED_FILENAME))
            .is_ok_and(|receipt| receipt == VERIFIED_CONTENT)
    {
        "ready"
    } else {
        "invalid"
    }
}

#[tauri::command(async)]
pub fn prediction_model_status() -> PredictionModelStatus {
    let Some(dir) = local_model_dir() else {
        return PredictionModelStatus {
            state: "unavailable",
            path: String::new(),
        };
    };
    PredictionModelStatus {
        state: inspect_model_dir(&dir),
        path: dir.display().to_string(),
    }
}

#[tauri::command]
pub fn cancel_prediction_model_download() {
    CANCEL_REQUESTED.store(true, Ordering::SeqCst);
}

async fn download_artifact(
    client: &reqwest::Client,
    app: &tauri::AppHandle,
    url: &str,
    destination: &Path,
    label: &'static str,
    expected_len: u64,
    expected_hash: &str,
) -> Result<(), String> {
    let response = client.get(url).send().await.map_err(|error| {
        if CANCEL_REQUESTED.load(Ordering::SeqCst) {
            "キャンセルしました。".to_owned()
        } else {
            format!("{label} への接続に失敗しました: {error}")
        }
    })?;
    if !response.status().is_success() {
        return Err(format!(
            "{label} のダウンロードに失敗しました（HTTP {}）。",
            response.status()
        ));
    }
    let total = response.content_length();
    let mut stream = response.bytes_stream();
    let mut file = std::fs::File::create(destination)
        .map_err(|error| format!("一時ファイルを作成できません: {error}"))?;
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut last_percent = None;
    let mut last_bytes = 0u64;
    while let Some(item) = stream.next().await {
        if CANCEL_REQUESTED.load(Ordering::SeqCst) {
            return Err("キャンセルしました。".into());
        }
        let bytes = item.map_err(|error| format!("{label} の受信に失敗しました: {error}"))?;
        file.write_all(&bytes)
            .map_err(|error| format!("書き込みに失敗しました: {error}"))?;
        hasher.update(&bytes);
        received += bytes.len() as u64;
        let percent = progress_percent(received, total);
        if percent != last_percent || (percent.is_none() && received - last_bytes >= 1_048_576) {
            last_percent = percent;
            last_bytes = received;
            let _ = app.emit(
                PROGRESS_EVENT,
                Progress {
                    file: label,
                    received,
                    total,
                    percent,
                },
            );
        }
    }
    file.flush()
        .map_err(|error| format!("一時ファイルを保存できません: {error}"))?;
    drop(file);
    if CANCEL_REQUESTED.load(Ordering::SeqCst) {
        return Err("キャンセルしました。".into());
    }
    if received != expected_len {
        return Err(format!("{label} のサイズが一致しません。"));
    }
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_hash) {
        return Err(format!("{label} の整合性チェックに失敗しました。"));
    }
    Ok(())
}

fn rollback_install(destination: &Path, backup: Option<&Path>) -> Result<(), String> {
    if destination.exists() {
        std::fs::remove_dir_all(destination)
            .map_err(|error| format!("新モデルを除去できません: {error}"))?;
    }
    if let Some(backup) = backup {
        std::fs::rename(backup, destination)
            .map_err(|error| format!("旧モデルを復元できません: {error}"))?;
    }
    Ok(())
}

fn persist_prediction_enabled_with<L, S>(
    lock: &std::sync::Mutex<()>,
    load: L,
    save: S,
) -> Result<(), String>
where
    L: FnOnce() -> Result<settings::Settings, String>,
    S: FnOnce(&settings::Settings) -> Result<(), String>,
{
    let _settings_guard = lock
        .lock()
        .map_err(|_| "設定ロックを取得できませんでした".to_string())?;
    let mut current = load()?;
    current.inline_prediction.enabled = true;
    save(&current)
}

#[tauri::command]
pub async fn download_prediction_model(
    app: tauri::AppHandle,
    lock: tauri::State<'_, crate::logic::SettingsLock>,
) -> Result<String, String> {
    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        return Err("インライン予測モデルは既にダウンロード中です。".into());
    }
    let _guard = DownloadGuard;
    CANCEL_REQUESTED.store(false, Ordering::SeqCst);
    let destination = local_model_dir().ok_or("LOCALAPPDATA が解決できません。")?;
    let parent = destination.parent().ok_or("保存先パスが不正です。")?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("保存先フォルダを作成できません: {error}"))?;
    let stage = tempfile::Builder::new()
        .prefix("prediction-download-")
        .tempdir_in(parent)
        .map_err(|error| format!("一時フォルダを作成できません: {error}"))?;

    let client = reqwest::Client::builder()
        .user_agent(concat!("nospacekey-config/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|error| format!("HTTP クライアントを初期化できません: {error}"))?;
    download_artifact(
        &client,
        &app,
        MODEL_URL,
        &stage.path().join(MODEL_FILENAME),
        "モデル",
        MODEL_LEN,
        MODEL_SHA256,
    )
    .await?;
    download_artifact(
        &client,
        &app,
        TOKENIZER_URL,
        &stage.path().join(TOKENIZER_FILENAME),
        "tokenizer",
        TOKENIZER_LEN,
        TOKENIZER_SHA256,
    )
    .await?;
    std::fs::write(stage.path().join(VERIFIED_FILENAME), VERIFIED_CONTENT)
        .map_err(|error| format!("検証記録を保存できません: {error}"))?;

    let stop_code = tauri::async_runtime::spawn_blocking(crate::commands::stop_engine)
        .await
        .map_err(|error| format!("エンジン停止処理を完了できませんでした: {error}"))?;
    if stop_code != 0 {
        return Err("エンジンの停止を確認できませんでした。".into());
    }
    let pipe = ipc::client::stable_pipe_name();
    let _lease = crate::commands::EngineAbsenceLease::acquire(&pipe)
        .map_err(|error| format!("エンジンの停止を確認できませんでした: {error}"))?;
    if CANCEL_REQUESTED.load(Ordering::SeqCst) {
        return Err("キャンセルしました。".into());
    }

    let backup_holder = tempfile::Builder::new()
        .prefix("prediction-backup-")
        .tempdir_in(parent)
        .map_err(|error| format!("退避先を作成できません: {error}"))?;
    let backup = backup_holder.path().to_path_buf();
    std::fs::remove_dir(&backup).map_err(|error| format!("退避先を準備できません: {error}"))?;
    let had_previous = destination.exists();
    if had_previous {
        std::fs::rename(&destination, &backup)
            .map_err(|error| format!("旧モデルを退避できません: {error}"))?;
    }
    if let Err(error) = std::fs::rename(stage.path(), &destination) {
        let restore = if had_previous {
            std::fs::rename(&backup, &destination)
                .map_err(|restore| format!("旧モデルも復元できません: {restore}"))
        } else {
            Ok(())
        };
        return match restore {
            Ok(()) => Err(format!("モデルを配置できません: {error}")),
            Err(restore) => Err(format!("モデルを配置できません: {error}; {restore}")),
        };
    }

    // apply_settings や Zenzai の導入と同じ安全な mutation 経路を使う。
    // lock poison や読み取り拒否時に panic／既定値での上書きを起こさない。
    let save_result = persist_prediction_enabled_with(
        &lock.0,
        || {
            settings::load_for_mutation()
                .map_err(crate::commands::settings_mutation_error_for_download)
        },
        |settings| settings::save(settings).map_err(|error| error.to_string()),
    );
    if let Err(error) = save_result {
        return match rollback_install(&destination, had_previous.then_some(backup.as_path())) {
            Ok(()) => Err(format!("設定を保存できません: {error}")),
            Err(rollback) => Err(format!(
                "設定を保存できません: {error}; rollback失敗: {rollback}"
            )),
        };
    }
    if had_previous {
        let _ = std::fs::remove_dir_all(&backup);
    }
    Ok("モデルを導入し、インライン予測を有効にしました。".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn model_directory_matches_runtime_resolution() {
        assert!(prediction_model_dir(Path::new(r"C:\Users\x\AppData\Local"))
            .ends_with(Path::new("Nospacekey/models/inline-prediction")));
    }

    #[test]
    fn status_distinguishes_missing_and_invalid_pairs() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(inspect_model_dir(temp.path()), "missing");
        std::fs::write(temp.path().join(TOKENIZER_FILENAME), b"bad").unwrap();
        assert_eq!(inspect_model_dir(temp.path()), "invalid");
    }

    #[test]
    fn progress_is_bounded_and_unknown_safe() {
        assert_eq!(progress_percent(50, Some(100)), Some(50));
        assert_eq!(progress_percent(150, Some(100)), Some(100));
        assert_eq!(progress_percent(1, None), None);
    }

    #[test]
    fn rollback_removes_new_install_and_restores_previous_directory() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("inline-prediction");
        let backup = parent.path().join("backup");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("new"), b"new").unwrap();
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(backup.join("old"), b"old").unwrap();

        rollback_install(&destination, Some(&backup)).unwrap();

        assert!(destination.join("old").is_file());
        assert!(!destination.join("new").exists());
        assert!(!backup.exists());
    }

    #[test]
    fn prediction_enable_persists_without_replacing_other_settings() {
        let lock = std::sync::Mutex::new(());
        let mut existing = settings::Settings::default();
        existing.update.include_beta = true;
        let saved = RefCell::new(None);

        persist_prediction_enabled_with(
            &lock,
            || Ok(existing),
            |settings| {
                saved.replace(Some(settings.clone()));
                Ok(())
            },
        )
        .unwrap();

        let saved = saved.into_inner().unwrap();
        assert!(saved.inline_prediction.enabled);
        assert!(saved.update.include_beta);
    }

    #[test]
    fn prediction_enable_does_not_save_when_loading_for_mutation_fails() {
        let lock = std::sync::Mutex::new(());
        let save_called = Cell::new(false);

        let result = persist_prediction_enabled_with(
            &lock,
            || Err("settings unreadable".into()),
            |_| {
                save_called.set(true);
                Ok(())
            },
        );

        assert_eq!(result.unwrap_err(), "settings unreadable");
        assert!(!save_called.get());
    }

    #[test]
    fn prediction_enable_reports_poisoned_lock_without_loading() {
        let lock = std::sync::Mutex::new(());
        let _ = std::panic::catch_unwind(|| {
            let _guard = lock.lock().unwrap();
            panic!("poison test lock");
        });
        let load_called = Cell::new(false);

        let result = persist_prediction_enabled_with(
            &lock,
            || {
                load_called.set(true);
                Ok(settings::Settings::default())
            },
            |_| Ok(()),
        );

        assert!(result.unwrap_err().contains("設定ロック"));
        assert!(!load_called.get());
    }
}
