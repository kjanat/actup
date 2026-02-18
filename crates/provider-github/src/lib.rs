//! GitHub provider for actup
//!
//! Implements the `Provider` trait for GitHub.com and GitHub Enterprise.
//! Handles REST API calls, `ETag` caching, rate limiting, and authentication.
//!
//! Exports [`REGISTRATION`] for the provider registration API —
//! core uses it to resolve presets, infer `api_base`, and construct
//! provider instances without hardcoding GitHub knowledge.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod batch;
mod client;
mod graphql;
mod provider;
mod types;

use actup_core::HostKind;
use actup_core::config::registration::{Capabilities, Preset, ProviderConfig, Registration};
use actup_core::provider::Provider;

pub use batch::BatchFetcher;
pub use client::{
    DEFAULT_MAX_RETRIES, DEFAULT_PER_PAGE, DEFAULT_RETRY_BASE_DELAY_MS, GitHubClient, MAX_PER_PAGE,
    MIN_PER_PAGE,
};
pub use graphql::{DEFAULT_MAX_TAGS_PER_REPO, GITHUB_GRAPHQL_URL, GraphQLClient};
pub use provider::GitHubProvider;
pub use types::*;

/// Default GitHub API host
pub const GITHUB_API_HOST: &str = "api.github.com";

/// Default GitHub host
pub const GITHUB_HOST: &str = "github.com";

/// Provider registration for GitHub (and GitHub Enterprise).
///
/// Declares the `github.com` preset with default API base and token env,
/// infers `api_base` for GHE hosts as `https://{host}/api/v3`, and
/// supports both GraphQL and REST modes.
pub static REGISTRATION: Registration = Registration {
    kind: HostKind::Github,
    presets: &[Preset {
        host: GITHUB_HOST,
        api_base: "https://api.github.com",
        token_env: "GITHUB_TOKEN",
    }],
    default_token_env: Some("GITHUB_TOKEN"),
    infer_api_base: infer_github_api_base,
    capabilities: Capabilities {
        supports_graphql: true,
        supports_rest: true,
    },
    create: create_github_provider,
};

/// Infer `api_base` for a GitHub-kind host.
///
/// Preset hosts (github.com) return `None` — the preset provides `api_base`.
/// Non-preset hosts (GHE) get `https://{host}/api/v3`.
fn infer_github_api_base(hostname: &str) -> Option<String> {
    if hostname == GITHUB_HOST {
        None
    } else {
        Some(format!("https://{hostname}/api/v3"))
    }
}

/// Construct a [`GitHubProvider`] from fully-resolved [`ProviderConfig`].
///
/// Reads the token from the environment variable named by
/// `config.token_env`. Missing env var is not an error — the provider
/// runs unauthenticated (rate limits apply).
///
/// Signature must match [`Registration::create`] fn pointer type.
#[allow(clippy::needless_pass_by_value)]
fn create_github_provider(config: ProviderConfig) -> actup_core::Result<Box<dyn Provider>> {
    let token = std::env::var(&config.token_env).ok();
    let provider = GitHubProvider::with_host(&config.api_base, &config.host, token.as_deref())?;
    Ok(Box::new(provider))
}
