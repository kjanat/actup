//! Version types and semver handling
//!
//! Represents versions of actions, supporting:
//! - Semantic version tags (v1, v1.2, v1.2.3)
//! - Commit SHAs
//! - Branch names

use std::{cmp::Ordering, fmt, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::clock::{Clock, SystemClock};

/// A parsed version from a git ref
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Version {
    /// Semantic version (major, minor, patch)
    /// Minor and patch are optional (v1 vs v1.2 vs v1.2.3)
    Semver {
        /// Major version number
        major: u32,
        /// Minor version (None for major-only like "v1")
        minor: Option<u32>,
        /// Patch version (None for major.minor like "v1.2")
        patch: Option<u32>,
        /// Pre-release suffix (e.g., "beta", "rc.1")
        prerelease: Option<String>,
        /// Original string representation (preserves "v" prefix, etc.)
        original: String,
    },

    /// Full 40-character commit SHA
    Sha {
        /// The 40-character hex string
        sha: String,
    },

    /// Branch name (not recommended for stability)
    Branch {
        /// Branch name (e.g., "main", "release/v1")
        name: String,
    },

    /// Other non-semver tag (date-based, custom naming, etc.)
    Other {
        /// The tag name
        tag: String,
    },
}

impl Version {
    /// Parse a ref string into a Version
    #[must_use]
    pub fn parse(ref_str: &str) -> Self {
        // Check for SHA first (40 hex chars)
        if ref_str.len() == 40 && ref_str.chars().all(|c| c.is_ascii_hexdigit()) {
            return Self::Sha {
                sha: ref_str.to_string(),
            };
        }

        // Try to parse as semver
        if let Some(semver) = Self::try_parse_semver(ref_str) {
            return semver;
        }

        // Could be a branch or other tag - we can't distinguish without context
        // Default to treating it as Other (tag)
        Self::Other {
            tag: ref_str.to_string(),
        }
    }

    /// Try to parse a string as semver
    fn try_parse_semver(s: &str) -> Option<Self> {
        let version_str = s.strip_prefix('v').unwrap_or(s);

        // First, split off prerelease suffix (everything after first '-')
        let (version_part, prerelease) = version_str
            .split_once('-')
            .map_or((version_str, None), |(v, p)| (v, Some(p.to_string())));

        // Split by '.' and parse numbers
        let mut parts = version_part.split('.');

        let major_str = parts.next()?;
        let major: u32 = major_str.parse().ok()?;

        let minor = parts.next().and_then(|s| s.parse().ok());
        let patch = parts.next().and_then(|s| s.parse().ok());

        Some(Self::Semver {
            major,
            minor,
            patch,
            prerelease,
            original: s.to_string(),
        })
    }

    /// Returns true if this is a semver version
    #[must_use]
    pub const fn is_semver(&self) -> bool {
        matches!(self, Self::Semver { .. })
    }

    /// Returns true if this is a full SHA
    #[must_use]
    pub const fn is_sha(&self) -> bool {
        matches!(self, Self::Sha { .. })
    }

    /// Returns true if this is a branch
    #[must_use]
    pub const fn is_branch(&self) -> bool {
        matches!(self, Self::Branch { .. })
    }

    /// Returns true if this is a prerelease version
    #[must_use]
    pub const fn is_prerelease(&self) -> bool {
        matches!(
            self,
            Self::Semver {
                prerelease: Some(_),
                ..
            }
        )
    }

    /// Returns true if this is a major-only version (e.g., v6 with no minor/patch)
    #[must_use]
    pub const fn is_major_only(&self) -> bool {
        matches!(
            self,
            Self::Semver {
                minor: None,
                patch: None,
                prerelease: None,
                ..
            }
        )
    }

    /// Get the major version if this is semver
    #[must_use]
    pub const fn major(&self) -> Option<u32> {
        match self {
            Self::Semver { major, .. } => Some(*major),
            _ => None,
        }
    }

    /// Get the minor version if this is semver with minor
    #[must_use]
    pub const fn minor(&self) -> Option<u32> {
        match self {
            Self::Semver { minor, .. } => *minor,
            _ => None,
        }
    }

    /// Get the patch version if this is semver with patch
    #[must_use]
    pub const fn patch(&self) -> Option<u32> {
        match self {
            Self::Semver { patch, .. } => *patch,
            _ => None,
        }
    }

    /// Get the original string representation
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Semver { original, .. } => original,
            Self::Sha { sha } => sha,
            Self::Branch { name } => name,
            Self::Other { tag } => tag,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            // Semver comparison
            (
                Self::Semver {
                    major: m1,
                    minor: mi1,
                    patch: p1,
                    prerelease: pr1,
                    ..
                },
                Self::Semver {
                    major: m2,
                    minor: mi2,
                    patch: p2,
                    prerelease: pr2,
                    ..
                },
            ) => {
                match m1.cmp(m2) {
                    Ordering::Equal => {}
                    ord => return ord,
                }
                // Treat None as 0 for comparison purposes
                let mi1 = mi1.unwrap_or(0);
                let mi2 = mi2.unwrap_or(0);
                match mi1.cmp(&mi2) {
                    Ordering::Equal => {}
                    ord => return ord,
                }
                let p1 = p1.unwrap_or(0);
                let p2 = p2.unwrap_or(0);
                match p1.cmp(&p2) {
                    Ordering::Equal => {}
                    ord => return ord,
                }
                // Prerelease: None > Some (release > prerelease)
                match (pr1, pr2) {
                    (None, None) => Ordering::Equal,
                    (None, Some(_)) => Ordering::Greater,
                    (Some(_), None) => Ordering::Less,
                    (Some(a), Some(b)) => a.cmp(b),
                }
            }

            // SHA comparison (lexicographic)
            (Self::Sha { sha: s1 }, Self::Sha { sha: s2 }) => s1.cmp(s2),

            // Branch comparison (lexicographic)
            (Self::Branch { name: n1 }, Self::Branch { name: n2 }) => n1.cmp(n2),

            // Other comparison (lexicographic)
            (Self::Other { tag: t1 }, Self::Other { tag: t2 }) => t1.cmp(t2),

            // Cross-type: Semver > Sha > Branch > Other
            (Self::Semver { .. }, _) | (Self::Branch { .. }, Self::Other { .. }) => {
                Ordering::Greater
            }
            (_, Self::Semver { .. }) | (Self::Other { .. }, Self::Branch { .. }) => Ordering::Less,
            (Self::Sha { .. }, _) => Ordering::Greater,
            (_, Self::Sha { .. }) => Ordering::Less,
        }
    }
}

/// A set of available versions for an action repository
///
/// Uses `Arc<Info>` internally for cheap cloning when the same version
/// appears in multiple collections (e.g., both `semver_tags` and `latest`).
/// Arc is used instead of Rc because Provider requires Send + Sync.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Set {
    /// All available semver versions, sorted
    semver_tags: Vec<Arc<Info>>,

    /// Latest version per major
    ///
    /// Examples:
    /// - `v1 -> v1.2.3`
    /// - `v2 -> v2.0.1`
    latest_per_major: std::collections::HashMap<u32, Arc<Info>>,

    /// Major-only tags that exist
    ///
    /// Examples:
    /// - `v1`
    /// - `v2`
    /// - `v6`
    ///
    /// Maps major version number to the [`Info`] for the major-only tag\
    /// Used to suggest major-only refs when user already uses major-only style
    major_tags: std::collections::HashMap<u32, Arc<Info>>,

    /// Overall latest semver version
    latest: Option<Arc<Info>>,

    /// Default branch info
    default_branch: Option<BranchInfo>,

    /// When this data was fetched (skipped in serialization — platform-dependent)
    #[serde(skip)]
    fetched_at: Option<std::time::SystemTime>,
}

/// Information about a specific version
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    /// The parsed version
    version: Version,

    /// The commit SHA this version points to
    sha: String,
}

impl Info {
    /// Create a new [`Info`]
    #[must_use]
    pub fn new(version: Version, sha: impl Into<String>) -> Self {
        Self {
            version,
            sha: sha.into(),
        }
    }

    /// Get the version
    #[must_use]
    pub const fn version(&self) -> &Version {
        &self.version
    }

    /// Get the commit SHA
    #[must_use]
    pub fn sha(&self) -> &str {
        &self.sha
    }
}

/// Information about a branch
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchInfo {
    /// Branch name
    name: String,

    /// Current HEAD commit SHA
    sha: String,
}

impl BranchInfo {
    /// Create a new `BranchInfo`
    #[must_use]
    pub fn new(name: impl Into<String>, sha: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            sha: sha.into(),
        }
    }

    /// Get the branch name
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the commit SHA
    #[must_use]
    pub fn sha(&self) -> &str {
        &self.sha
    }
}

impl Set {
    /// Create an empty [`Set`]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a version set with all fields (uses [`SystemClock`]).
    ///
    /// This is the primary constructor for providers building version information.
    /// Use [`Set::new()`] for an empty set.
    #[must_use]
    pub fn build(
        semver_tags: Vec<Arc<Info>>,
        latest_per_major: std::collections::HashMap<u32, Arc<Info>>,
        major_tags: std::collections::HashMap<u32, Arc<Info>>,
        latest: Option<Arc<Info>>,
    ) -> Self {
        Self::build_with_clock(
            semver_tags,
            latest_per_major,
            major_tags,
            latest,
            &SystemClock,
        )
    }

    /// Build a version set using the provided [`Clock`] for the `fetched_at` timestamp.
    #[must_use]
    pub fn build_with_clock(
        semver_tags: Vec<Arc<Info>>,
        latest_per_major: std::collections::HashMap<u32, Arc<Info>>,
        major_tags: std::collections::HashMap<u32, Arc<Info>>,
        latest: Option<Arc<Info>>,
        clock: &dyn Clock,
    ) -> Self {
        Self {
            semver_tags,
            latest_per_major,
            major_tags,
            latest,
            default_branch: None,
            fetched_at: Some(clock.now()),
        }
    }

    /// Get all semver tags
    #[must_use]
    pub fn semver_tags(&self) -> &[Arc<Info>] {
        &self.semver_tags
    }

    /// Get the latest version per major
    #[must_use]
    pub const fn latest_per_major(&self) -> &std::collections::HashMap<u32, Arc<Info>> {
        &self.latest_per_major
    }

    /// Get major-only tags that exist
    #[must_use]
    pub const fn major_tags(&self) -> &std::collections::HashMap<u32, Arc<Info>> {
        &self.major_tags
    }

    /// Get the overall latest semver version
    #[must_use]
    pub const fn latest(&self) -> Option<&Arc<Info>> {
        self.latest.as_ref()
    }

    /// Get the default branch info
    #[must_use]
    pub const fn default_branch(&self) -> Option<&BranchInfo> {
        self.default_branch.as_ref()
    }

    /// Get when this data was fetched
    #[must_use]
    pub const fn fetched_at(&self) -> Option<std::time::SystemTime> {
        self.fetched_at
    }

    /// Check if a tag with this exact string exists in the known tags.
    ///
    /// Prevents false "up to date" when user writes `6` but only `v6` exists.
    #[must_use]
    pub fn has_exact_tag(&self, ref_str: &str) -> bool {
        self.semver_tags
            .iter()
            .any(|t| t.version().as_str() == ref_str)
    }

    /// Find the latest version that satisfies the given policy for a current version
    #[must_use]
    pub fn find_latest_for(
        &self,
        current: &Version,
        allow_major: bool,
        allow_prerelease: bool,
    ) -> Option<&Arc<Info>> {
        match current {
            Version::Semver { major, .. } => {
                if allow_major {
                    // Return overall latest (filtered by prerelease if needed)
                    self.latest
                        .as_ref()
                        .filter(|v| allow_prerelease || !v.version().is_prerelease())
                } else {
                    // Return latest for this major
                    self.latest_per_major
                        .get(major)
                        .filter(|v| allow_prerelease || !v.version().is_prerelease())
                }
            }
            _ => {
                // For non-semver, just return latest if major updates allowed
                if allow_major {
                    self.latest.as_ref()
                } else {
                    None
                }
            }
        }
    }

    /// Set the default branch information
    pub fn set_default_branch(&mut self, branch: BranchInfo) {
        self.default_branch = Some(branch);
    }

    /// Builder-style setter for default branch
    #[must_use]
    pub fn with_default_branch(mut self, branch: BranchInfo) -> Self {
        self.default_branch = Some(branch);
        self
    }

    /// Get mutable access to major tags (for providers building version sets)
    pub const fn major_tags_mut(&mut self) -> &mut std::collections::HashMap<u32, Arc<Info>> {
        &mut self.major_tags
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    // Version::parse tests

    #[test]
    fn test_parse_semver_major_only() {
        let v = Version::parse("v1");
        assert!(matches!(
            v,
            Version::Semver {
                major: 1,
                minor: None,
                patch: None,
                ..
            }
        ));
    }

    #[test]
    fn test_parse_semver_major_minor() {
        let v = Version::parse("v1.2");
        assert!(matches!(
            v,
            Version::Semver {
                major: 1,
                minor: Some(2),
                patch: None,
                ..
            }
        ));
    }

    #[test]
    fn test_parse_semver_full() {
        let v = Version::parse("v1.2.3");
        assert!(matches!(
            v,
            Version::Semver {
                major: 1,
                minor: Some(2),
                patch: Some(3),
                ..
            }
        ));
    }

    #[test]
    fn test_parse_semver_without_v_prefix() {
        let v = Version::parse("1.2.3");
        assert!(matches!(
            v,
            Version::Semver {
                major: 1,
                minor: Some(2),
                patch: Some(3),
                ..
            }
        ));
    }

    #[test]
    fn test_parse_semver_with_prerelease() {
        let v = Version::parse("v1.0.0-beta.1");
        match v {
            Version::Semver { prerelease, .. } => {
                assert_eq!(prerelease, Some("beta.1".to_string()));
            }
            _ => panic!("expected Semver"),
        }

        // Also test simple prerelease
        let v = Version::parse("v2.0.0-rc");
        match v {
            Version::Semver { prerelease, .. } => {
                assert_eq!(prerelease, Some("rc".to_string()));
            }
            _ => panic!("expected Semver"),
        }
    }

    #[test]
    fn test_parse_sha() {
        let sha = "a".repeat(40);
        let v = Version::parse(&sha);
        assert!(matches!(v, Version::Sha { .. }));
        assert!(v.is_sha());
    }

    #[test]
    fn test_parse_sha_mixed_case() {
        let sha = "aAbBcCdDeEfF0123456789aAbBcCdDeEfF012345";
        let v = Version::parse(sha);
        assert!(v.is_sha());
    }

    #[test]
    fn test_parse_other() {
        let v = Version::parse("main");
        assert!(matches!(v, Version::Other { .. }));
    }

    #[test]
    fn test_parse_short_sha_is_other() {
        // Short SHAs should be treated as Other, not Sha
        let v = Version::parse("abc123");
        assert!(matches!(v, Version::Other { .. }));
    }

    // Version accessor tests

    #[test]
    fn test_version_is_semver() {
        assert!(Version::parse("v1.0.0").is_semver());
        assert!(!Version::parse("main").is_semver());
        assert!(!Version::parse(&"a".repeat(40)).is_semver());
    }

    #[test]
    fn test_version_is_sha() {
        assert!(Version::parse(&"a".repeat(40)).is_sha());
        assert!(!Version::parse("v1.0.0").is_sha());
    }

    #[test]
    fn test_version_is_branch() {
        let branch = Version::Branch {
            name: "main".to_string(),
        };
        assert!(branch.is_branch());
        assert!(!Version::parse("v1.0.0").is_branch());
    }

    #[test]
    fn test_version_is_prerelease() {
        assert!(Version::parse("v1.0.0-beta").is_prerelease());
        assert!(Version::parse("v1.0.0-rc.1").is_prerelease());
        assert!(!Version::parse("v1.0.0").is_prerelease());
    }

    #[test]
    fn test_version_major() {
        assert_eq!(Version::parse("v4.1.2").major(), Some(4));
        assert_eq!(Version::parse("v1").major(), Some(1));
        assert_eq!(Version::parse("main").major(), None);
    }

    #[test]
    fn test_version_minor() {
        assert_eq!(Version::parse("v4.1.2").minor(), Some(1));
        assert_eq!(Version::parse("v4").minor(), None);
    }

    #[test]
    fn test_version_patch() {
        assert_eq!(Version::parse("v4.1.2").patch(), Some(2));
        assert_eq!(Version::parse("v4.1").patch(), None);
    }

    #[test]
    fn test_is_major_only_true() {
        assert!(Version::parse("v1").is_major_only());
        assert!(Version::parse("v6").is_major_only());
        assert!(Version::parse("v42").is_major_only());
    }

    #[test]
    fn test_is_major_only_false_with_minor() {
        assert!(!Version::parse("v1.2").is_major_only());
        assert!(!Version::parse("v1.0").is_major_only());
    }

    #[test]
    fn test_is_major_only_false_with_patch() {
        assert!(!Version::parse("v1.2.3").is_major_only());
        assert!(!Version::parse("v1.0.0").is_major_only());
    }

    #[test]
    fn test_is_major_only_false_with_prerelease() {
        assert!(!Version::parse("v1.0.0-beta").is_major_only());
        assert!(!Version::parse("v2.0.0-rc.1").is_major_only());
    }

    #[test]
    fn test_version_as_str() {
        assert_eq!(Version::parse("v1.2.3").as_str(), "v1.2.3");
        let sha = "a".repeat(40);
        assert_eq!(Version::parse(&sha).as_str(), sha);
    }

    #[test]
    fn test_version_display() {
        assert_eq!(Version::parse("v1.2.3").to_string(), "v1.2.3");
    }

    // Version ordering tests

    #[test]
    fn test_semver_ordering_major() {
        let v1 = Version::parse("v1.0.0");
        let v2 = Version::parse("v2.0.0");
        assert!(v2 > v1);
    }

    #[test]
    fn test_semver_ordering_minor() {
        let v1_1 = Version::parse("v1.1.0");
        let v1_2 = Version::parse("v1.2.0");
        assert!(v1_2 > v1_1);
    }

    #[test]
    fn test_semver_ordering_patch() {
        let v1_0_1 = Version::parse("v1.0.1");
        let v1_0_2 = Version::parse("v1.0.2");
        assert!(v1_0_2 > v1_0_1);
    }

    #[test]
    fn test_prerelease_ordering() {
        let release = Version::parse("v1.0.0");
        let beta = Version::parse("v1.0.0-beta");
        assert!(release > beta); // release > prerelease
    }

    #[test]
    fn test_prerelease_alphabetic_ordering() {
        let alpha = Version::parse("v1.0.0-alpha");
        let beta = Version::parse("v1.0.0-beta");
        assert!(beta > alpha);
    }

    #[test]
    fn test_sha_ordering() {
        let sha1 = Version::Sha {
            sha: "a".repeat(40),
        };
        let sha2 = Version::Sha {
            sha: "b".repeat(40),
        };
        assert!(sha2 > sha1);
    }

    #[test]
    fn test_cross_type_ordering() {
        let semver = Version::parse("v1.0.0");
        let sha = Version::Sha {
            sha: "a".repeat(40),
        };
        let branch = Version::Branch {
            name: "main".to_string(),
        };
        let other = Version::Other {
            tag: "latest".to_string(),
        };

        // Semver > Sha > Branch > Other
        assert!(semver > sha);
        assert!(sha > branch);
        assert!(branch > other);
    }

    // version::Info tests

    #[test]
    fn test_version_info_new() {
        let info = Info::new(Version::parse("v1.0.0"), "abc123");
        assert_eq!(info.version().to_string(), "v1.0.0");
        assert_eq!(info.sha(), "abc123");
    }

    // BranchInfo tests

    #[test]
    fn test_branch_info() {
        let info = BranchInfo {
            name: "main".to_string(),
            sha: "abc123".to_string(),
        };
        assert_eq!(info.name(), "main");
        assert_eq!(info.sha(), "abc123");
    }

    // [`version::Set`] tests

    #[test]
    fn test_version_set_new() {
        let vs = Set::new();
        assert!(vs.semver_tags().is_empty());
        assert!(vs.latest().is_none());
        assert!(vs.default_branch().is_none());
    }

    #[test]
    fn test_version_set_build() {
        let v1 = Arc::new(Info::new(Version::parse("v1.0.0"), "aaa"));
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(1, Arc::clone(&v1));

        let vs = Set::build(
            vec![Arc::clone(&v1)],
            latest_per_major.clone(),
            HashMap::new(),
            Some(v1),
        );

        assert_eq!(vs.semver_tags().len(), 1);
        assert_eq!(vs.latest_per_major().len(), 1);
        assert!(vs.latest().is_some());
        assert!(vs.fetched_at().is_some());
    }

    #[test]
    fn test_find_latest_for_major_allowed() {
        let v3 = Arc::new(Info::new(Version::parse("v3.0.0"), "aaa"));
        let v4 = Arc::new(Info::new(Version::parse("v4.0.0"), "bbb"));

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(3, Arc::clone(&v3));
        latest_per_major.insert(4, Arc::clone(&v4));

        let vs = Set {
            semver_tags: vec![],
            latest_per_major,
            major_tags: HashMap::new(),
            latest: Some(v4),
            default_branch: None,
            fetched_at: None,
        };

        let current = Version::parse("v3.0.0");
        let result = vs.find_latest_for(&current, true, false);
        assert!(result.is_some());
        assert_eq!(result.unwrap().version.to_string(), "v4.0.0");
    }

    #[test]
    fn test_find_latest_for_major_not_allowed() {
        let v3_1 = Arc::new(Info::new(Version::parse("v3.1.0"), "aaa"));
        let v4 = Arc::new(Info::new(Version::parse("v4.0.0"), "bbb"));

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(3, Arc::clone(&v3_1));
        latest_per_major.insert(4, Arc::clone(&v4));

        let vs = Set {
            semver_tags: vec![],
            latest_per_major,
            major_tags: HashMap::new(),
            latest: Some(v4),
            default_branch: None,
            fetched_at: None,
        };

        let current = Version::parse("v3.0.0");
        let result = vs.find_latest_for(&current, false, false);
        assert!(result.is_some());
        assert_eq!(result.unwrap().version.to_string(), "v3.1.0");
    }

    #[test]
    fn test_find_latest_for_non_semver() {
        let v4 = Arc::new(Info::new(Version::parse("v4.0.0"), "bbb"));

        let vs = Set {
            semver_tags: vec![],
            latest_per_major: HashMap::new(),
            major_tags: HashMap::new(),
            latest: Some(v4),
            default_branch: None,
            fetched_at: None,
        };

        // Non-semver current version
        let current = Version::parse("main");

        // With allow_major, returns latest
        let result = vs.find_latest_for(&current, true, false);
        assert!(result.is_some());

        // Without allow_major, returns None
        let result = vs.find_latest_for(&current, false, false);
        assert!(result.is_none());
    }

    #[test]
    fn test_find_latest_for_filters_prerelease() {
        let beta = Arc::new(Info::new(Version::parse("v4.0.0-beta"), "aaa"));

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, Arc::clone(&beta));

        let vs = Set {
            semver_tags: vec![],
            latest_per_major,
            major_tags: HashMap::new(),
            latest: Some(beta),
            default_branch: None,
            fetched_at: None,
        };

        let current = Version::parse("v4.0.0");

        // Without allow_prerelease, filters out beta
        let result = vs.find_latest_for(&current, false, false);
        assert!(result.is_none());

        // With allow_prerelease, includes beta
        let result = vs.find_latest_for(&current, false, true);
        assert!(result.is_some());
    }

    #[test]
    fn test_set_default_branch() {
        let mut vs = Set::new();
        assert!(vs.default_branch().is_none());

        let branch = BranchInfo {
            name: "main".to_string(),
            sha: "abc123".to_string(),
        };
        vs.set_default_branch(branch);

        assert!(vs.default_branch().is_some());
        assert_eq!(vs.default_branch().unwrap().name(), "main");
        assert_eq!(vs.default_branch().unwrap().sha(), "abc123");
    }

    #[test]
    fn test_with_default_branch() {
        let branch = BranchInfo {
            name: "develop".to_string(),
            sha: "def456".to_string(),
        };

        let vs = Set::new().with_default_branch(branch);

        assert!(vs.default_branch().is_some());
        assert_eq!(vs.default_branch().unwrap().name(), "develop");
        assert_eq!(vs.default_branch().unwrap().sha(), "def456");
    }

    #[test]
    fn test_major_tags_mut() {
        let mut vs = Set::new();
        assert!(vs.major_tags().is_empty());

        let v1_tag = Arc::new(Info::new(Version::parse("v1"), "aaa"));
        let v2_tag = Arc::new(Info::new(Version::parse("v2"), "bbb"));

        vs.major_tags_mut().insert(1, v1_tag);
        vs.major_tags_mut().insert(2, v2_tag);

        assert_eq!(vs.major_tags().len(), 2);
        assert!(vs.major_tags().contains_key(&1));
        assert!(vs.major_tags().contains_key(&2));
    }

    #[test]
    fn test_version_set_serde_round_trip() {
        let v1 = Arc::new(Info::new(Version::parse("v1.0.0"), "aaa"));
        let v2 = Arc::new(Info::new(Version::parse("v2.0.0"), "bbb"));
        let mut latest_per_major = std::collections::HashMap::new();
        latest_per_major.insert(1, Arc::clone(&v1));
        latest_per_major.insert(2, Arc::clone(&v2));
        let mut major_tags = std::collections::HashMap::new();
        major_tags.insert(2, Arc::clone(&v2));
        let branch = BranchInfo::new("main", "ccc");

        let set = Set::build_with_clock(
            vec![Arc::clone(&v1), Arc::clone(&v2)],
            latest_per_major,
            major_tags,
            Some(Arc::clone(&v2)),
            &crate::clock::SystemClock,
        )
        .with_default_branch(branch);

        let json = serde_json::to_string(&set).unwrap();
        let deserialized: Set = serde_json::from_str(&json).unwrap();

        // Data fields round-trip
        assert_eq!(deserialized.semver_tags().len(), 2);
        assert_eq!(deserialized.latest_per_major().len(), 2);
        assert_eq!(deserialized.major_tags().len(), 1);
        assert!(deserialized.latest().is_some());
        assert!(deserialized.default_branch().is_some());

        // fetched_at is skipped — should be None after deserialization
        assert!(
            deserialized.fetched_at().is_none(),
            "fetched_at should be None after deserialization (serde skip)"
        );
    }

    #[test]
    fn test_version_info_serde_round_trip() {
        let info = Info::new(Version::parse("v1.2.3"), "abc123");
        let json = serde_json::to_string(&info).unwrap();
        let deserialized: Info = serde_json::from_str(&json).unwrap();
        assert_eq!(info, deserialized);
    }

    #[test]
    fn test_version_serde_round_trip_all_variants() {
        let versions = [
            Version::parse("v1.2.3"),
            Version::parse("v1"),
            Version::parse("v1.2"),
            Version::parse("1.2.3-beta.1"),
            Version::parse("abc123def456abc123def456abc123def456abcd"),
            Version::parse("main"),
            Version::parse("nightly"),
        ];
        for version in &versions {
            let json = serde_json::to_string(version).unwrap();
            let deserialized: Version = serde_json::from_str(&json).unwrap();
            assert_eq!(
                version.to_string(),
                deserialized.to_string(),
                "round-trip failed for {version}"
            );
        }
    }

    #[test]
    fn test_branch_info_serde_round_trip() {
        let branch = BranchInfo::new("develop", "def456");
        let json = serde_json::to_string(&branch).unwrap();
        let deserialized: BranchInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(branch, deserialized);
    }
}
