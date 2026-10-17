//! Pin command - pin all actions to their current SHA

use std::path::Path;

use actup_core::{
    ActionRef, CheckStatus, Config, UsesLocation, check_action_with_provider, provider,
};
use owo_colors::OwoColorize;

use crate::{
    cli::OutputFormat,
    common::{EditResult, ErrorTally, FileEdits, PendingEdit, WorkflowScanner, repin_sha_edit},
};

/// Get the prefix of an action ref (everything before @ref).
fn action_prefix(action: &ActionRef) -> String {
    let mut s = String::new();
    if let Some(h) = action.host() {
        s.push_str(h);
        s.push('/');
    }
    s.push_str(action.owner());
    s.push('/');
    s.push_str(action.repo());
    if let Some(p) = action.subpath() {
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
    let repo_id = action_ref.to_repo_id().without_subpath();

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
        .semver_tags()
        .iter()
        .find(|v| v.version().to_string() == action_ref.ref_str())
    {
        return Some(v.sha().to_string());
    }

    // Try to resolve the ref directly (might be a branch)
    match provider
        .resolve_ref_to_sha(&repo_id, action_ref.ref_str())
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

/// Build the pin edit for one action, returning the edit plus a display string,
/// or `None` to skip (already pinned and current, or resolution failed).
///
/// SHA-pinned refs are re-pinned only when outdated (never unpinned); tag and
/// branch refs are pinned to their current commit SHA.
async fn pin_action_edit(
    provider: &dyn actup_core::Provider,
    action_ref: &ActionRef,
    loc: &UsesLocation,
    config: &Config,
    errors: &mut ErrorTally,
) -> Option<(PendingEdit, String)> {
    if action_ref.is_sha() {
        let repo_id = action_ref.to_repo_id().without_subpath();
        let versions = match provider.get_versions(&repo_id).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{}: {} - {}", "Warning".yellow(), action_ref, e);
                errors.record(&e);
                return None;
            }
        };
        let result = check_action_with_provider(action_ref, &versions, config, provider).await;
        let CheckStatus::Outdated { target, .. } = result.status() else {
            return None; // already pinned and current
        };
        let short = &target.sha()[..7.min(target.sha().len())];
        let display = format!("{} ({short})", target.version());
        Some((repin_sha_edit(loc, action_ref, target), display))
    } else {
        let sha = resolve_sha(provider, action_ref, errors).await?;
        let new_uses = format!("{}@{sha}", action_prefix(action_ref));
        let comment = format!(" # {}", action_ref.ref_str());
        let edit = PendingEdit::replace_range(loc.line(), loc.value_range(), new_uses.clone())
            .with_comment(comment.clone());
        Some((edit, format!("{new_uses}{comment}")))
    }
}

/// Pin all workflow actions to their current commit SHA for reproducibility.
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
    let mut total_pins = 0;
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

            let Some((edit, new_display)) =
                pin_action_edit(provider, &action_ref, &loc, config, &mut errors).await
            else {
                continue;
            };

            if !file_edits.add(edit) {
                // Collision: two edits on same line
                eprintln!(
                    "{}: {}:{} - multiple actions on same line",
                    "Warning".yellow(),
                    relative,
                    loc.line()
                );
                errors.record_user();
                continue;
            }

            // Print progress for human output
            if matches!(format, OutputFormat::Human) {
                println!(
                    "{}:{}: {} -> {}",
                    relative,
                    loc.line(),
                    action_ref.to_string().yellow(),
                    new_display.cyan()
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
    use actup_core::{
        ActionRepoId, BoxFuture,
        provider::{ActionYml, Kind, Provider, Sha40},
        version,
    };

    use super::*;
    use crate::common::FileEdits;

    struct FixedShaProvider {
        sha: Sha40,
    }

    impl Provider for FixedShaProvider {
        fn kind(&self) -> Kind {
            Kind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(
            &self,
            _repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<version::Set>> {
            Box::pin(async { Ok(version::Set::new()) })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
            let sha = self.sha.clone();
            Box::pin(async move { Ok(sha) })
        }

        fn get_default_branch(
            &self,
            _repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<String>> {
            Box::pin(async { Ok("main".to_string()) })
        }

        fn get_readme(
            &self,
            _repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> BoxFuture<'_, actup_core::Result<Option<String>>> {
            Box::pin(async { Ok(None) })
        }

        fn get_action_yml(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<ActionYml>> {
            Box::pin(async { Ok(ActionYml::default()) })
        }
    }

    #[tokio::test]
    async fn test_pin_flow_style_steps_resolves_exact_refs() {
        let sha = "c".repeat(40);
        let Some(sha40) = Sha40::new(sha.clone()) else {
            panic!("fixture SHA must be valid");
        };
        let provider = FixedShaProvider { sha: sha40 };
        let content = [
            "steps:",
            "  - { uses: actions/upload-pages-artifact@v4, with: { path: dist } }",
            "  - { uses: actions/deploy-pages@v4, id: deployment }",
            "",
        ]
        .join("\n");
        let expected = [
            "steps:",
            "  - { uses: actions/upload-pages-artifact@SHA, with: { path: dist } } # v4",
            "  - { uses: actions/deploy-pages@SHA, id: deployment } # v4",
            "",
        ]
        .join("\n")
        .replace("SHA", &sha);

        let config = Config::default();
        let mut errors = ErrorTally::default();
        let mut file_edits =
            FileEdits::new(std::path::PathBuf::from("/pages.yml"), content.clone());
        let mut refs = Vec::new();
        for (loc, action_ref) in WorkflowScanner::extract_actions(&content) {
            refs.push(action_ref.to_string());
            let Some((edit, _)) =
                pin_action_edit(&provider, &action_ref, &loc, &config, &mut errors).await
            else {
                panic!("pin produced no edit for {action_ref}");
            };
            assert!(file_edits.add(edit));
        }

        assert_eq!(
            refs,
            [
                "actions/upload-pages-artifact@v4",
                "actions/deploy-pages@v4"
            ]
        );
        assert_eq!(errors.total(), 0);
        let Ok(pinned) = file_edits.apply() else {
            panic!("edits did not apply");
        };
        assert_eq!(pinned, expected);
    }

    fn make_action_ref(
        host: Option<&str>,
        owner: &str,
        repo: &str,
        subpath: Option<&str>,
        ref_str: &str,
    ) -> ActionRef {
        ActionRef::new(
            host.map(String::from),
            owner,
            repo,
            subpath.map(String::from),
            ref_str,
        )
    }

    // action_prefix tests

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
