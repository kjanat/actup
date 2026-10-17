//! Provider registry and file cache construction for the LSP server.

use actup_core::{Config, Registration, Retention, provider, validate_no_duplicate_presets};

/// Returns all compiled-in provider registrations.
///
/// # Panics
///
/// Panics if two registrations share a preset hostname (programmer invariant).
#[must_use]
pub fn available_registrations() -> Vec<&'static Registration> {
    let registrations = vec![
        &actup_provider_github::REGISTRATION,
        &actup_provider_gitlab::REGISTRATION,
        &actup_provider_gitea::REGISTRATION,
    ];
    validate_no_duplicate_presets(&registrations);
    registrations
}

/// Build a [`provider::Registry`] from the given config and the compiled-in registrations.
///
/// Providers reuse an answer for `config.cache.ttl`.
pub fn build_registry(config: &Config) -> actup_core::Result<provider::Registry> {
    provider::Registry::from_config(
        config,
        &available_registrations(),
        &Retention::expiring(config.cache.ttl),
    )
}

/// How the backend builds its provider registry and file cache from a loaded config.
#[derive(Clone, Copy)]
pub struct Providers {
    /// Builds the provider registry.
    pub registry: fn(&Config) -> actup_core::Result<provider::Registry>,
    /// Builds the persistent version cache.
    pub file_cache: fn(&Config) -> crate::file_cache::FileCache,
}

impl Providers {
    /// The compiled-in providers and the on-disk cache.
    pub const LIVE: Self = Self {
        registry: build_registry,
        file_cache: build_file_cache,
    };
}

/// Build a [`FileCache`](crate::file_cache::FileCache) from the given config.
///
/// Respects `config.cache.enabled`, `config.cache.path`, and `config.cache.ttl`.
#[must_use]
pub fn build_file_cache(config: &Config) -> crate::file_cache::FileCache {
    if config.cache.enabled {
        config.cache.path.as_ref().map_or_else(
            || crate::file_cache::FileCache::new(config.cache.ttl),
            |path| crate::file_cache::FileCache::with_path(path.clone(), config.cache.ttl),
        )
    } else {
        crate::file_cache::FileCache::disabled()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashSet;

    use actup_core::{HostConfig, HostKind};

    use super::*;

    #[test]
    fn available_registrations_cover_every_provider() {
        let kinds: Vec<HostKind> = available_registrations().iter().map(|r| r.kind).collect();
        assert_eq!(kinds, [HostKind::GitHub, HostKind::GitLab, HostKind::Gitea]);
    }

    #[test]
    fn available_registrations_no_duplicate_presets() {
        let regs = available_registrations();
        let mut seen = HashSet::new();
        for reg in &regs {
            for preset in reg.presets {
                assert!(
                    seen.insert(preset.host),
                    "duplicate preset: {}",
                    preset.host
                );
            }
        }
    }

    #[test]
    fn build_registry_default_config_registers_github() {
        let config = Config::default();
        let registry = build_registry(&config).unwrap();
        assert_eq!(registry.default_host(), Some("github.com"));
        assert!(registry.find(Some("github.com")).is_some());
    }

    #[test]
    fn build_registry_ghe_host_registers_alongside_default() {
        let mut config = Config::default();
        config.hosts.insert(
            "ghe.corp.com".to_string(),
            HostConfig {
                kind: Some(HostKind::GitHub),
                api_base: None,
                token_env: None,
                api_mode: None,
                max_tags_per_repo: None,
            },
        );
        let registry = build_registry(&config).unwrap();
        assert!(registry.find(Some("github.com")).is_some());
        assert!(registry.find(Some("ghe.corp.com")).is_some());
    }

    #[test]
    fn build_registry_invalid_hostname_errors() {
        let mut config = Config::default();
        config.hosts.insert(
            "UPPERCASE.COM".to_string(),
            HostConfig {
                kind: Some(HostKind::GitHub),
                api_base: None,
                token_env: None,
                api_mode: None,
                max_tags_per_repo: None,
            },
        );
        assert!(build_registry(&config).is_err());
    }

    #[test]
    fn build_file_cache_respects_disabled() {
        use actup_core::cache::Cache;
        let mut config = Config::default();
        config.cache.enabled = false;
        let cache = build_file_cache(&config);
        assert!(!cache.is_enabled());
    }

    #[test]
    fn build_file_cache_default_is_enabled() {
        use actup_core::cache::Cache;
        let config = Config::default();
        let cache = build_file_cache(&config);
        assert!(cache.is_enabled());
    }

    #[test]
    fn live_providers_use_the_compiled_in_registrations_and_the_disk_cache() {
        use actup_core::cache::Cache;
        let config = Config::default();
        let Ok(registry) = (Providers::LIVE.registry)(&config) else {
            panic!("the default config builds a registry");
        };
        assert_eq!(registry.default_host(), Some("github.com"));
        assert!((Providers::LIVE.file_cache)(&config).is_enabled());
    }

    #[tokio::test]
    async fn diagnostics_resolve_a_gitlab_ref_through_the_gitlab_provider()
    -> Result<(), Box<dyn std::error::Error>> {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        let project = "/api/v4/projects/group%2Fproject";
        let (old, new) = ("a".repeat(40), "b".repeat(40));
        let tag = |name: &str, sha: &str| {
            format!(r#"{{"name": "{name}", "target": "{sha}", "commit": null, "release": null}}"#)
        };
        for (url, body) in [
            (
                format!("{project}/repository/tags"),
                format!("[{}, {}]", tag("v1.4.0", &new), tag("v1.0.0", &old)),
            ),
            (
                project.to_string(),
                r#"{"id": 1, "name": "project", "path": "project", "path_with_namespace": "group/project", "default_branch": "main", "description": null, "web_url": "w"}"#.to_string(),
            ),
            (
                format!("{project}/repository/branches/main"),
                format!(r#"{{"name": "main", "commit": {{"id": "{new}", "short_id": "bbbbbbb", "title": "t"}}, "default": true}}"#),
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(url))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
        }
        let mut config = Config::default();
        config.hosts.insert(
            "gitlab.com".to_string(),
            HostConfig {
                kind: None,
                api_base: Some(format!("{}/api/v4", server.uri())),
                token_env: Some("_ACTUP_TEST_UNSET_GITLAB_TOKEN".to_string()),
                api_mode: Some(actup_core::config::ApiMode::Rest),
                max_tags_per_repo: None,
            },
        );
        let registry = build_registry(&config)?;
        let uri =
            tower_lsp_server::ls_types::Uri::from_file_path("/test/ci.yml").ok_or("no uri")?;
        let text = "steps:\n  - uses: gitlab.com/group/project@v1.0.0\n";

        let result = crate::diagnostics::compute_diagnostics(
            text,
            &uri,
            &config,
            &registry,
            &crate::file_cache::FileCache::disabled(),
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        let messages: Vec<&str> = result
            .diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert!(
            messages.iter().any(|m| m.contains("v1.4.0")),
            "{messages:?}"
        );
        Ok(())
    }

    #[test]
    fn build_registry_multiple_hosts() {
        let mut config = Config::default();
        config.hosts.insert(
            "ghe1.corp.com".to_string(),
            HostConfig {
                kind: Some(HostKind::GitHub),
                api_base: None,
                token_env: None,
                api_mode: None,
                max_tags_per_repo: None,
            },
        );
        config.hosts.insert(
            "ghe2.corp.com".to_string(),
            HostConfig {
                kind: Some(HostKind::GitHub),
                api_base: None,
                token_env: None,
                api_mode: None,
                max_tags_per_repo: None,
            },
        );

        let registry = build_registry(&config).unwrap();
        assert!(registry.find(Some("github.com")).is_some());
        assert!(registry.find(Some("ghe1.corp.com")).is_some());
        assert!(registry.find(Some("ghe2.corp.com")).is_some());
    }
}
