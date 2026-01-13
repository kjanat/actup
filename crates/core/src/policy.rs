//! Update policy and ref style configuration
//!
//! Controls how aggressively to update and what format to use for refs.

use serde::de::{self, Visitor};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Update policy determines what version bumps are allowed
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdatePolicy {
    /// Only patch updates within same major.minor (1.2.3 -> 1.2.4)
    Patch,

    /// Minor updates within same major (1.2.x -> 1.3.x)
    #[default]
    Minor,

    /// Major updates allowed (1.x -> 2.x)
    Major,

    /// Always update to absolute latest, regardless of major version
    Latest,
}

impl UpdatePolicy {
    /// Returns true if this policy allows major version bumps
    #[must_use]
    pub fn allows_major(&self) -> bool {
        matches!(self, Self::Major | Self::Latest)
    }

    /// Returns true if this policy allows minor version bumps
    #[must_use]
    pub fn allows_minor(&self) -> bool {
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

/// How to format version references when updating
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RefStyle {
    /// Keep the original style (tag stays tag, SHA stays SHA)
    #[default]
    Preserve,

    /// Always use version tags (v1.2.3)
    Tag,

    /// Always pin to full commit SHA (most secure)
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

/// Branch handling policy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BranchPolicy {
    /// Ignore branch refs (don't flag as outdated, but maybe warn)
    #[default]
    Ignore,

    /// Warn about branch refs (not stable, recommend pinning)
    Warn,

    /// Treat branch refs as updatable (pin to latest commit)
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
mod tests {
    use super::*;

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
    fn test_policy_from_str() {
        assert_eq!(
            UpdatePolicy::from_str_loose("PATCH"),
            Some(UpdatePolicy::Patch)
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
    fn test_ref_style_from_str() {
        assert_eq!(RefStyle::from_str_loose("SHA"), Some(RefStyle::Sha));
        assert_eq!(RefStyle::from_str_loose("commit"), Some(RefStyle::Sha));
        assert_eq!(RefStyle::from_str_loose("pin"), Some(RefStyle::Sha));
    }
}
