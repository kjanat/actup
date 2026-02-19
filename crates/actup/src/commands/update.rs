//! Update command - update actions to their latest versions

use std::path::Path;

use actup_core::{
    ActionRef, CheckStatus, Config, UsesLocation, check_action as core_check_action, policy,
    provider,
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
            let result = core_check_action(&action_ref, &versions, config);
            let CheckStatus::Outdated { target, .. } = result.status() else {
                continue; // Not outdated, skip
            };

            let style = config.ref_style_for(action_ref.owner(), action_ref.repo());
            let (new_ref, comment) = if style == policy::Style::Sha {
                let sha = target.sha().to_string();
                let tag = target.version().to_string();
                let comment = if config.pin.comment {
                    Some(format!(" # {tag}"))
                } else {
                    None
                };
                (sha, comment)
            } else {
                (target.version().to_string(), None)
            };

            let mut edit = create_edit(&loc, &action_ref, &new_ref);
            if let Some(ref c) = comment {
                edit = edit.with_comment(c.clone());
            }

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
                let display_suffix = comment.as_deref().unwrap_or("");
                println!(
                    "{}:{}: {} -> {}",
                    relative,
                    loc.line(),
                    action_ref.ref_str().red(),
                    format!("{new_ref}{display_suffix}").green()
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

    #[test]
    fn test_create_edit_sha_style_replaces_ref_with_sha() {
        let line = "      - uses: actions/checkout@v4";
        let loc = UsesLocation::new(10, 15, "actions/checkout@v4".to_string(), line.to_string());
        let sha = "b4ffde65f46336ab88eb53be808477a3936bae11";
        let edit = create_edit(&loc, &make_action_ref("actions", "checkout", "v4"), sha);

        let result = edit.apply(line).unwrap();
        assert_eq!(
            result,
            "      - uses: actions/checkout@b4ffde65f46336ab88eb53be808477a3936bae11"
        );
    }

    #[test]
    fn test_create_edit_sha_style_with_comment() {
        let line = "      - uses: actions/checkout@v4";
        let loc = UsesLocation::new(10, 15, "actions/checkout@v4".to_string(), line.to_string());
        let sha = "b4ffde65f46336ab88eb53be808477a3936bae11";
        let edit = create_edit(&loc, &make_action_ref("actions", "checkout", "v4"), sha)
            .with_comment(" # v4.2.0".to_string());

        let result = edit.apply(line).unwrap();
        assert_eq!(
            result,
            "      - uses: actions/checkout@b4ffde65f46336ab88eb53be808477a3936bae11 # v4.2.0"
        );
    }

    #[test]
    fn test_create_edit_sha_style_no_comment_when_disabled() {
        let line = "      - uses: actions/checkout@v4";
        let loc = UsesLocation::new(10, 15, "actions/checkout@v4".to_string(), line.to_string());
        let sha = "b4ffde65f46336ab88eb53be808477a3936bae11";
        // No .with_comment() — simulates pin.comment = false
        let edit = create_edit(&loc, &make_action_ref("actions", "checkout", "v4"), sha);

        let result = edit.apply(line).unwrap();
        assert!(
            !result.contains('#'),
            "no comment should be appended when pin.comment is false"
        );
    }

    #[test]
    fn test_create_edit_sha_style_skips_comment_when_line_has_existing() {
        let line = "      - uses: actions/checkout@v4 # existing comment";
        let loc = UsesLocation::new(10, 15, "actions/checkout@v4".to_string(), line.to_string());
        let sha = "b4ffde65f46336ab88eb53be808477a3936bae11";
        let edit = create_edit(&loc, &make_action_ref("actions", "checkout", "v4"), sha)
            .with_comment(" # v4.2.0".to_string());

        let result = edit.apply(line).unwrap();
        // Existing comment preserved, new comment not added (PendingEdit behavior)
        assert!(result.contains("# existing comment"));
        assert!(!result.contains("# v4.2.0"));
    }

    #[test]
    fn test_ref_style_sha_config_integration() {
        use actup_core::policy;

        let config = Config {
            ref_style: policy::Style::Sha,
            ..Config::default()
        };
        assert_eq!(
            config.ref_style_for("actions", "checkout"),
            policy::Style::Sha
        );
    }

    #[test]
    fn test_ref_style_sha_per_action_override() {
        use actup_core::config::ActionOverride;
        use actup_core::policy;

        let config = Config {
            action_overrides: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "actions/checkout".to_string(),
                    ActionOverride {
                        ref_style: Some(policy::Style::Sha),
                        ..ActionOverride::default()
                    },
                );
                m
            },
            ..Config::default()
        };
        // Global is preserve (default), but per-action override is sha
        assert_eq!(
            config.ref_style_for("actions", "checkout"),
            policy::Style::Sha
        );
        assert_eq!(
            config.ref_style_for("actions", "setup-node"),
            policy::Style::Preserve
        );
    }
}
