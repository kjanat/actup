//! actup CLI - GitHub Actions version manager

mod cli;
mod commands;
mod common;
mod output;

use std::process::ExitCode;

use actup_core::{CachingProvider, Config, ProviderRegistry};
use actup_provider_github::GitHubProvider;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, CliExitCode, Command};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // Initialize tracing
    let filter = if cli.verbose {
        EnvFilter::new("actup=debug,actup_core=debug,actup_provider_github=debug")
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
        | Command::Config { path } => path.clone(),
        Command::Schema => std::env::current_dir().unwrap_or_default(),
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
        Command::Check { path, strict } => {
            commands::check::run(&path, strict, cli.format, &config, &registry).await
        }
        Command::Update { path, dry_run } | Command::Fix { path, dry_run } => {
            commands::update::run(&path, dry_run, cli.format, &config, &registry).await
        }
        Command::Pin { path, dry_run } => {
            commands::pin::run(&path, dry_run, cli.format, &config, &registry).await
        }
        Command::Config { path } => commands::config::run(&path, cli.format, &config),
        Command::Schema => commands::schema::run(),
    }
}

fn build_registry(config: &Config) -> actup_core::Result<ProviderRegistry> {
    let mut registry = ProviderRegistry::new();

    // Set default host from config or fallback to github.com
    // (Core defaults are empty to remain provider-neutral)
    let default_host = if config.providers.default_host.is_empty() {
        "github.com"
    } else {
        &config.providers.default_host
    };
    registry.set_default_host(default_host);

    // Add GitHub provider (wrapped with caching to deduplicate API requests)
    let github_token = std::env::var("GITHUB_TOKEN").ok();
    let github_provider = GitHubProvider::new(github_token.as_deref())?;
    let cached_provider = CachingProvider::new(github_provider);
    registry.register(Box::new(cached_provider));

    Ok(registry)
}
