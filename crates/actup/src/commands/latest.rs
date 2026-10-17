//! Default command: look up the latest version of an action.
//!
//! `actup actions/checkout` prints the latest version tag.
//! `actup actions/checkout@v4` prints the latest within major v4.

use actup_core::{ActionRef, Config, provider, version};
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::cli::{CliExitCode, LatestArgs, OutputFormat};

/// Parsed action query from the positional argument.
struct ActionQuery {
    owner: String,
    repo: String,
    /// Optional constraint ref (e.g. `v4` from `actions/checkout@v4`)
    constraint: Option<String>,
}

/// Result of a latest-version lookup, serializable for JSON/TOML output.
#[derive(Serialize)]
pub struct LookupResult {
    host: String,
    provider: String,
    owner: String,
    repo: String,
    latest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    constraint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exact_version: Option<String>,
}

/// Parse `owner/repo` or `owner/repo@ref` into an [`ActionQuery`].
fn parse_action_query(input: &str) -> Option<ActionQuery> {
    // Split off optional @ref
    let (slug, constraint) = match input.split_once('@') {
        Some((slug, r)) if !r.is_empty() => (slug, Some(r.to_string())),
        _ => (input, None),
    };

    let (owner, repo) = slug.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }

    // Reject if there are extra slashes (subpath not supported for quick lookup)
    if repo.contains('/') {
        return None;
    }

    Some(ActionQuery {
        owner: owner.to_string(),
        repo: repo.to_string(),
        constraint,
    })
}

/// Pick the best version to display from a [`version::Set`].
///
/// If a constraint is given (e.g. `v4`), returns the latest within that major.
/// Otherwise returns the global latest. Also returns an optional "exact version"
/// string when the best match is a major-only tag with a more precise semver
/// available.
fn pick_best<'a>(
    versions: &'a version::Set,
    constraint: Option<&str>,
) -> Option<(&'a version::Info, Option<String>)> {
    if let Some(ref_str) = constraint {
        let parsed = version::Version::parse(ref_str);
        if let version::Version::Semver { major, .. } = &parsed {
            // Try major tag first, then latest_per_major
            if let Some(major_tag) = versions.major_tags().get(major) {
                let exact = versions
                    .latest_per_major()
                    .get(major)
                    .filter(|lpm| lpm.version() != major_tag.version())
                    .map(|lpm| lpm.version().to_string());
                return Some((major_tag, exact));
            }
            if let Some(lpm) = versions.latest_per_major().get(major) {
                return Some((lpm, None));
            }
        }
        // Non-semver constraint or no match for that major
        None
    } else {
        // Global latest
        let latest = versions.latest()?;
        // If there's a major tag for this version, prefer it
        if let version::Version::Semver { major, .. } = latest.version()
            && let Some(major_tag) = versions.major_tags().get(major)
        {
            let exact = if major_tag.version() == latest.version() {
                None
            } else {
                Some(latest.version().to_string())
            };
            return Some((major_tag, exact));
        }
        Some((latest, None))
    }
}

/// Run the latest-version lookup.
pub async fn run(args: &LatestArgs, _config: &Config, registry: &provider::Registry) -> i32 {
    let Some(action_str) = args.action.as_deref() else {
        // No action provided — caller should have printed help already
        return CliExitCode::Success.into();
    };

    let Some(query) = parse_action_query(action_str) else {
        eprintln!(
            "Invalid action reference: {action_str}\n\
             Expected format: owner/repo or owner/repo@ref"
        );
        return CliExitCode::UserError.into();
    };

    // Build a synthetic ActionRef for provider lookup
    let ref_str = query.constraint.as_deref().unwrap_or("v0");
    let action_ref = ActionRef::simple(&query.owner, &query.repo, ref_str);

    let Some(provider) = registry.find(action_ref.host()) else {
        eprintln!("No provider configured for default host");
        return CliExitCode::EnvironmentError.into();
    };

    let repo_id = action_ref.to_repo_id().without_subpath();
    let versions = match provider.get_versions(&repo_id).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return if e.is_environment_error() {
                CliExitCode::EnvironmentError.into()
            } else {
                CliExitCode::UserError.into()
            };
        }
    };

    let Some((best, exact_version)) = pick_best(&versions, query.constraint.as_deref()) else {
        let context = query
            .constraint
            .as_ref()
            .map(|c| format!(" matching {c}"))
            .unwrap_or_default();
        eprintln!(
            "No versions found for {}/{}{context}",
            query.owner, query.repo
        );
        return CliExitCode::UserError.into();
    };

    let result = LookupResult {
        host: provider.host().to_string(),
        provider: provider.kind().to_string(),
        owner: query.owner.clone(),
        repo: query.repo.clone(),
        latest: best.version().to_string(),
        latest_sha: Some(best.sha().to_string()),
        constraint: query.constraint.clone(),
        exact_version: exact_version.clone(),
    };

    match args.format {
        OutputFormat::Human => {
            let suffix = exact_version
                .as_ref()
                .filter(|ev| ev.as_str() != result.latest.as_str())
                .map(|ev| format!(" ({ev})"))
                .unwrap_or_default();
            println!(
                "{}/{}  {}{}",
                query.owner,
                query.repo,
                result.latest.green(),
                suffix.dimmed()
            );
        }
        OutputFormat::Json => match serde_json::to_string_pretty(&result) {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!("Error serializing result: {e}"),
        },
        OutputFormat::Toml => match toml::to_string_pretty(&result) {
            Ok(t) => println!("{t}"),
            Err(e) => eprintln!("Error serializing result: {e}"),
        },
    }

    CliExitCode::Success.into()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple() {
        let q = parse_action_query("actions/checkout").unwrap();
        assert_eq!(q.owner, "actions");
        assert_eq!(q.repo, "checkout");
        assert!(q.constraint.is_none());
    }

    #[test]
    fn parse_with_ref() {
        let q = parse_action_query("actions/checkout@v4").unwrap();
        assert_eq!(q.owner, "actions");
        assert_eq!(q.repo, "checkout");
        assert_eq!(q.constraint.as_deref(), Some("v4"));
    }

    #[test]
    fn parse_with_semver_ref() {
        let q = parse_action_query("actions/setup-node@v4.2.1").unwrap();
        assert_eq!(q.owner, "actions");
        assert_eq!(q.repo, "setup-node");
        assert_eq!(q.constraint.as_deref(), Some("v4.2.1"));
    }

    #[test]
    fn parse_rejects_empty_owner() {
        assert!(parse_action_query("/checkout").is_none());
    }

    #[test]
    fn parse_rejects_empty_repo() {
        assert!(parse_action_query("actions/").is_none());
    }

    #[test]
    fn parse_rejects_no_slash() {
        assert!(parse_action_query("actions-checkout").is_none());
    }

    #[test]
    fn parse_rejects_subpath() {
        assert!(parse_action_query("owner/repo/subpath").is_none());
    }

    #[test]
    fn parse_at_sign_no_ref() {
        // "actions/checkout@" — trailing @ with empty ref is treated as no constraint
        let q = parse_action_query("actions/checkout@").unwrap();
        assert!(q.constraint.is_none());
    }

    #[test]
    fn pick_best_global_latest() {
        use std::{collections::HashMap, sync::Arc};

        let info = Arc::new(version::Info::new(version::Version::parse("v6.0.2"), "abc"));
        let set = version::Set::build(
            vec![Arc::clone(&info)],
            HashMap::new(),
            HashMap::new(),
            Some(info),
        );

        let (best, exact) = pick_best(&set, None).unwrap();
        assert_eq!(best.version().to_string(), "v6.0.2");
        assert!(exact.is_none());
    }

    #[test]
    fn pick_best_with_major_tag() {
        use std::{collections::HashMap, sync::Arc};

        let full = Arc::new(version::Info::new(version::Version::parse("v6.0.2"), "abc"));
        let major = Arc::new(version::Info::new(version::Version::parse("v6"), "abc"));

        let mut major_tags = HashMap::new();
        major_tags.insert(6, Arc::clone(&major));
        let mut lpm = HashMap::new();
        lpm.insert(6, Arc::clone(&full));

        let set = version::Set::build(
            vec![Arc::clone(&full)],
            lpm,
            major_tags,
            Some(Arc::clone(&full)),
        );

        // Global latest should prefer major tag and show exact
        let (best, exact) = pick_best(&set, None).unwrap();
        assert_eq!(best.version().to_string(), "v6");
        assert_eq!(exact.as_deref(), Some("v6.0.2"));
    }

    #[test]
    fn pick_best_constrained_major() {
        use std::{collections::HashMap, sync::Arc};

        let v4 = Arc::new(version::Info::new(version::Version::parse("v4"), "sha4"));
        let v4_full = Arc::new(version::Info::new(
            version::Version::parse("v4.2.1"),
            "sha4",
        ));
        let v6 = Arc::new(version::Info::new(
            version::Version::parse("v6.0.0"),
            "sha6",
        ));

        let mut major_tags = HashMap::new();
        major_tags.insert(4, Arc::clone(&v4));
        let mut lpm = HashMap::new();
        lpm.insert(4, Arc::clone(&v4_full));
        lpm.insert(6, Arc::clone(&v6));

        let set = version::Set::build(
            vec![Arc::clone(&v4_full), Arc::clone(&v6)],
            lpm,
            major_tags,
            Some(Arc::clone(&v6)),
        );

        // Constrained to v4 should return major tag v4 with exact v4.2.1
        let (best, exact) = pick_best(&set, Some("v4")).unwrap();
        assert_eq!(best.version().to_string(), "v4");
        assert_eq!(exact.as_deref(), Some("v4.2.1"));
    }

    #[test]
    fn pick_best_no_match_for_constraint() {
        use std::{collections::HashMap, sync::Arc};

        let v6 = Arc::new(version::Info::new(version::Version::parse("v6.0.0"), "sha"));
        let set = version::Set::build(
            vec![Arc::clone(&v6)],
            HashMap::new(),
            HashMap::new(),
            Some(v6),
        );

        // No v99 exists
        assert!(pick_best(&set, Some("v99")).is_none());
    }

    #[test]
    fn pick_best_empty_set() {
        let set = version::Set::new();
        assert!(pick_best(&set, None).is_none());
    }
}
