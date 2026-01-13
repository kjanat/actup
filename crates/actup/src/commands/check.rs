//! Check command - scan workflows and report outdated actions

use std::fs;
use std::path::Path;

use actup_core::{discover_files, extract_uses_from_yaml, parse_uses, Config, ProviderRegistry};
use owo_colors::OwoColorize;

use crate::cli::OutputFormat;
use crate::output::{determine_exit_code, print_findings, print_summary, Finding, FindingStatus};

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
                eprintln!(
                    "{}: {} - {}",
                    "Warning".yellow(),
                    file.path.display(),
                    e
                );
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

            // Find if current ref matches latest
            let current_version = versions
                .semver_tags
                .iter()
                .find(|v| v.version.to_string() == action_ref.ref_str);
            let latest = versions.latest.as_ref();

            match (current_version, latest) {
                (Some(_), Some(latest_v)) if latest_v.version.to_string() == action_ref.ref_str => {
                    findings.push(Finding {
                        file: file_str,
                        line: loc.line,
                        action_ref: action_ref.clone(),
                        status: FindingStatus::UpToDate,
                    });
                }
                (_, Some(latest_v)) => {
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
                (_, None) => {
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

    print_findings(&findings, format);

    if matches!(format, OutputFormat::Human) {
        print_summary(&findings);
    }

    determine_exit_code(&findings, strict)
}
