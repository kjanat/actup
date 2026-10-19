//! Check command - scan workflows and report outdated actions

use std::{path::Path, sync::Arc};

use actup_core::{
    ActionRef, ActionRepoId, CheckResult, CheckStatus, Config, FileCache, NO_VERSIONS_FOUND_REASON,
    check_action_with_comment, provider, version,
};

use anstream::eprintln;

use crate::{
    cli::{CheckFormat, CliExitCode, FailOn, OutputFormat},
    common::{ErrorTally, InvalidUses, WorkflowScanner},
    output::{Finding, FindingStatus, Span, determine_exit_code, print_findings, print_summary},
    sarif,
    scan::{Scan, Versions},
};

/// Convert core [`CheckStatus`] to CLI [`FindingStatus`].
///
/// When `versions` is provided, a major-only target also names the full tag on its commit.
fn to_finding_status(status: CheckStatus, versions: Option<&version::Set>) -> FindingStatus {
    match status {
        CheckStatus::UpToDate => FindingStatus::UpToDate,
        CheckStatus::Ignored => FindingStatus::Ignored,
        CheckStatus::Outdated { current, target } => {
            let exact_version = if target.version().is_major_only() {
                versions
                    .and_then(|v| v.full_tag_on(target.sha()))
                    .map(|t| t.version().to_string())
            } else {
                None
            };
            FindingStatus::Outdated {
                current,
                latest: target,
                exact_version,
            }
        }
        CheckStatus::Pinned { sha, tag } => FindingStatus::Pinned { sha, tag },
        CheckStatus::Floating { ref_name } => FindingStatus::Floating { ref_name },
        CheckStatus::PinBranch { ref_name, sha } => FindingStatus::PinBranch { ref_name, sha },
        CheckStatus::Unresolvable {
            reason,
            is_environment_error,
        } => FindingStatus::Unresolvable {
            reason,
            is_environment_error,
        },
    }
}

/// Check a single action using pre-fetched versions.
///
/// When `versions_result` is `None` (no fetch attempted) or `Some(Err(...))` (fetch failed),
/// creates an Unresolvable finding with proper error classification.
async fn check_action_with_versions(
    action: &CollectedAction,
    versions_result: Option<&Result<version::Set, actup_core::Error>>,
    config: &Config,
    registry: &provider::Registry,
) -> Finding {
    let (status, result) = match (versions_result, registry.find_for_repo(&action.repo_id)) {
        (None, _) => {
            // No fetch attempted (shouldn't happen in normal flow)
            let status = FindingStatus::Unresolvable {
                reason: "no version fetch attempted".to_string(),
                is_environment_error: true,
            };
            (status, None)
        }
        (Some(Err(e)), _) => {
            // Fetch failed - use core's error classification
            let status = FindingStatus::Unresolvable {
                reason: actup_core::error::chain(e),
                is_environment_error: e.is_environment_error(),
            };
            (status, None)
        }
        (Some(Ok(_)), None) => {
            let status = FindingStatus::Unresolvable {
                reason: "no provider for fetched repo".to_string(),
                is_environment_error: true,
            };
            (status, None)
        }
        (Some(Ok(versions)), Some(provider)) => {
            let result = check_action_with_comment(
                &action.action_ref,
                &action.comment,
                versions,
                config,
                provider,
            )
            .await;
            (
                to_finding_status(result.status().clone(), Some(versions)),
                Some(result),
            )
        }
    };

    Finding::new(
        action.file.clone(),
        action.line,
        action.action_ref.clone(),
        status,
        action.provider.clone(),
        action.host.clone(),
    )
    .with_span(action.span)
    .with_ref(
        result.as_ref().and_then(CheckResult::ref_kind),
        result.as_ref().and_then(|r| r.same_commit().cloned()),
    )
    .with_suggestion(result.as_ref().and_then(|r| r.suggestion().cloned()))
    .with_stale(versions_result.and_then(|r| r.as_ref().ok()?.stale()))
}

/// Collected action for batch processing.
struct CollectedAction {
    comment: String,
    file: String,
    line: usize,
    action_ref: ActionRef,
    repo_id: ActionRepoId,
    provider: String,
    host: String,
    span: Span,
}

/// Scan workflows and report outdated actions across multiple providers.
pub async fn run(
    path: &Path,
    fail_on: &[FailOn],
    format: CheckFormat,
    github_annotations: bool,
    config: &Config,
    registry: &provider::Registry,
    cache: &Arc<FileCache>,
) -> CliExitCode {
    let scanner = WorkflowScanner::new(path, config, registry, format.messages());
    let discovered = scanner.discover();

    if discovered.is_empty() {
        scanner.print_empty_message();
        if !matches!(format.messages(), OutputFormat::Human) {
            print_findings(&[], format, &[]);
        }
        return CliExitCode::Success;
    }

    let mut findings = Vec::new();
    let mut io_errors = ErrorTally::default();

    let scan = Scan::collect(&scanner, &discovered, &mut io_errors);
    let versions_map = scan
        .fetch(&scanner, config, registry, cache, |_| true)
        .await;
    let collected = collect_actions(&scanner, &scan, config, &mut findings);
    let unresolved =
        process_collected_actions(collected, &versions_map, config, registry, &mut findings).await;
    resolve_fallback_refs(unresolved, registry, &mut findings).await;

    // Stable output: file path, then line number. Sorting after all batching
    // completes (not per-phase) keeps the order deterministic.
    findings.sort_by(|a, b| a.file().cmp(b.file()).then_with(|| a.line().cmp(&b.line())));

    print_findings(&findings, format, &scan.unreadable);
    if format == CheckFormat::Human {
        print_summary(&findings);
    }
    if github_annotations {
        for annotation in sarif::annotations(&findings) {
            eprintln!("{annotation}");
        }
    }

    // I/O errors take priority over outdated status (env error = exit 3).
    let io_exit = io_errors.exit_code();
    if io_exit != CliExitCode::Success {
        return io_exit;
    }

    determine_exit_code(&findings, fail_on)
}

/// Phases 1 and 2 run in [`Scan::collect`] and [`Scan::fetch`]. Emit
/// immediate "no provider" and ignored findings, and collect the remaining
/// scanned refs for Phase 3.
fn collect_actions(
    scanner: &WorkflowScanner<'_>,
    scan: &Scan,
    config: &Config,
    findings: &mut Vec<Finding>,
) -> Vec<CollectedAction> {
    let mut collected = Vec::new();
    for bad in &scan.invalid {
        let InvalidUses { action, error } = &bad.invalid;
        findings.push(
            Finding::new(
                scan.files[bad.file].relative.clone(),
                bad.loc.line(),
                action.clone(),
                FindingStatus::Unresolvable {
                    reason: error.to_string(),
                    is_environment_error: false,
                },
                "unknown".to_string(),
                scanner.host_for(action),
            )
            .with_span(Span::of(&bad.loc)),
        );
    }
    let sources: Vec<actup_core::patch::FileEdits<()>> = scan
        .files
        .iter()
        .map(|file| {
            actup_core::patch::FileEdits::new(file.path.clone(), file.content.clone(), file.format)
        })
        .collect();
    for scanned in &scan.refs {
        let relative = &scan.files[scanned.file].relative;
        let action_ref = &scanned.action_ref;
        if config.should_ignore(action_ref) {
            let site = scanner.site_for(relative, scanned.loc.line(), action_ref);
            findings.push(
                Finding::new(
                    relative.clone(),
                    scanned.loc.line(),
                    action_ref.clone(),
                    FindingStatus::Ignored,
                    site.provider,
                    site.host,
                )
                .with_span(Span::of(&scanned.loc)),
            );
            continue;
        }

        // No provider configured for the ref's host is an environment
        // error: emit immediately so it doesn't block batch fetching.
        let Some(provider) = scanner.provider_for(action_ref) else {
            let host = scanner.host_for(action_ref);
            findings.push(
                Finding::new(
                    relative.clone(),
                    scanned.loc.line(),
                    action_ref.clone(),
                    FindingStatus::Unresolvable {
                        reason: format!("no provider for host: {host}"),
                        is_environment_error: true,
                    },
                    "unknown".to_string(),
                    host,
                )
                .with_span(Span::of(&scanned.loc)),
            );
            continue;
        };

        collected.push(CollectedAction {
            comment: sources[scanned.file]
                .writable(&scanned.loc)
                .comment()
                .to_owned(),
            file: relative.clone(),
            line: scanned.loc.line(),
            action_ref: action_ref.clone(),
            repo_id: scanned.repo_id.clone(),
            provider: provider.kind().to_string(),
            host: provider.host().to_string(),
            span: Span::of(&scanned.loc),
        });
    }
    collected
}

/// Phase 3 — check each action against the batch-fetched versions.
///
/// Actions whose semver ref wasn't found in the batch get returned for
/// Phase 4's fallback `resolve_ref_to_sha` attempt. Everything else is
/// pushed straight into `findings`.
async fn process_collected_actions(
    collected: Vec<CollectedAction>,
    versions_map: &Versions,
    config: &Config,
    registry: &provider::Registry,
    findings: &mut Vec<Finding>,
) -> Vec<(CollectedAction, Finding)> {
    let mut unresolved = Vec::new();
    for action in collected {
        let versions_result = versions_map.get(&action.repo_id);
        let finding = check_action_with_versions(&action, versions_result, config, registry).await;

        if matches!(finding.status(), FindingStatus::Unresolvable { reason, .. }
                    if reason == NO_VERSIONS_FOUND_REASON)
        {
            unresolved.push((action, finding));
        } else {
            findings.push(finding);
        }
    }
    unresolved
}

/// Phase 4 — for semver refs not found in the batch fetch (e.g.
/// `taiki-e/install-action@v2` whose old tags sit beyond `max_tags`),
/// try `resolve_ref_to_sha` directly. If it resolves, mark Unknown rather
/// than Unresolvable — we know the ref exists but can't compare versions.
async fn resolve_fallback_refs(
    unresolved: Vec<(CollectedAction, Finding)>,
    registry: &provider::Registry,
    findings: &mut Vec<Finding>,
) {
    for (action, mut finding) in unresolved {
        let Some(provider) = registry.find_for_repo(&action.repo_id) else {
            findings.push(finding);
            continue;
        };

        match provider
            .resolve_ref_to_sha(&action.repo_id, action.action_ref.ref_str())
            .await
        {
            Ok(_) => {
                tracing::debug!(
                    owner = %action.repo_id.owner(),
                    repo = %action.repo_id.repo(),
                    ref_str = %action.action_ref.ref_str(),
                    provider = %action.provider,
                    "fallback: ref exists but version comparison unavailable (likely beyond max_tags)"
                );
                finding = Finding::new(
                    action.file,
                    action.line,
                    action.action_ref,
                    FindingStatus::Unknown {
                        reason: "ref exists but version comparison unavailable".to_string(),
                    },
                    action.provider,
                    action.host,
                )
                .with_span(action.span);
            }
            Err(e) if e.is_environment_error() => {
                tracing::debug!(
                    owner = %action.repo_id.owner(),
                    repo = %action.repo_id.repo(),
                    ref_str = %action.action_ref.ref_str(),
                    provider = %action.provider,
                    error = %actup_core::error::chain(&e),
                    "fallback: environment error during ref resolution"
                );
                finding = Finding::new(
                    action.file,
                    action.line,
                    action.action_ref,
                    FindingStatus::Unresolvable {
                        reason: actup_core::error::chain(&e),
                        is_environment_error: true,
                    },
                    action.provider,
                    action.host,
                )
                .with_span(action.span);
            }
            Err(_) => {}
        }
        findings.push(finding);
    }
}

// Tests for check command logic now live in actup-core::resolution module.
// The CLI-specific tests for output formatting are in output.rs.

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use actup_core::{
        provider::{ActionYml, Kind, Provider, Sha40},
        version,
    };

    use super::*;
    use actup_core::Access;

    struct TestProvider {
        resolve_error: Option<actup_core::Error>,
    }

    impl Provider for TestProvider {
        fn kind(&self) -> Kind {
            Kind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(
            &self,
            _repo: &ActionRepoId,
        ) -> actup_core::BoxFuture<'_, actup_core::Result<version::Set>> {
            Box::pin(async { Ok(version::Set::new()) })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> actup_core::BoxFuture<'_, actup_core::Result<Sha40>> {
            let result = self.resolve_error.clone().map_or_else(
                || Ok(Sha40::new("a".repeat(40)).unwrap_or_else(|| unreachable!())),
                Err,
            );
            Box::pin(async { result })
        }

        fn get_default_branch(
            &self,
            _repo: &ActionRepoId,
        ) -> actup_core::BoxFuture<'_, actup_core::Result<String>> {
            Box::pin(async { Ok("main".to_string()) })
        }

        fn get_readme(
            &self,
            _repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> actup_core::BoxFuture<'_, actup_core::Result<Option<String>>> {
            Box::pin(async { Ok(None) })
        }

        fn get_action_yml(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> actup_core::BoxFuture<'_, actup_core::Result<ActionYml>> {
            Box::pin(async { Ok(ActionYml::default()) })
        }
    }

    fn test_registry() -> provider::Registry {
        registry_resolving_to(None)
    }

    fn registry_resolving_to(resolve_error: Option<actup_core::Error>) -> provider::Registry {
        let mut registry = provider::Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(TestProvider { resolve_error }));
        registry
    }

    fn connection_refused() -> actup_core::Error {
        actup_core::Error::Network {
            operation: "GET request".to_string(),
            message: "connection failed".to_string(),
            source: Some(Arc::new(std::io::Error::other("connection refused"))),
        }
    }

    fn make_version_info(tag: &str, sha: &str) -> Arc<version::Info> {
        Arc::new(version::Info::new(version::Version::parse(tag), sha))
    }

    /// Test that `exact_version` names the full tag on the target's commit, not global latest.
    ///
    /// Scenario: user is on v4, target is v6 (major-only), global latest is v8.0.0.
    /// `exact_version` should be v6.0.2 (on the v6 commit), NOT v8.0.0.
    #[test]
    fn test_exact_version_takes_full_tag_on_target_commit() {
        // Build version::Set with multiple majors
        let v6_0_2 = make_version_info("v6.0.2", "aaa");
        let v8_0_0 = make_version_info("v8.0.0", "bbb");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(6, v6_0_2.clone());
        latest_per_major.insert(8, v8_0_0.clone());

        let versions = version::Set::build(
            vec![v6_0_2, v8_0_0.clone()],
            latest_per_major,
            HashMap::new(),
            Some(v8_0_0), // Global latest is v8.0.0
        );

        // Target is major-only v6 (suggesting upgrade from v4 to v6)
        // CheckStatus::Outdated has current: String, target: VersionInfo
        let status = CheckStatus::Outdated {
            current: "v4".to_string(),
            target: make_version_info("v6", "aaa"), // major-only tag
        };

        let finding_status = to_finding_status(status, Some(&versions));

        match finding_status {
            FindingStatus::Outdated { exact_version, .. } => {
                assert_eq!(
                    exact_version,
                    Some("v6.0.2".to_string()),
                    "exact_version should be the full tag on the target commit"
                );
            }
            _ => panic!("expected Outdated status"),
        }
    }

    #[test]
    fn test_exact_version_follows_major_tag_onto_prerelease() {
        let v3_0_0 = make_version_info("v3.0.0", "stable");
        let v3_1_rc = make_version_info("v3.1.0-rc.1", "rc");
        let v3 = make_version_info("v3", "rc");
        let versions = version::Set::build(
            vec![v3_0_0.clone(), v3_1_rc, v3.clone()],
            HashMap::from([(3, v3_0_0.clone())]),
            HashMap::from([(3, v3.clone())]),
            Some(v3_0_0),
        );
        let status = CheckStatus::Outdated {
            current: "v2".to_string(),
            target: v3,
        };

        let FindingStatus::Outdated { exact_version, .. } =
            to_finding_status(status, Some(&versions))
        else {
            panic!("expected Outdated status");
        };
        assert_eq!(exact_version.as_deref(), Some("v3.1.0-rc.1"));
    }

    /// Test that `exact_version` is None for fully-qualified versions (not major-only).
    #[test]
    fn test_exact_version_none_for_full_version() {
        let v6_0_2 = make_version_info("v6.0.2", "aaa");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(6, v6_0_2.clone());

        let versions = version::Set::build(
            vec![v6_0_2.clone()],
            latest_per_major,
            HashMap::new(),
            Some(v6_0_2),
        );

        // Target is full version v6.0.2 (not major-only)
        let status = CheckStatus::Outdated {
            current: "v6.0.1".to_string(),
            target: make_version_info("v6.0.2", "aaa"), // fully qualified
        };

        let finding_status = to_finding_status(status, Some(&versions));

        match finding_status {
            FindingStatus::Outdated { exact_version, .. } => {
                assert_eq!(
                    exact_version, None,
                    "exact_version should be None for fully-qualified target"
                );
            }
            _ => panic!("expected Outdated status"),
        }
    }

    /// Test that `exact_version` is None when no `version::Set` is provided.
    #[test]
    fn test_exact_version_none_without_versions() {
        let status = CheckStatus::Outdated {
            current: "v4".to_string(),
            target: make_version_info("v6", "ccc"), // major-only tag
        };

        let finding_status = to_finding_status(status, None);

        match finding_status {
            FindingStatus::Outdated { exact_version, .. } => {
                assert_eq!(
                    exact_version, None,
                    "exact_version should be None when version::Set is not provided"
                );
            }
            _ => panic!("expected Outdated status"),
        }
    }

    /// Test that `exact_version` is None when no full tag shares the target commit.
    #[test]
    fn test_exact_version_none_without_full_tag_on_target_commit() {
        let v8_0_0 = make_version_info("v8.0.0", "bbb");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(8, v8_0_0.clone());

        let versions = version::Set::build(
            vec![v8_0_0.clone()],
            latest_per_major,
            HashMap::new(),
            Some(v8_0_0),
        );

        let status = CheckStatus::Outdated {
            current: "v4".to_string(),
            target: make_version_info("v6", "ccc"), // major-only tag
        };

        let finding_status = to_finding_status(status, Some(&versions));

        match finding_status {
            FindingStatus::Outdated { exact_version, .. } => {
                assert_eq!(
                    exact_version, None,
                    "exact_version should be None when no full tag is on the target commit"
                );
            }
            _ => panic!("expected Outdated status"),
        }
    }

    /// Test conversion of `CheckStatus::UpToDate`.
    #[test]
    fn test_to_finding_status_up_to_date() {
        let status = CheckStatus::UpToDate;
        let finding_status = to_finding_status(status, None);
        assert!(matches!(finding_status, FindingStatus::UpToDate));
    }

    /// Test conversion of `CheckStatus::Ignored`.
    #[test]
    fn test_to_finding_status_ignored() {
        let status = CheckStatus::Ignored;
        let finding_status = to_finding_status(status, None);
        assert!(matches!(finding_status, FindingStatus::Ignored));
    }

    /// Test conversion of `CheckStatus::Pinned`.
    #[test]
    fn test_to_finding_status_pinned() {
        let status = CheckStatus::Pinned {
            sha: "abc123def456".to_string(),
            tag: Some("v4.0.0".to_string()),
        };
        let finding_status = to_finding_status(status, None);
        match finding_status {
            FindingStatus::Pinned { sha, tag } => {
                assert_eq!(sha, "abc123def456");
                assert_eq!(tag, Some("v4.0.0".to_string()));
            }
            _ => panic!("expected Pinned status"),
        }
    }

    /// Test conversion of `CheckStatus::Pinned` without tag.
    #[test]
    fn test_to_finding_status_pinned_no_tag() {
        let status = CheckStatus::Pinned {
            sha: "deadbeef".to_string(),
            tag: None,
        };
        let finding_status = to_finding_status(status, None);
        match finding_status {
            FindingStatus::Pinned { sha, tag } => {
                assert_eq!(sha, "deadbeef");
                assert_eq!(tag, None);
            }
            _ => panic!("expected Pinned status"),
        }
    }

    /// Test conversion of `CheckStatus::Unresolvable`.
    #[test]
    fn test_to_finding_status_unresolvable() {
        let status = CheckStatus::Unresolvable {
            reason: "not found".to_string(),
            is_environment_error: false,
        };
        let finding_status = to_finding_status(status, None);
        match finding_status {
            FindingStatus::Unresolvable {
                reason,
                is_environment_error,
            } => {
                assert_eq!(reason, "not found");
                assert!(!is_environment_error);
            }
            _ => panic!("expected Unresolvable status"),
        }
    }

    /// Test `check_action_with_versions` when versions are available.
    #[tokio::test]
    async fn test_check_action_with_versions_success() {
        use actup_core::{ActionRef, policy};

        let action_ref = ActionRef::simple(
            "actions".to_string(),
            "checkout".to_string(),
            "v3".to_string(),
        );

        let v4_0_0 = make_version_info("v4.0.0", "abc123");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, v4_0_0.clone());

        let versions = version::Set::build(
            vec![v4_0_0.clone()],
            latest_per_major,
            HashMap::new(),
            Some(v4_0_0),
        );

        let config = Config {
            policy: actup_core::config::Policy {
                bump: policy::Update::Major,
                ..Default::default()
            },
            ..Default::default()
        };
        let registry = test_registry();
        let collected = CollectedAction {
            comment: String::new(),
            file: "test.yml".to_string(),
            line: 10,
            repo_id: action_ref.to_repo_id(),
            action_ref,
            provider: "github".to_string(),
            host: "github.com".to_string(),
            span: Span { start: 9, end: 25 },
        };

        let finding =
            check_action_with_versions(&collected, Some(&Ok(versions)), &config, &registry).await;

        assert_eq!(finding.file(), "test.yml");
        assert_eq!(finding.line(), 10);
        assert_eq!(finding.action_ref().owner(), "actions");
        assert_eq!(finding.provider(), "github");
        assert!(matches!(finding.status(), FindingStatus::Outdated { .. }));
    }

    /// Test `check_action_with_versions` when versions are None (fetch failed).
    #[tokio::test]
    async fn test_check_action_with_versions_no_versions() {
        use actup_core::ActionRef;

        let action_ref = ActionRef::simple(
            "actions".to_string(),
            "checkout".to_string(),
            "v4".to_string(),
        );

        let config = Config::default();
        let registry = test_registry();
        let collected = CollectedAction {
            comment: String::new(),
            file: "test.yml".to_string(),
            line: 5,
            repo_id: action_ref.to_repo_id(),
            action_ref,
            provider: "github".to_string(),
            host: "github.com".to_string(),
            span: Span { start: 9, end: 25 },
        };

        let finding = check_action_with_versions(&collected, None, &config, &registry).await;

        assert_eq!(finding.file(), "test.yml");
        assert_eq!(finding.line(), 5);
        assert_eq!(finding.provider(), "github");
        match finding.status() {
            FindingStatus::Unresolvable {
                reason,
                is_environment_error,
            } => {
                assert_eq!(reason, "no version fetch attempted");
                assert!(is_environment_error);
            }
            _ => panic!("expected Unresolvable status with environment error"),
        }
    }

    /// Test `check_action_with_versions` propagates provider error message.
    #[tokio::test]
    async fn test_check_action_with_versions_provider_error() {
        use actup_core::ActionRef;

        let action_ref = ActionRef::simple(
            "actions".to_string(),
            "checkout".to_string(),
            "v4".to_string(),
        );

        let config = Config::default();
        let auth_error = actup_core::Error::Auth {
            provider: "github".to_string(),
            reason: "invalid token format".to_string(),
            source: Some(Arc::new(std::io::Error::other("bad header"))),
        };
        let registry = test_registry();
        let collected = CollectedAction {
            comment: String::new(),
            file: "test.yml".to_string(),
            line: 5,
            repo_id: action_ref.to_repo_id(),
            action_ref,
            provider: "github".to_string(),
            host: "github.com".to_string(),
            span: Span { start: 9, end: 25 },
        };

        let finding =
            check_action_with_versions(&collected, Some(&Err(auth_error)), &config, &registry)
                .await;

        match finding.status() {
            FindingStatus::Unresolvable {
                reason,
                is_environment_error,
            } => {
                assert_eq!(
                    reason,
                    "authentication failed for github: invalid token format: bad header"
                );
                assert!(is_environment_error);
            }
            _ => panic!("expected Unresolvable status with provider error"),
        }
    }

    #[tokio::test]
    async fn test_fallback_environment_error_keeps_the_cause() {
        let action_ref = ActionRef::simple("actions", "checkout", "v4");
        let collected = CollectedAction {
            comment: String::new(),
            file: "test.yml".to_string(),
            line: 5,
            repo_id: action_ref.to_repo_id(),
            action_ref,
            provider: "github".to_string(),
            host: "github.com".to_string(),
            span: Span { start: 9, end: 25 },
        };
        let finding = Finding::new(
            collected.file.clone(),
            collected.line,
            collected.action_ref.clone(),
            FindingStatus::UpToDate,
            collected.provider.clone(),
            collected.host.clone(),
        );
        let mut findings = Vec::new();
        resolve_fallback_refs(
            vec![(collected, finding)],
            &registry_resolving_to(Some(connection_refused())),
            &mut findings,
        )
        .await;
        let [finding] = findings.as_slice() else {
            panic!("expected one finding, got {findings:?}");
        };
        assert!(
            matches!(
                finding.status(),
                FindingStatus::Unresolvable { reason, is_environment_error: true }
                    if reason == "network error during GET request: connection failed: connection refused"
            ),
            "{:?}",
            finding.status()
        );
    }

    /// Test that user errors (`NotFound`) are classified as `is_environment_error: false`.
    #[tokio::test]
    async fn test_check_action_with_versions_user_error() {
        use actup_core::ActionRef;

        let action_ref = ActionRef::simple(
            "actions".to_string(),
            "nonexistent".to_string(),
            "v1".to_string(),
        );

        let config = Config::default();
        let not_found_error = actup_core::Error::NotFound {
            owner: "actions".to_string(),
            repo: "nonexistent".to_string(),
            host: Some("github.com".to_string()),
        };
        let registry = test_registry();
        let collected = CollectedAction {
            comment: String::new(),
            file: "test.yml".to_string(),
            line: 10,
            repo_id: action_ref.to_repo_id(),
            action_ref,
            provider: "github".to_string(),
            host: "github.com".to_string(),
            span: Span { start: 9, end: 25 },
        };

        let finding =
            check_action_with_versions(&collected, Some(&Err(not_found_error)), &config, &registry)
                .await;

        match finding.status() {
            FindingStatus::Unresolvable {
                reason,
                is_environment_error,
            } => {
                assert!(reason.contains("not found") || reason.contains("nonexistent"));
                assert!(!is_environment_error, "NotFound should be a user error");
            }
            _ => panic!("expected Unresolvable status with user error"),
        }
    }

    /// Test `exact_version` with minor-only version (v6.0 vs v6.0.2).
    #[test]
    fn test_exact_version_minor_only() {
        let v6_0_2 = make_version_info("v6.0.2", "aaa");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(6, v6_0_2.clone());

        let versions = version::Set::build(
            vec![v6_0_2.clone()],
            latest_per_major,
            HashMap::new(),
            Some(v6_0_2),
        );

        // Target is minor-only v6.0 (has minor, not major-only)
        let status = CheckStatus::Outdated {
            current: "v5.9.0".to_string(),
            target: make_version_info("v6.0", "bbb"), // minor-only
        };

        let finding_status = to_finding_status(status, Some(&versions));

        match finding_status {
            FindingStatus::Outdated { exact_version, .. } => {
                // Minor-only versions are not considered major-only, so exact_version should be None
                assert_eq!(
                    exact_version, None,
                    "exact_version should be None for minor-only target"
                );
            }
            _ => panic!("expected Outdated status"),
        }
    }

    /// Test `exact_version` is `None` when target version equals the exact version.
    ///
    /// Avoids redundant display like "v6 (v6)" - only show suffix when they differ.
    #[test]
    fn test_exact_version_equals_target() {
        let v6 = make_version_info("v6", "aaa");

        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(6, v6.clone());

        let versions =
            version::Set::build(vec![v6.clone()], latest_per_major, HashMap::new(), Some(v6));

        // Both target and exact are "v6" - no difference
        let status = CheckStatus::Outdated {
            current: "v5".to_string(),
            target: make_version_info("v6", "aaa"), // major-only tag
        };

        let finding_status = to_finding_status(status, Some(&versions));

        match finding_status {
            FindingStatus::Outdated { exact_version, .. } => {
                // When major-only tag v6 resolves to v6 (same), exact_version should be None
                // to avoid redundant display like "v6 (v6)"
                assert_eq!(exact_version, None);
            }
            _ => panic!("expected Outdated status"),
        }
    }

    fn collected(ref_str: &str) -> CollectedAction {
        let action_ref = ActionRef::simple("actions", "checkout", ref_str);
        CollectedAction {
            comment: String::new(),
            file: "ci.yml".to_string(),
            line: 3,
            repo_id: action_ref.to_repo_id(),
            action_ref,
            provider: "github".to_string(),
            host: "github.com".to_string(),
            span: Span { start: 9, end: 25 },
        }
    }

    #[tokio::test]
    async fn check_reports_short_sha_as_user_error() {
        let versions = version::Set::build(
            vec![make_version_info("v1.0.0", &"b".repeat(40))],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        let registry = test_registry();
        let config = Config::default();
        let short = check_action_with_versions(
            &collected("aaaaaaa"),
            Some(&Ok(versions.clone())),
            &config,
            &registry,
        )
        .await;
        assert!(
            matches!(
                short.status(),
                FindingStatus::Unresolvable {
                    reason,
                    is_environment_error: false,
                } if reason == actup_core::SHORT_SHA_REASON
            ),
            "{:?}",
            short.status()
        );
        assert_eq!(
            determine_exit_code(std::slice::from_ref(&short), &[]),
            CliExitCode::UserError
        );

        let named = check_action_with_versions(
            &collected("deadbee"),
            Some(&Ok(versions)),
            &config,
            &registry,
        )
        .await;
        assert!(
            matches!(named.status(), FindingStatus::UpToDate),
            "{:?}",
            named.status()
        );
    }

    #[tokio::test]
    async fn check_suggests_the_closest_tag_for_a_typo() {
        let versions = version::Set::build(
            vec![
                make_version_info("v4.1.0", &"a".repeat(40)),
                make_version_info("v4.2.0", &"b".repeat(40)),
            ],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        let typo = check_action_with_versions(
            &collected("v42.0"),
            Some(&Ok(versions)),
            &Config::default(),
            &test_registry(),
        )
        .await;
        assert!(
            matches!(
                typo.status(),
                FindingStatus::Unresolvable {
                    is_environment_error: false,
                    ..
                }
            ),
            "{:?}",
            typo.status()
        );
        assert_eq!(
            typo.suggestion().map(|t| t.version().as_str()),
            Some("v4.2.0")
        );
    }

    fn branch_v1_at(head: &str) -> version::Set {
        let mut versions = version::Set::build(
            vec![
                make_version_info("v1.0.4", &"4".repeat(40)),
                make_version_info("v1.0.5", &"5".repeat(40)),
            ],
            HashMap::new(),
            HashMap::new(),
            None,
        );
        versions.set_branches(HashMap::from([("v1".to_string(), head.to_string())]));
        versions
    }

    fn policy_config(
        branch: actup_core::policy::Branch,
        track: actup_core::policy::Track,
    ) -> Config {
        Config {
            policy: actup_core::config::Policy {
                branch,
                track,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn check_labels_branch_on_the_target_commit_as_same_commit() {
        use actup_core::policy::{Branch, Track};

        let registry = test_registry();
        let config = policy_config(Branch::Update, Track::Keep);
        let on_target = check_action_with_versions(
            &collected("v1"),
            Some(&Ok(branch_v1_at(&"5".repeat(40)))),
            &config,
            &registry,
        )
        .await;
        assert!(
            matches!(on_target.status(), FindingStatus::PinBranch { .. }),
            "{:?}",
            on_target.status()
        );
        assert_eq!(on_target.ref_kind(), Some(actup_core::RefKind::Branch));
        assert_eq!(
            on_target.same_commit().map(|t| t.version().as_str()),
            Some("v1.0.5")
        );

        let behind = check_action_with_versions(
            &collected("v1"),
            Some(&Ok(branch_v1_at(&"4".repeat(40)))),
            &config,
            &registry,
        )
        .await;
        assert_eq!(behind.ref_kind(), Some(actup_core::RefKind::Branch));
        assert!(behind.same_commit().is_none());
    }

    #[tokio::test]
    async fn check_tags_tag_and_sha_refs_with_their_kind() {
        use actup_core::policy::{Branch, Track};

        let (old, new) = ("4".repeat(40), "5".repeat(40));
        let versions = version::Set::build(
            vec![
                make_version_info("v1", &new),
                make_version_info("v1.0.4", &old),
                make_version_info("v1.0.5", &new),
            ],
            HashMap::new(),
            HashMap::from([(1, make_version_info("v1", &new))]),
            None,
        );
        let registry = test_registry();
        let exact = policy_config(Branch::Ignore, Track::Exact);

        let tag = check_action_with_versions(
            &collected("v1"),
            Some(&Ok(versions.clone())),
            &exact,
            &registry,
        )
        .await;
        assert!(
            matches!(tag.status(), FindingStatus::Outdated { .. }),
            "{:?}",
            tag.status()
        );
        assert_eq!(tag.ref_kind(), Some(actup_core::RefKind::Tag));
        assert_eq!(
            tag.same_commit().map(|t| t.version().as_str()),
            Some("v1.0.5")
        );

        let sha =
            check_action_with_versions(&collected(&old), Some(&Ok(versions)), &exact, &registry)
                .await;
        assert!(
            matches!(sha.status(), FindingStatus::Outdated { .. }),
            "{:?}",
            sha.status()
        );
        assert_eq!(sha.ref_kind(), Some(actup_core::RefKind::Sha));
        assert!(sha.same_commit().is_none());
    }

    /// Test [`CollectedAction`] structure (ensures proper collection).
    #[test]
    fn test_collected_action_structure() {
        use actup_core::ActionRef;

        let action_ref = ActionRef::simple(
            "actions".to_string(),
            "setup-node".to_string(),
            "v4".to_string(),
        );

        let repo_id = action_ref.to_repo_id();

        let collected = CollectedAction {
            comment: String::new(),
            file: "workflow.yml".to_string(),
            line: 15,
            action_ref: action_ref.clone(),
            repo_id,
            provider: "github".to_string(),
            host: "github.com".to_string(),
            span: Span { start: 9, end: 25 },
        };

        assert_eq!(collected.file, "workflow.yml");
        assert_eq!(collected.line, 15);
        assert_eq!(collected.action_ref.repo(), "setup-node");
        assert_eq!(collected.repo_id.owner(), action_ref.owner());
        assert_eq!(collected.repo_id.repo(), action_ref.repo());
    }

    #[tokio::test]
    async fn check_branch_pin_comments_survive_parallel_collection()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        let old = "a".repeat(40);
        let head = "b".repeat(40);
        std::fs::write(
            workflows.join("ci.yml"),
            format!(
                "jobs:\n  b:\n    steps:\n      - parallel:\n          - uses: actions/checkout@{old} # master keep\n"
            ),
        )?;
        let config = Config::default();
        let registry = test_registry();
        let scanner = WorkflowScanner::new(dir.path(), &config, &registry, OutputFormat::Json);
        let mut errors = ErrorTally::default();
        let scan = Scan::collect(&scanner, &scanner.discover(), &mut errors);
        let mut findings = Vec::new();
        let actions = collect_actions(&scanner, &scan, &config, &mut findings);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].comment, "# master keep");
        let mut versions = version::Set::new();
        versions.set_branches(std::collections::HashMap::from([(
            "master".to_owned(),
            head.clone(),
        )]));
        let finding =
            check_action_with_versions(&actions[0], Some(&Ok(versions)), &config, &registry).await;
        assert!(
            matches!(finding.status(), FindingStatus::Outdated { latest, .. } if latest.sha() == head)
        );
        Ok(())
    }

    #[test]
    fn test_collect_actions_records_the_resolved_host() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        std::fs::write(
            workflows.join("ci.yml"),
            "steps:\n  - uses: actions/checkout@v4\n  - uses: ghe.corp.example/o/r@v1\n  - uses: skip/me@v1\n",
        )?;
        let config = Config::from_contents(
            Some((Path::new("actup.toml"), "[scan]\nignore = [\"skip/*\"]\n")),
            actup_core::config::EnvOverrides::default(),
        )?;
        let registry = test_registry();
        let scanner = WorkflowScanner::new(dir.path(), &config, &registry, OutputFormat::Json);
        let mut io_errors = ErrorTally::default();
        let scan = Scan::collect(&scanner, &scanner.discover(), &mut io_errors);
        let mut findings = Vec::new();

        let collected = collect_actions(&scanner, &scan, &config, &mut findings);

        let hosts: Vec<(&str, &str)> = collected
            .iter()
            .map(|c| (c.action_ref.repo(), c.host.as_str()))
            .collect();
        assert_eq!(hosts, [("checkout", "github.com")]);
        let immediate: Vec<(&str, &str, bool)> = findings
            .iter()
            .map(|f| {
                (
                    f.action_ref().repo(),
                    f.host(),
                    matches!(f.status(), FindingStatus::Ignored),
                )
            })
            .collect();
        assert_eq!(
            immediate,
            [("r", "ghe.corp.example", false), ("me", "github.com", true)]
        );
        Ok(())
    }

    #[tokio::test]
    async fn ignored_refs_on_unserved_hosts_are_ignored_findings()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        std::fs::write(
            workflows.join("ci.yml"),
            "steps:\n  - uses: ghe.corp.example/o/r@v1\n  - uses: skip/me@v1\n",
        )?;
        let config = Config::from_contents(
            Some((
                Path::new("actup.toml"),
                "[scan]\nignore = [\"ghe.corp.example/o/*\", \"skip/*\"]\n",
            )),
            actup_core::config::EnvOverrides::default(),
        )?;
        let registry = test_registry();
        let scanner = WorkflowScanner::new(dir.path(), &config, &registry, OutputFormat::Json);
        let mut io_errors = ErrorTally::default();
        let scan = Scan::collect(&scanner, &scanner.discover(), &mut io_errors);
        let mut findings = Vec::new();

        let collected = collect_actions(&scanner, &scan, &config, &mut findings);

        assert!(collected.is_empty());
        let ignored: Vec<(&str, &str, &str, bool)> = findings
            .iter()
            .map(|f| {
                (
                    f.action_ref().repo(),
                    f.provider(),
                    f.host(),
                    matches!(f.status(), FindingStatus::Ignored),
                )
            })
            .collect();
        assert_eq!(
            ignored,
            [
                ("r", "unknown", "ghe.corp.example", true),
                ("me", "github", "github.com", true),
            ]
        );
        for format in [CheckFormat::Human, CheckFormat::Json] {
            assert_eq!(
                run(
                    dir.path(),
                    &[],
                    format,
                    false,
                    &config,
                    &registry,
                    &Arc::new(FileCache::disabled())
                )
                .await,
                CliExitCode::Success
            );
        }
        Ok(())
    }

    #[test]
    fn test_collect_actions_reads_markdown_and_text_extra_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("playground"))?;
        std::fs::write(
            dir.path().join("README.md"),
            "# Usage\n\n```yaml\n- uses: actions/checkout@v6\n```\n\nuses: prose/ref@v1\n",
        )?;
        std::fs::write(
            dir.path().join("playground/index.ts"),
            "export const step = { uses: \"actions/cache@v4\" };\n",
        )?;
        let mut config = Config::default();
        config.scan.extra_paths = vec!["README.md".parse()?, "playground/*.ts".parse()?];
        let registry = test_registry();
        let scanner = WorkflowScanner::new(dir.path(), &config, &registry, OutputFormat::Json);
        let mut io_errors = ErrorTally::default();
        let scan = Scan::collect(&scanner, &scanner.discover(), &mut io_errors);
        let mut findings = Vec::new();

        let collected = collect_actions(&scanner, &scan, &config, &mut findings);

        let refs: Vec<(&str, usize, &str)> = collected
            .iter()
            .map(|c| (c.file.as_str(), c.line, c.action_ref.repo()))
            .collect();
        let ts = std::path::Path::new("playground").join("index.ts");
        assert_eq!(
            refs,
            [
                ("README.md", 4, "checkout"),
                (ts.to_string_lossy().as_ref(), 1, "cache"),
            ]
        );
        assert_eq!(findings.len(), 0);
        Ok(())
    }

    const INVALID_REFS: &str = "jobs:\n  b:\n    steps:\n      - uses: o/r@a~1\n      - uses: o/r@v1.lock\n      - uses: o/r\n      - uses: o/r@\n      - uses: a/b@v4\n          more\n      - uses: ./local\n";

    #[tokio::test]
    async fn test_check_skips_uses_keys_outside_action_positions()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        std::fs::write(
            workflows.join("ci.yml"),
            "env: { uses: x }\njobs:\n  b:\n    env: { uses: x }\n    steps:\n      - uses: ./local\n        with:\n          uses: lint\n",
        )?;
        let config = Config::default();
        let registry = test_registry();
        let scanner = WorkflowScanner::new(dir.path(), &config, &registry, OutputFormat::Json);
        let mut io_errors = ErrorTally::default();
        let scan = Scan::collect(&scanner, &scanner.discover(), &mut io_errors);
        let mut findings = Vec::new();

        let collected = collect_actions(&scanner, &scan, &config, &mut findings);

        assert!(collected.is_empty());
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(
            run(
                dir.path(),
                &[],
                CheckFormat::Json,
                false,
                &config,
                &registry,
                &Arc::new(FileCache::from_config(&config, Access::ReadWrite))
            )
            .await,
            CliExitCode::Success
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_check_reports_invalid_refs_as_user_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let workflows = dir.path().join(".github/workflows");
        std::fs::create_dir_all(&workflows)?;
        std::fs::write(workflows.join("ci.yml"), INVALID_REFS)?;
        let config = Config::default();
        let registry = test_registry();
        let scanner = WorkflowScanner::new(dir.path(), &config, &registry, OutputFormat::Json);
        let mut io_errors = ErrorTally::default();
        let scan = Scan::collect(&scanner, &scanner.discover(), &mut io_errors);
        let mut findings = Vec::new();

        let collected = collect_actions(&scanner, &scan, &config, &mut findings);

        assert!(collected.is_empty());
        let reported: Vec<(usize, String, &str, &str)> = findings
            .iter()
            .map(|f| (f.line(), f.action_ref().to_string(), f.provider(), f.host()))
            .collect();
        assert_eq!(
            reported,
            [
                (4, "o/r@a~1".to_string(), "unknown", "github.com"),
                (5, "o/r@v1.lock".to_string(), "unknown", "github.com"),
                (6, "o/r@".to_string(), "unknown", "github.com"),
                (7, "o/r@".to_string(), "unknown", "github.com"),
                (8, "a/b@v4 more".to_string(), "unknown", "github.com"),
            ]
        );
        for finding in &findings {
            assert!(
                matches!(
                    finding.status(),
                    FindingStatus::Unresolvable {
                        reason,
                        is_environment_error: false,
                    } if reason.starts_with("invalid action reference")
                ),
                "{:?}",
                finding.status()
            );
        }
        assert_eq!(determine_exit_code(&findings, &[]), CliExitCode::UserError);
        for format in [CheckFormat::Human, CheckFormat::Json, CheckFormat::Toml] {
            assert_eq!(
                run(
                    dir.path(),
                    &[],
                    format,
                    false,
                    &config,
                    &registry,
                    &Arc::new(FileCache::from_config(&config, Access::ReadWrite))
                )
                .await,
                CliExitCode::UserError
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_run_without_workflows_succeeds_in_every_format()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let config = Config::default();
        let registry = test_registry();
        for format in [
            CheckFormat::Human,
            CheckFormat::Json,
            CheckFormat::Toml,
            CheckFormat::Table,
            CheckFormat::Sarif,
        ] {
            assert_eq!(
                run(
                    dir.path(),
                    &[],
                    format,
                    false,
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

    /// Test `FindingStatus::Unknown` is correctly created for fallback resolution.
    #[test]
    fn test_finding_status_unknown() {
        use actup_core::ActionRef;

        let action_ref = ActionRef::simple(
            "taiki-e".to_string(),
            "install-action".to_string(),
            "v2".to_string(),
        );

        let finding = Finding::new(
            "workflow.yml".to_string(),
            10,
            action_ref,
            FindingStatus::Unknown {
                reason: "ref exists but version comparison unavailable".to_string(),
            },
            "github".to_string(),
            "github.com".to_string(),
        );

        match finding.status() {
            FindingStatus::Unknown { reason } => {
                assert!(reason.contains("version comparison unavailable"));
            }
            _ => panic!("expected Unknown status"),
        }
    }
}
