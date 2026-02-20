//! Parser for `uses:` strings in GitHub Actions workflows
//!
//! Handles various formats:
//! - `owner/repo@ref`
//! - `owner/repo/path@ref` (reusable workflow or subpath action)
//! - `host/owner/repo@ref` (explicit host)
//! - `./path` (local, returned as None)

use crate::{ActionRef, Error, Result};

/// Parse a `uses:` string into an `ActionRef`
///
/// Returns `Ok(None)` for local references (./path).
///
/// # Errors
///
/// Returns `Err` for malformed references.
pub fn parse_uses(input: &str) -> Result<Option<ActionRef>> {
    let input = input.trim();

    // Local reference
    if input.starts_with("./") || input.starts_with("../") {
        return Ok(None);
    }

    // Docker reference (docker://...)
    if input.starts_with("docker://") {
        return Ok(None);
    }

    // Must have @ for ref
    let (path_part, ref_str) = input.split_once('@').ok_or_else(|| Error::InvalidRef {
        input: input.to_string(),
        reason: "missing '@' separator for version reference".to_string(),
    })?;

    if ref_str.is_empty() {
        return Err(Error::InvalidRef {
            input: input.to_string(),
            reason: "empty version reference after '@'".to_string(),
        });
    }

    // Split the path part by '/' using iterator
    let mut parts = path_part.split('/');

    let first = parts.next().ok_or_else(|| Error::InvalidRef {
        input: input.to_string(),
        reason: "expected at least owner/repo".to_string(),
    })?;

    let second = parts.next().ok_or_else(|| Error::InvalidRef {
        input: input.to_string(),
        reason: "expected at least owner/repo".to_string(),
    })?;

    // Determine if first part is a host (contains '.' or is a known pattern)
    let (host, owner, repo, subpath) = if looks_like_host(first) {
        // host/owner/repo[@ref] or host/owner/repo/path[@ref]
        let third = parts.next().ok_or_else(|| Error::InvalidRef {
            input: input.to_string(),
            reason: "with host prefix, expected host/owner/repo".to_string(),
        })?;
        let subpath: Option<String> = {
            let rest: Vec<_> = parts.collect();
            if rest.is_empty() {
                None
            } else {
                Some(rest.join("/"))
            }
        };
        (
            Some(first.to_string()),
            second.to_string(),
            third.to_string(),
            subpath,
        )
    } else {
        // owner/repo[@ref] or owner/repo/path[@ref]
        let subpath: Option<String> = {
            let rest: Vec<_> = parts.collect();
            if rest.is_empty() {
                None
            } else {
                Some(rest.join("/"))
            }
        };
        (None, first.to_string(), second.to_string(), subpath)
    };

    // Validate owner and repo aren't empty
    if owner.is_empty() {
        return Err(Error::InvalidRef {
            input: input.to_string(),
            reason: "empty owner".to_string(),
        });
    }

    if repo.is_empty() {
        return Err(Error::InvalidRef {
            input: input.to_string(),
            reason: "empty repo".to_string(),
        });
    }

    Ok(Some(ActionRef::new(host, owner, repo, subpath, ref_str)))
}

/// Check if a string looks like a hostname
fn looks_like_host(s: &str) -> bool {
    // Contains a dot (like github.com, ghe.corp.internal)
    if s.contains('.') {
        return true;
    }

    // localhost
    if s == "localhost" {
        return true;
    }

    // Contains a port (like localhost:8080)
    if s.contains(':') {
        return true;
    }

    false
}

/// Extract all `uses:` values from a YAML workflow file
///
/// Returns a list of (`line_number`, column, `uses_value`) tuples.
/// Line numbers are 1-indexed.
#[must_use]
pub fn extract_uses_from_yaml(content: &str) -> Vec<UsesLocation> {
    let mut results = Vec::new();

    for (line_idx, line) in content.lines().enumerate() {
        let line_num = line_idx + 1;

        // Find "uses:" in the line
        if let Some(pos) = line.find("uses:") {
            let after_uses = &line[pos + 5..];
            let leading_ws = after_uses.len() - after_uses.trim_start().len();
            let trimmed = after_uses.trim();

            // Handle quoted values
            let (value, quote_offset) = if trimmed.starts_with('"') || trimmed.starts_with('\'') {
                let quote = trimmed.chars().next().unwrap_or('"');
                let inner = trimmed
                    .strip_prefix(quote)
                    .and_then(|s| s.strip_suffix(quote))
                    .unwrap_or(trimmed);
                (inner, 1) // +1 for opening quote
            } else {
                // Unquoted, take until whitespace or comment
                (trimmed.split_whitespace().next().unwrap_or(trimmed), 0)
            };

            if !value.is_empty() {
                // Calculate column (1-indexed): pos of "uses:" + 5 + whitespace + quote
                let value_start = pos + 5 + leading_ws + quote_offset;
                results.push(UsesLocation::new(
                    line_num,
                    value_start + 1,
                    value.to_string(),
                    line.to_string(),
                ));
            }
        }
    }

    results
}

/// Location of a `uses:` value in a file
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UsesLocation {
    /// Line number (1-indexed)
    line: usize,
    /// Column number (1-indexed)
    column: usize,
    /// The uses value
    value: String,
    /// The full line (for context)
    raw_line: String,
}

impl UsesLocation {
    /// Create a new `UsesLocation`
    #[must_use]
    pub const fn new(line: usize, column: usize, value: String, raw_line: String) -> Self {
        Self {
            line,
            column,
            value,
            raw_line,
        }
    }

    /// Get the line number
    #[must_use]
    pub const fn line(&self) -> usize {
        self.line
    }

    /// Get the column number
    #[must_use]
    pub const fn column(&self) -> usize {
        self.column
    }

    /// Get the uses value
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Get the raw line
    #[must_use]
    pub fn raw_line(&self) -> &str {
        &self.raw_line
    }
}

impl UsesLocation {
    /// Get the byte range of the uses value in the line
    #[must_use]
    pub const fn value_range(&self) -> std::ops::Range<usize> {
        let start = self.column - 1;
        start..start + self.value.len()
    }

    /// Get just the ref portion range (after @)
    #[must_use]
    pub fn ref_range(&self) -> Option<std::ops::Range<usize>> {
        let at_pos = self.value.find('@')?;
        let start = self.column - 1 + at_pos + 1; // +1 to skip @
        let end = self.column - 1 + self.value.len();
        Some(start..end)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple() {
        let result = parse_uses("actions/checkout@v4").unwrap().unwrap();
        assert_eq!(result.owner(), "actions");
        assert_eq!(result.repo(), "checkout");
        assert_eq!(result.ref_str(), "v4");
        assert!(result.host().is_none());
        assert!(result.subpath().is_none());
    }

    #[test]
    fn test_parse_with_subpath() {
        let result = parse_uses("org/repo/.github/workflows/ci.yml@main")
            .unwrap()
            .unwrap();
        assert_eq!(result.owner(), "org");
        assert_eq!(result.repo(), "repo");
        assert_eq!(result.subpath(), Some(".github/workflows/ci.yml"));
        assert_eq!(result.ref_str(), "main");
    }

    #[test]
    fn test_parse_with_host() {
        let result = parse_uses("ghe.corp.com/org/action@v1").unwrap().unwrap();
        assert_eq!(result.host(), Some("ghe.corp.com"));
        assert_eq!(result.owner(), "org");
        assert_eq!(result.repo(), "action");
        assert_eq!(result.ref_str(), "v1");
    }

    #[test]
    fn test_parse_sha() {
        let sha = "a".repeat(40);
        let result = parse_uses(&format!("actions/checkout@{sha}"))
            .unwrap()
            .unwrap();
        assert_eq!(result.ref_str(), &sha);
        assert!(result.is_sha());
    }

    #[test]
    fn test_parse_local() {
        assert!(parse_uses("./local/action").unwrap().is_none());
        assert!(parse_uses("../parent/action").unwrap().is_none());
    }

    #[test]
    fn test_parse_docker() {
        assert!(parse_uses("docker://alpine:3.8").unwrap().is_none());
    }

    #[test]
    fn test_parse_invalid() {
        assert!(parse_uses("no-at-sign").is_err());
        assert!(parse_uses("missing@").is_err());
        assert!(parse_uses("@no-path").is_err());
    }

    #[test]
    fn test_extract_uses() {
        let yaml = r#"
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: "actions/setup-node@v3"
      - uses: './local/action'
"#;
        let locations = extract_uses_from_yaml(yaml);
        assert_eq!(locations.len(), 3);
        assert_eq!(locations[0].value, "actions/checkout@v4");
        assert_eq!(locations[1].value, "actions/setup-node@v3");
        assert_eq!(locations[2].value, "./local/action");
    }

    #[test]
    fn test_uses_location_serde_round_trip() {
        let loc = UsesLocation::new(
            10,
            15,
            "actions/checkout@v4".to_string(),
            "      uses: actions/checkout@v4".to_string(),
        );
        let json = serde_json::to_string(&loc).unwrap();
        let deserialized: UsesLocation = serde_json::from_str(&json).unwrap();
        assert_eq!(loc, deserialized);
    }
}
