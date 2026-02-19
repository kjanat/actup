//! LSP backend implementing the [`LanguageServer`] trait.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use actup_core::cache::Cache as _;
use actup_core::{Config, provider};
use dashmap::DashMap;
use tokio::sync::RwLock;
use tokio::task::AbortHandle;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CodeActionOptions, CodeActionProviderCapability, Diagnostic, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    InitializeParams, InitializeResult, InitializedParams, MessageType, ServerCapabilities,
    ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Uri, WorkDoneProgressOptions,
};
use tower_lsp_server::{Client, LanguageServer};

use crate::file_cache::FileCache;
use crate::provider_setup;

/// Debounce delay for diagnostic computation on document changes.
///
/// Prevents flooding providers with API requests when the user types rapidly.
/// After each edit, we wait this long before starting version checks. If another
/// edit arrives before the delay expires, the previous pending check is cancelled.
const DEBOUNCE_DELAY: Duration = Duration::from_millis(300);

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
    documents: Arc<DashMap<Uri, DocumentState>>,
    /// Resolved workspace root path (set during `initialize`).
    workspace_root: RwLock<Option<PathBuf>>,
    /// Loaded actup configuration (set during `initialize`, may be refreshed
    /// on config file changes).
    config: Arc<RwLock<Config>>,
    /// Provider registry for version lookups (set during `initialize`).
    ///
    /// `None` until `initialize` completes. Providers are created from config
    /// and static registrations (GitHub always present, GitLab/Gitea behind
    /// feature gates).
    registry: Arc<RwLock<Option<Arc<provider::Registry>>>>,
    /// Persistent file-based cache shared with the CLI (set during `initialize`).
    ///
    /// Stored as `Arc` so background diagnostic tasks can read/write the
    /// cache without holding a lock on the entire backend.
    file_cache: Arc<RwLock<Option<Arc<FileCache>>>>,
    /// Abort handles for in-flight diagnostic tasks, keyed by document URI.
    ///
    /// When a new edit arrives, we abort the previous pending task for that
    /// document before spawning a fresh one. This, combined with
    /// [`DEBOUNCE_DELAY`], prevents redundant version checks.
    in_flight: DashMap<Uri, AbortHandle>,
}

impl LspBackend {
    /// Create a new backend from the LSP client handle.
    ///
    /// Used as the constructor passed to [`LspService::new`].
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(DashMap::new()),
            workspace_root: RwLock::new(None),
            config: Arc::new(RwLock::new(Config::default())),
            registry: Arc::new(RwLock::new(None)),
            file_cache: Arc::new(RwLock::new(None)),
            in_flight: DashMap::new(),
        }
    }

    /// Schedule background diagnostic computation for a document.
    ///
    /// Cancels any previously scheduled (or in-flight) diagnostic task for
    /// the same URI, then spawns a new task that:
    /// 1. Waits `debounce` duration (cancelled if another edit arrives)
    /// 2. Snapshots config, registry, and cache
    /// 3. Runs [`compute_diagnostics`](crate::diagnostics::compute_diagnostics)
    /// 4. Publishes results only if the document version is still current
    ///
    /// Network errors within `compute_diagnostics` are surfaced as
    /// `Information`-severity diagnostics rather than propagated, so the
    /// editor UI is never blocked by slow or failing network calls.
    fn schedule_diagnostics(&self, uri: Uri, text: String, version: i32, debounce: Duration) {
        // Cancel any pending diagnostic task for this document.
        if let Some((_, prev)) = self.in_flight.remove(&uri) {
            prev.abort();
            tracing::debug!(uri = uri.as_str(), "cancelled previous diagnostic task");
        }

        let client = self.client.clone();
        let documents = Arc::clone(&self.documents);
        let config = Arc::clone(&self.config);
        let registry = Arc::clone(&self.registry);
        let file_cache = Arc::clone(&self.file_cache);
        let task_uri = uri.clone();

        let handle = tokio::spawn(async move {
            let uri = task_uri;
            // Debounce: wait before starting expensive version checks.
            // If the task is aborted during this sleep (because a new edit
            // arrived), no work is wasted.
            if !debounce.is_zero() {
                tokio::time::sleep(debounce).await;
            }

            // Snapshot shared state after debounce (picks up any config changes
            // that happened during the wait).
            let config = config.read().await.clone();
            let registry = registry.read().await.clone();
            let file_cache = file_cache.read().await.clone();

            let Some(registry) = registry else {
                tracing::debug!("no provider registry; skipping diagnostics");
                return;
            };

            let file_cache =
                file_cache.unwrap_or_else(|| Arc::new(crate::file_cache::FileCache::disabled()));

            let diagnostics = crate::diagnostics::compute_diagnostics(
                &text,
                &uri,
                &config,
                &registry,
                &file_cache,
            )
            .await;

            // Update cached diagnostics in document state, checking for staleness.
            let should_publish = if let Some(mut doc) = documents.get_mut(&uri) {
                if doc.version == version {
                    doc.diagnostics.clone_from(&diagnostics);
                    true
                } else {
                    tracing::debug!(
                        uri = uri.as_str(),
                        started = version,
                        current = doc.version,
                        "stale diagnostics, skipping publish"
                    );
                    false
                }
            } else {
                // Document was closed while we were computing. Don't publish.
                false
            };

            if should_publish {
                client
                    .publish_diagnostics(uri, diagnostics, Some(version))
                    .await;
            }
        });

        self.in_flight.insert(uri, handle.abort_handle());
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
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
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
            .insert(uri.clone(), DocumentState::new(text.clone(), version));

        // No debounce on open — user expects immediate feedback.
        self.schedule_diagnostics(uri, text, version, Duration::ZERO);
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
            // Cancel any pending diagnostic task — document no longer has refs.
            if let Some((_, prev)) = self.in_flight.remove(&uri) {
                prev.abort();
            }
            self.documents
                .insert(uri.clone(), DocumentState::new(text, version));
            self.client
                .publish_diagnostics(uri, Vec::new(), Some(version))
                .await;
            return;
        }

        tracing::debug!(uri = uri.as_str(), version, "document changed");
        self.documents
            .insert(uri.clone(), DocumentState::new(text.clone(), version));

        // Debounce: wait before checking versions to avoid API spam on rapid edits.
        self.schedule_diagnostics(uri, text, version, DEBOUNCE_DELAY);
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;

        // On save, re-trigger diagnostics with no debounce. Use the currently
        // stored document text (save doesn't carry text unless configured).
        let Some(doc) = self.documents.get(&uri) else {
            return;
        };
        let text = doc.text.clone();
        let version = doc.version;
        drop(doc);

        if !has_uses_keyword(&text) {
            return;
        }

        tracing::debug!(uri = uri.as_str(), "document saved, refreshing diagnostics");
        self.schedule_diagnostics(uri, text, version, Duration::ZERO);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        tracing::debug!(uri = uri.as_str(), "document closed");

        // Cancel any pending diagnostic task for this document.
        if let Some((_, prev)) = self.in_flight.remove(&uri) {
            prev.abort();
        }

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
    fn debounce_delay_is_300ms() {
        assert_eq!(DEBOUNCE_DELAY, Duration::from_millis(300));
    }

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

    #[tokio::test]
    async fn schedule_diagnostics_registers_in_flight_handle() {
        // Direct test: construct backend parts and verify in_flight tracking.
        let in_flight: DashMap<Uri, AbortHandle> = DashMap::new();
        let uri = Uri::from_file_path("/test/file.yml").unwrap();

        // Simulate what schedule_diagnostics does: insert an abort handle.
        let handle = tokio::spawn(async { tokio::time::sleep(Duration::from_secs(60)).await });
        in_flight.insert(uri.clone(), handle.abort_handle());

        assert!(in_flight.contains_key(&uri));

        // Simulate cancellation on new edit.
        if let Some((_, prev)) = in_flight.remove(&uri) {
            prev.abort();
        }
        assert!(!in_flight.contains_key(&uri));
    }

    #[tokio::test]
    async fn schedule_diagnostics_cancels_previous_task() {
        let in_flight: DashMap<Uri, AbortHandle> = DashMap::new();
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();

        // Spawn a long-running task (simulating slow version check).
        let task1 = tokio::spawn(async { tokio::time::sleep(Duration::from_secs(60)).await });
        let handle1 = task1.abort_handle();
        in_flight.insert(uri.clone(), handle1);

        // Second "edit" arrives — should cancel the first.
        if let Some((_, prev)) = in_flight.remove(&uri) {
            prev.abort();
        }
        let task2 = tokio::spawn(async { tokio::time::sleep(Duration::from_secs(60)).await });
        in_flight.insert(uri.clone(), task2.abort_handle());

        // First task should be cancelled.
        assert!(task1.await.unwrap_err().is_cancelled());

        // Cleanup.
        task2.abort();
    }

    #[tokio::test]
    async fn version_staleness_prevents_stale_publish() {
        let documents: Arc<DashMap<Uri, DocumentState>> = Arc::new(DashMap::new());
        let uri = Uri::from_file_path("/test/stale.yml").unwrap();

        // Store document at version 5.
        documents.insert(
            uri.clone(),
            DocumentState::new("uses: actions/checkout@v4".into(), 5),
        );

        // Simulate what the spawned task does: check version before publishing.
        let started_version = 3; // started computing for version 3
        let should_publish = if let Some(doc) = documents.get(&uri) {
            doc.version == started_version
        } else {
            false
        };

        // Version 3 != 5, so should NOT publish.
        assert!(!should_publish);

        // Now check with matching version.
        let started_version = 5;
        let should_publish = if let Some(doc) = documents.get(&uri) {
            doc.version == started_version
        } else {
            false
        };
        assert!(should_publish);
    }

    #[tokio::test]
    async fn closed_document_prevents_publish() {
        let documents: Arc<DashMap<Uri, DocumentState>> = Arc::new(DashMap::new());
        let uri = Uri::from_file_path("/test/closed.yml").unwrap();

        // Document not in map (was closed).
        let should_publish = documents.get(&uri).is_some();
        assert!(!should_publish);
    }
}
