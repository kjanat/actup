//! actup CLI - GitHub Actions version manager

#![warn(missing_docs)]

mod cli;
mod commands;
mod common;
mod file_cache;
mod output;

use std::process::ExitCode;

use actup_core::{
    Config, Registration, effective_host, provider, resolve_default_host,
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
    // Get the start directory from command path
    let start_dir = match &cli.command {
        Command::Check { path, .. }
        | Command::Update { path, .. }
        | Command::Fix { path, .. }
        | Command::Pin { path, .. }
        | Command::Config { path, .. } => path.clone(),
        Command::Schema | Command::Cache { .. } => match std::env::current_dir() {
            Ok(dir) => dir,
            Err(e) => {
                eprintln!("Error reading current directory: {e}");
                return CliExitCode::EnvironmentError.into();
            }
        },
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
        Command::Check {
            path,
            strict,
            format,
        } => commands::check::run(&path, strict, format, &config, &registry).await,
        Command::Update {
            path,
            dry_run,
            format,
        }
        | Command::Fix {
            path,
            dry_run,
            format,
        } => commands::update::run(&path, dry_run, format, &config, &registry).await,
        Command::Pin {
            path,
            dry_run,
            format,
        } => commands::pin::run(&path, dry_run, format, &config, &registry).await,
        Command::Config { path, format } => commands::config::run(&path, format, &config),
        Command::Schema => commands::schema::run(),
        Command::Cache { action, format } => commands::cache::run(&action, format, &config).await,
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
fn available_registrations() -> Vec<&'static Registration> {
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
    registry.register(provider);
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

    #[test]
    fn available_registrations_includes_github() {
        let regs = available_registrations();
        assert!(
            regs.iter().any(|r| r.kind == HostKind::Github),
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
        let github = regs.iter().find(|r| r.kind == HostKind::Github).unwrap();
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
}
