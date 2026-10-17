//! Read tools: README, action metadata, versions and the latest allowed version.

use std::sync::Arc;

use actup_core::{
    ActionRef, ActionRepoId, CheckStatus, Config, Info, Provider, RefKind,
    cache::Staleness,
    config::{Override, PolicyOverride},
    provider::Registry,
    version,
};
use rust_mcp_sdk::schema::schema_utils::CallToolError;
use serde::Serialize;

use crate::{
    args::{self, check_ref, repo_id},
    report::{Reply, Served, Status},
};

struct Lookup<'a> {
    repo: ActionRepoId,
    provider: &'a dyn Provider,
    served: Served,
}

fn lookup(registry: &Registry, repo: ActionRepoId) -> Result<Lookup<'_>, Reply> {
    if let Some(provider) = registry.find(repo.host()) {
        let served = Served::by(provider);
        return Ok(Lookup {
            repo,
            provider,
            served,
        });
    }
    let host = unserved_host(registry, &repo);
    let served = Served::unserved(&host);
    Err(Reply::failure(
        &served,
        &actup_core::Error::ProviderNotFound { host },
    ))
}

fn unserved_host(registry: &Registry, repo: &ActionRepoId) -> String {
    repo.host()
        .or_else(|| registry.default_host())
        .unwrap_or_default()
        .to_string()
}

fn invalid(tool: &str, message: String) -> CallToolError {
    CallToolError::invalid_arguments(tool, Some(message))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadmeBody<'a> {
    owner: &'a str,
    repo: &'a str,
    #[serde(rename = "ref")]
    reference: Option<&'a str>,
    readme: Option<String>,
}

/// `actionGetReadme`: the README at a ref, `null` when the repository has none.
///
/// # Errors
///
/// Returns an invalid-arguments error for a malformed repository or ref.
pub async fn readme(
    tool: &str,
    registry: &Registry,
    args: &args::Readme,
) -> Result<Reply, CallToolError> {
    let repo = repo_id(args.host.as_deref(), &args.owner, &args.repo, None)
        .map_err(|e| invalid(tool, e))?;
    if let Some(reference) = &args.reference {
        check_ref("ref", reference).map_err(|e| invalid(tool, e))?;
    }
    let found = match lookup(registry, repo) {
        Ok(found) => found,
        Err(reply) => return Ok(reply),
    };
    match found
        .provider
        .get_readme(&found.repo, args.reference.as_deref())
        .await
    {
        Ok(readme) => Reply::new(
            Some(&found.served),
            Status::UpToDate,
            ReadmeBody {
                owner: &args.owner,
                repo: &args.repo,
                reference: args.reference.as_deref(),
                readme,
            },
        ),
        Err(e) => Ok(Reply::failure(&found.served, &e)),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ActionYmlBody<'a> {
    owner: &'a str,
    repo: &'a str,
    #[serde(rename = "ref")]
    reference: &'a str,
    path: Option<&'a str>,
    action_yml: actup_core::provider::ActionYml,
}

/// `actionGetActionYml`: the raw and parsed `action.yml` at a ref.
///
/// # Errors
///
/// Returns an invalid-arguments error for a malformed repository, path or ref.
pub async fn action_yml(
    tool: &str,
    registry: &Registry,
    args: &args::ActionYml,
) -> Result<Reply, CallToolError> {
    let repo = repo_id(
        args.host.as_deref(),
        &args.owner,
        &args.repo,
        args.path.as_deref(),
    )
    .map_err(|e| invalid(tool, e))?;
    if let Some(reference) = &args.reference {
        check_ref("ref", reference).map_err(|e| invalid(tool, e))?;
    }
    let found = match lookup(registry, repo) {
        Ok(found) => found,
        Err(reply) => return Ok(reply),
    };
    let reference = match &args.reference {
        Some(reference) => reference.clone(),
        None => match found.provider.get_default_branch(&found.repo).await {
            Ok(branch) => branch,
            Err(e) => return Ok(Reply::failure(&found.served, &e)),
        },
    };
    match found.provider.get_action_yml(&found.repo, &reference).await {
        Ok(action_yml) => Reply::new(
            Some(&found.served),
            Status::UpToDate,
            ActionYmlBody {
                owner: &args.owner,
                repo: &args.repo,
                reference: &reference,
                path: args.path.as_deref(),
                action_yml,
            },
        ),
        Err(e) => Ok(Reply::failure(&found.served, &e)),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Branch<'a> {
    name: &'a str,
    sha: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Branches<'a> {
    default_branch: Option<&'a version::BranchInfo>,
    branches: Vec<Branch<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionsBody<'a> {
    owner: &'a str,
    repo: &'a str,
    tags: Vec<&'a Info>,
    latest: Option<&'a Info>,
    #[serde(flatten)]
    branches: Option<Branches<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stale: Option<Staleness>,
}

/// `getVersions`: the semver tags, the latest of them and optionally the branches.
///
/// # Errors
///
/// Returns an invalid-arguments error for a malformed repository.
pub async fn versions(
    tool: &str,
    registry: &Registry,
    args: &args::Versions,
) -> Result<Reply, CallToolError> {
    let repo = repo_id(args.host.as_deref(), &args.owner, &args.repo, None)
        .map_err(|e| invalid(tool, e))?;
    let found = match lookup(registry, repo) {
        Ok(found) => found,
        Err(reply) => return Ok(reply),
    };
    let set = match found.provider.get_versions(&found.repo).await {
        Ok(set) => set,
        Err(e) => return Ok(Reply::failure(&found.served, &e)),
    };
    let tags: Vec<&Info> = set
        .semver_tags()
        .iter()
        .map(Arc::as_ref)
        .filter(|t| args.include_prerelease || !t.version().is_prerelease())
        .collect();
    let latest = tags
        .iter()
        .copied()
        .max_by(|a, b| a.version().cmp(b.version()));
    let branches = args.include_branches.then(|| {
        let mut branches: Vec<Branch<'_>> = set
            .branches()
            .iter()
            .map(|(name, sha)| Branch { name, sha })
            .collect();
        branches.sort_by(|a, b| a.name.cmp(b.name));
        Branches {
            default_branch: set.default_branch(),
            branches,
        }
    });
    Reply::new(
        Some(&found.served),
        Status::UpToDate,
        VersionsBody {
            owner: &args.owner,
            repo: &args.repo,
            latest,
            tags,
            branches,
            stale: set.stale(),
        },
    )
    .map(|reply| reply.stale_if(set.stale().is_some()))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LatestBody<'a> {
    owner: &'a str,
    repo: &'a str,
    current_ref: &'a str,
    check: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<Info>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ref_kind: Option<RefKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    same_commit: Option<String>,
    latest: Option<&'a Info>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stale: Option<Staleness>,
}

/// `getLatest`: the version that policy allows `currentRef` to move to.
///
/// # Errors
///
/// Returns an invalid-arguments error for a malformed repository or ref.
pub async fn latest(
    tool: &str,
    registry: &Registry,
    config: &Config,
    args: &args::Latest,
) -> Result<Reply, CallToolError> {
    let repo = repo_id(args.host.as_deref(), &args.owner, &args.repo, None)
        .map_err(|e| invalid(tool, e))?;
    check_ref("currentRef", &args.current_ref).map_err(|e| invalid(tool, e))?;
    let current = ActionRef::new(
        repo.host().map(str::to_owned),
        repo.owner(),
        repo.repo(),
        None,
        args.current_ref.as_str(),
    );
    if config.should_ignore(&current) {
        let served = registry.find(repo.host()).map_or_else(
            || Served::unserved(&unserved_host(registry, &repo)),
            Served::by,
        );
        return Reply::new(
            Some(&served),
            Status::of_check(&CheckStatus::Ignored),
            LatestBody {
                owner: &args.owner,
                repo: &args.repo,
                current_ref: &args.current_ref,
                check: describe(&CheckStatus::Ignored).0,
                target: None,
                reason: None,
                ref_kind: None,
                same_commit: None,
                latest: None,
                stale: None,
            },
        );
    }
    let found = match lookup(registry, repo) {
        Ok(found) => found,
        Err(reply) => return Ok(reply),
    };
    let set = match found.provider.get_versions(&found.repo).await {
        Ok(set) => set,
        Err(e) => return Ok(Reply::failure(&found.served, &e)),
    };
    let action = ActionRef::new(
        found.repo.host().map(str::to_owned),
        found.repo.owner(),
        found.repo.repo(),
        None,
        args.current_ref.as_str(),
    );
    let config = with_bump(config, args.policy_override);
    let check =
        actup_core::check_action_with_provider(&action, &set, &config, found.provider).await;
    let (name, target, reason) = describe(check.status());
    Reply::new(
        Some(&found.served),
        Status::of_check(check.status()),
        LatestBody {
            owner: &args.owner,
            repo: &args.repo,
            current_ref: &args.current_ref,
            check: name,
            target,
            reason,
            ref_kind: check.ref_kind(),
            same_commit: check.same_commit().map(|t| t.version().to_string()),
            latest: set.latest().map(Arc::as_ref),
            stale: set.stale(),
        },
    )
    .map(|reply| reply.stale_if(set.stale().is_some()))
}

fn with_bump(config: &Config, bump: Option<actup_core::policy::Update>) -> Config {
    let mut config = config.clone();
    if let Some(bump) = bump
        && let Ok(pattern) = "*/*".parse()
    {
        config.overrides.push(Override {
            pattern,
            policy: PolicyOverride {
                bump: Some(bump),
                ..PolicyOverride::default()
            },
        });
    }
    config
}

fn describe(status: &CheckStatus) -> (&'static str, Option<Info>, Option<&str>) {
    match status {
        CheckStatus::UpToDate => ("upToDate", None, None),
        CheckStatus::Outdated { target, .. } => ("outdated", Some(target.as_ref().clone()), None),
        CheckStatus::Pinned { .. } => ("pinned", None, None),
        CheckStatus::Ignored => ("ignored", None, None),
        CheckStatus::Floating { .. } => ("floating", None, None),
        CheckStatus::PinBranch { ref_name, sha } => (
            "pinBranch",
            Some(Info::new(version::Version::parse(ref_name), sha.as_str())),
            None,
        ),
        CheckStatus::Unresolvable { reason, .. } => ("unresolvable", None, Some(reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latest_args(current_ref: &str) -> args::Latest {
        args::Latest {
            host: Some("ghe.corp.example".to_owned()),
            owner: "o".to_owned(),
            repo: "r".to_owned(),
            current_ref: current_ref.to_owned(),
            policy_override: None,
        }
    }

    #[tokio::test]
    async fn latest_reports_an_ignored_ref_on_an_unserved_host()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = Config::from_contents(
            Some((
                std::path::Path::new("actup.toml"),
                "[scan]\nignore = [\"ghe.corp.example/o/r@v1\"]\n",
            )),
            actup_core::config::EnvOverrides::default(),
        )?;
        let registry = Registry::new();

        let reply = latest("getLatest", &registry, &config, &latest_args("v1"))
            .await
            .map_err(|e| format!("{e:?}"))?;
        assert!(reply.succeeded());
        let body = reply
            .into_result()
            .structured_content
            .ok_or("no structured content")?;
        for (key, value) in [
            ("check", "ignored"),
            ("status", "upToDate"),
            ("provider", "unknown"),
            ("host", "ghe.corp.example"),
        ] {
            assert_eq!(body.get(key), Some(&serde_json::json!(value)), "{key}");
        }

        let reply = latest("getLatest", &registry, &config, &latest_args("v2"))
            .await
            .map_err(|e| format!("{e:?}"))?;
        assert!(!reply.succeeded());
        Ok(())
    }

    #[test]
    fn with_bump_overrides_every_action_last() {
        let action = ActionRef::simple("actions", "checkout", "v4");
        let config = Config::default();
        assert_eq!(
            with_bump(&config, Some(actup_core::policy::Update::Patch))
                .policy_for(&action)
                .bump,
            actup_core::policy::Update::Patch
        );
        assert_eq!(
            with_bump(&config, None).policy_for(&action),
            config.policy_for(&action)
        );
    }
}
