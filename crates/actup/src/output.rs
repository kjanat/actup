//! Output formatting for CLI

use actup_core::{ActionRef, VersionInfo};
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::cli::OutputFormat;

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
    UpToDate,
    Outdated {
        current: String,
        latest: VersionInfo,
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
            FindingStatus::Outdated { current, latest } => {
                println!(
                    "{}{}  {} -> {}",
                    line_prefix.dimmed(),
                    action_str.yellow(),
                    current.red(),
                    latest.version.to_string().green()
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
    #[derive(Serialize)]
    struct JsonFinding {
        file: String,
        line: usize,
        action: String,
        owner: String,
        repo: String,
        /// Host (e.g., "github.com", "ghe.corp.internal") - required by spec
        #[serde(skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        /// Provider kind (e.g., "github", "gitlab") - required by spec
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
        reason: Option<String>,
    }

    let json_findings: Vec<JsonFinding> = findings
        .iter()
        .map(|f| {
            let (status, current_version, latest_version, latest_sha, reason) = match &f.status {
                FindingStatus::UpToDate => ("up_to_date".into(), None, None, None, None),
                FindingStatus::Outdated { current, latest } => (
                    "outdated".into(),
                    Some(current.clone()),
                    Some(latest.version.to_string()),
                    Some(latest.sha.clone()),
                    None,
                ),
                FindingStatus::Pinned { sha, tag: _ } => {
                    ("pinned".into(), None, None, Some(sha.clone()), None)
                }
                FindingStatus::Unresolvable {
                    reason,
                    is_environment_error,
                } => (
                    if *is_environment_error {
                        "error".into()
                    } else {
                        "unresolvable".into()
                    },
                    None,
                    None,
                    None,
                    Some(reason.clone()),
                ),
            };

            JsonFinding {
                file: f.file.clone(),
                line: f.line,
                action: f.action_ref.to_string(),
                owner: f.action_ref.owner.clone(),
                repo: f.action_ref.repo.clone(),
                host: f.action_ref.host.clone(),
                provider: "github".to_string(), // TODO: get from registry when multi-provider
                reference: f.action_ref.ref_str.clone(),
                status,
                current_version,
                latest_version,
                latest_sha,
                reason,
            }
        })
        .collect();

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
            "Scanned {} action{}: {} up to date, {} outdated, {} pinned, {} unresolvable",
            total,
            if total == 1 { "" } else { "s" },
            up_to_date,
            outdated,
            pinned,
            unresolvable
        )
        .bold()
    );
}

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
