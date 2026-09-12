//! nospacekey の自動更新確認に共有される、GUI/IME 非依存の基盤。
//!
//! この crate は GitHub のリリース判定、checker 専用 state、通知 payload、
//! per-user task の定義だけを提供する。インストーラのダウンロード・SHA256
//! 内容取得・UAC 起動は意図的に公開 API に含めない。

pub mod client;
pub mod notification;
pub mod release;
pub mod scheduler;
pub mod state;

pub use client::MAX_RESPONSE_BYTES;
pub use release::{
    api_releases_url, compare_versions, format_version, is_official_release_asset_url,
    parse_sha256sums, parse_version, pick_installer_asset, select_installable_release,
    select_latest_release, validate_installer_asset_url, GithubAssetJson, GithubReleaseJson,
    InstallableRelease, InstallerAsset, Version,
};
pub use state::{NotificationTuple, StateStore, UpdateState, UPDATE_STATE_SCHEMA_VERSION};

/// 本番で問い合わせる URL。endpoint は argv/settings/state から変更できない。
pub const RELEASES_API_URL: &str =
    "https://api.github.com/repos/yachtida/nospacekey/releases?per_page=30";
