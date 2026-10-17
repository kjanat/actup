//! Version types and semver handling
//!
//! Represents versions of actions, supporting:
//! - Semantic version tags (v1, v1.2, v1.2.3)
//! - Commit SHAs
//! - Branch names

use std::{cmp::Ordering, fmt, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::{
    clock::{Clock, SystemClock},
    policy,
};

/// A parsed version from a git ref.
///
/// Wire form: serialized/deserialized as the Display string (e.g. `"v4.2.0"`,
/// `"abc1234..."`, `"main"`). The structural enum is a Rust-internal detail —
/// JSON consumers receive a single string. Round-trips through
/// [`Version::parse`] which never produces [`Version::Branch`] (branches live
/// separately in [`BranchInfo`]).
///
/// # Round-trip lossy for `Branch`
///
/// `Branch` is intentionally not preserved across (de)serialization. A
/// directly-constructed `Branch` round-trips to `Other` because `parse`
/// can't distinguish a branch name from any other non-semver string
/// without context:
///
/// ```text
/// Version::Branch { name: "main" }   serializes to → "main"
/// "main"                             deserializes to → Version::Other { tag: "main" }
/// ```
///
/// This is fine in practice because branch info on the wire travels via
/// [`BranchInfo`] (a separate field of [`Set`]), not via a `Version`. The
/// regression test in this module's `tests` enforces the invariant.
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

        if is_iso_date(ref_str) || is_short_sha(ref_str) {
            return Self::Other {
                tag: ref_str.to_string(),
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
        let version_str = version_str.split_once('+').map_or(version_str, |(v, _)| v);

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

    fn release(&self) -> Option<(u32, u32, u32)> {
        match self {
            Self::Semver {
                major,
                minor,
                patch,
                ..
            } => Some((*major, minor.unwrap_or(0), patch.unwrap_or(0))),
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

// Wire form is the Display string — see the type's doc-comment for rationale.
// Round-trips through `Version::parse`, which never yields `Version::Branch`
// (branches live in `BranchInfo`, not in serialized version sets).
impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::parse(&s))
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
                    (Some(a), Some(b)) => cmp_prerelease(a, b),
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

/// Returns true if `ref_str` is an abbreviated commit SHA of 7 to 39 hex characters.
#[must_use]
pub fn is_short_sha(ref_str: &str) -> bool {
    (7..40).contains(&ref_str.len()) && ref_str.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

// Precedence follows SemVer 2.0.0 §11.4.
fn cmp_prerelease(a: &str, b: &str) -> Ordering {
    let mut xs = a.split('.');
    let mut ys = b.split('.');
    loop {
        match (xs.next(), ys.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => match cmp_identifier(x, y) {
                Ordering::Equal => {}
                ord => return ord,
            },
        }
    }
}

fn cmp_identifier(x: &str, y: &str) -> Ordering {
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    match (numeric(x), numeric(y)) {
        (true, true) => {
            let x = x.trim_start_matches('0');
            let y = y.trim_start_matches('0');
            x.len().cmp(&y.len()).then_with(|| x.cmp(y))
        }
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => x.cmp(y),
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
    semver_tags: Tags,

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

    /// Branch name -> current HEAD SHA, for pinning floating branch refs.
    ///
    /// Populated by the git ls-refs path; empty when versions came from an
    /// API that does not enumerate branches.
    #[serde(default)]
    branches: std::collections::HashMap<String, String>,

    /// Non-semver tag name -> commit SHA.
    #[serde(default)]
    other_tags: std::collections::HashMap<String, String>,

    /// Whether the tag and branch maps held every ref of the repository at fetch time.
    #[serde(skip)]
    listing: Listing,

    /// When this data was fetched (skipped in serialization — platform-dependent)
    #[serde(skip)]
    fetched_at: Option<std::time::SystemTime>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(from = "Vec<Arc<Info>>")]
struct Tags {
    list: Vec<Arc<Info>>,
    by_sha: std::collections::HashMap<String, Vec<Arc<Info>>>,
}

impl From<Vec<Arc<Info>>> for Tags {
    fn from(list: Vec<Arc<Info>>) -> Self {
        let mut by_sha: std::collections::HashMap<String, Vec<Arc<Info>>> =
            std::collections::HashMap::new();
        for info in &list {
            by_sha
                .entry(info.sha().to_string())
                .or_default()
                .push(Arc::clone(info));
        }
        Self { list, by_sha }
    }
}

impl Serialize for Tags {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.list.serialize(serializer)
    }
}

fn rank(a: &Version, b: &Version) -> Ordering {
    let parts = |v: &Version| (v.minor().is_some(), v.patch().is_some());
    let has_build = |v: &Version| v.as_str().contains('+');
    a.cmp(b)
        .then_with(|| parts(a).cmp(&parts(b)))
        .then_with(|| has_build(b).cmp(&has_build(a)))
        .then_with(|| b.as_str().cmp(a.as_str()))
}

/// How much of a repository's refs a [`Set`] holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Listing {
    /// Some tags or branches are missing, as with a capped API listing.
    #[default]
    Partial,
    /// Every tag and branch is listed, as with a git ref listing.
    Complete,
}

/// Information about a specific version
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "tsify", derive(tsify_next::Tsify))]
pub struct Info {
    /// The parsed version. Wire form is the Display string (see [`Version`]).
    #[cfg_attr(feature = "tsify", tsify(type = "string"))]
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
            semver_tags: semver_tags.into(),
            latest_per_major,
            major_tags,
            latest,
            default_branch: None,
            branches: std::collections::HashMap::new(),
            other_tags: std::collections::HashMap::new(),
            listing: Listing::Partial,
            fetched_at: Some(clock.now()),
        }
    }

    /// Get all semver tags
    #[must_use]
    pub fn semver_tags(&self) -> &[Arc<Info>] {
        &self.semver_tags.list
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
        self.tag(ref_str).is_some()
    }

    /// Find the semver tag named exactly `name`.
    #[must_use]
    pub fn tag(&self, name: &str) -> Option<&Arc<Info>> {
        self.semver_tags
            .list
            .iter()
            .find(|t| t.version().as_str() == name)
    }

    /// Get every semver tag on commit `sha`.
    #[must_use]
    pub fn tags_on(&self, sha: &str) -> &[Arc<Info>] {
        self.semver_tags.by_sha.get(sha).map_or(&[], Vec::as_slice)
    }

    /// Find the highest full `major.minor.patch` tag on commit `sha`.
    #[must_use]
    pub fn full_tag_on(&self, sha: &str) -> Option<&Arc<Info>> {
        self.tags_on(sha)
            .iter()
            .filter(|t| t.version().patch().is_some())
            .max_by(|a, b| rank(a.version(), b.version()))
    }

    /// Find the tag that names commit `sha`: the highest full tag, else the most specific tag.
    #[must_use]
    pub fn tag_on(&self, sha: &str) -> Option<&Arc<Info>> {
        self.full_tag_on(sha).or_else(|| {
            self.tags_on(sha)
                .iter()
                .max_by(|a, b| rank(a.version(), b.version()))
        })
    }

    /// The commit SHA of the semver tag named exactly `name`.
    #[must_use]
    pub fn tag_sha(&self, name: &str) -> Option<&str> {
        self.tag(name).map(|t| t.sha())
    }

    /// Find the latest version that `bump` allows for a current version
    ///
    /// A component that `current` leaves out matches any value, so `v4` under
    /// [`policy::Update::Patch`] covers every `v4.x.y`. Prereleases count when
    /// `allow_prerelease` is set or when `current` is a prerelease of the same
    /// `major.minor.patch`. Returns `None` when every candidate ranks below
    /// `current`. Among equal-ranked tags it picks the one with the most
    /// version components, then the one without build metadata, then the
    /// lowest tag name.
    #[must_use]
    pub fn find_latest_for(
        &self,
        current: &Version,
        bump: policy::Update,
        allow_prerelease: bool,
    ) -> Option<&Arc<Info>> {
        let in_range = |v: &Version| match bump {
            policy::Update::Major | policy::Update::Latest => true,
            policy::Update::Minor => v.major() == current.major(),
            policy::Update::Patch => {
                v.major() == current.major()
                    && current.minor().is_none_or(|minor| v.minor() == Some(minor))
            }
        };
        let allows = |v: &Version| {
            v.is_semver()
                && in_range(v)
                && (!v.is_prerelease()
                    || allow_prerelease
                    || (current.is_prerelease() && v.release() == current.release()))
        };
        self.semver_tags
            .list
            .iter()
            .filter(|t| allows(t.version()))
            .max_by(|a, b| rank(a.version(), b.version()))
            .filter(|t| t.version() >= current)
    }

    /// Set the default branch information
    pub fn set_default_branch(&mut self, branch: BranchInfo) {
        self.default_branch = Some(branch);
    }

    /// Set the branch name -> HEAD SHA map used for pinning branch refs.
    pub fn set_branches(&mut self, branches: std::collections::HashMap<String, String>) {
        self.branches = branches;
    }

    /// Get the branch name -> HEAD SHA map.
    #[must_use]
    pub const fn branches(&self) -> &std::collections::HashMap<String, String> {
        &self.branches
    }

    /// Resolve a branch name to its current HEAD SHA, if known.
    ///
    /// Checks the branch map first, then the default branch.
    #[must_use]
    pub fn branch_sha(&self, name: &str) -> Option<&str> {
        self.branches.get(name).map(String::as_str).or_else(|| {
            self.default_branch
                .as_ref()
                .filter(|b| b.name() == name)
                .map(BranchInfo::sha)
        })
    }

    /// Set the non-semver tag name -> commit SHA map.
    pub fn set_other_tags(&mut self, tags: std::collections::HashMap<String, String>) {
        self.other_tags = tags;
    }

    /// Resolve a non-semver tag name to its commit SHA, if known.
    #[must_use]
    pub fn other_tag_sha(&self, name: &str) -> Option<&str> {
        self.other_tags.get(name).map(String::as_str)
    }

    /// Check if `name` is a known tag or branch.
    #[must_use]
    pub fn names_ref(&self, name: &str) -> bool {
        self.has_exact_tag(name)
            || self.other_tags.contains_key(name)
            || self.branch_sha(name).is_some()
    }

    /// Set how much of the repository's refs this set holds.
    pub const fn set_listing(&mut self, listing: Listing) {
        self.listing = listing;
    }

    /// Get how much of the repository's refs this set holds.
    #[must_use]
    pub const fn listing(&self) -> Listing {
        self.listing
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

    #[test]
    fn test_parse_all_digit_short_sha_is_not_semver() {
        for short in ["1234567", &"1".repeat(39)] {
            assert_eq!(
                Version::parse(short),
                Version::Other {
                    tag: short.to_string()
                },
                "{short}"
            );
        }
        assert_eq!(Version::parse("123456").major(), Some(123_456));
    }

    #[test]
    fn test_tag_on_prefers_full_tag_then_any_tag() {
        let sha = "1".repeat(40);
        let info = |tag: &str| Arc::new(Info::new(Version::parse(tag), sha.clone()));
        let full = Set::build(
            vec![info("v4"), info("v4.2.0"), info("v4.2")],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        assert_eq!(
            full.tag_on(&sha).map(|t| t.version().as_str()),
            Some("v4.2.0")
        );
        let major = Set::build(
            vec![info("v3"), info("v4")],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        assert_eq!(major.tag_on(&sha).map(|t| t.version().as_str()), Some("v4"));
        assert!(major.tag_on(&"2".repeat(40)).is_none());
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
    fn prerelease_precedence_chain_is_strictly_ascending() {
        let chains: [&[&str]; 2] = [
            &[
                "1.0.0-alpha",
                "1.0.0-alpha.1",
                "1.0.0-beta.2",
                "1.0.0-beta.11",
                "1.0.0-rc.1",
                "1.0.0",
            ],
            &[
                "v1.0.0-alpha",
                "v1.0.0-alpha.1",
                "v1.0.0-alpha.beta",
                "v1.0.0-beta",
                "v1.0.0-beta.2",
                "v1.0.0-beta.11",
                "v1.0.0-rc.1",
                "v1.0.0",
            ],
        ];
        for chain in chains {
            for (i, lo) in chain.iter().enumerate() {
                for hi in &chain[i + 1..] {
                    assert_eq!(
                        Version::parse(lo).cmp(&Version::parse(hi)),
                        Ordering::Less,
                        "{lo} < {hi}"
                    );
                    assert_eq!(
                        Version::parse(hi).cmp(&Version::parse(lo)),
                        Ordering::Greater,
                        "{hi} > {lo}"
                    );
                }
            }
        }
    }

    #[test]
    fn semver_precedence_table() {
        let cases = [
            ("v1.0.0-rc.2", "v1.0.0-rc.10", Ordering::Less),
            ("v1.0.0-alpha", "v1.0.0-beta", Ordering::Less),
            ("v1.0.0-1", "v1.0.0-alpha", Ordering::Less),
            ("v1.0.0-alpha", "v1.0.0-alpha.0", Ordering::Less),
            ("v1.0.0-RC.1", "v1.0.0-alpha", Ordering::Less),
            ("v1.0.0-rc.01", "v1.0.0-rc.1", Ordering::Equal),
            (
                "v1.0.0-rc.99999999999999999999",
                "v1.0.0-rc.100000000000000000000",
                Ordering::Less,
            ),
            ("v1.0.0+build", "v1.0.0", Ordering::Equal),
            ("v1.0.0+build.1", "v1.0.0+build.2", Ordering::Equal),
            (
                "v1.0.0-rc.1+exp.sha.5114f85",
                "v1.0.0-rc.1",
                Ordering::Equal,
            ),
            ("v1.0.0+build", "v1.0.0-rc.1", Ordering::Greater),
            ("v1.0.1+build", "v1.0.0", Ordering::Greater),
        ];
        for (a, b, want) in cases {
            assert_eq!(
                Version::parse(a).cmp(&Version::parse(b)),
                want,
                "{a} vs {b}"
            );
            assert_eq!(
                Version::parse(b).cmp(&Version::parse(a)),
                want.reverse(),
                "{b} vs {a}"
            );
        }
    }

    #[test]
    fn build_metadata_is_stripped_from_components() {
        let cases = [
            ("v1.0.0+build", 1, Some(0), Some(0), None),
            ("1.2.3+build-1", 1, Some(2), Some(3), None),
            (
                "v1.2.3-rc.1+exp.sha.5114f85",
                1,
                Some(2),
                Some(3),
                Some("rc.1"),
            ),
            ("v2+meta", 2, None, None, None),
        ];
        for (input, want_major, want_minor, want_patch, want_pre) in cases {
            let Version::Semver {
                major,
                minor,
                patch,
                prerelease,
                original,
            } = Version::parse(input)
            else {
                panic!("{input} should parse as semver");
            };
            assert_eq!(major, want_major, "{input} major");
            assert_eq!(minor, want_minor, "{input} minor");
            assert_eq!(patch, want_patch, "{input} patch");
            assert_eq!(prerelease.as_deref(), want_pre, "{input} prerelease");
            assert_eq!(original, input, "{input} keeps its original text");
        }
    }

    #[test]
    fn iso_dates_parse_as_other() {
        for input in ["2024-01-15", "1999-12-31"] {
            assert_eq!(
                Version::parse(input),
                Version::Other {
                    tag: input.to_string(),
                },
                "{input}"
            );
        }
        for input in ["2024.01.15", "v2024", "2024-01-15-rc"] {
            assert!(Version::parse(input).is_semver(), "{input}");
        }
    }

    #[test]
    fn cached_set_reparses_versions_on_load() -> Result<(), Box<dyn std::error::Error>> {
        let json = r#"{
            "semver_tags": [
                {"version": "v1.0.0+build", "sha": "a"},
                {"version": "v1.0.0-rc.2", "sha": "b"},
                {"version": "v1.0.0-rc.10", "sha": "c"},
                {"version": "2024-01-15", "sha": "d"}
            ],
            "latest_per_major": {},
            "major_tags": {},
            "latest": null,
            "default_branch": null
        }"#;
        let set: Set = serde_json::from_str(json)?;
        let versions: Vec<&Version> = set.semver_tags().iter().map(|t| t.version()).collect();
        let [build, rc2, rc10, date] = versions.as_slice() else {
            panic!("expected four cached tags, got {versions:?}");
        };
        assert_eq!(build.patch(), Some(0));
        assert!(rc10 > rc2);
        assert_eq!(
            *date,
            &Version::Other {
                tag: "2024-01-15".to_string(),
            }
        );
        Ok(())
    }

    #[test]
    fn restored_set_reads_as_partial() -> Result<(), Box<dyn std::error::Error>> {
        let mut set = Set::new();
        set.set_listing(Listing::Complete);
        let json = serde_json::to_string(&set)?;
        assert!(!json.contains("listing"), "{json}");
        let restored: Set = serde_json::from_str(&json)?;
        assert_eq!(restored.listing(), Listing::Partial);

        let old = r#"{"semver_tags": [], "latest_per_major": {}, "major_tags": {},
            "latest": null, "default_branch": null, "listing": "complete"}"#;
        let restored: Set = serde_json::from_str(old)?;
        assert_eq!(restored.listing(), Listing::Partial);
        Ok(())
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
        assert_eq!(vs.semver_tags(), []);
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
            semver_tags: vec![Arc::clone(&v4), v3].into(),
            latest_per_major,
            major_tags: HashMap::new(),
            latest: Some(v4),
            default_branch: None,
            branches: HashMap::new(),
            other_tags: HashMap::new(),
            listing: Listing::Partial,
            fetched_at: None,
        };

        let current = Version::parse("v3.0.0");
        let result = vs.find_latest_for(&current, policy::Update::Major, false);
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
            semver_tags: vec![Arc::clone(&v4), v3_1].into(),
            latest_per_major,
            major_tags: HashMap::new(),
            latest: Some(v4),
            default_branch: None,
            branches: HashMap::new(),
            other_tags: HashMap::new(),
            listing: Listing::Partial,
            fetched_at: None,
        };

        let current = Version::parse("v3.0.0");
        let result = vs.find_latest_for(&current, policy::Update::Minor, false);
        assert!(result.is_some());
        assert_eq!(result.unwrap().version.to_string(), "v3.1.0");
    }

    #[test]
    fn test_find_latest_for_non_semver() {
        let v4 = Arc::new(Info::new(Version::parse("v4.0.0"), "bbb"));

        let vs = Set {
            semver_tags: vec![Arc::clone(&v4)].into(),
            latest_per_major: HashMap::new(),
            major_tags: HashMap::new(),
            latest: Some(v4),
            default_branch: None,
            branches: HashMap::new(),
            other_tags: HashMap::new(),
            listing: Listing::Partial,
            fetched_at: None,
        };

        // Non-semver current version
        let current = Version::parse("main");

        // Under major, returns latest
        let result = vs.find_latest_for(&current, policy::Update::Major, false);
        assert!(result.is_some());

        // Under minor, returns None
        let result = vs.find_latest_for(&current, policy::Update::Minor, false);
        assert!(result.is_none());
    }

    #[test]
    fn test_find_latest_for_filters_prerelease() {
        let beta = Arc::new(Info::new(Version::parse("v4.1.0-beta"), "aaa"));

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, Arc::clone(&beta));

        let vs = Set {
            semver_tags: vec![Arc::clone(&beta)].into(),
            latest_per_major,
            major_tags: HashMap::new(),
            latest: Some(beta),
            default_branch: None,
            branches: HashMap::new(),
            other_tags: HashMap::new(),
            listing: Listing::Partial,
            fetched_at: None,
        };

        let current = Version::parse("v4.0.0");

        // Without allow_prerelease, filters out beta
        let result = vs.find_latest_for(&current, policy::Update::Minor, false);
        assert!(result.is_none());

        // With allow_prerelease, includes beta
        let result = vs.find_latest_for(&current, policy::Update::Minor, true);
        assert!(result.is_some());
    }

    #[test]
    fn find_latest_for_scans_unsorted_tags() -> Result<(), Box<dyn std::error::Error>> {
        let tag = |name: &str, sha: &str| Arc::new(Info::new(Version::parse(name), sha));
        let tags = vec![
            tag("v1.0.0", "a"),
            tag("nightly", "b"),
            tag("v2.0.0-rc.1", "c"),
            tag("v1.2.0+build.1", "e"),
            tag("v1.2.0", "d"),
            tag("v1.3.0-beta.1", "f"),
        ];
        let reversed = tags.iter().rev().cloned().collect();
        for semver_tags in [tags, reversed] {
            let vs = Set::build(semver_tags, HashMap::new(), HashMap::new(), None);
            let pick = |current: &str, bump: policy::Update, allow_prerelease: bool| {
                vs.find_latest_for(&Version::parse(current), bump, allow_prerelease)
                    .map(|t| t.sha().to_string())
            };

            let (major, minor) = (policy::Update::Major, policy::Update::Minor);
            assert_eq!(pick("v1.0.0", major, false).as_deref(), Some("d"));
            assert_eq!(pick("v1.0.0", minor, true).as_deref(), Some("f"));
            assert_eq!(pick("v1.0.0", major, true).as_deref(), Some("c"));
            assert_eq!(pick("nightly", major, false).as_deref(), Some("d"));
            assert_eq!(pick("nightly", minor, true), None);
            let Some(latest) = vs.find_latest_for(&Version::parse("v1.3.0-beta.1"), minor, false)
            else {
                return Err("expected a target for a prerelease current".into());
            };
            assert_eq!(latest.version().as_str(), "v1.3.0-beta.1");
        }
        Ok(())
    }

    #[test]
    fn full_tag_on_skips_partial_tags_and_other_commits() {
        let tag = |name: &str, sha: &str| Arc::new(Info::new(Version::parse(name), sha));
        let vs = Set::build(
            vec![
                tag("v3", "a"),
                tag("v3.1", "a"),
                tag("v3.1.0+b", "a"),
                tag("v3.1.0", "a"),
                tag("v3.0.0", "a"),
                tag("v3.2.0", "b"),
            ],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        assert_eq!(
            vs.full_tag_on("a").map(|t| t.version().as_str()),
            Some("v3.1.0")
        );
        assert!(vs.full_tag_on("c").is_none());
    }

    #[test]
    fn tags_on_indexes_every_tag_by_commit() -> Result<(), Box<dyn std::error::Error>> {
        let tag = |name: &str, sha: &str| Arc::new(Info::new(Version::parse(name), sha));
        let tags = vec![
            tag("v4", "a"),
            tag("v4.0", "a"),
            tag("v3.9.0", "b"),
            tag("v4.1", "c"),
            tag("v4.1.0", "c"),
        ];
        let reversed: Vec<_> = tags.iter().rev().cloned().collect();
        for semver_tags in [tags, reversed] {
            let built = Set::build(semver_tags, HashMap::new(), HashMap::new(), None);
            let restored: Set = serde_json::from_str(&serde_json::to_string(&built)?)?;
            for vs in [built, restored] {
                let mut on_a: Vec<&str> = vs
                    .tags_on("a")
                    .iter()
                    .map(|t| t.version().as_str())
                    .collect();
                on_a.sort_unstable();
                assert_eq!(on_a, ["v4", "v4.0"]);
                assert_eq!(vs.tags_on("d"), []);
                assert_eq!(vs.tag_on("a").map(|t| t.version().as_str()), Some("v4.0"));
                assert!(vs.full_tag_on("a").is_none());
                assert_eq!(vs.tag_on("c").map(|t| t.version().as_str()), Some("v4.1.0"));
                assert_eq!(vs.tag("v4.1").map(|t| t.sha()), Some("c"));
                assert!(vs.tag("v4.1.1").is_none());
            }
        }
        Ok(())
    }

    #[test]
    fn find_latest_for_breaks_ties_by_tag_name() {
        let tag = |name: &str| Arc::new(Info::new(Version::parse(name), name));
        let cases: [(&[&str], &str); 3] = [
            (&["v1.2.0+build.2", "v1.2.0+build.1"], "v1.2.0+build.1"),
            (&["v1.2.0+build.1", "v1.2.0"], "v1.2.0"),
            (&["v1.2", "v1.2.0+build.1"], "v1.2.0+build.1"),
        ];
        for (names, want) in cases {
            let tags: Vec<_> = names.iter().map(|n| tag(n)).collect();
            let reversed = tags.iter().rev().cloned().collect();
            for semver_tags in [tags, reversed] {
                let vs = Set::build(semver_tags, HashMap::new(), HashMap::new(), None);
                let latest =
                    vs.find_latest_for(&Version::parse("v1.0.0"), policy::Update::Latest, false);
                assert_eq!(latest.map(|t| t.sha()), Some(want), "{names:?}");
            }
        }
    }

    #[test]
    fn find_latest_for_never_picks_a_lower_version() {
        let tag = |name: &str| Arc::new(Info::new(Version::parse(name), name));
        let vs = Set::build(
            vec![tag("v3.0.0-rc.1"), tag("v2.0.0")],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        for (current, want) in [
            ("v3.0.0-rc.9", None),
            ("v3.0.1", None),
            ("v3.0.0-rc.1", Some("v3.0.0-rc.1")),
            ("v2.5.0", Some("v3.0.0-rc.1")),
        ] {
            let latest = vs.find_latest_for(&Version::parse(current), policy::Update::Latest, true);
            assert_eq!(latest.map(|t| t.sha()), want, "{current}");
        }
    }

    #[test]
    fn find_latest_for_honours_bump() {
        use policy::Update::{Latest, Major, Minor, Patch};

        let tag = |name: &str| Arc::new(Info::new(Version::parse(name), name));
        let vs = Set::build(
            ["v4", "v4.1.0", "v4.1.3", "v4.1.4-rc.1", "v4.9.3", "v5.0.0"]
                .into_iter()
                .map(tag)
                .collect(),
            HashMap::new(),
            HashMap::new(),
            None,
        );
        let pick = |current: &str, bump: policy::Update, allow_prerelease: bool| {
            vs.find_latest_for(&Version::parse(current), bump, allow_prerelease)
                .map(|t| t.sha().to_string())
        };

        assert_eq!(pick("v4.1.0", Patch, false).as_deref(), Some("v4.1.3"));
        assert_eq!(pick("v4.1.0", Patch, true).as_deref(), Some("v4.1.4-rc.1"));
        assert_eq!(pick("v4.1.0", Minor, false).as_deref(), Some("v4.9.3"));
        assert_eq!(pick("v4.1.0", Major, false).as_deref(), Some("v5.0.0"));
        assert_eq!(pick("v4.1.0", Latest, false).as_deref(), Some("v5.0.0"));
        assert_eq!(pick("v4.1", Patch, false).as_deref(), Some("v4.1.3"));
        assert_eq!(pick("v4", Patch, false).as_deref(), Some("v4.9.3"));
        assert_eq!(pick("v4.2.0", Patch, false), None);
        assert_eq!(pick("v3.0.0", Patch, false), None);
        assert_eq!(pick("v3.0.0", Minor, false), None);
        assert_eq!(pick("nightly", Patch, false), None);
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
    fn test_branches_returns_the_set_map() {
        let mut vs = Set::new();
        assert!(vs.branches().is_empty());
        let map = std::collections::HashMap::from([("dev".to_string(), "d".repeat(40))]);
        vs.set_branches(map.clone());
        assert_eq!(vs.branches(), &map);
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

    /// Regression: `Version::Branch` cannot round-trip — wire form is the
    /// Display string, and `Version::parse` cannot distinguish a branch
    /// name from any non-semver `Other` tag without context. A
    /// directly-constructed `Branch` deserializes back as `Other` with the
    /// same name. Branch info on the wire travels via [`BranchInfo`]
    /// (a separate field of [`Set`]) — this test guards the assumption
    /// stated in the [`Version`] doc-comment.
    #[test]
    fn test_version_branch_roundtrips_as_other() {
        let branch = Version::Branch {
            name: "main".to_string(),
        };

        // Wire form is the Display string — no structural marker for `Branch`.
        let json = serde_json::to_string(&branch).unwrap();
        assert_eq!(json, "\"main\"");

        // Round-trip lands in `Other`, not `Branch`.
        let deserialized: Version = serde_json::from_str(&json).unwrap();
        assert_eq!(
            deserialized,
            Version::Other {
                tag: "main".to_string(),
            }
        );
        assert!(!deserialized.is_branch(), "must NOT round-trip as Branch");
        // Display-equivalence is still preserved (both render as "main").
        assert_eq!(branch.to_string(), deserialized.to_string());
    }
}
