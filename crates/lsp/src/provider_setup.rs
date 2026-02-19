//! Provider registry construction for the LSP server.
//!
//! Mirrors the CLI's `build_registry` logic: collects compiled-in provider
//! registrations, resolves the default host, and registers providers for
//! each configured hostname.

use std::collections::HashSet;

use actup_core::{
    Config, Registration, effective_host, provider, resolve_default_host,
    validate_no_duplicate_presets,
};

/// Returns all compiled-in provider registrations.
///
/// Currently only GitHub is available. GitLab/Gitea will be added
/// behind feature gates once their provider crates export `REGISTRATION`.
///
/// # Panics
///
/// Panics if two registrations share a preset hostname (programmer invariant).
#[must_use]
pub fn available_registrations() -> Vec<&'static Registration> {
    let registrations = vec![
        &actup_provider_github::REGISTRATION,
        // Future:
        // #[cfg(feature = "gitlab")]
        // &actup_provider_gitlab::REGISTRATION,
        // #[cfg(feature = "gitea")]
        // &actup_provider_gitea::REGISTRATION,
    ];
    validate_no_duplicate_presets(&registrations);
    registrations
}

/// Resolve a hostname via config + registration presets, create the provider,
/// and register it in the registry.
fn resolve_and_register(
    hostname: &str,
    config: &Config,
    registrations: &[&'static Registration],
    registry: &mut provider::Registry,
) -> actup_core::Result<()> {
    let provider_config = effective_host(hostname, config, registrations, &config.fetch)?;

    let registration = registrations
        .iter()
        .find(|r| r.kind == provider_config.kind)
        .ok_or_else(|| actup_core::Error::InvalidConfig {
            path: format!("host \"{hostname}\""),
            reason: format!("no registration for kind {:?}", provider_config.kind),
        })?;

    let provider = (registration.create)(provider_config)?;
    registry.register(provider);
    Ok(())
}

/// Build a [`provider::Registry`] from the given config.
///
/// Follows the same algorithm as the CLI:
/// 1. Collect static registrations from compiled-in provider crates
/// 2. Resolve the default host (usually `github.com` from preset)
/// 3. Register providers for each `[hosts."hostname"]` entry in config
/// 4. Ensure the default host is always registered (zero-config for `github.com`)
pub fn build_registry(config: &Config) -> actup_core::Result<provider::Registry> {
    let registrations = available_registrations();
    let mut registry = provider::Registry::new();

    let default_host = resolve_default_host(config, &registrations)?;
    registry.set_default_host(&default_host);

    let mut registered_hosts = HashSet::new();
    for hostname in config.hosts.keys() {
        resolve_and_register(hostname, config, &registrations, &mut registry)?;
        registered_hosts.insert(hostname.clone());
    }

    // Ensure default host is always registered even without an explicit entry.
    if !registered_hosts.contains(&default_host) {
        resolve_and_register(&default_host, config, &registrations, &mut registry)?;
    }

    Ok(registry)
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
    use actup_core::{HostConfig, HostKind};

    use super::*;

    #[test]
    fn available_registrations_includes_github() {
        let regs = available_registrations();
        assert!(regs.iter().any(|r| r.kind == HostKind::GitHub));
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
