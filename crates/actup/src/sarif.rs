//! SARIF 2.1.0 output and GitHub Actions workflow-command annotations for `check`.

use actup_core::{ActionRef, RefKind, replace_unsafe_characters};
use serde_json::{Value, json};

use crate::output::{Finding, FindingStatus, same_commit_text, status_name};

const INFORMATION_URI: &str = env!("CARGO_PKG_REPOSITORY");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Error,
    Warning,
    Note,
}

impl Level {
    const fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Note => "note",
        }
    }
}

const fn base_level(status: &FindingStatus) -> Option<Level> {
    match status {
        FindingStatus::UpToDate | FindingStatus::Ignored => None,
        FindingStatus::Outdated { .. }
        | FindingStatus::Floating { .. }
        | FindingStatus::PinBranch { .. } => Some(Level::Warning),
        FindingStatus::Unresolvable {
            is_environment_error: false,
            ..
        } => Some(Level::Error),
        FindingStatus::Pinned { .. }
        | FindingStatus::Unknown { .. }
        | FindingStatus::Unresolvable { .. } => Some(Level::Note),
    }
}

fn level(finding: &Finding) -> Option<Level> {
    let base = base_level(finding.status())?;
    Some(if finding.same_commit().is_some() {
        Level::Note
    } else {
        base
    })
}

const fn rule_text(status: &FindingStatus) -> (&'static str, &'static str) {
    match status {
        FindingStatus::UpToDate => (
            "Up-to-date action",
            "The action is at the newest allowed version.",
        ),
        FindingStatus::Ignored => ("Ignored action", "`scan.ignore` matches the action."),
        FindingStatus::Outdated { .. } => (
            "Outdated action",
            "A newer version of the action is available under the update policy.",
        ),
        FindingStatus::Pinned { .. } => (
            "Pinned action",
            "The action is pinned to a full commit SHA.",
        ),
        FindingStatus::Floating { .. } => {
            ("Floating ref", "The action follows a branch that moves.")
        }
        FindingStatus::PinBranch { .. } => (
            "Branch ref to pin",
            "The action follows a branch that the policy pins to its current commit.",
        ),
        FindingStatus::Unknown { .. } => (
            "Unknown version",
            "The ref exists, but its version cannot be compared.",
        ),
        FindingStatus::Unresolvable {
            is_environment_error: false,
            ..
        } => (
            "Unresolvable ref",
            "The ref or its repository does not exist.",
        ),
        FindingStatus::Unresolvable { .. } => (
            "Check failed",
            "A network, authentication or rate limit error stopped the check.",
        ),
    }
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn describe(finding: &Finding) -> String {
    let action = finding.action_ref().to_string();
    if let Some(text) = same_commit_text(finding) {
        return format!("{action}: {text}");
    }
    match finding.status() {
        FindingStatus::UpToDate => format!("{action}: up to date"),
        FindingStatus::Ignored => format!("{action}: ignored"),
        FindingStatus::Outdated {
            current,
            latest,
            exact_version,
        } => {
            let version = latest.version().to_string();
            let exact = exact_version
                .as_ref()
                .filter(|exact| **exact != version)
                .map(|exact| format!(" ({exact})"))
                .unwrap_or_default();
            format!("{action}: {current} -> {version}{exact}")
        }
        FindingStatus::Pinned { sha, tag } => {
            let tag = tag.as_ref().map(|t| format!(" ({t})")).unwrap_or_default();
            format!("{action}: pinned to {}{tag}", short(sha))
        }
        FindingStatus::Floating { ref_name } => {
            let kind = RefKind::noun(finding.ref_kind());
            format!("{action}: floating {kind} '{ref_name}', pin to a tag or SHA")
        }
        FindingStatus::PinBranch { ref_name, sha } => {
            let kind = RefKind::noun(finding.ref_kind());
            format!(
                "{action}: pin {kind} '{ref_name}' to {} # {ref_name}",
                short(sha)
            )
        }
        FindingStatus::Unknown { reason } => format!("{action}: unknown: {reason}"),
        FindingStatus::Unresolvable { reason, .. } => format!("{action}: {reason}"),
    }
}

fn slug(action: &ActionRef) -> String {
    action.subpath().map_or_else(
        || format!("{}/{}", action.owner(), action.repo()),
        |subpath| format!("{}/{}/{subpath}", action.owner(), action.repo()),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathStyle {
    Unix,
    Windows,
}

impl PathStyle {
    const HOST: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Unix
    };

    fn slashes(self, path: &str) -> String {
        match self {
            Self::Unix => path.to_string(),
            Self::Windows => path.replace('\\', "/"),
        }
    }

    const fn is_drive(self, segment: &str) -> bool {
        matches!(self, Self::Windows)
            && matches!(segment.as_bytes(), [letter, b':'] if letter.is_ascii_alphabetic())
    }
}

fn path(finding: &Finding) -> String {
    PathStyle::HOST.slashes(finding.file())
}

fn encode(segment: &str) -> String {
    segment
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn uri(finding: &Finding) -> String {
    uri_in(PathStyle::HOST, finding.file())
}

fn uri_in(style: PathStyle, file: &str) -> String {
    let path = style.slashes(file);
    let (absolute, rest) = path
        .strip_prefix('/')
        .map_or((false, path.as_str()), |rest| (true, rest));
    let segments: Vec<String> = rest
        .split('/')
        .enumerate()
        .map(|(index, segment)| {
            if index == 0 && style.is_drive(segment) {
                segment.to_string()
            } else {
                encode(segment)
            }
        })
        .collect();
    let drive = rest
        .split('/')
        .next()
        .is_some_and(|segment| style.is_drive(segment));
    let joined = segments.join("/");
    if absolute || drive {
        format!("file:///{joined}")
    } else {
        joined
    }
}

/// A SARIF 2.1.0 log of every finding other than up-to-date and ignored ones, with `unreadable` files as execution notifications.
pub fn sarif(findings: &[Finding], unreadable: &[String]) -> Value {
    let mut rules: Vec<Value> = Vec::new();
    let mut rule_ids: Vec<&str> = Vec::new();
    let results: Vec<Value> = findings
        .iter()
        .filter_map(|finding| {
            let level = level(finding)?;
            let status = finding.status();
            let name = status_name(status);
            let index = rule_ids
                .iter()
                .position(|id| *id == name)
                .unwrap_or_else(|| {
                    let (title, description) = rule_text(status);
                    rule_ids.push(name);
                    rules.push(json!({
                        "id": format!("actup/{name}"),
                        "name": title,
                        "shortDescription": { "text": description },
                        "defaultConfiguration": {
                            "level": base_level(status).map_or("none", Level::name),
                        },
                        "helpUri": INFORMATION_URI,
                    }));
                    rule_ids.len() - 1
                });
            let uri = uri(finding);
            let mut region = json!({ "startLine": finding.line() });
            if let Some(span) = finding.span() {
                region["startColumn"] = json!(span.start);
                region["endColumn"] = json!(span.end);
            }
            let action = finding.action_ref();
            Some(json!({
                "ruleId": format!("actup/{name}"),
                "ruleIndex": index,
                "level": level.name(),
                "message": { "text": describe(finding) },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": uri },
                        "region": region,
                    },
                }],
                "partialFingerprints": {
                    "actup/v1": format!(
                        "{uri}|{name}|{}|{}",
                        slug(action),
                        action.ref_str()
                    ),
                },
            }))
        })
        .collect();
    let mut invocation = json!({ "executionSuccessful": unreadable.is_empty() });
    if !unreadable.is_empty() {
        invocation["toolExecutionNotifications"] = unreadable
            .iter()
            .map(|message| json!({ "level": "error", "message": { "text": message } }))
            .collect();
    }
    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "actup",
                    "semanticVersion": env!("CARGO_PKG_VERSION"),
                    "informationUri": INFORMATION_URI,
                    "rules": rules,
                },
            },
            "columnKind": "utf16CodeUnits",
            "results": results,
            "invocations": [invocation],
        }],
    })
}

fn escape(text: &str, property: bool) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '%' => escaped.push_str("%25"),
            '\r' => escaped.push_str("%0D"),
            '\n' => escaped.push_str("%0A"),
            ':' if property => escaped.push_str("%3A"),
            ',' if property => escaped.push_str("%2C"),
            c => escaped.push(c),
        }
    }
    replace_unsafe_characters(&escaped).into_owned()
}

fn escape_data(text: &str) -> String {
    escape(text, false)
}

fn escape_property(text: &str) -> String {
    escape(text, true)
}

/// One `::error` or `::warning` workflow command per finding at error or warning level.
pub fn annotations(findings: &[Finding]) -> Vec<String> {
    findings
        .iter()
        .filter_map(|finding| {
            let level = match level(finding)? {
                Level::Error => "error",
                Level::Warning => "warning",
                Level::Note => return None,
            };
            let columns = finding
                .span()
                .map(|span| format!(",col={},endColumn={}", span.start, span.end))
                .unwrap_or_default();
            Some(format!(
                "::{level} file={},line={}{columns}::{}",
                escape_property(&path(finding)),
                finding.line(),
                escape_data(&describe(finding))
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use actup_core::{RefKind, UsesLocation, version};

    use super::*;
    use crate::output::Span;

    fn info(tag: &str, sha: &str) -> Arc<version::Info> {
        Arc::new(version::Info::new(version::Version::parse(tag), sha))
    }

    fn finding(file: &str, line: usize, uses: &str, status: FindingStatus) -> Finding {
        let Ok(Some(action_ref)) = actup_core::parse_uses(uses) else {
            panic!("bad fixture {uses}");
        };
        Finding::new(
            file.to_string(),
            line,
            action_ref,
            status,
            "github".to_string(),
            "github.com".to_string(),
        )
    }

    fn fixtures() -> Vec<Finding> {
        let new = "b".repeat(40);
        vec![
            finding("ci.yml", 1, "o/up@v1", FindingStatus::UpToDate),
            finding(
                ".github/workflows/ci.yml",
                2,
                "o/old/sub@v1.0.0",
                FindingStatus::Outdated {
                    current: "v1.0.0".to_string(),
                    latest: info("v1.1.0", &new),
                    exact_version: None,
                },
            )
            .with_span(Span { start: 15, end: 31 }),
            finding(
                "/abs/ci.yml",
                3,
                "o/same@v1",
                FindingStatus::Outdated {
                    current: "v1".to_string(),
                    latest: info("v1.0.5", &new),
                    exact_version: None,
                },
            )
            .with_ref(Some(RefKind::Tag), Some(info("v1.0.5", &new))),
            finding("ci.yml", 4, "o/skip@v1", FindingStatus::Ignored),
            finding(
                "ci.yml",
                5,
                "o/gone@v9",
                FindingStatus::Unresolvable {
                    reason: "tag 'v9' not found".to_string(),
                    is_environment_error: false,
                },
            ),
            finding(
                "ci.yml",
                6,
                "o/net@v1",
                FindingStatus::Unresolvable {
                    reason: "network, 50% lost".to_string(),
                    is_environment_error: true,
                },
            ),
            finding(
                "ci.yml",
                7,
                "o/pin@main",
                FindingStatus::Floating {
                    ref_name: "main".to_string(),
                },
            )
            .with_ref(Some(RefKind::Branch), None),
        ]
    }

    #[test]
    fn sarif_lists_rules_results_and_notifications() -> Result<(), Box<dyn std::error::Error>> {
        let log = sarif(
            &fixtures(),
            &["ci.yml: stream did not contain valid UTF-8".to_string()],
        );
        let run = &log["runs"][0];
        assert_eq!(log["version"], "2.1.0");
        assert_eq!(run["columnKind"], "utf16CodeUnits");
        assert_eq!(run["tool"]["driver"]["name"], "actup");
        let rules: Vec<(&str, &str)> = run["tool"]["driver"]["rules"]
            .as_array()
            .ok_or("no rules")?
            .iter()
            .map(|r| {
                (
                    r["id"].as_str().unwrap_or_default(),
                    r["defaultConfiguration"]["level"]
                        .as_str()
                        .unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            rules,
            [
                ("actup/outdated", "warning"),
                ("actup/unresolvable", "error"),
                ("actup/error", "note"),
                ("actup/floating", "warning"),
            ]
        );
        let results: Vec<(&str, u64, &str, u64, &str)> = run["results"]
            .as_array()
            .ok_or("no results")?
            .iter()
            .map(|r| {
                (
                    r["ruleId"].as_str().unwrap_or_default(),
                    r["ruleIndex"].as_u64().unwrap_or(99),
                    r["level"].as_str().unwrap_or_default(),
                    r["locations"][0]["physicalLocation"]["region"]["startLine"]
                        .as_u64()
                        .unwrap_or_default(),
                    r["locations"][0]["physicalLocation"]["artifactLocation"]["uri"]
                        .as_str()
                        .unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            results,
            [
                (
                    "actup/outdated",
                    0,
                    "warning",
                    2,
                    ".github/workflows/ci.yml"
                ),
                ("actup/outdated", 0, "note", 3, "file:///abs/ci.yml"),
                ("actup/unresolvable", 1, "error", 5, "ci.yml"),
                ("actup/error", 2, "note", 6, "ci.yml"),
                ("actup/floating", 3, "warning", 7, "ci.yml"),
            ]
        );
        let first = &run["results"][0];
        assert_eq!(
            first["message"]["text"],
            "o/old/sub@v1.0.0: v1.0.0 -> v1.1.0"
        );
        assert_eq!(
            first["locations"][0]["physicalLocation"]["region"],
            json!({ "startLine": 2, "startColumn": 15, "endColumn": 31 })
        );
        assert_eq!(
            first["partialFingerprints"]["actup/v1"],
            ".github/workflows/ci.yml|outdated|o/old/sub|v1.0.0"
        );
        assert_eq!(
            run["results"][1]["message"]["text"],
            "o/same@v1: pin available, same commit: tag v1 = v1.0.5 (bbbbbbb)"
        );
        assert!(
            run["results"][1]["locations"][0]["physicalLocation"]["region"]
                .get("startColumn")
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn same_commit_on_a_less_specific_target_offers_no_pin() {
        let same = finding(
            "ci.yml",
            3,
            "o/same@v1.0.5",
            FindingStatus::Outdated {
                current: "v1.0.5".to_string(),
                latest: info("v1", &"b".repeat(40)),
                exact_version: None,
            },
        )
        .with_ref(Some(RefKind::Tag), Some(info("v1", &"b".repeat(40))));
        let findings = [same];
        let log = sarif(&findings, &[]);
        let result = &log["runs"][0]["results"][0];
        assert_eq!(
            result["message"]["text"],
            "o/same@v1.0.5: same commit: tag v1.0.5 = v1 (bbbbbbb)"
        );
        assert_eq!(result["level"], "note");
        assert_eq!(annotations(&findings), Vec::<String>::new());
    }

    #[test]
    fn sarif_marks_unreadable_files_as_failed_execution() {
        let log = sarif(
            &[],
            &["ci.yml: stream did not contain valid UTF-8".to_string()],
        );
        let run = &log["runs"][0];
        assert_eq!(
            run["invocations"],
            json!([{
                "executionSuccessful": false,
                "toolExecutionNotifications": [{
                    "level": "error",
                    "message": { "text": "ci.yml: stream did not contain valid UTF-8" },
                }],
            }])
        );
        assert_eq!(
            sarif(&[], &[])["runs"][0]["invocations"],
            json!([{ "executionSuccessful": true }])
        );
    }

    #[test]
    fn sarif_uris_are_uri_references() {
        let at = |file: &str| uri(&finding(file, 1, "o/r@v1", FindingStatus::UpToDate));
        assert_eq!(
            at(".github/workflows/my ci#1.yml"),
            ".github/workflows/my%20ci%231.yml"
        );
        assert_eq!(at("dé/a:b%.yml"), "d%C3%A9/a%3Ab%25.yml");
        assert_eq!(at("/abs/ci.yml"), "file:///abs/ci.yml");
        for (file, unix, windows) in [
            (
                "C:\\work\\ci.yml",
                "C%3A%5Cwork%5Cci.yml",
                "file:///C:/work/ci.yml",
            ),
            ("c:/ci.yml", "c%3A/ci.yml", "file:///c:/ci.yml"),
            ("c:", "c%3A", "file:///c:"),
            ("a\\b.yml", "a%5Cb.yml", "a/b.yml"),
            ("ab:/ci.yml", "ab%3A/ci.yml", "ab%3A/ci.yml"),
        ] {
            assert_eq!(uri_in(PathStyle::Unix, file), unix, "{file}");
            assert_eq!(uri_in(PathStyle::Windows, file), windows, "{file}");
        }
        for file in ["my ci#1.yml", "/x y/[z].yml", "a?b.yml"] {
            let uri = at(file);
            assert!(
                uri.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~%/:".contains(&b)),
                "{file}: {uri}"
            );
        }
    }

    #[test]
    fn annotations_cover_errors_and_warnings_with_escapes() {
        let mut findings = fixtures();
        findings.push(finding(
            "a,b:c.yml",
            8,
            "o/esc@v1",
            FindingStatus::Unresolvable {
                reason: "100% gone\u{202e}".to_string(),
                is_environment_error: false,
            },
        ));
        findings.push(finding(
            "::error file=x::forged.yml",
            8,
            "o/esc@v1",
            FindingStatus::Unresolvable {
                reason: "ref 'v\r\n::error::lf' not found".to_string(),
                is_environment_error: false,
            },
        ));
        findings.push(finding(
            "a##[b].yml",
            8,
            "o/esc/##[error]x@v1",
            FindingStatus::Unresolvable {
                reason: "ref '##[error]x' not found".to_string(),
                is_environment_error: false,
            },
        ));
        findings.push(finding(
            "/abs/ci.yml",
            9,
            "o/far@main",
            FindingStatus::Floating {
                ref_name: "main".to_string(),
            },
        ));
        assert_eq!(
            annotations(&findings),
            [
                "::warning file=.github/workflows/ci.yml,line=2,col=15,endColumn=31::o/old/sub@v1.0.0: v1.0.0 -> v1.1.0",
                "::error file=ci.yml,line=5::o/gone@v9: tag 'v9' not found",
                "::warning file=ci.yml,line=7::o/pin@main: floating branch 'main', pin to a tag or SHA",
                "::error file=a%2Cb%3Ac.yml,line=8::o/esc@v1: 100%25 gone\u{fffd}",
                "::error file=%3A%3Aerror file=x%3A%3Aforged.yml,line=8::o/esc@v1: ref 'v%0D%0A::error::lf' not found",
                "::error file=a##[b].yml,line=8::o/esc/##[error]x@v1: ref '##[error]x' not found",
                "::warning file=/abs/ci.yml,line=9::o/far@main: floating ref 'main', pin to a tag or SHA",
            ]
        );
    }

    #[test]
    fn describe_names_every_status() {
        let pinned = finding(
            "ci.yml",
            1,
            &format!("o/p@{}", "a".repeat(40)),
            FindingStatus::Pinned {
                sha: "a".repeat(40),
                tag: Some("v4.0.0".to_string()),
            },
        );
        let to_pin = finding(
            "ci.yml",
            2,
            "o/b@main",
            FindingStatus::PinBranch {
                ref_name: "main".to_string(),
                sha: "c".repeat(40),
            },
        )
        .with_ref(Some(RefKind::Branch), None);
        let unknown = finding(
            "ci.yml",
            3,
            "o/u@v2",
            FindingStatus::Unknown {
                reason: "beyond max_tags".to_string(),
            },
        );
        let exact = finding(
            "ci.yml",
            4,
            "o/x@v3",
            FindingStatus::Outdated {
                current: "v3".to_string(),
                latest: info("v4", &"d".repeat(40)),
                exact_version: Some("v4.2.1".to_string()),
            },
        );
        let described: Vec<String> = [&pinned, &to_pin, &unknown, &exact]
            .into_iter()
            .chain(&fixtures()[..1])
            .chain(&fixtures()[3..4])
            .map(describe)
            .collect();
        assert_eq!(
            described,
            [
                format!("o/p@{}: pinned to aaaaaaa (v4.0.0)", "a".repeat(40)),
                "o/b@main: pin branch 'main' to ccccccc # main".to_string(),
                "o/u@v2: unknown: beyond max_tags".to_string(),
                "o/x@v3: v3 -> v4 (v4.2.1)".to_string(),
                "o/up@v1: up to date".to_string(),
                "o/skip@v1: ignored".to_string(),
            ]
        );
        let levels: Vec<Option<Level>> = [&pinned, &to_pin, &unknown]
            .into_iter()
            .map(level)
            .collect();
        assert_eq!(
            levels,
            [Some(Level::Note), Some(Level::Warning), Some(Level::Note)]
        );
        assert_eq!(rule_text(&FindingStatus::UpToDate).0, "Up-to-date action");
        assert_eq!(rule_text(&FindingStatus::Ignored).0, "Ignored action");
    }

    #[test]
    fn span_counts_utf16_columns_of_the_value() {
        let line = "  - { name: \"déploiement ✓ 🚀\", uses: \"o/r@v1\" }";
        let loc = UsesLocation::new(1, 45, "o/r@v1".to_string(), line.to_string());
        assert_eq!(loc.value_range(), Some(44..50));
        assert_eq!(Span::of(&loc), Span { start: 40, end: 46 });

        let escaped = "  - uses: \"o/r@v\\x31\"";
        let loc = UsesLocation::new(1, 12, "o/r@v1".to_string(), escaped.to_string());
        assert_eq!(loc.value_range(), None);
        assert_eq!(Span::of(&loc), Span { start: 12, end: 18 });
    }
}
