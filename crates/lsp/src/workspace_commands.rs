//! Workspace-wide commands for the LSP server.
//!
//! Implements `actup.fixAllInWorkspace` — discovers all workflow and composite
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

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use actup_core::{Config, DiscoveredFile, discover_files, provider};
use dashmap::DashMap;
use tower_lsp_server::Client;
use tower_lsp_server::ls_types::{
    Diagnostic, DocumentChanges, NumberOrString, OneOf, OptionalVersionedTextDocumentIdentifier,
    TextDocumentEdit, TextEdit, Uri, WorkspaceEdit,
};

use crate::backend::DocumentState;
use crate::diagnostics::{self, DIAGNOSTIC_SOURCE, DiagnosticData};
use crate::file_cache::FileCache;

/// Command identifier registered in server capabilities.
pub const FIX_ALL_IN_WORKSPACE: &str = "actup.fixAllInWorkspace";

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
    ///
    /// Currently unused — will be wired to `window/workDoneProgress/cancel`
    /// once tower-lsp-server supports that notification. The infrastructure
    /// is in place so the cancel button is shown in the editor UI.
    #[allow(dead_code)]
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

/// Extract [`TextEdit`]s for outdated refs from a set of diagnostics.
fn edits_from_diagnostics(diags: &[Diagnostic]) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    for diag in diags {
        if diag.source.as_deref() != Some(DIAGNOSTIC_SOURCE) {
            continue;
        }
        let Some(data) = diag
            .data
            .as_ref()
            .and_then(|v| serde_json::from_value::<DiagnosticData>(v.clone()).ok())
        else {
            continue;
        };
        if let DiagnosticData::Outdated { target_ref, .. } = data {
            edits.push(TextEdit {
                range: diag.range,
                new_text: target_ref,
            });
        }
    }
    edits
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

/// Process a single discovered file: read, compute diagnostics, extract edits.
///
/// For files open in the editor, reads from the document store and snapshots
/// the version atomically with the text content. For files not open in the
/// editor, reads from disk with `version = None`.
///
/// Returns a [`FileOutcome`] with optional edits (including the snapshotted
/// version) and the count of actions checked. Read/parse failures are pushed
/// to `failures` without propagating.
#[allow(clippy::too_many_arguments)]
async fn process_file(
    file: &DiscoveredFile,
    workspace_root: &Path,
    config: &Config,
    registry: &provider::Registry,
    file_cache: &FileCache,
    documents: &DashMap<Uri, DocumentState>,
    failures: &mut Vec<(String, String)>,
    encoding: crate::position::PositionEncoding,
) -> FileOutcome {
    let path = file.path();
    let rel = path
        .strip_prefix(workspace_root)
        .unwrap_or(path)
        .display()
        .to_string();

    let Some(uri) = Uri::from_file_path(path) else {
        tracing::warn!(path = %rel, "failed to convert path to URI");
        failures.push((rel, "failed to convert path to URI".into()));
        return FileOutcome {
            edits: None,
            actions_checked: 0,
        };
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
                return FileOutcome {
                    edits: None,
                    actions_checked: 0,
                };
            }
        }
    };

    if !text.contains("uses:") {
        return FileOutcome {
            edits: None,
            actions_checked: 0,
        };
    }

    let result =
        diagnostics::compute_diagnostics(&text, &uri, config, registry, file_cache, encoding).await;
    let actions_checked = result.refs_checked;
    let edits = edits_from_diagnostics(&result.diagnostics);

    let result_edits = if edits.is_empty() {
        None
    } else {
        Some((uri, version, edits))
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
/// A cancel button is shown in the editor. The [`Cancelled`] flag is checked
/// between files; when set, the operation stops and returns partial results
/// with `cancelled: true`.
#[allow(clippy::too_many_arguments)]
pub async fn fix_all_in_workspace(
    client: Client,
    workspace_root: &Path,
    config: &Config,
    registry: &provider::Registry,
    file_cache: &FileCache,
    documents: &DashMap<Uri, DocumentState>,
    cancelled: &Cancelled,
    supports_document_changes: bool,
    encoding: crate::position::PositionEncoding,
) -> FixAllResult {
    let token = NumberOrString::String(format!("actup-workspace-{}", progress_id()));
    let progress = match client.create_work_done_progress(token.clone()).await {
        Ok(()) => {
            let ongoing = client
                .progress(token, "Fix all outdated actions")
                .with_percentage(0)
                .with_cancel_button()
                .with_message("Discovering files...")
                .begin()
                .await;
            Some(ongoing)
        }
        Err(_) => None,
    };

    let discovered = discover_files(workspace_root, &config.extra_paths);
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

    let result = scan_files(
        &discovered,
        workspace_root,
        config,
        registry,
        file_cache,
        documents,
        cancelled,
        supports_document_changes,
        progress.as_ref(),
        encoding,
    )
    .await;

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
#[allow(clippy::too_many_arguments)]
async fn scan_files(
    discovered: &[DiscoveredFile],
    workspace_root: &Path,
    config: &Config,
    registry: &provider::Registry,
    file_cache: &FileCache,
    documents: &DashMap<Uri, DocumentState>,
    cancelled: &Cancelled,
    supports_document_changes: bool,
    progress: Option<
        &tower_lsp_server::OngoingProgress<
            tower_lsp_server::Bounded,
            tower_lsp_server::Cancellable,
        >,
    >,
    encoding: crate::position::PositionEncoding,
) -> FixAllResult {
    let file_count = discovered.len();
    let mut all_edits: Vec<VersionedFileEdits> = Vec::new();
    let mut failures = Vec::new();
    let mut total_refs = 0usize;
    let mut total_actions = 0usize;
    let mut files_completed = 0usize;

    for (i, file) in discovered.iter().enumerate() {
        if cancelled.is_cancelled() {
            tracing::info!(
                completed = files_completed,
                total = file_count,
                "workspace fix cancelled by client"
            );
            let files_changed = all_edits.len();
            return FixAllResult {
                edit: build_workspace_edit(all_edits, supports_document_changes),
                total_files: file_count,
                files_scanned: files_completed,
                files_changed,
                refs_updated: total_refs,
                actions_checked: total_actions,
                failures,
                cancelled: true,
            };
        }

        let outcome = process_file(
            file,
            workspace_root,
            config,
            registry,
            file_cache,
            documents,
            &mut failures,
            encoding,
        )
        .await;

        total_actions += outcome.actions_checked;
        if let Some((uri, version, edits)) = outcome.edits {
            total_refs += edits.len();
            all_edits.push(VersionedFileEdits {
                uri,
                version,
                edits,
            });
        }

        files_completed = i + 1;
        report_file_progress(
            progress,
            file,
            workspace_root,
            files_completed,
            file_count,
            total_actions,
        )
        .await;
    }

    let files_changed = all_edits.len();
    FixAllResult {
        edit: build_workspace_edit(all_edits, supports_document_changes),
        total_files: file_count,
        files_scanned: file_count,
        files_changed,
        refs_updated: total_refs,
        actions_checked: total_actions,
        failures,
        cancelled: false,
    }
}

/// Report per-file progress to the client.
async fn report_file_progress(
    progress: Option<
        &tower_lsp_server::OngoingProgress<
            tower_lsp_server::Bounded,
            tower_lsp_server::Cancellable,
        >,
    >,
    file: &DiscoveredFile,
    workspace_root: &Path,
    files_completed: usize,
    file_count: usize,
    total_actions: usize,
) {
    let Some(p) = progress else { return };

    #[allow(clippy::cast_possible_truncation)]
    let pct = (5 + (files_completed * 90) / file_count) as u32;
    let rel = file
        .path()
        .strip_prefix(workspace_root)
        .unwrap_or_else(|_| file.path())
        .display();
    let msg = if total_actions > 0 {
        format!(
            "Checking {rel}... ({total_actions} actions in {files_completed}/{file_count} files)"
        )
    } else {
        format!("Checking {rel}... ({files_completed}/{file_count} files)")
    };
    p.report_with_message(msg, pct.min(95), None).await;
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
    use super::*;

    use crate::test_utils;

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
        assert_eq!(FIX_ALL_IN_WORKSPACE, "actup.fixAllInWorkspace");
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
        assert!(result.failures.is_empty());
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
    fn edits_from_diagnostics_skips_non_actup() {
        let diag = Diagnostic {
            range: make_range(0, 0, 5),
            source: Some("other-tool".into()),
            ..Default::default()
        };
        assert!(edits_from_diagnostics(&[diag]).is_empty());
    }

    #[test]
    fn edits_from_diagnostics_skips_unresolvable() {
        let data = DiagnosticData::Unresolvable {
            suggested_ref: Some("v2.0.0".into()),
        };
        let diag = Diagnostic {
            range: make_range(0, 0, 5),
            source: Some(DIAGNOSTIC_SOURCE.into()),
            data: serde_json::to_value(data).ok(),
            ..Default::default()
        };
        // Unresolvable refs are NOT auto-fixed in workspace-wide operations.
        assert!(edits_from_diagnostics(&[diag]).is_empty());
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
        assert!(edits_from_diagnostics(&[diag]).is_empty());
    }

    #[test]
    fn edits_from_diagnostics_skips_malformed_data() {
        let diag = Diagnostic {
            range: make_range(0, 0, 5),
            source: Some(DIAGNOSTIC_SOURCE.into()),
            data: Some(serde_json::json!({"kind": "unknown_variant"})),
            ..Default::default()
        };
        assert!(edits_from_diagnostics(&[diag]).is_empty());
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
        assert!(result.failures.is_empty());
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
}
