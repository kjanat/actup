//! Types for representing GitHub Actions references
//!
//! An action reference (from `uses:` in workflows) can be:
//! - `owner/repo@ref`
//! - `owner/repo/path@ref` (subpath for reusable workflows or nested actions)
//! - `host/owner/repo@ref` (explicit host for enterprise/self-hosted)
//! - `./path` (local, not handled for version checks)

use std::fmt;

/// A parsed action reference from a `uses:` statement
///
/// This is provider-neutral - it captures the structure without
/// assuming any particular git hosting platform.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActionRef {
    /// Optional explicit host (e.g., "github.com", "ghe.mycorp.internal")
    /// If None, uses the default provider's host
    pub host: Option<String>,

    /// Owner or organization (e.g., "actions", "my-org")
    pub owner: String,

    /// Repository name (e.g., "checkout", "setup-node")
    pub repo: String,

    /// Optional subpath within the repo (e.g., ".github/workflows/reusable.yml")
    /// Used for reusable workflows or actions in subdirectories
    pub subpath: Option<String>,

    /// The ref (version) - tag, branch, or commit SHA
    pub ref_str: String,
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

    /// Returns true if this looks like a semver tag (v1, v1.2, v1.2.3, etc.)
    #[must_use]
    pub fn looks_like_semver(&self) -> bool {
        let s = self.ref_str.strip_prefix('v').unwrap_or(&self.ref_str);
        // Check if it starts with a digit and contains only digits and dots
        s.chars().next().is_some_and(|c| c.is_ascii_digit())
            && s.chars().all(|c| c.is_ascii_digit() || c == '.')
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActionRepoId {
    /// Optional explicit host
    pub host: Option<String>,

    /// Owner or organization
    pub owner: String,

    /// Repository name
    pub repo: String,

    /// Optional subpath (for composite actions in subdirs)
    pub subpath: Option<String>,
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
mod tests {
    use super::*;

    #[test]
    fn test_is_sha() {
        let sha_ref = ActionRef::simple("owner", "repo", "a".repeat(40));
        assert!(sha_ref.is_sha());

        let tag_ref = ActionRef::simple("owner", "repo", "v1.2.3");
        assert!(!tag_ref.is_sha());
    }

    #[test]
    fn test_is_short_sha() {
        let short = ActionRef::simple("owner", "repo", "abc1234");
        assert!(short.is_short_sha());

        let full = ActionRef::simple("owner", "repo", "a".repeat(40));
        assert!(!full.is_short_sha());
    }

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
}
