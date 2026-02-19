//! LSP backend implementing the [`LanguageServer`] trait.

use dashmap::DashMap;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CodeActionOptions, CodeActionProviderCapability, Diagnostic, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, InitializeParams, InitializeResult,
    InitializedParams, MessageType, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri, WorkDoneProgressOptions,
};
use tower_lsp_server::{Client, LanguageServer};

/// State tracked per open document.
///
/// Stores the full document text (since we use `TextDocumentSyncKind::FULL`),
/// the document version for staleness checks, and any cached diagnostics
/// from the most recent analysis pass.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields read by future diagnostic/code-action tasks
pub struct DocumentState {
    /// Full document text (replaced on every change notification).
    pub text: String,
    /// Monotonically increasing version from the editor.
    pub version: i32,
    /// Cached diagnostics from the last analysis. Empty until first check.
    pub diagnostics: Vec<Diagnostic>,
}

impl DocumentState {
    /// Create initial state from a newly opened document.
    const fn new(text: String, version: i32) -> Self {
        Self {
            text,
            version,
            diagnostics: Vec::new(),
        }
    }
}

/// Returns `true` if the document text contains a `uses:` keyword,
/// indicating it may reference GitHub Actions that need version checking.
///
/// Documents without any `uses:` references are skipped entirely to avoid
/// unnecessary parsing and network requests.
fn has_uses_keyword(text: &str) -> bool {
    text.contains("uses:")
}

/// Core LSP backend for actup.
///
/// Holds the LSP [`Client`] handle for sending notifications (diagnostics,
/// log messages) back to the editor, and a concurrent map of open document
/// state for incremental analysis.
#[derive(Debug)]
pub struct LspBackend {
    /// LSP client handle for sending notifications.
    client: Client,
    /// Open document state, keyed by document URI.
    documents: DashMap<Uri, DocumentState>,
}

impl LspBackend {
    /// Create a new backend from the LSP client handle.
    ///
    /// Used as the constructor passed to [`LspService::new`].
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: DashMap::new(),
        }
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

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        let version = params.text_document.version;

        if !has_uses_keyword(&text) {
            tracing::debug!(uri = uri.as_str(), "no `uses:` found, skipping");
            self.documents
                .insert(uri.clone(), DocumentState::new(text, version));
            // Clear any stale diagnostics for this document.
            self.client
                .publish_diagnostics(uri, Vec::new(), Some(version))
                .await;
            return;
        }

        tracing::debug!(uri = uri.as_str(), version, "document opened");
        self.documents
            .insert(uri, DocumentState::new(text, version));

        // TODO(diagnostic-extraction): trigger async diagnostic computation
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;

        // FULL sync: the entire document text is in the single content change.
        let Some(change) = params.content_changes.into_iter().next() else {
            tracing::warn!(uri = uri.as_str(), "did_change with no content changes");
            return;
        };
        let text = change.text;

        if !has_uses_keyword(&text) {
            tracing::debug!(
                uri = uri.as_str(),
                "no `uses:` after change, clearing diagnostics"
            );
            self.documents
                .insert(uri.clone(), DocumentState::new(text, version));
            self.client
                .publish_diagnostics(uri, Vec::new(), Some(version))
                .await;
            return;
        }

        tracing::debug!(uri = uri.as_str(), version, "document changed");
        self.documents
            .insert(uri, DocumentState::new(text, version));

        // TODO(diagnostic-extraction): trigger async diagnostic computation
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        tracing::debug!(uri = uri.as_str(), "document closed");
        self.documents.remove(&uri);
        // Clear diagnostics for the closed document.
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_uses_keyword_detects_action_refs() {
        assert!(has_uses_keyword("    - uses: actions/checkout@v4"));
        assert!(has_uses_keyword("uses: owner/repo@main"));
        assert!(has_uses_keyword("  uses: ./local-action"));
    }

    #[test]
    fn has_uses_keyword_rejects_non_workflow_files() {
        assert!(!has_uses_keyword("name: CI\non:\n  push:\n"));
        assert!(!has_uses_keyword(""));
        assert!(!has_uses_keyword("run: echo hello"));
    }

    #[test]
    fn document_state_initializes_with_empty_diagnostics() {
        let state = DocumentState::new("test".into(), 1);
        assert_eq!(state.text, "test");
        assert_eq!(state.version, 1);
        assert!(state.diagnostics.is_empty());
    }
}
