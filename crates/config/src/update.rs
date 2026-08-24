//! 設定画面からのアプリ内アップデート確認・適用。
//!
//! GitHub の public repo (yachtida/nospacekey) のリリース一覧を API で問い合わせ、
//! 現在ビルドの CARGO_PKG_VERSION と比較する。新版本があればインストーラ(setup exe)を
//! %TEMP% 直下に試行ごと OS 乱数で一意に作る専用 staging ディレクトリへダウンロードし
//! SHA256 で検証、ShellExecuteExW の runas で昇格起動する。
//! SHA256 検証は二段 — 受信ストリームの照合に加え、昇格実行の前に（コマンド実行境界で）
//! ディスク上の実ファイルを再ハッシュし、検証後の置換・破損を昇格実行に持ち込まない。
//! 境界再検証〜起動成立までは staging directory と InstallerGuard の両ハンドルを保持する。
//! staging は削除共有なしで開いて親の rename/junction 差替えを拒否し、dest の最終親パスが
//! その directory handle 由来の最終パスと一致することも確認する。InstallerGuard は dest を
//! FILE_FLAG_OPEN_REPARSE_POINT で開いて reparse point なら即拒否、read 専用 +
//! share_mode=FILE_SHARE_READ（読み手は共有し書き込み・削除の同時オープンを拒否）、全範囲を
//! LockFileEx の排他 FAIL_IMMEDIATELY で押さえる。SHA256 はその同一ハンドルから算出し、
//! lpFile も同じハンドル由来の最終正規パス（GetFinalPathNameByHandleW）で渡す — 開いて閉じ、
//! 後からパスで起動すると、同一ユーザー別プロセスによる書き換え・置換・親リンク差替えが
//! そのまま昇格実行に通る（hash-to-elevated-exec TOCTOU）。
//! 期待ハッシュは SHA256SUMS.txt から導くが、導出できないリリース（添付漏れ・取得失敗・
//! 対象行なし）は検証なしでの昇格実行を許さず失敗させる（fail-closed）。
//! DL 元 URL は API 応答由来・コマンド引数由来を問わず、公式リリースアセット
//! (`https://github.com/yachtida/nospacekey/releases/download/{tag}/{file}`) に厳密一致し、
//! かつ canonical な新しい setup asset として tag/file が一致するものだけを許す
//! （validate_installer_asset_url）。改竄応答も直叩き引数もダウンロード前に失敗させる。
//! インストーラ(Inno Setup・per-machine)の PrepareToInstall が config/engine を taskkill し
//! restartreplace で使用中 DLL を置換するため、アプリ側は起動後ただ終了すればよい。
//!
//! `include_beta` で pre-release(beta) を通知に含めるか制御する（既定=false=安定版のみ）。
//! 自動確認は行わない（プライバシ: ユーザの明示操作時のみネットワークへ出る）。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::Emitter;

/// 進捗イベント名（フロントの listen と一致させる）。download.rs とは別系列。
const PROGRESS_EVENT: &str = "update-download-progress";

/// staging ディレクトリの接頭辞（%TEMP% 直下・試行ごとに OS 乱数で一意な名前が続く）。
const STAGING_DIR_PREFIX: &str = "nospacekey-update-";

/// staging ディレクトリ内のインストーラ保存名（固定）。ディレクトリ側が一意なので
/// 他試行・他プロセスと衝突しない。完成前は `.part` を付けて隠し、rename で本名へ出す。
const INSTALLER_FILENAME: &str = "nospacekey-update-setup.exe";

/// Metadata-provided installer size is untrusted. Keep a conservative hard cap
/// so a direct/mutated invoke cannot turn this command into an unbounded writer.
pub const MAX_INSTALLER_BYTES: u64 = 512 * 1024 * 1024;

/// 同時実行の排他フラグ。
static DOWNLOADING: AtomicBool = AtomicBool::new(false);
/// キャンセル要求フラグ（`cancel_update_download` が立て、DL 処理の各チェックポイントが
/// 見る — 受信ループの各チャンク・send()/受信の Err 到達時・rename/UAC 起動前の再判定）。
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

/// `DOWNLOADING` を必ず戻すガード（download.rs の DownloadGuard と同型）。early return /
/// `?` / panic のいずれでも解除する（さもないと一度失敗すると以後ずっと締め出される）。
struct UpdateGuard;
impl Drop for UpdateGuard {
    fn drop(&mut self) {
        DOWNLOADING.store(false, Ordering::SeqCst);
    }
}

// ============================================================================
// Shared release core
// ============================================================================
// Version parsing, comparison, release filtering, asset URL validation, and
// SHA256SUMS parsing are shared with the automatic checker. The manual flow
// below intentionally owns only sums-body retrieval, download, staging, hash,
// and UAC launch.
pub use nospacekey_update::release::{
    api_releases_url, compare_versions, format_version, is_valid_sha256_hex, parse_sha256sums,
    parse_version, select_latest_release, validate_installer_asset_url, validate_release_assets,
    GithubReleaseJson, InstallerAsset,
};

#[cfg(test)]
use nospacekey_update::release::{
    is_official_release_asset_url, pick_installer_asset, GithubAssetJson, Version,
};

/// 特定タグのリリースページ URL（手動確認の情報リンク用）。
fn release_tag_url(repo: &str, tag: &str) -> String {
    let base = repo
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_end_matches('/');
    format!("{base}/releases/tag/{tag}")
}

/// SHA256 hex の大小無視比較（download.rs の sha256_hex_matches と同判定）。
pub fn sha256_hex_matches(actual: &str, expected: &str) -> bool {
    actual.eq_ignore_ascii_case(expected)
}

/// DL 済みインストーラの整合性検証の純判定（fail-closed）。
/// 期待ハッシュ欠落も不一致も同じく Err — 検証なしでの実行を許さない。
/// 破棄すべき一時ファイルの掃除は呼び出し側の責務（純関数は IO しない）。
fn verify_installer_hash(actual: &str, expected: Option<&str>) -> Result<(), String> {
    let expected = validate_expected_sha256(expected)?;
    if !sha256_hex_matches(actual, expected) {
        return Err(format!(
            "整合性チェックに失敗しました（ダウンロードが破損した可能性があります）。\n期待 {expected}\n実際 {actual}"
        ));
    }
    Ok(())
}

/// IPC 境界と最終照合の双方で使う期待 SHA-256 の fail-closed 検証。
fn validate_expected_sha256(expected: Option<&str>) -> Result<&str, String> {
    let Some(expected) = expected else {
        return Err("整合性検証用のハッシュが無いためインストールを実行しません。".to_string());
    };
    if !is_valid_sha256_hex(expected) {
        return Err(
            "整合性検証用のハッシュ形式が不正なためインストールを実行しません。".to_string(),
        );
    }
    Ok(expected)
}

/// 進捗率 0..=100（download.rs と同一セマンティクス）。total 不明・0 は None。
pub fn progress_percent(received: u64, total: Option<u64>) -> Option<u8> {
    match total {
        Some(t) if t > 0 => Some(((received.min(t) * 100) / t) as u8),
        _ => None,
    }
}

// ============================================================================
// tauri コマンド
// ============================================================================

/// UI へ返す確認結果。`#[serde(tag = "kind")]` で JS は `status.kind` で分岐する。
#[derive(serde::Serialize)]
#[serde(tag = "kind")]
pub enum UpdateStatus {
    UpToDate {
        current: String,
    },
    Available {
        current: String,
        latest: String,
        installer_url: String,
        installer_name: String,
        installer_size: u64,
        /// SHA256SUMS.txt から導出した期待ハッシュ。導出不能なリリースは
        /// check_for_update が Err を返すため、ここへ載るのは常に実値（fail-closed）。
        expected_sha256: String,
        /// リリースノート本文（markdown・UI は textContent 表示かリンクで安全扱い）。
        notes: String,
        /// 当該リリースの GitHub ページ。
        notes_url: String,
    },
}

/// 進捗イベントのペイロード（download.rs の Progress と同形）。
#[derive(Clone, serde::Serialize)]
struct Progress {
    received: u64,
    total: Option<u64>,
    percent: Option<u8>,
}

/// HTTP クライアント組み立て（download.rs と同設定: native-tls/schannel・UA・timeout）。
fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("nospacekey-config/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP クライアントの初期化に失敗: {e}"))
}

/// Read a manual release response through the same bounded stream discipline
/// as the background checker. Content-Length is only an early rejection; the
/// stream is still capped when the header is absent or dishonest.
async fn bounded_response_bytes(response: reqwest::Response) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;

    if response
        .content_length()
        .is_some_and(|length| length > nospacekey_update::MAX_RESPONSE_BYTES as u64)
    {
        return Err("リリース応答が大きすぎます。".into());
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(item) = stream.next().await {
        let chunk =
            item.map_err(|error| format!("リリース情報の読み取りに失敗しました: {error}"))?;
        if body.len().saturating_add(chunk.len()) > nospacekey_update::MAX_RESPONSE_BYTES {
            return Err("リリース応答が大きすぎます。".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Validate untrusted installer metadata before any staging or network work.
fn validate_installer_size(installer_size: u64) -> Result<(), String> {
    if installer_size == 0 {
        return Err("インストーラのサイズ情報が不正です。".into());
    }
    if installer_size > MAX_INSTALLER_BYTES {
        return Err("インストーラのサイズが上限を超えています。".into());
    }
    Ok(())
}

fn validate_content_length(expected: u64, content_length: Option<u64>) -> Result<(), String> {
    if let Some(actual) = content_length {
        if actual > MAX_INSTALLER_BYTES {
            return Err("インストーラの応答サイズが上限を超えています。".into());
        }
        if actual != expected {
            return Err("インストーラの応答サイズが metadata と一致しません。".into());
        }
    }
    Ok(())
}

fn next_received_size(received: u64, chunk_len: usize, expected: u64) -> Result<u64, String> {
    let chunk_len = u64::try_from(chunk_len)
        .map_err(|_| "インストーラの受信サイズを計算できません。".to_string())?;
    let next = received
        .checked_add(chunk_len)
        .ok_or_else(|| "インストーラの受信サイズが上限を超えています。".to_string())?;
    if next > MAX_INSTALLER_BYTES || next > expected {
        return Err("インストーラの受信サイズが metadata または上限を超えています。".into());
    }
    Ok(next)
}

/// リリース一覧を問い合わせてバージョン比較結果を返す。
/// `include_beta`=true なら pre-release(beta) も候補に入れる。
#[tauri::command]
pub async fn check_for_update(include_beta: bool) -> Result<UpdateStatus, String> {
    let repo = env!("CARGO_PKG_REPOSITORY");
    let current_str = env!("CARGO_PKG_VERSION").to_string();
    let api = api_releases_url(repo).ok_or_else(|| "repository URL が不正です".to_string())?;
    let client = http_client()?;

    let resp = client
        .get(&api)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| {
            format!("最新バージョンの確認に失敗しました（ネットワークを確認してください）: {e}")
        })?;
    if !resp.status().is_success() {
        // 403 = レート制限の可能性。404 = リリース未公開。UI は Err を「確認できませんでした」へ。
        return Err(format!(
            "最新バージョンの確認に失敗しました（HTTP {}）。",
            resp.status()
        ));
    }
    let bytes = bounded_response_bytes(resp).await?;
    let items: Vec<GithubReleaseJson> = serde_json::from_slice(&bytes)
        .map_err(|e| format!("リリース情報の解析に失敗しました: {e}"))?;

    // Candidate selection and release metadata validation are shared with the
    // background checker.  The manual flow owns only the sums body fetch,
    // installer download, hash verification, staging, and UAC launch.
    let (latest, idx) = select_latest_release(&items, include_beta)
        .ok_or_else(|| "公開されているリリースが見つかりませんでした".to_string())?;
    let rel = &items[idx];
    let cur = parse_version(&current_str)
        .ok_or_else(|| format!("現在バージョンの解析に失敗しました: {current_str}"))?;

    // 新版本が *厳密に* 大きいときだけ案内する（同値・降格は最新扱い）。
    if compare_versions(&latest, &cur) != std::cmp::Ordering::Greater {
        return Ok(UpdateStatus::UpToDate {
            current: current_str,
        });
    }

    let latest_str = format_version(&latest);
    let (installer, sums) = validate_release_assets(rel, &latest)
        .ok_or_else(|| "最新版のインストーラが見つかりませんでした".to_string())?;
    validate_installer_size(installer.size)?;

    // 期待ハッシュは必須（fail-closed）: SHA256SUMS.txt の添付漏れ・取得失敗・対象行なしを
    // 検証スキップで通すと検証なしの昇格実行につながるため、更新の案内自体をしない。
    let expected = fetch_expected_sha256(&client, &sums, &installer.name)
        .await
        .ok_or_else(|| {
            "整合性検証用のハッシュを取得できませんでした（SHA256SUMS.txt）".to_string()
        })?;

    Ok(UpdateStatus::Available {
        current: current_str,
        latest: latest_str,
        installer_url: installer.url,
        installer_name: installer.name,
        installer_size: installer.size,
        expected_sha256: expected,
        notes: rel.body.clone(),
        notes_url: release_tag_url(repo, rel.tag_name.trim()),
    })
}

/// SHA256SUMS.txt アセットを取得し対象ファイルのハッシュを引く。
/// ファイルが無い・取得失敗・対象行無しのいずれも None — 呼び出し側は None を
/// 失敗へ畳める（fail-closed: 検証なしでの実行を許さない）。
async fn fetch_expected_sha256(
    client: &reqwest::Client,
    sums: &InstallerAsset,
    installer_name: &str,
) -> Option<String> {
    let resp = client.get(&sums.url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let text = String::from_utf8(bounded_response_bytes(resp).await.ok()?).ok()?;
    parse_sha256sums(&text, installer_name)
}

/// 進行中アップデートのキャンセル要求（DL 処理の各チェックポイント — 次チャンク、
/// read_timeout 発の send()/受信 Err、受信ループ後・rename 前の再判定、境界再検証後〜
/// UAC 起動前の再判定 — で気づいて中断・掃除する）。
#[tauri::command]
pub fn cancel_update_download() {
    CANCEL_REQUESTED.store(true, Ordering::SeqCst);
}

/// インストーラをダウンロード → SHA256 検証 → 昇格起動。進捗は PROGRESS_EVENT で通知。
/// 成功したら（インストーラが config を taskkill するので）呼び出し側(JS)でウィンドウを閉じる。
#[tauri::command]
pub async fn download_and_install_update(
    app: tauri::AppHandle,
    installer_url: String,
    expected_sha256: Option<String>,
    installer_size: u64,
) -> Result<(), String> {
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    use std::io::Write;

    // Direct Tauri invokes are not limited to the normal update dialog.  Bind
    // the URL to a newer canonical Nospacekey installer before taking any
    // staging or network side effect.
    let current = parse_version(env!("CARGO_PKG_VERSION"))
        .ok_or_else(|| "現在バージョンの解析に失敗しました。".to_string())?;
    validate_installer_asset_url(&installer_url, &current)?;

    // Tauri コマンドは UI 以外からも直接 invoke できる。欠落・短縮・非 hex は、排他取得、
    // staging 作成、HTTP 通信より前に拒否して不要な帯域・ディスク消費を起こさない。
    let expected_sha256 = validate_expected_sha256(expected_sha256.as_deref())?.to_owned();
    validate_installer_size(installer_size)?;

    // 排他: 既に走っていれば弾く。ガードで DOWNLOADING を必ず戻す。
    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        return Err("既にアップデート処理中です。".into());
    }
    let _guard = UpdateGuard;
    CANCEL_REQUESTED.store(false, Ordering::SeqCst);

    // staging: 試行ごとに OS 乱数で一意な専用ディレクトリ。従来の固定パス
    // （%TEMP% 直下の本名/.part）は同一ユーザーの別プロセスが事前に .part やその
    // symlink を置いて待ち構えられ — pre-open writer・reparse 差替えの起点に
    // になるため廃止した。以後の全エラー/キャンセル経路の掃除は TempDir の RAII が
    // ディレクトリごと担う（成功経路だけ keep() で残留に切り替える）。
    let staging = create_staging_dir()?;
    // 完成前のファイルを本名で観測させない（中断された半端ファイルを実行させない）。
    let part = staging.path().join(format!("{INSTALLER_FILENAME}.part"));
    let dest = staging.path().join(INSTALLER_FILENAME);

    let client = http_client()?;
    let resp = match client.get(&installer_url).send().await {
        Ok(c) => c,
        Err(e) => {
            // 巡6(同型拡張): ヘッダ待ちのストールも read_timeout の Err として現れる —
            // 受信ループ(巡5 M-2)と同じく、キャンセルが立っているならキャンセル扱いにする。
            // この時点では .part 未作成なので掃除は不要。
            if CANCEL_REQUESTED.load(Ordering::SeqCst) {
                return Err("キャンセルしました。".into());
            }
            return Err(format!(
                "ダウンロードに失敗しました（ネットワークを確認してください）: {e}"
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
    validate_content_length(installer_size, total)?;

    // .part は CREATE_NEW + share_mode=0（占有）で作る — 事前置きエントリ（正規ファイル・
    // symlink 等）は拒否し、DL 中は他プロセスの write/delete/rename（= reparse 差替え）を
    // 共有違反で全て拒否する。
    let mut file = create_part_exclusive(&part)?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    // 進捗イベントの間引き: download.rs と同一（整数% 変化時か total 不明なら 1MB 毎）。
    let mut last_emit_pct: Option<u8> = None;
    let mut last_emit_bytes: u64 = 0;
    let mut stream = resp.bytes_stream();

    // 中断経路の scrub は不要 — staging の RAII が .part ごとディレクトリを消す。
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
        let next_received = next_received_size(received, chunk.len(), installer_size)?;
        if let Err(e) = file.write_all(&chunk) {
            return Err(format!(
                "書き込みに失敗しました（ディスクの空き容量を確認してください）: {e}"
            ));
        }
        hasher.update(&chunk);
        received = next_received;
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

    if received != installer_size {
        return Err("インストーラの受信サイズが metadata と一致しません。".into());
    }

    // 巡3 Q9: 受信ループ後（ハッシュ検証・rename・起動の前）にキャンセルを再判定する —
    // この窓でキャンセルが無視されると、キャンセルしたはずなのに UAC プロンプトが出る。
    if CANCEL_REQUESTED.load(Ordering::SeqCst) {
        return Err("キャンセルしました。".into());
    }

    // 整合性検証は fail-closed: 期待ハッシュ欠落・不一致のいずれも破棄して失敗。
    // 確認側で None は弾いてあるが、フロント引数経由で None が流れる経路（旧 UI・改変）に
    // おける検証スキップ実行をここでも許さない。
    let actual = hex::encode(hasher.finalize());
    verify_installer_hash(&actual, Some(&expected_sha256))?;

    // 本名へ原子的に置き換え（同一 staging ディレクトリ内 rename）。
    if let Err(e) = std::fs::rename(&part, &dest) {
        return Err(format!("インストーラの配置に失敗しました: {e}"));
    }

    // コマンド実行境界での再検証: 上の検証は受信ストリームのハッシュであり、実行されるのは
    // rename 後のディスク上ファイル。検証後に置換・破損が起きていてもストリーム照合では
    // 捕捉できないため、昇格実行に持ち込む前に実ファイルを再ハッシュして突き合わせる。
    // 再検証〜起動成立までは InstallerGuard（reparse 拒否 + 全範囲排他 LockFileEx +
    // share_mode=FILE_SHARE_READ）を保持する — 開いて閉じた後にパスで起動すると、閉じた
    // 後の差し替えが TOCTOU として昇格実行に通ってしまう（hash-to-elevated-exec の隙間）。
    // エラー/キャンセル経路は宣言順により guard drop → staging drop が掃除する。
    let mut guard = match InstallerGuard::open_in_staging(&dest, &staging) {
        Ok(g) => g,
        Err(e) => return Err(e),
    };
    let on_disk = match guard.sha256() {
        Ok(h) => h,
        Err(e) => return Err(e),
    };
    verify_installer_hash(&on_disk, Some(&expected_sha256))?;

    // 巡4 B2: 昇格起動の直前にもう一度キャンセルを再判定 — この窓でキャンセルが
    // 無視されると「キャンセルしたのに UAC プロンプトが出る」。
    // 判定は runas に一番近い位置に置く — 直前の境界再ハッシュ中に立ったキャンセルも
    // ここで拾わないと、再検証の導入が逆にキャンセル不能の窓を開くことになる。
    // ガードは判定中も閉じない — 閉じた瞬間に置換が通り、掃除が別プロセスのファイルを
    // 消すだけに変わる窓が開く。
    if CANCEL_REQUESTED.load(Ordering::SeqCst) {
        return Err("キャンセルしました。".into());
    }

    // lpFile は guard ハンドル由来の最終正規パス（GetFinalPathNameByHandleW）— dest を
    // パス指定で再解決させると symlink/junction 経由の別実体を指させる余地が残る。
    let exec_path = match guard.execute_path() {
        Ok(p) => p,
        Err(e) => return Err(e),
    };

    // 昇格起動（runas → UAC）。インストーラが config/engine を taskkill し使用中 DLL を置換する。
    run_installer_elevated(&exec_path)?;

    // 起動成立（有効 hProcess 返却済み）— ここで初めてガードを解放（unlock → close）する。
    // イメージはロード済みで以後の実行中置換は OS が拒否するため閉じてよい。staging は
    // 削除しない: Inno Setup は自己 EXE 末尾の overlay を起動後に読むことがあるため、
    // 成功時は実ファイルを残す（%TEMP% 内なので OS・手動の一時掃除対象になるのみ）。
    drop(guard);
    staging.keep();
    Ok(())
}

/// 試行ごとに OS 乱数で一意になる staging ディレクトリを %TEMP% 直下に作り、直後に
/// directory handle を削除共有なしで開く。`.part` を閉じた後もこの handle を
/// ShellExecuteExW のプロセス作成成立まで保持するため、親ディレクトリの rename/delete と
/// 元パスへの junction 差替えは共有違反になる。TempDir 単体は path しか保持しないため、
/// この境界を守れない。
fn create_staging_dir() -> Result<StagingDir, String> {
    let temp = tempfile::Builder::new()
        .prefix(STAGING_DIR_PREFIX)
        .tempdir()
        .map_err(|e| format!("一時ディレクトリを作成できません: {e}"))?;
    StagingDir::open(temp)
}

/// TempDir の path に加え、同じ directory object を固定する handle とその最終パスを保持する。
/// Drop は必ず directory handle を先に閉じ、その後 TempDir の再帰掃除を走らせる。
#[derive(Debug)]
struct StagingDir {
    directory: Option<std::fs::File>,
    temp: Option<tempfile::TempDir>,
    final_path: std::path::PathBuf,
}

impl StagingDir {
    fn open(temp: tempfile::TempDir) -> Result<Self, String> {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use windows::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        // FILE_SHARE_DELETE を意図的に含めない。READ|WRITE は同じ directory 内での正当な
        // child 作成/rename を妨げず、directory object 自体の delete/rename だけを止める。
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(temp.path())
            .map_err(|e| format!("一時ディレクトリを固定できません: {e}"))?;
        let attrs = directory
            .metadata()
            .map_err(|e| format!("一時ディレクトリの属性を取得できません: {e}"))?
            .file_attributes();
        if attrs & FILE_ATTRIBUTE_DIRECTORY.0 == 0 || attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err("一時ディレクトリが通常のディレクトリではありません。".into());
        }
        let final_path = final_path_from_handle(&directory, "一時ディレクトリ")?;
        Ok(Self {
            directory: Some(directory),
            temp: Some(temp),
            final_path,
        })
    }

    fn path(&self) -> &Path {
        self.temp.as_ref().expect("staging は keep 前").path()
    }

    fn final_path(&self) -> &Path {
        &self.final_path
    }

    fn keep(mut self) {
        // 成功後は Inno Setup の overlay 読み取り用に実体を残す。directory handle を先に
        // 閉じてから TempDir を keep へ移す。
        drop(self.directory.take());
        if let Some(temp) = self.temp.take() {
            let _ = temp.keep();
        }
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        // 削除共有なしの directory handle が残ったままでは TempDir cleanup が失敗する。
        drop(self.directory.take());
        drop(self.temp.take());
    }
}

/// .part を CREATE_NEW + share_mode=0（占有）で作る。
/// - create_new: 既存エントリ（正規ファイル・symlink・ジャンクション等）があれば失敗 —
///   事前に置かれた .part を検知して拒否する。truncate を伴わないため、既存 link の
///   リンク先を破壊しない（CREATE_ALWAYS+truncate は symlink を解決して先を空にする）。
/// - share_mode=0: DL 中、他プロセスの読み書き・削除・リネーム（= reparse 差替えの
///   delete→create）を共有違反で全て拒否する。
fn create_part_exclusive(path: &Path) -> Result<std::fs::File, String> {
    use std::os::windows::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(0)
        .open(path)
        .map_err(|e| format!("一時ファイルを作成できません: {e}"))
}

/// rename 後の実行ファイルを境界再検証〜昇格プロセス作成の成立まで保持するガード。
///
/// 保持中の保護（open → 検証 → 起動の全期間）:
/// - 開き方: GENERIC_READ + share_mode=FILE_SHARE_READ + FILE_FLAG_OPEN_REPARSE_POINT。
///   読み手（ShellExecuteExW 側のローダー等）は共有し、書き込み・削除目的の同時オープン
///   は共有違反で拒否。reparse point（symlink/junction）そのものを開いて属性を見て
///   即拒否する — 検証対象と実行対象がリンク解決後の別実体にされる差替えを排除。
///   share_mode=0 は正当な読み手まで締め出すため使わない。
/// - LockFileEx exclusive + LOCKFILE_FAIL_IMMEDIATELY でバイト範囲 0..u64::MAX を取得。
///   縦深防御の第二層: 先取り writer（guard より前に開き切った書き手）はこの層に届く前に
///   open が共有違反で落ちる — guard の share_mode=FILE_SHARE_READ は既存 handle の要求
///   WRITE/DELETE アクセスとも共有違反になるため。範囲ロックは、既に範囲を押さえている
///   者がいれば即時失敗させ、保持中の他 handle からの範囲内読み書きを拒む。ロック取得に
///   失敗（他プロセスが範囲を押さえている等）なら fail-closed。SHA256 はロック取得後・
///   同一ハンドルから計算するため、検証・実行対象は常にロックで押さえた実体のまま。
/// - Drop で UnlockFileEx → CloseHandle（file の drop）。明示 unlock を先に行うのは
///   「Unlock/CloseHandle 漏れなし」をコード上明示するため（CloseHandle だけでも
///   OS はロックを解放する）。
#[derive(Debug)]
struct InstallerGuard {
    file: Option<std::fs::File>,
    execute_path: std::path::PathBuf,
}

impl InstallerGuard {
    /// production の guard を開く: 最終要素の reparse 拒否 → staging directory handle
    /// 由来の最終親パス照合 → 全範囲排他ロックの順。どれかに失敗しても handle は Drop が
    /// 閉じる（fail-closed）。
    fn open_in_staging(path: &Path, staging: &StagingDir) -> Result<Self, String> {
        Self::open_checked(path, Some(staging.final_path()))
    }

    /// 単独ファイル用のテスト helper。production は必ず open_in_staging を通す。
    #[cfg(test)]
    fn open(path: &Path) -> Result<Self, String> {
        Self::open_checked(path, None)
    }

    fn open_checked(path: &Path, expected_parent: Option<&Path>) -> Result<Self, String> {
        use std::os::windows::fs::MetadataExt;
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        };

        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
            .map_err(|e| format!("インストーラを開けませんでした: {e}"))?;
        // FILE_FLAG_OPEN_REPARSE_POINT で開いた handle の属性は reparse エントリ自身の
        // もの — 通常ファイルなら立たない。symlink/junction 等が立っていたら、リンク先
        // の別実体を検証・実行させない（fail-closed）。
        let attrs = file
            .metadata()
            .map_err(|e| format!("インストーラの属性を取得できませんでした: {e}"))?
            .file_attributes();
        if attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(
                "インストーラのパスがシンボリックリンク等の再解析ポイントを指しています。".into(),
            );
        }
        let execute_path = final_path_from_handle(&file, "インストーラ")?;
        if let Some(expected_parent) = expected_parent {
            if execute_path.parent() != Some(expected_parent) {
                return Err(
                    "インストーラの親ディレクトリが固定した一時ディレクトリと一致しません。".into(),
                );
            }
        }
        let guard = Self {
            file: Some(file),
            execute_path,
        };
        guard.lock_all_ranges()?;
        Ok(guard)
    }

    /// バイト範囲 0..u64::MAX を排他・即時失敗でロック。開始 Offset/OffsetHigh=0、
    /// 長さ low/high=MAX — ファイル全域に加え将来の追記領域も含めて押さえる。
    fn lock_all_ranges(&self) -> Result<(), String> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{
            LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
        };
        use windows::Win32::System::IO::OVERLAPPED;

        let Some(file) = self.file.as_ref() else {
            return Ok(());
        };
        let mut overlapped = OVERLAPPED::default();
        unsafe {
            LockFileEx(
                HANDLE(file.as_raw_handle() as _),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                None,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        }
        .map_err(|e| {
            format!("インストーラをロックできませんでした（他プロセスが使用中の可能性）: {e}")
        })
    }

    /// lock_all_ranges と同一 handle・同一範囲のアンロック（Drop から呼ぶ）。
    fn unlock_all_ranges(&self) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::UnlockFileEx;
        use windows::Win32::System::IO::OVERLAPPED;

        let Some(file) = self.file.as_ref() else {
            return;
        };
        let mut overlapped = OVERLAPPED::default();
        unsafe {
            let _ = UnlockFileEx(
                HANDLE(file.as_raw_handle() as _),
                None,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            );
        }
    }

    /// guard の同一ハンドルの全文を SHA256 hex 化（コマンド実行境界の再検証用）。
    /// パスで再 open しない — 検証したハンドルが指す実体と昇格実行される実体の同一性が
    /// この経路の前提。読み取り失敗は Err — 検証できないファイルは実行しない（fail-closed）。
    fn sha256(&mut self) -> Result<String, String> {
        match self.file.as_mut() {
            Some(file) => sha256_handle(file),
            None => Err("インストーラのガードが既に閉じています。".into()),
        }
    }

    /// guard ハンドルから GetFinalPathNameByHandleW で最終正規パス（symlink/junction
    /// 解決済み・親ディレクトリ正規化済み）を導き、ShellExecuteExW の lpFile に渡せる
    /// Win32 パス形式へ整形する。dest を生のパスで渡すと lpFile 解釈時の再解決で別実体
    /// を指させる余地が残るため、検証したハンドル由来のパスを使う。%TEMP% 自体が
    /// junction（ボリューム間リンク）でも、解決済み実パスが渡る。ガードの share_mode
    /// と範囲ロックが lpFile 再オープンから実行成立までの差し替えを拒否するため、
    /// このパス渡しはハンドル同一性の最後の詰め（file ID 相当の保護は handle 保持が担う）。
    fn execute_path(&self) -> Result<std::path::PathBuf, String> {
        if self.file.is_none() {
            return Err("インストーラのガードが既に閉じています。".into());
        }
        Ok(self.execute_path.clone())
    }
}

impl Drop for InstallerGuard {
    fn drop(&mut self) {
        self.unlock_all_ranges();
        self.file.take(); // CloseHandle（std::fs::File の drop）
    }
}

/// File/directory handle から junction/symlink 解決済みの最終 DOS path を得る。
/// staging root と installer の双方が同じ helper を通るため、表記揺れで親照合が外れない。
fn final_path_from_handle(
    file: &std::fs::File,
    subject: &str,
) -> Result<std::path::PathBuf, String> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, GETFINALPATHNAMEBYHANDLE_FLAGS,
        VOLUME_NAME_DOS,
    };

    // windows 0.62 の flags 型は BitOr を実装しないため、生値の OR で明示構築する。
    let flags = GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0);
    let handle = HANDLE(file.as_raw_handle() as _);
    let mut buf = vec![0u16; 1024];
    let mut len = unsafe { GetFinalPathNameByHandleW(handle, &mut buf, flags) };
    if len == 0 {
        return Err(format!("{subject}の最終パス解決に失敗しました。"));
    }
    if len as usize >= buf.len() {
        // buffer 不足: 戻り値は必要サイズ。その幅で再確保して一度だけ再試行する。
        buf = vec![0u16; len as usize + 1];
        len = unsafe { GetFinalPathNameByHandleW(handle, &mut buf, flags) };
        if len == 0 || len as usize >= buf.len() {
            return Err(format!("{subject}の最終パスが長すぎます。"));
        }
    }
    // 切り出しは fail-closed: 戻り値に終端 NUL が数えられていたら先頭 NUL で打ち切り、
    // サロゲート断片等の不正 UTF-16 は lossy 化せず Err にする。
    let end = buf[..len as usize]
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(len as usize);
    let final_path = String::from_utf16(&buf[..end])
        .map_err(|_| format!("{subject}の最終パスが不正な UTF-16 です。"))?;
    Ok(std::path::PathBuf::from(shell_compatible_path(&final_path)))
}

/// GetFinalPathNameByHandleW(VOLUME_NAME_DOS) の戻り `\\?\C:\...` / `\\?\UNC\...` を
/// ShellExecuteExW が解釈する Win32 パス（`C:\...` / `\\server\...`）へ戻す。
/// `\\?\` 前置のまま渡すと ShellExecuteExW はパスを解釈できないことがある。
fn shell_compatible_path(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.to_string()
    }
}

/// 開いたガードハンドルの全文を SHA256 hex 化（InstallerGuard::sha256 の本体）。
fn sha256_handle(file: &mut std::fs::File) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("インストーラの読み取りに失敗しました: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// ガードを開いて SHA256 hex 化する合成（既存単体テスト経路の互換維持用）。
#[cfg(test)]
fn sha256_file(path: &Path) -> Result<String, String> {
    let mut guard = InstallerGuard::open(path)?;
    guard.sha256()
}

/// インストーラを UAC 昇格付きで起動する。lpFile は呼び出し側が guard ハンドル由来の
/// 最終正規パス（InstallerGuard::execute_path — GetFinalPathNameByHandleW）を渡す。
/// SEE_MASK_NOCLOSEPROCESS で呼び、TRUE かつ有効な hProcess が返った時点でのみ成功とする
/// （旧 ShellExecuteW の >32 疑似ハンドル判定は「起動に失敗しても成功と区別できない」ため
/// 廃止）。hProcess は実行完了を待たず CloseHandle する — ハンドルが返る時点でイメージは
/// ロード済みであり、以後の実行中イメージ置換は OS が拒否するため、呼び出し側の読み取り
/// ガードは不要になる。呼び出し側はプロセス作成の成立（この関数の Ok）までガードを保持
/// すること。
fn run_installer_elevated(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let file: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let verb: Vec<u16> = "runas".encode_utf16().chain([0]).collect();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        // 失敗は Err（GetLastError 由来の HRESULT 化。UAC 拒否は ERROR_CANCELLED(1223)
        // の 0x8007_04b3、起動不能は FILE_NOT_FOUND/ACCESS_DENIED 等の HRESULT 化）。
        if let Err(e) = ShellExecuteExW(&mut info) {
            return Err(format!(
                "インストーラの起動に失敗しました（code {:#010x}）。",
                e.code().0 as u32
            ));
        }
        // TRUE でも hProcess が無効ならプロセス作成の成立を確認できない — fail-closed。
        if info.hProcess.is_invalid() {
            return Err(
                "インストーラの起動に失敗しました（プロセスハンドルを取得できません）。"
                    .to_string(),
            );
        }
        let _ = CloseHandle(info.hProcess);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(maj: u64, min: u64, pat: u64) -> Version {
        Version {
            major: maj,
            minor: min,
            patch: pat,
            pre: None,
        }
    }
    fn vp(maj: u64, min: u64, pat: u64, pre: &str) -> Version {
        Version {
            major: maj,
            minor: min,
            patch: pat,
            pre: Some(pre.into()),
        }
    }

    #[test]
    fn parse_version_accepts_plain_and_v_prefix() {
        assert_eq!(parse_version("1.2.1"), Some(v(1, 2, 1)));
        assert_eq!(parse_version("v1.2.1"), Some(v(1, 2, 1)));
        assert_eq!(parse_version("V1.2.1"), Some(v(1, 2, 1)));
        assert_eq!(parse_version("  1.2.1  "), Some(v(1, 2, 1)));
        assert_eq!(parse_version("10.20.30"), Some(v(10, 20, 30)));
    }

    #[test]
    fn parse_version_accepts_prerelease_and_strips_build_metadata() {
        assert_eq!(parse_version("1.2.2-beta.1"), Some(vp(1, 2, 2, "beta.1")));
        assert_eq!(parse_version("v1.2.2-beta.1"), Some(vp(1, 2, 2, "beta.1")));
        // build metadata は semver で比較無視 → 保持しない。
        assert_eq!(
            parse_version("1.2.2-beta.1+sha"),
            Some(vp(1, 2, 2, "beta.1"))
        );
        assert_eq!(parse_version("1.2.3+build"), Some(v(1, 2, 3)));
    }

    #[test]
    fn parse_version_rejects_malformed() {
        assert_eq!(parse_version("1.2"), None); // セグメント不足
        assert_eq!(parse_version("1.2.1.0"), None); // セグメント過多
        assert_eq!(parse_version("x.y.z"), None); // 非数値
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("1.2.x"), None);
        assert_eq!(parse_version("1.2.3-"), None); // 空 pre
    }

    #[test]
    fn compare_versions_release_ordering() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions(&v(1, 2, 1), &v(1, 3, 0)), Ordering::Less);
        assert_eq!(
            compare_versions(&v(1, 3, 0), &v(1, 2, 1)),
            Ordering::Greater
        );
        assert_eq!(compare_versions(&v(1, 2, 1), &v(1, 2, 1)), Ordering::Equal);
        assert_eq!(
            compare_versions(&v(2, 0, 0), &v(1, 9, 9)),
            Ordering::Greater
        );
        assert_eq!(compare_versions(&v(1, 9, 9), &v(2, 0, 0)), Ordering::Less);
        assert_eq!(
            compare_versions(&v(1, 2, 10), &v(1, 2, 9)),
            Ordering::Greater
        );
    }

    #[test]
    fn compare_versions_release_beats_prerelease() {
        use std::cmp::Ordering;
        // 同 (maj,min,patch) なら release > pre-release（semver 規則）。
        assert_eq!(
            compare_versions(&v(1, 2, 2), &vp(1, 2, 2, "beta.1")),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions(&vp(1, 2, 2, "beta.1"), &v(1, 2, 2)),
            Ordering::Less
        );
    }

    #[test]
    fn compare_versions_higher_patch_prerelease_beats_lower_release() {
        use std::cmp::Ordering;
        // 1.2.2-beta.1 は 1.2.1(安定版) より新しい（patch が上位）。= beta 通知の根拠。
        assert_eq!(
            compare_versions(&vp(1, 2, 2, "beta.1"), &v(1, 2, 1)),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions(&v(1, 2, 1), &vp(1, 2, 2, "beta.1")),
            Ordering::Less
        );
    }

    #[test]
    fn compare_versions_prerelease_identifiers() {
        use std::cmp::Ordering;
        // beta.1 < beta.2（数値識別子比較）。
        assert_eq!(
            compare_versions(&vp(1, 2, 2, "beta.1"), &vp(1, 2, 2, "beta.2")),
            Ordering::Less
        );
        // beta < rc（非数値識別子の辞書式）。
        assert_eq!(
            compare_versions(&vp(1, 2, 2, "beta.1"), &vp(1, 2, 2, "rc.1")),
            Ordering::Less
        );
        // 数値識別子 < 非数値識別子。
        assert_eq!(
            compare_versions(&vp(1, 2, 2, "1"), &vp(1, 2, 2, "alpha")),
            Ordering::Less
        );
        // 同値。
        assert_eq!(
            compare_versions(&vp(1, 2, 2, "beta.1"), &vp(1, 2, 2, "beta.1")),
            Ordering::Equal
        );
    }

    #[test]
    fn format_version_roundtrips_release_and_prerelease() {
        assert_eq!(format_version(&v(1, 2, 1)), "1.2.1");
        assert_eq!(format_version(&vp(1, 2, 2, "beta.1")), "1.2.2-beta.1");
    }

    #[test]
    fn api_releases_url_converts_github_html_to_list_api() {
        assert_eq!(
            api_releases_url("https://github.com/yachtida/nospacekey").unwrap(),
            "https://api.github.com/repos/yachtida/nospacekey/releases?per_page=30"
        );
        assert_eq!(
            api_releases_url("https://github.com/yachtida/nospacekey.git").unwrap(),
            "https://api.github.com/repos/yachtida/nospacekey/releases?per_page=30"
        );
        assert_eq!(
            api_releases_url("https://github.com/yachtida/nospacekey/").unwrap(),
            "https://api.github.com/repos/yachtida/nospacekey/releases?per_page=30"
        );
        assert!(api_releases_url("https://gitlab.com/o/r").is_none());
    }

    fn rel(tag: &str, prerelease: bool, draft: bool) -> GithubReleaseJson {
        GithubReleaseJson {
            tag_name: tag.to_string(),
            prerelease,
            draft,
            body: String::new(),
            assets: vec![],
        }
    }
    fn asset(name: &str, size: u64) -> GithubAssetJson {
        GithubAssetJson {
            name: name.to_string(),
            size,
            browser_download_url: format!(
                "https://github.com/yachtida/nospacekey/releases/download/v1.2.2/{name}"
            ),
            state: Some("uploaded".to_string()),
        }
    }

    #[test]
    fn select_latest_skips_drafts_and_unparseable() {
        let items = vec![
            rel("v1.2.2-beta.1", true, false),
            rel("nightly", false, true),        // draft → 除外
            rel("not-a-version", false, false), // 解析失敗 → 飛ばす
        ];
        // include_beta=true なら beta が唯一の有効リリース。
        let (ver, idx) = select_latest_release(&items, true).unwrap();
        assert_eq!(ver, vp(1, 2, 2, "beta.1"));
        assert_eq!(idx, 0);
    }

    #[test]
    fn select_latest_excludes_prerelease_when_beta_off() {
        let items = vec![
            rel("v1.2.2-beta.1", true, false),
            rel("v1.2.1", false, false),
        ];
        // include_beta=false → beta は除外 → 安定版 1.2.1 が選ばれる。
        let (ver, _) = select_latest_release(&items, false).unwrap();
        assert_eq!(ver, v(1, 2, 1));
    }

    #[test]
    fn select_latest_includes_prerelease_when_beta_on() {
        let items = vec![
            rel("v1.2.2-beta.1", true, false),
            rel("v1.2.1", false, false),
        ];
        // include_beta=true → beta 1.2.2-beta.1 が 1.2.1 より新しいので選ばれる。
        let (ver, _) = select_latest_release(&items, true).unwrap();
        assert_eq!(ver, vp(1, 2, 2, "beta.1"));
    }

    #[test]
    fn select_latest_picks_highest_version_not_first() {
        // リスト順 = 作成順（最新が先とは限らない）。版本で最大を選ぶ。
        let items = vec![
            rel("v1.2.2-beta.1", true, false), // 先だが 1.2.2-beta
            rel("v1.3.0", false, false),       // 後だが 1.3.0（より大きい）
        ];
        let (ver, _) = select_latest_release(&items, true).unwrap();
        assert_eq!(ver, v(1, 3, 0));
    }

    #[test]
    fn select_latest_none_when_all_filtered() {
        let items = vec![rel("v1.2.2-beta.1", true, false)];
        assert!(select_latest_release(&items, false).is_none()); // beta OFF で候補なし
        assert!(select_latest_release(&[], true).is_none());
    }

    #[test]
    fn pick_installer_prefers_plain_over_devsigned() {
        let assets = vec![
            asset("nospacekey-setup-1.2.1-devsigned.exe", 100),
            asset("nospacekey-setup-1.2.1.exe", 200),
            asset("SHA256SUMS.txt", 103),
        ];
        let got = pick_installer_asset(&assets, "1.2.1").unwrap();
        assert_eq!(got.name, "nospacekey-setup-1.2.1.exe");
        assert_eq!(got.size, 200);
        assert!(is_official_release_asset_url(&got.url));
    }

    #[test]
    fn pick_installer_falls_back_to_devsigned() {
        let assets = vec![
            asset("nospacekey-setup-1.2.1-devsigned.exe", 33137488),
            asset("SHA256SUMS.txt", 103),
        ];
        let got = pick_installer_asset(&assets, "1.2.1").unwrap();
        assert_eq!(got.name, "nospacekey-setup-1.2.1-devsigned.exe");
    }

    #[test]
    fn pick_installer_matches_prerelease_version_string() {
        // beta 版のアセット名はバージョン全体（-beta.1 含む）を含む。
        let assets = vec![asset(
            "nospacekey-setup-1.2.2-beta.1-devsigned.exe",
            33137488,
        )];
        let got = pick_installer_asset(&assets, "1.2.2-beta.1").unwrap();
        assert_eq!(got.name, "nospacekey-setup-1.2.2-beta.1-devsigned.exe");
    }

    #[test]
    fn pick_installer_none_when_version_mismatch() {
        let assets = vec![
            asset("nospacekey-setup-1.3.0.exe", 100),
            asset("SHA256SUMS.txt", 103),
        ];
        assert!(pick_installer_asset(&assets, "1.2.1").is_none());
    }

    #[test]
    fn parse_sha256sums_extracts_target_hash_two_space() {
        let devsigned = "ab".repeat(32);
        let release = "cd".repeat(32);
        let text = format!(
            "{devsigned}  nospacekey-setup-1.2.1-devsigned.exe\n\
             {release}  nospacekey-setup-1.2.1.exe\n"
        );
        assert_eq!(
            parse_sha256sums(&text, "nospacekey-setup-1.2.1-devsigned.exe").as_deref(),
            Some(devsigned.as_str())
        );
        assert_eq!(
            parse_sha256sums(&text, "nospacekey-setup-1.2.1.exe").as_deref(),
            Some(release.as_str())
        );
    }

    #[test]
    fn parse_sha256sums_handles_text_mode_separator() {
        let hash = "AB".repeat(32);
        let text = format!("{hash} *nospacekey-setup-1.2.1.exe\n");
        assert_eq!(
            parse_sha256sums(&text, "nospacekey-setup-1.2.1.exe").as_deref(),
            Some(hash.as_str())
        );
    }

    #[test]
    fn parse_sha256sums_none_when_target_absent() {
        assert!(
            parse_sha256sums("abc123  other-file.exe\n", "nospacekey-setup-1.2.1.exe").is_none()
        );
        assert!(parse_sha256sums("", "nospacekey-setup-1.2.1.exe").is_none());
    }

    #[test]
    fn parse_sha256sums_skips_malformed_lines() {
        // トークン不足の破損行が先行しても、そこで解析全体を諦めず後続の対象行を見つける
        // （関数ごそっと None だと SHA256SUMS.txt の傷1つで更新案内自体が消える）。
        let hash = "1f".repeat(32);
        let text = format!("deadbeef\n{hash}  nospacekey-setup-1.2.1.exe\n");
        assert_eq!(
            parse_sha256sums(&text, "nospacekey-setup-1.2.1.exe").as_deref(),
            Some(hash.as_str())
        );
        // 破損行しか無ければ対象は見つからない → None（fail-closed 側へ）。
        assert!(parse_sha256sums("deadbeef\n", "nospacekey-setup-1.2.1.exe").is_none());
    }

    #[test]
    fn parse_sha256sums_skips_invalid_target_hash_and_finds_later_valid_row() {
        let valid = "ef".repeat(32);
        let text = format!(
            "{}  nospacekey-setup-1.2.1.exe\n{valid}  nospacekey-setup-1.2.1.exe\n",
            "g".repeat(64)
        );
        assert_eq!(
            parse_sha256sums(&text, "nospacekey-setup-1.2.1.exe").as_deref(),
            Some(valid.as_str())
        );
        assert!(parse_sha256sums(
            &format!("{}  nospacekey-setup-1.2.1.exe\n", "a".repeat(63)),
            "nospacekey-setup-1.2.1.exe"
        )
        .is_none());
    }

    #[test]
    fn parse_sha256sums_rejects_conflicting_valid_rows() {
        let first = "ab".repeat(32);
        let second = "cd".repeat(32);
        let conflicting =
            format!("{first}  nospacekey-setup-1.2.1.exe\n{second}  nospacekey-setup-1.2.1.exe\n");
        assert!(parse_sha256sums(&conflicting, "nospacekey-setup-1.2.1.exe").is_none());

        // 大小だけ違う同値の重複は sha256sum の比較意味論と同じく許容する。
        let duplicate = format!(
            "{first}  nospacekey-setup-1.2.1.exe\n{}  nospacekey-setup-1.2.1.exe\n",
            first.to_uppercase()
        );
        assert_eq!(
            parse_sha256sums(&duplicate, "nospacekey-setup-1.2.1.exe").as_deref(),
            Some(first.as_str())
        );
    }

    #[test]
    fn official_asset_url_accepts_canonical_release_urls() {
        // 実在する公式アセットの形状（安定版・beta・SHA256SUMS.txt）。
        assert!(is_official_release_asset_url(
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.1/nospacekey-setup-1.2.1-devsigned.exe"
        ));
        assert!(is_official_release_asset_url(
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.2-beta.1/nospacekey-setup-1.2.2-beta.1-devsigned.exe"
        ));
        assert!(is_official_release_asset_url(
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.2-beta.1/SHA256SUMS.txt"
        ));
    }

    #[test]
    fn official_asset_url_rejects_other_repo_and_lookalikes() {
        const TAIL: &str = "/releases/download/v1.2.1/nospacekey-setup-1.2.1.exe";
        // 攻撃者リポジトリ（owner 違い）。
        assert!(!is_official_release_asset_url(&format!(
            "https://github.com/mallory/nospacekey{TAIL}"
        )));
        // lookalike repo 名（セグメント完全一致なので接尾・前置は落ちる）。
        assert!(!is_official_release_asset_url(&format!(
            "https://github.com/yachtida/nospacekey-lookalike{TAIL}"
        )));
        assert!(!is_official_release_asset_url(&format!(
            "https://github.com/yachtida-evil/nospacekey{TAIL}"
        )));
    }

    #[test]
    fn official_asset_url_rejects_wrong_scheme_and_lookalike_hosts() {
        const PATH: &str =
            "yachtida/nospacekey/releases/download/v1.2.1/nospacekey-setup-1.2.1.exe";
        assert!(!is_official_release_asset_url(&format!(
            "http://github.com/{PATH}"
        ))); // https 必須
             // host は完全一致 — サブドメイン・接尾・別ホストは全て落ちる。
        assert!(!is_official_release_asset_url(&format!(
            "https://www.github.com/{PATH}"
        )));
        assert!(!is_official_release_asset_url(&format!(
            "https://github.com.evil.com/{PATH}"
        )));
        assert!(!is_official_release_asset_url(&format!(
            "https://evil-github.com/{PATH}"
        )));
        assert!(!is_official_release_asset_url(&format!(
            "https://evil.com/{PATH}"
        )));
        // 実在ホストでも API 用はダウンロード元として認めない。
        assert!(!is_official_release_asset_url(
            "https://api.github.com/repos/yachtida/nospacekey/releases/download/v1.2.1/x.exe"
        ));
    }

    #[test]
    fn official_asset_url_rejects_userinfo_port_query_fragment() {
        const PATH: &str =
            "yachtida/nospacekey/releases/download/v1.2.1/nospacekey-setup-1.2.1.exe";
        assert!(!is_official_release_asset_url(&format!(
            "https://yachtida@github.com/{PATH}"
        )));
        assert!(!is_official_release_asset_url(&format!(
            "https://user:pass@github.com/{PATH}"
        )));
        // userinfo で host 誤認を狙う形（実 host は evil.com）。
        assert!(!is_official_release_asset_url(&format!(
            "https://github.com@evil.com/{PATH}"
        )));
        // 非既定ポート（既定 443 は URL 解析時に除去され残らない）。
        assert!(!is_official_release_asset_url(&format!(
            "https://github.com:8443/{PATH}"
        )));
        assert!(!is_official_release_asset_url(&format!(
            "https://github.com/{PATH}?token=1"
        )));
        assert!(!is_official_release_asset_url(&format!(
            "https://github.com/{PATH}#frag"
        )));
    }

    #[test]
    fn official_asset_url_rejects_traversal_and_encoded_paths() {
        // 生の `..` は URL 解析で解決され releases/download の外へ出る → prefix 不一致。
        assert!(!is_official_release_asset_url(
            "https://github.com/yachtida/nospacekey/releases/download/../../evil/x.exe"
        ));
        // %2e 系も URL 仕様上ドットセグメントとして解決される（同上）。
        assert!(!is_official_release_asset_url(
            "https://github.com/yachtida/nospacekey/releases/download/%2e%2e/evil/x.exe"
        ));
        // セグメント内に残るエンコード（%2f 等）は `%` ごと拒否 — 公式 URL はプレーン ASCII。
        assert!(!is_official_release_asset_url(
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.1/x%2f..%2fx.exe"
        ));
        assert!(!is_official_release_asset_url(
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.1/%2e%2e.exe"
        ));
        // 特殊 scheme では `\` も区切り扱い — トラバーサルは同様に解決されて外れる。
        assert!(!is_official_release_asset_url(
            r"https://github.com/yachtida/nospacekey/releases/download/v1\..\..\evil\x.exe"
        ));
    }

    #[test]
    fn official_asset_url_rejects_wrong_path_shape_and_non_urls() {
        // releases/download のパス形状でないもの・セグメント過不足。
        assert!(!is_official_release_asset_url(
            "https://github.com/yachtida/nospacekey/archive/refs/tags/v1.2.1.zip"
        ));
        assert!(!is_official_release_asset_url(
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.1" // ファイル無し
        ));
        assert!(!is_official_release_asset_url(
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.1/" // 空ファイル名
        ));
        assert!(!is_official_release_asset_url(
            "https://github.com/yachtida/nospacekey/releases/v1.2.1/x.exe" // download 無し
        ));
        assert!(!is_official_release_asset_url(
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.1/a/b.exe" // 7 セグメント
        ));
        // URL として成立しない・scheme 無し・非 HTTP 系。
        assert!(!is_official_release_asset_url(""));
        assert!(!is_official_release_asset_url("not a url"));
        assert!(!is_official_release_asset_url(
            "github.com/yachtida/nospacekey/releases/download/v1/x.exe" // scheme 無し
        ));
        assert!(!is_official_release_asset_url(r"\\attacker\share\x.exe"));
        assert!(!is_official_release_asset_url("file:///C:/x.exe"));
    }

    #[test]
    fn sha256_compare_case_insensitive() {
        assert!(sha256_hex_matches("ABCDef", "abcdEF"));
        assert!(!sha256_hex_matches("abcd", "abce"));
    }

    #[test]
    fn expected_sha256_requires_exact_hex_shape() {
        let lower = "ab".repeat(32);
        let upper = lower.to_uppercase();
        assert!(is_valid_sha256_hex(&lower));
        assert!(is_valid_sha256_hex(&upper));
        assert!(validate_expected_sha256(Some(&lower)).is_ok());
        assert!(validate_expected_sha256(Some(&upper)).is_ok());

        assert!(validate_expected_sha256(None).is_err());
        assert!(validate_expected_sha256(Some("")).is_err());
        assert!(validate_expected_sha256(Some(&"a".repeat(63))).is_err());
        assert!(validate_expected_sha256(Some(&"g".repeat(64))).is_err());
    }

    #[test]
    fn verify_installer_hash_fails_closed_on_missing_expected() {
        // 期待ハッシュ無しは検証スキップ実行を許さない（fail-closed の核心）。
        let err = verify_installer_hash(&"a".repeat(64), None).unwrap_err();
        assert!(err.contains("ハッシュが無いため"));
    }

    #[test]
    fn verify_installer_hash_rejects_mismatch() {
        let actual = "a".repeat(64);
        let expected = "b".repeat(64);
        let err = verify_installer_hash(&actual, Some(&expected)).unwrap_err();
        assert!(err.contains("整合性チェックに失敗しました"));
        assert!(err.contains(&actual) && err.contains(&expected)); // 期待/実際を併記
    }

    #[test]
    fn verify_installer_hash_accepts_match_regardless_of_case() {
        let actual = "ab".repeat(32);
        assert!(verify_installer_hash(&actual, Some(&actual)).is_ok());
        assert!(verify_installer_hash(&actual, Some(&actual.to_uppercase())).is_ok());
    }

    #[test]
    fn sha256_file_hashes_disk_content() {
        use sha2::{Digest, Sha256};
        // 境界再検証の対象はディスクに落ちた実ファイル — 既知バイトを書き込み、
        // ストリーム式でなくファイル読み取り経路のハッシュが一致することを確認する。
        let p = std::env::temp_dir().join("nospacekey-update-test-sha256file.bin");
        std::fs::write(&p, b"installer-bytes").unwrap();
        let expected = hex::encode(Sha256::digest(b"installer-bytes"));
        assert_eq!(sha256_file(&p).unwrap(), expected);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn sha256_file_fails_closed_on_unreadable_file() {
        // 開けない・読めないファイルは Err — 境界検証は「検証不能」を実行に流さない。
        let missing = std::env::temp_dir().join("nospacekey-update-test-missing.bin");
        let _ = std::fs::remove_file(&missing); // 前回実行の残骸があれば掃って冪等に
        assert!(sha256_file(&missing).is_err());
    }

    #[test]
    fn installer_guard_blocks_write_delete_rename_until_dropped() {
        // hash-to-elevated-exec TOCTOU ガードの実効性: 保持中は同一ユーザーからの書き込み
        // オープン・削除・リネーム（= 置換・横取り経路）が共有違反で拒否され、同じハンドル
        // から既知内容のハッシュが読める。読み手は共有する（ShellExecuteExW 側のローダー等
        // を締め出さない設計）。drop 後は再び掃除できる。UAC を出す起動は含めない
        // （テストは非対話で）。
        let staging = create_staging_dir().unwrap();
        let p = staging.path().join(INSTALLER_FILENAME);
        std::fs::write(&p, b"installer-bytes").unwrap();

        let mut guard = InstallerGuard::open(&p).unwrap();
        use sha2::{Digest, Sha256};
        let expected = hex::encode(Sha256::digest(b"installer-bytes"));
        assert_eq!(guard.sha256().unwrap(), expected);

        // ガード保持中: 読み手の open は共有される。
        assert!(std::fs::OpenOptions::new().read(true).open(&p).is_ok());
        // 書き込みオープン・削除・リネームは拒否。
        assert!(std::fs::OpenOptions::new().write(true).open(&p).is_err());
        assert!(std::fs::remove_file(&p).is_err());
        let moved = staging
            .path()
            .join("nospacekey-update-test-guard-moved.bin");
        assert!(std::fs::rename(&p, &moved).is_err());
        assert!(!moved.exists()); // リネーム失敗なら転送先は生えない

        drop(guard);
        std::fs::remove_file(&p).unwrap(); // drop 後は掃除可能
    }

    #[test]
    fn staging_dirs_are_unique_per_attempt() {
        // 試行ごとに OS 乱数で一意な staging が %TEMP% 直下に接頭辞付きで生える —
        // 固定パス時代のような「事前に .part を置いて待つ」攻撃の成立余地を消す。
        let a = create_staging_dir().unwrap();
        let b = create_staging_dir().unwrap();
        assert_ne!(a.path(), b.path());
        assert!(a.path().starts_with(std::env::temp_dir()));
        assert!(a
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(STAGING_DIR_PREFIX)));
    }

    #[test]
    fn staging_directory_cannot_be_renamed_while_guard_is_alive() {
        // `.part` の writer を閉じた後も directory handle が delete sharing を拒むため、
        // 親を退避して元 path に junction を置く攻撃の最初の move が失敗する。
        let staging = create_staging_dir().unwrap();
        let original = staging.path().to_owned();
        let moved = original.with_extension("moved");
        assert!(std::fs::rename(&original, &moved).is_err());
        assert!(original.is_dir());
        assert!(!moved.exists());
    }

    #[test]
    fn installer_guard_rejects_file_from_another_staging_directory() {
        // 最終要素が通常ファイルでも、親 junction を辿った結果が固定した root と異なれば
        // fail-closed。directory handle による rename 防止と独立した境界照合の回帰テスト。
        let expected = create_staging_dir().unwrap();
        let other = create_staging_dir().unwrap();
        let p = other.path().join(INSTALLER_FILENAME);
        std::fs::write(&p, b"installer-bytes").unwrap();

        let err = InstallerGuard::open_in_staging(&p, &expected).unwrap_err();
        assert!(err.contains("親ディレクトリ"));
    }

    #[test]
    fn part_creation_rejects_precreated_entry_and_preserves_link_target() {
        let staging = create_staging_dir().unwrap();
        let part = staging.path().join(format!("{INSTALLER_FILENAME}.part"));

        // 事前に置かれた正規ファイルは CREATE_NEW で拒否 — truncate もされない。
        std::fs::write(&part, b"pre-existing").unwrap();
        assert!(create_part_exclusive(&part).is_err());
        assert_eq!(std::fs::read(&part).unwrap(), b"pre-existing");

        // 事前に置かれた symlink も拒否 — create_new なのでリンクを追って truncate せず
        // リンク先は無傷。symlink 作成権限（開発者モード/管理者）が無い環境は明示 skip。
        let target = staging.path().join("link-target.bin");
        std::fs::write(&target, b"target-bytes").unwrap();
        std::fs::remove_file(&part).unwrap();
        match std::os::windows::fs::symlink_file(&target, &part) {
            Ok(()) => {}
            Err(_) => {
                eprintln!("skipping: symlink 作成権限が無いため precreated-symlink の確認を省略");
                return;
            }
        }
        assert!(create_part_exclusive(&part).is_err());
        assert_eq!(std::fs::read(&part).unwrap(), b"target-bytes");
    }

    #[test]
    fn guard_rejects_reparse_point_target() {
        let staging = create_staging_dir().unwrap();
        let target = staging.path().join("real.bin");
        std::fs::write(&target, b"installer-bytes").unwrap();
        let link = staging.path().join(INSTALLER_FILENAME);
        match std::os::windows::fs::symlink_file(&target, &link) {
            Ok(()) => {}
            Err(_) => {
                eprintln!("skipping: symlink 作成権限が無いため reparse 拒否の確認を省略");
                return;
            }
        }
        // FILE_FLAG_OPEN_REPARSE_POINT + 属性判定で reparse は即拒否 — リンク先の別実体を
        // 検証・実行させない（fail-closed）。
        let err = InstallerGuard::open(&link).unwrap_err();
        assert!(err.contains("再解析ポイント"));
    }

    #[test]
    fn guard_fails_closed_when_writer_preempts() {
        // guard オープンより前に書き手に先取りされた場合の fail-closed:
        // (a) 独占 write handle（share_mode=0）なら guard の open 自体が共有違反で失敗。
        // (b) FILE_SHARE_READ 付き write handle も同じ — guard の share_mode は読み取り
        // しか共有しないため、既存 handle の要求 WRITE アクセスと共有違反となり
        // CreateFile の段階で拒否される（LockFileEx には届かない）。どちらも実行可能
        // 状態に至らない。
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

        let staging = create_staging_dir().unwrap();
        let p = staging.path().join(INSTALLER_FILENAME);
        std::fs::write(&p, b"installer-bytes").unwrap();

        let held = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(&p)
            .unwrap();
        assert!(InstallerGuard::open(&p).is_err());
        drop(held);

        let writer = std::fs::OpenOptions::new()
            .write(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(&p)
            .unwrap();
        assert!(InstallerGuard::open(&p).is_err()); // 先取り writer は open 時点で拒否
        drop(writer);

        assert!(InstallerGuard::open(&p).is_ok()); // 書き手解放後は再び開ける
        std::fs::remove_file(&p).unwrap(); // 掃除可能
    }

    #[test]
    fn guard_execute_path_points_at_staged_installer() {
        // lpFile 用パスは guard ハンドル由来（GetFinalPathNameByHandleW）— 同一ファイルを
        // 指す絶対パスで、`\\?\` 前置のまま ShellExecuteExW に渡らない形へ整形されている。
        let staging = create_staging_dir().unwrap();
        let p = staging.path().join(INSTALLER_FILENAME);
        std::fs::write(&p, b"installer-bytes").unwrap();

        let guard = InstallerGuard::open(&p).unwrap();
        let exec = guard.execute_path().unwrap();
        assert!(exec.is_absolute());
        assert!(!exec.starts_with(r"\\?\"));
        assert_eq!(
            exec.file_name().and_then(|n| n.to_str()),
            Some(INSTALLER_FILENAME)
        );
        assert!(exec.metadata().is_ok());
    }

    #[test]
    fn progress_percent_caps_and_handles_unknown() {
        assert_eq!(progress_percent(50, Some(100)), Some(50));
        assert_eq!(progress_percent(150, Some(100)), Some(100)); // 頭打ち
        assert_eq!(progress_percent(10, None), None);
        assert_eq!(progress_percent(10, Some(0)), None);
    }

    #[test]
    fn installer_size_guards_reject_zero_oversize_and_mismatch() {
        assert!(validate_installer_size(1).is_ok());
        assert!(validate_installer_size(0).is_err());
        assert!(validate_installer_size(MAX_INSTALLER_BYTES + 1).is_err());
        assert!(validate_content_length(10, None).is_ok());
        assert!(validate_content_length(10, Some(10)).is_ok());
        assert!(validate_content_length(10, Some(9)).is_err());
        assert!(validate_content_length(10, Some(MAX_INSTALLER_BYTES + 1)).is_err());
    }

    #[test]
    fn installer_stream_size_guard_rejects_oversize_and_short_eof() {
        assert_eq!(next_received_size(0, 4, 4).unwrap(), 4);
        assert!(next_received_size(3, 2, 4).is_err());
        assert!(next_received_size(MAX_INSTALLER_BYTES, 1, MAX_INSTALLER_BYTES).is_err());
    }

    #[test]
    fn release_list_json_parses_and_selects_with_beta_flag() {
        // 公開リポの実状（v1.2.2-beta.1 + v1.2.1）を縮約したフィクスチャ。body の "#" は
        // r##"..."## で包んで "##" との早期終端を回避。
        let json = r##"[{
            "tag_name": "v1.2.2-beta.1",
            "prerelease": true,
            "draft": false,
            "body": "Mode HUD と Zenzai 重いPC向け fallback。",
            "assets": [
                {"name": "nospacekey-setup-1.2.2-beta.1-devsigned.exe", "size": 33137488, "state": "uploaded",
                 "browser_download_url": "https://github.com/yachtida/nospacekey/releases/download/v1.2.2-beta.1/nospacekey-setup-1.2.2-beta.1-devsigned.exe"},
                {"name": "SHA256SUMS.txt", "size": 103, "state": "uploaded",
                 "browser_download_url": "https://github.com/yachtida/nospacekey/releases/download/v1.2.2-beta.1/SHA256SUMS.txt"}
            ]
        },{
            "tag_name": "v1.2.1",
            "prerelease": false,
            "draft": false,
            "body": "Fixed: update installer stall.",
            "assets": [
                {"name": "nospacekey-setup-1.2.1-devsigned.exe", "size": 33137488, "state": "uploaded",
                 "browser_download_url": "https://github.com/yachtida/nospacekey/releases/download/v1.2.1/nospacekey-setup-1.2.1-devsigned.exe"}
            ]
        }]"##;
        let items: Vec<GithubReleaseJson> = serde_json::from_str(json).unwrap();
        assert_eq!(items.len(), 2);

        // beta OFF → 安定版 1.2.1。
        let (ver, idx) = select_latest_release(&items, false).unwrap();
        assert_eq!(ver, v(1, 2, 1));
        let inst = pick_installer_asset(&items[idx].assets, &format_version(&ver)).unwrap();
        assert_eq!(inst.name, "nospacekey-setup-1.2.1-devsigned.exe");

        // beta ON → 1.2.2-beta.1（1.2.1 より新しい）。
        let (ver, idx) = select_latest_release(&items, true).unwrap();
        assert_eq!(ver, vp(1, 2, 2, "beta.1"));
        let inst = pick_installer_asset(&items[idx].assets, &format_version(&ver)).unwrap();
        assert_eq!(inst.name, "nospacekey-setup-1.2.2-beta.1-devsigned.exe");
        assert!(is_official_release_asset_url(&inst.url));
    }
}
