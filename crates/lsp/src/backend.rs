//! LSP backend implementing the [`LanguageServer`] trait.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CodeActionOptions, CodeActionProviderCapability, InitializeParams, InitializeResult,
    InitializedParams, MessageType, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, WorkDoneProgressOptions,
};
use tower_lsp_server::{Client, LanguageServer};

/// Core LSP backend for actup.
///
/// Holds the LSP [`Client`] handle for sending notifications (diagnostics,
/// log messages) back to the editor. Future fields will include document
/// state, provider registry, and resolved config.
#[derive(Debug)]
pub struct LspBackend {
    /// LSP client handle for sending notifications.
    client: Client,
}

impl LspBackend {
    /// Create a new backend from the LSP client handle.
    ///
    /// Used as the constructor passed to [`LspService::new`].
    pub const fn new(client: Client) -> Self {
        Self { client }
    }
}

impl LanguageServer for LspBackend {
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
        tracing::info!("actup-lsp initializing");

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "actup-lsp".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            tower_lsp_server::ls_types::CodeActionKind::QUICKFIX,
                            tower_lsp_server::ls_types::CodeActionKind::SOURCE_FIX_ALL,
                        ]),
                        work_done_progress_options: WorkDoneProgressOptions {
                            work_done_progress: Some(false),
                        },
                        resolve_provider: Some(false),
                    },
                )),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "actup-lsp ready")
            .await;
        tracing::info!("actup-lsp initialized");
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("actup-lsp shutting down");
        Ok(())
    }
}
