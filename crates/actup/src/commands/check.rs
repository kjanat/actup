//! Check command - scan workflows and report outdated actions

use std::path::Path;

use actup_core::{
    ActionRef, BranchPolicy, Config, Provider, ProviderRegistry, Version, VersionInfo, VersionSet,
};

use crate::cli::OutputFormat;
use crate::common::{ErrorTally, WorkflowScanner};
use crate::output::{Finding, FindingStatus, determine_exit_code, print_findings, print_summary};

/// Determine the finding status for an action given its versions.
fn determine_status(
    action_ref: &ActionRef,
    versions: &VersionSet,
    config: &Config,
) -> FindingStatus {
    let policy = config.policy_for(&action_ref.owner, &action_ref.repo);
    let current = Version::parse(&action_ref.ref_str);

    let target = versions.find_latest_for(&current, policy.allows_major(), config.allow_prerelease);

    // Check if current matches target (handles v4 == v4.1.7 via major comparison)
    let is_up_to_date = is_version_current(&current, target, &action_ref.ref_str);

    match (is_up_to_date, target) {
        (true, _) => FindingStatus::UpToDate,
        (false, Some(latest)) => FindingStatus::Outdated {
            current: action_ref.ref_str.clone(),
            latest: latest.clone(),
        },
        (false, None) => status_for_no_target(&current, config.branch_policy),
    }
}

/// Check if current version is up-to-date with target.
fn is_version_current(current: &Version, target: Option<&VersionInfo>, ref_str: &str) -> bool {
    match (current, target) {
        // For major-only refs (v4), compare major versions
        (
            Version::Semver {
                major,
                minor: None,
                patch: None,
                ..
            },
            Some(t),
        ) => t.version.major() == Some(*major),
        // For full semver, exact match
        (_, Some(t)) => t.version.to_string() == ref_str,
        (_, None) => false,
    }
}

/// Determine status when no target version is found.
fn status_for_no_target(current: &Version, branch_policy: BranchPolicy) -> FindingStatus {
    if !current.is_branch() && current.is_semver() {
        return FindingStatus::Unresolvable {
            reason: "no versions found".to_string(),
            is_environment_error: false,
        };
    }

    match branch_policy {
        BranchPolicy::Ignore => FindingStatus::UpToDate, // Treat as OK when ignoring
        BranchPolicy::Warn => FindingStatus::Unresolvable {
            reason: "non-semver ref (branch or tag)".to_string(),
            is_environment_error: false,
        },
        BranchPolicy::Update => FindingStatus::Unresolvable {
            reason: "non-semver ref should be pinned to SHA".to_string(),
            is_environment_error: false,
        },
    }
}

/// Check a single action and return a finding.
async fn check_action(
    action_ref: &ActionRef,
    file: &str,
    line: usize,
    provider: &dyn Provider,
    config: &Config,
) -> Finding {
    let repo_id = action_ref.to_repo_id();

    let status = match provider.get_versions(&repo_id).await {
        Ok(versions) => determine_status(action_ref, &versions, config),
        Err(e) => FindingStatus::Unresolvable {
            reason: e.to_string(),
            is_environment_error: e.is_environment_error(),
        },
    };

    Finding {
        file: file.to_string(),
        line,
        action_ref: action_ref.clone(),
        status,
    }
}

pub async fn run(
    path: &Path,
    strict: bool,
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

    let mut findings = Vec::new();
    let mut io_errors = ErrorTally::default();

    for file in &discovered {
        let Some(content) = scanner.read_file(file, &mut io_errors) else {
            continue;
        };

        let relative = scanner.relative_path(file);

        for (loc, action_ref) in scanner.extract_actions(&content) {
            // Already pinned to SHA
            if action_ref.is_sha() {
                findings.push(Finding {
                    file: relative.clone(),
                    line: loc.line,
                    action_ref: action_ref.clone(),
                    status: FindingStatus::Pinned {
                        sha: action_ref.ref_str.clone(),
                        tag: None,
                    },
                });
                continue;
            }

            // No provider for host
            let Some(provider) = scanner.provider_for(&action_ref) else {
                findings.push(Finding {
                    file: relative.clone(),
                    line: loc.line,
                    action_ref: action_ref.clone(),
                    status: FindingStatus::Unresolvable {
                        reason: format!(
                            "no provider for host: {}",
                            action_ref.host.as_deref().unwrap_or("github.com")
                        ),
                        is_environment_error: false,
                    },
                });
                continue;
            };

            findings.push(check_action(&action_ref, &relative, loc.line, provider, config).await);
        }
    }

    print_findings(&findings, format);

    if matches!(format, OutputFormat::Human) {
        print_summary(&findings);
    }

    // I/O errors take priority (env error = exit 3)
    let io_exit = io_errors.exit_code();
    if io_exit != crate::cli::CliExitCode::Success {
        return io_exit.into();
    }

    determine_exit_code(&findings, strict)
}
