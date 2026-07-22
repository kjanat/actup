//! Diagnostic extraction from `uses:` references.
//!
//! Parses workflow/action YAML, resolves each `uses:` reference against
//! configured providers, and maps [`CheckStatus`] to LSP [`Diagnostic`]s.

use std::collections::HashMap;

use actup_core::{
    ActionRef, ActionRepoId, Config, Error as CoreError, check_action_with_provider,
    extract_uses_from_yaml, parse_uses, provider,
    resolution::{CheckResult, CheckStatus},
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
    /// The ref is unresolvable — includes a suggested ref if available.
    // TODO: implement closest-tag suggestion for typos (e.g. "v4.0" → "v4.0.0")
    // and attach this variant via `data:` in `status_to_diagnostic` once populated.
    #[serde(rename = "unresolvable")]
    Unresolvable {
        /// Optional suggested replacement (e.g. closest matching tag).
        suggested_ref: Option<String>,
    },
}

/// What follows a `uses:` ref on its line, used to keep a SHA pin's version
/// comment accurate when re-pinning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrailingComment {
    /// Nothing meaningful after the ref.
    None,
    /// A version comment like `# v8.1.0` that should be refreshed.
    Version,
    /// A non-version comment that must be preserved.
    Foreign,
}

/// Whether a trailing fragment is a version comment like `# v1.2.3` or `# 1.2`.
fn is_version_comment(tail: &str) -> bool {
    let Some(body) = tail.strip_prefix('#') else {
        return false;
    };
    let body = body.trim();
    let body = body.strip_prefix('v').unwrap_or(body);
    body.starts_with(|c: char| c.is_ascii_digit())
}

/// Classify whatever follows the ref (byte offset `ref_end`) on `raw_line`.
fn classify_trailing(raw_line: &str, ref_end: usize) -> TrailingComment {
    let tail = raw_line.get(ref_end..).unwrap_or("").trim_start();
    if tail.is_empty() {
        TrailingComment::None
    } else if is_version_comment(tail) {
        TrailingComment::Version
    } else {
        TrailingComment::Foreign
    }
}

/// The replacement an outdated diagnostic should apply.
pub struct OutdatedEdit {
    pub target_ref: String,
    pub repin_comment: Option<String>,
    pub replace_end_col: Option<u32>,
}

/// Build the edit for re-pinning to `new_sha` (whose version is `version`),
/// keeping the version comment accurate: refresh a stale one, append a missing
/// one, preserve a foreign one.
pub fn sha_repin_edit(
    new_sha: &str,
    version: &str,
    trailing: TrailingComment,
    line_end_col: u32,
) -> OutdatedEdit {
    let target_ref = new_sha.to_string();
    let comment = format!(" # {version}");
    match trailing {
        TrailingComment::Version => OutdatedEdit {
            target_ref,
            repin_comment: Some(comment),
            replace_end_col: Some(line_end_col),
        },
        TrailingComment::None => OutdatedEdit {
            target_ref,
            repin_comment: Some(comment),
            replace_end_col: None,
        },
        TrailingComment::Foreign => OutdatedEdit {
            target_ref,
            repin_comment: None,
            replace_end_col: None,
        },
    }
}

/// Parsed action reference with its source location in the document.
pub struct LocatedAction {
    /// Parsed action reference (owner/repo@ref).
    pub action_ref: ActionRef,
    /// 0-indexed line in the document.
    pub line: u32,
    /// Byte range of the ref portion (after `@`) within the line.
    /// Falls back to the full value range if no `@` is present.
    pub col_start: u32,
    pub col_end: u32,
    /// The original `uses:` value string (for diagnostic messages).
    uses_value: String,
    /// What follows the ref on its line (only meaningful for SHA pins).
    pub trailing: TrailingComment,
    /// 0-indexed end-of-line column (negotiated encoding), used to overwrite a
    /// stale version comment when re-pinning.
    pub line_end_col: u32,
}

/// Find the `uses:` action whose ref range contains the cursor `position`.
///
/// Used by code actions to offer pin transformations on the ref under the
/// cursor regardless of whether it has a diagnostic.
pub fn locate_action_at(
    text: &str,
    position: tower_lsp_server::ls_types::Position,
    encoding: crate::position::PositionEncoding,
) -> Option<LocatedAction> {
    extract_located_actions(text, encoding)
        .into_iter()
        .find(|a| {
            a.line == position.line && (a.col_start..=a.col_end).contains(&position.character)
        })
}

/// Extract and parse all `uses:` references from document text.
///
/// Skips local (`./`) and docker (`docker://`) references, and logs
/// parse errors without propagating them (a malformed ref in one line
/// should not block diagnostics for other lines).
///
/// Column positions are converted from byte offsets to the negotiated
/// LSP position encoding.
fn extract_located_actions(
    text: &str,
    encoding: crate::position::PositionEncoding,
) -> Vec<LocatedAction> {
    let locations = extract_uses_from_yaml(text);
    let mut actions = Vec::with_capacity(locations.len());

    for loc in locations {
        match parse_uses(loc.value()) {
            Ok(Some(action_ref)) => {
                // ref_range gives byte range of just the ref after @; fall back to full value.
                // Convert byte offsets to the negotiated LSP encoding.
                let raw_line = loc.raw_line();
                let r = loc.ref_range().unwrap_or_else(|| loc.value_range());
                let col_start = crate::position::byte_to_col(raw_line, r.start, encoding);
                let col_end = crate::position::byte_to_col(raw_line, r.end, encoding);
                let line_end_col = crate::position::byte_to_col(raw_line, raw_line.len(), encoding);
                // Only SHA pins carry a `# vX.Y.Z` comment we maintain on re-pin.
                let trailing = if action_ref.is_sha() {
                    classify_trailing(raw_line, r.end)
                } else {
                    TrailingComment::None
                };

                #[allow(clippy::cast_possible_truncation)]
                let line = (loc.line() - 1) as u32; // 1-indexed -> 0-indexed
                actions.push(LocatedAction {
                    action_ref,
                    line,
                    col_start,
                    col_end,
                    uses_value: loc.value().to_string(),
                    trailing,
                    line_end_col,
                });
            }
            Ok(None) => {
                // Local or docker ref — not our concern.
            }
            Err(e) => {
                tracing::debug!(
                    value = loc.value(),
                    error = %e,
                    "skipping unparseable uses: value"
                );
            }
        }
    }

    actions
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
    encoding: crate::position::PositionEncoding,
) -> Vec<ActionRepoId> {
    unique_repo_ids(&extract_located_actions(text, encoding))
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
    )
    .await
}

async fn compute_diagnostics_for_actions(
    actions: Vec<LocatedAction>,
    config: &Config,
    registry: &provider::Registry,
    versions_map: &HashMap<ActionRepoId, Result<actup_core::version::Set, CoreError>>,
) -> DiagnosticResult {
    let mut diagnostics = Vec::with_capacity(actions.len());
    let mut errors = DiagnosticErrors::default();

    for action in &actions {
        let range = Range {
            start: Position {
                line: action.line,
                character: action.col_start,
            },
            end: Position {
                line: action.line,
                character: action.col_end,
            },
        };

        let repo_id = action.action_ref.to_repo_id().without_subpath();
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
                        reason: e.to_string(),
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

        // For an outdated SHA pin, build the comment-aware re-pin edit here where
        // the line's trailing comment is known.
        let edit = match result.status() {
            CheckStatus::Outdated { target, .. } if action.action_ref.is_sha() => {
                Some(sha_repin_edit(
                    target.sha(),
                    &target.version().to_string(),
                    action.trailing,
                    action.line_end_col,
                ))
            }
            _ => None,
        };

        if let Some(diag) = status_to_diagnostic(result.status(), range, &action.uses_value, edit) {
            diagnostics.push(diag);
        }
    }

    DiagnosticResult {
        diagnostics,
        refs_checked: actions.len(),
        errors,
        version_sets: successful_version_sets(versions_map),
    }
}

pub async fn compute_diagnostics_with_versions(
    text: &str,
    config: &Config,
    registry: &provider::Registry,
    versions_map: &HashMap<ActionRepoId, Result<actup_core::version::Set, CoreError>>,
    encoding: crate::position::PositionEncoding,
) -> DiagnosticResult {
    let actions = extract_located_actions(text, encoding);
    if actions.is_empty() {
        return DiagnosticResult {
            diagnostics: Vec::new(),
            refs_checked: 0,
            errors: DiagnosticErrors::default(),
            version_sets: HashMap::new(),
        };
    }

    compute_diagnostics_for_actions(actions, config, registry, versions_map).await
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
) -> Option<Diagnostic> {
    match status {
        CheckStatus::UpToDate | CheckStatus::Ignored => None,

        CheckStatus::Outdated { current, target } => {
            // SHA-pinned refs re-pin to the new commit SHA (never unpin to a
            // tag); tag refs update to the tag. `edit` (built by the caller,
            // which knows the line's trailing comment) carries any version-comment
            // refresh; fall back to a bare ref replacement for direct callers.
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
                "{uses_value}: '{ref_name}' is a floating branch ref, pin to a tag or SHA"
            ),
            ..Default::default()
        }),

        CheckStatus::PinBranch { ref_name, sha } => Some(Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            code: Some(NumberOrString::String("pin-branch".into())),
            source: Some(DIAGNOSTIC_SOURCE.into()),
            message: format!(
                "{uses_value}: pin branch '{ref_name}' to {} # {ref_name}",
                sha.get(..7).unwrap_or(sha)
            ),
            ..Default::default()
        }),

        CheckStatus::Unresolvable {
            reason,
            is_environment_error,
        } => Some(unresolvable_diagnostic(
            range,
            uses_value,
            reason,
            *is_environment_error,
        )),
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
    // TODO: attach `DiagnosticData::Unresolvable { suggested_ref }` via `data:`
    // once closest-tag suggestion logic is implemented (like the Outdated
    // branch does), so `code_actions.rs` can offer "Change to ..." quickfixes.
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
    _uri: &Uri,
    config: &Config,
    registry: &provider::Registry,
    file_cache: &FileCache,
    encoding: crate::position::PositionEncoding,
) -> DiagnosticResult {
    let actions = extract_located_actions(text, encoding);
    if actions.is_empty() {
        return DiagnosticResult {
            diagnostics: Vec::new(),
            refs_checked: 0,
            errors: DiagnosticErrors::default(),
            version_sets: HashMap::new(),
        };
    }

    let unique_repos = unique_repo_ids(&actions);

    let versions_map = batch_fetch_versions(&unique_repos, config, registry, file_cache).await;

    compute_diagnostics_for_actions(actions, config, registry, &versions_map).await
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
        super::status_to_diagnostic(status, range, uses_value, None)
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
            DiagnosticData::Unresolvable { .. } => panic!("expected Outdated variant"),
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
    fn sha_repin_refreshes_stale_version_comment() {
        // Stale `# v8.1.0` next to an outdated SHA must be overwritten, not kept.
        let new_sha = "fac544c07dec837d0ccb6301d7b5580bf5edae39";
        let edit = sha_repin_edit(new_sha, "v8.2.0", TrailingComment::Version, 70);
        assert_eq!(edit.target_ref, new_sha);
        assert_eq!(edit.repin_comment.as_deref(), Some(" # v8.2.0"));
        assert_eq!(edit.replace_end_col, Some(70));
    }

    #[test]
    fn sha_repin_appends_comment_when_absent() {
        let new_sha = "fac544c07dec837d0ccb6301d7b5580bf5edae39";
        let edit = sha_repin_edit(new_sha, "v8.2.0", TrailingComment::None, 70);
        assert_eq!(edit.repin_comment.as_deref(), Some(" # v8.2.0"));
        assert_eq!(edit.replace_end_col, None);
    }

    #[test]
    fn sha_repin_preserves_foreign_comment() {
        let new_sha = "fac544c07dec837d0ccb6301d7b5580bf5edae39";
        let edit = sha_repin_edit(new_sha, "v8.2.0", TrailingComment::Foreign, 70);
        assert_eq!(edit.target_ref, new_sha);
        assert_eq!(edit.repin_comment, None);
        assert_eq!(edit.replace_end_col, None);
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
        let actions = extract_located_actions(yaml, crate::position::PositionEncoding::Utf16);
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
        let actions = extract_located_actions(yaml, crate::position::PositionEncoding::Utf16);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].col_start, 31);
        assert_eq!(actions[0].col_end, 33);
    }

    #[test]
    fn extract_located_actions_skips_docker_and_local() {
        let yaml = r"
      - uses: docker://node:16
      - uses: ./my-action
      - uses: actions/checkout@v4
";
        let actions = extract_located_actions(yaml, crate::position::PositionEncoding::Utf16);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_ref.repo(), "checkout");
    }

    #[test]
    fn extract_located_actions_handles_subpath_refs() {
        let yaml = "      - uses: owner/repo/.github/workflows/reusable.yml@v1\n";
        let actions = extract_located_actions(yaml, crate::position::PositionEncoding::Utf16);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_ref.owner(), "owner");
        assert_eq!(actions[0].action_ref.repo(), "repo");
        assert_eq!(actions[0].action_ref.ref_str(), "v1");
    }

    #[test]
    fn extract_located_actions_empty_yaml_returns_empty() {
        let yaml = "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n";
        let actions = extract_located_actions(yaml, crate::position::PositionEncoding::Utf16);
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
        let actions = extract_located_actions(yaml, crate::position::PositionEncoding::Utf16);
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].action_ref.repo(), "checkout");
        assert_eq!(actions[1].action_ref.repo(), "setup-node");
        assert_eq!(actions[2].action_ref.repo(), "build-push-action");
    }

    #[test]
    fn extract_preserves_uses_value_string() {
        let yaml = "      - uses: actions/checkout@v4.1.2\n";
        let actions = extract_located_actions(yaml, crate::position::PositionEncoding::Utf16);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].uses_value, "actions/checkout@v4.1.2");
    }

    // --- DiagnosticData serde edge cases ---

    #[test]
    fn diagnostic_data_unresolvable_none_roundtrips() {
        let data = DiagnosticData::Unresolvable {
            suggested_ref: None,
        };
        let json = serde_json::to_value(&data).unwrap();
        let back: DiagnosticData = serde_json::from_value(json).unwrap();
        match back {
            DiagnosticData::Unresolvable { suggested_ref } => assert!(suggested_ref.is_none()),
            DiagnosticData::Outdated { .. } => panic!("wrong variant"),
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
        };
        let json = serde_json::to_value(&unresolvable).unwrap();
        assert_eq!(json["kind"], "unresolvable");
    }

    // --- compute_diagnostics with mock provider ---

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
        assert!(result.diagnostics.is_empty());
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
            branch_policy: actup_core::policy::Branch::Ignore,
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

        assert!(result.diagnostics.is_empty());
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
        assert!(result.diagnostics.is_empty());
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
