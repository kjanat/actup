//! Version types and semver handling
//!
//! Represents versions of actions, supporting:
//! - Semantic version tags (v1, v1.2, v1.2.3)
//! - Commit SHAs
//! - Branch names

use std::cmp::Ordering;
use std::fmt;

/// A parsed version from a git ref
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

        // Split by '.' and parse numbers
        let mut parts = version_str.split('.');

        let major_str = parts.next()?;
        // Check for prerelease in major (shouldn't happen but handle it)
        let (major_str, _) = major_str.split_once('-').unwrap_or((major_str, ""));
        let major: u32 = major_str.parse().ok()?;

        let minor = parts.next().and_then(|s| {
            let (num, _) = s.split_once('-').unwrap_or((s, ""));
            num.parse().ok()
        });

        let (patch, prerelease) = parts.next().map_or((None, None), |s| {
            s.split_once('-').map_or_else(
                || (s.parse().ok(), None),
                |(num, pre)| (num.parse().ok(), Some(pre.to_string())),
            )
        });

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
#[derive(Debug, Clone, Default)]
pub struct VersionSet {
    /// All available semver versions, sorted
    pub semver_tags: Vec<VersionInfo>,

    /// Latest version per major (e.g., v1 -> v1.2.3, v2 -> v2.0.1)
    pub latest_per_major: std::collections::HashMap<u32, VersionInfo>,

    /// Overall latest semver version
    pub latest: Option<VersionInfo>,

    /// Default branch info
    pub default_branch: Option<BranchInfo>,

    /// When this data was fetched
    pub fetched_at: Option<std::time::SystemTime>,
}

/// Information about a specific version
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    /// The parsed version
    pub version: Version,

    /// The commit SHA this version points to
    pub sha: String,
}

impl VersionInfo {
    /// Create a new `VersionInfo`
    #[must_use]
    pub fn new(version: Version, sha: impl Into<String>) -> Self {
        Self {
            version,
            sha: sha.into(),
        }
    }
}

/// Information about a branch
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    /// Branch name
    pub name: String,

    /// Current HEAD commit SHA
    pub sha: String,
}

impl VersionSet {
    /// Create an empty `VersionSet`
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
mod tests {
    use super::*;

    #[test]
    fn test_parse_semver() {
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
    fn test_parse_sha() {
        let sha = "a".repeat(40);
        let v = Version::parse(&sha);
        assert!(matches!(v, Version::Sha { .. }));
    }

    #[test]
    fn test_parse_other() {
        let v = Version::parse("main");
        assert!(matches!(v, Version::Other { .. }));
    }

    #[test]
    fn test_semver_ordering() {
        let v1 = Version::parse("v1.0.0");
        let v2 = Version::parse("v2.0.0");
        let v1_1 = Version::parse("v1.1.0");
        let v1_0_1 = Version::parse("v1.0.1");

        assert!(v2 > v1);
        assert!(v1_1 > v1);
        assert!(v1_0_1 > v1);
        assert!(v2 > v1_1);
    }

    #[test]
    fn test_prerelease_ordering() {
        let release = Version::parse("v1.0.0");
        let beta = Version::parse("v1.0.0-beta");

        assert!(release > beta); // release > prerelease
    }
}
