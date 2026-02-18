//! Check command - scan workflows and report outdated actions

use std::collections::HashMap;
use std::path::Path;

use itertools::Itertools;

use actup_core::{
    ActionRef, ActionRepoId, CheckStatus, Config, NO_VERSIONS_FOUND_REASON, cache::Cache,
    check_action as core_check_action, provider, version,
};
use tracing::debug;

use crate::cli::OutputFormat;
use crate::common::{ErrorTally, WorkflowScanner};
use crate::file_cache::FileCache;
use crate::output::{Finding, FindingStatus, determine_exit_code, print_findings, print_summary};

/// Convert core [`CheckStatus`] to CLI [`FindingStatus`].
///
/// When `versions` is provided, computes the exact version for major-only suggestions.
fn to_finding_status(status: CheckStatus, versions: Option<&version::Set>) -> FindingStatus {
    match status {
        CheckStatus::UpToDate => FindingStatus::UpToDate,
        CheckStatus::Ignored => FindingStatus::Ignored,
        CheckStatus::Outdated { current, target } => {
            // If target is major-only (v6) but we have a full version (v6.0.2), include it
            // Must use latest_per_major to get the correct major's latest, not global latest
            // Only set exact_version when it differs from target (avoids redundant "v6 (v6)")
            let exact_version = if target.version().is_major_only() {
                let target_str = target.version().to_string();
                target.version().major().and_then(|major| {
                    versions
                        .and_then(|v| v.latest_per_major().get(&major))
                        .map(|l| l.version().to_string())
                        .filter(|ev| ev != &target_str)
                })
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
        CheckStatus::Unresolvable {
            reason,
            is_environment_error,
        } => FindingStatus::Unresolvable {
            reason,
            is_environment_error,
        },
    }
}

/// Check a single action using pre-fetched versions.
///
/// When `versions_result` is `None` (no fetch attempted) or `Some(Err(...))` (fetch failed),
/// creates an Unresolvable finding with proper error classification.
fn check_action_with_versions(
    action_ref: &ActionRef,
    file: &str,
    line: usize,
    versions_result: Option<&Result<version::Set, actup_core::Error>>,
    config: &Config,
    provider: &str,
) -> Finding {
    let status = match versions_result {
        None => {
            // No fetch attempted (shouldn't happen in normal flow)
            FindingStatus::Unresolvable {
                reason: "no version fetch attempted".to_string(),
                is_environment_error: true,
            }
        }
        Some(Err(e)) => {
            // Fetch failed - use core's error classification
            FindingStatus::Unresolvable {
                reason: e.to_string(),
                is_environment_error: e.is_environment_error(),
            }
        }
        Some(Ok(versions)) => {
            // Success - check for updates
            let result = core_check_action(action_ref, versions, config);
            to_finding_status(result.status().clone(), Some(versions))
        }
    };

    Finding::new(
        file.to_string(),
        line,
        action_ref.clone(),
        status,
        provider.to_string(),
    )
}

/// Collected action for batch processing.
struct CollectedAction {
    file: String,
    line: usize,
    action_ref: ActionRef,
    repo_id: ActionRepoId,
    provider: String,
}

/// Scan workflows and report outdated actions across multiple providers.
#[allow(clippy::too_many_lines)]
pub async fn run(
    path: &Path,
    strict: bool,
    format: OutputFormat,
    config: &Config,
    registry: &provider::Registry,
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

        for (loc, action_ref) in WorkflowScanner::extract_actions(&content) {
            // No provider for host - handle immediately
            // This is an environment/configuration error, not a user error in the workflow
            let Some(provider) = scanner.provider_for(&action_ref) else {
                findings.push(Finding::new(
                    relative.clone(),
                    loc.line(),
                    action_ref.clone(),
                    FindingStatus::Unresolvable {
                        reason: format!(
                            "no provider for host: {}",
                            action_ref.host().unwrap_or("github.com")
                        ),
                        is_environment_error: true,
                    },
                    "unknown".to_string(),
                ));
                continue;
            };

            // Collect for batch fetching
            let repo_id = action_ref.to_repo_id();
            collected_actions.push(CollectedAction {
                file: relative.clone(),
                line: loc.line(),
                action_ref,
                repo_id,
                provider: provider.kind().to_string(),
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

        batch_fetch_versions(&unique_repos, config, registry).await
    };

    // Phase 3: Process collected actions using cached versions
    let mut unresolved_semver_actions = Vec::new();

    for action in collected_actions {
        let versions_result = versions_map.get(&action.repo_id);
        let finding = check_action_with_versions(
            &action.action_ref,
            &action.file,
            action.line,
            versions_result,
            config,
            &action.provider,
        );

        // Track semver refs that weren't found in batch fetch for fallback resolution
        if matches!(finding.status(), FindingStatus::Unresolvable { reason, .. }
                    if reason == NO_VERSIONS_FOUND_REASON)
        {
            unresolved_semver_actions.push((action, finding));
        } else {
            findings.push(finding);
        }
    }

    // Phase 4: Fallback resolution for semver refs not found in batch fetch
    // This handles cases like taiki-e/install-action@v2 where old version tags
    // are beyond the max_tags limit but still exist
    for (action, mut finding) in unresolved_semver_actions {
        let Some(provider) = registry.find_for_repo(&action.repo_id) else {
            findings.push(finding);
            continue;
        };

        // Try to resolve the ref directly - if it exists, mark as Unknown
        // since we can verify the ref is valid but can't determine if it's outdated
        if provider
            .resolve_ref_to_sha(&action.repo_id, action.action_ref.ref_str())
            .await
            .is_ok()
        {
            tracing::debug!(
                owner = %action.repo_id.owner(),
                repo = %action.repo_id.repo(),
                ref_str = %action.action_ref.ref_str(),
                provider = %action.provider,
                "fallback: ref exists but version comparison unavailable (likely beyond max_tags)"
            );
            finding = Finding::new(
                action.file,
                action.line,
                action.action_ref,
                FindingStatus::Unknown {
                    reason: "ref exists but version comparison unavailable".to_string(),
                },
                action.provider,
            );
        }
        findings.push(finding);
    }

    // Sort by file path then line number for stable, readable output.
    // Only no-provider cases produce immediate findings (see early-return handling above);
    // SHA→Pinned determination is performed later in `core_check_action` during Phase 3.
    // Sorting after all batching is complete ensures deterministic output order.
    findings.sort_by(|a, b| a.file().cmp(b.file()).then_with(|| a.line().cmp(&b.line())));

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

/// Batch fetch versions for all repos using the registry's providers.
///
/// Returns a map of repo IDs to either successful version sets or errors.
/// Uses parallel fetching with file-based caching for performance.
#[allow(clippy::too_many_lines)]
async fn batch_fetch_versions(
    repos: &[ActionRepoId],
    config: &Config,
    registry: &provider::Registry,
) -> HashMap<ActionRepoId, Result<version::Set, actup_core::Error>> {
    use actup_core::cache;

    // Create file cache based on config
    let cache = if config.cache.enabled {
        config.cache.path.as_ref().map_or_else(
            || FileCache::new(config.cache.ttl),
            |path| FileCache::with_path(path.clone(), config.cache.ttl),
        )
    } else {
        FileCache::disabled()
    };

    let mut results = HashMap::new();
    let mut uncached = Vec::new();

    // Check file cache first (concurrently)
    let cache_ref = &cache;
    let cache_futures: Vec<_> = repos
        .iter()
        .filter_map(|repo| {
            let provider = registry.find_for_repo(repo)?;
            let cache_key = repo.cache_key(provider.cache_namespace(), provider.host());
            Some((repo.clone(), cache_key))
        })
        .map(|(repo, cache_key)| async move {
            let cache_result = cache_ref.get(&cache_key).await;
            (repo, cache_result)
        })
        .collect();

    let cache_results = futures::future::join_all(cache_futures).await;

    for (repo, cache_result) in cache_results {
        match cache_result {
            Ok(entry) => {
                debug!(
                    owner = %repo.owner(),
                    repo = %repo.repo(),
                    "cache hit"
                );
                results.insert(repo, Ok(entry.versions().clone()));
            }
            Err(cache::Error::NotFound | cache::Error::Disabled) => {
                uncached.push(repo);
            }
            Err(e) => {
                debug!(
                    owner = %repo.owner(),
                    repo = %repo.repo(),
                    error = %e,
                    "cache read error, will fetch"
                );
                uncached.push(repo);
            }
        }
    }

    if uncached.is_empty() {
        debug!("all repos found in cache");
        return results;
    }

    debug!(
        count = uncached.len(),
        "fetching uncached repos via registry providers"
    );

    // Parallel fetch uncached repos using registry providers
    let fetch_futures: Vec<_> = uncached
        .iter()
        .filter_map(|repo| {
            registry.find_for_repo(repo).map(|provider| {
                let repo = repo.clone();
                let cache_ns = provider.cache_namespace().to_string();
                let host = provider.host().to_string();
                async move {
                    let result = provider.get_versions(&repo).await;
                    (repo, cache_ns, host, result)
                }
            })
        })
        .collect();

    let fetched = futures::future::join_all(fetch_futures).await;

    // Process results and update cache
    for (repo, cache_ns, host, result) in fetched {
        match result {
            Ok(versions) => {
                let cache_key = repo.cache_key(&cache_ns, &host);
                let entry = cache::Entry::new(versions.clone());
                if let Err(e) = cache.set(&cache_key, entry).await {
                    debug!(
                        owner = %repo.owner(),
                        repo = %repo.repo(),
                        error = %e,
                        "failed to cache versions"
                    );
                }
                results.insert(repo, Ok(versions));
            }
            Err(e) => {
                debug!(
                    owner = %repo.owner(),
                    repo = %repo.repo(),
                    error = %e,
                    "failed to fetch versions"
                );
                results.insert(repo, Err(e));
            }
        }
    }

    results
}

// Tests for check command logic now live in actup-core::resolution module.
// The CLI-specific tests for output formatting are in output.rs.

#[cfg(test)]
mod tests {
    use super::*;
    use actup_core::version;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn make_version_info(tag: &str, sha: &str) -> Arc<version::Info> {
        Arc::new(version::Info::new(version::Version::parse(tag), sha))
    }

    /// Test that `exact_version` uses major-scoped latest, not global latest.
    ///
    /// Scenario: user is on v4, target is v6 (major-only), global latest is v8.0.0.
    /// `exact_version` should be v6.0.2 (latest for major 6), NOT v8.0.0.
    #[test]
    fn test_exact_version_uses_major_scoped_latest() {
        // Build version::Set with multiple majors
        let v6_0_2 = make_version_info("v6.0.2", "aaa");
        let v8_0_0 = make_version_info("v8.0.0", "bbb");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(6, v6_0_2.clone());
        latest_per_major.insert(8, v8_0_0.clone());

        let versions = version::Set::build(
            vec![v6_0_2, v8_0_0.clone()],
            latest_per_major,
            HashMap::new(),
            Some(v8_0_0), // Global latest is v8.0.0
        );

        // Target is major-only v6 (suggesting upgrade from v4 to v6)
        // CheckStatus::Outdated has current: String, target: VersionInfo
        let status = CheckStatus::Outdated {
            current: "v4".to_string(),
            target: make_version_info("v6", "ccc"), // major-only tag
        };

        let finding_status = to_finding_status(status, Some(&versions));

        // Verify exact_version is v6.0.2 (major 6's latest), NOT v8.0.0 (global)
        match finding_status {
            FindingStatus::Outdated { exact_version, .. } => {
                assert_eq!(
                    exact_version,
                    Some("v6.0.2".to_string()),
                    "exact_version should be latest for major 6, not global latest"
                );
            }
            _ => panic!("expected Outdated status"),
        }
    }

    /// Test that `exact_version` is None for fully-qualified versions (not major-only).
    #[test]
    fn test_exact_version_none_for_full_version() {
        let v6_0_2 = make_version_info("v6.0.2", "aaa");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(6, v6_0_2.clone());

        let versions = version::Set::build(
            vec![v6_0_2.clone()],
            latest_per_major,
            HashMap::new(),
            Some(v6_0_2),
        );

        // Target is full version v6.0.2 (not major-only)
        let status = CheckStatus::Outdated {
            current: "v6.0.1".to_string(),
            target: make_version_info("v6.0.2", "aaa"), // fully qualified
        };

        let finding_status = to_finding_status(status, Some(&versions));

        match finding_status {
            FindingStatus::Outdated { exact_version, .. } => {
                assert_eq!(
                    exact_version, None,
                    "exact_version should be None for fully-qualified target"
                );
            }
            _ => panic!("expected Outdated status"),
        }
    }

    /// Test that `exact_version` is None when no `version::Set` is provided.
    #[test]
    fn test_exact_version_none_without_versions() {
        let status = CheckStatus::Outdated {
            current: "v4".to_string(),
            target: make_version_info("v6", "ccc"), // major-only tag
        };

        let finding_status = to_finding_status(status, None);

        match finding_status {
            FindingStatus::Outdated { exact_version, .. } => {
                assert_eq!(
                    exact_version, None,
                    "exact_version should be None when version::Set is not provided"
                );
            }
            _ => panic!("expected Outdated status"),
        }
    }

    /// Test that `exact_version` is None when major is not found in `latest_per_major`.
    #[test]
    fn test_exact_version_none_when_major_not_in_map() {
        let v8_0_0 = make_version_info("v8.0.0", "bbb");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(8, v8_0_0.clone());

        let versions = version::Set::build(
            vec![v8_0_0.clone()],
            latest_per_major,
            HashMap::new(),
            Some(v8_0_0),
        );

        // Target is major-only v6, but v6 is not in latest_per_major
        let status = CheckStatus::Outdated {
            current: "v4".to_string(),
            target: make_version_info("v6", "ccc"), // major-only tag
        };

        let finding_status = to_finding_status(status, Some(&versions));

        match finding_status {
            FindingStatus::Outdated { exact_version, .. } => {
                assert_eq!(
                    exact_version, None,
                    "exact_version should be None when target major is not in latest_per_major"
                );
            }
            _ => panic!("expected Outdated status"),
        }
    }

    /// Test conversion of `CheckStatus::UpToDate`.
    #[test]
    fn test_to_finding_status_up_to_date() {
        let status = CheckStatus::UpToDate;
        let finding_status = to_finding_status(status, None);
        assert!(matches!(finding_status, FindingStatus::UpToDate));
    }

    /// Test conversion of `CheckStatus::Ignored`.
    #[test]
    fn test_to_finding_status_ignored() {
        let status = CheckStatus::Ignored;
        let finding_status = to_finding_status(status, None);
        assert!(matches!(finding_status, FindingStatus::Ignored));
    }

    /// Test conversion of `CheckStatus::Pinned`.
    #[test]
    fn test_to_finding_status_pinned() {
        let status = CheckStatus::Pinned {
            sha: "abc123def456".to_string(),
            tag: Some("v4.0.0".to_string()),
        };
        let finding_status = to_finding_status(status, None);
        match finding_status {
            FindingStatus::Pinned { sha, tag } => {
                assert_eq!(sha, "abc123def456");
                assert_eq!(tag, Some("v4.0.0".to_string()));
            }
            _ => panic!("expected Pinned status"),
        }
    }

    /// Test conversion of `CheckStatus::Pinned` without tag.
    #[test]
    fn test_to_finding_status_pinned_no_tag() {
        let status = CheckStatus::Pinned {
            sha: "deadbeef".to_string(),
            tag: None,
        };
        let finding_status = to_finding_status(status, None);
        match finding_status {
            FindingStatus::Pinned { sha, tag } => {
                assert_eq!(sha, "deadbeef");
                assert_eq!(tag, None);
            }
            _ => panic!("expected Pinned status"),
        }
    }

    /// Test conversion of `CheckStatus::Unresolvable`.
    #[test]
    fn test_to_finding_status_unresolvable() {
        let status = CheckStatus::Unresolvable {
            reason: "not found".to_string(),
            is_environment_error: false,
        };
        let finding_status = to_finding_status(status, None);
        match finding_status {
            FindingStatus::Unresolvable {
                reason,
                is_environment_error,
            } => {
                assert_eq!(reason, "not found");
                assert!(!is_environment_error);
            }
            _ => panic!("expected Unresolvable status"),
        }
    }

    /// Test `check_action_with_versions` when versions are available.
    #[test]
    fn test_check_action_with_versions_success() {
        use actup_core::{ActionRef, policy};

        let action_ref = ActionRef::simple(
            "actions".to_string(),
            "checkout".to_string(),
            "v3".to_string(),
        );

        let v4_0_0 = make_version_info("v4.0.0", "abc123");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, v4_0_0.clone());

        let versions = version::Set::build(
            vec![v4_0_0.clone()],
            latest_per_major,
            HashMap::new(),
            Some(v4_0_0),
        );

        let config = Config {
            update_policy: policy::Update::Major,
            ..Default::default()
        };

        let finding = check_action_with_versions(
            &action_ref,
            "test.yml",
            10,
            Some(&Ok(versions)),
            &config,
            "github",
        );

        assert_eq!(finding.file(), "test.yml");
        assert_eq!(finding.line(), 10);
        assert_eq!(finding.action_ref().owner(), "actions");
        assert_eq!(finding.provider(), "github");
        assert!(matches!(finding.status(), FindingStatus::Outdated { .. }));
    }

    /// Test `check_action_with_versions` when versions are None (fetch failed).
    #[test]
    fn test_check_action_with_versions_no_versions() {
        use actup_core::ActionRef;

        let action_ref = ActionRef::simple(
            "actions".to_string(),
            "checkout".to_string(),
            "v4".to_string(),
        );

        let config = Config::default();

        let finding =
            check_action_with_versions(&action_ref, "test.yml", 5, None, &config, "github");

        assert_eq!(finding.file(), "test.yml");
        assert_eq!(finding.line(), 5);
        assert_eq!(finding.provider(), "github");
        match finding.status() {
            FindingStatus::Unresolvable {
                reason,
                is_environment_error,
            } => {
                assert_eq!(reason, "no version fetch attempted");
                assert!(is_environment_error);
            }
            _ => panic!("expected Unresolvable status with environment error"),
        }
    }

    /// Test `check_action_with_versions` propagates provider error message.
    #[test]
    fn test_check_action_with_versions_provider_error() {
        use actup_core::ActionRef;

        let action_ref = ActionRef::simple(
            "actions".to_string(),
            "checkout".to_string(),
            "v4".to_string(),
        );

        let config = Config::default();
        let auth_error = actup_core::Error::Auth {
            provider: "github".to_string(),
            reason: "authentication failed".to_string(),
        };

        let finding = check_action_with_versions(
            &action_ref,
            "test.yml",
            5,
            Some(&Err(auth_error)),
            &config,
            "github",
        );

        match finding.status() {
            FindingStatus::Unresolvable {
                reason,
                is_environment_error,
            } => {
                assert!(reason.contains("authentication failed"));
                assert!(is_environment_error);
            }
            _ => panic!("expected Unresolvable status with provider error"),
        }
    }

    /// Test that user errors (`NotFound`) are classified as `is_environment_error: false`.
    #[test]
    fn test_check_action_with_versions_user_error() {
        use actup_core::ActionRef;

        let action_ref = ActionRef::simple(
            "actions".to_string(),
            "nonexistent".to_string(),
            "v1".to_string(),
        );

        let config = Config::default();
        let not_found_error = actup_core::Error::NotFound {
            owner: "actions".to_string(),
            repo: "nonexistent".to_string(),
            host: Some("github.com".to_string()),
        };

        let finding = check_action_with_versions(
            &action_ref,
            "test.yml",
            10,
            Some(&Err(not_found_error)),
            &config,
            "github",
        );

        match finding.status() {
            FindingStatus::Unresolvable {
                reason,
                is_environment_error,
            } => {
                assert!(reason.contains("not found") || reason.contains("nonexistent"));
                assert!(!is_environment_error, "NotFound should be a user error");
            }
            _ => panic!("expected Unresolvable status with user error"),
        }
    }

    /// Test `exact_version` with minor-only version (v6.0 vs v6.0.2).
    #[test]
    fn test_exact_version_minor_only() {
        let v6_0_2 = make_version_info("v6.0.2", "aaa");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(6, v6_0_2.clone());

        let versions = version::Set::build(
            vec![v6_0_2.clone()],
            latest_per_major,
            HashMap::new(),
            Some(v6_0_2),
        );

        // Target is minor-only v6.0 (has minor, not major-only)
        let status = CheckStatus::Outdated {
            current: "v5.9.0".to_string(),
            target: make_version_info("v6.0", "bbb"), // minor-only
        };

        let finding_status = to_finding_status(status, Some(&versions));

        match finding_status {
            FindingStatus::Outdated { exact_version, .. } => {
                // Minor-only versions are not considered major-only, so exact_version should be None
                assert_eq!(
                    exact_version, None,
                    "exact_version should be None for minor-only target"
                );
            }
            _ => panic!("expected Outdated status"),
        }
    }

    /// Test `exact_version` is `None` when target version equals the exact version.
    ///
    /// Avoids redundant display like "v6 (v6)" - only show suffix when they differ.
    #[test]
    fn test_exact_version_equals_target() {
        let v6 = make_version_info("v6", "aaa");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(6, v6.clone());

        let versions =
            version::Set::build(vec![v6.clone()], latest_per_major, HashMap::new(), Some(v6));

        // Both target and exact are "v6" - no difference
        let status = CheckStatus::Outdated {
            current: "v5".to_string(),
            target: make_version_info("v6", "aaa"), // major-only tag
        };

        let finding_status = to_finding_status(status, Some(&versions));

        match finding_status {
            FindingStatus::Outdated { exact_version, .. } => {
                // When major-only tag v6 resolves to v6 (same), exact_version should be None
                // to avoid redundant display like "v6 (v6)"
                assert_eq!(exact_version, None);
            }
            _ => panic!("expected Outdated status"),
        }
    }

    /// Test [`CollectedAction`] structure (ensures proper collection).
    #[test]
    fn test_collected_action_structure() {
        use actup_core::ActionRef;

        let action_ref = ActionRef::simple(
            "actions".to_string(),
            "setup-node".to_string(),
            "v4".to_string(),
        );

        let repo_id = action_ref.to_repo_id();

        let collected = CollectedAction {
            file: "workflow.yml".to_string(),
            line: 15,
            action_ref: action_ref.clone(),
            repo_id,
            provider: "github".to_string(),
        };

        assert_eq!(collected.file, "workflow.yml");
        assert_eq!(collected.line, 15);
        assert_eq!(collected.action_ref.repo(), "setup-node");
        assert_eq!(collected.repo_id.owner(), action_ref.owner());
        assert_eq!(collected.repo_id.repo(), action_ref.repo());
    }

    /// Test `FindingStatus::Unknown` is correctly created for fallback resolution.
    #[test]
    fn test_finding_status_unknown() {
        use actup_core::ActionRef;

        let action_ref = ActionRef::simple(
            "taiki-e".to_string(),
            "install-action".to_string(),
            "v2".to_string(),
        );

        let finding = Finding::new(
            "workflow.yml".to_string(),
            10,
            action_ref,
            FindingStatus::Unknown {
                reason: "ref exists but version comparison unavailable".to_string(),
            },
            "github".to_string(),
        );

        match finding.status() {
            FindingStatus::Unknown { reason } => {
                assert!(reason.contains("version comparison unavailable"));
            }
            _ => panic!("expected Unknown status"),
        }
    }

    /// Test that `batch_fetch_versions` returns empty map for empty input.
    #[tokio::test]
    async fn test_batch_fetch_versions_empty_input() {
        use actup_core::provider;

        let config = Config::default();
        let registry = provider::Registry::new();

        let repos: Vec<ActionRepoId> = vec![];
        let results = batch_fetch_versions(&repos, &config, &registry).await;

        assert!(results.is_empty());
    }
}
