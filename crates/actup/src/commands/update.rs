//! Update command - update actions to their latest versions

use std::path::Path;

use actup_core::{
    ActionRef, CheckStatus, Config, UsesLocation, check_action_with_provider, provider, version,
};
use owo_colors::OwoColorize;

use crate::{
    cli::OutputFormat,
    common::{EditResult, ErrorTally, FileEdits, PendingEdit, WorkflowScanner, report_unplaceable},
};

/// Create an edit that replaces the ref of `loc` with `new_ref`.
fn ref_edit(loc: &UsesLocation, new_ref: &str) -> Option<PendingEdit> {
    loc.ref_range()
        .map(|range| PendingEdit::replace_range(loc.line(), range, new_ref.to_string()))
}

/// Build the edit for an outdated action, preserving its reference style.
///
/// Tag refs update to the new tag. SHA pins re-pin to the target's commit SHA
/// (never unpin to a tag) and keep a `# vX.Y.Z` version comment accurate.
fn build_edit(
    loc: &UsesLocation,
    action_ref: &ActionRef,
    target: &version::Info,
) -> Option<PendingEdit> {
    if action_ref.is_sha() {
        crate::common::repin_sha_edit(loc, target)
    } else {
        ref_edit(loc, &target.version().to_string())
    }
}

/// Pin a floating branch ref to its HEAD SHA, tagging it with a `# branch` comment.
fn pin_branch_edit(loc: &UsesLocation, ref_name: &str, sha: &str) -> Option<PendingEdit> {
    ref_edit(loc, sha).map(|edit| edit.with_comment(format!(" # {ref_name}")))
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
            let Some(edit) = edit else {
                report_unplaceable(&action_ref, &loc, &mut errors);
                continue;
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
    use std::{collections::HashMap, sync::Arc};

    use actup_core::{
        ActionRepoId, BoxFuture,
        provider::{ActionYml, Kind, Provider, Sha40},
    };

    use super::*;
    use crate::cli::CliExitCode;

    struct TagProvider;

    impl Provider for TagProvider {
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
            let old = Arc::new(version::Info::new(
                version::Version::parse("v4.0.0"),
                sha40('a'),
            ));
            let new = Arc::new(version::Info::new(
                version::Version::parse("v4.2.0"),
                sha40('b'),
            ));
            let set = version::Set::build(
                vec![Arc::clone(&old), Arc::clone(&new)],
                HashMap::from([(4, Arc::clone(&new))]),
                HashMap::new(),
                Some(new),
            );
            Box::pin(async move { Ok(set) })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
            let sha = Sha40::new(sha40('a')).unwrap_or_else(|| unreachable!());
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
    async fn test_run_reports_value_not_written_verbatim() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        let file = workflows.join("ci.yml");
        let content = "on: push\njobs:\n  b:\n    runs-on: x\n    steps:\n      - uses: \"actions/checkout@v4.0.\\x30\"\n      - uses: actions/checkout@v4.0.0\n";
        std::fs::write(&file, content)?;
        let mut registry = provider::Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(TagProvider));

        let code = run(
            dir.path(),
            false,
            OutputFormat::Json,
            &Config::default(),
            &registry,
        )
        .await;

        assert_eq!(code, i32::from(CliExitCode::UserError));
        assert_eq!(
            std::fs::read_to_string(&file)?,
            content.replace("checkout@v4.0.0\n", "checkout@v4.2.0\n")
        );
        Ok(())
    }

    fn make_action_ref(owner: &str, repo: &str, ref_str: &str) -> ActionRef {
        ActionRef::simple(owner, repo, ref_str)
    }

    // Version resolution tests now live in actup-core::resolution module.

    #[test]
    fn test_ref_edit_replaces_only_the_ref() {
        let line = "      - uses: actions/checkout@v4";
        let loc = UsesLocation::new(10, 15, "actions/checkout@v4".to_string(), line.to_string());
        let edit = ref_edit(&loc, "v5").unwrap();
        assert_eq!(edit.line, 10);
        assert_eq!(
            edit.apply(line).unwrap(),
            "      - uses: actions/checkout@v5"
        );
    }

    #[test]
    fn test_pin_branch_edit_pins_with_branch_comment() {
        let sha = sha40('c');
        let line = "      - uses: a/b@main";
        let loc = sha_loc(line);
        let edit = pin_branch_edit(&loc, "main", &sha).unwrap();
        assert_eq!(
            edit.apply(line).unwrap(),
            format!("      - uses: a/b@{sha} # main")
        );
    }

    #[test]
    fn test_edits_skip_value_not_written_verbatim() {
        let line = "      - uses: \"actions/checkout@v\\x34\"";
        let loc = UsesLocation::new(1, 16, "actions/checkout@v4".to_string(), line.to_string());
        let target = version::Info::new(version::Version::parse("v5.0.0"), sha40('a'));
        let tag = make_action_ref("actions", "checkout", "v4");
        let pinned = make_action_ref("actions", "checkout", &sha40('b'));
        assert!(build_edit(&loc, &tag, &target).is_none());
        assert!(build_edit(&loc, &pinned, &target).is_none());
        assert!(pin_branch_edit(&loc, "v4", &sha40('c')).is_none());
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
        let result = build_edit(&loc, &action_ref, &target)
            .unwrap()
            .apply(line)
            .unwrap();
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
        let result = build_edit(&loc, &action_ref, &target)
            .unwrap()
            .apply(&line)
            .unwrap();
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
            assert!(file_edits.add(build_edit(&loc, &action_ref, &target).unwrap()));
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
}
