//! 設定画面からのアプリ内アップデート確認・適用。
//!
//! GitHub の public repo (yachtida/nospacekey) のリリース一覧を API で問い合わせ、
//! 現在ビルドの CARGO_PKG_VERSION と比較する。新版本があればインストーラ(setup exe)を
//! %TEMP% へダウンロードし SHA256 で検証、ShellExecuteW の runas で昇格起動する。
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

/// %TEMP% 配下のインストーラ保存名（固定・上書き）。毎回別名だと蓄積するため。
const INSTALLER_FILENAME: &str = "nospacekey-update-setup.exe";

/// 同時実行の排他フラグ。
static DOWNLOADING: AtomicBool = AtomicBool::new(false);
/// キャンセル要求フラグ（`cancel_update_download` が立て、受信ループが各チャンクで見る）。
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
// バージョン（semver major.minor.patch + pre-release）。純関数＝単体テスト対象
// ============================================================================

/// 解析済みバージョン。semver の major.minor.patch + pre-release。
/// build metadata("+...") は semver で比較無視なので保持しない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// pre-release 識別子（"beta.1" 等）。None = 正式 release。
    pub pre: Option<String>,
}

/// "1.2.1" / "v1.2.2-beta.1" → Version。先頭 v/V を許容。
/// コアは3分割かつ全部数値、pre は最初の '-' 以降（'+' build metadata は落とす）。
/// バージョン形式は version_consistency.rs が major.minor.patch(-pre) に拘束済みだが、
/// リモート(tag_name)は外部データなので寛容に解析し不正は None で畳む。
pub fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim().trim_start_matches('v').trim_start_matches('V');
    // build metadata 以降を落とす（semver で比較無視）。
    let s = s.split('+').next().unwrap_or("");
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) => (c, Some(p.to_string())),
        None => (s, None),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None; // 4 番目以降のセグメントがあれば不正
    }
    // pre が空文字（"1.2.3-"）は semver 上の不正なので関数ごと None へ。
    let pre = match pre {
        Some(p) if !p.is_empty() => Some(p),
        Some(_) => return None,
        None => None,
    };
    Some(Version { major, minor, patch, pre })
}

/// semver 準拠の比較。
/// - (maj,min,patch) を辞書式で比較。
/// - 同値なら pre 無し(release) > pre 有り(pre-release)。
/// - 両方 pre 有りなら識別子を '.' で分割し要素比較
///   （数値同士は数値比較 / それ以外は辞書式 / 数値 < 非数値 / 短い方を小さく）。
pub fn compare_versions(a: &Version, b: &Version) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.major, a.minor, a.patch).cmp(&(b.major, b.minor, b.patch)) {
        Ordering::Equal => {}
        non_eq => return non_eq,
    }
    match (&a.pre, &b.pre) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater, // release > pre-release
        (Some(_), None) => Ordering::Less,
        (Some(pa), Some(pb)) => compare_prerelease(pa, pb),
    }
}

/// pre-release 識別子列の比較（semver 11）。呼び出し側で (maj,min,patch) が等しいことが前提。
fn compare_prerelease(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ai: Vec<&str> = a.split('.').collect();
    let bi: Vec<&str> = b.split('.').collect();
    for (x, y) in ai.iter().zip(bi.iter()) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(nx), Ok(ny)) => nx.cmp(&ny),
            (Ok(_), Err(_)) => Ordering::Less, // 数値識別子 < 非数値識別子
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    ai.len().cmp(&bi.len()) // 共通要素が全て等しければ短い方が小さい
}

/// Version → "1.2.1" / "1.2.2-beta.1"。インストーラ名照合にも使う。
pub fn format_version(v: &Version) -> String {
    match &v.pre {
        Some(pre) => format!("{}.{}.{}-{}", v.major, v.minor, v.patch, pre),
        None => format!("{}.{}.{}", v.major, v.minor, v.patch),
    }
}

// ============================================================================
// リリース一覧の純関数（単体テスト対象）
// ============================================================================

/// GitHub releases HTML URL から releases 一覧 API URL を組み立てる純関数。
/// `/releases/latest` は pre-release を除外してしまうため、beta 含む/含まないは
/// `select_latest_release` で制御する（エンドポイントは一覧で共通）。
pub fn api_releases_url(repo: &str) -> Option<String> {
    let base = repo.trim_end_matches('/').trim_end_matches(".git").trim_end_matches('/');
    let path = base.strip_prefix("https://github.com/")?;
    Some(format!("https://api.github.com/repos/{path}/releases?per_page=30"))
}

/// 特定タグのリリースページ URL（情報リンク用）。
fn release_tag_url(repo: &str, tag: &str) -> String {
    let base = repo.trim_end_matches('/').trim_end_matches(".git").trim_end_matches('/');
    format!("{base}/releases/tag/{tag}")
}

/// リリース一覧から最新の1件を選ぶ。
/// draft は常に除外。`include_beta=false` なら pre-release も除外。
/// 残りを tag_name → Version で解析し最大版本を返す（解析失敗のタグは飛ばす）。
/// 戻り値は (最新Version, items 内の index)。
pub fn select_latest_release(
    items: &[GithubReleaseJson],
    include_beta: bool,
) -> Option<(Version, usize)> {
    let mut best: Option<(Version, usize)> = None;
    for (i, item) in items.iter().enumerate() {
        if item.draft {
            continue;
        }
        if !include_beta && item.prerelease {
            continue;
        }
        let Some(v) = parse_version(item.tag_name.trim()) else { continue };
        match &best {
            None => best = Some((v, i)),
            Some((bv, _)) => {
                if compare_versions(&v, bv) == std::cmp::Ordering::Greater {
                    best = Some((v, i));
                }
            }
        }
    }
    best
}

/// インストーラアセット。フロントへ渡し、DL にも使う。
#[derive(Clone, serde::Serialize)]
pub struct InstallerAsset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

/// リリースの assets から該当バージョンの setup exe を選ぶ。
/// `nospacekey-setup-{version}.exe`（OV 署名・配布用）を優先し、無ければ
/// `nospacekey-setup-{version}-devsigned.exe`（現在の公開実態）へ落ちる。
/// `version` は pre-release 含む完全版文字列（"1.2.2-beta.1" 等）。
pub fn pick_installer_asset(assets: &[GithubAssetJson], version: &str) -> Option<InstallerAsset> {
    let plain = format!("nospacekey-setup-{version}.exe");
    let devsigned = format!("nospacekey-setup-{version}-devsigned.exe");
    let mut devsigned_hit: Option<&GithubAssetJson> = None;
    for a in assets {
        if a.name == plain {
            return Some(asset_to(a));
        }
        if a.name == devsigned && devsigned_hit.is_none() {
            devsigned_hit = Some(a);
        }
    }
    devsigned_hit.map(asset_to)
}

fn asset_to(a: &GithubAssetJson) -> InstallerAsset {
    InstallerAsset {
        name: a.name.clone(),
        url: a.browser_download_url.clone(),
        size: a.size,
    }
}

/// SHA256SUMS.txt（sha256sum -c 互換: `<hash>  <filename>`）から対象ファイル名のハッシュを抽出。
/// セパレータが 2 空白でも ` *` でもよいよう空白トークン分割し first=hash, last=filename で照合。
pub fn parse_sha256sums(text: &str, target_filename: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut toks = line.split_whitespace();
        let hash = toks.next()?;
        let fname = toks.next_back()?;
        // sha256sum の text-mode は `*<filename>` と出力する（binary-mode はそのまま）。
        // 実際の SHA256SUMS.txt がどちらでも照合できるよう先頭の * を落とす。
        let fname = fname.strip_prefix('*').unwrap_or(fname);
        if fname == target_filename {
            return Some(hash.to_string());
        }
    }
    None
}

/// browser_download_url が GitHub 公式ホスト始まりか（改竄応答の防御）。
/// reqwest は 302 を追従して release-assets.githubusercontent.com へ飛ぶが、
/// ここで検証するのは API が返した *初期* URL のみ。
pub fn is_github_download_url(url: &str) -> bool {
    url.starts_with("https://github.com/")
}

/// SHA256 hex の大小無視比較（download.rs の sha256_hex_matches と同判定）。
pub fn sha256_hex_matches(actual: &str, expected: &str) -> bool {
    actual.eq_ignore_ascii_case(expected)
}

/// 進捗率 0..=100（download.rs と同一セマンティクス）。total 不明・0 は None。
pub fn progress_percent(received: u64, total: Option<u64>) -> Option<u8> {
    match total {
        Some(t) if t > 0 => Some(((received.min(t) * 100) / t) as u8),
        _ => None,
    }
}

// ============================================================================
// GitHub API 型（serde Deserialize）
// ============================================================================

#[derive(serde::Deserialize)]
pub struct GithubReleaseJson {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub assets: Vec<GithubAssetJson>,
}

#[derive(serde::Deserialize)]
pub struct GithubAssetJson {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    pub browser_download_url: String,
}

// ============================================================================
// tauri コマンド
// ============================================================================

/// UI へ返す確認結果。`#[serde(tag = "kind")]` で JS は `status.kind` で分岐する。
#[derive(serde::Serialize)]
#[serde(tag = "kind")]
pub enum UpdateStatus {
    UpToDate { current: String },
    Available {
        current: String,
        latest: String,
        installer_url: String,
        installer_name: String,
        installer_size: u64,
        /// SHA256SUMS.txt から導出した期待ハッシュ（無ければ None = 検証スキップ）。
        expected_sha256: Option<String>,
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
        .map_err(|e| format!("最新バージョンの確認に失敗しました（ネットワークを確認してください）: {e}"))?;
    if !resp.status().is_success() {
        // 403 = レート制限の可能性。404 = リリース未公開。UI は Err を「確認できませんでした」へ。
        return Err(format!("最新バージョンの確認に失敗しました（HTTP {}）。", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("リリース情報の読み取りに失敗しました: {e}"))?;
    let items: Vec<GithubReleaseJson> =
        serde_json::from_slice(&bytes).map_err(|e| format!("リリース情報の解析に失敗しました: {e}"))?;

    let (latest, idx) = select_latest_release(&items, include_beta)
        .ok_or_else(|| "公開されているリリースが見つかりませんでした".to_string())?;
    let rel = &items[idx];
    let cur = parse_version(&current_str)
        .ok_or_else(|| format!("現在バージョンの解析に失敗しました: {current_str}"))?;

    // 新版本が *厳密に* 大きいときだけ案内する（同値・降格は最新扱い）。
    if compare_versions(&latest, &cur) != std::cmp::Ordering::Greater {
        return Ok(UpdateStatus::UpToDate { current: current_str });
    }

    let latest_str = format_version(&latest);
    let installer = pick_installer_asset(&rel.assets, &latest_str)
        .ok_or_else(|| "最新版のインストーラが見つかりませんでした".to_string())?;

    // SHA256SUMS.txt があれば期待ハッシュを導出（無ければ検証省略）。
    let expected = fetch_expected_sha256(&client, &rel.assets, &installer.name).await;

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

/// SHA256SUMS.txt アセットを取得し対象ファイルのハッシュを引く。無ければ None。
/// ファイルが無い・取得失敗・対象行無しのいずれも「検証無しで続行」なので None へ畳む。
async fn fetch_expected_sha256(
    client: &reqwest::Client,
    assets: &[GithubAssetJson],
    installer_name: &str,
) -> Option<String> {
    let sums = assets.iter().find(|a| a.name == "SHA256SUMS.txt")?;
    let resp = client.get(&sums.browser_download_url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let text = resp.text().await.ok()?;
    parse_sha256sums(&text, installer_name)
}

/// 進行中ダウンロードのキャンセル要求（受信ループが次チャンクで気づいて中断・掃除する）。
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
) -> Result<(), String> {
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    use std::io::Write;

    // 排他: 既に走っていれば弾く。ガードで DOWNLOADING を必ず戻す。
    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        return Err("既にアップデート処理中です。".into());
    }
    let _guard = UpdateGuard;
    CANCEL_REQUESTED.store(false, Ordering::SeqCst);

    if !is_github_download_url(&installer_url) {
        return Err("ダウンロード元が GitHub 公式ホストではありません。".into());
    }

    let dir = std::env::temp_dir();
    let dest = dir.join(INSTALLER_FILENAME);
    // 完成前のファイルを本名で観測させない（中断された半端ファイルを実行させない）。
    let part = dir.join(format!("{INSTALLER_FILENAME}.part"));

    let client = http_client()?;
    let resp = client
        .get(&installer_url)
        .send()
        .await
        .map_err(|e| format!("ダウンロードに失敗しました（ネットワークを確認してください）: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("ダウンロードに失敗しました（HTTP {}）。", resp.status()));
    }
    let total = resp.content_length();

    let mut file =
        std::fs::File::create(&part).map_err(|e| format!("一時ファイルを作成できません: {e}"))?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    // 進捗イベントの間引き: download.rs と同一（整数% 変化時か total 不明なら 1MB 毎）。
    let mut last_emit_pct: Option<u8> = None;
    let mut last_emit_bytes: u64 = 0;
    let mut stream = resp.bytes_stream();

    let scrub = |file: std::fs::File, part: &Path| {
        drop(file);
        let _ = std::fs::remove_file(part);
    };

    while let Some(item) = stream.next().await {
        if CANCEL_REQUESTED.load(Ordering::SeqCst) {
            scrub(file, &part);
            return Err("キャンセルしました。".into());
        }
        let chunk = match item {
            Ok(c) => c,
            Err(e) => {
                scrub(file, &part);
                return Err(format!("受信中にエラーが発生しました: {e}"));
            }
        };
        if let Err(e) = file.write_all(&chunk) {
            scrub(file, &part);
            return Err(format!("書き込みに失敗しました（ディスクの空き容量を確認してください）: {e}"));
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
            let _ = app.emit(PROGRESS_EVENT, Progress { received, total, percent: pct });
        }
    }
    let _ = file.flush();
    drop(file);

    // 整合性検証（期待ハッシュがあれば照合）。不一致は破棄して明快に失敗。
    let actual = hex::encode(hasher.finalize());
    if let Some(expected) = expected_sha256.as_deref() {
        if !sha256_hex_matches(&actual, expected) {
            let _ = std::fs::remove_file(&part);
            return Err(format!(
                "整合性チェックに失敗しました（ダウンロードが破損した可能性があります）。\n期待 {expected}\n実際 {actual}"
            ));
        }
    }

    // 本名へ原子的に置き換え（同一ボリューム内 rename）。
    if let Err(e) = std::fs::rename(&part, &dest) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("インストーラの配置に失敗しました: {e}"));
    }

    // 昇格起動（runas → UAC）。インストーラが config/engine を taskkill し使用中 DLL を置換する。
    run_installer_elevated(&dest)?;

    Ok(())
}

/// インストーラを UAC 昇格付きで起動する。失敗コード(<=32)は Err へ。
fn run_installer_elevated(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let file: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let verb: Vec<u16> = "runas".encode_utf16().chain([0]).collect();
    unsafe {
        let hinst = ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        // 戻り値 <= 32 は Win32 エラーコード（0=OOM, 2=FILE_NOT_FOUND, 5=ACCESS_DENIED 等）。
        let code = hinst.0 as isize;
        if code <= 32 {
            return Err(format!("インストーラの起動に失敗しました（code {code}）。"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(maj: u64, min: u64, pat: u64) -> Version {
        Version { major: maj, minor: min, patch: pat, pre: None }
    }
    fn vp(maj: u64, min: u64, pat: u64, pre: &str) -> Version {
        Version { major: maj, minor: min, patch: pat, pre: Some(pre.into()) }
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
        assert_eq!(parse_version("1.2.2-beta.1+sha"), Some(vp(1, 2, 2, "beta.1")));
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
        assert_eq!(compare_versions(&v(1, 3, 0), &v(1, 2, 1)), Ordering::Greater);
        assert_eq!(compare_versions(&v(1, 2, 1), &v(1, 2, 1)), Ordering::Equal);
        assert_eq!(compare_versions(&v(2, 0, 0), &v(1, 9, 9)), Ordering::Greater);
        assert_eq!(compare_versions(&v(1, 9, 9), &v(2, 0, 0)), Ordering::Less);
        assert_eq!(compare_versions(&v(1, 2, 10), &v(1, 2, 9)), Ordering::Greater);
    }

    #[test]
    fn compare_versions_release_beats_prerelease() {
        use std::cmp::Ordering;
        // 同 (maj,min,patch) なら release > pre-release（semver 規則）。
        assert_eq!(compare_versions(&v(1, 2, 2), &vp(1, 2, 2, "beta.1")), Ordering::Greater);
        assert_eq!(compare_versions(&vp(1, 2, 2, "beta.1"), &v(1, 2, 2)), Ordering::Less);
    }

    #[test]
    fn compare_versions_higher_patch_prerelease_beats_lower_release() {
        use std::cmp::Ordering;
        // 1.2.2-beta.1 は 1.2.1(安定版) より新しい（patch が上位）。= beta 通知の根拠。
        assert_eq!(compare_versions(&vp(1, 2, 2, "beta.1"), &v(1, 2, 1)), Ordering::Greater);
        assert_eq!(compare_versions(&v(1, 2, 1), &vp(1, 2, 2, "beta.1")), Ordering::Less);
    }

    #[test]
    fn compare_versions_prerelease_identifiers() {
        use std::cmp::Ordering;
        // beta.1 < beta.2（数値識別子比較）。
        assert_eq!(compare_versions(&vp(1, 2, 2, "beta.1"), &vp(1, 2, 2, "beta.2")), Ordering::Less);
        // beta < rc（非数値識別子の辞書式）。
        assert_eq!(compare_versions(&vp(1, 2, 2, "beta.1"), &vp(1, 2, 2, "rc.1")), Ordering::Less);
        // 数値識別子 < 非数値識別子。
        assert_eq!(compare_versions(&vp(1, 2, 2, "1"), &vp(1, 2, 2, "alpha")), Ordering::Less);
        // 同値。
        assert_eq!(compare_versions(&vp(1, 2, 2, "beta.1"), &vp(1, 2, 2, "beta.1")), Ordering::Equal);
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
        }
    }

    #[test]
    fn select_latest_skips_drafts_and_unparseable() {
        let items = vec![
            rel("v1.2.2-beta.1", true, false),
            rel("nightly", false, true), // draft → 除外
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
        assert!(is_github_download_url(&got.url));
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
        let assets = vec![asset("nospacekey-setup-1.2.2-beta.1-devsigned.exe", 33137488)];
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
        let text = "abc123def456  nospacekey-setup-1.2.1-devsigned.exe\n\
                    fedcba987654  nospacekey-setup-1.2.1.exe\n";
        assert_eq!(
            parse_sha256sums(text, "nospacekey-setup-1.2.1-devsigned.exe").as_deref(),
            Some("abc123def456")
        );
        assert_eq!(
            parse_sha256sums(text, "nospacekey-setup-1.2.1.exe").as_deref(),
            Some("fedcba987654")
        );
    }

    #[test]
    fn parse_sha256sums_handles_text_mode_separator() {
        let text = "abc123 *nospacekey-setup-1.2.1.exe\n";
        assert_eq!(
            parse_sha256sums(text, "nospacekey-setup-1.2.1.exe").as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn parse_sha256sums_none_when_target_absent() {
        assert!(parse_sha256sums("abc123  other-file.exe\n", "nospacekey-setup-1.2.1.exe").is_none());
        assert!(parse_sha256sums("", "nospacekey-setup-1.2.1.exe").is_none());
    }

    #[test]
    fn is_github_download_url_gates_to_github_https() {
        assert!(is_github_download_url(
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.1/x.exe"
        ));
        assert!(!is_github_download_url("https://evil.com/x.exe"));
        assert!(!is_github_download_url("http://github.com/x.exe")); // https 必須
        assert!(!is_github_download_url(r"\\attacker\share\x.exe"));
        assert!(!is_github_download_url(""));
    }

    #[test]
    fn sha256_compare_case_insensitive() {
        assert!(sha256_hex_matches("ABCDef", "abcdEF"));
        assert!(!sha256_hex_matches("abcd", "abce"));
    }

    #[test]
    fn progress_percent_caps_and_handles_unknown() {
        assert_eq!(progress_percent(50, Some(100)), Some(50));
        assert_eq!(progress_percent(150, Some(100)), Some(100)); // 頭打ち
        assert_eq!(progress_percent(10, None), None);
        assert_eq!(progress_percent(10, Some(0)), None);
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
                {"name": "nospacekey-setup-1.2.2-beta.1-devsigned.exe", "size": 33137488,
                 "browser_download_url": "https://github.com/yachtida/nospacekey/releases/download/v1.2.2-beta.1/nospacekey-setup-1.2.2-beta.1-devsigned.exe"},
                {"name": "SHA256SUMS.txt", "size": 103,
                 "browser_download_url": "https://github.com/yachtida/nospacekey/releases/download/v1.2.2-beta.1/SHA256SUMS.txt"}
            ]
        },{
            "tag_name": "v1.2.1",
            "prerelease": false,
            "draft": false,
            "body": "Fixed: update installer stall.",
            "assets": [
                {"name": "nospacekey-setup-1.2.1-devsigned.exe", "size": 33137488,
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
        assert!(is_github_download_url(&inst.url));
    }
}
