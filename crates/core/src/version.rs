//! Version types and semver handling
//!
//! Represents versions of actions, supporting:
//! - Semantic version tags (v1, v1.2, v1.2.3)
//! - Commit SHAs
//! - Branch names

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;

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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VersionSet {
    /// All available semver versions, sorted
    pub semver_tags: Vec<VersionInfo>,

    /// Latest version per major
    ///
    /// Examples:
    /// - `v1 -> v1.2.3`
    /// - `v2 -> v2.0.1`
    pub latest_per_major: std::collections::HashMap<u32, VersionInfo>,

    /// Major-only tags that exist
    ///
    /// Examples:
    /// - `v1`
    /// - `v2`
    /// - `v6`
    ///
    /// Maps major version number to the [`VersionInfo`] for the major-only tag\
    /// Used to suggest major-only refs when user already uses major-only style
    pub major_tags: std::collections::HashMap<u32, VersionInfo>,

    /// Overall latest semver version
    pub latest: Option<VersionInfo>,

    /// Default branch info
    pub default_branch: Option<BranchInfo>,

    /// When this data was fetched
    pub fetched_at: Option<std::time::SystemTime>,
}

/// Information about a specific version
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    /// The parsed version
    pub version: Version,

    /// The commit SHA this version points to
    pub sha: String,
}

impl VersionInfo {
    /// Create a new [`VersionInfo`]
    #[must_use]
    pub fn new(version: Version, sha: impl Into<String>) -> Self {
        Self {
            version,
            sha: sha.into(),
        }
    }
}

/// Information about a branch
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchInfo {
    /// Branch name
    pub name: String,

    /// Current HEAD commit SHA
    pub sha: String,
}

impl VersionSet {
    /// Create an empty [`VersionSet`]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Find the latest version that satisfies the given policy for a current version
    #[must_use]
    pub fn find_latest_for(
        &self,
        current: &Version,
        allow_major: bool,
        allow_prerelease: bool,
    ) -> Option<&VersionInfo> {
        match current {
            Version::Semver { major, .. } => {
                if allow_major {
                    // Return overall latest (filtered by prerelease if needed)
                    self.latest
                        .as_ref()
                        .filter(|v| allow_prerelease || !v.version.is_prerelease())
                } else {
                    // Return latest for this major
                    self.latest_per_major
                        .get(major)
                        .filter(|v| allow_prerelease || !v.version.is_prerelease())
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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

    // VersionInfo tests

    #[test]
    fn test_version_info_new() {
        let info = VersionInfo::new(Version::parse("v1.0.0"), "abc123");
        assert_eq!(info.version.to_string(), "v1.0.0");
        assert_eq!(info.sha, "abc123");
    }

    // BranchInfo tests

    #[test]
    fn test_branch_info() {
        let info = BranchInfo {
            name: "main".to_string(),
            sha: "abc123".to_string(),
        };
        assert_eq!(info.name, "main");
        assert_eq!(info.sha, "abc123");
    }

    // VersionSet tests

    #[test]
    fn test_version_set_new() {
        let vs = VersionSet::new();
        assert!(vs.semver_tags.is_empty());
        assert!(vs.latest.is_none());
        assert!(vs.default_branch.is_none());
    }

    #[test]
    fn test_find_latest_for_major_allowed() {
        let v3 = VersionInfo::new(Version::parse("v3.0.0"), "aaa");
        let v4 = VersionInfo::new(Version::parse("v4.0.0"), "bbb");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(3, v3);
        latest_per_major.insert(4, v4.clone());

        let vs = VersionSet {
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
        let v3_1 = VersionInfo::new(Version::parse("v3.1.0"), "aaa");
        let v4 = VersionInfo::new(Version::parse("v4.0.0"), "bbb");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(3, v3_1);
        latest_per_major.insert(4, v4.clone());

        let vs = VersionSet {
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
        let v4 = VersionInfo::new(Version::parse("v4.0.0"), "bbb");

        let vs = VersionSet {
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
        let beta = VersionInfo::new(Version::parse("v4.0.0-beta"), "aaa");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, beta.clone());

        let vs = VersionSet {
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
}
