//! Pin command - pin all actions to their current SHA

use std::path::Path;

use actup_core::{ActionRef, Config, ProviderRegistry};
use owo_colors::OwoColorize;

use crate::cli::OutputFormat;
use crate::common::{EditResult, ErrorTally, FileEdits, PendingEdit, WorkflowScanner};

/// Get the prefix of an action ref (everything before @ref).
fn action_prefix(action: &ActionRef) -> String {
    let mut s = String::new();
    if let Some(h) = &action.host {
        s.push_str(h);
        s.push('/');
    }
    s.push_str(&action.owner);
    s.push('/');
    s.push_str(&action.repo);
    if let Some(p) = &action.subpath {
        s.push('/');
        s.push_str(p);
    }
    s
}

/// Resolve the SHA for an action reference.
///
/// First tries to find it in the version list, then falls back to resolving the ref directly.
async fn resolve_sha(
    provider: &dyn actup_core::Provider,
    action_ref: &ActionRef,
    errors: &mut ErrorTally,
) -> Option<String> {
    let repo_id = action_ref.to_repo_id();

    // Try to get versions first
    let versions = match provider.get_versions(&repo_id).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}: {} - {}", "Warning".yellow(), action_ref, e);
            errors.record(&e);
            return None;
        }
    };

    // Find version matching current ref
    if let Some(v) = versions
        .semver_tags
        .iter()
        .find(|v| v.version.to_string() == action_ref.ref_str)
    {
        return Some(v.sha.clone());
    }

    // Try to resolve the ref directly (might be a branch)
    match provider
        .resolve_ref_to_sha(&repo_id, &action_ref.ref_str)
        .await
    {
        Ok(sha40) => Some(sha40.as_str().to_string()),
        Err(e) => {
            eprintln!("{}: {} - {}", "Warning".yellow(), action_ref, e);
            errors.record(&e);
            None
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
    let mut total_pins = 0;
    let mut files_to_write: Vec<FileEdits> = Vec::new();

    for file in &discovered {
        let Some(content) = scanner.read_file(file, &mut errors) else {
            continue;
        };

        let relative = scanner.relative_path(file);
        let mut file_edits = FileEdits::new(file.path.clone(), content.clone());

        for (loc, action_ref) in scanner.extract_actions(&content) {
            // Skip already pinned (SHA)
            if action_ref.is_sha() {
                continue;
            }

            let Some(provider) = scanner.provider_for(&action_ref) else {
                continue;
            };

            let Some(sha) = resolve_sha(provider, &action_ref, &mut errors).await else {
                continue;
            };

            // Build the new uses value
            let prefix = action_prefix(&action_ref);
            let new_uses = format!("{prefix}@{sha}");
            let comment = format!(" # {}", action_ref.ref_str);

            // Create edit using value range for targeted replacement
            let range = loc.value_range();
            let edit = PendingEdit::replace_range(loc.line, range, new_uses.clone())
                .with_comment(comment.clone());

            if !file_edits.add(edit) {
                // Collision: two edits on same line
                eprintln!(
                    "{}: {}:{} - multiple actions on same line",
                    "Warning".yellow(),
                    relative,
                    loc.line
                );
                errors.record_user();
                continue;
            }

            // Print progress for human output
            if matches!(format, OutputFormat::Human) {
                println!(
                    "{}:{}: {} -> {}",
                    relative,
                    loc.line,
                    action_ref.to_string().yellow(),
                    format!("{new_uses}{comment}").cyan()
                );
            }
            total_pins += 1;
        }

        if !file_edits.is_empty() {
            files_to_write.push(file_edits);
        }
    }

    // Write all files
    for file_edits in &files_to_write {
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

    let result = EditResult {
        count: total_pins,
        errors,
    };

    result.print_summary(dry_run, "Pinned", "Would pin", format);

    // Pin command doesn't use "outdated" exit code - it's a deliberate action
    if result.errors.total() > 0 {
        result.errors.exit_code().into()
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_action_ref(
        host: Option<&str>,
        owner: &str,
        repo: &str,
        subpath: Option<&str>,
        ref_str: &str,
    ) -> ActionRef {
        ActionRef {
            host: host.map(String::from),
            owner: owner.to_string(),
            repo: repo.to_string(),
            subpath: subpath.map(String::from),
            ref_str: ref_str.to_string(),
        }
    }

    // ========================================================================
    // action_prefix tests
    // ========================================================================

    #[test]
    fn test_action_prefix_simple() {
        let action = make_action_ref(None, "actions", "checkout", None, "v4");
        assert_eq!(action_prefix(&action), "actions/checkout");
    }

    #[test]
    fn test_action_prefix_with_host() {
        let action = make_action_ref(Some("github.com"), "actions", "checkout", None, "v4");
        assert_eq!(action_prefix(&action), "github.com/actions/checkout");
    }

    #[test]
    fn test_action_prefix_with_subpath() {
        let action = make_action_ref(None, "actions", "cache", Some("restore"), "v4");
        assert_eq!(action_prefix(&action), "actions/cache/restore");
    }

    #[test]
    fn test_action_prefix_with_host_and_subpath() {
        let action = make_action_ref(
            Some("ghe.corp.com"),
            "org",
            "monorepo",
            Some(".github/actions/build"),
            "main",
        );
        assert_eq!(
            action_prefix(&action),
            "ghe.corp.com/org/monorepo/.github/actions/build"
        );
    }
}
