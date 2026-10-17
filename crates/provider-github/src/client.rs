//! HTTP client for GitHub API
//!
//! All async methods in this module can return errors for network failures,
//! rate limiting, authentication issues, or resource not found.

#![allow(clippy::missing_errors_doc)] // All async methods return Result with obvious error conditions

use std::{sync::Arc, time::Duration};

use actup_core::{Error, Result, error::Source};
use actup_providers::{
    DEFAULT_MAX_RETRIES, DEFAULT_RETRY_BASE_DELAY_MS, MAX_PER_PAGE, MIN_PER_PAGE,
};
use reqwest::{
    Client, Response, StatusCode,
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT},
};
use tracing::{debug, warn};

use crate::types::{
    GitHubBranch, GitHubCommit, GitHubContent, GitHubRef, GitHubRepo, GitHubTag, GitHubTagObject,
};

/// Default items per page
pub const DEFAULT_PER_PAGE: u8 = MAX_PER_PAGE;

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
            "{}/repos/{owner}/{repo}/tags?per_page={per_page}",
            self.api_base
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
        max_tags: u8,
    ) -> Result<Vec<GitHubTag>> {
        let max_tags = max_tags.max(MIN_PER_PAGE);
        let per_page = max_tags.min(MAX_PER_PAGE);

        let mut all_tags = Vec::with_capacity(usize::from(max_tags));
        let mut url = Some(format!(
            "{}/repos/{owner}/{repo}/tags?per_page={per_page}",
            self.api_base
        ));

        while let Some(current_url) = url {
            if all_tags.len() >= usize::from(max_tags) {
                break;
            }

            debug!(url = %current_url, max_tags, "fetching limited tags page");

            let response = self.get(&current_url).await?;
            url = Self::parse_next_link(&response);

            let tags: Vec<GitHubTag> = self.handle_response(response, owner, repo).await?;
            let remaining = usize::from(max_tags).saturating_sub(all_tags.len());
            all_tags.extend(tags.into_iter().take(remaining));

            if all_tags.len() >= usize::from(max_tags) {
                break;
            }
        }

        Ok(all_tags)
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
        let url = format!("{}/repos/{owner}/{repo}", self.api_base);
        debug!(url = %url, "fetching repo info");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Get a specific git ref
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or ref not found.
    pub async fn get_ref(&self, owner: &str, repo: &str, ref_name: &str) -> Result<GitHubRef> {
        // GitHub API wants refs without the leading "refs/"
        let ref_path = ref_name.strip_prefix("refs/").unwrap_or(ref_name);

        let url = format!("{}/repos/{owner}/{repo}/git/ref/{ref_path}", self.api_base);
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
        let mut url = format!("{}/repos/{owner}/{repo}/contents/{path}", self.api_base);

        if let Some(r) = ref_name {
            use std::fmt::Write;
            let _ = write!(url, "?ref={r}"); // infallible for String
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
        let url = format!("{}/repos/{owner}/{repo}/branches/{branch}", self.api_base);
        debug!(url = %url, "fetching branch");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Get a commit by full or abbreviated SHA
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or commit not found.
    /// An unknown commit in an existing repository is [`Error::RefNotFound`].
    pub async fn get_commit(&self, owner: &str, repo: &str, sha: &str) -> Result<GitHubCommit> {
        let url = format!("{}/repos/{owner}/{repo}/commits/{sha}", self.api_base);
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
        let url = format!("{}/repos/{owner}/{repo}/git/tags/{sha}", self.api_base);
        debug!(url = %url, "fetching tag object");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Make a GET request with retry and exponential backoff
    async fn get(&self, url: &str) -> Result<Response> {
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

            match self.client.get(url).send().await {
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
            StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
                // Check for rate limit
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(Duration::from_secs);

                Err(Error::RateLimited {
                    provider: "github".to_string(),
                    retry_after,
                })
            }
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
    use super::is_loopback_host;

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
