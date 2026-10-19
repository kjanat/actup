//! Update command - update actions to their latest versions

use anstream::println;
use std::{path::Path, sync::Arc};

use actup_core::{
    ActionRef, CheckStatus, Config, FileCache, check_action_with_comment,
    patch::{FileEdits, SkipReason, pin_edit, retarget_edit},
    policy, provider,
    rewrite::{Labels, Named},
    sanitize_line, version,
};
use owo_colors::OwoColorize;

use crate::{
    cli::{CliExitCode, OutputFormat},
    common::{
        Change, EditResult, WorkflowScanner, commit_file, report_invalid, report_skipped,
        report_unwritable, writable_versions,
    },
    scan::{Scan, versions_for},
};

fn new_ref(action_ref: &ActionRef, target: &version::Info, style: policy::Style) -> String {
    if style.writes_sha(action_ref.is_sha()) {
        target.sha().to_string()
    } else {
        target.version().to_string()
    }
}

fn target_display(action_ref: &ActionRef, target: &version::Info, style: policy::Style) -> String {
    if style.writes_sha(action_ref.is_sha()) {
        let short = &target.sha()[..7.min(target.sha().len())];
        format!("{} ({short})", target.version())
    } else {
        target.version().to_string()
    }
}

pub async fn run(
    path: &Path,
    dry_run: bool,
    format: OutputFormat,
    config: &Config,
    registry: &provider::Registry,
    cache: &Arc<FileCache>,
) -> CliExitCode {
    let Some(result) = edit(path, dry_run, format, config, registry, cache).await else {
        EditResult::default().print_report(dry_run, format);
        return CliExitCode::Success;
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
    cache: &Arc<FileCache>,
) -> Option<EditResult> {
    let scanner = WorkflowScanner::new(path, config, registry, format);
    let discovered = scanner.discover();

    if discovered.is_empty() {
        scanner.print_empty_message();
        return None;
    }

    let mut result = EditResult::default();
    let scan = Scan::collect(&scanner, &discovered, &mut result.errors);
    let versions = scan
        .fetch(&scanner, config, registry, cache, |_| true)
        .await;

    for (index, file) in scan.files.iter().enumerate() {
        let relative = &file.relative;
        let mut file_edits = FileEdits::new(file.path.clone(), file.content.clone(), file.format);

        for bad in scan.invalid_in(index) {
            report_invalid(
                &scanner,
                relative,
                bad.loc.line(),
                &bad.invalid,
                &mut result,
            );
        }
        for scanned in scan.refs_in(index) {
            let Some((provider, site)) = scanner.editable(file, scanned, &mut result) else {
                continue;
            };
            let loc = file_edits.writable(&scanned.loc);
            let (loc, action_ref) = (&loc, &scanned.action_ref);
            let versions = match writable_versions(versions_for(&versions, &scanned.repo_id)) {
                Ok(v) => v,
                Err(why) => {
                    report_unwritable(site, why, &mut result);
                    continue;
                }
            };

            // Use core check_action to determine if update needed
            let check =
                check_action_with_comment(action_ref, loc.comment(), versions, config, provider)
                    .await;
            let style = config.policy_for(action_ref).pin;
            let labels = Labels::fetched(action_ref.ref_str(), versions);
            let (edit, new_ref, target, show_comment) = match check.status() {
                CheckStatus::Outdated { target, .. } => (
                    retarget_edit(
                        loc,
                        action_ref.is_sha(),
                        target,
                        Named::listed(&target.version().to_string(), versions),
                        style,
                        &labels,
                        config.pin.comment,
                    ),
                    new_ref(action_ref, target, style),
                    target_display(action_ref, target, style),
                    false,
                ),
                CheckStatus::PinBranch { ref_name, sha } => (
                    pin_edit(loc, sha, ref_name, &labels, config.pin.comment),
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

        for (change, comment) in commit_file(file_edits, path, dry_run, &mut result) {
            if matches!(format, OutputFormat::Human) {
                println!(
                    "{}:{}: {} -> {}",
                    sanitize_line(relative),
                    change.site.line,
                    sanitize_line(&change.site.reference).red(),
                    sanitize_line(&change.written(comment.as_deref())).green()
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
        ActionRepoId, BoxFuture, SourceFormat, UsesLocation,
        patch::{EditOutcome, PendingEdit},
        provider::{ActionYml, Kind, Provider, Sha40},
    };

    use super::*;
    use crate::{
        cli::CliExitCode,
        common::{ErrorKind, SkipCause},
    };
    use actup_core::Access;

    fn uncached() -> Config {
        Config {
            cache: actup_core::config::Cache {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    struct TagProvider {
        latest: &'static str,
        rewrite: Option<(std::path::PathBuf, String)>,
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
            if let Some((file, content)) = &self.rewrite
                && let Err(e) = std::fs::write(file, content)
            {
                panic!("rewrite {}: {e}", file.display());
            }
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
            let mut set = version::Set::build(
                vec![Arc::clone(&old), Arc::clone(&new)],
                HashMap::from([(4, Arc::clone(&new))]),
                HashMap::new(),
                Some(new),
            );
            set.set_branches(HashMap::from([("dev".to_string(), sha40('c'))]));
            let set = match actup_core::cache::Staleness::after(
                &actup_core::Error::RateLimited {
                    provider: "github".to_string(),
                    retry_after: None,
                },
                std::time::Duration::from_hours(26),
            ) {
                Some(stale) if repo.repo() == "stale" => set.into_stale(stale),
                _ => set,
            };
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
            &uncached(),
            &registry("v4.2.0"),
            &Arc::new(FileCache::from_config(&uncached(), Access::ReadWrite)),
        )
        .await;

        assert_eq!(code, CliExitCode::Outdated);
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
        registry.register(Box::new(TagProvider {
            latest,
            rewrite: None,
        }));
        registry
    }

    #[tokio::test]
    async fn test_run_refuses_a_file_changed_during_lookup()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        let file = workflows.join("ci.yml");
        let changed = "on: push\njobs:\n  b:\n    runs-on: x\n    steps:\n      - uses: actions/checkout@main\n";
        std::fs::write(&file, changed.replace("@main", "@v4.0.0"))?;
        let mut registry = provider::Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(TagProvider {
            latest: "v4.2.0",
            rewrite: Some((file.clone(), changed.to_string())),
        }));

        let Some(result) = edit(
            dir.path(),
            false,
            OutputFormat::Json,
            &uncached(),
            &registry,
            &Arc::new(FileCache::from_config(&uncached(), Access::ReadWrite)),
        )
        .await
        else {
            panic!("no workflow found");
        };

        assert_eq!(result.edited, []);
        let kinds: Vec<ErrorKind> = result.errored.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [ErrorKind::Changed]);
        assert_eq!(result.exit_code(false), CliExitCode::EnvironmentError);
        assert_eq!(std::fs::read_to_string(&file)?, changed);
        Ok(())
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
        let config = uncached();

        let code = run(
            dir.path(),
            false,
            OutputFormat::Human,
            &config,
            &registry,
            &Arc::new(FileCache::from_config(&config, Access::ReadWrite)),
        )
        .await;
        assert_eq!(code, CliExitCode::Success);
        assert_eq!(std::fs::read_to_string(&file)?, expected);

        let code = run(
            dir.path(),
            false,
            OutputFormat::Human,
            &config,
            &registry,
            &Arc::new(FileCache::from_config(&config, Access::ReadWrite)),
        )
        .await;
        assert_eq!(code, CliExitCode::Success);
        assert_eq!(std::fs::read_to_string(&file)?, expected);
        Ok(())
    }

    async fn update_with_config(
        toml: &str,
        content: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        update_with_config_runs(toml, content, 2).await
    }

    async fn update_with_config_runs(
        toml: &str,
        content: &str,
        runs: usize,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        let file = workflows.join("ci.yml");
        std::fs::write(&file, content)?;
        let config = Config::from_contents(
            Some((
                Path::new("actup.toml"),
                &format!("{toml}\n[cache]\nenabled = false\n"),
            )),
            actup_core::config::EnvOverrides::default(),
        )?;
        let registry = registry("v4.2.0");

        let code = run(
            dir.path(),
            false,
            OutputFormat::Json,
            &config,
            &registry,
            &Arc::new(FileCache::from_config(&config, Access::ReadWrite)),
        )
        .await;
        assert_eq!(code, CliExitCode::Success);
        let written = std::fs::read_to_string(&file)?;
        for _ in 1..runs {
            let code = run(
                dir.path(),
                false,
                OutputFormat::Json,
                &config,
                &registry,
                &Arc::new(FileCache::from_config(&config, Access::ReadWrite)),
            )
            .await;
            assert_eq!(code, CliExitCode::Success);
            assert_eq!(std::fs::read_to_string(&file)?, written);
        }
        Ok(written)
    }

    #[tokio::test]
    async fn update_branch_pins_in_parallel_steps_is_idempotent()
    -> Result<(), Box<dyn std::error::Error>> {
        let before = format!(
            "jobs:\n  build:\n    steps:\n      - parallel:\n          - uses: 'o/r@{}' # dev keep this\n          - {{ uses: o/r@{} }} # dev\n",
            sha40('a'),
            sha40('c')
        );
        let written = update_with_config_runs("", &before, 2).await?;
        assert_eq!(written, before.replace(&sha40('a'), &sha40('c')));
        Ok(())
    }

    fn pin_style_workflow() -> String {
        [
            "steps:",
            "  - uses: actions/checkout@v4.0.0",
            "  - uses: o/p@OLD # v4.0.0",
            "  - uses: o/q@OLD",
            "  - uses: o/r@OLD # mine",
            "  - uses: pinned/x@v4.0.0",
            "",
        ]
        .join("\n")
        .replace("OLD", &sha40('a'))
    }

    #[tokio::test]
    async fn test_run_pin_preserve_keeps_each_ref_style() -> Result<(), Box<dyn std::error::Error>>
    {
        let written =
            update_with_config("[policy]\npin = \"preserve\"\n", &pin_style_workflow()).await?;
        let expected = [
            "steps:",
            "  - uses: actions/checkout@v4.2.0",
            "  - uses: o/p@NEW # v4.2.0",
            "  - uses: o/q@NEW # v4.2.0",
            "  - uses: o/r@NEW # mine",
            "  - uses: pinned/x@v4.2.0",
            "",
        ]
        .join("\n")
        .replace("NEW", &sha40('b'));
        assert_eq!(written, expected);
        Ok(())
    }

    #[tokio::test]
    async fn test_run_pin_sha_writes_shas_with_version_comments()
    -> Result<(), Box<dyn std::error::Error>> {
        let written =
            update_with_config("[policy]\npin = \"sha\"\n", &pin_style_workflow()).await?;
        let expected = [
            "steps:",
            "  - uses: actions/checkout@NEW # v4.2.0",
            "  - uses: o/p@NEW # v4.2.0",
            "  - uses: o/q@NEW # v4.2.0",
            "  - uses: o/r@NEW # mine",
            "  - uses: pinned/x@NEW # v4.2.0",
            "",
        ]
        .join("\n")
        .replace("NEW", &sha40('b'));
        assert_eq!(written, expected);
        Ok(())
    }

    #[tokio::test]
    async fn update_without_pin_comments_preserves_notes_and_quotes()
    -> Result<(), Box<dyn std::error::Error>> {
        let content = format!(
            "steps:\n  - uses: actions/checkout@v4.0.0\n  - uses: \"o/p@{}\" # v4.0.0 keep\n  - {{ uses: o/q@v4.0.0 }} # v4.0.0\n  - uses: o/r@v4.0.0 # mine\n",
            sha40('a')
        );
        let written = update_with_config(
            "[policy]\npin = \"sha\"\n[pin]\ncomment = false\n",
            &content,
        )
        .await?;
        let expected = format!(
            "steps:\n  - uses: actions/checkout@{0}\n  - uses: \"o/p@{0}\" # keep\n  - {{ uses: o/q@{0} }}\n  - uses: o/r@{0} # mine\n",
            sha40('b')
        );
        assert_eq!(written, expected);
        let branches = update_with_config_runs(
            "[policy]\nbranch = \"update\"\n[pin]\ncomment = false\n",
            "steps:\n  - uses: o/a@main # main keep\n",
            1,
        )
        .await?;
        assert_eq!(
            branches,
            format!("steps:\n  - uses: o/a@{} # keep\n", sha40('a'))
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_run_writes_no_comment_after_other_flow_keys()
    -> Result<(), Box<dyn std::error::Error>> {
        let content = [
            "steps:",
            "  - { uses: o/s@OLD, with: { x: 1 } }",
            "  - { uses: o/t@v4.0.0, id: x }",
            "  - { uses: \"o/u@OLD\" } # v4.0.0",
            "  - { uses: o/v@OLD, with: { x: 1 } } # v4.0.0",
            "  - { uses: o/w@OLD, with: { msg: it's } } # v4.0.0",
            "  - { uses: o/x@OLD, with: { a: it's, b: 'x # v9' } }",
            "  - { uses: o/y@OLD, with: { b: 'it''s # v9' } }",
            "  - { uses: o/z@OLD, with: { b: 'it''s # x' } } # v4.0.0",
            "  - { uses: o/q@OLD, with: { b: !!str 'x # v9' } }",
            "  - uses: o/r@OLD   ",
            "  - uses: \"o/p@OLD\" \t",
            "  - { uses: o/o@OLD }  ",
            "  - uses: o/n@OLD # v4.0.0  ",
            "",
        ]
        .join("\n")
        .replace("OLD", &sha40('a'));
        let written = update_with_config("[policy]\npin = \"sha\"\n", &content).await?;
        let expected = [
            "steps:",
            "  - { uses: o/s@NEW, with: { x: 1 } }",
            "  - { uses: o/t@NEW, id: x }",
            "  - { uses: \"o/u@NEW\" } # v4.2.0",
            "  - { uses: o/v@NEW, with: { x: 1 } } # v4.2.0",
            "  - { uses: o/w@NEW, with: { msg: it's } } # v4.2.0",
            "  - { uses: o/x@NEW, with: { a: it's, b: 'x # v9' } }",
            "  - { uses: o/y@NEW, with: { b: 'it''s # v9' } }",
            "  - { uses: o/z@NEW, with: { b: 'it''s # x' } } # v4.2.0",
            "  - { uses: o/q@NEW, with: { b: !!str 'x # v9' } }",
            "  - uses: o/r@NEW # v4.2.0",
            "  - uses: \"o/p@NEW\" # v4.2.0",
            "  - { uses: o/o@NEW } # v4.2.0",
            "  - uses: o/n@NEW # v4.2.0",
            "",
        ]
        .join("\n")
        .replace("NEW", &sha40('b'));
        assert_eq!(written, expected);
        Ok(())
    }

    #[tokio::test]
    async fn test_run_pins_branches_with_the_editor_comment_rule()
    -> Result<(), Box<dyn std::error::Error>> {
        let content = [
            "steps:",
            "  - uses: o/a@main",
            "  - uses: o/b@main # v1.0.0",
            "  - uses: \"o/c@main\" # v1.0.0",
            "  - uses: 'o/d@main'",
            "  - { uses: o/e@main } # v1.0.0",
            "  - { uses: o/f@main, with: { x: 1 } }",
            "  - { uses: 'o/g@main', with: { b: 'x # v9' } }",
            "  - { uses: o/h@main, with: { b: !!str 'x # v9' } }",
            "  - uses: o/i@main # keep me",
            "  - uses: o/j@main   ",
            "",
        ]
        .join("\n");
        let written =
            update_with_config_runs("[policy]\nbranch = \"update\"\n", &content, 1).await?;
        let expected = [
            "steps:",
            "  - uses: o/a@PIN # main",
            "  - uses: o/b@PIN # main",
            "  - uses: \"o/c@PIN\" # main",
            "  - uses: 'o/d@PIN' # main",
            "  - { uses: o/e@PIN } # main",
            "  - { uses: o/f@PIN, with: { x: 1 } }",
            "  - { uses: 'o/g@PIN', with: { b: 'x # v9' } }",
            "  - { uses: o/h@PIN, with: { b: !!str 'x # v9' } }",
            "  - uses: o/i@PIN # keep me",
            "  - uses: o/j@PIN # main",
            "",
        ]
        .join("\n")
        .replace("PIN", &sha40('a'));
        assert_eq!(written, expected);
        Ok(())
    }

    #[tokio::test]
    async fn test_run_comments_lines_with_a_hash_before_the_value()
    -> Result<(), Box<dyn std::error::Error>> {
        let old = sha40('a');
        let new = sha40('b');
        let line = |r: &str, comment: &str| {
            format!("steps:\n  - {{ name: \"Build #1\", uses: o/a@{r} }}{comment}\n")
        };
        for (toml, before, after) in [
            (
                "[policy]\npin = \"preserve\"\n",
                line(&old, ""),
                line(&new, " # v4.2.0"),
            ),
            (
                "[policy]\npin = \"sha\"\n",
                line("v4.0.0", ""),
                line(&new, " # v4.2.0"),
            ),
            (
                "[policy]\npin = \"tag\"\n",
                line(&old, " # v4.0.0"),
                line("v4.2.0", ""),
            ),
        ] {
            assert_eq!(update_with_config(toml, &before).await?, after, "{toml}");
        }
        let written =
            update_with_config_runs("[policy]\nbranch = \"update\"\n", &line("main", ""), 1)
                .await?;
        assert_eq!(written, line(&old, " # main"));
        Ok(())
    }

    #[tokio::test]
    async fn test_run_rewrites_labels_after_named_refs() -> Result<(), Box<dyn std::error::Error>> {
        let content = [
            "steps:",
            "  - uses: o/a@v4.0.0 # v4.0.0 keep",
            "  - uses: o/b@v4.0.0 # lint keep",
            "  - uses: o/c@OLD # dev keep",
            "  - uses: o/d@OLD # TODO fix",
            "  - { uses: \"o/e@v4.0.0\" } # v4.0.0 keep",
            "",
        ]
        .join("\n")
        .replace("OLD", &sha40('a'));
        let expected = [
            "steps:",
            "  - uses: o/a@v4.2.0 # v4.2.0 keep",
            "  - uses: o/b@v4.2.0 # lint keep",
            "  - uses: o/c@BRANCH # dev keep",
            "  - uses: o/d@NEW # TODO fix",
            "  - { uses: \"o/e@v4.2.0\" } # v4.2.0 keep",
            "",
        ]
        .join("\n")
        .replace("NEW", &sha40('b'))
        .replace("BRANCH", &sha40('c'));
        let written = update_with_config("[policy]\npin = \"preserve\"\n", &content).await?;
        assert_eq!(written, expected);

        let pinned = format!("steps:\n  - uses: o/f@{} # v4.0.0 keep\n", sha40('a'));
        let written = update_with_config("[policy]\npin = \"tag\"\n", &pinned).await?;
        assert_eq!(written, "steps:\n  - uses: o/f@v4.2.0 # keep\n");

        let branches = "steps:\n  - uses: o/g@main # main keep\n  - uses: o/h@main # v1.0.0 keep\n  - uses: o/i@main # lint keep\n";
        let written =
            update_with_config_runs("[policy]\nbranch = \"update\"\n", branches, 1).await?;
        assert_eq!(
            written,
            "steps:\n  - uses: o/g@PIN # main keep\n  - uses: o/h@PIN # main keep\n  - uses: o/i@PIN # lint keep\n"
                .replace("PIN", &sha40('a'))
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_run_pin_tag_unpins_outdated_shas() -> Result<(), Box<dyn std::error::Error>> {
        let written =
            update_with_config("[policy]\npin = \"tag\"\n", &pin_style_workflow()).await?;
        let expected = [
            "steps:",
            "  - uses: actions/checkout@v4.2.0",
            "  - uses: o/p@v4.2.0",
            "  - uses: o/q@v4.2.0",
            "  - uses: o/r@v4.2.0 # mine",
            "  - uses: pinned/x@v4.2.0",
            "",
        ]
        .join("\n");
        assert_eq!(written, expected);
        Ok(())
    }

    #[tokio::test]
    async fn test_run_keeps_the_note_of_a_rewritten_version_comment()
    -> Result<(), Box<dyn std::error::Error>> {
        let content = format!(
            "steps:\n  - uses: o/p@{} # v4.0.0 pinned for node16\n  - uses: o/q@v4.0.0\n",
            sha40('a')
        );
        let new = sha40('b');
        for (pin, sha_line, tag_line) in [
            (
                "preserve",
                format!("o/p@{new} # v4.2.0 pinned for node16"),
                "o/q@v4.2.0".to_string(),
            ),
            (
                "sha",
                format!("o/p@{new} # v4.2.0 pinned for node16"),
                format!("o/q@{new} # v4.2.0"),
            ),
            (
                "tag",
                "o/p@v4.2.0 # pinned for node16".to_string(),
                "o/q@v4.2.0".to_string(),
            ),
        ] {
            let written =
                update_with_config(&format!("[policy]\npin = \"{pin}\"\n"), &content).await?;
            assert_eq!(
                written,
                format!("steps:\n  - uses: {sha_line}\n  - uses: {tag_line}\n"),
                "{pin}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_run_refreshes_the_version_comment_of_a_retargeted_tag()
    -> Result<(), Box<dyn std::error::Error>> {
        let content = "steps:\n  - uses: o/p@v4.0.0 # v4.0.0 keep\n  - uses: o/q@v4.0.0 # mine\n  - uses: o/r@v4.0.0 # 1st step\n";
        for pin in ["preserve", "tag"] {
            let written =
                update_with_config(&format!("[policy]\npin = \"{pin}\"\n"), content).await?;
            assert_eq!(
                written,
                "steps:\n  - uses: o/p@v4.2.0 # v4.2.0 keep\n  - uses: o/q@v4.2.0 # mine\n  - uses: o/r@v4.2.0 # 1st step\n",
                "{pin}"
            );
        }
        let written = update_with_config("[policy]\npin = \"sha\"\n", content).await?;
        assert_eq!(
            written,
            format!(
                "steps:\n  - uses: o/p@{sha} # v4.2.0 keep\n  - uses: o/q@{sha} # mine\n  - uses: o/r@{sha} # 1st step\n",
                sha = "b".repeat(40)
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_run_pin_override_wins_for_matching_actions()
    -> Result<(), Box<dyn std::error::Error>> {
        let toml = "[policy]\npin = \"tag\"\n\n[[overrides]]\npattern = \"pinned/*\"\npolicy = { pin = \"sha\" }\n\n[[overrides]]\npattern = \"o/q\"\npolicy = { pin = \"preserve\" }\n";
        let written = update_with_config(toml, &pin_style_workflow()).await?;
        let expected = [
            "steps:",
            "  - uses: actions/checkout@v4.2.0",
            "  - uses: o/p@v4.2.0",
            "  - uses: o/q@NEW # v4.2.0",
            "  - uses: o/r@v4.2.0 # mine",
            "  - uses: pinned/x@NEW # v4.2.0",
            "",
        ]
        .join("\n")
        .replace("NEW", &sha40('b'));
        assert_eq!(written, expected);
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
        let config = uncached();

        for _ in 0..2 {
            let code = run(
                dir.path(),
                false,
                OutputFormat::Human,
                &config,
                &registry,
                &Arc::new(FileCache::from_config(&config, Access::ReadWrite)),
            )
            .await;
            assert_eq!(code, CliExitCode::Success);
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
            &uncached(),
            &registry("v4.2.0"),
            &Arc::new(FileCache::from_config(&uncached(), Access::ReadWrite)),
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
        assert_eq!(result.exit_code(false), CliExitCode::EnvironmentError);
        Ok(())
    }

    #[tokio::test]
    async fn test_run_updates_markdown_and_text_extra_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("playground"))?;
        let readme = dir.path().join("README.md");
        let script = dir.path().join("playground/index.ts");
        let md = "# Usage\n\n```yaml\nsteps:\n  - uses: o/p@OLD\n  - uses: actions/checkout@v4.0.0\n```\n\nProse uses: o/q@v4.0.0\n";
        let ts = "const workflow = `\n  - uses: o/p@OLD # v4.0.0\n  - uses: actions/checkout@v4.0.0\n`;\nexport const step = { uses: \"o/r@v4.0.0\" };\nprint(\"- uses: o/p@OLD # v4.0.0\\n\");\n";
        std::fs::write(&readme, md.replace("OLD", &sha40('a')))?;
        std::fs::write(&script, ts.replace("OLD", &sha40('a')))?;
        let mut config = uncached();
        config.scan.extra_paths = vec!["README.md".parse()?, "playground/*.ts".parse()?];
        let registry = registry("v4.2.0");

        let md_expected = "# Usage\n\n```yaml\nsteps:\n  - uses: o/p@NEW # v4.2.0\n  - uses: actions/checkout@v4.2.0\n```\n\nProse uses: o/q@v4.0.0\n"
            .replace("NEW", &sha40('b'));
        let ts_expected = "const workflow = `\n  - uses: o/p@NEW # v4.2.0\n  - uses: actions/checkout@v4.2.0\n`;\nexport const step = { uses: \"o/r@v4.2.0\" };\nprint(\"- uses: o/p@NEW # v4.2.0\\n\");\n"
            .replace("NEW", &sha40('b'));
        for _ in 0..2 {
            let code = run(
                dir.path(),
                false,
                OutputFormat::Json,
                &config,
                &registry,
                &Arc::new(FileCache::from_config(
                    &config,
                    actup_core::Access::ReadWrite,
                )),
            )
            .await;
            assert_eq!(code, CliExitCode::Success);
            assert_eq!(std::fs::read_to_string(&readme)?, md_expected);
            assert_eq!(std::fs::read_to_string(&script)?, ts_expected);
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_run_leaves_a_ref_with_a_regex_escape_invalid()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join("check.sh");
        let content = "grep -q 'uses: actions/checkout@v4\\.1\\.0' ci.yml\n";
        std::fs::write(&script, content)?;
        let mut config = uncached();
        config.scan.extra_paths = vec!["check.sh".parse()?];

        let Some(result) = edit(
            dir.path(),
            false,
            OutputFormat::Json,
            &config,
            &registry("v4.2.0"),
            &Arc::new(FileCache::disabled()),
        )
        .await
        else {
            panic!("no file found");
        };

        let errored: Vec<(&str, ErrorKind)> = result
            .errored
            .iter()
            .map(|e| (e.site.reference.as_str(), e.kind))
            .collect();
        assert_eq!(errored, [("v4\\.1\\.0", ErrorKind::InvalidRef)]);
        assert_eq!(result.edited, []);
        assert_eq!(result.exit_code(false), CliExitCode::UserError);
        assert_eq!(std::fs::read_to_string(&script)?, content);
        Ok(())
    }

    #[tokio::test]
    async fn test_edit_ignores_refs_on_unserved_hosts() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        let file = workflows.join("ci.yml");
        let content = "steps:\n  - uses: ghe.corp.example/o/a@v4.0.0\n";
        std::fs::write(&file, content)?;
        let config = Config::from_contents(
            Some((
                Path::new("actup.toml"),
                "[scan]\nignore = [\"ghe.corp.example/o/*\"]\n\n[cache]\nenabled = false\n",
            )),
            actup_core::config::EnvOverrides::default(),
        )?;

        let Some(result) = edit(
            dir.path(),
            false,
            OutputFormat::Json,
            &config,
            &registry("v4.2.0"),
            &Arc::new(FileCache::disabled()),
        )
        .await
        else {
            panic!("no workflow found");
        };

        let skipped: Vec<(usize, &str, &str, SkipCause)> = result
            .skipped
            .iter()
            .map(|s| {
                (
                    s.site.line,
                    s.site.provider.as_str(),
                    s.site.host.as_str(),
                    s.reason,
                )
            })
            .collect();
        assert_eq!(
            skipped,
            [(2, "unknown", "ghe.corp.example", SkipCause::Ignored)]
        );
        assert_eq!(result.errored, []);
        assert_eq!(result.exit_code(false), CliExitCode::Success);
        for dry_run in [true, false] {
            let code = run(
                dir.path(),
                dry_run,
                OutputFormat::Human,
                &config,
                &registry("v4.2.0"),
                &Arc::new(FileCache::disabled()),
            )
            .await;
            assert_eq!(code, CliExitCode::Success);
        }
        assert_eq!(std::fs::read_to_string(&file)?, content);
        Ok(())
    }

    #[tokio::test]
    async fn test_edit_reports_ignored_refs_without_failing()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        let file = workflows.join("ci.yml");
        let content = "steps:\n  - uses: skip/a@v4.0.0\n  - uses: o/b@v4.0.0\n";
        std::fs::write(&file, content)?;
        let config = Config::from_contents(
            Some((
                Path::new("actup.toml"),
                "[scan]\nignore = [\"skip/*\"]\n\n[cache]\nenabled = false\n",
            )),
            actup_core::config::EnvOverrides::default(),
        )?;

        let Some(result) = edit(
            dir.path(),
            false,
            OutputFormat::Json,
            &config,
            &registry("v4.2.0"),
            &Arc::new(FileCache::from_config(&config, Access::ReadWrite)),
        )
        .await
        else {
            panic!("no workflow found");
        };

        let skipped: Vec<(usize, SkipCause)> = result
            .skipped
            .iter()
            .map(|s| (s.site.line, s.reason))
            .collect();
        assert_eq!(skipped, [(2, SkipCause::Ignored)]);
        assert_eq!(result.exit_code(false), CliExitCode::Success);
        assert_eq!(
            std::fs::read_to_string(&file)?,
            content.replace("o/b@v4.0.0", "o/b@v4.2.0")
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_run_refuses_to_write_from_stale_versions()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        let file = workflows.join("ci.yml");
        let content =
            "on: push\njobs:\n  b:\n    runs-on: x\n    steps:\n      - uses: o/stale@v4.0.0\n";
        std::fs::write(&file, content)?;
        let registry = registry("v4.2.0");

        let Some(result) = edit(
            dir.path(),
            false,
            OutputFormat::Json,
            &uncached(),
            &registry,
            &Arc::new(FileCache::from_config(
                &uncached(),
                actup_core::Access::ReadWrite,
            )),
        )
        .await
        else {
            return Err("no workflows".into());
        };

        assert_eq!(std::fs::read_to_string(&file)?, content);
        assert_eq!(result.edited, Vec::new());
        let errored: Vec<(usize, ErrorKind, &str)> = result
            .errored
            .iter()
            .map(|e| (e.site.line, e.kind, e.message.as_str()))
            .collect();
        assert_eq!(
            errored,
            [(
                6,
                ErrorKind::Stale,
                "versions are stale (fetched 26h ago, rate limited); refusing to write"
            )]
        );
        assert_eq!(result.exit_code(false), CliExitCode::EnvironmentError);
        Ok(())
    }

    #[tokio::test]
    async fn test_edit_reports_invalid_refs_as_user_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        let content = "jobs:\n  b:\n    steps:\n      - uses: o/r@a~1\n      - uses: o/r@v1.lock\n      - uses: o/r\n      - uses: o/r@\n";
        std::fs::write(workflows.join("ci.yml"), content)?;

        let Some(result) = edit(
            dir.path(),
            false,
            OutputFormat::Json,
            &uncached(),
            &registry("v4.2.0"),
            &Arc::new(FileCache::from_config(
                &uncached(),
                actup_core::Access::ReadWrite,
            )),
        )
        .await
        else {
            panic!("no workflow found");
        };

        let errored: Vec<(usize, &str, &str, &str, ErrorKind)> = result
            .errored
            .iter()
            .map(|e| {
                (
                    e.site.line,
                    e.site.action.as_str(),
                    e.site.provider.as_str(),
                    e.site.host.as_str(),
                    e.kind,
                )
            })
            .collect();
        assert_eq!(
            errored,
            [
                (4, "o/r@a~1", "unknown", "github.com", ErrorKind::InvalidRef),
                (
                    5,
                    "o/r@v1.lock",
                    "unknown",
                    "github.com",
                    ErrorKind::InvalidRef
                ),
                (6, "o/r@", "unknown", "github.com", ErrorKind::InvalidRef),
                (7, "o/r@", "unknown", "github.com", ErrorKind::InvalidRef),
            ]
        );
        assert_eq!(result.edited, []);
        assert_eq!(result.exit_code(false), CliExitCode::UserError);
        assert_eq!(std::fs::read_to_string(workflows.join("ci.yml"))?, content);
        Ok(())
    }

    #[tokio::test]
    async fn test_run_without_workflows_reports_nothing_and_succeeds()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config = uncached();
        let registry = registry("v4.2.0");
        assert!(
            edit(
                dir.path(),
                false,
                OutputFormat::Json,
                &config,
                &registry,
                &Arc::new(FileCache::from_config(&config, Access::ReadWrite))
            )
            .await
            .is_none()
        );
        for format in [OutputFormat::Human, OutputFormat::Json, OutputFormat::Toml] {
            assert_eq!(
                run(
                    dir.path(),
                    false,
                    format,
                    &config,
                    &registry,
                    &Arc::new(FileCache::from_config(&config, Access::ReadWrite))
                )
                .await,
                CliExitCode::Success
            );
        }
        Ok(())
    }

    fn apply_at(line_body: &str, edit: PendingEdit) -> Result<String, Box<dyn std::error::Error>> {
        let content = format!("{}{line_body}", "\n".repeat(edit.line - 1));
        let mut file_edits = FileEdits::new(
            std::path::PathBuf::from("/test.yml"),
            content,
            SourceFormat::Yaml,
        );
        file_edits.add(edit, ());
        let plan = file_edits.plan();
        if !matches!(plan.entries.as_slice(), [((), EditOutcome::Applied { .. })]) {
            return Err(format!("edit did not apply: {:?}", plan.entries).into());
        }
        Ok(plan.content().trim_start_matches('\n').to_string())
    }

    #[test]
    fn test_branch_pin_names_the_branch_in_its_comment() -> Result<(), Box<dyn std::error::Error>> {
        let sha = sha40('c');
        for (tail, written) in [
            ("", " # main"),
            (" # v1.0.0", " # main"),
            (" # v1.0.0 keep", " # main keep"),
            (" # mine", " # mine"),
        ] {
            let line = format!("      - uses: a/b@main{tail}");
            let edit = pin_edit(
                &sha_loc(&line)?,
                &sha,
                "main",
                &Labels::unfetched("main"),
                true,
            )
            .ok_or("no edit")?;
            assert_eq!(
                apply_at(&line, edit)?,
                format!("      - uses: a/b@{sha}{written}"),
                "{tail}"
            );
        }
        for (line, written) in [
            (
                "      - { uses: a/b@main, with: { x: 1 } }",
                format!("      - {{ uses: a/b@{sha}, with: {{ x: 1 }} }}"),
            ),
            (
                "      - { uses: 'a/b@main' }",
                format!("      - {{ uses: 'a/b@{sha}' }} # main"),
            ),
        ] {
            let found = actup_core::extract_uses_from_yaml(line);
            let [loc] = found.as_slice() else {
                return Err(format!("expected one `uses:` in {line:?}").into());
            };
            let edit =
                pin_edit(loc, &sha, "main", &Labels::unfetched("main"), true).ok_or("no edit")?;
            assert_eq!(apply_at(line, edit)?, written, "{line}");
        }
        Ok(())
    }

    #[test]
    fn test_edits_skip_value_not_written_verbatim() {
        let line = "      - uses: \"actions/checkout@v\\x34\"";
        let loc = UsesLocation::new(1, 16, "actions/checkout@v4".to_string(), line.to_string());
        let target = version::Info::new(version::Version::parse("v5.0.0"), sha40('a'));
        let tag = make_action_ref("actions", "checkout", "v4");
        let pinned = make_action_ref("actions", "checkout", &sha40('b'));
        for action in [&tag, &pinned] {
            let labels = Labels::unfetched(action.ref_str());
            assert!(
                retarget_edit(
                    &loc,
                    action.is_sha(),
                    &target,
                    Named::Tag,
                    policy::Style::Preserve,
                    &labels,
                    true,
                )
                .is_none()
            );
        }
        assert!(pin_edit(&loc, &sha40('c'), "v4", &Labels::unfetched("v4"), true).is_none());
    }

    #[test]
    fn test_target_display_names_the_version_and_a_sha_pin() {
        let target = version::Info::new(version::Version::parse("v5.0.0"), sha40('b'));
        assert_eq!(
            target_display(
                &make_action_ref("a", "b", "v4"),
                &target,
                policy::Style::Preserve
            ),
            "v5.0.0"
        );
        assert_eq!(
            target_display(
                &make_action_ref("a", "b", &sha40('a')),
                &target,
                policy::Style::Preserve
            ),
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
    fn test_retarget_edit_tag_writes_tag() -> Result<(), Box<dyn std::error::Error>> {
        let line = "      - uses: actions/checkout@v4";
        let loc = sha_loc(line)?;
        let action_ref = make_action_ref("actions", "checkout", "v4");
        let target = version::Info::new(version::Version::parse("v5.0.0"), sha40('a'));
        let edit = retarget_edit(
            &loc,
            action_ref.is_sha(),
            &target,
            Named::Tag,
            policy::Style::Preserve,
            &Labels::unfetched(action_ref.ref_str()),
            true,
        )
        .ok_or("no edit")?;
        assert_eq!(
            apply_at(line, edit)?,
            "      - uses: actions/checkout@v5.0.0"
        );
        Ok(())
    }

    #[test]
    fn test_retarget_edit_sha_repins_to_sha_with_comment() -> Result<(), Box<dyn std::error::Error>>
    {
        let old = sha40('a');
        let new = sha40('b');
        let line = format!("      - uses: actions/checkout@{old}");
        let loc = sha_loc(&line)?;
        let action_ref = make_action_ref("actions", "checkout", &old);
        let target = version::Info::new(version::Version::parse("v7.0.1"), &new);
        let edit = retarget_edit(
            &loc,
            action_ref.is_sha(),
            &target,
            Named::Tag,
            policy::Style::Preserve,
            &Labels::unfetched(action_ref.ref_str()),
            true,
        )
        .ok_or("no edit")?;
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
            "  - { uses: g/h@NEW_SHA, with: { x: 1 } }",
            "",
        ]
        .join("\n")
        .replace("NEW_SHA", &new_sha);
        let target = version::Info::new(version::Version::parse("v5.0.0"), &new_sha);

        let mut file_edits = FileEdits::new(
            std::path::PathBuf::from("/test.yml"),
            content.clone(),
            SourceFormat::Yaml,
        );
        let mut seen = 0;
        for (loc, action_ref) in WorkflowScanner::extract_actions(&content, SourceFormat::Yaml) {
            file_edits.add(
                retarget_edit(
                    &loc,
                    action_ref.is_sha(),
                    &target,
                    Named::Tag,
                    policy::Style::Preserve,
                    &Labels::unfetched(action_ref.ref_str()),
                    true,
                )
                .ok_or("no edit")?,
                (),
            );
            seen += 1;
        }
        assert_eq!(seen, 6);

        let updated = file_edits.plan().content();
        assert_eq!(updated, expected);

        let refs: Vec<String> = WorkflowScanner::extract_actions(&updated, SourceFormat::Yaml)
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
