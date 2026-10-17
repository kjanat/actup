//! HTTP client for GitLab API
//!
//! All async methods in this module can return errors for network failures,
//! rate limiting, authentication issues, or resource not found.

#![allow(clippy::missing_errors_doc)] // All async methods return Result with obvious error conditions

use std::{num::NonZeroU8, sync::Arc, time::Duration};

use actup_core::{Error, Result};
use actup_providers::{DEFAULT_MAX_RETRIES, DEFAULT_RETRY_BASE_DELAY_MS, MAX_PER_PAGE};
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, HeaderValue, USER_AGENT},
};
use tracing::{debug, warn};

use crate::types::{GitLabBranch, GitLabCommit, GitLabError, GitLabFile, GitLabProject, GitLabTag};

/// HTTP client configured for GitLab API
#[derive(Debug, Clone)]
pub struct GitLabClient {
    client: Client,
    api_base: String,
    host: String,
    /// Max retry attempts for transient errors
    max_retries: u32,
    /// Base delay in ms for exponential backoff
    retry_base_delay_ms: u64,
}

impl GitLabClient {
    /// Create a new client for gitlab.com
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be created.
    pub fn new(token: Option<&str>) -> Result<Self> {
        Self::with_api_base(crate::GITLAB_API_BASE, crate::GITLAB_HOST, token)
    }

    /// Create a client for a custom API endpoint (e.g., self-hosted GitLab)
    ///
    /// # Arguments
    ///
    /// * `api_base` - API root URL (e.g., "<https://gitlab.example.com/api/v4>")
    /// * `host` - Display host name (e.g., "gitlab.example.com")
    /// * `token` - Optional personal access token or project access token
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be created or token is invalid.
    pub fn with_api_base(api_base: &str, host: &str, token: Option<&str>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!("actup/", env!("CARGO_PKG_VERSION"))),
        );
        headers.insert("Accept", HeaderValue::from_static("application/json"));

        if let Some(t) = token {
            headers.insert(
                "PRIVATE-TOKEN",
                HeaderValue::from_str(t).map_err(|e| Error::Auth {
                    provider: "gitlab".to_string(),
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

        if api_base.trim().is_empty() {
            return Err(Error::Internal {
                context: "validate API configuration".to_string(),
                message: "api_base must not be empty".to_string(),
                source: None,
            });
        }

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

    /// URL-encode a project path (owner/repo)
    fn encode_project_path(owner: &str, repo: &str) -> String {
        let path = format!("{owner}/{repo}");
        urlencoding::encode(&path).to_string()
    }

    /// List the first `max_tags` tags of a project, newest first.
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or project not found.
    pub async fn list_tags(
        &self,
        owner: &str,
        repo: &str,
        max_tags: NonZeroU8,
    ) -> Result<Vec<GitLabTag>> {
        let max_tags = usize::from(max_tags.get());
        let per_page = max_tags.min(usize::from(MAX_PER_PAGE));

        let project_id = Self::encode_project_path(owner, repo);
        let mut all_tags = Vec::with_capacity(max_tags);
        let mut page = 1;

        loop {
            let url = format!(
                "{}/projects/{project_id}/repository/tags?per_page={per_page}&page={page}",
                self.api_base
            );
            debug!(url = %url, page = page, "fetching tags page");

            let response = self.get(&url).await?;

            let has_next = response
                .headers()
                .get("x-next-page")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u32>().ok())
                .is_some();

            let tags: Vec<GitLabTag> = self.handle_response(response, owner, repo).await?;
            let remaining = max_tags - all_tags.len();
            let is_empty = tags.is_empty();
            all_tags.extend(tags.into_iter().take(remaining));

            if is_empty || !has_next || all_tags.len() >= max_tags {
                break;
            }

            page += 1;
        }

        Ok(all_tags)
    }

    /// Get project info
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or project not found.
    pub async fn get_project(&self, owner: &str, repo: &str) -> Result<GitLabProject> {
        let project_id = Self::encode_project_path(owner, repo);
        let url = format!("{}/projects/{project_id}", self.api_base);
        debug!(url = %url, "fetching project info");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Get branch info
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or branch not found.
    pub async fn get_branch(&self, owner: &str, repo: &str, branch: &str) -> Result<GitLabBranch> {
        let project_id = Self::encode_project_path(owner, repo);
        let branch_encoded = urlencoding::encode(branch);
        let url = format!(
            "{}/projects/{project_id}/repository/branches/{branch_encoded}",
            self.api_base
        );
        debug!(url = %url, branch = %branch, "fetching branch info");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Get a single tag by name
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or tag not found.
    pub async fn get_tag(&self, owner: &str, repo: &str, tag: &str) -> Result<GitLabTag> {
        let project_id = Self::encode_project_path(owner, repo);
        let tag_encoded = urlencoding::encode(tag);
        let url = format!(
            "{}/projects/{project_id}/repository/tags/{tag_encoded}",
            self.api_base
        );
        debug!(url = %url, tag = %tag, "fetching tag info");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Get file content from repository
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or file not found.
    pub async fn get_file(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        ref_name: &str,
    ) -> Result<GitLabFile> {
        let project_id = Self::encode_project_path(owner, repo);
        let path_encoded = urlencoding::encode(path);
        let ref_encoded = urlencoding::encode(ref_name);
        let url = format!(
            "{}/projects/{project_id}/repository/files/{path_encoded}?ref={ref_encoded}",
            self.api_base
        );
        debug!(url = %url, path = %path, ref_name = %ref_name, "fetching file");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Get a commit by full or abbreviated SHA
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or commit not found.
    pub async fn get_commit(&self, owner: &str, repo: &str, sha: &str) -> Result<GitLabCommit> {
        let project_id = Self::encode_project_path(owner, repo);
        let url = format!(
            "{}/projects/{project_id}/repository/commits/{}",
            self.api_base,
            urlencoding::encode(sha)
        );
        debug!(url = %url, sha = %sha, "fetching commit");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Resolve a ref (branch, tag, or short commit SHA) to a commit SHA
    ///
    /// GitLab doesn't expose a single resolve-ref endpoint, so we:
    /// 1. Try to get it as a branch first
    /// 2. Try a direct tag lookup
    /// 3. Try a commit lookup when the ref is a short SHA
    ///
    /// # Errors
    ///
    /// Returns error if ref doesn't exist or network failure.
    pub async fn resolve_ref(&self, owner: &str, repo: &str, ref_name: &str) -> Result<String> {
        // Try as branch first (propagate non-NotFound errors)
        match self.get_branch(owner, repo, ref_name).await {
            Ok(branch) => return Ok(branch.commit.id),
            Err(Error::NotFound { .. }) => {}
            Err(e) => return Err(e),
        }

        // Try direct tag lookup
        match self.get_tag(owner, repo, ref_name).await {
            Ok(tag) => return Ok(actup_providers::TagLike::sha(&tag).to_string()),
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
            Ok(commit) => Ok(commit.id),
            Err(Error::NotFound { .. }) => Err(not_found()),
            Err(e) => Err(e),
        }
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
                        serde_json::from_str::<GitLabError>(&text).map_or_else(
                            |_| status.to_string(),
                            |err| {
                                err.message
                                    .or(err.error)
                                    .or(err.error_description)
                                    .unwrap_or_else(|| status.to_string())
                            },
                        )
                    },
                );

                Err(Error::Auth {
                    provider: "gitlab".to_string(),
                    reason,
                    source: None,
                })
            }
            StatusCode::TOO_MANY_REQUESTS => Err(Error::RateLimited {
                provider: "gitlab".to_string(),
                retry_after: response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(Duration::from_secs),
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
