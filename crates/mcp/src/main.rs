//! actup-mcp: serves the actup MCP tools over stdio.

#![warn(missing_docs)]

use std::{path::PathBuf, process::ExitCode};

use actup_core::{Config, provider::Registry, user_config_path};
use actup_mcp::{Root, Server, serve_stdio};
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// Model Context Protocol server for checking and updating GitHub Actions `uses:` refs.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Directory that confines every path the tools read or write [default: current directory]
    #[arg(long, value_name = "DIR")]
    root: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("actup_mcp=info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .without_time()
        .init();

    let dir = match args.root.map_or_else(std::env::current_dir, Ok) {
        Ok(dir) => dir,
        Err(e) => return fail(2, &format!("cannot read the current directory: {e}")),
    };
    let root = match Root::new(&dir) {
        Ok(root) => root,
        Err(e) => return fail(2, &format!("invalid root {}: {e}", dir.display())),
    };
    let config = match Config::load(
        None,
        user_config_path().as_deref(),
        root.path(),
        Some(root.path()),
    ) {
        Ok(config) => config,
        Err(e) => return fail(2, &e.to_string()),
    };
    let registry = match Registry::from_config(&config, &[&actup_provider_github::REGISTRATION]) {
        Ok(registry) => registry,
        Err(e) => return fail(3, &format!("cannot initialize providers: {e}")),
    };

    tracing::info!(root = %root.path().display(), "actup-mcp serving on stdio");
    match serve_stdio(Server::new(config, registry, root)).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(3, &e.to_string()),
    }
}

fn fail(code: u8, message: &str) -> ExitCode {
    eprintln!("actup-mcp: {message}");
    ExitCode::from(code)
}
