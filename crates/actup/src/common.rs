//! Shared infrastructure for workflow scanning commands.
//!
//! This module provides the common pipeline used by check, update, and pin commands:
//! discover -> read -> extract uses -> filter -> provider lookup -> (command-specific logic)

use anstream::{eprintln, println};
use std::{fs, path::Path};

use actup_core::{
    ActionRef, Config, DiscoveredFile, EditValidationError, Error as CoreError, Provider,
    SourceFormat, UsesLocation,
    cache::Staleness,
    discover_files, parse_uses,
    patch::{EditOutcome, EditPlan, FileEdits, SkipReason, Splices},
    provider, resolve_within, rewrite_atomic, sanitize_line, version,
};
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::{
    cli::{CliExitCode, OutputFormat},
    scan::{ScannedFile, ScannedRef},
};

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

/// Why `update` or `pin` left an action reference unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipCause {
    /// `scan.ignore` matches the reference.
    Ignored,
    /// The `uses:` value is escaped or spans lines.
    Unplaceable,
    /// The edit overlaps another edit.
    Overlap,
    /// The edit does not fit its line.
    OutOfBounds,
}

impl From<SkipReason> for SkipCause {
    fn from(reason: SkipReason) -> Self {
        match reason {
            SkipReason::Unplaceable => Self::Unplaceable,
            SkipReason::Overlap => Self::Overlap,
            SkipReason::OutOfBounds => Self::OutOfBounds,
        }
    }
}

/// An action reference that `update` or `pin` left unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedRef {
    /// The reference and its provider.
    #[serde(flatten)]
    pub site: RefSite,
    /// Why the writer left it unchanged.
    pub reason: SkipCause,
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
    /// The versions came from an expired cache entry.
    Stale,
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
    let warning = format!(
        "{}:{}: {} - {}; edit it by hand",
        site.file,
        site.line,
        site.action,
        reason.describe()
    );
    eprintln!("{}: {}", "Warning".yellow(), sanitize_line(&warning));
    result.skipped.push(SkippedRef {
        site,
        reason: reason.into(),
    });
}

/// Record that `scan.ignore` keeps the writer off `site`.
pub fn report_ignored(site: RefSite, result: &mut EditResult) {
    result.skipped.push(SkippedRef {
        site,
        reason: SkipCause::Ignored,
    });
}

fn error_warning(site: &RefSite, err: &CoreError) -> String {
    format!(
        "{} - {}",
        sanitize_line(&site.action),
        sanitize_line(&actup_core::error::chain(err))
    )
}

/// Warn that `err` leaves `site` unchanged, count it, and record it as errored.
pub fn report_error(site: RefSite, err: &CoreError, result: &mut EditResult) {
    eprintln!("{}: {}", "Warning".yellow(), error_warning(&site, err));
    result.errors.record(err);
    result.errored.push(ErroredRef {
        site,
        kind: ErrorKind::from(err),
        message: actup_core::error::chain(err),
    });
}

/// Why a writer has no versions to write from.
#[derive(Debug)]
pub enum Unwritable {
    /// The provider failed.
    Failed(CoreError),
    /// The versions came from an expired cache entry.
    Stale(Staleness),
}

impl From<CoreError> for Unwritable {
    fn from(err: CoreError) -> Self {
        Self::Failed(err)
    }
}

/// The fetched versions of a writer's ref, refusing a set read from an expired cache entry.
pub fn writable_versions(
    versions: Result<&version::Set, CoreError>,
) -> Result<&version::Set, Unwritable> {
    let versions = versions?;
    versions
        .stale()
        .map_or(Ok(versions), |stale| Err(Unwritable::Stale(stale)))
}

/// Warn that `why` leaves `site` unchanged, count it as an error, and record it as errored.
///
/// A stale version set counts as an environment error.
pub fn report_unwritable(site: RefSite, why: Unwritable, result: &mut EditResult) {
    match why {
        Unwritable::Failed(err) => report_error(site, &err, result),
        Unwritable::Stale(stale) => {
            let message = format!("versions are stale ({stale}); refusing to write");
            eprintln!(
                "{}: {} - {message}",
                "Warning".yellow(),
                sanitize_line(&site.action)
            );
            result.errors.record_env();
            result.errored.push(ErroredRef {
                site,
                kind: ErrorKind::Stale,
                message,
            });
        }
    }
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
/// text that fails [`SourceFormat::validate_edit`] or a file that resolves outside `root`,
/// in a dry run too. Returns each written change with its version comment, and
/// records them in `result`. A refused or failed write records each change as
/// an errored ref and returns nothing.
pub fn commit_file(
    file_edits: FileEdits<Change>,
    root: &Path,
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
        resolve_within(root, &path)
            .map_err(WriteRefusal::Io)
            .and_then(|_| {
                splices
                    .format()
                    .validate_edit(splices.scanned(), &splices.planned())
                    .map_err(WriteRefusal::Invalid)
            })
    } else {
        write_splices(root, &path, &splices)
    };
    if let Err(refusal) = outcome {
        let message = refusal.message(path.display());
        let shown = refusal.message(sanitize_line(&path.display().to_string()));
        eprintln!("{}: {}", "Error".red(), sanitize_line(&shown));
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

    fn message(&self, path: impl std::fmt::Display) -> String {
        match self {
            Self::Io(e) => format!("writing {path} failed: {e}"),
            Self::Changed => format!("edit refused: {path} changed since scan"),
            Self::Invalid(e) => format!("edit refused: {path} after edit: {e}"),
        }
    }
}

impl From<std::io::Error> for WriteRefusal {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

fn write_splices(root: &Path, path: &Path, splices: &Splices) -> Result<(), WriteRefusal> {
    rewrite_atomic(root, path, |current| {
        let content = splices.apply(current).ok_or(WriteRefusal::Changed)?;
        splices
            .format()
            .validate_edit(current, &content)
            .map_err(WriteRefusal::Invalid)?;
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
            println!("{}", sanitize_line(&message).dimmed());
        }
    }

    /// Read a file's content, tracking errors in tally; an unreadable file yields its `path: error` message.
    #[allow(clippy::unused_self)]
    pub fn read_file(
        &self,
        file: &DiscoveredFile,
        errors: &mut ErrorTally,
    ) -> Result<String, String> {
        match fs::read_to_string(file.path()) {
            Ok(content) => Ok(content),
            Err(e) => {
                eprintln!(
                    "{}: {} - {}",
                    "Warning".yellow(),
                    sanitize_line(&file.path().display().to_string()),
                    sanitize_line(&e.to_string())
                );
                errors.record_env();
                Err(format!("{}: {e}", file.path().display()))
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
    pub fn extract_actions(
        content: &str,
        format: SourceFormat,
    ) -> impl Iterator<Item = (UsesLocation, ActionRef)> + '_ {
        format.extract_uses(content).into_iter().filter_map(|loc| {
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

    /// The provider serving `scanned` and its site in `file`, or `None` after recording it in `result` as unserved or ignored.
    pub fn editable(
        &self,
        file: &ScannedFile,
        scanned: &ScannedRef,
        result: &mut EditResult,
    ) -> Option<(&dyn Provider, RefSite)> {
        let line = scanned.loc.line();
        let Some(provider) = self.provider_for(&scanned.action_ref) else {
            report_unserved(self, &file.relative, line, &scanned.action_ref, result);
            return None;
        };
        let site = RefSite::new(&file.relative, line, &scanned.action_ref, provider);
        if self.config.should_ignore(&scanned.action_ref) {
            report_ignored(site, result);
            return None;
        }
        Some((provider, site))
    }

    /// The host an action resolves to: its written host, else the default host, else `github.com`.
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
                crate::structured::json_pretty(&self.report(dry_run)).map_err(|e| e.to_string())
            }
            OutputFormat::Toml => {
                crate::structured::toml(&self.report(dry_run)).map_err(|e| e.to_string())
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

        if let Some(notice) = self.ignored_notice() {
            println!("{}", notice.dimmed());
        }

        if let Some(err_summary) = self.errors.summary() {
            println!("{}", err_summary.red());
        }
    }

    fn unwritable(&self) -> usize {
        self.skipped
            .iter()
            .filter(|s| s.reason != SkipCause::Ignored)
            .count()
    }

    fn skipped_notice(&self) -> Option<String> {
        let count = self.unwritable();
        (count > 0).then(|| format!("Skipped {count} action reference(s); edit them by hand"))
    }

    fn ignored_notice(&self) -> Option<String> {
        let count = self.skipped.len() - self.unwritable();
        (count > 0).then(|| format!("Ignored {count} action reference(s) matched by `scan.ignore`"))
    }

    /// Determine exit code, considering skipped references and `dry_run` for outdated status.
    #[must_use]
    pub fn exit_code(&self, dry_run: bool) -> i32 {
        // Errors take priority
        let base = self.errors.exit_code();
        if base != CliExitCode::Success {
            return base.into();
        }

        if self.unwritable() > 0 || (dry_run && !self.edited.is_empty()) {
            return CliExitCode::Outdated.into();
        }

        CliExitCode::Success.into()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use actup_core::{Config, DiscoveredFile, FileKind, patch::PendingEdit, provider};

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
            source: None,
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
            reason: reason.into(),
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
    fn test_error_warning_keeps_untrusted_text_on_one_line() {
        let mut forged = site(3);
        forged.action = "o/r@v1\n::error::x".to_string();
        let err = CoreError::InvalidRef {
            input: "v1\r\n  L9: forged".to_string(),
            reason: "bad".to_string(),
        };
        let warning = error_warning(&forged, &err);
        assert_eq!(warning.lines().count(), 1, "{warning:?}");
        assert!(!warning.contains('\r'), "{warning:?}");
        assert!(
            warning.contains("o/r@v1\u{fffd}::error::x - "),
            "{warning:?}"
        );
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
    fn test_report_error_message_carries_the_cause() {
        let mut result = EditResult::default();
        let refused = CoreError::Network {
            operation: "GET request".into(),
            message: String::new(),
            source: Some(std::sync::Arc::new(std::io::Error::other(
                "connection refused",
            ))),
        };
        report_error(site(3), &refused, &mut result);
        assert_eq!(
            result.errored[0].message,
            "network error during GET request: connection refused"
        );
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
                hint: None,
                source: None,
            },
            CoreError::YamlParse {
                path: s(),
                reason: s(),
                source: None,
            },
            CoreError::Network {
                operation: s(),
                message: s(),
                source: None,
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
                source: None,
            },
            CoreError::Cache {
                operation: s(),
                reason: s(),
            },
            CoreError::ProviderNotFound { host: s() },
            CoreError::Internal {
                context: s(),
                message: s(),
                source: None,
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
        let mut fe = FileEdits::new(path.clone(), content.to_string(), SourceFormat::Yaml);
        fe.add(
            PendingEdit::replace_range(1, 10..12, "v2".to_string()).with_comment(" # c".into()),
            change(1, "v2", true),
        );

        let mut result = EditResult::default();
        let written = commit_file(fe, dir.path(), false, &mut result);

        assert_eq!(written.len(), 1);
        assert_eq!(written[0].1.as_deref(), Some(" # c"));
        assert_eq!(result.edited, [edited_ref(1, Some("# c"))]);
        assert_eq!(result.skipped, []);
        assert_eq!(result.errors.total(), 0);
        assert_eq!(fs::read_to_string(&path)?, "uses: a/b@v2 # c\n");
        Ok(())
    }

    #[test]
    fn test_commit_file_refuses_a_batch_with_overlapping_edits()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        let content = "uses: a/b@v1\nuses: c/d@v1\n";
        fs::write(&path, content)?;

        for dry_run in [false, true] {
            let mut fe = FileEdits::new(path.clone(), content.to_string(), SourceFormat::Yaml);
            for (line, range) in [(1, 10..12), (1, 11..12), (2, 10..12)] {
                fe.add(
                    PendingEdit::replace_range(line, range, "v2".to_string()),
                    change(line, "v2", false),
                );
            }
            let mut result = EditResult::default();
            let written = commit_file(fe, dir.path(), dry_run, &mut result);

            assert!(written.is_empty());
            assert_eq!(result.edited, []);
            let skipped: Vec<(usize, SkipCause)> = result
                .skipped
                .iter()
                .map(|skip| (skip.site.line, skip.reason))
                .collect();
            assert_eq!(
                skipped,
                [
                    (1, SkipCause::Overlap),
                    (1, SkipCause::Overlap),
                    (2, SkipCause::Overlap),
                ]
            );
            assert_eq!(result.errors.total(), 0);
            assert_eq!(result.exit_code(dry_run), i32::from(CliExitCode::Outdated));
            assert_eq!(fs::read_to_string(&path)?, content);
        }
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
        let mut fe = FileEdits::new(path.clone(), content.to_string(), SourceFormat::Yaml);
        fe.add(
            PendingEdit::replace_range(1, 13..15, "v2".to_string()),
            change(1, "v2", false),
        );

        let mut result = EditResult::default();
        let written = commit_file(fe, dir.path(), false, &mut result);

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
        let mut fe = FileEdits::new(path.clone(), content.to_string(), SourceFormat::Yaml);
        fe.add(
            PendingEdit::replace_range(1, 10..12, "v2".to_string()),
            change(1, "v2", false),
        );

        let mut result = EditResult::default();
        let written = commit_file(fe, dir.path(), true, &mut result);

        assert_eq!(written.len(), 1);
        assert_eq!(result.edited, [edited_ref(1, None)]);
        assert_eq!(fs::read_to_string(&path)?, content);
        Ok(())
    }

    #[test]
    fn test_commit_file_write_failure_counts_nothing() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        fs::create_dir(&path)?;
        let mut fe = FileEdits::new(
            path.clone(),
            "uses: a/b@v1\n".to_string(),
            SourceFormat::Yaml,
        );
        fe.add(
            PendingEdit::replace_range(1, 10..12, "v2".to_string()),
            change(1, "v2", false),
        );

        let mut result = EditResult::default();
        let written = commit_file(fe, dir.path(), false, &mut result);

        assert!(written.is_empty());
        assert_eq!(result.edited, []);
        let [errored] = result.errored.as_slice() else {
            panic!("expected one errored ref: {:?}", result.errored);
        };
        assert_eq!((errored.site.line, errored.kind), (1, ErrorKind::Write));
        assert_eq!(result.errors.exit_code(), CliExitCode::EnvironmentError);
        assert!(path.is_dir());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_commit_file_refuses_a_file_swapped_for_an_outside_symlink()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let target = outside.path().join("ci.yml");
        let content = "uses: a/b@v1\n";
        fs::write(&target, content)?;
        let path = root.path().join("ci.yml");
        std::os::unix::fs::symlink(&target, &path)?;

        for dry_run in [false, true] {
            let mut fe = FileEdits::new(path.clone(), content.to_string(), SourceFormat::Yaml);
            fe.add(
                PendingEdit::replace_range(1, 10..12, "v2".to_string()),
                change(1, "v2", false),
            );
            let mut result = EditResult::default();
            let written = commit_file(fe, root.path(), dry_run, &mut result);

            assert!(written.is_empty());
            assert_eq!(result.edited, []);
            let [errored] = result.errored.as_slice() else {
                panic!("expected one errored ref: {:?}", result.errored);
            };
            assert_eq!(errored.kind, ErrorKind::Write);
            assert_eq!(
                errored.message,
                format!(
                    "writing {} failed: resolves outside {}",
                    path.display(),
                    root.path().display()
                )
            );
        }
        assert_eq!(fs::read_to_string(&target)?, content);
        Ok(())
    }

    #[test]
    fn test_commit_file_refuses_a_file_changed_since_scan() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ci.yml");
        let scanned = "uses: a/b@v1\n";
        let changed = "uses: a/b@main\n";
        fs::write(&path, changed)?;
        let mut fe = FileEdits::new(path.clone(), scanned.to_string(), SourceFormat::Yaml);
        fe.add(
            PendingEdit::replace_range(1, 10..12, "v2".to_string()),
            change(1, "v2", false),
        );

        let mut result = EditResult::default();
        let written = commit_file(fe, dir.path(), false, &mut result);

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
        let mut fe = FileEdits::new(path.clone(), scanned.to_string(), SourceFormat::Yaml);
        fe.add(
            PendingEdit::replace_range(1, 10..12, "v2".to_string()),
            change(1, "v2", false),
        );

        let mut result = EditResult::default();
        let written = commit_file(fe, dir.path(), false, &mut result);

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
        let mut fe = FileEdits::new(path.clone(), content.to_string(), SourceFormat::Yaml);
        fe.add(
            PendingEdit::replace_range(2, 14..16, new.to_string()),
            change(2, "v2", false),
        );
        let mut result = EditResult::default();
        let written = commit_file(fe, dir, dry_run, &mut result).len();
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

    #[test]
    fn test_host_for_falls_back_to_the_default_host_then_github()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = Config::default();
        let bare = ActionRef::simple("o", "r", "v1");
        let hosted = parse_uses("ghe.corp.example/o/r@v1")?.ok_or("no action ref")?;

        let mut with_default = provider::Registry::new();
        with_default.set_default_host("git.example.org");
        let scanner = WorkflowScanner::new(
            Path::new("/repo"),
            &config,
            &with_default,
            OutputFormat::Json,
        );
        assert!(scanner.provider_for(&bare).is_none());
        assert_eq!(scanner.host_for(&bare), "git.example.org");
        assert_eq!(scanner.host_for(&hosted), "ghe.corp.example");
        let mut result = EditResult::default();
        report_unserved(&scanner, "ci.yml", 3, &bare, &mut result);
        let hosts: Vec<(&str, &str, ErrorKind)> = result
            .errored
            .iter()
            .map(|e| (e.site.host.as_str(), e.site.provider.as_str(), e.kind))
            .collect();
        assert_eq!(
            hosts,
            [("git.example.org", "unknown", ErrorKind::ProviderNotFound)]
        );

        let without_default = provider::Registry::new();
        let scanner = WorkflowScanner::new(
            Path::new("/repo"),
            &config,
            &without_default,
            OutputFormat::Json,
        );
        assert_eq!(scanner.host_for(&bare), "github.com");
        assert_eq!(scanner.host_for(&hosted), "ghe.corp.example");
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
    fn test_ignored_refs_neither_fail_nor_count_as_skipped()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut result = EditResult::default();
        report_ignored(site(3), &mut result);
        assert_eq!(result.exit_code(false), i32::from(CliExitCode::Success));
        assert_eq!(result.exit_code(true), i32::from(CliExitCode::Success));
        assert!(result.skipped_notice().is_none());
        assert_eq!(
            result.ignored_notice().as_deref(),
            Some("Ignored 1 action reference(s) matched by `scan.ignore`")
        );
        assert_eq!(
            serde_json::to_value(&result.skipped)?,
            serde_json::json!([{
                "file": "ci.yml",
                "line": 3,
                "action": "a/b@v4",
                "owner": "a",
                "repo": "b",
                "host": "github.com",
                "provider": "github",
                "reference": "v4",
                "reason": "ignored",
            }])
        );
        report_skipped(site(4), SkipReason::Overlap, &mut result);
        assert_eq!(result.exit_code(false), i32::from(CliExitCode::Outdated));
        assert_eq!(
            result.skipped_notice().as_deref(),
            Some("Skipped 1 action reference(s); edit them by hand")
        );
        result.print_summary(false, "Pinned", "Would pin", OutputFormat::Human);
        Ok(())
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
