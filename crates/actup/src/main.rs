//! actup CLI - GitHub Actions version manager

#![warn(missing_docs)]

mod cli;
mod commands;
mod common;
mod file_cache;
mod output;

use std::process::ExitCode;

use actup_core::{
    CachingProvider, Config, Registration, effective_host, provider, resolve_default_host,
    validate_no_duplicate_presets,
};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, CliExitCode, Command};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // Initialize tracing
    let filter = if cli.verbose {
        EnvFilter::new(
            "actup=debug,actup_core=debug,actup_provider_github=debug,actup_provider_gitlab=debug,actup_provider_gitea=debug",
        )
    } else {
        EnvFilter::new("actup=info")
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .init();

    let exit_code = run(cli).await;
    // Exit codes 0-3 are safe to convert to u8
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    ExitCode::from(exit_code as u8)
}

async fn run(cli: Cli) -> i32 {
    // Handle completions early (no config/registry needed)
    if let Some(Command::Completions { shell }) = cli.command {
        let Some(shell) = shell.or_else(Cli::detect_shell) else {
            eprintln!(
                "Could not detect shell from $SHELL. Specify one: bash, zsh, fish, elvish, powershell"
            );
            return CliExitCode::EnvironmentError.into();
        };
        Cli::print_completions(shell);
        return CliExitCode::Success.into();
    }

    // No subcommand and no positional action argument: print help
    if cli.command.is_none() && cli.latest.action.is_none() {
        let _ = <Cli as clap::CommandFactory>::command().print_help();
        println!();
        return CliExitCode::Success.into();
    }

    // Get the start directory from command path
    let start_dir = match &cli.command {
        Some(
            Command::Check { path, .. }
            | Command::Update { path, .. }
            | Command::Fix { path, .. }
            | Command::Pin { path, .. }
            | Command::Config { path, .. },
        ) => path.clone(),
        Some(Command::Schema | Command::Cache { .. }) | None => match std::env::current_dir() {
            Ok(dir) => dir,
            Err(e) => {
                eprintln!("Error reading current directory: {e}");
                return CliExitCode::EnvironmentError.into();
            }
        },
        Some(Command::Completions { .. }) => unreachable!("handled above"),
    };

    let config = match Config::load(None, &start_dir) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to load config, using defaults: {}", e);
            Config::default()
        }
    };

    // Build provider registry
    let registry = match build_registry(&config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error initializing providers: {e}");
            return CliExitCode::EnvironmentError.into();
        }
    };

    match cli.command {
        Some(Command::Check {
            path,
            strict,
            format,
        }) => commands::check::run(&path, strict, format, &config, &registry).await,
        Some(
            Command::Update {
                path,
                dry_run,
                format,
            }
            | Command::Fix {
                path,
                dry_run,
                format,
            },
        ) => commands::update::run(&path, dry_run, format, &config, &registry).await,
        Some(Command::Pin {
            path,
            dry_run,
            format,
        }) => commands::pin::run(&path, dry_run, format, &config, &registry).await,
        Some(Command::Config { path, format }) => {
            let registrations = available_registrations();
            commands::config::run(&path, format, &config, &registrations)
        }
        Some(Command::Schema) => commands::schema::run(),
        Some(Command::Cache { action, format }) => {
            commands::cache::run(&action, format, &config).await
        }
        Some(Command::Completions { .. }) => unreachable!("handled before config load"),
        None => commands::latest::run(&cli.latest, &config, &registry).await,
    }
}

/// Returns all provider registrations enabled by Cargo features.
///
/// Each compiled-in provider crate contributes its static [`Registration`].
/// The returned list is validated: duplicate preset hostnames across
/// providers cause a panic (programmer invariant).
///
/// Feature gates:
/// - GitHub is always included (`actup-provider-github` is non-optional)
/// - GitLab included when `gitlab` feature is enabled
/// - Gitea included when `gitea` feature is enabled
///
/// # Panics
///
/// Panics if two or more registrations share the same preset hostname.
#[must_use]
pub(crate) fn available_registrations() -> Vec<&'static Registration> {
    let registrations = vec![
        &actup_provider_github::REGISTRATION,
        // GitLab and Gitea providers will be added here once they export
        // `REGISTRATION`. The feature gates are ready:
        //
        // #[cfg(feature = "gitlab")]
        // &actup_provider_gitlab::REGISTRATION,
        //
        // #[cfg(feature = "gitea")]
        // &actup_provider_gitea::REGISTRATION,
    ];
    validate_no_duplicate_presets(&registrations);
    registrations
}

/// Resolve a hostname, create its provider, and register it.
///
/// Looks up the [`Registration`] by kind (from the resolved [`actup_core::ProviderConfig`])
/// and calls its `create` fn pointer to construct the concrete provider.
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
    registry.register(Box::new(CachingProvider::new(provider)));
    Ok(())
}

fn build_registry(config: &Config) -> actup_core::Result<provider::Registry> {
    let registrations = available_registrations();
    let mut registry = provider::Registry::new();

    // Resolve default host (validates + falls back to "github.com" preset)
    let default_host = resolve_default_host(config, &registrations)?;
    registry.set_default_host(&default_host);

    // Resolve and register explicit [hosts."hostname"] entries
    let mut registered_hosts = std::collections::HashSet::new();
    for hostname in config.hosts.keys() {
        resolve_and_register(hostname, config, &registrations, &mut registry)?;
        registered_hosts.insert(hostname.clone());
    }

    // Ensure the default host is always registered (preset provides zero-config)
    if !registered_hosts.contains(&default_host) {
        resolve_and_register(&default_host, config, &registrations, &mut registry)?;
    }

    Ok(registry)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use actup_core::HostKind;

    use super::*;

    /// Construct a `HostConfig` with only `kind` set (no Default impl available).
    fn host_config_with_kind(kind: HostKind) -> actup_core::HostConfig {
        actup_core::HostConfig {
            kind: Some(kind),
            api_base: None,
            token_env: None,
            api_mode: None,
            max_tags_per_repo: None,
        }
    }

    /// Construct an empty `HostConfig` (all None).
    fn host_config_empty() -> actup_core::HostConfig {
        actup_core::HostConfig {
            kind: None,
            api_base: None,
            token_env: None,
            api_mode: None,
            max_tags_per_repo: None,
        }
    }

    #[test]
    fn available_registrations_includes_github() {
        let regs = available_registrations();
        assert!(
            regs.iter().any(|r| r.kind == HostKind::GitHub),
            "GitHub registration must always be present"
        );
    }

    #[test]
    fn available_registrations_nonempty() {
        let regs = available_registrations();
        assert!(!regs.is_empty(), "must have at least one registration");
    }

    #[test]
    fn available_registrations_no_duplicate_presets() {
        let regs = available_registrations();
        let mut seen_hosts = std::collections::HashSet::new();
        for reg in &regs {
            for preset in reg.presets {
                assert!(
                    seen_hosts.insert(preset.host),
                    "duplicate preset host: {}",
                    preset.host
                );
            }
        }
    }

    #[test]
    fn available_registrations_github_preset_host() {
        let regs = available_registrations();
        let github = regs.iter().find(|r| r.kind == HostKind::GitHub).unwrap();
        assert!(
            github.presets.iter().any(|p| p.host == "github.com"),
            "GitHub preset must include github.com"
        );
    }

    #[test]
    fn available_registrations_all_have_capabilities() {
        let regs = available_registrations();
        for reg in &regs {
            assert!(
                reg.capabilities.supports_graphql || reg.capabilities.supports_rest,
                "{:?} provider has no supported API modes",
                reg.kind
            );
        }
    }

    #[test]
    fn available_registrations_is_deterministic() {
        let a = available_registrations();
        let b = available_registrations();
        assert_eq!(a.len(), b.len());
        for (ra, rb) in a.iter().zip(b.iter()) {
            assert_eq!(ra.kind, rb.kind);
            assert_eq!(ra.presets.len(), rb.presets.len());
        }
    }

    #[test]
    fn available_registrations_expected_count() {
        let regs = available_registrations();
        // Currently only GitHub exports REGISTRATION.
        // When GitLab/Gitea add theirs, update expected count
        // based on default features (github + gitlab + gitea = 3).
        let mut expected = 1; // GitHub is always present

        // GitLab/Gitea registrations not yet exported by provider crates.
        // When they are, uncomment:
        // #[cfg(feature = "gitlab")]
        // { expected += 1; }
        // #[cfg(feature = "gitea")]
        // { expected += 1; }
        let _ = &mut expected; // suppress unused_assignments

        assert_eq!(regs.len(), expected);
    }

    #[test]
    fn build_registry_empty_config_uses_github_preset() {
        let config = Config::default();
        let registry = build_registry(&config).unwrap();
        assert_eq!(
            registry.default_host(),
            Some("github.com"),
            "empty config must default to github.com"
        );
        assert!(
            registry.find(Some("github.com")).is_some(),
            "github.com provider must be registered"
        );
    }

    #[test]
    fn build_registry_explicit_default_host_github() {
        let config = Config {
            default_host: Some("github.com".to_string()),
            ..Default::default()
        };
        let registry = build_registry(&config).unwrap();
        assert_eq!(registry.default_host(), Some("github.com"));
        assert!(registry.find(Some("github.com")).is_some());
    }

    #[test]
    fn build_registry_default_host_always_registered_even_without_hosts_entry() {
        // default_host = github.com, no [hosts."github.com"] entry
        let config = Config::default();
        assert!(config.hosts.is_empty());
        let registry = build_registry(&config).unwrap();
        assert!(
            registry.find(Some("github.com")).is_some(),
            "default host must be registered even without explicit hosts entry"
        );
    }

    #[test]
    fn build_registry_hosts_entry_for_default_host_no_duplicate() {
        // explicit [hosts."github.com"] should not cause double-registration
        let mut config = Config::default();
        config.hosts.insert(
            "github.com".to_string(),
            host_config_with_kind(HostKind::GitHub),
        );
        let registry = build_registry(&config).unwrap();
        assert!(registry.find(Some("github.com")).is_some());
        // Registry allows duplicates in Vec but build_registry skips re-registering
        // default_host if already in config.hosts
        assert_eq!(
            registry
                .providers()
                .iter()
                .filter(|p| p.host() == "github.com")
                .count(),
            1,
            "github.com should be registered exactly once"
        );
    }

    #[test]
    fn build_registry_ghe_host_requires_kind() {
        // [hosts."ghe.corp.com"] without kind should error
        let mut config = Config::default();
        config
            .hosts
            .insert("ghe.corp.com".to_string(), host_config_empty());
        let result = build_registry(&config);
        assert!(result.is_err(), "GHE host without kind must error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("kind"),
            "error must mention missing kind: {err}"
        );
    }

    #[test]
    fn build_registry_ghe_host_with_github_kind() {
        // [hosts."ghe.corp.com"] with kind = "github" should succeed
        // (api_base inferred, token_env defaults)
        let mut config = Config::default();
        config.hosts.insert(
            "ghe.corp.com".to_string(),
            host_config_with_kind(HostKind::GitHub),
        );
        let registry = build_registry(&config).unwrap();
        assert!(
            registry.find(Some("ghe.corp.com")).is_some(),
            "GHE host with github kind must be registered"
        );
        // github.com should also be registered (default)
        assert!(registry.find(Some("github.com")).is_some());
    }

    #[test]
    fn build_registry_invalid_hostname_errors() {
        let mut config = Config::default();
        config.hosts.insert(
            "UPPERCASE.COM".to_string(),
            host_config_with_kind(HostKind::GitHub),
        );
        let result = build_registry(&config);
        assert!(result.is_err(), "uppercase hostname must error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("lowercase") || err.contains("case"),
            "error must mention case: {err}"
        );
    }

    #[test]
    fn build_registry_invalid_default_host_errors() {
        let config = Config {
            default_host: Some("INVALID HOST".to_string()),
            ..Default::default()
        };
        let result = build_registry(&config);
        assert!(
            result.is_err(),
            "invalid default_host must error: {result:?}"
        );
    }

    #[test]
    fn build_registry_invalid_api_base_in_hosts_errors() {
        let mut config = Config::default();
        config.hosts.insert(
            "ghe.corp.com".to_string(),
            actup_core::HostConfig {
                kind: Some(HostKind::GitHub),
                api_base: Some("no-scheme.example.com/api".to_string()),
                token_env: None,
                api_mode: None,
                max_tags_per_repo: None,
            },
        );
        let result = build_registry(&config);
        assert!(result.is_err(), "api_base without scheme must error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("scheme") || err.contains("http"),
            "error must mention scheme: {err}"
        );
    }

    #[test]
    fn build_registry_fetch_settings_propagated() {
        use std::num::NonZeroU8;

        use actup_core::{ApiMode, FetchSettings};

        let config = Config {
            fetch: FetchSettings {
                api_mode: ApiMode::Rest,
                max_tags_per_repo: NonZeroU8::new(50).unwrap(),
            },
            ..Default::default()
        };
        // Should succeed — github provider supports both graphql and rest
        let registry = build_registry(&config).unwrap();
        assert!(registry.find(Some("github.com")).is_some());
    }

    #[test]
    fn build_registry_multiple_hosts() {
        let mut config = Config::default();
        config.hosts.insert(
            "ghe1.corp.com".to_string(),
            host_config_with_kind(HostKind::GitHub),
        );
        config.hosts.insert(
            "ghe2.corp.com".to_string(),
            host_config_with_kind(HostKind::GitHub),
        );
        let registry = build_registry(&config).unwrap();
        assert!(registry.find(Some("ghe1.corp.com")).is_some());
        assert!(registry.find(Some("ghe2.corp.com")).is_some());
        assert!(registry.find(Some("github.com")).is_some());
    }
}
