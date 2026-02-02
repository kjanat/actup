//! Check command - scan workflows and report outdated actions

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use itertools::Itertools;

use actup_core::{
    ActionRef, ActionRepoId, CheckStatus, Config, VersionSet, check_action as core_check_action,
};
use actup_provider_github::{BatchFetcher, GitHubProvider};
use tracing::debug;

use crate::cli::OutputFormat;
use crate::common::{ErrorTally, WorkflowScanner};
use crate::file_cache::FileCache;
use crate::output::{Finding, FindingStatus, determine_exit_code, print_findings, print_summary};

/// Convert core [`CheckStatus`] to CLI [`FindingStatus`].
///
/// When `versions` is provided, computes the exact version for major-only suggestions.
fn to_finding_status(status: CheckStatus, versions: Option<&VersionSet>) -> FindingStatus {
    match status {
        CheckStatus::UpToDate | CheckStatus::Ignored => FindingStatus::UpToDate,
        CheckStatus::Outdated { current, target } => {
            // If target is major-only (v6) but we have a full version (v6.0.2), include it
            let exact_version = if target.version.is_major_only() {
                versions
                    .and_then(|v| v.latest.as_ref())
                    .map(|l| l.version.to_string())
            } else {
                None
            };
            FindingStatus::Outdated {
                current,
                latest: target,
                exact_version,
            }
        }
        CheckStatus::Pinned { sha, tag } => FindingStatus::Pinned { sha, tag },
        CheckStatus::Unresolvable { reason } => FindingStatus::Unresolvable {
            reason,
            is_environment_error: false,
        },
    }
}

/// Check a single action using pre-fetched versions.
fn check_action_with_versions(
    action_ref: &ActionRef,
    file: &str,
    line: usize,
    versions: Option<&VersionSet>,
    config: &Config,
) -> Finding {
    let status = versions.map_or_else(
        || FindingStatus::Unresolvable {
            reason: "failed to fetch versions".to_string(),
            is_environment_error: true,
        },
        |v| {
            let result = core_check_action(action_ref, v, config);
            to_finding_status(result.status, Some(v))
        },
    );

    Finding {
        file: file.to_string(),
        line,
        action_ref: action_ref.clone(),
        status,
    }
}

/// Collected action for batch processing.
struct CollectedAction {
    file: String,
    line: usize,
    action_ref: ActionRef,
    repo_id: ActionRepoId,
}

pub async fn run(
    path: &Path,
    strict: bool,
    format: OutputFormat,
    config: &Config,
    registry: &actup_core::ProviderRegistry,
) -> i32 {
    let scanner = WorkflowScanner::new(path, config, registry, format);
    let discovered = scanner.discover();

    if discovered.is_empty() {
        scanner.print_empty_message();
        return 0;
    }

    let mut findings = Vec::new();
    let mut io_errors = ErrorTally::default();

    // Phase 1: Collect all actions and build unique repo set
    let mut collected_actions = Vec::new();

    for file in &discovered {
        let Some(content) = scanner.read_file(file, &mut io_errors) else {
            continue;
        };

        let relative = scanner.relative_path(file);

        for (loc, action_ref) in scanner.extract_actions(&content) {
            // Already pinned to SHA - handle immediately
            if action_ref.is_sha() {
                findings.push(Finding {
                    file: relative.clone(),
                    line: loc.line,
                    action_ref: action_ref.clone(),
                    status: FindingStatus::Pinned {
                        sha: action_ref.ref_str.clone(),
                        tag: None,
                    },
                });
                continue;
            }

            // No provider for host - handle immediately
            if scanner.provider_for(&action_ref).is_none() {
                findings.push(Finding {
                    file: relative.clone(),
                    line: loc.line,
                    action_ref: action_ref.clone(),
                    status: FindingStatus::Unresolvable {
                        reason: format!(
                            "no provider for host: {}",
                            action_ref.host.as_deref().unwrap_or("github.com")
                        ),
                        is_environment_error: false,
                    },
                });
                continue;
            }

            // Collect for batch fetching
            let repo_id = action_ref.to_repo_id();
            collected_actions.push(CollectedAction {
                file: relative.clone(),
                line: loc.line,
                action_ref,
                repo_id,
            });
        }
    }

    // Phase 2: Batch fetch versions for all unique repos
    let versions_map = if collected_actions.is_empty() {
        HashMap::new()
    } else {
        // Build unique repo list (deduplicated)
        let unique_repos: Vec<ActionRepoId> = collected_actions
            .iter()
            .map(|c| c.repo_id.clone())
            .unique()
            .collect();

        debug!(
            total_actions = collected_actions.len(),
            unique_repos = unique_repos.len(),
            "batch fetching versions"
        );

        batch_fetch_versions(&unique_repos, config).await
    };

    // Phase 3: Process collected actions using cached versions
    for action in collected_actions {
        let versions = versions_map.get(&action.repo_id);
        findings.push(check_action_with_versions(
            &action.action_ref,
            &action.file,
            action.line,
            versions,
            config,
        ));
    }

    print_findings(&findings, format);

    if matches!(format, OutputFormat::Human) {
        print_summary(&findings);
    }

    // I/O errors take priority (env error = exit 3)
    let io_exit = io_errors.exit_code();
    if io_exit != crate::cli::CliExitCode::Success {
        return io_exit.into();
    }

    determine_exit_code(&findings, strict)
}

/// Batch fetch versions for all repos using GraphQL (if token) or parallel REST.
async fn batch_fetch_versions(
    repos: &[ActionRepoId],
    config: &Config,
) -> HashMap<ActionRepoId, VersionSet> {
    // Create file cache based on config
    let cache = if config.cache.enabled {
        config.cache.path.as_ref().map_or_else(
            || FileCache::new(config.cache.ttl),
            |path| FileCache::with_path(path.clone(), config.cache.ttl),
        )
    } else {
        FileCache::disabled()
    };

    let cache = Arc::new(cache);

    // Create provider with optional token
    let token = std::env::var("GITHUB_TOKEN").ok();
    let provider = match GitHubProvider::new(token.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "failed to create GitHub provider");
            return HashMap::new();
        }
    };

    // Create batch fetcher with GraphQL support if token available
    let max_tags = config.providers.max_tags_per_repo.get();
    let fetcher = BatchFetcher::with_graphql(provider, cache, token.as_deref(), max_tags);

    if fetcher.has_graphql() {
        debug!("using GraphQL batch fetching");
    } else {
        debug!("using parallel REST fetching (no token or GraphQL unavailable)");
    }

    fetcher.fetch_versions(repos).await
}

// Tests for check command logic now live in actup-core::resolution module.
// The CLI-specific tests for output formatting are in output.rs.
