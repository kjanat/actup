//! Update command - update actions to their latest versions

use std::path::Path;

use actup_core::{ActionRef, Config, ProviderRegistry, UsesLocation, Version, VersionInfo};
use owo_colors::OwoColorize;

use crate::cli::OutputFormat;
use crate::common::{EditResult, ErrorTally, FileEdits, PendingEdit, WorkflowScanner};

/// Check if an update is needed and return the new ref if so.
fn needs_update(action_ref: &ActionRef, latest: &VersionInfo) -> Option<String> {
    let current = Version::parse(&action_ref.ref_str);

    let dominated = match &current {
        // For major-only refs (v4), compare major versions
        Version::Semver {
            major,
            minor: None,
            patch: None,
            ..
        } => latest.version.major() != Some(*major),
        // For full semver, exact match
        _ => latest.version.to_string() != action_ref.ref_str,
    };

    dominated.then(|| latest.version.to_string())
}

/// Create an edit for updating an action ref.
fn create_edit(loc: &UsesLocation, action_ref: &ActionRef, new_ref: &str) -> PendingEdit {
    loc.ref_range().map_or_else(
        || PendingEdit::replace_ref(loc.line, action_ref.ref_str.clone(), new_ref.to_string()),
        |range| PendingEdit::replace_range(loc.line, range, new_ref.to_string()),
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
    registry: &ProviderRegistry,
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
        let mut file_edits = FileEdits::new(file.path.clone(), content.clone());

        for (loc, action_ref) in scanner.extract_actions(&content) {
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

            let policy = config.policy_for(&action_ref.owner, &action_ref.repo);
            let current = Version::parse(&action_ref.ref_str);

            let Some(latest) =
                versions.find_latest_for(&current, policy.allows_major(), config.allow_prerelease)
            else {
                continue;
            };

            let Some(new_ref) = needs_update(&action_ref, latest) else {
                continue;
            };

            let edit = create_edit(&loc, &action_ref, &new_ref);

            if !file_edits.add(edit) {
                eprintln!(
                    "{}: {}:{} - multiple actions on same line",
                    "Warning".yellow(),
                    relative,
                    loc.line
                );
                errors.record_user();
                continue;
            }

            if matches!(format, OutputFormat::Human) {
                println!(
                    "{}:{}: {} -> {}",
                    relative,
                    loc.line,
                    action_ref.ref_str.red(),
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
