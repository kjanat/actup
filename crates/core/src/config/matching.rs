//! Action patterns for `scan.ignore` and `[[overrides]]`.

use std::{fmt, str::FromStr};

use glob::{MatchOptions, Pattern};
use serde::{Deserialize, Serialize};

use crate::{ActionRef, parser::looks_like_host};

const SEGMENT: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

const HOST: MatchOptions = MatchOptions {
    case_sensitive: false,
    ..SEGMENT
};

/// A glob over `[host/]owner/repo[/subpath][@ref]`.
///
/// Each part is a glob where `*` stays within one `/`-separated segment and
/// `**` spans segments. An absent host, subpath or ref matches any value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ActionPattern {
    raw: String,
    host: Option<Pattern>,
    owner: Pattern,
    repo: Pattern,
    subpath: Option<Pattern>,
    ref_name: Option<Pattern>,
}

impl ActionPattern {
    /// Return the pattern as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Return whether `action` matches, using `default_host` for a ref without a host.
    #[must_use]
    pub fn matches(&self, action: &ActionRef, default_host: &str) -> bool {
        let host = action.host().unwrap_or(default_host);
        self.host
            .as_ref()
            .is_none_or(|p| p.matches_with(host, HOST))
            && self.owner.matches_with(action.owner(), SEGMENT)
            && self.repo.matches_with(action.repo(), SEGMENT)
            && self
                .subpath
                .as_ref()
                .is_none_or(|p| action.subpath().is_some_and(|s| p.matches_with(s, SEGMENT)))
            && self
                .ref_name
                .as_ref()
                .is_none_or(|p| p.matches_with(action.ref_str(), SEGMENT))
    }
}

impl FromStr for ActionPattern {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let invalid = |reason: &str| format!("invalid action pattern `{raw}`: {reason}");
        let glob = |part: &str| Pattern::new(part).map_err(|e| invalid(&e.to_string()));

        let (path, ref_name) = match raw.split_once('@') {
            Some((_, "")) => return Err(invalid("empty ref after `@`")),
            Some((path, r)) => (path, Some(r)),
            None => (raw, None),
        };

        let mut segments: Vec<&str> = path.split('/').collect();
        let host = if segments.first().is_some_and(|s| looks_like_host(s)) {
            Some(segments.remove(0))
        } else {
            None
        };
        let [owner, repo, subpath @ ..] = segments.as_slice() else {
            return Err(invalid("expected `[host/]owner/repo[/subpath][@ref]`"));
        };
        if owner.is_empty() || repo.is_empty() || subpath.iter().any(|s| s.is_empty()) {
            return Err(invalid("empty path segment"));
        }

        Ok(Self {
            raw: raw.to_string(),
            host: host.map(glob).transpose()?,
            owner: glob(owner)?,
            repo: glob(repo)?,
            subpath: (!subpath.is_empty())
                .then(|| glob(&subpath.join("/")))
                .transpose()?,
            ref_name: ref_name.map(glob).transpose()?,
        })
    }
}

impl TryFrom<String> for ActionPattern {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl From<ActionPattern> for String {
    fn from(pattern: ActionPattern) -> Self {
        pattern.raw
    }
}

impl fmt::Display for ActionPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(uses: &str) -> ActionRef {
        let Ok(Some(action)) = crate::parse_uses(uses) else {
            panic!("bad uses: {uses}");
        };
        action
    }

    fn matches(pattern: &str, uses: &str) -> Result<bool, String> {
        Ok(pattern
            .parse::<ActionPattern>()?
            .matches(&action(uses), "github.com"))
    }

    #[test]
    fn owner_repo_matches_every_ref_and_subpath() -> Result<(), String> {
        assert!(matches("actions/checkout", "actions/checkout@v4")?);
        assert!(matches("actions/checkout", "actions/checkout/sub@v4")?);
        assert!(matches(
            "actions/checkout",
            "ghe.corp.com/actions/checkout@v4"
        )?);
        assert!(!matches("actions/checkout", "actions/cache@v4")?);
        assert!(!matches("actions/checkout", "Actions/checkout@v4")?);
        Ok(())
    }

    #[test]
    fn globs_stay_within_a_segment() -> Result<(), String> {
        assert!(matches("my-org/*", "my-org/anything@v1")?);
        assert!(matches("*/setup-*", "actions/setup-node@v4")?);
        assert!(!matches("my-org/*", "other-org/anything@v1")?);
        assert!(matches("o/r/tools/*", "o/r/tools/lint@v1")?);
        assert!(!matches("o/r/tools/*", "o/r/tools/lint/deep@v1")?);
        assert!(matches("o/r/tools/**", "o/r/tools/lint/deep@v1")?);
        Ok(())
    }

    #[test]
    fn ref_part_matches_only_that_ref() -> Result<(), String> {
        assert!(matches("owner/repo@v1.91.0", "owner/repo@v1.91.0")?);
        assert!(!matches("owner/repo@v1.91.0", "owner/repo@v2")?);
        assert!(matches("owner/repo@v1.*", "owner/repo@v1.2.3")?);
        Ok(())
    }

    #[test]
    fn host_qualified_pattern_uses_default_host_for_bare_refs() -> Result<(), String> {
        let pattern: ActionPattern = "ghe.corp.com/platform/*".parse()?;
        assert!(pattern.matches(&action("ghe.corp.com/platform/ci@v1"), "github.com"));
        assert!(pattern.matches(&action("GHE.corp.com/platform/ci@v1"), "github.com"));
        assert!(!pattern.matches(&action("platform/ci@v1"), "github.com"));
        assert!(pattern.matches(&action("platform/ci@v1"), "ghe.corp.com"));
        assert!(!pattern.matches(&action("github.com/platform/ci@v1"), "ghe.corp.com"));
        Ok(())
    }

    #[test]
    fn subpath_pattern_requires_a_subpath() -> Result<(), String> {
        assert!(matches(
            "o/r/.github/workflows/*.yml",
            "o/r/.github/workflows/ci.yml@v1"
        )?);
        assert!(!matches("o/r/.github/workflows/*.yml", "o/r@v1")?);
        Ok(())
    }

    #[test]
    fn rejects_malformed_patterns() {
        for raw in [
            "checkout",
            "owner/",
            "/repo",
            "owner/repo@",
            "o/r//x",
            "o/[r",
        ] {
            let err = raw.parse::<ActionPattern>().err();
            assert!(
                err.as_deref()
                    .is_some_and(|e| e.contains("invalid action pattern")),
                "{raw}: {err:?}"
            );
        }
    }

    #[test]
    fn serde_round_trips_the_raw_string() -> Result<(), Box<dyn std::error::Error>> {
        let pattern: ActionPattern = serde_json::from_str("\"ghe.corp.com/o/*@v1\"")?;
        assert_eq!(pattern.as_str(), "ghe.corp.com/o/*@v1");
        assert_eq!(pattern.to_string(), "ghe.corp.com/o/*@v1");
        assert_eq!(serde_json::to_string(&pattern)?, "\"ghe.corp.com/o/*@v1\"");
        assert!(serde_json::from_str::<ActionPattern>("\"nope\"").is_err());
        Ok(())
    }
}
