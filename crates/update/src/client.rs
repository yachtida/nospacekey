//! 短命 checker 用 GitHub API client。

use crate::{release::GithubReleaseJson, RELEASES_API_URL};
use chrono::{DateTime, TimeZone, Utc};
use futures_util::StreamExt;
use std::time::Duration;

pub const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub enum ClientError {
    Build(String),
    Request(String),
    Http {
        status: u16,
        retry_not_before: Option<DateTime<Utc>>,
    },
    BodyTooLarge,
    Body(String),
    Json(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Build(error) => write!(f, "client build failed: {error}"),
            Self::Request(error) => write!(f, "request failed: {error}"),
            Self::Http { status, .. } => write!(f, "GitHub returned HTTP {status}"),
            Self::BodyTooLarge => write!(f, "response body exceeds 2 MiB"),
            Self::Body(error) => write!(f, "response body failed: {error}"),
            Self::Json(error) => write!(f, "response JSON failed: {error}"),
        }
    }
}

impl std::error::Error for ClientError {}

pub struct ReleaseClient {
    client: reqwest::Client,
}

impl ReleaseClient {
    pub fn production() -> Result<Self, ClientError> {
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                "nospacekey-update-checker/",
                env!("CARGO_PKG_VERSION")
            ))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| ClientError::Build(error.to_string()))?;
        Ok(Self { client })
    }

    /// テスト用の HTTP client 差し替え seam。production binary から endpoint は変更できない。
    #[cfg(test)]
    pub(crate) fn from_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    pub async fn fetch(
        &self,
    ) -> Result<(Vec<GithubReleaseJson>, Option<DateTime<Utc>>), ClientError> {
        self.fetch_url(RELEASES_API_URL).await
    }

    #[cfg(test)]
    pub(crate) async fn fetch_url(
        &self,
        endpoint: &str,
    ) -> Result<(Vec<GithubReleaseJson>, Option<DateTime<Utc>>), ClientError> {
        self.fetch_url_impl(endpoint).await
    }

    #[cfg(not(test))]
    async fn fetch_url(
        &self,
        endpoint: &str,
    ) -> Result<(Vec<GithubReleaseJson>, Option<DateTime<Utc>>), ClientError> {
        self.fetch_url_impl(endpoint).await
    }

    async fn fetch_url_impl(
        &self,
        endpoint: &str,
    ) -> Result<(Vec<GithubReleaseJson>, Option<DateTime<Utc>>), ClientError> {
        let response = self
            .client
            .get(endpoint)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|error| ClientError::Request(error.to_string()))?;
        if !response.status().is_success() {
            let retry_not_before = if matches!(response.status().as_u16(), 403 | 429) {
                retry_not_before(response.headers())
            } else {
                None
            };
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                retry_not_before,
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(ClientError::BodyTooLarge);
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| ClientError::Body(error.to_string()))?;
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(ClientError::BodyTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body)
            .map(|releases| (releases, None))
            .map_err(|error| ClientError::Json(error.to_string()))
    }
}

fn retry_not_before(headers: &reqwest::header::HeaderMap) -> Option<DateTime<Utc>> {
    if let Some(value) = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(seconds) = value.trim().parse::<i64>() {
            return Some(Utc::now() + chrono::Duration::seconds(seconds.max(0)));
        }
        if let Ok(date) = DateTime::parse_from_rfc2822(value) {
            return Some(date.with_timezone(&Utc));
        }
    }
    headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|seconds| Utc.timestamp_opt(seconds, 0).single())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_contract_has_fixed_limits() {
        assert_eq!(MAX_RESPONSE_BYTES, 2 * 1024 * 1024);
        assert!(crate::RELEASES_API_URL
            .starts_with("https://api.github.com/repos/yachtida/nospacekey/"));
        let client = reqwest::Client::builder().build().unwrap();
        let _ = ReleaseClient::from_client(client);
    }

    #[test]
    fn retry_after_is_only_recorded_for_parseable_values() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "retry-after",
            reqwest::header::HeaderValue::from_static("60"),
        );
        assert!(retry_not_before(&headers).is_some());
        headers.insert(
            "retry-after",
            reqwest::header::HeaderValue::from_static("not-a-date"),
        );
        assert!(retry_not_before(&headers).is_none());
    }
}
