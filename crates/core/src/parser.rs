//! Parser for `uses:` strings in GitHub Actions workflows
//!
//! Handles various formats:
//! - `owner/repo@ref`
//! - `owner/repo/path@ref` (reusable workflow or subpath action)
//! - `host/owner/repo@ref` (explicit host)
//! - `./path` (local, returned as None)

use std::{collections::BTreeSet, net::Ipv6Addr, ops::Range};

use saphyr_parser::{Event, Parser, ScalarStyle, Span};

use crate::{ActionRef, ActionRepoId, Error, Result};

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
    check_ref_name(ref_str).map_err(|reason| Error::InvalidRef {
        input: input.to_string(),
        reason,
    })?;

    let id = parse_path(input, path_part)?;
    Ok(Some(ActionRef::new(
        id.host().map(str::to_owned),
        id.owner(),
        id.repo(),
        id.subpath().map(str::to_owned),
        ref_str,
    )))
}

/// What a `uses:` string names.
#[derive(Debug, Clone)]
pub enum Uses {
    /// A local path or a `docker://` image.
    Local,
    /// A remote reference that parses.
    Action(ActionRef),
    /// A remote reference that fails to parse.
    Invalid {
        /// The `[host/]owner/repo[/subpath]@ref` parts of the input, split as written.
        action: ActionRef,
        /// The `InvalidRef` error from [`parse_uses`].
        error: Error,
    },
}

/// Sort a `uses:` string into a local, a valid or an invalid reference.
#[must_use]
pub fn classify_uses(input: &str) -> Uses {
    match parse_uses(input) {
        Ok(None) => Uses::Local,
        Ok(Some(action)) => Uses::Action(action),
        Err(error) => Uses::Invalid {
            action: split_uses(input),
            error,
        },
    }
}

fn split_uses(input: &str) -> ActionRef {
    let input = input.trim();
    let (path, reference) = input.split_once('@').unwrap_or((input, ""));
    let segments: Vec<&str> = path.split('/').collect();
    let (host, rest) = match segments.split_first() {
        Some((first, rest)) if names_host(first, segments.len()) => {
            (Some(first.to_ascii_lowercase()), rest)
        }
        _ => (None, segments.as_slice()),
    };
    let part = |i: usize| rest.get(i).copied().unwrap_or_default();
    let subpath = rest
        .get(2..)
        .filter(|tail| !tail.is_empty())
        .map(|tail| tail.join("/"));
    ActionRef::new(host, part(0), part(1), subpath, reference)
}

/// Parse the `[host/]owner/repo[/subpath]` part of a `uses:` string.
///
/// # Errors
///
/// Returns `Err` for malformed paths.
pub fn parse_action_path(path: &str) -> Result<ActionRepoId> {
    parse_path(path, path)
}

fn parse_path(input: &str, path: &str) -> Result<ActionRepoId> {
    let segments: Vec<&str> = path.split('/').collect();
    let (host, rest) = match segments.split_first() {
        Some((first, rest)) if names_host(first, segments.len()) => {
            (Some(first.to_ascii_lowercase()), rest)
        }
        _ => (None, segments.as_slice()),
    };

    let [owner, repo, subpath @ ..] = rest else {
        return Err(Error::InvalidRef {
            input: input.to_string(),
            reason: "expected at least owner/repo".to_string(),
        });
    };
    check_name(input, "owner", owner)?;
    check_name(input, "repo", repo)?;
    if let Some(segment) = subpath.iter().find(|s| matches!(**s, "" | "." | "..")) {
        return Err(Error::InvalidRef {
            input: input.to_string(),
            reason: format!("path segment `{segment}` is empty, `.` or `..`"),
        });
    }
    let subpath = (!subpath.is_empty()).then(|| subpath.join("/"));

    Ok(ActionRepoId::new(host, *owner, *repo, subpath))
}

fn check_name(input: &str, field: &str, name: &str) -> Result<()> {
    let valid = !matches!(name, "" | "." | "..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if valid {
        return Ok(());
    }
    Err(Error::InvalidRef {
        input: input.to_string(),
        reason: format!("{field} `{name}` must match `[A-Za-z0-9._-]+`"),
    })
}

/// Check `name` against the rules of `git check-ref-format` for a ref name.
///
/// # Errors
///
/// Returns the rule that `name` breaks.
pub fn check_ref_name(name: &str) -> std::result::Result<(), String> {
    const FORBIDDEN: [char; 8] = [' ', '~', '^', ':', '?', '*', '[', '\\'];
    if name.is_empty() || name == "@" {
        return Err(format!("`{name}` is not a ref name"));
    }
    if let Some(c) = name
        .chars()
        .find(|c| c.is_ascii_control() || FORBIDDEN.contains(c))
    {
        return Err(format!("ref `{name}` holds the character {c:?}"));
    }
    if name.contains("..") || name.contains("@{") {
        return Err(format!("ref `{name}` holds `..` or `@{{`"));
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return Err(format!("ref `{name}` has an empty `/` component"));
    }
    if name.ends_with('.') {
        return Err(format!("ref `{name}` ends with `.`"));
    }
    let is_lock = |c: &str| {
        std::path::Path::new(c)
            .extension()
            .is_some_and(|ext| ext == "lock")
    };
    name.split('/')
        .find(|c| c.starts_with('.') || is_lock(c))
        .map_or(Ok(()), |component| {
            Err(format!(
                "ref component `{component}` starts with `.` or ends with `.lock`"
            ))
        })
}

/// Whether `segment` names a host when it starts a path of three or more segments.
#[must_use]
pub fn is_host_segment(segment: &str) -> bool {
    names_host(segment, 3)
}

pub(crate) fn names_host(first: &str, segments: usize) -> bool {
    if segments < 3 {
        return false;
    }
    if let Some(literal) = first.strip_prefix('[') {
        return literal.split_once(']').is_some_and(|(addr, port)| {
            addr.parse::<Ipv6Addr>().is_ok()
                && (port.is_empty() || port.strip_prefix(':').is_some_and(is_port))
        });
    }
    let is_dns_name = |name: &str| name.split('.').all(is_dns_label);
    if let Some((name, port)) = first.rsplit_once(':') {
        return is_dns_name(name) && is_port(port);
    }
    is_dns_name(first) && (first.contains('.') || first.eq_ignore_ascii_case("localhost"))
}

fn is_dns_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(head), Some(tail)) => {
            head.is_ascii_alphanumeric()
                && tail.is_ascii_alphanumeric()
                && bytes
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        }
        _ => false,
    }
}

fn is_port(port: &str) -> bool {
    port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>().is_ok()
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
///
/// Lines are numbered as [`line_spans`] splits them. A leading byte order mark
/// stays part of line 1.
#[must_use]
pub fn extract_uses_from_yaml(content: &str) -> Vec<UsesLocation> {
    walk_uses(content).0
}

/// Why an edited document fails [`validate_edit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditValidationError {
    /// The edited document is not valid YAML.
    Syntax(String),
    /// The edited document holds a different number of `uses:` values.
    UsesCount {
        /// `uses:` values before the edit.
        before: usize,
        /// `uses:` values after the edit.
        after: usize,
    },
}

impl std::fmt::Display for EditValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Syntax(reason) => write!(f, "invalid YAML: {reason}"),
            Self::UsesCount { before, after } => {
                write!(f, "`uses:` count changed from {before} to {after}")
            }
        }
    }
}

impl std::error::Error for EditValidationError {}

/// Check that `after` parses as YAML to the end and holds as many `uses:` values as `before`.
///
/// # Errors
///
/// Returns the first YAML error in `after`, or the two `uses:` counts when they differ.
pub fn validate_edit(before: &str, after: &str) -> std::result::Result<(), EditValidationError> {
    let (locations, error) = walk_uses(after);
    if let Some(reason) = error {
        return Err(EditValidationError::Syntax(reason));
    }
    let before = extract_uses_from_yaml(before).len();
    if locations.len() == before {
        Ok(())
    } else {
        Err(EditValidationError::UsesCount {
            before,
            after: locations.len(),
        })
    }
}

fn walk_uses(content: &str) -> (Vec<UsesLocation>, Option<String>) {
    let body = content.strip_prefix('\u{feff}').unwrap_or(content);
    let source = Source {
        text: content,
        lines: line_spans(content),
        bom: body.len() < content.len(),
    };
    let mut results = Vec::new();
    let mut stack: Vec<Ctx> = Vec::new();

    for event in Parser::new_from_str(body) {
        let (event, span) = match event {
            Ok(parsed) => parsed,
            Err(e) => return (results, Some(e.to_string())),
        };

        match event {
            Event::Scalar(value, style, _, _) => match classify_node(&mut stack) {
                NodeRole::MapKey => {
                    if let Some(Ctx::Map { key, .. }) = stack.last_mut() {
                        *key = Some(value.to_string());
                    }
                }
                NodeRole::MapValue(Some(key)) if key == "uses" => {
                    if let Some(loc) = build_location(&source, value.as_ref(), style, span) {
                        results.push(if names_action(&stack) {
                            loc
                        } else {
                            loc.outside_action_key()
                        });
                    }
                }
                NodeRole::MapValue(_) | NodeRole::Item | NodeRole::Root => {}
            },
            Event::MappingStart(..) => {
                let slot = classify_node(&mut stack).slot();
                stack.push(Ctx::Map {
                    slot,
                    expect_key: true,
                    key: None,
                });
            }
            Event::SequenceStart(..) => {
                let slot = classify_node(&mut stack).slot();
                stack.push(Ctx::Seq { slot });
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

    (results, None)
}

/// A collection context on the structural walk stack.
enum Ctx {
    /// A YAML mapping. `expect_key` toggles between key and value positions;
    /// `key` holds the current scalar key.
    Map {
        slot: Slot,
        expect_key: bool,
        key: Option<String>,
    },
    /// A YAML sequence. Every child is an item; no key/value distinction.
    Seq { slot: Slot },
}

impl Ctx {
    const fn slot(&self) -> &Slot {
        match self {
            Self::Map { slot, .. } | Self::Seq { slot } => slot,
        }
    }
}

/// Where a collection sits in its parent.
enum Slot {
    /// The document root.
    Root,
    /// The value of a scalar key.
    Key(String),
    /// A sequence item, a mapping key, or the value of a non-scalar key.
    Other,
}

/// The structural role of the node currently being visited.
enum NodeRole {
    /// A mapping key.
    MapKey,
    /// A mapping value, with its key when the key was a scalar.
    MapValue(Option<String>),
    /// An item inside a sequence.
    Item,
    /// A top-level node (no enclosing collection).
    Root,
}

impl NodeRole {
    fn slot(self) -> Slot {
        match self {
            Self::Root => Slot::Root,
            Self::MapValue(Some(key)) => Slot::Key(key),
            Self::MapValue(None) | Self::MapKey | Self::Item => Slot::Other,
        }
    }
}

/// Classify the node about to be visited against its enclosing collection and
/// advance that collection's key/value state.
fn classify_node(stack: &mut [Ctx]) -> NodeRole {
    match stack.last_mut() {
        Some(Ctx::Map {
            expect_key, key, ..
        }) => {
            if *expect_key {
                *expect_key = false;
                *key = None;
                NodeRole::MapKey
            } else {
                *expect_key = true;
                NodeRole::MapValue(key.take())
            }
        }
        Some(Ctx::Seq { .. }) => NodeRole::Item,
        None => NodeRole::Root,
    }
}

/// Whether a `uses` key in the innermost mapping of `stack` names an action:
/// `jobs.<id>.uses`, `jobs.<id>.steps[*].uses` or `runs.steps[*].uses`.
fn names_action(stack: &[Ctx]) -> bool {
    let is = |slot: &Slot, name: &str| matches!(slot, Slot::Key(key) if key == name);
    let slots: Vec<&Slot> = stack.iter().map(Ctx::slot).collect();
    match slots.as_slice() {
        [Slot::Root, jobs, Slot::Key(_)] => is(jobs, "jobs"),
        [Slot::Root, jobs, Slot::Key(_), steps, Slot::Other] => {
            is(jobs, "jobs") && is(steps, "steps")
        }
        [Slot::Root, runs, steps, Slot::Other] => is(runs, "runs") && is(steps, "steps"),
        _ => false,
    }
}

/// 1-indexed lines of `content` whose line break falls inside a scalar.
#[must_use]
pub fn lines_ending_in_scalars(content: &str) -> BTreeSet<usize> {
    let body = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = BTreeSet::new();
    for event in Parser::new_from_str(body) {
        match event {
            Ok((Event::Scalar(..), span)) => lines.extend(span.start.line()..span.end.line()),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    lines
}

/// Byte range of each line in `content`, excluding its `\r\n`, `\r` or `\n` break.
#[must_use]
pub fn line_spans(content: &str) -> Vec<Range<usize>> {
    let bytes = content.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while let Some(&byte) = bytes.get(i) {
        let break_len = match byte {
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => 2,
            b'\r' | b'\n' => 1,
            _ => {
                i += 1;
                continue;
            }
        };
        spans.push(start..i);
        i += break_len;
        start = i;
    }
    if start < bytes.len() {
        spans.push(start..bytes.len());
    }
    spans
}

struct Source<'a> {
    text: &'a str,
    lines: Vec<Range<usize>>,
    bom: bool,
}

/// Build a [`UsesLocation`] for a `uses:` value scalar.
///
/// `span.start.col()` is a 0-indexed character column of the text after any
/// byte order mark. For quoted scalars it points at the opening quote while
/// `value` holds the unquoted content.
fn build_location(
    source: &Source<'_>,
    value: &str,
    style: ScalarStyle,
    span: Span,
) -> Option<UsesLocation> {
    if value.is_empty() {
        return None;
    }
    let line = span.start.line(); // 1-indexed
    let raw_line = source
        .text
        .get(source.lines.get(line.checked_sub(1)?)?.clone())?
        .to_string();
    let quote_skip = usize::from(matches!(
        style,
        ScalarStyle::SingleQuoted | ScalarStyle::DoubleQuoted
    ));
    let bom_skip = usize::from(source.bom && line == 1);
    let byte_at = |col: usize| {
        raw_line
            .char_indices()
            .nth(col + bom_skip)
            .map_or(raw_line.len(), |(byte, _)| byte)
    };
    let byte_col = byte_at(span.start.col() + quote_skip);
    let scalar_start = byte_at(span.start.col());
    let span_end = if span.end.line() == line {
        byte_at(span.end.col())
    } else {
        raw_line.len()
    };
    let scalar_range = raw_line.get(scalar_start..span_end).map(|text| RefRange {
        start: scalar_start,
        end: scalar_start + text.trim_end().len(),
    });
    let mut location = UsesLocation::new(line, byte_col + 1, value.to_string(), raw_line);
    location.scalar_range = scalar_range;
    Some(location)
}

/// Byte range of a substring within a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tsify", derive(tsify::Tsify))]
pub struct RefRange {
    /// 0-indexed inclusive start byte offset.
    pub start: usize,
    /// 0-indexed exclusive end byte offset.
    pub end: usize,
}

/// Location of a `uses:` value in a file
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tsify", derive(tsify::Tsify))]
pub struct UsesLocation {
    /// Line number (1-indexed)
    line: usize,
    /// Byte column of the value within `raw_line` (1-indexed)
    column: usize,
    /// The uses value
    value: String,
    /// The full line (for context)
    raw_line: String,
    /// Byte range of the value within the line. Present only if `raw_line`
    /// holds the value verbatim.
    value_range: Option<RefRange>,
    /// Byte range of the ref portion (after `@`) within the line. Present
    /// only if the value contains `@` and `raw_line` holds the value verbatim.
    ref_range: Option<RefRange>,
    /// Byte range of the scalar within the line, quotes included, up to the
    /// line end when the scalar continues on later lines. Present only for
    /// locations that [`extract_uses_from_yaml`] found.
    scalar_range: Option<RefRange>,
    /// Whether GitHub reads the value as an action or reusable workflow:
    /// `jobs.<id>.uses`, `jobs.<id>.steps[*].uses` or `runs.steps[*].uses`.
    #[serde(default = "at_action_key")]
    action_key: bool,
}

const fn at_action_key() -> bool {
    true
}

impl UsesLocation {
    /// Create a new `UsesLocation`
    #[must_use]
    pub fn new(line: usize, column: usize, value: String, raw_line: String) -> Self {
        let value_range = column.checked_sub(1).and_then(|start| {
            let end = start.checked_add(value.len())?;
            (raw_line.get(start..end) == Some(value.as_str())).then_some(RefRange { start, end })
        });
        let ref_range = value_range.and_then(|range| {
            let at_pos = value.find('@')?;
            Some(RefRange {
                start: range.start + at_pos + 1,
                end: range.end,
            })
        });
        Self {
            line,
            column,
            value,
            raw_line,
            value_range,
            ref_range,
            scalar_range: None,
            action_key: true,
        }
    }

    const fn outside_action_key(mut self) -> Self {
        self.action_key = false;
        self
    }

    pub(crate) const fn on_line(mut self, line: usize) -> Self {
        self.line = line;
        self
    }

    /// Sort the value like [`classify_uses`].
    ///
    /// Returns `None` for an invalid value under a `uses` key that GitHub does
    /// not read as an action, such as a step input named `uses`.
    #[must_use]
    pub fn classify(&self) -> Option<Uses> {
        match classify_uses(&self.value) {
            Uses::Invalid { .. } if !self.action_key => None,
            uses => Some(uses),
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
    /// Get the byte range of the uses value in the line, if the line holds it verbatim.
    #[must_use]
    pub fn value_range(&self) -> Option<std::ops::Range<usize>> {
        self.value_range.map(|r| r.start..r.end)
    }

    /// Get just the ref portion range (after @)
    #[must_use]
    pub fn ref_range(&self) -> Option<std::ops::Range<usize>> {
        self.ref_range.map(|r| r.start..r.end)
    }

    /// Get the byte range of the scalar in the line, quotes included.
    #[must_use]
    pub fn scalar_range(&self) -> Option<std::ops::Range<usize>> {
        self.scalar_range.map(|r| r.start..r.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn validate_edit_accepts_a_ref_change() {
        let before = "steps:\n  - uses: a/b@v1\n  - { uses: c/d@v1 }\n";
        let after = "steps:\n  - uses: a/b@v2 # v2\n  - { uses: c/d@v2 }\n";
        assert_eq!(validate_edit(before, after), Ok(()));
    }

    #[test]
    fn validate_edit_rejects_invalid_yaml() {
        let before = "steps:\n  - uses: a/b@v1\n";
        let Err(EditValidationError::Syntax(reason)) =
            validate_edit(before, "steps:\n  - uses: a/b@v2\n  - [\n")
        else {
            panic!("expected a syntax error");
        };
        assert!(reason.contains("line"), "{reason}");
    }

    #[test]
    fn validate_edit_rejects_a_changed_uses_count() {
        let before = "steps:\n  - uses: a/b@v1\n";
        let after = "steps:\n  - uses: a/b@v2\n  - uses: c/d@v2\n";
        assert_eq!(
            validate_edit(before, after),
            Err(EditValidationError::UsesCount {
                before: 1,
                after: 2
            })
        );
        assert_eq!(
            validate_edit(before, after).map_err(|e| e.to_string()),
            Err("`uses:` count changed from 1 to 2".to_string())
        );
        assert_eq!(
            validate_edit(before, "steps: []\n"),
            Err(EditValidationError::UsesCount {
                before: 1,
                after: 0
            })
        );
    }

    #[test]
    fn test_parse_simple() -> TestResult {
        let result = parse_uses("actions/checkout@v4")?.ok_or("`parse_uses(..)?` is None")?;
        assert_eq!(result.owner(), "actions");
        assert_eq!(result.repo(), "checkout");
        assert_eq!(result.ref_str(), "v4");
        assert!(result.host().is_none());
        assert!(result.subpath().is_none());
        Ok(())
    }

    #[test]
    fn empty_path_segments_are_invalid() {
        for input in [
            "o/r/@v1",
            "ghe.corp.com/o/r/@v1",
            "o/r//x@v1",
            "ghe.corp.com/o/r//x@v1",
            "o/r/x/@v1",
        ] {
            let Err(Error::InvalidRef {
                input: named,
                reason,
            }) = parse_uses(input)
            else {
                panic!("expected `{input}` to be invalid");
            };
            assert_eq!(named, input);
            assert_eq!(reason, "path segment `` is empty, `.` or `..`", "{input}");
        }
        let Err(Error::InvalidRef { reason, .. }) = parse_action_path("o/r//x") else {
            panic!("expected `o/r//x` to be invalid");
        };
        assert_eq!(reason, "path segment `` is empty, `.` or `..`");
    }

    #[test]
    fn test_parse_with_subpath() -> TestResult {
        let result = parse_uses("org/repo/.github/workflows/ci.yml@main")?
            .ok_or("`parse_uses(..)?` is None")?;
        assert_eq!(result.owner(), "org");
        assert_eq!(result.repo(), "repo");
        assert_eq!(result.subpath(), Some(".github/workflows/ci.yml"));
        assert_eq!(result.ref_str(), "main");
        Ok(())
    }

    #[test]
    fn test_parse_with_host() -> TestResult {
        let result =
            parse_uses("ghe.corp.com/org/action@v1")?.ok_or("`parse_uses(..)?` is None")?;
        assert_eq!(result.host(), Some("ghe.corp.com"));
        assert_eq!(result.owner(), "org");
        assert_eq!(result.repo(), "action");
        assert_eq!(result.ref_str(), "v1");
        Ok(())
    }

    #[test]
    fn test_parse_sha() -> TestResult {
        let sha = "a".repeat(40);
        let result =
            parse_uses(&format!("actions/checkout@{sha}"))?.ok_or("`parse_uses(..)?` is None")?;
        assert_eq!(result.ref_str(), &sha);
        assert!(result.is_sha());
        Ok(())
    }

    #[test]
    fn test_parse_local() -> TestResult {
        assert!(parse_uses("./local/action")?.is_none());
        assert!(parse_uses("../parent/action")?.is_none());
        Ok(())
    }

    #[test]
    fn test_parse_docker() -> TestResult {
        assert!(parse_uses("docker://alpine:3.8")?.is_none());
        Ok(())
    }

    #[test]
    fn test_parse_invalid() {
        assert!(parse_uses("no-at-sign").is_err());
        assert!(parse_uses("missing@").is_err());
        assert!(parse_uses("@no-path").is_err());
    }

    #[test]
    fn invalid_git_ref_names_are_rejected() {
        for uses in [
            "o/r@../../x/y",
            "o/r@./x",
            "o/r@a/.b",
            "o/r@..",
            "o/r@a//b",
            "o/r@a/",
            "o/r@/a",
            "o/r@v1.lock",
            "o/r@v1.",
            "o/r@a b",
            "o/r@a~1",
            "o/r@a^",
            "o/r@a:b",
            "o/r@a?b",
            "o/r@a*",
            "o/r@a[b",
            "o/r@a\\b",
            "o/r@a@{1}",
            "o/r@@",
            "o/r@a\u{7}",
            "o/r/../x@v1",
            "o/r/./x@v1",
            "o/r//x@v1",
        ] {
            assert!(
                matches!(parse_uses(uses), Err(Error::InvalidRef { .. })),
                "{uses}"
            );
        }
    }

    #[test]
    fn valid_git_ref_names_parse() {
        for uses in [
            "o/r@v1",
            "o/r@release/v1",
            "o/r@feat#1",
            "o/r@v1.0.0+build",
            "o/r@a.b",
            "o/r@100%",
            "o/r/.github/workflows/ci.yml@main",
        ] {
            assert!(matches!(parse_uses(uses), Ok(Some(_))), "{uses}");
        }
    }

    #[test]
    fn host_needs_three_segments_and_a_dns_shaped_first_label()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        type Parts<'a> = (Option<&'a str>, &'a str, &'a str, Option<&'a str>);
        let cases: [(&str, Parts<'_>); 12] = [
            (
                "my.org/repo/.github/workflows/ci.yml@main",
                (Some("my.org"), "repo", ".github", Some("workflows/ci.yml")),
            ),
            ("ghe.corp/o/r@v1", (Some("ghe.corp"), "o", "r", None)),
            ("localhost/o/r@v1", (Some("localhost"), "o", "r", None)),
            (
                "localhost:3000/o/r@v1",
                (Some("localhost:3000"), "o", "r", None),
            ),
            ("host:8443/o/r@v1", (Some("host:8443"), "o", "r", None)),
            ("[::1]/o/r@v1", (Some("[::1]"), "o", "r", None)),
            ("[::1]:8080/o/r@v1", (Some("[::1]:8080"), "o", "r", None)),
            (
                "GitHub.COM/Actions/Checkout@v4",
                (Some("github.com"), "Actions", "Checkout", None),
            ),
            ("a.b/c@v1", (None, "a.b", "c", None)),
            ("ghe.corp.com/org@v1", (None, "ghe.corp.com", "org", None)),
            ("ghe/o/r@v1", (None, "ghe", "o", Some("r"))),
            ("a..b/o/r@v1", (None, "a..b", "o", Some("r"))),
        ];
        for (uses, want) in cases {
            let Some(action) = parse_uses(uses)? else {
                return Err(format!("{uses} parsed as local").into());
            };
            let got = (
                action.host(),
                action.owner(),
                action.repo(),
                action.subpath(),
            );
            assert_eq!(got, want, "{uses}");
        }
        Ok(())
    }

    #[test]
    fn rejects_owner_or_repo_outside_the_name_charset() {
        for uses in [
            "own er/repo@v1",
            "owner/re$po@v1",
            "ghe.corp/o!/r@v1",
            "ghe.corp/o/r+x@v1",
            "o/..@v1",
            "owner/.@v1",
            "/repo@v1",
            "owner/@v1",
            "host:99999/o/r@v1",
            "-bad-.com:8/o/r@v1",
            "[nope]/o/r@v1",
        ] {
            let result = parse_uses(uses);
            assert!(
                matches!(result, Err(Error::InvalidRef { .. })),
                "{uses}: {result:?}"
            );
        }
    }

    #[test]
    fn classify_reports_invalid_values_only_at_action_keys() {
        let reported = |yaml: &str| -> Vec<(usize, bool)> {
            extract_uses_from_yaml(yaml)
                .iter()
                .map(|loc| (loc.line(), loc.classify().is_some()))
                .collect()
        };
        let workflow = "jobs:\n  call:\n    uses: o/r@\n  b:\n    env: { uses: x }\n    steps:\n      - uses: o/r@\n        with:\n          uses: lint\n      - { uses: o/r@ }\n";
        assert_eq!(
            reported(workflow),
            [(3, true), (5, false), (7, true), (9, false), (10, true)]
        );
        let composite =
            "runs:\n  using: composite\n  steps:\n    - uses: o/r@\n      with: { uses: lint }\n";
        assert_eq!(reported(composite), [(4, true), (5, false)]);
        let stray = "steps:\n  - uses: o/r@\nother:\n  b:\n    uses: o/r@\n";
        assert_eq!(reported(stray), [(2, false), (5, false)]);
        let valid = "env:\n  uses: o/r@v1\n";
        assert!(matches!(
            extract_uses_from_yaml(valid)[0].classify(),
            Some(Uses::Action(_))
        ));
    }

    #[test]
    fn classify_uses_keeps_the_parts_of_an_invalid_ref() {
        assert!(matches!(classify_uses("./local"), Uses::Local));
        assert!(matches!(
            classify_uses("o/r@v1"),
            Uses::Action(action) if action == ActionRef::simple("o", "r", "v1")
        ));
        let cases = [
            ("o/r@a~1", ActionRef::simple("o", "r", "a~1")),
            ("o/r@v1.lock", ActionRef::simple("o", "r", "v1.lock")),
            ("o/r", ActionRef::simple("o", "r", "")),
            ("o/r@", ActionRef::simple("o", "r", "")),
            ("o", ActionRef::simple("o", "", "")),
            (
                "GHE.corp/o/r/sub/x@../y",
                ActionRef::new(
                    Some("ghe.corp".into()),
                    "o",
                    "r",
                    Some("sub/x".into()),
                    "../y",
                ),
            ),
        ];
        for (uses, expected) in cases {
            let classified = classify_uses(uses);
            assert!(
                matches!(
                    &classified,
                    Uses::Invalid { action, error: Error::InvalidRef { input, .. } }
                        if *action == expected && input == uses
                ),
                "{uses}: {classified:?}"
            );
        }
    }

    #[test]
    fn parse_action_path_applies_the_uses_host_rule()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            parse_action_path("GHE.corp/o/r/sub")?,
            ActionRepoId::new(Some("ghe.corp".into()), "o", "r", Some("sub".into()))
        );
        assert_eq!(
            parse_action_path("a.b/c")?,
            ActionRepoId::new(None, "a.b", "c", None)
        );
        let result = parse_action_path("o");
        assert!(
            matches!(&result, Err(Error::InvalidRef { input, .. }) if input == "o"),
            "{result:?}"
        );
        Ok(())
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
    fn test_extract_quoted_ref_range_is_correct() -> TestResult {
        // Quoted value: ref_range must land on the ref content, skipping the quote.
        let yaml = "steps:\n  - uses: \"actions/checkout@v4\"\n";
        let locations = extract_uses_from_yaml(yaml);
        assert_eq!(locations.len(), 1);
        let loc = &locations[0];
        let range = loc.ref_range().ok_or("`loc.ref_range()` is None")?;
        assert_eq!(&loc.raw_line[range], "v4");
        Ok(())
    }

    fn assert_exact_ranges(yaml: &str, expected: &[(usize, &str, &str)]) {
        let locations = extract_uses_from_yaml(yaml);
        let found: Vec<(usize, &str, &str, &str)> = locations
            .iter()
            .map(|loc| {
                let raw = loc.raw_line();
                let value_text = loc
                    .value_range()
                    .and_then(|r| raw.get(r))
                    .unwrap_or("<no value range>");
                let ref_text = loc
                    .ref_range()
                    .and_then(|r| raw.get(r))
                    .unwrap_or("<no ref>");
                (loc.line(), loc.value(), value_text, ref_text)
            })
            .collect();
        let wanted: Vec<(usize, &str, &str, &str)> = expected
            .iter()
            .map(|&(line, value, git_ref)| (line, value, value, git_ref))
            .collect();
        assert_eq!(found, wanted, "yaml:\n{yaml}");
    }

    #[test]
    fn test_flow_mapping_with_nested_mapping_excludes_comma() {
        assert_exact_ranges(
            "steps:\n  - { uses: actions/upload-pages-artifact@v4, with: { path: dist } }\n",
            &[(2, "actions/upload-pages-artifact@v4", "v4")],
        );
    }

    #[test]
    fn test_flow_mapping_last_entry_excludes_brace() {
        assert_exact_ranges(
            "steps:\n  - { uses: actions/deploy-pages@v4 }\n  - {uses: a/b@v1}\n",
            &[(2, "actions/deploy-pages@v4", "v4"), (3, "a/b@v1", "v1")],
        );
    }

    #[test]
    fn test_flow_mapping_before_other_key_excludes_comma() {
        assert_exact_ranges(
            "steps:\n  - { uses: actions/deploy-pages@v4, id: deployment }\n  - {uses: a/b@v1,id: x}\n",
            &[(2, "actions/deploy-pages@v4", "v4"), (3, "a/b@v1", "v1")],
        );
    }

    #[test]
    fn test_flow_sequence_of_flow_mappings() {
        assert_exact_ranges(
            "steps: [{ uses: a/b@v4 }, { uses: c/d@v1 }]\n",
            &[(1, "a/b@v4", "v4"), (1, "c/d@v1", "v1")],
        );
    }

    #[test]
    fn test_flow_sequence_without_spaces_excludes_bracket() {
        assert_exact_ranges(
            "steps: [{uses: a/b@v4},{uses: c/d@v1}]\n",
            &[(1, "a/b@v4", "v4"), (1, "c/d@v1", "v1")],
        );
    }

    #[test]
    fn test_flow_mapping_quoted_values() {
        assert_exact_ranges(
            "steps:\n  - { uses: \"a/b@v4\", with: { x: 1 } }\n  - { uses: 'c/d@v1' }\n  - [{ \"uses\": \"e/f@v2\" }]\n",
            &[
                (2, "a/b@v4", "v4"),
                (3, "c/d@v1", "v1"),
                (4, "e/f@v2", "v2"),
            ],
        );
    }

    #[test]
    fn test_flow_mapping_trailing_comments() {
        assert_exact_ranges(
            "steps:\n  - { uses: a/b@v4, with: { x: 1 } } # pinned\n  - {\n      uses: c/d@v1, # note\n      id: y\n    }\n  - uses: e/f@v2 # block\n",
            &[
                (2, "a/b@v4", "v4"),
                (4, "c/d@v1", "v1"),
                (7, "e/f@v2", "v2"),
            ],
        );
    }

    #[test]
    fn test_flow_mapping_after_multibyte_text() {
        assert_exact_ranges(
            "steps:\n  - { name: \"déploiement ✓\", uses: a/b@v4 }\n",
            &[(2, "a/b@v4", "v4")],
        );
    }

    #[test]
    fn test_flow_sequence_entry_ends_at_bracket() {
        assert_exact_ranges(
            "steps: [uses: a/b@v4]\njobs: [uses: c/d@v1, uses: e/f@v2]\n",
            &[
                (1, "a/b@v4", "v4"),
                (2, "c/d@v1", "v1"),
                (2, "e/f@v2", "v2"),
            ],
        );
    }

    #[test]
    fn line_spans_end_lines_at_crlf_cr_and_lf() {
        let cases: [(&str, &[&str]); 11] = [
            ("", &[]),
            ("a", &["a"]),
            ("a\n", &["a"]),
            ("a\r\nb\r\n", &["a", "b"]),
            ("a\n\nb", &["a", "", "b"]),
            ("a\rb\n", &["a", "b"]),
            ("\r\n\r\n", &["", ""]),
            ("x\r", &["x"]),
            ("x\n\r", &["x", ""]),
            ("x\r\r\ny", &["x", "", "y"]),
            ("\u{feff}é\r✓\r\n", &["\u{feff}é", "✓"]),
        ];
        for (content, lines) in cases {
            let spans: Vec<&str> = line_spans(content)
                .into_iter()
                .map(|span| &content[span])
                .collect();
            assert_eq!(spans, lines, "{content:?}");
        }
    }

    #[test]
    fn test_ranges_on_crlf_lone_cr_and_bom_lines() {
        let yaml = "\u{feff}uses: a/b@v1\r\nx:\r  - uses: c/d@v2\r\n  - uses: \"e/f@v3\" # c\ry: [{ uses: g/h@v4 }]";
        assert_exact_ranges(
            yaml,
            &[
                (1, "a/b@v1", "v1"),
                (3, "c/d@v2", "v2"),
                (4, "e/f@v3", "v3"),
                (5, "g/h@v4", "v4"),
            ],
        );
        let spans = line_spans(yaml);
        let refs: Vec<&str> = extract_uses_from_yaml(yaml)
            .iter()
            .filter_map(|loc| {
                let line = spans.get(loc.line() - 1)?;
                assert_eq!(&yaml[line.clone()], loc.raw_line());
                let range = loc.ref_range()?;
                yaml.get(line.start + range.start..line.start + range.end)
            })
            .collect();
        assert_eq!(refs, ["v1", "v2", "v3", "v4"]);
    }

    #[test]
    fn uses_location_serializes_its_value_range()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let verbatim = UsesLocation::new(1, 9, "a/b@v1".into(), "  uses: a/b@v1".into());
        let json = serde_json::to_value(&verbatim)?;
        assert_eq!(
            json["value_range"],
            serde_json::json!({ "start": 8, "end": 14 })
        );
        assert_eq!(
            json["ref_range"],
            serde_json::json!({ "start": 12, "end": 14 })
        );
        assert_eq!(json["action_key"], serde_json::json!(true));
        assert_eq!(serde_json::from_value::<UsesLocation>(json)?, verbatim);

        let [input] = extract_uses_from_yaml("env:\n  uses: x\n")
            .try_into()
            .map_err(|_| "one value")?;
        let json = serde_json::to_value(&input)?;
        assert_eq!(json["action_key"], serde_json::json!(false));
        assert_eq!(serde_json::from_value::<UsesLocation>(json)?, input);
        let mut legacy = serde_json::to_value(&verbatim)?;
        if let Some(fields) = legacy.as_object_mut() {
            fields.remove("action_key");
        }
        assert_eq!(serde_json::from_value::<UsesLocation>(legacy)?, verbatim);

        let escaped = UsesLocation::new(1, 10, "a/b@v10".into(), r#"  uses: "a/b@v1\x30""#.into());
        let json = serde_json::to_value(&escaped)?;
        assert_eq!(json["value_range"], serde_json::Value::Null);
        assert_eq!(json["ref_range"], serde_json::Value::Null);
        assert_eq!(escaped.value_range(), None);
        Ok(())
    }

    #[test]
    fn is_host_segment_matches_host_detection() {
        for host in [
            "github.com",
            "GitHub.COM",
            "localhost",
            "localhost:8080",
            "[::1]",
            "[::1]:3000",
        ] {
            assert!(is_host_segment(host), "{host}");
            let parsed = parse_action_path(&format!("{host}/o/r")).ok();
            assert_eq!(
                parsed.as_ref().and_then(ActionRepoId::host),
                Some(host.to_ascii_lowercase()).as_deref()
            );
        }
        for segment in [
            "actions",
            "my_org",
            "-bad.host",
            "a..b",
            "host:99999",
            "[nope]",
        ] {
            assert!(!is_host_segment(segment), "{segment}");
        }
    }

    #[test]
    fn test_bom_before_a_flow_mapping() {
        assert_exact_ranges("\u{feff}{ uses: a/b@v1 }\r\n", &[(1, "a/b@v1", "v1")]);
    }

    #[test]
    fn lines_ending_in_scalars_lists_lines_a_scalar_continues_past() {
        let cases: [(&str, &[usize]); 6] = [
            ("s:\n  - uses: a/b@v1\n", &[]),
            (
                "s:\n  - { uses: a/b@v1, name: \"multi\n    line\" }\n",
                &[2],
            ),
            ("s:\n  - { uses: a/b@v1, name: multi\n    word }\n", &[2]),
            ("s: [{ uses: a/b@v1 }, 'x\n  y\n  z']\n", &[1, 2]),
            ("\u{feff}a: \"b\r\n  c\"\r\nd: e\r\n", &[1]),
            ("s:\n  - { uses: a/b@v1, # c\n      id: x }\n", &[]),
        ];
        for (yaml, lines) in cases {
            let found: Vec<usize> = lines_ending_in_scalars(yaml).into_iter().collect();
            assert_eq!(found, lines, "{yaml:?}");
        }
    }

    #[test]
    fn test_no_ranges_when_line_differs_from_value() {
        for (yaml, value) in [
            (
                "steps:\n  - uses: \"actions/checkout@v\\x34\"\n",
                "actions/checkout@v4",
            ),
            ("steps:\n  - uses: 'a/b@v''4'\n", "a/b@v'4"),
            ("steps:\n  - uses: |\n      a/b@v4\n", "a/b@v4\n"),
            ("steps:\n  - uses: a/b@v4\n      more\n", "a/b@v4 more"),
            ("steps:\n  - uses: \"a/b@\\\n      v4\"\n", "a/b@v4"),
        ] {
            let locations = extract_uses_from_yaml(yaml);
            let [loc] = locations.as_slice() else {
                panic!("expected one location in {yaml:?}: {locations:?}");
            };
            assert_eq!(loc.value(), value, "{yaml:?}");
            assert_eq!(loc.value_range(), None, "{yaml:?}");
            assert_eq!(loc.ref_range(), None, "{yaml:?}");
        }
    }

    #[test]
    fn scalar_range_covers_the_scalar_and_its_quotes() {
        let cases: [(&str, &str); 8] = [
            ("steps:\n  - uses: a/b@v4\n", "a/b@v4"),
            ("steps:\n  - uses: a/b@v4 # pin\n", "a/b@v4"),
            ("steps:\n  - uses: \"a/b@v4\"\n", "\"a/b@v4\""),
            ("steps:\n  - uses: 'a/b@v''4'\n", "'a/b@v''4'"),
            ("\u{feff}{ uses: \"é/b@v\\x34\" }\n", "\"é/b@v\\x34\""),
            ("steps:\n  - uses: a/b@v4\n      more\n", "a/b@v4"),
            ("steps:\n  - uses: \"a/b@\\\n      v4\"\n", "\"a/b@\\"),
            ("steps:\n  - { uses: a/b@v4, id: x }\n", "a/b@v4"),
        ];
        for (yaml, scalar) in cases {
            let locations = extract_uses_from_yaml(yaml);
            let Some(loc) = locations.first() else {
                panic!("no location in {yaml:?}");
            };
            let text = loc.scalar_range().and_then(|r| loc.raw_line().get(r));
            assert_eq!(text, Some(scalar), "{yaml:?}");
        }
        let locations =
            extract_uses_from_yaml("steps: [{ uses: \"a/b@v\\x34\" }, { uses: \"c/d@v\\x31\" }]\n");
        let scalars: Vec<Option<&str>> = locations
            .iter()
            .map(|loc| loc.scalar_range().and_then(|r| loc.raw_line().get(r)))
            .collect();
        assert_eq!(scalars, [Some("\"a/b@v\\x34\""), Some("\"c/d@v\\x31\"")]);
        let built = UsesLocation::new(1, 9, "a/b@v1".into(), "  uses: a/b@v1".into());
        assert_eq!(built.scalar_range(), None);
    }

    #[test]
    fn test_no_ranges_when_column_is_outside_the_line() {
        let raw = "  - uses: a/b@v4".to_string();
        for column in [0, 12, 40] {
            let loc = UsesLocation::new(1, column, "a/b@v4".to_string(), raw.clone());
            assert_eq!(loc.value_range(), None, "column {column}");
            assert_eq!(loc.ref_range(), None, "column {column}");
        }
        let loc = UsesLocation::new(1, 11, "a/b@v4".to_string(), raw);
        assert_eq!(loc.value_range(), Some(10..16));
        assert_eq!(loc.ref_range(), Some(14..16));
    }

    #[test]
    fn test_uses_location_serde_round_trip() -> TestResult {
        let loc = UsesLocation::new(
            10,
            15,
            "actions/checkout@v4".to_string(),
            "      uses: actions/checkout@v4".to_string(),
        );
        let json = serde_json::to_string(&loc)?;
        let deserialized: UsesLocation = serde_json::from_str(&json)?;
        assert_eq!(loc, deserialized);
        Ok(())
    }
}
