//! LSP backend implementing the [`LanguageServer`] trait.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsStr,
    path::{Component, Path, PathBuf, Prefix},
    sync::{Arc, OnceLock},
    time::Duration,
};

use actup_core::{
    Config,
    cache::Cache as _,
    provider,
    rewrite::{PinMarker, Rewrite},
};
use dashmap::DashMap;
use tokio::{
    sync::{Mutex, RwLock},
    task::AbortHandle,
};
use tower_lsp_server::{
    Client, LanguageServer,
    jsonrpc::Result,
    ls_types::{
        ClientCapabilities, CodeActionOptions, CodeActionParams, CodeActionProviderCapability,
        CodeActionResponse, CompletionOptions, CompletionParams, CompletionResponse, Diagnostic,
        DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
        DidChangeWatchedFilesRegistrationOptions, DidChangeWorkspaceFoldersParams,
        DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
        ExecuteCommandOptions, ExecuteCommandParams, FileSystemWatcher, GlobPattern, Hover,
        HoverParams, HoverProviderCapability, InitializeParams, InitializeResult,
        InitializedParams, MessageType, OneOf, Registration, RelativePattern, ServerCapabilities,
        ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
        TextDocumentSyncSaveOptions, Unregistration, Uri, WorkDoneProgressOptions,
        WorkspaceFoldersServerCapabilities, WorkspaceServerCapabilities,
    },
};

use crate::{file_cache::FileCache, provider_setup};

const CONFIG_WATCHER_ID: &str = "actup-config-watcher";
const DID_CHANGE_WATCHED_FILES: &str = "workspace/didChangeWatchedFiles";

/// Debounce delay for diagnostic computation on document changes.
///
/// Prevents flooding providers with API requests when the user types rapidly.
/// After each edit, we wait this long before starting version checks. If another
/// edit arrives before the delay expires, the previous pending check is cancelled.
const DEBOUNCE_DELAY: Duration = Duration::from_millis(300);

const SEARCH_DEBOUNCE_DELAY: Duration = Duration::from_millis(250);

/// State tracked per open document.
///
/// Stores the full document text (since we use `TextDocumentSyncKind::FULL`),
/// the document version for staleness checks, and any cached diagnostics
/// from the most recent analysis pass.
#[derive(Debug, Clone)]
pub struct DocumentState {
    /// Full document text (replaced on every change notification).
    pub text: String,
    /// Monotonically increasing version from the editor.
    pub version: i32,
    /// Cached diagnostics from the last analysis. Empty until first check.
    pub diagnostics: Vec<Diagnostic>,
    /// Successful version lookups from the last diagnostics pass.
    pub version_sets: Arc<HashMap<actup_core::ActionRepoId, actup_core::version::Set>>,
}

impl DocumentState {
    /// Create initial state from a newly opened document.
    pub(crate) fn new(text: String, version: i32) -> Self {
        Self {
            text,
            version,
            diagnostics: Vec::new(),
            version_sets: Arc::new(HashMap::new()),
        }
    }
}

#[derive(Debug, Clone)]
enum ConfigState {
    Loaded(Box<Config>),
    Invalid(String),
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
    (!path_str.trim().is_empty()).then(|| PathBuf::from(path_str))
}

fn resolve_config_path(config_path: Option<&Path>, start_dir: &Path) -> Option<PathBuf> {
    config_path.map(|p| start_dir.join(p))
}

/// Build the LSP [`ServerCapabilities`] advertised during `initialize`.
///
/// Kept separate from the handler so each capability co-locates with a
/// comment about what implements it, not buried inside 100+ lines of
/// handler orchestration.
fn server_capabilities(encoding: crate::position::PositionEncoding) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(encoding.to_lsp()),
        // Full text sync: we re-parse the whole document on each change.
        // Incremental sync would require tracking ranges — not worth the
        // complexity for workflow files which are small.
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                ..Default::default()
            },
        )),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        // `@` triggers version completion on `owner/repo@<cursor>`.
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec!["@".to_owned()]),
            ..Default::default()
        }),
        // Quick-fix + source.fixAll.actup for the "Update outdated action"
        // flow. Resolve not needed since edits are computed eagerly.
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![
                tower_lsp_server::ls_types::CodeActionKind::QUICKFIX,
                tower_lsp_server::ls_types::CodeActionKind::REFACTOR,
                crate::code_actions::SOURCE_FIX_ALL_ACTUP,
            ]),
            work_done_progress_options: WorkDoneProgressOptions {
                work_done_progress: Some(false),
            },
            resolve_provider: Some(false),
        })),
        // The workspace-wide fix command reports progress, hence the flag.
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
    }
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
    /// Result of the latest config load (set during `initialize`, refreshed
    /// on config file changes). An invalid config pauses diagnostics.
    config: Arc<RwLock<ConfigState>>,
    /// Provider registry for version lookups (set during `initialize`).
    ///
    /// `None` until `initialize` completes. Providers are created from config
    /// and static registrations.
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
    search_debounce: crate::completion::SearchDebounce,
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
    /// `--config` flag).
    config_path: RwLock<Option<PathBuf>>,
    user_config: Option<PathBuf>,
    config_reload: Mutex<()>,
    config_watchers_registered: Mutex<bool>,
    /// Negotiated position encoding for `Position::character` columns.
    ///
    /// Set once during `initialize` based on client capabilities.
    /// Defaults to UTF-16 per LSP spec if the client doesn't advertise
    /// encoding support.
    position_encoding: OnceLock<crate::position::PositionEncoding>,
    /// Builds the provider registry and file cache from each loaded config.
    providers: provider_setup::Providers,
}

impl LspBackend {
    /// Create a new backend from the LSP client handle.
    ///
    /// Used as the constructor passed to `LspService::new`.
    #[cfg(not(test))]
    pub fn new(client: Client) -> Self {
        Self::with_providers(
            client,
            actup_core::user_config_path(),
            provider_setup::Providers::LIVE,
        )
    }

    /// Create a new backend that builds its registry and file cache with `providers`.
    pub fn with_providers(
        client: Client,
        user_config: Option<PathBuf>,
        providers: provider_setup::Providers,
    ) -> Self {
        Self {
            client,
            documents: Arc::new(DashMap::new()),
            workspace_root: RwLock::new(None),
            client_capabilities: RwLock::new(ClientCapabilities::default()),
            config: Arc::new(RwLock::new(ConfigState::Loaded(Box::default()))),
            registry: Arc::new(RwLock::new(None)),
            file_cache: Arc::new(RwLock::new(None)),
            in_flight: DashMap::new(),
            action_meta_cache: crate::hover::new_action_meta_cache(),
            search_debounce: crate::completion::SearchDebounce::new(SEARCH_DEBOUNCE_DELAY),
            error_dedup: Arc::new(crate::error_ux::ErrorDeduplicator::new()),
            config_path: RwLock::new(None),
            user_config,
            config_reload: Mutex::new(()),
            config_watchers_registered: Mutex::new(false),
            position_encoding: OnceLock::new(),
            providers,
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
            let (diagnostics, version_sets) = match config {
                ConfigState::Invalid(_) => (Vec::new(), Arc::new(HashMap::new())),
                ConfigState::Loaded(config) => 'loaded: {
                    let registry = registry.read().await.clone();
                    let file_cache = file_cache.read().await.clone();

                    let Some(registry) = registry else {
                        tracing::debug!("no provider registry; publishing empty diagnostics");
                        break 'loaded (Vec::new(), Arc::new(HashMap::new()));
                    };

                    let file_cache = file_cache
                        .unwrap_or_else(|| Arc::new(crate::file_cache::FileCache::disabled()));

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

                    (result.diagnostics, Arc::new(result.version_sets))
                }
            };

            // Update cached diagnostics in document state, checking for staleness.
            let should_publish = if let Some(mut doc) = documents.get_mut(&uri) {
                if doc.version == version {
                    doc.diagnostics.clone_from(&diagnostics);
                    doc.version_sets = Arc::clone(&version_sets);
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

        let config = match Config::load(
            resolve_config_path(config_path.as_deref(), start_dir).as_deref(),
            self.user_config.as_deref(),
            start_dir,
            workspace_root.as_deref(),
        ) {
            Ok(c) => {
                log_config_sources(&c, "loaded config");
                ConfigState::Loaded(Box::new(c))
            }
            Err(e) => self.invalid_config(&e).await,
        };

        let (registry, file_cache) = if let ConfigState::Loaded(config) = &config {
            // Build provider registry from config + compiled-in registrations.
            let registry = match (self.providers.registry)(config) {
                Ok(r) => {
                    if let Some(host) = r.default_host() {
                        tracing::info!(default_host = host, "provider registry built");
                    }
                    Some(Arc::new(r))
                }
                Err(e) => {
                    let e = actup_core::error::chain(&e);
                    tracing::error!("failed to build provider registry: {e}");
                    crate::error_ux::show_message(
                        &self.client,
                        MessageType::ERROR,
                        format!(
                            "actup: failed to initialize providers: {e}. Diagnostics are paused until the config is fixed."
                        ),
                    )
                    .await;
                    None
                }
            };

            // Initialize persistent file cache (shared with CLI).
            let file_cache = Arc::new((self.providers.file_cache)(config));
            tracing::info!(enabled = file_cache.is_enabled(), "file cache initialized");
            (registry, Some(file_cache))
        } else {
            (None, None)
        };

        // Store state for use by other handlers.
        *self.workspace_root.write().await = workspace_root;
        *self.config_path.write().await = config_path;
        *self.config.write().await = config;
        *self.registry.write().await = registry;
        *self.file_cache.write().await = file_cache;

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "actup-lsp".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            offset_encoding: None,
            capabilities: server_capabilities(encoding),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        let has_registry = self.registry.read().await.is_some();

        match &*self.config.read().await {
            ConfigState::Loaded(config) => {
                let policy = config.policy;
                let ignored = config.scan.ignore.len();
                tracing::info!(?policy, ignored, has_registry, "actup-lsp initialized");
            }
            ConfigState::Invalid(_) => {
                tracing::info!(has_registry, "actup-lsp initialized with an invalid config");
            }
        }

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
            // Yield so the aborts are processed before teardown continues.
            tokio::task::yield_now().await;
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

        let ConfigState::Loaded(config) = self.config.read().await.clone() else {
            return Ok(None);
        };
        let registry = self.registry.read().await.clone();
        let file_cache = self.file_cache.read().await.clone();

        let Some(registry) = registry else {
            return Ok(None);
        };

        let file_cache =
            file_cache.unwrap_or_else(|| Arc::new(crate::file_cache::FileCache::disabled()));

        let encoding = self.position_encoding.get().copied().unwrap_or_default();
        let ctx = crate::hover::HoverContext {
            config: &config,
            registry: &registry,
            file_cache: &file_cache,
            meta_cache: &self.action_meta_cache,
            encoding,
        };
        let hover = crate::hover::compute_hover(
            &doc.text,
            crate::diagnostics::uri_format(&uri),
            position,
            Some(doc.version_sets.as_ref()),
            &ctx,
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
            Some(doc.version_sets.as_ref()),
            encoding,
            &self.search_debounce,
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

        let Some((doc_version, text, cached)) = self
            .documents
            .get(&uri)
            .map(|doc| (Some(doc.version), doc.text.clone(), doc.diagnostics.clone()))
        else {
            return Ok(None);
        };
        let supports_doc_changes = self.supports_document_changes().await;
        let encoding = self.position_encoding.get().copied().unwrap_or_default();
        let document = crate::code_actions::DocumentText::new(
            &text,
            crate::diagnostics::uri_format(&uri),
            encoding,
        );

        // Per-diagnostic quickfixes (only when quickfix kind is allowed).
        if crate::code_actions::kind_allowed(
            only,
            &tower_lsp_server::ls_types::CodeActionKind::QUICKFIX,
        ) {
            let diagnostics = if context_diagnostics.is_empty() {
                &cached
            } else {
                context_diagnostics
            };
            actions.extend(
                document.retain(crate::code_actions::code_actions_for_diagnostics(
                    &uri,
                    doc_version,
                    diagnostics,
                    supports_doc_changes,
                )),
            );
        }

        // Fix-all-in-file source action (uses ALL cached diagnostics, not
        // just the ones at the cursor). Only included when the requested
        // kind filter allows source.fixAll or source.fixAll.actup.
        if crate::code_actions::kind_allowed(only, &crate::code_actions::SOURCE_FIX_ALL_ACTUP)
            && let Some(fix_all) = crate::code_actions::fix_all_in_file(
                &uri,
                doc_version,
                &document.fixable(&cached),
                supports_doc_changes,
            )
        {
            actions.extend(document.retain(vec![fix_all]));
        }

        // "Pin to {SHA, major, branch}" refactor actions for the ref under the
        // cursor — offered regardless of diagnostics.
        if crate::code_actions::kind_allowed(
            only,
            &tower_lsp_server::ls_types::CodeActionKind::REFACTOR,
        ) {
            actions.extend(
                document.retain(
                    self.pin_actions_at(
                        &uri,
                        params.range.start,
                        doc_version,
                        supports_doc_changes,
                    )
                    .await,
                ),
            );
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
            crate::error_ux::show_message(
                &self.client,
                MessageType::WARNING,
                "actup: no workspace root; cannot scan for workflow files",
            )
            .await;
            return Ok(None);
        };

        // Snapshot config, registry, and cache.
        let state = self.config.read().await.clone();
        let config = match state {
            ConfigState::Loaded(config) => config,
            ConfigState::Invalid(error) => {
                crate::error_ux::show_message(
                    &self.client,
                    MessageType::ERROR,
                    format!("actup: {error}"),
                )
                .await;
                return Ok(None);
            }
        };
        let registry = self.registry.read().await.clone();
        let file_cache = self.file_cache.read().await.clone();

        let Some(registry) = registry else {
            crate::error_ux::show_message(
                &self.client,
                MessageType::WARNING,
                "actup: no provider registry available; cannot check versions",
            )
            .await;
            return Ok(None);
        };

        let file_cache =
            file_cache.unwrap_or_else(|| Arc::new(crate::file_cache::FileCache::disabled()));

        let encoding = self.position_encoding.get().copied().unwrap_or_default();
        let supports_doc_changes = self.supports_document_changes().await;
        let cancelled = crate::workspace_commands::Cancelled::new();
        let ctx = crate::workspace_commands::ScanContext {
            workspace_root: &workspace_root,
            config: &config,
            registry: &registry,
            file_cache: &file_cache,
            documents: &self.documents,
            cancelled: &cancelled,
            supports_document_changes: supports_doc_changes,
            encoding,
        };
        let result =
            crate::workspace_commands::fix_all_in_workspace(self.client.clone(), &ctx).await;

        self.apply_fix_all_result(result).await;
        Ok(None)
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let watched = self.watched_configs().await;
        let is_config_event = params.changes.iter().any(|event| {
            event
                .uri
                .to_file_path()
                .is_some_and(|path| watched.contains(&path))
        });

        if !is_config_event {
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
        self.register_config_watchers().await;
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

/// Capability inspection helpers.
impl LspBackend {
    /// Check whether the client advertised `workspace.workspaceEdit.documentChanges`.
    ///
    /// When `true`, edits use `WorkspaceEdit.document_changes` with versioned
    /// document identifiers. When `false`, edits fall back to the unversioned
    /// `WorkspaceEdit.changes` map.
    async fn supports_document_changes(&self) -> bool {
        let caps = self.client_capabilities.read().await;
        caps.workspace
            .as_ref()
            .and_then(|ws| ws.workspace_edit.as_ref())
            .is_some_and(|we| we.document_changes == Some(true))
    }

    /// Build "Pin to {SHA, major, branch}" and unpin code actions for the ref at `position`.
    ///
    /// Returns an empty vec when there's no ref under the cursor or no provider.
    async fn pin_actions_at(
        &self,
        uri: &Uri,
        position: tower_lsp_server::ls_types::Position,
        doc_version: Option<i32>,
        supports_doc_changes: bool,
    ) -> Vec<tower_lsp_server::ls_types::CodeActionOrCommand> {
        let encoding = self.position_encoding.get().copied().unwrap_or_default();

        // Resolve the ref under the cursor and copy out what we need before any
        // await, so the `DashMap` guard isn't held across await points.
        let (action, version_sets) = {
            let Some(doc) = self.documents.get(uri) else {
                return Vec::new();
            };
            let Some(action) = crate::diagnostics::locate_action_at(
                &doc.text,
                crate::diagnostics::uri_format(uri),
                position,
                encoding,
            ) else {
                return Vec::new();
            };
            (action, Arc::clone(&doc.version_sets))
        };

        let Some(registry) = self.registry.read().await.clone() else {
            return Vec::new();
        };
        let repo_id = action.action_ref.to_repo_id().without_subpath();
        let Some(provider) = registry.find_for_repo(&repo_id) else {
            return Vec::new();
        };

        pin_actions_for(
            &action,
            version_sets.get(&repo_id),
            provider,
            uri,
            doc_version,
            supports_doc_changes,
        )
        .await
    }
}

async fn pin_actions_for(
    action: &crate::diagnostics::LocatedAction,
    cached: Option<&actup_core::version::Set>,
    provider: &dyn actup_core::Provider,
    uri: &Uri,
    doc_version: Option<i32>,
    supports_doc_changes: bool,
) -> Vec<tower_lsp_server::ls_types::CodeActionOrCommand> {
    let cached = cached.filter(|versions| versions.stale().is_none());
    let fetched = if cached.is_none() {
        let repo_id = action.action_ref.to_repo_id().without_subpath();
        provider
            .get_versions(&repo_id)
            .await
            .ok()
            .filter(|versions| versions.stale().is_none())
    } else {
        None
    };
    let versions = cached.or(fetched.as_ref());
    let targets = compute_pin_targets(action, versions, provider).await;
    let ref_range = tower_lsp_server::ls_types::Range {
        start: tower_lsp_server::ls_types::Position {
            line: action.line,
            character: action.col_start,
        },
        end: tower_lsp_server::ls_types::Position {
            line: action.line,
            character: action.col_end,
        },
    };
    crate::code_actions::pin_code_actions(
        uri,
        doc_version,
        supports_doc_changes,
        ref_range,
        action.trailing(versions),
        action.line_end(),
        &targets,
    )
}

/// Resolve the SHA, major-tag, default-branch and unpin targets for a ref.
///
/// Tags in `versions` resolve the SHA without a network call; branch refs (and
/// unlisted tags) fall back to a provider lookup. No-op targets (a target equal
/// to the current ref, or a SHA pin of an already-pinned ref) are dropped. The
/// unpin target needs a pin marker naming a fetched tag on the pinned commit.
async fn compute_pin_targets(
    action: &crate::diagnostics::LocatedAction,
    versions: Option<&actup_core::version::Set>,
    provider: &dyn actup_core::Provider,
) -> crate::code_actions::PinTargets {
    let action_ref = &action.action_ref;
    let ref_str = action_ref.ref_str();
    let repo_id = action_ref.to_repo_id().without_subpath();

    let sha = if action_ref.is_sha() {
        None
    } else {
        let cached = versions.and_then(|v| {
            v.semver_tags()
                .iter()
                .find(|t| t.version().to_string() == ref_str)
                .map(|t| t.sha().to_string())
        });
        match cached {
            Some(s) => Some((s, ref_str.to_string())),
            None => provider
                .resolve_ref_to_sha(&repo_id, ref_str)
                .await
                .ok()
                .map(|s| {
                    let label = versions
                        .filter(|_| action_ref.abbreviates(s.as_str()))
                        .and_then(|v| v.tag_on(s.as_str()))
                        .map_or_else(|| ref_str.to_string(), |t| t.version().to_string());
                    (s.as_str().to_string(), label)
                }),
        }
    };

    let major = major_target(action_ref, versions);

    let branch = provider
        .get_default_branch(&repo_id)
        .await
        .ok()
        .filter(|b| b.as_str() != ref_str);

    let unpin = versions
        .filter(|_| action_ref.is_sha())
        .and_then(|v| PinMarker::unpinnable(ref_str, &action.tail, v))
        .map(Rewrite::unpin);

    crate::code_actions::PinTargets {
        sha,
        major,
        branch,
        unpin,
        current_is_sha: action_ref.is_sha(),
    }
}

/// The "Pin to major" target for `action_ref`, or `None` when it's a no-op or
/// no major can be determined.
///
/// Prefers the major of the current ref (parsed for tags, looked up by SHA for
/// pins). Falls back to the repo's latest major so the action is offered
/// consistently even for branch refs or SHAs pinned off a branch — where the
/// current ref carries no version of its own.
fn major_target(
    action_ref: &actup_core::ActionRef,
    versions: Option<&actup_core::version::Set>,
) -> Option<String> {
    let ref_str = action_ref.ref_str();
    let current_major = if action_ref.is_sha() {
        versions.and_then(|v| v.tag_on(ref_str).and_then(|t| t.version().major()))
    } else {
        actup_core::version::Version::parse(ref_str).major()
    };
    current_major
        .or_else(|| versions.and_then(|v| v.latest().and_then(|l| l.version().major())))
        .map(|m| format!("v{m}"))
        .filter(|m| m.as_str() != ref_str)
}

/// Post-command helpers (separate impl block to keep `LanguageServer` methods focused).
impl LspBackend {
    /// Apply a [`FixAllResult`](crate::workspace_commands::FixAllResult) to the workspace and notify the user.
    async fn apply_fix_all_result(&self, result: crate::workspace_commands::FixAllResult) {
        if result.cancelled && result.refs_updated == 0 {
            crate::error_ux::show_message(
                &self.client,
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
            crate::error_ux::show_message(
                &self.client,
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
                crate::error_ux::show_message(&self.client, MessageType::INFO, &msg).await;
                tracing::info!(%msg);
            }
            Ok(response) => {
                let reason = response
                    .failure_reason
                    .as_deref()
                    .unwrap_or("unknown reason");
                tracing::warn!(reason, "workspace edit rejected by client");
                crate::error_ux::show_message(
                    &self.client,
                    MessageType::WARNING,
                    format!("actup: edit rejected by editor: {reason}"),
                )
                .await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to apply workspace edit");
                crate::error_ux::show_message(
                    &self.client,
                    MessageType::ERROR,
                    format!("actup: failed to apply edit: {e}"),
                )
                .await;
            }
        }

        if !result.failures.is_empty() {
            let count = result.failures.len();
            crate::error_ux::log_message(
                &self.client,
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
        let watch_caps = caps
            .workspace
            .as_ref()
            .and_then(|ws| ws.did_change_watched_files.as_ref());
        let supports_watch = watch_caps.is_some_and(|w| w.dynamic_registration == Some(true));
        let relative_patterns =
            watch_caps.is_some_and(|w| w.relative_pattern_support == Some(true));
        drop(caps);

        if !supports_watch {
            tracing::debug!("client does not support dynamic file watcher registration");
            return;
        }

        let mut registered = self.config_watchers_registered.lock().await;
        if *registered {
            let unregistration = Unregistration {
                id: CONFIG_WATCHER_ID.to_owned(),
                method: DID_CHANGE_WATCHED_FILES.to_owned(),
            };
            if let Err(e) = self
                .client
                .unregister_capability(vec![unregistration])
                .await
            {
                tracing::warn!(error = %e, "failed to unregister config file watchers");
                return;
            }
            *registered = false;
        }

        let watched = self.watched_configs().await;
        let watch_options = DidChangeWatchedFilesRegistrationOptions {
            watchers: config_file_watchers(&watched, relative_patterns),
        };

        let registration = Registration {
            id: CONFIG_WATCHER_ID.to_owned(),
            method: DID_CHANGE_WATCHED_FILES.to_owned(),
            register_options: serde_json::to_value(watch_options).ok(),
        };

        let result = self.client.register_capability(vec![registration]).await;
        *registered = result.is_ok();
        drop(registered);
        match result {
            Ok(()) => tracing::info!("registered config file watchers"),
            Err(e) => tracing::warn!(error = %e, "failed to register config file watchers"),
        }
    }

    async fn watched_configs(&self) -> WatchedConfigs {
        let workspace_root = self.workspace_root.read().await.clone();
        let start_dir = workspace_root.clone().unwrap_or_else(|| PathBuf::from("."));
        let explicit = resolve_config_path(self.config_path.read().await.as_deref(), &start_dir);
        WatchedConfigs {
            search_dirs: actup_core::repo_search_dirs(&start_dir, workspace_root.as_deref()),
            user: self.user_config.clone(),
            explicit: explicit.and_then(|p| std::path::absolute(p).ok()),
        }
    }

    async fn invalid_config(&self, error: &actup_core::Error) -> ConfigState {
        let error = actup_core::error::chain(error);
        tracing::error!("{error}");
        crate::error_ux::show_message(
            &self.client,
            MessageType::ERROR,
            format!("actup: {error}. Diagnostics are paused until the config parses."),
        )
        .await;
        ConfigState::Invalid(error)
    }

    /// Reload config from workspace root and rebuild providers.
    ///
    /// Called when a config file changes or workspace folders change.
    /// Compares old and new policy to decide whether diagnostics need refreshing.
    /// A load error pauses diagnostics until a later reload succeeds.
    async fn reload_config(&self) {
        let _reload = self.config_reload.lock().await;
        let workspace_root = self.workspace_root.read().await.clone();
        let config_path = self.config_path.read().await.clone();
        let start_dir = workspace_root
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("."));

        let old_state = self.config.read().await.clone();

        let new_config = match Config::load(
            resolve_config_path(config_path.as_deref(), start_dir).as_deref(),
            self.user_config.as_deref(),
            start_dir,
            workspace_root.as_deref(),
        ) {
            Ok(c) => {
                log_config_sources(&c, "reloaded config");
                c
            }
            Err(e) => {
                let invalid = self.invalid_config(&e).await;
                *self.config.write().await = invalid;
                *self.registry.write().await = None;
                *self.file_cache.write().await = None;
                self.refresh_all_diagnostics().await;
                return;
            }
        };

        let policy_changed = match &old_state {
            ConfigState::Loaded(old_config) => {
                configs_differ_for_diagnostics(old_config, &new_config)
            }
            ConfigState::Invalid(_) => true,
        };

        // Update stored config.
        *self.config.write().await = ConfigState::Loaded(Box::new(new_config.clone()));

        // Rebuild provider registry (config may change hosts/tokens).
        let registry = match (self.providers.registry)(&new_config) {
            Ok(r) => Some(Arc::new(r)),
            Err(e) => {
                let e = actup_core::error::chain(&e);
                tracing::error!("failed to rebuild provider registry: {e}");
                crate::error_ux::show_message(
                    &self.client,
                    MessageType::ERROR,
                    format!(
                        "actup: failed to rebuild providers: {e}. Diagnostics are paused until the config is fixed."
                    ),
                )
                .await;
                None
            }
        };
        *self.registry.write().await = registry;

        // Rebuild file cache (TTL or path may have changed).
        let file_cache = Arc::new((self.providers.file_cache)(&new_config));
        *self.file_cache.write().await = Some(file_cache);

        // Reset error deduplication on config reload so that persistent errors
        // (e.g. auth) are re-shown if the user changed their token/config.
        self.error_dedup.reset_all();

        if policy_changed {
            tracing::info!("policy changed, refreshing diagnostics for all open documents");
            crate::error_ux::log_message(
                &self.client,
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

fn log_config_sources(config: &Config, message: &str) {
    if config.sources.is_empty() {
        tracing::info!("{message} from defaults");
    }
    for source in &config.sources {
        tracing::info!(%source, "{message}");
    }
}

#[derive(Debug, Clone, Default)]
struct WatchedConfigs {
    search_dirs: Vec<PathBuf>,
    user: Option<PathBuf>,
    explicit: Option<PathBuf>,
}

impl WatchedConfigs {
    fn contains(&self, path: &Path) -> bool {
        let resolved = resolve_event_path(path);
        [Some(path), resolved.as_deref()]
            .into_iter()
            .flatten()
            .any(|path| self.lists(path))
    }

    fn lists(&self, path: &Path) -> bool {
        self.user.as_deref() == Some(path)
            || self.explicit.as_deref() == Some(path)
            || self.search_dirs.iter().any(|dir| {
                actup_core::iter_config_candidates(dir).any(|candidate| candidate == path)
            })
    }
}

fn resolve_event_path(path: &Path) -> Option<PathBuf> {
    if path.exists() {
        return Some(actup_core::canonical_path(path));
    }
    Some(actup_core::canonical_path(path.parent()?).join(path.file_name()?))
}

fn candidate_groups(base: &Path) -> BTreeMap<PathBuf, Vec<String>> {
    let mut groups: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for candidate in actup_core::iter_config_candidates(base) {
        if let (Some(dir), Some(name)) = (candidate.parent(), candidate.file_name()) {
            groups
                .entry(dir.to_path_buf())
                .or_default()
                .push(name.to_string_lossy().into_owned());
        }
    }
    groups
}

fn relative_watcher(base: &Path, pattern: String) -> Option<FileSystemWatcher> {
    Some(FileSystemWatcher {
        glob_pattern: GlobPattern::Relative(RelativePattern {
            base_uri: OneOf::Right(Uri::from_file_path(base)?),
            pattern,
        }),
        kind: None,
    })
}

fn file_watcher(file: &Path) -> Option<FileSystemWatcher> {
    let name = file.file_name()?.to_string_lossy().into_owned();
    relative_watcher(file.parent()?, name)
}

const fn string_watcher(glob: String) -> FileSystemWatcher {
    FileSystemWatcher {
        glob_pattern: GlobPattern::String(glob),
        kind: None,
    }
}

fn glob_escape(segment: &str) -> String {
    let mut escaped = String::with_capacity(segment.len());
    for c in segment.chars() {
        if matches!(c, '*' | '?' | '[' | '{' | '}') {
            escaped.extend(['[', c, ']']);
        } else {
            escaped.push(c);
        }
    }
    escaped
}

fn prefix_glob(prefix: Prefix<'_>) -> String {
    let unc = |server: &OsStr, share: &OsStr| {
        format!(
            "//{}/{}",
            glob_escape(&server.to_string_lossy()),
            glob_escape(&share.to_string_lossy())
        )
    };
    match prefix {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => format!(
            "[{}{}]:",
            char::from(letter.to_ascii_lowercase()),
            char::from(letter.to_ascii_uppercase())
        ),
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => unc(server, share),
        Prefix::DeviceNS(device) => format!("//./{}", glob_escape(&device.to_string_lossy())),
        Prefix::Verbatim(name) => format!("//[?]/{}", glob_escape(&name.to_string_lossy())),
    }
}

fn absolute_glob(path: &Path) -> String {
    let mut glob = String::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => glob.push_str(&prefix_glob(prefix.kind())),
            Component::RootDir => glob.push('/'),
            Component::CurDir | Component::ParentDir | Component::Normal(_) => {
                if !glob.is_empty() && !glob.ends_with('/') {
                    glob.push('/');
                }
                glob.push_str(&glob_escape(&component.as_os_str().to_string_lossy()));
            }
        }
    }
    glob
}

fn absolute_glob_in(dir: &Path, pattern: &str) -> String {
    let mut glob = absolute_glob(dir);
    if !glob.ends_with('/') {
        glob.push('/');
    }
    glob.push_str(pattern);
    glob
}

fn config_file_watchers(
    watched: &WatchedConfigs,
    relative_patterns: bool,
) -> Vec<FileSystemWatcher> {
    let search_dirs = watched.search_dirs.iter().map(PathBuf::as_path);
    let files = [&watched.user, &watched.explicit].into_iter().flatten();

    if !relative_patterns {
        let workspace = candidate_groups(Path::new(""))
            .into_iter()
            .map(|(dir, names)| {
                let mut segments = vec!["**".to_owned()];
                segments.extend(
                    dir.components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned()),
                );
                segments.push(format!("{{{}}}", names.join(",")));
                string_watcher(segments.join("/"))
            });
        let above_root = search_dirs
            .skip(1)
            .flat_map(|dir| candidate_groups(dir).into_iter())
            .map(|(dir, names)| {
                string_watcher(absolute_glob_in(&dir, &format!("{{{}}}", names.join(","))))
            });
        let files = files.map(|file| string_watcher(absolute_glob(file)));
        return workspace.chain(above_root).chain(files).collect();
    }

    let candidates = search_dirs
        .flat_map(|dir| candidate_groups(dir).into_iter())
        .filter_map(|(dir, names)| relative_watcher(&dir, format!("{{{}}}", names.join(","))));
    let files = files.filter_map(|file| file_watcher(file));
    candidates.chain(files).collect()
}

/// Returns `true` if two configs differ in ways that affect diagnostic output.
///
/// Compares policy-relevant fields: `[policy]`, the ignore list, `[[overrides]]`,
/// default host, and host configuration. Host changes affect provider resolution
/// which changes what versions are fetched and thus which diagnostics are emitted.
fn configs_differ_for_diagnostics(old: &Config, new: &Config) -> bool {
    old.policy != new.policy
        || old.scan.ignore != new.scan.ignore
        || old.overrides != new.overrides
        || old.default_host != new.default_host
        || hosts_differ(&old.hosts, &new.hosts)
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn debounce_delay_is_300ms() {
        assert_eq!(DEBOUNCE_DELAY, Duration::from_millis(300));
    }

    fn aref(ref_str: &str) -> actup_core::ActionRef {
        actup_core::ActionRef::simple("actions", "checkout", ref_str)
    }

    async fn sha_pin_target(ref_str: &str) -> Option<(String, String)> {
        let text = format!("    - uses: actions/checkout@{ref_str}\n");
        let position = tower_lsp_server::ls_types::Position {
            line: 0,
            character: 30,
        };
        let action = crate::diagnostics::locate_action_at(
            &text,
            actup_core::SourceFormat::Yaml,
            position,
            crate::position::PositionEncoding::Utf16,
        )?;
        let versions =
            crate::test_utils::simple_version_set("v4.2.0", crate::test_utils::DUMMY_SHA);
        let provider = crate::test_utils::MockProvider::new("github.com");
        compute_pin_targets(&action, Some(&versions), &provider)
            .await
            .sha
    }

    #[tokio::test]
    async fn sha_pin_target_labels_short_sha_with_its_tag() {
        let sha = crate::test_utils::DUMMY_SHA.to_string();
        assert_eq!(
            sha_pin_target("abc1234").await,
            Some((sha.clone(), "v4.2.0".to_string()))
        );
        assert_eq!(
            sha_pin_target("deadbee").await,
            Some((sha, "deadbee".to_string()))
        );
    }

    #[test]
    fn major_target_from_full_semver_tag() {
        let v = crate::test_utils::simple_version_set("v7.2.3", "a".repeat(40).as_str());
        assert_eq!(
            major_target(&aref("v7.2.3"), Some(&v)).as_deref(),
            Some("v7")
        );
    }

    #[test]
    fn major_target_skips_noop_for_major_ref() {
        let v = crate::test_utils::simple_version_set("v7.2.3", "a".repeat(40).as_str());
        // Already on `v7` — offering "pin to major v7" would be a no-op.
        assert_eq!(major_target(&aref("v7"), Some(&v)), None);
    }

    #[test]
    fn major_target_falls_back_to_latest_for_branch_pinned_sha() {
        // SHA pinned off `main` doesn't match any tag, but the repo's latest
        // major (v8) must still be offered — this is the inconsistency fix.
        let v = crate::test_utils::simple_version_set("v8.0.1", "b".repeat(40).as_str());
        let unrelated_sha = "c".repeat(40);
        assert_eq!(
            major_target(&aref(&unrelated_sha), Some(&v)).as_deref(),
            Some("v8")
        );
    }

    #[test]
    fn major_target_falls_back_to_latest_for_branch_ref() {
        let v = crate::test_utils::simple_version_set("v8.0.1", "b".repeat(40).as_str());
        assert_eq!(major_target(&aref("main"), Some(&v)).as_deref(), Some("v8"));
    }

    #[test]
    fn major_target_uses_matched_tag_major_for_sha() {
        // SHA that maps to v2.2.0 stays on its own major (v2), not the latest.
        let sha = "d".repeat(40);
        let v = crate::test_utils::simple_version_set("v2.2.0", &sha);
        assert_eq!(major_target(&aref(&sha), Some(&v)).as_deref(), Some("v2"));
    }

    async fn unpinned(
        line: &str,
        versions: &actup_core::version::Set,
    ) -> std::result::Result<Option<String>, Box<dyn std::error::Error>> {
        let encoding = crate::position::PositionEncoding::Utf16;
        let at = tower_lsp_server::ls_types::Position {
            line: 0,
            character: 30,
        };
        let action = crate::diagnostics::locate_action_at(
            line,
            actup_core::SourceFormat::Yaml,
            at,
            encoding,
        )
        .ok_or("no action under the cursor")?;
        let provider = crate::test_utils::MockProvider::new("github.com");
        let targets = compute_pin_targets(&action, Some(versions), &provider).await;
        if targets.unpin.is_none() {
            return Ok(None);
        }
        let uri = Uri::from_file_path("/test/ci.yml").ok_or("no uri")?;
        let ref_range = tower_lsp_server::ls_types::Range {
            start: tower_lsp_server::ls_types::Position {
                line: 0,
                character: action.col_start,
            },
            end: tower_lsp_server::ls_types::Position {
                line: 0,
                character: action.col_end,
            },
        };
        let only_unpin = crate::code_actions::PinTargets {
            sha: None,
            major: None,
            branch: None,
            unpin: targets.unpin,
            current_is_sha: true,
        };
        let actions = crate::code_actions::pin_code_actions(
            &uri,
            None,
            false,
            ref_range,
            action.trailing(Some(versions)),
            action.line_end(),
            &only_unpin,
        );
        let [tower_lsp_server::ls_types::CodeActionOrCommand::CodeAction(unpin)] =
            actions.as_slice()
        else {
            return Err(format!("expected one unpin action, got {actions:?}").into());
        };
        assert_eq!(
            unpin.kind,
            Some(tower_lsp_server::ls_types::CodeActionKind::REFACTOR)
        );
        let edits = unpin
            .edit
            .as_ref()
            .and_then(|edit| edit.changes.as_ref())
            .and_then(|changes| changes.get(&uri))
            .ok_or("unpin action has no edits")?;
        let text = crate::workspace_commands::apply_text_edits(line, edits, encoding)
            .ok_or("unpin edit does not fit")?;
        Ok(Some(format!("{}: {text}", unpin.title)))
    }

    const DEV_LINE: &str = "  - uses: actions/checkout@dev # main keep";

    async fn pin_texts(
        line: &str,
        cached: Option<&actup_core::version::Set>,
        provider: crate::test_utils::MockProvider,
    ) -> std::result::Result<(usize, Vec<(String, String)>), Box<dyn std::error::Error>> {
        let encoding = crate::position::PositionEncoding::Utf16;
        let action = crate::diagnostics::locate_action_at(
            line,
            actup_core::SourceFormat::Yaml,
            tower_lsp_server::ls_types::Position {
                line: 0,
                character: 28,
            },
            encoding,
        )
        .ok_or("no action under the cursor")?;
        let calls = provider.call_count();
        let uri = Uri::from_file_path("/test/ci.yml").ok_or("no uri")?;
        let actions = pin_actions_for(&action, cached, &provider, &uri, None, false).await;
        let written = actions
            .iter()
            .filter_map(|action| match action {
                tower_lsp_server::ls_types::CodeActionOrCommand::CodeAction(action) => {
                    let edits = action.edit.as_ref()?.changes.as_ref()?.get(&uri)?;
                    let text = crate::workspace_commands::apply_text_edits(line, edits, encoding)?;
                    Some((action.title.clone(), text))
                }
                tower_lsp_server::ls_types::CodeActionOrCommand::Command(_) => None,
            })
            .collect();
        Ok((calls.load(std::sync::atomic::Ordering::SeqCst), written))
    }

    fn dev_and_main() -> actup_core::version::Set {
        let mut versions = actup_core::version::Set::new();
        versions.set_branches(std::collections::HashMap::from([
            ("main".to_string(), "c".repeat(40)),
            ("dev".to_string(), "d".repeat(40)),
        ]));
        versions
    }

    #[tokio::test]
    async fn pin_actions_read_labels_from_the_versions_they_use()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dev = "d".repeat(40);
        let listed = |pinned: &str, branch: &str| {
            vec![
                (
                    "Pin to commit SHA (dev)".to_string(),
                    format!("  - uses: actions/checkout@{dev} # {pinned}"),
                ),
                (
                    "Pin to branch main".to_string(),
                    format!("  - uses: actions/checkout@main # {branch}"),
                ),
            ]
        };
        let mock = || crate::test_utils::MockProvider::new("github.com").with_resolve(Ok(&dev));

        let fetching = mock().with_versions("actions", "checkout", dev_and_main());
        assert_eq!(
            pin_texts(DEV_LINE, None, fetching).await?,
            (1, listed("dev keep", "keep"))
        );

        let cached = dev_and_main();
        assert_eq!(
            pin_texts(DEV_LINE, Some(&cached), mock()).await?,
            (0, listed("dev keep", "keep"))
        );

        let failing = mock().with_error(
            "actions",
            "checkout",
            actup_core::Error::RateLimited {
                provider: "github".to_string(),
                retry_after: None,
            },
        );
        assert_eq!(
            pin_texts(DEV_LINE, None, failing).await?,
            (1, listed("main keep", "main keep"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn pin_actions_skip_stale_version_sets()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let a = "a".repeat(40);
        let text = format!(
            "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@{a} # v4.1.0\n      - uses: actions/checkout@dev # keep\n"
        );
        let rate_limited = || actup_core::Error::RateLimited {
            provider: "github".to_string(),
            retry_after: None,
        };
        let staleness =
            actup_core::cache::Staleness::after(&rate_limited(), Duration::from_hours(30))
                .ok_or("no staleness")?;
        let fresh = crate::test_utils::simple_version_set("v4.1.0", &a);
        let stale = fresh.clone().into_stale(staleness);
        let limited = || {
            crate::test_utils::MockProvider::new("github.com")
                .with_resolve(Err(rate_limited()))
                .with_default_branch(Err(rate_limited()))
        };
        let provider = limited().with_error("actions", "checkout", rate_limited());
        let (service, _socket) = tower_lsp_server::LspService::new(|client| {
            LspBackend::with_providers(client, None, crate::test_utils::MOCK_PROVIDERS)
        });
        let backend = service.inner();
        *backend.registry.write().await =
            Some(Arc::new(crate::test_utils::mock_registry(provider)));
        let uri = Uri::from_file_path("/test/ci.yml").ok_or("no uri")?;
        let repo = actup_core::ActionRepoId::new(None, "actions", "checkout", None);
        let titles = |line: u32| {
            let position = tower_lsp_server::ls_types::Position {
                line,
                character: 33,
            };
            let uri = uri.clone();
            async move {
                backend
                    .pin_actions_at(&uri, position, Some(1), false)
                    .await
                    .into_iter()
                    .map(|action| match action {
                        tower_lsp_server::ls_types::CodeActionOrCommand::CodeAction(action) => {
                            action.title
                        }
                        tower_lsp_server::ls_types::CodeActionOrCommand::Command(command) => {
                            command.title
                        }
                    })
                    .collect::<Vec<_>>()
            }
        };
        let open = |versions: actup_core::version::Set| {
            let mut doc = DocumentState::new(text.clone(), 1);
            doc.version_sets = Arc::new(HashMap::from([(repo.clone(), versions)]));
            backend.documents.insert(uri.clone(), doc);
        };

        open(fresh.clone());
        assert_eq!(
            titles(3).await,
            ["Pin to major tag v4", "actup: unpin to `v4.1.0`"]
        );

        open(stale.clone());
        assert_eq!(titles(3).await, Vec::<String>::new());
        assert_eq!(titles(4).await, Vec::<String>::new());

        *backend.registry.write().await = Some(Arc::new(crate::test_utils::mock_registry(
            limited().with_versions("actions", "checkout", stale.clone()),
        )));
        backend
            .documents
            .insert(uri.clone(), DocumentState::new(text.clone(), 1));
        assert_eq!(titles(3).await, Vec::<String>::new());
        assert_eq!(titles(4).await, Vec::<String>::new());

        *backend.registry.write().await = Some(Arc::new(crate::test_utils::mock_registry(
            limited().with_versions("actions", "checkout", fresh),
        )));
        open(stale);
        assert_eq!(
            titles(3).await,
            ["Pin to major tag v4", "actup: unpin to `v4.1.0`"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn unpin_needs_a_marker_naming_a_tag_on_the_pinned_commit()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let a = "a".repeat(40);
        let versions = crate::test_utils::simple_version_set("v4.1.0", &a);
        let elsewhere = crate::test_utils::simple_version_set("v4.1.0", &"b".repeat(40));
        let line = |tail: &str| format!("  - uses: actions/checkout@{a}{tail}");

        assert_eq!(
            unpinned(&line(" # v4.1.0"), &versions).await?.as_deref(),
            Some("actup: unpin to `v4.1.0`:   - uses: actions/checkout@v4.1.0")
        );
        assert_eq!(
            unpinned(&line("  #v4.1.0 keep me"), &versions)
                .await?
                .as_deref(),
            Some("actup: unpin to `v4.1.0`:   - uses: actions/checkout@v4.1.0 # keep me")
        );
        assert_eq!(unpinned(&line(" # v4.1.0"), &elsewhere).await?, None);
        assert_eq!(unpinned(&line(" # v4.2.0"), &versions).await?, None);
        assert_eq!(unpinned(&line(" # main"), &versions).await?, None);
        assert_eq!(unpinned(&line(" # mine"), &versions).await?, None);
        assert_eq!(unpinned(&line(""), &versions).await?, None);
        assert_eq!(
            unpinned("  - uses: actions/checkout@v4.1.0 # v4.1.0", &versions).await?,
            None
        );
        assert_eq!(
            unpinned(
                &format!("  - uses: \"actions/checkout@{a}\" # v4.1.0"),
                &versions
            )
            .await?
            .as_deref(),
            Some("actup: unpin to `v4.1.0`:   - uses: \"actions/checkout@v4.1.0\"")
        );
        assert_eq!(
            unpinned(
                &format!("  - {{ uses: actions/checkout@{a} }} # v4.1.0 keep"),
                &versions
            )
            .await?
            .as_deref(),
            Some("actup: unpin to `v4.1.0`:   - { uses: actions/checkout@v4.1.0 } # keep")
        );
        assert_eq!(
            unpinned(
                &format!("  - {{ uses: actions/checkout@{a}, with: {{ x: 1 }} }} # v4.1.0 keep"),
                &versions
            )
            .await?
            .as_deref(),
            Some(
                "actup: unpin to `v4.1.0`:   - { uses: actions/checkout@v4.1.0, with: { x: 1 } } # keep"
            )
        );
        Ok(())
    }

    #[test]
    fn major_target_for_sha_ignores_tag_order() {
        let sha = "d".repeat(40);
        let tags = [
            crate::test_utils::make_version_info("v3.9.0", &sha),
            crate::test_utils::make_version_info("v4.0.0", &sha),
        ];
        for list in [tags.to_vec(), tags.iter().rev().cloned().collect()] {
            let v = actup_core::version::Set::build(
                list,
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
                None,
            );
            assert_eq!(major_target(&aref(&sha), Some(&v)).as_deref(), Some("v4"));
        }
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
        assert_eq!(state.diagnostics, []);
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
    fn extract_config_path_treats_a_blank_value_as_unset() {
        for blank in ["", "   ", "\t\n"] {
            let params = InitializeParams {
                initialization_options: Some(serde_json::json!({ "configPath": blank })),
                ..Default::default()
            };
            assert_eq!(extract_config_path(&params), None, "{blank:?}");
        }
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
        let handle = tokio::spawn(async { tokio::time::sleep(Duration::from_mins(1)).await });
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
        let task1 = tokio::spawn(async { tokio::time::sleep(Duration::from_mins(1)).await });
        let handle1 = task1.abort_handle();
        in_flight.insert(uri.clone(), handle1);

        // Second "edit" arrives — should cancel the first.
        if let Some((_, prev)) = in_flight.remove(&uri) {
            prev.abort();
        }
        let task2 = tokio::spawn(async { tokio::time::sleep(Duration::from_mins(1)).await });
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

    fn relative_watchers(watchers: &[FileSystemWatcher]) -> Vec<(PathBuf, String)> {
        watchers
            .iter()
            .filter_map(|w| match &w.glob_pattern {
                GlobPattern::Relative(RelativePattern {
                    base_uri: OneOf::Right(uri),
                    pattern,
                }) => Some((uri.to_file_path()?.into_owned(), pattern.clone())),
                _ => None,
            })
            .collect()
    }

    const ROOT_NAMES: &str = "{actup.toml,actup.json,actup.jsonc,actup.yaml,actup.yml,\
                              .actup.toml,.actup.json,.actup.jsonc,.actup.yaml,.actup.yml}";
    const SUBDIR_NAMES: &str = "{actup.toml,actup.json,actup.jsonc,actup.yaml,actup.yml}";

    #[test]
    fn watched_configs_contain_every_discovery_candidate() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let start = temp.path().join("repo");
        let watched = WatchedConfigs {
            search_dirs: vec![start.clone(), temp.path().to_path_buf()],
            ..WatchedConfigs::default()
        };

        for candidate in [
            start.join("actup.toml"),
            start.join(".actup.json"),
            start.join("actup.yaml"),
            start.join(".config/actup.yml"),
            start.join(".github/actup.jsonc"),
            temp.path().join("actup.toml"),
            temp.path().join(".github/actup.yaml"),
        ] {
            assert!(watched.contains(&candidate), "{}", candidate.display());
        }
        for other in [
            start.join("nested/actup.toml"),
            start.join(".github/.actup.toml"),
            start.join("Cargo.toml"),
            start.join("actup.toml.bak"),
            start.join("not-actup.toml"),
            temp.path().with_file_name("actup.toml"),
        ] {
            assert!(!watched.contains(&other), "{}", other.display());
        }
        Ok(())
    }

    #[test]
    fn watched_configs_contain_user_and_explicit_config() {
        let user = PathBuf::from("/home/user/.config/actup/config.toml");
        let explicit = PathBuf::from("/elsewhere/custom.toml");
        let watched = WatchedConfigs {
            search_dirs: Vec::new(),
            user: Some(user.clone()),
            explicit: Some(explicit.clone()),
        };
        assert!(watched.contains(&user));
        assert!(watched.contains(&explicit));
        assert!(!watched.contains(Path::new("/elsewhere/actup.toml")));
        assert!(!WatchedConfigs::default().contains(&user));
    }

    #[cfg(unix)]
    #[test]
    fn watched_configs_match_events_through_a_symlink() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let real = temp.path().canonicalize()?.join("real");
        std::fs::create_dir(&real)?;
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link)?;
        let watched = WatchedConfigs {
            search_dirs: actup_core::repo_search_dirs(&link, Some(&link)),
            ..WatchedConfigs::default()
        };
        assert_eq!(watched.search_dirs, std::slice::from_ref(&real));
        assert!(watched.contains(&link.join("actup.toml")));
        std::fs::write(real.join("actup.toml"), "")?;
        assert!(watched.contains(&link.join("actup.toml")));
        assert!(!watched.contains(&link.join("other.toml")));
        assert!(!watched.contains(&link.join("missing/actup.toml")));
        Ok(())
    }

    #[test]
    fn config_file_watchers_cover_candidates_user_and_explicit() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let start = temp.path().join("repo");
        let user = temp.path().join("home/actup/config.toml");
        let explicit = temp.path().join("custom.yaml");
        let watched = WatchedConfigs {
            search_dirs: vec![start.clone(), temp.path().to_path_buf()],
            user: Some(user),
            explicit: Some(explicit),
        };
        let watchers = config_file_watchers(&watched, true);
        assert!(watchers.iter().all(|w| w.kind.is_none()));

        let relative = relative_watchers(&watchers);
        assert_eq!(relative.len(), watchers.len());
        for expected in [
            (start.clone(), ROOT_NAMES),
            (start.join(".config"), SUBDIR_NAMES),
            (start.join(".github"), SUBDIR_NAMES),
            (temp.path().to_path_buf(), ROOT_NAMES),
            (temp.path().join(".github"), SUBDIR_NAMES),
            (temp.path().join("home/actup"), "config.toml"),
            (temp.path().to_path_buf(), "custom.yaml"),
        ] {
            assert!(
                relative.contains(&(expected.0.clone(), expected.1.to_owned())),
                "missing {expected:?} in {relative:?}"
            );
        }
        assert_eq!(watchers.len(), 2 * 3 + 2);
        Ok(())
    }

    fn slash_glob(path: &Path) -> String {
        let slashed = path.to_string_lossy().replace('\\', "/");
        match slashed.split_once(':') {
            Some((drive, rest))
                if drive.len() == 1 && drive.chars().all(|c| c.is_ascii_alphabetic()) =>
            {
                format!(
                    "[{}{}]:{rest}",
                    drive.to_ascii_lowercase(),
                    drive.to_ascii_uppercase()
                )
            }
            _ => slashed,
        }
    }

    fn slash_glob_in(dir: &Path, pattern: &str) -> String {
        format!("{}/{pattern}", slash_glob(dir).trim_end_matches('/'))
    }

    #[test]
    fn config_file_watchers_without_relative_patterns_use_absolute_globs() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let start = temp.path().join("repo");
        let user = temp.path().join("home/actup/config.toml");
        let explicit = temp.path().join("custom.yaml");
        let watched = WatchedConfigs {
            search_dirs: vec![start.clone(), temp.path().to_path_buf()],
            user: Some(user.clone()),
            explicit: Some(explicit.clone()),
        };
        let watchers = config_file_watchers(&watched, false);
        let patterns: Vec<_> = watchers
            .iter()
            .map(|w| match &w.glob_pattern {
                GlobPattern::String(s) => s.clone(),
                GlobPattern::Relative(r) => panic!("unexpected relative pattern {r:?}"),
            })
            .collect();
        assert_eq!(
            patterns.get(..3),
            Some(
                &[
                    format!("**/{ROOT_NAMES}"),
                    format!("**/.config/{SUBDIR_NAMES}"),
                    format!("**/.github/{SUBDIR_NAMES}"),
                ][..]
            )
        );
        for expected in [
            slash_glob_in(temp.path(), ROOT_NAMES),
            slash_glob_in(&temp.path().join(".config"), SUBDIR_NAMES),
            slash_glob_in(&temp.path().join(".github"), SUBDIR_NAMES),
            slash_glob(&user),
            slash_glob(&explicit),
        ] {
            assert!(
                patterns.contains(&expected),
                "missing {expected} in {patterns:?}"
            );
        }
        let start_glob = slash_glob(&start);
        assert!(
            !patterns.iter().any(|p| p.starts_with(&start_glob)),
            "{patterns:?}"
        );
        assert_eq!(watchers.len(), 2 * 3 + 2);
        assert!(watchers.iter().all(|w| w.kind.is_none()));
        Ok(())
    }

    #[test]
    fn absolute_glob_escapes_glob_syntax() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let odd = temp.path().join("we*ird[x]{y}?");
        assert_eq!(
            absolute_glob(&odd),
            format!("{}/we[*]ird[[]x][{{]y[}}][?]", slash_glob(temp.path()))
        );
        Ok(())
    }

    #[test]
    fn prefix_glob_uses_forward_slashes() {
        let os = OsStr::new;
        for (prefix, expected) in [
            (Prefix::Disk(b'C'), "[cC]:"),
            (Prefix::Disk(b'c'), "[cC]:"),
            (Prefix::VerbatimDisk(b'D'), "[dD]:"),
            (Prefix::UNC(os("server"), os("share")), "//server/share"),
            (
                Prefix::UNC(os("wsl.localhost"), os("Ubuntu")),
                "//wsl.localhost/Ubuntu",
            ),
            (
                Prefix::VerbatimUNC(os("server"), os("share")),
                "//server/share",
            ),
            (Prefix::UNC(os("s*rv"), os("sh[a]re")), "//s[*]rv/sh[[]a]re"),
            (Prefix::DeviceNS(os("pipe")), "//./pipe"),
            (Prefix::Verbatim(os("volume")), "//[?]/volume"),
        ] {
            assert_eq!(prefix_glob(prefix), expected, "{prefix:?}");
        }
    }

    #[test]
    fn disk_prefix_glob_matches_either_drive_case() -> std::result::Result<(), glob::PatternError> {
        for letter in *b"cC" {
            let pattern = glob::Pattern::new(&format!(
                "{}/repo/actup.toml",
                prefix_glob(Prefix::Disk(letter))
            ))?;
            for reported in ["c:/repo/actup.toml", "C:/repo/actup.toml"] {
                assert!(pattern.matches(reported), "{pattern} vs {reported}");
            }
            assert!(!pattern.matches("d:/repo/actup.toml"), "{pattern}");
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn absolute_glob_writes_windows_prefixes_with_forward_slashes() {
        for (path, expected) in [
            (r"C:\repo\actup.toml", "[cC]:/repo/actup.toml"),
            (r"c:\repo\actup.toml", "[cC]:/repo/actup.toml"),
            (
                r"\\server\share\repo\actup.toml",
                "//server/share/repo/actup.toml",
            ),
            (
                r"\\wsl.localhost\Ubuntu\home\actup.toml",
                "//wsl.localhost/Ubuntu/home/actup.toml",
            ),
            (r"\\?\C:\repo\actup.toml", "[cC]:/repo/actup.toml"),
            (
                r"\\?\UNC\server\share\repo\actup.toml",
                "//server/share/repo/actup.toml",
            ),
        ] {
            assert_eq!(absolute_glob(Path::new(path)), expected, "{path}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn absolute_glob_in_appends_pattern_after_unc_root() {
        assert_eq!(
            absolute_glob_in(Path::new(r"\\server\share"), "{actup.toml}"),
            "//server/share/{actup.toml}"
        );
        assert_eq!(
            absolute_glob_in(Path::new(r"\\?\C:\"), "{actup.toml}"),
            "[cC]:/{actup.toml}"
        );
    }

    async fn initialized_service(
        root: &Path,
    ) -> std::result::Result<
        (
            tower_lsp_server::LspService<LspBackend>,
            tower_lsp_server::ClientSocket,
        ),
        Box<dyn std::error::Error>,
    > {
        use tower::Service as _;
        use tower_lsp_server::{
            jsonrpc::Request,
            ls_types::{
                DidChangeWatchedFilesClientCapabilities, WorkspaceClientCapabilities,
                WorkspaceFolder,
            },
        };

        let (mut service, socket) = tower_lsp_server::LspService::new(|client| {
            LspBackend::with_providers(client, None, crate::test_utils::MOCK_PROVIDERS)
        });
        let params = InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: Uri::from_file_path(root).ok_or("bad root uri")?,
                name: "test".to_owned(),
            }]),
            ..Default::default()
        };
        service
            .call(
                Request::build("initialize")
                    .id(1)
                    .params(serde_json::to_value(params)?)
                    .finish(),
            )
            .await?;
        service
            .call(
                Request::build("initialized")
                    .params(serde_json::json!({}))
                    .finish(),
            )
            .await?;
        *service.inner().client_capabilities.write().await = ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                did_change_watched_files: Some(DidChangeWatchedFilesClientCapabilities {
                    dynamic_registration: Some(true),
                    relative_pattern_support: Some(true),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        Ok((service, socket))
    }

    async fn next_request(
        socket: &mut tower_lsp_server::ClientSocket,
    ) -> Option<tower_lsp_server::jsonrpc::Request> {
        use futures::StreamExt as _;
        while let Some(message) = socket.next().await {
            if message.id().is_some() {
                return Some(message);
            }
        }
        None
    }

    async fn answer(
        socket: &mut tower_lsp_server::ClientSocket,
        succeed: bool,
    ) -> std::result::Result<String, Box<dyn std::error::Error>> {
        use futures::SinkExt as _;
        use tower_lsp_server::jsonrpc::{Error, Response};

        let request = next_request(socket).await.ok_or("socket closed")?;
        let id = request.id().cloned().ok_or("request without id")?;
        let response = if succeed {
            Response::from_ok(id, serde_json::Value::Null)
        } else {
            Response::from_error(id, Error::internal_error())
        };
        socket.send(response).await?;
        Ok(request.method().to_owned())
    }

    const REGISTER: &str = "client/registerCapability";
    const UNREGISTER: &str = "client/unregisterCapability";

    #[tokio::test]
    async fn config_watcher_reregistrations_do_not_interleave()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (service, mut socket) = initialized_service(temp.path()).await?;
        let backend = service.inner();

        let client = async {
            let first = next_request(&mut socket).await.ok_or("no first request")?;
            let overlapping =
                tokio::time::timeout(Duration::from_millis(200), next_request(&mut socket)).await;
            assert!(overlapping.is_err(), "overlapping request {overlapping:?}");
            let id = first.id().cloned().ok_or("request without id")?;
            futures::SinkExt::send(
                &mut socket,
                tower_lsp_server::jsonrpc::Response::from_ok(id, serde_json::Value::Null),
            )
            .await?;
            Ok::<_, Box<dyn std::error::Error>>([
                first.method().to_owned(),
                answer(&mut socket, true).await?,
                answer(&mut socket, true).await?,
            ])
        };
        let ((), (), methods) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                backend.register_config_watchers(),
                backend.register_config_watchers(),
                client
            )
        })
        .await?;

        assert_eq!(methods?, [REGISTER, UNREGISTER, REGISTER]);
        assert!(*backend.config_watchers_registered.lock().await);
        Ok(())
    }

    #[tokio::test]
    async fn failed_config_watcher_unregister_skips_register()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (service, mut socket) = initialized_service(temp.path()).await?;
        let backend = service.inner();
        let deadline = Duration::from_secs(10);

        let ((), first) = tokio::time::timeout(deadline, async {
            tokio::join!(
                backend.register_config_watchers(),
                answer(&mut socket, true)
            )
        })
        .await?;
        assert_eq!(first?, REGISTER);

        let ((), failed) = tokio::time::timeout(deadline, async {
            tokio::join!(
                backend.register_config_watchers(),
                answer(&mut socket, false)
            )
        })
        .await?;
        assert_eq!(failed?, UNREGISTER);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), next_request(&mut socket))
                .await
                .is_err(),
            "register followed a failed unregister"
        );
        assert!(*backend.config_watchers_registered.lock().await);

        let ((), retried) = tokio::time::timeout(deadline, async {
            tokio::join!(backend.register_config_watchers(), async {
                Ok::<_, Box<dyn std::error::Error>>([
                    answer(&mut socket, true).await?,
                    answer(&mut socket, true).await?,
                ])
            })
        })
        .await?;
        assert_eq!(retried?, [UNREGISTER, REGISTER]);
        Ok(())
    }

    #[tokio::test]
    async fn failed_reload_drops_registry_and_file_cache()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let config_file = temp.path().join("actup.toml");
        std::fs::write(&config_file, "[cache]\nenabled = false\n")?;
        let (service, _socket) = tower_lsp_server::LspService::new(|client| {
            LspBackend::with_providers(client, None, crate::test_utils::MOCK_PROVIDERS)
        });
        let backend = service.inner();
        let root = Uri::from_file_path(temp.path()).ok_or("bad root uri")?;
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![tower_lsp_server::ls_types::WorkspaceFolder {
                    uri: root,
                    name: "test".to_owned(),
                }]),
                ..Default::default()
            })
            .await?;
        assert!(backend.registry.read().await.is_some());
        assert!(backend.file_cache.read().await.is_some());

        let uri = Uri::from_file_path(temp.path().join("ci.yml")).ok_or("bad file uri")?;
        let text = "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@v4.1.0\n";
        backend
            .documents
            .insert(uri.clone(), DocumentState::new(text.to_owned(), 1));

        std::fs::write(&config_file, "[policy]\nbump = \"sideways\"\n")?;
        backend.reload_config().await;

        assert!(matches!(
            &*backend.config.read().await,
            ConfigState::Invalid(_)
        ));
        assert!(backend.registry.read().await.is_none());
        assert!(backend.file_cache.read().await.is_none());

        let position = tower_lsp_server::ls_types::Position {
            line: 3,
            character: 30,
        };
        let completion = backend
            .completion(CompletionParams {
                text_document_position: tower_lsp_server::ls_types::TextDocumentPositionParams {
                    text_document: tower_lsp_server::ls_types::TextDocumentIdentifier {
                        uri: uri.clone(),
                    },
                    position,
                },
                work_done_progress_params:
                    tower_lsp_server::ls_types::WorkDoneProgressParams::default(),
                partial_result_params: tower_lsp_server::ls_types::PartialResultParams::default(),
                context: None,
            })
            .await?;
        assert!(completion.is_none());
        assert_eq!(
            backend.pin_actions_at(&uri, position, Some(1), true).await,
            []
        );
        Ok(())
    }

    #[tokio::test]
    async fn reload_with_rejected_providers_drops_the_registry()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        use futures::StreamExt as _;

        let temp = tempfile::tempdir()?;
        let config_file = temp.path().join("actup.toml");
        std::fs::write(&config_file, "[cache]\nenabled = false\n")?;
        let (service, mut socket) = initialized_service(temp.path()).await?;
        let backend = service.inner();
        assert!(backend.registry.read().await.is_some());

        std::fs::write(&config_file, REJECTED_HOST_CONFIG)?;
        let drain = async {
            while let Ok(Some(_)) =
                tokio::time::timeout(Duration::from_millis(200), socket.next()).await
            {}
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(backend.reload_config(), drain)
        })
        .await?;

        let stored = backend.config.read().await.clone();
        let ConfigState::Loaded(config) = stored else {
            panic!("reload stored {stored:?}");
        };
        assert!(config.hosts.contains_key("git.example.com"));
        assert!(provider_setup::build_registry(&config).is_err());
        assert!(backend.registry.read().await.is_none());
        Ok(())
    }

    const REJECTED_HOST_CONFIG: &str = "[hosts.\"git.example.com\"]\napi_mode = \"rest\"\n";

    async fn shown_and_published(
        socket: &mut tower_lsp_server::ClientSocket,
        uri: &Uri,
    ) -> std::result::Result<
        (
            Vec<tower_lsp_server::ls_types::ShowMessageParams>,
            tower_lsp_server::ls_types::PublishDiagnosticsParams,
        ),
        Box<dyn std::error::Error>,
    > {
        use futures::StreamExt as _;
        let mut shown = Vec::new();
        while let Some(message) = socket.next().await {
            let params = message.params().cloned().unwrap_or_default();
            match message.method() {
                "window/showMessage" => shown.push(serde_json::from_value(params)?),
                "textDocument/publishDiagnostics" => {
                    let published: tower_lsp_server::ls_types::PublishDiagnosticsParams =
                        serde_json::from_value(params)?;
                    if published.uri == *uri {
                        return Ok((shown, published));
                    }
                }
                _ => {}
            }
        }
        Err("socket closed before publishDiagnostics".into())
    }

    #[tokio::test]
    async fn reload_with_rejected_providers_publishes_empty_diagnostics()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        use futures::StreamExt as _;

        let temp = tempfile::tempdir()?;
        let config_file = temp.path().join("actup.toml");
        std::fs::write(&config_file, "[cache]\nenabled = false\n")?;
        let (service, mut socket) = initialized_service(temp.path()).await?;
        let backend = service.inner();
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(50), socket.next()).await
        {
        }

        let uri = Uri::from_file_path(temp.path().join("ci.yml")).ok_or("bad file uri")?;
        let text = "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@v4.1.0\n";
        let mut document = DocumentState::new(text.to_owned(), 3);
        document.diagnostics.push(Diagnostic::default());
        backend.documents.insert(uri.clone(), document);

        std::fs::write(&config_file, REJECTED_HOST_CONFIG)?;
        let ((), outcome) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                backend.reload_config(),
                shown_and_published(&mut socket, &uri)
            )
        })
        .await?;
        let (shown, published) = outcome?;

        assert_eq!(published.diagnostics, Vec::<Diagnostic>::new());
        assert_eq!(published.version, Some(3));
        assert!(
            shown.iter().any(|m| m.typ == MessageType::ERROR
                && m.message.contains("failed to rebuild providers")),
            "shown {shown:?}"
        );
        assert!(
            backend
                .documents
                .get(&uri)
                .is_some_and(|doc| doc.diagnostics.is_empty())
        );
        Ok(())
    }

    #[tokio::test]
    async fn shown_messages_replace_bidi_and_control_characters()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        std::fs::write(
            temp.path().join("actup.toml"),
            "[hosts.\"git.\u{202e}moc\u{1b}[2K.example.com\"]\napi_mode = \"rest\"\n",
        )?;
        let (service, mut socket) = initialized_service(temp.path()).await?;
        let backend = service.inner();

        let uri = Uri::from_file_path(temp.path().join("ci.yml")).ok_or("bad file uri")?;
        let text = "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@v4.1.0\n";
        backend
            .documents
            .insert(uri.clone(), DocumentState::new(text.to_owned(), 1));
        backend.schedule_diagnostics(uri.clone(), text.to_owned(), 1, Duration::ZERO);
        let (shown, _) = tokio::time::timeout(
            Duration::from_secs(10),
            shown_and_published(&mut socket, &uri),
        )
        .await??;

        let Some(error) = shown.iter().find(|m| m.typ == MessageType::ERROR) else {
            panic!("shown {shown:?}");
        };
        assert!(
            error
                .message
                .contains("git.\u{fffd}moc\u{fffd}[2K.example.com"),
            "{error:?}"
        );
        assert!(!error.message.contains(['\u{202e}', '\u{1b}']), "{error:?}");
        Ok(())
    }

    #[tokio::test]
    async fn initialize_with_rejected_providers_shows_the_error()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        std::fs::write(temp.path().join("actup.toml"), REJECTED_HOST_CONFIG)?;
        let (service, mut socket) = initialized_service(temp.path()).await?;
        let backend = service.inner();
        assert!(backend.registry.read().await.is_none());

        let uri = Uri::from_file_path(temp.path().join("ci.yml")).ok_or("bad file uri")?;
        let text = "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@v4.1.0\n";
        backend
            .documents
            .insert(uri.clone(), DocumentState::new(text.to_owned(), 1));
        backend.schedule_diagnostics(uri.clone(), text.to_owned(), 1, Duration::ZERO);
        let (shown, published) = tokio::time::timeout(
            Duration::from_secs(10),
            shown_and_published(&mut socket, &uri),
        )
        .await??;

        assert_eq!(published.diagnostics, Vec::<Diagnostic>::new());
        assert!(
            shown.iter().any(|m| m.typ == MessageType::ERROR
                && m.message.contains("failed to initialize providers")),
            "shown {shown:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn interleaved_reloads_keep_latest_workspace_root()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        use futures::StreamExt as _;

        let temp = tempfile::tempdir()?;
        let stale = temp.path().join("stale");
        let latest = temp.path().join("latest");
        std::fs::create_dir(&stale)?;
        std::fs::create_dir(&latest)?;
        std::fs::write(stale.join("actup.toml"), "[policy]\nbump = \"sideways\"\n")?;
        let latest_config = actup_core::canonical_path(&latest).join("actup.toml");
        std::fs::write(&latest_config, "[cache]\nenabled = false\n")?;
        let (service, mut socket) = initialized_service(temp.path()).await?;
        let backend = service.inner();

        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(50), socket.next()).await
        {
        }
        backend.client.log_message(MessageType::INFO, "fill").await;
        *backend.workspace_root.write().await = Some(stale);

        let drain = async {
            while let Ok(Some(_)) =
                tokio::time::timeout(Duration::from_millis(200), socket.next()).await
            {}
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                backend.reload_config(),
                async {
                    *backend.workspace_root.write().await = Some(latest.clone());
                    backend.reload_config().await;
                },
                drain
            )
        })
        .await?;

        assert_eq!(
            backend.workspace_root.read().await.as_deref(),
            Some(latest.as_path())
        );
        let stored = backend.config.read().await.clone();
        let ConfigState::Loaded(config) = stored else {
            panic!("stale reload stored {stored:?}");
        };
        assert_eq!(config.loaded_from(), Some(latest_config.as_path()));
        assert!(!config.cache.enabled);
        assert!(backend.registry.read().await.is_some());
        assert!(backend.file_cache.read().await.is_some());
        Ok(())
    }

    #[test]
    fn config_log_names_every_source() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let config = Config {
            sources: vec![
                actup_core::ConfigSource::User {
                    path: PathBuf::from("/home/u/.config/actup/config.toml"),
                },
                actup_core::ConfigSource::Env {
                    vars: vec!["ACTUP_POLICY__PIN".to_owned()],
                },
            ],
            ..Config::default()
        };
        assert_eq!(
            actup_test_support::logged(|| log_config_sources(&config, "loaded config"))?.1,
            " INFO loaded config source=user config /home/u/.config/actup/config.toml\n \
             INFO loaded config source=environment ACTUP_POLICY__PIN\n"
        );
        assert_eq!(
            actup_test_support::logged(|| log_config_sources(&Config::default(), "loaded config"))?
                .1,
            " INFO loaded config from defaults\n"
        );
        Ok(())
    }

    #[tokio::test]
    async fn workspace_root_bounds_config_discovery()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root)?;
        std::fs::write(
            temp.path().join("actup.toml"),
            "[policy]\nbump = \"patch\"\n",
        )?;
        let (service, _socket) = initialized_service(&root).await?;
        let backend = service.inner();

        let stored = backend.config.read().await.clone();
        let ConfigState::Loaded(config) = stored else {
            panic!("initialize stored {stored:?}");
        };
        assert_eq!(config.loaded_from(), None);
        assert_eq!(config.policy.bump, actup_core::policy::Update::Latest);
        let watched = backend.watched_configs().await;
        assert_eq!(watched.search_dirs, [actup_core::canonical_path(&root)]);
        assert!(!watched.contains(&temp.path().join("actup.toml")));
        Ok(())
    }

    #[test]
    fn configs_differ_detects_policy_change() {
        let base = Config::default();
        let mut changed = base.clone();
        changed.policy.bump = actup_core::policy::Update::Major;
        assert!(configs_differ_for_diagnostics(&base, &changed));
    }

    #[test]
    fn configs_differ_detects_ignore_change() -> std::result::Result<(), String> {
        let base = Config::default();
        let mut changed = base.clone();
        changed.scan.ignore.push("actions/checkout".parse()?);
        assert!(configs_differ_for_diagnostics(&base, &changed));
        Ok(())
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
        changed.policy.prerelease = !base.policy.prerelease;
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
        changed.policy.pin = actup_core::policy::Style::Sha;
        assert!(configs_differ_for_diagnostics(&base, &changed));
    }

    #[test]
    fn configs_differ_detects_branch_policy_change() {
        let base = Config::default();
        let mut changed = base.clone();
        // Default is `Ignore`; change to `Warn` to trigger a diff.
        changed.policy.branch = actup_core::policy::Branch::Warn;
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
    fn configs_differ_detects_track_change() {
        let base = Config::default();
        let mut changed = base.clone();
        changed.policy.track = actup_core::policy::Track::Exact;
        assert!(configs_differ_for_diagnostics(&base, &changed));
    }

    #[test]
    fn configs_differ_detects_override_change() -> std::result::Result<(), String> {
        let base = Config::default();
        let mut changed = base.clone();
        changed.overrides.push(actup_core::config::Override {
            pattern: "actions/*".parse()?,
            policy: actup_core::config::PolicyOverride {
                bump: Some(actup_core::policy::Update::Patch),
                ..Default::default()
            },
        });
        assert!(configs_differ_for_diagnostics(&base, &changed));
        let mut edited = changed.clone();
        edited.overrides[0].policy.bump = Some(actup_core::policy::Update::Minor);
        assert!(configs_differ_for_diagnostics(&changed, &edited));
        Ok(())
    }

    #[test]
    fn hosts_differ_different_lengths() {
        let a = std::collections::HashMap::new();
        let mut b = std::collections::HashMap::new();
        b.insert(
            "github.com".to_owned(),
            actup_core::HostConfig {
                kind: Some(actup_core::HostKind::GitHub),
                api_base: None,
                token_env: None,
                api_mode: None,
                max_tags_per_repo: None,
            },
        );
        assert!(hosts_differ(&a, &b));
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

    #[test]
    fn extract_config_path_handles_non_string_value() {
        let params = InitializeParams {
            initialization_options: Some(serde_json::json!({ "configPath": 123 })),
            ..Default::default()
        };
        assert!(extract_config_path(&params).is_none());
    }

    #[test]
    fn has_uses_keyword_case_sensitive() {
        assert!(!has_uses_keyword("USES: actions/checkout@v4"));
        assert!(has_uses_keyword("uses: actions/checkout@v4"));
    }

    #[test]
    fn configs_differ_detects_default_host_change() {
        let base = Config::default();
        let mut changed = base.clone();
        changed.default_host = Some("ghe.corp.com".to_owned());
        assert!(configs_differ_for_diagnostics(&base, &changed));
    }

    #[test]
    fn hosts_differ_empty_maps_equal() {
        let a = std::collections::HashMap::new();
        let b = std::collections::HashMap::new();
        assert!(!hosts_differ(&a, &b));
    }

    type Releases = Arc<std::sync::Mutex<actup_core::version::Set>>;

    struct ReleasingProvider(Releases);

    impl actup_core::Provider for ReleasingProvider {
        fn kind(&self) -> actup_core::provider::Kind {
            actup_core::provider::Kind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(
            &self,
            _repo: &actup_core::ActionRepoId,
        ) -> actup_core::BoxFuture<'_, actup_core::Result<actup_core::version::Set>> {
            let set =
                self.0
                    .lock()
                    .map(|set| set.clone())
                    .map_err(|e| actup_core::Error::Internal {
                        context: "releases".into(),
                        message: e.to_string(),
                        source: None,
                    });
            Box::pin(async move { set })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &actup_core::ActionRepoId,
            _ref_name: &str,
        ) -> actup_core::BoxFuture<'_, actup_core::Result<actup_core::provider::Sha40>> {
            let sha = actup_core::provider::Sha40::new("a".repeat(40)).ok_or_else(|| {
                actup_core::Error::Internal {
                    context: "releases".into(),
                    message: "invalid SHA".into(),
                    source: None,
                }
            });
            Box::pin(async move { sha })
        }

        fn get_default_branch(
            &self,
            _repo: &actup_core::ActionRepoId,
        ) -> actup_core::BoxFuture<'_, actup_core::Result<String>> {
            Box::pin(async { Ok("main".to_owned()) })
        }

        fn get_readme(
            &self,
            _repo: &actup_core::ActionRepoId,
            _ref_name: Option<&str>,
        ) -> actup_core::BoxFuture<'_, actup_core::Result<Option<String>>> {
            Box::pin(async { Ok(None) })
        }

        fn get_action_yml(
            &self,
            _repo: &actup_core::ActionRepoId,
            _ref_name: &str,
        ) -> actup_core::BoxFuture<'_, actup_core::Result<actup_core::provider::ActionYml>>
        {
            Box::pin(async { Ok(actup_core::provider::ActionYml::default()) })
        }
    }

    fn released(latest: &str, sha: char) -> actup_core::version::Set {
        let current = crate::test_utils::make_version_info("v4.0.0", &"a".repeat(40));
        let latest = crate::test_utils::make_version_info(latest, &sha.to_string().repeat(40));
        actup_core::version::Set::build(
            vec![current, Arc::clone(&latest)],
            HashMap::from([(4, Arc::clone(&latest))]),
            HashMap::new(),
            Some(latest),
        )
    }

    async fn published_messages(
        backend: &LspBackend,
        socket: &mut tower_lsp_server::ClientSocket,
        uri: &Uri,
        version: i32,
    ) -> std::result::Result<Vec<String>, Box<dyn std::error::Error>> {
        let text = "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@v4.0.0\n";
        backend
            .documents
            .insert(uri.clone(), DocumentState::new(text.to_owned(), version));
        backend.schedule_diagnostics(uri.clone(), text.to_owned(), version, Duration::ZERO);
        let (_, published) =
            tokio::time::timeout(Duration::from_secs(10), shown_and_published(socket, uri))
                .await??;
        assert_eq!(published.version, Some(version));
        Ok(published
            .diagnostics
            .into_iter()
            .map(|d| d.message)
            .collect())
    }

    #[tokio::test]
    async fn diagnostics_publish_a_new_release_once_the_cache_ttl_passes()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        use futures::StreamExt as _;

        let temp = tempfile::tempdir()?;
        std::fs::write(
            temp.path().join("actup.toml"),
            "[cache]\nenabled = false\nttl = 60\n",
        )?;
        let (service, mut socket) = initialized_service(temp.path()).await?;
        let backend = service.inner();
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(50), socket.next()).await
        {
        }

        let releases: Releases = Arc::new(std::sync::Mutex::new(released("v4.1.0", 'b')));
        let clock = Arc::new(crate::test_utils::StepClock::new());
        let dyn_clock: Arc<dyn actup_core::Clock> = clock.clone();
        let ttl = Duration::from_secs(60);
        let mut registry = provider::Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(actup_core::CachingProvider::with_retention(
            ReleasingProvider(Arc::clone(&releases)),
            actup_core::Retention::Expiring {
                ttl,
                clock: dyn_clock,
            },
        )));
        *backend.registry.write().await = Some(Arc::new(registry));
        let uri = Uri::from_file_path(temp.path().join("ci.yml")).ok_or("bad file uri")?;
        let targets = |messages: &[String], version: &str| {
            messages.iter().filter(|m| m.contains(version)).count()
        };

        let first = published_messages(backend, &mut socket, &uri, 1).await?;
        assert_eq!(targets(&first, "-> v4.1.0"), 1, "{first:?}");

        *releases.lock().map_err(|e| e.to_string())? = released("v4.2.0", 'c');
        clock.advance(ttl);
        let cached = published_messages(backend, &mut socket, &uri, 2).await?;
        assert_eq!(cached, first);

        clock.advance(Duration::from_secs(1));
        let refreshed = published_messages(backend, &mut socket, &uri, 3).await?;
        assert_eq!(targets(&refreshed, "-> v4.2.0"), 1, "{refreshed:?}");
        Ok(())
    }
}
