//! Parser for `uses:` strings in GitHub Actions workflows
//!
//! Handles various formats:
//! - `owner/repo@ref`
//! - `owner/repo/path@ref` (reusable workflow or subpath action)
//! - `host/owner/repo@ref` (explicit host)
//! - `./path` (local, returned as None)

use saphyr_parser::{Event, Parser, ScalarStyle, Span};

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

/// Extract all `uses:` values from a YAML workflow file.
///
/// Walks the document structurally with a real YAML parser, so it finds `uses:`
/// regardless of serialization (block or flow style, quoted keys, the value on a
/// continuation line, etc.) and never matches `uses:` text inside a `run:`
/// script. Each result carries the line/column of the value as it appears in the
/// source so callers can build LSP ranges and surgical `@ref` edits.
///
/// If the document is not valid YAML (e.g. a half-typed buffer in the LSP), the
/// refs found in the valid prefix are returned and parsing stops at the error —
/// no brittle text-scanning fallback.
#[must_use]
pub fn extract_uses_from_yaml(content: &str) -> Vec<UsesLocation> {
    let mut results = Vec::new();
    let mut stack: Vec<Ctx> = Vec::new();

    for event in Parser::new_from_str(content) {
        let Ok((event, span)) = event else {
            break;
        };

        match event {
            Event::Scalar(value, style, _, _) => match classify_node(&mut stack) {
                NodeRole::MapKey => {
                    if let Some(Ctx::Map {
                        pending_uses_key, ..
                    }) = stack.last_mut()
                    {
                        *pending_uses_key = value.as_ref() == "uses";
                    }
                }
                NodeRole::MapValue { key_was_uses: true } => {
                    if let Some(loc) = build_location(content, value.as_ref(), style, span) {
                        results.push(loc);
                    }
                }
                NodeRole::MapValue { .. } | NodeRole::Item | NodeRole::Root => {}
            },
            Event::MappingStart(..) => {
                classify_node(&mut stack);
                stack.push(Ctx::Map {
                    expect_key: true,
                    pending_uses_key: false,
                });
            }
            Event::SequenceStart(..) => {
                classify_node(&mut stack);
                stack.push(Ctx::Seq);
            }
            Event::Alias(_) => {
                classify_node(&mut stack);
            }
            Event::MappingEnd | Event::SequenceEnd => {
                stack.pop();
            }
            Event::Nothing
            | Event::StreamStart
            | Event::StreamEnd
            | Event::DocumentStart(_)
            | Event::DocumentEnd => {}
        }
    }

    results
}

/// A collection context on the structural walk stack.
enum Ctx {
    /// A YAML mapping. `expect_key` toggles between key and value positions;
    /// `pending_uses_key` records whether the current key scalar was `uses`.
    Map {
        expect_key: bool,
        pending_uses_key: bool,
    },
    /// A YAML sequence. Every child is an item; no key/value distinction.
    Seq,
}

/// The structural role of the node currently being visited.
enum NodeRole {
    /// A mapping key.
    MapKey,
    /// A mapping value; `key_was_uses` is whether its key was the `uses` scalar.
    MapValue { key_was_uses: bool },
    /// An item inside a sequence.
    Item,
    /// A top-level node (no enclosing collection).
    Root,
}

/// Classify the node about to be visited against its enclosing collection and
/// advance that collection's key/value state.
const fn classify_node(stack: &mut [Ctx]) -> NodeRole {
    match stack.last_mut() {
        Some(Ctx::Map {
            expect_key,
            pending_uses_key,
        }) => {
            if *expect_key {
                *expect_key = false;
                NodeRole::MapKey
            } else {
                *expect_key = true;
                let key_was_uses = *pending_uses_key;
                *pending_uses_key = false;
                NodeRole::MapValue { key_was_uses }
            }
        }
        Some(Ctx::Seq) => NodeRole::Item,
        None => NodeRole::Root,
    }
}

/// Build a [`UsesLocation`] for a `uses:` value scalar.
///
/// `span.start.col()` is 0-indexed and, for quoted scalars, points at the
/// opening quote while `value` holds the unquoted content — so skip one column
/// for quoted styles to land on the content, then convert to the 1-indexed
/// column [`UsesLocation::new`] expects.
fn build_location(
    content: &str,
    value: &str,
    style: ScalarStyle,
    span: Span,
) -> Option<UsesLocation> {
    if value.is_empty() {
        return None;
    }
    let line = span.start.line(); // 1-indexed
    let raw_line = content.lines().nth(line - 1)?.to_string();
    let quote_skip = usize::from(matches!(
        style,
        ScalarStyle::SingleQuoted | ScalarStyle::DoubleQuoted
    ));
    let column = span.start.col() + quote_skip + 1;
    Some(UsesLocation::new(line, column, value.to_string(), raw_line))
}

/// Byte range of a substring within a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tsify", derive(tsify_next::Tsify))]
pub struct RefRange {
    /// 0-indexed inclusive start byte offset.
    pub start: usize,
    /// 0-indexed exclusive end byte offset.
    pub end: usize,
}

/// Location of a `uses:` value in a file
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tsify", derive(tsify_next::Tsify))]
pub struct UsesLocation {
    /// Line number (1-indexed)
    line: usize,
    /// Column number (1-indexed)
    column: usize,
    /// The uses value
    value: String,
    /// The full line (for context)
    raw_line: String,
    /// Byte range of the ref portion (after `@`) within the line. Present
    /// only if the value contains `@`. Computed at construction.
    ref_range: Option<RefRange>,
}

impl UsesLocation {
    /// Create a new `UsesLocation`
    #[must_use]
    pub fn new(line: usize, column: usize, value: String, raw_line: String) -> Self {
        let ref_range = value.find('@').map(|at_pos| RefRange {
            start: column - 1 + at_pos + 1,
            end: column - 1 + value.len(),
        });
        Self {
            line,
            column,
            value,
            raw_line,
            ref_range,
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
        self.ref_range.map(|r| r.start..r.end)
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
    fn test_extract_value_on_continuation_line() {
        // `uses:` key and its value on separate physical lines — valid YAML the
        // old line scanner silently dropped (value reported nothing => "up-to-date").
        let yaml = "steps:\n  - uses:\n      actions/checkout@v4\n";
        let locations = extract_uses_from_yaml(yaml);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].value, "actions/checkout@v4");
        // Location must point at the line the value actually lives on.
        assert_eq!(locations[0].line, 3);
    }

    #[test]
    fn test_extract_quoted_key() {
        // Quoted mapping key — `find("uses:")` never matches the substring.
        let yaml = "steps:\n  - \"uses\": actions/checkout@v4\n";
        let locations = extract_uses_from_yaml(yaml);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].value, "actions/checkout@v4");
    }

    #[test]
    fn test_extract_spaced_key() {
        // Space before the colon is valid YAML; the substring is `uses ` not `uses:`.
        let yaml = "steps:\n  - uses : actions/checkout@v4\n";
        let locations = extract_uses_from_yaml(yaml);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].value, "actions/checkout@v4");
    }

    #[test]
    fn test_extract_flow_mapping() {
        // Flow style: old scanner grabbed `actions/checkout@v4,` (trailing comma).
        let yaml = "steps:\n  - { uses: actions/checkout@v4, with: x }\n";
        let locations = extract_uses_from_yaml(yaml);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].value, "actions/checkout@v4");
    }

    #[test]
    fn test_extract_ignores_uses_inside_run_script() {
        // `uses:` appearing in a shell script value must NOT be extracted.
        let yaml = "steps:\n  - run: |\n      echo \"uses: fake/thing@v1\"\n";
        let locations = extract_uses_from_yaml(yaml);
        assert!(
            locations.is_empty(),
            "extracted bogus refs from a run script: {locations:?}"
        );
    }

    #[test]
    fn test_extract_quoted_ref_range_is_correct() {
        // Quoted value: ref_range must land on the ref content, skipping the quote.
        let yaml = "steps:\n  - uses: \"actions/checkout@v4\"\n";
        let locations = extract_uses_from_yaml(yaml);
        assert_eq!(locations.len(), 1);
        let loc = &locations[0];
        let range = loc.ref_range().unwrap();
        assert_eq!(&loc.raw_line[range], "v4");
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
