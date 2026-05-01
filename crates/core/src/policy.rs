//! Update policy and ref style configuration
//!
//! Controls how aggressively to update and what format to use for refs.

use std::fmt;

use schemars::JsonSchema;
use serde::{
    Deserialize, Serialize,
    de::{self, Visitor},
};

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
pub enum Update {
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
    #[default]
    Latest,
}

impl Update {
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

impl fmt::Display for Update {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Patch => write!(f, "patch"),
            Self::Minor => write!(f, "minor"),
            Self::Major => write!(f, "major"),
            Self::Latest => write!(f, "latest"),
        }
    }
}

impl<'de> Deserialize<'de> for Update {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct UpdateVisitor;

        impl Visitor<'_> for UpdateVisitor {
            type Value = Update;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("one of: patch, minor, major, latest")
            }

            fn visit_str<E>(self, value: &str) -> Result<Update, E>
            where
                E: de::Error,
            {
                Update::from_str_loose(value)
                    .ok_or_else(|| de::Error::invalid_value(de::Unexpected::Str(value), &self))
            }
        }

        deserializer.deserialize_str(UpdateVisitor)
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
pub enum Style {
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

impl Style {
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

impl fmt::Display for Style {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preserve => write!(f, "preserve"),
            Self::Tag => write!(f, "tag"),
            Self::Sha => write!(f, "sha"),
        }
    }
}

impl<'de> Deserialize<'de> for Style {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct StyleVisitor;

        impl Visitor<'_> for StyleVisitor {
            type Value = Style;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("one of: preserve, tag, sha")
            }

            fn visit_str<E>(self, value: &str) -> Result<Style, E>
            where
                E: de::Error,
            {
                Style::from_str_loose(value)
                    .ok_or_else(|| de::Error::invalid_value(de::Unexpected::Str(value), &self))
            }
        }

        deserializer.deserialize_str(StyleVisitor)
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
pub enum Branch {
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

impl Branch {
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

impl fmt::Display for Branch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ignore => write!(f, "ignore"),
            Self::Warn => write!(f, "warn"),
            Self::Update => write!(f, "update"),
        }
    }
}

impl<'de> Deserialize<'de> for Branch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct BranchVisitor;

        impl Visitor<'_> for BranchVisitor {
            type Value = Branch;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("one of: ignore, warn, update")
            }

            fn visit_str<E>(self, value: &str) -> Result<Branch, E>
            where
                E: de::Error,
            {
                Branch::from_str_loose(value)
                    .ok_or_else(|| de::Error::invalid_value(de::Unexpected::Str(value), &self))
            }
        }

        deserializer.deserialize_str(BranchVisitor)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // Update tests

    #[test]
    fn test_update_policy_allows() {
        assert!(!Update::Patch.allows_major());
        assert!(!Update::Patch.allows_minor());

        assert!(!Update::Minor.allows_major());
        assert!(Update::Minor.allows_minor());

        assert!(Update::Major.allows_major());
        assert!(Update::Major.allows_minor());

        assert!(Update::Latest.allows_major());
        assert!(Update::Latest.allows_minor());
    }

    #[test]
    fn test_update_policy_from_str() {
        assert_eq!(Update::from_str_loose("PATCH"), Some(Update::Patch));
        assert_eq!(Update::from_str_loose("minor"), Some(Update::Minor));
        assert_eq!(Update::from_str_loose("MAJOR"), Some(Update::Major));
        assert_eq!(Update::from_str_loose("latest"), Some(Update::Latest));
        assert_eq!(Update::from_str_loose("all"), Some(Update::Latest));
        assert_eq!(Update::from_str_loose("any"), Some(Update::Latest));
        assert_eq!(Update::from_str_loose("invalid"), None);
    }

    #[test]
    fn test_update_policy_display() {
        assert_eq!(Update::Patch.to_string(), "patch");
        assert_eq!(Update::Minor.to_string(), "minor");
        assert_eq!(Update::Major.to_string(), "major");
        assert_eq!(Update::Latest.to_string(), "latest");
    }

    #[test]
    fn test_update_policy_default() {
        assert_eq!(Update::default(), Update::Latest);
    }

    // Style tests

    #[test]
    fn test_ref_style_from_str() {
        assert_eq!(Style::from_str_loose("preserve"), Some(Style::Preserve));
        assert_eq!(Style::from_str_loose("KEEP"), Some(Style::Preserve));
        assert_eq!(Style::from_str_loose("original"), Some(Style::Preserve));

        assert_eq!(Style::from_str_loose("tag"), Some(Style::Tag));
        assert_eq!(Style::from_str_loose("TAGS"), Some(Style::Tag));
        assert_eq!(Style::from_str_loose("version"), Some(Style::Tag));

        assert_eq!(Style::from_str_loose("SHA"), Some(Style::Sha));
        assert_eq!(Style::from_str_loose("commit"), Some(Style::Sha));
        assert_eq!(Style::from_str_loose("pin"), Some(Style::Sha));

        assert_eq!(Style::from_str_loose("invalid"), None);
    }

    #[test]
    fn test_ref_style_display() {
        assert_eq!(Style::Preserve.to_string(), "preserve");
        assert_eq!(Style::Tag.to_string(), "tag");
        assert_eq!(Style::Sha.to_string(), "sha");
    }

    #[test]
    fn test_ref_style_default() {
        assert_eq!(Style::default(), Style::Preserve);
    }

    // Branch tests

    #[test]
    fn test_branch_policy_from_str() {
        assert_eq!(Branch::from_str_loose("ignore"), Some(Branch::Ignore));
        assert_eq!(Branch::from_str_loose("SKIP"), Some(Branch::Ignore));

        assert_eq!(Branch::from_str_loose("warn"), Some(Branch::Warn));
        assert_eq!(Branch::from_str_loose("WARNING"), Some(Branch::Warn));

        assert_eq!(Branch::from_str_loose("update"), Some(Branch::Update));
        assert_eq!(Branch::from_str_loose("PIN"), Some(Branch::Update));

        assert_eq!(Branch::from_str_loose("invalid"), None);
    }

    #[test]
    fn test_branch_policy_display() {
        assert_eq!(Branch::Ignore.to_string(), "ignore");
        assert_eq!(Branch::Warn.to_string(), "warn");
        assert_eq!(Branch::Update.to_string(), "update");
    }

    // Deserialization tests

    #[test]
    fn test_update_policy_deserialize() {
        let json = r#""patch""#;
        let policy: Update = serde_json::from_str(json).unwrap();
        assert_eq!(policy, Update::Patch);

        let json = r#""LATEST""#;
        let policy: Update = serde_json::from_str(json).unwrap();
        assert_eq!(policy, Update::Latest);
    }

    #[test]
    fn test_ref_style_deserialize() {
        let json = r#""sha""#;
        let style: Style = serde_json::from_str(json).unwrap();
        assert_eq!(style, Style::Sha);

        let json = r#""TAG""#;
        let style: Style = serde_json::from_str(json).unwrap();
        assert_eq!(style, Style::Tag);
    }

    #[test]
    fn test_branch_policy_deserialize() {
        let json = r#""warn""#;
        let policy: Branch = serde_json::from_str(json).unwrap();
        assert_eq!(policy, Branch::Warn);

        let json = r#""UPDATE""#;
        let policy: Branch = serde_json::from_str(json).unwrap();
        assert_eq!(policy, Branch::Update);
    }

    #[test]
    fn test_deserialize_invalid_values() {
        let result: Result<Update, _> = serde_json::from_str(r#""invalid""#);
        assert!(result.is_err());

        let result: Result<Style, _> = serde_json::from_str(r#""invalid""#);
        assert!(result.is_err());

        let result: Result<Branch, _> = serde_json::from_str(r#""invalid""#);
        assert!(result.is_err());
    }
}
