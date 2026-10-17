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
    rewrite::{Gap, Labels, Named, Rewrite, Trailing, after_value, closer, labeled_head},
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
    /// Comment that `new` writes in place of an old label comment.
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

    /// Record the comment that the replacement text ends with.
    #[must_use]
    pub fn with_inline_comment(mut self, comment: String) -> Self {
        self.inline_comment = Some(comment);
        self
    }
}

/// Build the edit that moves the ref at `loc` to `target` under the `style` pin style.
///
/// `kind` says whether `target` names a tag or a branch, and `labels` which
/// comments after the ref name it. A SHA is written by [`Rewrite::pin`] and
/// a tag or branch by [`Rewrite::named`]. Returns `None` when the line does
/// not hold the value verbatim.
#[must_use]
pub fn retarget_edit(
    loc: &UsesLocation,
    current_is_sha: bool,
    target: &version::Info,
    kind: Named,
    style: Style,
    labels: &Labels<'_>,
) -> Option<PendingEdit> {
    let range = loc.ref_range()?;
    let trailing = Trailing::after(loc.raw_line(), range.end, labels);
    let name = target.version().to_string();
    if style.writes_sha(current_is_sha) {
        place_edit(loc, &Rewrite::pin(target.sha(), &name, trailing), true)
    } else {
        let labeled = kind == Named::Tag && !current_is_sha;
        place_edit(
            loc,
            &Rewrite::named(&name, kind, current_is_sha, trailing),
            labeled,
        )
    }
}

/// Build the [`pin_edit`] that pins a ref to the `target` commit SHA with a ` # <version>` comment.
#[must_use]
pub fn repin_sha_edit(
    loc: &UsesLocation,
    target: &version::Info,
    labels: &Labels<'_>,
) -> Option<PendingEdit> {
    pin_edit(loc, target.sha(), &target.version().to_string(), labels)
}

/// Build an edit that writes `sha` over the ref at `loc`, keeping only the note of a label comment.
#[must_use]
pub fn sha_edit(loc: &UsesLocation, sha: &str, labels: &Labels<'_>) -> Option<PendingEdit> {
    named_edit(loc, sha, Named::Tag, true, labels)
}

/// Build an edit that writes the tag or branch `new_ref` over the ref at `loc`, as [`Rewrite::named`] writes it.
#[must_use]
pub fn named_edit(
    loc: &UsesLocation,
    new_ref: &str,
    kind: Named,
    current_is_sha: bool,
    labels: &Labels<'_>,
) -> Option<PendingEdit> {
    let range = loc.ref_range()?;
    let trailing = Trailing::after(loc.raw_line(), range.end, labels);
    place_edit(
        loc,
        &Rewrite::named(new_ref, kind, current_is_sha, trailing),
        kind == Named::Tag && !current_is_sha,
    )
}

/// Build an edit that pins the ref at `loc` to `sha` with a ` # <label>` comment, as [`Rewrite::pin`] writes it.
#[must_use]
pub fn pin_edit(
    loc: &UsesLocation,
    sha: &str,
    label: &str,
    labels: &Labels<'_>,
) -> Option<PendingEdit> {
    let range = loc.ref_range()?;
    place_edit(
        loc,
        &Rewrite::pin(
            sha,
            label,
            Trailing::after(loc.raw_line(), range.end, labels),
        ),
        true,
    )
}

fn place_edit(loc: &UsesLocation, rewrite: &Rewrite, labeled: bool) -> Option<PendingEdit> {
    let range = loc.ref_range()?;
    Some(match rewrite.comment.clone() {
        Some(comment) if rewrite.to_line_end && labeled => {
            rewrite_edit(loc, rewrite)?.with_inline_comment(comment)
        }
        Some(_) if rewrite.to_line_end => rewrite_edit(loc, rewrite)?,
        Some(comment) => PendingEdit::replace_range(loc.line(), range, rewrite.new_ref.clone())
            .with_comment(comment),
        None => rewrite_edit(loc, rewrite)?,
    })
}

/// Build the edit that writes `rewrite` at the ref of `loc`, keeping the [`closer`] of the value.
#[must_use]
pub fn rewrite_edit(loc: &UsesLocation, rewrite: &Rewrite) -> Option<PendingEdit> {
    let range = loc.ref_range()?;
    let raw = loc.raw_line();
    let (text, to_line_end) = rewrite.splice(closer(raw, range.end));
    let end = if to_line_end { raw.len() } else { range.end };
    Some(PendingEdit::replace_range(
        loc.line(),
        range.start..end,
        text,
    ))
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
        /// The comment the edit writes, appended to its line or in place of
        /// an old label comment.
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
    uses_by_line: HashMap<usize, LineUses>,
    commentable_lines: BTreeSet<usize>,
    comment_lines: BTreeSet<usize>,
    edits: Vec<(PendingEdit, L)>,
}

#[derive(Clone, Copy)]
enum LineUses {
    One { value_end: Option<usize> },
    Many,
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
        let mut uses_by_line = HashMap::new();
        for loc in format.extract_uses(&content) {
            let value_end = loc.ref_range().or_else(|| loc.value_range()).map(|r| r.end);
            uses_by_line
                .entry(loc.line())
                .and_modify(|uses| *uses = LineUses::Many)
                .or_insert(LineUses::One { value_end });
        }
        Self {
            path,
            format,
            commentable_lines: format.commentable_lines(&content),
            comment_lines: format.comment_lines(&content),
            content,
            uses_by_line,
            edits: Vec::new(),
        }
    }

    /// Return `loc` with its line cut after the value and its [`label_span`](crate::rewrite::label_span) when that line takes no comment.
    ///
    /// On a line outside [`SourceFormat::comment_lines`], an edit built from
    /// the result reads a label comment right after the value as a YAML
    /// comment and leaves the rest of the line alone.
    #[must_use]
    pub fn writable(&self, loc: &UsesLocation) -> UsesLocation {
        let raw = loc.raw_line();
        let head = loc
            .value_range()
            .filter(|_| !self.comment_lines.contains(&loc.line()))
            .map(|range| {
                let values = match self.uses_by_line.get(&loc.line()) {
                    Some(LineUses::One { .. }) => 1,
                    Some(LineUses::Many) | None => 2,
                };
                labeled_head(Gap::after(self.format, values), raw, range.end)
            });
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
    /// comment after that value, no other applied edit with a comment, and is in
    /// [`SourceFormat::commentable_lines`]. The comment replaces the
    /// trailing whitespace of its line.
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
        let mut applied: Vec<(usize, usize)> = Vec::new();
        for (i, range) in placed {
            if overlaps {
                outcomes[i] = EditOutcome::Skipped(SkipReason::Overlap);
                continue;
            }
            let EditKind::ReplaceRange { new, .. } = &self.edits[i].0.kind;
            let range_end = range.end;
            splices.push((range, false, new));
            outcomes[i] = EditOutcome::Applied {
                comment: self.edits[i].0.inline_comment.clone(),
            };
            applied.push((i, range_end));
        }

        let mut commented_per_line: HashMap<usize, usize> = HashMap::new();
        for &(i, _) in &applied {
            if self.edits[i].0.comment.is_some() {
                *commented_per_line.entry(self.edits[i].0.line).or_insert(0) += 1;
            }
        }
        for &(i, edit_end) in &applied {
            let edit = &self.edits[i].0;
            let Some(comment) = &edit.comment else {
                continue;
            };
            let Some(span) = edit.line.checked_sub(1).and_then(|idx| spans.get(idx)) else {
                continue;
            };
            let sole_comment = commented_per_line.get(&edit.line) == Some(&1);
            let line = &self.content[span.clone()];
            let uncommented_sole_uses = match self.uses_by_line.get(&edit.line) {
                Some(LineUses::One {
                    value_end: Some(end),
                }) => after_value(line, *end).is_empty(),
                Some(LineUses::One { value_end: None } | LineUses::Many) => false,
                None => !line.contains('#'),
            };
            let commentable = self.commentable_lines.contains(&edit.line);
            if uncommented_sole_uses && sole_comment && commentable {
                let text_end = span.start + line.trim_end().len();
                splices.push((text_end.max(edit_end)..span.end, true, comment));
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
            .filter_map(|&(i, _)| self.edits[i].0.line.checked_sub(1))
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

    const NO_REFS: Labels<'static> = Labels::unfetched("");

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

    #[test]
    fn test_comment_gate_reads_the_line_after_its_uses_value()
    -> Result<(), Box<dyn std::error::Error>> {
        let line = "  - uses: o/a@v1 # c";
        for range in [19..20, 18..19, 14..16, 0..1] {
            let edit = PendingEdit::replace_range(1, range.clone(), "X".into())
                .with_comment(" # v9".into());
            let result = apply_line(line, edit).ok_or("edit did not apply")?;
            assert!(!result.contains("# v9"), "{range:?}: {result}");
        }
        let line = "  - { name: \"#1\", uses: o/a@v1 }";
        let edit = PendingEdit::replace_range(1, 0..1, " ".into()).with_comment(" # v9".into());
        assert_eq!(
            apply_line(line, edit).ok_or("edit did not apply")?,
            "  - { name: \"#1\", uses: o/a@v1 } # v9"
        );
        let edit = PendingEdit::replace_range(1, 2..3, "#".into()).with_comment(" # v9".into());
        assert_eq!(
            apply_line("  - run: x # y", edit).ok_or("edit did not apply")?,
            "  # run: x # y"
        );
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
        let result = apply_line(
            &line,
            repin_sha_edit(&loc, &target, &NO_REFS).ok_or("no edit")?,
        )
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
        let result = apply_line(
            &line,
            repin_sha_edit(&loc, &target, &NO_REFS).ok_or("no edit")?,
        )
        .ok_or("edit did not apply")?;
        assert_eq!(
            result,
            format!("      - uses: actions/checkout@{new} # v7.0.1")
        );
        Ok(())
    }

    fn quoted_loc(line: &str) -> Result<UsesLocation, Box<dyn std::error::Error>> {
        let locations = crate::extract_uses_from_yaml(line);
        let [loc] = locations.as_slice() else {
            return Err(format!("expected one `uses:`, got {locations:?}").into());
        };
        Ok(loc.clone())
    }

    fn applied(
        line: &str,
        edit: Option<PendingEdit>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let mut file_edits = FileEdits::new(
            PathBuf::from("/test.yml"),
            line.to_string(),
            SourceFormat::Yaml,
        );
        let edit = edit.ok_or("no edit")?;
        file_edits.add(edit, ());
        Ok(file_edits.plan().content())
    }

    #[test]
    fn test_closers_keep_their_place_before_the_comment() -> Result<(), Box<dyn std::error::Error>>
    {
        let old = sha40('a');
        let new = sha40('b');
        let target = version::Info::new(version::Version::parse("v7.0.1"), &new);
        for (open, close) in [
            ("\"", "\""),
            ("'", "'"),
            ("{ uses: ", " }"),
            ("[{ uses: '", "' }]"),
        ] {
            let at = |body: &str, comment: &str| {
                let uses = if open.starts_with(['{', '[']) {
                    ""
                } else {
                    "uses: "
                };
                format!("      - {uses}{open}actions/checkout@{body}{close}{comment}\n")
            };
            let wrap =
                |line: String| -> Result<(String, UsesLocation), Box<dyn std::error::Error>> {
                    let loc = quoted_loc(&line)?;
                    Ok((line, loc))
                };
            let (line, loc) = wrap(at(&old, " # v7.0.0"))?;
            assert_eq!(
                applied(&line, repin_sha_edit(&loc, &target, &NO_REFS))?,
                at(&new, " # v7.0.1")
            );
            let (line, loc) = wrap(at(&old, ""))?;
            assert_eq!(
                applied(&line, repin_sha_edit(&loc, &target, &NO_REFS))?,
                at(&new, " # v7.0.1")
            );
            let (line, loc) = wrap(at(&old, " # v7.0.0"))?;
            assert_eq!(
                applied(
                    &line,
                    named_edit(&loc, "v7.0.1", Named::Tag, true, &NO_REFS)
                )?,
                at("v7.0.1", "")
            );
            let (line, loc) = wrap(at("v7.0.0", " # v7.0.0 keep"))?;
            assert_eq!(
                applied(
                    &line,
                    retarget_edit(&loc, false, &target, Named::Tag, Style::Preserve, &NO_REFS)
                )?,
                at("v7.0.1", " # v7.0.1 keep")
            );
        }
        Ok(())
    }

    #[test]
    fn test_repin_writes_no_comment_after_other_keys() -> Result<(), Box<dyn std::error::Error>> {
        let old = sha40('a');
        let new = sha40('b');
        let target = version::Info::new(version::Version::parse("v7.0.1"), &new);
        for (with, comment, repinned, unpinned) in [
            ("{ x: 1 }", "", "", ""),
            ("{ x: 1 }", " # v7.0.0", " # v7.0.1", ""),
            ("{ x: 1 }", " # v7.0.0 keep", " # v7.0.1 keep", " # keep"),
            ("{ msg: it's }", " # v7.0.0", " # v7.0.1", ""),
            ("{ a: it's, b: 'x # v9' }", "", "", ""),
            ("{ b: 'it''s # v9' }", "", "", ""),
            ("{ b: 'it''s # x' }", " # v7.0.0", " # v7.0.1", ""),
            ("{ b: !!str 'x # v9' }", "", "", ""),
            ("{ b: &a \"x # v9\" }", " # v7.0.0", " # v7.0.1", ""),
        ] {
            let at = |r: &str, comment: &str| {
                format!("      - {{ uses: actions/checkout@{r}, with: {with} }}{comment}\n")
            };
            let line = at(&old, comment);
            let loc = quoted_loc(&line)?;
            assert_eq!(
                applied(&line, repin_sha_edit(&loc, &target, &NO_REFS))?,
                at(&new, repinned),
                "{line}"
            );
            assert_eq!(
                applied(
                    &line,
                    named_edit(&loc, "v7.0.0", Named::Tag, true, &NO_REFS)
                )?,
                at("v7.0.0", unpinned),
                "{line}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_a_new_comment_replaces_trailing_whitespace() -> Result<(), Box<dyn std::error::Error>> {
        let old = sha40('a');
        let new = sha40('b');
        let target = version::Info::new(version::Version::parse("v7.0.1"), &new);
        for (line, expected) in [
            (
                format!("      - uses: a/b@{old}   \n"),
                format!("      - uses: a/b@{new} # v7.0.1\n"),
            ),
            (
                format!("      - uses: \"a/b@{old}\" \t\n"),
                format!("      - uses: \"a/b@{new}\" # v7.0.1\n"),
            ),
            (
                format!("      - {{ uses: 'a/b@{old}' }}  \r\n"),
                format!("      - {{ uses: 'a/b@{new}' }} # v7.0.1\r\n"),
            ),
            (
                format!("      - uses: a/b@{old} # v7.0.0  \n"),
                format!("      - uses: a/b@{new} # v7.0.1\n"),
            ),
            (
                format!("      - uses: a/b@{old} # mine  \n"),
                format!("      - uses: a/b@{new} # mine  \n"),
            ),
        ] {
            let loc = quoted_loc(&line)?;
            assert_eq!(
                applied(&line, repin_sha_edit(&loc, &target, &NO_REFS))?,
                expected,
                "{line:?}"
            );
            assert_eq!(
                applied(&line, pin_edit(&loc, &new, "v7.0.1", &NO_REFS))?,
                expected,
                "{line:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_a_hash_before_the_value_allows_a_comment() -> Result<(), Box<dyn std::error::Error>> {
        let old = sha40('a');
        let new = sha40('b');
        let target = version::Info::new(version::Version::parse("v7.0.1"), &new);
        let at = |r: &str, comment: &str| {
            format!("      - {{ name: \"Build #1\", uses: a/b@{r} }}{comment}\n")
        };
        let (old, new) = (old.as_str(), new.as_str());
        for (current, comment, is_sha, style, written, written_comment) in [
            (old, "", true, Style::Preserve, new, " # v7.0.1"),
            (old, " # v7.0.0", true, Style::Preserve, new, " # v7.0.1"),
            ("v7.0.0", "", false, Style::Sha, new, " # v7.0.1"),
            (old, " # v7.0.0", true, Style::Tag, "v7.0.1", ""),
            ("v7.0.0", "", false, Style::Preserve, "v7.0.1", ""),
        ] {
            let line = at(current, comment);
            let loc = quoted_loc(&line)?;
            assert_eq!(
                applied(
                    &line,
                    retarget_edit(&loc, is_sha, &target, Named::Tag, style, &NO_REFS)
                )?,
                at(written, written_comment),
                "{line} {style}"
            );
        }
        let line = at("main", "");
        let loc = quoted_loc(&line)?;
        assert_eq!(
            applied(&line, pin_edit(&loc, new, "main", &NO_REFS))?,
            at(new, " # main")
        );
        let line = at("main", " # mine");
        let loc = quoted_loc(&line)?;
        assert_eq!(
            applied(&line, pin_edit(&loc, new, "main", &NO_REFS))?,
            at(new, " # mine")
        );
        Ok(())
    }

    fn retargeted(
        line: &str,
        is_sha: bool,
        target: &version::Info,
        kind: Named,
        style: Style,
        labels: &Labels<'_>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let loc = quoted_loc(line)?;
        applied(
            line,
            retarget_edit(&loc, is_sha, target, kind, style, labels),
        )
    }

    #[test]
    fn test_labels_after_named_refs_follow_the_writer() -> Result<(), Box<dyn std::error::Error>> {
        let old = sha40('a');
        let new = sha40('b');
        let tag = version::Info::new(version::Version::parse("v4.2.0"), &new);
        let main = version::Info::new(version::Version::parse("main"), sha40('c'));
        let mut versions = version::Set::new();
        versions.set_branches(HashMap::from([("main".to_string(), sha40('c'))]));
        let at = |r: &str, comment: &str| format!("      - uses: o/a@{r}{comment}\n");
        let fetched = |current| Labels::fetched(current, &versions);
        let (preserve, branch) = (Style::Preserve, Named::Branch);

        let line = at("main", " # main keep");
        let written = retargeted(
            &line,
            false,
            &tag,
            Named::Tag,
            preserve,
            &Labels::unfetched("main"),
        )?;
        assert_eq!(written, at("v4.2.0", " # v4.2.0 keep"));
        let loc = quoted_loc(&line)?;
        let pinned = applied(
            &line,
            pin_edit(&loc, &new, "main", &Labels::unfetched("main")),
        )?;
        assert_eq!(pinned, at(&new, " # main keep"));

        let line = at(&old, " # main keep");
        let written = retargeted(&line, true, &tag, Named::Tag, preserve, &fetched(&old))?;
        assert_eq!(written, at(&new, " # v4.2.0 keep"));
        let written = retargeted(
            &line,
            true,
            &tag,
            Named::Tag,
            preserve,
            &Labels::unfetched(&old),
        )?;
        assert_eq!(written, at(&new, " # main keep"));

        let line = at("v4.0.0", " # v4.0.0 keep");
        let unfetched = Labels::unfetched("v4.0.0");
        let written = retargeted(&line, false, &tag, Named::Tag, preserve, &unfetched)?;
        assert_eq!(written, at("v4.2.0", " # v4.2.0 keep"));
        let written = retargeted(&line, false, &main, branch, preserve, &unfetched)?;
        assert_eq!(written, at("main", " # keep"));
        let written = retargeted(&line, false, &main, branch, Style::Sha, &unfetched)?;
        assert_eq!(written, at(&sha40('c'), " # main keep"));

        let line = at(&old, " # v4.0.0 keep");
        let written = retargeted(&line, true, &tag, Named::Tag, Style::Tag, &fetched(&old))?;
        assert_eq!(written, at("v4.2.0", " # keep"));

        let line = at("v4.0.0", " # lint keep");
        let written = retargeted(&line, false, &tag, Named::Tag, preserve, &fetched("v4.0.0"))?;
        assert_eq!(written, at("v4.2.0", " # lint keep"));
        let line = at(&old, " # TODO fix");
        let written = retargeted(&line, true, &tag, Named::Tag, preserve, &fetched(&old))?;
        assert_eq!(written, at(&new, " # TODO fix"));
        Ok(())
    }

    #[test]
    fn test_a_line_with_two_values_takes_no_comment() -> Result<(), Box<dyn std::error::Error>> {
        let old = sha40('a');
        let new = sha40('b');
        let target = version::Info::new(version::Version::parse("v7.0.1"), &new);
        let line = format!("      - [{{ uses: a/b@v1 }}, {{ uses: c/d@{old} }}] # v7.0.0\n");
        let locations = crate::extract_uses_from_yaml(&line);
        let [_, loc] = locations.as_slice() else {
            return Err(format!("expected two `uses:`, got {locations:?}").into());
        };
        let file_edits =
            FileEdits::<()>::new(PathBuf::from("/t.yml"), line.clone(), SourceFormat::Yaml);
        let loc = file_edits.writable(loc);
        assert_eq!(
            applied(&line, repin_sha_edit(&loc, &target, &NO_REFS))?,
            line.replace(&old, &new)
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
        let result = apply_line(
            &line,
            repin_sha_edit(&loc, &target, &NO_REFS).ok_or("no edit")?,
        )
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
            let edit = retarget_edit(&loc, is_sha, &target, Named::Tag, style, &NO_REFS)
                .ok_or("no edit")?;
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
            let edit =
                retarget_edit(&loc, true, &target, Named::Tag, style, &NO_REFS).ok_or("no edit")?;
            assert_eq!(edit.inline_comment.as_deref(), comment, "{style}");
            let result = apply_line(&line, edit).ok_or("edit did not apply")?;
            assert_eq!(
                result,
                format!("      - uses: actions/checkout@{written}"),
                "{style}"
            );
        }
        let bare = format!("      - uses: actions/checkout@{old} # v4.0.0");
        let edit =
            named_edit(&sha_loc(&bare)?, "v4.2.0", Named::Tag, true, &NO_REFS).ok_or("no edit")?;
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
            (
                "      - uses: actions/checkout@v4.0.0 # 1st step",
                "      - uses: actions/checkout@v4.2.0 # 1st step",
                None,
            ),
        ] {
            for style in [Style::Preserve, Style::Tag] {
                let edit =
                    retarget_edit(&sha_loc(line)?, false, &target, Named::Tag, style, &NO_REFS)
                        .ok_or("no edit")?;
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
    fn test_a_sha_target_keeps_a_comment_that_names_no_version()
    -> Result<(), Box<dyn std::error::Error>> {
        let target = version::Info::new(version::Version::parse("v4.2.0"), sha40('b'));
        let old = sha40('a');
        let new = sha40('b');
        for (line, current_is_sha, style) in [
            (
                "      - uses: actions/checkout@v4.0.0 # 1st step".to_string(),
                false,
                Style::Sha,
            ),
            (
                format!("      - uses: actions/checkout@{old} # 1st step"),
                true,
                Style::Preserve,
            ),
        ] {
            let edit = retarget_edit(
                &sha_loc(&line)?,
                current_is_sha,
                &target,
                Named::Tag,
                style,
                &NO_REFS,
            )
            .ok_or("no edit")?;
            assert_eq!(edit.inline_comment, None, "{line}");
            assert_eq!(
                apply_line(&line, edit).ok_or("edit did not apply")?,
                format!("      - uses: actions/checkout@{new} # 1st step"),
                "{line}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_pin_edit_comments_only_a_line_that_ends_at_the_ref()
    -> Result<(), Box<dyn std::error::Error>> {
        let sha = sha40('c');
        for (line, written) in [
            ("- uses: o/b@main", format!("- uses: o/b@{sha} # main")),
            (
                "- uses: o/b@main # v1.0.0 keep",
                format!("- uses: o/b@{sha} # main keep"),
            ),
            (
                "- { uses: 'o/b@main' }",
                format!("- {{ uses: 'o/b@{sha}' }} # main"),
            ),
            (
                "- { uses: \"o/b@main\" }",
                format!("- {{ uses: \"o/b@{sha}\" }} # main"),
            ),
            (
                "- { uses: o/b@main, with: { x: 1 } }",
                format!("- {{ uses: o/b@{sha}, with: {{ x: 1 }} }}"),
            ),
            (
                "- uses: o/b@main # mine",
                format!("- uses: o/b@{sha} # mine"),
            ),
        ] {
            let found = crate::extract_uses_from_yaml(line);
            let [loc] = found.as_slice() else {
                return Err(format!("expected one `uses:` in {line:?}").into());
            };
            let edit = pin_edit(loc, &sha, "main", &NO_REFS).ok_or("no edit")?;
            assert_eq!(
                apply_line(line, edit).ok_or("edit did not apply")?,
                written,
                "{line}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_a_sha_retarget_refreshes_a_build_metadata_version_comment()
    -> Result<(), Box<dyn std::error::Error>> {
        let target = version::Info::new(version::Version::parse("v4.2.0"), sha40('b'));
        let line = format!(
            "      - uses: actions/checkout@{} # v4.0.0+build keep",
            sha40('a')
        );
        let edit = retarget_edit(
            &sha_loc(&line)?,
            true,
            &target,
            Named::Tag,
            Style::Preserve,
            &NO_REFS,
        )
        .ok_or("no edit")?;
        assert_eq!(edit.inline_comment.as_deref(), Some(" # v4.2.0 keep"));
        assert_eq!(
            apply_line(&line, edit).ok_or("edit did not apply")?,
            format!(
                "      - uses: actions/checkout@{} # v4.2.0 keep",
                sha40('b')
            )
        );
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
            assert!(retarget_edit(&loc, is_sha, &target, Named::Tag, style, &NO_REFS).is_none());
        }
    }

    #[test]
    fn test_repin_sha_skips_value_not_written_verbatim() {
        let old = sha40('a');
        let line = format!("      - uses: \"actions/checkout@\\x61{}\"", &old[1..]);
        let loc = UsesLocation::new(1, 16, format!("actions/checkout@{old}"), line);
        let target = version::Info::new(version::Version::parse("v7.0.1"), sha40('b'));
        assert!(repin_sha_edit(&loc, &target, &NO_REFS).is_none());
    }

    fn text_retargeted(
        format: SourceFormat,
        content: &str,
        current_is_sha: bool,
        style: Style,
        labels: &Labels<'_>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let target = version::Info::new(version::Version::parse("v4.2.0"), sha40('b'));
        let [loc] = format
            .extract_uses(content)
            .try_into()
            .map_err(|_| "one `uses:`")?;
        let mut fe = FileEdits::new(PathBuf::from("/test"), content.to_string(), format);
        let edit = retarget_edit(
            &fe.writable(&loc),
            current_is_sha,
            &target,
            Named::Tag,
            style,
            labels,
        )
        .ok_or("no edit")?;
        fe.add(edit, ());
        Ok(fe.plan().content())
    }

    #[test]
    fn test_writable_rewrites_the_label_of_a_pin_on_a_line_without_comments()
    -> Result<(), Box<dyn std::error::Error>> {
        let old = sha40('a');
        let new = sha40('b');
        let text = format!("run(\"uses: o/p@{old} # v4.0.0\");\n");
        let noted = format!("run(\"uses: o/p@{old} # v4.0.0 keep this\");\n");
        let foreign = format!("run(\"uses: o/p@{old} # TODO fix\");\n");
        let closed = format!("[{{\"uses\": \"o/p@{old}\"}},  # v4.0.0 keep\n");
        let markdown = format!("```yaml\n- uses: o/p@{old} # v4.0.0\n```\n");
        let cases = [
            (
                SourceFormat::Text,
                &text,
                Style::Preserve,
                format!("run(\"uses: o/p@{new} # v4.2.0\");\n"),
            ),
            (
                SourceFormat::Text,
                &text,
                Style::Tag,
                "run(\"uses: o/p@v4.2.0\");\n".to_string(),
            ),
            (
                SourceFormat::Text,
                &noted,
                Style::Preserve,
                format!("run(\"uses: o/p@{new} # v4.2.0 keep this\");\n"),
            ),
            (
                SourceFormat::Text,
                &noted,
                Style::Tag,
                "run(\"uses: o/p@v4.2.0 # keep this\");\n".to_string(),
            ),
            (
                SourceFormat::Text,
                &foreign,
                Style::Preserve,
                format!("run(\"uses: o/p@{new} # TODO fix\");\n"),
            ),
            (
                SourceFormat::Text,
                &closed,
                Style::Preserve,
                format!("[{{\"uses\": \"o/p@{new}\"}}, # v4.2.0 keep\n"),
            ),
            (
                SourceFormat::Markdown,
                &markdown,
                Style::Preserve,
                format!("```yaml\n- uses: o/p@{new} # v4.2.0\n```\n"),
            ),
            (
                SourceFormat::Markdown,
                &markdown,
                Style::Tag,
                "```yaml\n- uses: o/p@v4.2.0\n```\n".to_string(),
            ),
        ];
        for (format, content, style, expected) in cases {
            assert_eq!(
                text_retargeted(format, content, true, style, &NO_REFS)?,
                expected,
                "{content:?} {style}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_writable_keeps_a_comment_after_two_text_values()
    -> Result<(), Box<dyn std::error::Error>> {
        let (other, old, new) = (sha40('c'), sha40('a'), sha40('b'));
        let target = version::Info::new(version::Version::parse("v4.2.0"), &new);
        let line =
            format!("[{{\"uses\": \"a/b@{other}\"}}, {{\"uses\": \"o/p@{old}\"}}]  # v4.0.0\n");
        let locations = SourceFormat::Text.extract_uses(&line);
        let [_, loc] = locations.as_slice() else {
            return Err(format!("expected two `uses:`, got {locations:?}").into());
        };
        let mut fe = FileEdits::new(PathBuf::from("/test"), line.clone(), SourceFormat::Text);
        let edit = repin_sha_edit(&fe.writable(loc), &target, &NO_REFS).ok_or("no edit")?;
        fe.add(edit, ());
        assert_eq!(fe.plan().content(), line.replace(&old, &new));
        Ok(())
    }

    #[test]
    fn test_writable_rewrites_the_label_of_a_tag_on_a_line_without_comments()
    -> Result<(), Box<dyn std::error::Error>> {
        for (tagged, current, expected) in [
            (
                "run(\"uses: o/p@v4.0.0 # v4.0.0 keep\");\n",
                "v4.0.0",
                "run(\"uses: o/p@v4.2.0 # v4.2.0 keep\");\n",
            ),
            (
                "run(\"uses: o/p@main # main keep\");\n",
                "main",
                "run(\"uses: o/p@v4.2.0 # v4.2.0 keep\");\n",
            ),
            (
                "run(\"uses: o/p@main # lint keep\");\n",
                "main",
                "run(\"uses: o/p@v4.2.0 # lint keep\");\n",
            ),
        ] {
            assert_eq!(
                text_retargeted(
                    SourceFormat::Text,
                    tagged,
                    false,
                    Style::Preserve,
                    &Labels::unfetched(current)
                )?,
                expected,
                "{tagged:?}"
            );
        }

        let new = sha40('b');
        let target = version::Info::new(version::Version::parse("v4.2.0"), &new);
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
        let edit = repin_sha_edit(&fe.writable(&loc), &target, &NO_REFS).ok_or("no edit")?;
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
