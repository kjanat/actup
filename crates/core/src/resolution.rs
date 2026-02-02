//! Canonical resolution logic for checking action versions
//!
//! This module provides the single source of truth for version resolution,
//! used by CLI, LSP, and MCP interfaces.

use crate::policy::BranchPolicy;
use crate::{ActionRef, Config, Error, ProviderRegistry, Version, VersionInfo, VersionSet};

/// Result of checking a single action reference
#[derive(Debug, Clone)]
pub struct CheckResult {
    /// The action reference that was checked
    pub action_ref: ActionRef,
    /// The check status
    pub status: CheckStatus,
}

/// Status of an action version check
#[derive(Debug, Clone)]
pub enum CheckStatus {
    /// Action is at the latest allowed version
    UpToDate,
    /// Action has a newer version available
    Outdated {
        /// Current version string
        current: String,
        /// Target version to update to
        target: VersionInfo,
    },
    /// Action is pinned to a SHA
    Pinned {
        /// The SHA it's pinned to
        sha: String,
        /// The tag this SHA corresponds to, if known
        tag: Option<String>,
    },
    /// Action is in the ignore list
    Ignored,
    /// Could not resolve version info
    Unresolvable {
        /// Reason for failure
        reason: String,
    },
}

/// Check if current version is up-to-date with target.
///
/// Handles major-only refs like "v4" matching any v4.x.x.
fn is_version_current(current: &Version, target: Option<&VersionInfo>, ref_str: &str) -> bool {
    match (current, target) {
        // For major-only refs (v4), compare major versions
        (
            Version::Semver {
                major,
                minor: None,
                patch: None,
                ..
            },
            Some(t),
        ) => t.version.major() == Some(*major),
        // For full semver, exact match
        (_, Some(t)) => t.version.to_string() == ref_str,
        (_, None) => false,
    }
}

/// Determine status when no target version is found.
fn status_for_no_target(current: &Version, branch_policy: BranchPolicy) -> CheckStatus {
    // Semver refs with no versions found
    if !current.is_branch() && current.is_semver() {
        return CheckStatus::Unresolvable {
            reason: "no versions found".into(),
        };
    }

    // Branch or other non-semver refs
    match branch_policy {
        BranchPolicy::Ignore => CheckStatus::UpToDate, // Treat as OK when ignoring
        BranchPolicy::Warn => CheckStatus::Unresolvable {
            reason: "non-semver ref (branch or tag)".into(),
        },
        BranchPolicy::Update => CheckStatus::Unresolvable {
            reason: "non-semver ref should be pinned to SHA".into(),
        },
    }
}

/// Check action against already-fetched versions.
///
/// This is the synchronous version for when you already have the `VersionSet`.
#[must_use]
pub fn check_action(action_ref: &ActionRef, versions: &VersionSet, config: &Config) -> CheckResult {
    // Ignored?
    if config.should_ignore(&action_ref.owner, &action_ref.repo) {
        return CheckResult {
            action_ref: action_ref.clone(),
            status: CheckStatus::Ignored,
        };
    }

    // Already pinned to SHA?
    if action_ref.is_sha() {
        let tag = versions
            .semver_tags
            .iter()
            .find(|v| v.sha == action_ref.ref_str)
            .map(|v| v.version.to_string());
        return CheckResult {
            action_ref: action_ref.clone(),
            status: CheckStatus::Pinned {
                sha: action_ref.ref_str.clone(),
                tag,
            },
        };
    }

    let current = Version::parse(&action_ref.ref_str);

    // Non-semver refs (branches like "stable", "main", "nightly") respect branch_policy
    if !current.is_semver() {
        return CheckResult {
            action_ref: action_ref.clone(),
            status: status_for_non_semver(&current, config.branch_policy, versions),
        };
    }

    // Find target version
    let policy = config.policy_for(&action_ref.owner, &action_ref.repo);
    let target = find_best_target(
        versions,
        &current,
        policy.allows_major(),
        config.allow_prerelease,
    );

    // Check if current is up-to-date (handles v4 == v4.1.7 via major comparison)
    if is_version_current(&current, target.as_ref(), &action_ref.ref_str) {
        return CheckResult {
            action_ref: action_ref.clone(),
            status: CheckStatus::UpToDate,
        };
    }

    target.map_or_else(
        || CheckResult {
            action_ref: action_ref.clone(),
            status: status_for_no_target(&current, config.branch_policy),
        },
        |t| CheckResult {
            action_ref: action_ref.clone(),
            status: CheckStatus::Outdated {
                current: action_ref.ref_str.clone(),
                target: t,
            },
        },
    )
}

/// Handle non-semver refs based on branch policy.
fn status_for_non_semver(
    current: &Version,
    branch_policy: BranchPolicy,
    versions: &VersionSet,
) -> CheckStatus {
    // TODO: Use `current` to provide smarter suggestions for non-semver refs,
    // e.g., comparing branch head commits or suggesting SHA pinning based on
    // the specific ref type (branch vs non-standard tag).
    let _ = current; // Silence unused warning until TODO is implemented
    match branch_policy {
        BranchPolicy::Ignore => CheckStatus::UpToDate,
        BranchPolicy::Warn => CheckStatus::Unresolvable {
            reason: "non-semver ref (branch or tag)".into(),
        },
        BranchPolicy::Update => {
            // Suggest pinning to latest version
            versions.latest.as_ref().map_or_else(
                || CheckStatus::Unresolvable {
                    reason: "non-semver ref should be pinned to SHA".into(),
                },
                |latest| CheckStatus::Outdated {
                    current: "non-semver".into(),
                    target: latest.clone(),
                },
            )
        }
    }
}

/// Find the best target version, respecting version style.
///
/// If user has major-only ref (v4) and major tag exists for target (v6),
/// return the major tag. Otherwise return full semver.
fn find_best_target(
    versions: &VersionSet,
    current: &Version,
    allow_major: bool,
    allow_prerelease: bool,
) -> Option<VersionInfo> {
    // First, find the latest version according to policy
    let latest = versions.find_latest_for(current, allow_major, allow_prerelease)?;

    // Check if user has major-only style (v4, v5, etc.)
    let user_has_major_only = matches!(
        current,
        Version::Semver {
            minor: None,
            patch: None,
            ..
        }
    );

    if user_has_major_only {
        // Try to find a major-only tag for the target's major version
        if let Some(target_major) = latest.version.major()
            && let Some(major_tag) = versions.major_tags.get(&target_major)
        {
            return Some(major_tag.clone());
        }
    }

    // Fall back to the full version
    Some(latest.clone())
}

/// Fetch versions and check for CLI/LSP/MCP use.
///
/// This is the async version that handles provider lookup and version fetching.
///
/// # Errors
/// Returns error if provider not found or version fetch fails.
pub async fn check_action_remote(
    action_ref: &ActionRef,
    config: &Config,
    registry: &ProviderRegistry,
) -> Result<CheckResult, Error> {
    // Performance optimization: check ignore list early to skip ProviderRegistry::find
    // and provider.get_versions() network calls. Note: check_action() repeats this guard
    // for correctness when called directly with pre-fetched versions.
    if config.should_ignore(&action_ref.owner, &action_ref.repo) {
        return Ok(CheckResult {
            action_ref: action_ref.clone(),
            status: CheckStatus::Ignored,
        });
    }

    let provider =
        registry
            .find(action_ref.host.as_deref())
            .ok_or_else(|| Error::ProviderNotFound {
                host: action_ref.host.clone().unwrap_or_default(),
            })?;

    let versions = provider.get_versions(&action_ref.to_repo_id()).await?;

    Ok(check_action(action_ref, &versions, config))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::policy::UpdatePolicy;
    use std::collections::HashMap;

    fn make_action_ref(owner: &str, repo: &str, ref_str: &str) -> ActionRef {
        ActionRef {
            host: None,
            owner: owner.to_string(),
            repo: repo.to_string(),
            subpath: None,
            ref_str: ref_str.to_string(),
        }
    }

    fn make_version_info(ver: &str, sha: &str) -> VersionInfo {
        VersionInfo {
            version: Version::parse(ver),
            sha: sha.to_string(),
        }
    }

    fn make_version_set(
        tags: Vec<VersionInfo>,
        latest: Option<VersionInfo>,
        latest_per_major: HashMap<u32, VersionInfo>,
    ) -> VersionSet {
        VersionSet {
            semver_tags: tags,
            latest_per_major,
            major_tags: HashMap::new(),
            latest,
            default_branch: None,
            fetched_at: None,
        }
    }

    #[test]
    fn test_check_action_up_to_date() {
        let action = make_action_ref("actions", "checkout", "v4.2.0");
        let v4_2 = make_version_info("v4.2.0", "abc123");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, v4_2.clone());
        let versions = make_version_set(vec![v4_2.clone()], Some(v4_2), latest_per_major);
        let config = Config::default();

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status, CheckStatus::UpToDate));
    }

    #[test]
    fn test_check_action_outdated() {
        let action = make_action_ref("actions", "checkout", "v4.0.0");
        let v4_0 = make_version_info("v4.0.0", "aaa");
        let v4_2 = make_version_info("v4.2.0", "bbb");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, v4_2.clone());
        let versions = make_version_set(vec![v4_0, v4_2.clone()], Some(v4_2), latest_per_major);
        let config = Config::default();

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status, CheckStatus::Outdated { .. }));
    }

    #[test]
    fn test_check_action_major_tag_up_to_date() {
        // "v4" should be up-to-date if latest v4.x.x exists
        let action = make_action_ref("actions", "checkout", "v4");
        let v4_2 = make_version_info("v4.2.0", "abc123");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, v4_2.clone());
        let versions = make_version_set(vec![v4_2.clone()], Some(v4_2), latest_per_major);
        let config = Config::default();

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status, CheckStatus::UpToDate));
    }

    #[test]
    fn test_check_action_ignored() {
        let action = make_action_ref("ignored", "action", "v1.0.0");
        let v2 = make_version_info("v2.0.0", "abc123");
        let versions = make_version_set(vec![v2.clone()], Some(v2), HashMap::new());
        let config = Config {
            ignore: vec!["ignored/action".to_string()],
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status, CheckStatus::Ignored));
    }

    #[test]
    fn test_check_action_pinned() {
        let sha = "abc123def456abc123def456abc123def456abcd";
        let action = make_action_ref("actions", "checkout", sha);
        let v4 = make_version_info("v4.0.0", sha);
        let versions = make_version_set(vec![v4.clone()], Some(v4), HashMap::new());
        let config = Config::default();

        let result = check_action(&action, &versions, &config);
        assert!(matches!(
            result.status,
            CheckStatus::Pinned { tag: Some(_), .. }
        ));
    }

    #[test]
    fn test_check_action_branch_ignored() {
        let action = make_action_ref("actions", "checkout", "main");
        let versions = make_version_set(vec![], None, HashMap::new());
        let config = Config {
            branch_policy: BranchPolicy::Ignore,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status, CheckStatus::UpToDate));
    }

    #[test]
    fn test_check_action_branch_warn() {
        let action = make_action_ref("actions", "checkout", "main");
        let versions = make_version_set(vec![], None, HashMap::new());
        let config = Config {
            branch_policy: BranchPolicy::Warn,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status, CheckStatus::Unresolvable { .. }));
    }

    #[test]
    fn test_check_action_with_policy_override() {
        let action = make_action_ref("actions", "checkout", "v3.0.0");
        let v3 = make_version_info("v3.0.0", "aaa");
        let v4 = make_version_info("v4.0.0", "bbb");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(3, v3.clone());
        latest_per_major.insert(4, v4.clone());
        let versions = make_version_set(vec![v3, v4.clone()], Some(v4), latest_per_major);

        // With minor policy (default), v3.0.0 is up-to-date (latest v3.x)
        let config = Config {
            update_policy: UpdatePolicy::Minor,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status, CheckStatus::UpToDate));
    }

    // Issue 1: Branch Policy for Non-Semver Refs (e.g., "stable", "nightly")

    #[test]
    fn test_check_action_non_semver_branch_policy_ignore() {
        // Non-semver ref like "stable" should be treated as UpToDate when branch_policy is ignore
        let action = make_action_ref("dtolnay", "rust-toolchain", "stable");
        let v1 = make_version_info("v1.0.0", "abc123");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(1, v1.clone());
        let versions = make_version_set(vec![v1.clone()], Some(v1), latest_per_major);

        let config = Config {
            branch_policy: BranchPolicy::Ignore,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        // With branch_policy: ignore, non-semver refs should NOT be marked outdated
        // They should be treated as "up to date" (skipped)
        assert!(
            matches!(result.status, CheckStatus::UpToDate),
            "Expected UpToDate for 'stable' with branch_policy: ignore, got {:?}",
            result.status
        );
    }

    #[test]
    fn test_check_action_non_semver_branch_policy_warn() {
        // Non-semver ref like "stable" should be Unresolvable with warning when branch_policy is warn
        let action = make_action_ref("dtolnay", "rust-toolchain", "stable");
        let v1 = make_version_info("v1.0.0", "abc123");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(1, v1.clone());
        let versions = make_version_set(vec![v1.clone()], Some(v1), latest_per_major);

        let config = Config {
            branch_policy: BranchPolicy::Warn,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        // With branch_policy: warn, non-semver refs should be flagged as unresolvable
        match &result.status {
            CheckStatus::Unresolvable { reason } => {
                assert!(
                    reason.contains("non-semver"),
                    "Expected reason to mention non-semver, got: {reason}"
                );
            }
            other => {
                panic!("Expected Unresolvable for 'stable' with branch_policy: warn, got {other:?}")
            }
        }
    }

    #[test]
    fn test_check_action_non_semver_branch_policy_update() {
        // Non-semver ref like "stable" should suggest pinning when branch_policy is update
        let action = make_action_ref("dtolnay", "rust-toolchain", "stable");
        let v1 = make_version_info("v1.0.0", "abc123");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(1, v1.clone());
        let versions = make_version_set(vec![v1.clone()], Some(v1), latest_per_major);

        let config = Config {
            branch_policy: BranchPolicy::Update,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        // With branch_policy: update, non-semver refs should be flagged for pinning
        // Current implementation returns Unresolvable - but ideally should return Outdated
        // with a target to pin to (either latest version or SHA)
        match &result.status {
            CheckStatus::Unresolvable { reason } => {
                assert!(
                    reason.contains("pin") || reason.contains("SHA"),
                    "Expected reason to mention pinning/SHA, got: {reason}"
                );
            }
            CheckStatus::Outdated { target, .. } => {
                // This is the preferred behavior - suggest updating to latest
                assert!(
                    target.version.is_semver(),
                    "Expected target to be a semver version"
                );
            }
            other => panic!(
                "Expected Unresolvable or Outdated for 'stable' with branch_policy: update, got {other:?}"
            ),
        }
    }

    // Issue 2: Version Style Preservation (major-only vs full semver)

    #[test]
    fn test_check_action_major_tag_suggests_major_tag() {
        // User has "v4" (major-only), latest is v6.0.2, and "v6" major tag exists
        // Should suggest "v6" not "v6.0.2"
        let action = make_action_ref("actions", "checkout", "v4");

        let v4_2 = make_version_info("v4.2.0", "sha4");
        let v6_0_2 = make_version_info("v6.0.2", "sha6full");
        // Simulate that v6 major tag exists by having it in semver_tags
        let v6_major = make_version_info("v6", "sha6major");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, v4_2.clone());
        latest_per_major.insert(6, v6_0_2.clone());

        // Include both v6.0.2 and v6 tags
        let mut versions = make_version_set(
            vec![v4_2, v6_0_2.clone(), v6_major.clone()],
            Some(v6_0_2),
            latest_per_major,
        );
        // Add major_tags field for the implementation to use
        // (This field may need to be added to VersionSet)
        versions.major_tags.insert(6, v6_major);

        let config = Config {
            update_policy: UpdatePolicy::Major,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        match &result.status {
            CheckStatus::Outdated { target, .. } => {
                // When user has major-only ref and major tag exists, suggest major tag
                let version = &target.version;
                assert_eq!(
                    version.to_string(),
                    "v6",
                    "Expected major-only suggestion 'v6', got '{version}'"
                );
            }
            other => panic!("Expected Outdated, got {other:?}"),
        }
    }

    #[test]
    fn test_check_action_major_tag_falls_back_to_full() {
        // User has "v4" (major-only), latest is v6.0.2, but NO "v6" major tag exists
        // Should fall back to "v6.0.2"
        let action = make_action_ref("actions", "checkout", "v4");

        let v4_2 = make_version_info("v4.2.0", "sha4");
        let v6_0_2 = make_version_info("v6.0.2", "sha6");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, v4_2.clone());
        latest_per_major.insert(6, v6_0_2.clone());

        // No v6 major tag - only full semver versions
        let versions = make_version_set(vec![v4_2, v6_0_2.clone()], Some(v6_0_2), latest_per_major);

        let config = Config {
            update_policy: UpdatePolicy::Major,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        match &result.status {
            CheckStatus::Outdated { target, .. } => {
                // When no major tag exists, fall back to full semver
                assert_eq!(
                    target.version.to_string(),
                    "v6.0.2",
                    "Expected full semver 'v6.0.2' when no major tag exists, got '{}'",
                    target.version
                );
            }
            other => panic!("Expected Outdated, got {other:?}"),
        }
    }

    #[test]
    fn test_check_action_full_semver_stays_full() {
        // User has "v4.1.0" (full semver), latest is v6.0.2
        // Should suggest full semver "v6.0.2" even if "v6" major tag exists
        let action = make_action_ref("actions", "checkout", "v4.1.0");

        let v4_1 = make_version_info("v4.1.0", "sha4");
        let v6_0_2 = make_version_info("v6.0.2", "sha6full");
        let v6_major = make_version_info("v6", "sha6major");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, v4_1.clone());
        latest_per_major.insert(6, v6_0_2.clone());

        let mut versions = make_version_set(
            vec![v4_1, v6_0_2.clone(), v6_major.clone()],
            Some(v6_0_2),
            latest_per_major,
        );
        versions.major_tags.insert(6, v6_major);

        let config = Config {
            update_policy: UpdatePolicy::Major,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        match &result.status {
            CheckStatus::Outdated { target, .. } => {
                // User has full semver, should get full semver back
                assert_eq!(
                    target.version.to_string(),
                    "v6.0.2",
                    "Expected full semver 'v6.0.2' for full semver input, got '{}'",
                    target.version
                );
            }
            other => panic!("Expected Outdated, got {other:?}"),
        }
    }
}
