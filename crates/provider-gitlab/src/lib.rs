//! GitLab provider for actup
//!
//! Implements the `Provider` trait for GitLab.com and self-hosted GitLab.
//! Handles REST API calls, authentication, rate limiting, and project path encoding.
//!
//! # Features
//!
//! - Full support for GitLab.com and self-hosted GitLab instances
//! - Personal access token and project access token authentication
//! - Automatic pagination for tags and other resources
//! - Base64 decoding for file contents
//! - URL encoding for project paths with namespaces
//!
//! # Examples
//!
//! ```no_run
//! use actup_provider_gitlab::GitLabProvider;
//! use actup_core::provider::Provider;
//!
//! # async fn example() -> actup_core::Result<()> {
//! // Create provider for gitlab.com
//! let provider = GitLabProvider::new(Some("your-token"))?;
//!
//! // Create provider for self-hosted GitLab
//! let provider = GitLabProvider::with_host(
//!     "https://gitlab.example.com",
//!     "gitlab.example.com",
//!     Some("your-token")
//! )?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod client;
mod provider;
mod types;

pub use client::{
    DEFAULT_MAX_RETRIES, DEFAULT_PER_PAGE, DEFAULT_RETRY_BASE_DELAY_MS, GitLabClient, MAX_PER_PAGE,
    MIN_PER_PAGE,
};
pub use provider::GitLabProvider;
pub use types::{
    GitLabBranch, GitLabBranchCommit, GitLabCommit, GitLabError, GitLabFile, GitLabProject,
    GitLabRelease, GitLabTag, GitLabTreeEntry,
};

/// Default GitLab host
pub const GITLAB_HOST: &str = "gitlab.com";
