//! Check command - scan workflows and report outdated actions

use std::fs;
use std::path::Path;

use actup_core::{
    Config, ProviderRegistry, Version, discover_files, extract_uses_from_yaml, parse_uses,
};
use owo_colors::OwoColorize;

use crate::cli::OutputFormat;
use crate::output::{Finding, FindingStatus, determine_exit_code, print_findings, print_summary};

pub async fn run(
    path: &Path,
    strict: bool,
    format: OutputFormat,
    config: &Config,
    registry: &ProviderRegistry,
) -> i32 {
    let discovered = discover_files(path, &config.extra_paths);

    if discovered.is_empty() {
        if matches!(format, OutputFormat::Human) {
            println!(
                "{}",
                "No workflow files found in .github/workflows/".dimmed()
            );
        }
        return 0;
    }

    let mut findings = Vec::new();

    for file in &discovered {
        let content = match fs::read_to_string(&file.path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{}: {} - {}", "Warning".yellow(), file.path.display(), e);
                continue;
            }
        };

        let uses_locations = extract_uses_from_yaml(&content);

        for loc in uses_locations {
            let action_ref = match parse_uses(&loc.value) {
                Ok(Some(r)) => r,
                Ok(None) => continue, // Local action like ./path
                Err(e) => {
                    eprintln!(
                        "{}: {}:{} - {}",
                        "Warning".yellow(),
                        file.path.display(),
                        loc.line,
                        e
                    );
                    continue;
                }
            };

            let file_str = file
                .path
                .strip_prefix(path)
                .unwrap_or(&file.path)
                .display()
                .to_string();

            // Check if action should be ignored (per-action override or ignore list)
            if config.should_ignore(&action_ref.owner, &action_ref.repo) {
                continue;
            }

            // Check if it's already a SHA (40 hex chars)
            if action_ref.is_sha() {
                findings.push(Finding {
                    file: file_str,
                    line: loc.line,
                    action_ref: action_ref.clone(),
                    status: FindingStatus::Pinned {
                        sha: action_ref.ref_str.clone(),
                        tag: None,
                    },
                });
                continue;
            }

            // Try to resolve versions
            let provider = match registry.find(action_ref.host.as_deref()) {
                Some(p) => p,
                None => {
                    findings.push(Finding {
                        file: file_str,
                        line: loc.line,
                        action_ref: action_ref.clone(),
                        status: FindingStatus::Unresolvable {
                            reason: format!(
                                "no provider for host: {}",
                                action_ref.host.as_deref().unwrap_or("github.com")
                            ),
                        },
                    });
                    continue;
                }
            };

            let repo_id = action_ref.to_repo_id();
            let versions = match provider.get_versions(&repo_id).await {
                Ok(v) => v,
                Err(e) => {
                    findings.push(Finding {
                        file: file_str,
                        line: loc.line,
                        action_ref: action_ref.clone(),
                        status: FindingStatus::Unresolvable {
                            reason: e.to_string(),
                        },
                    });
                    continue;
                }
            };

            // Get effective policy for this action (respects per-action overrides)
            let policy = config.policy_for(&action_ref.owner, &action_ref.repo);

            // Parse current ref as Version for proper comparison
            let current_version = Version::parse(&action_ref.ref_str);

            // Find the appropriate latest version based on policy
            let target = versions.find_latest_for(
                &current_version,
                policy.allows_major(),
                config.allow_prerelease,
            );

            // Check if current matches target (handles v4 == v4.1.7 via major comparison)
            let is_up_to_date = match (&current_version, target) {
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
                (_, Some(t)) => t.version.to_string() == action_ref.ref_str,
                (_, None) => false,
            };

            match (is_up_to_date, target) {
                (true, _) => {
                    findings.push(Finding {
                        file: file_str,
                        line: loc.line,
                        action_ref: action_ref.clone(),
                        status: FindingStatus::UpToDate,
                    });
                }
                (false, Some(latest_v)) => {
                    findings.push(Finding {
                        file: file_str,
                        line: loc.line,
                        action_ref: action_ref.clone(),
                        status: FindingStatus::Outdated {
                            current: action_ref.ref_str.clone(),
                            latest: latest_v.clone(),
                        },
                    });
                }
                (false, None) => {
                    // No semver tags - could be branch ref or no tags at all
                    // Check branch_policy before marking as unresolvable
                    if current_version.is_branch() || !current_version.is_semver() {
                        match config.branch_policy {
                            actup_core::BranchPolicy::Ignore => continue,
                            actup_core::BranchPolicy::Warn => {
                                findings.push(Finding {
                                    file: file_str,
                                    line: loc.line,
                                    action_ref: action_ref.clone(),
                                    status: FindingStatus::Unresolvable {
                                        reason: "non-semver ref (branch or tag)".to_string(),
                                    },
                                });
                            }
                            actup_core::BranchPolicy::Update => {
                                findings.push(Finding {
                                    file: file_str,
                                    line: loc.line,
                                    action_ref: action_ref.clone(),
                                    status: FindingStatus::Unresolvable {
                                        reason: "non-semver ref should be pinned to SHA"
                                            .to_string(),
                                    },
                                });
                            }
                        }
                    } else {
                        findings.push(Finding {
                            file: file_str,
                            line: loc.line,
                            action_ref: action_ref.clone(),
                            status: FindingStatus::Unresolvable {
                                reason: "no versions found".to_string(),
                            },
                        });
                    }
                }
            }
        }
    }

    print_findings(&findings, format);

    if matches!(format, OutputFormat::Human) {
        print_summary(&findings);
    }

    determine_exit_code(&findings, strict)
}
