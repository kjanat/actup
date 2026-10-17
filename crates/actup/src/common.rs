//! Shared infrastructure for workflow scanning commands.
//!
//! This module provides the common pipeline used by check, update, and pin commands:
//! discover -> read -> extract uses -> filter -> provider lookup -> (command-specific logic)

use std::{
    collections::{BTreeSet, HashMap},
    fs,
    ops::Range,
    path::{Path, PathBuf},
};

use actup_core::{
    ActionRef, Config, DiscoveredFile, EditValidationError, Error as CoreError, Provider,
    UsesLocation, discover_files, extract_uses_from_yaml, line_spans, lines_ending_in_scalars,
    parse_uses, provider,
    rewrite::{Trailing, is_version_comment},
    rewrite_atomic, validate_edit, version,
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

    /// Record a user error that has no [`CoreError`].
    pub const fn record_user(&mut self) {
        self.user += 1;
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

/// Build an edit that pins a ref to the `target` commit SHA.
///
/// Keeps a `# vX.Y.Z` version comment accurate: a stale version comment is
/// refreshed, a missing one is added, and a foreign comment is left
/// untouched. Shared by `update` and `pin`.
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
        .with_inline_comment(version_comment)
    } else {
        // `with_comment` adds the version note only when the line has none, so a
        // foreign comment is preserved.
        PendingEdit::replace_range(loc.line(), range, new_sha.to_string())
            .with_comment(version_comment)
    })
}

/// Build an edit that writes the tag `tag` over a SHA pin and drops a trailing version comment.
pub fn unpin_edit(loc: &UsesLocation, tag: &str) -> Option<PendingEdit> {
    let range = loc.ref_range()?;
    let raw = loc.raw_line();
    let end = match Trailing::after(raw, range.end) {
        Trailing::Version => raw.len(),
        Trailing::None | Trailing::Foreign => range.end,
    };
    Some(PendingEdit::replace_range(
        loc.line(),
        range.start..end,
        tag.to_string(),
    ))
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

/// Where an action reference sits and which provider serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefSite {
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
    /// Host of the provider serving the reference.
    pub host: String,
    /// Provider kind serving the reference.
    pub provider: String,
    /// The ref after `@` as found.
    pub reference: String,
}

impl RefSite {
    /// Describe `action_ref` at `file:line`, served by `provider`.
    #[must_use]
    pub fn new(file: &str, line: usize, action_ref: &ActionRef, provider: &dyn Provider) -> Self {
        Self {
            file: file.to_string(),
            line,
            action: action_ref.to_string(),
            owner: action_ref.owner().to_string(),
            repo: action_ref.repo().to_string(),
            host: provider.host().to_string(),
            provider: provider.kind().to_string(),
            reference: action_ref.ref_str().to_string(),
        }
    }
}

/// Record that no provider serves `action_ref`, with provider `unknown`.
pub fn report_unserved(
    scanner: &WorkflowScanner<'_>,
    file: &str,
    line: usize,
    action_ref: &ActionRef,
    result: &mut EditResult,
) {
    let host = scanner.host_for(action_ref);
    let site = RefSite {
        file: file.to_string(),
        line,
        action: action_ref.to_string(),
        owner: action_ref.owner().to_string(),
        repo: action_ref.repo().to_string(),
        host: host.clone(),
        provider: "unknown".to_string(),
        reference: action_ref.ref_str().to_string(),
    };
    report_error(site, &CoreError::ProviderNotFound { host }, result);
}

/// An action reference that `update` or `pin` left unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedRef {
    /// The reference and its provider.
    #[serde(flatten)]
    pub site: RefSite,
    /// Why the writer left it unchanged.
    pub reason: SkipReason,
}

/// An action reference that `update` or `pin` changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EditedRef {
    /// The reference and its provider.
    #[serde(flatten)]
    pub site: RefSite,
    /// The ref after `@` as written.
    pub to: String,
    /// The version comment written on the line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// The kind of error that left an action reference unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The `uses:` reference is malformed.
    InvalidRef,
    /// The repository does not exist.
    NotFound,
    /// The ref does not exist in the repository.
    RefNotFound,
    /// The configuration is invalid.
    InvalidConfig,
    /// YAML parsing failed.
    YamlParse,
    /// A network request failed.
    Network,
    /// The provider rate limited the request.
    RateLimited,
    /// The provider denied access.
    Forbidden,
    /// Authentication failed or is missing.
    Auth,
    /// A cache operation failed.
    Cache,
    /// No provider serves the host.
    ProviderNotFound,
    /// An internal error occurred.
    Internal,
    /// Writing the file failed.
    Write,
    /// The file changed between the scan and the write.
    Changed,
    /// The edited file fails YAML validation.
    InvalidEdit,
}

impl From<&CoreError> for ErrorKind {
    fn from(err: &CoreError) -> Self {
        match err {
            CoreError::InvalidRef { .. } => Self::InvalidRef,
            CoreError::NotFound { .. } => Self::NotFound,
            CoreError::RefNotFound { .. } => Self::RefNotFound,
            CoreError::InvalidConfig { .. } => Self::InvalidConfig,
            CoreError::YamlParse { .. } => Self::YamlParse,
            CoreError::Network { .. } => Self::Network,
            CoreError::RateLimited { .. } => Self::RateLimited,
            CoreError::Forbidden { .. } => Self::Forbidden,
            CoreError::Auth { .. } => Self::Auth,
            CoreError::Cache { .. } => Self::Cache,
            CoreError::ProviderNotFound { .. } => Self::ProviderNotFound,
            CoreError::Internal { .. } => Self::Internal,
        }
    }
}

/// An action reference that `update` or `pin` left unchanged after an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErroredRef {
    /// The reference and its provider.
    #[serde(flatten)]
    pub site: RefSite,
    /// The kind of error.
    pub kind: ErrorKind,
    /// The error message.
    pub message: String,
}

/// Warn that the writer leaves `site` unchanged and record it as skipped.
pub fn report_skipped(site: RefSite, reason: SkipReason, result: &mut EditResult) {
    eprintln!(
        "{}: {}:{}: {} - {}; edit it by hand",
        "Warning".yellow(),
        site.file,
        site.line,
        site.action,
        reason.describe()
    );
    result.skipped.push(SkippedRef { site, reason });
}

/// Warn that `err` leaves `site` unchanged, count it, and record it as errored.
pub fn report_error(site: RefSite, err: &CoreError, result: &mut EditResult) {
    eprintln!("{}: {} - {err}", "Warning".yellow(), site.action);
    result.errors.record(err);
    result.errored.push(ErroredRef {
        site,
        kind: ErrorKind::from(err),
        message: err.to_string(),
    });
}

// FileEdits - accumulated edits for a single file

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
    uses_per_line: HashMap<usize, usize>,
    lines_in_scalars: BTreeSet<usize>,
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

    /// The scanned text with every splice applied.
    #[must_use]
    pub fn planned(&self) -> String {
        splice(&self.scanned, &self.edits)
    }
}

#[cfg(test)]
impl<L> EditPlan<L> {
    pub(crate) fn content(&self) -> String {
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
    /// Create a new `FileEdits` for a file.
    #[must_use]
    pub fn new(path: PathBuf, content: String) -> Self {
        let mut uses_per_line = HashMap::new();
        for loc in extract_uses_from_yaml(&content) {
            *uses_per_line.entry(loc.line()).or_insert(0) += 1;
        }
        Self {
            path,
            lines_in_scalars: lines_ending_in_scalars(&content),
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
    /// `#`, no other applied edit with a comment, and ends outside a scalar.
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
            outcomes[i] = EditOutcome::Applied {
                comment: self.edits[i].0.inline_comment.clone(),
            };
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
            let ends_outside_scalar = !self.lines_in_scalars.contains(&edit.line);
            if sole_uses && sole_comment && uncommented && ends_outside_scalar {
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

/// One action reference change, for progress output and the report.
#[derive(Debug, Clone)]
pub struct Change {
    /// The reference and its provider.
    pub site: RefSite,
    /// The ref after `@` as written.
    pub new_ref: String,
    /// The written target for progress output, without any appended comment.
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
/// The write re-reads the file and places the edits on it. A rewritten line
/// that differs from the scan refuses the whole file, and so does an edited
/// text that fails [`validate_edit`], in a dry run too. Returns each written
/// change with its version comment, and records them in `result`. A refused or
/// failed write records each change as an errored ref and returns nothing.
pub fn commit_file(
    file_edits: FileEdits<Change>,
    dry_run: bool,
    result: &mut EditResult,
) -> Vec<(Change, Option<String>)> {
    let EditPlan {
        path,
        entries,
        splices,
    } = file_edits.plan();
    let mut written = Vec::new();
    for (change, outcome) in entries {
        match outcome {
            EditOutcome::Applied { comment } => written.push((change, comment)),
            EditOutcome::Skipped(reason) => report_skipped(change.site, reason, result),
        }
    }
    if written.is_empty() {
        return written;
    }
    let outcome = if dry_run {
        validate_edit(&splices.scanned, &splices.planned()).map_err(WriteRefusal::Invalid)
    } else {
        write_splices(&path, &splices)
    };
    if let Err(refusal) = outcome {
        let message = refusal.message(&path);
        eprintln!("{}: {message}", "Error".red());
        for (change, _) in written {
            if matches!(refusal, WriteRefusal::Invalid(_)) {
                result.errors.record_user();
            } else {
                result.errors.record_env();
            }
            result.errored.push(ErroredRef {
                site: change.site,
                kind: refusal.kind(),
                message: message.clone(),
            });
        }
        return Vec::new();
    }
    result
        .edited
        .extend(written.iter().map(|(change, comment)| EditedRef {
            site: change.site.clone(),
            to: change.new_ref.clone(),
            comment: comment.as_deref().map(|c| c.trim_start().to_string()),
        }));
    written
}

enum WriteRefusal {
    Io(std::io::Error),
    Changed,
    Invalid(EditValidationError),
}

impl WriteRefusal {
    const fn kind(&self) -> ErrorKind {
        match self {
            Self::Io(_) => ErrorKind::Write,
            Self::Changed => ErrorKind::Changed,
            Self::Invalid(_) => ErrorKind::InvalidEdit,
        }
    }

    fn message(&self, path: &Path) -> String {
        match self {
            Self::Io(e) => format!("writing {} failed: {e}", path.display()),
            Self::Changed => format!("edit refused: {} changed since scan", path.display()),
            Self::Invalid(e) => format!("edit refused: {} after edit: {e}", path.display()),
        }
    }
}

impl From<std::io::Error> for WriteRefusal {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

fn write_splices(path: &Path, splices: &Splices) -> Result<(), WriteRefusal> {
    rewrite_atomic(path, |current| {
        let content = splices.apply(current).ok_or(WriteRefusal::Changed)?;
        validate_edit(current, &content).map_err(WriteRefusal::Invalid)?;
        Ok(content)
    })
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

    /// The host an action resolves to: its written host, else the default host.
    #[must_use]
    pub fn host_for(&self, action: &ActionRef) -> String {
        action
            .host()
            .or_else(|| self.registry.default_host())
            .unwrap_or("github.com")
            .to_string()
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
    /// Whether the run left files unwritten.
    pub dry_run: bool,
    /// Action references changed, or to be changed in a dry run.
    pub edited: &'a [EditedRef],
    /// Action references left unchanged by a writer limitation.
    pub skipped: &'a [SkippedRef],
    /// Action references left unchanged by an error.
    pub errors: &'a [ErroredRef],
}

/// Summary of edit operations.
#[derive(Default)]
pub struct EditResult {
    /// Action references changed, or to be changed in a dry run.
    pub edited: Vec<EditedRef>,
    /// Action references left unchanged by a writer limitation.
    pub skipped: Vec<SkippedRef>,
    /// Action references left unchanged by an error.
    pub errored: Vec<ErroredRef>,
    /// Error tally.
    pub errors: ErrorTally,
}

impl EditResult {
    /// Print the summary: text for human output, an [`EditReport`] for JSON and TOML.
    pub fn print_summary(&self, dry_run: bool, verb: &str, dry_verb: &str, format: OutputFormat) {
        match format {
            OutputFormat::Human => self.print_human(dry_run, verb, dry_verb),
            OutputFormat::Json | OutputFormat::Toml => self.print_report(dry_run, format),
        }
    }

    /// Print the [`EditReport`] for JSON and TOML output; print nothing for human output.
    pub fn print_report(&self, dry_run: bool, format: OutputFormat) {
        let text = match format {
            OutputFormat::Human => return,
            OutputFormat::Json => {
                serde_json::to_string_pretty(&self.report(dry_run)).map_err(|e| e.to_string())
            }
            OutputFormat::Toml => {
                toml::to_string_pretty(&self.report(dry_run)).map_err(|e| e.to_string())
            }
        };
        match text {
            Ok(text) => println!("{text}"),
            Err(e) => eprintln!("Error serializing edit report: {e}"),
        }
    }

    /// The structured summary of this run.
    #[must_use]
    pub fn report(&self, dry_run: bool) -> EditReport<'_> {
        EditReport {
            dry_run,
            edited: &self.edited,
            skipped: &self.skipped,
            errors: &self.errored,
        }
    }

    fn print_human(&self, dry_run: bool, verb: &str, dry_verb: &str) {
        println!();
        let count = self.edited.len();
        let msg = if dry_run {
            format!("{dry_verb} {count} action reference(s)")
        } else {
            format!("{verb} {count} action reference(s)")
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

        if !self.skipped.is_empty() || (dry_run && !self.edited.is_empty()) {
            return CliExitCode::Outdated.into();
        }

        CliExitCode::Success.into()
    }
}

#[cfg(test)]
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
    fn test_repin_sha_skips_value_not_written_verbatim() {
        let old = sha40('a');
        let line = format!("      - uses: \"actions/checkout@\\x61{}\"", &old[1..]);
        let loc = UsesLocation::new(1, 16, format!("actions/checkout@{old}"), line);
        let target = version::Info::new(version::Version::parse("v7.0.1"), sha40('b'));
        assert!(repin_sha_edit(&loc, &target).is_none());
    }

    fn site(line: usize) -> RefSite {
        RefSite {
            file: "ci.yml".to_string(),
            line,
            action: "a/b@v4".to_string(),
            owner: "a".to_string(),
            repo: "b".to_string(),
            host: "github.com".to_string(),
            provider: "github".to_string(),
            reference: "v4".to_string(),
        }
    }

    fn skipped_ref(reason: SkipReason) -> SkippedRef {
        SkippedRef {
            site: site(3),
            reason,
        }
    }

    fn edited_ref(line: usize, comment: Option<&str>) -> EditedRef {
        EditedRef {
            site: site(line),
            to: "v5".to_string(),
            comment: comment.map(String::from),
        }
    }

    #[test]
    fn test_report_skipped_records_a_skip_and_no_user_error() {
        let mut result = EditResult::default();
        report_skipped(site(3), SkipReason::Unplaceable, &mut result);
        assert_eq!(result.skipped, [skipped_ref(SkipReason::Unplaceable)]);
        assert_eq!(result.errors.total(), 0);
        assert_eq!(result.exit_code(false), i32::from(CliExitCode::Outdated));
    }

    #[test]
    fn test_report_error_records_the_kind_and_counts_the_error() {
        let mut result = EditResult::default();
        report_error(site(3), &not_found(), &mut result);
        report_error(
            site(4),
            &CoreError::RateLimited {
                provider: "github".into(),
                retry_after: None,
            },
            &mut result,
        );
        let kinds: Vec<(usize, ErrorKind)> = result
            .errored
            .iter()
            .map(|errored| (errored.site.line, errored.kind))
            .collect();
        assert_eq!(
            kinds,
            [(3, ErrorKind::NotFound), (4, ErrorKind::RateLimited)]
        );
        assert_eq!(result.errored[0].message, not_found().to_string());
        assert_eq!(result.errors.exit_code(), CliExitCode::EnvironmentError);
    }

    #[test]
    fn test_error_kind_names_every_core_error() -> Result<(), Box<dyn std::error::Error>> {
        let s = String::new;
        let errors = [
            CoreError::InvalidRef {
                input: s(),
                reason: s(),
            },
            not_found(),
            CoreError::RefNotFound {
                owner: s(),
                repo: s(),
                ref_name: s(),
                host: None,
            },
            CoreError::InvalidConfig {
                path: s(),
                reason: s(),
            },
            CoreError::YamlParse {
                path: s(),
                reason: s(),
            },
            CoreError::Network {
                operation: s(),
                message: s(),
            },
            CoreError::RateLimited {
                provider: s(),
                retry_after: None,
            },
            CoreError::Forbidden {
                provider: s(),
                reason: s(),
            },
            CoreError::Auth {
                provider: s(),
                reason: s(),
            },
            CoreError::Cache {
                operation: s(),
                reason: s(),
            },
            CoreError::ProviderNotFound { host: s() },
            CoreError::Internal {
                context: s(),
                message: s(),
            },
        ];
        let names: Vec<serde_json::Value> = errors
            .iter()
            .map(|e| serde_json::to_value(ErrorKind::from(e)))
            .collect::<Result<_, _>>()?;
        assert_eq!(
            names,
            [
                "invalid_ref",
                "not_found",
                "ref_not_found",
                "invalid_config",
                "yaml_parse",
                "network",
                "rate_limited",
                "forbidden",
                "auth",
                "cache",
                "provider_not_found",
                "internal",
            ]
        );
        assert_eq!(serde_json::to_value(ErrorKind::Write)?, "write");
        Ok(())
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
        assert_eq!(plan.content(), "content");
    }

    #[test]
    fn test_file_edits_add_and_apply() {
        let content = "line1\nuses: foo@v1\nline3\n";
        let mut fe = FileEdits::new(PathBuf::from("/test"), content.to_string());

        fe.add(PendingEdit::replace_range(2, 10..12, "v2".to_string()), ());

        let plan = fe.plan();
        assert_eq!(plan.content(), "line1\nuses: foo@v2\nline3\n");
        assert_eq!(plan.entries, [((), EditOutcome::Applied { comment: None })]);
    }

    #[test]
    fn test_file_edits_identical_ranges_overlap() {
        let mut fe = FileEdits::new(PathBuf::from("/test"), "line1\nline2\n".to_string());

        fe.add(PendingEdit::replace_range(1, 4..5, "ONE".to_string()), 1);
        fe.add(PendingEdit::replace_range(1, 4..5, "UNO".to_string()), 2);

        let plan = fe.plan();
        assert_eq!(plan.content(), "lineONE\nline2\n");
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
        assert_eq!(plan.content(), "x: [a@1111, bb@, ccc@3]\n");
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
        assert_eq!(plan.content(), "abXZghijQ\n");
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
            let mut fe = FileEdits::new(PathBuf::from("/test"), content.to_string());
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
            let refs: Vec<String> = extract_uses_from_yaml(&plan.content())
                .iter()
                .map(|loc| loc.value().to_string())
                .collect();
            assert_eq!(refs, ["a/b@v2"], "{content:?}");
        }
    }

    #[test]
    fn test_file_edits_comment_follows_an_edit_that_ends_the_line() {
        let mut fe = FileEdits::new(PathBuf::from("/test"), "uses: a@1\nnext: x\n".to_string());

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
        let fe = FileEdits::<()>::new(PathBuf::from("/test"), with_newline.to_string());
        assert_eq!(fe.plan().content(), with_newline);

        let without_newline = "line1\nline2";
        let fe2 = FileEdits::<()>::new(PathBuf::from("/test"), without_newline.to_string());
        assert_eq!(fe2.plan().content(), without_newline);
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
        assert_eq!(plan.content(), content);
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
        assert_eq!(plan.content(), "é@1\n");
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
            fe.plan().content(),
            "  - uses: actions/checkout@v4\n  - uses: actions/setup-node@v5\n"
        );
    }

    #[test]
    fn test_file_edits_keep_every_line_break_and_the_bom() {
        let content =
            "\u{feff}uses: a/b@v1\r\nx:\r  - uses: c/d@v2\r\n  - uses: e/f@v3\n  - uses: g/h@v4";
        let mut fe = FileEdits::new(PathBuf::from("/test"), content.to_string());
        for loc in extract_uses_from_yaml(content) {
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

    fn change(line: usize, target: &str, show_comment: bool) -> Change {
        Change {
            site: site(line),
            new_ref: "v5".to_string(),
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
        let written = commit_file(fe, false, &mut result);

        assert_eq!(written.len(), 1);
        assert_eq!(written[0].1.as_deref(), Some(" # c"));
        assert_eq!(result.edited, [edited_ref(1, Some("# c"))]);
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].reason, SkipReason::Overlap);
        assert_eq!(result.errors.total(), 0);
        assert_eq!(fs::read_to_string(&path)?, "uses: a/b@v2 # c\n");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_commit_file_keeps_mode_and_bom() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        let content = "\u{feff}uses: a/b@v1\n";
        fs::write(&path, content)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let mut fe = FileEdits::new(path.clone(), content.to_string());
        fe.add(
            PendingEdit::replace_range(1, 13..15, "v2".to_string()),
            change(1, "v2", false),
        );

        let mut result = EditResult::default();
        let written = commit_file(fe, false, &mut result);

        assert_eq!(written.len(), 1);
        assert_eq!(fs::read(&path)?, "\u{feff}uses: a/b@v2\n".as_bytes());
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o7777, 0o600);
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
        let written = commit_file(fe, true, &mut result);

        assert_eq!(written.len(), 1);
        assert_eq!(result.edited, [edited_ref(1, None)]);
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
        let written = commit_file(fe, false, &mut result);

        assert!(written.is_empty());
        assert_eq!(result.edited, []);
        let [errored] = result.errored.as_slice() else {
            panic!("expected one errored ref: {:?}", result.errored);
        };
        assert_eq!((errored.site.line, errored.kind), (1, ErrorKind::Write));
        assert_eq!(result.errors.exit_code(), CliExitCode::EnvironmentError);
        assert!(!path.exists());
        Ok(())
    }

    fn plan_v2(content: &str) -> EditPlan<()> {
        let mut fe = FileEdits::new(PathBuf::from("/test"), content.to_string());
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

    #[test]
    fn test_commit_file_refuses_a_file_changed_since_scan() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        let scanned = "uses: a/b@v1\n";
        let changed = "uses: a/b@main\n";
        fs::write(&path, changed)?;
        let mut fe = FileEdits::new(path.clone(), scanned.to_string());
        fe.add(
            PendingEdit::replace_range(1, 10..12, "v2".to_string()),
            change(1, "v2", false),
        );

        let mut result = EditResult::default();
        let written = commit_file(fe, false, &mut result);

        assert!(written.is_empty());
        assert_eq!(result.edited, []);
        let [errored] = result.errored.as_slice() else {
            panic!("expected one errored ref: {:?}", result.errored);
        };
        assert_eq!(errored.kind, ErrorKind::Changed);
        assert_eq!(
            errored.message,
            format!("edit refused: {} changed since scan", path.display())
        );
        assert_eq!(result.errors.exit_code(), CliExitCode::EnvironmentError);
        assert_eq!(fs::read_to_string(&path)?, changed);
        Ok(())
    }

    #[test]
    fn test_commit_file_keeps_changes_made_elsewhere_since_scan()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        let scanned = "uses: a/b@v1\nname: a\n";
        fs::write(&path, "uses: a/b@v1\nname: renamed\n")?;
        let mut fe = FileEdits::new(path.clone(), scanned.to_string());
        fe.add(
            PendingEdit::replace_range(1, 10..12, "v2".to_string()),
            change(1, "v2", false),
        );

        let mut result = EditResult::default();
        let written = commit_file(fe, false, &mut result);

        assert_eq!(written.len(), 1);
        assert_eq!(result.errors.total(), 0);
        assert_eq!(fs::read_to_string(&path)?, "uses: a/b@v2\nname: renamed\n");
        Ok(())
    }

    fn commit_injected(
        dir: &Path,
        content: &str,
        new: &str,
        dry_run: bool,
    ) -> Result<(PathBuf, EditResult, usize), Box<dyn std::error::Error>> {
        let path = dir.join("ci.yml");
        fs::write(&path, content)?;
        let mut fe = FileEdits::new(path.clone(), content.to_string());
        fe.add(
            PendingEdit::replace_range(2, 14..16, new.to_string()),
            change(2, "v2", false),
        );
        let mut result = EditResult::default();
        let written = commit_file(fe, dry_run, &mut result).len();
        Ok((path, result, written))
    }

    #[test]
    fn test_commit_file_refuses_an_edit_that_breaks_the_yaml()
    -> Result<(), Box<dyn std::error::Error>> {
        let content = "steps:\n  - uses: a/b@v1\n";
        for dry_run in [false, true] {
            let dir = tempfile::tempdir()?;
            let (path, result, written) =
                commit_injected(dir.path(), content, "v2\n  - [", dry_run)?;

            assert_eq!(written, 0);
            assert_eq!(result.edited, []);
            let [errored] = result.errored.as_slice() else {
                panic!("expected one errored ref: {:?}", result.errored);
            };
            assert_eq!(errored.kind, ErrorKind::InvalidEdit);
            let prefix = format!(
                "edit refused: {} after edit: invalid YAML: ",
                path.display()
            );
            assert!(errored.message.starts_with(&prefix), "{}", errored.message);
            assert_eq!(result.errors.exit_code(), CliExitCode::UserError);
            assert_eq!(fs::read_to_string(&path)?, content);
        }
        Ok(())
    }

    #[test]
    fn test_commit_file_refuses_an_edit_that_changes_the_uses_count()
    -> Result<(), Box<dyn std::error::Error>> {
        let content = "steps:\n  - uses: a/b@v1\n";
        let dir = tempfile::tempdir()?;
        let (path, result, written) =
            commit_injected(dir.path(), content, "v2\n  - uses: c/d@v2", false)?;

        assert_eq!(written, 0);
        let messages: Vec<&str> = result.errored.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(
            messages,
            [format!(
                "edit refused: {} after edit: `uses:` count changed from 1 to 2",
                path.display()
            )]
        );
        assert_eq!(fs::read_to_string(&path)?, content);
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
    fn test_workflow_scanner_relative_path_for_file_input() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = tempfile::tempdir()?;
        let workflows_dir = temp.path().join(".github/workflows");
        fs::create_dir_all(&workflows_dir)?;
        let path = workflows_dir.join("release.yml");
        fs::write(&path, "name: test\n")?;
        assert!(path.is_file());

        let config = Config::default();
        let registry = provider::Registry::new();
        let scanner = WorkflowScanner::new(&path, &config, &registry, OutputFormat::Human);
        let file = DiscoveredFile::new(path.clone(), FileKind::Workflow);

        assert_eq!(scanner.relative_path(&file), path.display().to_string());
        Ok(())
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
            edited: vec![edited_ref(1, None), edited_ref(2, None)],
            skipped: vec![skipped_ref(SkipReason::Unplaceable)],
            ..EditResult::default()
        };
        assert_eq!(result.exit_code(false), i32::from(CliExitCode::Outdated));
        assert_eq!(result.exit_code(true), i32::from(CliExitCode::Outdated));
        assert_eq!(
            result.skipped_notice().as_deref(),
            Some("Skipped 1 action reference(s); edit them by hand")
        );
    }

    #[test]
    fn test_edit_report_lists_every_ref() -> Result<(), Box<dyn std::error::Error>> {
        let mut result = EditResult {
            edited: vec![edited_ref(1, Some("# v5"))],
            skipped: vec![skipped_ref(SkipReason::Unplaceable)],
            ..EditResult::default()
        };
        report_error(site(5), &not_found(), &mut result);
        let site_json = |line: usize| {
            serde_json::json!({
                "file": "ci.yml",
                "line": line,
                "action": "a/b@v4",
                "owner": "a",
                "repo": "b",
                "host": "github.com",
                "provider": "github",
                "reference": "v4",
            })
        };
        let with = |line: usize, extra: serde_json::Value| {
            let mut entry = site_json(line);
            if let (Some(entry), Some(extra)) = (entry.as_object_mut(), extra.as_object()) {
                entry.extend(extra.clone());
            }
            entry
        };
        let expected = serde_json::json!({
            "dry_run": true,
            "edited": [with(1, serde_json::json!({ "to": "v5", "comment": "# v5" }))],
            "skipped": [with(3, serde_json::json!({ "reason": "unplaceable" }))],
            "errors": [with(5, serde_json::json!({
                "kind": "not_found",
                "message": not_found().to_string(),
            }))],
        });
        assert_eq!(serde_json::to_value(result.report(true))?, expected);
        let toml_text = toml::to_string_pretty(&result.report(false))?;
        let parsed: toml::Value = toml::from_str(&toml_text)?;
        let mut expected_toml = serde_json::to_value(result.report(true))?;
        expected_toml["dry_run"] = serde_json::json!(false);
        assert_eq!(serde_json::to_value(parsed)?, expected_toml);
        for format in [OutputFormat::Human, OutputFormat::Json, OutputFormat::Toml] {
            result.print_summary(false, "Updated", "Would update", format);
        }
        Ok(())
    }

    #[test]
    fn test_empty_edit_report_lists_no_refs() -> Result<(), Box<dyn std::error::Error>> {
        let result = EditResult::default();
        assert_eq!(
            serde_json::to_value(result.report(false))?,
            serde_json::json!({ "dry_run": false, "edited": [], "skipped": [], "errors": [] })
        );
        assert_eq!(
            toml::to_string_pretty(&result.report(true))?,
            "dry_run = true\nedited = []\nskipped = []\nerrors = []\n"
        );
        Ok(())
    }

    #[test]
    fn test_edit_result_errors_outrank_skipped_refs() {
        let mut errors = ErrorTally::default();
        errors.record(&not_found());
        let result = EditResult {
            skipped: vec![skipped_ref(SkipReason::Overlap)],
            errors,
            ..EditResult::default()
        };
        assert_eq!(result.exit_code(false), i32::from(CliExitCode::UserError));
    }

    #[test]
    fn test_edit_result_exit_code_dry_run_with_changes() {
        let result = EditResult {
            edited: vec![edited_ref(1, None)],
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
            edited: vec![edited_ref(1, None)],
            errors,
            ..EditResult::default()
        };
        // Env error takes priority over changes
        assert_eq!(result.exit_code(true), 3);
    }
}
