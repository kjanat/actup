//! actup-lsp: Language Server Protocol implementation for actup.
//!
//! Provides diagnostics and code actions for GitHub Actions workflow files,
//! highlighting outdated `uses:` references and offering quick fixes.

#![warn(missing_docs)]

mod backend;

use tower_lsp_server::{LspService, Server};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("actup_lsp=info")),
        )
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("actup-lsp starting");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(backend::LspBackend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
