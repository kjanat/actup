//! HTTP client for GitHub API
//!
//! All async methods in this module can return errors for network failures,
//! rate limiting, authentication issues, or resource not found.

#![allow(clippy::missing_errors_doc)] // All async methods return Result with obvious error conditions

use std::{num::NonZeroU8, sync::Arc, time::Duration};

use actup_core::{Error, Result, encode_path, error::Source};
use actup_providers::{
    DEFAULT_MAX_RETRIES, DEFAULT_RETRY_BASE_DELAY_MS, MAX_PER_PAGE, MIN_PER_PAGE,
};
use reqwest::{
    Client, Response, StatusCode,
    header::{ACCEPT, AUTHORIZATION, ETAG, HeaderMap, HeaderValue, IF_NONE_MATCH, USER_AGENT},
};
use tracing::{debug, warn};

use crate::types::{
    GitHubBranch, GitHubCommit, GitHubContent, GitHubRef, GitHubRepo, GitHubSearch, GitHubTag,
    GitHubTagObject,
};

/// Maximum repositories one action search returns
pub const SEARCH_RESULTS: u8 = 20;

/// Topic that the action search requires
pub const ACTION_TOPIC: &str = "github-actions";

/// Default items per page
pub const DEFAULT_PER_PAGE: u8 = MAX_PER_PAGE;

/// A response body with the `ETag` the server sent for it.
#[derive(Debug)]
pub struct Tagged<T> {
    /// The parsed response body.
    pub body: T,
    /// The response `ETag` header.
    pub etag: Option<String>,
}

/// The answer to a GET sent with an `If-None-Match` validator.
#[derive(Debug)]
pub enum Conditional<T> {
    /// The server answered 304 to the validator.
    NotModified,
    /// The server sent the resource.
    Modified(Tagged<T>),
}

/// HTTP client configured for GitHub API
#[derive(Debug, Clone)]
pub struct GitHubClient {
    client: Client,
    api_base: String,
    host: String,
    /// Max retry attempts for transient errors
    max_retries: u32,
    /// Base delay in ms for exponential backoff
    retry_base_delay_ms: u64,
}

impl GitHubClient {
    /// Create a new client for github.com
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be created.
    pub fn new(token: Option<&str>) -> Result<Self> {
        Self::with_api_base(crate::GITHUB_API_HOST, crate::GITHUB_HOST, token)
    }

    /// Create a client for a custom API endpoint (e.g., GitHub Enterprise)
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be created or token is invalid.
    pub fn with_api_base(api_host: &str, host: &str, token: Option<&str>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        let user_agent = format!("actup/{}", env!("CARGO_PKG_VERSION"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&user_agent).map_err(|e| Error::Network {
                operation: "build user-agent header".to_string(),
                message: String::new(),
                source: Some(Arc::new(e)),
            })?,
        );
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );

        if let Some(t) = token {
            let auth_value = format!("Bearer {t}");
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&auth_value).map_err(|e| Error::Auth {
                    provider: "github".to_string(),
                    reason: "invalid token format".into(),
                    source: Some(Arc::new(e)),
                })?,
            );
        }

        let mut builder = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30));

        // Bypass system proxy for loopback hosts: when HTTP(S)_PROXY is set in
        // the environment, reqwest will otherwise route requests to localhost
        // mock/dev servers through the proxy and fail.
        if is_loopback_host(api_host) {
            builder = builder.no_proxy();
        }

        let client = builder.build().map_err(|e| Error::Network {
            operation: "create HTTP client".to_string(),
            message: String::new(),
            source: Some(Arc::new(e)),
        })?;

        let api_base = if api_host.starts_with("http") {
            api_host.to_string()
        } else {
            format!("https://{api_host}")
        };

        Ok(Self {
            client,
            api_base,
            host: host.to_string(),
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_delay_ms: DEFAULT_RETRY_BASE_DELAY_MS,
        })
    }

    /// Set maximum retry attempts for transient errors (0 = no retries)
    #[must_use]
    pub const fn max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    /// Set base delay in milliseconds for exponential backoff (doubles each retry)
    #[must_use]
    pub const fn retry_base_delay_ms(mut self, ms: u64) -> Self {
        self.retry_base_delay_ms = ms;
        self
    }

    /// Get the host this client serves
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    fn repo_url(&self, owner: &str, repo: &str) -> Result<String> {
        Ok(format!(
            "{}/repos/{}/{}",
            self.api_base,
            encode_path(owner)?,
            encode_path(repo)?
        ))
    }

    /// Get the API base URL this client targets.
    #[must_use]
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// List tags for a repository (paginated, fetches all)
    ///
    /// # Arguments
    /// * `owner` - Repository owner
    /// * `repo` - Repository name
    /// * `per_page` - Items per page (clamped to 1..=100)
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or repo not found.
    pub async fn list_tags(
        &self,
        owner: &str,
        repo: &str,
        per_page: Option<u8>,
    ) -> Result<Vec<GitHubTag>> {
        let per_page = per_page
            .unwrap_or(DEFAULT_PER_PAGE)
            .clamp(MIN_PER_PAGE, MAX_PER_PAGE);

        let mut all_tags = Vec::new();
        let mut url = Some(format!(
            "{}/tags?per_page={per_page}",
            self.repo_url(owner, repo)?
        ));

        while let Some(current_url) = url {
            debug!(url = %current_url, "fetching tags page");

            let response = self.get(&current_url).await?;
            url = Self::parse_next_link(&response);

            let tags: Vec<GitHubTag> = self.handle_response(response, owner, repo).await?;
            all_tags.extend(tags);
        }

        Ok(all_tags)
    }

    /// List tags for a repository, stopping after `max_tags` entries.
    ///
    /// This avoids walking every pagination page when actup only needs the
    /// newest N tags for policy evaluation.
    pub async fn list_tags_limited(
        &self,
        owner: &str,
        repo: &str,
        max_tags: NonZeroU8,
    ) -> Result<Vec<GitHubTag>> {
        Ok(self
            .list_tags_limited_tagged(owner, repo, max_tags)
            .await?
            .body)
    }

    /// List the newest `max_tags` tags with the `ETag` of the first page.
    pub async fn list_tags_limited_tagged(
        &self,
        owner: &str,
        repo: &str,
        max_tags: NonZeroU8,
    ) -> Result<Tagged<Vec<GitHubTag>>> {
        let response = self.get(&self.tags_url(owner, repo, max_tags)?).await?;
        self.collect_tags(response, owner, repo, max_tags).await
    }

    /// List the newest `max_tags` tags, sending `etag` as `If-None-Match` on the first page.
    ///
    /// A 304 to the first page answers [`Conditional::NotModified`].
    pub async fn list_tags_limited_if_none_match(
        &self,
        owner: &str,
        repo: &str,
        max_tags: NonZeroU8,
        etag: &str,
    ) -> Result<Conditional<Vec<GitHubTag>>> {
        let response = self
            .get_if_none_match(&self.tags_url(owner, repo, max_tags)?, Some(etag))
            .await?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(Conditional::NotModified);
        }
        Ok(Conditional::Modified(
            self.collect_tags(response, owner, repo, max_tags).await?,
        ))
    }

    fn tags_url(&self, owner: &str, repo: &str, max_tags: NonZeroU8) -> Result<String> {
        let per_page = max_tags.get().min(MAX_PER_PAGE);
        let url = format!("{}/tags?per_page={per_page}", self.repo_url(owner, repo)?);
        debug!(url = %url, max_tags, "fetching first tags page");
        Ok(url)
    }

    async fn collect_tags(
        &self,
        first: Response,
        owner: &str,
        repo: &str,
        max_tags: NonZeroU8,
    ) -> Result<Tagged<Vec<GitHubTag>>> {
        let max_tags = usize::from(max_tags.get());
        let mut url = Self::parse_next_link(&first);
        let Tagged { body: first, etag } =
            self.tagged::<Vec<GitHubTag>>(first, owner, repo).await?;
        let mut all_tags = Vec::with_capacity(max_tags);
        all_tags.extend(first.into_iter().take(max_tags));

        while let Some(current_url) = url {
            if all_tags.len() >= max_tags {
                break;
            }

            debug!(url = %current_url, max_tags, "fetching limited tags page");

            let response = self.get(&current_url).await?;
            url = Self::parse_next_link(&response);

            let tags: Vec<GitHubTag> = self.handle_response(response, owner, repo).await?;
            let remaining = max_tags.saturating_sub(all_tags.len());
            all_tags.extend(tags.into_iter().take(remaining));
        }

        Ok(Tagged {
            body: all_tags,
            etag,
        })
    }

    /// Parse Link header for next page URL
    fn parse_next_link(response: &Response) -> Option<String> {
        let link_header = response.headers().get("link")?.to_str().ok()?;

        // Link header format: <url>; rel="next", <url>; rel="last"
        for part in link_header.split(',') {
            let part = part.trim();
            if part.contains(r#"rel="next""#) {
                // Extract URL between < and >
                let start = part.find('<')? + 1;
                let end = part.find('>')?;
                return Some(part[start..end].to_string());
            }
        }
        None
    }

    /// Get repository info
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or repo not found.
    pub async fn get_repo(&self, owner: &str, repo: &str) -> Result<GitHubRepo> {
        let url = self.repo_url(owner, repo)?;
        debug!(url = %url, "fetching repo info");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Get repository info with its `ETag`.
    pub async fn get_repo_tagged(&self, owner: &str, repo: &str) -> Result<Tagged<GitHubRepo>> {
        let url = self.repo_url(owner, repo)?;
        debug!(url = %url, "fetching repo info");

        let response = self.get(&url).await?;
        self.tagged(response, owner, repo).await
    }

    /// Search repositories that match `query` and carry the `github-actions` topic
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit or auth failure.
    pub async fn search_repositories(&self, query: &str) -> Result<GitHubSearch> {
        let url = reqwest::Url::parse_with_params(
            &format!("{}/search/repositories", self.api_base),
            &[
                ("q", format!("{query} topic:{ACTION_TOPIC}")),
                ("per_page", SEARCH_RESULTS.to_string()),
            ],
        )
        .map_err(|e| Error::Internal {
            context: "search URL".to_string(),
            message: "invalid search URL".to_string(),
            source: Some(Arc::new(e)),
        })?;
        debug!(url = %url, "searching repositories");

        let response = self.get(url.as_str()).await?;
        self.handle_response(response, "search", query).await
    }

    /// Get repository info, sending `etag` as `If-None-Match`.
    pub async fn get_repo_if_none_match(
        &self,
        owner: &str,
        repo: &str,
        etag: &str,
    ) -> Result<Conditional<GitHubRepo>> {
        let url = self.repo_url(owner, repo)?;
        debug!(url = %url, "fetching repo info");

        let response = self.get_if_none_match(&url, Some(etag)).await?;
        self.conditional(response, owner, repo).await
    }

    /// Get a specific git ref
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or ref not found.
    pub async fn get_ref(&self, owner: &str, repo: &str, ref_name: &str) -> Result<GitHubRef> {
        // GitHub API wants refs without the leading "refs/"
        let ref_path = ref_name.strip_prefix("refs/").unwrap_or(ref_name);

        let url = format!(
            "{}/git/ref/{}",
            self.repo_url(owner, repo)?,
            encode_path(ref_path)?
        );
        debug!(url = %url, "fetching ref");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Get file content from repository
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or path not found.
    pub async fn get_content(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        ref_name: Option<&str>,
    ) -> Result<GitHubContent> {
        let mut url = format!(
            "{}/contents/{}",
            self.repo_url(owner, repo)?,
            encode_path(path)?
        );

        if let Some(r) = ref_name {
            url.push_str("?ref=");
            url.push_str(&encode_path(r)?);
        }

        debug!(url = %url, "fetching content");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Get branch info
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or branch not found.
    pub async fn get_branch(&self, owner: &str, repo: &str, branch: &str) -> Result<GitHubBranch> {
        let url = format!(
            "{}/branches/{}",
            self.repo_url(owner, repo)?,
            encode_path(branch)?
        );
        debug!(url = %url, "fetching branch");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Get branch info with its `ETag`.
    pub async fn get_branch_tagged(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Tagged<GitHubBranch>> {
        let url = format!(
            "{}/branches/{}",
            self.repo_url(owner, repo)?,
            encode_path(branch)?
        );
        debug!(url = %url, "fetching branch");

        let response = self.get(&url).await?;
        self.tagged(response, owner, repo).await
    }

    /// Get branch info, sending `etag` as `If-None-Match`.
    pub async fn get_branch_if_none_match(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        etag: &str,
    ) -> Result<Conditional<GitHubBranch>> {
        let url = format!(
            "{}/branches/{}",
            self.repo_url(owner, repo)?,
            encode_path(branch)?
        );
        debug!(url = %url, "fetching branch");

        let response = self.get_if_none_match(&url, Some(etag)).await?;
        self.conditional(response, owner, repo).await
    }

    /// Get a commit by full or abbreviated SHA
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or commit not found.
    /// An unknown commit in an existing repository is [`Error::RefNotFound`].
    pub async fn get_commit(&self, owner: &str, repo: &str, sha: &str) -> Result<GitHubCommit> {
        let url = format!(
            "{}/commits/{}",
            self.repo_url(owner, repo)?,
            encode_path(sha)?
        );
        debug!(url = %url, "fetching commit");

        let response = self.get(&url).await?;
        if response.status() == StatusCode::UNPROCESSABLE_ENTITY {
            return Err(Error::RefNotFound {
                owner: owner.to_string(),
                repo: repo.to_string(),
                ref_name: sha.to_string(),
                host: Some(self.host.clone()),
            });
        }
        self.handle_response(response, owner, repo).await
    }

    /// Get an annotated tag object (for dereferencing to commit SHA)
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or tag not found.
    pub async fn get_tag_object(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> Result<GitHubTagObject> {
        let url = format!(
            "{}/git/tags/{}",
            self.repo_url(owner, repo)?,
            encode_path(sha)?
        );
        debug!(url = %url, "fetching tag object");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Make a GET request with retry and exponential backoff
    async fn get(&self, url: &str) -> Result<Response> {
        self.get_if_none_match(url, None).await
    }

    async fn get_if_none_match(&self, url: &str, etag: Option<&str>) -> Result<Response> {
        let mut last_error = None;

        for attempt in 0..self.max_retries {
            if attempt > 0 {
                let delay = Duration::from_millis(self.retry_base_delay_ms * 2u64.pow(attempt - 1));
                debug!(
                    attempt,
                    delay_ms = delay.as_millis(),
                    "retrying after backoff"
                );
                tokio::time::sleep(delay).await;
            }

            let request = etag.map_or_else(
                || self.client.get(url),
                |etag| self.client.get(url).header(IF_NONE_MATCH, etag),
            );
            match request.send().await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    // Only retry on transient errors
                    let is_retryable = e.is_timeout() || e.is_connect();
                    if !is_retryable || attempt + 1 >= self.max_retries {
                        return Err(if e.is_timeout() {
                            Error::Network {
                                operation: "GET request".to_string(),
                                message: "request timed out".to_string(),
                                source: Some(Arc::new(e)),
                            }
                        } else if e.is_connect() {
                            Error::Network {
                                operation: "GET request".to_string(),
                                message: "connection failed".to_string(),
                                source: Some(Arc::new(e)),
                            }
                        } else {
                            Error::Network {
                                operation: "GET request".to_string(),
                                message: String::new(),
                                source: Some(Arc::new(e)),
                            }
                        });
                    }
                    warn!(attempt, error = %e, "request failed, will retry");
                    last_error = Some(e);
                }
            }
        }

        // Should never reach here, but handle it
        Err(Error::Network {
            operation: "GET request".to_string(),
            message: "max retries exceeded".to_string(),
            source: last_error.map(|e| -> Source { Arc::new(e) }),
        })
    }

    async fn tagged<T: serde::de::DeserializeOwned>(
        &self,
        response: Response,
        owner: &str,
        repo: &str,
    ) -> Result<Tagged<T>> {
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = self.handle_response(response, owner, repo).await?;
        Ok(Tagged { body, etag })
    }

    async fn conditional<T: serde::de::DeserializeOwned>(
        &self,
        response: Response,
        owner: &str,
        repo: &str,
    ) -> Result<Conditional<T>> {
        if response.status() == StatusCode::NOT_MODIFIED {
            debug!(owner, repo, "not modified");
            return Ok(Conditional::NotModified);
        }
        Ok(Conditional::Modified(
            self.tagged(response, owner, repo).await?,
        ))
    }

    /// Handle response, checking for errors
    async fn handle_response<T: serde::de::DeserializeOwned>(
        &self,
        response: Response,
        owner: &str,
        repo: &str,
    ) -> Result<T> {
        let status = response.status();

        // Check rate limit headers
        if let Some(remaining) = response.headers().get("x-ratelimit-remaining")
            && let Ok(remaining_str) = remaining.to_str()
            && let Ok(remaining_num) = remaining_str.parse::<u32>()
            && remaining_num < 10
        {
            warn!(remaining = remaining_num, "GitHub API rate limit low");
        }

        match status {
            StatusCode::OK => response.json().await.map_err(|e| Error::Network {
                operation: "parse response".to_string(),
                message: String::new(),
                source: Some(Arc::new(e)),
            }),
            StatusCode::NOT_FOUND => Err(Error::NotFound {
                owner: owner.to_string(),
                repo: repo.to_string(),
                host: Some(self.host.clone()),
            }),
            StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => Err(Error::RateLimited {
                provider: "github".to_string(),
                retry_after: retry_after(response.headers(), std::time::SystemTime::now()),
            }),
            StatusCode::UNAUTHORIZED => Err(Error::Auth {
                provider: "github".to_string(),
                reason: "unauthorized - check your token".to_string(),
                source: None,
            }),
            _ => {
                let body = response.text().await.unwrap_or_default();
                Err(Error::Network {
                    operation: "API request".to_string(),
                    message: format!("status {status}: {body}"),
                    source: None,
                })
            }
        }
    }
}

fn retry_after(headers: &HeaderMap, now: std::time::SystemTime) -> Option<Duration> {
    let number = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    if let Some(secs) = number("retry-after") {
        return Some(Duration::from_secs(secs));
    }
    if number("x-ratelimit-remaining") != Some(0) {
        return None;
    }
    let reset = std::time::UNIX_EPOCH + Duration::from_secs(number("x-ratelimit-reset")?);
    Some(reset.duration_since(now).unwrap_or_default())
}

/// Returns `true` when `api_host` references the loopback interface.
///
/// Strips an optional `http(s)://` scheme and port, then exact-matches the
/// host. Covers IPv4 (`127.0.0.0/8`), IPv6 (`::1`), the `localhost` name, and
/// the reserved `.localhost` TLD (RFC 6761).
fn is_loopback_host(api_host: &str) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr};

    let after_scheme = api_host
        .strip_prefix("https://")
        .or_else(|| api_host.strip_prefix("http://"))
        .unwrap_or(api_host);

    let host_with_port = after_scheme.split('/').next().unwrap_or("");

    // Bracketed IPv6 (`[::1]:8080`) → take what's inside the brackets. Bare
    // IPv6 literals contain multiple colons, so don't try to strip a port.
    // Anything else (IPv4 or hostname) may have a single trailing `:<port>`.
    let host = match host_with_port.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None if host_with_port.matches(':').count() >= 2 => host_with_port,
        None => host_with_port
            .rsplit_once(':')
            .map_or(host_with_port, |(h, _)| h),
    };

    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if let Ok(ip) = host.parse::<Ipv6Addr>() {
        return ip.is_loopback();
    }
    host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use reqwest::header::{HeaderMap, HeaderValue};

    use super::{is_loopback_host, retry_after};

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_static(value));
        }
        map
    }

    #[test]
    fn retry_after_prefers_the_retry_after_header_then_an_exhausted_reset() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        assert_eq!(
            retry_after(
                &headers(&[("retry-after", "30"), ("x-ratelimit-remaining", "0")]),
                now
            ),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            retry_after(
                &headers(&[
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", "1720")
                ]),
                now
            ),
            Some(Duration::from_mins(12))
        );
        assert_eq!(
            retry_after(
                &headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "900")]),
                now
            ),
            Some(Duration::ZERO)
        );
        assert_eq!(
            retry_after(
                &headers(&[
                    ("x-ratelimit-remaining", "12"),
                    ("x-ratelimit-reset", "1720")
                ]),
                now
            ),
            None
        );
        assert_eq!(retry_after(&headers(&[]), now), None);
    }

    #[test]
    fn loopback_names_and_addresses() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.0.0.10"));
        assert!(is_loopback_host("127.255.255.254"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("api.localhost"));
    }

    #[test]
    fn loopback_with_scheme_and_port() {
        assert!(is_loopback_host("http://localhost"));
        assert!(is_loopback_host("https://localhost:8080"));
        assert!(is_loopback_host("http://127.0.0.1:3000"));
        assert!(is_loopback_host("http://[::1]:8080"));
        assert!(is_loopback_host("https://api.localhost/v3"));
    }

    #[test]
    fn non_loopback_substrings_are_rejected() {
        assert!(!is_loopback_host("not-localhost.example.com"));
        assert!(!is_loopback_host("mylocalhost.io"));
        assert!(!is_loopback_host("localhost.evil.com"));
        assert!(!is_loopback_host("1127.0.0.1.example.com"));
        assert!(!is_loopback_host("api.github.com"));
        assert!(!is_loopback_host("ghe.mycorp.internal"));
    }
}
