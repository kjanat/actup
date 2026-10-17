//! Diagnostic extraction from `uses:` references.
//!
//! Parses workflow/action YAML, resolves each `uses:` reference against
//! configured providers, and maps [`CheckStatus`] to LSP [`Diagnostic`]s.

use std::collections::HashMap;

use actup_core::{
    ActionRef, ActionRepoId, Config, Error as CoreError, SHORT_SHA_REASON, SourceFormat, Uses,
    check_action_with_provider, parse_uses, provider,
    resolution::{CheckResult, CheckStatus, RefKind},
    rewrite::{Rewrite, Trailing, after_value, closer},
};
use serde::{Deserialize, Serialize};
use tower_lsp_server::ls_types::{
    Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range, Uri,
};

use crate::{
    error_ux::{self, DiagnosticErrors},
    file_cache::FileCache,
};

/// Diagnostic source identifier shown in the editor.
pub const DIAGNOSTIC_SOURCE: &str = "actup";

/// Structured data attached to diagnostics via [`Diagnostic::data`].
///
/// Carries the information code actions need to construct edits without
/// re-fetching version data. Serialized as JSON into the diagnostic's
/// opaque `data` field and deserialized in the code action handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum DiagnosticData {
    /// The ref is outdated — target ref string is the replacement.
    #[serde(rename = "outdated")]
    Outdated {
        /// The ref string to replace with (e.g. "v2.0.0" or a commit SHA).
        target_ref: String,
        /// When re-pinning a SHA, the version comment to append after the SHA
        /// (e.g. " # v8.2.0"). `None` for tag updates and SHA pins whose line
        /// carries a non-version comment that must be preserved.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repin_comment: Option<String>,
        /// When set, extend the replacement to this 0-indexed column (in the
        /// negotiated encoding) so a stale version comment is overwritten rather
        /// than left behind. `None` replaces only the ref range.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replace_end_col: Option<u32>,
    },
    /// The ref is a short SHA; the full SHA it abbreviates replaces it.
    #[serde(rename = "expand-sha")]
    ExpandSha {
        /// The full 40-character commit SHA.
        target_ref: String,
        /// The `# vX.Y.Z` comment to append when a tag points at the commit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repin_comment: Option<String>,
        /// When set, extend the replacement to this column to overwrite a
        /// stale version comment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replace_end_col: Option<u32>,
    },
    /// The ref is a branch or non-semver tag to pin to the commit it names.
    #[serde(rename = "pin-branch")]
    PinBranch {
        /// The full commit SHA the ref names.
        sha: String,
        /// The ref as written, named in the `# <ref>` comment.
        ref_name: String,
        /// The comment written after the SHA, absent when other text follows the ref.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repin_comment: Option<String>,
        /// When set, extend the replacement to this column to overwrite a
        /// version comment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replace_end_col: Option<u32>,
    },
    /// The ref is unresolvable — includes a suggested ref if available.
    #[serde(rename = "unresolvable")]
    Unresolvable {
        /// Optional suggested replacement (e.g. closest matching tag).
        suggested_ref: Option<String>,
        /// The `# vX.Y.Z` comment to append when the suggestion is a SHA pin.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repin_comment: Option<String>,
        /// When set, extend the replacement to this column to overwrite a
        /// stale version comment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replace_end_col: Option<u32>,
    },
}

/// The replacement an outdated diagnostic should apply.
pub struct OutdatedEdit {
    pub target_ref: String,
    pub repin_comment: Option<String>,
    pub replace_end_col: Option<u32>,
}

impl OutdatedEdit {
    /// Place `rewrite` on the line that `end` describes.
    pub fn new(rewrite: Rewrite, end: LineEnd<'_>) -> Self {
        let (text, to_line_end) = rewrite.splice(end.closer);
        let after_ref = text.get(rewrite.new_ref.len()..).unwrap_or("").to_string();
        Self {
            target_ref: rewrite.new_ref,
            repin_comment: (!after_ref.is_empty()).then_some(after_ref),
            replace_end_col: to_line_end.then_some(end.line_end_col),
        }
    }
}

/// The text after a ref on its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineEnd<'a> {
    /// The [`closer`] of the value.
    pub closer: &'a str,
    /// 0-indexed end-of-line column in the negotiated encoding.
    pub line_end_col: u32,
}

/// Parsed action reference with its source location in the document.
pub struct LocatedAction {
    /// Parsed action reference (owner/repo@ref).
    pub action_ref: ActionRef,
    /// 0-indexed line in the document.
    pub line: u32,
    /// Range of the ref portion (after `@`) within the line, or the whole line
    /// when the line does not hold the value verbatim.
    pub col_start: u32,
    pub col_end: u32,
    /// Whether `col_start..col_end` covers the ref text, so edits may target it.
    pub editable: bool,
    /// The original `uses:` value string (for diagnostic messages).
    uses_value: String,
    /// The [`closer`] of the value, empty when the line takes no comment.
    pub closer: String,
    /// The text after the value on its line, empty when the ref is not editable
    /// or the line takes no comment.
    pub tail: String,
    /// Whether the line may take a version comment.
    takes_comment: bool,
    /// 0-indexed end-of-line column (negotiated encoding), used to overwrite a
    /// stale version comment when re-pinning.
    pub line_end_col: u32,
}

/// Return the [`SourceFormat`] of the file `uri` names, or YAML for a non-file URI.
pub fn uri_format(uri: &Uri) -> SourceFormat {
    if !uri.scheme().as_str().eq_ignore_ascii_case("file") {
        return SourceFormat::Yaml;
    }
    uri.to_file_path()
        .map_or(SourceFormat::Yaml, |path| SourceFormat::from_path(&path))
}

impl LocatedAction {
    /// What follows the ref on its line, [`Trailing::Foreign`] on a line that takes no comment.
    pub fn trailing(&self) -> Trailing<'_> {
        if self.takes_comment {
            Trailing::of(&self.closer, &self.tail)
        } else {
            Trailing::Foreign
        }
    }
}

/// Find the `uses:` action whose ref range contains the cursor `position`.
///
/// Used by code actions to offer pin transformations on the ref under the
/// cursor regardless of whether it has a diagnostic.
pub fn locate_action_at(
    text: &str,
    format: SourceFormat,
    position: tower_lsp_server::ls_types::Position,
    encoding: crate::position::PositionEncoding,
) -> Option<LocatedAction> {
    extract_located_actions(text, format, encoding)
        .into_iter()
        .find(|a| {
            a.editable
                && a.line == position.line
                && (a.col_start..=a.col_end).contains(&position.character)
        })
}

impl LocatedAction {
    /// The range of the ref, or of the line when the line does not hold the ref verbatim.
    const fn range(&self) -> Range {
        Range {
            start: Position {
                line: self.line,
                character: self.col_start,
            },
            end: Position {
                line: self.line,
                character: self.col_end,
            },
        }
    }

    /// The text after the ref on its line.
    pub fn line_end(&self) -> LineEnd<'_> {
        LineEnd {
            closer: &self.closer,
            line_end_col: self.line_end_col,
        }
    }
}

/// The `uses:` values of a document: valid references, and diagnostics for invalid ones.
struct LocatedUses {
    actions: Vec<LocatedAction>,
    invalid: Vec<Diagnostic>,
}

/// Extract and parse all `uses:` references from document text read as `format`.
///
/// Skips local (`./`) and docker (`docker://`) references. A value that fails
/// to parse becomes an error diagnostic on its ref, or on its line when the
/// line does not hold the ref verbatim.
///
/// Column positions are converted from byte offsets to the negotiated
/// LSP position encoding. A ref on a line outside
/// [`SourceFormat::comment_lines`] gets [`Trailing::Foreign`], an empty
/// closer and an empty tail, so no edit appends, rewrites or reads a comment
/// there.
fn locate_uses(
    text: &str,
    format: SourceFormat,
    encoding: crate::position::PositionEncoding,
) -> LocatedUses {
    let locations = format.extract_uses(text);
    let commentable = format.comment_lines(text);
    let mut located = LocatedUses {
        actions: Vec::with_capacity(locations.len()),
        invalid: Vec::new(),
    };

    for loc in locations {
        let Some(line) = loc
            .line()
            .checked_sub(1)
            .and_then(|line| u32::try_from(line).ok())
        else {
            continue;
        };
        // Convert byte offsets to the negotiated LSP encoding.
        let raw_line = loc.raw_line();
        let line_end_col = crate::position::byte_to_col(raw_line, raw_line.len(), encoding);
        let ref_cols = loc.ref_range().map(|r| {
            (
                crate::position::byte_to_col(raw_line, r.start, encoding),
                crate::position::byte_to_col(raw_line, r.end, encoding),
            )
        });
        let action_ref = match loc.classify() {
            None | Some(Uses::Local) => continue,
            Some(Uses::Action(action_ref)) => action_ref,
            Some(Uses::Invalid { error, .. }) => {
                let (col_start, col_end) = ref_cols.unwrap_or((0, line_end_col));
                let reason = match &error {
                    CoreError::InvalidRef { reason, .. } => reason.clone(),
                    other => other.to_string(),
                };
                located.invalid.push(unresolvable_diagnostic(
                    Range {
                        start: Position {
                            line,
                            character: col_start,
                        },
                        end: Position {
                            line,
                            character: col_end,
                        },
                    },
                    loc.value(),
                    &reason,
                    false,
                ));
                continue;
            }
        };
        let (col_start, col_end, editable, takes_comment, closing, tail) =
            loc.ref_range()
                .map_or((0, line_end_col, false, true, "", ""), |r| {
                    let takes_comment = commentable.contains(&loc.line());
                    let (col_start, col_end) = ref_cols.unwrap_or((0, line_end_col));
                    (
                        col_start,
                        col_end,
                        true,
                        takes_comment,
                        if takes_comment {
                            closer(raw_line, r.end)
                        } else {
                            ""
                        },
                        if takes_comment {
                            after_value(raw_line, r.end)
                        } else {
                            ""
                        },
                    )
                });
        located.actions.push(LocatedAction {
            action_ref,
            line,
            col_start,
            col_end,
            editable,
            uses_value: loc.value().to_string(),
            closer: closing.to_string(),
            tail: tail.to_string(),
            takes_comment,
            line_end_col,
        });
    }

    located
}

fn extract_located_actions(
    text: &str,
    format: SourceFormat,
    encoding: crate::position::PositionEncoding,
) -> Vec<LocatedAction> {
    locate_uses(text, format, encoding).actions
}

fn unique_repo_ids(actions: &[LocatedAction]) -> Vec<ActionRepoId> {
    let mut seen = std::collections::HashSet::new();
    actions
        .iter()
        .filter_map(|action| {
            let id = action.action_ref.to_repo_id().without_subpath();
            seen.insert(id.clone()).then_some(id)
        })
        .collect()
}

pub fn extract_unique_repo_ids(
    text: &str,
    format: SourceFormat,
    encoding: crate::position::PositionEncoding,
) -> Vec<ActionRepoId> {
    unique_repo_ids(&extract_located_actions(text, format, encoding))
}

/// Fetch version sets for a batch of unique repos, using file cache first.
///
/// Returns a map from `ActionRepoId` to either a fetched `version::Set`
/// or an error. Repos found in cache are not re-fetched.
pub async fn batch_fetch_versions(
    repos: &[ActionRepoId],
    config: &Config,
    registry: &provider::Registry,
    file_cache: &FileCache,
) -> HashMap<ActionRepoId, Result<actup_core::version::Set, CoreError>> {
    actup_provider_github::fetch_versions_with_registry(
        repos,
        config,
        registry,
        std::sync::Arc::new(file_cache.clone()),
        &actup_core::fetch::Silent,
    )
    .await
}

async fn compute_diagnostics_for_actions(
    located: LocatedUses,
    config: &Config,
    registry: &provider::Registry,
    versions_map: &HashMap<ActionRepoId, Result<actup_core::version::Set, CoreError>>,
) -> DiagnosticResult {
    let LocatedUses { actions, invalid } = located;
    let refs_checked = actions.len() + invalid.len();
    let mut diagnostics = Vec::with_capacity(refs_checked);
    let mut errors = DiagnosticErrors::default();

    for action in &actions {
        let range = action.range();

        let repo_id = action.action_ref.to_repo_id().without_subpath();
        let versions = versions_map.get(&repo_id).and_then(|r| r.as_ref().ok());
        let result = match versions_map.get(&repo_id) {
            Some(Ok(versions)) => match registry.find_for_repo(&repo_id) {
                Some(provider) => {
                    check_action_with_provider(&action.action_ref, versions, config, provider).await
                }
                None => CheckResult::new(
                    action.action_ref.clone(),
                    CheckStatus::Unresolvable {
                        reason: "internal: provider not found for fetched repo".into(),
                        is_environment_error: true,
                    },
                ),
            },
            Some(Err(e)) => {
                errors.record(e);
                CheckResult::new(
                    action.action_ref.clone(),
                    CheckStatus::Unresolvable {
                        reason: actup_core::error::chain(e),
                        is_environment_error: e.is_environment_error(),
                    },
                )
            }
            None => CheckResult::new(
                action.action_ref.clone(),
                CheckStatus::Unresolvable {
                    reason: "internal: repo not in version map".into(),
                    is_environment_error: true,
                },
            ),
        };

        let edit = match result.status() {
            CheckStatus::Outdated { target, .. } => Some(OutdatedEdit::new(
                Rewrite::outdated(
                    config.policy_for(&action.action_ref).pin,
                    action.action_ref.is_sha(),
                    target,
                    action.trailing(),
                ),
                action.line_end(),
            )),
            CheckStatus::PinBranch { ref_name, sha } => Some(OutdatedEdit::new(
                Rewrite::pin(sha, ref_name, action.trailing()),
                action.line_end(),
            )),
            CheckStatus::Unresolvable {
                reason,
                is_environment_error: false,
            } if action.editable && reason == SHORT_SHA_REASON => {
                expand_edit(action, registry.find_for_repo(&repo_id), versions).await
            }
            _ => None,
        };

        if let Some(diag) = status_to_diagnostic(
            result.status(),
            range,
            &action.uses_value,
            edit,
            result.ref_kind(),
        ) {
            let mut diag = same_commit_diagnostic(diag, &result, &action.uses_value);
            if diag.data.is_none()
                && let Some(tag) = result.suggestion()
            {
                diag.data = suggestion_data(action, config, tag);
            }
            if !action.editable {
                diag.data = None;
            }
            if let Some(stale) = versions.and_then(actup_core::version::Set::stale) {
                diag.message = format!("{} (stale: {stale})", diag.message);
            }
            diagnostics.push(diag);
        }
    }

    diagnostics.extend(archived_diagnostics(&actions, config, registry).await);

    diagnostics.extend(invalid);
    diagnostics.sort_by_key(|d| (d.range.start.line, d.range.start.character));
    DiagnosticResult {
        diagnostics,
        refs_checked,
        errors,
        version_sets: successful_version_sets(versions_map),
    }
}

/// The diagnostic text for a repository state worth reporting.
pub const fn repo_state_note(state: provider::RepoState) -> Option<&'static str> {
    match state {
        provider::RepoState::Active => None,
        provider::RepoState::Archived => Some("repository is archived"),
        provider::RepoState::Disabled => Some("repository is disabled"),
    }
}

async fn archived_diagnostics(
    actions: &[LocatedAction],
    config: &Config,
    registry: &provider::Registry,
) -> Vec<Diagnostic> {
    let reported: Vec<&LocatedAction> = actions
        .iter()
        .filter(|a| {
            !config.should_ignore(&a.action_ref)
                && config.policy_for(&a.action_ref).archived == actup_core::policy::Archived::Hint
        })
        .collect();
    let mut repos: Vec<ActionRepoId> = reported
        .iter()
        .map(|a| a.action_ref.to_repo_id().without_subpath())
        .collect();
    repos.sort_by_key(ToString::to_string);
    repos.dedup();
    let lookups = repos.iter().map(|repo| async move {
        let state = match registry.find_for_repo(repo) {
            Some(provider) => provider.get_repo_state(repo).await.ok().flatten(),
            None => None,
        };
        (repo, state)
    });
    let states: HashMap<&ActionRepoId, provider::RepoState> = futures::future::join_all(lookups)
        .await
        .into_iter()
        .filter_map(|(repo, state)| state.map(|s| (repo, s)))
        .collect();
    reported
        .into_iter()
        .filter_map(|action| {
            let repo = action.action_ref.to_repo_id().without_subpath();
            let note = repo_state_note(*states.get(&repo)?)?;
            Some(Diagnostic {
                range: Range {
                    start: Position {
                        line: action.line,
                        character: action.col_start,
                    },
                    end: Position {
                        line: action.line,
                        character: action.col_end,
                    },
                },
                severity: Some(DiagnosticSeverity::HINT),
                code: Some(NumberOrString::String("archived".into())),
                source: Some(DIAGNOSTIC_SOURCE.into()),
                message: format!("{}: {note}", action.uses_value),
                ..Default::default()
            })
        })
        .collect()
}

/// Build the edit that expands a short SHA ref to the full SHA it abbreviates,
/// commented with the tag on that commit when one exists.
async fn expand_edit(
    action: &LocatedAction,
    provider: Option<&dyn provider::Provider>,
    versions: Option<&actup_core::version::Set>,
) -> Option<OutdatedEdit> {
    let repo_id = action.action_ref.to_repo_id().without_subpath();
    let sha = provider?
        .resolve_ref_to_sha(&repo_id, action.action_ref.ref_str())
        .await
        .ok()
        .filter(|sha| action.action_ref.abbreviates(sha.as_str()))?;
    let sha = sha.as_str();
    let untagged = || OutdatedEdit {
        target_ref: sha.to_string(),
        repin_comment: None,
        replace_end_col: None,
    };
    Some(
        versions
            .and_then(|v| v.tag_on(sha))
            .map_or_else(untagged, |tag| {
                OutdatedEdit::new(
                    Rewrite::pin(sha, tag.version().as_str(), action.trailing()),
                    action.line_end(),
                )
            }),
    )
}

pub async fn compute_diagnostics_with_versions(
    text: &str,
    format: SourceFormat,
    config: &Config,
    registry: &provider::Registry,
    versions_map: &HashMap<ActionRepoId, Result<actup_core::version::Set, CoreError>>,
    encoding: crate::position::PositionEncoding,
) -> DiagnosticResult {
    let located = locate_uses(text, format, encoding);
    compute_diagnostics_for_actions(located, config, registry, versions_map).await
}

/// Map a [`CheckStatus`] to an LSP [`Diagnostic`].
///
/// Returns `None` for statuses that don't warrant a diagnostic
/// (`UpToDate`, `Ignored`).
fn status_to_diagnostic(
    status: &CheckStatus,
    range: Range,
    uses_value: &str,
    edit: Option<OutdatedEdit>,
    ref_kind: Option<RefKind>,
) -> Option<Diagnostic> {
    let kind = RefKind::noun(ref_kind);
    match status {
        CheckStatus::UpToDate | CheckStatus::Ignored => None,

        CheckStatus::Outdated { current, target } => {
            let OutdatedEdit {
                target_ref,
                repin_comment,
                replace_end_col,
            } = edit.unwrap_or_else(|| {
                let is_sha_pin = parse_uses(uses_value)
                    .ok()
                    .flatten()
                    .is_some_and(|r| r.is_sha());
                OutdatedEdit {
                    target_ref: if is_sha_pin {
                        target.sha().to_string()
                    } else {
                        target.version().to_string()
                    },
                    repin_comment: None,
                    replace_end_col: None,
                }
            });
            let data = DiagnosticData::Outdated {
                target_ref,
                repin_comment,
                replace_end_col,
            };
            Some(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                code: Some(NumberOrString::String("outdated".into())),
                source: Some(DIAGNOSTIC_SOURCE.into()),
                // Message reads in versions regardless of the edit's ref style.
                message: format!(
                    "{uses_value}: {current} -> {} ({})",
                    target.version(),
                    target.sha().get(..7).unwrap_or_else(|| target.sha()),
                ),
                data: serde_json::to_value(data).ok(),
                ..Default::default()
            })
        }

        CheckStatus::Pinned { sha, tag } => {
            let tag_info = tag
                .as_ref()
                .map_or_else(|| "unknown version".into(), |t| format!("= {t}"));
            Some(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::HINT),
                code: Some(NumberOrString::String("pinned".into())),
                source: Some(DIAGNOSTIC_SOURCE.into()),
                message: format!(
                    "{uses_value}: pinned to {} ({tag_info})",
                    sha.get(..7).unwrap_or(sha),
                ),
                ..Default::default()
            })
        }

        CheckStatus::Floating { ref_name } => Some(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String("floating".into())),
            source: Some(DIAGNOSTIC_SOURCE.into()),
            message: format!(
                "{uses_value}: '{ref_name}' is a floating {kind}, pin to a tag or SHA"
            ),
            ..Default::default()
        }),

        CheckStatus::PinBranch { ref_name, sha } => Some(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String("pin-branch".into())),
            source: Some(DIAGNOSTIC_SOURCE.into()),
            message: format!(
                "{uses_value}: pin {kind} '{ref_name}' to {} # {ref_name}",
                sha.get(..7).unwrap_or(sha)
            ),
            data: edit.and_then(|e| pin_branch_data(ref_name, e)),
            ..Default::default()
        }),

        CheckStatus::Unresolvable {
            reason,
            is_environment_error,
        } => {
            let mut diag =
                unresolvable_diagnostic(range, uses_value, reason, *is_environment_error);
            diag.data = edit.and_then(|e| {
                serde_json::to_value(DiagnosticData::ExpandSha {
                    target_ref: e.target_ref,
                    repin_comment: e.repin_comment,
                    replace_end_col: e.replace_end_col,
                })
                .ok()
            });
            Some(diag)
        }
    }
}

fn suggestion_data(
    action: &LocatedAction,
    config: &Config,
    tag: &actup_core::version::Info,
) -> Option<serde_json::Value> {
    let edit = OutdatedEdit::new(
        Rewrite::outdated(
            config.policy_for(&action.action_ref).pin,
            action.action_ref.is_sha(),
            tag,
            action.trailing(),
        ),
        action.line_end(),
    );
    serde_json::to_value(DiagnosticData::Unresolvable {
        suggested_ref: Some(edit.target_ref),
        repin_comment: edit.repin_comment,
        replace_end_col: edit.replace_end_col,
    })
    .ok()
}

fn pin_branch_data(ref_name: &str, edit: OutdatedEdit) -> Option<serde_json::Value> {
    serde_json::to_value(DiagnosticData::PinBranch {
        sha: edit.target_ref,
        ref_name: ref_name.to_string(),
        repin_comment: edit.repin_comment,
        replace_end_col: edit.replace_end_col,
    })
    .ok()
}

fn same_commit_diagnostic(diag: Diagnostic, result: &CheckResult, uses_value: &str) -> Diagnostic {
    let Some(target) = result.same_commit() else {
        return diag;
    };
    let kind = result
        .ref_kind()
        .map(|k| format!("{k} "))
        .unwrap_or_default();
    let lead = if result.same_commit_pins() {
        "pin available, "
    } else {
        ""
    };
    Diagnostic {
        severity: Some(DiagnosticSeverity::INFORMATION),
        code: Some(NumberOrString::String("same-commit".into())),
        message: format!(
            "{uses_value}: {lead}same commit: {kind}{} = {} ({})",
            result.action_ref().ref_str(),
            target.version(),
            target.sha().get(..7).unwrap_or_else(|| target.sha()),
        ),
        ..diag
    }
}

/// Build the diagnostic for an unresolvable ref, splitting environment errors
/// (info-level, "check failed") from user errors (error-level, reason shown).
fn unresolvable_diagnostic(
    range: Range,
    uses_value: &str,
    reason: &str,
    is_environment_error: bool,
) -> Diagnostic {
    let severity = if is_environment_error {
        DiagnosticSeverity::INFORMATION
    } else {
        DiagnosticSeverity::ERROR
    };
    let message = if is_environment_error {
        error_ux::format_check_failed(uses_value, reason)
    } else {
        format!("{uses_value}: {reason}")
    };
    Diagnostic {
        range,
        severity: Some(severity),
        code: Some(NumberOrString::String("unresolvable".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message,
        ..Default::default()
    }
}

/// Result of a diagnostic computation: diagnostics to publish plus any
/// environment errors that should trigger user notifications.
pub struct DiagnosticResult {
    /// LSP diagnostics to publish for the document.
    pub diagnostics: Vec<Diagnostic>,
    /// Total number of `uses:` action refs checked (including up-to-date/ignored
    /// refs that don't produce diagnostics). Used for accurate progress reporting.
    pub refs_checked: usize,
    /// Environment errors encountered during version fetching.
    pub errors: DiagnosticErrors,
    /// Successful version lookups keyed by repo, reused by hover/completion.
    pub version_sets: HashMap<ActionRepoId, actup_core::version::Set>,
}

fn successful_version_sets(
    versions_map: &HashMap<ActionRepoId, Result<actup_core::version::Set, CoreError>>,
) -> HashMap<ActionRepoId, actup_core::version::Set> {
    versions_map
        .iter()
        .filter_map(|(repo, result)| {
            result
                .as_ref()
                .ok()
                .map(|versions| (repo.clone(), versions.clone()))
        })
        .collect()
}

/// Compute diagnostics for all `uses:` references in a document.
///
/// This is the main entry point called from `did_open` / `did_change`.
/// It extracts refs, deduplicates repos, fetches versions (with caching),
/// checks each ref against policy, and maps results to LSP diagnostics.
///
/// Environment errors are both:
/// 1. Surfaced as `Information`-severity diagnostics on the affected refs
/// 2. Collected in [`DiagnosticResult::errors`] for user notifications
pub async fn compute_diagnostics(
    text: &str,
    uri: &Uri,
    config: &Config,
    registry: &provider::Registry,
    file_cache: &FileCache,
    encoding: crate::position::PositionEncoding,
) -> DiagnosticResult {
    let located = locate_uses(text, uri_format(uri), encoding);
    let unique_repos = unique_repo_ids(&located.actions);
    let versions_map = if unique_repos.is_empty() {
        HashMap::new()
    } else {
        batch_fetch_versions(&unique_repos, config, registry, file_cache).await
    };

    compute_diagnostics_for_actions(located, config, registry, &versions_map).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use actup_core::version;

    use super::*;
    use crate::test_utils::{self, DUMMY_SHA, DUMMY_SHA_2};

    fn make_range(line: u32, col_start: u32, col_end: u32) -> Range {
        test_utils::make_range(line, col_start, col_end)
    }

    /// Test shim: most tests don't exercise the comment-aware re-pin edit, so
    /// this shadows the real `status_to_diagnostic` with a `None` edit. Tests
    /// that need an edit call `super::status_to_diagnostic` with four args.
    fn status_to_diagnostic(
        status: &CheckStatus,
        range: Range,
        uses_value: &str,
    ) -> Option<Diagnostic> {
        super::status_to_diagnostic(status, range, uses_value, None, None)
    }

    // --- CheckStatus -> Diagnostic severity mapping ---

    #[test]
    fn outdated_maps_to_warning() {
        let target = Arc::new(version::Info::new(
            version::Version::parse("v2.0.0"),
            DUMMY_SHA,
        ));
        let status = CheckStatus::Outdated {
            current: "v1.0.0".into(),
            target,
        };
        let diag = status_to_diagnostic(&status, make_range(0, 10, 16), "actions/checkout@v1.0.0")
            .unwrap();
        assert_eq!(diag.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(diag.code, Some(NumberOrString::String("outdated".into())));
        assert!(diag.message.contains("v1.0.0"));
        assert!(diag.message.contains("v2.0.0"));
    }

    #[test]
    fn outdated_message_contains_short_sha() {
        let target = Arc::new(version::Info::new(
            version::Version::parse("v3.0.0"),
            DUMMY_SHA,
        ));
        let status = CheckStatus::Outdated {
            current: "v2.5.0".into(),
            target,
        };
        let diag = status_to_diagnostic(&status, make_range(0, 0, 5), "o/r@v2.5.0").unwrap();
        assert!(
            diag.message.contains(&DUMMY_SHA[..7]),
            "expected short SHA in message, got: {}",
            diag.message
        );
    }

    #[test]
    fn outdated_embeds_diagnostic_data() {
        let target = Arc::new(version::Info::new(
            version::Version::parse("v4.0.0"),
            DUMMY_SHA,
        ));
        let status = CheckStatus::Outdated {
            current: "v3".into(),
            target,
        };
        let diag = status_to_diagnostic(&status, make_range(0, 0, 5), "o/r@v3").unwrap();
        let data: DiagnosticData = serde_json::from_value(diag.data.unwrap()).unwrap();
        match data {
            DiagnosticData::Outdated { target_ref, .. } => assert_eq!(target_ref, "v4.0.0"),
            DiagnosticData::Unresolvable { .. }
            | DiagnosticData::ExpandSha { .. }
            | DiagnosticData::PinBranch { .. } => {
                panic!("expected Outdated variant")
            }
        }
    }

    #[test]
    fn outdated_sha_pin_repins_to_sha_not_tag() {
        // A SHA pin that's behind must re-pin to the newer SHA, never unpin to a tag.
        let new_sha = "2222222222222222222222222222222222222222";
        let target = Arc::new(version::Info::new(
            version::Version::parse("v7.0.1"),
            new_sha,
        ));
        let status = CheckStatus::Outdated {
            current: "v7.0.0".into(),
            target,
        };
        let old_sha = "1".repeat(40);
        let uses_value = format!("actions/checkout@{old_sha}");
        let diag = status_to_diagnostic(&status, make_range(0, 31, 71), &uses_value).unwrap();

        let data: DiagnosticData = serde_json::from_value(diag.data.unwrap()).unwrap();
        let DiagnosticData::Outdated { target_ref, .. } = data else {
            panic!("expected Outdated data");
        };
        assert_eq!(target_ref, new_sha, "SHA pin must re-pin to the new SHA");
        // The human message still reads in versions, not raw SHAs.
        assert!(diag.message.contains("v7.0.1"), "got: {}", diag.message);
    }

    #[test]
    fn outdated_edit_extends_to_the_line_end_only_when_asked() {
        let rewrite = |to_line_end| Rewrite {
            new_ref: "v8.2.0".into(),
            comment: Some(" # x".into()),
            to_line_end,
        };
        let end = LineEnd {
            closer: " ",
            line_end_col: 70,
        };
        let edit = OutdatedEdit::new(rewrite(true), end);
        assert_eq!(edit.target_ref, "v8.2.0");
        assert_eq!(edit.repin_comment.as_deref(), Some(" # x"));
        assert_eq!(edit.replace_end_col, Some(70));
        assert_eq!(OutdatedEdit::new(rewrite(false), end).replace_end_col, None);
    }

    #[test]
    fn outdated_edit_writes_the_comment_after_the_closer() {
        let rewrite = |comment: Option<&str>, to_line_end| Rewrite {
            new_ref: "v8.2.0".into(),
            comment: comment.map(str::to_string),
            to_line_end,
        };
        let end = LineEnd {
            closer: "\" } ",
            line_end_col: 70,
        };
        let placed = |rewrite| {
            let edit = OutdatedEdit::new(rewrite, end);
            (edit.repin_comment, edit.replace_end_col)
        };
        assert_eq!(
            placed(rewrite(Some(" # x"), false)),
            (Some("\" } # x".to_string()), Some(70))
        );
        assert_eq!(
            placed(rewrite(Some(" # x"), true)),
            (Some("\" } # x".to_string()), Some(70))
        );
        assert_eq!(
            placed(rewrite(None, true)),
            (Some("\" }".to_string()), Some(70))
        );
        assert_eq!(placed(rewrite(None, false)), (None, None));
    }

    #[test]
    fn outdated_tag_still_updates_to_tag() {
        // Regression: a tag ref keeps updating to the tag, not the SHA.
        let target = Arc::new(version::Info::new(
            version::Version::parse("v4.2.0"),
            DUMMY_SHA,
        ));
        let status = CheckStatus::Outdated {
            current: "v3".into(),
            target,
        };
        let diag =
            status_to_diagnostic(&status, make_range(0, 0, 5), "actions/checkout@v3").unwrap();
        let data: DiagnosticData = serde_json::from_value(diag.data.unwrap()).unwrap();
        let DiagnosticData::Outdated { target_ref, .. } = data else {
            panic!("expected Outdated data");
        };
        assert_eq!(target_ref, "v4.2.0");
    }

    #[test]
    fn pinned_maps_to_hint() {
        let status = CheckStatus::Pinned {
            sha: DUMMY_SHA.into(),
            tag: Some("v3.1.0".into()),
        };
        let diag = status_to_diagnostic(
            &status,
            make_range(5, 20, 60),
            &format!("actions/checkout@{DUMMY_SHA}"),
        )
        .unwrap();
        assert_eq!(diag.severity, Some(DiagnosticSeverity::HINT));
        assert_eq!(diag.code, Some(NumberOrString::String("pinned".into())));
        assert!(diag.message.contains("v3.1.0"));
    }

    #[test]
    fn pinned_unknown_tag() {
        let status = CheckStatus::Pinned {
            sha: DUMMY_SHA.into(),
            tag: None,
        };
        let diag = status_to_diagnostic(
            &status,
            make_range(0, 0, 40),
            &format!("owner/repo@{DUMMY_SHA}"),
        )
        .unwrap();
        assert!(diag.message.contains("unknown version"));
    }

    #[test]
    fn pinned_has_no_embedded_data() {
        let status = CheckStatus::Pinned {
            sha: DUMMY_SHA.into(),
            tag: Some("v1.0.0".into()),
        };
        let diag = status_to_diagnostic(&status, make_range(0, 0, 40), "o/r@sha").unwrap();
        // Pinned diagnostics have no DiagnosticData (no code action target).
        assert!(diag.data.is_none());
    }

    #[test]
    fn pinned_message_uses_short_sha() {
        let status = CheckStatus::Pinned {
            sha: DUMMY_SHA.into(),
            tag: Some("v2.0.0".into()),
        };
        let diag = status_to_diagnostic(&status, make_range(0, 0, 40), "o/r@sha").unwrap();
        assert!(
            diag.message.contains(&DUMMY_SHA[..7]),
            "expected short SHA, got: {}",
            diag.message
        );
    }

    #[test]
    fn unresolvable_user_error_maps_to_error() {
        let status = CheckStatus::Unresolvable {
            reason: "repository not found".into(),
            is_environment_error: false,
        };
        let diag = status_to_diagnostic(&status, make_range(2, 8, 14), "bad/repo@v1").unwrap();
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert!(diag.message.contains("repository not found"));
    }

    #[test]
    fn unresolvable_user_error_code_is_unresolvable() {
        let status = CheckStatus::Unresolvable {
            reason: "not found".into(),
            is_environment_error: false,
        };
        let diag = status_to_diagnostic(&status, make_range(0, 0, 5), "o/r@v1").unwrap();
        assert_eq!(
            diag.code,
            Some(NumberOrString::String("unresolvable".into()))
        );
    }

    #[test]
    fn unresolvable_env_error_maps_to_information() {
        let status = CheckStatus::Unresolvable {
            reason: "rate limited".into(),
            is_environment_error: true,
        };
        let diag =
            status_to_diagnostic(&status, make_range(1, 5, 10), "actions/setup-node@v4").unwrap();
        assert_eq!(diag.severity, Some(DiagnosticSeverity::INFORMATION));
        assert!(
            diag.message.contains("(check failed: rate limited)"),
            "expected check-failed format, got: {}",
            diag.message
        );
    }

    #[test]
    fn unresolvable_user_error_does_not_use_check_failed_format() {
        let status = CheckStatus::Unresolvable {
            reason: "repository not found".into(),
            is_environment_error: false,
        };
        let diag = status_to_diagnostic(&status, make_range(0, 0, 10), "bad/repo@v1").unwrap();
        assert!(
            !diag.message.contains("check failed"),
            "user errors should not use check-failed format, got: {}",
            diag.message
        );
        assert!(diag.message.contains("repository not found"));
    }

    #[test]
    fn up_to_date_produces_no_diagnostic() {
        assert!(
            status_to_diagnostic(
                &CheckStatus::UpToDate,
                make_range(0, 0, 10),
                "actions/checkout@v4"
            )
            .is_none()
        );
    }

    #[test]
    fn ignored_produces_no_diagnostic() {
        assert!(
            status_to_diagnostic(
                &CheckStatus::Ignored,
                make_range(0, 0, 10),
                "actions/checkout@v4"
            )
            .is_none()
        );
    }

    #[test]
    fn diagnostic_source_is_actup() {
        let status = CheckStatus::Outdated {
            current: "v1".into(),
            target: Arc::new(version::Info::new(
                version::Version::parse("v2.0.0"),
                "0000000000000000000000000000000000000000",
            )),
        };
        let diag = status_to_diagnostic(&status, make_range(0, 0, 5), "o/r@v1").unwrap();
        assert_eq!(diag.source.as_deref(), Some("actup"));
    }

    // --- Extraction from YAML ---

    #[test]
    fn extract_located_actions_parses_workflow() {
        let yaml = r"
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v3
      - uses: ./local-action
";
        let actions = extract_located_actions(
            yaml,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        );
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].action_ref.owner(), "actions");
        assert_eq!(actions[0].action_ref.repo(), "checkout");
        assert_eq!(actions[0].action_ref.ref_str(), "v4");
        assert_eq!(actions[1].action_ref.repo(), "setup-node");
        assert_eq!(actions[0].line, 7);
        assert_eq!(actions[1].line, 8);
    }

    #[test]
    fn extract_located_actions_ref_range_points_after_at() {
        let yaml = "      - uses: actions/checkout@v4\n";
        let actions = extract_located_actions(
            yaml,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        );
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].col_start, 31);
        assert_eq!(actions[0].col_end, 33);
    }

    #[test]
    fn extract_located_actions_flow_style_ranges_cover_only_ref() {
        let sha = "a".repeat(40);
        let yaml = format!(
            "steps:\n  - {{ uses: a/b@v4, with: {{ x: 1 }} }}\n  - [{{ uses: c/d@v1 }}]\n  - {{ name: \"✓ ok\", uses: e/f@v2 }}\n  - {{ uses: g/h@{sha}, id: x }} # v1.0.0\n"
        );
        let actions = extract_located_actions(
            &yaml,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        );
        let located: Vec<(String, u32, u32, u32, Trailing<'_>)> = actions
            .iter()
            .map(|a| {
                (
                    a.action_ref.ref_str().to_string(),
                    a.line,
                    a.col_start,
                    a.col_end,
                    a.trailing(),
                )
            })
            .collect();
        assert_eq!(
            located,
            [
                ("v4".to_string(), 1, 16, 18, Trailing::Foreign),
                ("v1".to_string(), 2, 17, 19, Trailing::None),
                ("v2".to_string(), 3, 30, 32, Trailing::None),
                (sha, 4, 16, 56, Trailing::Version { note: "" }),
            ]
        );
    }

    #[test]
    fn extract_located_actions_number_lines_at_crlf_cr_and_lf() {
        let yaml = "\u{feff}uses: a/b@v1\r\nx:\r  - uses: c/d@v2\r\n  - uses: \"e/f@v3\"\ny: [{ uses: g/h@v4 }]";
        let located: Vec<(String, u32, u32, u32)> = extract_located_actions(
            yaml,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        )
        .into_iter()
        .map(|a| {
            assert!(a.editable, "{}", a.action_ref);
            (
                a.action_ref.ref_str().to_string(),
                a.line,
                a.col_start,
                a.col_end,
            )
        })
        .collect();
        assert_eq!(
            located,
            [
                ("v1".to_string(), 0, 11, 13),
                ("v2".to_string(), 2, 14, 16),
                ("v3".to_string(), 3, 15, 17),
                ("v4".to_string(), 4, 16, 18),
            ]
        );
    }

    #[test]
    fn extract_located_actions_without_verbatim_value_cover_the_line()
    -> Result<(), std::num::TryFromIntError> {
        for yaml in [
            "steps:\n  - uses: \"actions/checkout@v\\x34\"\n",
            "steps:\n  - uses: 'a/b@v''4'\n",
            "steps:\n  - uses: |\n      a/b@v4\n",
            "steps:\n  - uses: \"a/b@\\\n      v4\"\n",
            "steps:\n  - name: \"✓\"\n    uses: \"a/b@v\\x34\"\n",
        ] {
            let actions = extract_located_actions(
                yaml,
                SourceFormat::Yaml,
                crate::position::PositionEncoding::Utf16,
            );
            let [action] = actions.as_slice() else {
                panic!("expected one action in {yaml:?}");
            };
            let Some(line) = yaml.lines().nth(action.line as usize) else {
                panic!("line {} missing in {yaml:?}", action.line);
            };
            let line_len = line.encode_utf16().count();
            assert!(!action.editable, "{yaml:?}");
            assert_eq!(
                (action.col_start, action.col_end, action.trailing()),
                (0, u32::try_from(line_len)?, Trailing::None),
                "{yaml:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn locate_action_at_skips_value_not_written_verbatim() {
        let yaml = "steps:\n  - uses: \"actions/checkout@v\\x34\"\n  - uses: a/b@v4\n";
        let encoding = crate::position::PositionEncoding::Utf16;
        let at = |line, character| {
            locate_action_at(
                yaml,
                SourceFormat::Yaml,
                Position { line, character },
                encoding,
            )
        };
        assert!(at(1, 0).is_none());
        assert!(at(1, 28).is_none());
        let Some(action) = at(2, 15) else {
            panic!("verbatim ref not located");
        };
        assert_eq!(action.action_ref.repo(), "b");
    }

    #[test]
    fn extract_located_actions_skips_docker_and_local() {
        let yaml = r"
      - uses: docker://node:16
      - uses: ./my-action
      - uses: actions/checkout@v4
";
        let actions = extract_located_actions(
            yaml,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        );
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_ref.repo(), "checkout");
    }

    fn invalid_ref_errors(yaml: &str) -> Vec<(u32, u32, u32, String)> {
        locate_uses(
            yaml,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        )
        .invalid
        .into_iter()
        .map(|d| {
            assert_eq!(d.severity, Some(DiagnosticSeverity::ERROR));
            assert_eq!(d.code, Some(NumberOrString::String("unresolvable".into())));
            (
                d.range.start.line,
                d.range.start.character,
                d.range.end.character,
                d.message,
            )
        })
        .collect()
    }

    #[test]
    fn a_plain_scalar_over_two_lines_is_an_invalid_ref() {
        let yaml = "runs:\n  steps:\n  - uses: a/b@v4\n      more\n";
        assert!(
            extract_located_actions(
                yaml,
                SourceFormat::Yaml,
                crate::position::PositionEncoding::Utf16
            )
            .is_empty()
        );
        assert_eq!(
            invalid_ref_errors(yaml),
            [(
                2,
                0,
                16,
                "a/b@v4 more: ref `v4 more` holds the character ' '".to_string()
            )]
        );
    }

    #[test]
    fn uses_keys_outside_action_positions_get_no_diagnostic() {
        let yaml =
            "jobs:\n  b:\n    steps:\n      - uses: ./local\n        with:\n          uses: lint\n";
        assert_eq!(invalid_ref_errors(yaml), []);
    }

    #[test]
    fn invalid_git_refs_reach_no_provider() {
        let yaml = "runs:\n  steps:\n  - uses: o/r@../../x/y\n  - uses: o/r@./x\n  - uses: o/r@a/.b\n  - uses: o/s@v1\n";
        let repos = extract_unique_repo_ids(
            yaml,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        );
        assert_eq!(repos, [ActionRepoId::simple("o", "s")]);
        let lines: Vec<(u32, u32, u32)> = invalid_ref_errors(yaml)
            .into_iter()
            .map(|(line, start, end, _)| (line, start, end))
            .collect();
        assert_eq!(lines, [(2, 14, 23), (3, 14, 17), (4, 14, 18)]);
    }

    #[tokio::test]
    async fn invalid_refs_are_error_diagnostics() {
        let provider = test_utils::MockProvider::new("github.com");
        let registry = test_utils::mock_registry(provider);
        let yaml = "runs:\n  steps:\n  - uses: o/r@a~1\n  - uses: o/r@v1.lock\n  - uses: o/r\n  - uses: o/r@\n  - uses: ./local\n";
        let result = compute_diagnostics(
            yaml,
            &Uri::from_file_path("/test/ci.yml").unwrap(),
            &Config::default(),
            &registry,
            &crate::file_cache::FileCache::disabled(),
            crate::position::PositionEncoding::Utf16,
        )
        .await;
        let found: Vec<(u32, u32, u32, Option<DiagnosticSeverity>, &str)> = result
            .diagnostics
            .iter()
            .map(|d| {
                (
                    d.range.start.line,
                    d.range.start.character,
                    d.range.end.character,
                    d.severity,
                    d.message.as_str(),
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                (
                    2,
                    14,
                    17,
                    Some(DiagnosticSeverity::ERROR),
                    "o/r@a~1: ref `a~1` holds the character '~'"
                ),
                (
                    3,
                    14,
                    21,
                    Some(DiagnosticSeverity::ERROR),
                    "o/r@v1.lock: ref component `v1.lock` starts with `.` or ends with `.lock`"
                ),
                (
                    4,
                    0,
                    13,
                    Some(DiagnosticSeverity::ERROR),
                    "o/r: missing '@' separator for version reference"
                ),
                (
                    5,
                    14,
                    14,
                    Some(DiagnosticSeverity::ERROR),
                    "o/r@: empty version reference after '@'"
                ),
            ]
        );
        assert_eq!(result.refs_checked, 4);
        assert!(!result.errors.has_errors());
    }

    #[test]
    fn extract_located_actions_handles_subpath_refs() {
        let yaml = "      - uses: owner/repo/.github/workflows/reusable.yml@v1\n";
        let actions = extract_located_actions(
            yaml,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        );
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_ref.owner(), "owner");
        assert_eq!(actions[0].action_ref.repo(), "repo");
        assert_eq!(actions[0].action_ref.ref_str(), "v1");
    }

    #[test]
    fn extract_located_actions_empty_yaml_returns_empty() {
        let yaml = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n";
        let actions = extract_located_actions(
            yaml,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn extract_located_actions_multiple_steps_different_repos() {
        let yaml = r"
jobs:
  build:
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v3
      - uses: docker/build-push-action@v5
      - uses: ./local
";
        let actions = extract_located_actions(
            yaml,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        );
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].action_ref.repo(), "checkout");
        assert_eq!(actions[1].action_ref.repo(), "setup-node");
        assert_eq!(actions[2].action_ref.repo(), "build-push-action");
    }

    #[test]
    fn extract_preserves_uses_value_string() {
        let yaml = "      - uses: actions/checkout@v4.1.2\n";
        let actions = extract_located_actions(
            yaml,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        );
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].uses_value, "actions/checkout@v4.1.2");
    }

    // --- DiagnosticData serde edge cases ---

    #[test]
    fn diagnostic_data_unresolvable_none_roundtrips() {
        let data = DiagnosticData::Unresolvable {
            suggested_ref: None,
            repin_comment: None,
            replace_end_col: None,
        };
        let json = serde_json::to_value(&data).unwrap();
        let back: DiagnosticData = serde_json::from_value(json).unwrap();
        match back {
            DiagnosticData::Unresolvable { suggested_ref, .. } => assert!(suggested_ref.is_none()),
            DiagnosticData::Outdated { .. }
            | DiagnosticData::ExpandSha { .. }
            | DiagnosticData::PinBranch { .. } => {
                panic!("wrong variant")
            }
        }
    }

    #[test]
    fn diagnostic_data_tagged_correctly() {
        let outdated = DiagnosticData::Outdated {
            target_ref: "v1".into(),
            repin_comment: None,
            replace_end_col: None,
        };
        let json = serde_json::to_value(&outdated).unwrap();
        assert_eq!(json["kind"], "outdated");

        let unresolvable = DiagnosticData::Unresolvable {
            suggested_ref: None,
            repin_comment: None,
            replace_end_col: None,
        };
        let json = serde_json::to_value(&unresolvable).unwrap();
        assert_eq!(json["kind"], "unresolvable");
    }

    // --- compute_diagnostics with mock provider ---

    async fn diagnostics_under(
        yaml: &str,
        versions: version::Set,
        policy: actup_core::config::Policy,
    ) -> Result<Vec<Diagnostic>, Box<dyn std::error::Error>> {
        let provider = test_utils::MockProvider::new("github.com").with_versions(
            "rust-lang",
            "crates-io-auth-action",
            versions,
        );
        let registry = test_utils::mock_registry(provider);
        let config = Config {
            policy,
            ..Default::default()
        };
        let uri = Uri::from_file_path("/test/ci.yml").ok_or("bad uri")?;
        Ok(compute_diagnostics(
            yaml,
            &uri,
            &config,
            &registry,
            &crate::file_cache::FileCache::disabled(),
            crate::position::PositionEncoding::Utf16,
        )
        .await
        .diagnostics)
    }

    fn v1_versions(v1_tag: Option<&str>, branch_head: Option<&str>) -> version::Set {
        let mut tags = vec![
            test_utils::make_version_info("v1.0.4", DUMMY_SHA_2),
            test_utils::make_version_info("v1.0.5", DUMMY_SHA),
        ];
        tags.extend(v1_tag.map(|sha| test_utils::make_version_info("v1", sha)));
        let mut versions = version::Set::build(
            tags,
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            None,
        );
        if let Some(head) = branch_head {
            versions.set_branches(std::collections::HashMap::from([(
                "v1".to_string(),
                head.to_string(),
            )]));
        }
        versions
    }

    #[tokio::test]
    async fn unresolvable_typo_offers_the_closest_tag() -> Result<(), Box<dyn std::error::Error>> {
        let versions = version::Set::build(
            vec![
                test_utils::make_version_info("v4.1.0", DUMMY_SHA_2),
                test_utils::make_version_info("v4.2.0", DUMMY_SHA),
            ],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        let yaml = "    - uses: rust-lang/crates-io-auth-action@v42.0\n";
        let diags =
            diagnostics_under(yaml, versions, actup_core::config::Policy::default()).await?;
        let [diag] = diags.as_slice() else {
            return Err(format!("expected one diagnostic, got {diags:?}").into());
        };
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diag.code,
            Some(NumberOrString::String("unresolvable".into()))
        );
        let data: DiagnosticData = serde_json::from_value(diag.data.clone().ok_or("no data")?)?;
        let DiagnosticData::Unresolvable { suggested_ref, .. } = data else {
            return Err(format!("expected unresolvable data, got {data:?}").into());
        };
        assert_eq!(suggested_ref.as_deref(), Some("v4.2.0"));
        let ref_start = yaml.find("v42.0").ok_or("ref")?;
        assert_eq!(usize::try_from(diag.range.start.character)?, ref_start);
        assert_eq!(
            usize::try_from(diag.range.end.character)?,
            ref_start + "v42.0".len()
        );
        Ok(())
    }

    #[tokio::test]
    async fn closest_tag_quickfix_follows_the_pin_style() -> Result<(), Box<dyn std::error::Error>>
    {
        use tower_lsp_server::ls_types::CodeActionOrCommand;

        let versions = version::Set::build(
            vec![
                test_utils::make_version_info("v4.1.0", DUMMY_SHA_2),
                test_utils::make_version_info("v4.2.0", DUMMY_SHA),
            ],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        let policy = actup_core::config::Policy {
            pin: actup_core::policy::Style::Sha,
            ..Default::default()
        };
        let yaml = "    - uses: rust-lang/crates-io-auth-action@v42.0 # v4\n";
        let diags = diagnostics_under(yaml, versions, policy).await?;
        let [diag] = diags.as_slice() else {
            return Err(format!("expected one diagnostic, got {diags:?}").into());
        };
        let data: DiagnosticData = serde_json::from_value(diag.data.clone().ok_or("no data")?)?;
        let DiagnosticData::Unresolvable {
            suggested_ref,
            repin_comment,
            replace_end_col,
        } = data
        else {
            return Err(format!("expected unresolvable data, got {data:?}").into());
        };
        assert_eq!(suggested_ref.as_deref(), Some(DUMMY_SHA));
        assert_eq!(repin_comment.as_deref(), Some(" # v4.2.0"));
        assert_eq!(replace_end_col, Some(u32::try_from(yaml.trim_end().len())?));

        let uri = Uri::from_file_path("/test/ci.yml").ok_or("bad uri")?;
        let actions =
            crate::code_actions::code_actions_for_diagnostics(&uri, Some(1), &diags, true);
        let [CodeActionOrCommand::CodeAction(action)] = actions.as_slice() else {
            return Err(format!("expected one action, got {actions:?}").into());
        };
        assert_eq!(action.title, format!("Change to {}", &DUMMY_SHA[..7]));
        let edit = serde_json::to_string(&action.edit)?;
        assert!(edit.contains(&format!("{DUMMY_SHA} # v4.2.0")), "{edit}");
        Ok(())
    }

    async fn archived_codes(
        state: Result<Option<provider::RepoState>, CoreError>,
        config: &Config,
    ) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions(
                "actions",
                "create-release",
                test_utils::simple_version_set("v1.1.4", DUMMY_SHA),
            )
            .with_repo_state(state);
        let registry = test_utils::mock_registry(provider);
        let uri = Uri::from_file_path("/test/ci.yml").ok_or("bad uri")?;
        let yaml = "    - uses: actions/create-release@v1.0.0\n    - uses: actions/create-release/sub@v1.1.4\n";
        let result = compute_diagnostics(
            yaml,
            &uri,
            config,
            &registry,
            &crate::file_cache::FileCache::disabled(),
            crate::position::PositionEncoding::Utf16,
        )
        .await;
        Ok(result
            .diagnostics
            .into_iter()
            .filter(|d| d.code == Some(NumberOrString::String("archived".into())))
            .map(|d| {
                assert_eq!(d.severity, Some(DiagnosticSeverity::HINT));
                (d.range.start.line.to_string(), d.message)
            })
            .collect())
    }

    #[tokio::test]
    async fn archived_and_disabled_repositories_get_a_hint()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = Config::default();
        assert_eq!(
            archived_codes(Ok(Some(provider::RepoState::Archived)), &config).await?,
            [
                (
                    "0".to_string(),
                    "actions/create-release@v1.0.0: repository is archived".to_string()
                ),
                (
                    "1".to_string(),
                    "actions/create-release/sub@v1.1.4: repository is archived".to_string()
                ),
            ]
        );
        let disabled = archived_codes(Ok(Some(provider::RepoState::Disabled)), &config).await?;
        assert!(
            disabled
                .iter()
                .all(|(_, m)| m.ends_with("repository is disabled")),
            "{disabled:?}"
        );
        assert_eq!(disabled.len(), 2);
        for quiet in [
            Ok(Some(provider::RepoState::Active)),
            Ok(None),
            Err(CoreError::RateLimited {
                provider: "github".into(),
                retry_after: None,
            }),
        ] {
            assert_eq!(archived_codes(quiet, &config).await?, []);
        }
        Ok(())
    }

    #[tokio::test]
    async fn unserved_hosts_get_no_archived_lookup() -> Result<(), Box<dyn std::error::Error>> {
        let provider = test_utils::MockProvider::new("github.com")
            .with_repo_state(Ok(Some(provider::RepoState::Archived)));
        let registry = test_utils::mock_registry(provider);
        let uri = Uri::from_file_path("/test/ci.yml").ok_or("bad uri")?;
        let result = compute_diagnostics(
            "    - uses: ghe.corp.com/actions/create-release@v1\n",
            &uri,
            &Config::default(),
            &registry,
            &crate::file_cache::FileCache::disabled(),
            crate::position::PositionEncoding::Utf16,
        )
        .await;
        assert!(
            result
                .diagnostics
                .iter()
                .all(|d| d.code != Some(NumberOrString::String("archived".into()))),
            "{:?}",
            result.diagnostics
        );
        Ok(())
    }

    #[tokio::test]
    async fn archived_policy_ignore_silences_the_hint() -> Result<(), Box<dyn std::error::Error>> {
        let ignore = Config {
            policy: actup_core::config::Policy {
                archived: actup_core::policy::Archived::Ignore,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            archived_codes(Ok(Some(provider::RepoState::Archived)), &ignore).await?,
            []
        );
        let mut first_ref_ignored = Config::default();
        first_ref_ignored
            .overrides
            .push(actup_core::config::Override {
                pattern: "actions/create-release@v1.0.0".parse()?,
                policy: actup_core::config::PolicyOverride {
                    archived: Some(actup_core::policy::Archived::Ignore),
                    ..Default::default()
                },
            });
        let codes =
            archived_codes(Ok(Some(provider::RepoState::Archived)), &first_ref_ignored).await?;
        let lines: Vec<&str> = codes.iter().map(|(line, _)| line.as_str()).collect();
        assert_eq!(lines, ["1"]);
        let mut scan_ignored = Config::default();
        scan_ignored.scan.ignore = vec!["actions/create-release".parse()?];
        assert_eq!(
            archived_codes(Ok(Some(provider::RepoState::Archived)), &scan_ignored).await?,
            []
        );
        Ok(())
    }

    #[tokio::test]
    async fn branch_on_the_target_commit_gets_same_commit_info()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = actup_core::config::Policy {
            branch: actup_core::policy::Branch::Update,
            ..Default::default()
        };
        let yaml = "    - uses: rust-lang/crates-io-auth-action@v1\n";

        let same = diagnostics_under(yaml, v1_versions(None, Some(DUMMY_SHA)), policy).await?;
        let [diag] = same.as_slice() else {
            return Err(format!("expected one diagnostic, got {same:?}").into());
        };
        assert_eq!(diag.severity, Some(DiagnosticSeverity::INFORMATION));
        assert_eq!(
            diag.code,
            Some(NumberOrString::String("same-commit".into()))
        );
        assert_eq!(
            diag.message,
            format!(
                "rust-lang/crates-io-auth-action@v1: pin available, same commit: branch v1 = v1.0.5 ({})",
                &DUMMY_SHA[..7]
            )
        );

        let behind = diagnostics_under(yaml, v1_versions(None, Some(DUMMY_SHA_2)), policy).await?;
        let [diag] = behind.as_slice() else {
            return Err(format!("expected one diagnostic, got {behind:?}").into());
        };
        assert_eq!(diag.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(diag.code, Some(NumberOrString::String("pin-branch".into())));
        assert_eq!(
            diag.message,
            format!(
                "rust-lang/crates-io-auth-action@v1: pin branch 'v1' to {} # v1",
                &DUMMY_SHA_2[..7]
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn non_semver_tag_under_update_pins_the_tag() -> Result<(), Box<dyn std::error::Error>> {
        let policy = actup_core::config::Policy {
            branch: actup_core::policy::Branch::Update,
            ..Default::default()
        };
        let mut versions = v1_versions(None, None);
        versions.set_other_tags(std::collections::HashMap::from([(
            "nightly".to_string(),
            DUMMY_SHA_2.to_string(),
        )]));
        let yaml = "    - uses: rust-lang/crates-io-auth-action@nightly\n";
        let diags = diagnostics_under(yaml, versions, policy).await?;
        let [diag] = diags.as_slice() else {
            return Err(format!("expected one diagnostic, got {diags:?}").into());
        };
        assert_eq!(diag.code, Some(NumberOrString::String("pin-branch".into())));
        assert_eq!(
            diag.message,
            format!(
                "rust-lang/crates-io-auth-action@nightly: pin tag 'nightly' to {} # nightly",
                &DUMMY_SHA_2[..7]
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn unlisted_ref_names_no_kind() -> Result<(), Box<dyn std::error::Error>> {
        let yaml = "    - uses: rust-lang/crates-io-auth-action@stable\n";
        let cases = [
            (
                actup_core::policy::Branch::Update,
                "pin-branch",
                format!(
                    "rust-lang/crates-io-auth-action@stable: pin ref 'stable' to {} # stable",
                    &DUMMY_SHA[..7]
                ),
            ),
            (
                actup_core::policy::Branch::Warn,
                "floating",
                "rust-lang/crates-io-auth-action@stable: 'stable' is a floating ref, pin to a tag or SHA"
                    .to_string(),
            ),
        ];
        for (branch, code, message) in cases {
            let policy = actup_core::config::Policy {
                branch,
                ..Default::default()
            };
            let diags = diagnostics_under(yaml, v1_versions(None, None), policy).await?;
            let [diag] = diags.as_slice() else {
                return Err(format!("expected one diagnostic, got {diags:?}").into());
            };
            assert_eq!(diag.code, Some(NumberOrString::String(code.into())));
            assert_eq!(diag.message, message);
        }
        Ok(())
    }

    fn v1_alias_versions() -> version::Set {
        let major = test_utils::make_version_info("v1", DUMMY_SHA);
        version::Set::build(
            vec![
                Arc::clone(&major),
                test_utils::make_version_info("v1.0.5", DUMMY_SHA),
            ],
            std::collections::HashMap::new(),
            std::collections::HashMap::from([(1, major)]),
            None,
        )
    }

    #[tokio::test]
    async fn less_specific_target_on_the_same_commit_offers_no_pin()
    -> Result<(), Box<dyn std::error::Error>> {
        let policy = actup_core::config::Policy {
            track: actup_core::policy::Track::Major,
            ..Default::default()
        };
        let yaml = "    - uses: rust-lang/crates-io-auth-action@v1.0.5\n";
        let diags = diagnostics_under(yaml, v1_alias_versions(), policy).await?;
        let [diag] = diags.as_slice() else {
            return Err(format!("expected one diagnostic, got {diags:?}").into());
        };
        assert_eq!(
            diag.message,
            format!(
                "rust-lang/crates-io-auth-action@v1.0.5: same commit: tag v1.0.5 = v1 ({})",
                &DUMMY_SHA[..7]
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn tag_on_the_target_commit_keeps_its_quickfix() -> Result<(), Box<dyn std::error::Error>>
    {
        use tower_lsp_server::ls_types::CodeActionOrCommand;

        let policy = actup_core::config::Policy {
            track: actup_core::policy::Track::Exact,
            ..Default::default()
        };
        let yaml = "    - uses: rust-lang/crates-io-auth-action@v1\n";
        let diags = diagnostics_under(yaml, v1_versions(Some(DUMMY_SHA), None), policy).await?;
        let [diag] = diags.as_slice() else {
            return Err(format!("expected one diagnostic, got {diags:?}").into());
        };
        assert_eq!(diag.severity, Some(DiagnosticSeverity::INFORMATION));
        assert_eq!(
            diag.code,
            Some(NumberOrString::String("same-commit".into()))
        );
        assert!(diag.message.contains("tag v1 = v1.0.5"), "{}", diag.message);

        let uri = Uri::from_file_path("/test/ci.yml").ok_or("bad uri")?;
        let actions =
            crate::code_actions::code_actions_for_diagnostics(&uri, Some(1), &diags, true);
        let [CodeActionOrCommand::CodeAction(action)] = actions.as_slice() else {
            return Err(format!("expected one action, got {actions:?}").into());
        };
        assert_eq!(action.title, "Update to v1.0.5");

        let behind = diagnostics_under(yaml, v1_versions(Some(DUMMY_SHA_2), None), policy).await?;
        let [diag] = behind.as_slice() else {
            return Err(format!("expected one diagnostic, got {behind:?}").into());
        };
        assert_eq!(diag.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(diag.code, Some(NumberOrString::String("outdated".into())));
        Ok(())
    }

    #[tokio::test]
    async fn an_expired_entry_after_a_rate_limit_marks_outdated_diagnostics_stale()
    -> Result<(), Box<dyn std::error::Error>> {
        use actup_core::cache::Cache;

        let temp = tempfile::TempDir::new()?;
        let cache = crate::file_cache::FileCache::with_path(
            temp.path().to_path_buf(),
            std::time::Duration::ZERO,
        );
        let key = ActionRepoId::simple("actions", "checkout").cache_key("github", "github.com");
        cache
            .set(
                &key,
                actup_core::cache::Entry::new(test_utils::simple_version_set("v4.2.0", DUMMY_SHA)),
            )
            .await?;
        let registry =
            test_utils::mock_registry(test_utils::MockProvider::new("github.com").with_error(
                "actions",
                "checkout",
                CoreError::RateLimited {
                    provider: "github".into(),
                    retry_after: Some(std::time::Duration::from_mins(12)),
                },
            ));
        let uri = Uri::from_file_path("/test/ci.yml").ok_or("bad uri")?;
        let config = Config::default();
        let diagnostics = |yaml: &'static str| {
            compute_diagnostics(
                yaml,
                &uri,
                &config,
                &registry,
                &cache,
                crate::position::PositionEncoding::Utf16,
            )
        };

        let outdated = diagnostics("    - uses: actions/checkout@v3\n").await;
        let [diag] = outdated.diagnostics.as_slice() else {
            return Err(format!("expected one diagnostic, got {:?}", outdated.diagnostics).into());
        };
        assert_eq!(diag.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(diag.code, Some(NumberOrString::String("outdated".into())));
        assert!(
            diag.message.starts_with(&format!(
                "actions/checkout@v3: v3 -> v4.2.0 ({}) (stale: fetched ",
                &DUMMY_SHA[..7]
            )),
            "{}",
            diag.message
        );
        assert!(
            diag.message.ends_with(" ago, rate limited, resets in 12m)"),
            "{}",
            diag.message
        );
        assert!(!outdated.errors.has_errors());

        let current = diagnostics("    - uses: actions/checkout@v4.2.0\n").await;
        assert_eq!(current.diagnostics, []);
        Ok(())
    }

    #[tokio::test]
    async fn compute_diagnostics_empty_text_returns_empty() {
        let provider = test_utils::MockProvider::new("github.com");
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        let result = compute_diagnostics(
            "name: CI\non: push\n",
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;
        assert_eq!(result.diagnostics, []);
        assert!(!result.errors.has_errors());
    }

    #[tokio::test]
    async fn compute_diagnostics_outdated_ref_produces_warning() {
        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        let yaml = "    - uses: actions/checkout@v3\n";
        let result = compute_diagnostics(
            yaml,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(
            result.diagnostics[0].severity,
            Some(DiagnosticSeverity::WARNING)
        );
        assert!(result.diagnostics[0].message.contains("v4.2.0"));
    }

    #[tokio::test]
    async fn compute_diagnostics_up_to_date_ref_no_diagnostic() {
        let versions = test_utils::simple_version_set("v4.0.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        let yaml = "    - uses: actions/checkout@v4.0.0\n";
        let result = compute_diagnostics(
            yaml,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        assert!(
            result.diagnostics.is_empty(),
            "up-to-date ref should produce no diagnostics, got: {:?}",
            result
                .diagnostics
                .iter()
                .map(|d| &d.message)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn compute_diagnostics_respects_branch_policy_for_v_major_ref() {
        let v2_0_4 = Arc::new(version::Info::new(
            version::Version::parse("v2.0.4"),
            DUMMY_SHA,
        ));
        let mut latest_per_major = std::collections::HashMap::new();
        latest_per_major.insert(2, Arc::clone(&v2_0_4));
        let versions = version::Set::build(
            vec![Arc::clone(&v2_0_4)],
            latest_per_major,
            std::collections::HashMap::new(),
            Some(v2_0_4),
        );
        let provider = test_utils::MockProvider::new("github.com").with_versions(
            "denoland",
            "setup-deno",
            versions,
        );
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config {
            policy: actup_core::config::Policy {
                branch: actup_core::policy::Branch::Ignore,
                ..Default::default()
            },
            ..Default::default()
        };
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        let yaml = "    - uses: denoland/setup-deno@v2\n";
        let result = compute_diagnostics(
            yaml,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        assert!(
            result.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            result
                .diagnostics
                .iter()
                .map(|diag| diag.message.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn bump_policy_limits_quickfix_target() -> Result<(), Box<dyn std::error::Error>> {
        use actup_core::policy::Update;
        use tower_lsp_server::ls_types::{CodeActionOrCommand, DocumentChanges, OneOf};

        let versions = version::Set::build(
            [
                ("v4.1.0", DUMMY_SHA),
                ("v4.1.3", DUMMY_SHA_2),
                ("v4.9.3", DUMMY_SHA_2),
                ("v5.0.0", DUMMY_SHA_2),
            ]
            .into_iter()
            .map(|(tag, sha)| test_utils::make_version_info(tag, sha))
            .collect(),
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            None,
        );
        let uri = Uri::from_file_path("/test/ci.yml").ok_or("bad uri")?;
        let yaml = "    - uses: actions/checkout@v4.1.0\n";
        for (bump, want) in [
            (Update::Patch, "v4.1.3"),
            (Update::Minor, "v4.9.3"),
            (Update::Major, "v5.0.0"),
            (Update::Latest, "v5.0.0"),
        ] {
            let provider = test_utils::MockProvider::new("github.com").with_versions(
                "actions",
                "checkout",
                versions.clone(),
            );
            let registry = test_utils::mock_registry(provider);
            let cache = crate::file_cache::FileCache::disabled();
            let config = Config {
                policy: actup_core::config::Policy {
                    bump,
                    ..Default::default()
                },
                ..Default::default()
            };
            let result = compute_diagnostics(
                yaml,
                &uri,
                &config,
                &registry,
                &cache,
                crate::position::PositionEncoding::Utf16,
            )
            .await;

            let actions = crate::code_actions::code_actions_for_diagnostics(
                &uri,
                Some(1),
                &result.diagnostics,
                true,
            );
            let [CodeActionOrCommand::CodeAction(action)] = actions.as_slice() else {
                return Err(format!("bump = {bump}: expected one action, got {actions:?}").into());
            };
            let Some(DocumentChanges::Edits(doc_edits)) = action
                .edit
                .as_ref()
                .and_then(|e| e.document_changes.as_ref())
            else {
                return Err(format!("bump = {bump}: expected document edits").into());
            };
            let [doc_edit] = doc_edits.as_slice() else {
                return Err(format!("bump = {bump}: expected one document edit").into());
            };
            let [OneOf::Left(edit)] = doc_edit.edits.as_slice() else {
                return Err(format!("bump = {bump}: expected one text edit").into());
            };
            assert_eq!(edit.new_text, want, "bump = {bump}");
            assert_eq!(action.title, format!("Update to {want}"), "bump = {bump}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn compute_diagnostics_provider_error_produces_env_diagnostic() {
        let provider = test_utils::MockProvider::new("github.com").with_error(
            "actions",
            "checkout",
            actup_core::Error::RateLimited {
                provider: "github".into(),
                retry_after: None,
            },
        );
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        let yaml = "    - uses: actions/checkout@v4\n";
        let result = compute_diagnostics(
            yaml,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(
            result.diagnostics[0].severity,
            Some(DiagnosticSeverity::INFORMATION)
        );
        assert!(result.diagnostics[0].message.contains("check failed"));
        assert!(result.errors.has_errors());
    }

    #[tokio::test]
    async fn provider_error_diagnostic_carries_the_cause() -> Result<(), Box<dyn std::error::Error>>
    {
        let provider = test_utils::MockProvider::new("github.com").with_error(
            "actions",
            "checkout",
            actup_core::Error::Network {
                operation: "GET request".into(),
                message: String::new(),
                source: Some(Arc::new(std::io::Error::other("connection refused"))),
            },
        );
        let registry = test_utils::mock_registry(provider);
        let uri = Uri::from_file_path("/test/ci.yml").ok_or("bad uri")?;
        let result = compute_diagnostics(
            "    - uses: actions/checkout@v4\n",
            &uri,
            &Config::default(),
            &registry,
            &crate::file_cache::FileCache::disabled(),
            crate::position::PositionEncoding::Utf16,
        )
        .await;
        let [diag] = result.diagnostics.as_slice() else {
            return Err(format!("expected one diagnostic, got {:?}", result.diagnostics).into());
        };
        assert!(
            diag.message
                .contains("network error during GET request: connection refused"),
            "{}",
            diag.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn compute_diagnostics_drops_edits_for_value_not_written_verbatim()
    -> Result<(), Box<dyn std::error::Error>> {
        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").ok_or("bad uri")?;

        let yaml = "    - uses: \"actions/checkout@v\\x33\"\n    - uses: actions/checkout@v3\n";
        let result = compute_diagnostics(
            yaml,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        let [escaped, plain] = result.diagnostics.as_slice() else {
            panic!("expected two diagnostics: {:?}", result.diagnostics);
        };
        assert_eq!(escaped.code, plain.code);
        assert_eq!(escaped.range, test_utils::make_range(0, 0, 36));
        assert!(escaped.data.is_none());
        assert_eq!(plain.range, test_utils::make_range(1, 29, 31));
        assert!(plain.data.is_some());

        let actions = crate::code_actions::code_actions_for_diagnostics(
            &uri,
            Some(1),
            &result.diagnostics,
            true,
        );
        assert_eq!(actions.len(), 1);
        assert!(
            crate::code_actions::fix_all_in_file(&uri, Some(1), &result.diagnostics, true)
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn compute_diagnostics_deduplicates_repos() {
        // Same repo referenced twice — should only fetch versions once.
        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        let yaml = "    - uses: actions/checkout@v3\n    - uses: actions/checkout@v2\n";
        let result = compute_diagnostics(
            yaml,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        // Both refs should produce diagnostics (both outdated).
        assert_eq!(result.diagnostics.len(), 2);
    }

    #[tokio::test]
    async fn compute_diagnostics_mixed_repos() {
        let checkout_versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let node_versions = test_utils::simple_version_set("v4.0.0", DUMMY_SHA_2);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", checkout_versions)
            .with_versions("actions", "setup-node", node_versions);
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        let yaml = "    - uses: actions/checkout@v3\n    - uses: actions/setup-node@v4.0.0\n";
        let result = compute_diagnostics(
            yaml,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        // checkout@v3 is outdated (v4.2.0 available), setup-node@v4.0.0 is up to date.
        let warnings: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.severity == Some(DiagnosticSeverity::WARNING))
            .collect();
        assert_eq!(warnings.len(), 1, "only checkout should be outdated");
        assert!(warnings[0].message.contains("checkout"));
    }

    #[tokio::test]
    async fn compute_diagnostics_local_refs_not_checked() {
        let provider = test_utils::MockProvider::new("github.com");
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        let yaml = "    - uses: ./local-action\n    - uses: docker://node:16\n";
        let result = compute_diagnostics(
            yaml,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        assert_eq!(result.diagnostics, []);
    }

    // --- batch_fetch_versions ---

    #[tokio::test]
    async fn batch_fetch_versions_returns_results_for_all_repos() {
        let versions = test_utils::simple_version_set("v2.0.0", DUMMY_SHA);
        let provider =
            test_utils::MockProvider::new("github.com").with_versions("owner", "repo", versions);
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();

        let repo = ActionRepoId::simple("owner", "repo");
        let results =
            batch_fetch_versions(std::slice::from_ref(&repo), &config, &registry, &cache).await;

        assert!(results.contains_key(&repo));
        assert!(results[&repo].is_ok());
    }

    #[tokio::test]
    async fn batch_fetch_versions_error_propagated() {
        let provider = test_utils::MockProvider::new("github.com").with_error(
            "bad",
            "repo",
            actup_core::Error::NotFound {
                owner: "bad".into(),
                repo: "repo".into(),
                host: None,
            },
        );
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();

        let repo = ActionRepoId::simple("bad", "repo");
        let results =
            batch_fetch_versions(std::slice::from_ref(&repo), &config, &registry, &cache).await;

        assert!(results[&repo].is_err());
    }

    #[tokio::test]
    async fn batch_fetch_versions_caches_results() {
        let versions = test_utils::simple_version_set("v1.0.0", DUMMY_SHA);
        let provider =
            test_utils::MockProvider::new("github.com").with_versions("owner", "repo", versions);
        let registry = test_utils::mock_registry(provider);
        let config = Config::default();

        let temp = tempfile::TempDir::new().unwrap();
        let cache = crate::file_cache::FileCache::with_path(
            temp.path().to_path_buf(),
            std::time::Duration::from_hours(1),
        );

        let repo = ActionRepoId::simple("owner", "repo");
        let results =
            batch_fetch_versions(std::slice::from_ref(&repo), &config, &registry, &cache).await;
        assert!(results[&repo].is_ok());

        // Second fetch should hit file cache (even though provider would return
        // the same data, this tests the caching path works without error).
        let results2 =
            batch_fetch_versions(std::slice::from_ref(&repo), &config, &registry, &cache).await;
        assert!(results2[&repo].is_ok());
    }

    // --- SHA-pinned refs ---

    #[tokio::test]
    async fn compute_diagnostics_sha_pinned_produces_hint() {
        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        // Use a full 40-char SHA as the ref to trigger the Pinned code path.
        let yaml = format!("    - uses: actions/checkout@{DUMMY_SHA}\n");
        let result = compute_diagnostics(
            &yaml,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(
            result.diagnostics[0].severity,
            Some(DiagnosticSeverity::HINT)
        );
        assert_eq!(
            result.diagnostics[0].code,
            Some(NumberOrString::String("pinned".into()))
        );
        // Pinned to a known tag should show the tag in the message.
        assert!(
            result.diagnostics[0].message.contains("v4.2.0"),
            "expected tag in pinned message, got: {}",
            result.diagnostics[0].message
        );
    }

    async fn short_sha_diagnostics(line: &str, tag_sha: &str) -> Vec<Diagnostic> {
        short_sha_diagnostics_in("/test/ci.yml", &format!("{line}\n"), tag_sha).await
    }

    async fn short_sha_diagnostics_in(path: &str, text: &str, tag_sha: &str) -> Vec<Diagnostic> {
        let versions = test_utils::simple_version_set("v4.2.0", tag_sha);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        match diagnostics_at(provider, path, text).await {
            Ok(diagnostics) => diagnostics,
            Err(e) => panic!("diagnostics for {text:?}: {e}"),
        }
    }

    fn expand_data(diag: &Diagnostic) -> Option<DiagnosticData> {
        serde_json::from_value(diag.data.clone()?).ok()
    }

    #[tokio::test]
    async fn short_sha_comment_follows_the_file_format() {
        let comment = |diags: &[Diagnostic]| {
            let [diag] = diags else {
                panic!("expected one diagnostic, got {diags:?}");
            };
            let Some(DiagnosticData::ExpandSha {
                repin_comment,
                replace_end_col,
                ..
            }) = expand_data(diag)
            else {
                panic!("expected expand-sha data, got {diag:?}");
            };
            (diag.range.start.line, repin_comment, replace_end_col)
        };
        let md = "Prose uses: actions/checkout@abc1234\n```yaml\n- uses: actions/checkout@abc1234\n```\n";
        let diags = short_sha_diagnostics_in("/test/README.md", md, DUMMY_SHA).await;
        assert_eq!(comment(&diags), (2, Some(" # v4.2.0".to_owned()), None));

        let ts = "const step = { uses: \"actions/checkout@abc1234\" }; # v1\n";
        let diags = short_sha_diagnostics_in("/test/index.ts", ts, DUMMY_SHA).await;
        assert_eq!(comment(&diags), (0, None, None));
    }

    #[test]
    fn located_actions_read_no_tail_on_lines_without_comments() {
        let sha = "a".repeat(40);
        let encoding = crate::position::PositionEncoding::Utf16;
        let text = extract_located_actions(
            &format!("run(\"uses: o/p@{sha} # v4\");\n"),
            SourceFormat::Text,
            encoding,
        );
        let [action] = text.as_slice() else {
            panic!("expected one action, got {}", text.len());
        };
        assert_eq!(
            (action.trailing(), action.tail.as_str()),
            (Trailing::Foreign, "")
        );
        let yaml = extract_located_actions(
            &format!("- uses: o/p@{sha} # v4\n"),
            SourceFormat::Yaml,
            encoding,
        );
        let [action] = yaml.as_slice() else {
            panic!("expected one action, got {}", yaml.len());
        };
        assert_eq!(
            (action.trailing(), action.tail.as_str()),
            (Trailing::Version { note: "" }, "# v4")
        );
    }

    #[test]
    fn located_actions_read_the_tail_after_the_closer() {
        let sha = "a".repeat(40);
        let yaml = format!(
            "- uses: \"o/p@{sha}\" # v4\n- uses: 'o/q@v1'\n- uses: o/r@v1\n- {{ uses: o/s@v1 }} # v1\n- [{{ uses: o/t@v1 }}, {{ uses: o/u@v1 }}]\n"
        );
        let actions = extract_located_actions(
            &yaml,
            SourceFormat::Yaml,
            crate::position::PositionEncoding::Utf16,
        );
        let located: Vec<(Trailing<'_>, &str, &str)> = actions
            .iter()
            .map(|a| (a.trailing(), a.closer.as_str(), a.tail.as_str()))
            .collect();
        let version = Trailing::Version { note: "" };
        assert_eq!(
            located,
            [
                (version, "\" ", "# v4"),
                (Trailing::None, "'", ""),
                (Trailing::None, "", ""),
                (version, " } ", "# v1"),
                (Trailing::Foreign, "", ""),
                (Trailing::Foreign, "", ""),
            ]
        );
    }

    #[test]
    fn uri_format_reads_the_path_extension() -> Result<(), Box<dyn std::error::Error>> {
        let format = |uri: &str| uri.parse::<Uri>().map(|uri| uri_format(&uri));
        assert_eq!(format("file:///repo/ci.yml")?, SourceFormat::Yaml);
        assert_eq!(format("file:///repo/README.md")?, SourceFormat::Markdown);
        assert_eq!(format("file:///repo/main.tf")?, SourceFormat::Text);
        assert_eq!(format("untitled:Untitled-1")?, SourceFormat::Yaml);
        Ok(())
    }

    #[tokio::test]
    async fn short_sha_diagnostic_expands_to_tagged_commit() {
        let diags = short_sha_diagnostics("    - uses: actions/checkout@abc1234", DUMMY_SHA).await;
        let [diag] = diags.as_slice() else {
            panic!("expected one diagnostic, got {diags:?}");
        };
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            diag.message,
            format!("actions/checkout@abc1234: {SHORT_SHA_REASON}")
        );
        let Some(DiagnosticData::ExpandSha {
            target_ref,
            repin_comment,
            replace_end_col,
        }) = expand_data(diag)
        else {
            panic!("expected expand-sha data, got {:?}", diag.data);
        };
        assert_eq!(target_ref, DUMMY_SHA);
        assert_eq!(repin_comment.as_deref(), Some(" # v4.2.0"));
        assert_eq!(replace_end_col, None);
    }

    #[tokio::test]
    async fn short_sha_diagnostic_refreshes_stale_version_comment() {
        let line = "    - uses: actions/checkout@abc1234 # v4.1.0";
        let diags = short_sha_diagnostics(line, DUMMY_SHA).await;
        let Some(Some(DiagnosticData::ExpandSha {
            repin_comment,
            replace_end_col,
            ..
        })) = diags.first().map(expand_data)
        else {
            panic!("expected expand-sha data, got {diags:?}");
        };
        assert_eq!(repin_comment.as_deref(), Some(" # v4.2.0"));
        assert_eq!(replace_end_col, u32::try_from(line.len()).ok());
    }

    #[tokio::test]
    async fn short_sha_diagnostic_on_untagged_commit_has_no_comment() {
        let diags =
            short_sha_diagnostics("    - uses: actions/checkout@abc1234", DUMMY_SHA_2).await;
        let Some(Some(DiagnosticData::ExpandSha {
            target_ref,
            repin_comment: None,
            replace_end_col: None,
        })) = diags.first().map(expand_data)
        else {
            panic!("expected comment-free expand-sha data, got {diags:?}");
        };
        assert_eq!(target_ref, DUMMY_SHA);
    }

    #[tokio::test]
    async fn hex_ref_naming_another_commit_gets_no_short_sha_diagnostic() {
        let diags = short_sha_diagnostics("    - uses: actions/checkout@deadbee", DUMMY_SHA).await;
        assert_eq!(diags, []);
    }

    fn complete_provider() -> test_utils::MockProvider {
        let mut versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        versions.set_listing(version::Listing::Complete);
        test_utils::MockProvider::new("github.com").with_versions("actions", "checkout", versions)
    }

    async fn diagnostics_from(
        provider: test_utils::MockProvider,
        yaml: &str,
    ) -> Result<Vec<Diagnostic>, Box<dyn std::error::Error>> {
        diagnostics_at(provider, "/test/ci.yml", yaml).await
    }

    async fn diagnostics_at(
        provider: test_utils::MockProvider,
        path: &str,
        text: &str,
    ) -> Result<Vec<Diagnostic>, Box<dyn std::error::Error>> {
        let registry = test_utils::mock_registry(provider);
        let uri = Uri::from_file_path(path).ok_or("bad uri")?;
        Ok(compute_diagnostics(
            text,
            &uri,
            &Config::default(),
            &registry,
            &crate::file_cache::FileCache::disabled(),
            crate::position::PositionEncoding::Utf16,
        )
        .await
        .diagnostics)
    }

    #[tokio::test]
    async fn short_sha_expands_only_to_the_commit_it_abbreviates()
    -> Result<(), Box<dyn std::error::Error>> {
        let missing = CoreError::RefNotFound {
            owner: "actions".to_string(),
            repo: "checkout".to_string(),
            ref_name: "fedcba9".to_string(),
            host: None,
        };
        for (ref_name, provider) in [
            ("fedcba9", complete_provider()),
            ("abc1234", complete_provider().with_resolve(Err(missing))),
        ] {
            let resolves = provider.resolve_count();
            let yaml = format!("    - uses: actions/checkout@{ref_name}\n");
            let diags = diagnostics_from(provider, &yaml).await?;
            let [diag] = diags.as_slice() else {
                return Err(format!("expected one diagnostic, got {diags:?}").into());
            };
            assert!(diag.message.ends_with(SHORT_SHA_REASON), "{}", diag.message);
            assert!(
                !matches!(expand_data(diag), Some(DiagnosticData::ExpandSha { .. })),
                "{ref_name}: {:?}",
                diag.data
            );
            assert_eq!(resolves.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn non_editable_short_sha_skips_the_expand_lookup()
    -> Result<(), Box<dyn std::error::Error>> {
        let provider = complete_provider();
        let resolves = provider.resolve_count();
        let yaml = "    - uses: \"actions/checkout@abc\\x31234\"\n";
        let diags = diagnostics_from(provider, yaml).await?;
        let [diag] = diags.as_slice() else {
            return Err(format!("expected one diagnostic, got {diags:?}").into());
        };
        assert_eq!(
            diag.message,
            format!("actions/checkout@abc1234: {SHORT_SHA_REASON}")
        );
        assert_eq!(diag.data, None);
        assert_eq!(resolves.load(std::sync::atomic::Ordering::SeqCst), 0);

        let provider = complete_provider();
        let resolves = provider.resolve_count();
        let diags = diagnostics_from(provider, "    - uses: actions/checkout@abc1234\n").await?;
        assert!(
            matches!(
                diags.first().and_then(expand_data),
                Some(DiagnosticData::ExpandSha { .. })
            ),
            "{diags:?}"
        );
        assert_eq!(resolves.load(std::sync::atomic::Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn compute_diagnostics_sha_pinned_unknown_tag() {
        // SHA that doesn't match any known tag.
        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        // Use DUMMY_SHA_2 which doesn't match any tag in the version set.
        let yaml = format!("    - uses: actions/checkout@{DUMMY_SHA_2}\n");
        let result = compute_diagnostics(
            &yaml,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(
            result.diagnostics[0].severity,
            Some(DiagnosticSeverity::HINT)
        );
        assert!(
            result.diagnostics[0].message.contains("unknown version"),
            "expected 'unknown version' for unmatched SHA, got: {}",
            result.diagnostics[0].message
        );
    }

    // --- Quiet mode ---

    #[tokio::test]
    async fn quiet_mode_no_uses_keyword() {
        let provider = test_utils::MockProvider::new("github.com");
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/readme.md").unwrap();

        let text = "# README\nThis is not a workflow file.\n";
        let result = compute_diagnostics(
            text,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;
        assert_eq!(result.diagnostics, []);
    }

    #[test]
    fn status_to_diagnostic_uptodate_returns_none() {
        let status = CheckStatus::UpToDate;
        assert!(status_to_diagnostic(&status, make_range(0, 0, 5), "o/r@v1").is_none());
    }

    #[test]
    fn status_to_diagnostic_ignored_returns_none() {
        let status = CheckStatus::Ignored;
        assert!(status_to_diagnostic(&status, make_range(0, 0, 5), "o/r@v1").is_none());
    }

    #[tokio::test]
    async fn compute_diagnostics_counts_refs_checked() {
        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        let yaml = "    - uses: actions/checkout@v3\n    - uses: actions/checkout@v4.2.0\n";
        let result = compute_diagnostics(
            yaml,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        // Both refs are checked even though one is up-to-date.
        assert_eq!(result.refs_checked, 2);
    }

    #[tokio::test]
    async fn batch_fetch_versions_handles_provider_not_found() {
        let provider = test_utils::MockProvider::new("github.com");
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();

        let repo = ActionRepoId::new(
            Some("gitlab.com".to_string()),
            "owner".to_string(),
            "repo".to_string(),
            None,
        );
        let results =
            batch_fetch_versions(std::slice::from_ref(&repo), &config, &registry, &cache).await;

        assert!(results.contains_key(&repo));
        assert!(results[&repo].is_err());
    }
}
