//! Line-relative edits to workflow text, planned onto the original bytes.

use std::{
    collections::{BTreeSet, HashMap},
    ops::Range,
    path::PathBuf,
};

use serde::Serialize;

use crate::{
    SourceFormat, UsesLocation, line_spans,
    policy::Style,
    rewrite::{Trailing, labeled_comment, note_comment},
    version,
};

/// The kind of edit to apply.
#[derive(Debug, Clone)]
pub enum EditKind {
    /// Replace exact byte range with new content.
    ReplaceRange {
        /// Byte range within the line to replace.
        range: Range<usize>,
        /// New content to insert.
        new: String,
    },
}

/// A pending edit to a specific line.
#[derive(Debug, Clone)]
pub struct PendingEdit {
    /// Line number (1-indexed).
    pub line: usize,
    /// The edit to apply.
    pub kind: EditKind,
    /// Optional comment to append (for pin command).
    pub comment: Option<String>,
    /// Version comment that `new` writes in place of an old one.
    pub inline_comment: Option<String>,
}

impl PendingEdit {
    /// Create an edit that replaces a byte range.
    #[must_use]
    pub const fn replace_range(line: usize, range: Range<usize>, new: String) -> Self {
        Self {
            line,
            kind: EditKind::ReplaceRange { range, new },
            comment: None,
            inline_comment: None,
        }
    }

    /// Add a comment to append after the edit.
    #[must_use]
    pub fn with_comment(mut self, comment: String) -> Self {
        self.comment = Some(comment);
        self
    }

    /// Record the version comment that the replacement text ends with.
    #[must_use]
    pub fn with_inline_comment(mut self, comment: String) -> Self {
        self.inline_comment = Some(comment);
        self
    }
}

fn retag_edit(loc: &UsesLocation, tag: &str) -> Option<PendingEdit> {
    let range = loc.ref_range()?;
    let raw = loc.raw_line();
    Some(match Trailing::after(raw, range.end) {
        Trailing::Version { note } => {
            let comment = labeled_comment(tag, note);
            PendingEdit::replace_range(
                loc.line(),
                range.start..raw.len(),
                format!("{tag}{comment}"),
            )
            .with_inline_comment(comment)
        }
        Trailing::None | Trailing::Foreign => {
            PendingEdit::replace_range(loc.line(), range, tag.to_string())
        }
    })
}

/// Build the edit that moves the ref at `loc` to `target` under the `style` pin style.
///
/// A version comment after the ref names the new version and keeps its
/// note. A SHA pin rewritten to a tag (`pin = "tag"`) drops the version and
/// keeps the note. Returns `None` when the line does not hold the value
/// verbatim.
#[must_use]
pub fn retarget_edit(
    loc: &UsesLocation,
    current_is_sha: bool,
    target: &version::Info,
    style: Style,
) -> Option<PendingEdit> {
    let tag = target.version().to_string();
    if style.writes_sha(current_is_sha) {
        repin_sha_edit(loc, target)
    } else if current_is_sha {
        unpin_edit(loc, &tag)
    } else {
        retag_edit(loc, &tag)
    }
}

/// Build an edit that pins a ref to the `target` commit SHA.
///
/// Keeps a `# vX.Y.Z` version comment accurate: a stale version is
/// refreshed and the note after it kept, a missing comment is added, and a
/// foreign comment is left untouched. Shared by `update` and `pin`.
#[must_use]
pub fn repin_sha_edit(loc: &UsesLocation, target: &version::Info) -> Option<PendingEdit> {
    let new_sha = target.sha();
    let version = target.version().to_string();
    let range = loc.ref_range()?;

    let raw = loc.raw_line();
    Some(match Trailing::after(raw, range.end) {
        Trailing::Version { note } => {
            let comment = labeled_comment(&version, note);
            PendingEdit::replace_range(
                loc.line(),
                range.start..raw.len(),
                format!("{new_sha}{comment}"),
            )
            .with_inline_comment(comment)
        }
        // `with_comment` adds the version note only when the line has none, so a
        // foreign comment is preserved.
        Trailing::None | Trailing::Foreign => {
            PendingEdit::replace_range(loc.line(), range, new_sha.to_string())
                .with_comment(labeled_comment(&version, ""))
        }
    })
}

/// Build an edit that writes the tag `tag` over a SHA pin.
///
/// A trailing version comment loses its version and keeps its note.
#[must_use]
pub fn unpin_edit(loc: &UsesLocation, tag: &str) -> Option<PendingEdit> {
    let range = loc.ref_range()?;
    let raw = loc.raw_line();
    Some(match Trailing::after(raw, range.end) {
        Trailing::Version { note } => PendingEdit::replace_range(
            loc.line(),
            range.start..raw.len(),
            format!("{tag}{}", note_comment(note).unwrap_or_default()),
        ),
        Trailing::None | Trailing::Foreign => {
            PendingEdit::replace_range(loc.line(), range, tag.to_string())
        }
    })
}

/// Why a planned edit leaves an action reference unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The `uses:` value is escaped or spans lines and has no in-place range.
    Unplaceable,
    /// Two edits of the file overlap, and the writer leaves every edit of the file out.
    Overlap,
    /// The edit's range does not fit its line in the source text.
    OutOfBounds,
}

impl SkipReason {
    /// The writer limit as a short phrase.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Unplaceable => "`uses:` value is escaped or spans lines",
            Self::Overlap => "edits in this file overlap",
            Self::OutOfBounds => "edit does not fit its line",
        }
    }
}

/// What the writer does with one edit of a [`FileEdits`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOutcome {
    /// The edit is written.
    Applied {
        /// The version comment the edit writes, appended to its line or in
        /// place of an old one.
        comment: Option<String>,
    },
    /// The edit is left out.
    Skipped(SkipReason),
}

/// Accumulated edits for a single file, each carrying a caller label `L`.
///
/// Edit ranges are line-relative; [`FileEdits::plan`] places them on the
/// original text by byte offset and applies them right to left.
pub struct FileEdits<L> {
    /// Absolute path to the file.
    pub path: PathBuf,
    /// Original file content.
    content: String,
    format: SourceFormat,
    uses_per_line: HashMap<usize, usize>,
    commentable_lines: BTreeSet<usize>,
    edits: Vec<(PendingEdit, L)>,
}

/// The planned result of a [`FileEdits`].
pub struct EditPlan<L> {
    /// Absolute path to the file.
    pub path: PathBuf,
    /// Each label with its outcome, in insertion order.
    pub entries: Vec<(L, EditOutcome)>,
    /// The applied edits, ready to place on the file as it is at write time.
    pub splices: Splices,
}

/// The byte splices of an [`EditPlan`] with the scanned lines they rewrite.
pub struct Splices {
    format: SourceFormat,
    scanned: String,
    edits: Vec<(Range<usize>, String)>,
    lines: Vec<(Range<usize>, bool)>,
}

impl Splices {
    /// Apply the splices to `current`, or `None` when a rewritten line differs from the scan.
    #[must_use]
    pub fn apply(&self, current: &str) -> Option<String> {
        let intact = self.lines.iter().all(|(line, unterminated)| {
            current.get(line.clone()) == self.scanned.get(line.clone())
                && (!unterminated || current.len() == line.end)
        });
        intact.then(|| splice(current, &self.edits))
    }

    /// The format the scanned text is read as.
    #[must_use]
    pub const fn format(&self) -> SourceFormat {
        self.format
    }

    /// The text the plan was made from.
    #[must_use]
    pub fn scanned(&self) -> &str {
        &self.scanned
    }

    /// The scanned text with every splice applied.
    #[must_use]
    pub fn planned(&self) -> String {
        splice(&self.scanned, &self.edits)
    }
}

impl<L> EditPlan<L> {
    /// The scanned text with every applied edit.
    #[must_use]
    pub fn content(&self) -> String {
        self.splices.planned()
    }
}

fn splice(text: &str, edits: &[(Range<usize>, String)]) -> String {
    let mut content = text.to_string();
    for (range, new) in edits.iter().rev() {
        content.replace_range(range.clone(), new);
    }
    content
}

impl<L> FileEdits<L> {
    /// Create a new `FileEdits` for a file whose refs are read as `format`.
    #[must_use]
    pub fn new(path: PathBuf, content: String, format: SourceFormat) -> Self {
        let mut uses_per_line = HashMap::new();
        for loc in format.extract_uses(&content) {
            *uses_per_line.entry(loc.line()).or_insert(0) += 1;
        }
        Self {
            path,
            format,
            commentable_lines: format.commentable_lines(&content),
            content,
            uses_per_line,
            edits: Vec::new(),
        }
    }

    /// Return `loc` with its line cut after the value when that line takes no comment.
    ///
    /// An edit built from the result leaves the text after the ref alone on a
    /// line outside [`SourceFormat::commentable_lines`].
    #[must_use]
    pub fn writable(&self, loc: &UsesLocation) -> UsesLocation {
        let head = loc
            .value_range()
            .filter(|_| !self.commentable_lines.contains(&loc.line()))
            .and_then(|range| loc.raw_line().get(..range.end));
        head.map_or_else(
            || loc.clone(),
            |head| {
                UsesLocation::new(
                    loc.line(),
                    loc.column(),
                    loc.value().to_owned(),
                    head.to_owned(),
                )
            },
        )
    }

    /// Add an edit with its label.
    pub fn add(&mut self, edit: PendingEdit, label: L) {
        self.edits.push((edit, label));
    }

    /// Place every edit on the original text and build the new content.
    ///
    /// When any two placed edits overlap, every placed edit is skipped and
    /// the text stays unchanged; edits that share a line without overlapping
    /// all apply. A
    /// comment is appended only when its line holds a single `uses:` value, no
    /// `#`, no other applied edit with a comment, and is in
    /// [`SourceFormat::commentable_lines`].
    #[must_use]
    pub fn plan(self) -> EditPlan<L> {
        let spans = line_spans(&self.content);
        let mut outcomes = vec![EditOutcome::Skipped(SkipReason::OutOfBounds); self.edits.len()];

        let mut placed: Vec<(usize, Range<usize>)> = self
            .edits
            .iter()
            .enumerate()
            .filter_map(|(i, (edit, _))| Some((i, absolute_range(&self.content, &spans, edit)?)))
            .collect();
        placed.sort_by_key(|(i, range)| (range.start, range.end, *i));

        let overlaps = placed.windows(2).any(|pair| {
            let (prev, range) = (&pair[0].1, &pair[1].1);
            let starts_inside = range.start < prev.end;
            let shares_start = range.start == prev.start;
            starts_inside || shares_start
        });
        let mut splices: Vec<(Range<usize>, bool, &str)> = Vec::new();
        let mut applied: Vec<usize> = Vec::new();
        for (i, range) in placed {
            if overlaps {
                outcomes[i] = EditOutcome::Skipped(SkipReason::Overlap);
                continue;
            }
            let EditKind::ReplaceRange { new, .. } = &self.edits[i].0.kind;
            splices.push((range, false, new));
            outcomes[i] = EditOutcome::Applied {
                comment: self.edits[i].0.inline_comment.clone(),
            };
            applied.push(i);
        }

        let mut commented_per_line: HashMap<usize, usize> = HashMap::new();
        for &i in &applied {
            if self.edits[i].0.comment.is_some() {
                *commented_per_line.entry(self.edits[i].0.line).or_insert(0) += 1;
            }
        }
        for &i in &applied {
            let edit = &self.edits[i].0;
            let Some(comment) = &edit.comment else {
                continue;
            };
            let Some(span) = edit.line.checked_sub(1).and_then(|idx| spans.get(idx)) else {
                continue;
            };
            let sole_uses = self.uses_per_line.get(&edit.line).copied().unwrap_or(0) <= 1;
            let sole_comment = commented_per_line.get(&edit.line) == Some(&1);
            let uncommented = !self.content[span.clone()].contains('#');
            let commentable = self.commentable_lines.contains(&edit.line);
            if sole_uses && sole_comment && uncommented && commentable {
                splices.push((span.end..span.end, true, comment));
                outcomes[i] = EditOutcome::Applied {
                    comment: Some(comment.clone()),
                };
            }
        }

        splices.sort_by_key(|(range, is_comment, _)| (range.start, range.end, *is_comment));
        let edits: Vec<(Range<usize>, String)> = splices
            .into_iter()
            .map(|(range, _, text)| (range, text.to_string()))
            .collect();
        let edited_lines: BTreeSet<usize> = applied
            .iter()
            .filter_map(|&i| self.edits[i].0.line.checked_sub(1))
            .collect();
        let lines = edited_lines
            .into_iter()
            .filter_map(|idx| {
                let span = spans.get(idx)?;
                let end = spans
                    .get(idx + 1)
                    .map_or(self.content.len(), |next| next.start);
                Some((span.start..end, span.end == self.content.len()))
            })
            .collect();
        EditPlan {
            path: self.path,
            entries: self
                .edits
                .into_iter()
                .zip(outcomes)
                .map(|((_, label), outcome)| (label, outcome))
                .collect(),
            splices: Splices {
                format: self.format,
                scanned: self.content,
                edits,
                lines,
            },
        }
    }
}

fn absolute_range(
    content: &str,
    spans: &[Range<usize>],
    edit: &PendingEdit,
) -> Option<Range<usize>> {
    let span = spans.get(edit.line.checked_sub(1)?)?;
    let EditKind::ReplaceRange { range, .. } = &edit.kind;
    if range.start > range.end || range.end > span.len() {
        return None;
    }
    let absolute = span.start + range.start..span.start + range.end;
    (content.is_char_boundary(absolute.start) && content.is_char_boundary(absolute.end))
        .then_some(absolute)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_line(line: &str, edit: PendingEdit) -> Option<String> {
        let mut fe = FileEdits::new(PathBuf::from("/test"), line.to_string(), SourceFormat::Yaml);
        fe.add(edit, ());
        let plan = fe.plan();
        match plan.entries.as_slice() {
            [((), EditOutcome::Applied { .. })] => Some(plan.content()),
            _ => None,
        }
    }

    #[test]
    fn test_pending_edit_replace_range() -> Result<(), Box<dyn std::error::Error>> {
        let edit = PendingEdit::replace_range(1, 10..14, "v2.0.0".to_string());
        let line = "  - uses: actions/checkout@v1.0.0";
        let result = apply_line(line, edit).ok_or("edit did not apply")?;
        assert_eq!(result, "  - uses: v2.0.0ons/checkout@v1.0.0");
        Ok(())
    }

    #[test]
    fn test_pending_edit_replace_range_exact() -> Result<(), Box<dyn std::error::Error>> {
        // Replace just the ref portion
        let line = "  - uses: actions/checkout@v1";
        let edit = PendingEdit::replace_range(1, 27..29, "v2".to_string());
        let result = apply_line(line, edit).ok_or("edit did not apply")?;
        assert_eq!(result, "  - uses: actions/checkout@v2");
        Ok(())
    }

    #[test]
    fn test_pending_edit_replace_range_out_of_bounds() {
        let edit = PendingEdit::replace_range(1, 100..110, "new".to_string());
        let line = "short";
        assert!(apply_line(line, edit).is_none());
    }

    #[test]
    fn test_pending_edit_with_comment() -> Result<(), Box<dyn std::error::Error>> {
        let edit = PendingEdit::replace_range(1, 27..29, "abc123".to_string())
            .with_comment(" # v1".to_string());
        let line = "  - uses: actions/checkout@v1";
        let result = apply_line(line, edit).ok_or("edit did not apply")?;
        assert_eq!(result, "  - uses: actions/checkout@abc123 # v1");
        Ok(())
    }

    #[test]
    fn test_pending_edit_comment_not_added_if_exists() -> Result<(), Box<dyn std::error::Error>> {
        let edit = PendingEdit::replace_range(1, 27..29, "v2".to_string())
            .with_comment(" # pinned".to_string());
        let line = "  - uses: actions/checkout@v1 # existing comment";
        let result = apply_line(line, edit).ok_or("edit did not apply")?;
        // Comment not added because line already has one
        assert_eq!(result, "  - uses: actions/checkout@v2 # existing comment");
        Ok(())
    }

    // repin_sha_edit tests

    fn sha40(byte: char) -> String {
        byte.to_string().repeat(40)
    }

    fn sha_loc(line_body: &str) -> Result<UsesLocation, Box<dyn std::error::Error>> {
        let value = line_body
            .split("uses: ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .ok_or("line has no `uses:` value")?;
        let column = line_body.find(value).ok_or("value not in line")? + 1;
        Ok(UsesLocation::new(
            1,
            column,
            value.to_string(),
            line_body.to_string(),
        ))
    }

    #[test]
    fn test_repin_sha_adds_version_comment() -> Result<(), Box<dyn std::error::Error>> {
        let old = sha40('a');
        let new = sha40('b');
        let line = format!("      - uses: actions/checkout@{old}");
        let loc = sha_loc(&line)?;
        let target = version::Info::new(version::Version::parse("v7.0.1"), &new);
        let result = apply_line(&line, repin_sha_edit(&loc, &target).ok_or("no edit")?)
            .ok_or("edit did not apply")?;
        assert_eq!(
            result,
            format!("      - uses: actions/checkout@{new} # v7.0.1")
        );
        Ok(())
    }

    #[test]
    fn test_repin_sha_refreshes_stale_comment() -> Result<(), Box<dyn std::error::Error>> {
        let old = sha40('a');
        let new = sha40('b');
        let line = format!("      - uses: actions/checkout@{old} # v7.0.0");
        let loc = sha_loc(&line)?;
        let target = version::Info::new(version::Version::parse("v7.0.1"), &new);
        let result = apply_line(&line, repin_sha_edit(&loc, &target).ok_or("no edit")?)
            .ok_or("edit did not apply")?;
        assert_eq!(
            result,
            format!("      - uses: actions/checkout@{new} # v7.0.1")
        );
        Ok(())
    }

    #[test]
    fn test_repin_sha_preserves_foreign_comment() -> Result<(), Box<dyn std::error::Error>> {
        let old = sha40('a');
        let new = sha40('b');
        let line = format!("      - uses: actions/checkout@{old} # managed by bot");
        let loc = sha_loc(&line)?;
        let target = version::Info::new(version::Version::parse("v7.0.1"), &new);
        let result = apply_line(&line, repin_sha_edit(&loc, &target).ok_or("no edit")?)
            .ok_or("edit did not apply")?;
        assert_eq!(
            result,
            format!("      - uses: actions/checkout@{new} # managed by bot")
        );
        Ok(())
    }

    #[test]
    fn test_retarget_edit_follows_the_pin_style() -> Result<(), Box<dyn std::error::Error>> {
        let old = sha40('a');
        let new = sha40('b');
        let target = version::Info::new(version::Version::parse("v4.2.0"), &new);
        let tag_line = "      - uses: actions/checkout@v4.0.0";
        let sha_line = format!("      - uses: actions/checkout@{old} # v4.0.0");
        let cases = [
            (
                tag_line.to_string(),
                false,
                Style::Preserve,
                "v4.2.0".to_string(),
            ),
            (
                tag_line.to_string(),
                false,
                Style::Sha,
                format!("{new} # v4.2.0"),
            ),
            (
                sha_line.clone(),
                true,
                Style::Preserve,
                format!("{new} # v4.2.0"),
            ),
            (sha_line, true, Style::Tag, "v4.2.0".to_string()),
        ];
        for (line, is_sha, style, written) in cases {
            let loc = sha_loc(&line)?;
            let edit = retarget_edit(&loc, is_sha, &target, style).ok_or("no edit")?;
            let result = apply_line(&line, edit).ok_or("edit did not apply")?;
            assert_eq!(
                result,
                format!("      - uses: actions/checkout@{written}"),
                "{style} {is_sha}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_rewritten_version_comment_keeps_its_note() -> Result<(), Box<dyn std::error::Error>> {
        let old = sha40('a');
        let new = sha40('b');
        let target = version::Info::new(version::Version::parse("v4.2.0"), &new);
        let line = format!("      - uses: actions/checkout@{old}  #v4.0.0  pinned for node16");
        let loc = sha_loc(&line)?;
        for (style, written, comment) in [
            (
                Style::Preserve,
                format!("{new} # v4.2.0 pinned for node16"),
                Some(" # v4.2.0 pinned for node16"),
            ),
            (Style::Tag, "v4.2.0 # pinned for node16".to_string(), None),
        ] {
            let edit = retarget_edit(&loc, true, &target, style).ok_or("no edit")?;
            assert_eq!(edit.inline_comment.as_deref(), comment, "{style}");
            let result = apply_line(&line, edit).ok_or("edit did not apply")?;
            assert_eq!(
                result,
                format!("      - uses: actions/checkout@{written}"),
                "{style}"
            );
        }
        let bare = format!("      - uses: actions/checkout@{old} # v4.0.0");
        let edit = unpin_edit(&sha_loc(&bare)?, "v4.2.0").ok_or("no edit")?;
        assert_eq!(
            apply_line(&bare, edit).ok_or("edit did not apply")?,
            "      - uses: actions/checkout@v4.2.0"
        );
        Ok(())
    }

    #[test]
    fn test_retargeted_tag_refreshes_its_version_comment() -> Result<(), Box<dyn std::error::Error>>
    {
        let target = version::Info::new(version::Version::parse("v4.2.0"), sha40('b'));
        for (line, written, comment) in [
            (
                "      - uses: actions/checkout@v4.0.0 # v4.0.0 keep",
                "      - uses: actions/checkout@v4.2.0 # v4.2.0 keep",
                Some(" # v4.2.0 keep"),
            ),
            (
                "      - uses: actions/checkout@v4 #v4.0.0",
                "      - uses: actions/checkout@v4.2.0 # v4.2.0",
                Some(" # v4.2.0"),
            ),
            (
                "      - uses: actions/checkout@v4.0.0 # mine",
                "      - uses: actions/checkout@v4.2.0 # mine",
                None,
            ),
        ] {
            for style in [Style::Preserve, Style::Tag] {
                let edit =
                    retarget_edit(&sha_loc(line)?, false, &target, style).ok_or("no edit")?;
                assert_eq!(edit.inline_comment.as_deref(), comment, "{line} {style}");
                assert_eq!(
                    apply_line(line, edit).ok_or("edit did not apply")?,
                    written,
                    "{style}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn test_retarget_edit_skips_value_not_written_verbatim() {
        let line = "      - uses: \"actions/checkout@v4.0.\\x30\"".to_string();
        let loc = UsesLocation::new(1, 16, "actions/checkout@v4.0.0".to_string(), line);
        let target = version::Info::new(version::Version::parse("v4.2.0"), sha40('b'));
        for (is_sha, style) in [
            (false, Style::Preserve),
            (false, Style::Sha),
            (true, Style::Tag),
        ] {
            assert!(retarget_edit(&loc, is_sha, &target, style).is_none());
        }
    }

    #[test]
    fn test_repin_sha_skips_value_not_written_verbatim() {
        let old = sha40('a');
        let line = format!("      - uses: \"actions/checkout@\\x61{}\"", &old[1..]);
        let loc = UsesLocation::new(1, 16, format!("actions/checkout@{old}"), line);
        let target = version::Info::new(version::Version::parse("v7.0.1"), sha40('b'));
        assert!(repin_sha_edit(&loc, &target).is_none());
    }

    #[test]
    fn test_writable_keeps_text_after_the_ref_on_lines_without_comments()
    -> Result<(), Box<dyn std::error::Error>> {
        let old = sha40('a');
        let new = sha40('b');
        let target = version::Info::new(version::Version::parse("v4.2.0"), &new);
        let text = format!("run(\"uses: o/p@{old} # v4.0.0\");\n");
        let markdown = format!("```yaml\n- uses: o/p@{old} # v4.0.0\n```\n");
        let cases = [
            (
                SourceFormat::Text,
                &text,
                1,
                Style::Preserve,
                format!("run(\"uses: o/p@{new} # v4.0.0\");\n"),
            ),
            (
                SourceFormat::Text,
                &text,
                1,
                Style::Tag,
                "run(\"uses: o/p@v4.2.0 # v4.0.0\");\n".to_string(),
            ),
            (
                SourceFormat::Markdown,
                &markdown,
                2,
                Style::Preserve,
                format!("```yaml\n- uses: o/p@{new} # v4.2.0\n```\n"),
            ),
            (
                SourceFormat::Markdown,
                &markdown,
                2,
                Style::Tag,
                "```yaml\n- uses: o/p@v4.2.0\n```\n".to_string(),
            ),
        ];
        for (format, content, line, style, expected) in cases {
            let [loc] = format
                .extract_uses(content)
                .try_into()
                .map_err(|_| "one `uses:`")?;
            assert_eq!(loc.line(), line);
            let mut fe = FileEdits::new(PathBuf::from("/test"), content.clone(), format);
            let edit = retarget_edit(&fe.writable(&loc), true, &target, style).ok_or("no edit")?;
            fe.add(edit, ());
            assert_eq!(fe.plan().content(), expected, "{format:?} {style}");
        }

        let fresh = "run(\"uses: o/p@v4.0.0\");\n";
        let [loc] = SourceFormat::Text
            .extract_uses(fresh)
            .try_into()
            .map_err(|_| "one `uses:`")?;
        let mut fe = FileEdits::new(
            PathBuf::from("/test"),
            fresh.to_string(),
            SourceFormat::Text,
        );
        let edit = repin_sha_edit(&fe.writable(&loc), &target).ok_or("no edit")?;
        fe.add(edit, ());
        let plan = fe.plan();
        assert_eq!(plan.entries, [((), EditOutcome::Applied { comment: None })]);
        assert_eq!(plan.content(), format!("run(\"uses: o/p@{new}\");\n"));
        assert_eq!(plan.splices.format(), SourceFormat::Text);
        Ok(())
    }

    #[test]
    fn test_skip_reasons_describe_the_writer_limit() {
        assert_eq!(
            SkipReason::Unplaceable.describe(),
            "`uses:` value is escaped or spans lines"
        );
        assert_eq!(SkipReason::Overlap.describe(), "edits in this file overlap");
        assert_eq!(
            SkipReason::OutOfBounds.describe(),
            "edit does not fit its line"
        );
    }

    #[test]
    fn test_file_edits_empty() {
        let plan = FileEdits::<()>::new(
            PathBuf::from("/test"),
            "content".to_string(),
            SourceFormat::Yaml,
        )
        .plan();
        assert_eq!(plan.entries.len(), 0);
        assert_eq!(plan.content(), "content");
    }

    #[test]
    fn test_file_edits_add_and_apply() {
        let content = "line1\nuses: foo@v1\nline3\n";
        let mut fe = FileEdits::new(
            PathBuf::from("/test"),
            content.to_string(),
            SourceFormat::Yaml,
        );

        fe.add(PendingEdit::replace_range(2, 10..12, "v2".to_string()), ());

        let plan = fe.plan();
        assert_eq!(plan.content(), "line1\nuses: foo@v2\nline3\n");
        assert_eq!(plan.entries, [((), EditOutcome::Applied { comment: None })]);
    }

    #[test]
    fn test_file_edits_identical_ranges_overlap() {
        let mut fe = FileEdits::new(
            PathBuf::from("/test"),
            "line1\nline2\n".to_string(),
            SourceFormat::Yaml,
        );

        fe.add(PendingEdit::replace_range(1, 4..5, "ONE".to_string()), 1);
        fe.add(PendingEdit::replace_range(1, 4..5, "UNO".to_string()), 2);

        let plan = fe.plan();
        assert_eq!(plan.content(), "line1\nline2\n");
        assert_eq!(
            plan.entries,
            [
                (1, EditOutcome::Skipped(SkipReason::Overlap)),
                (2, EditOutcome::Skipped(SkipReason::Overlap)),
            ]
        );
    }

    #[test]
    fn test_file_edits_apply_every_disjoint_edit_on_one_line() {
        let content = "x: [a@1, bb@22, ccc@333]\n";
        let mut fe = FileEdits::new(
            PathBuf::from("/test"),
            content.to_string(),
            SourceFormat::Yaml,
        );

        fe.add(PendingEdit::replace_range(1, 20..23, "3".to_string()), 'c');
        fe.add(PendingEdit::replace_range(1, 6..7, "1111".to_string()), 'a');
        fe.add(PendingEdit::replace_range(1, 12..14, String::new()), 'b');

        let plan = fe.plan();
        assert_eq!(plan.content(), "x: [a@1111, bb@, ccc@3]\n");
        assert!(
            plan.entries
                .iter()
                .all(|(_, outcome)| *outcome == EditOutcome::Applied { comment: None })
        );
    }

    #[test]
    fn test_file_edits_apply_edits_that_touch() {
        let content = "abcdefghijkl\n";
        let mut fe = FileEdits::new(
            PathBuf::from("/test"),
            content.to_string(),
            SourceFormat::Yaml,
        );

        fe.add(
            PendingEdit::replace_range(1, 2..6, "X".to_string()),
            "first",
        );
        fe.add(
            PendingEdit::replace_range(1, 6..6, "Z".to_string()),
            "touches",
        );
        fe.add(
            PendingEdit::replace_range(1, 10..12, "Q".to_string()),
            "later",
        );

        let plan = fe.plan();
        assert_eq!(plan.content(), "abXZghijQ\n");
        assert!(
            plan.entries
                .iter()
                .all(|(_, outcome)| *outcome == EditOutcome::Applied { comment: None })
        );
    }

    #[test]
    fn test_file_edits_refuse_the_batch_when_any_edits_overlap() {
        let overlapping: [(usize, Range<usize>); 3] = [(1, 4..8), (1, 6..6), (2, 0..1)];
        for (line, range) in overlapping {
            let content = "abcdefghijkl\nmnop\n";
            let mut fe = FileEdits::new(
                PathBuf::from("/test"),
                content.to_string(),
                SourceFormat::Yaml,
            );
            fe.add(
                PendingEdit::replace_range(1, 2..6, "X".to_string()).with_comment(" # x".into()),
                "first",
            );
            fe.add(
                PendingEdit::replace_range(1, 6..6, "Z".to_string()),
                "touches",
            );
            fe.add(
                PendingEdit::replace_range(line, range.clone(), "Y".to_string()),
                "overlaps",
            );
            fe.add(
                PendingEdit::replace_range(2, 0..1, "M".to_string()),
                "other line",
            );
            fe.add(
                PendingEdit::replace_range(3, 0..9, "!".to_string()),
                "out of bounds",
            );

            let plan = fe.plan();
            assert_eq!(plan.content(), content, "{line}:{range:?}");
            assert_eq!(plan.splices.apply(content), Some(content.to_string()));
            assert_eq!(
                plan.entries,
                [
                    ("first", EditOutcome::Skipped(SkipReason::Overlap)),
                    ("touches", EditOutcome::Skipped(SkipReason::Overlap)),
                    ("overlaps", EditOutcome::Skipped(SkipReason::Overlap)),
                    ("other line", EditOutcome::Skipped(SkipReason::Overlap)),
                    (
                        "out of bounds",
                        EditOutcome::Skipped(SkipReason::OutOfBounds)
                    ),
                ],
                "{line}:{range:?}"
            );
        }
    }

    #[test]
    fn test_file_edits_write_no_comment_on_a_line_with_two_uses() {
        let content = "a: [{ uses: a/b@v1 }, { uses: c/d@v2 }]\nb:\n  - uses: e/f@v3\n";
        let mut fe = FileEdits::new(
            PathBuf::from("/test"),
            content.to_string(),
            SourceFormat::Yaml,
        );

        fe.add(
            PendingEdit::replace_range(1, 16..18, "S1".to_string()).with_comment(" # v1".into()),
            1,
        );
        fe.add(PendingEdit::replace_range(1, 34..36, "S2".to_string()), 2);
        fe.add(
            PendingEdit::replace_range(3, 14..16, "S3".to_string()).with_comment(" # v3".into()),
            3,
        );

        let plan = fe.plan();
        assert_eq!(
            plan.content(),
            "a: [{ uses: a/b@S1 }, { uses: c/d@S2 }]\nb:\n  - uses: e/f@S3 # v3\n"
        );
        assert_eq!(
            plan.entries,
            [
                (1, EditOutcome::Applied { comment: None }),
                (2, EditOutcome::Applied { comment: None }),
                (
                    3,
                    EditOutcome::Applied {
                        comment: Some(" # v3".to_string())
                    }
                ),
            ]
        );
    }

    #[test]
    fn test_file_edits_write_no_comment_for_two_commented_edits_on_a_line() {
        let mut fe = FileEdits::new(
            PathBuf::from("/test"),
            "a@1 b@2".to_string(),
            SourceFormat::Yaml,
        );

        fe.add(
            PendingEdit::replace_range(1, 2..3, "X".to_string()).with_comment(" # 1".into()),
            1,
        );
        fe.add(
            PendingEdit::replace_range(1, 6..7, "Y".to_string()).with_comment(" # 2".into()),
            2,
        );

        let plan = fe.plan();
        assert_eq!(plan.content(), "a@X b@Y");
        assert_eq!(
            plan.entries,
            [
                (1, EditOutcome::Applied { comment: None }),
                (2, EditOutcome::Applied { comment: None }),
            ]
        );
    }

    #[test]
    fn test_file_edits_write_no_comment_where_a_scalar_continues() {
        for content in [
            "s:\n  - { uses: a/b@v1, name: \"multi\n    line\" }\n",
            "s:\n  - { uses: a/b@v1, name: multi\n    word }\n",
        ] {
            let mut fe = FileEdits::new(
                PathBuf::from("/test"),
                content.to_string(),
                SourceFormat::Yaml,
            );
            fe.add(
                PendingEdit::replace_range(2, 16..18, "v2".to_string())
                    .with_comment(" # v2".into()),
                (),
            );

            let plan = fe.plan();
            assert_eq!(plan.content(), content.replace("@v1", "@v2"), "{content:?}");
            assert_eq!(
                plan.entries,
                [((), EditOutcome::Applied { comment: None })],
                "{content:?}"
            );
            let refs: Vec<String> = SourceFormat::Yaml
                .extract_uses(&plan.content())
                .iter()
                .map(|loc| loc.value().to_string())
                .collect();
            assert_eq!(refs, ["a/b@v2"], "{content:?}");
        }
    }

    #[test]
    fn test_file_edits_comment_follows_an_edit_that_ends_the_line() {
        let mut fe = FileEdits::new(
            PathBuf::from("/test"),
            "uses: a@1\nnext: x\n".to_string(),
            SourceFormat::Yaml,
        );

        fe.add(
            PendingEdit::replace_range(1, 8..9, "2".to_string()).with_comment(" # 2".into()),
            (),
        );

        let plan = fe.plan();
        assert_eq!(plan.content(), "uses: a@2 # 2\nnext: x\n");
    }

    #[test]
    fn test_file_edits_preserves_trailing_newline() {
        let with_newline = "line1\nline2\n";
        let fe = FileEdits::<()>::new(
            PathBuf::from("/test"),
            with_newline.to_string(),
            SourceFormat::Yaml,
        );
        assert_eq!(fe.plan().content(), with_newline);

        let without_newline = "line1\nline2";
        let fe2 = FileEdits::<()>::new(
            PathBuf::from("/test"),
            without_newline.to_string(),
            SourceFormat::Yaml,
        );
        assert_eq!(fe2.plan().content(), without_newline);
    }

    #[test]
    fn test_file_edits_apply_failure() {
        let content = "short\n";
        let mut fe = FileEdits::new(
            PathBuf::from("/test"),
            content.to_string(),
            SourceFormat::Yaml,
        );

        fe.add(PendingEdit::replace_range(1, 100..110, "x".to_string()), ());
        fe.add(PendingEdit::replace_range(2, 0..0, "x".to_string()), ());
        fe.add(PendingEdit::replace_range(0, 0..0, "x".to_string()), ());
        fe.add(
            PendingEdit::replace_range(1, Range { start: 3, end: 2 }, "x".to_string()),
            (),
        );

        let plan = fe.plan();
        assert_eq!(plan.content(), content);
        assert!(
            plan.entries
                .iter()
                .all(|((), outcome)| *outcome == EditOutcome::Skipped(SkipReason::OutOfBounds))
        );
    }

    #[test]
    fn test_file_edits_skip_a_range_inside_a_multibyte_char() {
        let mut fe = FileEdits::new(
            PathBuf::from("/test"),
            "é@1\n".to_string(),
            SourceFormat::Yaml,
        );

        fe.add(PendingEdit::replace_range(1, 1..2, "x".to_string()), ());

        let plan = fe.plan();
        assert_eq!(plan.content(), "é@1\n");
        assert_eq!(
            plan.entries,
            [((), EditOutcome::Skipped(SkipReason::OutOfBounds))]
        );
    }

    #[test]
    fn test_file_edits_multiple_lines() {
        let content = "  - uses: actions/checkout@v1\n  - uses: actions/setup-node@v2\n";
        let mut fe = FileEdits::new(
            PathBuf::from("/test"),
            content.to_string(),
            SourceFormat::Yaml,
        );

        fe.add(PendingEdit::replace_range(1, 27..29, "v4".to_string()), ());
        fe.add(PendingEdit::replace_range(2, 29..31, "v5".to_string()), ());

        assert_eq!(
            fe.plan().content(),
            "  - uses: actions/checkout@v4\n  - uses: actions/setup-node@v5\n"
        );
    }

    #[test]
    fn test_file_edits_keep_every_line_break_and_the_bom() {
        let content =
            "\u{feff}uses: a/b@v1\r\nx:\r  - uses: c/d@v2\r\n  - uses: e/f@v3\n  - uses: g/h@v4";
        let mut fe = FileEdits::new(
            PathBuf::from("/test"),
            content.to_string(),
            SourceFormat::Yaml,
        );
        for loc in SourceFormat::Yaml.extract_uses(content) {
            let Some(range) = loc.ref_range() else {
                panic!("no ref range for {loc:?}");
            };
            fe.add(
                PendingEdit::replace_range(loc.line(), range, "NEW".to_string())
                    .with_comment(" # c".into()),
                loc.line(),
            );
        }

        let plan = fe.plan();
        assert_eq!(
            plan.content().as_bytes(),
            "\u{feff}uses: a/b@NEW # c\r\nx:\r  - uses: c/d@NEW # c\r\n  - uses: e/f@NEW # c\n  - uses: g/h@NEW # c"
                .as_bytes()
        );
        assert_eq!(
            plan.entries
                .iter()
                .map(|(line, _)| *line)
                .collect::<Vec<_>>(),
            [1, 3, 4, 5]
        );
    }

    fn plan_v2(content: &str) -> EditPlan<()> {
        let mut fe = FileEdits::new(
            PathBuf::from("/test"),
            content.to_string(),
            SourceFormat::Yaml,
        );
        fe.add(
            PendingEdit::replace_range(1, 10..12, "v2".to_string()).with_comment(" # v2".into()),
            (),
        );
        fe.plan()
    }

    #[test]
    fn test_splices_apply_keeps_changes_outside_edited_lines() {
        let plan = plan_v2("uses: a/b@v1\nname: a\n");

        assert_eq!(
            plan.splices.apply("uses: a/b@v1\nname: longer\nnext: x\n"),
            Some("uses: a/b@v2 # v2\nname: longer\nnext: x\n".to_string())
        );
        assert_eq!(
            plan.splices.apply("uses: a/b@v1\n"),
            Some("uses: a/b@v2 # v2\n".to_string())
        );
    }

    #[test]
    fn test_splices_apply_refuses_a_changed_edited_line() {
        let plan = plan_v2("uses: a/b@v1\nname: a\n");

        for current in [
            "uses: a/b@v3\nname: a\n",
            "name: a\nuses: a/b@v1\n",
            "uses: a/b@v1 # c\nname: a\n",
            "uses: a/b@v1\r\nname: a\n",
            "uses: a/b",
            "",
        ] {
            assert_eq!(plan.splices.apply(current), None, "{current:?}");
        }
    }

    #[test]
    fn test_splices_apply_refuses_an_extended_last_line() {
        let plan = plan_v2("uses: a/b@v1");

        assert_eq!(
            plan.splices.apply("uses: a/b@v1"),
            Some("uses: a/b@v2 # v2".to_string())
        );
        assert_eq!(plan.splices.apply("uses: a/b@v1 # c"), None);
        assert_eq!(plan.splices.apply("uses: a/b@v1\n"), None);
    }
}
