//! Update policy and ref style configuration
//!
//! Controls how aggressively to update and what format to use for refs.

use schemars::JsonSchema;
use serde::de::{self, Visitor};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Update policy determines what version bumps are allowed.
///
/// # Workflow Examples
///
/// ```yaml
/// # With update_policy: patch (1.2.3 -> 1.2.4 only)
/// - uses: actions/checkout@v4.1.0  # stays at v4.1.x
///
/// # With update_policy: minor (1.2.x -> 1.3.x)
/// - uses: actions/checkout@v4.1.0  # can update to v4.2.0
///
/// # With update_policy: major (1.x -> 2.x)
/// - uses: actions/checkout@v3      # can update to v4
///
/// # With update_policy: latest (always newest)
/// - uses: actions/checkout@v2      # updates to v4
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum UpdatePolicy {
    /// Only patch updates within same major.minor (1.2.3 -> 1.2.4).
    ///
    /// ```yaml
    /// # Before:
    /// - uses: actions/checkout@v4.1.0
    ///
    /// # After (update_policy: patch):
    /// - uses: actions/checkout@v4.1.1
    /// ```
    Patch,

    /// Minor updates within same major (1.2.x -> 1.3.x).
    ///
    /// ```yaml
    /// # Before:
    /// - uses: actions/checkout@v4.1.0
    ///
    /// # After (update_policy: minor):
    /// - uses: actions/checkout@v4.2.0
    /// ```
    #[default]
    Minor,

    /// Major updates allowed (1.x -> 2.x).
    ///
    /// ```yaml
    /// # Before:
    /// - uses: actions/checkout@v3
    ///
    /// # After (update_policy: major):
    /// - uses: actions/checkout@v4
    /// ```
    Major,

    /// Always update to absolute latest, regardless of major version.
    ///
    /// ```yaml
    /// # Before:
    /// - uses: actions/checkout@v2
    ///
    /// # After (update_policy: latest):
    /// - uses: actions/checkout@v4
    /// ```
    Latest,
}

impl UpdatePolicy {
    /// Returns true if this policy allows major version bumps
    #[must_use]
    pub const fn allows_major(&self) -> bool {
        matches!(self, Self::Major | Self::Latest)
    }

    /// Returns true if this policy allows minor version bumps
    #[must_use]
    pub const fn allows_minor(&self) -> bool {
        !matches!(self, Self::Patch)
    }

    /// Parse from string (case-insensitive)
    #[must_use]
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "patch" => Some(Self::Patch),
            "minor" => Some(Self::Minor),
            "major" => Some(Self::Major),
            "latest" | "all" | "any" => Some(Self::Latest),
            _ => None,
        }
    }
}

impl fmt::Display for UpdatePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Patch => write!(f, "patch"),
            Self::Minor => write!(f, "minor"),
            Self::Major => write!(f, "major"),
            Self::Latest => write!(f, "latest"),
        }
    }
}

impl<'de> Deserialize<'de> for UpdatePolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct UpdatePolicyVisitor;

        impl Visitor<'_> for UpdatePolicyVisitor {
            type Value = UpdatePolicy;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("one of: patch, minor, major, latest")
            }

            fn visit_str<E>(self, value: &str) -> Result<UpdatePolicy, E>
            where
                E: de::Error,
            {
                UpdatePolicy::from_str_loose(value)
                    .ok_or_else(|| de::Error::invalid_value(de::Unexpected::Str(value), &self))
            }
        }

        deserializer.deserialize_str(UpdatePolicyVisitor)
    }
}

/// How to format version references when updating.
///
/// # Workflow Examples
///
/// ```yaml
/// # With ref_style: preserve - keeps original format
/// - uses: actions/checkout@v4      # stays as tag
/// - uses: actions/checkout@abc123  # stays as SHA
///
/// # With ref_style: tag - converts to version tags
/// - uses: actions/checkout@v4.1.1  # always a tag
///
/// # With ref_style: sha - pins to commit SHA (most secure)
/// - uses: actions/checkout@b4ffde65f46336ab88eb53be808477a3936bae11 # v4.1.1
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RefStyle {
    /// Keep the original style (tag stays tag, SHA stays SHA).
    ///
    /// ```yaml
    /// # Before:
    /// - uses: actions/checkout@v4
    ///
    /// # After (ref_style: preserve):
    /// - uses: actions/checkout@v4.2.0
    /// # Tag format preserved
    /// ```
    #[default]
    Preserve,

    /// Always use version tags (v1.2.3).
    ///
    /// ```yaml
    /// # Before:
    /// - uses: actions/checkout@abc1234...
    ///
    /// # After (ref_style: tag):
    /// - uses: actions/checkout@v4.2.0
    /// # SHA converted to tag
    /// ```
    Tag,

    /// Always pin to full commit SHA (most secure).
    ///
    /// ```yaml
    /// # Before:
    /// - uses: actions/checkout@v4
    ///
    /// # After (ref_style: sha):
    /// - uses: actions/checkout@b4ffde65f46336ab88eb53be808477a3936bae11 # v4.1.1
    /// # Tag pinned to full SHA with comment
    /// ```
    Sha,
}

impl RefStyle {
    /// Parse from string (case-insensitive)
    #[must_use]
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "preserve" | "keep" | "original" => Some(Self::Preserve),
            "tag" | "tags" | "version" => Some(Self::Tag),
            "sha" | "commit" | "pin" => Some(Self::Sha),
            _ => None,
        }
    }
}

impl fmt::Display for RefStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preserve => write!(f, "preserve"),
            Self::Tag => write!(f, "tag"),
            Self::Sha => write!(f, "sha"),
        }
    }
}

impl<'de> Deserialize<'de> for RefStyle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct RefStyleVisitor;

        impl Visitor<'_> for RefStyleVisitor {
            type Value = RefStyle;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("one of: preserve, tag, sha")
            }

            fn visit_str<E>(self, value: &str) -> Result<RefStyle, E>
            where
                E: de::Error,
            {
                RefStyle::from_str_loose(value)
                    .ok_or_else(|| de::Error::invalid_value(de::Unexpected::Str(value), &self))
            }
        }

        deserializer.deserialize_str(RefStyleVisitor)
    }
}

/// Branch handling policy.
///
/// # Workflow Examples
///
/// ```yaml
/// # With branch_policy: ignore - branch refs skipped silently
/// - uses: actions/checkout@main  # no warning, no update
///
/// # With branch_policy: warn - warns but doesn't change
/// - uses: actions/checkout@main  # warning: consider pinning to a tag
///
/// # With branch_policy: update - pins branch to current HEAD SHA
/// - uses: actions/checkout@b4ffde65f46336ab88eb53be808477a3936bae11 # main
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BranchPolicy {
    /// Ignore branch refs (don't flag as outdated).
    ///
    /// ```yaml
    /// # With branch_policy: ignore
    /// - uses: actions/checkout@main
    /// # No warning, no update - branch refs skipped
    /// ```
    #[default]
    Ignore,

    /// Warn about branch refs (recommend pinning to tag or SHA).
    ///
    /// ```yaml
    /// # With branch_policy: warn
    /// - uses: actions/checkout@main
    /// # Warning: 'main' is a branch ref, consider pinning to a tag
    /// ```
    Warn,

    /// Treat branch refs as updatable (pin to latest commit SHA).
    ///
    /// ```yaml
    /// # Before:
    /// - uses: actions/checkout@main
    ///
    /// # After (branch_policy: update):
    /// - uses: actions/checkout@b4ffde65f46336ab88eb53be808477a3936bae11 # main
    /// # Branch pinned to current HEAD SHA
    /// ```
    Update,
}

impl BranchPolicy {
    /// Parse from string (case-insensitive)
    #[must_use]
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "ignore" | "skip" => Some(Self::Ignore),
            "warn" | "warning" => Some(Self::Warn),
            "update" | "pin" => Some(Self::Update),
            _ => None,
        }
    }
}

impl fmt::Display for BranchPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ignore => write!(f, "ignore"),
            Self::Warn => write!(f, "warn"),
            Self::Update => write!(f, "update"),
        }
    }
}

impl<'de> Deserialize<'de> for BranchPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct BranchPolicyVisitor;

        impl Visitor<'_> for BranchPolicyVisitor {
            type Value = BranchPolicy;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("one of: ignore, warn, update")
            }

            fn visit_str<E>(self, value: &str) -> Result<BranchPolicy, E>
            where
                E: de::Error,
            {
                BranchPolicy::from_str_loose(value)
                    .ok_or_else(|| de::Error::invalid_value(de::Unexpected::Str(value), &self))
            }
        }

        deserializer.deserialize_str(BranchPolicyVisitor)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ========================================================================
    // UpdatePolicy tests
    // ========================================================================

    #[test]
    fn test_update_policy_allows() {
        assert!(!UpdatePolicy::Patch.allows_major());
        assert!(!UpdatePolicy::Patch.allows_minor());

        assert!(!UpdatePolicy::Minor.allows_major());
        assert!(UpdatePolicy::Minor.allows_minor());

        assert!(UpdatePolicy::Major.allows_major());
        assert!(UpdatePolicy::Major.allows_minor());

        assert!(UpdatePolicy::Latest.allows_major());
        assert!(UpdatePolicy::Latest.allows_minor());
    }

    #[test]
    fn test_update_policy_from_str() {
        assert_eq!(
            UpdatePolicy::from_str_loose("PATCH"),
            Some(UpdatePolicy::Patch)
        );
        assert_eq!(
            UpdatePolicy::from_str_loose("minor"),
            Some(UpdatePolicy::Minor)
        );
        assert_eq!(
            UpdatePolicy::from_str_loose("MAJOR"),
            Some(UpdatePolicy::Major)
        );
        assert_eq!(
            UpdatePolicy::from_str_loose("latest"),
            Some(UpdatePolicy::Latest)
        );
        assert_eq!(
            UpdatePolicy::from_str_loose("all"),
            Some(UpdatePolicy::Latest)
        );
        assert_eq!(
            UpdatePolicy::from_str_loose("any"),
            Some(UpdatePolicy::Latest)
        );
        assert_eq!(UpdatePolicy::from_str_loose("invalid"), None);
    }

    #[test]
    fn test_update_policy_display() {
        assert_eq!(UpdatePolicy::Patch.to_string(), "patch");
        assert_eq!(UpdatePolicy::Minor.to_string(), "minor");
        assert_eq!(UpdatePolicy::Major.to_string(), "major");
        assert_eq!(UpdatePolicy::Latest.to_string(), "latest");
    }

    #[test]
    fn test_update_policy_default() {
        assert_eq!(UpdatePolicy::default(), UpdatePolicy::Minor);
    }

    // ========================================================================
    // RefStyle tests
    // ========================================================================

    #[test]
    fn test_ref_style_from_str() {
        assert_eq!(
            RefStyle::from_str_loose("preserve"),
            Some(RefStyle::Preserve)
        );
        assert_eq!(RefStyle::from_str_loose("KEEP"), Some(RefStyle::Preserve));
        assert_eq!(
            RefStyle::from_str_loose("original"),
            Some(RefStyle::Preserve)
        );

        assert_eq!(RefStyle::from_str_loose("tag"), Some(RefStyle::Tag));
        assert_eq!(RefStyle::from_str_loose("TAGS"), Some(RefStyle::Tag));
        assert_eq!(RefStyle::from_str_loose("version"), Some(RefStyle::Tag));

        assert_eq!(RefStyle::from_str_loose("SHA"), Some(RefStyle::Sha));
        assert_eq!(RefStyle::from_str_loose("commit"), Some(RefStyle::Sha));
        assert_eq!(RefStyle::from_str_loose("pin"), Some(RefStyle::Sha));

        assert_eq!(RefStyle::from_str_loose("invalid"), None);
    }

    #[test]
    fn test_ref_style_display() {
        assert_eq!(RefStyle::Preserve.to_string(), "preserve");
        assert_eq!(RefStyle::Tag.to_string(), "tag");
        assert_eq!(RefStyle::Sha.to_string(), "sha");
    }

    #[test]
    fn test_ref_style_default() {
        assert_eq!(RefStyle::default(), RefStyle::Preserve);
    }

    // ========================================================================
    // BranchPolicy tests
    // ========================================================================

    #[test]
    fn test_branch_policy_from_str() {
        assert_eq!(
            BranchPolicy::from_str_loose("ignore"),
            Some(BranchPolicy::Ignore)
        );
        assert_eq!(
            BranchPolicy::from_str_loose("SKIP"),
            Some(BranchPolicy::Ignore)
        );

        assert_eq!(
            BranchPolicy::from_str_loose("warn"),
            Some(BranchPolicy::Warn)
        );
        assert_eq!(
            BranchPolicy::from_str_loose("WARNING"),
            Some(BranchPolicy::Warn)
        );

        assert_eq!(
            BranchPolicy::from_str_loose("update"),
            Some(BranchPolicy::Update)
        );
        assert_eq!(
            BranchPolicy::from_str_loose("PIN"),
            Some(BranchPolicy::Update)
        );

        assert_eq!(BranchPolicy::from_str_loose("invalid"), None);
    }

    #[test]
    fn test_branch_policy_display() {
        assert_eq!(BranchPolicy::Ignore.to_string(), "ignore");
        assert_eq!(BranchPolicy::Warn.to_string(), "warn");
        assert_eq!(BranchPolicy::Update.to_string(), "update");
    }

    // ========================================================================
    // Deserialization tests
    // ========================================================================

    #[test]
    fn test_update_policy_deserialize() {
        let json = r#""patch""#;
        let policy: UpdatePolicy = serde_json::from_str(json).unwrap();
        assert_eq!(policy, UpdatePolicy::Patch);

        let json = r#""LATEST""#;
        let policy: UpdatePolicy = serde_json::from_str(json).unwrap();
        assert_eq!(policy, UpdatePolicy::Latest);
    }

    #[test]
    fn test_ref_style_deserialize() {
        let json = r#""sha""#;
        let style: RefStyle = serde_json::from_str(json).unwrap();
        assert_eq!(style, RefStyle::Sha);

        let json = r#""TAG""#;
        let style: RefStyle = serde_json::from_str(json).unwrap();
        assert_eq!(style, RefStyle::Tag);
    }

    #[test]
    fn test_branch_policy_deserialize() {
        let json = r#""warn""#;
        let policy: BranchPolicy = serde_json::from_str(json).unwrap();
        assert_eq!(policy, BranchPolicy::Warn);

        let json = r#""UPDATE""#;
        let policy: BranchPolicy = serde_json::from_str(json).unwrap();
        assert_eq!(policy, BranchPolicy::Update);
    }

    #[test]
    fn test_deserialize_invalid_values() {
        let result: Result<UpdatePolicy, _> = serde_json::from_str(r#""invalid""#);
        assert!(result.is_err());

        let result: Result<RefStyle, _> = serde_json::from_str(r#""invalid""#);
        assert!(result.is_err());

        let result: Result<BranchPolicy, _> = serde_json::from_str(r#""invalid""#);
        assert!(result.is_err());
    }
}
