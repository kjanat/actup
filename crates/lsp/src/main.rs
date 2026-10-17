//! actup-lsp: Language Server Protocol implementation for actup.
//!
//! Provides diagnostics and code actions for GitHub Actions workflow files,
//! highlighting outdated `uses:` references and offering quick fixes.

#![warn(missing_docs)]

mod backend;
mod code_actions;
mod completion;
mod diagnostics;
mod error_ux;
mod file_cache;
mod hover;
mod position;
mod provider_setup;
mod workspace_commands;

#[cfg(test)]
mod test_utils;

#[cfg(test)]
mod integration_tests;

#[cfg(not(test))]
#[tokio::main]
async fn main() {
    use std::io::IsTerminal;

    use tower_lsp_server::Server;
    use tracing_subscriber::EnvFilter;

    let is_tty = std::io::stderr().is_terminal();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("actup_lsp=info")),
        )
        .with_target(false)
        .without_time()
        .with_level(is_tty)
        .with_ansi(is_tty)
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("actup-lsp starting");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = backend::service(backend::LspBackend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
