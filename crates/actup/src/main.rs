//! actup CLI - GitHub Actions version manager

#![warn(missing_docs)]

mod cli;
mod commands;
mod common;
mod file_cache;
mod output;

use std::process::ExitCode;

use actup_core::{CachingProvider, Config, provider};
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

fn build_registry(config: &Config) -> actup_core::Result<provider::Registry> {
    let mut registry = provider::Registry::new();

    // Set default host from config or fallback to github.com
    // (Core defaults are empty to remain provider-neutral)
    let default_host = if config.providers.default_host.is_empty() {
        "github.com"
    } else {
        &config.providers.default_host
    };
    registry.set_default_host(default_host);

    let mut has_default_github_instance = false;

    // Register configured provider instances
    for instance in &config.providers.instances {
        match instance.kind.as_str() {
            "github" => {
                if instance.host == default_host {
                    has_default_github_instance = true;
                }

                let token = instance
                    .token_env
                    .as_ref()
                    .and_then(|env_var| std::env::var(env_var).ok());

                // Compute host-aware api_base: github.com uses api.github.com,
                // GHES uses {host}/api/v3
                let default_api_base = if instance.host.is_empty() || instance.host == "github.com"
                {
                    "https://api.github.com".to_string()
                } else {
                    format!("https://{}/api/v3", instance.host)
                };
                let api_base = instance.api_base.as_deref().unwrap_or(&default_api_base);

                let provider =
                    GitHubProvider::with_host(api_base, &instance.host, token.as_deref())?;
                let cached = CachingProvider::new(provider);
                registry.register(Box::new(cached));
            }

            #[cfg(feature = "gitlab")]
            "gitlab" => {
                let token = instance
                    .token_env
                    .as_ref()
                    .and_then(|env_var| std::env::var(env_var).ok());

                let default_base = format!("https://{}", instance.host);
                let api_base = instance.api_base.as_deref().unwrap_or(&default_base);

                let provider =
                    GitLabProvider::with_host(api_base, &instance.host, token.as_deref())?;
                let cached = CachingProvider::new(provider);
                registry.register(Box::new(cached));
            }

            #[cfg(feature = "gitea")]
            "gitea" | "forgejo" => {
                let token = instance
                    .token_env
                    .as_ref()
                    .and_then(|env_var| std::env::var(env_var).ok());

                let default_base = format!("https://{}", instance.host);
                let api_base = instance.api_base.as_deref().unwrap_or(&default_base);

                let provider = GiteaProvider::new(api_base, &instance.host, token.as_deref())?;
                let cached = CachingProvider::new(provider);
                registry.register(Box::new(cached));
            }

            _ => {
                tracing::warn!(
                    kind = %instance.kind,
                    host = %instance.host,
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
