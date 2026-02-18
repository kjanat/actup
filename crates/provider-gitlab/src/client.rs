//! HTTP client for GitLab API
//!
//! All async methods in this module can return errors for network failures,
//! rate limiting, authentication issues, or resource not found.

#![allow(clippy::missing_errors_doc)] // All async methods return Result with obvious error conditions

use crate::types::{
    GitLabBranch, GitLabError, GitLabFile, GitLabProject, GitLabTag, GitLabTreeEntry,
};
use actup_core::{Error, Result};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Client, Response, StatusCode};
use std::time::Duration;
use tracing::{debug, warn};

/// Maximum items per page allowed by GitLab API
pub const MAX_PER_PAGE: u8 = 100;
/// Minimum items per page
pub const MIN_PER_PAGE: u8 = 1;
/// Default items per page
pub const DEFAULT_PER_PAGE: u8 = MAX_PER_PAGE;

/// Default max retry attempts for transient errors
pub const DEFAULT_MAX_RETRIES: u32 = 3;
/// Default base delay in milliseconds for exponential backoff
pub const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 100;

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
        Self::with_api_base("https://gitlab.com", "gitlab.com", token)
    }

    /// Create a client for a custom API endpoint (e.g., self-hosted GitLab)
    ///
    /// # Arguments
    ///
    /// * `api_base` - API base URL (e.g., "<https://gitlab.example.com>")
    /// * `host` - Display host name (e.g., "gitlab.example.com")
    /// * `token` - Optional personal access token or project access token
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be created or token is invalid.
    pub fn with_api_base(api_base: &str, host: &str, token: Option<&str>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("actup/0.1"));

        if let Some(t) = token {
            headers.insert(
                "PRIVATE-TOKEN",
                HeaderValue::from_str(t).map_err(|e| Error::Auth {
                    provider: "gitlab".to_string(),
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
            format!("{api_base}/api/v4")
        } else {
            format!("https://{api_base}/api/v4")
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

    /// List tags for a project (paginated, fetches all)
    ///
    /// # Arguments
    /// * `owner` - Project owner/namespace
    /// * `repo` - Project name
    /// * `per_page` - Items per page (clamped to 1..=100)
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or project not found.
    pub async fn list_tags(
        &self,
        owner: &str,
        repo: &str,
        per_page: Option<u8>,
    ) -> Result<Vec<GitLabTag>> {
        let per_page = per_page
            .unwrap_or(DEFAULT_PER_PAGE)
            .clamp(MIN_PER_PAGE, MAX_PER_PAGE);

        let project_id = Self::encode_project_path(owner, repo);
        let mut all_tags = Vec::new();
        let mut page = 1;

        loop {
            let url = format!(
                "{}/projects/{project_id}/repository/tags?per_page={per_page}&page={page}",
                self.api_base
            );
            debug!(url = %url, page = page, "fetching tags page");

            let response = self.get(&url).await?;

            // Check if there are more pages
            let has_next = response
                .headers()
                .get("x-next-page")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u32>().ok())
                .is_some();

            let tags: Vec<GitLabTag> = self.handle_response(response, owner, repo).await?;

            if tags.is_empty() {
                break;
            }

            all_tags.extend(tags);

            if !has_next {
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

    /// List directory contents
    ///
    /// # Errors
    ///
    /// Returns error on network failure, rate limit, auth failure, or path not found.
    pub async fn list_tree(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        ref_name: &str,
    ) -> Result<Vec<GitLabTreeEntry>> {
        let project_id = Self::encode_project_path(owner, repo);
        let ref_encoded = urlencoding::encode(ref_name);
        let url = format!(
            "{}/projects/{project_id}/repository/tree?path={}&ref={ref_encoded}",
            self.api_base,
            urlencoding::encode(path)
        );
        debug!(url = %url, path = %path, ref_name = %ref_name, "listing tree");

        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Resolve a ref (tag or branch) to a commit SHA
    ///
    /// GitLab doesn't have a direct "resolve ref" endpoint, so we:
    /// 1. Try to get it as a branch first
    /// 2. Fall back to checking tags
    ///
    /// # Errors
    ///
    /// Returns error if ref doesn't exist or network failure.
    pub async fn resolve_ref(&self, owner: &str, repo: &str, ref_name: &str) -> Result<String> {
        // Try as branch first
        if let Ok(branch) = self.get_branch(owner, repo, ref_name).await {
            return Ok(branch.commit.id);
        }

        // Try as tag
        let tags = self.list_tags(owner, repo, None).await?;
        for tag in tags {
            if tag.name == ref_name {
                return Ok(tag.target);
            }
        }

        Err(Error::RefNotFound {
            owner: owner.to_string(),
            repo: repo.to_string(),
            ref_name: ref_name.to_string(),
            host: Some(self.host.clone()),
        })
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
                })
            }
        }
    }
}
