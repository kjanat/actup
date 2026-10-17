//! Output formatting for CLI

use anstream::{eprintln, println};
use std::sync::Arc;

use actup_core::{ActionRef, RefKind, sanitize_line, version};
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
    host: String,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    ref_kind: Option<RefKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    same_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggestion: Option<String>,
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
            host: f.host().to_string(),
            provider: f.provider().to_string(),
            reference: f.action_ref().ref_str().to_string(),
            status: status.into(),
            current_version,
            latest_version,
            latest_sha,
            exact_version,
            pinned_tag,
            reason,
            ref_kind: f.ref_kind(),
            same_commit: f.same_commit().map(|t| t.version().to_string()),
            suggestion: f.suggestion().map(|t| t.version().to_string()),
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
    /// Host of the provider serving the reference
    host: String,
    ref_kind: Option<RefKind>,
    same_commit: Option<Arc<version::Info>>,
    suggestion: Option<Arc<version::Info>>,
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
        host: String,
    ) -> Self {
        Self {
            file,
            line,
            action_ref,
            status,
            provider,
            host,
            ref_kind: None,
            same_commit: None,
            suggestion: None,
        }
    }

    /// Record the kind of ref and the suggested target on its current commit.
    #[must_use]
    pub fn with_ref(
        mut self,
        ref_kind: Option<RefKind>,
        same_commit: Option<Arc<version::Info>>,
    ) -> Self {
        self.ref_kind = ref_kind;
        self.same_commit = same_commit;
        self
    }

    /// Record the listed tag closest to an unresolvable ref.
    #[must_use]
    pub fn with_suggestion(mut self, suggestion: Option<Arc<version::Info>>) -> Self {
        self.suggestion = suggestion;
        self
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

    /// Get the host of the provider serving the reference
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Get the kind of ref, when the version set lists it
    #[must_use]
    pub const fn ref_kind(&self) -> Option<RefKind> {
        self.ref_kind
    }

    /// Get the suggested target, when it sits on the commit the ref already names
    #[must_use]
    pub const fn same_commit(&self) -> Option<&Arc<version::Info>> {
        self.same_commit.as_ref()
    }

    /// Get the listed tag closest to an unresolvable ref
    #[must_use]
    pub const fn suggestion(&self) -> Option<&Arc<version::Info>> {
        self.suggestion.as_ref()
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
            println!("{}", sanitize_line(finding.file()).bold());
            current_file = finding.file().to_string();
        }

        print_finding_line(finding);
    }
}

/// Print one finding's `  Lnn: <action>  <status>` line.
fn print_finding_line(finding: &Finding) {
    let line_prefix = format!("  L{}: ", finding.line());
    let action_str = sanitize_line(&finding.action_ref().to_string()).into_owned();

    match finding.status() {
        _ if let Some(label) = same_commit_label(finding) => {
            println!("{}{}  {}", line_prefix.dimmed(), action_str, label.cyan());
        }
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
                "{}{}  {}{} -> {}{}",
                line_prefix.dimmed(),
                action_str.yellow(),
                sanitize_line(current).red(),
                kind_note(finding).dimmed(),
                sanitize_line(&version_str).green(),
                sanitize_line(&suffix).dimmed()
            );
        }
        FindingStatus::Pinned { sha, tag } => {
            let tag_info = tag.as_ref().map(|t| format!(" ({t})")).unwrap_or_default();
            println!(
                "{}{}  {}{}",
                line_prefix.dimmed(),
                action_str,
                format!("pinned to {}", &sha[..7.min(sha.len())]).cyan(),
                sanitize_line(&tag_info).dimmed()
            );
        }
        FindingStatus::Floating { ref_name } => {
            let ref_name = sanitize_line(ref_name);
            println!(
                "{}{}  {}",
                line_prefix.dimmed(),
                action_str.yellow(),
                format!("floating branch '{ref_name}' — pin to a tag or SHA").yellow()
            );
        }
        FindingStatus::PinBranch { ref_name, sha } => {
            let ref_name = sanitize_line(ref_name);
            println!(
                "{}{}  {}",
                line_prefix.dimmed(),
                action_str.yellow(),
                pin_label(finding, &ref_name, sha).yellow()
            );
        }
        FindingStatus::Unknown { reason } => {
            println!(
                "{}{}  {}",
                line_prefix.dimmed(),
                action_str.yellow(),
                sanitize_line(&format!("unknown: {reason}")).yellow()
            );
        }
        FindingStatus::Unresolvable {
            reason,
            is_environment_error,
        } => print_unresolvable(finding, &line_prefix, reason, *is_environment_error),
    }
}

fn print_unresolvable(finding: &Finding, line_prefix: &str, reason: &str, environment: bool) {
    let label = if environment { "error" } else { "unresolvable" };
    println!(
        "{}{}  {}",
        line_prefix.dimmed(),
        sanitize_line(&finding.action_ref().to_string()).red(),
        sanitize_line(&format!("{label}: {reason}")).red()
    );
    if let Some(hint) = suggestion_hint(finding) {
        println!(
            "{}{}",
            " ".repeat(line_prefix.len()),
            sanitize_line(&hint).dimmed()
        );
    }
}

fn suggestion_hint(finding: &Finding) -> Option<String> {
    finding
        .suggestion()
        .map(|tag| format!("hint: change to {}", tag.version()))
}

fn kind_note(finding: &Finding) -> String {
    finding
        .ref_kind()
        .map_or_else(String::new, |k| format!(" ({k})"))
}

fn pin_label(finding: &Finding, ref_name: &str, sha: &str) -> String {
    let kind = finding
        .ref_kind()
        .map_or_else(|| "ref".to_string(), |k| k.to_string());
    format!(
        "pin {kind} '{ref_name}' -> {} # {ref_name}",
        sha.get(..7).unwrap_or(sha)
    )
}

fn same_commit_label(finding: &Finding) -> Option<String> {
    let target = finding.same_commit()?;
    let kind = finding
        .ref_kind()
        .map(|k| format!("{k} "))
        .unwrap_or_default();
    let current = version::Version::parse(finding.action_ref().ref_str());
    let lead = if target.version().is_more_specific_than(&current) {
        "pin available, "
    } else {
        ""
    };
    let label = format!(
        "{lead}same commit: {kind}{} = {} ({})",
        finding.action_ref().ref_str(),
        target.version(),
        target.sha().get(..7).unwrap_or_else(|| target.sha()),
    );
    Some(sanitize_line(&label).into_owned())
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
    println!();
    println!("{}", summary_line(findings).bold());
}

#[derive(Debug, Default, PartialEq, Eq)]
struct StatusCounts {
    up_to_date: usize,
    ignored: usize,
    outdated: usize,
    same_commit: usize,
    pinned: usize,
    floating: usize,
    to_pin: usize,
    unknown: usize,
    unresolvable: usize,
}

impl StatusCounts {
    fn tally(findings: &[Finding]) -> Self {
        let mut counts = Self::default();
        for finding in findings {
            let same_commit = finding.same_commit().is_some();
            let bucket = match finding.status() {
                FindingStatus::UpToDate => &mut counts.up_to_date,
                FindingStatus::Ignored => &mut counts.ignored,
                FindingStatus::Outdated { .. }
                | FindingStatus::Floating { .. }
                | FindingStatus::PinBranch { .. }
                    if same_commit =>
                {
                    &mut counts.same_commit
                }
                FindingStatus::Outdated { .. } => &mut counts.outdated,
                FindingStatus::Pinned { .. } => &mut counts.pinned,
                FindingStatus::Floating { .. } => &mut counts.floating,
                FindingStatus::PinBranch { .. } => &mut counts.to_pin,
                FindingStatus::Unknown { .. } => &mut counts.unknown,
                FindingStatus::Unresolvable { .. } => &mut counts.unresolvable,
            };
            *bucket += 1;
        }
        counts
    }

    const fn total(&self) -> usize {
        self.up_to_date
            + self.ignored
            + self.outdated
            + self.same_commit
            + self.pinned
            + self.floating
            + self.to_pin
            + self.unknown
            + self.unresolvable
    }
}

fn summary_line(findings: &[Finding]) -> String {
    let counts = StatusCounts::tally(findings);
    let total = counts.total();
    let StatusCounts {
        up_to_date,
        ignored,
        outdated,
        same_commit,
        pinned,
        floating,
        to_pin,
        unknown,
        unresolvable,
    } = counts;
    let sha_pinned = findings.iter().filter(|f| f.action_ref().is_sha()).count();

    format!(
        "Scanned {total} action{}: {up_to_date} up to date, {ignored} ignored, {outdated} outdated, {same_commit} same commit, {pinned} pinned, {floating} floating, {to_pin} branch refs to pin, {unknown} unknown, {unresolvable} unresolvable; {sha_pinned} of {total} SHA-pinned",
        if total == 1 { "" } else { "s" }
    )
}

/// Determines CLI exit code based on findings.
///
/// Exit codes:
/// - 0: Success ([`FindingStatus::UpToDate`], Ignored, Pinned, Floating are all considered healthy)
/// - 1: Outdated or pinnable branch found (only with `strict` mode)
/// - 2: User error (bad ref, repo not found)
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
        let host = action_ref.host().unwrap_or("github.com").to_string();
        Finding::new(
            file.to_string(),
            line,
            action_ref,
            status,
            "github".to_string(),
            host,
        )
    }

    const REPRO_SHA: &str = "c6f97d42243bad5fab37ca0427f495c86d5b1a18";

    fn same_commit_finding() -> Finding {
        make_finding(
            "ci.yml",
            3,
            make_action_ref("rust-lang", "crates-io-auth-action", "v1"),
            FindingStatus::PinBranch {
                ref_name: "v1".to_string(),
                sha: REPRO_SHA.to_string(),
            },
        )
        .with_ref(
            Some(RefKind::Branch),
            Some(make_version_info("v1.0.5", REPRO_SHA)),
        )
    }

    fn bump_finding() -> Finding {
        make_finding(
            "ci.yml",
            4,
            make_action_ref("docker", "setup-buildx-action", "v3"),
            FindingStatus::Outdated {
                current: "v3".to_string(),
                latest: make_version_info("v4", "abc123"),
                exact_version: None,
            },
        )
        .with_ref(Some(RefKind::Tag), None)
    }

    #[test]
    fn json_marks_ref_kind_and_same_commit() -> Result<(), Box<dyn std::error::Error>> {
        let same = serde_json::to_value(JsonFinding::from(&same_commit_finding()))?;
        assert_eq!(same["status"], "pin_branch");
        assert_eq!(same["ref_kind"], "branch");
        assert_eq!(same["same_commit"], "v1.0.5");

        let bump = serde_json::to_value(JsonFinding::from(&bump_finding()))?;
        assert_eq!(bump["status"], "outdated");
        assert_eq!(bump["ref_kind"], "tag");
        assert!(bump.get("same_commit").is_none(), "{bump}");

        let unlisted = serde_json::to_value(JsonFinding::from(&make_finding(
            "ci.yml",
            5,
            make_action_ref("actions", "checkout", "v4"),
            FindingStatus::UpToDate,
        )))?;
        assert!(unlisted.get("ref_kind").is_none(), "{unlisted}");
        Ok(())
    }

    fn typo_finding() -> Finding {
        make_finding(
            "ci.yml",
            6,
            make_action_ref("actions", "checkout", "v42.0"),
            FindingStatus::Unresolvable {
                reason: "no versions found".to_string(),
                is_environment_error: false,
            },
        )
        .with_suggestion(Some(make_version_info("v4.2.0", "abc123")))
    }

    #[test]
    fn unresolvable_findings_carry_the_suggested_tag() -> Result<(), Box<dyn std::error::Error>> {
        let typo = typo_finding();
        assert_eq!(
            suggestion_hint(&typo).as_deref(),
            Some("hint: change to v4.2.0")
        );
        let json = serde_json::to_value(JsonFinding::from(&typo))?;
        assert_eq!(json["status"], "unresolvable");
        assert_eq!(json["suggestion"], "v4.2.0");

        let bump = serde_json::to_value(JsonFinding::from(&bump_finding()))?;
        assert!(bump.get("suggestion").is_none(), "{bump}");
        assert_eq!(suggestion_hint(&bump_finding()), None);
        print_human(&[typo]);
        Ok(())
    }

    #[test]
    fn same_commit_label_replaces_the_bump_arrow() {
        assert_eq!(
            same_commit_label(&same_commit_finding()).as_deref(),
            Some("pin available, same commit: branch v1 = v1.0.5 (c6f97d4)")
        );
        assert_eq!(same_commit_label(&bump_finding()), None);
        assert_eq!(kind_note(&bump_finding()), " (tag)");
    }

    #[test]
    fn pin_label_names_the_ref_kind() {
        let finding = |kind| {
            make_finding(
                "ci.yml",
                3,
                make_action_ref("dtolnay", "rust-toolchain", "nightly"),
                FindingStatus::PinBranch {
                    ref_name: "nightly".to_string(),
                    sha: REPRO_SHA.to_string(),
                },
            )
            .with_ref(kind, None)
        };
        let label = |kind| pin_label(&finding(kind), "nightly", REPRO_SHA);
        assert_eq!(
            label(Some(RefKind::Tag)),
            "pin tag 'nightly' -> c6f97d4 # nightly"
        );
        assert_eq!(
            label(Some(RefKind::Branch)),
            "pin branch 'nightly' -> c6f97d4 # nightly"
        );
        assert_eq!(label(None), "pin ref 'nightly' -> c6f97d4 # nightly");
    }

    #[test]
    fn same_commit_label_on_a_less_specific_target_offers_no_pin() {
        let finding = make_finding(
            "ci.yml",
            3,
            make_action_ref("rust-lang", "crates-io-auth-action", "v1.0.5"),
            FindingStatus::Outdated {
                current: "v1.0.5".to_string(),
                latest: make_version_info("v1", REPRO_SHA),
                exact_version: None,
            },
        )
        .with_ref(Some(RefKind::Tag), Some(make_version_info("v1", REPRO_SHA)));
        assert_eq!(
            same_commit_label(&finding).as_deref(),
            Some("same commit: tag v1.0.5 = v1 (c6f97d4)")
        );
    }

    #[test]
    fn summary_counts_same_commit_apart_from_outdated() {
        let findings = vec![same_commit_finding(), bump_finding()];
        assert_eq!(
            summary_line(&findings),
            "Scanned 2 actions: 0 up to date, 0 ignored, 1 outdated, 1 same commit, 0 pinned, 0 floating, 0 branch refs to pin, 0 unknown, 0 unresolvable; 0 of 2 SHA-pinned"
        );
        print_human(&findings);
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
        assert_eq!(
            summary_line(&findings),
            "Scanned 1 action: 1 up to date, 0 ignored, 0 outdated, 0 same commit, 0 pinned, 0 floating, 0 branch refs to pin, 0 unknown, 0 unresolvable; 0 of 1 SHA-pinned"
        );
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
                make_action_ref("actions", "cache", &"a".repeat(40)),
                FindingStatus::Pinned {
                    sha: "a".repeat(40),
                    tag: None,
                },
            ),
            make_finding(
                "test.yml",
                5,
                make_action_ref("actions", "setup-go", "main"),
                FindingStatus::PinBranch {
                    ref_name: "main".to_string(),
                    sha: "f".repeat(40),
                },
            ),
            make_finding(
                "test.yml",
                6,
                make_action_ref("unknown", "action", "v1"),
                FindingStatus::Unresolvable {
                    reason: "not found".to_string(),
                    is_environment_error: false,
                },
            ),
        ];

        assert_eq!(
            summary_line(&findings),
            "Scanned 6 actions: 1 up to date, 1 ignored, 1 outdated, 0 same commit, 1 pinned, 0 floating, 1 branch refs to pin, 0 unknown, 1 unresolvable; 1 of 6 SHA-pinned"
        );
    }

    #[test]
    fn test_summary_counts_every_status_once() {
        let statuses = [
            FindingStatus::UpToDate,
            FindingStatus::Ignored,
            FindingStatus::Outdated {
                current: "v3".to_string(),
                latest: make_version_info("v4.0.0", "abc123"),
                exact_version: None,
            },
            FindingStatus::Pinned {
                sha: "a".repeat(40),
                tag: None,
            },
            FindingStatus::Floating {
                ref_name: "dev".to_string(),
            },
            FindingStatus::PinBranch {
                ref_name: "main".to_string(),
                sha: "f".repeat(40),
            },
            FindingStatus::Unknown {
                reason: "tag beyond max_tags".to_string(),
            },
            FindingStatus::Unresolvable {
                reason: "not found".to_string(),
                is_environment_error: false,
            },
            FindingStatus::Unresolvable {
                reason: "timeout".to_string(),
                is_environment_error: true,
            },
        ];
        let findings: Vec<Finding> = statuses
            .into_iter()
            .zip(1..)
            .map(|(status, line)| {
                make_finding("test.yml", line, make_action_ref("o", "r", "v1"), status)
            })
            .collect();

        let counts = StatusCounts::tally(&findings);
        assert_eq!(
            counts,
            StatusCounts {
                up_to_date: 1,
                ignored: 1,
                outdated: 1,
                same_commit: 0,
                pinned: 1,
                floating: 1,
                to_pin: 1,
                unknown: 1,
                unresolvable: 2,
            }
        );
        assert_eq!(counts.total(), findings.len());
        assert_eq!(
            summary_line(&findings),
            "Scanned 9 actions: 1 up to date, 1 ignored, 1 outdated, 0 same commit, 1 pinned, 1 floating, 1 branch refs to pin, 1 unknown, 2 unresolvable; 0 of 9 SHA-pinned"
        );
    }

    #[test]
    fn test_print_summary_empty() {
        let findings: Vec<Finding> = vec![];
        assert_eq!(
            summary_line(&findings),
            "Scanned 0 actions: 0 up to date, 0 ignored, 0 outdated, 0 same commit, 0 pinned, 0 floating, 0 branch refs to pin, 0 unknown, 0 unresolvable; 0 of 0 SHA-pinned"
        );
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
            host: "github.com".to_string(),
            ref_kind: None,
            same_commit: None,
            suggestion: None,
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
            host: "github.com".to_string(),
            ref_kind: None,
            same_commit: None,
            suggestion: None,
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
            host: "github.com".to_string(),
            ref_kind: None,
            same_commit: None,
            suggestion: None,
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
            host: "github.com".to_string(),
            ref_kind: None,
            same_commit: None,
            suggestion: None,
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
            host: "github.com".to_string(),
            ref_kind: None,
            same_commit: None,
            suggestion: None,
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
            host: "github.com".to_string(),
            ref_kind: None,
            same_commit: None,
            suggestion: None,
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
            host: "github.com".to_string(),
            ref_kind: None,
            same_commit: None,
            suggestion: None,
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
            host: "github.com".to_string(),
            ref_kind: None,
            same_commit: None,
            suggestion: None,
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
            host: "github.com".to_string(),
            ref_kind: None,
            same_commit: None,
            suggestion: None,
        };

        let json_finding = JsonFinding::from(&finding);
        let serialized = serde_json::to_string(&json_finding).unwrap();

        assert!(
            !serialized.contains("pinned_tag"),
            "JSON should not contain pinned_tag field for non-pinned: {serialized}"
        );
    }
}
