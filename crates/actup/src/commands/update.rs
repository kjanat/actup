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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_action_ref(owner: &str, repo: &str, ref_str: &str) -> ActionRef {
        ActionRef {
            owner: owner.to_string(),
            repo: repo.to_string(),
            ref_str: ref_str.to_string(),
            host: None,
            subpath: None,
        }
    }

    fn make_version_info(ver: &str, sha: &str) -> VersionInfo {
        VersionInfo {
            version: Version::parse(ver),
            sha: sha.to_string(),
        }
    }

    // ========================================================================
    // needs_update tests
    // ========================================================================

    #[test]
    fn test_needs_update_same_version() {
        let action_ref = make_action_ref("actions", "checkout", "v4.2.0");
        let latest = make_version_info("v4.2.0", "aaa");
        assert!(needs_update(&action_ref, &latest).is_none());
    }

    #[test]
    fn test_needs_update_outdated() {
        let action_ref = make_action_ref("actions", "checkout", "v4.0.0");
        let latest = make_version_info("v4.2.0", "aaa");
        let result = needs_update(&action_ref, &latest);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "v4.2.0");
    }

    #[test]
    fn test_needs_update_major_only_same() {
        let action_ref = make_action_ref("actions", "checkout", "v4");
        let latest = make_version_info("v4.2.0", "aaa");
        // v4 -> v4.x.x, major is same so no update needed
        assert!(needs_update(&action_ref, &latest).is_none());
    }

    #[test]
    fn test_needs_update_major_only_different() {
        let action_ref = make_action_ref("actions", "checkout", "v3");
        let latest = make_version_info("v4.0.0", "aaa");
        // v3 -> v4.x.x, major differs so needs update
        let result = needs_update(&action_ref, &latest);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "v4.0.0");
    }

    // ========================================================================
    // create_edit tests
    // ========================================================================

    #[test]
    fn test_create_edit_with_ref_range() {
        let loc = UsesLocation {
            line: 10,
            column: 15,
            value: "actions/checkout@v4".to_string(),
            raw_line: "      - uses: actions/checkout@v4".to_string(),
        };
        let action_ref = make_action_ref("actions", "checkout", "v4");
        let edit = create_edit(&loc, &action_ref, "v5");

        // Should use replace_range when ref_range is available
        assert_eq!(edit.line, 10);
    }

    #[test]
    fn test_create_edit_fallback_replace_ref() {
        // When ref_range returns None (no @), falls back to replace_ref
        let loc = UsesLocation {
            line: 5,
            column: 15,
            value: "./local-action".to_string(),
            raw_line: "      - uses: ./local-action".to_string(),
        };
        let action_ref = make_action_ref(".", "local-action", "");
        let edit = create_edit(&loc, &action_ref, "v1.0.0");

        assert_eq!(edit.line, 5);
    }
}
