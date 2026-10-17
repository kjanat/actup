//! HTTP client for Gitea API
//!
//! All async methods in this module can return errors for network failures,
//! rate limiting, authentication issues, or resource not found.

#![allow(clippy::missing_errors_doc)] // All async methods return Result with obvious error conditions

use std::{sync::Arc, time::Duration};

use actup_core::{Error, Result};
use actup_providers::{
    DEFAULT_MAX_RETRIES, DEFAULT_RETRY_BASE_DELAY_MS, MAX_PER_PAGE, MIN_PER_PAGE,
};
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, HeaderValue, USER_AGENT},
};
use tracing::{debug, warn};

use crate::types::{
    GiteaArchive, GiteaBranch, GiteaCommit, GiteaContent, GiteaError, GiteaRepository, GiteaTag,
};

/// Default items per page
pub const DEFAULT_PER_PAGE: u8 = 50; // Gitea recommends 50

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
    /// * `api_base` - API root URL (e.g., "<https://codeberg.org/api/v1>")
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
                    reason: "invalid token format".into(),
                    source: Some(Arc::new(e)),
                })?,
            );
        }

        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Network {
                operation: "create HTTP client".to_string(),
                message: String::new(),
                source: Some(Arc::new(e)),
            })?;

        let api_base = api_base.trim_end_matches('/');
        let api_base_url = if api_base.starts_with("http") {
            api_base.to_string()
        } else {
            format!("https://{api_base}")
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

    /// Get the archive flag of a repository
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or repo not found.
    pub async fn get_repo_archive(&self, owner: &str, repo: &str) -> Result<GiteaArchive> {
        let url = format!("{}/repos/{owner}/{repo}", self.api_base);
        debug!(url = %url, "fetching repo archive flag");

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

    /// Get a single tag by name
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or tag not found.
    pub async fn get_tag(&self, owner: &str, repo: &str, tag: &str) -> Result<GiteaTag> {
        let url = format!("{}/repos/{owner}/{repo}/tags/{tag}", self.api_base);
        debug!(url = %url, tag = %tag, "fetching tag info");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Resolve a tag, branch, or short commit SHA to the commit SHA it names
    ///
    /// Tries the tag first, then the branch, then a commit lookup when the
    /// ref is a short SHA. A tag resolves to its commit, never to an
    /// annotated tag object.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RefNotFound`] when no tag, branch, or commit matches,
    /// and auth, rate-limit, or network errors unchanged.
    pub async fn resolve_ref(&self, owner: &str, repo: &str, ref_name: &str) -> Result<String> {
        match self.get_tag(owner, repo, ref_name).await {
            Ok(tag) => return Ok(tag.commit.sha),
            Err(Error::NotFound { .. }) => {}
            Err(e) => return Err(e),
        }

        match self.get_branch(owner, repo, ref_name).await {
            Ok(branch) => return Ok(branch.commit.id),
            Err(Error::NotFound { .. }) => {}
            Err(e) => return Err(e),
        }

        let not_found = || Error::RefNotFound {
            owner: owner.to_string(),
            repo: repo.to_string(),
            ref_name: ref_name.to_string(),
            host: Some(self.host.clone()),
        };
        if !actup_core::version::is_short_sha(ref_name) {
            return Err(not_found());
        }
        match self.get_commit(owner, repo, ref_name).await {
            Ok(commit) => Ok(commit.sha),
            Err(Error::NotFound { .. }) => Err(not_found()),
            Err(e) => Err(e),
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
                        message: String::new(),
                        source: Some(Arc::new(e)),
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
                    message: String::new(),
                    source: Some(Arc::new(e)),
                })?;

                serde_json::from_str(&text).map_err(|e| Error::Internal {
                    context: "parse response".to_string(),
                    message: "JSON parse error".into(),
                    source: Some(Arc::new(e)),
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
                    source: None,
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
                    source: None,
                })
            }
        }
    }
}
