//! Batch fetching support for GitHub provider
//!
//! Provides two strategies:
//! - GraphQL: Single batched request for all repos (requires auth)
//! - Parallel REST: Concurrent REST requests (works without auth)

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use actup_core::{
    ActionRepoId, Config, Provider,
    cache::{self, Cache},
    effective_host, provider, version,
};
use futures::StreamExt;
use tracing::{debug, warn};

use crate::{GitHubProvider, graphql::GraphQLClient};

/// Maximum concurrent fallback REST requests when registry batching misses.
const FALLBACK_CONCURRENCY_LIMIT: usize = 50;

struct RegistryFetchPlan {
    results: HashMap<ActionRepoId, actup_core::Result<version::Set>>,
    github_hosts: HashMap<String, Vec<ActionRepoId>>,
    fallback_repos: Vec<ActionRepoId>,
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

async fn classify_repos_for_registry_fetch<C: Cache>(
    repos: &[ActionRepoId],
    registry: &provider::Registry,
    cache: &C,
) -> RegistryFetchPlan {
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

        if provider.kind() == provider::Kind::GitHub && provider.supports_batch_fetch() {
            let host = repo
                .host()
                .or_else(|| registry.default_host())
                .unwrap_or(crate::GITHUB_HOST)
                .to_string();
            plan.github_hosts.entry(host).or_default().push(repo);
            continue;
        }

        let cache_key = repo.cache_key(provider.cache_namespace(), provider.host());
        match cache.get(&cache_key).await {
            Ok(entry) => {
                debug!(owner = %repo.owner(), repo = %repo.repo(), "cache hit");
                plan.results.insert(repo, Ok(entry.versions().clone()));
            }
            Err(cache::Error::NotFound | cache::Error::Disabled) => {
                plan.fallback_repos.push(repo);
            }
            Err(e) => {
                debug!(owner = %repo.owner(), repo = %repo.repo(), error = %e, "cache read error, will fetch");
                plan.fallback_repos.push(repo);
            }
        }
    }

    plan
}

async fn fetch_github_host_groups<C: Cache>(
    github_hosts: HashMap<String, Vec<ActionRepoId>>,
    config: &Config,
    cache: Arc<C>,
    results: &mut HashMap<ActionRepoId, actup_core::Result<version::Set>>,
) -> Vec<ActionRepoId> {
    let github_registration = [&crate::REGISTRATION];
    let mut fallback_repos = Vec::new();

    for (host, host_repos) in github_hosts {
        match effective_host(&host, config, &github_registration, &config.fetch) {
            Ok(provider_config) => {
                match crate::create_batch_fetcher(&provider_config, Arc::clone(&cache)) {
                    Ok(fetcher) => {
                        results.extend(fetcher.fetch_versions(&host_repos).await);
                    }
                    Err(e) => {
                        debug!(host = %host, error = %e, "failed to build GitHub batch fetcher, falling back to provider fetches");
                        fallback_repos.extend(host_repos);
                    }
                }
            }
            Err(e) => {
                debug!(host = %host, error = %e, "failed to resolve GitHub host config, falling back to provider fetches");
                fallback_repos.extend(host_repos);
            }
        }
    }

    fallback_repos
}

async fn fetch_fallback_repos<C: Cache>(
    fallback_repos: Vec<ActionRepoId>,
    registry: &provider::Registry,
    cache: &C,
    results: &mut HashMap<ActionRepoId, actup_core::Result<version::Set>>,
) {
    let fetched: Vec<_> = futures::stream::iter(fallback_repos)
        .map(|repo| async move {
            let provider = registry.find_for_repo(&repo)?;
            let cache_ns = provider.cache_namespace().to_string();
            let host = provider.host().to_string();
            let result = provider.get_versions(&repo).await;
            Some((repo, cache_ns, host, result))
        })
        .buffer_unordered(FALLBACK_CONCURRENCY_LIMIT)
        .filter_map(futures::future::ready)
        .collect()
        .await;

    for (repo, cache_ns, host, result) in fetched {
        match result {
            Ok(versions) => {
                let cache_key = repo.cache_key(&cache_ns, &host);
                let entry = cache::Entry::new(versions.clone());
                if let Err(e) = cache.set(&cache_key, entry).await {
                    debug!(owner = %repo.owner(), repo = %repo.repo(), error = %e, "failed to cache versions");
                }
                results.insert(repo, Ok(versions));
            }
            Err(e) => {
                debug!(owner = %repo.owner(), repo = %repo.repo(), error = %e, "failed to fetch versions");
                results.insert(repo, Err(e));
            }
        }
    }
}

/// Fetch versions for repo ids using GitHub batching when available.
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
) -> HashMap<ActionRepoId, actup_core::Result<version::Set>> {
    let RegistryFetchPlan {
        mut results,
        github_hosts,
        mut fallback_repos,
    } = classify_repos_for_registry_fetch(repos, registry, cache.as_ref()).await;

    fallback_repos.extend(
        fetch_github_host_groups(github_hosts, config, Arc::clone(&cache), &mut results).await,
    );

    fetch_fallback_repos(fallback_repos, registry, cache.as_ref(), &mut results).await;

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
    max_tags: u8,
}

impl<C: Cache> BatchFetcher<C> {
    /// Create a new batch fetcher with REST-only mode
    pub const fn rest_only(provider: GitHubProvider, cache: Arc<C>, max_tags: u8) -> Self {
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
        max_tags: u8,
    ) -> Self {
        let graphql_endpoint = provider.graphql_endpoint();
        let graphql = token.and_then(|t| {
            GraphQLClient::with_endpoint(&graphql_endpoint, t)
                .inspect_err(|e| warn!(error = %e, "failed to create GraphQL client, using REST"))
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
    ///    that GraphQL leaves out)
    /// 4. Store successful results in cache
    ///
    /// Returns every input repo's version set or its fetch error.
    pub async fn fetch_versions(
        &self,
        repos: &[ActionRepoId],
    ) -> HashMap<ActionRepoId, actup_core::Result<version::Set>> {
        let mut results = HashMap::new();
        let mut uncached = Vec::new();

        let unique_repos: HashSet<_> = repos.iter().cloned().collect();

        for repo in &unique_repos {
            let cache_key = repo.cache_key("github", self.provider.host());

            match self.cache.get(&cache_key).await {
                Ok(entry) => {
                    debug!(
                        owner = %repo.owner(),
                        repo = %repo.repo(),
                        "cache hit"
                    );
                    results.insert(repo.clone(), Ok(entry.versions().clone()));
                }
                Err(cache::Error::NotFound | cache::Error::Disabled) => {
                    uncached.push(repo.clone());
                }
                Err(e) => {
                    warn!(
                        owner = %repo.owner(),
                        repo = %repo.repo(),
                        error = %e,
                        "cache read error"
                    );
                    uncached.push(repo.clone());
                }
            }
        }

        if uncached.is_empty() {
            debug!("all repos found in cache");
            return results;
        }

        debug!(count = uncached.len(), "fetching uncached repos");

        let mut fetched = HashMap::new();
        let mut unlisted = Vec::new();
        for (repo, result) in self.fetch_parallel_git(&uncached).await {
            match result {
                Err(e) if e.is_environment_error() => {
                    debug!(owner = %repo.owner(), repo = %repo.repo(), error = %e, "git protocol unavailable, using the API");
                    unlisted.push(repo);
                }
                result => {
                    fetched.insert(repo, result);
                }
            }
        }

        if !unlisted.is_empty() {
            let mut via_api: HashMap<_, _> = match &self.graphql {
                Some(graphql) => self
                    .fetch_graphql(graphql, &unlisted)
                    .await
                    .into_iter()
                    .map(|(repo, versions)| (repo, Ok(versions)))
                    .collect(),
                None => HashMap::new(),
            };
            let missing: Vec<_> = unlisted
                .into_iter()
                .filter(|repo| !via_api.contains_key(repo))
                .collect();
            via_api.extend(self.fetch_parallel_rest(&missing).await);
            fetched.extend(via_api);
        }

        for (repo, result) in fetched {
            match &result {
                Ok(versions) => {
                    let cache_key = repo.cache_key("github", self.provider.host());
                    if let Err(e) = self
                        .cache
                        .set(&cache_key, cache::Entry::new(versions.clone()))
                        .await
                    {
                        warn!(
                            owner = %repo.owner(),
                            repo = %repo.repo(),
                            error = %e,
                            "failed to cache versions"
                        );
                    }
                }
                Err(e) => {
                    debug!(owner = %repo.owner(), repo = %repo.repo(), error = %e, "failed to fetch versions");
                }
            }
            results.insert(repo, result);
        }

        results
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
                debug!(error = %e, "GraphQL batch fetch failed, using REST");
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

    /// Fetch using parallel REST requests with bounded concurrency
    async fn fetch_parallel_rest(
        &self,
        repos: &[ActionRepoId],
    ) -> HashMap<ActionRepoId, actup_core::Result<version::Set>> {
        debug!(count = repos.len(), "using parallel REST fetch");

        futures::stream::iter(repos.iter().cloned())
            .map(|repo| async move {
                let result = self.provider.versions_via_rest(&repo).await;
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
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[allow(dead_code, reason = "helper function for future tests")]
    fn make_repo(owner: &str, repo: &str) -> ActionRepoId {
        ActionRepoId::simple(owner, repo)
    }

    #[test]
    fn test_rest_only_has_no_graphql() {
        let provider = GitHubProvider::new(None).unwrap();
        let cache = Arc::new(cache::Noop);
        let fetcher = BatchFetcher::rest_only(provider, cache, 10);

        assert!(!fetcher.has_graphql());
    }

    #[test]
    fn test_with_graphql_no_token() {
        let provider = GitHubProvider::new(None).unwrap();
        let cache = Arc::new(cache::Noop);
        let fetcher = BatchFetcher::with_graphql(provider, cache, None, 10);

        // No token = no GraphQL
        assert!(!fetcher.has_graphql());
    }
}
