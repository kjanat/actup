//! LSP backend implementing the [`LanguageServer`] trait.

use std::path::PathBuf;
use std::sync::Arc;

use actup_core::cache::Cache as _;
use actup_core::{Config, provider};
use dashmap::DashMap;
use tokio::sync::RwLock;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CodeActionOptions, CodeActionProviderCapability, Diagnostic, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, InitializeParams, InitializeResult,
    InitializedParams, MessageType, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri, WorkDoneProgressOptions,
};
use tower_lsp_server::{Client, LanguageServer};

use crate::file_cache::FileCache;
use crate::provider_setup;

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

/// Extracts the workspace root path from LSP [`InitializeParams`].
///
/// Tries sources in priority order per LSP spec:
/// 1. `workspace_folders[0].uri` (modern, preferred)
/// 2. `root_uri` (deprecated but widely used)
/// 3. `root_path` (legacy, string-based)
///
/// Returns `None` if no workspace root can be determined (e.g. single-file mode).
#[allow(deprecated)] // root_uri and root_path are deprecated but necessary fallbacks
fn extract_workspace_root(params: &InitializeParams) -> Option<PathBuf> {
    // 1. workspace_folders (preferred)
    if let Some(folders) = &params.workspace_folders
        && let Some(first) = folders.first()
        && let Some(path) = first.uri.to_file_path()
    {
        return Some(path.into_owned());
    }

    // 2. root_uri (deprecated but common)
    if let Some(uri) = &params.root_uri
        && let Some(path) = uri.to_file_path()
    {
        return Some(path.into_owned());
    }

    // 3. root_path (legacy string)
    if let Some(path) = &params.root_path {
        return Some(PathBuf::from(path));
    }

    None
}

/// Extracts an optional config file path from LSP `initializationOptions`.
///
/// Clients can pass `{ "configPath": "/path/to/actup.toml" }` to override
/// the standard config discovery (analogous to CLI's `--config` flag).
fn extract_config_path(params: &InitializeParams) -> Option<PathBuf> {
    let opts = params.initialization_options.as_ref()?;
    let path_str = opts.get("configPath")?.as_str()?;
    Some(PathBuf::from(path_str))
}

/// Core LSP backend for actup.
///
/// Holds the LSP [`Client`] handle for sending notifications (diagnostics,
/// log messages) back to the editor, and a concurrent map of open document
/// state for incremental analysis.
///
/// The provider [`Registry`](provider::Registry) and [`FileCache`] are
/// initialized during `initialize` after config is loaded, and wrapped in
/// `Arc` for shared access from background diagnostic tasks.
pub struct LspBackend {
    /// LSP client handle for sending notifications.
    client: Client,
    /// Open document state, keyed by document URI.
    documents: DashMap<Uri, DocumentState>,
    /// Resolved workspace root path (set during `initialize`).
    workspace_root: RwLock<Option<PathBuf>>,
    /// Loaded actup configuration (set during `initialize`, may be refreshed
    /// on config file changes).
    config: RwLock<Config>,
    /// Provider registry for version lookups (set during `initialize`).
    ///
    /// `None` until `initialize` completes. Providers are created from config
    /// and static registrations (GitHub always present, GitLab/Gitea behind
    /// feature gates).
    registry: RwLock<Option<Arc<provider::Registry>>>,
    /// Persistent file-based cache shared with the CLI (set during `initialize`).
    ///
    /// Stored as `Arc` so background diagnostic tasks can read/write the
    /// cache without holding a lock on the entire backend.
    file_cache: RwLock<Option<Arc<FileCache>>>,
}

impl LspBackend {
    /// Create a new backend from the LSP client handle.
    ///
    /// Used as the constructor passed to [`LspService::new`].
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: DashMap::new(),
            workspace_root: RwLock::new(None),
            config: RwLock::new(Config::default()),
            registry: RwLock::new(None),
            file_cache: RwLock::new(None),
        }
    }
}

impl LanguageServer for LspBackend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        tracing::info!("actup-lsp initializing");

        // Extract workspace root from params.
        let workspace_root = extract_workspace_root(&params);
        let config_path = extract_config_path(&params);

        if let Some(root) = &workspace_root {
            tracing::info!(root = %root.display(), "workspace root");
        } else {
            tracing::warn!("no workspace root detected; using defaults");
        }

        // Load config: explicit path > repo discovery > user config > defaults.
        let start_dir = workspace_root.as_deref().unwrap_or_else(|| {
            // If no workspace root, use cwd as fallback for config discovery.
            std::path::Path::new(".")
        });

        let config = match Config::load(config_path.as_deref(), start_dir) {
            Ok(c) => {
                if let Some(from) = &c.loaded_from {
                    tracing::info!(path = %from.display(), "loaded config");
                } else {
                    tracing::info!("using default config (no config file found)");
                }
                c
            }
            Err(e) => {
                tracing::warn!("failed to load config, using defaults: {e}");
                Config::default()
            }
        };

        // Build provider registry from config + compiled-in registrations.
        let registry = match provider_setup::build_registry(&config) {
            Ok(r) => {
                if let Some(host) = r.default_host() {
                    tracing::info!(default_host = host, "provider registry built");
                }
                Some(Arc::new(r))
            }
            Err(e) => {
                tracing::error!("failed to build provider registry: {e}");
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!("actup-lsp: failed to initialize providers: {e}"),
                    )
                    .await;
                None
            }
        };

        // Initialize persistent file cache (shared with CLI).
        let file_cache = Arc::new(provider_setup::build_file_cache(&config));
        tracing::info!(enabled = file_cache.is_enabled(), "file cache initialized");

        // Store state for use by other handlers.
        *self.workspace_root.write().await = workspace_root;
        *self.config.write().await = config;
        *self.registry.write().await = registry;
        *self.file_cache.write().await = Some(file_cache);

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
        let config = self.config.read().await;
        let policy = config.update_policy;
        let ignored = config.ignore.len();
        drop(config);

        let has_registry = self.registry.read().await.is_some();

        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "actup-lsp ready (policy={policy:?}, {ignored} ignored patterns, providers={})",
                    if has_registry { "ok" } else { "unavailable" }
                ),
            )
            .await;
        tracing::info!(?policy, ignored, has_registry, "actup-lsp initialized");
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
#[allow(clippy::unwrap_used)]
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

    #[test]
    fn extract_workspace_root_from_root_path() {
        #[allow(deprecated)]
        let params = InitializeParams {
            root_path: Some("/home/user/project".to_owned()),
            ..Default::default()
        };
        let root = extract_workspace_root(&params);
        assert_eq!(root, Some(PathBuf::from("/home/user/project")));
    }

    #[test]
    fn extract_workspace_root_returns_none_when_empty() {
        let params = InitializeParams::default();
        assert!(extract_workspace_root(&params).is_none());
    }

    #[test]
    fn extract_config_path_from_init_options() {
        let params = InitializeParams {
            initialization_options: Some(serde_json::json!({ "configPath": "/tmp/actup.toml" })),
            ..Default::default()
        };
        assert_eq!(
            extract_config_path(&params),
            Some(PathBuf::from("/tmp/actup.toml"))
        );
    }

    #[test]
    fn extract_config_path_returns_none_when_missing() {
        let params = InitializeParams::default();
        assert!(extract_config_path(&params).is_none());

        // Present but no configPath key
        let params = InitializeParams {
            initialization_options: Some(serde_json::json!({ "other": "value" })),
            ..Default::default()
        };
        assert!(extract_config_path(&params).is_none());
    }

    #[test]
    fn extract_workspace_root_prefers_workspace_folders() {
        let uri = Uri::from_file_path("/workspace/root").unwrap();
        #[allow(deprecated)]
        let params = InitializeParams {
            workspace_folders: Some(vec![tower_lsp_server::ls_types::WorkspaceFolder {
                uri,
                name: "root".to_owned(),
            }]),
            root_uri: Uri::from_file_path("/other/root"),
            root_path: Some("/legacy/root".to_owned()),
            ..Default::default()
        };
        let root = extract_workspace_root(&params);
        assert_eq!(root, Some(PathBuf::from("/workspace/root")));
    }
}
