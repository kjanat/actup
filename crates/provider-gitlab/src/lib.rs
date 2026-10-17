//! GitLab provider for actup
//!
//! Implements the `Provider` trait for GitLab.com and self-hosted GitLab.
//! Handles REST API calls, authentication, rate limiting, and project path encoding.
//! Exports [`REGISTRATION`] for the provider registration API.
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
//!     "https://gitlab.example.com/api/v4",
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

use actup_core::{
    HostKind,
    config::registration::{Capabilities, Preset, ProviderConfig, Registration},
    provider::Provider,
};
pub use client::GitLabClient;
pub use provider::GitLabProvider;
pub use types::{
    GitLabBranch, GitLabBranchCommit, GitLabCommit, GitLabError, GitLabFile, GitLabProject,
    GitLabTag,
};

/// Default GitLab host
pub const GITLAB_HOST: &str = "gitlab.com";

/// API root of [`GITLAB_HOST`].
pub const GITLAB_API_BASE: &str = "https://gitlab.com/api/v4";

/// Environment variable that holds a GitLab token.
pub const GITLAB_TOKEN_ENV: &str = "GITLAB_TOKEN";

/// Provider registration for GitLab.com and self-hosted GitLab.
///
/// Declares the `gitlab.com` preset, infers `https://{host}/api/v4` for other
/// hosts, and supports the REST API only.
pub static REGISTRATION: Registration = Registration {
    kind: HostKind::GitLab,
    presets: &[Preset {
        host: GITLAB_HOST,
        api_base: GITLAB_API_BASE,
        token_env: GITLAB_TOKEN_ENV,
    }],
    default_token_env: Some(GITLAB_TOKEN_ENV),
    infer_api_base: infer_gitlab_api_base,
    capabilities: Capabilities {
        supports_graphql: false,
        supports_rest: true,
    },
    create: create_gitlab_provider,
};

fn infer_gitlab_api_base(hostname: &str) -> Option<String> {
    (hostname != GITLAB_HOST).then(|| format!("https://{hostname}/api/v4"))
}

fn create_gitlab_provider(
    ProviderConfig {
        host,
        api_base,
        token_env,
        max_tags_per_repo,
        ..
    }: ProviderConfig,
) -> actup_core::Result<Box<dyn Provider>> {
    let token = token_env.and_then(|var| std::env::var(var).ok());
    let provider =
        GitLabProvider::with_host(&api_base, &host, token.as_deref())?.max_tags(max_tags_per_repo);
    Ok(Box::new(provider))
}
