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

use std::sync::Arc;

use actup_core::{
    ApiMode, HostKind,
    cache::Cache,
    config::registration::{Capabilities, Preset, ProviderConfig, Registration},
    provider::Provider,
};
pub use batch::{BatchFetcher, fetch_versions_with_registry};
pub use client::{DEFAULT_PER_PAGE, GitHubClient};
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
    kind: HostKind::GitHub,
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
    let provider = GitHubProvider::with_host_and_max_tags(
        &config.api_base,
        &config.host,
        token.as_deref(),
        config.max_tags_per_repo.get(),
    )?;
    Ok(Box::new(provider))
}

/// Construct a [`BatchFetcher`] from a fully-resolved GitHub [`ProviderConfig`].
///
/// # Errors
/// Returns error if the config is not for a GitHub host or the underlying
/// provider cannot be created.
pub fn create_batch_fetcher<C: Cache>(
    config: &ProviderConfig,
    cache: Arc<C>,
) -> actup_core::Result<BatchFetcher<C>> {
    let token = std::env::var(&config.token_env).ok();
    create_batch_fetcher_with_token(config, cache, token.as_deref())
}

fn create_batch_fetcher_with_token<C: Cache>(
    config: &ProviderConfig,
    cache: Arc<C>,
    token: Option<&str>,
) -> actup_core::Result<BatchFetcher<C>> {
    if config.kind != HostKind::GitHub {
        return Err(actup_core::Error::InvalidConfig {
            path: format!("host \"{}\"", config.host),
            reason: format!("expected GitHub host config, found {:?}", config.kind),
        });
    }

    let provider = GitHubProvider::with_host_and_max_tags(
        &config.api_base,
        &config.host,
        token,
        config.max_tags_per_repo.get(),
    )?;

    Ok(match config.api_mode {
        ApiMode::Graphql => {
            BatchFetcher::with_graphql(provider, cache, token, config.max_tags_per_repo.get())
        }
        ApiMode::Rest => BatchFetcher::rest_only(provider, cache, config.max_tags_per_repo.get()),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::{num::NonZeroU8, sync::Arc};

    use actup_core::{ApiMode, HostKind, cache, config::registration::ProviderConfig};

    use super::*;

    #[test]
    fn registration_kind_is_github() {
        assert_eq!(REGISTRATION.kind, HostKind::GitHub);
    }

    #[test]
    fn registration_has_one_preset() {
        assert_eq!(REGISTRATION.presets.len(), 1);
    }

    #[test]
    fn preset_host_is_github_com() {
        assert_eq!(REGISTRATION.presets[0].host, "github.com");
    }

    #[test]
    fn preset_api_base_is_public_api() {
        assert_eq!(REGISTRATION.presets[0].api_base, "https://api.github.com");
    }

    #[test]
    fn preset_token_env_is_github_token() {
        assert_eq!(REGISTRATION.presets[0].token_env, "GITHUB_TOKEN");
    }

    #[test]
    fn default_token_env_matches_preset() {
        assert_eq!(REGISTRATION.default_token_env, Some("GITHUB_TOKEN"));
    }

    #[test]
    fn capabilities_supports_graphql() {
        assert!(REGISTRATION.capabilities.supports_graphql);
    }

    #[test]
    fn capabilities_supports_rest() {
        assert!(REGISTRATION.capabilities.supports_rest);
    }

    #[test]
    fn infer_api_base_returns_none_for_github_com() {
        let result = (REGISTRATION.infer_api_base)("github.com");
        assert_eq!(result, None, "preset host should not need inference");
    }

    #[test]
    fn infer_api_base_returns_ghe_url() {
        let result = (REGISTRATION.infer_api_base)("ghe.corp.com");
        assert_eq!(result, Some("https://ghe.corp.com/api/v3".into()));
    }

    #[test]
    fn infer_api_base_ghe_with_subdomain() {
        let result = (REGISTRATION.infer_api_base)("github.internal.example.org");
        assert_eq!(
            result,
            Some("https://github.internal.example.org/api/v3".into())
        );
    }

    #[test]
    fn infer_api_base_ghe_short_hostname() {
        let result = (REGISTRATION.infer_api_base)("ghes");
        assert_eq!(result, Some("https://ghes/api/v3".into()));
    }

    #[test]
    fn create_produces_provider_with_valid_config() {
        // No GITHUB_TOKEN env needed — provider runs unauthenticated
        let config = ProviderConfig {
            host: "github.com".into(),
            kind: HostKind::GitHub,
            api_base: "https://api.github.com".into(),
            token_env: "_ACTUP_TEST_NONEXISTENT_TOKEN_VAR".into(),
            api_mode: ApiMode::Graphql,
            max_tags_per_repo: NonZeroU8::new(10).unwrap(),
        };
        let result = (REGISTRATION.create)(config);
        assert!(result.is_ok(), "create should succeed without token");
    }

    #[test]
    fn create_produces_provider_for_ghe() {
        let config = ProviderConfig {
            host: "ghe.corp.com".into(),
            kind: HostKind::GitHub,
            api_base: "https://ghe.corp.com/api/v3".into(),
            token_env: "_ACTUP_TEST_NONEXISTENT_TOKEN_VAR".into(),
            api_mode: ApiMode::Rest,
            max_tags_per_repo: NonZeroU8::new(20).unwrap(),
        };
        let result = (REGISTRATION.create)(config);
        assert!(result.is_ok(), "create should succeed for GHE host");
    }

    #[test]
    fn preset_values_match_module_constants() {
        assert_eq!(REGISTRATION.presets[0].host, GITHUB_HOST);
        assert_eq!(
            REGISTRATION.presets[0].api_base,
            format!("https://{GITHUB_API_HOST}")
        );
    }

    #[test]
    fn debug_output_includes_kind_and_presets() {
        let debug = format!("{REGISTRATION:?}");
        assert!(debug.contains("GitHub"));
        assert!(debug.contains("github.com"));
        assert!(debug.contains("GITHUB_TOKEN"));
        // fn pointers excluded by finish_non_exhaustive
        assert!(debug.contains(".."));
        assert!(!debug.contains("infer_api_base"));
        assert!(!debug.contains("create"));
    }

    #[test]
    fn create_batch_fetcher_uses_rest_mode_when_configured() {
        let config = ProviderConfig {
            host: "github.com".into(),
            kind: HostKind::GitHub,
            api_base: "https://api.github.com".into(),
            token_env: "_ACTUP_TEST_BATCH_FETCHER_REST".into(),
            api_mode: ApiMode::Rest,
            max_tags_per_repo: NonZeroU8::new(10).unwrap(),
        };

        let fetcher = create_batch_fetcher(&config, Arc::new(cache::Noop)).unwrap();

        assert!(!fetcher.has_graphql());
    }

    #[test]
    fn create_batch_fetcher_enables_graphql_when_token_present() {
        let config = ProviderConfig {
            host: "github.com".into(),
            kind: HostKind::GitHub,
            api_base: "https://api.github.com".into(),
            token_env: "_ACTUP_UNUSED_TEST_TOKEN".into(),
            api_mode: ApiMode::Graphql,
            max_tags_per_repo: NonZeroU8::new(10).unwrap(),
        };

        let fetcher =
            create_batch_fetcher_with_token(&config, Arc::new(cache::Noop), Some("test-token"))
                .unwrap();

        assert!(fetcher.has_graphql());
    }
}
