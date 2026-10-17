//! Batch fetching support for GitHub provider
//!
//! Provides two strategies:
//! - GraphQL: Single batched request for all repos (requires auth)
//! - Parallel REST: Concurrent REST requests (works without auth)

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    num::NonZeroU8,
    sync::Arc,
};

use actup_core::{
    ActionRepoId, Config, Provider,
    cache::{self, Cache},
    effective_host,
    fetch::{Outcome, Progress, Silent},
    provider, version,
};
use futures::StreamExt;
use tracing::{debug, warn};

use crate::{
    GitHubProvider,
    graphql::GraphQLClient,
    provider::{RestListing, RestValidators, Revalidation},
};

/// Maximum concurrent fallback REST requests when registry batching misses.
const FALLBACK_CONCURRENCY_LIMIT: usize = 50;

struct Served<'r> {
    repo: ActionRepoId,
    provider: &'r dyn Provider,
    cache_key: String,
}

struct RegistryFetchPlan<'r> {
    results: HashMap<ActionRepoId, actup_core::Result<version::Set>>,
    github_hosts: HashMap<String, Vec<Served<'r>>>,
    fallback_repos: Vec<(Served<'r>, Option<cache::Entry>)>,
}

enum Cached {
    Fresh(version::Set),
    Fetch(Option<cache::Entry>),
}

async fn read_cached<C: Cache>(cache: &C, repo: &ActionRepoId, key: &str) -> Cached {
    match cache.lookup(key).await {
        Ok(cache::Lookup::Fresh(entry)) => {
            debug!(owner = %repo.owner(), repo = %repo.repo(), "cache hit");
            Cached::Fresh(entry.versions().clone())
        }
        Ok(cache::Lookup::Expired(entry)) => Cached::Fetch(Some(entry)),
        Err(cache::Error::NotFound | cache::Error::Disabled) => Cached::Fetch(None),
        Err(e) => {
            warn!(owner = %repo.owner(), repo = %repo.repo(), error = %e, "cache read error, will fetch");
            Cached::Fetch(None)
        }
    }
}

enum Refresh {
    Listed(RestListing),
    Renewed(cache::Entry),
}

fn stale_or_error(
    repo: &ActionRepoId,
    expired: Option<&cache::Entry>,
    error: actup_core::Error,
) -> actup_core::Result<version::Set> {
    if let Some(versions) = expired.and_then(|entry| entry.stale_versions(&error)) {
        debug!(owner = %repo.owner(), repo = %repo.repo(), error = %actup_core::error::chain(&error), "using expired cache entry");
        return Ok(versions);
    }
    debug!(owner = %repo.owner(), repo = %repo.repo(), error = %actup_core::error::chain(&error), "failed to fetch versions");
    Err(error)
}

fn provider_not_found_error(
    repo: &ActionRepoId,
    registry: &provider::Registry,
) -> actup_core::Error {
    actup_core::Error::ProviderNotFound {
        host: repo
            .host()
            .or_else(|| registry.default_host())
            .unwrap_or("(default)")
            .to_string(),
    }
}

async fn classify_repos_for_registry_fetch<'r, C: Cache>(
    repos: &[ActionRepoId],
    registry: &'r provider::Registry,
    cache: &C,
) -> RegistryFetchPlan<'r> {
    let mut plan = RegistryFetchPlan {
        results: HashMap::with_capacity(repos.len()),
        github_hosts: HashMap::new(),
        fallback_repos: Vec::new(),
    };
    let mut seen = HashSet::new();

    for repo in repos {
        let repo = repo.without_subpath();
        if !seen.insert(repo.clone()) {
            continue;
        }

        let Some(provider) = registry.find_for_repo(&repo) else {
            plan.results
                .insert(repo.clone(), Err(provider_not_found_error(&repo, registry)));
            continue;
        };

        let served = Served {
            cache_key: repo.cache_key(provider.cache_namespace(), provider.host()),
            repo,
            provider,
        };
        if provider.kind() == provider::Kind::GitHub && provider.supports_batch_fetch() {
            let host = served
                .repo
                .host()
                .or_else(|| registry.default_host())
                .unwrap_or(crate::GITHUB_HOST)
                .to_string();
            plan.github_hosts.entry(host).or_default().push(served);
            continue;
        }

        read_into_plan(
            &mut plan.results,
            &mut plan.fallback_repos,
            cache,
            served,
            &Silent,
        )
        .await;
    }

    plan
}

async fn read_into_plan<'r, C: Cache>(
    results: &mut HashMap<ActionRepoId, actup_core::Result<version::Set>>,
    fallback_repos: &mut Vec<(Served<'r>, Option<cache::Entry>)>,
    cache: &C,
    served: Served<'r>,
    progress: &dyn Progress,
) {
    match read_cached(cache, &served.repo, &served.cache_key).await {
        Cached::Fresh(versions) => {
            results.insert(served.repo, Ok(versions));
            progress.finished(Outcome::Cache);
        }
        Cached::Fetch(expired) => fallback_repos.push((served, expired)),
    }
}

async fn fetch_github_host_groups<'r, C: Cache>(
    github_hosts: HashMap<String, Vec<Served<'r>>>,
    config: &Config,
    cache: Arc<C>,
    results: &mut HashMap<ActionRepoId, actup_core::Result<version::Set>>,
    progress: &dyn Progress,
) -> Vec<Served<'r>> {
    let github_registration = [&crate::REGISTRATION];
    let mut fallback_repos = Vec::new();

    for (host, host_repos) in github_hosts {
        match effective_host(&host, config, &github_registration, &config.fetch) {
            Ok(provider_config) => {
                match crate::create_batch_fetcher(&provider_config, Arc::clone(&cache)) {
                    Ok(fetcher) => {
                        let repos: Vec<ActionRepoId> =
                            host_repos.into_iter().map(|served| served.repo).collect();
                        results.extend(fetcher.fetch_versions(&repos, progress).await);
                    }
                    Err(e) => {
                        debug!(host = %host, error = %actup_core::error::chain(&e), "failed to build GitHub batch fetcher, falling back to provider fetches");
                        fallback_repos.extend(host_repos);
                    }
                }
            }
            Err(e) => {
                debug!(host = %host, error = %actup_core::error::chain(&e), "failed to resolve GitHub host config, falling back to provider fetches");
                fallback_repos.extend(host_repos);
            }
        }
    }

    fallback_repos
}

async fn fetch_served(
    (served, expired): (Served<'_>, Option<cache::Entry>),
) -> (
    Served<'_>,
    Option<cache::Entry>,
    actup_core::Result<version::Set>,
) {
    let result = served.provider.get_versions(&served.repo).await;
    (served, expired, result)
}

async fn fetch_fallback_repos<C: Cache>(
    fallback_repos: Vec<(Served<'_>, Option<cache::Entry>)>,
    cache: &C,
    results: &mut HashMap<ActionRepoId, actup_core::Result<version::Set>>,
    progress: &dyn Progress,
) {
    let fetched: Vec<_> = futures::stream::iter(fallback_repos)
        .map(fetch_served)
        .buffer_unordered(FALLBACK_CONCURRENCY_LIMIT)
        .inspect(|(_, _, result)| {
            progress.finished(if result.is_ok() {
                Outcome::Provider
            } else {
                Outcome::Failed
            });
        })
        .collect()
        .await;

    for (served, expired, result) in fetched {
        let Served {
            repo, cache_key, ..
        } = served;
        match result {
            Ok(versions) => {
                if versions.stale().is_none()
                    && let Err(e) = cache
                        .set(&cache_key, cache::Entry::new(versions.clone()))
                        .await
                {
                    debug!(owner = %repo.owner(), repo = %repo.repo(), error = %e, "failed to cache versions");
                }
                results.insert(repo, Ok(versions));
            }
            Err(e) => {
                let result = stale_or_error(&repo, expired.as_ref(), e);
                results.insert(repo, result);
            }
        }
    }
}

/// Fetch versions for repo ids using GitHub batching when available, reporting each repo to `progress`.
///
/// GitHub repos are grouped by host, resolved through config, and fetched via
/// [`BatchFetcher`], which returns each repo's version set or its error.
/// Repos on a host whose batch fetcher cannot be built, and repos of other
/// providers, fall back to their registered provider with shared cache
/// reads and writes.
pub async fn fetch_versions_with_registry<C: Cache>(
    repos: &[ActionRepoId],
    config: &Config,
    registry: &provider::Registry,
    cache: Arc<C>,
    progress: &dyn Progress,
) -> HashMap<ActionRepoId, actup_core::Result<version::Set>> {
    let RegistryFetchPlan {
        mut results,
        github_hosts,
        mut fallback_repos,
    } = classify_repos_for_registry_fetch(repos, registry, cache.as_ref()).await;

    progress.start(
        results.len() + github_hosts.values().map(Vec::len).sum::<usize>() + fallback_repos.len(),
    );
    for result in results.values() {
        progress.finished(if result.is_ok() {
            Outcome::Cache
        } else {
            Outcome::Failed
        });
    }

    for served in fetch_github_host_groups(
        github_hosts,
        config,
        Arc::clone(&cache),
        &mut results,
        progress,
    )
    .await
    {
        read_into_plan(
            &mut results,
            &mut fallback_repos,
            cache.as_ref(),
            served,
            progress,
        )
        .await;
    }

    fetch_fallback_repos(fallback_repos, cache.as_ref(), &mut results, progress).await;

    results
}

/// Batch fetcher for GitHub repositories
pub struct BatchFetcher<C: Cache> {
    /// REST provider for individual requests
    provider: GitHubProvider,
    /// Optional GraphQL client (requires auth token)
    graphql: Option<GraphQLClient>,
    /// Persistent cache
    cache: Arc<C>,
    /// Maximum tags to fetch per repo
    max_tags: NonZeroU8,
}

fn finish(progress: &dyn Progress, ok: bool, outcome: Outcome) {
    progress.finished(if ok { outcome } else { Outcome::Failed });
}

impl<C: Cache> BatchFetcher<C> {
    /// Create a new batch fetcher with REST-only mode
    pub const fn rest_only(provider: GitHubProvider, cache: Arc<C>, max_tags: NonZeroU8) -> Self {
        Self {
            provider,
            graphql: None,
            cache,
            max_tags,
        }
    }

    /// Create a new batch fetcher with GraphQL support
    ///
    /// Falls back to parallel REST if GraphQL client creation fails.
    pub fn with_graphql(
        provider: GitHubProvider,
        cache: Arc<C>,
        token: Option<&str>,
        max_tags: NonZeroU8,
    ) -> Self {
        let graphql_endpoint = provider.graphql_endpoint();
        let graphql = token.and_then(|t| {
            GraphQLClient::with_endpoint(&graphql_endpoint, t)
                .inspect_err(|e| warn!(error = %actup_core::error::chain(e), "failed to create GraphQL client, using REST"))
                .ok()
        });

        Self {
            provider,
            graphql,
            cache,
            max_tags,
        }
    }

    /// Fetch versions for multiple repositories
    ///
    /// Strategy:
    /// 1. Check cache for each repo
    /// 2. List refs over the git protocol
    /// 3. For repos whose git listing hit an environment error, query the API
    ///    (GraphQL if available, else parallel REST; REST also covers repos
    ///    that GraphQL leaves out). REST sends the `ETag`s of an expired
    ///    entry, and an answer of 304 to all three calls renews that entry.
    /// 4. Store successful results in cache
    ///
    /// Returns every input repo's version set or its fetch error. A rate
    /// limit or network failure with an expired entry returns that entry's
    /// versions marked stale. Reports each repo to `progress`.
    pub async fn fetch_versions(
        &self,
        repos: &[ActionRepoId],
        progress: &dyn Progress,
    ) -> HashMap<ActionRepoId, actup_core::Result<version::Set>> {
        let mut results = HashMap::new();
        let mut uncached = Vec::new();
        let mut expired = HashMap::new();

        let unique_repos: HashSet<_> = repos.iter().cloned().collect();

        for repo in &unique_repos {
            let cache_key = repo.cache_key("github", self.provider.host());

            match read_cached(self.cache.as_ref(), repo, &cache_key).await {
                Cached::Fresh(versions) => {
                    results.insert(repo.clone(), Ok(versions));
                    progress.finished(Outcome::Cache);
                }
                Cached::Fetch(entry) => {
                    if let Some(entry) = entry {
                        expired.insert(repo.clone(), entry);
                    }
                    uncached.push(repo.clone());
                }
            }
        }

        if uncached.is_empty() {
            debug!("all repos found in cache");
            return results;
        }

        debug!(count = uncached.len(), "fetching uncached repos");

        let listed = |versions| {
            Refresh::Listed(RestListing {
                versions,
                validators: BTreeMap::new(),
            })
        };
        let mut fetched = HashMap::new();
        let mut unlisted = Vec::new();
        for (repo, result) in self.fetch_parallel_git(&uncached).await {
            match result {
                Err(e) if e.is_environment_error() => {
                    debug!(owner = %repo.owner(), repo = %repo.repo(), error = %actup_core::error::chain(&e), "git protocol unavailable, using the API");
                    unlisted.push(repo);
                }
                result => {
                    finish(progress, result.is_ok(), Outcome::Git);
                    fetched.insert(repo, result.map(listed));
                }
            }
        }

        if !unlisted.is_empty() {
            let mut via_api: HashMap<_, _> = match &self.graphql {
                Some(graphql) => self
                    .fetch_graphql(graphql, &unlisted)
                    .await
                    .into_iter()
                    .map(|(repo, versions)| (repo, Ok(listed(versions))))
                    .collect(),
                None => HashMap::new(),
            };
            let missing: Vec<_> = unlisted
                .into_iter()
                .filter(|repo| !via_api.contains_key(repo))
                .collect();
            via_api.extend(self.fetch_parallel_rest(&missing, &expired).await);
            for result in via_api.values() {
                finish(progress, result.is_ok(), Outcome::Api);
            }
            fetched.extend(via_api);
        }

        for (repo, result) in fetched {
            let expired = expired.remove(&repo);
            let versions = self.settle(&repo, result, expired).await;
            results.insert(repo, versions);
        }

        results
    }

    async fn settle(
        &self,
        repo: &ActionRepoId,
        result: actup_core::Result<Refresh>,
        expired: Option<cache::Entry>,
    ) -> actup_core::Result<version::Set> {
        let entry = match result {
            Ok(Refresh::Listed(listing)) => {
                cache::Entry::new(listing.versions).with_validators(listing.validators)
            }
            Ok(Refresh::Renewed(entry)) => entry,
            Err(e) => return stale_or_error(repo, expired.as_ref(), e),
        };
        let cache_key = repo.cache_key("github", self.provider.host());
        let versions = entry.versions().clone();
        if let Err(e) = self.cache.set(&cache_key, entry).await {
            warn!(
                owner = %repo.owner(),
                repo = %repo.repo(),
                error = %e,
                "failed to cache versions"
            );
        }
        Ok(versions)
    }

    /// Fetch using GraphQL batch query
    async fn fetch_graphql(
        &self,
        client: &GraphQLClient,
        repos: &[ActionRepoId],
    ) -> HashMap<ActionRepoId, version::Set> {
        debug!(count = repos.len(), "using GraphQL batch fetch");

        client
            .batch_get_versions(repos, self.max_tags)
            .await
            .unwrap_or_else(|e| {
                debug!(error = %actup_core::error::chain(&e), "GraphQL batch fetch failed, using REST");
                HashMap::new()
            })
    }

    /// Fetch using parallel git ls-refs with bounded concurrency
    async fn fetch_parallel_git(
        &self,
        repos: &[ActionRepoId],
    ) -> Vec<(ActionRepoId, actup_core::Result<version::Set>)> {
        debug!(count = repos.len(), "using parallel git ls-refs fetch");

        futures::stream::iter(repos.iter().cloned())
            .map(|repo| async move {
                let result = self.provider.versions_via_git(&repo).await;
                (repo, result)
            })
            .buffer_unordered(FALLBACK_CONCURRENCY_LIMIT)
            .collect()
            .await
    }

    async fn fetch_parallel_rest(
        &self,
        repos: &[ActionRepoId],
        expired: &HashMap<ActionRepoId, cache::Entry>,
    ) -> HashMap<ActionRepoId, actup_core::Result<Refresh>> {
        debug!(count = repos.len(), "using parallel REST fetch");

        futures::stream::iter(repos.iter().cloned())
            .map(|repo| async move {
                let stored = expired.get(&repo).and_then(|entry| {
                    RestValidators::from_entry(entry.validators(), entry.versions())
                        .map(|since| (entry, since))
                });
                let result = match stored {
                    Some((entry, since)) => self
                        .provider
                        .revalidate_via_rest(&repo, &since)
                        .await
                        .map(|answer| match answer {
                            Revalidation::NotModified => Refresh::Renewed(entry.clone().renewed()),
                            Revalidation::Modified(listing) => Refresh::Listed(*listing),
                        }),
                    None => self
                        .provider
                        .list_via_rest(&repo)
                        .await
                        .map(Refresh::Listed),
                };
                (repo, result)
            })
            .buffer_unordered(FALLBACK_CONCURRENCY_LIMIT)
            .collect()
            .await
    }

    /// Get access to the underlying provider for single-repo operations
    #[must_use]
    pub const fn provider(&self) -> &GitHubProvider {
        &self.provider
    }

    /// Check if GraphQL is available
    #[must_use]
    pub const fn has_graphql(&self) -> bool {
        self.graphql.is_some()
    }
}

#[cfg(test)]
mod tests {
    use actup_core::config::DEFAULT_MAX_TAGS_PER_REPO;

    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn test_rest_only_has_no_graphql() -> TestResult {
        let provider = GitHubProvider::new(None)?;
        let cache = Arc::new(cache::Noop);
        let fetcher = BatchFetcher::rest_only(provider, cache, DEFAULT_MAX_TAGS_PER_REPO);

        assert!(!fetcher.has_graphql());
        Ok(())
    }

    #[test]
    fn test_with_graphql_no_token() -> TestResult {
        let provider = GitHubProvider::new(None)?;
        let cache = Arc::new(cache::Noop);
        let fetcher = BatchFetcher::with_graphql(provider, cache, None, DEFAULT_MAX_TAGS_PER_REPO);

        // No token = no GraphQL
        assert!(!fetcher.has_graphql());
        Ok(())
    }
}
