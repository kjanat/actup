//! Code action generation for `uses:` reference diagnostics.
//!
//! Produces [`CodeAction`]s from diagnostics, enabling:
//! - Single-ref quick fixes (e.g. "Update to vX.Y.Z") for outdated refs
//! - Fix-all-in-file source action that updates every outdated ref at once
//!
//! Entry points:
//! - [`code_actions_for_diagnostics`]: per-diagnostic quickfixes
//! - [`fix_all_in_file`]: aggregated source action for all outdated refs

use std::collections::HashMap;

use tower_lsp_server::ls_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Diagnostic, DocumentChanges, OneOf,
    OptionalVersionedTextDocumentIdentifier, TextDocumentEdit, TextEdit, Uri, WorkspaceEdit,
};

use crate::diagnostics::{DIAGNOSTIC_SOURCE, DiagnosticData};

/// Custom [`CodeActionKind`] for actup's fix-all source action.
///
/// Registered in server capabilities so editors show it in the
/// "Source Action" menu. Hierarchically under `source.fixAll` per LSP spec.
pub const SOURCE_FIX_ALL_ACTUP: CodeActionKind = CodeActionKind::new("source.fixAll.actup");

/// Build a [`WorkspaceEdit`] for a single file, choosing between versioned
/// `documentChanges` and unversioned `changes` based on client capability.
///
/// When `supports_document_changes` is `true`, uses
/// [`OptionalVersionedTextDocumentIdentifier`] with the document `version`
/// so clients can reject edits targeting a stale document.
///
/// When `false`, falls back to the `changes` map (no version tracking).
fn make_edit(
    uri: &Uri,
    version: Option<i32>,
    edits: Vec<TextEdit>,
    supports_document_changes: bool,
) -> WorkspaceEdit {
    if supports_document_changes {
        WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version,
                },
                edits: edits.into_iter().map(OneOf::Left).collect(),
            }])),
            ..Default::default()
        }
    } else {
        let mut changes = HashMap::new();
        changes.insert(uri.clone(), edits);
        WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }
    }
}

/// Build code actions from a slice of diagnostics.
///
/// For each `Outdated` diagnostic, produces a quickfix "Update to vX.Y.Z"
/// action with a [`WorkspaceEdit`] that replaces only the ref portion.
///
/// For each `Unresolvable` diagnostic with a suggestion, produces a quickfix
/// "Change to vX.Y.Z" action.
///
/// Diagnostics without embedded [`DiagnosticData`] or from other sources
/// are silently skipped.
///
/// `version` is the document version from `DocumentState` — `Some(n)` for
/// tracked documents, `None` for untracked (shouldn't happen in normal flow).
///
/// `supports_document_changes` gates whether the edit uses versioned
/// `documentChanges` or falls back to unversioned `changes`.
pub fn code_actions_for_diagnostics(
    uri: &Uri,
    version: Option<i32>,
    diagnostics: &[Diagnostic],
    supports_document_changes: bool,
) -> Vec<CodeActionOrCommand> {
    let mut actions = Vec::new();

    for diag in diagnostics {
        // Only process our own diagnostics.
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

        match data {
            DiagnosticData::Outdated { target_ref, .. } => {
                let edit = TextEdit {
                    range: diag.range,
                    new_text: target_ref.clone(),
                };

                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Update to {target_ref}"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diag.clone()]),
                    edit: Some(make_edit(
                        uri,
                        version,
                        vec![edit],
                        supports_document_changes,
                    )),
                    is_preferred: Some(true),
                    ..Default::default()
                }));
            }
            DiagnosticData::Unresolvable {
                suggested_ref: Some(suggested),
            } => {
                let edit = TextEdit {
                    range: diag.range,
                    new_text: suggested.clone(),
                };

                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Change to {suggested}"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diag.clone()]),
                    edit: Some(make_edit(
                        uri,
                        version,
                        vec![edit],
                        supports_document_changes,
                    )),
                    is_preferred: Some(false),
                    ..Default::default()
                }));
            }
            DiagnosticData::Unresolvable {
                suggested_ref: None,
            } => {}
        }
    }

    actions
}

/// Build a single "Fix all outdated actions" source action for the file.
///
/// Aggregates all `Outdated` diagnostics into one [`WorkspaceEdit`] with
/// multiple [`TextEdit`]s. Returns `None` if there are fewer than 2
/// actionable diagnostics (single-ref quickfixes already cover that case).
///
/// The action uses [`SOURCE_FIX_ALL_ACTUP`] kind so it appears in the
/// editor's "Source Action" menu and responds to `source.fixAll` requests.
///
/// `version` is the document version from `DocumentState` — `Some(n)` for
/// tracked documents, `None` for untracked.
///
/// `supports_document_changes` gates whether the edit uses versioned
/// `documentChanges` or falls back to unversioned `changes`.
pub fn fix_all_in_file(
    uri: &Uri,
    version: Option<i32>,
    diagnostics: &[Diagnostic],
    supports_document_changes: bool,
) -> Option<CodeActionOrCommand> {
    let mut edits = Vec::new();
    let mut fixed_diagnostics = Vec::new();

    for diag in diagnostics {
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
            fixed_diagnostics.push(diag.clone());
        }
    }

    // Not useful as a bulk action if there's fewer than 2 fixable refs.
    if edits.len() < 2 {
        return None;
    }

    let count = fixed_diagnostics.len();
    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Fix all outdated actions ({count} refs)"),
        kind: Some(SOURCE_FIX_ALL_ACTUP),
        diagnostics: Some(fixed_diagnostics),
        edit: Some(make_edit(uri, version, edits, supports_document_changes)),
        is_preferred: Some(false),
        ..Default::default()
    }))
}

/// Returns `true` if `requested` is a parent kind of `candidate`.
///
/// In LSP, kind `source.fixAll` is a prefix of `source.fixAll.actup`,
/// meaning a request for `source.fixAll` should include our action.
fn kind_matches(requested: &CodeActionKind, candidate: &CodeActionKind) -> bool {
    let req = requested.as_str();
    let cand = candidate.as_str();
    cand == req || cand.starts_with(req) && cand.as_bytes().get(req.len()) == Some(&b'.')
}

/// Check whether the `context.only` filter (if present) includes a kind.
///
/// If `only` is `None`, all kinds are allowed. If `Some`, at least one
/// entry must match via [`kind_matches`] (hierarchical prefix match).
pub fn kind_allowed(only: Option<&[CodeActionKind]>, kind: &CodeActionKind) -> bool {
    only.is_none_or(|allowed| allowed.iter().any(|a| kind_matches(a, kind)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use crate::test_utils;

    /// Document version used across all code-action tests.
    const TEST_VERSION: Option<i32> = Some(5);

    fn make_range(line: u32, col_start: u32, col_end: u32) -> tower_lsp_server::ls_types::Range {
        test_utils::make_range(line, col_start, col_end)
    }

    fn make_outdated_diagnostic(
        range: tower_lsp_server::ls_types::Range,
        uses_value: &str,
        target_ref: &str,
    ) -> Diagnostic {
        test_utils::make_outdated_diagnostic(range, uses_value, target_ref)
    }

    fn make_unresolvable_diagnostic(
        range: tower_lsp_server::ls_types::Range,
        suggested: Option<&str>,
    ) -> Diagnostic {
        test_utils::make_unresolvable_diagnostic(range, suggested)
    }

    /// Extract [`TextEdit`]s from a versioned [`WorkspaceEdit`], asserting
    /// the edit uses `document_changes` with the expected URI and version.
    fn extract_text_edits<'a>(
        edit: &'a WorkspaceEdit,
        expected_uri: &Uri,
        expected_version: Option<i32>,
    ) -> Vec<&'a TextEdit> {
        assert!(
            edit.changes.is_none(),
            "versioned edits must not use unversioned `changes`"
        );
        let doc_changes = edit
            .document_changes
            .as_ref()
            .expect("expected document_changes");
        let DocumentChanges::Edits(doc_edits) = doc_changes else {
            panic!("expected DocumentChanges::Edits");
        };
        assert_eq!(doc_edits.len(), 1, "expected one TextDocumentEdit");
        let doc_edit = &doc_edits[0];
        assert_eq!(&doc_edit.text_document.uri, expected_uri);
        assert_eq!(doc_edit.text_document.version, expected_version);

        doc_edit
            .edits
            .iter()
            .map(|e| match e {
                OneOf::Left(text_edit) => text_edit,
                OneOf::Right(_) => panic!("expected TextEdit, got AnnotatedTextEdit"),
            })
            .collect()
    }

    #[test]
    fn outdated_diagnostic_produces_quickfix() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag = make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        assert_eq!(actions.len(), 1);

        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };
        assert_eq!(action.title, "Update to v4.2.0");
        assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
        assert_eq!(action.is_preferred, Some(true));

        let edits = extract_text_edits(action.edit.as_ref().unwrap(), &uri, TEST_VERSION);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "v4.2.0");
        assert_eq!(edits[0].range, make_range(7, 31, 33));
    }

    #[test]
    fn outdated_action_links_diagnostic() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag = make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");

        let actions =
            code_actions_for_diagnostics(&uri, TEST_VERSION, std::slice::from_ref(&diag), true);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };

        // Code action must link back to the triggering diagnostic.
        let linked = action.diagnostics.as_ref().unwrap();
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].range, diag.range);
        assert_eq!(linked[0].message, diag.message);
    }

    #[test]
    fn unresolvable_with_suggestion_produces_quickfix() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag = make_unresolvable_diagnostic(make_range(3, 10, 15), Some("v2.1.0"));

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        assert_eq!(actions.len(), 1);

        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };
        assert_eq!(action.title, "Change to v2.1.0");
        assert_eq!(action.is_preferred, Some(false));
    }

    #[test]
    fn unresolvable_without_suggestion_produces_no_action() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag = make_unresolvable_diagnostic(make_range(3, 10, 15), None);

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        assert!(actions.is_empty());
    }

    #[test]
    fn foreign_diagnostic_source_ignored() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag = Diagnostic {
            range: make_range(5, 0, 10),
            source: Some("other-tool".into()),
            ..Default::default()
        };

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        assert!(actions.is_empty());
    }

    #[test]
    fn diagnostic_without_data_ignored() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag = Diagnostic {
            range: make_range(5, 0, 10),
            source: Some(DIAGNOSTIC_SOURCE.into()),
            data: None,
            ..Default::default()
        };

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        assert!(actions.is_empty());
    }

    #[test]
    fn multiple_diagnostics_produce_multiple_actions() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag1 =
            make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");
        let diag2 =
            make_outdated_diagnostic(make_range(8, 31, 33), "actions/setup-node@v3", "v4.0.0");

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag1, diag2], true);
        assert_eq!(actions.len(), 2);

        // Each action targets its own diagnostic.
        let CodeActionOrCommand::CodeAction(a1) = &actions[0] else {
            panic!("expected CodeAction");
        };
        let CodeActionOrCommand::CodeAction(a2) = &actions[1] else {
            panic!("expected CodeAction");
        };
        assert_eq!(a1.title, "Update to v4.2.0");
        assert_eq!(a2.title, "Update to v4.0.0");
    }

    #[test]
    fn diagnostic_data_roundtrips_through_json() {
        let data = DiagnosticData::Outdated {
            target_ref: "v5.0.0".into(),
            target_sha: Some("abc123def456abc123def456abc123def456abcd".into()),
        };
        let json = serde_json::to_value(&data).unwrap();
        let back: DiagnosticData = serde_json::from_value(json).unwrap();

        match back {
            DiagnosticData::Outdated {
                target_ref,
                target_sha,
            } => {
                assert_eq!(target_ref, "v5.0.0");
                assert_eq!(
                    target_sha.as_deref(),
                    Some("abc123def456abc123def456abc123def456abcd"),
                );
            }
            DiagnosticData::Unresolvable { .. } => panic!("wrong variant"),
        }
    }

    #[test]
    fn unresolvable_data_roundtrips_through_json() {
        let data = DiagnosticData::Unresolvable {
            suggested_ref: Some("v3.1.0".into()),
        };
        let json = serde_json::to_value(&data).unwrap();
        let back: DiagnosticData = serde_json::from_value(json).unwrap();

        match back {
            DiagnosticData::Unresolvable { suggested_ref } => {
                assert_eq!(suggested_ref.as_deref(), Some("v3.1.0"));
            }
            DiagnosticData::Outdated { .. } => panic!("wrong variant"),
        }
    }

    #[test]
    fn fix_all_returns_none_for_single_outdated() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag = make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");

        // Single outdated ref — quickfix already covers this, no fix-all needed.
        assert!(fix_all_in_file(&uri, TEST_VERSION, &[diag], true).is_none());
    }

    #[test]
    fn fix_all_returns_none_for_empty_diagnostics() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        assert!(fix_all_in_file(&uri, TEST_VERSION, &[], true).is_none());
    }

    #[test]
    fn fix_all_returns_none_for_non_outdated_diagnostics() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag1 = make_unresolvable_diagnostic(make_range(3, 10, 15), Some("v2.1.0"));
        let diag2 = make_unresolvable_diagnostic(make_range(5, 10, 15), None);

        // Only unresolvable diagnostics — fix-all only handles Outdated.
        assert!(fix_all_in_file(&uri, TEST_VERSION, &[diag1, diag2], true).is_none());
    }

    #[test]
    fn fix_all_aggregates_multiple_outdated() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag1 =
            make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");
        let diag2 =
            make_outdated_diagnostic(make_range(8, 34, 36), "actions/setup-node@v3", "v4.0.0");
        let diag3 = make_outdated_diagnostic(make_range(12, 20, 22), "actions/cache@v2", "v4.1.0");

        let action = fix_all_in_file(&uri, TEST_VERSION, &[diag1, diag2, diag3], true).unwrap();
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected CodeAction");
        };

        assert_eq!(action.title, "Fix all outdated actions (3 refs)");
        assert_eq!(action.kind, Some(SOURCE_FIX_ALL_ACTUP));
        assert_eq!(action.is_preferred, Some(false));

        let edits = extract_text_edits(action.edit.as_ref().unwrap(), &uri, TEST_VERSION);
        assert_eq!(edits.len(), 3);
        assert_eq!(edits[0].new_text, "v4.2.0");
        assert_eq!(edits[1].new_text, "v4.0.0");
        assert_eq!(edits[2].new_text, "v4.1.0");

        // All 3 diagnostics linked.
        let linked = action.diagnostics.as_ref().unwrap();
        assert_eq!(linked.len(), 3);
    }

    #[test]
    fn fix_all_skips_foreign_and_non_outdated() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let outdated1 =
            make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");
        let outdated2 =
            make_outdated_diagnostic(make_range(8, 31, 33), "actions/setup-node@v3", "v4.0.0");
        let foreign = Diagnostic {
            range: make_range(9, 0, 10),
            source: Some("other-tool".into()),
            ..Default::default()
        };
        let unresolvable = make_unresolvable_diagnostic(make_range(10, 5, 12), None);

        let action = fix_all_in_file(
            &uri,
            TEST_VERSION,
            &[outdated1, foreign, unresolvable, outdated2],
            true,
        )
        .unwrap();
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected CodeAction");
        };

        // Only the 2 outdated refs are in the edit.
        let edits = extract_text_edits(action.edit.as_ref().unwrap(), &uri, TEST_VERSION);
        assert_eq!(edits.len(), 2);
        assert_eq!(action.diagnostics.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn kind_allowed_none_allows_all() {
        assert!(kind_allowed(None, &CodeActionKind::QUICKFIX));
        assert!(kind_allowed(None, &SOURCE_FIX_ALL_ACTUP));
    }

    #[test]
    fn kind_allowed_exact_match() {
        let only = vec![SOURCE_FIX_ALL_ACTUP];
        assert!(kind_allowed(Some(&only), &SOURCE_FIX_ALL_ACTUP));
        assert!(!kind_allowed(Some(&only), &CodeActionKind::QUICKFIX));
    }

    #[test]
    fn kind_allowed_parent_match() {
        // Requesting source.fixAll should include source.fixAll.actup.
        let only = vec![CodeActionKind::SOURCE_FIX_ALL];
        assert!(kind_allowed(Some(&only), &SOURCE_FIX_ALL_ACTUP));
    }

    #[test]
    fn kind_allowed_no_partial_prefix_collision() {
        // "source.fix" is NOT a parent of "source.fixAll.actup" (no dot boundary).
        let only = vec![CodeActionKind::new("source.fix")];
        assert!(!kind_allowed(Some(&only), &SOURCE_FIX_ALL_ACTUP));
    }

    #[test]
    fn kind_allowed_empty_only_allows_nothing() {
        let only: Vec<CodeActionKind> = vec![];
        assert!(!kind_allowed(Some(&only), &CodeActionKind::QUICKFIX));
    }

    // --- Edit generation preserves formatting ---

    #[test]
    fn edit_replaces_only_ref_range() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        // Range covers just "v3" (2 chars), not the entire `uses:` line.
        let diag = make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };

        let edits = extract_text_edits(action.edit.as_ref().unwrap(), &uri, TEST_VERSION);
        assert_eq!(edits.len(), 1);

        // Only the ref portion is replaced, not the whole line.
        assert_eq!(edits[0].range.start.line, 7);
        assert_eq!(edits[0].range.start.character, 31);
        assert_eq!(edits[0].range.end.character, 33);
        assert_eq!(edits[0].new_text, "v4.2.0");
    }

    #[test]
    fn edit_handles_longer_replacement() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        // Replacing "v3" (2 chars) with "v4.2.0" (6 chars).
        let diag = make_outdated_diagnostic(make_range(0, 10, 12), "o/r@v3", "v4.2.0");

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };

        let edits = extract_text_edits(action.edit.as_ref().unwrap(), &uri, TEST_VERSION);
        // The range stays the same (2 chars), but the new_text is longer.
        // LSP handles the expansion — we just provide the replacement text.
        assert_eq!(edits[0].range, make_range(0, 10, 12));
        assert_eq!(edits[0].new_text, "v4.2.0");
    }

    #[test]
    fn fix_all_edits_maintain_per_line_ranges() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag1 =
            make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");
        let diag2 =
            make_outdated_diagnostic(make_range(8, 34, 36), "actions/setup-node@v3", "v4.0.0");
        let diag3 = make_outdated_diagnostic(make_range(12, 20, 22), "actions/cache@v2", "v4.1.0");

        let action = fix_all_in_file(&uri, TEST_VERSION, &[diag1, diag2, diag3], true).unwrap();
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected CodeAction");
        };

        let edits = extract_text_edits(action.edit.as_ref().unwrap(), &uri, TEST_VERSION);

        // Each edit preserves its specific range — no overlap or combined ranges.
        assert_eq!(edits[0].range, make_range(7, 31, 33));
        assert_eq!(edits[1].range, make_range(8, 34, 36));
        assert_eq!(edits[2].range, make_range(12, 20, 22));
    }

    #[test]
    fn fix_all_title_includes_ref_count() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag1 = make_outdated_diagnostic(make_range(0, 10, 12), "a/b@v1", "v2.0.0");
        let diag2 = make_outdated_diagnostic(make_range(1, 10, 12), "a/c@v1", "v2.0.0");

        let action = fix_all_in_file(&uri, TEST_VERSION, &[diag1, diag2], true).unwrap();
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected CodeAction");
        };
        assert_eq!(action.title, "Fix all outdated actions (2 refs)");
    }

    #[test]
    fn code_action_edit_targets_correct_uri() {
        let uri = Uri::from_file_path("/project/.github/workflows/ci.yml").unwrap();
        let diag = make_outdated_diagnostic(make_range(0, 0, 2), "o/r@v1", "v2.0.0");

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };

        // extract_text_edits already asserts URI matches.
        let edits = extract_text_edits(action.edit.as_ref().unwrap(), &uri, TEST_VERSION);
        assert_eq!(edits.len(), 1);
    }

    #[test]
    fn mixed_diagnostics_produce_correct_action_types() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let outdated = make_outdated_diagnostic(make_range(0, 10, 12), "a/b@v1", "v2.0.0");
        let suggestion = make_unresolvable_diagnostic(make_range(1, 10, 15), Some("v3.0.0"));
        let no_suggestion = make_unresolvable_diagnostic(make_range(2, 10, 15), None);

        let actions = code_actions_for_diagnostics(
            &uri,
            TEST_VERSION,
            &[outdated, suggestion, no_suggestion],
            true,
        );
        assert_eq!(actions.len(), 2); // outdated + suggestion, NOT the no_suggestion

        let CodeActionOrCommand::CodeAction(a0) = &actions[0] else {
            panic!("expected CodeAction");
        };
        let CodeActionOrCommand::CodeAction(a1) = &actions[1] else {
            panic!("expected CodeAction");
        };
        assert!(a0.title.starts_with("Update to"));
        assert!(a1.title.starts_with("Change to"));
        assert_eq!(a0.is_preferred, Some(true));
        assert_eq!(a1.is_preferred, Some(false));
    }

    #[test]
    fn kind_allowed_with_multiple_kinds() {
        let only = vec![CodeActionKind::QUICKFIX, CodeActionKind::SOURCE_FIX_ALL];
        assert!(kind_allowed(Some(&only), &SOURCE_FIX_ALL_ACTUP));
        assert!(kind_allowed(Some(&only), &CodeActionKind::QUICKFIX));
    }

    #[test]
    fn fix_all_preserves_diagnostic_order() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag1 = make_outdated_diagnostic(make_range(12, 20, 22), "a/c@v2", "v4.1.0");
        let diag2 = make_outdated_diagnostic(make_range(7, 31, 33), "a/b@v3", "v4.2.0");

        let action = fix_all_in_file(&uri, TEST_VERSION, &[diag1, diag2], true).unwrap();
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected CodeAction");
        };

        let edits = extract_text_edits(action.edit.as_ref().unwrap(), &uri, TEST_VERSION);
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].range.start.line, 12);
        assert_eq!(edits[1].range.start.line, 7);
    }

    #[test]
    fn diagnostic_data_json_has_kind_field() {
        let data = DiagnosticData::Outdated {
            target_ref: "v2.0.0".into(),
            target_sha: None,
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json.get("kind").unwrap().as_str(), Some("outdated"));
        // target_sha is None, should be omitted from JSON
        assert!(json.get("target_sha").is_none());
    }

    #[test]
    fn versioned_edit_does_not_use_unversioned_changes() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag = make_outdated_diagnostic(make_range(0, 10, 12), "o/r@v1", "v2.0.0");

        let actions = code_actions_for_diagnostics(&uri, Some(42), &[diag], true);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };
        let edit = action.edit.as_ref().unwrap();

        // Must use document_changes, never unversioned changes.
        assert!(edit.changes.is_none());
        assert!(edit.document_changes.is_some());

        // Version must be exactly what was passed.
        let DocumentChanges::Edits(doc_edits) = edit.document_changes.as_ref().unwrap() else {
            panic!("expected Edits");
        };
        assert_eq!(doc_edits[0].text_document.version, Some(42));
    }

    #[test]
    fn fallback_to_unversioned_changes_when_not_supported() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag = make_outdated_diagnostic(make_range(0, 10, 12), "o/r@v1", "v2.0.0");

        let actions = code_actions_for_diagnostics(&uri, Some(5), &[diag], false);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };
        let edit = action.edit.as_ref().unwrap();

        // Must use unversioned changes, not document_changes.
        assert!(edit.document_changes.is_none());
        let changes = edit.changes.as_ref().expect("expected changes map");
        let file_edits = changes.get(&uri).expect("expected edits for URI");
        assert_eq!(file_edits.len(), 1);
        assert_eq!(file_edits[0].new_text, "v2.0.0");
    }

    #[test]
    fn fix_all_fallback_to_unversioned_changes() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag1 = make_outdated_diagnostic(make_range(0, 10, 12), "a/b@v1", "v2.0.0");
        let diag2 = make_outdated_diagnostic(make_range(1, 10, 12), "a/c@v1", "v3.0.0");

        let action = fix_all_in_file(&uri, Some(5), &[diag1, diag2], false).unwrap();
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected CodeAction");
        };
        let edit = action.edit.as_ref().unwrap();

        assert!(edit.document_changes.is_none());
        let changes = edit.changes.as_ref().expect("expected changes map");
        let file_edits = changes.get(&uri).expect("expected edits for URI");
        assert_eq!(file_edits.len(), 2);
    }

    #[test]
    fn none_version_for_untracked_document() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag = make_outdated_diagnostic(make_range(0, 10, 12), "o/r@v1", "v2.0.0");

        let actions = code_actions_for_diagnostics(&uri, None, &[diag], true);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };
        let edit = action.edit.as_ref().unwrap();
        let DocumentChanges::Edits(doc_edits) = edit.document_changes.as_ref().unwrap() else {
            panic!("expected Edits");
        };
        // Untracked document should have version None (null).
        assert_eq!(doc_edits[0].text_document.version, None);
    }
}
