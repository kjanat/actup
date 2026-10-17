//! Gitea provider for actup
//!
//! Implements the `Provider` trait for Gitea and Forgejo instances.
//! Handles REST API calls, authentication, rate limiting, and pagination.
//! Exports [`REGISTRATION`] for the provider registration API.
//!
//! # Features
//!
//! - Full support for Gitea and Forgejo (soft fork of Gitea)
//! - API token authentication
//! - Automatic pagination for tags and other resources
//! - Base64 decoding for file contents
//! - Compatible with Gitea API v1
//!
//! # Examples
//!
//! ```no_run
//! use actup_provider_gitea::GiteaProvider;
//! use actup_core::provider::Provider;
//!
//! # async fn example() -> actup_core::Result<()> {
//! // Create provider for a Gitea instance
//! let provider = GiteaProvider::new(
//!     "https://gitea.com/api/v1",
//!     "gitea.com",
//!     Some("your-token")
//! )?;
//!
//! // Works with Forgejo too (same API)
//! let provider = GiteaProvider::new(
//!     "https://forgejo.example.com/api/v1",
//!     "forgejo.example.com",
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
pub use client::GiteaClient;
pub use provider::GiteaProvider;
pub use types::*;

/// Codeberg, a public Forgejo instance.
pub const CODEBERG_HOST: &str = "codeberg.org";

/// API root of [`CODEBERG_HOST`].
pub const CODEBERG_API_BASE: &str = "https://codeberg.org/api/v1";

/// Environment variable that holds a Codeberg token.
pub const CODEBERG_TOKEN_ENV: &str = "CODEBERG_TOKEN";

/// Environment variable that holds a token for any other Gitea or Forgejo host.
pub const GITEA_TOKEN_ENV: &str = "GITEA_TOKEN";

/// Provider registration for Gitea and Forgejo.
///
/// Declares the `codeberg.org` preset, infers `https://{host}/api/v1` for
/// other hosts, and supports the REST API only.
pub static REGISTRATION: Registration = Registration {
    kind: HostKind::Gitea,
    presets: &[Preset {
        host: CODEBERG_HOST,
        api_base: CODEBERG_API_BASE,
        token_env: CODEBERG_TOKEN_ENV,
    }],
    default_token_env: Some(GITEA_TOKEN_ENV),
    infer_api_base: infer_gitea_api_base,
    capabilities: Capabilities {
        supports_graphql: false,
        supports_rest: true,
    },
    create: create_gitea_provider,
};

fn infer_gitea_api_base(hostname: &str) -> Option<String> {
    (hostname != CODEBERG_HOST).then(|| format!("https://{hostname}/api/v1"))
}

fn create_gitea_provider(
    ProviderConfig {
        host,
        api_base,
        token_env,
        max_tags_per_repo,
        ..
    }: &ProviderConfig,
) -> actup_core::Result<Box<dyn Provider>> {
    let token = token_env.as_deref().and_then(|var| std::env::var(var).ok());
    let provider =
        GiteaProvider::new(api_base, host, token.as_deref())?.max_tags(*max_tags_per_repo);
    Ok(Box::new(provider))
}
