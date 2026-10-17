//! Default command: look up the latest version of an action.
//!
//! `actup actions/checkout` prints the latest version tag.
//! `actup actions/checkout@v4` prints the latest within major v4.
//! `actup gitlab.com/group/project` asks the provider of `gitlab.com`.

use actup_core::{ActionRef, Config, parse_action_path, provider, sanitize_for_terminal, version};
use anstream::{eprintln, println};
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::{
    cli::{CliExitCode, LatestArgs, OutputFormat},
    scan::{batch_fetch_versions, versions_for},
};

/// Parsed action query from the positional argument.
struct ActionQuery {
    /// Host written before the owner, if any.
    host: Option<String>,
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

/// Parse `[host/]owner/repo` or `[host/]owner/repo@ref` into an [`ActionQuery`].
fn parse_action_query(input: &str) -> Option<ActionQuery> {
    // Split off optional @ref
    let (slug, constraint) = input.split_once('@').map_or((input, None), |(slug, r)| {
        (slug, (!r.is_empty()).then(|| r.to_string()))
    });

    let id = parse_action_path(slug).ok()?;
    // A subpath is not supported for quick lookup.
    if id.subpath().is_some() {
        return None;
    }

    Some(ActionQuery {
        host: id.host().map(str::to_owned),
        owner: id.owner().to_string(),
        repo: id.repo().to_string(),
        constraint,
    })
}

/// Pick the best version to display from a [`version::Set`].
///
/// If a constraint is given (e.g. `v4`), returns the latest within that major.
/// Otherwise returns the global latest. When the best match is a major-only tag,
/// also returns the full tag on that tag's commit.
fn pick_best<'a>(
    versions: &'a version::Set,
    constraint: Option<&str>,
) -> Option<(&'a version::Info, Option<String>)> {
    if let Some(ref_str) = constraint {
        let parsed = version::Version::parse(ref_str);
        if let version::Version::Semver { major, .. } = &parsed {
            // Try major tag first, then latest_per_major
            if let Some(major_tag) = versions.major_tags().get(major) {
                return Some((major_tag, full_tag_on(versions, major_tag)));
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
            return Some((major_tag, full_tag_on(versions, major_tag)));
        }
        Some((latest, None))
    }
}

fn full_tag_on(versions: &version::Set, major_tag: &version::Info) -> Option<String> {
    versions
        .full_tag_on(major_tag.sha())
        .map(|t| t.version().to_string())
}

/// Run the latest-version lookup.
pub async fn run(args: &LatestArgs, config: &Config, registry: &provider::Registry) -> i32 {
    let Some(action_str) = args.action.as_deref() else {
        // No action provided — caller should have printed help already
        return CliExitCode::Success.into();
    };

    let Some(query) = parse_action_query(action_str) else {
        eprintln!(
            "Invalid action reference: {}\n\
             Expected format: owner/repo or owner/repo@ref",
            sanitize_for_terminal(action_str)
        );
        return CliExitCode::UserError.into();
    };

    // Build a synthetic ActionRef for provider lookup
    let ref_str = query.constraint.as_deref().unwrap_or("v0");
    let action_ref = ActionRef::new(query.host.clone(), &query.owner, &query.repo, None, ref_str);

    let Some(provider) = registry.find(action_ref.host()) else {
        let host = action_ref.host().map_or_else(
            || "the default host".to_string(),
            |host| format!("host {host}"),
        );
        eprintln!("No provider configured for {host}");
        return CliExitCode::EnvironmentError.into();
    };

    let repo_id = action_ref.to_repo_id().without_subpath();
    let fetched = batch_fetch_versions(std::slice::from_ref(&repo_id), config, registry).await;
    let versions = match versions_for(&fetched, &repo_id) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", sanitize_for_terminal(&e.to_string()));
            return if e.is_environment_error() {
                CliExitCode::EnvironmentError.into()
            } else {
                CliExitCode::UserError.into()
            };
        }
    };

    let Some((best, exact_version)) = pick_best(versions, query.constraint.as_deref()) else {
        let context = query
            .constraint
            .as_ref()
            .map(|c| format!(" matching {c}"))
            .unwrap_or_default();
        let message = format!(
            "No versions found for {}/{}{context}",
            query.owner, query.repo
        );
        eprintln!("{}", sanitize_for_terminal(&message));
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
                "{}  {}{}",
                sanitize_for_terminal(&format!("{}/{}", query.owner, query.repo)),
                sanitize_for_terminal(&result.latest).green(),
                sanitize_for_terminal(&suffix).dimmed()
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
    fn parse_keeps_a_host_prefix() -> Result<(), Box<dyn std::error::Error>> {
        let q = parse_action_query("GitLab.com/group/project@v1").ok_or("no query")?;
        assert_eq!(
            (q.host.as_deref(), q.owner.as_str(), q.repo.as_str()),
            (Some("gitlab.com"), "group", "project")
        );
        assert_eq!(q.constraint.as_deref(), Some("v1"));
        assert!(parse_action_query("gitlab.com/group/project/sub").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn run_without_a_provider_for_the_host_is_an_environment_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = Config::default();
        let registry = crate::build_registry(&config)?;
        let args = LatestArgs {
            action: Some("git.example.org/group/project".to_string()),
            format: crate::cli::OutputFormat::Json,
        };
        assert_eq!(
            run(&args, &config, &registry).await,
            i32::from(CliExitCode::EnvironmentError)
        );
        Ok(())
    }

    #[cfg(feature = "gitlab")]
    #[tokio::test]
    async fn run_resolves_a_gitlab_ref_through_the_gitlab_provider()
    -> Result<(), Box<dyn std::error::Error>> {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        let project = "/api/v4/projects/group%2Fproject";
        let sha = "b".repeat(40);
        let tags = format!(
            r#"[{{"name": "v1.4.0", "target": "{sha}", "commit": null, "release": null}}]"#
        );
        for (url, body) in [
            (format!("{project}/repository/tags"), tags),
            (
                project.to_string(),
                r#"{"id": 1, "name": "project", "path": "project", "path_with_namespace": "group/project", "default_branch": "main", "description": null, "web_url": "w"}"#.to_string(),
            ),
            (
                format!("{project}/repository/branches/main"),
                format!(r#"{{"name": "main", "commit": {{"id": "{sha}", "short_id": "bbbbbbb", "title": "t"}}, "default": true}}"#),
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(url))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
        }
        let mut config = Config::default();
        config.cache.enabled = false;
        config.hosts.insert(
            "gitlab.com".to_string(),
            actup_core::config::HostConfig {
                kind: None,
                api_base: Some(format!("{}/api/v4", server.uri())),
                token_env: Some("_ACTUP_TEST_UNSET_GITLAB_TOKEN".to_string()),
                api_mode: Some(actup_core::config::ApiMode::Rest),
                max_tags_per_repo: None,
            },
        );
        let registry = crate::build_registry(&config)?;
        let args = LatestArgs {
            action: Some("gitlab.com/group/project".to_string()),
            format: crate::cli::OutputFormat::Json,
        };

        assert_eq!(
            run(&args, &config, &registry).await,
            i32::from(CliExitCode::Success)
        );
        let requests = server.received_requests().await.unwrap_or_default();
        assert!(
            requests
                .iter()
                .any(|r| r.url.path() == format!("{project}/repository/tags")),
            "{requests:?}"
        );
        Ok(())
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
    fn pick_best_names_the_tag_on_a_lagging_major_tag() {
        use std::{collections::HashMap, sync::Arc};

        let older = Arc::new(version::Info::new(version::Version::parse("v6.0.1"), "old"));
        let newer = Arc::new(version::Info::new(version::Version::parse("v6.0.2"), "new"));
        let major = Arc::new(version::Info::new(version::Version::parse("v6"), "old"));
        let set = version::Set::build(
            vec![Arc::clone(&older), Arc::clone(&newer), Arc::clone(&major)],
            HashMap::from([(6, Arc::clone(&newer))]),
            HashMap::from([(6, major)]),
            Some(newer),
        );

        for constraint in [None, Some("v6")] {
            let Some((best, exact)) = pick_best(&set, constraint) else {
                panic!("expected a pick for {constraint:?}");
            };
            assert_eq!(best.version().as_str(), "v6", "{constraint:?}");
            assert_eq!(exact.as_deref(), Some("v6.0.1"), "{constraint:?}");
        }
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
