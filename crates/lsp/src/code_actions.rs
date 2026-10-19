//! Code action generation for `uses:` reference diagnostics.
//!
//! Produces [`CodeAction`]s from diagnostics, enabling:
//! - Single-ref quick fixes (e.g. "Update to vX.Y.Z") for outdated refs
//! - "Expand to full SHA" quick fixes for short SHA refs
//! - `Pin <ref> to <sha>` quick fixes for branch and non-semver tag refs
//! - Fix-all-in-file source action that applies every edit CLI `update` makes
//!
//! Entry points:
//! - [`code_actions_for_diagnostics`]: per-diagnostic quickfixes
//! - [`fix_all_in_file`]: aggregated source action for every `update` edit

use std::collections::HashMap;

use tower_lsp_server::ls_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Diagnostic, DocumentChanges, OneOf,
    OptionalVersionedTextDocumentIdentifier, Position, Range, TextDocumentEdit, TextEdit, Uri,
    WorkspaceEdit,
};

use actup_core::{
    SourceFormat,
    rewrite::{Named, Rewrite, Trailing},
};

use crate::{
    diagnostics::{DIAGNOSTIC_SOURCE, DiagnosticData, LineEnd, OutdatedEdit},
    position::PositionEncoding,
    workspace_commands::apply_text_edits,
};

/// The open document text that a code action's edits must apply to.
pub struct DocumentText<'a> {
    text: &'a str,
    format: SourceFormat,
    encoding: PositionEncoding,
}

impl<'a> DocumentText<'a> {
    /// Wrap the text of an open document read as `format` and the client's position encoding.
    pub const fn new(text: &'a str, format: SourceFormat, encoding: PositionEncoding) -> Self {
        Self {
            text,
            format,
            encoding,
        }
    }

    /// Whether `edits` apply to the text and the result passes [`SourceFormat::validate_edit`].
    pub fn accepts(&self, edits: &[TextEdit]) -> bool {
        apply_text_edits(self.text, edits, self.encoding)
            .is_some_and(|edited| self.format.validate_edit(self.text, &edited).is_ok())
    }

    /// Keep the actions whose edits the text accepts.
    pub fn retain(&self, actions: Vec<CodeActionOrCommand>) -> Vec<CodeActionOrCommand> {
        actions
            .into_iter()
            .filter(|action| match action {
                CodeActionOrCommand::CodeAction(CodeAction {
                    edit: Some(edit), ..
                }) => self.accepts(&text_edits(edit)),
                CodeActionOrCommand::CodeAction(_) | CodeActionOrCommand::Command(_) => true,
            })
            .collect()
    }

    /// Keep the diagnostics whose own [`update_text_edit`] the text accepts.
    pub fn fixable(&self, diagnostics: &[Diagnostic]) -> Vec<Diagnostic> {
        diagnostics
            .iter()
            .filter(|diag| update_text_edit(diag).is_some_and(|edit| self.accepts(&[edit])))
            .cloned()
            .collect()
    }
}

fn text_edits(edit: &WorkspaceEdit) -> Vec<TextEdit> {
    let changed = edit
        .changes
        .iter()
        .flat_map(|changes| changes.values().flatten().cloned());
    let documents = match &edit.document_changes {
        Some(DocumentChanges::Edits(documents)) => documents.as_slice(),
        Some(DocumentChanges::Operations(_)) | None => &[],
    };
    let versioned = documents.iter().flat_map(|doc| {
        doc.edits.iter().map(|edit| match edit {
            OneOf::Left(edit) => edit.clone(),
            OneOf::Right(annotated) => annotated.text_edit.clone(),
        })
    });
    changed.chain(versioned).collect()
}

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

/// Build the replacement edit for an outdated ref.
///
/// `replace_end_col` (when set) extends the edit past the ref to overwrite a
/// stale `# vX.Y.Z` version comment; `repin_comment` is the refreshed comment
/// appended after a SHA re-pin. Tag updates pass both as `None`/default and just
/// replace the ref range.
pub fn outdated_text_edit(
    diag_range: Range,
    target_ref: &str,
    repin_comment: Option<&str>,
    replace_end_col: Option<u32>,
) -> TextEdit {
    let range = replace_end_col.map_or(diag_range, |end| Range {
        start: diag_range.start,
        end: Position {
            line: diag_range.start.line,
            character: end,
        },
    });
    let new_text = repin_comment.map_or_else(
        || target_ref.to_string(),
        |comment| format!("{target_ref}{comment}"),
    );
    TextEdit { range, new_text }
}

/// Shorten a ref for a code-action title: a full 40-char commit SHA is
/// displayed as its 7-char prefix; tags and other refs are shown verbatim.
fn display_ref(target_ref: &str) -> String {
    if target_ref.len() == 40 && target_ref.bytes().all(|b| b.is_ascii_hexdigit()) {
        target_ref[..7].to_string()
    } else {
        target_ref.to_string()
    }
}

/// Build code actions from a slice of diagnostics.
///
/// For each `Outdated` diagnostic, produces a quickfix "Update to vX.Y.Z"
/// action with a [`WorkspaceEdit`] that replaces only the ref portion.
///
/// For each `PinBranch` diagnostic, produces a quickfix `Pin <ref> to <sha>`.
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

        let Some((title, edit, preferred)) = quick_fix(diag.range, data) else {
            continue;
        };
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title,
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diag.clone()]),
            edit: Some(make_edit(
                uri,
                version,
                vec![edit],
                supports_document_changes,
            )),
            is_preferred: Some(preferred),
            ..Default::default()
        }));
    }

    actions
}

fn quick_fix(range: Range, data: DiagnosticData) -> Option<(String, TextEdit, bool)> {
    match data {
        DiagnosticData::Outdated {
            target_ref,
            repin_comment,
            replace_end_col,
            ..
        } => {
            let edit = outdated_text_edit(
                range,
                &target_ref,
                repin_comment.as_deref(),
                replace_end_col,
            );
            Some((
                format!("Update to {}", display_ref(&target_ref)),
                edit,
                true,
            ))
        }
        DiagnosticData::ExpandSha {
            target_ref,
            repin_comment,
            replace_end_col,
        } => {
            let edit = outdated_text_edit(
                range,
                &target_ref,
                repin_comment.as_deref(),
                replace_end_col,
            );
            Some(("Expand to full SHA".to_string(), edit, true))
        }
        DiagnosticData::PinBranch {
            sha,
            ref_name,
            repin_comment,
            replace_end_col,
        } => {
            let title = format!("Pin {ref_name} to {}", display_ref(&sha));
            let edit = outdated_text_edit(range, &sha, repin_comment.as_deref(), replace_end_col);
            Some((title, edit, true))
        }
        DiagnosticData::Unresolvable {
            suggested_ref: Some(suggested),
            repin_comment,
            replace_end_col,
        } => {
            let edit =
                outdated_text_edit(range, &suggested, repin_comment.as_deref(), replace_end_col);
            Some((
                format!("Change to {}", display_ref(&suggested)),
                edit,
                false,
            ))
        }
        DiagnosticData::Unresolvable {
            suggested_ref: None,
            ..
        } => None,
    }
}

/// Offer an explicit SHA update for outdated refs intersecting the requested range.
///
/// The target follows the update policy already applied by diagnostics. When
/// an already-offered quickfix makes the identical edit, omit the alternative.
pub fn update_and_pin_actions(
    uri: &Uri,
    version: Option<i32>,
    diagnostics: &[Diagnostic],
    requested: Range,
    supports_document_changes: bool,
    quickfixes: &[CodeActionOrCommand],
) -> Vec<CodeActionOrCommand> {
    let point = |p: Position| (p.line, p.character);
    diagnostics
        .iter()
        .filter_map(|diag| {
            if diag.source.as_deref() != Some(DIAGNOSTIC_SOURCE)
                || point(diag.range.start) > point(requested.end)
                || point(diag.range.end) < point(requested.start)
            {
                return None;
            }
            let DiagnosticData::Outdated { pin: Some(pin), .. } =
                serde_json::from_value(diag.data.clone()?).ok()?
            else {
                return None;
            };
            let edit = outdated_text_edit(
                diag.range,
                pin.sha.as_str(),
                pin.repin_comment.as_deref(),
                pin.replace_end_col,
            );
            if quickfixes.iter().any(|action| match action {
                CodeActionOrCommand::CodeAction(CodeAction {
                    kind: Some(kind),
                    edit: Some(existing),
                    ..
                }) if *kind == CodeActionKind::QUICKFIX => {
                    text_edits(existing).as_slice() == std::slice::from_ref(&edit)
                }
                _ => false,
            }) {
                return None;
            }
            Some(pin_action(
                uri,
                version,
                supports_document_changes,
                format!("Update to {} & pin to SHA", pin.label),
                edit,
            ))
        })
        .collect()
}

/// Resolved targets for the "Pin to …" and unpin code actions. Each is `None`
/// when it's unavailable or would be a no-op for the current ref.
pub struct PinTargets {
    /// `(full commit SHA, version label)` for the version-commented SHA pin.
    pub sha: Option<(actup_core::provider::Sha40, String)>,
    /// Whether SHA pins include a generated label comment.
    pub comment: bool,
    /// Major tag to float on, e.g. `v7`.
    pub major: Option<String>,
    /// Default branch to track, e.g. `main`.
    pub branch: Option<String>,
    /// Unpin to the tag a SHA pin's `# <tag>` marker names, when that tag is on the pinned commit.
    pub unpin: Option<Rewrite>,
    /// Whether the ref under the cursor is a SHA pin.
    pub current_is_sha: bool,
}

/// Build "Pin to …" and unpin refactor actions for the ref under the cursor.
///
/// `ref_range` is the ref portion's range; `trailing` and `end` describe the
/// text after it. The SHA pin and the major tag write their own label over a
/// label comment as [`Rewrite::pin`] and [`Rewrite::named`] do, the branch
/// keeps only the note, and a quoted value keeps its closing quote before the
/// comment. The unpin action replaces the marker comment with its note. None
/// of these is a `source.fixAll` action.
pub fn pin_code_actions(
    uri: &Uri,
    version: Option<i32>,
    supports_document_changes: bool,
    ref_range: Range,
    trailing: Trailing<'_>,
    end: LineEnd<'_>,
    targets: &PinTargets,
) -> Vec<CodeActionOrCommand> {
    let floating =
        |new_ref: &str, kind| Rewrite::named(new_ref, kind, targets.current_is_sha, trailing);
    let placed = |rewrite: Rewrite| {
        let e = OutdatedEdit::new(rewrite, end);
        outdated_text_edit(
            ref_range,
            &e.target_ref,
            e.repin_comment.as_deref(),
            e.replace_end_col,
        )
    };
    let mut actions = Vec::new();

    if let Some((sha, label)) = &targets.sha {
        actions.push(pin_action(
            uri,
            version,
            supports_document_changes,
            format!("Pin to commit SHA ({label})"),
            placed(Rewrite::pin(sha.as_str(), label, trailing, targets.comment)),
        ));
    }
    if let Some(major) = &targets.major {
        actions.push(pin_action(
            uri,
            version,
            supports_document_changes,
            format!("Pin to major tag {major}"),
            placed(floating(major, Named::Tag)),
        ));
    }
    if let Some(branch) = &targets.branch {
        actions.push(pin_action(
            uri,
            version,
            supports_document_changes,
            format!("Pin to branch {branch}"),
            placed(floating(branch, Named::Branch)),
        ));
    }
    if let Some(unpin) = &targets.unpin {
        actions.push(pin_action(
            uri,
            version,
            supports_document_changes,
            format!("actup: unpin to `{}`", unpin.new_ref),
            placed(unpin.clone()),
        ));
    }
    actions
}

fn pin_action(
    uri: &Uri,
    version: Option<i32>,
    supports_document_changes: bool,
    title: String,
    edit: TextEdit,
) -> CodeActionOrCommand {
    CodeActionOrCommand::CodeAction(CodeAction {
        title,
        kind: Some(CodeActionKind::REFACTOR),
        edit: Some(make_edit(
            uri,
            version,
            vec![edit],
            supports_document_changes,
        )),
        ..Default::default()
    })
}

/// The edit CLI `update` makes for an actup diagnostic: an outdated ref or a ref to pin.
pub fn update_text_edit(diag: &Diagnostic) -> Option<TextEdit> {
    if diag.source.as_deref() != Some(DIAGNOSTIC_SOURCE) {
        return None;
    }
    let data = serde_json::from_value::<DiagnosticData>(diag.data.clone()?).ok()?;
    match data {
        DiagnosticData::Outdated {
            target_ref: new_ref,
            repin_comment,
            replace_end_col,
            ..
        }
        | DiagnosticData::PinBranch {
            sha: new_ref,
            repin_comment,
            replace_end_col,
            ..
        } => Some(outdated_text_edit(
            diag.range,
            &new_ref,
            repin_comment.as_deref(),
            replace_end_col,
        )),
        DiagnosticData::ExpandSha { .. } | DiagnosticData::Unresolvable { .. } => None,
    }
}

/// Build a single "Fix all outdated actions" source action for the file.
///
/// Aggregates the [`update_text_edit`] of every diagnostic into one
/// [`WorkspaceEdit`] with multiple [`TextEdit`]s. Returns `None` if there are fewer than 2
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
        if let Some(edit) = update_text_edit(diag) {
            edits.push(edit);
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
mod tests {
    use super::*;
    use crate::test_utils;

    /// Document version used across all code-action tests.
    const TEST_VERSION: Option<i32> = Some(5);

    fn make_range(line: u32, col_start: u32, col_end: u32) -> tower_lsp_server::ls_types::Range {
        test_utils::make_range(line, col_start, col_end)
    }

    const fn unquoted(rewrite_end_col: u32) -> LineEnd<'static> {
        LineEnd {
            closer: "",
            rewrite_end_col,
        }
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
        let Some(DocumentChanges::Edits(doc_edits)) = &edit.document_changes else {
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
    fn outdated_diagnostic_produces_quickfix() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag = make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        assert_eq!(actions.len(), 1);

        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };
        assert_eq!(action.title, "Update to v4.2.0");
        assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
        assert_eq!(action.is_preferred, Some(true));

        let edits = extract_text_edits(
            action.edit.as_ref().ok_or("action has no edit")?,
            &uri,
            TEST_VERSION,
        );
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "v4.2.0");
        assert_eq!(edits[0].range, make_range(7, 31, 33));
        Ok(())
    }

    async fn update_pin_diagnostics(
        text: &str,
        style: actup_core::policy::Style,
        comment: bool,
        target_sha: &str,
    ) -> Vec<Diagnostic> {
        use actup_core::{Config, policy::Update, version};
        let old = test_utils::make_version_info("v4.0.0", test_utils::DUMMY_SHA_2);
        let patch = test_utils::make_version_info("v4.0.1", target_sha);
        let latest = test_utils::make_version_info("v5.0.0", &"c".repeat(40));
        let versions = version::Set::build(
            vec![old, patch, std::sync::Arc::clone(&latest)],
            HashMap::new(),
            HashMap::new(),
            Some(latest),
        );
        let mut config = Config::default();
        config.policy.bump = Update::Patch;
        config.policy.pin = style;
        config.pin.comment = comment;
        let registry = test_utils::mock_registry(test_utils::MockProvider::new("github.com"));
        let versions = HashMap::from([(
            actup_core::ActionRepoId::new(None, "actions", "checkout", None),
            Ok(versions),
        )]);
        crate::diagnostics::compute_diagnostics_with_versions(
            text,
            SourceFormat::Yaml,
            &config,
            &registry,
            &versions,
            PositionEncoding::Utf16,
        )
        .await
        .diagnostics
    }

    #[tokio::test]
    async fn update_and_pin_uses_policy_target_and_optional_comments()
    -> Result<(), Box<dyn std::error::Error>> {
        let uri: Uri = "file:///test/ci.yml".parse()?;
        let text = "- uses: \"actions/checkout@v4.0.0\" # v4.0.0 keep\n";
        for style in [
            actup_core::policy::Style::Preserve,
            actup_core::policy::Style::Tag,
            actup_core::policy::Style::Sha,
        ] {
            for comment in [true, false] {
                let diags =
                    update_pin_diagnostics(text, style, comment, test_utils::DUMMY_SHA).await;
                let diag = diags.first().ok_or("no outdated diagnostic")?;
                let actions =
                    update_and_pin_actions(&uri, TEST_VERSION, &diags, diag.range, true, &[]);
                let [CodeActionOrCommand::CodeAction(action)] = actions.as_slice() else {
                    return Err(format!("expected update-and-pin: {actions:?}").into());
                };
                assert_eq!(action.title, "Update to v4.0.1 & pin to SHA");
                assert_eq!(action.kind, Some(CodeActionKind::REFACTOR));
                let edits =
                    extract_text_edits(action.edit.as_ref().ok_or("no edit")?, &uri, TEST_VERSION);
                let written = apply_text_edits(text, &[edits[0].clone()], PositionEncoding::Utf16)
                    .ok_or("invalid edit")?;
                let label = if comment { "v4.0.1 " } else { "" };
                assert_eq!(
                    written,
                    format!(
                        "- uses: \"actions/checkout@{}\" # {label}keep\n",
                        test_utils::DUMMY_SHA
                    )
                );
                let quickfixes = code_actions_for_diagnostics(&uri, TEST_VERSION, &diags, true);
                let all_kinds = update_and_pin_actions(
                    &uri,
                    TEST_VERSION,
                    &diags,
                    diag.range,
                    true,
                    &quickfixes,
                );
                assert_eq!(
                    all_kinds.len(),
                    usize::from(style != actup_core::policy::Style::Sha)
                );
                assert_eq!(
                    update_and_pin_actions(
                        &uri,
                        TEST_VERSION,
                        &diags,
                        make_range(1, 0, 0),
                        true,
                        &[]
                    ),
                    []
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn update_and_pin_rejects_invalid_sha_and_uneditable_values()
    -> Result<(), Box<dyn std::error::Error>> {
        let uri: Uri = "file:///test/ci.yml".parse()?;
        for (text, sha) in [
            ("- uses: actions/checkout@v4.0.0\n", "bad-sha"),
            (
                "- uses: \"actions/checkout@v4\\x2e0.0\"\n",
                test_utils::DUMMY_SHA,
            ),
        ] {
            let diags =
                update_pin_diagnostics(text, actup_core::policy::Style::Tag, true, sha).await;
            assert_ne!(diags, []);
            assert_eq!(
                update_and_pin_actions(&uri, TEST_VERSION, &diags, make_range(0, 0, 80), true, &[]),
                []
            );
        }
        let mut diags = update_pin_diagnostics(
            "- uses: actions/checkout@v4.0.0\n",
            actup_core::policy::Style::Tag,
            true,
            test_utils::DUMMY_SHA,
        )
        .await;
        let diag = diags.first_mut().ok_or("no diagnostic")?;
        diag.data.as_mut().ok_or("no metadata")?["pin"]["sha"] = serde_json::json!("x".repeat(40));
        assert_eq!(
            update_and_pin_actions(&uri, TEST_VERSION, &diags, make_range(0, 0, 80), true, &[]),
            []
        );
        Ok(())
    }

    #[test]
    fn outdated_action_links_diagnostic() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag = make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");

        let actions =
            code_actions_for_diagnostics(&uri, TEST_VERSION, std::slice::from_ref(&diag), true);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };

        // Code action must link back to the triggering diagnostic.
        let linked = action
            .diagnostics
            .as_ref()
            .ok_or("action links no diagnostics")?;
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].range, diag.range);
        assert_eq!(linked[0].message, diag.message);
        Ok(())
    }

    #[test]
    fn sha_target_shortens_title_keeps_full_sha_edit() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let new_sha = "2".repeat(40);
        let old_sha = "1".repeat(40);
        let diag = make_outdated_diagnostic(
            make_range(7, 31, 71),
            &format!("actions/checkout@{old_sha}"),
            &new_sha,
        );

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };
        // Title shows a short SHA, not 40 characters.
        assert_eq!(action.title, format!("Update to {}", &new_sha[..7]));
        // The edit still writes the full SHA.
        let edits = extract_text_edits(
            action.edit.as_ref().ok_or("action has no edit")?,
            &uri,
            TEST_VERSION,
        );
        assert_eq!(edits[0].new_text, new_sha);
        Ok(())
    }

    #[test]
    fn expand_sha_diagnostic_produces_quickfix() -> Result<(), Box<dyn std::error::Error>> {
        let mut diag = make_unresolvable_diagnostic(make_range(6, 31, 38), None);
        diag.data = serde_json::to_value(DiagnosticData::ExpandSha {
            target_ref: test_utils::DUMMY_SHA.to_string(),
            repin_comment: Some(" # v4.2.0".to_string()),
            replace_end_col: Some(48),
        })
        .ok();
        let uri = "file:///test/ci.yml".parse::<Uri>()?;

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        let [CodeActionOrCommand::CodeAction(action)] = actions.as_slice() else {
            return Err(format!("expected one code action, got {actions:?}").into());
        };
        assert_eq!(action.title, "Expand to full SHA");
        assert_eq!(action.kind, Some(CodeActionKind::QUICKFIX));
        assert_eq!(action.is_preferred, Some(true));
        let edit = action.edit.as_ref().ok_or("expected a workspace edit")?;
        let edits = extract_text_edits(edit, &uri, TEST_VERSION);
        assert_eq!(
            edits,
            [&TextEdit {
                range: make_range(6, 31, 48),
                new_text: format!("{} # v4.2.0", test_utils::DUMMY_SHA),
            }]
        );
        Ok(())
    }

    #[test]
    fn unresolvable_with_suggestion_produces_quickfix() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag = make_unresolvable_diagnostic(make_range(3, 10, 15), Some("v2.1.0"));

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        assert_eq!(actions.len(), 1);

        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };
        assert_eq!(action.title, "Change to v2.1.0");
        assert_eq!(action.is_preferred, Some(false));
        Ok(())
    }

    #[test]
    fn unresolvable_without_suggestion_produces_no_action() -> Result<(), Box<dyn std::error::Error>>
    {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag = make_unresolvable_diagnostic(make_range(3, 10, 15), None);

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        assert_eq!(actions, []);
        Ok(())
    }

    #[test]
    fn foreign_diagnostic_source_ignored() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag = Diagnostic {
            range: make_range(5, 0, 10),
            source: Some("other-tool".into()),
            ..Default::default()
        };

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        assert_eq!(actions, []);
        Ok(())
    }

    #[test]
    fn diagnostic_without_data_ignored() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag = Diagnostic {
            range: make_range(5, 0, 10),
            source: Some(DIAGNOSTIC_SOURCE.into()),
            data: None,
            ..Default::default()
        };

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        assert_eq!(actions, []);
        Ok(())
    }

    #[test]
    fn multiple_diagnostics_produce_multiple_actions() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
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
        Ok(())
    }

    #[test]
    fn outdated_text_edit_refreshes_stale_comment() {
        // Re-pin replaces SHA *and* the stale comment in one edit, extending to
        // the supplied end column.
        let new_sha = "fac544c07dec837d0ccb6301d7b5580bf5edae39";
        let edit = outdated_text_edit(make_range(0, 31, 71), new_sha, Some(" # v8.2.0"), Some(86));
        assert_eq!(edit.new_text, format!("{new_sha} # v8.2.0"));
        assert_eq!(edit.range.start.character, 31);
        assert_eq!(edit.range.end.character, 86); // extends over the old comment
    }

    #[test]
    fn outdated_text_edit_appends_comment_no_extend() {
        let new_sha = "fac544c07dec837d0ccb6301d7b5580bf5edae39";
        let edit = outdated_text_edit(make_range(0, 31, 71), new_sha, Some(" # v8.2.0"), None);
        assert_eq!(edit.new_text, format!("{new_sha} # v8.2.0"));
        assert_eq!(edit.range.end.character, 71); // only the ref range
    }

    #[test]
    fn outdated_text_edit_tag_is_plain_replacement() {
        let edit = outdated_text_edit(make_range(0, 31, 33), "v4.2.0", None, None);
        assert_eq!(edit.new_text, "v4.2.0");
        assert_eq!(edit.range, make_range(0, 31, 33));
    }

    #[test]
    fn pin_actions_offer_sha_major_branch() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let sha = "9c091bb21b7c1c1d1991bb908d89e4e9dddfe3e0";
        let targets = PinTargets {
            comment: true,
            sha: Some((
                actup_core::provider::Sha40::new(sha).ok_or("invalid test SHA")?,
                "v7.2.3".to_string(),
            )),
            major: Some("v7".to_string()),
            branch: Some("main".to_string()),
            unpin: None,
            current_is_sha: false,
        };
        let actions = pin_code_actions(
            &uri,
            TEST_VERSION,
            true,
            make_range(0, 31, 37),
            Trailing::None,
            unquoted(40),
            &targets,
        );
        let titles: Vec<&str> = actions
            .iter()
            .map(|a| match a {
                CodeActionOrCommand::CodeAction(c) => c.title.as_str(),
                CodeActionOrCommand::Command(_) => panic!("expected CodeAction"),
            })
            .collect();
        assert_eq!(
            titles,
            [
                "Pin to commit SHA (v7.2.3)",
                "Pin to major tag v7",
                "Pin to branch main"
            ]
        );

        // The SHA pin writes the full SHA plus a version comment.
        let CodeActionOrCommand::CodeAction(sha_action) = &actions[0] else {
            panic!("expected CodeAction");
        };
        let edits = extract_text_edits(
            sha_action.edit.as_ref().ok_or("action has no edit")?,
            &uri,
            TEST_VERSION,
        );
        assert_eq!(edits[0].new_text, format!("{sha} # v7.2.3"));
        assert_eq!(sha_action.kind, Some(CodeActionKind::REFACTOR));
        Ok(())
    }

    #[test]
    fn pin_to_major_and_branch_rewrite_the_label() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        for (current_is_sha, trailing, major, branch) in [
            (true, Trailing::Label { note: "" }, "v7", "main"),
            (
                true,
                Trailing::Label { note: "keep" },
                "v7 # keep",
                "main # keep",
            ),
            (
                false,
                Trailing::Label { note: "keep" },
                "v7 # v7 keep",
                "main # keep",
            ),
            (false, Trailing::Label { note: "" }, "v7 # v7", "main"),
        ] {
            let targets = PinTargets {
                comment: true,
                sha: None,
                major: Some("v7".to_string()),
                branch: Some("main".to_string()),
                unpin: None,
                current_is_sha,
            };
            let actions = pin_code_actions(
                &uri,
                None,
                false,
                make_range(0, 31, 71),
                trailing,
                unquoted(86),
                &targets,
            );
            let texts: Vec<(String, u32)> = actions
                .iter()
                .flat_map(|action| edits_of(action, &uri))
                .map(|edit| (edit.new_text, edit.range.end.character))
                .collect();
            assert_eq!(
                texts,
                [(major.to_string(), 86), (branch.to_string(), 86)],
                "{current_is_sha} {trailing:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn floating_pins_keep_the_note_of_a_version_comment() -> Result<(), Box<dyn std::error::Error>>
    {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let sha = "9c091bb21b7c1c1d1991bb908d89e4e9dddfe3e0";
        let targets = PinTargets {
            comment: true,
            sha: Some((
                actup_core::provider::Sha40::new(sha).ok_or("invalid test SHA")?,
                "v7.2.3".to_string(),
            )),
            major: Some("v7".to_string()),
            branch: Some("main".to_string()),
            unpin: None,
            current_is_sha: true,
        };
        let actions = pin_code_actions(
            &uri,
            TEST_VERSION,
            true,
            make_range(0, 31, 71),
            Trailing::Label {
                note: "pinned for node16",
            },
            unquoted(104),
            &targets,
        );
        let written: Vec<(String, u32)> = actions
            .iter()
            .map(|action| {
                let edits = edits_of_versioned(action, &uri);
                let [edit] = edits.as_slice() else {
                    panic!("expected one edit: {edits:?}");
                };
                (edit.new_text.clone(), edit.range.end.character)
            })
            .collect();
        assert_eq!(
            written,
            [
                (format!("{sha} # v7.2.3 pinned for node16"), 104),
                ("v7 # pinned for node16".to_string(), 104),
                ("main # pinned for node16".to_string(), 104),
            ]
        );
        Ok(())
    }

    fn edits_of_versioned(action: &CodeActionOrCommand, uri: &Uri) -> Vec<TextEdit> {
        let CodeActionOrCommand::CodeAction(CodeAction {
            edit: Some(edit), ..
        }) = action
        else {
            panic!("expected a CodeAction with an edit");
        };
        extract_text_edits(edit, uri, TEST_VERSION)
            .into_iter()
            .cloned()
            .collect()
    }

    #[test]
    fn unpin_action_replaces_the_marker_and_stays_out_of_fix_all()
    -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let targets = PinTargets {
            comment: true,
            sha: None,
            major: None,
            branch: None,
            unpin: Some(Rewrite::unpin(actup_core::rewrite::PinMarker {
                tag: "v4",
                note: "keep",
            })),
            current_is_sha: true,
        };
        let actions = pin_code_actions(
            &uri,
            TEST_VERSION,
            true,
            make_range(0, 31, 71),
            Trailing::Foreign,
            unquoted(86),
            &targets,
        );
        let [CodeActionOrCommand::CodeAction(action)] = actions.as_slice() else {
            return Err(format!("expected one action, got {actions:?}").into());
        };
        assert_eq!(action.title, "actup: unpin to `v4`");
        let kind = action.kind.clone().ok_or("no kind")?;
        assert_eq!(kind, CodeActionKind::REFACTOR);
        assert!(!kind_allowed(
            Some(&[CodeActionKind::SOURCE_FIX_ALL]),
            &kind
        ));
        let edits = extract_text_edits(action.edit.as_ref().ok_or("no edit")?, &uri, TEST_VERSION);
        assert_eq!(edits[0].new_text, "v4 # keep");
        assert_eq!(edits[0].range, make_range(0, 31, 86));
        Ok(())
    }

    #[test]
    fn pin_actions_empty_when_no_targets() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let targets = PinTargets {
            comment: true,
            sha: None,
            major: None,
            branch: None,
            unpin: None,
            current_is_sha: false,
        };
        let actions = pin_code_actions(
            &uri,
            TEST_VERSION,
            true,
            make_range(0, 31, 37),
            Trailing::None,
            unquoted(40),
            &targets,
        );
        assert_eq!(actions, []);
        Ok(())
    }

    #[test]
    fn diagnostic_data_roundtrips_through_json() -> Result<(), Box<dyn std::error::Error>> {
        let data = DiagnosticData::Outdated {
            pin: None,
            target_ref: "v5.0.0".into(),
            repin_comment: None,
            replace_end_col: None,
        };
        let json = serde_json::to_value(&data)?;
        let back: DiagnosticData = serde_json::from_value(json)?;

        match back {
            DiagnosticData::Outdated { target_ref, .. } => assert_eq!(target_ref, "v5.0.0"),
            DiagnosticData::Unresolvable { .. }
            | DiagnosticData::ExpandSha { .. }
            | DiagnosticData::PinBranch { .. } => {
                panic!("wrong variant")
            }
        }
        Ok(())
    }

    #[test]
    fn unresolvable_data_roundtrips_through_json() -> Result<(), Box<dyn std::error::Error>> {
        let data = DiagnosticData::Unresolvable {
            suggested_ref: Some("v3.1.0".into()),
            repin_comment: None,
            replace_end_col: None,
        };
        let json = serde_json::to_value(&data)?;
        let back: DiagnosticData = serde_json::from_value(json)?;

        match back {
            DiagnosticData::Unresolvable { suggested_ref, .. } => {
                assert_eq!(suggested_ref.as_deref(), Some("v3.1.0"));
            }
            DiagnosticData::Outdated { .. }
            | DiagnosticData::ExpandSha { .. }
            | DiagnosticData::PinBranch { .. } => {
                panic!("wrong variant")
            }
        }
        Ok(())
    }

    #[test]
    fn fix_all_returns_none_for_single_outdated() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag = make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");

        // Single outdated ref — quickfix already covers this, no fix-all needed.
        assert!(fix_all_in_file(&uri, TEST_VERSION, &[diag], true).is_none());
        Ok(())
    }

    #[test]
    fn fix_all_returns_none_for_empty_diagnostics() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        assert!(fix_all_in_file(&uri, TEST_VERSION, &[], true).is_none());
        Ok(())
    }

    #[test]
    fn fix_all_returns_none_for_non_outdated_diagnostics() -> Result<(), Box<dyn std::error::Error>>
    {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag1 = make_unresolvable_diagnostic(make_range(3, 10, 15), Some("v2.1.0"));
        let diag2 = make_unresolvable_diagnostic(make_range(5, 10, 15), None);

        // Only unresolvable diagnostics — fix-all only handles Outdated.
        assert!(fix_all_in_file(&uri, TEST_VERSION, &[diag1, diag2], true).is_none());
        Ok(())
    }

    #[test]
    fn fix_all_aggregates_multiple_outdated() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag1 =
            make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");
        let diag2 =
            make_outdated_diagnostic(make_range(8, 34, 36), "actions/setup-node@v3", "v4.0.0");
        let diag3 = make_outdated_diagnostic(make_range(12, 20, 22), "actions/cache@v2", "v4.1.0");

        let action = fix_all_in_file(&uri, TEST_VERSION, &[diag1, diag2, diag3], true)
            .ok_or("no fix-all action")?;
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected CodeAction");
        };

        assert_eq!(action.title, "Fix all outdated actions (3 refs)");
        assert_eq!(action.kind, Some(SOURCE_FIX_ALL_ACTUP));
        assert_eq!(action.is_preferred, Some(false));

        let edits = extract_text_edits(
            action.edit.as_ref().ok_or("action has no edit")?,
            &uri,
            TEST_VERSION,
        );
        assert_eq!(edits.len(), 3);
        assert_eq!(edits[0].new_text, "v4.2.0");
        assert_eq!(edits[1].new_text, "v4.0.0");
        assert_eq!(edits[2].new_text, "v4.1.0");

        // All 3 diagnostics linked.
        let linked = action
            .diagnostics
            .as_ref()
            .ok_or("action links no diagnostics")?;
        assert_eq!(linked.len(), 3);
        Ok(())
    }

    #[test]
    fn fix_all_skips_foreign_and_non_outdated() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
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
        .ok_or("no fix-all action")?;
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected CodeAction");
        };

        // Only the 2 outdated refs are in the edit.
        let edits = extract_text_edits(
            action.edit.as_ref().ok_or("action has no edit")?,
            &uri,
            TEST_VERSION,
        );
        assert_eq!(edits.len(), 2);
        assert_eq!(
            action
                .diagnostics
                .as_ref()
                .ok_or("action links no diagnostics")?
                .len(),
            2
        );
        Ok(())
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
    fn edit_replaces_only_ref_range() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        // Range covers just "v3" (2 chars), not the entire `uses:` line.
        let diag = make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };

        let edits = extract_text_edits(
            action.edit.as_ref().ok_or("action has no edit")?,
            &uri,
            TEST_VERSION,
        );
        assert_eq!(edits.len(), 1);

        // Only the ref portion is replaced, not the whole line.
        assert_eq!(edits[0].range.start.line, 7);
        assert_eq!(edits[0].range.start.character, 31);
        assert_eq!(edits[0].range.end.character, 33);
        assert_eq!(edits[0].new_text, "v4.2.0");
        Ok(())
    }

    #[test]
    fn edit_handles_longer_replacement() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        // Replacing "v3" (2 chars) with "v4.2.0" (6 chars).
        let diag = make_outdated_diagnostic(make_range(0, 10, 12), "o/r@v3", "v4.2.0");

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };

        let edits = extract_text_edits(
            action.edit.as_ref().ok_or("action has no edit")?,
            &uri,
            TEST_VERSION,
        );
        // The range stays the same (2 chars), but the new_text is longer.
        // LSP handles the expansion — we just provide the replacement text.
        assert_eq!(edits[0].range, make_range(0, 10, 12));
        assert_eq!(edits[0].new_text, "v4.2.0");
        Ok(())
    }

    #[test]
    fn fix_all_edits_maintain_per_line_ranges() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag1 =
            make_outdated_diagnostic(make_range(7, 31, 33), "actions/checkout@v3", "v4.2.0");
        let diag2 =
            make_outdated_diagnostic(make_range(8, 34, 36), "actions/setup-node@v3", "v4.0.0");
        let diag3 = make_outdated_diagnostic(make_range(12, 20, 22), "actions/cache@v2", "v4.1.0");

        let action = fix_all_in_file(&uri, TEST_VERSION, &[diag1, diag2, diag3], true)
            .ok_or("no fix-all action")?;
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected CodeAction");
        };

        let edits = extract_text_edits(
            action.edit.as_ref().ok_or("action has no edit")?,
            &uri,
            TEST_VERSION,
        );

        // Each edit preserves its specific range — no overlap or combined ranges.
        assert_eq!(edits[0].range, make_range(7, 31, 33));
        assert_eq!(edits[1].range, make_range(8, 34, 36));
        assert_eq!(edits[2].range, make_range(12, 20, 22));
        Ok(())
    }

    #[test]
    fn fix_all_title_includes_ref_count() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag1 = make_outdated_diagnostic(make_range(0, 10, 12), "a/b@v1", "v2.0.0");
        let diag2 = make_outdated_diagnostic(make_range(1, 10, 12), "a/c@v1", "v2.0.0");

        let action = fix_all_in_file(&uri, TEST_VERSION, &[diag1, diag2], true)
            .ok_or("no fix-all action")?;
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected CodeAction");
        };
        assert_eq!(action.title, "Fix all outdated actions (2 refs)");
        Ok(())
    }

    #[test]
    fn code_action_edit_targets_correct_uri() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///project/.github/workflows/ci.yml".parse::<Uri>()?;
        let diag = make_outdated_diagnostic(make_range(0, 0, 2), "o/r@v1", "v2.0.0");

        let actions = code_actions_for_diagnostics(&uri, TEST_VERSION, &[diag], true);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };

        // extract_text_edits already asserts URI matches.
        let edits = extract_text_edits(
            action.edit.as_ref().ok_or("action has no edit")?,
            &uri,
            TEST_VERSION,
        );
        assert_eq!(edits.len(), 1);
        Ok(())
    }

    #[test]
    fn mixed_diagnostics_produce_correct_action_types() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
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
        Ok(())
    }

    #[test]
    fn kind_allowed_with_multiple_kinds() {
        let only = vec![CodeActionKind::QUICKFIX, CodeActionKind::SOURCE_FIX_ALL];
        assert!(kind_allowed(Some(&only), &SOURCE_FIX_ALL_ACTUP));
        assert!(kind_allowed(Some(&only), &CodeActionKind::QUICKFIX));
    }

    #[test]
    fn fix_all_preserves_diagnostic_order() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag1 = make_outdated_diagnostic(make_range(12, 20, 22), "a/c@v2", "v4.1.0");
        let diag2 = make_outdated_diagnostic(make_range(7, 31, 33), "a/b@v3", "v4.2.0");

        let action = fix_all_in_file(&uri, TEST_VERSION, &[diag1, diag2], true)
            .ok_or("no fix-all action")?;
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected CodeAction");
        };

        let edits = extract_text_edits(
            action.edit.as_ref().ok_or("action has no edit")?,
            &uri,
            TEST_VERSION,
        );
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].range.start.line, 12);
        assert_eq!(edits[1].range.start.line, 7);
        Ok(())
    }

    #[test]
    fn diagnostic_data_json_has_kind_field() -> Result<(), Box<dyn std::error::Error>> {
        let data = DiagnosticData::Outdated {
            pin: None,
            target_ref: "v2.0.0".into(),
            repin_comment: None,
            replace_end_col: None,
        };
        let json = serde_json::to_value(&data)?;
        assert_eq!(
            json.get("kind").ok_or("no kind field")?.as_str(),
            Some("outdated")
        );
        Ok(())
    }

    #[test]
    fn versioned_edit_does_not_use_unversioned_changes() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag = make_outdated_diagnostic(make_range(0, 10, 12), "o/r@v1", "v2.0.0");

        let actions = code_actions_for_diagnostics(&uri, Some(42), &[diag], true);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };
        let edit = action.edit.as_ref().ok_or("action has no edit")?;

        // Must use document_changes, never unversioned changes.
        assert!(edit.changes.is_none());
        assert!(edit.document_changes.is_some());

        // Version must be exactly what was passed.
        let DocumentChanges::Edits(doc_edits) = edit
            .document_changes
            .as_ref()
            .ok_or("edit has no document changes")?
        else {
            panic!("expected Edits");
        };
        assert_eq!(doc_edits[0].text_document.version, Some(42));
        Ok(())
    }

    #[test]
    fn fallback_to_unversioned_changes_when_not_supported() -> Result<(), Box<dyn std::error::Error>>
    {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag = make_outdated_diagnostic(make_range(0, 10, 12), "o/r@v1", "v2.0.0");

        let actions = code_actions_for_diagnostics(&uri, Some(5), &[diag], false);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };
        let edit = action.edit.as_ref().ok_or("action has no edit")?;

        // Must use unversioned changes, not document_changes.
        assert!(edit.document_changes.is_none());
        let changes = edit.changes.as_ref().ok_or("expected changes map")?;
        let file_edits = changes.get(&uri).ok_or("expected edits for URI")?;
        assert_eq!(file_edits.len(), 1);
        assert_eq!(file_edits[0].new_text, "v2.0.0");
        Ok(())
    }

    #[test]
    fn fix_all_fallback_to_unversioned_changes() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag1 = make_outdated_diagnostic(make_range(0, 10, 12), "a/b@v1", "v2.0.0");
        let diag2 = make_outdated_diagnostic(make_range(1, 10, 12), "a/c@v1", "v3.0.0");

        let action =
            fix_all_in_file(&uri, Some(5), &[diag1, diag2], false).ok_or("no fix-all action")?;
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected CodeAction");
        };
        let edit = action.edit.as_ref().ok_or("action has no edit")?;

        assert!(edit.document_changes.is_none());
        let changes = edit.changes.as_ref().ok_or("expected changes map")?;
        let file_edits = changes.get(&uri).ok_or("expected edits for URI")?;
        assert_eq!(file_edits.len(), 2);
        Ok(())
    }

    #[test]
    fn none_version_for_untracked_document() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let diag = make_outdated_diagnostic(make_range(0, 10, 12), "o/r@v1", "v2.0.0");

        let actions = code_actions_for_diagnostics(&uri, None, &[diag], true);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("expected CodeAction");
        };
        let edit = action.edit.as_ref().ok_or("action has no edit")?;
        let DocumentChanges::Edits(doc_edits) = edit
            .document_changes
            .as_ref()
            .ok_or("edit has no document changes")?
        else {
            panic!("expected Edits");
        };
        // Untracked document should have version None (null).
        assert_eq!(doc_edits[0].text_document.version, None);
        Ok(())
    }

    const VALIDATED: &str = "steps:\n  - uses: a/b@v1\n  - uses: a/c@v1\n";

    fn titles(actions: &[CodeActionOrCommand]) -> Vec<&str> {
        actions
            .iter()
            .map(|action| match action {
                CodeActionOrCommand::CodeAction(action) => action.title.as_str(),
                CodeActionOrCommand::Command(command) => command.title.as_str(),
            })
            .collect()
    }

    #[test]
    fn document_text_accepts_only_edits_that_keep_the_yaml_valid() {
        let document = DocumentText::new(
            VALIDATED,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        );
        let edit = |line, new_text: &str| TextEdit {
            range: make_range(line, 14, 16),
            new_text: new_text.to_string(),
        };

        assert!(document.accepts(&[edit(1, "v2"), edit(2, "v3")]));
        assert!(!document.accepts(&[edit(1, "v2\n  - [")]));
        assert!(!document.accepts(&[edit(1, "v2\n  - uses: d/e@v1")]));
        assert!(!document.accepts(&[edit(7, "v2")]));
        assert!(!document.accepts(&[edit(1, "v2"), edit(1, "v3")]));

        let text = DocumentText::new(
            VALIDATED,
            SourceFormat::Text,
            crate::position::PositionEncoding::Utf16,
        );
        assert!(text.accepts(&[edit(1, "v2\n  - [")]));
        assert!(!text.accepts(&[edit(1, "v2\n  - uses: d/e@v1")]));
    }

    #[test]
    fn document_text_retains_only_actions_with_valid_edits()
    -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let document = DocumentText::new(
            VALIDATED,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        );
        let good = make_outdated_diagnostic(make_range(1, 14, 16), "a/b@v1", "v2.0.0");
        let corrupting = make_outdated_diagnostic(make_range(2, 14, 16), "a/c@v1", "v2\n  - [");
        let suggestion = make_unresolvable_diagnostic(make_range(9, 14, 16), Some("v3.0.0"));

        for supports_document_changes in [true, false] {
            let actions = code_actions_for_diagnostics(
                &uri,
                TEST_VERSION,
                &[good.clone(), corrupting.clone(), suggestion.clone()],
                supports_document_changes,
            );
            assert_eq!(titles(&actions).len(), 3);
            assert_eq!(titles(&document.retain(actions)), ["Update to v2.0.0"]);
        }

        let targets = PinTargets {
            comment: true,
            sha: None,
            major: Some("v1".to_string()),
            branch: Some("main\n  - [".to_string()),
            unpin: None,
            current_is_sha: false,
        };
        let pins = pin_code_actions(
            &uri,
            TEST_VERSION,
            true,
            make_range(1, 14, 16),
            Trailing::None,
            unquoted(16),
            &targets,
        );
        assert_eq!(titles(&document.retain(pins)), ["Pin to major tag v1"]);

        let command = CodeActionOrCommand::Command(tower_lsp_server::ls_types::Command {
            title: "run".to_string(),
            command: "x".to_string(),
            arguments: None,
        });
        let bare = CodeActionOrCommand::CodeAction(CodeAction {
            title: "bare".to_string(),
            ..Default::default()
        });
        assert_eq!(
            titles(&document.retain(vec![command, bare])),
            ["run", "bare"]
        );
        Ok(())
    }

    #[test]
    fn fix_all_leaves_out_edits_the_document_refuses() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let text = "steps:\n  - uses: a/b@v1\n  - uses: a/c@v1\n  - uses: a/d@v1\n";
        let document = DocumentText::new(
            text,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        );
        let diagnostics = [
            make_outdated_diagnostic(make_range(1, 14, 16), "a/b@v1", "v2.0.0"),
            make_outdated_diagnostic(make_range(2, 14, 16), "a/c@v1", "v2\n  - ["),
            make_unresolvable_diagnostic(make_range(2, 14, 16), Some("v3.0.0")),
            make_outdated_diagnostic(make_range(3, 14, 16), "a/d@v1", "v4.0.0"),
        ];

        let fixable = document.fixable(&diagnostics);
        assert_eq!(fixable, [diagnostics[0].clone(), diagnostics[3].clone()]);
        let action = fix_all_in_file(&uri, TEST_VERSION, &fixable, true).ok_or("no fix-all")?;
        let retained = document.retain(vec![action]);
        let [CodeActionOrCommand::CodeAction(action)] = retained.as_slice() else {
            return Err("the document refused the fix-all".into());
        };
        let edits = extract_text_edits(action.edit.as_ref().ok_or("no edit")?, &uri, TEST_VERSION);
        let new_texts: Vec<&str> = edits.iter().map(|edit| edit.new_text.as_str()).collect();
        assert_eq!(new_texts, ["v2.0.0", "v4.0.0"]);

        let overlapping = [
            make_outdated_diagnostic(make_range(1, 14, 16), "a/b@v1", "v2.0.0"),
            make_outdated_diagnostic(make_range(1, 15, 16), "a/b@v1", "3"),
        ];
        let fixable = document.fixable(&overlapping);
        assert_eq!(fixable.len(), 2);
        let action = fix_all_in_file(&uri, TEST_VERSION, &fixable, true).ok_or("no fix-all")?;
        assert_eq!(document.retain(vec![action]), []);
        Ok(())
    }

    const PIN_STYLE_WORKFLOW: &str = "steps:\n  - uses: actions/checkout@v4.0.0\n  - uses: o/p@OLD # v4.0.0\n  - uses: o/q@OLD\n  - uses: o/r@OLD # mine\n  - uses: pinned/x@v4.0.0\n";

    fn pin_style_versions() -> actup_core::version::Set {
        let old = test_utils::make_version_info("v4.0.0", &"a".repeat(40));
        let new = test_utils::make_version_info("v4.2.0", &"b".repeat(40));
        actup_core::version::Set::build(
            vec![std::sync::Arc::clone(&new), old],
            HashMap::from([(4, std::sync::Arc::clone(&new))]),
            HashMap::new(),
            Some(new),
        )
    }

    fn edits_of(action: &CodeActionOrCommand, uri: &Uri) -> Vec<TextEdit> {
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("expected CodeAction");
        };
        action
            .edit
            .as_ref()
            .and_then(|edit| edit.changes.as_ref())
            .and_then(|changes| changes.get(uri))
            .cloned()
            .unwrap_or_default()
    }

    async fn fix_under_config(toml: &str) -> Result<String, Box<dyn std::error::Error>> {
        fix_text_under_config(PIN_STYLE_WORKFLOW, toml).await
    }

    async fn fix_text_under_config(
        workflow: &str,
        toml: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let text = workflow.replace("OLD", &"a".repeat(40));
        let mut provider = test_utils::MockProvider::new("github.com");
        for (owner, repo) in [
            ("actions", "checkout"),
            ("o", "p"),
            ("o", "q"),
            ("o", "r"),
            ("pinned", "x"),
        ] {
            provider = provider.with_versions(owner, repo, pin_style_versions());
        }
        let registry = test_utils::mock_registry(provider);
        let config = actup_core::Config::from_contents(
            Some((std::path::Path::new("actup.toml"), toml)),
            actup_core::config::EnvOverrides::default(),
        )?;
        let uri = "file:///test/ci.yml".parse::<Uri>()?;
        let encoding = crate::position::PositionEncoding::Utf16;
        let result = crate::diagnostics::compute_diagnostics(
            &text,
            &uri,
            &config,
            &registry,
            &crate::file_cache::FileCache::disabled(),
            encoding,
        )
        .await;

        let fix_all =
            fix_all_in_file(&uri, None, &result.diagnostics, false).ok_or("no fix-all")?;
        let quick_edits: Vec<TextEdit> =
            code_actions_for_diagnostics(&uri, None, &result.diagnostics, false)
                .iter()
                .flat_map(|action| edits_of(action, &uri))
                .collect();
        let fixed =
            crate::workspace_commands::apply_text_edits(&text, &edits_of(&fix_all, &uri), encoding)
                .ok_or("fix-all edits do not fit")?;
        let quickfixed = crate::workspace_commands::apply_text_edits(&text, &quick_edits, encoding)
            .ok_or("quickfix edits do not fit")?;
        assert_eq!(quickfixed, fixed);
        Ok(fixed)
    }

    fn expected(lines: [&str; 5]) -> String {
        format!("steps:\n{}\n", lines.join("\n")).replace("NEW", &"b".repeat(40))
    }

    #[tokio::test]
    async fn every_pin_style_keeps_the_note_of_a_version_comment()
    -> Result<(), Box<dyn std::error::Error>> {
        let workflow = "steps:\n  - uses: o/p@OLD # v4.0.0 pinned for node16\n  - uses: o/q@v4.0.0 # v4.0.0 keep\n";
        let sha = "b".repeat(40);
        for (pin, expected) in [
            (
                "preserve",
                [
                    format!("o/p@{sha} # v4.2.0 pinned for node16"),
                    "o/q@v4.2.0 # v4.2.0 keep".to_string(),
                ],
            ),
            (
                "sha",
                [
                    format!("o/p@{sha} # v4.2.0 pinned for node16"),
                    format!("o/q@{sha} # v4.2.0 keep"),
                ],
            ),
            (
                "tag",
                [
                    "o/p@v4.2.0 # pinned for node16".to_string(),
                    "o/q@v4.2.0 # v4.2.0 keep".to_string(),
                ],
            ),
        ] {
            let toml = format!("[policy]\npin = \"{pin}\"\n");
            assert_eq!(
                fix_text_under_config(workflow, &toml).await?,
                format!(
                    "steps:\n  - uses: {}\n  - uses: {}\n",
                    expected[0], expected[1]
                ),
                "{pin}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn every_pin_style_keeps_a_comment_that_names_no_version()
    -> Result<(), Box<dyn std::error::Error>> {
        let workflow = "steps:\n  - uses: o/p@OLD # 1st step\n  - uses: o/q@v4.0.0 # 1st step\n";
        let sha = "b".repeat(40);
        for (pin, expected) in [
            ("preserve", [format!("o/p@{sha}"), "o/q@v4.2.0".to_string()]),
            ("sha", [format!("o/p@{sha}"), format!("o/q@{sha}")]),
            ("tag", ["o/p@v4.2.0".to_string(), "o/q@v4.2.0".to_string()]),
        ] {
            let toml = format!("[policy]\npin = \"{pin}\"\n");
            assert_eq!(
                fix_text_under_config(workflow, &toml).await?,
                format!(
                    "steps:\n  - uses: {} # 1st step\n  - uses: {} # 1st step\n",
                    expected[0], expected[1]
                ),
                "{pin}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn pin_preserve_keeps_each_ref_style() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            fix_under_config("[policy]\npin = \"preserve\"\n").await?,
            expected([
                "  - uses: actions/checkout@v4.2.0",
                "  - uses: o/p@NEW # v4.2.0",
                "  - uses: o/q@NEW # v4.2.0",
                "  - uses: o/r@NEW # mine",
                "  - uses: pinned/x@v4.2.0",
            ])
        );
        Ok(())
    }

    #[tokio::test]
    async fn pin_sha_writes_shas_with_version_comments() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            fix_under_config("[policy]\npin = \"sha\"\n").await?,
            expected([
                "  - uses: actions/checkout@NEW # v4.2.0",
                "  - uses: o/p@NEW # v4.2.0",
                "  - uses: o/q@NEW # v4.2.0",
                "  - uses: o/r@NEW # mine",
                "  - uses: pinned/x@NEW # v4.2.0",
            ])
        );
        Ok(())
    }

    #[tokio::test]
    async fn pin_tag_unpins_outdated_shas() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            fix_under_config("[policy]\npin = \"tag\"\n").await?,
            expected([
                "  - uses: actions/checkout@v4.2.0",
                "  - uses: o/p@v4.2.0",
                "  - uses: o/q@v4.2.0",
                "  - uses: o/r@v4.2.0 # mine",
                "  - uses: pinned/x@v4.2.0",
            ])
        );
        Ok(())
    }

    #[tokio::test]
    async fn pin_override_wins_for_matching_actions() -> Result<(), Box<dyn std::error::Error>> {
        let toml = "[policy]\npin = \"tag\"\n\n[[overrides]]\npattern = \"pinned/*\"\npolicy = { pin = \"sha\" }\n\n[[overrides]]\npattern = \"o/q\"\npolicy = { pin = \"preserve\" }\n";
        assert_eq!(
            fix_under_config(toml).await?,
            expected([
                "  - uses: actions/checkout@v4.2.0",
                "  - uses: o/p@v4.2.0",
                "  - uses: o/q@NEW # v4.2.0",
                "  - uses: o/r@v4.2.0 # mine",
                "  - uses: pinned/x@NEW # v4.2.0",
            ])
        );
        Ok(())
    }

    const PIN_BRANCH_WORKFLOW: &str = "steps:\n  - uses: o/b@main\n  - uses: o/t@nightly # keep me\n  - uses: o/v@main # v1.0.0\n  - uses: \"o/e@ma\\x69n\"\n";

    fn pin_branch_versions() -> actup_core::version::Set {
        let v1 = test_utils::make_version_info("v1.0.0", &"a".repeat(40));
        let mut versions = actup_core::version::Set::build(
            vec![std::sync::Arc::clone(&v1)],
            HashMap::from([(1, std::sync::Arc::clone(&v1))]),
            HashMap::new(),
            Some(v1),
        );
        versions.set_branches(HashMap::from([("main".to_string(), "c".repeat(40))]));
        versions.set_other_tags(HashMap::from([("nightly".to_string(), "d".repeat(40))]));
        versions
    }

    #[tokio::test]
    async fn pin_branch_quick_fixes_and_fix_all_agree() -> Result<(), Box<dyn std::error::Error>> {
        let mut provider = test_utils::MockProvider::new("github.com");
        for repo in ["b", "t", "v", "e"] {
            provider = provider.with_versions("o", repo, pin_branch_versions());
        }
        let registry = test_utils::mock_registry(provider);
        let config = actup_core::Config::from_contents(
            Some((
                std::path::Path::new("actup.toml"),
                "[policy]\nbranch = \"update\"\n",
            )),
            actup_core::config::EnvOverrides::default(),
        )?;
        let uri = "file:///test/ci.yml".parse::<Uri>()?;
        let encoding = crate::position::PositionEncoding::Utf16;
        let diagnostics = crate::diagnostics::compute_diagnostics(
            PIN_BRANCH_WORKFLOW,
            &uri,
            &config,
            &registry,
            &crate::file_cache::FileCache::disabled(),
            encoding,
        )
        .await
        .diagnostics;
        let codes: Vec<(u32, bool)> = diagnostics
            .iter()
            .filter(|d| {
                d.code
                    == Some(tower_lsp_server::ls_types::NumberOrString::String(
                        "pin-branch".into(),
                    ))
            })
            .map(|d| (d.range.start.line, d.data.is_some()))
            .collect();
        assert_eq!(codes, [(1, true), (2, true), (3, true), (4, false)]);
        let fixable: Vec<u32> =
            DocumentText::new(PIN_BRANCH_WORKFLOW, SourceFormat::Yaml, encoding)
                .fixable(&diagnostics)
                .iter()
                .map(|d| d.range.start.line)
                .collect();
        assert_eq!(fixable, [1, 2, 3]);

        let quick = code_actions_for_diagnostics(&uri, None, &diagnostics, false);
        let titles: Vec<&str> = quick
            .iter()
            .filter_map(|action| match action {
                CodeActionOrCommand::CodeAction(c) => Some(c.title.as_str()),
                CodeActionOrCommand::Command(_) => None,
            })
            .collect();
        assert_eq!(
            titles,
            [
                "Pin main to ccccccc",
                "Pin nightly to ddddddd",
                "Pin main to ccccccc"
            ]
        );
        let quick_edits: Vec<TextEdit> = quick
            .iter()
            .flat_map(|action| edits_of(action, &uri))
            .collect();

        let fix_all = fix_all_in_file(&uri, None, &diagnostics, false).ok_or("no fix-all")?;
        let CodeActionOrCommand::CodeAction(action) = &fix_all else {
            return Err("expected a code action".into());
        };
        assert_eq!(action.kind, Some(SOURCE_FIX_ALL_ACTUP));
        let fixed = crate::workspace_commands::apply_text_edits(
            PIN_BRANCH_WORKFLOW,
            &edits_of(&fix_all, &uri),
            encoding,
        )
        .ok_or("fix-all edits do not fit")?;
        let quickfixed = crate::workspace_commands::apply_text_edits(
            PIN_BRANCH_WORKFLOW,
            &quick_edits,
            encoding,
        )
        .ok_or("quickfix edits do not fit")?;
        assert_eq!(quickfixed, fixed);
        assert_eq!(
            fixed,
            format!(
                "steps:\n  - uses: o/b@{c} # main\n  - uses: o/t@{d} # keep me\n  - uses: o/v@{c} # main\n  - uses: \"o/e@ma\\x69n\"\n",
                c = "c".repeat(40),
                d = "d".repeat(40),
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn closers_keep_their_place_before_the_comment() -> Result<(), Box<dyn std::error::Error>>
    {
        let old = "a".repeat(40);
        let text = format!(
            "steps:\n  - uses: \"o/b@main\"\n  - uses: 'o/t@nightly' # v1.0.0\n  - uses: \"o/s@{old}\" # v1.0.0\n  - uses: 'o/u@{old}'\n  - {{ uses: o/f@main }}\n  - [{{ uses: 'o/g@{old}' }}] # v1.0.0\n  - [{{ uses: o/h@main }}, {{ uses: o/i@main }}]\n  - {{ uses: o/k@{old}, with: {{ x: 1 }} }}\n  - {{ uses: o/l@{old}, with: {{ x: 1 }} }} # v1.0.0\n  - {{ uses: o/m@{old}, with: {{ msg: it's }} }} # v1.0.0\n  - {{ uses: o/n@{old}, with: {{ a: it's, b: 'x # v9' }} }}\n  - {{ uses: o/p@{old}, with: {{ b: 'it''s # v9' }} }}\n  - {{ uses: o/q@{old}, with: {{ b: 'it''s # x' }} }} # v1.0.0\n  - {{ uses: o/r@{old}, with: {{ b: !!str 'x # v9' }} }}\n  - {{ uses: o/w@main, with: {{ b: &a 'x # v9' }} }}\n  - {{ uses: o/x@{old}, with: {{ ? 'x # v9' : y }} }}\n  - {{ uses: o/y@{old}, with: {{ b: &a !!str \"x # v9\" }} }} # v1.0.0\n  - uses: o/z@{old}   \n  - uses: \"o/v@main\" \t\n  - {{ uses: o/a@{old} }}  \n  - uses: o/c@{old} # v1.0.0  \n  - {{ name: \"Build #1\", uses: o/d@{old} }}\n  - {{ name: \"Build #1\", uses: o/e@main }}\n  - {{ name: \"Build #1\", uses: o/j@v1.0.0 }}\n  - uses: o/b@v1.0.0 # v1.0.0 keep\n  - uses: o/b@{old} # main keep\n  - uses: o/b@main # main keep\n  - uses: o/b@{old} # lint keep\n  - uses: o/b@v1.0.0 # TODO fix\n"
        );
        let new = test_utils::make_version_info("v1.2.0", &"b".repeat(40));
        let mut versions = actup_core::version::Set::build(
            vec![
                std::sync::Arc::clone(&new),
                test_utils::make_version_info("v1.0.0", &old),
            ],
            HashMap::from([(1, std::sync::Arc::clone(&new))]),
            HashMap::new(),
            Some(new),
        );
        versions.set_branches(HashMap::from([("main".to_string(), "c".repeat(40))]));
        versions.set_other_tags(HashMap::from([("nightly".to_string(), "d".repeat(40))]));
        let mut provider = test_utils::MockProvider::new("github.com");
        for repo in [
            "b", "t", "s", "u", "f", "g", "h", "i", "k", "l", "m", "n", "p", "q", "r", "w", "x",
            "y", "z", "v", "a", "c", "d", "e", "j",
        ] {
            provider = provider.with_versions("o", repo, versions.clone());
        }
        let registry = test_utils::mock_registry(provider);
        let config = actup_core::Config::from_contents(
            Some((
                std::path::Path::new("actup.toml"),
                "[policy]\nbranch = \"update\"\n",
            )),
            actup_core::config::EnvOverrides::default(),
        )?;
        let uri = "file:///test/ci.yml".parse::<Uri>()?;
        let encoding = crate::position::PositionEncoding::Utf16;
        let diagnostics = crate::diagnostics::compute_diagnostics(
            &text,
            &uri,
            &config,
            &registry,
            &crate::file_cache::FileCache::disabled(),
            encoding,
        )
        .await
        .diagnostics;
        let fix_all = fix_all_in_file(&uri, None, &diagnostics, false).ok_or("no fix-all")?;
        let fixed =
            crate::workspace_commands::apply_text_edits(&text, &edits_of(&fix_all, &uri), encoding)
                .ok_or("fix-all edits do not fit")?;
        assert_eq!(
            fixed,
            format!(
                "steps:\n  - uses: \"o/b@{c}\" # main\n  - uses: 'o/t@{d}' # nightly\n  - uses: \"o/s@{b}\" # v1.2.0\n  - uses: 'o/u@{b}' # v1.2.0\n  - {{ uses: o/f@{c} }} # main\n  - [{{ uses: 'o/g@{b}' }}] # v1.2.0\n  - [{{ uses: o/h@{c} }}, {{ uses: o/i@{c} }}]\n  - {{ uses: o/k@{b}, with: {{ x: 1 }} }}\n  - {{ uses: o/l@{b}, with: {{ x: 1 }} }} # v1.2.0\n  - {{ uses: o/m@{b}, with: {{ msg: it's }} }} # v1.2.0\n  - {{ uses: o/n@{b}, with: {{ a: it's, b: 'x # v9' }} }}\n  - {{ uses: o/p@{b}, with: {{ b: 'it''s # v9' }} }}\n  - {{ uses: o/q@{b}, with: {{ b: 'it''s # x' }} }} # v1.2.0\n  - {{ uses: o/r@{b}, with: {{ b: !!str 'x # v9' }} }}\n  - {{ uses: o/w@{c}, with: {{ b: &a 'x # v9' }} }}\n  - {{ uses: o/x@{b}, with: {{ ? 'x # v9' : y }} }}\n  - {{ uses: o/y@{b}, with: {{ b: &a !!str \"x # v9\" }} }} # v1.2.0\n  - uses: o/z@{b} # v1.2.0\n  - uses: \"o/v@{c}\" # main\n  - {{ uses: o/a@{b} }} # v1.2.0\n  - uses: o/c@{b} # v1.2.0\n  - {{ name: \"Build #1\", uses: o/d@{b} }} # v1.2.0\n  - {{ name: \"Build #1\", uses: o/e@{c} }} # main\n  - {{ name: \"Build #1\", uses: o/j@v1.2.0 }}\n  - uses: o/b@v1.2.0 # v1.2.0 keep\n  - uses: o/b@{c} # main keep\n  - uses: o/b@{c} # main keep\n  - uses: o/b@{b} # lint keep\n  - uses: o/b@v1.2.0 # TODO fix\n",
                b = "b".repeat(40),
                c = "c".repeat(40),
                d = "d".repeat(40),
            )
        );
        Ok(())
    }

    #[test]
    fn pin_actions_keep_the_closer() -> Result<(), Box<dyn std::error::Error>> {
        let uri = "file:///test/workflow.yml".parse::<Uri>()?;
        let targets = PinTargets {
            comment: true,
            sha: Some((
                actup_core::provider::Sha40::new("c".repeat(40)).ok_or("invalid test SHA")?,
                "v7.2.3".to_string(),
            )),
            major: Some("v7".to_string()),
            branch: None,
            unpin: None,
            current_is_sha: false,
        };
        let end = LineEnd {
            closer: "\" ",
            rewrite_end_col: 47,
        };
        let actions = pin_code_actions(
            &uri,
            None,
            false,
            make_range(0, 31, 37),
            Trailing::Label { note: "keep" },
            end,
            &targets,
        );
        let edits: Vec<TextEdit> = actions
            .iter()
            .flat_map(|action| edits_of(action, &uri))
            .collect();
        assert_eq!(
            edits,
            [
                TextEdit {
                    range: make_range(0, 31, 47),
                    new_text: format!("{}\" # v7.2.3 keep", "c".repeat(40)),
                },
                TextEdit {
                    range: make_range(0, 31, 47),
                    new_text: "v7\" # v7 keep".to_string(),
                },
            ]
        );
        Ok(())
    }
}
