//! Canonical resolution logic for checking action versions
//!
//! This module provides the single source of truth for version resolution,
//! used by CLI, LSP, and MCP interfaces.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{action_ref::ActionRef, config::Config, error::Error, policy, provider, version};

/// Reason string for unresolvable status when no versions are found.
///
/// Used for semver refs where batch fetching found no versions,
/// but the ref might still exist (e.g., older tags beyond `max_tags` limit).
pub const NO_VERSIONS_FOUND_REASON: &str = "no versions found";

/// Result of checking a single action reference
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "tsify", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "tsify", tsify(into_wasm_abi, from_wasm_abi))]
pub struct CheckResult {
    /// The action reference that was checked
    action_ref: ActionRef,
    /// The check status
    status: CheckStatus,
}

impl CheckResult {
    /// Create a new check result
    #[must_use]
    pub const fn new(action_ref: ActionRef, status: CheckStatus) -> Self {
        Self { action_ref, status }
    }

    /// Get the action reference
    #[must_use]
    pub const fn action_ref(&self) -> &ActionRef {
        &self.action_ref
    }

    /// Get the check status
    #[must_use]
    pub const fn status(&self) -> &CheckStatus {
        &self.status
    }
}

/// Status of an action version check
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "tsify", derive(tsify_next::Tsify))]
pub enum CheckStatus {
    /// Action is at the latest allowed version
    UpToDate,
    /// Action has a newer version available
    Outdated {
        /// Current version string
        current: String,
        /// Target version to update to
        target: std::sync::Arc<version::Info>,
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
    /// Action references a floating branch (or non-semver tag) under `warn`
    /// branch policy: the ref resolves, but it moves, so it is advisory only.
    Floating {
        /// The branch or non-semver ref that floats
        ref_name: String,
    },
    /// Could not resolve version info
    Unresolvable {
        /// Reason for failure
        reason: String,
        /// Whether this is an environment error (network, auth) vs user error (invalid ref)
        is_environment_error: bool,
    },
}

/// Check if current version is up-to-date with target.
///
/// Handles major-only refs like "v4" matching any v4.x.x,
/// and minor-only refs like "v4.2" matching any v4.2.x.
fn is_version_current(current: &version::Version, target: Option<&Arc<version::Info>>) -> bool {
    match (current, target) {
        // For major-only refs (v4), compare major versions
        (
            version::Version::Semver {
                major,
                minor: None,
                patch: None,
                ..
            },
            Some(t),
        ) => t.version().major() == Some(*major),
        // For minor-only refs (v4.2), compare major and minor versions
        (
            version::Version::Semver {
                major,
                minor: Some(minor),
                patch: None,
                ..
            },
            Some(t),
        ) => t.version().major() == Some(*major) && t.version().minor() == Some(*minor),
        // For full semver, compare Version values directly
        (_, Some(t)) => t.version() == current,
        (_, None) => false,
    }
}

/// Determine status when no target version is found for a semver ref.
///
/// This is only called for semver refs (non-semver refs are handled earlier
/// by `status_for_non_semver`).
fn status_for_no_target(current: &version::Version) -> CheckStatus {
    debug_assert!(
        current.is_semver(),
        "status_for_no_target called with non-semver ref"
    );
    CheckStatus::Unresolvable {
        reason: NO_VERSIONS_FOUND_REASON.into(),
        is_environment_error: false,
    }
}

fn should_respect_branch_policy_for_major_ref(
    action_ref: &ActionRef,
    versions: &version::Set,
    status: &CheckStatus,
) -> bool {
    let current = version::Version::parse(action_ref.ref_str());
    let Some(current_major) = current.major() else {
        return false;
    };

    current.is_major_only()
        && action_ref.ref_str().starts_with('v')
        && !versions.has_exact_tag(action_ref.ref_str())
        && !versions.major_tags().contains_key(&current_major)
        && matches!(status, CheckStatus::Outdated { target, .. } if target.version().major() == Some(current_major))
}

/// Check action using pre-fetched versions and provider ref resolution.
///
/// Uses the provider only to disambiguate floating major refs like `v2` when
/// the fetched tag set contains only concrete tags like `v2.0.4`.
#[cfg_attr(
    target_arch = "wasm32",
    allow(
        clippy::future_not_send,
        reason = "wasm32 providers intentionally use non-Send boxed futures"
    )
)]
pub async fn check_action_with_provider(
    action_ref: &ActionRef,
    versions: &version::Set,
    config: &Config,
    provider: &dyn provider::Provider,
) -> CheckResult {
    let result = check_action(action_ref, versions, config);

    if should_respect_branch_policy_for_major_ref(action_ref, versions, result.status()) {
        let repo_id = action_ref.to_repo_id().without_subpath();
        return match provider
            .resolve_ref_to_sha(&repo_id, action_ref.ref_str())
            .await
        {
            Ok(_) => CheckResult::new(
                action_ref.clone(),
                status_for_non_semver(
                    &version::Version::Other {
                        tag: action_ref.ref_str().to_string(),
                    },
                    config.branch_policy,
                    versions,
                    config.allow_prerelease,
                ),
            ),
            Err(e) if e.is_environment_error() => CheckResult::new(
                action_ref.clone(),
                CheckStatus::Unresolvable {
                    reason: e.to_string(),
                    is_environment_error: true,
                },
            ),
            Err(_) => result,
        };
    }

    result
}

/// Check action against already-fetched versions.
///
/// This is the synchronous version for when you already have the [`version::Set`].
#[must_use]
pub fn check_action(
    action_ref: &ActionRef,
    versions: &version::Set,
    config: &Config,
) -> CheckResult {
    // Ignored?
    if config.should_ignore(action_ref.owner(), action_ref.repo(), action_ref.ref_str()) {
        return CheckResult::new(action_ref.clone(), CheckStatus::Ignored);
    }

    // Already pinned to SHA?
    if action_ref.is_sha() {
        return check_pinned_sha(action_ref, versions, config);
    }

    let current = version::Version::parse(action_ref.ref_str());

    // Non-semver refs (branches like "stable", "main", "nightly") respect branch_policy
    if !current.is_semver() {
        return CheckResult::new(
            action_ref.clone(),
            status_for_non_semver(
                &current,
                config.branch_policy,
                versions,
                config.allow_prerelease,
            ),
        );
    }

    // Find target version
    let policy = config.policy_for(action_ref.owner(), action_ref.repo());
    let target = find_best_target(
        versions,
        &current,
        policy.allows_major(),
        config.allow_prerelease,
    );

    // Check if current is up-to-date (handles v4 == v4.1.7 via major comparison)
    if is_version_current(&current, target.as_ref()) {
        // For major-only refs (like "v4" or "6"), verify the exact tag exists.
        // Prevents false "up to date" when user writes "6" but only "v6" exists.
        if current.is_major_only() && !versions.has_exact_tag(action_ref.ref_str()) {
            return CheckResult::new(
                action_ref.clone(),
                target.map_or_else(
                    || CheckStatus::Unresolvable {
                        reason: format!("tag '{}' not found", action_ref.ref_str()),
                        is_environment_error: false,
                    },
                    |t| CheckStatus::Outdated {
                        current: action_ref.ref_str().to_string(),
                        target: t,
                    },
                ),
            );
        }
        return CheckResult::new(action_ref.clone(), CheckStatus::UpToDate);
    }

    target.map_or_else(
        || CheckResult::new(action_ref.clone(), status_for_no_target(&current)),
        |t| {
            CheckResult::new(
                action_ref.clone(),
                CheckStatus::Outdated {
                    current: action_ref.ref_str().to_string(),
                    target: t,
                },
            )
        },
    )
}

/// Determine status for a SHA-pinned ref.
///
/// If the SHA matches a known tag's commit, the pinned version is compared
/// against the latest allowed version: a newer version yields `Outdated` whose
/// `target` carries the newer commit SHA (so updates re-pin rather than unpin to
/// a tag); otherwise it stays `Pinned`. A SHA that matches no known tag can't be
/// compared, so it also stays `Pinned`.
fn check_pinned_sha(
    action_ref: &ActionRef,
    versions: &version::Set,
    config: &Config,
) -> CheckResult {
    let sha = action_ref.ref_str().to_string();

    let Some(pinned) = versions
        .semver_tags()
        .iter()
        .find(|v| v.sha() == action_ref.ref_str())
        .cloned()
    else {
        return CheckResult::new(action_ref.clone(), CheckStatus::Pinned { sha, tag: None });
    };

    let policy = config.policy_for(action_ref.owner(), action_ref.repo());
    let target = find_best_target(
        versions,
        pinned.version(),
        policy.allows_major(),
        config.allow_prerelease,
    );

    match target {
        Some(t) if !is_version_current(pinned.version(), Some(&t)) => CheckResult::new(
            action_ref.clone(),
            CheckStatus::Outdated {
                current: pinned.version().to_string(),
                target: t,
            },
        ),
        _ => CheckResult::new(
            action_ref.clone(),
            CheckStatus::Pinned {
                sha,
                tag: Some(pinned.version().to_string()),
            },
        ),
    }
}

/// Handle non-semver refs based on branch policy.
fn status_for_non_semver(
    current: &version::Version,
    branch_policy: policy::Branch,
    versions: &version::Set,
    allow_prerelease: bool,
) -> CheckStatus {
    // TODO: Use `current` to provide smarter suggestions for non-semver refs,
    // e.g., comparing branch head commits or suggesting SHA pinning based on
    // the specific ref type (branch vs non-standard tag).
    match branch_policy {
        policy::Branch::Ignore => CheckStatus::UpToDate,
        policy::Branch::Warn => CheckStatus::Floating {
            ref_name: current.to_string(),
        },
        policy::Branch::Update => {
            // Suggest pinning to latest version, respecting prerelease config
            let latest = versions
                .latest()
                .filter(|v| allow_prerelease || !v.version().is_prerelease());
            latest.map_or_else(
                || CheckStatus::Unresolvable {
                    reason: "non-semver ref should be pinned to SHA".into(),
                    is_environment_error: false,
                },
                |l| CheckStatus::Outdated {
                    current: current.to_string(),
                    target: l.clone(),
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
    versions: &version::Set,
    current: &version::Version,
    allow_major: bool,
    allow_prerelease: bool,
) -> Option<Arc<version::Info>> {
    // First, find the latest version according to policy
    let latest = versions.find_latest_for(current, allow_major, allow_prerelease)?;

    // Check if user has major-only style (v4, v5, etc.)
    let user_has_major_only = current.is_major_only();

    if user_has_major_only {
        // Try to find a major-only tag for the target's major version
        if let Some(target_major) = latest.version().major()
            && let Some(major_tag) = versions.major_tags().get(&target_major)
        {
            return Some(Arc::clone(major_tag));
        }
    }

    // Fall back to the full version
    Some(Arc::clone(latest))
}

/// Fetch versions and check for CLI/LSP/MCP use.
///
/// This is the async version that handles provider lookup and version fetching.
///
/// # Errors
/// Returns error if provider not found or version fetch fails.
#[cfg_attr(
    target_arch = "wasm32",
    allow(
        clippy::future_not_send,
        reason = "wasm32 providers intentionally use non-Send boxed futures"
    )
)]
pub async fn check_action_remote(
    action_ref: &ActionRef,
    config: &Config,
    registry: &provider::Registry,
) -> Result<CheckResult, Error> {
    // Performance optimization: check ignore list early to skip [`provider::Registry::find`]
    // and [`provider.get_versions()`] network calls. Note: [`check_action()`] repeats this guard
    // for correctness when called directly with pre-fetched versions.
    if config.should_ignore(action_ref.owner(), action_ref.repo(), action_ref.ref_str()) {
        return Ok(CheckResult::new(action_ref.clone(), CheckStatus::Ignored));
    }

    let provider = registry
        .find(action_ref.host())
        .ok_or_else(|| Error::ProviderNotFound {
            host: action_ref
                .host()
                .map_or_else(|| "(default)".into(), String::from),
        })?;

    let repo_id = action_ref.to_repo_id().without_subpath();
    let versions = provider.get_versions(&repo_id).await?;
    let result = check_action_with_provider(action_ref, &versions, config, provider).await;

    // Fallback: if batch fetch returned no versions but the ref actually exists,
    // treat as up-to-date. This handles edge cases like:
    // - Repos with only lightweight tags that aren't returned by the versions API
    // - Refs that exist but aren't semver-tagged (branches, commits)
    //
    // Trade-off: We can't determine if there's a newer version without fetching
    // all tags, so we optimistically assume up-to-date.
    if matches!(result.status(), CheckStatus::Unresolvable { reason, .. } if reason == NO_VERSIONS_FOUND_REASON)
    {
        match provider
            .resolve_ref_to_sha(&repo_id, action_ref.ref_str())
            .await
        {
            Ok(_) => {
                return Ok(CheckResult::new(action_ref.clone(), CheckStatus::UpToDate));
            }
            Err(e) if e.is_environment_error() => {
                return Ok(CheckResult::new(
                    action_ref.clone(),
                    CheckStatus::Unresolvable {
                        reason: e.to_string(),
                        is_environment_error: true,
                    },
                ));
            }
            Err(_) => {}
        }
    }

    Ok(result)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{
        policy,
        provider::{ActionYml, Kind, Provider, Sha40},
    };

    struct ResolveMockProvider {
        resolve_ref: bool,
    }

    impl ResolveMockProvider {
        const fn new(resolve_ref: bool) -> Self {
            Self { resolve_ref }
        }
    }

    impl Provider for ResolveMockProvider {
        fn kind(&self) -> Kind {
            Kind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(
            &self,
            _repo: &crate::ActionRepoId,
        ) -> crate::BoxFuture<'_, crate::Result<version::Set>> {
            Box::pin(async { Ok(version::Set::new()) })
        }

        fn resolve_ref_to_sha(
            &self,
            repo: &crate::ActionRepoId,
            ref_name: &str,
        ) -> crate::BoxFuture<'_, crate::Result<Sha40>> {
            let result = if self.resolve_ref {
                Ok(Sha40::new("a".repeat(40)).unwrap())
            } else {
                Err(Error::RefNotFound {
                    owner: repo.owner().to_string(),
                    repo: repo.repo().to_string(),
                    ref_name: ref_name.to_string(),
                    host: repo.host().map(ToOwned::to_owned),
                })
            };
            Box::pin(async move { result })
        }

        fn get_default_branch(
            &self,
            _repo: &crate::ActionRepoId,
        ) -> crate::BoxFuture<'_, crate::Result<String>> {
            Box::pin(async { Ok("main".to_string()) })
        }

        fn get_readme(
            &self,
            _repo: &crate::ActionRepoId,
            _ref_name: Option<&str>,
        ) -> crate::BoxFuture<'_, crate::Result<Option<String>>> {
            Box::pin(async { Ok(None) })
        }

        fn get_action_yml(
            &self,
            _repo: &crate::ActionRepoId,
            _ref_name: &str,
        ) -> crate::BoxFuture<'_, crate::Result<ActionYml>> {
            Box::pin(async { Ok(ActionYml::default()) })
        }
    }

    fn make_action_ref(owner: &str, repo: &str, ref_str: &str) -> ActionRef {
        ActionRef::new(None, owner, repo, None, ref_str)
    }

    fn make_version_info(ver: &str, sha: &str) -> version::Info {
        version::Info::new(version::Version::parse(ver), sha)
    }

    fn make_version_set(
        tags: Vec<version::Info>,
        latest: Option<version::Info>,
        latest_per_major: HashMap<u32, version::Info>,
        major_tags: HashMap<u32, version::Info>,
    ) -> version::Set {
        version::Set::build(
            tags.into_iter().map(Arc::new).collect(),
            latest_per_major
                .into_iter()
                .map(|(k, v)| (k, Arc::new(v)))
                .collect(),
            major_tags
                .into_iter()
                .map(|(k, v)| (k, Arc::new(v)))
                .collect(),
            latest.map(Arc::new),
        )
    }

    #[test]
    fn test_check_action_up_to_date() {
        let action = make_action_ref("actions", "checkout", "v4.2.0");
        let v4_2 = make_version_info("v4.2.0", "abc123");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, v4_2.clone());
        let versions = make_version_set(
            vec![v4_2.clone()],
            Some(v4_2),
            latest_per_major,
            HashMap::new(),
        );
        let config = Config::default();

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status(), CheckStatus::UpToDate));
    }

    #[test]
    fn test_check_action_outdated() {
        let action = make_action_ref("actions", "checkout", "v4.0.0");
        let v4_0 = make_version_info("v4.0.0", "aaa");
        let v4_2 = make_version_info("v4.2.0", "bbb");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, v4_2.clone());
        let versions = make_version_set(
            vec![v4_0, v4_2.clone()],
            Some(v4_2),
            latest_per_major,
            HashMap::new(),
        );
        let config = Config::default();

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status(), CheckStatus::Outdated { .. }));
    }

    #[test]
    fn test_check_action_major_tag_up_to_date() {
        // "v4" should be up-to-date if the repo has a "v4" tag
        let action = make_action_ref("actions", "checkout", "v4");
        let v4_tag = make_version_info("v4", "abc123");
        let v4_2 = make_version_info("v4.2.0", "abc123");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, v4_2.clone());
        // Include both v4 (major tag) and v4.2.0 in semver_tags
        let versions = make_version_set(
            vec![v4_tag, v4_2.clone()],
            Some(v4_2),
            latest_per_major,
            HashMap::new(),
        );
        let config = Config::default();

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status(), CheckStatus::UpToDate));
    }

    #[test]
    fn test_check_action_major_no_v_prefix_not_up_to_date() {
        // "6" (no v prefix) should NOT be up-to-date when only "v6" tag exists.
        // Tag "6" doesn't exist — GitHub Actions would fail at runtime.
        let action = make_action_ref("actions", "setup-node", "6");
        let v6_tag = make_version_info("v6", "sha6");
        let v6_0_2 = make_version_info("v6.0.2", "sha6full");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(6, v6_0_2.clone());
        let mut major_tags = HashMap::new();
        major_tags.insert(6, v6_tag.clone());
        let versions = make_version_set(
            vec![v6_tag, v6_0_2.clone()],
            Some(v6_0_2),
            latest_per_major,
            major_tags,
        );
        let config = Config::default();

        let result = check_action(&action, &versions, &config);
        // Should be Outdated (suggesting "v6"), NOT UpToDate
        match result.status() {
            CheckStatus::Outdated { current, target } => {
                assert_eq!(current, "6");
                assert!(
                    target.version().to_string().starts_with("v6"),
                    "Should suggest 'v6.x', got '{}'",
                    target.version()
                );
            }
            other => panic!("Expected Outdated for '6' when only 'v6' exists, got {other:?}"),
        }
    }

    #[test]
    fn test_check_action_major_no_v_prefix_up_to_date_when_exists() {
        // "6" should be UpToDate when a "6" tag actually exists
        let action = make_action_ref("actions", "setup-node", "6");
        let tag_6 = make_version_info("6", "sha6_no_v"); // exact match
        let v6_tag = make_version_info("v6", "sha6_with_v");
        let v6_0_2 = make_version_info("v6.0.2", "sha6full");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(6, v6_0_2.clone());
        let mut major_tags = HashMap::new();
        major_tags.insert(6, v6_tag.clone());

        let versions = make_version_set(
            vec![tag_6, v6_tag, v6_0_2.clone()],
            Some(v6_0_2),
            latest_per_major,
            major_tags,
        );
        let config = Config::default();

        let result = check_action(&action, &versions, &config);
        assert!(
            matches!(result.status(), CheckStatus::UpToDate),
            "Expected UpToDate when '6' tag exists, got {:?}",
            result.status()
        );
    }

    #[test]
    fn test_check_action_ignored() {
        let action = make_action_ref("ignored", "action", "v1.0.0");
        let v2 = make_version_info("v2.0.0", "abc123");
        let versions = make_version_set(vec![v2.clone()], Some(v2), HashMap::new(), HashMap::new());
        let config = Config {
            ignore: vec!["ignored/action".to_string()],
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status(), CheckStatus::Ignored));
    }

    #[test]
    fn test_check_action_pinned() {
        let sha = "abc123def456abc123def456abc123def456abcd";
        let action = make_action_ref("actions", "checkout", sha);
        let v4 = make_version_info("v4.0.0", sha);
        let versions = make_version_set(vec![v4.clone()], Some(v4), HashMap::new(), HashMap::new());
        let config = Config::default();

        let result = check_action(&action, &versions, &config);
        assert!(matches!(
            result.status(),
            CheckStatus::Pinned { tag: Some(_), .. }
        ));
    }

    #[test]
    fn test_check_action_pinned_sha_outdated() {
        // SHA pinned to v7.0.0, but v7.0.1 exists -> outdated, with the newer SHA
        // available as the target so the updater can re-pin (not unpin to a tag).
        let pinned_sha = "1111111111111111111111111111111111111111";
        let newer_sha = "2222222222222222222222222222222222222222";
        let action = make_action_ref("actions", "checkout", pinned_sha);
        let v7_0_0 = make_version_info("v7.0.0", pinned_sha);
        let v7_0_1 = make_version_info("v7.0.1", newer_sha);
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(7, v7_0_1.clone());
        let versions = make_version_set(
            vec![v7_0_0, v7_0_1.clone()],
            Some(v7_0_1),
            latest_per_major,
            HashMap::new(),
        );
        let config = Config::default();

        let result = check_action(&action, &versions, &config);
        match result.status() {
            CheckStatus::Outdated { current, target } => {
                assert_eq!(current, "v7.0.0");
                assert_eq!(target.version().to_string(), "v7.0.1");
                assert_eq!(target.sha(), newer_sha);
            }
            other => panic!("expected Outdated, got {other:?}"),
        }
    }

    #[test]
    fn test_check_action_pinned_sha_unknown_stays_pinned() {
        // A SHA that matches no known tag can't be compared -> stays Pinned.
        let unknown_sha = "9999999999999999999999999999999999999999";
        let action = make_action_ref("actions", "checkout", unknown_sha);
        let v7 = make_version_info("v7.0.1", "2222222222222222222222222222222222222222");
        let versions = make_version_set(vec![v7.clone()], Some(v7), HashMap::new(), HashMap::new());
        let config = Config::default();

        let result = check_action(&action, &versions, &config);
        assert!(matches!(
            result.status(),
            CheckStatus::Pinned { tag: None, .. }
        ));
    }

    #[test]
    fn test_check_action_branch_ignored() {
        let action = make_action_ref("actions", "checkout", "main");
        let versions = make_version_set(vec![], None, HashMap::new(), HashMap::new());
        let config = Config {
            branch_policy: policy::Branch::Ignore,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status(), CheckStatus::UpToDate));
    }

    #[test]
    fn test_check_action_branch_warn() {
        let action = make_action_ref("actions", "checkout", "main");
        let versions = make_version_set(vec![], None, HashMap::new(), HashMap::new());
        let config = Config {
            branch_policy: policy::Branch::Warn,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status(), CheckStatus::Floating { .. }));
    }

    #[test]
    fn test_check_action_with_policy_override() {
        let action = make_action_ref("actions", "checkout", "v3.0.0");
        let v3 = make_version_info("v3.0.0", "aaa");
        let v4 = make_version_info("v4.0.0", "bbb");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(3, v3.clone());
        latest_per_major.insert(4, v4.clone());
        let versions = make_version_set(
            vec![v3, v4.clone()],
            Some(v4),
            latest_per_major,
            HashMap::new(),
        );

        // With minor policy (default), v3.0.0 is up-to-date (latest v3.x)
        let config = Config {
            update_policy: policy::Update::Minor,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status(), CheckStatus::UpToDate));
    }

    // Issue 1: Branch Policy for Non-Semver Refs (e.g., "stable", "nightly")

    #[test]
    fn test_check_action_non_semver_branch_policy_ignore() {
        // Non-semver ref like "stable" should be treated as UpToDate when branch_policy is ignore
        let action = make_action_ref("dtolnay", "rust-toolchain", "stable");
        let v1 = make_version_info("v1.0.0", "abc123");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(1, v1.clone());
        let versions =
            make_version_set(vec![v1.clone()], Some(v1), latest_per_major, HashMap::new());

        let config = Config {
            branch_policy: policy::Branch::Ignore,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        // With branch_policy: ignore, non-semver refs should NOT be marked outdated
        // They should be treated as "up to date" (skipped)
        assert!(
            matches!(result.status(), CheckStatus::UpToDate),
            "Expected UpToDate for 'stable' with branch_policy: ignore, got {:?}",
            result.status()
        );
    }

    #[test]
    fn test_check_action_non_semver_branch_policy_warn() {
        // Non-semver ref like "stable" is a floating branch under branch_policy: warn
        let action = make_action_ref("dtolnay", "rust-toolchain", "stable");
        let v1 = make_version_info("v1.0.0", "abc123");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(1, v1.clone());
        let versions =
            make_version_set(vec![v1.clone()], Some(v1), latest_per_major, HashMap::new());

        let config = Config {
            branch_policy: policy::Branch::Warn,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        match result.status() {
            CheckStatus::Floating { ref_name } => {
                assert_eq!(ref_name, "stable");
            }
            other => {
                panic!("Expected Floating for 'stable' with branch_policy: warn, got {other:?}")
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
        let versions =
            make_version_set(vec![v1.clone()], Some(v1), latest_per_major, HashMap::new());

        let config = Config {
            branch_policy: policy::Branch::Update,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        // With branch_policy: update, non-semver refs should be flagged for pinning
        // Current implementation returns Unresolvable - but ideally should return Outdated
        // with a target to pin to (either latest version or SHA)
        match result.status() {
            CheckStatus::Unresolvable { reason, .. } => {
                assert!(
                    reason.contains("pin") || reason.contains("SHA"),
                    "Expected reason to mention pinning/SHA, got: {reason}"
                );
            }
            CheckStatus::Outdated { target, .. } => {
                // This is the preferred behavior - suggest updating to latest
                assert!(
                    target.version().is_semver(),
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
        let mut major_tags = HashMap::new();
        major_tags.insert(6, v6_major.clone());
        let versions = make_version_set(
            vec![v4_2, v6_0_2.clone(), v6_major],
            Some(v6_0_2),
            latest_per_major,
            major_tags,
        );

        let config = Config {
            update_policy: policy::Update::Major,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        match result.status() {
            CheckStatus::Outdated { target, .. } => {
                // When user has major-only ref and major tag exists, suggest major tag
                let version = target.version();
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
        let versions = make_version_set(
            vec![v4_2, v6_0_2.clone()],
            Some(v6_0_2),
            latest_per_major,
            HashMap::new(),
        );

        let config = Config {
            update_policy: policy::Update::Major,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        match result.status() {
            CheckStatus::Outdated { target, .. } => {
                // When no major tag exists, fall back to full semver
                assert_eq!(
                    target.version().to_string(),
                    "v6.0.2",
                    "Expected full semver 'v6.0.2' when no major tag exists, got '{}'",
                    target.version()
                );
            }
            other => panic!("Expected Outdated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_check_action_with_provider_treats_resolved_v_major_ref_as_floating() {
        let action = make_action_ref("denoland", "setup-deno", "v2");
        let v2_0_4 = make_version_info("v2.0.4", "sha2");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(2, v2_0_4.clone());

        let versions = make_version_set(
            vec![v2_0_4.clone()],
            Some(v2_0_4),
            latest_per_major,
            HashMap::new(),
        );
        let config = Config {
            branch_policy: policy::Branch::Ignore,
            ..Default::default()
        };

        let provider = ResolveMockProvider::new(true);
        let result = check_action_with_provider(&action, &versions, &config, &provider).await;

        assert!(matches!(result.status(), CheckStatus::UpToDate));
    }

    #[tokio::test]
    async fn test_check_action_with_provider_keeps_missing_no_v_major_outdated() {
        let action = make_action_ref("actions", "setup-node", "6");
        let v6_tag = make_version_info("v6", "sha6");
        let v6_0_2 = make_version_info("v6.0.2", "sha6full");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(6, v6_0_2.clone());
        let mut major_tags = HashMap::new();
        major_tags.insert(6, v6_tag.clone());

        let versions = make_version_set(
            vec![v6_tag, v6_0_2.clone()],
            Some(v6_0_2),
            latest_per_major,
            major_tags,
        );
        let config = Config::default();

        let provider = ResolveMockProvider::new(true);
        let result = check_action_with_provider(&action, &versions, &config, &provider).await;

        assert!(matches!(result.status(), CheckStatus::Outdated { current, .. } if current == "6"));
    }

    #[test]
    fn test_semver_ref_with_no_versions_is_unresolvable() {
        // A semver ref against a repo with no semver tags cannot be compared.
        // ls-refs lists every tag, so an empty set means genuinely no versions,
        // not a truncated fetch window.
        let action = make_action_ref("owner", "repo", "v99");
        let versions = make_version_set(vec![], None, HashMap::new(), HashMap::new());
        let config = Config::default();

        let result = check_action(&action, &versions, &config);
        assert!(
            matches!(
                result.status(),
                CheckStatus::Unresolvable {
                    is_environment_error: false,
                    ..
                }
            ),
            "got: {:?}",
            result.status()
        );
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

        let mut major_tags = HashMap::new();
        major_tags.insert(6, v6_major.clone());
        let versions = make_version_set(
            vec![v4_1, v6_0_2.clone(), v6_major],
            Some(v6_0_2),
            latest_per_major,
            major_tags,
        );

        let config = Config {
            update_policy: policy::Update::Major,
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        match result.status() {
            CheckStatus::Outdated { target, .. } => {
                // User has full semver, should get full semver back
                assert_eq!(
                    target.version().to_string(),
                    "v6.0.2",
                    "Expected full semver 'v6.0.2' for full semver input, got '{}'",
                    target.version()
                );
            }
            other => panic!("Expected Outdated, got {other:?}"),
        }
    }

    #[test]
    fn test_check_result_serde_round_trip_up_to_date() {
        let result = CheckResult::new(
            make_action_ref("actions", "checkout", "v4.2.0"),
            CheckStatus::UpToDate,
        );
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: CheckResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, deserialized);
    }

    #[test]
    fn test_check_result_serde_round_trip_outdated() {
        let target = Arc::new(make_version_info("v4.2.0", "abc123"));
        let result = CheckResult::new(
            make_action_ref("actions", "checkout", "v4.0.0"),
            CheckStatus::Outdated {
                current: "v4.0.0".to_string(),
                target,
            },
        );
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: CheckResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, deserialized);
    }

    #[test]
    fn test_check_result_serde_round_trip_pinned() {
        let result = CheckResult::new(
            make_action_ref("actions", "checkout", "abc123"),
            CheckStatus::Pinned {
                sha: "abc123def456abc123def456abc123def456abcd".to_string(),
                tag: Some("v4.0.0".to_string()),
            },
        );
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: CheckResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, deserialized);
    }

    #[test]
    fn test_check_result_serde_round_trip_unresolvable() {
        let result = CheckResult::new(
            make_action_ref("actions", "checkout", "nonexistent"),
            CheckStatus::Unresolvable {
                reason: "tag not found".to_string(),
                is_environment_error: false,
            },
        );
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: CheckResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, deserialized);
    }

    #[test]
    fn test_check_result_serde_round_trip_ignored() {
        let result = CheckResult::new(
            make_action_ref("ignored", "action", "v1"),
            CheckStatus::Ignored,
        );
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: CheckResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, deserialized);
    }
}
