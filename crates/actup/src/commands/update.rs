//! Update command - update actions to their latest versions

use std::path::Path;

use actup_core::{
    ActionRef, CheckStatus, Config, UsesLocation, check_action_with_provider, provider, version,
};
use owo_colors::OwoColorize;

use crate::{
    cli::OutputFormat,
    common::{
        Change, EditResult, FileEdits, PendingEdit, RefSite, SkipReason, WorkflowScanner,
        commit_file, report_error, report_skipped, report_unserved,
    },
};

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

fn new_ref(action_ref: &ActionRef, target: &version::Info) -> String {
    if action_ref.is_sha() {
        target.sha().to_string()
    } else {
        target.version().to_string()
    }
}

fn target_display(action_ref: &ActionRef, target: &version::Info) -> String {
    if action_ref.is_sha() {
        let short = &target.sha()[..7.min(target.sha().len())];
        format!("{} ({short})", target.version())
    } else {
        target.version().to_string()
    }
}

/// Pin a floating branch ref to its HEAD SHA, tagging it with a `# branch` comment.
fn pin_branch_edit(loc: &UsesLocation, ref_name: &str, sha: &str) -> Option<PendingEdit> {
    ref_edit(loc, sha).map(|edit| edit.with_comment(format!(" # {ref_name}")))
}

pub async fn run(
    path: &Path,
    dry_run: bool,
    format: OutputFormat,
    config: &Config,
    registry: &provider::Registry,
) -> i32 {
    let Some(result) = edit(path, dry_run, format, config, registry).await else {
        EditResult::default().print_report(dry_run, format);
        return 0;
    };
    result.print_summary(dry_run, "Updated", "Would update", format);
    result.exit_code(dry_run)
}

async fn edit(
    path: &Path,
    dry_run: bool,
    format: OutputFormat,
    config: &Config,
    registry: &provider::Registry,
) -> Option<EditResult> {
    let scanner = WorkflowScanner::new(path, config, registry, format);
    let discovered = scanner.discover();

    if discovered.is_empty() {
        scanner.print_empty_message();
        return None;
    }

    let mut result = EditResult::default();

    for file in &discovered {
        let Some(content) = scanner.read_file(file, &mut result.errors) else {
            continue;
        };

        let relative = scanner.relative_path(file);
        let mut file_edits = FileEdits::new(file.path().to_path_buf(), content.clone());

        for (loc, action_ref) in WorkflowScanner::extract_actions(&content) {
            let Some(provider) = scanner.provider_for(&action_ref) else {
                report_unserved(&scanner, &relative, loc.line(), &action_ref, &mut result);
                continue;
            };

            let site = RefSite::new(&relative, loc.line(), &action_ref, provider);
            let repo_id = action_ref.to_repo_id().without_subpath();
            let versions = match provider.get_versions(&repo_id).await {
                Ok(v) => v,
                Err(e) => {
                    report_error(site, &e, &mut result);
                    continue;
                }
            };

            // Use core check_action to determine if update needed
            let check = check_action_with_provider(&action_ref, &versions, config, provider).await;
            let (edit, new_ref, target, show_comment) = match check.status() {
                CheckStatus::Outdated { target, .. } => (
                    build_edit(&loc, &action_ref, target),
                    new_ref(&action_ref, target),
                    target_display(&action_ref, target),
                    false,
                ),
                CheckStatus::PinBranch { ref_name, sha } => (
                    pin_branch_edit(&loc, ref_name, sha),
                    sha.clone(),
                    sha[..7.min(sha.len())].to_string(),
                    true,
                ),
                _ => continue, // Not actionable, skip
            };
            let Some(edit) = edit else {
                report_skipped(site, SkipReason::Unplaceable, &mut result);
                continue;
            };
            let change = Change {
                site,
                new_ref,
                target,
                show_comment,
            };
            file_edits.add(edit, change);
        }

        for (change, comment) in commit_file(file_edits, dry_run, &mut result) {
            if matches!(format, OutputFormat::Human) {
                println!(
                    "{}:{}: {} -> {}",
                    relative,
                    change.site.line,
                    change.site.reference.red(),
                    change.written(comment.as_deref()).green()
                );
            }
        }
    }

    Some(result)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use actup_core::{
        ActionRepoId, BoxFuture,
        provider::{ActionYml, Kind, Provider, Sha40},
    };

    use super::*;
    use crate::{
        cli::CliExitCode,
        common::{EditOutcome, ErrorKind},
    };

    struct TagProvider {
        latest: &'static str,
    }

    impl Provider for TagProvider {
        fn kind(&self) -> Kind {
            Kind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(
            &self,
            repo: &ActionRepoId,
        ) -> BoxFuture<'_, actup_core::Result<version::Set>> {
            if repo.repo() == "missing" {
                let err = actup_core::Error::NotFound {
                    owner: repo.owner().to_string(),
                    repo: repo.repo().to_string(),
                    host: None,
                };
                return Box::pin(async move { Err(err) });
            }
            let old = Arc::new(version::Info::new(
                version::Version::parse("v4.0.0"),
                sha40('a'),
            ));
            let new = Arc::new(version::Info::new(
                version::Version::parse(self.latest),
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
    async fn test_run_skips_value_not_written_verbatim() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        let file = workflows.join("ci.yml");
        let content = "on: push\njobs:\n  b:\n    runs-on: x\n    steps:\n      - uses: \"actions/checkout@v4.0.\\x30\"\n      - uses: actions/checkout@v4.0.0\n";
        std::fs::write(&file, content)?;

        let code = run(
            dir.path(),
            false,
            OutputFormat::Json,
            &Config::default(),
            &registry("v4.2.0"),
        )
        .await;

        assert_eq!(code, i32::from(CliExitCode::Outdated));
        assert_eq!(
            std::fs::read_to_string(&file)?,
            content.replace("checkout@v4.0.0\n", "checkout@v4.2.0\n")
        );
        Ok(())
    }

    fn make_action_ref(owner: &str, repo: &str, ref_str: &str) -> ActionRef {
        ActionRef::simple(owner, repo, ref_str)
    }

    fn registry(latest: &'static str) -> provider::Registry {
        let mut registry = provider::Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(TagProvider { latest }));
        registry
    }

    #[tokio::test]
    async fn test_run_updates_every_action_on_a_line_in_one_pass()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        let file = workflows.join("ci.yml");
        let content = [
            "on: push",
            "jobs:",
            "  two:",
            "    runs-on: x",
            "    steps: [{ uses: actions/checkout@v4.0.0 }, { uses: \"o/b@v4.0.0\" }]",
            "  three:",
            "    runs-on: x",
            "    steps: [{ uses: o/c@v4.0.0 }, { uses: 'o/d@v4.0.0' }, { uses: o/e@OLD }] # note",
            "  single:",
            "    runs-on: x",
            "    steps:",
            "      - uses: o/f@OLD",
            "      - uses: o/g@v4.0.0 # mine",
            "",
        ]
        .join("\n")
        .replace("OLD", &sha40('a'));
        let expected = [
            "on: push",
            "jobs:",
            "  two:",
            "    runs-on: x",
            "    steps: [{ uses: actions/checkout@v4.10.1 }, { uses: \"o/b@v4.10.1\" }]",
            "  three:",
            "    runs-on: x",
            "    steps: [{ uses: o/c@v4.10.1 }, { uses: 'o/d@v4.10.1' }, { uses: o/e@NEW }] # note",
            "  single:",
            "    runs-on: x",
            "    steps:",
            "      - uses: o/f@NEW # v4.10.1",
            "      - uses: o/g@v4.10.1 # mine",
            "",
        ]
        .join("\n")
        .replace("NEW", &sha40('b'));
        std::fs::write(&file, &content)?;
        let registry = registry("v4.10.1");
        let config = Config::default();

        let code = run(dir.path(), false, OutputFormat::Human, &config, &registry).await;
        assert_eq!(code, i32::from(CliExitCode::Success));
        assert_eq!(std::fs::read_to_string(&file)?, expected);

        let code = run(dir.path(), false, OutputFormat::Human, &config, &registry).await;
        assert_eq!(code, i32::from(CliExitCode::Success));
        assert_eq!(std::fs::read_to_string(&file)?, expected);
        Ok(())
    }

    async fn assert_update_writes(
        content: &str,
        expected: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        let file = workflows.join("ci.yml");
        std::fs::write(&file, content)?;
        let registry = registry("v4.2.0");
        let config = Config::default();

        for _ in 0..2 {
            let code = run(dir.path(), false, OutputFormat::Human, &config, &registry).await;
            assert_eq!(code, i32::from(CliExitCode::Success));
            assert_eq!(std::fs::read(&file)?, expected.as_bytes());
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_run_changes_only_ref_bytes_in_a_crlf_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let content = "on: push\r\njobs:\r\n  b:\r\n    runs-on: x\r\n    steps:\r\n      - uses: actions/checkout@v4.0.0\r\n      - uses: o/p@OLD\r\n      - { uses: \"o/q@v4.0.0\" } # keep\r\n"
            .replace("OLD", &sha40('a'));
        let expected = "on: push\r\njobs:\r\n  b:\r\n    runs-on: x\r\n    steps:\r\n      - uses: actions/checkout@v4.2.0\r\n      - uses: o/p@NEW # v4.2.0\r\n      - { uses: \"o/q@v4.2.0\" } # keep\r\n"
            .replace("NEW", &sha40('b'));
        assert_update_writes(&content, &expected).await
    }

    #[tokio::test]
    async fn test_run_keeps_mixed_line_breaks_and_the_bom() -> Result<(), Box<dyn std::error::Error>>
    {
        let content = "\u{feff}on: push\r\njobs:\n  b:\r    runs-on: x\r\n    steps:\n      - uses: o/p@OLD\r      - uses: o/q@v4.0.0\r\n      - uses: o/r@v4.0.0"
            .replace("OLD", &sha40('a'));
        let expected = "\u{feff}on: push\r\njobs:\n  b:\r    runs-on: x\r\n    steps:\n      - uses: o/p@NEW # v4.2.0\r      - uses: o/q@v4.2.0\r\n      - uses: o/r@v4.2.0"
            .replace("NEW", &sha40('b'));
        assert_update_writes(&content, &expected).await
    }

    #[tokio::test]
    async fn test_edit_records_edited_and_errored_refs() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        let content = "on: push\njobs:\n  b:\n    runs-on: x\n    steps:\n      - uses: actions/checkout@v4.0.0\n      - uses: o/missing@v4.0.0\n      - uses: o/p@OLD # v4.0.0\n      - uses: ghe.corp.example/o/b@v1\n"
            .replace("OLD", &sha40('a'));
        std::fs::write(workflows.join("ci.yml"), &content)?;

        let Some(result) = edit(
            dir.path(),
            false,
            OutputFormat::Json,
            &Config::default(),
            &registry("v4.2.0"),
        )
        .await
        else {
            panic!("no workflow found");
        };

        let edited: Vec<(usize, &str, &str, &str, Option<&str>)> = result
            .edited
            .iter()
            .map(|e| {
                (
                    e.site.line,
                    e.site.host.as_str(),
                    e.site.reference.as_str(),
                    e.to.as_str(),
                    e.comment.as_deref(),
                )
            })
            .collect();
        let old = sha40('a');
        let new = sha40('b');
        assert_eq!(
            edited,
            [
                (6, "github.com", "v4.0.0", "v4.2.0", None),
                (
                    8,
                    "github.com",
                    old.as_str(),
                    new.as_str(),
                    Some("# v4.2.0")
                ),
            ]
        );
        assert!(result.edited.iter().all(|e| e.site.provider == "github"));
        let errored: Vec<(usize, &str, ErrorKind)> = result
            .errored
            .iter()
            .map(|e| (e.site.line, e.site.action.as_str(), e.kind))
            .collect();
        assert_eq!(
            errored,
            [
                (7, "o/missing@v4.0.0", ErrorKind::NotFound),
                (9, "ghe.corp.example/o/b@v1", ErrorKind::ProviderNotFound),
            ]
        );
        assert_eq!(
            result.exit_code(false),
            i32::from(CliExitCode::EnvironmentError)
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_run_without_workflows_reports_nothing_and_succeeds()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config = Config::default();
        let registry = registry("v4.2.0");
        assert!(
            edit(dir.path(), false, OutputFormat::Json, &config, &registry)
                .await
                .is_none()
        );
        for format in [OutputFormat::Human, OutputFormat::Json, OutputFormat::Toml] {
            assert_eq!(run(dir.path(), false, format, &config, &registry).await, 0);
        }
        Ok(())
    }

    fn apply_at(line_body: &str, edit: PendingEdit) -> Result<String, Box<dyn std::error::Error>> {
        let content = format!("{}{line_body}", "\n".repeat(edit.line - 1));
        let mut file_edits = FileEdits::new(std::path::PathBuf::from("/test.yml"), content);
        file_edits.add(edit, ());
        let plan = file_edits.plan();
        if !matches!(plan.entries.as_slice(), [((), EditOutcome::Applied { .. })]) {
            return Err(format!("edit did not apply: {:?}", plan.entries).into());
        }
        Ok(plan.content.trim_start_matches('\n').to_string())
    }

    #[test]
    fn test_ref_edit_replaces_only_the_ref() -> Result<(), Box<dyn std::error::Error>> {
        let line = "      - uses: actions/checkout@v4";
        let loc = UsesLocation::new(10, 15, "actions/checkout@v4".to_string(), line.to_string());
        let edit = ref_edit(&loc, "v5").ok_or("no edit")?;
        assert_eq!(edit.line, 10);
        assert_eq!(apply_at(line, edit)?, "      - uses: actions/checkout@v5");
        Ok(())
    }

    #[test]
    fn test_pin_branch_edit_pins_with_branch_comment() -> Result<(), Box<dyn std::error::Error>> {
        let sha = sha40('c');
        let line = "      - uses: a/b@main";
        let loc = sha_loc(line)?;
        let edit = pin_branch_edit(&loc, "main", &sha).ok_or("no edit")?;
        assert_eq!(
            apply_at(line, edit)?,
            format!("      - uses: a/b@{sha} # main")
        );
        Ok(())
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

    #[test]
    fn test_target_display_names_the_version_and_a_sha_pin() {
        let target = version::Info::new(version::Version::parse("v5.0.0"), sha40('b'));
        assert_eq!(
            target_display(&make_action_ref("a", "b", "v4"), &target),
            "v5.0.0"
        );
        assert_eq!(
            target_display(&make_action_ref("a", "b", &sha40('a')), &target),
            "v5.0.0 (bbbbbbb)"
        );
    }

    fn sha40(byte: char) -> String {
        byte.to_string().repeat(40)
    }

    fn sha_loc(line_body: &str) -> Result<UsesLocation, Box<dyn std::error::Error>> {
        let value = line_body
            .split("uses: ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .ok_or("line has no `uses:` value")?;
        let column = line_body.find(value).ok_or("value not in line")? + 1;
        Ok(UsesLocation::new(
            1,
            column,
            value.to_string(),
            line_body.to_string(),
        ))
    }

    #[test]
    fn test_build_edit_tag_writes_tag() -> Result<(), Box<dyn std::error::Error>> {
        let line = "      - uses: actions/checkout@v4";
        let loc = sha_loc(line)?;
        let action_ref = make_action_ref("actions", "checkout", "v4");
        let target = version::Info::new(version::Version::parse("v5.0.0"), sha40('a'));
        let edit = build_edit(&loc, &action_ref, &target).ok_or("no edit")?;
        assert_eq!(
            apply_at(line, edit)?,
            "      - uses: actions/checkout@v5.0.0"
        );
        Ok(())
    }

    #[test]
    fn test_build_edit_sha_repins_to_sha_with_comment() -> Result<(), Box<dyn std::error::Error>> {
        let old = sha40('a');
        let new = sha40('b');
        let line = format!("      - uses: actions/checkout@{old}");
        let loc = sha_loc(&line)?;
        let action_ref = make_action_ref("actions", "checkout", &old);
        let target = version::Info::new(version::Version::parse("v7.0.1"), &new);
        let edit = build_edit(&loc, &action_ref, &target).ok_or("no edit")?;
        assert_eq!(
            apply_at(&line, edit)?,
            format!("      - uses: actions/checkout@{new} # v7.0.1")
        );
        Ok(())
    }

    #[test]
    fn test_flow_style_update_round_trip_changes_only_refs()
    -> Result<(), Box<dyn std::error::Error>> {
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
            file_edits.add(build_edit(&loc, &action_ref, &target).ok_or("no edit")?, ());
            seen += 1;
        }
        assert_eq!(seen, 6);

        let updated = file_edits.plan().content;
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
        Ok(())
    }
}
