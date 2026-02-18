//! actup CLI - GitHub Actions version manager

#![warn(missing_docs)]

mod cli;
mod commands;
mod common;
mod file_cache;
mod output;

use std::process::ExitCode;

use actup_core::{CachingProvider, Config, Registration, provider, validate_no_duplicate_presets};
#[cfg(feature = "gitea")]
use actup_provider_gitea::GiteaProvider;
use actup_provider_github::GitHubProvider;
#[cfg(feature = "gitlab")]
use actup_provider_gitlab::GitLabProvider;
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

fn build_registry(config: &Config) -> actup_core::Result<provider::Registry> {
    // Validate provider registrations early (panics on duplicate preset hosts).
    // The full registration-based provider construction will replace the
    // manual matching below in the `cli-build-registry` task.
    let _registrations = available_registrations();

    let mut registry = provider::Registry::new();

    // Set default host from config (falls back to "github.com" when None)
    let default_host = config.default_host.as_deref().unwrap_or("github.com");
    registry.set_default_host(default_host);

    let mut has_default_github_instance = false;

    // Register configured host entries from [hosts."hostname"]
    for (host, host_config) in &config.hosts {
        let kind_str = host_config.kind.map_or("github", |k| match k {
            actup_core::HostKind::Github => "github",
            actup_core::HostKind::Gitlab => "gitlab",
            actup_core::HostKind::Gitea => "gitea",
        });

        match kind_str {
            "github" => {
                if host == default_host {
                    has_default_github_instance = true;
                }

                let token = host_config
                    .token_env
                    .as_ref()
                    .and_then(|env_var| std::env::var(env_var).ok());

                // Compute host-aware api_base: github.com uses api.github.com,
                // GHES uses {host}/api/v3
                let default_api_base = if host.is_empty() || host == "github.com" {
                    "https://api.github.com".to_string()
                } else {
                    format!("https://{host}/api/v3")
                };
                let api_base = host_config.api_base.as_deref().unwrap_or(&default_api_base);

                let provider = GitHubProvider::with_host(api_base, host, token.as_deref())?;
                let cached = CachingProvider::new(provider);
                registry.register(Box::new(cached));
            }

            #[cfg(feature = "gitlab")]
            "gitlab" => {
                let token = host_config
                    .token_env
                    .as_ref()
                    .and_then(|env_var| std::env::var(env_var).ok());

                let default_base = format!("https://{host}");
                let api_base = host_config.api_base.as_deref().unwrap_or(&default_base);

                let provider = GitLabProvider::with_host(api_base, host, token.as_deref())?;
                let cached = CachingProvider::new(provider);
                registry.register(Box::new(cached));
            }

            #[cfg(feature = "gitea")]
            "gitea" => {
                let token = host_config
                    .token_env
                    .as_ref()
                    .and_then(|env_var| std::env::var(env_var).ok());

                let default_base = format!("https://{host}");
                let api_base = host_config.api_base.as_deref().unwrap_or(&default_base);

                let provider = GiteaProvider::new(api_base, host, token.as_deref())?;
                let cached = CachingProvider::new(provider);
                registry.register(Box::new(cached));
            }

            _ => {
                tracing::warn!(
                    kind = kind_str,
                    host = %host,
                    "unknown provider kind in config, skipping"
                );
            }
        }
    }

    // Register fallback GitHub provider after configured instances so configured
    // github.com providers take precedence in registry lookups.
    if !has_default_github_instance {
        let github_token = std::env::var("GITHUB_TOKEN").ok();
        let github_provider = GitHubProvider::new(github_token.as_deref())?;
        let cached_provider = CachingProvider::new(github_provider);
        registry.register(Box::new(cached_provider));
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
