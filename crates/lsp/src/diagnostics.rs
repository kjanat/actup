//! Diagnostic extraction from `uses:` references.
//!
//! Parses workflow/action YAML, resolves each `uses:` reference against
//! configured providers, and maps [`CheckStatus`] to LSP [`Diagnostic`]s.

use std::collections::HashMap;

use actup_core::cache::Cache as _;
use actup_core::resolution::{CheckResult, CheckStatus};
use actup_core::{
    ActionRef, ActionRepoId, Config, Error as CoreError, check_action, extract_uses_from_yaml,
    parse_uses, provider,
};
use serde::{Deserialize, Serialize};
use tower_lsp_server::ls_types::{
    Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range, Uri,
};

use crate::error_ux::{self, DiagnosticErrors};
use crate::file_cache::FileCache;

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
        /// The ref string to replace with (e.g. "v2.0.0").
        target_ref: String,
        /// SHA of the target version, for "Update & pin to SHA" code actions.
        /// `None` for backward compatibility with older diagnostics.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_sha: Option<String>,
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

/// Per-action check result stored in [`DocumentState`](crate::backend::DocumentState)
/// for cursor-based code actions.
///
/// Unlike diagnostics (which only exist for Outdated/Unresolvable refs),
/// entries are stored for ALL `uses:` refs — including `UpToDate` and `Ignored`.
/// This enables cursor-based "Pin to SHA" code actions on refs that have
/// no diagnostic.
#[derive(Debug, Clone)]
pub struct ActionCheckEntry {
    /// Range of the ref portion (after `@`) in the document.
    pub ref_range: Range,
    /// Range of the full `uses:` value (e.g. `actions/checkout@v4`).
    ///
    /// Available for end-of-value context (e.g. detecting existing trailing
    /// comments before appending new ones).
    #[allow(dead_code)] // Reserved for future comment-aware edits
    pub value_range: Range,
    /// The original ref string (e.g. `v4`, `v4.2.0`, `main`).
    pub ref_str: String,
    /// Resolved SHA of the current ref, if available.
    pub resolved_sha: Option<String>,
}

/// Parsed action reference with its source location in the document.
struct LocatedAction {
    /// Parsed action reference (owner/repo@ref).
    action_ref: ActionRef,
    /// 0-indexed line in the document.
    line: u32,
    /// Column range of the ref portion (after `@`) in LSP encoding.
    /// Falls back to the full value range if no `@` is present.
    ref_col_start: u32,
    ref_col_end: u32,
    /// Column range of the full `uses:` value in LSP encoding.
    value_col_start: u32,
    value_col_end: u32,
    /// The original `uses:` value string (for diagnostic messages).
    uses_value: String,
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
                let raw_line = loc.raw_line();

                // Value range: full `uses:` value (e.g. "actions/checkout@v4").
                let vr = loc.value_range();
                let value_col_start = crate::position::byte_to_col(raw_line, vr.start, encoding);
                let value_col_end = crate::position::byte_to_col(raw_line, vr.end, encoding);

                // Ref range: just the ref after @; fall back to full value.
                let rr = loc.ref_range().unwrap_or_else(|| vr.clone());
                let ref_col_start = crate::position::byte_to_col(raw_line, rr.start, encoding);
                let ref_col_end = crate::position::byte_to_col(raw_line, rr.end, encoding);

                #[allow(clippy::cast_possible_truncation)]
                let line = (loc.line() - 1) as u32; // 1-indexed -> 0-indexed
                actions.push(LocatedAction {
                    action_ref,
                    line,
                    ref_col_start,
                    ref_col_end,
                    value_col_start,
                    value_col_end,
                    uses_value: loc.value().to_string(),
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

/// Fetch version sets for a batch of unique repos, using file cache first.
///
/// Returns a map from `ActionRepoId` to either a fetched `version::Set`
/// or an error. Repos found in cache are not re-fetched.
async fn batch_fetch_versions(
    repos: &[ActionRepoId],
    registry: &provider::Registry,
    file_cache: &FileCache,
) -> HashMap<ActionRepoId, Result<actup_core::version::Set, CoreError>> {
    let mut results: HashMap<ActionRepoId, Result<actup_core::version::Set, CoreError>> =
        HashMap::with_capacity(repos.len());

    // Phase 1: check file cache
    let mut uncached = Vec::new();
    for repo in repos {
        let provider = registry.find_for_repo(repo);
        let cache_key = provider.map(|p| {
            let default_host = registry.default_host().unwrap_or("github.com");
            repo.cache_key(p.cache_namespace(), default_host)
        });

        if let Some(key) = &cache_key
            && let Ok(entry) = file_cache.get(key).await
        {
            tracing::debug!(repo = %format!("{}/{}", repo.owner(), repo.repo()), "cache hit");
            results.insert(repo.clone(), Ok(entry.versions().clone()));
            continue;
        }

        uncached.push((repo.clone(), cache_key));
    }

    // Phase 2: fetch uncached from providers sequentially.
    // TODO: parallelize with semaphore-bounded concurrency (tokio::sync::Semaphore).
    for (repo, cache_key) in &uncached {
        let Some(provider) = registry.find_for_repo(repo) else {
            let host = repo.host().map_or_else(|| "(default)".into(), String::from);
            results.insert(repo.clone(), Err(CoreError::ProviderNotFound { host }));
            continue;
        };

        match provider.get_versions(repo).await {
            Ok(versions) => {
                // Write to file cache (best effort)
                if let Some(key) = cache_key {
                    let entry = actup_core::cache::Entry::new(versions.clone());
                    if let Err(e) = file_cache.set(key, entry).await {
                        tracing::debug!(error = %e, "failed to write cache");
                    }
                }
                results.insert(repo.clone(), Ok(versions));
            }
            Err(e) => {
                results.insert(repo.clone(), Err(e));
            }
        }
    }

    results
}

/// Map a [`CheckStatus`] to an LSP [`Diagnostic`].
///
/// Returns `None` for statuses that don't warrant a diagnostic
/// (`UpToDate`, `Ignored`).
fn status_to_diagnostic(
    status: &CheckStatus,
    range: Range,
    uses_value: &str,
) -> Option<Diagnostic> {
    match status {
        CheckStatus::UpToDate | CheckStatus::Ignored => None,

        CheckStatus::Outdated { current, target } => {
            let target_ref = target.version().to_string();
            let data = DiagnosticData::Outdated {
                target_ref: target_ref.clone(),
                target_sha: Some(target.sha().to_string()),
            };
            Some(Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::WARNING),
                code: Some(NumberOrString::String("outdated".into())),
                source: Some(DIAGNOSTIC_SOURCE.into()),
                message: format!(
                    "{uses_value}: {current} -> {target_ref} ({})",
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

        CheckStatus::Unresolvable {
            reason,
            is_environment_error,
        } => {
            let severity = if *is_environment_error {
                DiagnosticSeverity::INFORMATION
            } else {
                DiagnosticSeverity::ERROR
            };
            // Environment errors use "(check failed: reason)" to distinguish
            // from user errors which show the reason directly.
            let message = if *is_environment_error {
                error_ux::format_check_failed(uses_value, reason)
            } else {
                format!("{uses_value}: {reason}")
            };
            // TODO: attach `DiagnosticData::Unresolvable { suggested_ref }` via
            // `data:` once closest-tag suggestion logic is implemented (like
            // the Outdated branch does). Without it, the code action handler in
            // `code_actions.rs` cannot offer "Change to ..." quickfixes.
            Some(Diagnostic {
                range,
                severity: Some(severity),
                code: Some(NumberOrString::String("unresolvable".into())),
                source: Some(DIAGNOSTIC_SOURCE.into()),
                message,
                ..Default::default()
            })
        }
    }
}

/// Result of a diagnostic computation: diagnostics to publish plus any
/// environment errors that should trigger user notifications.
pub struct DiagnosticResult {
    /// LSP diagnostics to publish for the document.
    pub diagnostics: Vec<Diagnostic>,
    /// Per-action check entries for ALL `uses:` refs (not just diagnostic-producing ones).
    ///
    /// Used by cursor-based code actions (e.g. "Pin to SHA") that need to
    /// operate on `UpToDate` refs which have no diagnostic.
    pub entries: Vec<ActionCheckEntry>,
    /// Total number of `uses:` action refs checked (including up-to-date/ignored
    /// refs that don't produce diagnostics). Used for accurate progress reporting.
    pub refs_checked: usize,
    /// Environment errors encountered during version fetching.
    pub errors: DiagnosticErrors,
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
            entries: Vec::new(),
            refs_checked: 0,
            errors: DiagnosticErrors::default(),
        };
    }

    // Deduplicate repos for batch fetching.
    let unique_repos: Vec<ActionRepoId> = {
        let mut seen = std::collections::HashSet::new();
        actions
            .iter()
            .filter_map(|a| {
                let id = a.action_ref.to_repo_id();
                seen.insert(id.clone()).then_some(id)
            })
            .collect()
    };

    let versions_map = batch_fetch_versions(&unique_repos, registry, file_cache).await;

    let mut diagnostics = Vec::with_capacity(actions.len());
    let mut entries = Vec::with_capacity(actions.len());
    let mut errors = DiagnosticErrors::default();

    for action in &actions {
        let ref_range = Range {
            start: Position {
                line: action.line,
                character: action.ref_col_start,
            },
            end: Position {
                line: action.line,
                character: action.ref_col_end,
            },
        };
        let value_range = Range {
            start: Position {
                line: action.line,
                character: action.value_col_start,
            },
            end: Position {
                line: action.line,
                character: action.value_col_end,
            },
        };

        let repo_id = action.action_ref.to_repo_id();
        let result = match versions_map.get(&repo_id) {
            Some(Ok(versions)) => check_action(&action.action_ref, versions, config),
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
            None => {
                // Should not happen (all repos are in the map), but handle gracefully.
                CheckResult::new(
                    action.action_ref.clone(),
                    CheckStatus::Unresolvable {
                        reason: "internal: repo not in version map".into(),
                        is_environment_error: true,
                    },
                )
            }
        };

        entries.push(ActionCheckEntry {
            ref_range,
            value_range,
            ref_str: action.action_ref.ref_str().to_string(),
            resolved_sha: result.resolved_sha().map(String::from),
        });

        if let Some(diag) = status_to_diagnostic(result.status(), ref_range, &action.uses_value) {
            diagnostics.push(diag);
        }
    }

    DiagnosticResult {
        diagnostics,
        entries,
        refs_checked: actions.len(),
        errors,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use actup_core::version;
    use std::sync::Arc;

    use crate::test_utils::{self, DUMMY_SHA, DUMMY_SHA_2};

    fn make_range(line: u32, col_start: u32, col_end: u32) -> Range {
        test_utils::make_range(line, col_start, col_end)
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
            DiagnosticData::Outdated {
                target_ref,
                target_sha,
            } => {
                assert_eq!(target_ref, "v4.0.0");
                assert_eq!(target_sha.as_deref(), Some(DUMMY_SHA));
            }
            DiagnosticData::Unresolvable { .. } => panic!("expected Outdated variant"),
        }
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
        assert_eq!(actions[0].ref_col_start, 31);
        assert_eq!(actions[0].ref_col_end, 33);
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
    fn target_sha_omitted_when_none() {
        let data = DiagnosticData::Outdated {
            target_ref: "v2.0.0".into(),
            target_sha: None,
        };
        let json = serde_json::to_value(&data).unwrap();
        assert!(
            json.get("target_sha").is_none(),
            "None target_sha must be omitted"
        );
        assert_eq!(json["target_ref"], "v2.0.0");
    }

    #[test]
    fn target_sha_present_when_some() {
        let data = DiagnosticData::Outdated {
            target_ref: "v2.0.0".into(),
            target_sha: Some("abc123def456abc123def456abc123def456abcd".into()),
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(
            json["target_sha"],
            "abc123def456abc123def456abc123def456abcd"
        );
    }

    #[test]
    fn target_sha_backward_compat_missing_field() {
        // Old diagnostics without target_sha should deserialize with None
        let json = serde_json::json!({
            "kind": "outdated",
            "target_ref": "v3.0.0"
        });
        let data: DiagnosticData = serde_json::from_value(json).unwrap();
        match data {
            DiagnosticData::Outdated {
                target_ref,
                target_sha,
            } => {
                assert_eq!(target_ref, "v3.0.0");
                assert!(target_sha.is_none(), "missing field should default to None");
            }
            DiagnosticData::Unresolvable { .. } => panic!("wrong variant"),
        }
    }

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
            target_sha: None,
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

        let repo = ActionRepoId::simple("owner", "repo");
        let results = batch_fetch_versions(std::slice::from_ref(&repo), &registry, &cache).await;

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

        let repo = ActionRepoId::simple("bad", "repo");
        let results = batch_fetch_versions(std::slice::from_ref(&repo), &registry, &cache).await;

        assert!(results[&repo].is_err());
    }

    #[tokio::test]
    async fn batch_fetch_versions_caches_results() {
        let versions = test_utils::simple_version_set("v1.0.0", DUMMY_SHA);
        let provider =
            test_utils::MockProvider::new("github.com").with_versions("owner", "repo", versions);
        let registry = test_utils::mock_registry(provider);

        let temp = tempfile::TempDir::new().unwrap();
        let cache = crate::file_cache::FileCache::with_path(
            temp.path().to_path_buf(),
            std::time::Duration::from_secs(3600),
        );

        let repo = ActionRepoId::simple("owner", "repo");
        let results = batch_fetch_versions(std::slice::from_ref(&repo), &registry, &cache).await;
        assert!(results[&repo].is_ok());

        // Second fetch should hit file cache (even though provider would return
        // the same data, this tests the caching path works without error).
        let results2 = batch_fetch_versions(std::slice::from_ref(&repo), &registry, &cache).await;
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

        let repo = ActionRepoId::new(
            Some("gitlab.com".to_string()),
            "owner".to_string(),
            "repo".to_string(),
            None,
        );
        let results = batch_fetch_versions(std::slice::from_ref(&repo), &registry, &cache).await;

        assert!(results.contains_key(&repo));
        assert!(results[&repo].is_err());
    }

    // --- ActionCheckEntry population ---

    #[test]
    fn extract_located_actions_computes_value_range() {
        //                   14          30
        //                    |           |
        let yaml = "      - uses: actions/checkout@v4\n";
        let actions = extract_located_actions(yaml, crate::position::PositionEncoding::Utf16);
        assert_eq!(actions.len(), 1);
        // value = "actions/checkout@v4" starts at col 14, ends at col 33
        assert_eq!(actions[0].value_col_start, 14);
        assert_eq!(actions[0].value_col_end, 33);
        // ref = "v4" starts at col 31, ends at col 33
        assert_eq!(actions[0].ref_col_start, 31);
        assert_eq!(actions[0].ref_col_end, 33);
    }

    #[tokio::test]
    async fn entries_populated_for_all_refs_including_up_to_date() {
        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        // v3 is outdated, v4.2.0 is up-to-date — both should produce entries.
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

        assert_eq!(
            result.entries.len(),
            2,
            "entries for ALL refs, not just diagnostics"
        );
        assert_eq!(
            result.diagnostics.len(),
            1,
            "only outdated produces diagnostic"
        );
    }

    #[tokio::test]
    async fn entries_carry_ref_str() {
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

        assert_eq!(result.entries[0].ref_str, "v3");
        assert_eq!(result.entries[1].ref_str, "v4.2.0");
    }

    #[tokio::test]
    async fn entries_carry_resolved_sha_for_known_tags() {
        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        // v4.2.0 is an exact semver match — resolved_sha should be populated.
        let yaml = "    - uses: actions/checkout@v4.2.0\n";
        let result = compute_diagnostics(
            yaml,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        assert_eq!(result.entries.len(), 1);
        assert_eq!(
            result.entries[0].resolved_sha.as_deref(),
            Some(DUMMY_SHA),
            "exact semver match should resolve SHA",
        );
    }

    #[tokio::test]
    async fn entries_ref_range_and_value_range_differ() {
        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions);
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        let yaml = "    - uses: actions/checkout@v4.2.0\n";
        let result = compute_diagnostics(
            yaml,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        let entry = &result.entries[0];
        // value_range covers "actions/checkout@v4.2.0", ref_range covers "v4.2.0".
        assert!(
            entry.value_range.start.character < entry.ref_range.start.character,
            "value_range should start before ref_range",
        );
        assert_eq!(
            entry.value_range.end, entry.ref_range.end,
            "both ranges should end at the same column",
        );
    }

    #[tokio::test]
    async fn entries_count_matches_refs_checked() {
        let versions = test_utils::simple_version_set("v4.2.0", DUMMY_SHA);
        let node_versions = test_utils::simple_version_set("v4.0.0", DUMMY_SHA_2);
        let provider = test_utils::MockProvider::new("github.com")
            .with_versions("actions", "checkout", versions)
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

        assert_eq!(result.entries.len(), result.refs_checked);
    }

    #[tokio::test]
    async fn entries_empty_for_no_uses_refs() {
        let provider = test_utils::MockProvider::new("github.com");
        let registry = test_utils::mock_registry(provider);
        let cache = crate::file_cache::FileCache::disabled();
        let config = Config::default();
        let uri = Uri::from_file_path("/test/ci.yml").unwrap();

        let yaml = "name: CI\non: push\n";
        let result = compute_diagnostics(
            yaml,
            &uri,
            &config,
            &registry,
            &cache,
            crate::position::PositionEncoding::Utf16,
        )
        .await;

        assert!(result.entries.is_empty());
    }

    #[tokio::test]
    async fn entries_none_sha_for_provider_error() {
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

        assert_eq!(result.entries.len(), 1);
        assert!(
            result.entries[0].resolved_sha.is_none(),
            "provider error should yield None resolved_sha",
        );
    }
}
