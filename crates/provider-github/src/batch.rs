//! Batch fetching support for GitHub provider
//!
//! Provides two strategies:
//! - GraphQL: Single batched request for all repos (requires auth)
//! - Parallel REST: Concurrent REST requests (works without auth)

use crate::GitHubProvider;
use crate::graphql::GraphQLClient;
use actup_core::cache::{self, Cache};
use actup_core::version;
use actup_core::{ActionRepoId, Provider};
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, warn};

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
        let graphql = token.and_then(|t| {
            GraphQLClient::new(t)
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
    /// 2. Batch fetch missing repos (GraphQL if available, else parallel REST)
    /// 3. Store results in cache
    ///
    /// Returns map of repo -> version set (errors logged but don't fail batch)
    pub async fn fetch_versions(
        &self,
        repos: &[ActionRepoId],
    ) -> HashMap<ActionRepoId, version::Set> {
        let mut results = HashMap::new();
        let mut uncached = Vec::new();

        // Deduplicate input repos to avoid repeated cache reads and duplicate fetches
        let unique_repos: HashSet<_> = repos.iter().cloned().collect();

        // 1. Check cache for each unique repo
        for repo in &unique_repos {
            let cache_key = repo.cache_key("github", self.provider.host());

            match self.cache.get(&cache_key).await {
                Ok(entry) => {
                    debug!(
                        owner = %repo.owner(),
                        repo = %repo.repo(),
                        "cache hit"
                    );
                    results.insert(repo.clone(), entry.versions().clone());
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

        // 2. Batch fetch missing repos
        let fetched = if let Some(graphql) = &self.graphql {
            self.fetch_graphql(graphql, &uncached).await
        } else {
            self.fetch_parallel_rest(&uncached).await
        };

        // 3. Store results in cache and collect
        for (repo, versions) in fetched {
            let cache_key = repo.cache_key("github", self.provider.host());
            let entry = cache::Entry::new(versions.clone());

            if let Err(e) = self.cache.set(&cache_key, entry).await {
                warn!(
                    owner = %repo.owner(),
                    repo = %repo.repo(),
                    error = %e,
                    "failed to cache versions"
                );
            }

            results.insert(repo, versions);
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

        match client.batch_get_versions(repos, self.max_tags).await {
            Ok(results) => results,
            Err(e) => {
                warn!(error = %e, "GraphQL batch fetch failed, falling back to REST");
                self.fetch_parallel_rest(repos).await
            }
        }
    }

    /// Fetch using parallel REST requests with bounded concurrency
    async fn fetch_parallel_rest(
        &self,
        repos: &[ActionRepoId],
    ) -> HashMap<ActionRepoId, version::Set> {
        use actup_core::Provider;

        /// Maximum concurrent REST requests to avoid overwhelming the API
        const CONCURRENCY_LIMIT: usize = 50;

        debug!(count = repos.len(), "using parallel REST fetch");

        let results: Vec<_> = futures::stream::iter(repos.iter().cloned())
            .map(|repo| async move {
                let result = self.provider.get_versions(&repo).await;
                (repo, result)
            })
            .buffer_unordered(CONCURRENCY_LIMIT)
            .collect()
            .await;

        let mut map = HashMap::new();
        for (repo, result) in results {
            match result {
                Ok(versions) => {
                    map.insert(repo, versions);
                }
                Err(e) => {
                    warn!(
                        owner = %repo.owner(),
                        repo = %repo.repo(),
                        error = %e,
                        "failed to fetch versions"
                    );
                }
            }
        }

        map
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

    // Integration tests for actual fetching would require mocking
    // TODO: implement GitHub API mocking and add integration tests
}
