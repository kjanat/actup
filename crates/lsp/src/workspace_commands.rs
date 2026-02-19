//! Workspace-wide commands for the LSP server.
//!
//! Implements `actup.fixAllInWorkspace` — discovers all workflow and composite
//! action files, computes diagnostics, and applies a single multi-file
//! [`WorkspaceEdit`] with all outdated ref replacements.

use std::collections::HashMap;
use std::path::Path;

use actup_core::{Config, DiscoveredFile, discover_files, provider};
use tower_lsp_server::Client;
use tower_lsp_server::ls_types::{Diagnostic, NumberOrString, TextEdit, Uri, WorkspaceEdit};

use crate::diagnostics::{self, DIAGNOSTIC_SOURCE, DiagnosticData};
use crate::file_cache::FileCache;

/// Command identifier registered in server capabilities.
pub const FIX_ALL_IN_WORKSPACE: &str = "actup.fixAllInWorkspace";

/// Result of a workspace-wide fix operation.
#[derive(Debug)]
pub struct FixAllResult {
    /// The workspace edit to apply (may span multiple files).
    pub edit: WorkspaceEdit,
    /// Number of files scanned.
    pub files_scanned: usize,
    /// Number of files with edits.
    pub files_changed: usize,
    /// Number of individual ref replacements.
    pub refs_updated: usize,
    /// Files that failed during diagnostic computation (path, error message).
    pub failures: Vec<(String, String)>,
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
        if let DiagnosticData::Outdated { target_ref } = data {
            edits.push(TextEdit {
                range: diag.range,
                new_text: target_ref,
            });
        }
    }
    edits
}

/// Process a single discovered file: read, compute diagnostics, extract edits.
///
/// Returns `Some((uri, edits))` if the file has outdated refs, `None` if
/// the file is up-to-date, skipped, or unreadable (errors pushed to `failures`).
async fn process_file(
    file: &DiscoveredFile,
    workspace_root: &Path,
    config: &Config,
    registry: &provider::Registry,
    file_cache: &FileCache,
    failures: &mut Vec<(String, String)>,
) -> Option<(Uri, Vec<TextEdit>)> {
    let path = file.path();
    let rel = path
        .strip_prefix(workspace_root)
        .unwrap_or(path)
        .display()
        .to_string();

    let text = match tokio::fs::read_to_string(path).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(path = %rel, error = %e, "failed to read file");
            failures.push((rel, e.to_string()));
            return None;
        }
    };

    if !text.contains("uses:") {
        return None;
    }

    let uri = Uri::from_file_path(path)?;
    let diags = diagnostics::compute_diagnostics(&text, &uri, config, registry, file_cache).await;
    let edits = edits_from_diagnostics(&diags);

    if edits.is_empty() {
        None
    } else {
        Some((uri, edits))
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
pub async fn fix_all_in_workspace(
    client: Client,
    workspace_root: &Path,
    config: &Config,
    registry: &provider::Registry,
    file_cache: &FileCache,
) -> FixAllResult {
    let token = NumberOrString::String(format!("actup-workspace-{}", progress_id()));
    let progress = match client.create_work_done_progress(token.clone()).await {
        Ok(()) => {
            let ongoing = client
                .progress(token, "Fix all outdated actions")
                .with_percentage(0)
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
        return FixAllResult {
            edit: WorkspaceEdit::default(),
            files_scanned: 0,
            files_changed: 0,
            refs_updated: 0,
            failures: Vec::new(),
        };
    }

    if let Some(ref p) = progress {
        p.report_with_message(format!("Scanning {file_count} files..."), 5)
            .await;
    }

    let mut all_changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    let mut failures = Vec::new();
    let mut total_refs = 0usize;

    for (i, file) in discovered.iter().enumerate() {
        if let Some((uri, edits)) = process_file(
            file,
            workspace_root,
            config,
            registry,
            file_cache,
            &mut failures,
        )
        .await
        {
            total_refs += edits.len();
            all_changes.insert(uri, edits);
        }

        if let Some(ref p) = progress {
            #[allow(clippy::cast_possible_truncation)]
            let pct = (5 + ((i + 1) * 90) / file_count) as u32;
            let rel = file
                .path()
                .strip_prefix(workspace_root)
                .unwrap_or_else(|_| file.path())
                .display();
            p.report_with_message(format!("Checking {rel}..."), pct.min(95))
                .await;
        }
    }

    let files_changed = all_changes.len();
    let edit = if all_changes.is_empty() {
        WorkspaceEdit::default()
    } else {
        WorkspaceEdit {
            changes: Some(all_changes),
            ..Default::default()
        }
    };

    let summary = format!("Updated {total_refs} refs in {files_changed} files");
    if let Some(p) = progress {
        p.finish_with_message(summary).await;
    }

    FixAllResult {
        edit,
        files_scanned: file_count,
        files_changed,
        refs_updated: total_refs,
        failures,
    }
}

/// Monotonic progress ID for unique tokens.
fn progress_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{DiagnosticSeverity, NumberOrString, Position, Range};

    fn make_range(line: u32, col_start: u32, col_end: u32) -> Range {
        Range {
            start: Position {
                line,
                character: col_start,
            },
            end: Position {
                line,
                character: col_end,
            },
        }
    }

    fn make_outdated_diagnostic(range: Range, target_ref: &str) -> Diagnostic {
        let data = DiagnosticData::Outdated {
            target_ref: target_ref.into(),
        };
        Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String("outdated".into())),
            source: Some(DIAGNOSTIC_SOURCE.into()),
            message: format!("o/r@old -> {target_ref}"),
            data: serde_json::to_value(data).ok(),
            ..Default::default()
        }
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
            files_scanned: 0,
            files_changed: 0,
            refs_updated: 0,
            failures: Vec::new(),
        };
        assert_eq!(result.files_scanned, 0);
        assert_eq!(result.refs_updated, 0);
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
}
