//! Canonical resolution logic for checking action versions
//!
//! This module provides the single source of truth for version resolution,
//! used by CLI, LSP, and MCP interfaces.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
    action_ref::ActionRef,
    config::{Config, Policy},
    error::Error,
    policy, provider, version,
};

/// Reason string for unresolvable status when no versions are found.
///
/// Used for semver refs where batch fetching found no versions,
/// but the ref might still exist (e.g., older tags beyond `max_tags` limit).
pub const NO_VERSIONS_FOUND_REASON: &str = "no versions found";

/// Reason string for a short SHA ref that abbreviates a commit.
pub const SHORT_SHA_REASON: &str = "short SHA; pin to the full 40-character commit";

/// Result of checking a single action reference
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "tsify", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "tsify", tsify(into_wasm_abi, from_wasm_abi))]
pub struct CheckResult {
    /// The action reference that was checked
    action_ref: ActionRef,
    /// The check status
    status: CheckStatus,
    /// What the ref names, when the version set lists it
    #[serde(default)]
    ref_kind: Option<RefKind>,
    /// The suggested target, when it sits on the commit the ref already names
    #[serde(default)]
    same_commit: Option<Arc<version::Info>>,
}

/// Kind of ref a `uses:` reference names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "tsify", derive(tsify_next::Tsify))]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    /// A git tag
    Tag,
    /// A git branch
    Branch,
    /// A full commit SHA
    Sha,
}

impl std::fmt::Display for RefKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Tag => "tag",
            Self::Branch => "branch",
            Self::Sha => "sha",
        })
    }
}

impl CheckResult {
    /// Create a new check result
    #[must_use]
    pub const fn new(action_ref: ActionRef, status: CheckStatus) -> Self {
        Self {
            action_ref,
            status,
            ref_kind: None,
            same_commit: None,
        }
    }

    fn described(
        action_ref: &ActionRef,
        status: CheckStatus,
        versions: &version::Set,
        config: &Config,
        resolved: Option<&str>,
    ) -> Self {
        let listed = current_commit(action_ref, versions);
        let Some(sha) = listed.map(|(_, sha)| sha).or(resolved) else {
            return Self::new(action_ref.clone(), status);
        };
        let target = match &status {
            CheckStatus::Outdated { target, .. } => Some(Arc::clone(target)),
            CheckStatus::Floating { .. } | CheckStatus::PinBranch { .. } => {
                let current = version::Version::parse(action_ref.ref_str());
                current
                    .is_semver()
                    .then(|| find_best_target(versions, &current, config.policy_for(action_ref)))
                    .flatten()
            }
            _ => None,
        };
        let same_commit = target.filter(|t| t.sha() == sha);
        Self {
            action_ref: action_ref.clone(),
            status,
            ref_kind: listed.map(|(kind, _)| kind),
            same_commit,
        }
    }

    /// Get the kind of ref, when the version set lists it
    #[must_use]
    pub const fn ref_kind(&self) -> Option<RefKind> {
        self.ref_kind
    }

    /// Get the suggested target, when it sits on the commit the ref already names
    #[must_use]
    pub const fn same_commit(&self) -> Option<&Arc<version::Info>> {
        self.same_commit.as_ref()
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
    /// Action references a floating branch under `update` branch policy: it
    /// should be pinned to the branch's current HEAD SHA.
    PinBranch {
        /// The branch ref to pin (used for the `# branch` comment)
        ref_name: String,
        /// The branch's current HEAD commit SHA to pin to
        sha: String,
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
/// Under [`policy::Track::Exact`] only the identical version is current.
fn is_version_current(
    current: &version::Version,
    target: &version::Info,
    track: policy::Track,
    versions: &version::Set,
) -> bool {
    let t = target.version();
    let identical =
        t == current || (same_release(t, current) && versions.has_exact_tag(current.as_str()));
    if track == policy::Track::Exact {
        return identical;
    }
    match current {
        version::Version::Semver {
            major,
            minor: None,
            patch: None,
            ..
        } => t.major() == Some(*major),
        version::Version::Semver {
            major,
            minor: Some(minor),
            patch: None,
            ..
        } => t.major() == Some(*major) && t.minor() == Some(*minor),
        _ => identical,
    }
}

/// Check if `current`, whose tag sits on `current_sha` when listed, is up to date with `target`.
///
/// A target on the current commit is current under [`policy::Track::Keep`]
/// or when it has the current ref's granularity. A higher target on another
/// commit is newer code.
fn is_current(
    current: &version::Version,
    current_sha: Option<&str>,
    target: &version::Info,
    track: policy::Track,
    versions: &version::Set,
) -> bool {
    let granularity = |v: &version::Version| (v.minor().is_some(), v.patch().is_some());
    match current_sha {
        Some(sha) if sha == target.sha() => {
            track == policy::Track::Keep || granularity(current) == granularity(target.version())
        }
        Some(_) if target.version() > current => false,
        _ => is_version_current(current, target, track, versions),
    }
}

fn same_release(a: &version::Version, b: &version::Version) -> bool {
    match (a, b) {
        (
            version::Version::Semver {
                major,
                minor,
                patch,
                prerelease,
                ..
            },
            version::Version::Semver {
                major: other_major,
                minor: other_minor,
                patch: other_patch,
                prerelease: other_prerelease,
                ..
            },
        ) => {
            major == other_major
                && minor == other_minor
                && patch == other_patch
                && prerelease == other_prerelease
        }
        _ => false,
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

struct Lookup {
    found: CheckStatus,
    missing: CheckStatus,
    commit: Option<CheckStatus>,
}

/// Check action using pre-fetched versions and provider ref resolution.
///
/// Asks the provider whether the ref exists when `versions` does not list it
/// and the answer changes the status.
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
    let (status, lookup) = classify(action_ref, versions, config);
    let Some(lookup) = lookup else {
        return CheckResult::described(action_ref, status, versions, config, None);
    };

    let repo_id = action_ref.to_repo_id().without_subpath();
    let resolved = provider
        .resolve_ref_to_sha(&repo_id, action_ref.ref_str())
        .await;
    let status = match &resolved {
        Ok(sha) => match lookup.commit {
            Some(commit) if action_ref.abbreviates(sha.as_str()) => commit,
            _ => lookup.found,
        },
        Err(e) if e.is_environment_error() => CheckStatus::Unresolvable {
            reason: e.to_string(),
            is_environment_error: true,
        },
        Err(_) => lookup.missing,
    };
    let resolved = resolved.as_ref().ok().map(provider::Sha40::as_str);
    CheckResult::described(action_ref, status, versions, config, resolved)
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
    let status = classify(action_ref, versions, config).0;
    CheckResult::described(action_ref, status, versions, config, None)
}

fn current_commit<'a>(
    action_ref: &'a ActionRef,
    versions: &'a version::Set,
) -> Option<(RefKind, &'a str)> {
    let name = action_ref.ref_str();
    if action_ref.is_sha() {
        return Some((RefKind::Sha, name));
    }
    versions
        .tag(name)
        .map(|t| t.sha())
        .or_else(|| versions.other_tag_sha(name))
        .map(|sha| (RefKind::Tag, sha))
        .or_else(|| versions.branch_sha(name).map(|sha| (RefKind::Branch, sha)))
}

fn classify(
    action_ref: &ActionRef,
    versions: &version::Set,
    config: &Config,
) -> (CheckStatus, Option<Lookup>) {
    if config.should_ignore(action_ref) {
        return (CheckStatus::Ignored, None);
    }

    let policy = config.policy_for(action_ref);

    if action_ref.is_sha() {
        return (check_pinned_sha(action_ref, versions, policy), None);
    }

    let ref_str = action_ref.ref_str();

    if action_ref.is_short_sha() && !versions.names_ref(ref_str) {
        return classify_short_sha(ref_str, policy.branch, versions);
    }

    let current = version::Version::parse(ref_str);

    if !current.is_semver() {
        return classify_non_semver(ref_str, policy.branch, versions);
    }

    if !versions.has_exact_tag(ref_str) && versions.branch_sha(ref_str).is_some() {
        return (
            status_for_non_semver(ref_str, policy.branch, versions),
            None,
        );
    }

    let Some(target) = find_best_target(versions, &current, policy) else {
        return (status_for_no_target(&current), None);
    };

    let current_sha = versions.tag(ref_str).map(|t| t.sha());
    if is_current(&current, current_sha, &target, policy.track, versions) {
        if current_sha.is_some() {
            return (CheckStatus::UpToDate, None);
        }
        if !current.is_major_only() {
            let missing = CheckStatus::Unresolvable {
                reason: format!("tag '{ref_str}' not found; use '{}'", target.version()),
                is_environment_error: false,
            };
            return defer(
                missing.clone(),
                Lookup {
                    found: CheckStatus::UpToDate,
                    missing,
                    commit: None,
                },
                versions,
            );
        }
    }

    let status = CheckStatus::Outdated {
        current: ref_str.to_string(),
        target,
    };
    if should_respect_branch_policy_for_major_ref(action_ref, versions, &status) {
        let found = status_for_non_semver(ref_str, policy.branch, versions);
        return defer(
            status.clone(),
            Lookup {
                found,
                missing: status,
                commit: None,
            },
            versions,
        );
    }
    (status, None)
}

fn classify_non_semver(
    ref_name: &str,
    branch_policy: policy::Branch,
    versions: &version::Set,
) -> (CheckStatus, Option<Lookup>) {
    let status = status_for_non_semver(ref_name, branch_policy, versions);
    if versions.names_ref(ref_name) {
        return (status, None);
    }
    defer(
        CheckStatus::Unresolvable {
            reason: format!("ref '{ref_name}' is not a listed tag or branch"),
            is_environment_error: true,
        },
        Lookup {
            found: status,
            missing: CheckStatus::Unresolvable {
                reason: format!("ref '{ref_name}' not found"),
                is_environment_error: false,
            },
            commit: None,
        },
        versions,
    )
}

/// Pair `status` with a provider `lookup` for a ref that `versions` does not name.
///
/// A complete listing answers locally: the ref is a commit when `lookup`
/// carries a commit status, and missing otherwise.
fn defer(
    status: CheckStatus,
    lookup: Lookup,
    versions: &version::Set,
) -> (CheckStatus, Option<Lookup>) {
    match versions.listing() {
        version::Listing::Complete => (lookup.commit.unwrap_or(lookup.missing), None),
        version::Listing::Partial => (status, Some(lookup)),
    }
}

/// Classify a short SHA ref that `versions` does not list as a tag or branch.
///
/// A tag or branch with that name on another commit takes precedence and
/// follows the branch policy.
fn classify_short_sha(
    ref_name: &str,
    branch_policy: policy::Branch,
    versions: &version::Set,
) -> (CheckStatus, Option<Lookup>) {
    let short = CheckStatus::Unresolvable {
        reason: SHORT_SHA_REASON.into(),
        is_environment_error: false,
    };
    defer(
        short.clone(),
        Lookup {
            found: status_for_non_semver(ref_name, branch_policy, versions),
            missing: CheckStatus::Unresolvable {
                reason: format!("ref '{ref_name}' not found"),
                is_environment_error: false,
            },
            commit: Some(short),
        },
        versions,
    )
}

/// Determine status for a SHA-pinned ref.
///
/// If the SHA matches a known tag's commit, the most specific tag on it is the
/// pinned version. It is compared against the latest allowed version: a newer
/// version yields `Outdated` whose `target` carries the newer commit SHA (so
/// updates re-pin rather than unpin to a tag); otherwise it stays `Pinned`. A SHA that matches no known tag can't be
/// compared, so it also stays `Pinned`. A target on the pinned commit is never
/// an update.
fn check_pinned_sha(
    action_ref: &ActionRef,
    versions: &version::Set,
    policy: Policy,
) -> CheckStatus {
    let sha = action_ref.ref_str().to_string();

    let Some(pinned) = versions.tag_on(action_ref.ref_str()).cloned() else {
        return CheckStatus::Pinned { sha, tag: None };
    };

    let target = find_best_target(versions, pinned.version(), policy);

    match target {
        Some(t)
            if t.sha() != sha
                && !is_current(pinned.version(), Some(&sha), &t, policy.track, versions) =>
        {
            CheckStatus::Outdated {
                current: pinned.version().to_string(),
                target: t,
            }
        }
        _ => CheckStatus::Pinned {
            sha,
            tag: Some(pinned.version().to_string()),
        },
    }
}

/// Handle non-semver refs based on branch policy.
fn status_for_non_semver(
    ref_name: &str,
    branch_policy: policy::Branch,
    versions: &version::Set,
) -> CheckStatus {
    match branch_policy {
        policy::Branch::Ignore => CheckStatus::UpToDate,
        policy::Branch::Warn => CheckStatus::Floating {
            ref_name: ref_name.to_string(),
        },
        policy::Branch::Update => {
            // Pin the branch to its current HEAD commit SHA (locking it where it
            // is), keeping the ref's meaning. Only a `# branch` comment marks
            // what was pinned. Falls back when the branch head is unknown (e.g.
            // versions came from an API that doesn't enumerate branches).
            versions.branch_sha(ref_name).map_or_else(
                || CheckStatus::Unresolvable {
                    reason: "branch ref should be pinned to a SHA".into(),
                    is_environment_error: false,
                },
                |sha| CheckStatus::PinBranch {
                    ref_name: ref_name.to_string(),
                    sha: sha.to_string(),
                },
            )
        }
    }
}

/// Find the best target version, respecting [`Policy::track`].
///
/// Returns the major-only tag for the target's major (v6) when the track asks
/// for one, the repo has it, and it points at the latest version's commit.
/// [`policy::Track::Major`] also needs `bump` to allow every version of that
/// major. Otherwise returns the full version.
fn find_best_target(
    versions: &version::Set,
    current: &version::Version,
    policy: Policy,
) -> Option<Arc<version::Info>> {
    let latest = versions.find_latest_for(current, policy.bump, policy.prerelease)?;
    let major_tag = latest
        .version()
        .major()
        .and_then(|major| versions.major_tags().get(&major));

    let whole_major = policy.bump != policy::Update::Patch || current.minor().is_none();
    let target = match (policy.track, major_tag) {
        (policy::Track::Keep, Some(tag))
            if current.is_major_only() && tag.sha() == latest.sha() =>
        {
            tag
        }
        (policy::Track::Major, Some(tag)) if whole_major && tag.sha() == latest.sha() => tag,
        _ => latest,
    };
    Some(Arc::clone(target))
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
    if config.should_ignore(action_ref) {
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

    #[derive(Clone, Copy)]
    enum Resolve {
        Found,
        To(&'static str),
        Missing,
        Offline,
    }

    struct ResolveMockProvider {
        lookup: Resolve,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ResolveMockProvider {
        const fn new(resolve_ref: bool) -> Self {
            Self::with(if resolve_ref {
                Resolve::Found
            } else {
                Resolve::Missing
            })
        }

        const fn with(lookup: Resolve) -> Self {
            Self {
                lookup,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
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
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let result = match self.lookup {
                Resolve::Found => Ok(Sha40::new("a".repeat(40)).unwrap()),
                Resolve::To(sha) => Sha40::new(sha).ok_or_else(|| Error::Internal {
                    context: "mock resolve".to_string(),
                    message: format!("invalid SHA {sha}"),
                }),
                Resolve::Missing => Err(Error::RefNotFound {
                    owner: repo.owner().to_string(),
                    repo: repo.repo().to_string(),
                    ref_name: ref_name.to_string(),
                    host: repo.host().map(ToOwned::to_owned),
                }),
                Resolve::Offline => Err(Error::Network {
                    operation: "resolve".to_string(),
                    message: "offline".to_string(),
                }),
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

    fn with_branch(mut versions: version::Set, name: &str) -> version::Set {
        versions.set_branches(HashMap::from([(name.to_string(), "b".repeat(40))]));
        versions
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
        let tag_6 = make_version_info("6", "sha6full"); // exact match
        let v6_tag = make_version_info("v6", "sha6full");
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
        let Ok(pattern) = "ignored/action".parse() else {
            panic!("pattern");
        };
        let config = Config {
            scan: crate::config::Scan {
                ignore: vec![pattern],
                ..Default::default()
            },
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
        let versions = with_branch(
            make_version_set(vec![], None, HashMap::new(), HashMap::new()),
            "main",
        );
        let config = Config {
            policy: Policy {
                branch: policy::Branch::Ignore,
                ..Default::default()
            },
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status(), CheckStatus::UpToDate));
    }

    #[test]
    fn test_check_action_branch_warn() {
        let action = make_action_ref("actions", "checkout", "main");
        let versions = with_branch(
            make_version_set(vec![], None, HashMap::new(), HashMap::new()),
            "main",
        );
        let config = Config {
            policy: Policy {
                branch: policy::Branch::Warn,
                ..Default::default()
            },
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
            policy: Policy {
                bump: policy::Update::Minor,
                ..Default::default()
            },
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        assert!(matches!(result.status(), CheckStatus::UpToDate));
    }

    // Issue 1: Branch Policy for Non-Semver Refs (e.g., "stable", "nightly")

    #[test]
    fn test_check_action_non_semver_branch_policy_ignore() {
        // Non-semver ref like "stable" should be treated as UpToDate when policy.branch is ignore
        let action = make_action_ref("dtolnay", "rust-toolchain", "stable");
        let v1 = make_version_info("v1.0.0", "abc123");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(1, v1.clone());
        let versions = with_branch(
            make_version_set(vec![v1.clone()], Some(v1), latest_per_major, HashMap::new()),
            "stable",
        );

        let config = Config {
            policy: Policy {
                branch: policy::Branch::Ignore,
                ..Default::default()
            },
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        // With policy.branch = ignore, non-semver refs should NOT be marked outdated
        // They should be treated as "up to date" (skipped)
        assert!(
            matches!(result.status(), CheckStatus::UpToDate),
            "Expected UpToDate for 'stable' with policy.branch = ignore, got {:?}",
            result.status()
        );
    }

    #[test]
    fn test_check_action_non_semver_branch_policy_warn() {
        // Non-semver ref like "stable" is a floating branch under policy.branch = warn
        let action = make_action_ref("dtolnay", "rust-toolchain", "stable");
        let v1 = make_version_info("v1.0.0", "abc123");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(1, v1.clone());
        let versions = with_branch(
            make_version_set(vec![v1.clone()], Some(v1), latest_per_major, HashMap::new()),
            "stable",
        );

        let config = Config {
            policy: Policy {
                branch: policy::Branch::Warn,
                ..Default::default()
            },
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        match result.status() {
            CheckStatus::Floating { ref_name } => {
                assert_eq!(ref_name, "stable");
            }
            other => {
                panic!("Expected Floating for 'stable' with policy.branch = warn, got {other:?}")
            }
        }
    }

    #[test]
    fn test_check_action_non_semver_branch_policy_update_pins_branch_sha() {
        // Under update, a branch ref pins to the branch's current HEAD SHA.
        let action = make_action_ref("dtolnay", "rust-toolchain", "stable");
        let v1 = make_version_info("v1.0.0", "abc123");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(1, v1.clone());
        let mut versions =
            make_version_set(vec![v1.clone()], Some(v1), latest_per_major, HashMap::new());
        versions.set_branches(HashMap::from([(
            "stable".to_string(),
            "deadbeef".to_string(),
        )]));

        let config = Config {
            policy: Policy {
                branch: policy::Branch::Update,
                ..Default::default()
            },
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        match result.status() {
            CheckStatus::PinBranch { ref_name, sha } => {
                assert_eq!(ref_name, "stable");
                assert_eq!(sha, "deadbeef");
            }
            other => panic!("Expected PinBranch for 'stable' under update, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_check_action_update_unresolvable_when_branch_head_unknown() {
        // Update policy but no branch head available (e.g. API without branch
        // enumeration) falls back to unresolvable rather than guessing a tag.
        let action = make_action_ref("dtolnay", "rust-toolchain", "stable");
        let v1 = make_version_info("v1.0.0", "abc123");
        let versions = make_version_set(vec![v1.clone()], Some(v1), HashMap::new(), HashMap::new());

        let config = Config {
            policy: Policy {
                branch: policy::Branch::Update,
                ..Default::default()
            },
            ..Default::default()
        };

        let provider = ResolveMockProvider::new(true);
        let result = check_action_with_provider(&action, &versions, &config, &provider).await;
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
    fn test_per_action_branch_policy_override() {
        // Global ignore stays quiet, but a per-action override flags one repo.
        let action = make_action_ref("kjanat", "runner", "master");
        let versions = with_branch(
            make_version_set(vec![], None, HashMap::new(), HashMap::new()),
            "master",
        );

        let Ok(pattern) = "kjanat/runner".parse() else {
            panic!("pattern");
        };
        let config = Config {
            overrides: vec![crate::config::Override {
                pattern,
                policy: crate::config::PolicyOverride {
                    branch: Some(policy::Branch::Warn),
                    ..Default::default()
                },
            }],
            ..Default::default()
        };

        let result = check_action(&action, &versions, &config);
        assert!(
            matches!(result.status(), CheckStatus::Floating { .. }),
            "override should flag master as floating, got {:?}",
            result.status()
        );
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
        let v6_major = make_version_info("v6", "sha6full");

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
            policy: Policy {
                bump: policy::Update::Major,
                ..Default::default()
            },
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
            policy: Policy {
                bump: policy::Update::Major,
                ..Default::default()
            },
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
            policy: Policy {
                branch: policy::Branch::Ignore,
                ..Default::default()
            },
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
            policy: Policy {
                bump: policy::Update::Major,
                ..Default::default()
            },
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

    fn track_versions() -> version::Set {
        let v4_1 = make_version_info("v4.1.0", "sha4");
        let v4_major = make_version_info("v4", "sha4");
        let v6_0_2 = make_version_info("v6.0.2", "sha6");
        let v6_major = make_version_info("v6", "sha6");
        make_version_set(
            vec![
                v4_1.clone(),
                v4_major.clone(),
                v6_0_2.clone(),
                v6_major.clone(),
            ],
            Some(v6_0_2.clone()),
            HashMap::from([(4, v4_1), (6, v6_0_2)]),
            HashMap::from([(4, v4_major), (6, v6_major)]),
        )
    }

    fn track_config(track: policy::Track) -> Config {
        Config {
            policy: Policy {
                bump: policy::Update::Major,
                track,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn target_for(ref_str: &str, track: policy::Track, versions: &version::Set) -> Option<String> {
        let action = make_action_ref("actions", "checkout", ref_str);
        match check_action(&action, versions, &track_config(track)).status() {
            CheckStatus::Outdated { target, .. } => Some(target.version().to_string()),
            CheckStatus::UpToDate => None,
            other => panic!("{ref_str} under {track}: unexpected {other:?}"),
        }
    }

    #[test]
    fn test_track_keep_follows_current_granularity() {
        let versions = track_versions();
        assert_eq!(
            target_for("v4", policy::Track::Keep, &versions).as_deref(),
            Some("v6")
        );
        assert_eq!(
            target_for("v4.1.0", policy::Track::Keep, &versions).as_deref(),
            Some("v6.0.2")
        );
        assert_eq!(target_for("v6", policy::Track::Keep, &versions), None);
        assert_eq!(target_for("v6.0.2", policy::Track::Keep, &versions), None);
    }

    #[test]
    fn test_track_major_writes_major_tag() {
        let versions = track_versions();
        assert_eq!(
            target_for("v4", policy::Track::Major, &versions).as_deref(),
            Some("v6")
        );
        assert_eq!(
            target_for("v4.1.0", policy::Track::Major, &versions).as_deref(),
            Some("v6")
        );
        assert_eq!(
            target_for("v6.0.2", policy::Track::Major, &versions).as_deref(),
            Some("v6")
        );
        assert_eq!(target_for("v6", policy::Track::Major, &versions), None);
    }

    #[test]
    fn test_track_major_falls_back_to_full_without_major_tag() {
        let v4_1 = make_version_info("v4.1.0", "sha4");
        let v6_0_2 = make_version_info("v6.0.2", "sha6");
        let versions = make_version_set(
            vec![v4_1.clone(), v6_0_2.clone()],
            Some(v6_0_2.clone()),
            HashMap::from([(4, v4_1), (6, v6_0_2)]),
            HashMap::new(),
        );
        assert_eq!(
            target_for("v4.1.0", policy::Track::Major, &versions).as_deref(),
            Some("v6.0.2")
        );
        assert_eq!(target_for("v6.0.2", policy::Track::Major, &versions), None);
    }

    fn stale_major_tag_versions() -> version::Set {
        let v4_1 = make_version_info("v4.1.0", "4444444444444444444444444444444444444444");
        let v6_0_0 = make_version_info("v6.0.0", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let v6_major = make_version_info("v6", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let v6_0_2 = make_version_info("v6.0.2", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        make_version_set(
            vec![v4_1.clone(), v6_0_0, v6_major.clone(), v6_0_2.clone()],
            Some(v6_0_2.clone()),
            HashMap::from([(4, v4_1), (6, v6_0_2)]),
            HashMap::from([(6, v6_major)]),
        )
    }

    #[test]
    fn test_track_major_skips_stale_major_tag_on_tag_ref() {
        let versions = stale_major_tag_versions();
        assert_eq!(target_for("v6.0.2", policy::Track::Major, &versions), None);
        assert_eq!(
            target_for("v4.1.0", policy::Track::Major, &versions).as_deref(),
            Some("v6.0.2")
        );
    }

    #[test]
    fn test_track_major_skips_stale_major_tag_on_sha_pin() {
        let versions = stale_major_tag_versions();
        let config = track_config(policy::Track::Major);

        let latest = make_action_ref(
            "actions",
            "checkout",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        let result = check_action(&latest, &versions, &config);
        assert!(
            matches!(result.status(), CheckStatus::Pinned { tag: Some(tag), .. } if tag == "v6.0.2"),
            "got {:?}",
            result.status()
        );

        let older = make_action_ref(
            "actions",
            "checkout",
            "4444444444444444444444444444444444444444",
        );
        let result = check_action(&older, &versions, &config);
        assert!(
            matches!(
                result.status(),
                CheckStatus::Outdated { target, .. }
                    if target.version().to_string() == "v6.0.2"
                        && target.sha() == "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            ),
            "got {:?}",
            result.status()
        );
    }

    #[test]
    fn test_track_exact_writes_full_version() {
        let versions = track_versions();
        assert_eq!(
            target_for("v4", policy::Track::Exact, &versions).as_deref(),
            Some("v6.0.2")
        );
        assert_eq!(
            target_for("v6", policy::Track::Exact, &versions).as_deref(),
            Some("v6.0.2")
        );
        assert_eq!(
            target_for("v4.1.0", policy::Track::Exact, &versions).as_deref(),
            Some("v6.0.2")
        );
        assert_eq!(target_for("v6.0.2", policy::Track::Exact, &versions), None);
    }

    #[test]
    fn test_track_from_override() {
        let versions = track_versions();
        let Ok(pattern) = "actions/*".parse() else {
            panic!("pattern");
        };
        let config = Config {
            overrides: vec![crate::config::Override {
                pattern,
                policy: crate::config::PolicyOverride {
                    track: Some(policy::Track::Exact),
                    ..Default::default()
                },
            }],
            ..track_config(policy::Track::Keep)
        };
        let result = check_action(
            &make_action_ref("actions", "checkout", "v6"),
            &versions,
            &config,
        );
        assert!(
            matches!(result.status(), CheckStatus::Outdated { target, .. } if target.version().to_string() == "v6.0.2"),
            "got {:?}",
            result.status()
        );
    }

    #[test]
    fn test_pinned_sha_stays_pinned_when_track_changes_only_the_tag() {
        let pinned = "6666666666666666666666666666666666666666";
        let v6_0_2 = make_version_info("v6.0.2", pinned);
        let v6_major = make_version_info("v6", pinned);
        let versions = make_version_set(
            vec![v6_0_2.clone(), v6_major.clone()],
            Some(v6_0_2.clone()),
            HashMap::from([(6, v6_0_2)]),
            HashMap::from([(6, v6_major)]),
        );
        let action = make_action_ref("actions", "checkout", pinned);
        for track in [
            policy::Track::Keep,
            policy::Track::Major,
            policy::Track::Exact,
        ] {
            let result = check_action(&action, &versions, &track_config(track));
            assert!(
                matches!(result.status(), CheckStatus::Pinned { tag: Some(_), .. }),
                "{track}: got {:?}",
                result.status()
            );
        }
    }

    fn prerelease_set(tags: &[&str]) -> version::Set {
        make_version_set(
            tags.iter()
                .map(|t| make_version_info(t, &format!("sha-{t}")))
                .collect(),
            None,
            HashMap::new(),
            HashMap::new(),
        )
    }

    fn target_of(status: &CheckStatus) -> Result<Option<String>, String> {
        match status {
            CheckStatus::Outdated { target, .. } => Ok(Some(target.version().to_string())),
            CheckStatus::UpToDate => Ok(None),
            other => Err(format!("unexpected {other:?}")),
        }
    }

    fn target_under(
        toml_str: &str,
        action: &ActionRef,
        versions: &version::Set,
    ) -> Result<Option<String>, Box<dyn std::error::Error>> {
        let config: Config = toml::from_str(toml_str)?;
        Ok(target_of(check_action(action, versions, &config).status())?)
    }

    #[tokio::test]
    async fn test_prerelease_flag_selects_newer_major_prerelease()
    -> Result<(), Box<dyn std::error::Error>> {
        let versions = prerelease_set(&["v2.0.0", "v3.0.0-rc.1"]);
        let action = make_action_ref("actions", "checkout", "v2");
        let provider = ResolveMockProvider::new(true);
        for (flag, want) in [(true, Some("v3.0.0-rc.1")), (false, None)] {
            let config: Config = toml::from_str(&format!(
                "[policy]\nbump = \"major\"\nbranch = \"ignore\"\nprerelease = {flag}"
            ))?;
            let result = check_action_with_provider(&action, &versions, &config, &provider).await;
            assert_eq!(
                target_of(result.status())?.as_deref(),
                want,
                "prerelease = {flag}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_prerelease_current_takes_stable_release() -> Result<(), Box<dyn std::error::Error>> {
        let versions = prerelease_set(&["v2.0.0", "v3.0.0-rc.1", "v3.0.0"]);
        let action = make_action_ref("actions", "checkout", "v3.0.0-rc.1");
        for flag in [false, true] {
            assert_eq!(
                target_under(
                    &format!("[policy]\nprerelease = {flag}"),
                    &action,
                    &versions
                )?
                .as_deref(),
                Some("v3.0.0"),
                "prerelease = {flag}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_prerelease_current_takes_newer_prerelease_of_same_version()
    -> Result<(), Box<dyn std::error::Error>> {
        let versions = prerelease_set(&["v3.0.0-rc.1", "v3.0.0-rc.2", "v3.1.0-beta.1"]);
        let rc1 = make_action_ref("actions", "checkout", "v3.0.0-rc.1");
        let rc2 = make_action_ref("actions", "checkout", "v3.0.0-rc.2");
        let off = "[policy]\nprerelease = false";
        let on = "[policy]\nprerelease = true";

        assert_eq!(
            target_under(off, &rc1, &versions)?.as_deref(),
            Some("v3.0.0-rc.2")
        );
        assert_eq!(target_under(off, &rc2, &versions)?, None);
        assert_eq!(
            target_under(on, &rc1, &versions)?.as_deref(),
            Some("v3.1.0-beta.1")
        );
        Ok(())
    }

    #[test]
    fn test_prerelease_flag_keeps_major_under_minor_bump() -> Result<(), Box<dyn std::error::Error>>
    {
        let versions = prerelease_set(&["v2.0.0", "v2.1.0-rc.1", "v3.0.0", "v3.1.0-rc.1"]);
        let action = make_action_ref("actions", "checkout", "v2.0.0");

        assert_eq!(
            target_under(
                "[policy]\nbump = \"minor\"\nprerelease = true",
                &action,
                &versions
            )?
            .as_deref(),
            Some("v2.1.0-rc.1")
        );
        assert_eq!(
            target_under(
                "[policy]\nbump = \"minor\"\nprerelease = false",
                &action,
                &versions
            )?,
            None
        );
        Ok(())
    }

    #[test]
    fn test_prerelease_from_override() -> Result<(), Box<dyn std::error::Error>> {
        let versions = prerelease_set(&["v2.0.0", "v2.1.0-rc.1"]);
        let checkout = make_action_ref("actions", "checkout", "v2.0.0");
        let other = make_action_ref("other", "tool", "v2.0.0");

        let enable = "[policy]\nprerelease = false\n\n[[overrides]]\npattern = \"actions/*\"\npolicy = { prerelease = true }";
        assert_eq!(
            target_under(enable, &checkout, &versions)?.as_deref(),
            Some("v2.1.0-rc.1")
        );
        assert_eq!(target_under(enable, &other, &versions)?, None);

        let disable = "[policy]\nprerelease = true\n\n[[overrides]]\npattern = \"actions/*\"\npolicy = { prerelease = false }";
        assert_eq!(target_under(disable, &checkout, &versions)?, None);
        assert_eq!(
            target_under(disable, &other, &versions)?.as_deref(),
            Some("v2.1.0-rc.1")
        );
        Ok(())
    }

    #[test]
    fn test_twin_build_tag_on_same_commit_is_current() -> Result<(), Box<dyn std::error::Error>> {
        let sha = "1111111111111111111111111111111111111111";
        let versions = make_version_set(
            vec![
                make_version_info("v1.0.0", sha),
                make_version_info("v1.0.0+build", sha),
            ],
            None,
            HashMap::new(),
            HashMap::new(),
        );
        let twin = make_action_ref("actions", "checkout", "v1.0.0+build");
        let bare = make_action_ref("actions", "checkout", "1.0.0");
        for track in ["keep", "major", "exact"] {
            let config = format!("[policy]\ntrack = \"{track}\"");
            assert_eq!(
                target_under(&config, &twin, &versions)?,
                None,
                "track = {track}"
            );
            assert_eq!(
                target_under(&config, &bare, &versions)?.as_deref(),
                Some("v1.0.0"),
                "track = {track}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_track_major_moves_full_ref_to_major_tag_of_same_release()
    -> Result<(), Box<dyn std::error::Error>> {
        let sha = "6666666666666666666666666666666666666666";
        let v6_major = make_version_info("v6", sha);
        let versions = make_version_set(
            vec![make_version_info("v6.0.0", sha), v6_major.clone()],
            None,
            HashMap::new(),
            HashMap::from([(6, v6_major)]),
        );
        let action = make_action_ref("actions", "checkout", "v6.0.0");
        assert_eq!(
            target_under("[policy]\ntrack = \"major\"", &action, &versions)?.as_deref(),
            Some("v6")
        );
        Ok(())
    }

    const BUMPS: [&str; 4] = ["patch", "minor", "major", "latest"];

    fn bump_versions(major_tags: bool) -> version::Set {
        let tags = [
            ("v4.1.0", "1"),
            ("v4.1.3", "2"),
            ("v4.9.3", "3"),
            ("v5.0.0", "5"),
        ];
        let mut infos: Vec<version::Info> = tags
            .iter()
            .map(|(tag, sha)| make_version_info(tag, &sha.repeat(40)))
            .collect();
        let mut majors = HashMap::new();
        if major_tags {
            for (major, tag, sha) in [(4, "v4", "3"), (5, "v5", "5")] {
                let info = make_version_info(tag, &sha.repeat(40));
                infos.push(info.clone());
                majors.insert(major, info);
            }
        }
        make_version_set(infos, None, HashMap::new(), majors)
    }

    fn bump_config(bump: &str) -> String {
        format!("[policy]\nbump = \"{bump}\"")
    }

    #[test]
    fn test_bump_limits_full_version_target() -> Result<(), Box<dyn std::error::Error>> {
        let versions = bump_versions(false);
        let action = make_action_ref("actions", "checkout", "v4.1.0");
        for (bump, want) in BUMPS
            .into_iter()
            .zip(["v4.1.3", "v4.9.3", "v5.0.0", "v5.0.0"])
        {
            assert_eq!(
                target_under(&bump_config(bump), &action, &versions)?.as_deref(),
                Some(want),
                "bump = {bump}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_bump_on_major_only_ref() -> Result<(), Box<dyn std::error::Error>> {
        let versions = bump_versions(true);
        let action = make_action_ref("actions", "checkout", "v4");
        for (bump, want) in BUMPS.into_iter().zip([None, None, Some("v5"), Some("v5")]) {
            assert_eq!(
                target_under(&bump_config(bump), &action, &versions)?.as_deref(),
                want,
                "bump = {bump}"
            );
        }
        for (bump, want) in BUMPS
            .into_iter()
            .zip(["v4.9.3", "v4.9.3", "v5.0.0", "v5.0.0"])
        {
            let config = format!("{}\ntrack = \"exact\"", bump_config(bump));
            assert_eq!(
                target_under(&config, &action, &versions)?.as_deref(),
                Some(want),
                "bump = {bump}, track = exact"
            );
        }
        Ok(())
    }

    #[test]
    fn test_bump_on_minor_only_ref() -> Result<(), Box<dyn std::error::Error>> {
        let mut tags: Vec<version::Info> = bump_versions(false)
            .semver_tags()
            .iter()
            .map(|t| (**t).clone())
            .collect();
        tags.push(make_version_info("v4.1", &"2".repeat(40)));
        let versions = make_version_set(tags, None, HashMap::new(), HashMap::new());
        let action = make_action_ref("actions", "checkout", "v4.1");
        for (bump, want) in
            BUMPS
                .into_iter()
                .zip([None, Some("v4.9.3"), Some("v5.0.0"), Some("v5.0.0")])
        {
            assert_eq!(
                target_under(&bump_config(bump), &action, &versions)?.as_deref(),
                want,
                "bump = {bump}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_bump_on_sha_pinned_ref() -> Result<(), Box<dyn std::error::Error>> {
        let versions = bump_versions(false);
        let action = make_action_ref("actions", "checkout", &"1".repeat(40));
        for (bump, want, sha) in [
            ("patch", "v4.1.3", "2"),
            ("minor", "v4.9.3", "3"),
            ("major", "v5.0.0", "5"),
            ("latest", "v5.0.0", "5"),
        ] {
            let config: Config = toml::from_str(&bump_config(bump))?;
            let result = check_action(&action, &versions, &config);
            let CheckStatus::Outdated { current, target } = result.status() else {
                return Err(format!("bump = {bump}: got {:?}", result.status()).into());
            };
            assert_eq!(current, "v4.1.0", "bump = {bump}");
            assert_eq!(target.version().as_str(), want, "bump = {bump}");
            assert_eq!(target.sha(), sha.repeat(40), "bump = {bump}");
        }
        Ok(())
    }

    #[test]
    fn test_track_major_keeps_patch_bumps_off_the_major_tag()
    -> Result<(), Box<dyn std::error::Error>> {
        let tags = |major_on: &str| {
            let v4 = make_version_info("v4", &major_on.repeat(40));
            make_version_set(
                vec![
                    make_version_info("v4.1.0", &"1".repeat(40)),
                    make_version_info("v4.1.3", &"2".repeat(40)),
                    make_version_info("v4.9.3", &"3".repeat(40)),
                    v4.clone(),
                ],
                None,
                HashMap::new(),
                HashMap::from([(4, v4)]),
            )
        };
        let full = make_action_ref("actions", "checkout", "v4.1.0");
        let major = make_action_ref("actions", "checkout", "v4");
        for (bump, major_on, action, want) in [
            ("patch", "2", &full, Some("v4.1.3")),
            ("minor", "2", &full, Some("v4.9.3")),
            ("minor", "3", &full, Some("v4")),
            ("patch", "3", &major, None),
        ] {
            let config = format!("{}\ntrack = \"major\"", bump_config(bump));
            assert_eq!(
                target_under(&config, action, &tags(major_on))?.as_deref(),
                want,
                "bump = {bump}, v4 on {major_on}, ref {}",
                action.ref_str()
            );
        }
        Ok(())
    }

    #[test]
    fn test_patch_bump_never_downgrades_a_missing_ref() -> Result<(), Box<dyn std::error::Error>> {
        let versions = bump_versions(false);
        let action = make_action_ref("actions", "checkout", "v4.1.9");
        let config: Config = toml::from_str(&bump_config("patch"))?;
        assert_eq!(
            check_action(&action, &versions, &config).status(),
            &unresolvable(NO_VERSIONS_FOUND_REASON, false)
        );
        assert_eq!(
            target_under(&bump_config("minor"), &action, &versions)?.as_deref(),
            Some("v4.9.3")
        );
        Ok(())
    }

    #[test]
    fn test_bump_from_override() -> Result<(), Box<dyn std::error::Error>> {
        let versions = bump_versions(false);
        let checkout = make_action_ref("actions", "checkout", "v4.1.0");
        let other = make_action_ref("other", "tool", "v4.1.0");
        let config = "[policy]\nbump = \"latest\"\n\n[[overrides]]\npattern = \"actions/*\"\npolicy = { bump = \"patch\" }";
        assert_eq!(
            target_under(config, &checkout, &versions)?.as_deref(),
            Some("v4.1.3")
        );
        assert_eq!(
            target_under(config, &other, &versions)?.as_deref(),
            Some("v5.0.0")
        );
        Ok(())
    }

    async fn status_with(
        action: &ActionRef,
        versions: &version::Set,
        config: &Config,
        resolve: Resolve,
    ) -> CheckStatus {
        let provider = ResolveMockProvider::with(resolve);
        check_action_with_provider(action, versions, config, &provider)
            .await
            .status()
            .clone()
    }

    fn unresolvable(reason: &str, is_environment_error: bool) -> CheckStatus {
        CheckStatus::Unresolvable {
            reason: reason.to_string(),
            is_environment_error,
        }
    }

    fn branch_config(branch: policy::Branch) -> Config {
        Config {
            policy: Policy {
                branch,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_minor_only_ref_without_tag_is_unresolvable() {
        let versions = prerelease_set(&["v4.1.0", "v4.2.0", "v4.2.1"]);
        let action = make_action_ref("actions", "checkout", "v4.2");
        let config = Config::default();
        let missing = unresolvable("tag 'v4.2' not found; use 'v4.2.1'", false);

        assert_eq!(check_action(&action, &versions, &config).status(), &missing);
        assert_eq!(
            status_with(&action, &versions, &config, Resolve::Missing).await,
            missing
        );
        assert_eq!(
            status_with(&action, &versions, &config, Resolve::Found).await,
            CheckStatus::UpToDate
        );
        assert_eq!(
            status_with(&action, &versions, &config, Resolve::Offline).await,
            unresolvable("network error during resolve: offline", true)
        );
    }

    #[tokio::test]
    async fn test_minor_only_ref_with_tag_is_up_to_date() {
        let versions = prerelease_set(&["v4.2", "v4.2.0"]);
        let action = make_action_ref("actions", "checkout", "v4.2");
        let config = Config::default();

        assert_eq!(
            check_action(&action, &versions, &config).status(),
            &CheckStatus::UpToDate
        );
        assert_eq!(
            status_with(&action, &versions, &config, Resolve::Missing).await,
            CheckStatus::UpToDate
        );
    }

    #[test]
    fn test_bare_minor_ref_needs_its_own_tag() {
        let action = make_action_ref("actions", "checkout", "1.2");
        let config = Config::default();

        let versions = prerelease_set(&["v1.2.0"]);
        assert_eq!(
            check_action(&action, &versions, &config).status(),
            &unresolvable("tag '1.2' not found; use 'v1.2.0'", false)
        );

        let versions = prerelease_set(&["v1.2.0", "1.2"]);
        assert_eq!(
            check_action(&action, &versions, &config).status(),
            &CheckStatus::UpToDate
        );
    }

    #[tokio::test]
    async fn test_unlisted_non_semver_ref_under_each_branch_policy() {
        let versions = with_branch(prerelease_set(&["v1.0.0"]), "main");
        let nope = make_action_ref("actions", "checkout", "nope");
        let main = make_action_ref("actions", "checkout", "main");
        let cases = [
            (
                policy::Branch::Ignore,
                CheckStatus::UpToDate,
                CheckStatus::UpToDate,
            ),
            (
                policy::Branch::Warn,
                CheckStatus::Floating {
                    ref_name: "nope".to_string(),
                },
                CheckStatus::Floating {
                    ref_name: "main".to_string(),
                },
            ),
            (
                policy::Branch::Update,
                unresolvable("branch ref should be pinned to a SHA", false),
                CheckStatus::PinBranch {
                    ref_name: "main".to_string(),
                    sha: "b".repeat(40),
                },
            ),
        ];

        for (branch, found, listed) in cases {
            let config = branch_config(branch);
            assert_eq!(
                check_action(&nope, &versions, &config).status(),
                &unresolvable("ref 'nope' is not a listed tag or branch", true),
                "{branch}"
            );
            assert_eq!(
                status_with(&nope, &versions, &config, Resolve::Missing).await,
                unresolvable("ref 'nope' not found", false),
                "{branch}"
            );
            assert_eq!(
                status_with(&nope, &versions, &config, Resolve::Found).await,
                found,
                "{branch}"
            );
            assert_eq!(
                status_with(&nope, &versions, &config, Resolve::Offline).await,
                unresolvable("network error during resolve: offline", true),
                "{branch}"
            );
            assert_eq!(
                status_with(&main, &versions, &config, Resolve::Missing).await,
                listed,
                "{branch}"
            );
        }
    }

    #[tokio::test]
    async fn test_sha_matching_no_tag_stays_pinned() {
        let sha = "9".repeat(40);
        let versions = prerelease_set(&["v1.0.0"]);
        let action = make_action_ref("actions", "checkout", &sha);
        assert_eq!(
            status_with(&action, &versions, &Config::default(), Resolve::Missing).await,
            CheckStatus::Pinned { sha, tag: None }
        );
    }

    #[tokio::test]
    async fn test_missing_prerelease_ref_above_every_tag_is_unresolvable() {
        let versions = prerelease_set(&["v3.0.0-rc.1"]);
        let action = make_action_ref("actions", "checkout", "v3.0.0-rc.9");
        let missing = unresolvable(NO_VERSIONS_FOUND_REASON, false);
        for flag in [false, true] {
            let config = Config {
                policy: Policy {
                    prerelease: flag,
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                check_action(&action, &versions, &config).status(),
                &missing,
                "prerelease = {flag}"
            );
            assert_eq!(
                status_with(&action, &versions, &config, Resolve::Missing).await,
                missing,
                "prerelease = {flag}"
            );
        }
    }

    #[tokio::test]
    async fn test_flow_style_major_ref_sees_newer_major() -> Result<(), Box<dyn std::error::Error>>
    {
        let v4 = make_version_info("v4", &"4".repeat(40));
        let v5 = make_version_info("v5", &"5".repeat(40));
        let versions = make_version_set(
            vec![
                v4.clone(),
                make_version_info("v4.0.0", &"4".repeat(40)),
                v5.clone(),
                make_version_info("v5.0.0", &"5".repeat(40)),
            ],
            None,
            HashMap::new(),
            HashMap::from([(4, v4), (5, v5)]),
        );
        let config = Config::default();
        let provider = ResolveMockProvider::new(false);

        let yaml = "steps:\n  - { uses: actions/deploy-pages@v4, id: deployment }\n";
        let locations = crate::extract_uses_from_yaml(yaml);
        let [uses] = locations.as_slice() else {
            return Err(format!("expected one `uses:`, got {locations:?}").into());
        };
        let Some(action) = crate::parse_uses(uses.value())? else {
            return Err("expected a remote ref".into());
        };
        let result = check_action_with_provider(&action, &versions, &config, &provider).await;
        assert!(
            matches!(
                result.status(),
                CheckStatus::Outdated { current, target }
                    if current == "v4" && target.version().as_str() == "v5"
            ),
            "got {:?}",
            result.status()
        );

        let comma = make_action_ref("actions", "deploy-pages", "v4,");
        let result = check_action_with_provider(&comma, &versions, &config, &provider).await;
        assert_eq!(result.status(), &unresolvable("ref 'v4,' not found", false));
        Ok(())
    }

    const SHORT_SHA_COMMIT: &str = "abc1234def5678abc1234def5678abc1234def56";

    fn short_sha_status() -> CheckStatus {
        unresolvable(SHORT_SHA_REASON, false)
    }

    #[tokio::test]
    async fn test_short_sha_is_unresolvable_under_each_branch_policy() {
        let versions = prerelease_set(&["v1.0.0"]);
        let short = make_action_ref("actions", "checkout", "abc1234");
        for branch in [
            policy::Branch::Ignore,
            policy::Branch::Warn,
            policy::Branch::Update,
        ] {
            let config = branch_config(branch);
            assert_eq!(
                check_action(&short, &versions, &config).status(),
                &short_sha_status(),
                "{branch}"
            );
            assert_eq!(
                status_with(&short, &versions, &config, Resolve::To(SHORT_SHA_COMMIT)).await,
                short_sha_status(),
                "{branch}"
            );
            assert_eq!(
                status_with(&short, &versions, &config, Resolve::Missing).await,
                unresolvable("ref 'abc1234' not found", false),
                "{branch}"
            );
            assert_eq!(
                status_with(&short, &versions, &config, Resolve::Offline).await,
                unresolvable("network error during resolve: offline", true),
                "{branch}"
            );
        }
    }

    #[test]
    fn test_all_digit_short_sha_is_not_compared_as_semver() {
        let versions = prerelease_set(&["v1.0.0", "v2.0.0"]);
        let action = make_action_ref("actions", "checkout", "1234567");
        assert_eq!(
            check_action(&action, &versions, &Config::default()).status(),
            &short_sha_status()
        );
    }

    #[tokio::test]
    async fn test_hex_named_ref_on_another_commit_follows_branch_policy() {
        let versions = prerelease_set(&["v1.0.0"]);
        let named = make_action_ref("actions", "checkout", "deadbee");
        for (branch, want) in [
            (policy::Branch::Ignore, CheckStatus::UpToDate),
            (
                policy::Branch::Warn,
                CheckStatus::Floating {
                    ref_name: "deadbee".to_string(),
                },
            ),
        ] {
            assert_eq!(
                status_with(&named, &versions, &branch_config(branch), Resolve::Found).await,
                want,
                "{branch}"
            );
        }
    }

    #[tokio::test]
    async fn test_listed_hex_branch_skips_short_sha_lookup() {
        let versions = with_branch(prerelease_set(&["v1.0.0"]), "deadbee");
        let named = make_action_ref("actions", "checkout", "deadbee");
        assert_eq!(
            status_with(
                &named,
                &versions,
                &branch_config(policy::Branch::Warn),
                Resolve::Missing
            )
            .await,
            CheckStatus::Floating {
                ref_name: "deadbee".to_string()
            }
        );
        assert_eq!(
            check_action(&named, &versions, &branch_config(policy::Branch::Update)).status(),
            &CheckStatus::PinBranch {
                ref_name: "deadbee".to_string(),
                sha: "b".repeat(40),
            }
        );
    }

    fn complete(mut versions: version::Set) -> version::Set {
        versions.set_listing(version::Listing::Complete);
        versions.set_other_tags(HashMap::from([("stable".to_string(), "c".repeat(40))]));
        versions
    }

    #[tokio::test]
    async fn test_complete_listing_answers_without_provider_lookup() {
        let mut versions = complete(prerelease_set(&["v1.0.0", "v2.0.4", "v4.2.1"]));
        versions.set_branches(HashMap::from([("main".to_string(), "b".repeat(40))]));
        let warn = branch_config(policy::Branch::Warn);
        let cases = [
            (
                "stable",
                CheckStatus::Floating {
                    ref_name: "stable".to_string(),
                },
            ),
            (
                "main",
                CheckStatus::Floating {
                    ref_name: "main".to_string(),
                },
            ),
            ("nope", unresolvable("ref 'nope' not found", false)),
            ("abc1234", short_sha_status()),
            (
                "v4.2",
                unresolvable("tag 'v4.2' not found; use 'v4.2.1'", false),
            ),
        ];
        for (ref_str, want) in cases {
            let provider = ResolveMockProvider::with(Resolve::Found);
            let action = make_action_ref("actions", "checkout", ref_str);
            let result = check_action_with_provider(&action, &versions, &warn, &provider).await;
            assert_eq!(result.status(), &want, "{ref_str}");
            assert_eq!(
                provider.calls.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{ref_str}"
            );
        }
    }

    #[tokio::test]
    async fn test_restored_complete_listing_asks_the_provider()
    -> Result<(), Box<dyn std::error::Error>> {
        let versions = complete(prerelease_set(&["v1.0.0"]));
        let restored: version::Set = serde_json::from_str(&serde_json::to_string(&versions)?)?;
        let provider = ResolveMockProvider::with(Resolve::Found);
        let action = make_action_ref("actions", "checkout", "nightly");
        let warn = branch_config(policy::Branch::Warn);
        let result = check_action_with_provider(&action, &restored, &warn, &provider).await;
        assert_eq!(
            result.status(),
            &CheckStatus::Floating {
                ref_name: "nightly".to_string()
            }
        );
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_complete_listing_keeps_unlisted_v_major_outdated() {
        let versions = complete(prerelease_set(&["v2.0.4"]));
        let provider = ResolveMockProvider::with(Resolve::Found);
        let action = make_action_ref("denoland", "setup-deno", "v2");
        let result =
            check_action_with_provider(&action, &versions, &Config::default(), &provider).await;
        assert!(
            matches!(result.status(), CheckStatus::Outdated { target, .. } if target.version().as_str() == "v2.0.4"),
            "{:?}",
            result.status()
        );
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_short_sha_listed_as_tag_follows_branch_policy() {
        let mut versions = prerelease_set(&["v1.0.0"]);
        versions.set_other_tags(HashMap::from([("deadbee".to_string(), "c".repeat(40))]));
        let provider = ResolveMockProvider::with(Resolve::Missing);
        let action = make_action_ref("actions", "checkout", "deadbee");
        let result = check_action_with_provider(
            &action,
            &versions,
            &branch_config(policy::Branch::Warn),
            &provider,
        )
        .await;
        assert_eq!(
            result.status(),
            &CheckStatus::Floating {
                ref_name: "deadbee".to_string()
            }
        );
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_semver_shaped_branch_follows_branch_policy() {
        let versions = with_branch(prerelease_set(&["v4.2.0", "v4.2.1"]), "v4.2");
        let action = make_action_ref("actions", "checkout", "v4.2");
        for (branch, want) in [
            (policy::Branch::Ignore, CheckStatus::UpToDate),
            (
                policy::Branch::Warn,
                CheckStatus::Floating {
                    ref_name: "v4.2".to_string(),
                },
            ),
            (
                policy::Branch::Update,
                CheckStatus::PinBranch {
                    ref_name: "v4.2".to_string(),
                    sha: "b".repeat(40),
                },
            ),
        ] {
            let config = branch_config(branch);
            assert_eq!(
                check_action(&action, &versions, &config).status(),
                &want,
                "{branch}"
            );
            assert_eq!(
                status_with(&action, &versions, &config, Resolve::Missing).await,
                want,
                "{branch}"
            );
        }
    }

    #[test]
    fn test_semver_tag_wins_over_branch_of_the_same_name() {
        let versions = with_branch(prerelease_set(&["v4.2", "v4.2.0"]), "v4.2");
        let action = make_action_ref("actions", "checkout", "v4.2");
        assert_eq!(
            check_action(&action, &versions, &branch_config(policy::Branch::Warn)).status(),
            &CheckStatus::UpToDate
        );
    }

    #[test]
    fn test_sha_pin_takes_the_most_specific_tag_on_its_commit()
    -> Result<(), Box<dyn std::error::Error>> {
        let pinned = "4".repeat(40);
        let newer = "9".repeat(40);
        let action = make_action_ref("actions", "checkout", &pinned);
        for order in [["v4", "v4.0.0"], ["v4.0.0", "v4"]] {
            let mut tags: Vec<version::Info> = order
                .iter()
                .map(|tag| make_version_info(tag, &pinned))
                .collect();
            tags.push(make_version_info("v4.9.3", &newer));
            let versions = make_version_set(tags, None, HashMap::new(), HashMap::new());
            for track in ["exact", "major"] {
                let config: Config =
                    toml::from_str(&format!("[policy]\nbump = \"patch\"\ntrack = \"{track}\""))?;
                assert_eq!(
                    check_action(&action, &versions, &config).status(),
                    &CheckStatus::Pinned {
                        sha: pinned.clone(),
                        tag: Some("v4.0.0".to_string()),
                    },
                    "{order:?}, track = {track}"
                );
            }
            let result = check_action(&action, &versions, &Config::default());
            assert!(
                matches!(
                    result.status(),
                    CheckStatus::Outdated { current, target }
                        if current == "v4.0.0" && target.version().as_str() == "v4.9.3"
                ),
                "{order:?}: {:?}",
                result.status()
            );
        }
        Ok(())
    }

    fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }
        (0..items.len())
            .flat_map(|i| {
                let mut rest = items.to_vec();
                let head = rest.remove(i);
                permutations(&rest).into_iter().map(move |mut order| {
                    order.insert(0, head.clone());
                    order
                })
            })
            .collect()
    }

    fn versions_in_order(tags: &[(&str, &str)]) -> version::Set {
        let infos: Vec<version::Info> = tags
            .iter()
            .map(|(tag, sha)| make_version_info(tag, sha))
            .collect();
        let major_tags = infos
            .iter()
            .filter(|info| info.version().is_major_only())
            .filter_map(|info| Some((info.version().major()?, info.clone())))
            .collect();
        make_version_set(infos, None, HashMap::new(), major_tags)
    }

    fn status_under(track: policy::Track, ref_str: &str, versions: &version::Set) -> CheckStatus {
        let action = make_action_ref("actions", "checkout", ref_str);
        check_action(&action, versions, &track_config(track))
            .status()
            .clone()
    }

    const TRACKS: [policy::Track; 3] = [
        policy::Track::Keep,
        policy::Track::Major,
        policy::Track::Exact,
    ];

    #[test]
    fn test_major_ref_on_the_latest_commit_is_current_in_any_order() {
        let sha = "4".repeat(40);
        let older = "3".repeat(40);
        let tags = [
            ("v4", sha.as_str()),
            ("v4.2", sha.as_str()),
            ("v4.2.0", sha.as_str()),
            ("v3.9.0", older.as_str()),
        ];
        for order in permutations(&tags) {
            let versions = versions_in_order(&order);
            assert_eq!(
                status_under(policy::Track::Keep, "v4", &versions),
                CheckStatus::UpToDate,
                "{order:?}"
            );
            for track in [policy::Track::Keep, policy::Track::Exact] {
                assert_eq!(
                    status_under(track, "v4.2.0", &versions),
                    CheckStatus::UpToDate,
                    "{order:?}, track = {track}"
                );
            }
            assert!(
                matches!(
                    status_under(policy::Track::Major, "v4.2.0", &versions),
                    CheckStatus::Outdated { target, .. } if target.version().as_str() == "v4"
                ),
                "{order:?}"
            );
            for track in TRACKS {
                assert_eq!(
                    status_under(track, &sha, &versions),
                    CheckStatus::Pinned {
                        sha: sha.clone(),
                        tag: Some("v4.2.0".to_string()),
                    },
                    "{order:?}, track = {track}"
                );
            }
            assert_eq!(
                status_under(policy::Track::Major, "v4", &versions),
                CheckStatus::UpToDate,
                "{order:?}"
            );
            assert!(
                matches!(
                    status_under(policy::Track::Exact, "v4", &versions),
                    CheckStatus::Outdated { target, .. } if target.version().as_str() == "v4.2.0"
                ),
                "{order:?}"
            );
        }
    }

    #[test]
    fn test_major_ref_behind_a_newer_patch_is_outdated_in_any_order() {
        let old = "4".repeat(40);
        let new = "5".repeat(40);
        let tags = [
            ("v4", old.as_str()),
            ("v4.2", old.as_str()),
            ("v4.2.0", old.as_str()),
            ("v4.2.1", new.as_str()),
        ];
        for order in permutations(&tags) {
            let versions = versions_in_order(&order);
            for track in TRACKS {
                for ref_str in ["v4", "v4.2", "v4.2.0"] {
                    assert!(
                        matches!(
                            status_under(track, ref_str, &versions),
                            CheckStatus::Outdated { current, target }
                                if current == ref_str
                                    && target.version().as_str() == "v4.2.1"
                                    && target.sha() == new
                        ),
                        "{ref_str}, {order:?}, track = {track}"
                    );
                }
                assert!(
                    matches!(
                        status_under(track, &old, &versions),
                        CheckStatus::Outdated { current, target }
                            if current == "v4.2.0" && target.sha() == new
                    ),
                    "{order:?}, track = {track}"
                );
            }
        }
    }

    #[test]
    fn test_newer_tag_on_the_current_commit_is_current() {
        let sha = "4".repeat(40);
        for order in permutations(&[("v4.2.0", sha.as_str()), ("v4.2.1", sha.as_str())]) {
            let versions = versions_in_order(&order);
            for track in TRACKS {
                assert_eq!(
                    status_under(track, "v4.2.0", &versions),
                    CheckStatus::UpToDate,
                    "{order:?}, track = {track}"
                );
            }
        }
    }

    #[test]
    fn test_same_version_on_another_commit_follows_the_track() {
        let (a, b) = ("a".repeat(40), "b".repeat(40));
        for order in permutations(&[("v4.2", a.as_str()), ("v4.2.0", b.as_str())]) {
            let versions = versions_in_order(&order);
            assert_eq!(
                status_under(policy::Track::Keep, "v4.2", &versions),
                CheckStatus::UpToDate,
                "{order:?}"
            );
            assert!(
                matches!(
                    status_under(policy::Track::Exact, "v4.2", &versions),
                    CheckStatus::Outdated { target, .. } if target.sha() == b
                ),
                "{order:?}"
            );
        }
    }

    fn branch_v1_at(head: &str) -> version::Set {
        let mut versions = versions_in_order(&[
            ("v1.0.4", "4".repeat(40).as_str()),
            ("v1.0.5", "5".repeat(40).as_str()),
        ]);
        versions.set_branches(HashMap::from([("v1".to_string(), head.to_string())]));
        versions
    }

    #[tokio::test]
    async fn test_branch_on_the_target_commit_is_same_commit()
    -> Result<(), Box<dyn std::error::Error>> {
        let head = "5".repeat(40);
        let versions = branch_v1_at(&head);
        let action = make_action_ref("rust-lang", "crates-io-auth-action", "v1");
        for (branch, want) in [
            (
                policy::Branch::Warn,
                CheckStatus::Floating {
                    ref_name: "v1".to_string(),
                },
            ),
            (
                policy::Branch::Update,
                CheckStatus::PinBranch {
                    ref_name: "v1".to_string(),
                    sha: head.clone(),
                },
            ),
        ] {
            let provider = ResolveMockProvider::with(Resolve::Missing);
            let result =
                check_action_with_provider(&action, &versions, &branch_config(branch), &provider)
                    .await;
            assert_eq!(result.status(), &want, "{branch}");
            assert_eq!(result.ref_kind(), Some(RefKind::Branch), "{branch}");
            assert_eq!(
                result.same_commit().map(|t| t.version().as_str()),
                Some("v1.0.5"),
                "{branch}"
            );
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            let json = serde_json::to_value(&result)?;
            assert_eq!(json["ref_kind"], "branch");
            assert_eq!(serde_json::from_value::<CheckResult>(json)?, result);
        }

        let ignored = check_action(&action, &versions, &branch_config(policy::Branch::Ignore));
        assert_eq!(ignored.status(), &CheckStatus::UpToDate);
        assert_eq!(ignored.ref_kind(), Some(RefKind::Branch));
        assert_eq!(ignored.same_commit(), None);
        Ok(())
    }

    #[test]
    fn test_branch_behind_the_target_is_not_same_commit() {
        let versions = branch_v1_at(&"4".repeat(40));
        let action = make_action_ref("rust-lang", "crates-io-auth-action", "v1");
        for branch in [policy::Branch::Warn, policy::Branch::Update] {
            let result = check_action(&action, &versions, &branch_config(branch));
            assert!(
                matches!(
                    result.status(),
                    CheckStatus::Floating { .. } | CheckStatus::PinBranch { .. }
                ),
                "{branch}: {:?}",
                result.status()
            );
            assert_eq!(result.ref_kind(), Some(RefKind::Branch), "{branch}");
            assert_eq!(result.same_commit(), None, "{branch}");
        }
    }

    #[tokio::test]
    async fn test_unlisted_ref_on_the_target_commit_is_same_commit() {
        const OLD: &str = "4444444444444444444444444444444444444444";
        const NEW: &str = "5555555555555555555555555555555555555555";
        let versions = versions_in_order(&[("v1.0.4", OLD), ("v1.0.5", NEW)]);
        let action = make_action_ref("rust-lang", "crates-io-auth-action", "v1");
        let warn = branch_config(policy::Branch::Warn);
        for (head, want) in [(NEW, Some("v1.0.5")), (OLD, None)] {
            let provider = ResolveMockProvider::with(Resolve::To(head));
            let result = check_action_with_provider(&action, &versions, &warn, &provider).await;
            assert_eq!(
                result.status(),
                &CheckStatus::Floating {
                    ref_name: "v1".to_string()
                },
                "{head}"
            );
            assert_eq!(result.ref_kind(), None, "{head}");
            assert_eq!(
                result.same_commit().map(|t| t.version().as_str()),
                want,
                "{head}"
            );
        }
    }

    #[test]
    fn test_tag_on_the_target_commit_is_same_commit() {
        let (old, new) = ("4".repeat(40), "5".repeat(40));
        let versions = versions_in_order(&[
            ("v1", new.as_str()),
            ("v1.0.4", old.as_str()),
            ("v1.0.5", new.as_str()),
        ]);
        let result_for = |ref_str: &str, track| {
            check_action(
                &make_action_ref("actions", "checkout", ref_str),
                &versions,
                &track_config(track),
            )
        };

        let exact = result_for("v1", policy::Track::Exact);
        assert!(
            matches!(exact.status(), CheckStatus::Outdated { current, target }
                if current == "v1" && target.version().as_str() == "v1.0.5"),
            "{:?}",
            exact.status()
        );
        assert_eq!(exact.ref_kind(), Some(RefKind::Tag));
        assert_eq!(
            exact.same_commit().map(|t| t.version().as_str()),
            Some("v1.0.5")
        );

        let keep = result_for("v1", policy::Track::Keep);
        assert_eq!(keep.status(), &CheckStatus::UpToDate);
        assert_eq!(keep.ref_kind(), Some(RefKind::Tag));
        assert_eq!(keep.same_commit(), None);

        let behind = result_for("v1.0.4", policy::Track::Keep);
        assert!(
            matches!(behind.status(), CheckStatus::Outdated { target, .. } if target.sha() == new),
            "{:?}",
            behind.status()
        );
        assert_eq!(behind.ref_kind(), Some(RefKind::Tag));
        assert_eq!(behind.same_commit(), None);

        let unlisted = result_for("v2", policy::Track::Keep);
        assert_eq!(unlisted.ref_kind(), None);
        assert_eq!(unlisted.same_commit(), None);
    }

    #[test]
    fn test_sha_ref_is_never_same_commit() {
        let (old, new) = ("4".repeat(40), "5".repeat(40));
        let versions = versions_in_order(&[("v1.0.4", old.as_str()), ("v1.0.5", new.as_str())]);
        let config = track_config(policy::Track::Keep);

        let behind = check_action(
            &make_action_ref("actions", "checkout", &old),
            &versions,
            &config,
        );
        assert!(
            matches!(behind.status(), CheckStatus::Outdated { target, .. } if target.sha() == new),
            "{:?}",
            behind.status()
        );
        assert_eq!(behind.ref_kind(), Some(RefKind::Sha));
        assert_eq!(behind.same_commit(), None);

        let latest = check_action(
            &make_action_ref("actions", "checkout", &new),
            &versions,
            &config,
        );
        assert!(
            matches!(latest.status(), CheckStatus::Pinned { .. }),
            "{:?}",
            latest.status()
        );
        assert_eq!(latest.ref_kind(), Some(RefKind::Sha));
        assert_eq!(latest.same_commit(), None);
    }

    #[test]
    fn test_check_result_without_ref_facts_deserializes() -> Result<(), Box<dyn std::error::Error>>
    {
        let result = CheckResult::new(
            make_action_ref("actions", "checkout", "v4"),
            CheckStatus::UpToDate,
        );
        let mut json = serde_json::to_value(&result)?;
        let Some(fields) = json.as_object_mut() else {
            panic!("expected a JSON object");
        };
        fields.remove("ref_kind");
        fields.remove("same_commit");
        assert_eq!(serde_json::from_value::<CheckResult>(json)?, result);
        Ok(())
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
