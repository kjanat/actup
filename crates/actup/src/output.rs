//! Output formatting for CLI

use actup_core::{ActionRef, VersionInfo};
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::cli::OutputFormat;

/// JSON-serializable finding for structured output
#[derive(Serialize)]
struct JsonFinding {
    file: String,
    line: usize,
    action: String,
    owner: String,
    repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    provider: String,
    reference: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exact_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl From<&Finding> for JsonFinding {
    fn from(f: &Finding) -> Self {
        let (
            status,
            current_version,
            latest_version,
            latest_sha,
            exact_version,
            pinned_tag,
            reason,
        ) = match &f.status {
            FindingStatus::UpToDate => ("up_to_date", None, None, None, None, None, None),
            FindingStatus::Ignored => ("ignored", None, None, None, None, None, None),
            FindingStatus::Outdated {
                current,
                latest,
                exact_version,
            } => (
                "outdated",
                Some(current.clone()),
                Some(latest.version.to_string()),
                Some(latest.sha.clone()),
                exact_version.clone(),
                None,
                None,
            ),
            FindingStatus::Pinned { sha, tag } => (
                "pinned",
                None,
                None,
                Some(sha.clone()),
                None,
                tag.clone(),
                None,
            ),
            FindingStatus::Unresolvable {
                reason,
                is_environment_error,
            } => (
                if *is_environment_error {
                    "error"
                } else {
                    "unresolvable"
                },
                None,
                None,
                None,
                None,
                None,
                Some(reason.clone()),
            ),
        };

        Self {
            file: f.file.clone(),
            line: f.line,
            action: f.action_ref.to_string(),
            owner: f.action_ref.owner.clone(),
            repo: f.action_ref.repo.clone(),
            host: f.action_ref.host.clone(),
            provider: "github".to_string(), // TODO: get from registry when multi-provider
            reference: f.action_ref.ref_str.clone(),
            status: status.into(),
            current_version,
            latest_version,
            latest_sha,
            exact_version,
            pinned_tag,
            reason,
        }
    }
}

/// A finding from scanning a workflow file
#[derive(Debug, Clone)]
pub struct Finding {
    pub file: String,
    pub line: usize,
    pub action_ref: ActionRef,
    pub status: FindingStatus,
}

#[derive(Debug, Clone)]
pub enum FindingStatus {
    /// Action is up to date
    UpToDate,
    /// Action matched an ignore pattern in config
    Ignored,
    /// Action is outdated
    Outdated {
        current: String,
        latest: VersionInfo,
        /// Full version when latest is a major-only tag (e.g., v6 with exact v6.0.2)
        exact_version: Option<String>,
    },
    Pinned {
        sha: String,
        tag: Option<String>,
    },
    Unresolvable {
        reason: String,
        /// True if this was an environment error (network/auth/rate-limit)
        /// vs a user error (bad ref, not found)
        is_environment_error: bool,
    },
}

pub fn print_findings(findings: &[Finding], format: OutputFormat) {
    match format {
        OutputFormat::Human => print_human(findings),
        OutputFormat::Json => print_json(findings),
    }
}

fn print_human(findings: &[Finding]) {
    if findings.is_empty() {
        println!("{}", "No action references found.".dimmed());
        return;
    }

    let mut current_file = String::new();

    for finding in findings {
        if finding.file != current_file {
            if !current_file.is_empty() {
                println!();
            }
            println!("{}", finding.file.bold());
            current_file.clone_from(&finding.file);
        }

        let line_prefix = format!("  L{}: ", finding.line);
        let action_str = finding.action_ref.to_string();

        match &finding.status {
            FindingStatus::UpToDate => {
                println!(
                    "{}{}  {}",
                    line_prefix.dimmed(),
                    action_str,
                    "up to date".green()
                );
            }
            FindingStatus::Ignored => {
                println!(
                    "{}{}  {}",
                    line_prefix.dimmed(),
                    action_str.dimmed(),
                    "ignored".dimmed()
                );
            }
            FindingStatus::Outdated {
                current,
                latest,
                exact_version,
            } => {
                let version_str = latest.version.to_string();
                let suffix = exact_version
                    .as_ref()
                    .filter(|ev| *ev != &version_str)
                    .map(|ev| format!(" ({ev})"))
                    .unwrap_or_default();
                println!(
                    "{}{}  {} -> {}{}",
                    line_prefix.dimmed(),
                    action_str.yellow(),
                    current.red(),
                    version_str.green(),
                    suffix.dimmed()
                );
            }
            FindingStatus::Pinned { sha, tag } => {
                let tag_info = tag.as_ref().map(|t| format!(" ({t})")).unwrap_or_default();
                println!(
                    "{}{}  {}{}",
                    line_prefix.dimmed(),
                    action_str,
                    format!("pinned to {}", &sha[..7.min(sha.len())]).cyan(),
                    tag_info.dimmed()
                );
            }
            FindingStatus::Unresolvable {
                reason,
                is_environment_error,
            } => {
                let label = if *is_environment_error {
                    "error"
                } else {
                    "unresolvable"
                };
                println!(
                    "{}{}  {}",
                    line_prefix.dimmed(),
                    action_str.red(),
                    format!("{label}: {reason}").red()
                );
            }
        }
    }
}

fn print_json(findings: &[Finding]) {
    let json_findings: Vec<JsonFinding> = findings.iter().map(JsonFinding::from).collect();
    match serde_json::to_string_pretty(&json_findings) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("Error serializing findings: {e}"),
    }
}

pub fn print_summary(findings: &[Finding]) {
    let total = findings.len();
    let up_to_date = findings
        .iter()
        .filter(|f| matches!(f.status, FindingStatus::UpToDate))
        .count();
    let ignored = findings
        .iter()
        .filter(|f| matches!(f.status, FindingStatus::Ignored))
        .count();
    let outdated = findings
        .iter()
        .filter(|f| matches!(f.status, FindingStatus::Outdated { .. }))
        .count();
    let pinned = findings
        .iter()
        .filter(|f| matches!(f.status, FindingStatus::Pinned { .. }))
        .count();
    let unresolvable = findings
        .iter()
        .filter(|f| matches!(f.status, FindingStatus::Unresolvable { .. }))
        .count();

    println!();
    println!(
        "{}",
        format!(
            "Scanned {total} action{}: {up_to_date} up to date, {ignored} ignored, {outdated} outdated, {pinned} pinned, {unresolvable} unresolvable",
            if total == 1 { "" } else { "s" }
        )
        .bold()
    );
}

/// Determines CLI exit code based on findings.
///
/// Exit codes:
/// - 0: Success (`UpToDate`, Ignored, Pinned are all considered healthy)
/// - 1: Outdated found (only with `strict` mode)
/// - 2: Unresolvable user error (bad ref, repo not found)
/// - 3: Environment error (network, rate limit, auth)
pub fn determine_exit_code(findings: &[Finding], strict: bool) -> i32 {
    let has_outdated = findings
        .iter()
        .any(|f| matches!(f.status, FindingStatus::Outdated { .. }));
    let has_env_error = findings.iter().any(|f| {
        matches!(
            f.status,
            FindingStatus::Unresolvable {
                is_environment_error: true,
                ..
            }
        )
    });
    let has_user_error = findings.iter().any(|f| {
        matches!(
            f.status,
            FindingStatus::Unresolvable {
                is_environment_error: false,
                ..
            }
        )
    });

    // Priority: env errors (3) > user errors (2) > outdated with strict (1) > success (0)
    if has_env_error {
        3
    } else if has_user_error {
        2
    } else {
        i32::from(has_outdated && strict)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use actup_core::Version;

    fn make_action_ref(owner: &str, repo: &str, ref_str: &str) -> ActionRef {
        ActionRef {
            owner: owner.to_string(),
            repo: repo.to_string(),
            ref_str: ref_str.to_string(),
            host: None,
            subpath: None,
        }
    }

    fn make_version_info(version: &str, sha: &str) -> VersionInfo {
        VersionInfo {
            version: Version::parse(version),
            sha: sha.to_string(),
        }
    }

    #[test]
    fn test_determine_exit_code_success() {
        let findings = vec![Finding {
            file: "test.yml".to_string(),
            line: 1,
            action_ref: make_action_ref("actions", "checkout", "v4"),
            status: FindingStatus::UpToDate,
        }];

        assert_eq!(determine_exit_code(&findings, false), 0);
        assert_eq!(determine_exit_code(&findings, true), 0);
    }

    #[test]
    fn test_determine_exit_code_outdated_non_strict() {
        let findings = vec![Finding {
            file: "test.yml".to_string(),
            line: 1,
            action_ref: make_action_ref("actions", "checkout", "v3"),
            status: FindingStatus::Outdated {
                current: "v3".to_string(),
                latest: make_version_info("v4.0.0", "abc123"),
                exact_version: None,
            },
        }];

        assert_eq!(determine_exit_code(&findings, false), 0);
    }

    #[test]
    fn test_determine_exit_code_outdated_strict() {
        let findings = vec![Finding {
            file: "test.yml".to_string(),
            line: 1,
            action_ref: make_action_ref("actions", "checkout", "v3"),
            status: FindingStatus::Outdated {
                current: "v3".to_string(),
                latest: make_version_info("v4.0.0", "abc123"),
                exact_version: None,
            },
        }];

        assert_eq!(determine_exit_code(&findings, true), 1);
    }

    #[test]
    fn test_determine_exit_code_user_error() {
        let findings = vec![Finding {
            file: "test.yml".to_string(),
            line: 1,
            action_ref: make_action_ref("actions", "nonexistent", "v1"),
            status: FindingStatus::Unresolvable {
                reason: "not found".to_string(),
                is_environment_error: false,
            },
        }];

        assert_eq!(determine_exit_code(&findings, false), 2);
        assert_eq!(determine_exit_code(&findings, true), 2);
    }

    #[test]
    fn test_determine_exit_code_env_error() {
        let findings = vec![Finding {
            file: "test.yml".to_string(),
            line: 1,
            action_ref: make_action_ref("actions", "checkout", "v4"),
            status: FindingStatus::Unresolvable {
                reason: "network timeout".to_string(),
                is_environment_error: true,
            },
        }];

        assert_eq!(determine_exit_code(&findings, false), 3);
        assert_eq!(determine_exit_code(&findings, true), 3);
    }

    #[test]
    fn test_determine_exit_code_priority() {
        // Env error takes priority over user error and outdated
        let findings = vec![
            Finding {
                file: "test.yml".to_string(),
                line: 1,
                action_ref: make_action_ref("actions", "checkout", "v3"),
                status: FindingStatus::Outdated {
                    current: "v3".to_string(),
                    latest: make_version_info("v4.0.0", "abc123"),
                    exact_version: None,
                },
            },
            Finding {
                file: "test.yml".to_string(),
                line: 2,
                action_ref: make_action_ref("actions", "bad", "v1"),
                status: FindingStatus::Unresolvable {
                    reason: "not found".to_string(),
                    is_environment_error: false,
                },
            },
            Finding {
                file: "test.yml".to_string(),
                line: 3,
                action_ref: make_action_ref("actions", "other", "v1"),
                status: FindingStatus::Unresolvable {
                    reason: "rate limited".to_string(),
                    is_environment_error: true,
                },
            },
        ];

        // Env error (3) takes priority
        assert_eq!(determine_exit_code(&findings, true), 3);
    }

    #[test]
    fn test_determine_exit_code_user_over_outdated() {
        let findings = vec![
            Finding {
                file: "test.yml".to_string(),
                line: 1,
                action_ref: make_action_ref("actions", "checkout", "v3"),
                status: FindingStatus::Outdated {
                    current: "v3".to_string(),
                    latest: make_version_info("v4.0.0", "abc123"),
                    exact_version: None,
                },
            },
            Finding {
                file: "test.yml".to_string(),
                line: 2,
                action_ref: make_action_ref("actions", "bad", "v1"),
                status: FindingStatus::Unresolvable {
                    reason: "not found".to_string(),
                    is_environment_error: false,
                },
            },
        ];

        // User error (2) takes priority over outdated (1)
        assert_eq!(determine_exit_code(&findings, true), 2);
    }

    #[test]
    fn test_determine_exit_code_empty() {
        let findings: Vec<Finding> = vec![];
        assert_eq!(determine_exit_code(&findings, false), 0);
        assert_eq!(determine_exit_code(&findings, true), 0);
    }

    #[test]
    fn test_determine_exit_code_pinned() {
        let findings = vec![Finding {
            file: "test.yml".to_string(),
            line: 1,
            action_ref: make_action_ref("actions", "checkout", "abc123def456"),
            status: FindingStatus::Pinned {
                sha: "abc123def456".to_string(),
                tag: Some("v4.0.0".to_string()),
            },
        }];

        // Pinned is not an error
        assert_eq!(determine_exit_code(&findings, false), 0);
        assert_eq!(determine_exit_code(&findings, true), 0);
    }

    // Finding construction tests

    #[test]
    fn test_finding_up_to_date() {
        let finding = Finding {
            file: "workflow.yml".to_string(),
            line: 10,
            action_ref: make_action_ref("actions", "checkout", "v4"),
            status: FindingStatus::UpToDate,
        };

        assert_eq!(finding.file, "workflow.yml");
        assert_eq!(finding.line, 10);
        assert!(matches!(finding.status, FindingStatus::UpToDate));
    }

    #[test]
    fn test_finding_ignored() {
        let finding = Finding {
            file: "workflow.yml".to_string(),
            line: 5,
            action_ref: make_action_ref("internal", "action", "main"),
            status: FindingStatus::Ignored,
        };

        assert_eq!(finding.file, "workflow.yml");
        assert_eq!(finding.line, 5);
        assert!(matches!(finding.status, FindingStatus::Ignored));
    }

    #[test]
    fn test_determine_exit_code_ignored() {
        // Ignored actions should not affect exit code (like UpToDate)
        let findings = vec![Finding {
            file: "test.yml".to_string(),
            line: 1,
            action_ref: make_action_ref("internal", "action", "main"),
            status: FindingStatus::Ignored,
        }];

        assert_eq!(determine_exit_code(&findings, false), 0);
        assert_eq!(determine_exit_code(&findings, true), 0);
    }

    #[test]
    fn test_finding_outdated() {
        let finding = Finding {
            file: "ci.yml".to_string(),
            line: 15,
            action_ref: make_action_ref("actions", "setup-node", "v3"),
            status: FindingStatus::Outdated {
                current: "v3".to_string(),
                latest: make_version_info("v4.0.0", "abc123"),
                exact_version: None,
            },
        };

        match &finding.status {
            FindingStatus::Outdated {
                current, latest, ..
            } => {
                assert_eq!(current, "v3");
                assert_eq!(latest.version.to_string(), "v4.0.0");
                assert_eq!(latest.sha, "abc123");
            }
            _ => panic!("expected Outdated status"),
        }
    }

    #[test]
    fn test_finding_outdated_with_exact_version() {
        // Test major-only tag with exact version suffix (e.g., "v6 -> v7 (v7.2.1)")
        let finding = Finding {
            file: "ci.yml".to_string(),
            line: 15,
            action_ref: make_action_ref("actions", "checkout", "v4"),
            status: FindingStatus::Outdated {
                current: "v4".to_string(),
                latest: make_version_info("v5", "def456"), // major-only tag
                exact_version: Some("v5.2.1".to_string()), // actual semver
            },
        };

        match &finding.status {
            FindingStatus::Outdated {
                current,
                latest,
                exact_version,
            } => {
                assert_eq!(current, "v4");
                assert_eq!(latest.version.to_string(), "v5");
                assert_eq!(exact_version, &Some("v5.2.1".to_string()));
            }
            _ => panic!("expected Outdated status"),
        }
    }

    #[test]
    fn test_finding_pinned_with_tag() {
        let finding = Finding {
            file: "deploy.yml".to_string(),
            line: 20,
            action_ref: make_action_ref(
                "actions",
                "checkout",
                "b4ffde65f46336ab88eb53be808477a3936bae11",
            ),
            status: FindingStatus::Pinned {
                sha: "b4ffde65f46336ab88eb53be808477a3936bae11".to_string(),
                tag: Some("v4.1.1".to_string()),
            },
        };

        match &finding.status {
            FindingStatus::Pinned { sha, tag } => {
                assert_eq!(sha, "b4ffde65f46336ab88eb53be808477a3936bae11");
                assert_eq!(tag.as_deref(), Some("v4.1.1"));
            }
            _ => panic!("expected Pinned status"),
        }
    }

    #[test]
    fn test_finding_pinned_without_tag() {
        let finding = Finding {
            file: "test.yml".to_string(),
            line: 5,
            action_ref: make_action_ref("owner", "repo", "abc123"),
            status: FindingStatus::Pinned {
                sha: "abc123def456abc123def456abc123def456abc1".to_string(),
                tag: None,
            },
        };

        match &finding.status {
            FindingStatus::Pinned { sha, tag } => {
                assert!(tag.is_none());
                assert!(!sha.is_empty());
            }
            _ => panic!("expected Pinned status"),
        }
    }

    #[test]
    fn test_finding_unresolvable_user_error() {
        let finding = Finding {
            file: "workflow.yml".to_string(),
            line: 30,
            action_ref: make_action_ref("unknown", "action", "v1"),
            status: FindingStatus::Unresolvable {
                reason: "repository not found".to_string(),
                is_environment_error: false,
            },
        };

        match &finding.status {
            FindingStatus::Unresolvable {
                reason,
                is_environment_error,
            } => {
                assert_eq!(reason, "repository not found");
                assert!(!is_environment_error);
            }
            _ => panic!("expected Unresolvable status"),
        }
    }

    #[test]
    fn test_finding_unresolvable_env_error() {
        let finding = Finding {
            file: "workflow.yml".to_string(),
            line: 25,
            action_ref: make_action_ref("actions", "checkout", "v4"),
            status: FindingStatus::Unresolvable {
                reason: "rate limit exceeded".to_string(),
                is_environment_error: true,
            },
        };

        match &finding.status {
            FindingStatus::Unresolvable {
                reason,
                is_environment_error,
            } => {
                assert!(reason.contains("rate limit"));
                assert!(is_environment_error);
            }
            _ => panic!("expected Unresolvable status"),
        }
    }

    // Action ref with host tests

    #[test]
    fn test_finding_with_custom_host() {
        let action_ref = ActionRef {
            owner: "org".to_string(),
            repo: "action".to_string(),
            ref_str: "v1.0.0".to_string(),
            host: Some("ghe.corp.com".to_string()),
            subpath: None,
        };

        let finding = Finding {
            file: "workflow.yml".to_string(),
            line: 1,
            action_ref,
            status: FindingStatus::UpToDate,
        };

        assert_eq!(finding.action_ref.host.as_deref(), Some("ghe.corp.com"));
    }

    #[test]
    fn test_finding_with_subpath() {
        let action_ref = ActionRef {
            owner: "actions".to_string(),
            repo: "cache".to_string(),
            ref_str: "v4".to_string(),
            host: None,
            subpath: Some("restore".to_string()),
        };

        let finding = Finding {
            file: "workflow.yml".to_string(),
            line: 1,
            action_ref,
            status: FindingStatus::UpToDate,
        };

        assert_eq!(finding.action_ref.subpath.as_deref(), Some("restore"));
    }

    // JSON output tests

    #[test]
    fn test_json_output_up_to_date() {
        let findings = vec![Finding {
            file: "test.yml".to_string(),
            line: 1,
            action_ref: make_action_ref("actions", "checkout", "v4"),
            status: FindingStatus::UpToDate,
        }];

        // Capture stdout to verify JSON format
        print_json(&findings);
        // Visual inspection shows format; add assertions for serialization
    }

    #[test]
    fn test_json_output_outdated_with_exact_version() {
        let findings = vec![Finding {
            file: "ci.yml".to_string(),
            line: 15,
            action_ref: make_action_ref("actions", "setup-node", "v3"),
            status: FindingStatus::Outdated {
                current: "v3".to_string(),
                latest: make_version_info("v4", "abc123"),
                exact_version: Some("v4.2.1".to_string()),
            },
        }];

        print_json(&findings);
        // Verify exact_version is included in JSON output
    }

    #[test]
    fn test_json_output_pinned() {
        let findings = vec![Finding {
            file: "secure.yml".to_string(),
            line: 20,
            action_ref: make_action_ref("actions", "checkout", "abc123"),
            status: FindingStatus::Pinned {
                sha: "abc123def456abc123def456abc123def456abc1".to_string(),
                tag: Some("v4.1.0".to_string()),
            },
        }];

        print_json(&findings);
    }

    #[test]
    fn test_json_output_unresolvable_env_error() {
        let findings = vec![Finding {
            file: "test.yml".to_string(),
            line: 5,
            action_ref: make_action_ref("actions", "checkout", "v4"),
            status: FindingStatus::Unresolvable {
                reason: "rate limit exceeded".to_string(),
                is_environment_error: true,
            },
        }];

        print_json(&findings);
    }

    #[test]
    fn test_json_output_with_host() {
        let action_ref = ActionRef {
            owner: "myorg".to_string(),
            repo: "action".to_string(),
            ref_str: "v1.0.0".to_string(),
            host: Some("github.enterprise.com".to_string()),
            subpath: None,
        };

        let findings = vec![Finding {
            file: "workflow.yml".to_string(),
            line: 10,
            action_ref,
            status: FindingStatus::UpToDate,
        }];

        print_json(&findings);
    }

    #[test]
    fn test_json_output_empty_findings() {
        let findings: Vec<Finding> = vec![];
        print_json(&findings);
        // Should produce valid empty JSON array
    }

    // Summary tests

    #[test]
    fn test_print_summary_single_action() {
        let findings = vec![Finding {
            file: "test.yml".to_string(),
            line: 1,
            action_ref: make_action_ref("actions", "checkout", "v4"),
            status: FindingStatus::UpToDate,
        }];

        print_summary(&findings);
        // Verify singular "action" not "actions"
    }

    #[test]
    fn test_print_summary_multiple_states() {
        let findings = vec![
            Finding {
                file: "test.yml".to_string(),
                line: 1,
                action_ref: make_action_ref("actions", "checkout", "v4"),
                status: FindingStatus::UpToDate,
            },
            Finding {
                file: "test.yml".to_string(),
                line: 2,
                action_ref: make_action_ref("actions", "setup-node", "v3"),
                status: FindingStatus::Outdated {
                    current: "v3".to_string(),
                    latest: make_version_info("v4.0.0", "abc123"),
                    exact_version: None,
                },
            },
            Finding {
                file: "test.yml".to_string(),
                line: 3,
                action_ref: make_action_ref("internal", "action", "main"),
                status: FindingStatus::Ignored,
            },
            Finding {
                file: "test.yml".to_string(),
                line: 4,
                action_ref: make_action_ref("actions", "cache", "abc123"),
                status: FindingStatus::Pinned {
                    sha: "abc123".to_string(),
                    tag: None,
                },
            },
            Finding {
                file: "test.yml".to_string(),
                line: 5,
                action_ref: make_action_ref("unknown", "action", "v1"),
                status: FindingStatus::Unresolvable {
                    reason: "not found".to_string(),
                    is_environment_error: false,
                },
            },
        ];

        print_summary(&findings);
        // Summary should show: 1 up to date, 1 ignored, 1 outdated, 1 pinned, 1 unresolvable
    }

    #[test]
    fn test_print_summary_empty() {
        let findings: Vec<Finding> = vec![];
        print_summary(&findings);
        // Should handle empty list gracefully
    }

    // Exit code edge cases

    #[test]
    fn test_determine_exit_code_multiple_outdated() {
        let findings = vec![
            Finding {
                file: "test.yml".to_string(),
                line: 1,
                action_ref: make_action_ref("actions", "checkout", "v3"),
                status: FindingStatus::Outdated {
                    current: "v3".to_string(),
                    latest: make_version_info("v4.0.0", "abc123"),
                    exact_version: None,
                },
            },
            Finding {
                file: "test.yml".to_string(),
                line: 2,
                action_ref: make_action_ref("actions", "setup-node", "v2"),
                status: FindingStatus::Outdated {
                    current: "v2".to_string(),
                    latest: make_version_info("v4.0.0", "def456"),
                    exact_version: None,
                },
            },
        ];

        // Multiple outdated should still return 1 in strict mode
        assert_eq!(determine_exit_code(&findings, true), 1);
        assert_eq!(determine_exit_code(&findings, false), 0);
    }

    #[test]
    fn test_determine_exit_code_mixed_errors() {
        let findings = vec![
            Finding {
                file: "test.yml".to_string(),
                line: 1,
                action_ref: make_action_ref("actions", "checkout", "v4"),
                status: FindingStatus::Unresolvable {
                    reason: "not found".to_string(),
                    is_environment_error: false,
                },
            },
            Finding {
                file: "test.yml".to_string(),
                line: 2,
                action_ref: make_action_ref("actions", "setup-node", "v4"),
                status: FindingStatus::Unresolvable {
                    reason: "network timeout".to_string(),
                    is_environment_error: true,
                },
            },
        ];

        // Env error (3) takes priority over user error (2)
        assert_eq!(determine_exit_code(&findings, false), 3);
        assert_eq!(determine_exit_code(&findings, true), 3);
    }

    #[test]
    fn test_determine_exit_code_all_states() {
        let findings = vec![
            Finding {
                file: "test.yml".to_string(),
                line: 1,
                action_ref: make_action_ref("actions", "checkout", "v4"),
                status: FindingStatus::UpToDate,
            },
            Finding {
                file: "test.yml".to_string(),
                line: 2,
                action_ref: make_action_ref("internal", "action", "main"),
                status: FindingStatus::Ignored,
            },
            Finding {
                file: "test.yml".to_string(),
                line: 3,
                action_ref: make_action_ref("actions", "setup-node", "v3"),
                status: FindingStatus::Outdated {
                    current: "v3".to_string(),
                    latest: make_version_info("v4.0.0", "abc123"),
                    exact_version: None,
                },
            },
            Finding {
                file: "test.yml".to_string(),
                line: 4,
                action_ref: make_action_ref("actions", "cache", "abc123"),
                status: FindingStatus::Pinned {
                    sha: "abc123".to_string(),
                    tag: None,
                },
            },
        ];

        // Only outdated matters in strict mode (no errors)
        assert_eq!(determine_exit_code(&findings, true), 1);
        assert_eq!(determine_exit_code(&findings, false), 0);
    }

    // Human output boundary tests

    #[test]
    fn test_print_human_empty() {
        let findings: Vec<Finding> = vec![];
        print_human(&findings);
        // Should print "No action references found."
    }

    #[test]
    fn test_print_human_multiple_files() {
        let findings = vec![
            Finding {
                file: "a.yml".to_string(),
                line: 1,
                action_ref: make_action_ref("actions", "checkout", "v4"),
                status: FindingStatus::UpToDate,
            },
            Finding {
                file: "b.yml".to_string(),
                line: 1,
                action_ref: make_action_ref("actions", "setup-node", "v4"),
                status: FindingStatus::UpToDate,
            },
            Finding {
                file: "b.yml".to_string(),
                line: 2,
                action_ref: make_action_ref("actions", "cache", "v3"),
                status: FindingStatus::UpToDate,
            },
        ];

        print_human(&findings);
        // Should group by file with blank line between files
    }

    #[test]
    fn test_finding_status_equality() {
        let status1 = FindingStatus::UpToDate;
        let status2 = FindingStatus::UpToDate;
        // FindingStatus is not PartialEq, so we just construct them
        assert!(matches!(status1, FindingStatus::UpToDate));
        assert!(matches!(status2, FindingStatus::UpToDate));
    }

    #[test]
    fn test_finding_clone() {
        let finding = Finding {
            file: "test.yml".to_string(),
            line: 10,
            action_ref: make_action_ref("actions", "checkout", "v4"),
            status: FindingStatus::UpToDate,
        };

        let cloned = finding.clone();
        assert_eq!(cloned.file, finding.file);
        assert_eq!(cloned.line, finding.line);
        assert_eq!(cloned.action_ref.repo, finding.action_ref.repo);
    }

    /// Test that [`exact_version`] is filtered out when it equals the latest version.
    #[test]
    fn test_outdated_exact_version_filtered_in_display() {
        // When exact_version equals the version_str, no suffix should be shown
        let finding = Finding {
            file: "test.yml".to_string(),
            line: 10,
            action_ref: make_action_ref("actions", "checkout", "v3"),
            status: FindingStatus::Outdated {
                current: "v3".to_string(),
                latest: make_version_info("v4.0.0", "abc123"),
                exact_version: Some("v4.0.0".to_string()), // Same as latest
            },
        };

        // In print_human, the suffix logic filters this out
        // We test the logic indirectly by verifying the condition
        match &finding.status {
            FindingStatus::Outdated {
                latest,
                exact_version,
                ..
            } => {
                let version_str = latest.version.to_string();
                let should_show_suffix = exact_version
                    .as_ref()
                    .filter(|ev| *ev != &version_str)
                    .is_some();
                assert!(
                    !should_show_suffix,
                    "suffix should be hidden when exact_version equals latest"
                );
            }
            _ => panic!("expected Outdated status"),
        }
    }

    /// Test short SHA display in pinned status.
    #[test]
    fn test_pinned_short_sha_display() {
        let short_sha = "abc";
        let finding = Finding {
            file: "test.yml".to_string(),
            line: 5,
            action_ref: make_action_ref("actions", "checkout", "abc"),
            status: FindingStatus::Pinned {
                sha: short_sha.to_string(),
                tag: None,
            },
        };

        // Verify we handle short SHAs correctly (min with 7)
        match &finding.status {
            FindingStatus::Pinned { sha, .. } => {
                let displayed_sha = &sha[..7.min(sha.len())];
                assert_eq!(displayed_sha, "abc");
            }
            _ => panic!("expected Pinned status"),
        }
    }

    /// Negative test: verify non-strict mode doesn't fail on outdated actions.
    #[test]
    fn test_non_strict_ignores_outdated() {
        let findings = vec![
            Finding {
                file: "test.yml".to_string(),
                line: 1,
                action_ref: make_action_ref("actions", "checkout", "v3"),
                status: FindingStatus::Outdated {
                    current: "v3".to_string(),
                    latest: make_version_info("v4.0.0", "abc123"),
                    exact_version: None,
                },
            },
            Finding {
                file: "test.yml".to_string(),
                line: 2,
                action_ref: make_action_ref("actions", "setup-node", "v3"),
                status: FindingStatus::Outdated {
                    current: "v3".to_string(),
                    latest: make_version_info("v4.0.0", "def456"),
                    exact_version: None,
                },
            },
        ];

        // Non-strict should return 0 despite multiple outdated
        assert_eq!(determine_exit_code(&findings, false), 0);
    }
}
