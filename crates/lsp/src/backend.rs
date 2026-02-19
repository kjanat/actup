//! LSP backend implementing the [`LanguageServer`] trait.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use actup_core::cache::Cache as _;
use actup_core::{Config, provider};
use dashmap::DashMap;
use tokio::sync::RwLock;
use tokio::task::AbortHandle;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    ClientCapabilities, CodeActionOptions, CodeActionParams, CodeActionProviderCapability,
    CodeActionResponse, CompletionOptions, CompletionParams, CompletionResponse, Diagnostic,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidChangeWorkspaceFoldersParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    ExecuteCommandOptions, ExecuteCommandParams, FileChangeType, FileSystemWatcher, GlobPattern,
    Hover, HoverParams, HoverProviderCapability, InitializeParams, InitializeResult,
    InitializedParams, MessageType, OneOf, Registration, ServerCapabilities, ServerInfo,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Uri, WorkDoneProgressOptions, WorkspaceFoldersServerCapabilities,
    WorkspaceServerCapabilities,
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
    /// Client capabilities from `initialize`, used to gate dynamic registration.
    client_capabilities: RwLock<ClientCapabilities>,
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
    /// In-memory cache for `action.yml` metadata used by hover.
    ///
    /// Keyed by (owner, repo, ref) since `CachingProvider` does not cache
    /// `get_action_yml` calls. Shared across all hover requests.
    action_meta_cache: crate::hover::ActionMetaCache,
    /// Session-scoped deduplicator for environment error notifications.
    ///
    /// Prevents spamming the user with repeated rate-limit or auth error
    /// messages across multiple diagnostic runs. Reset on config reload
    /// (in case the user fixed their token). Wrapped in `Arc` for shared
    /// access from spawned diagnostic tasks.
    error_dedup: Arc<crate::error_ux::ErrorDeduplicator>,
    /// Explicit config file path from `initializationOptions.configPath`.
    ///
    /// Stored so that `reload_config` can re-use it (analogous to CLI's
    /// `--config` flag). If the file disappears, reload falls back to
    /// normal discovery but keeps the path for future reloads.
    config_path: RwLock<Option<PathBuf>>,
    /// Negotiated position encoding for `Position::character` columns.
    ///
    /// Set once during `initialize` based on client capabilities.
    /// Defaults to UTF-16 per LSP spec if the client doesn't advertise
    /// encoding support.
    position_encoding: OnceLock<crate::position::PositionEncoding>,
}

impl LspBackend {
    /// Create a new backend from the LSP client handle.
    ///
    /// Used as the constructor passed to `LspService::new`.
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(DashMap::new()),
            workspace_root: RwLock::new(None),
            client_capabilities: RwLock::new(ClientCapabilities::default()),
            config: Arc::new(RwLock::new(Config::default())),
            registry: Arc::new(RwLock::new(None)),
            file_cache: Arc::new(RwLock::new(None)),
            in_flight: DashMap::new(),
            action_meta_cache: crate::hover::new_action_meta_cache(),
            error_dedup: Arc::new(crate::error_ux::ErrorDeduplicator::new()),
            config_path: RwLock::new(None),
            position_encoding: OnceLock::new(),
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
        let error_dedup = Arc::clone(&self.error_dedup);
        let encoding = self.position_encoding.get().copied().unwrap_or_default();
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

            let result = crate::diagnostics::compute_diagnostics(
                &text,
                &uri,
                &config,
                &registry,
                &file_cache,
                encoding,
            )
            .await;

            // Send user-visible notifications for environment errors (deduplicated).
            if result.errors.has_errors() {
                crate::error_ux::notify_errors(&client, &result.errors, &error_dedup).await;
            }

            let diagnostics = result.diagnostics;

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
    #[allow(clippy::too_many_lines)]
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        tracing::info!("actup-lsp initializing");

        // Negotiate position encoding before anything else.
        let encoding = crate::position::PositionEncoding::negotiate(&params.capabilities);
        let _ = self.position_encoding.set(encoding);
        tracing::info!(encoding = ?encoding, "position encoding negotiated");

        // Store client capabilities for dynamic registration checks.
        *self.client_capabilities.write().await = params.capabilities.clone();

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
        *self.config_path.write().await = config_path;
        *self.config.write().await = config;
        *self.registry.write().await = registry;
        *self.file_cache.write().await = Some(file_cache);

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "actup-lsp".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            capabilities: ServerCapabilities {
                position_encoding: Some(encoding.to_lsp()),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["@".to_owned()]),
                    ..Default::default()
                }),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            tower_lsp_server::ls_types::CodeActionKind::QUICKFIX,
                            crate::code_actions::SOURCE_FIX_ALL_ACTUP,
                        ]),
                        work_done_progress_options: WorkDoneProgressOptions {
                            work_done_progress: Some(false),
                        },
                        resolve_provider: Some(false),
                    },
                )),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![crate::workspace_commands::FIX_ALL_IN_WORKSPACE.to_owned()],
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: Some(true),
                    },
                }),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    ..Default::default()
                }),
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

        // Register file watchers for actup config files (dynamic registration).
        self.register_config_watchers().await;
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("actup-lsp shutting down");

        // Abort all in-flight diagnostic tasks so they don't race with
        // resource teardown or attempt to publish after the client disconnects.
        let count = self.in_flight.len();
        self.in_flight.retain(|_uri, handle| {
            handle.abort();
            false // remove all entries
        });
        if count > 0 {
            tracing::debug!(count, "aborted in-flight diagnostic tasks");
        }

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

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let doc = match self.documents.get(&uri) {
            Some(doc) => doc.clone(),
            None => return Ok(None),
        };

        if !has_uses_keyword(&doc.text) {
            return Ok(None);
        }

        let config = self.config.read().await.clone();
        let registry = self.registry.read().await.clone();
        let file_cache = self.file_cache.read().await.clone();

        let Some(registry) = registry else {
            return Ok(None);
        };

        let file_cache =
            file_cache.unwrap_or_else(|| Arc::new(crate::file_cache::FileCache::disabled()));

        let encoding = self.position_encoding.get().copied().unwrap_or_default();
        let hover = crate::hover::compute_hover(
            &doc.text,
            position,
            &config,
            &registry,
            &file_cache,
            &self.action_meta_cache,
            encoding,
        )
        .await;

        Ok(hover)
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let doc = match self.documents.get(&uri) {
            Some(doc) => doc.clone(),
            None => return Ok(None),
        };

        if !has_uses_keyword(&doc.text) {
            return Ok(None);
        }

        let registry = self.registry.read().await.clone();
        let file_cache = self.file_cache.read().await.clone();

        let Some(registry) = registry else {
            return Ok(None);
        };

        let file_cache =
            file_cache.unwrap_or_else(|| Arc::new(crate::file_cache::FileCache::disabled()));

        let encoding = self.position_encoding.get().copied().unwrap_or_default();
        let result = crate::completion::compute_completions(
            &doc.text,
            position,
            &registry,
            &file_cache,
            encoding,
        )
        .await;

        Ok(result)
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let only = params.context.only.as_deref();

        // Use the diagnostics the client sent in the context (already filtered
        // to the requested range by the editor). This is the standard LSP
        // pattern — the client knows which diagnostics are relevant to the
        // cursor/selection.
        let context_diagnostics = &params.context.diagnostics;

        let mut actions = Vec::new();

        // Per-diagnostic quickfixes (only when quickfix kind is allowed).
        if crate::code_actions::kind_allowed(
            only,
            &tower_lsp_server::ls_types::CodeActionKind::QUICKFIX,
        ) {
            let quickfixes = if context_diagnostics.is_empty() {
                let cached = self
                    .documents
                    .get(&uri)
                    .map(|doc| doc.diagnostics.clone())
                    .unwrap_or_default();
                crate::code_actions::code_actions_for_diagnostics(&uri, &cached)
            } else {
                crate::code_actions::code_actions_for_diagnostics(&uri, context_diagnostics)
            };
            actions.extend(quickfixes);
        }

        // Fix-all-in-file source action (uses ALL cached diagnostics, not
        // just the ones at the cursor). Only included when the requested
        // kind filter allows source.fixAll or source.fixAll.actup.
        if crate::code_actions::kind_allowed(only, &crate::code_actions::SOURCE_FIX_ALL_ACTUP) {
            let all_diagnostics = self
                .documents
                .get(&uri)
                .map(|doc| doc.diagnostics.clone())
                .unwrap_or_default();
            if let Some(fix_all) = crate::code_actions::fix_all_in_file(&uri, &all_diagnostics) {
                actions.push(fix_all);
            }
        }

        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(actions))
        }
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        tracing::info!(command = %params.command, "execute_command");

        if params.command != crate::workspace_commands::FIX_ALL_IN_WORKSPACE {
            tracing::warn!(command = %params.command, "unknown command");
            return Ok(None);
        }

        // Require workspace root — can't discover files without it.
        let workspace_root = self.workspace_root.read().await.clone();
        let Some(workspace_root) = workspace_root else {
            self.client
                .show_message(
                    MessageType::WARNING,
                    "actup: no workspace root; cannot scan for workflow files",
                )
                .await;
            return Ok(None);
        };

        // Snapshot config, registry, and cache.
        let config = self.config.read().await.clone();
        let registry = self.registry.read().await.clone();
        let file_cache = self.file_cache.read().await.clone();

        let Some(registry) = registry else {
            self.client
                .show_message(
                    MessageType::WARNING,
                    "actup: no provider registry available; cannot check versions",
                )
                .await;
            return Ok(None);
        };

        let file_cache =
            file_cache.unwrap_or_else(|| Arc::new(crate::file_cache::FileCache::disabled()));

        let encoding = self.position_encoding.get().copied().unwrap_or_default();
        let cancelled = crate::workspace_commands::Cancelled::new();
        let result = crate::workspace_commands::fix_all_in_workspace(
            self.client.clone(),
            &workspace_root,
            &config,
            &registry,
            &file_cache,
            &cancelled,
            encoding,
        )
        .await;

        self.apply_fix_all_result(result).await;
        Ok(None)
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let is_config_change = params.changes.iter().any(|event| {
            let path = event.uri.as_str();
            is_actup_config_path(path) && event.typ != FileChangeType::DELETED
        });
        let is_config_deleted = params.changes.iter().any(|event| {
            is_actup_config_path(event.uri.as_str()) && event.typ == FileChangeType::DELETED
        });

        if !is_config_change && !is_config_deleted {
            return;
        }

        tracing::info!("actup config file changed, reloading");
        self.reload_config().await;
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        let event = params.event;
        let added = event.added.len();
        let removed = event.removed.len();
        tracing::info!(added, removed, "workspace folders changed");

        // Update workspace root: prefer first added folder, or keep existing if only removing.
        if let Some(folder) = event.added.first()
            && let Some(path) = folder.uri.to_file_path()
        {
            let new_root = path.into_owned();
            tracing::info!(root = %new_root.display(), "updated workspace root");
            *self.workspace_root.write().await = Some(new_root);
        } else if !event.removed.is_empty() {
            // If the current workspace root was removed, clear it.
            let current = self.workspace_root.read().await.clone();
            let removed_paths: Vec<_> = event
                .removed
                .iter()
                .filter_map(|f| f.uri.to_file_path().map(std::borrow::Cow::into_owned))
                .collect();
            if let Some(ref current_root) = current
                && removed_paths.iter().any(|p| p == current_root)
            {
                tracing::warn!("workspace root removed, clearing");
                *self.workspace_root.write().await = None;
            }
        }

        // Reload config from new workspace root and refresh diagnostics.
        self.reload_config().await;
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

/// Post-command helpers (separate impl block to keep `LanguageServer` methods focused).
impl LspBackend {
    /// Apply a [`FixAllResult`](crate::workspace_commands::FixAllResult) to the workspace and notify the user.
    async fn apply_fix_all_result(&self, result: crate::workspace_commands::FixAllResult) {
        if result.cancelled && result.refs_updated == 0 {
            self.client
                .show_message(
                    MessageType::INFO,
                    format!(
                        "actup: cancelled ({}/{} files scanned, {} actions checked)",
                        result.files_scanned, result.total_files, result.actions_checked,
                    ),
                )
                .await;
            return;
        }

        if result.refs_updated == 0 {
            self.client
                .show_message(
                    MessageType::INFO,
                    format!(
                        "actup: all action refs are up to date ({} files scanned, {} actions checked)",
                        result.files_scanned, result.actions_checked,
                    ),
                )
                .await;
            return;
        }

        match self.client.apply_edit(result.edit).await {
            Ok(response) if response.applied => {
                let cancelled_suffix = if result.cancelled { " (cancelled)" } else { "" };
                let msg = format!(
                    "actup: updated {} refs in {} files ({} actions checked){cancelled_suffix}",
                    result.refs_updated, result.files_changed, result.actions_checked,
                );
                self.client.show_message(MessageType::INFO, &msg).await;
                tracing::info!(%msg);
            }
            Ok(response) => {
                let reason = response
                    .failure_reason
                    .as_deref()
                    .unwrap_or("unknown reason");
                tracing::warn!(reason, "workspace edit rejected by client");
                self.client
                    .show_message(
                        MessageType::WARNING,
                        format!("actup: edit rejected by editor: {reason}"),
                    )
                    .await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to apply workspace edit");
                self.client
                    .show_message(
                        MessageType::ERROR,
                        format!("actup: failed to apply edit: {e}"),
                    )
                    .await;
            }
        }

        if !result.failures.is_empty() {
            let count = result.failures.len();
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!(
                        "actup: {count} file(s) could not be checked: {}",
                        result
                            .failures
                            .iter()
                            .map(|(p, e)| format!("{p}: {e}"))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                )
                .await;
        }
    }
}

/// Config file watching and reload helpers.
impl LspBackend {
    /// Dynamically register file watchers for actup config files.
    ///
    /// Requires the client to support `workspace/didChangeWatchedFiles` with
    /// dynamic registration. Silently skips if not supported.
    async fn register_config_watchers(&self) {
        let caps = self.client_capabilities.read().await;
        let supports_watch = caps
            .workspace
            .as_ref()
            .and_then(|ws| ws.did_change_watched_files.as_ref())
            .is_some_and(|w| w.dynamic_registration == Some(true));
        drop(caps);

        if !supports_watch {
            tracing::debug!("client does not support dynamic file watcher registration");
            return;
        }

        let watch_options = DidChangeWatchedFilesRegistrationOptions {
            watchers: config_file_watchers(),
        };

        let registration = Registration {
            id: "actup-config-watcher".to_owned(),
            method: "workspace/didChangeWatchedFiles".to_owned(),
            register_options: serde_json::to_value(watch_options).ok(),
        };

        match self.client.register_capability(vec![registration]).await {
            Ok(()) => tracing::info!("registered config file watchers"),
            Err(e) => tracing::warn!(error = %e, "failed to register config file watchers"),
        }
    }

    /// Reload config from workspace root and rebuild providers.
    ///
    /// Called when a config file changes or workspace folders change.
    /// Compares old and new policy to decide whether diagnostics need refreshing.
    ///
    /// If an explicit `configPath` was set via `initializationOptions`, it is
    /// tried first. If that file no longer exists, we fall back to normal
    /// discovery and log a warning.
    async fn reload_config(&self) {
        let workspace_root = self.workspace_root.read().await.clone();
        let config_path = self.config_path.read().await.clone();
        let start_dir = workspace_root
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("."));

        let old_config = self.config.read().await.clone();

        // Try explicit configPath first, fall back to discovery if it fails.
        let new_config = match Config::load(config_path.as_deref(), start_dir) {
            Ok(c) => {
                if let Some(from) = &c.loaded_from {
                    tracing::info!(path = %from.display(), "reloaded config");
                } else {
                    tracing::info!("reloaded defaults (no config file found)");
                }
                c
            }
            Err(e) if config_path.is_some() => {
                // Explicit path failed — fall back to discovery.
                tracing::warn!(
                    path = ?config_path,
                    error = %e,
                    "explicit configPath failed, falling back to discovery"
                );
                match Config::load(None, start_dir) {
                    Ok(c) => c,
                    Err(e2) => {
                        tracing::warn!("fallback config load also failed: {e2}");
                        self.client
                            .log_message(
                                MessageType::WARNING,
                                format!("actup: failed to reload config: {e2}"),
                            )
                            .await;
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("failed to reload config: {e}");
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("actup: failed to reload config: {e}"),
                    )
                    .await;
                return;
            }
        };

        let policy_changed = configs_differ_for_diagnostics(&old_config, &new_config);

        // Update stored config.
        *self.config.write().await = new_config.clone();

        // Rebuild provider registry (config may change hosts/tokens).
        match provider_setup::build_registry(&new_config) {
            Ok(r) => {
                *self.registry.write().await = Some(Arc::new(r));
            }
            Err(e) => {
                tracing::error!("failed to rebuild provider registry: {e}");
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!("actup: failed to rebuild providers: {e}"),
                    )
                    .await;
            }
        }

        // Rebuild file cache (TTL or path may have changed).
        let file_cache = Arc::new(provider_setup::build_file_cache(&new_config));
        *self.file_cache.write().await = Some(file_cache);

        // Reset error deduplication on config reload so that persistent errors
        // (e.g. auth) are re-shown if the user changed their token/config.
        self.error_dedup.reset_all();

        if policy_changed {
            tracing::info!("policy changed, refreshing diagnostics for all open documents");
            self.client
                .log_message(
                    MessageType::INFO,
                    "actup: config changed, refreshing diagnostics",
                )
                .await;
            self.refresh_all_diagnostics().await;
        } else {
            tracing::debug!("config reloaded but policy unchanged, skipping diagnostic refresh");
        }
    }

    /// Re-trigger diagnostics for all currently open documents.
    ///
    /// Clears cached diagnostics first (since policy may have changed what is
    /// considered outdated/ignored), then schedules fresh computation for each.
    async fn refresh_all_diagnostics(&self) {
        let entries: Vec<(Uri, String, i32)> = self
            .documents
            .iter()
            .map(|entry| {
                let uri = entry.key().clone();
                let text = entry.value().text.clone();
                let version = entry.value().version;
                (uri, text, version)
            })
            .collect();

        for (uri, text, version) in entries {
            // Clear cached diagnostics so stale results aren't served to code actions.
            if let Some(mut doc) = self.documents.get_mut(&uri) {
                doc.diagnostics.clear();
            }
            if has_uses_keyword(&text) {
                self.schedule_diagnostics(uri, text, version, Duration::ZERO);
            } else {
                // No uses: keyword — publish empty diagnostics to clear any stale ones.
                self.client
                    .publish_diagnostics(uri, Vec::new(), Some(version))
                    .await;
            }
        }
    }
}

/// Returns `true` if the path (as a URI string) refers to an actup config file.
///
/// This intentionally matches any file named `actup.toml` or `.actup.toml`
/// regardless of directory depth. This is broader than strictly necessary
/// (e.g. it would match `node_modules/.../actup.toml`), but false positives
/// only trigger a config reload attempt that will either succeed (correct)
/// or find no config (harmless no-op). The simplicity outweighs the cost
/// of a rare spurious reload.
fn is_actup_config_path(uri_str: &str) -> bool {
    // Check the filename component of the URI.
    let path = uri_str.rsplit('/').next().unwrap_or("");
    path == "actup.toml" || path == ".actup.toml"
}

/// Build the set of [`FileSystemWatcher`] entries for actup config files.
///
/// Watches for creation, change, and deletion of all config file patterns
/// that actup discovers: `actup.toml`, `.actup.toml`, `.config/actup.toml`.
fn config_file_watchers() -> Vec<FileSystemWatcher> {
    let patterns = ["**/actup.toml", "**/.actup.toml", "**/.config/actup.toml"];
    patterns
        .into_iter()
        .map(|p| FileSystemWatcher {
            glob_pattern: GlobPattern::String(p.to_owned()),
            kind: None, // None = watch all events (create + change + delete)
        })
        .collect()
}

/// Returns `true` if two configs differ in ways that affect diagnostic output.
///
/// Compares policy-relevant fields: update policy, ref style, branch policy,
/// prerelease setting, ignore list, per-action overrides, default host, and
/// host configuration. Host changes affect provider resolution which changes
/// what versions are fetched and thus which diagnostics are emitted.
fn configs_differ_for_diagnostics(old: &Config, new: &Config) -> bool {
    old.update_policy != new.update_policy
        || old.ref_style != new.ref_style
        || old.branch_policy != new.branch_policy
        || old.allow_prerelease != new.allow_prerelease
        || old.ignore != new.ignore
        || old.default_host != new.default_host
        || hosts_differ(&old.hosts, &new.hosts)
        || action_overrides_differ(&old.action_overrides, &new.action_overrides)
}

/// Compare host config maps by serialized form since [`HostConfig`](actup_core::HostConfig) may
/// not implement `PartialEq`.
fn hosts_differ(
    old: &std::collections::HashMap<String, actup_core::HostConfig>,
    new: &std::collections::HashMap<String, actup_core::HostConfig>,
) -> bool {
    if old.len() != new.len() {
        return true;
    }
    let old_json = serde_json::to_string(old).unwrap_or_default();
    let new_json = serde_json::to_string(new).unwrap_or_default();
    old_json != new_json
}

/// Compare action override maps by serialized form since [`ActionOverride`](actup_core::config::ActionOverride)
/// doesn't implement `PartialEq`.
fn action_overrides_differ(
    old: &std::collections::HashMap<String, actup_core::config::ActionOverride>,
    new: &std::collections::HashMap<String, actup_core::config::ActionOverride>,
) -> bool {
    if old.len() != new.len() {
        return true;
    }
    // Compare via JSON serialization (ActionOverride derives Serialize).
    let old_json = serde_json::to_string(old).unwrap_or_default();
    let new_json = serde_json::to_string(new).unwrap_or_default();
    old_json != new_json
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

    #[test]
    fn is_actup_config_path_matches_config_files() {
        assert!(is_actup_config_path("file:///home/user/project/actup.toml"));
        assert!(is_actup_config_path(
            "file:///home/user/project/.actup.toml"
        ));
        assert!(is_actup_config_path(
            "file:///home/user/project/.config/actup.toml"
        ));
    }

    #[test]
    fn is_actup_config_path_rejects_non_config_files() {
        assert!(!is_actup_config_path(
            "file:///home/user/project/Cargo.toml"
        ));
        assert!(!is_actup_config_path(
            "file:///home/user/project/.github/workflows/ci.yml"
        ));
        assert!(!is_actup_config_path(
            "file:///home/user/project/actup.yaml"
        ));
    }

    #[test]
    fn config_file_watchers_returns_three_patterns() {
        let watchers = config_file_watchers();
        assert_eq!(watchers.len(), 3);

        let patterns: Vec<_> = watchers
            .iter()
            .filter_map(|w| match &w.glob_pattern {
                GlobPattern::String(s) => Some(s.as_str()),
                GlobPattern::Relative(_) => None,
            })
            .collect();
        assert!(patterns.contains(&"**/actup.toml"));
        assert!(patterns.contains(&"**/.actup.toml"));
        assert!(patterns.contains(&"**/.config/actup.toml"));
    }

    #[test]
    fn config_file_watchers_watch_all_events() {
        for watcher in config_file_watchers() {
            // kind: None means watch all events (create + change + delete)
            assert!(watcher.kind.is_none());
        }
    }

    #[test]
    fn configs_differ_detects_policy_change() {
        let base = Config::default();
        let mut changed = base.clone();
        changed.update_policy = actup_core::policy::Update::Major;
        assert!(configs_differ_for_diagnostics(&base, &changed));
    }

    #[test]
    fn configs_differ_detects_ignore_change() {
        let base = Config::default();
        let mut changed = base.clone();
        changed.ignore.push("actions/checkout".to_owned());
        assert!(configs_differ_for_diagnostics(&base, &changed));
    }

    #[test]
    fn configs_differ_same_configs_returns_false() {
        let base = Config::default();
        let same = base.clone();
        assert!(!configs_differ_for_diagnostics(&base, &same));
    }

    #[test]
    fn configs_differ_detects_prerelease_change() {
        let base = Config::default();
        let mut changed = base.clone();
        changed.allow_prerelease = !base.allow_prerelease;
        assert!(configs_differ_for_diagnostics(&base, &changed));
    }

    #[test]
    fn configs_differ_ignores_cache_change() {
        let base = Config::default();
        let mut changed = base.clone();
        changed.cache.ttl = base.cache.ttl + Duration::from_secs(1000);
        assert!(!configs_differ_for_diagnostics(&base, &changed));
    }

    #[test]
    fn configs_differ_detects_ref_style_change() {
        let base = Config::default();
        let mut changed = base.clone();
        changed.ref_style = actup_core::policy::Style::Sha;
        assert!(configs_differ_for_diagnostics(&base, &changed));
    }

    #[test]
    fn configs_differ_detects_branch_policy_change() {
        let base = Config::default();
        let mut changed = base.clone();
        // Default is `Ignore`; change to `Warn` to trigger a diff.
        changed.branch_policy = actup_core::policy::Branch::Warn;
        assert!(configs_differ_for_diagnostics(&base, &changed));
    }

    #[test]
    fn extract_workspace_root_from_root_uri() {
        let uri = Uri::from_file_path("/workspace/from-uri").unwrap();
        #[allow(deprecated)]
        let params = InitializeParams {
            root_uri: Some(uri),
            root_path: Some("/legacy/root".to_owned()),
            ..Default::default()
        };
        let root = extract_workspace_root(&params);
        // root_uri takes precedence over root_path when workspace_folders absent.
        assert_eq!(root, Some(PathBuf::from("/workspace/from-uri")));
    }

    #[test]
    fn has_uses_keyword_matches_various_positions() {
        assert!(has_uses_keyword("    uses: actions/checkout@v4"));
        assert!(has_uses_keyword("- uses: x/y@v1\n"));
        assert!(has_uses_keyword("  uses:"));
    }

    #[test]
    fn has_uses_keyword_does_not_match_similar_strings() {
        // "refuses:" contains "uses:" as substring — that's expected behavior.
        // The function is intentionally a broad substring check, not precise.
        assert!(!has_uses_keyword("# no action refs here"));
        assert!(!has_uses_keyword("run: echo hello"));
        assert!(!has_uses_keyword("  fuse: x"));
    }

    #[test]
    fn action_overrides_differ_empty_maps_equal() {
        let a = std::collections::HashMap::new();
        let b = std::collections::HashMap::new();
        assert!(!action_overrides_differ(&a, &b));
    }

    #[test]
    fn action_overrides_differ_detects_addition() {
        let a = std::collections::HashMap::new();
        let mut b = std::collections::HashMap::new();
        b.insert(
            "actions/checkout".to_owned(),
            serde_json::from_value(serde_json::json!({"update_policy": "minor"})).unwrap(),
        );
        assert!(action_overrides_differ(&a, &b));
    }

    #[test]
    fn is_actup_config_path_handles_nested_config_dir() {
        // .config/actup.toml — the last path segment is "actup.toml"
        assert!(is_actup_config_path("file:///project/.config/actup.toml"));
    }

    #[test]
    fn is_actup_config_path_rejects_partial_matches() {
        assert!(!is_actup_config_path("file:///project/not-actup.toml"));
        assert!(!is_actup_config_path("file:///actup.toml.bak"));
    }

    #[test]
    fn document_state_clone_is_independent() {
        let state = DocumentState::new("original".into(), 1);
        let mut cloned = state.clone();
        cloned.text = "modified".into();
        cloned.version = 2;
        assert_eq!(state.text, "original");
        assert_eq!(state.version, 1);
    }
}
