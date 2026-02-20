//! Types for representing GitHub Actions references
//!
//! An action reference (from `uses:` in workflows) can be:
//! - `owner/repo@ref`
//! - `owner/repo/path@ref` (subpath for reusable workflows or nested actions)
//! - `host/owner/repo@ref` (explicit host for enterprise/self-hosted)
//! - `./path` (local, not handled for version checks)

use serde::{Deserialize, Serialize};
use std::fmt;

/// A parsed action reference from a `uses:` statement
///
/// This is provider-neutral - it captures the structure without
/// assuming any particular git hosting platform.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActionRef {
    /// Optional explicit host (e.g., "github.com", "ghe.mycorp.internal")
    /// If None, uses the default provider's host
    host: Option<String>,

    /// Owner or organization (e.g., "actions", "my-org")
    owner: String,

    /// Repository name (e.g., "checkout", "setup-node")
    repo: String,

    /// Optional subpath within the repo (e.g., ".github/workflows/reusable.yml")
    /// Used for reusable workflows or actions in subdirectories
    subpath: Option<String>,

    /// The ref (version) - tag, branch, or commit SHA
    ref_str: String,
}

impl ActionRef {
    /// Create a new `ActionRef` with all fields
    #[must_use]
    pub fn new(
        host: Option<String>,
        owner: impl Into<String>,
        repo: impl Into<String>,
        subpath: Option<String>,
        ref_str: impl Into<String>,
    ) -> Self {
        Self {
            host,
            owner: owner.into(),
            repo: repo.into(),
            subpath,
            ref_str: ref_str.into(),
        }
    }

    /// Create a simple `ActionRef` without host or subpath
    #[must_use]
    pub fn simple(
        owner: impl Into<String>,
        repo: impl Into<String>,
        ref_str: impl Into<String>,
    ) -> Self {
        Self::new(None, owner, repo, None, ref_str)
    }

    /// Get the host as an optional string slice
    #[must_use]
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// Get the owner as a string slice
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Get the repo as a string slice
    #[must_use]
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// Get the subpath as an optional string slice
    #[must_use]
    pub fn subpath(&self) -> Option<&str> {
        self.subpath.as_deref()
    }

    /// Get the ref string as a string slice
    #[must_use]
    pub fn ref_str(&self) -> &str {
        &self.ref_str
    }

    /// Returns true if this ref looks like a commit SHA (40 hex chars)
    #[must_use]
    pub fn is_sha(&self) -> bool {
        self.ref_str.len() == 40 && self.ref_str.chars().all(|c| c.is_ascii_hexdigit())
    }

    /// Returns true if this ref looks like a short SHA (7+ hex chars, not 40)
    #[must_use]
    pub fn is_short_sha(&self) -> bool {
        let len = self.ref_str.len();
        (7..40).contains(&len) && self.ref_str.chars().all(|c| c.is_ascii_hexdigit())
    }

    /// Returns true if this looks like a semver tag (v1, v1.2, v1.2.3, v1.0.0-beta, etc.)
    #[must_use]
    pub fn looks_like_semver(&self) -> bool {
        let s = self.ref_str.strip_prefix('v').unwrap_or(&self.ref_str);

        // Split off prerelease suffix if present
        let version_part = s.split('-').next().unwrap_or(s);

        // Check if version part starts with a digit and contains only digits and dots
        version_part
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
            && version_part.chars().all(|c| c.is_ascii_digit() || c == '.')
    }

    /// Convert to `ActionRepoId` (drops the ref portion)
    #[must_use]
    pub fn to_repo_id(&self) -> ActionRepoId {
        ActionRepoId {
            host: self.host.clone(),
            owner: self.owner.clone(),
            repo: self.repo.clone(),
            subpath: self.subpath.clone(),
        }
    }

    /// Format as a `uses:` string
    #[must_use]
    pub fn to_uses_string(&self) -> String {
        let mut s = String::new();
        if let Some(h) = &self.host {
            s.push_str(h);
            s.push('/');
        }
        s.push_str(&self.owner);
        s.push('/');
        s.push_str(&self.repo);
        if let Some(p) = &self.subpath {
            s.push('/');
            s.push_str(p);
        }
        s.push('@');
        s.push_str(&self.ref_str);
        s
    }
}

impl fmt::Display for ActionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_uses_string())
    }
}

/// Identifies an action repository (without the ref/version)
///
/// Used as a cache key and for provider lookups.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActionRepoId {
    /// Optional explicit host
    host: Option<String>,

    /// Owner or organization
    owner: String,

    /// Repository name
    repo: String,

    /// Optional subpath (for composite actions in subdirs)
    subpath: Option<String>,
}

impl ActionRepoId {
    /// Create a new `ActionRepoId`
    #[must_use]
    pub fn new(
        host: Option<String>,
        owner: impl Into<String>,
        repo: impl Into<String>,
        subpath: Option<String>,
    ) -> Self {
        Self {
            host,
            owner: owner.into(),
            repo: repo.into(),
            subpath,
        }
    }

    /// Simple constructor without host/subpath
    #[must_use]
    pub fn simple(owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self::new(None, owner, repo, None)
    }

    /// Get the host as an optional string slice
    #[must_use]
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// Get the owner as a string slice
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Get the repo as a string slice
    #[must_use]
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// Get the subpath as an optional string slice
    #[must_use]
    pub fn subpath(&self) -> Option<&str> {
        self.subpath.as_deref()
    }

    /// Create a cache key string
    ///
    /// Format: `<provider_namespace>::<host>/<owner>/<repo>[/subpath]`
    #[must_use]
    pub fn cache_key(&self, provider_namespace: &str, default_host: &str) -> String {
        let host = self.host.as_deref().unwrap_or(default_host);
        self.subpath.as_ref().map_or_else(
            || format!("{provider_namespace}::{host}/{}/{}", self.owner, self.repo),
            |p| {
                format!(
                    "{provider_namespace}::{host}/{}/{}/{p}",
                    self.owner, self.repo
                )
            },
        )
    }
}

impl fmt::Display for ActionRepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(h) = &self.host {
            write!(f, "{h}/")?;
        }
        write!(f, "{}/{}", self.owner, self.repo)?;
        if let Some(p) = &self.subpath {
            write!(f, "/{p}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ActionRef construction tests

    #[test]
    fn test_action_ref_new() {
        let action = ActionRef::new(
            Some("github.com".to_string()),
            "actions",
            "checkout",
            Some("v4".to_string()),
            "v4.1.0",
        );
        assert_eq!(action.host(), Some("github.com"));
        assert_eq!(action.owner(), "actions");
        assert_eq!(action.repo(), "checkout");
        assert_eq!(action.subpath(), Some("v4"));
        assert_eq!(action.ref_str(), "v4.1.0");
    }

    #[test]
    fn test_action_ref_simple() {
        let action = ActionRef::simple("actions", "checkout", "v4");
        assert!(action.host().is_none());
        assert_eq!(action.owner(), "actions");
        assert_eq!(action.repo(), "checkout");
        assert!(action.subpath().is_none());
        assert_eq!(action.ref_str(), "v4");
    }

    // ActionRef::is_sha tests

    #[test]
    fn test_is_sha() {
        let sha_ref = ActionRef::simple("owner", "repo", "a".repeat(40));
        assert!(sha_ref.is_sha());

        let tag_ref = ActionRef::simple("owner", "repo", "v1.2.3");
        assert!(!tag_ref.is_sha());
    }

    #[test]
    fn test_is_sha_mixed_case() {
        let sha = ActionRef::simple("owner", "repo", "aAbBcCdDeEfF0123456789aAbBcCdDeEfF012345");
        assert!(sha.is_sha());
    }

    #[test]
    fn test_is_sha_invalid_chars() {
        let invalid = ActionRef::simple("owner", "repo", "g".repeat(40)); // g is not hex
        assert!(!invalid.is_sha());
    }

    // ActionRef::is_short_sha tests

    #[test]
    fn test_is_short_sha() {
        let short = ActionRef::simple("owner", "repo", "abc1234");
        assert!(short.is_short_sha());

        let full = ActionRef::simple("owner", "repo", "a".repeat(40));
        assert!(!full.is_short_sha());
    }

    #[test]
    fn test_is_short_sha_too_short() {
        let too_short = ActionRef::simple("owner", "repo", "abc123"); // 6 chars
        assert!(!too_short.is_short_sha());
    }

    #[test]
    fn test_is_short_sha_boundary() {
        let seven = ActionRef::simple("owner", "repo", "a".repeat(7));
        assert!(seven.is_short_sha());

        let thirty_nine = ActionRef::simple("owner", "repo", "a".repeat(39));
        assert!(thirty_nine.is_short_sha());
    }

    // ActionRef::looks_like_semver tests

    #[test]
    fn test_looks_like_semver() {
        assert!(ActionRef::simple("o", "r", "v1").looks_like_semver());
        assert!(ActionRef::simple("o", "r", "v1.2").looks_like_semver());
        assert!(ActionRef::simple("o", "r", "v1.2.3").looks_like_semver());
        assert!(ActionRef::simple("o", "r", "1.2.3").looks_like_semver());
        assert!(!ActionRef::simple("o", "r", "main").looks_like_semver());
        assert!(!ActionRef::simple("o", "r", "release/v1").looks_like_semver());
    }

    #[test]
    fn test_looks_like_semver_prerelease() {
        assert!(ActionRef::simple("o", "r", "v1.0.0-beta").looks_like_semver());
        assert!(ActionRef::simple("o", "r", "v1.0.0-rc.1").looks_like_semver());
        assert!(ActionRef::simple("o", "r", "v2.0.0-alpha.2").looks_like_semver());
    }

    // ActionRef::to_repo_id tests

    #[test]
    fn test_to_repo_id() {
        let action = ActionRef::new(
            Some("ghe.corp.com".to_string()),
            "org",
            "repo",
            Some("path/to/action".to_string()),
            "v1.0.0",
        );
        let id = action.to_repo_id();

        assert_eq!(id.host(), Some("ghe.corp.com"));
        assert_eq!(id.owner(), "org");
        assert_eq!(id.repo(), "repo");
        assert_eq!(id.subpath(), Some("path/to/action"));
    }

    // ActionRef::to_uses_string tests

    #[test]
    fn test_to_uses_string() {
        let simple = ActionRef::simple("actions", "checkout", "v4");
        assert_eq!(simple.to_uses_string(), "actions/checkout@v4");

        let with_host = ActionRef::new(Some("ghe.corp.com".into()), "org", "action", None, "v1");
        assert_eq!(with_host.to_uses_string(), "ghe.corp.com/org/action@v1");

        let with_subpath = ActionRef::new(
            None,
            "org",
            "repo",
            Some(".github/workflows/ci.yml".into()),
            "main",
        );
        assert_eq!(
            with_subpath.to_uses_string(),
            "org/repo/.github/workflows/ci.yml@main"
        );
    }

    #[test]
    fn test_to_uses_string_full() {
        let full = ActionRef::new(
            Some("ghe.corp.com".to_string()),
            "org",
            "monorepo",
            Some("actions/deploy".to_string()),
            "v2.0.0",
        );
        assert_eq!(
            full.to_uses_string(),
            "ghe.corp.com/org/monorepo/actions/deploy@v2.0.0"
        );
    }

    // ActionRef Display tests

    #[test]
    fn test_action_ref_display() {
        let action = ActionRef::simple("actions", "checkout", "v4");
        assert_eq!(format!("{action}"), "actions/checkout@v4");
    }

    // ActionRepoId tests

    #[test]
    fn test_action_repo_id_new() {
        let id = ActionRepoId::new(Some("github.com".to_string()), "actions", "checkout", None);
        assert_eq!(id.host(), Some("github.com"));
        assert_eq!(id.owner(), "actions");
        assert_eq!(id.repo(), "checkout");
    }

    #[test]
    fn test_action_repo_id_simple() {
        let id = ActionRepoId::simple("actions", "checkout");
        assert!(id.host().is_none());
        assert!(id.subpath().is_none());
    }

    #[test]
    fn test_cache_key() {
        let id = ActionRepoId::simple("actions", "checkout");
        assert_eq!(
            id.cache_key("github", "github.com"),
            "github::github.com/actions/checkout"
        );

        let with_host = ActionRepoId::new(Some("ghe.corp.com".into()), "org", "action", None);
        assert_eq!(
            with_host.cache_key("github", "github.com"),
            "github::ghe.corp.com/org/action"
        );
    }

    #[test]
    fn test_cache_key_with_subpath() {
        let id = ActionRepoId::new(None, "org", "monorepo", Some("actions/build".to_string()));
        let key = id.cache_key("github", "github.com");
        assert!(key.contains("actions/build"));
    }

    #[test]
    fn test_action_repo_id_display() {
        let id = ActionRepoId::simple("actions", "checkout");
        assert_eq!(format!("{id}"), "actions/checkout");

        let with_host = ActionRepoId::new(Some("ghe.corp.com".to_string()), "org", "action", None);
        assert_eq!(format!("{with_host}"), "ghe.corp.com/org/action");
    }

    #[test]
    fn test_action_ref_serde_round_trip() {
        let action = ActionRef::new(
            Some("github.com".to_string()),
            "actions",
            "checkout",
            Some(".github/workflows/ci.yml".to_string()),
            "v4.1.0",
        );
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: ActionRef = serde_json::from_str(&json).unwrap();
        assert_eq!(action, deserialized);
    }

    #[test]
    fn test_action_ref_serde_round_trip_minimal() {
        let action = ActionRef::new(None, "owner", "repo", None, "main");
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: ActionRef = serde_json::from_str(&json).unwrap();
        assert_eq!(action, deserialized);
    }

    #[test]
    fn test_action_repo_id_serde_round_trip() {
        let id = ActionRepoId::new(
            Some("ghe.corp.com".to_string()),
            "org",
            "action",
            Some("sub/path".to_string()),
        );
        let json = serde_json::to_string(&id).unwrap();
        let deserialized: ActionRepoId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, deserialized);
    }
}
