//! HTTP client for Gitea API
//!
//! All async methods in this module can return errors for network failures,
//! rate limiting, authentication issues, or resource not found.

#![allow(clippy::missing_errors_doc)] // All async methods return Result with obvious error conditions

use std::time::Duration;

use actup_core::{Error, Result};
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, HeaderValue, USER_AGENT},
};
use tracing::{debug, warn};

use crate::types::{
    GiteaBranch, GiteaCommit, GiteaContent, GiteaError, GiteaRef, GiteaRepository, GiteaTag,
};

/// Maximum items per page allowed by Gitea API
pub const MAX_PER_PAGE: u8 = 100;
/// Minimum items per page
pub const MIN_PER_PAGE: u8 = 1;
/// Default items per page
pub const DEFAULT_PER_PAGE: u8 = 50; // Gitea recommends 50

/// Default max retry attempts for transient errors
pub const DEFAULT_MAX_RETRIES: u32 = 3;
/// Default base delay in milliseconds for exponential backoff
pub const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 100;

/// HTTP client configured for Gitea API
#[derive(Debug, Clone)]
pub struct GiteaClient {
    client: Client,
    api_base: String,
    host: String,
    /// Max retry attempts for transient errors
    max_retries: u32,
    /// Base delay in ms for exponential backoff
    retry_base_delay_ms: u64,
}

impl GiteaClient {
    /// Create a new client for a Gitea instance
    ///
    /// # Arguments
    ///
    /// * `api_base` - API base URL (e.g., "<https://gitea.com>" or "<https://git.example.com>")
    /// * `host` - Display host name (e.g., "gitea.com")
    /// * `token` - Optional API token
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be created or token is invalid.
    pub fn new(api_base: &str, host: &str, token: Option<&str>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!("actup/", env!("CARGO_PKG_VERSION"))),
        );
        headers.insert("Accept", HeaderValue::from_static("application/json"));

        if let Some(t) = token {
            let auth_value = format!("token {t}");
            headers.insert(
                "Authorization",
                HeaderValue::from_str(&auth_value).map_err(|e| Error::Auth {
                    provider: "gitea".to_string(),
                    reason: format!("invalid token format: {e}"),
                })?,
            );
        }

        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Network {
                operation: "create HTTP client".to_string(),
                message: e.to_string(),
            })?;

        let api_base_url = if api_base.starts_with("http") {
            format!("{api_base}/api/v1")
        } else {
            format!("https://{api_base}/api/v1")
        };

        Ok(Self {
            client,
            api_base: api_base_url,
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
    ) -> Result<Vec<GiteaTag>> {
        let per_page = per_page
            .unwrap_or(DEFAULT_PER_PAGE)
            .clamp(MIN_PER_PAGE, MAX_PER_PAGE);

        let mut all_tags = Vec::new();
        let mut page = 1;

        loop {
            let url = format!(
                "{}/repos/{owner}/{repo}/tags?page={page}&limit={per_page}",
                self.api_base
            );
            debug!(url = %url, page = page, "fetching tags page");

            let response = self.get(&url).await?;
            let tags: Vec<GiteaTag> = self.handle_response(response, owner, repo).await?;

            if tags.is_empty() {
                break;
            }

            // Check if this is the last page before extending
            let is_last_page = tags.len() < per_page as usize;
            all_tags.extend(tags);

            if is_last_page {
                break;
            }

            page += 1;
        }

        Ok(all_tags)
    }

    /// Get repository info
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or repo not found.
    pub async fn get_repo(&self, owner: &str, repo: &str) -> Result<GiteaRepository> {
        let url = format!("{}/repos/{owner}/{repo}", self.api_base);
        debug!(url = %url, "fetching repo info");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Get branch info
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or branch not found.
    pub async fn get_branch(&self, owner: &str, repo: &str, branch: &str) -> Result<GiteaBranch> {
        let url = format!("{}/repos/{owner}/{repo}/branches/{branch}", self.api_base);
        debug!(url = %url, branch = %branch, "fetching branch info");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Get file content from repository
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or file not found.
    pub async fn get_contents(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        ref_name: &str,
    ) -> Result<GiteaContent> {
        let url = format!(
            "{}/repos/{owner}/{repo}/contents/{path}?ref={ref_name}",
            self.api_base
        );
        debug!(url = %url, path = %path, ref_name = %ref_name, "fetching file");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Get a git ref (tag or branch)
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or ref not found.
    pub async fn get_git_ref(&self, owner: &str, repo: &str, ref_name: &str) -> Result<GiteaRef> {
        // Gitea expects full ref path (refs/heads/main or refs/tags/v1.0.0)
        let ref_path = if ref_name.starts_with("refs/") {
            ref_name.to_string()
        } else {
            // Try as tag first, then branch
            format!("refs/tags/{ref_name}")
        };

        let url = format!("{}/repos/{owner}/{repo}/git/refs/{ref_path}", self.api_base);
        debug!(url = %url, ref_name = %ref_name, "fetching git ref");

        let response = self.get(&url).await?;
        let result: Result<GiteaRef> = self.handle_response(response, owner, repo).await;

        match result {
            Ok(git_ref) => Ok(git_ref),
            Err(Error::NotFound { .. }) if !ref_path.starts_with("refs/heads/") => {
                // Tag not found, try as branch
                let branch_path = format!("refs/heads/{ref_name}");
                let url = format!(
                    "{}/repos/{owner}/{repo}/git/refs/{branch_path}",
                    self.api_base
                );
                debug!(url = %url, ref_name = %ref_name, "tag not found, trying as branch");
                let response = self.get(&url).await?;
                self.handle_response(response, owner, repo).await
            }
            Err(e) => Err(e), // Auth, rate-limit, network errors propagate unchanged
        }
    }

    /// Get a commit by SHA
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or commit not found.
    pub async fn get_commit(&self, owner: &str, repo: &str, sha: &str) -> Result<GiteaCommit> {
        let url = format!("{}/repos/{owner}/{repo}/git/commits/{sha}", self.api_base);
        debug!(url = %url, sha = %sha, "fetching commit");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Execute a GET request with retry logic
    async fn get(&self, url: &str) -> Result<Response> {
        let mut retries = 0;

        loop {
            match self.client.get(url).send().await {
                Ok(resp) => return Ok(resp),
                Err(e) if retries < self.max_retries && e.is_timeout() => {
                    retries += 1;
                    // Use saturating operations to avoid overflow on large retry counts
                    let exp = retries.saturating_sub(1);
                    let multiplier = 1u64.checked_shl(exp).unwrap_or(u64::MAX);
                    let delay_ms = self.retry_base_delay_ms.saturating_mul(multiplier);
                    let delay = Duration::from_millis(delay_ms);
                    warn!(
                        error = %e,
                        retry = retries,
                        max_retries = self.max_retries,
                        delay_ms = delay.as_millis(),
                        "request timeout, retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(e) => {
                    return Err(Error::Network {
                        operation: format!("GET {url}"),
                        message: e.to_string(),
                    });
                }
            }
        }
    }

    /// Handle response, converting errors to actup-core Error types
    async fn handle_response<T: serde::de::DeserializeOwned>(
        &self,
        response: Response,
        owner: &str,
        repo: &str,
    ) -> Result<T> {
        let status = response.status();
        let url = response.url().to_string();

        match status {
            StatusCode::OK => {
                let text = response.text().await.map_err(|e| Error::Network {
                    operation: format!("read response from {url}"),
                    message: e.to_string(),
                })?;

                serde_json::from_str(&text).map_err(|e| Error::Internal {
                    context: "parse response".to_string(),
                    message: format!("JSON parse error: {e}"),
                })
            }
            StatusCode::NOT_FOUND => Err(Error::NotFound {
                owner: owner.to_string(),
                repo: repo.to_string(),
                host: Some(self.host.clone()),
            }),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                let error_text = response.text().await.ok();
                let reason = error_text.map_or_else(
                    || status.to_string(),
                    |text| {
                        serde_json::from_str::<GiteaError>(&text).map_or_else(
                            |_| status.to_string(),
                            |err| err.message.unwrap_or_else(|| status.to_string()),
                        )
                    },
                );

                Err(Error::Auth {
                    provider: "gitea".to_string(),
                    reason,
                })
            }
            // Gitea doesn't provide GitHub-style rate-limit headers
            StatusCode::TOO_MANY_REQUESTS => Err(Error::RateLimited {
                provider: "gitea".to_string(),
                retry_after: None,
            }),
            _ => {
                let error_text = response.text().await.ok();
                Err(Error::Network {
                    operation: format!("GET {url}"),
                    message: format!("HTTP {status}: {}", error_text.unwrap_or_default()),
                })
            }
        }
    }
}
