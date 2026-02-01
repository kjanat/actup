//! Shared infrastructure for workflow scanning commands.
//!
//! This module provides the common pipeline used by check, update, and pin commands:
//! discover -> read -> extract uses -> filter -> provider lookup -> (command-specific logic)

use std::collections::HashMap;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use actup_core::{
    ActionRef, Config, DiscoveredFile, Error as CoreError, Provider, ProviderRegistry,
    UsesLocation, discover_files, extract_uses_from_yaml, parse_uses,
};
use owo_colors::OwoColorize;

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

    /// Record a user error.
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
            CliExitCode::Unresolvable
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
    /// Replace the ref portion only (after @).
    ReplaceRef {
        /// Old ref string (for validation).
        old_ref: String,
        /// New ref string.
        new_ref: String,
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

    /// Create an edit that replaces the ref portion.
    #[must_use]
    pub const fn replace_ref(line: usize, old_ref: String, new_ref: String) -> Self {
        Self {
            line,
            kind: EditKind::ReplaceRef { old_ref, new_ref },
            comment: None,
        }
    }

    /// Add a comment to append after the edit.
    #[must_use]
    pub fn with_comment(mut self, comment: String) -> Self {
        self.comment = Some(comment);
        self
    }

    /// Apply this edit to a line, returning the new line content.
    ///
    /// Returns `None` if the edit cannot be applied (mismatch).
    #[must_use]
    pub fn apply(&self, line: &str) -> Option<String> {
        let mut result = match &self.kind {
            EditKind::ReplaceRange { range, new } => {
                if range.end > line.len() {
                    return None;
                }
                let mut s = String::with_capacity(line.len() + new.len());
                s.push_str(&line[..range.start]);
                s.push_str(new);
                s.push_str(&line[range.end..]);
                s
            }
            EditKind::ReplaceRef { old_ref, new_ref } => {
                let pattern = format!("@{old_ref}");
                if !line.contains(&pattern) {
                    return None;
                }
                let new_pattern = format!("@{new_ref}");
                line.replacen(&pattern, &new_pattern, 1)
            }
        };

        // Append comment if specified and line doesn't already have one
        if let Some(comment) = &self.comment
            && !result.contains('#')
        {
            result.push_str(comment);
        }

        Some(result)
    }
}

// FileEdits - accumulated edits for a single file

/// Accumulated edits for a single file.
pub struct FileEdits {
    /// Absolute path to the file.
    pub path: PathBuf,
    /// Original file content.
    content: String,
    /// Pending edits by line number.
    edits: HashMap<usize, PendingEdit>,
}

impl FileEdits {
    /// Create a new `FileEdits` for a file.
    #[must_use]
    pub fn new(path: PathBuf, content: String) -> Self {
        Self {
            path,
            content,
            edits: HashMap::new(),
        }
    }

    /// Add an edit. Returns false if an edit already exists for that line (collision).
    pub fn add(&mut self, edit: PendingEdit) -> bool {
        if self.edits.contains_key(&edit.line) {
            return false;
        }
        self.edits.insert(edit.line, edit);
        true
    }

    /// Check if there are any edits pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    /// Apply all edits and return the new content.
    ///
    /// Returns `Err` with line number if any edit fails to apply.
    pub fn apply(&self) -> Result<String, usize> {
        let lines: Vec<&str> = self.content.lines().collect();
        let mut new_lines: Vec<String> = Vec::with_capacity(lines.len());

        for (i, line) in lines.iter().enumerate() {
            let line_num = i + 1;
            if let Some(edit) = self.edits.get(&line_num) {
                match edit.apply(line) {
                    Some(new_line) => new_lines.push(new_line),
                    None => return Err(line_num),
                }
            } else {
                new_lines.push((*line).to_string());
            }
        }

        // Preserve trailing newline
        let result = if self.content.ends_with('\n') {
            new_lines.join("\n") + "\n"
        } else {
            new_lines.join("\n")
        };

        Ok(result)
    }

    /// Write the edited content to disk.
    ///
    /// Does nothing if `dry_run` is true.
    pub fn write(&self, dry_run: bool) -> std::io::Result<()> {
        if dry_run {
            return Ok(());
        }
        let content = self.apply().map_err(|line| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("edit mismatch at line {line}"),
            )
        })?;
        fs::write(&self.path, content)
    }
}

// WorkflowScanner - shared file discovery and action extraction

/// Shared infrastructure for scanning workflow files.
pub struct WorkflowScanner<'a> {
    path: &'a Path,
    config: &'a Config,
    registry: &'a ProviderRegistry,
    format: OutputFormat,
}

impl<'a> WorkflowScanner<'a> {
    /// Create a new scanner.
    #[must_use]
    pub const fn new(
        path: &'a Path,
        config: &'a Config,
        registry: &'a ProviderRegistry,
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
        discover_files(self.path, &self.config.extra_paths)
    }

    /// Print "no files found" message (human format only).
    pub fn print_empty_message(&self) {
        if matches!(self.format, OutputFormat::Human) {
            println!(
                "{}",
                "No workflow files found in .github/workflows/".dimmed()
            );
        }
    }

    /// Read a file's content, tracking errors in tally.
    #[allow(clippy::unused_self)]
    pub fn read_file(&self, file: &DiscoveredFile, errors: &mut ErrorTally) -> Option<String> {
        match fs::read_to_string(&file.path) {
            Ok(content) => Some(content),
            Err(e) => {
                eprintln!("{}: {} - {}", "Warning".yellow(), file.path.display(), e);
                errors.record_env();
                None
            }
        }
    }

    /// Get relative path for display.
    #[must_use]
    pub fn relative_path(&self, file: &DiscoveredFile) -> String {
        file.path
            .strip_prefix(self.path)
            .unwrap_or(&file.path)
            .display()
            .to_string()
    }

    /// Extract action references from content, filtering as appropriate.
    ///
    /// Yields `(UsesLocation, ActionRef)` pairs for non-local, non-ignored actions.
    pub fn extract_actions<'b>(
        &self,
        content: &'b str,
    ) -> impl Iterator<Item = (UsesLocation, ActionRef)> + 'b
    where
        'a: 'b,
    {
        let config = self.config;
        extract_uses_from_yaml(content)
            .into_iter()
            .filter_map(move |loc| {
                let Ok(Some(action_ref)) = parse_uses(&loc.value) else {
                    return None; // Local action or parse error
                };

                // Filter ignored actions
                if config.should_ignore(&action_ref.owner, &action_ref.repo) {
                    return None;
                }

                Some((loc, action_ref))
            })
    }

    /// Look up provider for an action. Returns None if not found.
    #[must_use]
    pub fn provider_for(&self, action: &ActionRef) -> Option<&dyn Provider> {
        self.registry.find(action.host.as_deref())
    }
}

// EditResult - result of applying edits across files

/// Summary of edit operations.
pub struct EditResult {
    /// Total number of edits applied.
    pub count: usize,
    /// Error tally.
    pub errors: ErrorTally,
}

impl EditResult {
    /// Print human-readable summary.
    pub fn print_summary(&self, dry_run: bool, verb: &str, dry_verb: &str, format: OutputFormat) {
        if !matches!(format, OutputFormat::Human) {
            return;
        }

        println!();
        let msg = if dry_run {
            format!("{dry_verb} {count} action reference(s)", count = self.count)
        } else {
            format!("{verb} {count} action reference(s)", count = self.count)
        };
        println!("{}", msg.bold());

        if let Some(err_summary) = self.errors.summary() {
            println!("{}", err_summary.red());
        }
    }

    /// Determine exit code, considering `dry_run` for outdated status.
    #[must_use]
    pub fn exit_code(&self, dry_run: bool) -> i32 {
        // Errors take priority
        let base = self.errors.exit_code();
        if base != CliExitCode::Success {
            return base.into();
        }

        // For dry-run, return outdated if there were changes
        if dry_run && self.count > 0 {
            return CliExitCode::Outdated.into();
        }

        CliExitCode::Success.into()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ErrorTally tests

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
        tally.record_user();
        tally.record_user();

        assert_eq!(tally.total(), 2);
        assert_eq!(tally.exit_code(), CliExitCode::Unresolvable);
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
        tally.record_user();
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
        assert_eq!(tally2.exit_code(), CliExitCode::Unresolvable);
    }

    // PendingEdit tests

    #[test]
    fn test_pending_edit_replace_range() {
        let edit = PendingEdit::replace_range(1, 10..14, "v2.0.0".to_string());
        let line = "  - uses: actions/checkout@v1.0.0";
        let result = edit.apply(line).unwrap();
        assert_eq!(result, "  - uses: v2.0.0ons/checkout@v1.0.0");
    }

    #[test]
    fn test_pending_edit_replace_range_exact() {
        // Replace just the ref portion
        let line = "  - uses: actions/checkout@v1";
        let edit = PendingEdit::replace_range(1, 27..29, "v2".to_string());
        let result = edit.apply(line).unwrap();
        assert_eq!(result, "  - uses: actions/checkout@v2");
    }

    #[test]
    fn test_pending_edit_replace_range_out_of_bounds() {
        let edit = PendingEdit::replace_range(1, 100..110, "new".to_string());
        let line = "short";
        assert!(edit.apply(line).is_none());
    }

    #[test]
    fn test_pending_edit_replace_ref() {
        let edit = PendingEdit::replace_ref(1, "v1".to_string(), "v2".to_string());
        let line = "  - uses: actions/checkout@v1";
        let result = edit.apply(line).unwrap();
        assert_eq!(result, "  - uses: actions/checkout@v2");
    }

    #[test]
    fn test_pending_edit_replace_ref_not_found() {
        let edit = PendingEdit::replace_ref(1, "v3".to_string(), "v4".to_string());
        let line = "  - uses: actions/checkout@v1";
        assert!(edit.apply(line).is_none());
    }

    #[test]
    fn test_pending_edit_with_comment() {
        let edit = PendingEdit::replace_ref(1, "v1".to_string(), "abc123".to_string())
            .with_comment(" # v1".to_string());
        let line = "  - uses: actions/checkout@v1";
        let result = edit.apply(line).unwrap();
        assert_eq!(result, "  - uses: actions/checkout@abc123 # v1");
    }

    #[test]
    fn test_pending_edit_comment_not_added_if_exists() {
        let edit = PendingEdit::replace_ref(1, "v1".to_string(), "v2".to_string())
            .with_comment(" # pinned".to_string());
        let line = "  - uses: actions/checkout@v1 # existing comment";
        let result = edit.apply(line).unwrap();
        // Comment not added because line already has one
        assert_eq!(result, "  - uses: actions/checkout@v2 # existing comment");
    }

    // FileEdits tests

    #[test]
    fn test_file_edits_empty() {
        let fe = FileEdits::new(PathBuf::from("/test"), "content".to_string());
        assert!(fe.is_empty());
    }

    #[test]
    fn test_file_edits_add_and_apply() {
        let content = "line1\nuses: foo@v1\nline3\n";
        let mut fe = FileEdits::new(PathBuf::from("/test"), content.to_string());

        let edit = PendingEdit::replace_ref(2, "v1".to_string(), "v2".to_string());
        assert!(fe.add(edit));
        assert!(!fe.is_empty());

        let result = fe.apply().unwrap();
        assert_eq!(result, "line1\nuses: foo@v2\nline3\n");
    }

    #[test]
    fn test_file_edits_collision() {
        let mut fe = FileEdits::new(PathBuf::from("/test"), "line1\nline2\n".to_string());

        let edit1 = PendingEdit::replace_ref(1, "1".to_string(), "ONE".to_string());
        let edit2 = PendingEdit::replace_ref(1, "1".to_string(), "UNO".to_string());

        assert!(fe.add(edit1));
        assert!(!fe.add(edit2)); // Collision
    }

    #[test]
    fn test_file_edits_preserves_trailing_newline() {
        let with_newline = "line1\nline2\n";
        let fe = FileEdits::new(PathBuf::from("/test"), with_newline.to_string());
        let result = fe.apply().unwrap();
        assert!(result.ends_with('\n'));

        let without_newline = "line1\nline2";
        let fe2 = FileEdits::new(PathBuf::from("/test"), without_newline.to_string());
        let result2 = fe2.apply().unwrap();
        assert!(!result2.ends_with('\n'));
    }

    #[test]
    fn test_file_edits_apply_failure() {
        let content = "short\n";
        let mut fe = FileEdits::new(PathBuf::from("/test"), content.to_string());

        // Edit that will fail (range out of bounds)
        let edit = PendingEdit::replace_range(1, 100..110, "x".to_string());
        fe.add(edit);

        let result = fe.apply();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), 1); // Failed at line 1
    }

    #[test]
    fn test_file_edits_multiple_lines() {
        let content = "  - uses: actions/checkout@v1\n  - uses: actions/setup-node@v2\n";
        let mut fe = FileEdits::new(PathBuf::from("/test"), content.to_string());

        fe.add(PendingEdit::replace_ref(
            1,
            "v1".to_string(),
            "v4".to_string(),
        ));
        fe.add(PendingEdit::replace_ref(
            2,
            "v2".to_string(),
            "v5".to_string(),
        ));

        let result = fe.apply().unwrap();
        assert_eq!(
            result,
            "  - uses: actions/checkout@v4\n  - uses: actions/setup-node@v5\n"
        );
    }

    // EditResult tests

    #[test]
    fn test_edit_result_exit_code_success() {
        let result = EditResult {
            count: 0,
            errors: ErrorTally::default(),
        };
        assert_eq!(result.exit_code(false), 0);
        assert_eq!(result.exit_code(true), 0);
    }

    #[test]
    fn test_edit_result_exit_code_dry_run_with_changes() {
        let result = EditResult {
            count: 5,
            errors: ErrorTally::default(),
        };
        assert_eq!(result.exit_code(false), 0); // Not dry run
        assert_eq!(result.exit_code(true), 1); // Dry run = outdated
    }

    #[test]
    fn test_edit_result_exit_code_errors_priority() {
        let mut errors = ErrorTally::default();
        errors.record_env();

        let result = EditResult { count: 5, errors };
        // Env error takes priority over changes
        assert_eq!(result.exit_code(true), 3);
    }
}
