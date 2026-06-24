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
    if !action_ref.is_sha() {
        return create_edit(loc, action_ref, &target.version().to_string());
    }

    let new_sha = target.sha();
    let version_comment = format!(" # {}", target.version());

    let Some(range) = loc.ref_range() else {
        return PendingEdit::replace_ref(
            loc.line(),
            action_ref.ref_str().to_string(),
            new_sha.to_string(),
        );
    };

    let raw = loc.raw_line();
    let tail = raw.get(range.end..).unwrap_or("").trim_start();
    if is_version_comment(tail) {
        // Overwrite the SHA and the stale version comment together so the paper
        // trail stays accurate.
        PendingEdit::replace_range(
            loc.line(),
            range.start..raw.len(),
            format!("{new_sha}{version_comment}"),
        )
    } else {
        // Update the SHA; `with_comment` adds the version note only when the line
        // has no comment, so a foreign comment is left untouched.
        PendingEdit::replace_range(loc.line(), range, new_sha.to_string())
            .with_comment(version_comment)
    }
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
            let CheckStatus::Outdated { target, .. } = result.status() else {
                continue; // Not outdated, skip
            };

            let edit = build_edit(&loc, &action_ref, target);

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
                    target.version().to_string().green()
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
    fn test_build_edit_sha_refreshes_stale_version_comment() {
        let old = sha40('a');
        let new = sha40('b');
        let line = format!("      - uses: actions/checkout@{old} # v7.0.0");
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
    fn test_build_edit_sha_preserves_foreign_comment() {
        let old = sha40('a');
        let new = sha40('b');
        let line = format!("      - uses: actions/checkout@{old} # managed by bot");
        let loc = sha_loc(&line);
        let action_ref = make_action_ref("actions", "checkout", &old);
        let target =
            actup_core::version::Info::new(actup_core::version::Version::parse("v7.0.1"), &new);
        let result = build_edit(&loc, &action_ref, &target).apply(&line).unwrap();
        assert_eq!(
            result,
            format!("      - uses: actions/checkout@{new} # managed by bot")
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
