//! Workspace-wide commands for the LSP server.
//!
//! Implements `actup-lsp/fixAllInWorkspace` — discovers all workflow and composite
//! action files, computes diagnostics, and applies a single multi-file
//! [`WorkspaceEdit`] with all outdated ref replacements.
//!
//! Progress is reported in phases via `$/progress`:
//! 1. Discovery: "Discovering files..." (0%)
//! 2. Per-file scanning: "Checking `<path>`..." with action counts (5-95%)
//! 3. Summary: "Updated N refs in M files" (done)
//!
//! The operation shows a cancellation button. When cancelled (via the
//! [`Cancelled`] flag), it stops after the current file and returns partial
//! results.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use actup_core::{ActionRepoId, Config, DiscoveredFile, discover_files, line_spans, provider};
use dashmap::DashMap;
use tower_lsp_server::{
    Client,
    ls_types::{
        Diagnostic, DocumentChanges, NumberOrString, OneOf,
        OptionalVersionedTextDocumentIdentifier, Position, TextDocumentEdit, TextEdit, Uri,
        WorkspaceEdit,
    },
};

use crate::{backend::DocumentState, diagnostics, file_cache::FileCache, position::col_to_byte};

/// Command identifier registered in server capabilities.
pub const FIX_ALL_IN_WORKSPACE: &str = "actup-lsp/fixAllInWorkspace";

/// Bounded, cancellable `$/progress` handle from tower-lsp-server.
///
/// Aliased because the bare type takes four lines and shows up on every
/// function that threads progress through the workspace scan.
type WorkspaceProgress =
    tower_lsp_server::OngoingProgress<tower_lsp_server::Bounded, tower_lsp_server::Cancellable>;

/// Shared, read-only references threaded through the workspace scan.
///
/// Introduced to collapse 10-param function signatures. The lifetime binds
/// all references to the same call — none of these are stored or cloned
/// beyond the scan.
pub struct ScanContext<'a> {
    pub workspace_root: &'a Path,
    pub config: &'a Config,
    pub registry: &'a provider::Registry,
    pub file_cache: &'a FileCache,
    pub documents: &'a DashMap<Uri, DocumentState>,
    pub cancelled: &'a Cancelled,
    pub supports_document_changes: bool,
    pub encoding: crate::position::PositionEncoding,
}

/// Running state accumulated as the scan proceeds.
///
/// On cancellation or completion, [`ScanAccumulator::into_result`] builds
/// the [`FixAllResult`] so the two former early-return sites no longer
/// duplicate the struct construction.
#[derive(Default)]
struct ScanAccumulator {
    all_edits: Vec<VersionedFileEdits>,
    failures: Vec<(String, String)>,
    total_refs: usize,
    total_actions: usize,
    files_completed: usize,
}

impl ScanAccumulator {
    fn into_result(
        self,
        total_files: usize,
        supports_document_changes: bool,
        cancelled: bool,
    ) -> FixAllResult {
        let files_changed = self.all_edits.len();
        FixAllResult {
            edit: build_workspace_edit(self.all_edits, supports_document_changes),
            total_files,
            files_scanned: self.files_completed,
            files_changed,
            refs_updated: self.total_refs,
            actions_checked: self.total_actions,
            failures: self.failures,
            cancelled,
        }
    }
}

/// Cooperative cancellation flag for workspace operations.
///
/// Shared between the `execute_command` handler and the workspace scan loop.
/// Set to `true` when the operation should stop (e.g. client cancels the
/// request). The scan loop checks this between files and aborts early.
#[derive(Debug, Clone)]
pub struct Cancelled(Arc<AtomicBool>);

impl Cancelled {
    /// Create a new cancellation flag (initially not cancelled).
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Signal cancellation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Check whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for Cancelled {
    fn default() -> Self {
        Self::new()
    }
}

/// Cancellation flags of running workspace operations, keyed by progress token.
#[derive(Debug, Default)]
pub struct ProgressCancels(Arc<DashMap<NumberOrString, Cancelled>>);

impl ProgressCancels {
    /// Track a new operation under a fresh progress token until the returned guard drops.
    pub fn start(&self) -> TrackedProgress {
        let token = NumberOrString::String(format!("actup-workspace-{}", progress_id()));
        let cancelled = Cancelled::new();
        self.0.insert(token.clone(), cancelled.clone());
        TrackedProgress {
            cancels: Arc::clone(&self.0),
            token,
            cancelled,
        }
    }

    /// Cancel the operation reporting progress under `token`, if one is running.
    pub fn cancel(&self, token: &NumberOrString) {
        if let Some(cancelled) = self.0.get(token) {
            cancelled.cancel();
        } else {
            tracing::debug!(?token, "no running operation for progress token");
        }
    }
}

/// A running operation registered in [`ProgressCancels`].
#[derive(Debug)]
pub struct TrackedProgress {
    cancels: Arc<DashMap<NumberOrString, Cancelled>>,
    /// Progress token the operation reports under.
    pub token: NumberOrString,
    /// Flag set when the client cancels `token`.
    pub cancelled: Cancelled,
}

impl Drop for TrackedProgress {
    fn drop(&mut self) {
        self.cancels.remove(&self.token);
    }
}

/// Result of a workspace-wide fix operation.
#[derive(Debug)]
pub struct FixAllResult {
    /// The workspace edit to apply (may span multiple files).
    pub edit: WorkspaceEdit,
    /// Total number of discovered workflow/action files.
    pub total_files: usize,
    /// Number of files scanned (may be less than `total_files` if cancelled).
    pub files_scanned: usize,
    /// Number of files with edits.
    pub files_changed: usize,
    /// Number of individual ref replacements.
    pub refs_updated: usize,
    /// Total number of `uses:` actions checked across all files.
    pub actions_checked: usize,
    /// Files that failed during diagnostic computation (path, error message).
    pub failures: Vec<(String, String)>,
    /// Whether the operation was cancelled before completion.
    pub cancelled: bool,
}

impl FixAllResult {
    /// Create an empty result (no files scanned, no edits).
    fn empty() -> Self {
        Self {
            edit: WorkspaceEdit::default(),
            total_files: 0,
            files_scanned: 0,
            files_changed: 0,
            refs_updated: 0,
            actions_checked: 0,
            failures: Vec::new(),
            cancelled: false,
        }
    }

    /// Human-readable summary for progress finish messages.
    fn summary(&self) -> String {
        if self.cancelled {
            format!(
                "Cancelled ({}/{} files, {} actions checked)",
                self.files_scanned, self.total_files, self.actions_checked,
            )
        } else {
            format!(
                "Updated {} refs in {} files ({} actions checked)",
                self.refs_updated, self.files_changed, self.actions_checked,
            )
        }
    }
}

/// Extract the [`TextEdit`]s CLI `update` makes from a set of diagnostics.
fn edits_from_diagnostics(diags: &[Diagnostic]) -> Vec<TextEdit> {
    diags
        .iter()
        .filter_map(crate::code_actions::update_text_edit)
        .collect()
}

/// Outcome of processing a single file.
struct FileOutcome {
    /// URI, document version, and edits if outdated refs were found.
    ///
    /// `version` is `Some(n)` when the file was open in the editor (read from
    /// the document store), `None` when read from disk. Captured atomically
    /// with the text content so clients can reject stale edits.
    edits: Option<(Uri, Option<i32>, Vec<TextEdit>)>,
    /// Number of `uses:` actions checked in this file.
    actions_checked: usize,
}

struct LoadedFile {
    file: DiscoveredFile,
    uri: Uri,
    text: String,
    version: Option<i32>,
}

enum PreparedFile {
    Loaded(LoadedFile),
    Failed(DiscoveredFile),
}

/// Load a discovered file once so workspace commands can reuse the same text
/// for repo prefetch and final edit generation.
///
/// For files open in the editor, reads from the document store and snapshots
/// the version atomically with the text content. For files not open in the
/// editor, reads from disk with `version = None`.
///
/// Read/URI failures are recorded in `failures` and returned as
/// [`PreparedFile::Failed`] so the progress loop still counts the file.
async fn prepare_file(
    file: &DiscoveredFile,
    workspace_root: &Path,
    documents: &DashMap<Uri, DocumentState>,
    failures: &mut Vec<(String, String)>,
) -> PreparedFile {
    let path = file.path();
    let rel = relative_display(file, workspace_root);

    let Some(uri) = Uri::from_file_path(path) else {
        tracing::warn!(path = %rel, "failed to convert path to URI");
        failures.push((rel, "failed to convert path to URI".into()));
        return PreparedFile::Failed(file.clone());
    };

    // Read text + version atomically: prefer editor buffer (document store)
    // over disk so edits match the content the user sees.
    let (text, version) = if let Some(doc) = documents.get(&uri) {
        let t = doc.text.clone();
        let v = doc.version;
        drop(doc); // release DashMap shard lock before async work
        (t, Some(v))
    } else {
        match tokio::fs::read_to_string(path).await {
            Ok(t) => (t, None),
            Err(e) => {
                tracing::warn!(path = %rel, error = %e, "failed to read file");
                failures.push((rel, e.to_string()));
                return PreparedFile::Failed(file.clone());
            }
        }
    };

    PreparedFile::Loaded(LoadedFile {
        file: file.clone(),
        uri,
        text,
        version,
    })
}

async fn prefetch_workspace_versions(
    prepared_files: &[PreparedFile],
    config: &Config,
    registry: &provider::Registry,
    file_cache: &FileCache,
    encoding: crate::position::PositionEncoding,
) -> std::collections::HashMap<ActionRepoId, Result<actup_core::version::Set, actup_core::Error>> {
    let mut seen = std::collections::HashSet::new();
    let mut unique_repos = Vec::new();

    for prepared in prepared_files {
        let PreparedFile::Loaded(loaded) = prepared else {
            continue;
        };

        if !loaded.text.contains("uses:") {
            continue;
        }

        for repo in crate::diagnostics::extract_unique_repo_ids(
            &loaded.text,
            loaded.file.format(),
            config,
            encoding,
        ) {
            if seen.insert(repo.clone()) {
                unique_repos.push(repo);
            }
        }
    }

    diagnostics::batch_fetch_versions(&unique_repos, config, registry, file_cache).await
}

async fn process_loaded_file(
    loaded: &LoadedFile,
    config: &Config,
    registry: &provider::Registry,
    versions_map: &std::collections::HashMap<
        ActionRepoId,
        Result<actup_core::version::Set, actup_core::Error>,
    >,
    encoding: crate::position::PositionEncoding,
) -> FileOutcome {
    if !loaded.text.contains("uses:") {
        return FileOutcome {
            edits: None,
            actions_checked: 0,
        };
    }

    let result = diagnostics::compute_diagnostics_with_versions(
        &loaded.text,
        loaded.file.format(),
        config,
        registry,
        versions_map,
        encoding,
    )
    .await;
    let actions_checked = result.refs_checked;
    let edits = edits_from_diagnostics(&result.diagnostics);

    let result_edits = if edits.is_empty() {
        None
    } else {
        Some((loaded.uri.clone(), loaded.version, edits))
    };

    FileOutcome {
        edits: result_edits,
        actions_checked,
    }
}

/// Execute the fix-all-in-workspace command.
///
/// 1. Discovers workflow/action files under `workspace_root`
/// 2. Reads and computes diagnostics for each file
/// 3. Extracts outdated-ref edits from diagnostics
/// 4. Returns a [`FixAllResult`] with the aggregated workspace edit
///
/// Progress is reported via `$/progress` notifications using the
/// tower-lsp-server builder API. If the client doesn't support
/// `window/workDoneProgress/create`, progress is silently skipped.
///
/// The progress reports under `token`, with a cancel button. The [`Cancelled`] flag is checked
/// between files; when set, the operation stops and returns partial results
/// with `cancelled: true`.
pub async fn fix_all_in_workspace(
    client: Client,
    token: NumberOrString,
    ctx: &ScanContext<'_>,
) -> FixAllResult {
    let progress = match client.create_work_done_progress(token.clone()).await {
        Ok(()) => Some(
            client
                .progress(token, "Fix all outdated actions")
                .with_percentage(0)
                .with_cancel_button()
                .with_message("Discovering files...")
                .begin()
                .await,
        ),
        Err(_) => None,
    };

    let discovered = discover_files(ctx.workspace_root, &ctx.config.scan.extra_paths);
    let file_count = discovered.len();

    if file_count == 0 {
        if let Some(p) = progress {
            p.finish_with_message("No workflow files found").await;
        }
        return FixAllResult::empty();
    }

    if let Some(ref p) = progress {
        p.report_with_message(format!("Scanning {file_count} files..."), 5, None)
            .await;
    }

    let result = scan_files(&discovered, ctx, progress.as_ref()).await;

    if let Some(p) = progress {
        p.finish_with_message(result.summary()).await;
    }

    result
}

/// Per-file edits with the document version snapshotted at read time.
struct VersionedFileEdits {
    uri: Uri,
    /// `Some(n)` for files open in the editor, `None` for disk-only files.
    version: Option<i32>,
    edits: Vec<TextEdit>,
}

/// Scan discovered files, compute diagnostics, and collect edits.
///
/// Reports per-file progress and checks cancellation between files.
async fn scan_files(
    discovered: &[DiscoveredFile],
    ctx: &ScanContext<'_>,
    progress: Option<&WorkspaceProgress>,
) -> FixAllResult {
    let file_count = discovered.len();
    let mut acc = ScanAccumulator::default();
    let mut prepared_files = Vec::with_capacity(file_count);

    // Phase 1: load each file's text (from doc store or disk).
    for file in discovered {
        if ctx.cancelled.is_cancelled() {
            tracing::info!(
                completed = acc.files_completed,
                total = file_count,
                "workspace fix cancelled during preload"
            );
            return acc.into_result(file_count, ctx.supports_document_changes, true);
        }
        prepared_files
            .push(prepare_file(file, ctx.workspace_root, ctx.documents, &mut acc.failures).await);
    }

    // Phase 2: one batched version fetch covering every unique repo.
    let versions_map = prefetch_workspace_versions(
        &prepared_files,
        ctx.config,
        ctx.registry,
        ctx.file_cache,
        ctx.encoding,
    )
    .await;

    // Phase 3: per-file diagnostics + edit collection, with progress reports.
    for (i, prepared) in prepared_files.iter().enumerate() {
        if ctx.cancelled.is_cancelled() {
            tracing::info!(
                completed = acc.files_completed,
                total = file_count,
                "workspace fix cancelled by client"
            );
            return acc.into_result(file_count, ctx.supports_document_changes, true);
        }

        let (file, outcome) = match prepared {
            PreparedFile::Loaded(loaded) => (
                &loaded.file,
                process_loaded_file(
                    loaded,
                    ctx.config,
                    ctx.registry,
                    &versions_map,
                    ctx.encoding,
                )
                .await,
            ),
            PreparedFile::Failed(file) => (
                file,
                FileOutcome {
                    edits: None,
                    actions_checked: 0,
                },
            ),
        };

        acc.total_actions += outcome.actions_checked;
        let refusal = match (prepared, &outcome.edits) {
            (PreparedFile::Loaded(loaded), Some((_, _, edits))) => refuse_edits(loaded, edits, ctx)
                .await
                .map(|reason| (loaded, reason)),
            _ => None,
        };
        if let Some((loaded, reason)) = refusal {
            let rel = relative_display(&loaded.file, ctx.workspace_root);
            tracing::warn!(path = %rel, %reason, "edit refused");
            acc.failures
                .push((rel.clone(), format!("edit refused: {rel} {reason}")));
        } else if let Some((uri, version, edits)) = outcome.edits {
            acc.total_refs += edits.len();
            acc.all_edits.push(VersionedFileEdits {
                uri,
                version,
                edits,
            });
        }

        acc.files_completed = i + 1;
        report_file_progress(
            progress,
            file,
            ctx.workspace_root,
            acc.files_completed,
            file_count,
            acc.total_actions,
        )
        .await;
    }

    // All files scanned: overwrite files_scanned with the full count so callers
    // see total coverage, not just the last increment.
    acc.files_completed = file_count;
    acc.into_result(file_count, ctx.supports_document_changes, false)
}

async fn refuse_edits(
    loaded: &LoadedFile,
    edits: &[TextEdit],
    ctx: &ScanContext<'_>,
) -> Option<String> {
    if changed_since_scan(loaded, ctx.documents).await {
        return Some("changed since scan".to_string());
    }
    let Some(edited) = apply_text_edits(&loaded.text, edits, ctx.encoding) else {
        return Some("edits do not fit the text".to_string());
    };
    loaded
        .file
        .format()
        .validate_edit(&loaded.text, &edited)
        .err()
        .map(|e| format!("after edit: {e}"))
}

pub fn apply_text_edits(
    text: &str,
    edits: &[TextEdit],
    encoding: crate::position::PositionEncoding,
) -> Option<String> {
    let spans = line_spans(text);
    let offset = |pos: Position| -> Option<usize> {
        let line = usize::try_from(pos.line).ok()?;
        let span = match spans.get(line) {
            Some(span) => span.clone(),
            None if line == spans.len() => text.len()..text.len(),
            None => return None,
        };
        let byte = span.start + col_to_byte(text.get(span)?, pos.character, encoding);
        text.is_char_boundary(byte).then_some(byte)
    };
    let mut ranges = edits
        .iter()
        .map(|edit| {
            Some((
                offset(edit.range.start)?..offset(edit.range.end)?,
                &edit.new_text,
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    ranges.sort_by_key(|(range, _)| (range.start, range.end));
    let ordered = ranges.iter().all(|(range, _)| range.start <= range.end);
    let disjoint = ranges
        .windows(2)
        .all(|pair| pair[0].0.end <= pair[1].0.start);
    if !(ordered && disjoint) {
        return None;
    }
    let mut edited = text.to_string();
    for (range, new_text) in ranges.into_iter().rev() {
        edited.replace_range(range, new_text);
    }
    Some(edited)
}

async fn changed_since_scan(loaded: &LoadedFile, documents: &DashMap<Uri, DocumentState>) -> bool {
    let open = documents
        .get(&loaded.uri)
        .map(|doc| (doc.version, doc.text == loaded.text));
    match (loaded.version, open) {
        (Some(version), Some((current, _))) => current != version,
        (Some(_), None) => true,
        (None, Some((_, same_text))) => !same_text,
        (None, None) => tokio::fs::read_to_string(loaded.file.path())
            .await
            .map_or(true, |text| text != loaded.text),
    }
}

fn relative_display(file: &DiscoveredFile, workspace_root: &Path) -> String {
    let Ok(relative) = file.path().strip_prefix(workspace_root) else {
        return file.path().display().to_string();
    };
    relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Report per-file progress to the client.
async fn report_file_progress(
    progress: Option<&WorkspaceProgress>,
    file: &DiscoveredFile,
    workspace_root: &Path,
    files_completed: usize,
    file_count: usize,
    total_actions: usize,
) {
    let Some(p) = progress else { return };

    // Percentage is scaled into [5, 95]. The raw expression is always <= 95,
    // so `try_from` is infallible — but we route around `as` / `unwrap` so
    // a future tweak to the scaling can't introduce a silent overflow.
    let raw_pct = (5 + (files_completed * 90) / file_count).min(95);
    let pct = u32::try_from(raw_pct).unwrap_or(95);
    let rel = relative_display(file, workspace_root);
    let msg = if total_actions > 0 {
        format!(
            "Checking {rel}... ({total_actions} actions in {files_completed}/{file_count} files)"
        )
    } else {
        format!("Checking {rel}... ({files_completed}/{file_count} files)")
    };
    p.report_with_message(msg, pct, None).await;
}

/// Build a [`WorkspaceEdit`] from accumulated per-file changes.
///
/// When `supports_document_changes` is `true`, uses
/// [`DocumentChanges::Edits`] with [`OptionalVersionedTextDocumentIdentifier`]
/// so clients can reject edits that target a stale document version. Files
/// open in the editor carry `version: Some(n)` (snapshotted at read time);
/// files read from disk use `version: None` (null), which per LSP spec means
/// "content on disk is the master."
///
/// When `false`, falls back to the unversioned `changes` map.
fn build_workspace_edit(
    file_edits: Vec<VersionedFileEdits>,
    supports_document_changes: bool,
) -> WorkspaceEdit {
    if file_edits.is_empty() {
        WorkspaceEdit::default()
    } else if supports_document_changes {
        let doc_edits = file_edits
            .into_iter()
            .map(|fe| TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: fe.uri,
                    version: fe.version,
                },
                edits: fe.edits.into_iter().map(OneOf::Left).collect(),
            })
            .collect();
        WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(doc_edits)),
            ..Default::default()
        }
    } else {
        let mut changes = std::collections::HashMap::new();
        for fe in file_edits {
            changes.insert(fe.uri, fe.edits);
        }
        WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }
    }
}

/// Monotonic progress ID for unique tokens.
fn progress_id() -> u64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::atomic::Ordering;

    use dashmap::DashMap;

    use super::*;
    use crate::{
        diagnostics::{DIAGNOSTIC_SOURCE, DiagnosticData},
        test_utils,
    };

    fn make_range(line: u32, col_start: u32, col_end: u32) -> tower_lsp_server::ls_types::Range {
        test_utils::make_range(line, col_start, col_end)
    }

    fn make_outdated_diagnostic(
        range: tower_lsp_server::ls_types::Range,
        target_ref: &str,
    ) -> Diagnostic {
        test_utils::make_outdated_diagnostic(range, "o/r@old", target_ref)
    }

    #[test]
    fn command_name_is_stable() {
        assert_eq!(FIX_ALL_IN_WORKSPACE, "actup-lsp/fixAllInWorkspace");
    }

    #[test]
    fn progress_ids_are_unique() {
        let a = progress_id();
        let b = progress_id();
        assert_ne!(a, b);
    }

    #[test]
    fn fix_all_result_default_is_empty() {
        let result = FixAllResult {
            edit: WorkspaceEdit::default(),
            total_files: 0,
            files_scanned: 0,
            files_changed: 0,
            refs_updated: 0,
            actions_checked: 0,
            failures: Vec::new(),
            cancelled: false,
        };
        assert_eq!(result.files_scanned, 0);
        assert_eq!(result.refs_updated, 0);
        assert_eq!(result.actions_checked, 0);
        assert!(!result.cancelled);
        assert_eq!(result.failures, []);
    }

    #[test]
    fn edits_from_diagnostics_extracts_outdated() {
        let diags = vec![
            make_outdated_diagnostic(make_range(7, 31, 33), "v4.2.0"),
            make_outdated_diagnostic(make_range(8, 31, 33), "v3.1.0"),
        ];
        let edits = edits_from_diagnostics(&diags);
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].new_text, "v4.2.0");
        assert_eq!(edits[1].new_text, "v3.1.0");
    }

    #[test]
    fn edits_from_diagnostics_extracts_pin_branch() {
        let sha = "c".repeat(40);
        let data = DiagnosticData::PinBranch {
            sha: sha.clone(),
            ref_name: "main".into(),
            repin_comment: Some(" # main".into()),
            replace_end_col: None,
        };
        let diag = Diagnostic {
            range: make_range(3, 15, 19),
            source: Some(DIAGNOSTIC_SOURCE.into()),
            data: serde_json::to_value(data).ok(),
            ..Default::default()
        };
        assert_eq!(
            edits_from_diagnostics(&[diag]),
            [TextEdit {
                range: make_range(3, 15, 19),
                new_text: format!("{sha} # main"),
            }]
        );
    }

    #[test]
    fn edits_from_diagnostics_skips_non_actup() {
        let diag = Diagnostic {
            range: make_range(0, 0, 5),
            source: Some("other-tool".into()),
            ..Default::default()
        };
        assert_eq!(edits_from_diagnostics(&[diag]), []);
    }

    #[test]
    fn edits_from_diagnostics_skips_unresolvable() {
        let data = DiagnosticData::Unresolvable {
            suggested_ref: Some("v2.0.0".into()),
            repin_comment: None,
            replace_end_col: None,
        };
        let diag = Diagnostic {
            range: make_range(0, 0, 5),
            source: Some(DIAGNOSTIC_SOURCE.into()),
            data: serde_json::to_value(data).ok(),
            ..Default::default()
        };
        // Unresolvable refs are NOT auto-fixed in workspace-wide operations.
        assert_eq!(edits_from_diagnostics(&[diag]), []);
    }

    #[test]
    fn cancelled_flag_starts_false() {
        let flag = Cancelled::new();
        assert!(!flag.is_cancelled());
    }

    #[test]
    fn cancelled_flag_can_be_set() {
        let flag = Cancelled::new();
        flag.cancel();
        assert!(flag.is_cancelled());
    }

    #[test]
    fn progress_cancels_cancel_only_the_named_running_operation() {
        let cancels = ProgressCancels::default();
        let first = cancels.start();
        let second = cancels.start();
        assert_ne!(first.token, second.token);

        cancels.cancel(&first.token);
        assert!(first.cancelled.is_cancelled());
        assert!(!second.cancelled.is_cancelled());

        let (token, cancelled) = (second.token.clone(), second.cancelled.clone());
        drop(second);
        cancels.cancel(&token);
        assert!(!cancelled.is_cancelled());
        cancels.cancel(&NumberOrString::String("unknown".into()));
    }

    #[test]
    fn cancelled_flag_clone_shares_state() {
        let flag = Cancelled::new();
        let clone = flag.clone();
        flag.cancel();
        assert!(clone.is_cancelled());
    }

    #[test]
    fn build_workspace_edit_empty_returns_default() {
        let edit = build_workspace_edit(Vec::new(), true);
        assert!(edit.changes.is_none());
        assert!(edit.document_changes.is_none());
    }

    #[test]
    fn build_workspace_edit_versioned_open_file() {
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();
        let edits = vec![VersionedFileEdits {
            uri: uri.clone(),
            version: Some(7),
            edits: vec![TextEdit {
                range: make_range(0, 0, 2),
                new_text: "v4".into(),
            }],
        }];
        let edit = build_workspace_edit(edits, true);
        assert!(edit.changes.is_none(), "must not use unversioned changes");
        let DocumentChanges::Edits(doc_edits) = edit.document_changes.unwrap() else {
            panic!("expected DocumentChanges::Edits");
        };
        assert_eq!(doc_edits.len(), 1);
        assert_eq!(doc_edits[0].text_document.uri, uri);
        assert_eq!(doc_edits[0].text_document.version, Some(7));
        assert_eq!(doc_edits[0].edits.len(), 1);
    }

    #[test]
    fn build_workspace_edit_null_version_for_disk_file() {
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();
        let edits = vec![VersionedFileEdits {
            uri,
            version: None,
            edits: vec![TextEdit {
                range: make_range(0, 0, 2),
                new_text: "v4".into(),
            }],
        }];
        let edit = build_workspace_edit(edits, true);
        let DocumentChanges::Edits(doc_edits) = edit.document_changes.unwrap() else {
            panic!("expected DocumentChanges::Edits");
        };
        assert_eq!(doc_edits[0].text_document.version, None);
    }

    #[test]
    fn build_workspace_edit_mixed_open_and_disk() {
        let open_uri = Uri::from_file_path("/test/open.yml").unwrap();
        let disk_uri = Uri::from_file_path("/test/disk.yml").unwrap();
        let edits = vec![
            VersionedFileEdits {
                uri: open_uri.clone(),
                version: Some(3),
                edits: vec![TextEdit {
                    range: make_range(1, 0, 5),
                    new_text: "v2".into(),
                }],
            },
            VersionedFileEdits {
                uri: disk_uri.clone(),
                version: None,
                edits: vec![TextEdit {
                    range: make_range(2, 0, 5),
                    new_text: "v3".into(),
                }],
            },
        ];
        let edit = build_workspace_edit(edits, true);
        let DocumentChanges::Edits(doc_edits) = edit.document_changes.unwrap() else {
            panic!("expected DocumentChanges::Edits");
        };
        assert_eq!(doc_edits.len(), 2);
        assert_eq!(doc_edits[0].text_document.uri, open_uri);
        assert_eq!(doc_edits[0].text_document.version, Some(3));
        assert_eq!(doc_edits[1].text_document.uri, disk_uri);
        assert_eq!(doc_edits[1].text_document.version, None);
    }

    #[test]
    fn fix_all_result_tracks_actions_checked() {
        let result = FixAllResult {
            edit: WorkspaceEdit::default(),
            total_files: 5,
            files_scanned: 5,
            files_changed: 2,
            refs_updated: 3,
            actions_checked: 10,
            failures: Vec::new(),
            cancelled: false,
        };
        assert_eq!(result.actions_checked, 10);
    }

    #[test]
    fn fix_all_result_tracks_cancelled() {
        let result = FixAllResult {
            edit: WorkspaceEdit::default(),
            total_files: 5,
            files_scanned: 3,
            files_changed: 1,
            refs_updated: 1,
            actions_checked: 5,
            failures: Vec::new(),
            cancelled: true,
        };
        assert!(result.cancelled);
        assert_eq!(result.files_scanned, 3);
    }

    #[test]
    fn fix_all_result_summary_normal() {
        let result = FixAllResult {
            edit: WorkspaceEdit::default(),
            total_files: 5,
            files_scanned: 5,
            files_changed: 2,
            refs_updated: 3,
            actions_checked: 10,
            failures: Vec::new(),
            cancelled: false,
        };
        let summary = result.summary();
        assert!(summary.contains("3 refs"));
        assert!(summary.contains("2 files"));
        assert!(summary.contains("10 actions checked"));
        assert!(!summary.contains("Cancelled"));
    }

    #[test]
    fn fix_all_result_summary_cancelled() {
        let result = FixAllResult {
            edit: WorkspaceEdit::default(),
            total_files: 10,
            files_scanned: 2,
            files_changed: 0,
            refs_updated: 0,
            actions_checked: 5,
            failures: Vec::new(),
            cancelled: true,
        };
        let summary = result.summary();
        assert!(summary.contains("Cancelled"));
        assert!(summary.contains("2/10 files"));
        assert!(summary.contains("5 actions checked"));
    }

    #[test]
    fn edits_from_diagnostics_skips_none_data() {
        let diag = Diagnostic {
            range: make_range(0, 0, 5),
            source: Some(DIAGNOSTIC_SOURCE.into()),
            data: None,
            ..Default::default()
        };
        assert_eq!(edits_from_diagnostics(&[diag]), []);
    }

    #[test]
    fn edits_from_diagnostics_skips_malformed_data() {
        let diag = Diagnostic {
            range: make_range(0, 0, 5),
            source: Some(DIAGNOSTIC_SOURCE.into()),
            data: Some(serde_json::json!({"kind": "unknown_variant"})),
            ..Default::default()
        };
        assert_eq!(edits_from_diagnostics(&[diag]), []);
    }

    #[test]
    fn cancelled_default_is_not_cancelled() {
        let flag = Cancelled::default();
        assert!(!flag.is_cancelled());
    }

    #[test]
    fn fix_all_result_empty_has_no_edits() {
        let result = FixAllResult::empty();
        assert_eq!(result.total_files, 0);
        assert_eq!(result.refs_updated, 0);
        assert!(!result.cancelled);
        assert_eq!(result.failures, []);
    }

    #[test]
    fn edits_from_diagnostics_preserves_order() {
        let diags = vec![
            make_outdated_diagnostic(make_range(10, 31, 33), "v2.0.0"),
            make_outdated_diagnostic(make_range(5, 31, 33), "v3.0.0"),
            make_outdated_diagnostic(make_range(15, 31, 33), "v4.0.0"),
        ];
        let edits = edits_from_diagnostics(&diags);
        assert_eq!(edits.len(), 3);
        assert_eq!(edits[0].range.start.line, 10);
        assert_eq!(edits[1].range.start.line, 5);
        assert_eq!(edits[2].range.start.line, 15);
    }

    #[test]
    fn cancelled_multiple_cancels_idempotent() {
        let flag = Cancelled::new();
        flag.cancel();
        flag.cancel();
        flag.cancel();
        assert!(flag.is_cancelled());
    }

    #[test]
    fn build_workspace_edit_fallback_to_unversioned_changes() {
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();
        let edits = vec![VersionedFileEdits {
            uri: uri.clone(),
            version: Some(7),
            edits: vec![TextEdit {
                range: make_range(0, 0, 2),
                new_text: "v4".into(),
            }],
        }];
        let edit = build_workspace_edit(edits, false);
        assert!(
            edit.document_changes.is_none(),
            "must not use documentChanges when unsupported"
        );
        let changes = edit.changes.unwrap();
        let file_edits = &changes[&uri];
        assert_eq!(file_edits.len(), 1);
        assert_eq!(file_edits[0].new_text, "v4");
    }

    #[test]
    fn relative_display_joins_parts_with_slashes() {
        let root = std::path::PathBuf::from("ws");
        let inside = DiscoveredFile::new(
            root.join(".github").join("workflows").join("a.yml"),
            actup_core::FileKind::Workflow,
        );
        let outside_path = std::path::PathBuf::from("other").join("b.yml");
        let outside = DiscoveredFile::new(outside_path.clone(), actup_core::FileKind::Workflow);

        assert_eq!(relative_display(&inside, &root), ".github/workflows/a.yml");
        assert_eq!(
            relative_display(&outside, &root),
            outside_path.display().to_string()
        );
    }

    #[tokio::test]
    async fn scan_files_refuses_a_disk_file_changed_since_scan()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::TempDir::new()?;
        let workflows_dir = temp.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows_dir)?;
        let changed = workflows_dir.join("a.yml");
        let kept = workflows_dir.join("b.yml");
        let yaml = "jobs:\n  test:\n    steps:\n      - uses: actions/checkout@v3\n";
        std::fs::write(&changed, yaml)?;
        std::fs::write(&kept, yaml)?;
        let edited = yaml.replace("@v3", "@main");

        let versions = test_utils::simple_version_set("v4.2.0", test_utils::DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions)
            .with_rewrite(changed.clone(), &edited);
        let registry = test_utils::mock_registry(provider);
        let cache = FileCache::disabled();
        let documents = DashMap::new();
        let cancelled = Cancelled::new();
        let discovered = discover_files(temp.path(), &[]);
        let config = Config::default();
        let ctx = ScanContext {
            workspace_root: temp.path(),
            config: &config,
            registry: &registry,
            file_cache: &cache,
            documents: &documents,
            cancelled: &cancelled,
            supports_document_changes: false,
            encoding: crate::position::PositionEncoding::Utf16,
        };
        let result = scan_files(&discovered, &ctx, None).await;

        assert_eq!(
            result.failures,
            [(
                ".github/workflows/a.yml".to_string(),
                "edit refused: .github/workflows/a.yml changed since scan".to_string()
            )]
        );
        assert_eq!((result.files_changed, result.refs_updated), (1, 1));
        let edit_map = result.edit.changes.ok_or("no changes")?;
        let uris: Vec<&Uri> = edit_map.keys().collect();
        assert_eq!(uris, [&Uri::from_file_path(&kept).ok_or("no uri")?]);
        assert_eq!(std::fs::read_to_string(&changed)?, edited);
        Ok(())
    }

    #[tokio::test]
    async fn scan_files_refuses_edits_to_invalid_yaml() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::TempDir::new()?;
        let workflows_dir = temp.path().join(".github").join("workflows");
        std::fs::create_dir_all(&workflows_dir)?;
        let yaml = "jobs:\n  test:\n    steps:\n      - uses: actions/checkout@v3\n  bad: [\n";
        std::fs::write(workflows_dir.join("ci.yml"), yaml)?;

        let versions = test_utils::simple_version_set("v4.2.0", test_utils::DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let cache = FileCache::disabled();
        let documents = DashMap::new();
        let cancelled = Cancelled::new();
        let discovered = discover_files(temp.path(), &[]);
        let config = Config::default();
        let mut ctx = validation_ctx(&config, &registry, &cache, &documents, &cancelled);
        ctx.workspace_root = temp.path();
        let result = scan_files(&discovered, &ctx, None).await;

        let [(path, message)] = result.failures.as_slice() else {
            panic!("expected one failure: {:?}", result.failures);
        };
        assert_eq!(path, ".github/workflows/ci.yml");
        assert!(
            message
                .starts_with("edit refused: .github/workflows/ci.yml after edit: invalid YAML: "),
            "{message}"
        );
        assert_eq!((result.files_changed, result.refs_updated), (0, 0));
        assert_eq!(result.edit, WorkspaceEdit::default());
        Ok(())
    }

    fn test_uri() -> Uri {
        let Some(uri) = Uri::from_file_path("/test/ci.yml") else {
            panic!("/test/ci.yml has no file URI");
        };
        uri
    }

    fn loaded(text: &str, version: Option<i32>) -> LoadedFile {
        LoadedFile {
            uri: test_uri(),
            file: DiscoveredFile::new(
                std::path::PathBuf::from("/test/ci.yml"),
                actup_core::FileKind::Workflow,
            ),
            text: text.to_string(),
            version,
        }
    }

    fn open_document(documents: &DashMap<Uri, DocumentState>, text: &str, version: i32) {
        documents.insert(test_uri(), DocumentState::new(text.to_string(), version));
    }

    fn text_edit(line: u32, start: u32, end: u32, new_text: &str) -> TextEdit {
        TextEdit {
            range: tower_lsp_server::ls_types::Range {
                start: Position {
                    line,
                    character: start,
                },
                end: Position {
                    line,
                    character: end,
                },
            },
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn apply_text_edits_places_disjoint_edits() {
        let text = "a: é\nuses: o/r@v1\nb: 1\n";
        let utf16 = crate::position::PositionEncoding::Utf16;
        let edits = [
            text_edit(1, 10, 12, "v2"),
            text_edit(0, 4, 4, "!"),
            text_edit(2, 4, 4, "2"),
            text_edit(3, 0, 0, "c: 3\n"),
        ];
        assert_eq!(
            apply_text_edits(text, &edits, utf16),
            Some("a: é!\nuses: o/r@v2\nb: 12\nc: 3\n".to_string())
        );
        assert_eq!(
            apply_text_edits(
                text,
                &[text_edit(0, 3, 4, "e")],
                crate::position::PositionEncoding::Utf8
            ),
            None
        );
        assert_eq!(
            apply_text_edits(text, &[text_edit(4, 0, 0, "x")], utf16),
            None
        );
        assert_eq!(
            apply_text_edits(
                text,
                &[text_edit(1, 10, 12, "x"), text_edit(1, 11, 12, "y")],
                utf16
            ),
            None
        );
        assert_eq!(
            apply_text_edits(text, &[text_edit(1, 12, 10, "x")], utf16),
            None
        );
    }

    fn validation_ctx<'a>(
        config: &'a Config,
        registry: &'a provider::Registry,
        cache: &'a FileCache,
        documents: &'a DashMap<Uri, DocumentState>,
        cancelled: &'a Cancelled,
    ) -> ScanContext<'a> {
        ScanContext {
            workspace_root: Path::new("/test"),
            config,
            registry,
            file_cache: cache,
            documents,
            cancelled,
            supports_document_changes: true,
            encoding: crate::position::PositionEncoding::Utf16,
        }
    }

    #[tokio::test]
    async fn refuse_edits_rejects_a_corrupting_edit() {
        let text = "steps:\n  - uses: a/b@v1\n";
        let documents = DashMap::new();
        open_document(&documents, text, 2);
        let (config, registry, cache, cancelled) = (
            Config::default(),
            provider::Registry::new(),
            FileCache::disabled(),
            Cancelled::new(),
        );
        let ctx = validation_ctx(&config, &registry, &cache, &documents, &cancelled);
        let file = loaded(text, Some(2));

        assert_eq!(
            refuse_edits(&file, &[text_edit(1, 14, 16, "v2")], &ctx).await,
            None
        );
        let Some(reason) = refuse_edits(&file, &[text_edit(1, 14, 16, "v2\n  - [")], &ctx).await
        else {
            panic!("a corrupting edit must be refused");
        };
        assert!(reason.starts_with("after edit: invalid YAML: "), "{reason}");
        assert_eq!(
            refuse_edits(&file, &[text_edit(1, 14, 16, "v2\n  - uses: c/d@v2")], &ctx).await,
            Some("after edit: `uses:` count changed from 1 to 2".to_string())
        );
        assert_eq!(
            refuse_edits(&file, &[text_edit(9, 0, 0, "x")], &ctx).await,
            Some("edits do not fit the text".to_string())
        );
        assert_eq!(
            refuse_edits(&loaded(text, Some(1)), &[text_edit(1, 14, 16, "v2")], &ctx).await,
            Some("changed since scan".to_string())
        );
    }

    #[tokio::test]
    async fn changed_since_scan_compares_open_documents() {
        let documents = DashMap::new();
        assert!(changed_since_scan(&loaded("a", Some(3)), &documents).await);

        open_document(&documents, "a", 3);
        assert!(!changed_since_scan(&loaded("a", Some(3)), &documents).await);
        assert!(!changed_since_scan(&loaded("a", None), &documents).await);

        open_document(&documents, "b", 4);
        assert!(changed_since_scan(&loaded("a", Some(3)), &documents).await);
        assert!(changed_since_scan(&loaded("a", None), &documents).await);
    }

    #[tokio::test]
    async fn changed_since_scan_rereads_a_closed_file() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::TempDir::new()?;
        let path = temp.path().join("ci.yml");
        std::fs::write(&path, "a")?;
        let mut file = loaded("a", None);
        file.file = DiscoveredFile::new(path.clone(), actup_core::FileKind::Workflow);
        file.uri = Uri::from_file_path(&path).ok_or("no uri")?;
        let documents = DashMap::new();

        assert!(!changed_since_scan(&file, &documents).await);
        std::fs::write(&path, "b")?;
        assert!(changed_since_scan(&file, &documents).await);
        std::fs::remove_file(&path)?;
        assert!(changed_since_scan(&file, &documents).await);
        Ok(())
    }

    #[tokio::test]
    async fn scan_files_prefetches_versions_once_for_duplicate_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        let workflows_dir = temp.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();

        let workflow_a = workflows_dir.join("a.yml");
        let workflow_b = workflows_dir.join("b.yml");
        let yaml = "jobs:\n  test:\n    steps:\n      - uses: actions/checkout@v3\n";
        std::fs::write(&workflow_a, yaml).unwrap();
        std::fs::write(&workflow_b, yaml).unwrap();

        let versions = test_utils::simple_version_set("v4.2.0", test_utils::DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let calls = provider.call_count();
        let registry = test_utils::mock_registry(provider);
        let cache = FileCache::disabled();
        let documents = DashMap::new();
        let cancelled = Cancelled::new();
        let discovered = discover_files(temp.path(), &[]);

        let config = Config::default();
        let ctx = ScanContext {
            workspace_root: temp.path(),
            config: &config,
            registry: &registry,
            file_cache: &cache,
            documents: &documents,
            cancelled: &cancelled,
            supports_document_changes: false,
            encoding: crate::position::PositionEncoding::Utf16,
        };
        let result = scan_files(&discovered, &ctx, None).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.total_files, 2);
        assert_eq!(result.files_scanned, 2);
        assert_eq!(result.files_changed, 2);
        assert_eq!(result.refs_updated, 2);
    }

    #[tokio::test]
    async fn scan_files_edits_markdown_and_text_extra_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::TempDir::new()?;
        std::fs::create_dir_all(temp.path().join("playground"))?;
        let readme = temp.path().join("README.md");
        let script = temp.path().join("playground/index.ts");
        std::fs::write(
            &readme,
            "# Usage\n\n```yaml\n- uses: actions/checkout@v3\n```\nuses: actions/checkout@v3\n",
        )?;
        std::fs::write(
            &script,
            "export const step = { uses: \"actions/checkout@v3\" };\n",
        )?;

        let versions = test_utils::simple_version_set("v4.2.0", test_utils::DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let cache = FileCache::disabled();
        let documents = DashMap::new();
        let cancelled = Cancelled::new();
        let extra = ["README.md".parse()?, "playground/*.ts".parse()?];
        let discovered = discover_files(temp.path(), &extra);

        let config = Config::default();
        let ctx = ScanContext {
            workspace_root: temp.path(),
            config: &config,
            registry: &registry,
            file_cache: &cache,
            documents: &documents,
            cancelled: &cancelled,
            supports_document_changes: false,
            encoding: crate::position::PositionEncoding::Utf16,
        };
        let result = scan_files(&discovered, &ctx, None).await;

        assert_eq!(result.files_changed, 2);
        assert_eq!(result.refs_updated, 2);
        let changes = result.edit.changes.ok_or("no changes")?;
        let lines = |path: &std::path::Path| -> Result<Vec<(u32, u32, String)>, &str> {
            let uri = Uri::from_file_path(path).ok_or("bad file uri")?;
            Ok(changes
                .get(&uri)
                .ok_or("no edits for the file")?
                .iter()
                .map(|e| {
                    (
                        e.range.start.line,
                        e.range.start.character,
                        e.new_text.clone(),
                    )
                })
                .collect())
        };
        let [readme_edit] = lines(&readme)?.try_into().map_err(|_| "one README edit")?;
        let [script_edit] = lines(&script)?.try_into().map_err(|_| "one script edit")?;
        assert_eq!((readme_edit.0, readme_edit.1), (3, 25));
        assert_eq!((script_edit.0, script_edit.1), (0, 46));
        assert_eq!(readme_edit.2, script_edit.2);
        Ok(())
    }
}
