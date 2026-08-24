//! GitHub release の最小型と、候補選択の純関数。

use semver::Version as SemverVersion;
use serde::Deserialize;
use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

impl Version {
    fn from_semver(v: SemverVersion) -> Self {
        let pre = if v.pre.is_empty() {
            None
        } else {
            Some(v.pre.to_string())
        };
        Self {
            major: v.major,
            minor: v.minor,
            patch: v.patch,
            pre,
        }
    }

    fn as_semver(&self) -> Option<SemverVersion> {
        let mut text = format!("{}.{}.{}", self.major, self.minor, self.patch);
        if let Some(pre) = &self.pre {
            text.push('-');
            text.push_str(pre);
        }
        SemverVersion::parse(&text).ok()
    }
}

/// `1.2.1` / `v1.2.2-beta.1` を semver として解析する。
/// build metadata は比較対象外なので結果へ保持しない。
pub fn parse_version(value: &str) -> Option<Version> {
    let trimmed = value.trim();
    let without_v = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    SemverVersion::parse(without_v)
        .ok()
        .map(Version::from_semver)
}

pub fn compare_versions(a: &Version, b: &Version) -> Ordering {
    match (a.as_semver(), b.as_semver()) {
        (Some(a), Some(b)) => a.cmp(&b),
        _ => (a.major, a.minor, a.patch, &a.pre).cmp(&(b.major, b.minor, b.patch, &b.pre)),
    }
}

pub fn format_version(version: &Version) -> String {
    match &version.pre {
        Some(pre) => format!(
            "{}.{}.{}-{pre}",
            version.major, version.minor, version.patch
        ),
        None => format!("{}.{}.{}", version.major, version.minor, version.patch),
    }
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
pub struct GithubAssetJson {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    pub browser_download_url: String,
    /// GitHub API の upload 状態。欠落は未完了として扱う（fail closed）。
    #[serde(default)]
    pub state: Option<String>,
}

impl GithubAssetJson {
    pub fn is_uploaded(&self) -> bool {
        self.state.as_deref() == Some("uploaded")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Channel {
    Stable,
    Beta,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct InstallerAsset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct InstallableRelease {
    pub version: Version,
    pub channel: Channel,
    pub installer: InstallerAsset,
    pub sums: InstallerAsset,
    pub release_tag: String,
    pub release_notes: String,
}

/// GitHub HTML repo URL から API URL を作る純関数（config の手動確認互換）。
pub fn api_releases_url(repo: &str) -> Option<String> {
    let base = repo
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_end_matches('/');
    let path = base.strip_prefix("https://github.com/")?;
    if path.split('/').count() != 2 || path.contains('?') || path.contains('#') {
        return None;
    }
    Some(format!(
        "https://api.github.com/repos/{path}/releases?per_page=30"
    ))
}

fn release_matches_channel(item: &GithubReleaseJson, version: &Version, channel: Channel) -> bool {
    // GitHub の prerelease boolean と semver の pre-release 部分が食い違う応答は、
    // stable/beta いずれでも候補にしない。API の不整合を通知へ昇格させないため。
    if item.prerelease != version.pre.is_some() {
        return false;
    }
    match channel {
        Channel::Stable => !item.prerelease && version.pre.is_none(),
        Channel::Beta => true,
    }
}

/// draft、不正 tag、API flag 不整合を除外して最大バージョンを返す。
pub fn select_latest_release(
    items: &[GithubReleaseJson],
    include_beta: bool,
) -> Option<(Version, usize)> {
    let channel = if include_beta {
        Channel::Beta
    } else {
        Channel::Stable
    };
    let mut best: Option<(Version, usize)> = None;
    for (index, item) in items.iter().enumerate() {
        if item.draft {
            continue;
        }
        let Some(version) = parse_version(&item.tag_name) else {
            continue;
        };
        if !release_matches_channel(item, &version, channel) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(current, _)| compare_versions(&version, current) == Ordering::Greater)
        {
            best = Some((version, index));
        }
    }
    best
}

pub fn pick_installer_asset(assets: &[GithubAssetJson], version: &str) -> Option<InstallerAsset> {
    let plain = format!("nospacekey-setup-{version}.exe");
    let devsigned = format!("nospacekey-setup-{version}-devsigned.exe");
    let mut fallback = None;
    for asset in assets {
        if !asset.is_uploaded() || !is_official_release_asset_url(&asset.browser_download_url) {
            continue;
        }
        if asset.name == plain {
            return Some(asset_to(asset));
        }
        if asset.name == devsigned && fallback.is_none() {
            fallback = Some(asset_to(asset));
        }
    }
    fallback
}

pub fn pick_sums_asset(assets: &[GithubAssetJson]) -> Option<InstallerAsset> {
    assets
        .iter()
        .find(|asset| {
            asset.name == "SHA256SUMS.txt"
                && asset.is_uploaded()
                && is_official_release_asset_url(&asset.browser_download_url)
        })
        .map(asset_to)
}

fn asset_to(asset: &GithubAssetJson) -> InstallerAsset {
    InstallerAsset {
        name: asset.name.clone(),
        url: asset.browser_download_url.clone(),
        size: asset.size,
    }
}

/// A release's installer and checksum metadata must both be complete and
/// point at assets in that exact release.  This is shared by the background
/// checker and the manual Config flow; callers still decide whether to fetch
/// the sums body and verify the downloaded bytes.
pub fn validate_release_assets(
    release: &GithubReleaseJson,
    version: &Version,
) -> Option<(InstallerAsset, InstallerAsset)> {
    let version_text = format_version(version);
    let installer = pick_installer_asset(&release.assets, &version_text)?;
    let sums = pick_sums_asset(&release.assets)?;
    if !is_exact_release_asset_url(&installer.url, &release.tag_name, &installer.name)
        || !is_exact_release_asset_url(&sums.url, &release.tag_name, &sums.name)
    {
        return None;
    }
    Some((installer, sums))
}

/// インストーラと SHA256SUMS の metadata が同一 release に揃う候補だけを返す。
/// checker はこの関数の結果だけを使い、sums 本体を取得しない。
pub fn select_installable_release(
    items: &[GithubReleaseJson],
    current: &Version,
    include_beta: bool,
) -> Option<InstallableRelease> {
    let (version, index) = select_latest_release(items, include_beta)?;
    if compare_versions(&version, current) != Ordering::Greater {
        return None;
    }
    let release = &items[index];
    let (installer, sums) = validate_release_assets(release, &version)?;
    let channel = if version.pre.is_some() {
        Channel::Beta
    } else {
        Channel::Stable
    };
    Some(InstallableRelease {
        version,
        channel,
        installer,
        sums,
        release_tag: release.tag_name.clone(),
        release_notes: release.body.clone(),
    })
}

fn is_exact_release_asset_url(url: &str, tag: &str, name: &str) -> bool {
    let expected = format!("https://github.com/yachtida/nospacekey/releases/download/{tag}/{name}");
    url == expected
}

pub fn is_valid_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn parse_sha256sums(text: &str, target_filename: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(hash), Some(name)) = (fields.next(), fields.next_back()) else {
            continue;
        };
        if name.strip_prefix('*').unwrap_or(name) != target_filename || !is_valid_sha256_hex(hash) {
            continue;
        }
        if let Some(existing) = found.as_deref() {
            if !existing.eq_ignore_ascii_case(hash) {
                return None;
            }
            // Preserve the first spelling for callers that display or compare
            // the supplied checksum, while accepting a case-only duplicate.
            continue;
        }
        found = Some(hash.to_string());
    }
    found
}

/// 公式 GitHub release asset の初期 URL を厳密に許可する。
pub fn is_official_release_asset_url(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let Some(segments) = url.path_segments() else {
        return false;
    };
    match segments.collect::<Vec<_>>().as_slice() {
        ["yachtida", "nospacekey", "releases", "download", tag, file] => {
            !tag.is_empty() && !file.is_empty() && !tag.contains('%') && !file.contains('%')
        }
        _ => false,
    }
}

/// Validate the installer identity at the direct download command boundary.
///
/// `is_official_release_asset_url` intentionally checks only the URL's repository
/// and release-asset shape because it is also used for checksum assets.  The
/// elevated installer path needs the stronger invariant that the tag and file
/// identify a canonical, newer Nospacekey release.  Keeping that check here
/// makes the manual Config path and the checker use the same asset vocabulary.
pub fn validate_installer_asset_url(value: &str, current: &Version) -> Result<(), String> {
    if !is_official_release_asset_url(value) {
        return Err("ダウンロード元が公式リリースの URL ではありません。".into());
    }
    let url = reqwest::Url::parse(value)
        .map_err(|_| "ダウンロード元 URL を解析できません。".to_string())?;
    let segments = url
        .path_segments()
        .ok_or_else(|| "ダウンロード元 URL のパスが不正です。".to_string())?
        .collect::<Vec<_>>();
    let ["yachtida", "nospacekey", "releases", "download", tag, file] = segments.as_slice() else {
        return Err("ダウンロード元 URL のリリースパスが不正です。".into());
    };
    let version = parse_version(tag)
        .ok_or_else(|| "ダウンロード元 URL のタグが semver ではありません。".to_string())?;
    if compare_versions(&version, current) != Ordering::Greater {
        return Err("ダウンロード元のバージョンが現在のバージョンより新しくありません。".into());
    }
    let version_text = format_version(&version);
    let plain = format!("nospacekey-setup-{version_text}.exe");
    let devsigned = format!("nospacekey-setup-{version_text}-devsigned.exe");
    if *file != plain && *file != devsigned {
        return Err("ダウンロード元のファイル名が canonical installer と一致しません。".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str, url: &str) -> GithubAssetJson {
        GithubAssetJson {
            name: name.into(),
            size: 1,
            browser_download_url: url.into(),
            state: Some("uploaded".into()),
        }
    }

    fn release(tag: &str, prerelease: bool, assets: Vec<GithubAssetJson>) -> GithubReleaseJson {
        GithubReleaseJson {
            tag_name: tag.into(),
            prerelease,
            draft: false,
            body: String::new(),
            assets,
        }
    }

    #[test]
    fn stable_rejects_semver_prerelease_when_flag_is_inconsistent() {
        let releases = vec![release("v9.0.0-beta.1", false, vec![])];
        assert!(select_latest_release(&releases, false).is_none());
        let releases = vec![release("v9.0.0", true, vec![])];
        assert!(select_latest_release(&releases, true).is_none());
    }

    #[test]
    fn installable_requires_uploaded_official_installer_and_sums() {
        let current = parse_version("1.0.0").unwrap();
        let mut installer = asset("nospacekey-setup-2.0.0.exe", "https://github.com/yachtida/nospacekey/releases/download/v2.0.0/nospacekey-setup-2.0.0.exe");
        installer.state = None;
        let no_state = release("v2.0.0", false, vec![installer.clone(), asset("SHA256SUMS.txt", "https://github.com/yachtida/nospacekey/releases/download/v2.0.0/SHA256SUMS.txt")]);
        assert!(select_installable_release(&[no_state], &current, false).is_none());
        installer.state = Some("uploaded".into());
        let yes = release("v2.0.0", false, vec![installer, asset("SHA256SUMS.txt", "https://github.com/yachtida/nospacekey/releases/download/v2.0.0/SHA256SUMS.txt")]);
        assert_eq!(
            select_installable_release(&[yes], &current, false)
                .unwrap()
                .channel,
            Channel::Stable
        );
    }

    #[test]
    fn installable_rejects_asset_urls_from_a_different_release() {
        let version = parse_version("2.0.0").unwrap();
        let release = release(
            "v2.0.0",
            false,
            vec![
                asset(
                    "nospacekey-setup-2.0.0.exe",
                    "https://github.com/yachtida/nospacekey/releases/download/v1.9.0/nospacekey-setup-2.0.0.exe",
                ),
                asset(
                    "SHA256SUMS.txt",
                    "https://github.com/yachtida/nospacekey/releases/download/v2.0.0/SHA256SUMS.txt",
                ),
            ],
        );
        assert!(validate_release_assets(&release, &version).is_none());
    }

    #[test]
    fn direct_installer_identity_accepts_new_stable_beta_and_devsigned_assets() {
        let current = parse_version("1.2.1").unwrap();
        for (tag, file) in [
            ("v1.2.2", "nospacekey-setup-1.2.2.exe"),
            ("v1.2.2", "nospacekey-setup-1.2.2-devsigned.exe"),
            ("v1.2.3-beta.1", "nospacekey-setup-1.2.3-beta.1.exe"),
            (
                "v1.2.3-beta.1",
                "nospacekey-setup-1.2.3-beta.1-devsigned.exe",
            ),
        ] {
            let url =
                format!("https://github.com/yachtida/nospacekey/releases/download/{tag}/{file}");
            assert!(
                validate_installer_asset_url(&url, &current).is_ok(),
                "{url}"
            );
        }
    }

    #[test]
    fn direct_installer_identity_rejects_arbitrary_mismatch_and_old_assets() {
        let current = parse_version("1.2.2").unwrap();
        for url in [
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.3/other.exe",
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.4/nospacekey-setup-1.2.3.exe",
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.2/nospacekey-setup-1.2.2.exe",
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.1/nospacekey-setup-1.2.1.exe",
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.3/nospacekey-setup-1.2.3.exe?download=1",
            "https://github.com/yachtida/nospacekey/releases/download/v1.2.3/nospacekey-setup-1.2.%33.exe",
        ] {
            assert!(validate_installer_asset_url(url, &current).is_err(), "{url}");
        }
    }
}
