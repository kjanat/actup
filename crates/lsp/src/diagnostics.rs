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
    },
    /// The ref is unresolvable — includes a suggested ref if available.
    #[serde(rename = "unresolvable")]
    Unresolvable {
        /// Optional suggested replacement (e.g. closest matching tag).
        suggested_ref: Option<String>,
    },
}

/// Parsed action reference with its source location in the document.
struct LocatedAction {
    /// Parsed action reference (owner/repo@ref).
    action_ref: ActionRef,
    /// 0-indexed line in the document.
    line: u32,
    /// Byte range of the ref portion (after `@`) within the line.
    /// Falls back to the full value range if no `@` is present.
    col_start: u32,
    col_end: u32,
    /// The original `uses:` value string (for diagnostic messages).
    uses_value: String,
}

/// Extract and parse all `uses:` references from document text.
///
/// Skips local (`./`) and docker (`docker://`) references, and logs
/// parse errors without propagating them (a malformed ref in one line
/// should not block diagnostics for other lines).
fn extract_located_actions(text: &str) -> Vec<LocatedAction> {
    let locations = extract_uses_from_yaml(text);
    let mut actions = Vec::with_capacity(locations.len());

    for loc in locations {
        match parse_uses(loc.value()) {
            Ok(Some(action_ref)) => {
                // ref_range gives byte range of just the ref after @; fall back to full value.
                // Truncation is safe: YAML lines are bounded well within u32::MAX.
                #[allow(clippy::cast_possible_truncation)]
                let (col_start, col_end) = loc.ref_range().map_or_else(
                    || {
                        let r = loc.value_range();
                        (r.start as u32, r.end as u32)
                    },
                    |r| (r.start as u32, r.end as u32),
                );

                #[allow(clippy::cast_possible_truncation)]
                let line = (loc.line() - 1) as u32; // 1-indexed -> 0-indexed
                actions.push(LocatedAction {
                    action_ref,
                    line,
                    col_start,
                    col_end,
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

    // Phase 2: fetch uncached from providers (sequentially to avoid overwhelming the API;
    // could be parallelized later with semaphore-bounded concurrency)
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
            Some(Diagnostic {
                range,
                severity: Some(severity),
                code: Some(NumberOrString::String("unresolvable".into())),
                source: Some(DIAGNOSTIC_SOURCE.into()),
                message: format!("{uses_value}: {reason}"),
                ..Default::default()
            })
        }
    }
}

/// Compute diagnostics for all `uses:` references in a document.
///
/// This is the main entry point called from `did_open` / `did_change`.
/// It extracts refs, deduplicates repos, fetches versions (with caching),
/// checks each ref against policy, and maps results to LSP diagnostics.
///
/// Network errors are surfaced as `Information`-severity diagnostics
/// rather than blocking the entire document.
pub async fn compute_diagnostics(
    text: &str,
    _uri: &Uri,
    config: &Config,
    registry: &provider::Registry,
    file_cache: &FileCache,
) -> Vec<Diagnostic> {
    let actions = extract_located_actions(text);
    if actions.is_empty() {
        return Vec::new();
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

        let repo_id = action.action_ref.to_repo_id();
        let result = match versions_map.get(&repo_id) {
            Some(Ok(versions)) => check_action(&action.action_ref, versions, config),
            Some(Err(e)) => CheckResult::new(
                action.action_ref.clone(),
                CheckStatus::Unresolvable {
                    reason: e.to_string(),
                    is_environment_error: e.is_environment_error(),
                },
            ),
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

        if let Some(diag) = status_to_diagnostic(result.status(), range, &action.uses_value) {
            diagnostics.push(diag);
        }
    }

    diagnostics
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use actup_core::version;
    use std::sync::Arc;

    /// Helper to build a range at (line, `col_start..col_end`).
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

    #[test]
    fn outdated_maps_to_warning() {
        let target = Arc::new(version::Info::new(
            version::Version::parse("v2.0.0"),
            "abc1234567890abcdef1234567890abcdef123456",
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
    fn pinned_maps_to_hint() {
        let status = CheckStatus::Pinned {
            sha: "abc1234567890abcdef1234567890abcdef123456".into(),
            tag: Some("v3.1.0".into()),
        };
        let diag = status_to_diagnostic(
            &status,
            make_range(5, 20, 60),
            "actions/checkout@abc1234567890abcdef1234567890abcdef123456",
        )
        .unwrap();
        assert_eq!(diag.severity, Some(DiagnosticSeverity::HINT));
        assert_eq!(diag.code, Some(NumberOrString::String("pinned".into())));
        assert!(diag.message.contains("v3.1.0"));
    }

    #[test]
    fn pinned_unknown_tag() {
        let status = CheckStatus::Pinned {
            sha: "abc1234567890abcdef1234567890abcdef123456".into(),
            tag: None,
        };
        let diag = status_to_diagnostic(
            &status,
            make_range(0, 0, 40),
            "owner/repo@abc1234567890abcdef1234567890abcdef123456",
        )
        .unwrap();
        assert!(diag.message.contains("unknown version"));
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
    fn unresolvable_env_error_maps_to_information() {
        let status = CheckStatus::Unresolvable {
            reason: "rate limited".into(),
            is_environment_error: true,
        };
        let diag =
            status_to_diagnostic(&status, make_range(1, 5, 10), "actions/setup-node@v4").unwrap();
        assert_eq!(diag.severity, Some(DiagnosticSeverity::INFORMATION));
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
        let actions = extract_located_actions(yaml);
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].action_ref.owner(), "actions");
        assert_eq!(actions[0].action_ref.repo(), "checkout");
        assert_eq!(actions[0].action_ref.ref_str(), "v4");
        assert_eq!(actions[1].action_ref.repo(), "setup-node");
        // Line numbers are 0-indexed
        assert_eq!(actions[0].line, 7); // "- uses: actions/checkout@v4" is line 8 (1-indexed)
        assert_eq!(actions[1].line, 8);
    }

    #[test]
    fn extract_located_actions_ref_range_points_after_at() {
        let yaml = "      - uses: actions/checkout@v4\n";
        let actions = extract_located_actions(yaml);
        assert_eq!(actions.len(), 1);
        // Value "actions/checkout@v4" starts at byte 14 (col 15, 1-indexed).
        // ref_range: at_pos=16 in value, start = 14 + 16 + 1 = 31, end = 14 + 19 = 33
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
        let actions = extract_located_actions(yaml);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action_ref.repo(), "checkout");
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
}
