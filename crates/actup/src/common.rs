//! Shared infrastructure for workflow scanning commands.
//!
//! This module provides the common pipeline used by check, update, and pin commands:
//! discover -> read -> extract uses -> filter -> provider lookup -> (command-specific logic)

use std::{
    collections::HashMap,
    fs,
    ops::Range,
    path::{Path, PathBuf},
};

use actup_core::{
    ActionRef, Config, DiscoveredFile, Error as CoreError, Provider, UsesLocation, discover_files,
    extract_uses_from_yaml, parse_uses, provider, version,
};
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::cli::{CliExitCode, OutputFormat};

// ErrorTally - centralized error counting and exit code determination

/// Tracks errors by category for consistent exit code behavior.
#[derive(Default, Debug)]
pub struct ErrorTally {
    /// User errors: bad refs, not found, invalid config
    user: usize,
    /// Environment errors: network, auth, rate limit
    env: usize,
}

impl ErrorTally {
    /// Record an error, categorizing it automatically.
    pub const fn record(&mut self, err: &CoreError) {
        if err.is_environment_error() {
            self.env += 1;
        } else {
            self.user += 1;
        }
    }

    /// Record an environment error (for I/O failures, etc.).
    pub const fn record_env(&mut self) {
        self.env += 1;
    }

    /// Total error count.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.user + self.env
    }

    /// Determine exit code based on error categories.
    ///
    /// Priority: env errors (3) > user errors (2) > success (0)
    #[must_use]
    pub const fn exit_code(&self) -> CliExitCode {
        if self.env > 0 {
            CliExitCode::EnvironmentError
        } else if self.user > 0 {
            CliExitCode::UserError
        } else {
            CliExitCode::Success
        }
    }

    /// Human-readable summary line for errors.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        if self.total() == 0 {
            return None;
        }
        let mut parts = Vec::new();
        if self.user > 0 {
            parts.push(format!("{} user error(s)", self.user));
        }
        if self.env > 0 {
            parts.push(format!("{} environment error(s)", self.env));
        }
        Some(parts.join(", "))
    }
}

// PendingEdit - represents an edit to apply to a line

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
}

impl PendingEdit {
    /// Create an edit that replaces a byte range.
    #[must_use]
    pub const fn replace_range(line: usize, range: Range<usize>, new: String) -> Self {
        Self {
            line,
            kind: EditKind::ReplaceRange { range, new },
            comment: None,
        }
    }

    /// Add a comment to append after the edit.
    #[must_use]
    pub fn with_comment(mut self, comment: String) -> Self {
        self.comment = Some(comment);
        self
    }
}

/// Build an edit that re-pins a SHA-pinned ref to the `target` commit SHA.
///
/// Re-pins to the SHA (never unpins to a tag) and keeps a `# vX.Y.Z` version
/// comment accurate: a stale version comment is refreshed, a missing one is
/// added, and a foreign comment is left untouched. Shared by `update` and `pin`.
pub fn repin_sha_edit(loc: &UsesLocation, target: &version::Info) -> Option<PendingEdit> {
    let new_sha = target.sha();
    let version_comment = format!(" # {}", target.version());
    let range = loc.ref_range()?;

    let raw = loc.raw_line();
    let tail = raw.get(range.end..).unwrap_or("").trim_start();
    Some(if is_version_comment(tail) {
        // Overwrite the SHA and the stale version comment together.
        PendingEdit::replace_range(
            loc.line(),
            range.start..raw.len(),
            format!("{new_sha}{version_comment}"),
        )
    } else {
        // `with_comment` adds the version note only when the line has none, so a
        // foreign comment is preserved.
        PendingEdit::replace_range(loc.line(), range, new_sha.to_string())
            .with_comment(version_comment)
    })
}

/// Why `update` or `pin` leaves an action reference unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The `uses:` value is escaped or spans lines and has no in-place range.
    Unplaceable,
    /// The edit's range overlaps the range of an earlier edit.
    Overlap,
    /// The edit's range does not fit its line in the source text.
    OutOfBounds,
}

impl SkipReason {
    const fn describe(self) -> &'static str {
        match self {
            Self::Unplaceable => "`uses:` value is escaped or spans lines",
            Self::Overlap => "edit overlaps another edit",
            Self::OutOfBounds => "edit does not fit its line",
        }
    }
}

/// An action reference that `update` or `pin` left unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedRef {
    /// File path as displayed.
    pub file: String,
    /// Line number (1-indexed).
    pub line: usize,
    /// The full `uses:` reference.
    pub action: String,
    /// Owner or group.
    pub owner: String,
    /// Repository or project.
    pub repo: String,
    /// Host written in the reference, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Provider kind serving the reference.
    pub provider: String,
    /// The ref after `@`.
    pub reference: String,
    /// Why the writer left it unchanged.
    pub reason: SkipReason,
}

/// Warn that the writer leaves `action_ref` unchanged and record it as skipped.
pub fn report_skipped(
    relative: &str,
    line: usize,
    action_ref: &ActionRef,
    provider: &str,
    reason: SkipReason,
    result: &mut EditResult,
) {
    eprintln!(
        "{}: {relative}:{line}: {action_ref} - {}; edit it by hand",
        "Warning".yellow(),
        reason.describe()
    );
    result.skipped.push(SkippedRef {
        file: relative.to_string(),
        line,
        action: action_ref.to_string(),
        owner: action_ref.owner().to_string(),
        repo: action_ref.repo().to_string(),
        host: action_ref.host().map(String::from),
        provider: provider.to_string(),
        reference: action_ref.ref_str().to_string(),
        reason,
    });
}

/// Whether a trailing line fragment is a version comment like `# v1.2.3` or `# 1.2`.
fn is_version_comment(tail: &str) -> bool {
    let Some(body) = tail.strip_prefix('#') else {
        return false;
    };
    let body = body.trim();
    let body = body.strip_prefix('v').unwrap_or(body);
    body.starts_with(|c: char| c.is_ascii_digit())
}

// FileEdits - accumulated edits for a single file

/// What the writer does with one edit of a [`FileEdits`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOutcome {
    /// The edit is written, with `comment` appended at the end of its line when present.
    Applied {
        /// The appended comment, if one is written.
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
    uses_per_line: HashMap<usize, usize>,
    edits: Vec<(PendingEdit, L)>,
}

/// The planned result of a [`FileEdits`].
pub struct EditPlan<L> {
    /// Absolute path to the file.
    pub path: PathBuf,
    /// The file content with every applied edit.
    pub content: String,
    /// Each label with its outcome, in insertion order.
    pub entries: Vec<(L, EditOutcome)>,
}

impl<L> FileEdits<L> {
    /// Create a new `FileEdits` for a file.
    #[must_use]
    pub fn new(path: PathBuf, content: String) -> Self {
        let mut uses_per_line = HashMap::new();
        for loc in extract_uses_from_yaml(&content) {
            *uses_per_line.entry(loc.line()).or_insert(0) += 1;
        }
        Self {
            path,
            content,
            uses_per_line,
            edits: Vec::new(),
        }
    }

    /// Add an edit with its label.
    pub fn add(&mut self, edit: PendingEdit, label: L) {
        self.edits.push((edit, label));
    }

    /// Place every edit on the original text and build the new content.
    ///
    /// An edit whose range overlaps an earlier edit (in source order) is
    /// skipped; edits that share a line without overlapping all apply. A
    /// comment is appended only when its line holds a single `uses:` value, no
    /// `#`, and no other applied edit with a comment.
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

        let mut splices: Vec<(Range<usize>, bool, &str)> = Vec::new();
        let mut applied: Vec<usize> = Vec::new();
        let mut last: Option<Range<usize>> = None;
        for (i, range) in placed {
            let overlaps = last.as_ref().is_some_and(|prev| {
                let starts_inside = range.start < prev.end;
                let shares_start = range.start == prev.start;
                starts_inside || shares_start
            });
            if overlaps {
                outcomes[i] = EditOutcome::Skipped(SkipReason::Overlap);
                continue;
            }
            let EditKind::ReplaceRange { new, .. } = &self.edits[i].0.kind;
            splices.push((range.clone(), false, new));
            outcomes[i] = EditOutcome::Applied { comment: None };
            applied.push(i);
            last = Some(range);
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
            if sole_uses && sole_comment && uncommented {
                splices.push((span.end..span.end, true, comment));
                outcomes[i] = EditOutcome::Applied {
                    comment: Some(comment.clone()),
                };
            }
        }

        splices.sort_by_key(|(range, is_comment, _)| (range.start, range.end, *is_comment));
        let mut content = self.content.clone();
        for (range, _, text) in splices.into_iter().rev() {
            content.replace_range(range, text);
        }

        EditPlan {
            path: self.path,
            content,
            entries: self
                .edits
                .into_iter()
                .zip(outcomes)
                .map(|((_, label), outcome)| (label, outcome))
                .collect(),
        }
    }
}

fn line_spans(content: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = 0;
    for piece in content.split_inclusive('\n') {
        let text = piece
            .strip_suffix('\n')
            .map_or(piece, |line| line.strip_suffix('\r').unwrap_or(line));
        spans.push(start..start + text.len());
        start += piece.len();
    }
    spans
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

/// One action reference change, for progress output.
#[derive(Debug, Clone)]
pub struct Change {
    /// The action reference as found.
    pub action: ActionRef,
    /// Provider kind serving the reference.
    pub provider: String,
    /// Line number (1-indexed).
    pub line: usize,
    /// The written target, without any appended comment.
    pub target: String,
    /// Whether progress output shows the appended comment after `target`.
    pub show_comment: bool,
}

impl Change {
    /// The target as written, with the appended `comment` when shown.
    #[must_use]
    pub fn written(&self, comment: Option<&str>) -> String {
        match comment {
            Some(comment) if self.show_comment => format!("{}{comment}", self.target),
            _ => self.target.clone(),
        }
    }
}

/// Plan `file_edits`, report its skipped edits, and write it unless `dry_run`.
///
/// Returns each written change with its appended comment, and counts them in
/// `result`. A failed write records an environment error and returns nothing.
pub fn commit_file(
    file_edits: FileEdits<Change>,
    relative: &str,
    dry_run: bool,
    result: &mut EditResult,
) -> Vec<(Change, Option<String>)> {
    let EditPlan {
        path,
        content,
        entries,
    } = file_edits.plan();
    let mut written = Vec::new();
    for (change, outcome) in entries {
        match outcome {
            EditOutcome::Applied { comment } => written.push((change, comment)),
            EditOutcome::Skipped(reason) => {
                report_skipped(
                    relative,
                    change.line,
                    &change.action,
                    &change.provider,
                    reason,
                    result,
                );
            }
        }
    }
    if written.is_empty() {
        return written;
    }
    if !dry_run && let Err(e) = fs::write(&path, content) {
        eprintln!("{}: {} - {e}", "Error writing file".red(), path.display());
        result.errors.record_env();
        return Vec::new();
    }
    result.count += written.len();
    written
}

// WorkflowScanner - shared file discovery and action extraction

/// Shared infrastructure for scanning workflow files.
pub struct WorkflowScanner<'a> {
    path: &'a Path,
    config: &'a Config,
    registry: &'a provider::Registry,
    format: OutputFormat,
}

impl<'a> WorkflowScanner<'a> {
    /// Create a new scanner.
    #[must_use]
    pub const fn new(
        path: &'a Path,
        config: &'a Config,
        registry: &'a provider::Registry,
        format: OutputFormat,
    ) -> Self {
        Self {
            path,
            config,
            registry,
            format,
        }
    }

    /// Discover workflow files. Returns empty vec if none found.
    #[must_use]
    pub fn discover(&self) -> Vec<DiscoveredFile> {
        discover_files(self.path, &self.config.scan.extra_paths)
    }

    /// Print "no files found" message (human format only).
    pub fn print_empty_message(&self) {
        if matches!(self.format, OutputFormat::Human) {
            let message = if looks_like_yaml_input(self.path) {
                format!(
                    "No workflow or action file found at {}",
                    self.path.display()
                )
            } else {
                format!("No workflow files found under {}", self.path.display())
            };
            println!("{}", message.dimmed());
        }
    }

    /// Read a file's content, tracking errors in tally.
    #[allow(clippy::unused_self)]
    pub fn read_file(&self, file: &DiscoveredFile, errors: &mut ErrorTally) -> Option<String> {
        match fs::read_to_string(file.path()) {
            Ok(content) => Some(content),
            Err(e) => {
                eprintln!("{}: {} - {}", "Warning".yellow(), file.path().display(), e);
                errors.record_env();
                None
            }
        }
    }

    /// Get relative path for display.
    #[must_use]
    pub fn relative_path(&self, file: &DiscoveredFile) -> String {
        if self.path.is_file() {
            return file.path().display().to_string();
        }

        let relative = file
            .path()
            .strip_prefix(self.path)
            .unwrap_or_else(|_| file.path());

        if relative.as_os_str().is_empty() {
            file.path().display().to_string()
        } else {
            relative.display().to_string()
        }
    }

    /// Extract action references from content, filtering as appropriate.
    ///
    /// Yields `(UsesLocation, ActionRef)` pairs for non-local actions.
    ///
    /// Note: Ignored actions are NOT filtered here - they flow through to the check
    /// phase where they're marked with `CheckStatus::Ignored` for visibility.
    pub fn extract_actions(content: &str) -> impl Iterator<Item = (UsesLocation, ActionRef)> + '_ {
        extract_uses_from_yaml(content)
            .into_iter()
            .filter_map(|loc| {
                let Ok(Some(action_ref)) = parse_uses(loc.value()) else {
                    return None; // Local action or parse error
                };

                Some((loc, action_ref))
            })
    }

    /// Look up provider for an action. Returns None if not found.
    #[must_use]
    pub fn provider_for(&self, action: &ActionRef) -> Option<&dyn Provider> {
        self.registry.find(action.host())
    }
}

fn looks_like_yaml_input(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "yml" || ext == "yaml")
}

// EditResult - result of applying edits across files

/// Structured summary of an `update` or `pin` run.
#[derive(Debug, Serialize)]
pub struct EditReport<'a> {
    /// Number of edited action references.
    pub edited: usize,
    /// Whether the run left files unwritten.
    pub dry_run: bool,
    /// Action references left unchanged.
    pub skipped: &'a [SkippedRef],
}

/// Summary of edit operations.
#[derive(Default)]
pub struct EditResult {
    /// Total number of edits applied.
    pub count: usize,
    /// Action references left unchanged by a writer limitation.
    pub skipped: Vec<SkippedRef>,
    /// Error tally.
    pub errors: ErrorTally,
}

impl EditResult {
    /// Print the summary: text for human output, an [`EditReport`] for JSON and TOML.
    pub fn print_summary(&self, dry_run: bool, verb: &str, dry_verb: &str, format: OutputFormat) {
        let structured = match format {
            OutputFormat::Human => None,
            OutputFormat::Json => {
                Some(serde_json::to_string_pretty(&self.report(dry_run)).map_err(|e| e.to_string()))
            }
            OutputFormat::Toml => {
                Some(toml::to_string_pretty(&self.report(dry_run)).map_err(|e| e.to_string()))
            }
        };
        match structured {
            Some(Ok(text)) => println!("{text}"),
            Some(Err(e)) => eprintln!("Error serializing edit report: {e}"),
            None => self.print_human(dry_run, verb, dry_verb),
        }
    }

    /// The structured summary of this run.
    #[must_use]
    pub fn report(&self, dry_run: bool) -> EditReport<'_> {
        EditReport {
            edited: self.count,
            dry_run,
            skipped: &self.skipped,
        }
    }

    fn print_human(&self, dry_run: bool, verb: &str, dry_verb: &str) {
        println!();
        let msg = if dry_run {
            format!("{dry_verb} {count} action reference(s)", count = self.count)
        } else {
            format!("{verb} {count} action reference(s)", count = self.count)
        };
        println!("{}", msg.bold());

        if let Some(notice) = self.skipped_notice() {
            println!("{}", notice.yellow());
        }

        if let Some(err_summary) = self.errors.summary() {
            println!("{}", err_summary.red());
        }
    }

    fn skipped_notice(&self) -> Option<String> {
        (!self.skipped.is_empty()).then(|| {
            format!(
                "Skipped {} action reference(s); edit them by hand",
                self.skipped.len()
            )
        })
    }

    /// Determine exit code, considering skipped references and `dry_run` for outdated status.
    #[must_use]
    pub fn exit_code(&self, dry_run: bool) -> i32 {
        // Errors take priority
        let base = self.errors.exit_code();
        if base != CliExitCode::Success {
            return base.into();
        }

        if !self.skipped.is_empty() || (dry_run && self.count > 0) {
            return CliExitCode::Outdated.into();
        }

        CliExitCode::Success.into()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::{Path, PathBuf};

    use actup_core::{Config, DiscoveredFile, FileKind, provider};

    use super::*;

    // ErrorTally tests

    fn not_found() -> CoreError {
        CoreError::NotFound {
            owner: "foo".into(),
            repo: "bar".into(),
            host: None,
        }
    }

    #[test]
    fn test_error_tally_default() {
        let tally = ErrorTally::default();
        assert_eq!(tally.total(), 0);
        assert_eq!(tally.exit_code(), CliExitCode::Success);
        assert!(tally.summary().is_none());
    }

    #[test]
    fn test_error_tally_user_errors() {
        let mut tally = ErrorTally::default();
        tally.record(&not_found());
        tally.record(&not_found());

        assert_eq!(tally.total(), 2);
        assert_eq!(tally.exit_code(), CliExitCode::UserError);
        assert_eq!(tally.summary(), Some("2 user error(s)".to_string()));
    }

    #[test]
    fn test_error_tally_env_errors() {
        let mut tally = ErrorTally::default();
        tally.record_env();

        assert_eq!(tally.total(), 1);
        assert_eq!(tally.exit_code(), CliExitCode::EnvironmentError);
        assert_eq!(tally.summary(), Some("1 environment error(s)".to_string()));
    }

    #[test]
    fn test_error_tally_mixed_errors() {
        let mut tally = ErrorTally::default();
        tally.record(&not_found());
        tally.record_env();

        assert_eq!(tally.total(), 2);
        // Env errors take priority
        assert_eq!(tally.exit_code(), CliExitCode::EnvironmentError);
        assert_eq!(
            tally.summary(),
            Some("1 user error(s), 1 environment error(s)".to_string())
        );
    }

    #[test]
    fn test_error_tally_record_core_error() {
        let mut tally = ErrorTally::default();

        // Network error is environment error
        let net_err = CoreError::Network {
            operation: "fetch".into(),
            message: "timeout".into(),
        };
        tally.record(&net_err);
        assert_eq!(tally.exit_code(), CliExitCode::EnvironmentError);

        // NotFound is user error
        let mut tally2 = ErrorTally::default();
        let not_found = CoreError::NotFound {
            owner: "foo".into(),
            repo: "bar".into(),
            host: None,
        };
        tally2.record(&not_found);
        assert_eq!(tally2.exit_code(), CliExitCode::UserError);
    }

    // PendingEdit tests

    fn apply_line(line: &str, edit: PendingEdit) -> Option<String> {
        let mut fe = FileEdits::new(PathBuf::from("/test"), line.to_string());
        fe.add(edit, ());
        let plan = fe.plan();
        match plan.entries.as_slice() {
            [((), EditOutcome::Applied { .. })] => Some(plan.content),
            _ => None,
        }
    }

    #[test]
    fn test_pending_edit_replace_range() {
        let edit = PendingEdit::replace_range(1, 10..14, "v2.0.0".to_string());
        let line = "  - uses: actions/checkout@v1.0.0";
        let result = apply_line(line, edit).unwrap();
        assert_eq!(result, "  - uses: v2.0.0ons/checkout@v1.0.0");
    }

    #[test]
    fn test_pending_edit_replace_range_exact() {
        // Replace just the ref portion
        let line = "  - uses: actions/checkout@v1";
        let edit = PendingEdit::replace_range(1, 27..29, "v2".to_string());
        let result = apply_line(line, edit).unwrap();
        assert_eq!(result, "  - uses: actions/checkout@v2");
    }

    #[test]
    fn test_pending_edit_replace_range_out_of_bounds() {
        let edit = PendingEdit::replace_range(1, 100..110, "new".to_string());
        let line = "short";
        assert!(apply_line(line, edit).is_none());
    }

    #[test]
    fn test_pending_edit_with_comment() {
        let edit = PendingEdit::replace_range(1, 27..29, "abc123".to_string())
            .with_comment(" # v1".to_string());
        let line = "  - uses: actions/checkout@v1";
        let result = apply_line(line, edit).unwrap();
        assert_eq!(result, "  - uses: actions/checkout@abc123 # v1");
    }

    #[test]
    fn test_pending_edit_comment_not_added_if_exists() {
        let edit = PendingEdit::replace_range(1, 27..29, "v2".to_string())
            .with_comment(" # pinned".to_string());
        let line = "  - uses: actions/checkout@v1 # existing comment";
        let result = apply_line(line, edit).unwrap();
        // Comment not added because line already has one
        assert_eq!(result, "  - uses: actions/checkout@v2 # existing comment");
    }

    // repin_sha_edit tests

    fn sha40(byte: char) -> String {
        byte.to_string().repeat(40)
    }

    fn sha_loc(line_body: &str) -> UsesLocation {
        let value = line_body
            .split("uses: ")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
        let column = line_body.find(value).unwrap() + 1;
        UsesLocation::new(1, column, value.to_string(), line_body.to_string())
    }

    #[test]
    fn test_repin_sha_adds_version_comment() {
        let old = sha40('a');
        let new = sha40('b');
        let line = format!("      - uses: actions/checkout@{old}");
        let loc = sha_loc(&line);
        let target = version::Info::new(version::Version::parse("v7.0.1"), &new);
        let result = apply_line(&line, repin_sha_edit(&loc, &target).unwrap()).unwrap();
        assert_eq!(
            result,
            format!("      - uses: actions/checkout@{new} # v7.0.1")
        );
    }

    #[test]
    fn test_repin_sha_refreshes_stale_comment() {
        let old = sha40('a');
        let new = sha40('b');
        let line = format!("      - uses: actions/checkout@{old} # v7.0.0");
        let loc = sha_loc(&line);
        let target = version::Info::new(version::Version::parse("v7.0.1"), &new);
        let result = apply_line(&line, repin_sha_edit(&loc, &target).unwrap()).unwrap();
        assert_eq!(
            result,
            format!("      - uses: actions/checkout@{new} # v7.0.1")
        );
    }

    #[test]
    fn test_repin_sha_preserves_foreign_comment() {
        let old = sha40('a');
        let new = sha40('b');
        let line = format!("      - uses: actions/checkout@{old} # managed by bot");
        let loc = sha_loc(&line);
        let target = version::Info::new(version::Version::parse("v7.0.1"), &new);
        let result = apply_line(&line, repin_sha_edit(&loc, &target).unwrap()).unwrap();
        assert_eq!(
            result,
            format!("      - uses: actions/checkout@{new} # managed by bot")
        );
    }

    #[test]
    fn test_repin_sha_skips_value_not_written_verbatim() {
        let old = sha40('a');
        let line = format!("      - uses: \"actions/checkout@\\x61{}\"", &old[1..]);
        let loc = UsesLocation::new(1, 16, format!("actions/checkout@{old}"), line);
        let target = version::Info::new(version::Version::parse("v7.0.1"), sha40('b'));
        assert!(repin_sha_edit(&loc, &target).is_none());
    }

    fn skipped_ref(reason: SkipReason) -> SkippedRef {
        SkippedRef {
            file: "ci.yml".to_string(),
            line: 3,
            action: "a/b@v4".to_string(),
            owner: "a".to_string(),
            repo: "b".to_string(),
            host: None,
            provider: "github".to_string(),
            reference: "v4".to_string(),
            reason,
        }
    }

    #[test]
    fn test_report_skipped_records_a_skip_and_no_user_error() {
        let mut result = EditResult::default();
        report_skipped(
            "ci.yml",
            3,
            &ActionRef::simple("a", "b", "v4"),
            "github",
            SkipReason::Unplaceable,
            &mut result,
        );
        assert_eq!(result.skipped, [skipped_ref(SkipReason::Unplaceable)]);
        assert_eq!(result.errors.total(), 0);
        assert_eq!(result.exit_code(false), i32::from(CliExitCode::Outdated));
    }

    #[test]
    fn test_skip_reasons_describe_the_writer_limit() {
        assert_eq!(
            SkipReason::Unplaceable.describe(),
            "`uses:` value is escaped or spans lines"
        );
        assert_eq!(SkipReason::Overlap.describe(), "edit overlaps another edit");
        assert_eq!(
            SkipReason::OutOfBounds.describe(),
            "edit does not fit its line"
        );
    }

    // FileEdits tests

    #[test]
    fn test_file_edits_empty() {
        let plan = FileEdits::<()>::new(PathBuf::from("/test"), "content".to_string()).plan();
        assert_eq!(plan.entries.len(), 0);
        assert_eq!(plan.content, "content");
    }

    #[test]
    fn test_file_edits_add_and_apply() {
        let content = "line1\nuses: foo@v1\nline3\n";
        let mut fe = FileEdits::new(PathBuf::from("/test"), content.to_string());

        fe.add(PendingEdit::replace_range(2, 10..12, "v2".to_string()), ());

        let plan = fe.plan();
        assert_eq!(plan.content, "line1\nuses: foo@v2\nline3\n");
        assert_eq!(plan.entries, [((), EditOutcome::Applied { comment: None })]);
    }

    #[test]
    fn test_file_edits_identical_ranges_overlap() {
        let mut fe = FileEdits::new(PathBuf::from("/test"), "line1\nline2\n".to_string());

        fe.add(PendingEdit::replace_range(1, 4..5, "ONE".to_string()), 1);
        fe.add(PendingEdit::replace_range(1, 4..5, "UNO".to_string()), 2);

        let plan = fe.plan();
        assert_eq!(plan.content, "lineONE\nline2\n");
        assert_eq!(
            plan.entries,
            [
                (1, EditOutcome::Applied { comment: None }),
                (2, EditOutcome::Skipped(SkipReason::Overlap)),
            ]
        );
    }

    #[test]
    fn test_file_edits_apply_every_disjoint_edit_on_one_line() {
        let content = "x: [a@1, bb@22, ccc@333]\n";
        let mut fe = FileEdits::new(PathBuf::from("/test"), content.to_string());

        fe.add(PendingEdit::replace_range(1, 20..23, "3".to_string()), 'c');
        fe.add(PendingEdit::replace_range(1, 6..7, "1111".to_string()), 'a');
        fe.add(PendingEdit::replace_range(1, 12..14, String::new()), 'b');

        let plan = fe.plan();
        assert_eq!(plan.content, "x: [a@1111, bb@, ccc@3]\n");
        assert!(
            plan.entries
                .iter()
                .all(|(_, outcome)| *outcome == EditOutcome::Applied { comment: None })
        );
    }

    #[test]
    fn test_file_edits_reject_only_overlapping_edits() {
        let content = "abcdefghijkl\n";
        let mut fe = FileEdits::new(PathBuf::from("/test"), content.to_string());

        fe.add(
            PendingEdit::replace_range(1, 2..6, "X".to_string()),
            "first",
        );
        fe.add(
            PendingEdit::replace_range(1, 4..8, "Y".to_string()),
            "straddles",
        );
        fe.add(
            PendingEdit::replace_range(1, 6..6, "Z".to_string()),
            "touches",
        );
        fe.add(
            PendingEdit::replace_range(1, 6..6, "W".to_string()),
            "same point",
        );
        fe.add(
            PendingEdit::replace_range(1, 10..12, "Q".to_string()),
            "later",
        );

        let plan = fe.plan();
        assert_eq!(plan.content, "abXZghijQ\n");
        assert_eq!(
            plan.entries,
            [
                ("first", EditOutcome::Applied { comment: None }),
                ("straddles", EditOutcome::Skipped(SkipReason::Overlap)),
                ("touches", EditOutcome::Applied { comment: None }),
                ("same point", EditOutcome::Skipped(SkipReason::Overlap)),
                ("later", EditOutcome::Applied { comment: None }),
            ]
        );
    }

    #[test]
    fn test_file_edits_write_no_comment_on_a_line_with_two_uses() {
        let content = "a: [{ uses: a/b@v1 }, { uses: c/d@v2 }]\nb:\n  - uses: e/f@v3\n";
        let mut fe = FileEdits::new(PathBuf::from("/test"), content.to_string());

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
            plan.content,
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
        let mut fe = FileEdits::new(PathBuf::from("/test"), "a@1 b@2".to_string());

        fe.add(
            PendingEdit::replace_range(1, 2..3, "X".to_string()).with_comment(" # 1".into()),
            1,
        );
        fe.add(
            PendingEdit::replace_range(1, 6..7, "Y".to_string()).with_comment(" # 2".into()),
            2,
        );

        let plan = fe.plan();
        assert_eq!(plan.content, "a@X b@Y");
        assert_eq!(
            plan.entries,
            [
                (1, EditOutcome::Applied { comment: None }),
                (2, EditOutcome::Applied { comment: None }),
            ]
        );
    }

    #[test]
    fn test_file_edits_comment_follows_an_edit_that_ends_the_line() {
        let mut fe = FileEdits::new(PathBuf::from("/test"), "uses: a@1\nnext: x\n".to_string());

        fe.add(
            PendingEdit::replace_range(1, 8..9, "2".to_string()).with_comment(" # 2".into()),
            (),
        );

        let plan = fe.plan();
        assert_eq!(plan.content, "uses: a@2 # 2\nnext: x\n");
    }

    #[test]
    fn test_file_edits_preserves_trailing_newline() {
        let with_newline = "line1\nline2\n";
        let fe = FileEdits::<()>::new(PathBuf::from("/test"), with_newline.to_string());
        assert_eq!(fe.plan().content, with_newline);

        let without_newline = "line1\nline2";
        let fe2 = FileEdits::<()>::new(PathBuf::from("/test"), without_newline.to_string());
        assert_eq!(fe2.plan().content, without_newline);
    }

    #[test]
    fn test_file_edits_apply_failure() {
        let content = "short\n";
        let mut fe = FileEdits::new(PathBuf::from("/test"), content.to_string());

        fe.add(PendingEdit::replace_range(1, 100..110, "x".to_string()), ());
        fe.add(PendingEdit::replace_range(2, 0..0, "x".to_string()), ());
        fe.add(PendingEdit::replace_range(0, 0..0, "x".to_string()), ());
        fe.add(
            PendingEdit::replace_range(1, Range { start: 3, end: 2 }, "x".to_string()),
            (),
        );

        let plan = fe.plan();
        assert_eq!(plan.content, content);
        assert!(
            plan.entries
                .iter()
                .all(|((), outcome)| *outcome == EditOutcome::Skipped(SkipReason::OutOfBounds))
        );
    }

    #[test]
    fn test_file_edits_skip_a_range_inside_a_multibyte_char() {
        let mut fe = FileEdits::new(PathBuf::from("/test"), "é@1\n".to_string());

        fe.add(PendingEdit::replace_range(1, 1..2, "x".to_string()), ());

        let plan = fe.plan();
        assert_eq!(plan.content, "é@1\n");
        assert_eq!(
            plan.entries,
            [((), EditOutcome::Skipped(SkipReason::OutOfBounds))]
        );
    }

    #[test]
    fn test_file_edits_multiple_lines() {
        let content = "  - uses: actions/checkout@v1\n  - uses: actions/setup-node@v2\n";
        let mut fe = FileEdits::new(PathBuf::from("/test"), content.to_string());

        fe.add(PendingEdit::replace_range(1, 27..29, "v4".to_string()), ());
        fe.add(PendingEdit::replace_range(2, 29..31, "v5".to_string()), ());

        assert_eq!(
            fe.plan().content,
            "  - uses: actions/checkout@v4\n  - uses: actions/setup-node@v5\n"
        );
    }

    #[test]
    fn test_line_spans_split_like_str_lines() {
        for content in [
            "",
            "a",
            "a\n",
            "a\r\nb\r\n",
            "a\n\nb",
            "a\rb\n",
            "\r\n\r\n",
            "x\r",
            "x\n\r",
        ] {
            let spans: Vec<&str> = line_spans(content)
                .into_iter()
                .map(|span| &content[span])
                .collect();
            let lines: Vec<&str> = content.lines().collect();
            assert_eq!(spans, lines, "{content:?}");
        }
    }

    fn change(line: usize, target: &str, show_comment: bool) -> Change {
        Change {
            action: ActionRef::simple("a", "b", "v1"),
            provider: "github".to_string(),
            line,
            target: target.to_string(),
            show_comment,
        }
    }

    #[test]
    fn test_change_written_shows_only_a_written_comment() {
        assert_eq!(
            change(1, "a/b@S", true).written(Some(" # v1")),
            "a/b@S # v1"
        );
        assert_eq!(change(1, "a/b@S", true).written(None), "a/b@S");
        assert_eq!(
            change(1, "v1.0.0 (abc1234)", false).written(Some(" # v1.0.0")),
            "v1.0.0 (abc1234)"
        );
    }

    #[test]
    fn test_commit_file_writes_and_counts_applied_edits() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        let content = "uses: a/b@v1\n";
        fs::write(&path, content)?;
        let mut fe = FileEdits::new(path.clone(), content.to_string());
        fe.add(
            PendingEdit::replace_range(1, 10..12, "v2".to_string()).with_comment(" # c".into()),
            change(1, "v2", true),
        );
        fe.add(
            PendingEdit::replace_range(1, 11..12, "3".to_string()),
            change(1, "v3", false),
        );

        let mut result = EditResult::default();
        let written = commit_file(fe, "ci.yml", false, &mut result);

        assert_eq!(written.len(), 1);
        assert_eq!(written[0].1.as_deref(), Some(" # c"));
        assert_eq!(result.count, 1);
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].reason, SkipReason::Overlap);
        assert_eq!(result.errors.total(), 0);
        assert_eq!(fs::read_to_string(&path)?, "uses: a/b@v2 # c\n");
        Ok(())
    }

    #[test]
    fn test_commit_file_dry_run_leaves_the_file() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        let content = "uses: a/b@v1\n";
        fs::write(&path, content)?;
        let mut fe = FileEdits::new(path.clone(), content.to_string());
        fe.add(
            PendingEdit::replace_range(1, 10..12, "v2".to_string()),
            change(1, "v2", false),
        );

        let mut result = EditResult::default();
        let written = commit_file(fe, "ci.yml", true, &mut result);

        assert_eq!(written.len(), 1);
        assert_eq!(result.count, 1);
        assert_eq!(fs::read_to_string(&path)?, content);
        Ok(())
    }

    #[test]
    fn test_commit_file_write_failure_counts_nothing() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("missing").join("ci.yml");
        let mut fe = FileEdits::new(path.clone(), "uses: a/b@v1\n".to_string());
        fe.add(
            PendingEdit::replace_range(1, 10..12, "v2".to_string()),
            change(1, "v2", false),
        );

        let mut result = EditResult::default();
        let written = commit_file(fe, "ci.yml", false, &mut result);

        assert!(written.is_empty());
        assert_eq!(result.count, 0);
        assert_eq!(result.errors.exit_code(), CliExitCode::EnvironmentError);
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn test_workflow_scanner_relative_path_for_directory_input() {
        let config = Config::default();
        let registry = provider::Registry::new();
        let root = Path::new("/repo");
        let scanner = WorkflowScanner::new(root, &config, &registry, OutputFormat::Human);
        let file = DiscoveredFile::new(
            PathBuf::from("/repo/.github/workflows/release.yml"),
            FileKind::Workflow,
        );

        assert_eq!(
            scanner.relative_path(&file),
            ".github/workflows/release.yml"
        );
    }

    #[test]
    fn test_workflow_scanner_relative_path_for_file_input() {
        let temp = tempfile::tempdir().unwrap();
        let workflows_dir = temp.path().join(".github/workflows");
        fs::create_dir_all(&workflows_dir).unwrap();
        let path = workflows_dir.join("release.yml");
        fs::write(&path, "name: test\n").unwrap();
        assert!(path.is_file());

        let config = Config::default();
        let registry = provider::Registry::new();
        let scanner = WorkflowScanner::new(&path, &config, &registry, OutputFormat::Human);
        let file = DiscoveredFile::new(path.clone(), FileKind::Workflow);

        assert_eq!(scanner.relative_path(&file), path.display().to_string());
    }

    // EditResult tests

    #[test]
    fn test_edit_result_exit_code_success() {
        let result = EditResult::default();
        assert_eq!(result.exit_code(false), 0);
        assert_eq!(result.exit_code(true), 0);
        assert!(result.skipped_notice().is_none());
    }

    #[test]
    fn test_edit_result_skipped_refs_exit_outdated() {
        let result = EditResult {
            count: 2,
            skipped: vec![skipped_ref(SkipReason::Unplaceable)],
            errors: ErrorTally::default(),
        };
        assert_eq!(result.exit_code(false), i32::from(CliExitCode::Outdated));
        assert_eq!(result.exit_code(true), i32::from(CliExitCode::Outdated));
        assert_eq!(
            result.skipped_notice().as_deref(),
            Some("Skipped 1 action reference(s); edit them by hand")
        );
    }

    #[test]
    fn test_edit_report_lists_skipped_refs() -> Result<(), Box<dyn std::error::Error>> {
        let result = EditResult {
            count: 2,
            skipped: vec![skipped_ref(SkipReason::Unplaceable)],
            errors: ErrorTally::default(),
        };
        let expected = serde_json::json!({
            "edited": 2,
            "dry_run": true,
            "skipped": [{
                "file": "ci.yml",
                "line": 3,
                "action": "a/b@v4",
                "owner": "a",
                "repo": "b",
                "provider": "github",
                "reference": "v4",
                "reason": "unplaceable",
            }],
        });
        assert_eq!(serde_json::to_value(result.report(true))?, expected);
        assert_eq!(
            toml::to_string_pretty(&result.report(false))?,
            "edited = 2\ndry_run = false\n\n[[skipped]]\nfile = \"ci.yml\"\nline = 3\naction = \"a/b@v4\"\nowner = \"a\"\nrepo = \"b\"\nprovider = \"github\"\nreference = \"v4\"\nreason = \"unplaceable\"\n"
        );
        for format in [OutputFormat::Human, OutputFormat::Json, OutputFormat::Toml] {
            result.print_summary(false, "Updated", "Would update", format);
        }
        Ok(())
    }

    #[test]
    fn test_edit_result_errors_outrank_skipped_refs() {
        let mut errors = ErrorTally::default();
        errors.record(&not_found());
        let result = EditResult {
            count: 0,
            skipped: vec![skipped_ref(SkipReason::Overlap)],
            errors,
        };
        assert_eq!(result.exit_code(false), i32::from(CliExitCode::UserError));
    }

    #[test]
    fn test_edit_result_exit_code_dry_run_with_changes() {
        let result = EditResult {
            count: 5,
            ..EditResult::default()
        };
        assert_eq!(result.exit_code(false), 0); // Not dry run
        assert_eq!(result.exit_code(true), 1); // Dry run = outdated
    }

    #[test]
    fn test_edit_result_exit_code_errors_priority() {
        let mut errors = ErrorTally::default();
        errors.record_env();

        let result = EditResult {
            count: 5,
            skipped: Vec::new(),
            errors,
        };
        // Env error takes priority over changes
        assert_eq!(result.exit_code(true), 3);
    }
}
