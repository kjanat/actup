//! Output formatting for CLI

use std::sync::Arc;

use actup_core::{ActionRef, version};
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
        ) = match f.status() {
            FindingStatus::UpToDate => ("up_to_date", None, None, None, None, None, None),
            FindingStatus::Ignored => ("ignored", None, None, None, None, None, None),
            FindingStatus::Outdated {
                current,
                latest,
                exact_version,
            } => (
                "outdated",
                Some(current.clone()),
                Some(latest.version().to_string()),
                Some(latest.sha().to_string()),
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
            FindingStatus::Floating { ref_name } => (
                "floating",
                Some(ref_name.clone()),
                None,
                None,
                None,
                None,
                None,
            ),
            FindingStatus::PinBranch { ref_name, sha } => (
                "pin_branch",
                Some(ref_name.clone()),
                None,
                Some(sha.clone()),
                None,
                None,
                None,
            ),
            FindingStatus::Unknown { reason } => (
                "unknown",
                None,
                None,
                None,
                None,
                None,
                Some(reason.clone()),
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
            file: f.file().to_string(),
            line: f.line(),
            action: f.action_ref().to_string(),
            owner: f.action_ref().owner().to_string(),
            repo: f.action_ref().repo().to_string(),
            host: f.action_ref().host().map(String::from),
            provider: f.provider().to_string(),
            reference: f.action_ref().ref_str().to_string(),
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
    file: String,
    line: usize,
    action_ref: ActionRef,
    status: FindingStatus,
    /// Provider name (e.g., "github", "gitlab")
    provider: String,
}

impl Finding {
    /// Create a new finding
    #[must_use]
    pub const fn new(
        file: String,
        line: usize,
        action_ref: ActionRef,
        status: FindingStatus,
        provider: String,
    ) -> Self {
        Self {
            file,
            line,
            action_ref,
            status,
            provider,
        }
    }

    /// Get the file path
    #[must_use]
    pub fn file(&self) -> &str {
        &self.file
    }

    /// Get the line number
    #[must_use]
    pub const fn line(&self) -> usize {
        self.line
    }

    /// Get the action reference
    #[must_use]
    pub const fn action_ref(&self) -> &ActionRef {
        &self.action_ref
    }

    /// Get the status
    #[must_use]
    pub const fn status(&self) -> &FindingStatus {
        &self.status
    }

    /// Get the provider name
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }
}

/// Status of a discovered action with respect to being up-to-date, ignored, or outdated.
#[derive(Debug, Clone)]
pub enum FindingStatus {
    /// Action is up to date
    UpToDate,
    /// Action matched an ignore pattern in config
    Ignored,
    /// Action is outdated
    Outdated {
        current: String,
        latest: Arc<version::Info>,
        /// Full version when latest is a major-only tag (e.g., v6 with exact v6.0.2)
        exact_version: Option<String>,
    },
    /// Action is pinned to a specific SHA.
    Pinned { sha: String, tag: Option<String> },
    /// Action references a floating branch under `warn` branch policy.
    Floating { ref_name: String },
    /// Action references a floating branch under `update` branch policy, to be
    /// pinned to the branch's current HEAD SHA.
    PinBranch { ref_name: String, sha: String },
    /// Action ref exists but version comparison unavailable.
    ///
    /// This occurs when the ref resolves successfully but version information
    /// couldn't be fetched (e.g., tags beyond `max_tags` limit). The ref is valid
    /// but we can't determine if it's outdated.
    Unknown {
        /// Why version comparison is unavailable
        reason: String,
    },
    /// Action reference could not be resolved.
    Unresolvable {
        reason: String,
        /// True if this was an environment error (network/auth/rate-limit)
        /// vs a user error (bad ref, not found)
        is_environment_error: bool,
    },
}

/// Prints the given findings in the requested output format.
pub fn print_findings(findings: &[Finding], format: OutputFormat) {
    match format {
        OutputFormat::Human => print_human(findings),
        OutputFormat::Json => print_json(findings),
        OutputFormat::Toml => print_toml(findings),
    }
}

fn print_human(findings: &[Finding]) {
    if findings.is_empty() {
        println!("{}", "No action references found.".dimmed());
        return;
    }

    let mut current_file = String::new();

    for finding in findings {
        if finding.file() != current_file {
            if !current_file.is_empty() {
                println!();
            }
            println!("{}", finding.file().bold());
            current_file = finding.file().to_string();
        }

        print_finding_line(finding);
    }
}

/// Print one finding's `  Lnn: <action>  <status>` line.
fn print_finding_line(finding: &Finding) {
    let line_prefix = format!("  L{}: ", finding.line());
    let action_str = finding.action_ref().to_string();

    match finding.status() {
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
            let version_str = latest.version().to_string();
            let suffix = exact_version
                .as_ref()
                .filter(|ev| ev.as_str() != version_str)
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
        FindingStatus::Floating { ref_name } => {
            println!(
                "{}{}  {}",
                line_prefix.dimmed(),
                action_str.yellow(),
                format!("floating branch '{ref_name}' — pin to a tag or SHA").yellow()
            );
        }
        FindingStatus::PinBranch { ref_name, sha } => {
            println!(
                "{}{}  {}",
                line_prefix.dimmed(),
                action_str.yellow(),
                format!(
                    "pin branch '{ref_name}' -> {} # {ref_name}",
                    &sha[..7.min(sha.len())]
                )
                .yellow()
            );
        }
        FindingStatus::Unknown { reason } => {
            println!(
                "{}{}  {}",
                line_prefix.dimmed(),
                action_str.yellow(),
                format!("unknown: {reason}").yellow()
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

fn print_json(findings: &[Finding]) {
    let json_findings: Vec<JsonFinding> = findings.iter().map(JsonFinding::from).collect();
    match serde_json::to_string_pretty(&json_findings) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("Error serializing findings: {e}"),
    }
}

fn print_toml(findings: &[Finding]) {
    // TOML requires a table as root, so wrap the findings vector
    #[derive(Serialize)]
    struct TomlFindings {
        findings: Vec<JsonFinding>,
    }

    let toml_findings: Vec<JsonFinding> = findings.iter().map(JsonFinding::from).collect();
    let wrapper = TomlFindings {
        findings: toml_findings,
    };
    match toml::to_string_pretty(&wrapper) {
        Ok(toml) => println!("{toml}"),
        Err(e) => eprintln!("Error serializing findings: {e}"),
    }
}

/// Prints a summary of findings.
pub fn print_summary(findings: &[Finding]) {
    let total = findings.len();
    let up_to_date = findings
        .iter()
        .filter(|f| matches!(f.status(), FindingStatus::UpToDate))
        .count();
    let ignored = findings
        .iter()
        .filter(|f| matches!(f.status(), FindingStatus::Ignored))
        .count();
    let outdated = findings
        .iter()
        .filter(|f| matches!(f.status(), FindingStatus::Outdated { .. }))
        .count();
    let pinned = findings
        .iter()
        .filter(|f| matches!(f.status(), FindingStatus::Pinned { .. }))
        .count();
    let floating = findings
        .iter()
        .filter(|f| matches!(f.status(), FindingStatus::Floating { .. }))
        .count();
    let to_pin = findings
        .iter()
        .filter(|f| matches!(f.status(), FindingStatus::PinBranch { .. }))
        .count();
    let unresolvable = findings
        .iter()
        .filter(|f| matches!(f.status(), FindingStatus::Unresolvable { .. }))
        .count();

    println!();
    println!(
        "{}",
        format!(
            "Scanned {total} action{}: {up_to_date} up to date, {ignored} ignored, {outdated} outdated, {pinned} pinned, {floating} floating, {to_pin} to pin, {unresolvable} unresolvable",
            if total == 1 { "" } else { "s" }
        )
        .bold()
    );
}

/// Determines CLI exit code based on findings.
///
/// Exit codes:
/// - 0: Success ([`FindingStatus::UpToDate`], Ignored, Pinned, Floating are all considered healthy)
/// - 1: Outdated or pinnable branch found (only with `strict` mode)
/// - 2: Unresolvable user error (bad ref, repo not found)
/// - 3: Environment error (network, rate limit, auth)
pub fn determine_exit_code(findings: &[Finding], strict: bool) -> i32 {
    let has_outdated = findings.iter().any(|f| {
        matches!(
            f.status(),
            FindingStatus::Outdated { .. } | FindingStatus::PinBranch { .. }
        )
    });
    let has_env_error = findings.iter().any(|f| {
        matches!(
            f.status(),
            FindingStatus::Unresolvable {
                is_environment_error: true,
                ..
            }
        )
    });
    let has_user_error = findings.iter().any(|f| {
        matches!(
            f.status(),
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

    fn make_action_ref(owner: &str, repo: &str, ref_str: &str) -> ActionRef {
        ActionRef::simple(owner.to_string(), repo.to_string(), ref_str.to_string())
    }

    fn make_version_info(version: &str, sha: &str) -> Arc<version::Info> {
        Arc::new(version::Info::new(version::Version::parse(version), sha))
    }

    fn make_finding(
        file: &str,
        line: usize,
        action_ref: ActionRef,
        status: FindingStatus,
    ) -> Finding {
        Finding::new(
            file.to_string(),
            line,
            action_ref,
            status,
            "github".to_string(),
        )
    }

    #[test]
    fn test_determine_exit_code_success() {
        let findings = vec![make_finding(
            "test.yml",
            1,
            make_action_ref("actions", "checkout", "v4"),
            FindingStatus::UpToDate,
        )];

        assert_eq!(determine_exit_code(&findings, false), 0);
        assert_eq!(determine_exit_code(&findings, true), 0);
    }

    #[test]
    fn test_determine_exit_code_outdated_non_strict() {
        let findings = vec![make_finding(
            "test.yml",
            1,
            make_action_ref("actions", "checkout", "v3"),
            FindingStatus::Outdated {
                current: "v3".to_string(),
                latest: make_version_info("v4.0.0", "abc123"),
                exact_version: None,
            },
        )];

        assert_eq!(determine_exit_code(&findings, false), 0);
    }

    #[test]
    fn test_determine_exit_code_outdated_strict() {
        let findings = vec![make_finding(
            "test.yml",
            1,
            make_action_ref("actions", "checkout", "v3"),
            FindingStatus::Outdated {
                current: "v3".to_string(),
                latest: make_version_info("v4.0.0", "abc123"),
                exact_version: None,
            },
        )];

        assert_eq!(determine_exit_code(&findings, true), 1);
    }

    #[test]
    fn test_determine_exit_code_user_error() {
        let findings = vec![make_finding(
            "test.yml",
            1,
            make_action_ref("actions", "nonexistent", "v1"),
            FindingStatus::Unresolvable {
                reason: "not found".to_string(),
                is_environment_error: false,
            },
        )];

        assert_eq!(determine_exit_code(&findings, false), 2);
        assert_eq!(determine_exit_code(&findings, true), 2);
    }

    #[test]
    fn test_determine_exit_code_env_error() {
        let findings = vec![make_finding(
            "test.yml",
            1,
            make_action_ref("actions", "checkout", "v4"),
            FindingStatus::Unresolvable {
                reason: "network timeout".to_string(),
                is_environment_error: true,
            },
        )];

        assert_eq!(determine_exit_code(&findings, false), 3);
        assert_eq!(determine_exit_code(&findings, true), 3);
    }

    #[test]
    fn test_determine_exit_code_priority() {
        // Env error takes priority over user error and outdated
        let findings = vec![
            make_finding(
                "test.yml",
                1,
                make_action_ref("actions", "checkout", "v3"),
                FindingStatus::Outdated {
                    current: "v3".to_string(),
                    latest: make_version_info("v4.0.0", "abc123"),
                    exact_version: None,
                },
            ),
            make_finding(
                "test.yml",
                2,
                make_action_ref("actions", "bad", "v1"),
                FindingStatus::Unresolvable {
                    reason: "not found".to_string(),
                    is_environment_error: false,
                },
            ),
            make_finding(
                "test.yml",
                3,
                make_action_ref("actions", "other", "v1"),
                FindingStatus::Unresolvable {
                    reason: "rate limited".to_string(),
                    is_environment_error: true,
                },
            ),
        ];

        // Env error (3) takes priority
        assert_eq!(determine_exit_code(&findings, true), 3);
    }

    #[test]
    fn test_determine_exit_code_user_over_outdated() {
        let findings = vec![
            make_finding(
                "test.yml",
                1,
                make_action_ref("actions", "checkout", "v3"),
                FindingStatus::Outdated {
                    current: "v3".to_string(),
                    latest: make_version_info("v4.0.0", "abc123"),
                    exact_version: None,
                },
            ),
            make_finding(
                "test.yml",
                2,
                make_action_ref("actions", "bad", "v1"),
                FindingStatus::Unresolvable {
                    reason: "not found".to_string(),
                    is_environment_error: false,
                },
            ),
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
        let findings = vec![make_finding(
            "test.yml",
            1,
            make_action_ref("actions", "checkout", "abc123def456"),
            FindingStatus::Pinned {
                sha: "abc123def456".to_string(),
                tag: Some("v4.0.0".to_string()),
            },
        )];

        // Pinned is not an error
        assert_eq!(determine_exit_code(&findings, false), 0);
        assert_eq!(determine_exit_code(&findings, true), 0);
    }

    // Finding construction tests

    #[test]
    fn test_finding_up_to_date() {
        let finding = make_finding(
            "workflow.yml",
            10,
            make_action_ref("actions", "checkout", "v4"),
            FindingStatus::UpToDate,
        );

        assert_eq!(finding.file, "workflow.yml");
        assert_eq!(finding.line, 10);
        assert!(matches!(finding.status, FindingStatus::UpToDate));
    }

    #[test]
    fn test_finding_ignored() {
        let finding = make_finding(
            "workflow.yml",
            5,
            make_action_ref("internal", "action", "main"),
            FindingStatus::Ignored,
        );

        assert_eq!(finding.file, "workflow.yml");
        assert_eq!(finding.line, 5);
        assert!(matches!(finding.status, FindingStatus::Ignored));
    }

    #[test]
    fn test_determine_exit_code_ignored() {
        // Ignored actions should not affect exit code (like UpToDate)
        let findings = vec![make_finding(
            "test.yml",
            1,
            make_action_ref("internal", "action", "main"),
            FindingStatus::Ignored,
        )];

        assert_eq!(determine_exit_code(&findings, false), 0);
        assert_eq!(determine_exit_code(&findings, true), 0);
    }

    #[test]
    fn test_finding_outdated() {
        let finding = make_finding(
            "ci.yml",
            15,
            make_action_ref("actions", "setup-node", "v3"),
            FindingStatus::Outdated {
                current: "v3".to_string(),
                latest: make_version_info("v4.0.0", "abc123"),
                exact_version: None,
            },
        );

        match finding.status() {
            FindingStatus::Outdated {
                current, latest, ..
            } => {
                assert_eq!(current, "v3");
                assert_eq!(latest.version().to_string(), "v4.0.0");
                assert_eq!(latest.sha(), "abc123");
            }
            _ => panic!("expected Outdated status"),
        }
    }

    #[test]
    fn test_finding_outdated_with_exact_version() {
        // Test major-only tag with exact version suffix (e.g., "v6 -> v7 (v7.2.1)")
        let finding = make_finding(
            "ci.yml",
            15,
            make_action_ref("actions", "checkout", "v4"),
            FindingStatus::Outdated {
                current: "v4".to_string(),
                latest: make_version_info("v5", "def456"), // major-only tag
                exact_version: Some("v5.2.1".to_string()), // actual semver
            },
        );

        match finding.status() {
            FindingStatus::Outdated {
                current,
                latest,
                exact_version,
            } => {
                assert_eq!(current, "v4");
                assert_eq!(latest.version().to_string(), "v5");
                assert_eq!(exact_version, &Some("v5.2.1".to_string()));
            }
            _ => panic!("expected Outdated status"),
        }
    }

    #[test]
    fn test_finding_pinned_with_tag() {
        let finding = make_finding(
            "deploy.yml",
            20,
            make_action_ref(
                "actions",
                "checkout",
                "b4ffde65f46336ab88eb53be808477a3936bae11",
            ),
            FindingStatus::Pinned {
                sha: "b4ffde65f46336ab88eb53be808477a3936bae11".to_string(),
                tag: Some("v4.1.1".to_string()),
            },
        );

        match finding.status() {
            FindingStatus::Pinned { sha, tag } => {
                assert_eq!(sha, "b4ffde65f46336ab88eb53be808477a3936bae11");
                assert_eq!(tag.as_deref(), Some("v4.1.1"));
            }
            _ => panic!("expected Pinned status"),
        }
    }

    #[test]
    fn test_finding_pinned_without_tag() {
        let finding = make_finding(
            "test.yml",
            5,
            make_action_ref("owner", "repo", "abc123"),
            FindingStatus::Pinned {
                sha: "abc123def456abc123def456abc123def456abc1".to_string(),
                tag: None,
            },
        );

        match finding.status() {
            FindingStatus::Pinned { sha, tag } => {
                assert!(tag.is_none());
                assert_ne!(sha, "");
            }
            _ => panic!("expected Pinned status"),
        }
    }

    #[test]
    fn test_finding_unresolvable_user_error() {
        let finding = make_finding(
            "workflow.yml",
            30,
            make_action_ref("unknown", "action", "v1"),
            FindingStatus::Unresolvable {
                reason: "repository not found".to_string(),
                is_environment_error: false,
            },
        );

        match finding.status() {
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
        let finding = make_finding(
            "workflow.yml",
            25,
            make_action_ref("actions", "checkout", "v4"),
            FindingStatus::Unresolvable {
                reason: "rate limit exceeded".to_string(),
                is_environment_error: true,
            },
        );

        match finding.status() {
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
        let action_ref = ActionRef::new(
            Some("ghe.corp.com".to_string()),
            "org",
            "action",
            None,
            "v1.0.0",
        );

        let finding = make_finding("workflow.yml", 1, action_ref, FindingStatus::UpToDate);

        assert_eq!(finding.action_ref.host(), Some("ghe.corp.com"));
    }

    #[test]
    fn test_finding_with_subpath() {
        let action_ref =
            ActionRef::new(None, "actions", "cache", Some("restore".to_string()), "v4");

        let finding = make_finding("workflow.yml", 1, action_ref, FindingStatus::UpToDate);

        assert_eq!(finding.action_ref.subpath(), Some("restore"));
    }

    // JSON output tests

    #[test]
    fn test_json_output_up_to_date() {
        let finding = make_finding(
            "test.yml",
            1,
            make_action_ref("actions", "checkout", "v4"),
            FindingStatus::UpToDate,
        );

        let json_finding = JsonFinding::from(&finding);
        let json = serde_json::to_string_pretty(&[&json_finding]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert!(parsed.is_array());
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["status"], "up_to_date");
        assert_eq!(arr[0]["file"], "test.yml");
        assert_eq!(arr[0]["owner"], "actions");
        assert_eq!(arr[0]["repo"], "checkout");
    }

    #[test]
    fn test_json_output_outdated_with_exact_version() {
        let finding = make_finding(
            "ci.yml",
            15,
            make_action_ref("actions", "setup-node", "v3"),
            FindingStatus::Outdated {
                current: "v3".to_string(),
                latest: make_version_info("v4", "abc123"),
                exact_version: Some("v4.2.1".to_string()),
            },
        );

        let json_finding = JsonFinding::from(&finding);
        let json = serde_json::to_string_pretty(&json_finding).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["status"], "outdated");
        assert_eq!(parsed["current_version"], "v3");
        assert_eq!(parsed["latest_version"], "v4");
        assert_eq!(parsed["exact_version"], "v4.2.1");
    }

    #[test]
    fn test_json_output_pinned() {
        let finding = make_finding(
            "secure.yml",
            20,
            make_action_ref("actions", "checkout", "abc123"),
            FindingStatus::Pinned {
                sha: "abc123def456abc123def456abc123def456abc1".to_string(),
                tag: Some("v4.1.0".to_string()),
            },
        );

        let json_finding = JsonFinding::from(&finding);
        let json = serde_json::to_string_pretty(&json_finding).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["status"], "pinned");
        assert_eq!(
            parsed["latest_sha"],
            "abc123def456abc123def456abc123def456abc1"
        );
        assert_eq!(parsed["pinned_tag"], "v4.1.0");
    }

    #[test]
    fn test_json_output_unresolvable_env_error() {
        let finding = make_finding(
            "test.yml",
            5,
            make_action_ref("actions", "checkout", "v4"),
            FindingStatus::Unresolvable {
                reason: "rate limit exceeded".to_string(),
                is_environment_error: true,
            },
        );

        let json_finding = JsonFinding::from(&finding);
        let json = serde_json::to_string_pretty(&json_finding).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["status"], "error"); // is_environment_error = true -> "error"
        assert_eq!(parsed["reason"], "rate limit exceeded");
    }

    #[test]
    fn test_json_output_with_host() {
        let action_ref = ActionRef::new(
            Some("github.enterprise.com".to_string()),
            "myorg",
            "action",
            None,
            "v1.0.0",
        );

        let finding = make_finding("workflow.yml", 10, action_ref, FindingStatus::UpToDate);

        let json_finding = JsonFinding::from(&finding);
        let json = serde_json::to_string_pretty(&json_finding).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["host"], "github.enterprise.com");
        assert_eq!(parsed["owner"], "myorg");
        assert_eq!(parsed["repo"], "action");
    }

    #[test]
    fn test_json_output_empty_findings() {
        let findings: [Finding; 0] = [];

        let json_findings: Vec<JsonFinding> = findings.iter().map(JsonFinding::from).collect();
        let json = serde_json::to_string_pretty(&json_findings).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert!(parsed.is_array());
        assert_eq!(parsed, serde_json::json!([]));
    }

    // Summary tests

    #[test]
    fn test_print_summary_single_action() {
        let findings = vec![make_finding(
            "test.yml",
            1,
            make_action_ref("actions", "checkout", "v4"),
            FindingStatus::UpToDate,
        )];

        print_summary(&findings);
        // Verify singular "action" not "actions"
    }

    #[test]
    fn test_print_summary_multiple_states() {
        let findings = vec![
            make_finding(
                "test.yml",
                1,
                make_action_ref("actions", "checkout", "v4"),
                FindingStatus::UpToDate,
            ),
            make_finding(
                "test.yml",
                2,
                make_action_ref("actions", "setup-node", "v3"),
                FindingStatus::Outdated {
                    current: "v3".to_string(),
                    latest: make_version_info("v4.0.0", "abc123"),
                    exact_version: None,
                },
            ),
            make_finding(
                "test.yml",
                3,
                make_action_ref("internal", "action", "main"),
                FindingStatus::Ignored,
            ),
            make_finding(
                "test.yml",
                4,
                make_action_ref("actions", "cache", "abc123"),
                FindingStatus::Pinned {
                    sha: "abc123".to_string(),
                    tag: None,
                },
            ),
            make_finding(
                "test.yml",
                5,
                make_action_ref("unknown", "action", "v1"),
                FindingStatus::Unresolvable {
                    reason: "not found".to_string(),
                    is_environment_error: false,
                },
            ),
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
            make_finding(
                "test.yml",
                1,
                make_action_ref("actions", "checkout", "v3"),
                FindingStatus::Outdated {
                    current: "v3".to_string(),
                    latest: make_version_info("v4.0.0", "abc123"),
                    exact_version: None,
                },
            ),
            make_finding(
                "test.yml",
                2,
                make_action_ref("actions", "setup-node", "v2"),
                FindingStatus::Outdated {
                    current: "v2".to_string(),
                    latest: make_version_info("v4.0.0", "def456"),
                    exact_version: None,
                },
            ),
        ];

        // Multiple outdated should still return 1 in strict mode
        assert_eq!(determine_exit_code(&findings, true), 1);
        assert_eq!(determine_exit_code(&findings, false), 0);
    }

    #[test]
    fn test_determine_exit_code_mixed_errors() {
        let findings = vec![
            make_finding(
                "test.yml",
                1,
                make_action_ref("actions", "checkout", "v4"),
                FindingStatus::Unresolvable {
                    reason: "not found".to_string(),
                    is_environment_error: false,
                },
            ),
            make_finding(
                "test.yml",
                2,
                make_action_ref("actions", "setup-node", "v4"),
                FindingStatus::Unresolvable {
                    reason: "network timeout".to_string(),
                    is_environment_error: true,
                },
            ),
        ];

        // Env error (3) takes priority over user error (2)
        assert_eq!(determine_exit_code(&findings, false), 3);
        assert_eq!(determine_exit_code(&findings, true), 3);
    }

    #[test]
    fn test_determine_exit_code_all_states() {
        let findings = vec![
            make_finding(
                "test.yml",
                1,
                make_action_ref("actions", "checkout", "v4"),
                FindingStatus::UpToDate,
            ),
            make_finding(
                "test.yml",
                2,
                make_action_ref("internal", "action", "main"),
                FindingStatus::Ignored,
            ),
            make_finding(
                "test.yml",
                3,
                make_action_ref("actions", "setup-node", "v3"),
                FindingStatus::Outdated {
                    current: "v3".to_string(),
                    latest: make_version_info("v4.0.0", "abc123"),
                    exact_version: None,
                },
            ),
            make_finding(
                "test.yml",
                4,
                make_action_ref("actions", "cache", "abc123"),
                FindingStatus::Pinned {
                    sha: "abc123".to_string(),
                    tag: None,
                },
            ),
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
            make_finding(
                "a.yml",
                1,
                make_action_ref("actions", "checkout", "v4"),
                FindingStatus::UpToDate,
            ),
            make_finding(
                "b.yml",
                1,
                make_action_ref("actions", "setup-node", "v4"),
                FindingStatus::UpToDate,
            ),
            make_finding(
                "b.yml",
                2,
                make_action_ref("actions", "cache", "v3"),
                FindingStatus::UpToDate,
            ),
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
        let finding = make_finding(
            "test.yml",
            10,
            make_action_ref("actions", "checkout", "v4"),
            FindingStatus::UpToDate,
        );

        let cloned = finding.clone();
        assert_eq!(cloned.file, finding.file);
        assert_eq!(cloned.line, finding.line);
        assert_eq!(cloned.action_ref.repo(), finding.action_ref.repo());
    }

    /// Test that [`exact_version`] is filtered out when it equals the latest version.
    #[test]
    fn test_outdated_exact_version_filtered_in_display() {
        // When exact_version equals the version_str, no suffix should be shown
        let finding = make_finding(
            "test.yml",
            10,
            make_action_ref("actions", "checkout", "v3"),
            FindingStatus::Outdated {
                current: "v3".to_string(),
                latest: make_version_info("v4.0.0", "abc123"),
                exact_version: Some("v4.0.0".to_string()), // Same as latest
            },
        );

        // In print_human, the suffix logic filters this out
        // We test the logic indirectly by verifying the condition
        match finding.status() {
            FindingStatus::Outdated {
                latest,
                exact_version,
                ..
            } => {
                let version_str = latest.version().to_string();
                let should_show_suffix =
                    exact_version.as_ref().is_some_and(|ev| ev != &version_str);
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
        let finding = make_finding(
            "test.yml",
            5,
            make_action_ref("actions", "checkout", "abc"),
            FindingStatus::Pinned {
                sha: short_sha.to_string(),
                tag: None,
            },
        );

        // Verify we handle short SHAs correctly (min with 7)
        match finding.status() {
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
            make_finding(
                "test.yml",
                1,
                make_action_ref("actions", "checkout", "v3"),
                FindingStatus::Outdated {
                    current: "v3".to_string(),
                    latest: make_version_info("v4.0.0", "abc123"),
                    exact_version: None,
                },
            ),
            make_finding(
                "test.yml",
                2,
                make_action_ref("actions", "setup-node", "v3"),
                FindingStatus::Outdated {
                    current: "v3".to_string(),
                    latest: make_version_info("v4.0.0", "def456"),
                    exact_version: None,
                },
            ),
        ];

        // Non-strict should return 0 despite multiple outdated
        assert_eq!(determine_exit_code(&findings, false), 0);
    }

    /// Test `From<&Finding>` trait implementation for `JsonFinding`.
    #[test]
    fn test_json_finding_from_finding_up_to_date() {
        let finding = Finding {
            file: "test.yml".to_string(),
            line: 10,
            action_ref: make_action_ref("actions", "checkout", "v4"),
            status: FindingStatus::UpToDate,
            provider: "github".to_string(),
        };

        let json_finding = JsonFinding::from(&finding);

        assert_eq!(json_finding.file, "test.yml");
        assert_eq!(json_finding.line, 10);
        assert_eq!(json_finding.owner, "actions");
        assert_eq!(json_finding.repo, "checkout");
        assert_eq!(json_finding.reference, "v4");
        assert_eq!(json_finding.status, "up_to_date");
        assert!(json_finding.current_version.is_none());
        assert!(json_finding.latest_version.is_none());
        assert!(json_finding.pinned_tag.is_none());
        assert!(json_finding.reason.is_none());
    }

    /// Test `From<&Finding>` for Outdated status.
    #[test]
    fn test_json_finding_from_finding_outdated() {
        let finding = Finding {
            file: "ci.yml".to_string(),
            line: 15,
            action_ref: make_action_ref("actions", "setup-node", "v3"),
            status: FindingStatus::Outdated {
                current: "v3".to_string(),
                latest: make_version_info("v4.0.0", "abc123"),
                exact_version: Some("v4.0.0".to_string()),
            },
            provider: "github".to_string(),
        };

        let json_finding = JsonFinding::from(&finding);

        assert_eq!(json_finding.status, "outdated");
        assert_eq!(json_finding.current_version, Some("v3".to_string()));
        assert_eq!(json_finding.latest_version, Some("v4.0.0".to_string()));
        assert_eq!(json_finding.latest_sha, Some("abc123".to_string()));
        assert_eq!(json_finding.exact_version, Some("v4.0.0".to_string()));
        assert!(json_finding.pinned_tag.is_none()); // pinned_tag only for Pinned status
    }

    /// Test `From<&Finding>` for Pinned status with tag - verifies `pinned_tag` is set.
    #[test]
    fn test_json_finding_from_finding_pinned_with_tag() {
        let finding = Finding {
            file: "secure.yml".to_string(),
            line: 20,
            action_ref: make_action_ref("actions", "checkout", "abc123def456"),
            status: FindingStatus::Pinned {
                sha: "abc123def456abc123def456abc123def456abc1".to_string(),
                tag: Some("v4.1.0".to_string()),
            },
            provider: "github".to_string(),
        };

        let json_finding = JsonFinding::from(&finding);

        assert_eq!(json_finding.status, "pinned");
        assert_eq!(
            json_finding.latest_sha,
            Some("abc123def456abc123def456abc123def456abc1".to_string())
        );
        assert_eq!(json_finding.pinned_tag, Some("v4.1.0".to_string()));
        assert!(json_finding.current_version.is_none());
        assert!(json_finding.latest_version.is_none());
    }

    /// Test `From<&Finding>` for Pinned status without tag - verifies `pinned_tag` is None.
    #[test]
    fn test_json_finding_from_finding_pinned_without_tag() {
        let finding = Finding {
            file: "workflow.yml".to_string(),
            line: 5,
            action_ref: make_action_ref("actions", "cache", "deadbeef"),
            status: FindingStatus::Pinned {
                sha: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
                tag: None,
            },
            provider: "github".to_string(),
        };

        let json_finding = JsonFinding::from(&finding);

        assert_eq!(json_finding.status, "pinned");
        assert!(json_finding.pinned_tag.is_none());
        assert_eq!(
            json_finding.latest_sha,
            Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string())
        );
    }

    /// Test `From<&Finding>` for Ignored status - verifies `pinned_tag` is omitted.
    #[test]
    fn test_json_finding_from_finding_ignored() {
        let finding = Finding {
            file: "test.yml".to_string(),
            line: 1,
            action_ref: make_action_ref("internal", "action", "main"),
            status: FindingStatus::Ignored,
            provider: "github".to_string(),
        };

        let json_finding = JsonFinding::from(&finding);

        assert_eq!(json_finding.status, "ignored");
        assert!(json_finding.pinned_tag.is_none());
        assert!(json_finding.current_version.is_none());
        assert!(json_finding.latest_version.is_none());
    }

    /// Test `From<&Finding>` for Unresolvable user error.
    #[test]
    fn test_json_finding_from_finding_unresolvable_user() {
        let finding = Finding {
            file: "test.yml".to_string(),
            line: 5,
            action_ref: make_action_ref("unknown", "action", "v1"),
            status: FindingStatus::Unresolvable {
                reason: "repository not found".to_string(),
                is_environment_error: false,
            },
            provider: "github".to_string(),
        };

        let json_finding = JsonFinding::from(&finding);

        assert_eq!(json_finding.status, "unresolvable");
        assert_eq!(
            json_finding.reason,
            Some("repository not found".to_string())
        );
        assert!(json_finding.pinned_tag.is_none());
    }

    /// Test `From<&Finding>` for Unresolvable environment error.
    #[test]
    fn test_json_finding_from_finding_unresolvable_env() {
        let finding = Finding {
            file: "test.yml".to_string(),
            line: 5,
            action_ref: make_action_ref("actions", "checkout", "v4"),
            status: FindingStatus::Unresolvable {
                reason: "rate limit exceeded".to_string(),
                is_environment_error: true,
            },
            provider: "github".to_string(),
        };

        let json_finding = JsonFinding::from(&finding);

        assert_eq!(json_finding.status, "error"); // env errors use "error" status
        assert_eq!(json_finding.reason, Some("rate limit exceeded".to_string()));
        assert!(json_finding.pinned_tag.is_none());
    }

    /// Test that `pinned_tag` is correctly serialized in JSON output.
    #[test]
    fn test_json_serialization_pinned_tag_present() {
        let finding = Finding {
            file: "test.yml".to_string(),
            line: 1,
            action_ref: make_action_ref("actions", "checkout", "abc123"),
            status: FindingStatus::Pinned {
                sha: "abc123abc123abc123abc123abc123abc123abc1".to_string(),
                tag: Some("v4.2.0".to_string()),
            },
            provider: "github".to_string(),
        };

        let json_finding = JsonFinding::from(&finding);
        let serialized = serde_json::to_string(&json_finding).unwrap();

        assert!(
            serialized.contains("\"pinned_tag\":\"v4.2.0\""),
            "JSON should contain pinned_tag field: {serialized}"
        );
    }

    /// Test that `pinned_tag` is omitted from JSON when None (`skip_serializing_if`).
    #[test]
    fn test_json_serialization_pinned_tag_omitted() {
        let finding = Finding {
            file: "test.yml".to_string(),
            line: 1,
            action_ref: make_action_ref("actions", "checkout", "v4"),
            status: FindingStatus::UpToDate,
            provider: "github".to_string(),
        };

        let json_finding = JsonFinding::from(&finding);
        let serialized = serde_json::to_string(&json_finding).unwrap();

        assert!(
            !serialized.contains("pinned_tag"),
            "JSON should not contain pinned_tag field for non-pinned: {serialized}"
        );
    }
}
