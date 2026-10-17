//! Update command - update actions to their latest versions

use std::path::Path;

use actup_core::{
    ActionRef, CheckStatus, Config, UsesLocation, check_action_with_provider, provider, version,
};
use owo_colors::OwoColorize;

use crate::{
    cli::OutputFormat,
    common::{EditResult, ErrorTally, FileEdits, PendingEdit, WorkflowScanner},
};

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

/// Build the edit for an outdated action, preserving its reference style.
///
/// Tag refs update to the new tag. SHA pins re-pin to the target's commit SHA
/// (never unpin to a tag) and keep a `# vX.Y.Z` version comment accurate.
fn build_edit(loc: &UsesLocation, action_ref: &ActionRef, target: &version::Info) -> PendingEdit {
    if action_ref.is_sha() {
        crate::common::repin_sha_edit(loc, action_ref, target)
    } else {
        create_edit(loc, action_ref, &target.version().to_string())
    }
}

/// Pin a floating branch ref to its HEAD SHA, tagging it with a `# branch` comment.
fn pin_branch_edit(loc: &UsesLocation, ref_name: &str, sha: &str) -> PendingEdit {
    PendingEdit::replace_ref(loc.line(), ref_name.to_string(), sha.to_string())
        .with_comment(format!(" # {ref_name}"))
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
            let Some(provider) = scanner.provider_for(&action_ref) else {
                continue;
            };

            let repo_id = action_ref.to_repo_id().without_subpath();
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
            let (edit, target_display) = match result.status() {
                CheckStatus::Outdated { target, .. } => (
                    build_edit(&loc, &action_ref, target),
                    target.version().to_string(),
                ),
                CheckStatus::PinBranch { ref_name, sha } => (
                    pin_branch_edit(&loc, ref_name, sha),
                    format!("{} # {ref_name}", &sha[..7.min(sha.len())]),
                ),
                _ => continue, // Not actionable, skip
            };

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
                    target_display.green()
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

    fn sha40(byte: char) -> String {
        byte.to_string().repeat(40)
    }

    fn sha_loc(line_body: &str) -> UsesLocation {
        // column of the value start in `      - uses: <value>`
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
    fn test_build_edit_tag_writes_tag() {
        let line = "      - uses: actions/checkout@v4";
        let loc = sha_loc(line);
        let action_ref = make_action_ref("actions", "checkout", "v4");
        let target = actup_core::version::Info::new(
            actup_core::version::Version::parse("v5.0.0"),
            sha40('a'),
        );
        let result = build_edit(&loc, &action_ref, &target).apply(line).unwrap();
        assert_eq!(result, "      - uses: actions/checkout@v5.0.0");
    }

    #[test]
    fn test_build_edit_sha_repins_to_sha_with_comment() {
        let old = sha40('a');
        let new = sha40('b');
        let line = format!("      - uses: actions/checkout@{old}");
        let loc = sha_loc(&line);
        let action_ref = make_action_ref("actions", "checkout", &old);
        let target =
            actup_core::version::Info::new(actup_core::version::Version::parse("v7.0.1"), &new);
        let result = build_edit(&loc, &action_ref, &target).apply(&line).unwrap();
        assert_eq!(
            result,
            format!("      - uses: actions/checkout@{new} # v7.0.1")
        );
    }

    #[test]
    fn test_flow_style_update_round_trip_changes_only_refs() {
        let old_sha = sha40('a');
        let new_sha = sha40('b');
        let content = [
            "steps:",
            "  - { uses: actions/upload-pages-artifact@v4, with: { path: dist } }",
            "  - { uses: actions/deploy-pages@v4, id: deployment }",
            "  - {uses: \"a/b@v4\"} # keep",
            "  - [{ uses: 'c/d@v4' }]",
            "  - { name: \"déploiement ✓\", uses: e/f@v4 }",
            "  - { uses: g/h@OLD_SHA, with: { x: 1 } }",
            "",
        ]
        .join("\n")
        .replace("OLD_SHA", &old_sha);
        let expected = [
            "steps:",
            "  - { uses: actions/upload-pages-artifact@v5.0.0, with: { path: dist } }",
            "  - { uses: actions/deploy-pages@v5.0.0, id: deployment }",
            "  - {uses: \"a/b@v5.0.0\"} # keep",
            "  - [{ uses: 'c/d@v5.0.0' }]",
            "  - { name: \"déploiement ✓\", uses: e/f@v5.0.0 }",
            "  - { uses: g/h@NEW_SHA, with: { x: 1 } } # v5.0.0",
            "",
        ]
        .join("\n")
        .replace("NEW_SHA", &new_sha);
        let target = version::Info::new(version::Version::parse("v5.0.0"), &new_sha);

        let mut file_edits = FileEdits::new(std::path::PathBuf::from("/test.yml"), content.clone());
        let mut seen = 0;
        for (loc, action_ref) in WorkflowScanner::extract_actions(&content) {
            assert!(file_edits.add(build_edit(&loc, &action_ref, &target)));
            seen += 1;
        }
        assert_eq!(seen, 6);

        let Ok(updated) = file_edits.apply() else {
            panic!("edits did not apply");
        };
        assert_eq!(updated, expected);

        let refs: Vec<String> = WorkflowScanner::extract_actions(&updated)
            .map(|(_, action_ref)| action_ref.ref_str().to_string())
            .collect();
        assert_eq!(
            refs,
            [
                "v5.0.0",
                "v5.0.0",
                "v5.0.0",
                "v5.0.0",
                "v5.0.0",
                new_sha.as_str()
            ]
        );
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
