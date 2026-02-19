//! Code action generation for `uses:` reference diagnostics.
//!
//! Produces [`CodeAction`]s from diagnostics, enabling single-ref
//! quick fixes (e.g. "Update to vX.Y.Z") for outdated action references.
//!
//! The primary entry point is [`code_actions_for_diagnostics`], which takes
//! the diagnostics the client already associated with the cursor/selection
//! range and generates appropriate code actions from their embedded data.

use std::collections::HashMap;

use tower_lsp_server::ls_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Diagnostic, TextEdit, Uri, WorkspaceEdit,
};

use crate::diagnostics::{DIAGNOSTIC_SOURCE, DiagnosticData};

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
pub fn code_actions_for_diagnostics(
    uri: &Uri,
    diagnostics: &[Diagnostic],
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
            DiagnosticData::Outdated { target_ref } => {
                let edit = TextEdit {
                    range: diag.range,
                    new_text: target_ref.clone(),
                };
                let mut changes = HashMap::new();
                changes.insert(uri.clone(), vec![edit]);

                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Update to {target_ref}"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diag.clone()]),
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes),
                        ..Default::default()
                    }),
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
                let mut changes = HashMap::new();
                changes.insert(uri.clone(), vec![edit]);

                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Change to {suggested}"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diag.clone()]),
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes),
                        ..Default::default()
                    }),
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

    fn make_outdated_diagnostic(range: Range, uses_value: &str, target_ref: &str) -> Diagnostic {
        let data = DiagnosticData::Outdated {
            target_ref: target_ref.into(),
        };
        Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String("outdated".into())),
            source: Some(DIAGNOSTIC_SOURCE.into()),
            message: format!("{uses_value}: v1.0.0 -> {target_ref} (abc1234)"),
            data: serde_json::to_value(data).ok(),
            ..Default::default()
        }
    }

    fn make_unresolvable_diagnostic(range: Range, suggested: Option<&str>) -> Diagnostic {
        let data = DiagnosticData::Unresolvable {
            suggested_ref: suggested.map(String::from),
        };
        Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(NumberOrString::String("unresolvable".into())),
            source: Some(DIAGNOSTIC_SOURCE.into()),
            message: "bad/repo@v1: repository not found".into(),
            data: serde_json::to_value(data).ok(),
            ..Default::default()
        }
    }

    #[test]
    fn outdated_diagnostic_produces_quickfix() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag = make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");

        let actions = code_actions_for_diagnostics(&uri, &[diag]);
        assert_eq!(actions.len(), 1);

        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };
        assert_eq!(action.title, "Update to v4.2.0");
        assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
        assert_eq!(action.is_preferred, Some(true));

        // Verify the edit replaces only the ref range.
        let edit = action.edit.as_ref().unwrap();
        let changes = edit.changes.as_ref().unwrap();
        let edits = &changes[&uri];
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "v4.2.0");
        assert_eq!(edits[0].range, make_range(7, 31, 33));
    }

    #[test]
    fn outdated_action_links_diagnostic() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag = make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");

        let actions = code_actions_for_diagnostics(&uri, std::slice::from_ref(&diag));
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

        let actions = code_actions_for_diagnostics(&uri, &[diag]);
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

        let actions = code_actions_for_diagnostics(&uri, &[diag]);
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

        let actions = code_actions_for_diagnostics(&uri, &[diag]);
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

        let actions = code_actions_for_diagnostics(&uri, &[diag]);
        assert!(actions.is_empty());
    }

    #[test]
    fn multiple_diagnostics_produce_multiple_actions() {
        let uri = Uri::from_file_path("/test/workflow.yml").unwrap();
        let diag1 =
            make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");
        let diag2 =
            make_outdated_diagnostic(make_range(8, 31, 33), "actions/setup-node@v3", "v4.0.0");

        let actions = code_actions_for_diagnostics(&uri, &[diag1, diag2]);
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
        };
        let json = serde_json::to_value(&data).unwrap();
        let back: DiagnosticData = serde_json::from_value(json).unwrap();

        match back {
            DiagnosticData::Outdated { target_ref } => assert_eq!(target_ref, "v5.0.0"),
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
}
