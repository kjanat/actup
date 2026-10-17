//! Pin command - pin all actions to their current SHA

use std::path::Path;

use actup_core::{
    ActionRef, CheckStatus, Config, UsesLocation, check_action_with_provider, provider,
};
use owo_colors::OwoColorize;

use crate::{
    cli::OutputFormat,
    common::{
        Change, EditResult, ErrorTally, FileEdits, PendingEdit, SkipReason, WorkflowScanner,
        commit_file, repin_sha_edit, report_skipped,
    },
};

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

enum PinStep {
    Edit(PendingEdit, Box<Change>),
    Unplaceable,
    Nothing,
}

/// Build the pin step for one action.
///
/// SHA-pinned refs are re-pinned only when outdated (never unpinned); tag and
/// branch refs are pinned to their current commit SHA.
async fn pin_action_edit(
    provider: &dyn actup_core::Provider,
    action_ref: &ActionRef,
    loc: &UsesLocation,
    config: &Config,
    errors: &mut ErrorTally,
) -> PinStep {
    if action_ref.is_sha() {
        let repo_id = action_ref.to_repo_id().without_subpath();
        let versions = match provider.get_versions(&repo_id).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{}: {} - {}", "Warning".yellow(), action_ref, e);
                errors.record(&e);
                return PinStep::Nothing;
            }
        };
        let result = check_action_with_provider(action_ref, &versions, config, provider).await;
        let CheckStatus::Outdated { target, .. } = result.status() else {
            return PinStep::Nothing;
        };
        let Some(edit) = repin_sha_edit(loc, target) else {
            return PinStep::Unplaceable;
        };
        let short = &target.sha()[..7.min(target.sha().len())];
        let change = Change {
            action: action_ref.clone(),
            provider: provider.kind().to_string(),
            line: loc.line(),
            target: format!("{} ({short})", target.version()),
            show_comment: false,
        };
        PinStep::Edit(edit, Box::new(change))
    } else {
        let (Some(range), Some((prefix, _))) = (loc.value_range(), loc.value().split_once('@'))
        else {
            return PinStep::Unplaceable;
        };
        let Some(sha) = resolve_sha(provider, action_ref, errors).await else {
            return PinStep::Nothing;
        };
        let new_uses = format!("{prefix}@{sha}");
        let edit = PendingEdit::replace_range(loc.line(), range, new_uses.clone())
            .with_comment(format!(" # {}", action_ref.ref_str()));
        let change = Change {
            action: action_ref.clone(),
            provider: provider.kind().to_string(),
            line: loc.line(),
            target: new_uses,
            show_comment: true,
        };
        PinStep::Edit(edit, Box::new(change))
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

    let mut result = EditResult::default();

    for file in &discovered {
        let Some(content) = scanner.read_file(file, &mut result.errors) else {
            continue;
        };

        let relative = scanner.relative_path(file);
        let mut file_edits = FileEdits::new(file.path().to_path_buf(), content.clone());

        for (loc, action_ref) in WorkflowScanner::extract_actions(&content) {
            let Some(provider) = scanner.provider_for(&action_ref) else {
                continue;
            };

            match pin_action_edit(provider, &action_ref, &loc, config, &mut result.errors).await {
                PinStep::Edit(edit, change) => file_edits.add(edit, *change),
                PinStep::Unplaceable => report_skipped(
                    &relative,
                    loc.line(),
                    &action_ref,
                    &provider.kind().to_string(),
                    SkipReason::Unplaceable,
                    &mut result,
                ),
                PinStep::Nothing => {}
            }
        }

        for (change, comment) in commit_file(file_edits, &relative, dry_run, &mut result) {
            if matches!(format, OutputFormat::Human) {
                println!(
                    "{}:{}: {} -> {}",
                    relative,
                    change.line,
                    change.action.to_string().yellow(),
                    change.written(comment.as_deref()).cyan()
                );
            }
        }
    }

    result.print_summary(dry_run, "Pinned", "Would pin", format);
    result.exit_code(false)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use actup_core::{
        ActionRepoId, BoxFuture,
        provider::{ActionYml, Kind, Provider, Sha40},
        version,
    };

    use super::*;
    use crate::{cli::CliExitCode, common::FileEdits};

    struct FixedShaProvider {
        sha: Sha40,
        versions: version::Set,
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
            let versions = self.versions.clone();
            Box::pin(async move { Ok(versions) })
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

    struct RepoShaProvider {
        shas: HashMap<&'static str, char>,
    }

    impl Provider for RepoShaProvider {
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
            repo: &ActionRepoId,
            ref_name: &str,
        ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
            let sha = self
                .shas
                .get(repo.repo())
                .and_then(|c| Sha40::new(c.to_string().repeat(40)))
                .ok_or_else(|| actup_core::Error::InvalidRef {
                    input: ref_name.to_string(),
                    reason: "unknown repo".to_string(),
                });
            Box::pin(async move { sha })
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

    fn sha(c: char) -> String {
        c.to_string().repeat(40)
    }

    fn workflow(dir: &Path, content: &str) -> std::io::Result<std::path::PathBuf> {
        let workflows = dir.join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        let file = workflows.join("ci.yml");
        std::fs::write(&file, content)?;
        Ok(file)
    }

    fn repo_sha_registry() -> provider::Registry {
        let mut registry = provider::Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(RepoShaProvider {
            shas: HashMap::from([
                ("checkout", 'a'),
                ("actionlint", 'b'),
                ("b", 'c'),
                ("d", 'd'),
                ("f", 'e'),
                ("h", 'f'),
                ("i", '1'),
            ]),
        }));
        registry
    }

    #[tokio::test]
    async fn test_run_pins_every_action_on_a_line_in_one_pass()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let content = [
            "on: push",
            "jobs:",
            "  lint:",
            "    runs-on: ubuntu-latest",
            "    steps: [{ uses: actions/checkout@v7 }, { uses: kjanat/actionlint@v1 }]",
            "  three:",
            "    runs-on: ubuntu-latest",
            "    steps: [{ uses: \"o/b@v1.2.3\" }, { uses: 'o/d@main' }, { uses: o/f@v10 }] # keep",
            "  single:",
            "    runs-on: ubuntu-latest",
            "    steps:",
            "      - uses: o/h@v2",
            "      - uses: o/i@v3 # mine",
            "",
        ]
        .join("\n");
        let expected = [
            "on: push",
            "jobs:",
            "  lint:",
            "    runs-on: ubuntu-latest",
            "    steps: [{ uses: actions/checkout@AAA }, { uses: kjanat/actionlint@BBB }]",
            "  three:",
            "    runs-on: ubuntu-latest",
            "    steps: [{ uses: \"o/b@CCC\" }, { uses: 'o/d@DDD' }, { uses: o/f@EEE }] # keep",
            "  single:",
            "    runs-on: ubuntu-latest",
            "    steps:",
            "      - uses: o/h@FFF # v2",
            "      - uses: o/i@111 # mine",
            "",
        ]
        .join("\n")
        .replace("AAA", &sha('a'))
        .replace("BBB", &sha('b'))
        .replace("CCC", &sha('c'))
        .replace("DDD", &sha('d'))
        .replace("EEE", &sha('e'))
        .replace("FFF", &sha('f'))
        .replace("111", &sha('1'));
        let file = workflow(dir.path(), &content)?;
        let registry = repo_sha_registry();
        let config = Config::default();

        let code = run(dir.path(), false, OutputFormat::Human, &config, &registry).await;
        assert_eq!(code, i32::from(CliExitCode::Success));
        assert_eq!(std::fs::read_to_string(&file)?, expected);

        let code = run(dir.path(), false, OutputFormat::Human, &config, &registry).await;
        assert_eq!(code, i32::from(CliExitCode::Success));
        assert_eq!(std::fs::read_to_string(&file)?, expected);
        Ok(())
    }

    #[tokio::test]
    async fn test_run_skips_an_unplaceable_value_without_a_user_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let content = "on: push\njobs:\n  b:\n    runs-on: x\n    steps: [{ uses: \"o/h@v\\x32\" }, { uses: o/i@v3 }]\n";
        let file = workflow(dir.path(), content)?;

        let code = run(
            dir.path(),
            false,
            OutputFormat::Json,
            &Config::default(),
            &repo_sha_registry(),
        )
        .await;

        assert_eq!(code, i32::from(CliExitCode::Outdated));
        assert_eq!(
            std::fs::read_to_string(&file)?,
            content.replace("o/i@v3", &format!("o/i@{}", sha('1')))
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_pin_flow_style_steps_resolves_exact_refs() {
        let sha = "c".repeat(40);
        let Some(sha40) = Sha40::new(sha.clone()) else {
            panic!("fixture SHA must be valid");
        };
        let provider = FixedShaProvider {
            sha: sha40,
            versions: version::Set::new(),
        };
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
            let PinStep::Edit(edit, change) =
                pin_action_edit(&provider, &action_ref, &loc, &config, &mut errors).await
            else {
                panic!("pin produced no edit for {action_ref}");
            };
            file_edits.add(edit, *change);
        }

        assert_eq!(
            refs,
            [
                "actions/upload-pages-artifact@v4",
                "actions/deploy-pages@v4"
            ]
        );
        assert_eq!(errors.total(), 0);
        assert_eq!(file_edits.plan().content, expected);
    }

    #[tokio::test]
    async fn test_pin_keeps_the_host_as_written() -> Result<(), Box<dyn std::error::Error>> {
        let sha = "d".repeat(40);
        let sha40 = Sha40::new(sha.clone()).ok_or("fixture SHA must be valid")?;
        let provider = FixedShaProvider {
            sha: sha40,
            versions: version::Set::new(),
        };
        let content = "steps:\n  - uses: GitHub.COM/actions/checkout@v4\n".to_string();

        let config = Config::default();
        let mut errors = ErrorTally::default();
        let mut file_edits = FileEdits::new(std::path::PathBuf::from("/ci.yml"), content.clone());
        let mut hosts = Vec::new();
        for (loc, action_ref) in WorkflowScanner::extract_actions(&content) {
            hosts.push(action_ref.host().map(str::to_owned));
            let PinStep::Edit(edit, change) =
                pin_action_edit(&provider, &action_ref, &loc, &config, &mut errors).await
            else {
                return Err("pin produced no edit".into());
            };
            assert_eq!(
                change.written(Some(" # v4")),
                format!("GitHub.COM/actions/checkout@{sha} # v4")
            );
            file_edits.add(edit, *change);
        }

        assert_eq!(hosts, [Some("github.com".to_owned())]);
        assert_eq!(errors.total(), 0);
        assert_eq!(
            file_edits.plan().content,
            format!("steps:\n  - uses: GitHub.COM/actions/checkout@{sha} # v4\n")
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_pin_reports_values_not_written_verbatim() -> Result<(), Box<dyn std::error::Error>>
    {
        let old = "a".repeat(40);
        let new = "b".repeat(40);
        let versions = version::Set::build(
            vec![
                std::sync::Arc::new(version::Info::new(version::Version::parse("v4.0.0"), &old)),
                std::sync::Arc::new(version::Info::new(version::Version::parse("v4.2.0"), &new)),
            ],
            std::collections::HashMap::from([(
                4,
                std::sync::Arc::new(version::Info::new(version::Version::parse("v4.2.0"), &new)),
            )]),
            std::collections::HashMap::new(),
            Some(std::sync::Arc::new(version::Info::new(
                version::Version::parse("v4.2.0"),
                &new,
            ))),
        );
        let sha40 = Sha40::new("c".repeat(40)).ok_or("fixture SHA must be valid")?;
        let provider = FixedShaProvider {
            sha: sha40,
            versions,
        };
        let content = format!(
            "steps:\n  - uses: \"actions/checkout@v\\x34\"\n  - uses: \"actions/cache@\\x61{}\"\n",
            &old[1..]
        );

        let config = Config::default();
        let mut errors = ErrorTally::default();
        let mut refs = Vec::new();
        for (loc, action_ref) in WorkflowScanner::extract_actions(&content) {
            refs.push(action_ref.to_string());
            let step = pin_action_edit(&provider, &action_ref, &loc, &config, &mut errors).await;
            assert!(matches!(step, PinStep::Unplaceable), "{action_ref}");
        }

        assert_eq!(
            refs,
            [
                "actions/checkout@v4".to_string(),
                format!("actions/cache@{old}")
            ]
        );
        assert_eq!(errors.total(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn test_pin_repins_an_outdated_sha_with_its_version()
    -> Result<(), Box<dyn std::error::Error>> {
        let old = "a".repeat(40);
        let new = "b".repeat(40);
        let latest =
            std::sync::Arc::new(version::Info::new(version::Version::parse("v4.2.0"), &new));
        let versions = version::Set::build(
            vec![
                std::sync::Arc::new(version::Info::new(version::Version::parse("v4.0.0"), &old)),
                std::sync::Arc::clone(&latest),
            ],
            HashMap::from([(4, std::sync::Arc::clone(&latest))]),
            HashMap::new(),
            Some(latest),
        );
        let provider = FixedShaProvider {
            sha: Sha40::new(old.clone()).ok_or("fixture SHA must be valid")?,
            versions,
        };
        let content = format!("steps:\n  - uses: actions/cache@{old}\n");

        let mut errors = ErrorTally::default();
        let mut file_edits = FileEdits::new(std::path::PathBuf::from("/ci.yml"), content.clone());
        for (loc, action_ref) in WorkflowScanner::extract_actions(&content) {
            let PinStep::Edit(edit, change) = pin_action_edit(
                &provider,
                &action_ref,
                &loc,
                &Config::default(),
                &mut errors,
            )
            .await
            else {
                return Err("pin produced no edit".into());
            };
            assert_eq!(change.written(Some(" # v4.2.0")), "v4.2.0 (bbbbbbb)");
            file_edits.add(edit, *change);
        }

        assert_eq!(errors.total(), 0);
        assert_eq!(
            file_edits.plan().content,
            format!("steps:\n  - uses: actions/cache@{new} # v4.2.0\n")
        );
        Ok(())
    }
}
