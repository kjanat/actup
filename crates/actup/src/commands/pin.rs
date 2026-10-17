//! Pin command - pin all actions to their current SHA

use std::{path::Path, sync::Arc};

use actup_core::{
    ActionRef, CheckStatus, Config, UsesLocation, check_action_with_provider, provider,
};
use owo_colors::OwoColorize;

use crate::{
    cli::OutputFormat,
    common::{
        Change, EditResult, FileEdits, PendingEdit, RefSite, SkipReason, WorkflowScanner,
        commit_file, repin_sha_edit, report_error, report_skipped, report_unserved,
    },
};

/// The commit a ref pins to.
enum PinTarget {
    /// A tag or branch ref on commit `sha`.
    Named { sha: String },
    /// A short SHA ref of commit `sha`, with the tag on that commit if any.
    Expanded {
        sha: String,
        tag: Option<Arc<actup_core::Info>>,
    },
}

/// Resolve the SHA for an action reference.
///
/// First tries to find it in the version list, then falls back to resolving the ref directly.
async fn resolve_sha(
    provider: &dyn actup_core::Provider,
    action_ref: &ActionRef,
) -> actup_core::Result<PinTarget> {
    let repo_id = action_ref.to_repo_id().without_subpath();

    let versions = provider.get_versions(&repo_id).await?;

    // Find version matching current ref
    if let Some(v) = versions
        .semver_tags()
        .iter()
        .find(|v| v.version().to_string() == action_ref.ref_str())
    {
        return Ok(PinTarget::Named {
            sha: v.sha().to_string(),
        });
    }

    // Try to resolve the ref directly (might be a branch)
    let sha = provider
        .resolve_ref_to_sha(&repo_id, action_ref.ref_str())
        .await?
        .as_str()
        .to_string();
    Ok(if action_ref.abbreviates(&sha) {
        PinTarget::Expanded {
            tag: versions.tag_on(&sha).cloned(),
            sha,
        }
    } else {
        PinTarget::Named { sha }
    })
}

enum PinStep {
    Edit(PendingEdit, Box<Change>),
    Unplaceable,
    Failed(actup_core::Error),
    Nothing,
}

/// Build the pin step for one action.
///
/// SHA-pinned refs are re-pinned only when outdated (never unpinned); tag and
/// branch refs are pinned to their current commit SHA. A short SHA expands to
/// the full SHA it abbreviates, commented with the tag on that commit if any.
async fn pin_action_edit(
    provider: &dyn actup_core::Provider,
    action_ref: &ActionRef,
    loc: &UsesLocation,
    config: &Config,
    site: &RefSite,
) -> PinStep {
    if action_ref.is_sha() {
        let repo_id = action_ref.to_repo_id().without_subpath();
        let versions = match provider.get_versions(&repo_id).await {
            Ok(v) => v,
            Err(e) => return PinStep::Failed(e),
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
            site: site.clone(),
            new_ref: target.sha().to_string(),
            target: format!("{} ({short})", target.version()),
            show_comment: false,
        };
        PinStep::Edit(edit, Box::new(change))
    } else {
        let (Some(range), Some((prefix, _))) = (loc.value_range(), loc.value().split_once('@'))
        else {
            return PinStep::Unplaceable;
        };
        let pin = match resolve_sha(provider, action_ref).await {
            Ok(pin) => pin,
            Err(e) => return PinStep::Failed(e),
        };
        let (edit, new_ref, target, show_comment) = match pin {
            PinTarget::Expanded {
                sha,
                tag: Some(tag),
            } => {
                let Some(edit) = repin_sha_edit(loc, &tag) else {
                    return PinStep::Unplaceable;
                };
                let target = format!("{prefix}@{sha} # {}", tag.version());
                (edit, sha, target, false)
            }
            PinTarget::Expanded { sha, tag: None } => {
                let new_uses = format!("{prefix}@{sha}");
                let edit = PendingEdit::replace_range(loc.line(), range, new_uses.clone());
                (edit, sha, new_uses, false)
            }
            PinTarget::Named { sha } => {
                let new_uses = format!("{prefix}@{sha}");
                let edit = PendingEdit::replace_range(loc.line(), range, new_uses.clone())
                    .with_comment(format!(" # {}", action_ref.ref_str()));
                (edit, sha, new_uses, true)
            }
        };
        let change = Change {
            site: site.clone(),
            new_ref,
            target,
            show_comment,
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
    let Some(result) = edit(path, dry_run, format, config, registry).await else {
        EditResult::default().print_report(dry_run, format);
        return 0;
    };
    result.print_summary(dry_run, "Pinned", "Would pin", format);
    result.exit_code(false)
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
            match pin_action_edit(provider, &action_ref, &loc, config, &site).await {
                PinStep::Edit(edit, change) => file_edits.add(edit, *change),
                PinStep::Unplaceable => report_skipped(site, SkipReason::Unplaceable, &mut result),
                PinStep::Failed(e) => report_error(site, &e, &mut result),
                PinStep::Nothing => {}
            }
        }

        for (change, comment) in commit_file(file_edits, dry_run, &mut result) {
            if matches!(format, OutputFormat::Human) {
                println!(
                    "{}:{}: {} -> {}",
                    relative,
                    change.site.line,
                    change.site.action.yellow(),
                    change.written(comment.as_deref()).cyan()
                );
            }
        }
    }

    Some(result)
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
    use crate::{
        cli::CliExitCode,
        common::{ErrorKind, FileEdits},
    };

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

    async fn assert_pin_writes(
        content: &str,
        expected: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let file = workflow(dir.path(), content)?;
        let registry = repo_sha_registry();
        let config = Config::default();

        for _ in 0..2 {
            let code = run(dir.path(), false, OutputFormat::Human, &config, &registry).await;
            assert_eq!(code, i32::from(CliExitCode::Success));
            assert_eq!(std::fs::read(&file)?, expected.as_bytes());
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_run_pins_only_ref_bytes_in_a_crlf_file() -> Result<(), Box<dyn std::error::Error>>
    {
        let content = "on: push\r\njobs:\r\n  b:\r\n    runs-on: x\r\n    steps:\r\n      - uses: actions/checkout@v4\r\n      - uses: o/b@v1 # mine\r\n      - { uses: 'o/d@main' }\r\n";
        let expected = "on: push\r\njobs:\r\n  b:\r\n    runs-on: x\r\n    steps:\r\n      - uses: actions/checkout@AAA # v4\r\n      - uses: o/b@CCC # mine\r\n      - { uses: 'o/d@DDD' } # main\r\n"
            .replace("AAA", &sha('a'))
            .replace("CCC", &sha('c'))
            .replace("DDD", &sha('d'));
        assert_pin_writes(content, &expected).await
    }

    #[tokio::test]
    async fn test_run_pin_keeps_mixed_line_breaks_and_the_bom()
    -> Result<(), Box<dyn std::error::Error>> {
        let content = "\u{feff}on: push\r\njobs:\n  b:\r    runs-on: x\r\n    steps:\n      - uses: o/f@v1\r      - uses: \"o/h@v2\"\r\n      - uses: o/i@v3";
        let expected = "\u{feff}on: push\r\njobs:\n  b:\r    runs-on: x\r\n    steps:\n      - uses: o/f@EEE # v1\r      - uses: \"o/h@FFF\" # v2\r\n      - uses: o/i@111 # v3"
            .replace("EEE", &sha('e'))
            .replace("FFF", &sha('f'))
            .replace("111", &sha('1'));
        assert_pin_writes(content, &expected).await
    }

    #[tokio::test]
    async fn test_edit_records_edited_and_errored_refs() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        workflow(
            dir.path(),
            "steps:\n  - uses: o/h@v2\n  - uses: o/unknown@v1\n  - uses: ghe.corp.example/o/b@v1\n",
        )?;

        let Some(result) = edit(
            dir.path(),
            true,
            OutputFormat::Json,
            &Config::default(),
            &repo_sha_registry(),
        )
        .await
        else {
            panic!("no workflow found");
        };

        let edited: Vec<(usize, &str, &str, String, Option<&str>)> = result
            .edited
            .iter()
            .map(|e| {
                (
                    e.site.line,
                    e.site.host.as_str(),
                    e.site.reference.as_str(),
                    e.to.clone(),
                    e.comment.as_deref(),
                )
            })
            .collect();
        assert_eq!(edited, [(2, "github.com", "v2", sha('f'), Some("# v2"))]);
        let errored: Vec<(usize, &str, ErrorKind)> = result
            .errored
            .iter()
            .map(|e| (e.site.line, e.site.host.as_str(), e.kind))
            .collect();
        assert_eq!(
            errored,
            [
                (3, "github.com", ErrorKind::InvalidRef),
                (4, "ghe.corp.example", ErrorKind::ProviderNotFound),
            ]
        );
        assert_eq!(result.errored[1].site.provider, "unknown");
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
        let registry = repo_sha_registry();
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
        let mut file_edits =
            FileEdits::new(std::path::PathBuf::from("/pages.yml"), content.clone());
        let mut refs = Vec::new();
        for (loc, action_ref) in WorkflowScanner::extract_actions(&content) {
            refs.push(action_ref.to_string());
            let PinStep::Edit(edit, change) = pin_action_edit(
                &provider,
                &action_ref,
                &loc,
                &config,
                &RefSite::new("ci.yml", loc.line(), &action_ref, &provider),
            )
            .await
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
        let mut file_edits = FileEdits::new(std::path::PathBuf::from("/ci.yml"), content.clone());
        let mut hosts = Vec::new();
        for (loc, action_ref) in WorkflowScanner::extract_actions(&content) {
            hosts.push(action_ref.host().map(str::to_owned));
            let PinStep::Edit(edit, change) = pin_action_edit(
                &provider,
                &action_ref,
                &loc,
                &config,
                &RefSite::new("ci.yml", loc.line(), &action_ref, &provider),
            )
            .await
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
        let mut refs = Vec::new();
        for (loc, action_ref) in WorkflowScanner::extract_actions(&content) {
            refs.push(action_ref.to_string());
            let step = pin_action_edit(
                &provider,
                &action_ref,
                &loc,
                &config,
                &RefSite::new("ci.yml", loc.line(), &action_ref, &provider),
            )
            .await;
            assert!(matches!(step, PinStep::Unplaceable), "{action_ref}");
        }

        assert_eq!(
            refs,
            [
                "actions/checkout@v4".to_string(),
                format!("actions/cache@{old}")
            ]
        );
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

        let mut file_edits = FileEdits::new(std::path::PathBuf::from("/ci.yml"), content.clone());
        for (loc, action_ref) in WorkflowScanner::extract_actions(&content) {
            let PinStep::Edit(edit, change) = pin_action_edit(
                &provider,
                &action_ref,
                &loc,
                &Config::default(),
                &RefSite::new("ci.yml", loc.line(), &action_ref, &provider),
            )
            .await
            else {
                return Err("pin produced no edit".into());
            };
            assert_eq!(change.written(Some(" # v4.2.0")), "v4.2.0 (bbbbbbb)");
            file_edits.add(edit, *change);
        }

        assert_eq!(
            file_edits.plan().content,
            format!("steps:\n  - uses: actions/cache@{new} # v4.2.0\n")
        );
        Ok(())
    }

    const FULL_SHA: &str = "abc1234def5678abc1234def5678abc1234def56";

    fn pin_provider(tags: &[(&str, &str)], resolves_to: &str) -> Option<FixedShaProvider> {
        let tags = tags
            .iter()
            .map(|(tag, sha)| Arc::new(version::Info::new(version::Version::parse(tag), *sha)))
            .collect();
        Some(FixedShaProvider {
            sha: Sha40::new(resolves_to)?,
            versions: version::Set::build(tags, HashMap::new(), HashMap::new(), None),
        })
    }

    struct Pinned {
        line: String,
        new_ref: String,
        comment: Option<String>,
    }

    async fn pinned_line(provider: &FixedShaProvider, line: &str) -> Option<Pinned> {
        let content = format!("steps:\n{line}\n");
        let (loc, action_ref) = WorkflowScanner::extract_actions(&content).next()?;
        let site = RefSite::new("ci.yml", loc.line(), &action_ref, provider);
        let PinStep::Edit(edit, change) =
            pin_action_edit(provider, &action_ref, &loc, &Config::default(), &site).await
        else {
            return None;
        };
        let mut file_edits = FileEdits::new(std::path::PathBuf::from("/ci.yml"), content.clone());
        file_edits.add(edit, *change);
        let plan = file_edits.plan();
        let [(change, crate::common::EditOutcome::Applied { comment })] = plan.entries.as_slice()
        else {
            return None;
        };
        Some(Pinned {
            line: plan.content.lines().nth(1)?.to_owned(),
            new_ref: change.new_ref.clone(),
            comment: comment.clone(),
        })
    }

    #[tokio::test]
    async fn pin_expands_short_sha_with_tag_comment() -> Result<(), Box<dyn std::error::Error>> {
        let provider = pin_provider(&[("v4", FULL_SHA), ("v4.2.0", FULL_SHA)], FULL_SHA)
            .ok_or("fixture SHA must be valid")?;
        for line in ["  - uses: o/r@abc1234", "  - uses: o/r@abc1234 # v4.1.0"] {
            let pinned = pinned_line(&provider, line)
                .await
                .ok_or("pin produced no edit")?;
            assert_eq!(pinned.line, format!("  - uses: o/r@{FULL_SHA} # v4.2.0"));
            assert_eq!(pinned.new_ref, FULL_SHA);
            assert_eq!(pinned.comment.as_deref(), Some(" # v4.2.0"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn pin_expands_untagged_short_sha_without_comment()
    -> Result<(), Box<dyn std::error::Error>> {
        let other = "b".repeat(40);
        let provider =
            pin_provider(&[("v4.2.0", &other)], FULL_SHA).ok_or("fixture SHA must be valid")?;
        let pinned = pinned_line(&provider, "  - uses: o/r@abc1234")
            .await
            .ok_or("pin produced no edit")?;
        assert_eq!(pinned.line, format!("  - uses: o/r@{FULL_SHA}"));
        assert_eq!(pinned.new_ref, FULL_SHA);
        assert_eq!(pinned.comment, None);
        Ok(())
    }

    #[tokio::test]
    async fn pin_treats_hex_ref_on_another_commit_as_a_name()
    -> Result<(), Box<dyn std::error::Error>> {
        let other = "c".repeat(40);
        let provider = pin_provider(&[], &other).ok_or("fixture SHA must be valid")?;
        let pinned = pinned_line(&provider, "  - uses: o/r@deadbee")
            .await
            .ok_or("pin produced no edit")?;
        assert_eq!(pinned.line, format!("  - uses: o/r@{other} # deadbee"));
        assert_eq!(pinned.new_ref, other);
        assert_eq!(pinned.comment.as_deref(), Some(" # deadbee"));
        Ok(())
    }
}
