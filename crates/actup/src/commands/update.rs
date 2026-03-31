//! Update command - update actions to their latest versions

use std::path::Path;

use actup_core::{
    ActionRef, CheckStatus, Config, UsesLocation, check_action_with_provider, provider,
};
use owo_colors::OwoColorize;

use crate::cli::OutputFormat;
use crate::common::{EditResult, ErrorTally, FileEdits, PendingEdit, WorkflowScanner};

/// Create an edit for updating an action ref.
fn create_edit(loc: &UsesLocation, action_ref: &ActionRef, new_ref: &str) -> PendingEdit {
    loc.ref_range().map_or_else(
        || {
            PendingEdit::replace_ref(
                loc.line(),
                action_ref.ref_str().to_string(),
                new_ref.to_string(),
            )
        },
        |range| PendingEdit::replace_range(loc.line(), range, new_ref.to_string()),
    )
}

/// Write accumulated file edits, recording errors.
fn write_files(files: &[FileEdits], dry_run: bool, errors: &mut ErrorTally) {
    for file_edits in files {
        if let Err(e) = file_edits.write(dry_run) {
            eprintln!(
                "{}: {} - {}",
                "Error writing file".red(),
                file_edits.path.display(),
                e
            );
            errors.record_env();
        }
    }
}

pub async fn run(
    path: &Path,
    dry_run: bool,
    format: OutputFormat,
    config: &Config,
    registry: &provider::Registry,
) -> i32 {
    let scanner = WorkflowScanner::new(path, config, registry, format);
    let discovered = scanner.discover();

    if discovered.is_empty() {
        scanner.print_empty_message();
        return 0;
    }

    let mut errors = ErrorTally::default();
    let mut total_updates = 0;
    let mut files_to_write: Vec<FileEdits> = Vec::new();

    for file in &discovered {
        let Some(content) = scanner.read_file(file, &mut errors) else {
            continue;
        };

        let relative = scanner.relative_path(file);
        let mut file_edits = FileEdits::new(file.path().to_path_buf(), content.clone());

        for (loc, action_ref) in WorkflowScanner::extract_actions(&content) {
            // Skip SHAs - can't determine "latest" for a pinned ref
            if action_ref.is_sha() {
                continue;
            }

            let Some(provider) = scanner.provider_for(&action_ref) else {
                continue;
            };

            let repo_id = action_ref.to_repo_id();
            let versions = match provider.get_versions(&repo_id).await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("{}: {} - {}", "Warning".yellow(), action_ref, e);
                    errors.record(&e);
                    continue;
                }
            };

            // Use core check_action to determine if update needed
            let result = check_action_with_provider(&action_ref, &versions, config, provider).await;
            let CheckStatus::Outdated { target, .. } = result.status() else {
                continue; // Not outdated, skip
            };

            let new_ref = target.version().to_string();

            let edit = create_edit(&loc, &action_ref, &new_ref);

            if !file_edits.add(edit) {
                eprintln!(
                    "{}: {}:{} - multiple actions on same line",
                    "Warning".yellow(),
                    relative,
                    loc.line()
                );
                errors.record_user();
                continue;
            }

            if matches!(format, OutputFormat::Human) {
                println!(
                    "{}:{}: {} -> {}",
                    relative,
                    loc.line(),
                    action_ref.ref_str().red(),
                    new_ref.green()
                );
            }
            total_updates += 1;
        }

        if !file_edits.is_empty() {
            files_to_write.push(file_edits);
        }
    }

    write_files(&files_to_write, dry_run, &mut errors);

    let result = EditResult {
        count: total_updates,
        errors,
    };

    result.print_summary(dry_run, "Updated", "Would update", format);
    result.exit_code(dry_run)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_action_ref(owner: &str, repo: &str, ref_str: &str) -> ActionRef {
        ActionRef::simple(owner, repo, ref_str)
    }

    // Version resolution tests now live in actup-core::resolution module.

    // create_edit tests

    #[test]
    fn test_create_edit_with_ref_range() {
        let loc = UsesLocation::new(
            10,
            15,
            "actions/checkout@v4".to_string(),
            "      - uses: actions/checkout@v4".to_string(),
        );
        let action_ref = make_action_ref("actions", "checkout", "v4");
        let edit = create_edit(&loc, &action_ref, "v5");

        // Should use replace_range when ref_range is available
        assert_eq!(edit.line, 10);
    }

    #[test]
    fn test_create_edit_fallback_replace_ref() {
        // When ref_range returns None (no @), falls back to replace_ref
        let loc = UsesLocation::new(
            5,
            15,
            "./local-action".to_string(),
            "      - uses: ./local-action".to_string(),
        );
        let action_ref = make_action_ref(".", "local-action", "");
        let edit = create_edit(&loc, &action_ref, "v1.0.0");

        assert_eq!(edit.line, 5);
    }
}
