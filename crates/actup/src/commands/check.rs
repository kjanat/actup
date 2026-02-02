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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use actup_core::UpdatePolicy;
    use std::collections::HashMap;

    fn make_action_ref(owner: &str, repo: &str, ref_str: &str) -> ActionRef {
        ActionRef {
            owner: owner.to_string(),
            repo: repo.to_string(),
            ref_str: ref_str.to_string(),
            host: None,
            subpath: None,
        }
    }

    fn make_version_info(ver: &str, sha: &str) -> VersionInfo {
        VersionInfo {
            version: Version::parse(ver),
            sha: sha.to_string(),
        }
    }

    fn make_version_set(
        tags: Vec<VersionInfo>,
        latest: Option<VersionInfo>,
        latest_per_major: HashMap<u32, VersionInfo>,
    ) -> VersionSet {
        VersionSet {
            semver_tags: tags,
            latest_per_major,
            major_tags: HashMap::new(),
            latest,
            default_branch: None,
            fetched_at: None,
        }
    }

    // ========================================================================
    // is_version_current tests
    // ========================================================================

    #[test]
    fn test_is_version_current_exact_match() {
        let current = Version::parse("v1.2.3");
        let target = make_version_info("v1.2.3", "aaa");
        assert!(is_version_current(&current, Some(&target), "v1.2.3"));
    }

    #[test]
    fn test_is_version_current_not_match() {
        let current = Version::parse("v1.0.0");
        let target = make_version_info("v1.2.3", "aaa");
        assert!(!is_version_current(&current, Some(&target), "v1.0.0"));
    }

    #[test]
    fn test_is_version_current_major_only() {
        let current = Version::parse("v4");
        let target = make_version_info("v4.2.1", "aaa");
        // v4 is up-to-date if latest in major 4 is v4.x.x
        assert!(is_version_current(&current, Some(&target), "v4"));
    }

    #[test]
    fn test_is_version_current_major_mismatch() {
        let current = Version::parse("v3");
        let target = make_version_info("v4.0.0", "aaa");
        assert!(!is_version_current(&current, Some(&target), "v3"));
    }

    #[test]
    fn test_is_version_current_no_target() {
        let current = Version::parse("v1.0.0");
        assert!(!is_version_current(&current, None, "v1.0.0"));
    }

    // ========================================================================
    // status_for_no_target tests
    // ========================================================================

    #[test]
    fn test_status_for_no_target_semver_no_versions() {
        let current = Version::parse("v1.0.0");
        let status = status_for_no_target(&current, BranchPolicy::Ignore);
        match status {
            FindingStatus::Unresolvable {
                reason,
                is_environment_error,
            } => {
                assert_eq!(reason, "no versions found");
                assert!(!is_environment_error);
            }
            _ => panic!("expected Unresolvable"),
        }
    }

    #[test]
    fn test_status_for_no_target_branch_ignore() {
        let current = Version::parse("main");
        let status = status_for_no_target(&current, BranchPolicy::Ignore);
        assert!(matches!(status, FindingStatus::UpToDate));
    }

    #[test]
    fn test_status_for_no_target_branch_warn() {
        let current = Version::parse("main");
        let status = status_for_no_target(&current, BranchPolicy::Warn);
        match status {
            FindingStatus::Unresolvable { reason, .. } => {
                assert!(reason.contains("non-semver"));
            }
            _ => panic!("expected Unresolvable"),
        }
    }

    #[test]
    fn test_status_for_no_target_branch_update() {
        let current = Version::parse("main");
        let status = status_for_no_target(&current, BranchPolicy::Update);
        match status {
            FindingStatus::Unresolvable { reason, .. } => {
                assert!(reason.contains("pinned to SHA"));
            }
            _ => panic!("expected Unresolvable"),
        }
    }

    // ========================================================================
    // determine_status tests
    // ========================================================================

    #[test]
    fn test_determine_status_up_to_date() {
        let action_ref = make_action_ref("actions", "checkout", "v4.2.0");
        let v4_2 = make_version_info("v4.2.0", "aaa");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, v4_2.clone());
        let versions = make_version_set(vec![v4_2.clone()], Some(v4_2), latest_per_major);
        let config = Config::default();

        let status = determine_status(&action_ref, &versions, &config);
        assert!(matches!(status, FindingStatus::UpToDate));
    }

    #[test]
    fn test_determine_status_outdated() {
        let action_ref = make_action_ref("actions", "checkout", "v4.0.0");
        let v4_0 = make_version_info("v4.0.0", "aaa");
        let v4_2 = make_version_info("v4.2.0", "bbb");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, v4_2.clone());
        let versions = make_version_set(vec![v4_0, v4_2.clone()], Some(v4_2), latest_per_major);
        let config = Config::default();

        let status = determine_status(&action_ref, &versions, &config);
        match status {
            FindingStatus::Outdated { current, latest } => {
                assert_eq!(current, "v4.0.0");
                assert_eq!(latest.version.to_string(), "v4.2.0");
            }
            _ => panic!("expected Outdated, got {status:?}"),
        }
    }

    #[test]
    fn test_determine_status_major_tag_up_to_date() {
        let action_ref = make_action_ref("actions", "checkout", "v4");
        let v4_2 = make_version_info("v4.2.0", "aaa");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(4, v4_2.clone());
        let versions = make_version_set(vec![v4_2.clone()], Some(v4_2), latest_per_major);
        let config = Config::default();

        let status = determine_status(&action_ref, &versions, &config);
        assert!(matches!(status, FindingStatus::UpToDate));
    }

    #[test]
    fn test_determine_status_with_policy_override() {
        let action_ref = make_action_ref("actions", "checkout", "v3.0.0");
        let v3 = make_version_info("v3.0.0", "aaa");
        let v4 = make_version_info("v4.0.0", "bbb");
        let mut latest_per_major = HashMap::new();
        latest_per_major.insert(3, v3.clone());
        latest_per_major.insert(4, v4.clone());
        let versions = make_version_set(vec![v3, v4.clone()], Some(v4), latest_per_major);

        // With minor policy, v3.0.0 is up-to-date (latest v3.x)
        let config = Config {
            update_policy: UpdatePolicy::Minor,
            ..Default::default()
        };

        let status = determine_status(&action_ref, &versions, &config);
        assert!(matches!(status, FindingStatus::UpToDate));
    }
}
