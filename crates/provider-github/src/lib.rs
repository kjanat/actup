//! GitHub provider for actup
//!
//! Implements the `Provider` trait for GitHub.com and GitHub Enterprise.
//! Handles REST API calls, `ETag` caching, rate limiting, and authentication.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod client;
mod provider;
mod types;

pub use client::{
    DEFAULT_MAX_RETRIES, DEFAULT_PER_PAGE, DEFAULT_RETRY_BASE_DELAY_MS, GitHubClient, MAX_PER_PAGE,
    MIN_PER_PAGE,
};
pub use provider::GitHubProvider;
pub use types::*;

/// Default GitHub API host
pub const GITHUB_API_HOST: &str = "api.github.com";

/// Default GitHub host
pub const GITHUB_HOST: &str = "github.com";
