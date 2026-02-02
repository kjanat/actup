//! Caching provider wrapper for deduplicating API requests
//!
//! Wraps any `Provider` implementation with an in-memory cache to avoid
//! duplicate network requests within a single command invocation.

use crate::provider::{ActionYml, Provider, ProviderKind, Sha40};
use crate::{ActionRepoId, Result, VersionSet};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::RwLock;

/// A provider wrapper that caches results to avoid duplicate API calls.
///
/// This is primarily useful within a single command invocation where the same
/// action may be referenced multiple times across different workflow files.
///
/// # Example
///
/// ```rust
/// let github = GitHubProvider::new(token)?;
/// let cached = CachingProvider::new(github);
///
/// // First call fetches from network
/// let v1 = cached.get_versions(&repo).await?;
///
/// // Second call returns cached result
/// let v2 = cached.get_versions(&repo).await?;
/// ```
pub struct CachingProvider<P> {
    inner: P,
    version_cache: RwLock<HashMap<ActionRepoId, VersionSet>>,
    sha_cache: RwLock<HashMap<(ActionRepoId, String), Sha40>>,
    branch_cache: RwLock<HashMap<ActionRepoId, String>>,
}

impl<P: Provider> CachingProvider<P> {
    /// Create a new caching provider wrapping the given provider.
    #[must_use]
    pub fn new(inner: P) -> Self {
        Self {
            inner,
            version_cache: RwLock::new(HashMap::new()),
            sha_cache: RwLock::new(HashMap::new()),
            branch_cache: RwLock::new(HashMap::new()),
        }
    }

    /// Clear all cached data.
    ///
    /// # Panics
    ///
    /// Panics if any cache lock is poisoned (indicates a prior panic while holding the lock).
    #[allow(
        clippy::expect_used,
        reason = "poison = unrecoverable; panic is intentional"
    )]
    pub fn clear_cache(&self) {
        self.version_cache
            .write()
            .expect("version_cache lock poisoned")
            .clear();
        self.sha_cache
            .write()
            .expect("sha_cache lock poisoned")
            .clear();
        self.branch_cache
            .write()
            .expect("branch_cache lock poisoned")
            .clear();
    }

    /// Get cache statistics for debugging.
    #[must_use]
    pub fn cache_stats(&self) -> CacheStats {
        CacheStats {
            versions: self.version_cache.read().map(|g| g.len()).unwrap_or(0),
            shas: self.sha_cache.read().map(|g| g.len()).unwrap_or(0),
            branches: self.branch_cache.read().map(|g| g.len()).unwrap_or(0),
        }
    }
}

/// Statistics about cached entries.
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    /// Number of cached version sets
    pub versions: usize,
    /// Number of cached SHA resolutions
    pub shas: usize,
    /// Number of cached default branches
    pub branches: usize,
}

impl<P: Provider> Provider for CachingProvider<P> {
    fn kind(&self) -> ProviderKind {
        self.inner.kind()
    }

    fn host(&self) -> &str {
        self.inner.host()
    }

    fn cache_namespace(&self) -> &str {
        self.inner.cache_namespace()
    }

    fn get_versions(
        &self,
        repo: &ActionRepoId,
    ) -> Pin<Box<dyn Future<Output = Result<VersionSet>> + Send + '_>> {
        let repo = repo.clone();

        Box::pin(async move {
            // Check cache first (short-lived lock)
            if let Ok(guard) = self.version_cache.read()
                && let Some(cached) = guard.get(&repo)
            {
                return Ok(cached.clone());
            }

            // Cache miss - fetch from provider (no lock held during network call)
            let versions = self.inner.get_versions(&repo).await?;

            // Store in cache (double-check to handle TOCTOU race)
            if let Ok(mut guard) = self.version_cache.write() {
                if let Some(existing) = guard.get(&repo) {
                    return Ok(existing.clone());
                }
                guard.insert(repo, versions.clone());
            }

            Ok(versions)
        })
    }

    fn resolve_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Sha40>> + Send + '_>> {
        let repo = repo.clone();
        let ref_name = ref_name.to_string();

        Box::pin(async move {
            let cache_key = (repo.clone(), ref_name.clone());

            // Check cache first
            if let Ok(guard) = self.sha_cache.read()
                && let Some(cached) = guard.get(&cache_key)
            {
                return Ok(cached.clone());
            }

            // Cache miss - fetch from provider
            let sha = self.inner.resolve_ref_to_sha(&repo, &ref_name).await?;

            // Store in cache (double-check to handle TOCTOU race)
            if let Ok(mut guard) = self.sha_cache.write() {
                if let Some(existing) = guard.get(&cache_key) {
                    return Ok(existing.clone());
                }
                guard.insert(cache_key, sha.clone());
            }

            Ok(sha)
        })
    }

    fn get_default_branch(
        &self,
        repo: &ActionRepoId,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        let repo = repo.clone();

        Box::pin(async move {
            // Check cache first
            if let Ok(guard) = self.branch_cache.read()
                && let Some(cached) = guard.get(&repo)
            {
                return Ok(cached.clone());
            }

            // Cache miss - fetch from provider
            let branch = self.inner.get_default_branch(&repo).await?;

            // Store in cache (double-check to handle TOCTOU race)
            if let Ok(mut guard) = self.branch_cache.write() {
                if let Some(existing) = guard.get(&repo) {
                    return Ok(existing.clone());
                }
                guard.insert(repo, branch.clone());
            }

            Ok(branch)
        })
    }

    fn get_readme(
        &self,
        repo: &ActionRepoId,
        ref_name: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>>> + Send + '_>> {
        // README content is typically not fetched repeatedly in a single run,
        // so we don't cache it to avoid memory bloat.
        self.inner.get_readme(repo, ref_name)
    }

    fn get_action_yml(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<ActionYml>> + Send + '_>> {
        // action.yml content is typically not fetched repeatedly in a single run,
        // so we don't cache it to avoid memory bloat.
        self.inner.get_action_yml(repo, ref_name)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use crate::version::{Version, VersionInfo};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock provider that counts calls
    struct CountingProvider {
        call_count: Arc<AtomicUsize>,
    }

    impl CountingProvider {
        fn new() -> Self {
            Self {
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn calls(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    impl Provider for CountingProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(
            &self,
            _repo: &ActionRepoId,
        ) -> Pin<Box<dyn Future<Output = Result<VersionSet>> + Send + '_>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(VersionSet {
                    latest: Some(VersionInfo::new(
                        Version::parse("v1.0.0"),
                        "abc123def456abc123def456abc123def456abc1",
                    )),
                    ..Default::default()
                })
            })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> Pin<Box<dyn Future<Output = Result<Sha40>> + Send + '_>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(Sha40::new("a".repeat(40)).unwrap()) })
        }

        fn get_default_branch(
            &self,
            _repo: &ActionRepoId,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok("main".to_string()) })
        }

        fn get_readme(
            &self,
            _repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> Pin<Box<dyn Future<Output = Result<Option<String>>> + Send + '_>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(Some("# README".to_string())) })
        }

        fn get_action_yml(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> Pin<Box<dyn Future<Output = Result<ActionYml>> + Send + '_>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(ActionYml::default()) })
        }
    }

    fn make_repo(owner: &str, repo: &str) -> ActionRepoId {
        ActionRepoId {
            host: None,
            owner: owner.to_string(),
            repo: repo.to_string(),
            subpath: None,
        }
    }

    #[tokio::test]
    async fn test_get_versions_caches() {
        let inner = CountingProvider::new();
        let cached = CachingProvider::new(inner);
        let repo = make_repo("actions", "checkout");

        // First call
        let v1 = cached.get_versions(&repo).await.unwrap();
        assert_eq!(cached.inner.calls(), 1);

        // Second call - should use cache
        let v2 = cached.get_versions(&repo).await.unwrap();
        assert_eq!(cached.inner.calls(), 1); // Still 1

        // Results should match
        assert_eq!(
            v1.latest.as_ref().unwrap().version,
            v2.latest.as_ref().unwrap().version
        );
    }

    #[tokio::test]
    async fn test_get_versions_different_repos() {
        let inner = CountingProvider::new();
        let cached = CachingProvider::new(inner);

        let repo1 = make_repo("actions", "checkout");
        let repo2 = make_repo("actions", "setup-node");

        cached.get_versions(&repo1).await.unwrap();
        cached.get_versions(&repo2).await.unwrap();

        // Different repos should both call provider
        assert_eq!(cached.inner.calls(), 2);
    }

    #[tokio::test]
    async fn test_resolve_ref_caches() {
        let inner = CountingProvider::new();
        let cached = CachingProvider::new(inner);
        let repo = make_repo("actions", "checkout");

        cached.resolve_ref_to_sha(&repo, "v4").await.unwrap();
        cached.resolve_ref_to_sha(&repo, "v4").await.unwrap();

        assert_eq!(cached.inner.calls(), 1);
    }

    #[tokio::test]
    async fn test_resolve_ref_different_refs() {
        let inner = CountingProvider::new();
        let cached = CachingProvider::new(inner);
        let repo = make_repo("actions", "checkout");

        cached.resolve_ref_to_sha(&repo, "v3").await.unwrap();
        cached.resolve_ref_to_sha(&repo, "v4").await.unwrap();

        assert_eq!(cached.inner.calls(), 2);
    }

    #[tokio::test]
    async fn test_get_default_branch_caches() {
        let inner = CountingProvider::new();
        let cached = CachingProvider::new(inner);
        let repo = make_repo("actions", "checkout");

        cached.get_default_branch(&repo).await.unwrap();
        cached.get_default_branch(&repo).await.unwrap();

        assert_eq!(cached.inner.calls(), 1);
    }

    #[tokio::test]
    async fn test_readme_not_cached() {
        let inner = CountingProvider::new();
        let cached = CachingProvider::new(inner);
        let repo = make_repo("actions", "checkout");

        cached.get_readme(&repo, None).await.unwrap();
        cached.get_readme(&repo, None).await.unwrap();

        // README is NOT cached
        assert_eq!(cached.inner.calls(), 2);
    }

    #[tokio::test]
    async fn test_action_yml_not_cached() {
        let inner = CountingProvider::new();
        let cached = CachingProvider::new(inner);
        let repo = make_repo("actions", "checkout");

        cached.get_action_yml(&repo, "v4").await.unwrap();
        cached.get_action_yml(&repo, "v4").await.unwrap();

        // action.yml is NOT cached
        assert_eq!(cached.inner.calls(), 2);
    }

    #[tokio::test]
    async fn test_clear_cache() {
        let inner = CountingProvider::new();
        let cached = CachingProvider::new(inner);
        let repo = make_repo("actions", "checkout");

        cached.get_versions(&repo).await.unwrap();
        assert_eq!(cached.inner.calls(), 1);

        cached.clear_cache();

        // After clear, should fetch again
        cached.get_versions(&repo).await.unwrap();
        assert_eq!(cached.inner.calls(), 2);
    }

    #[tokio::test]
    async fn test_cache_stats() {
        let inner = CountingProvider::new();
        let cached = CachingProvider::new(inner);

        let repo1 = make_repo("actions", "checkout");
        let repo2 = make_repo("actions", "setup-node");

        cached.get_versions(&repo1).await.unwrap();
        cached.get_versions(&repo2).await.unwrap();
        cached.resolve_ref_to_sha(&repo1, "v4").await.unwrap();
        cached.get_default_branch(&repo1).await.unwrap();

        let stats = cached.cache_stats();
        assert_eq!(stats.versions, 2);
        assert_eq!(stats.shas, 1);
        assert_eq!(stats.branches, 1);
    }

    #[tokio::test]
    async fn test_provider_delegation() {
        let inner = CountingProvider::new();
        let cached = CachingProvider::new(inner);

        assert_eq!(cached.kind(), ProviderKind::GitHub);
        assert_eq!(cached.host(), "github.com");
        assert_eq!(cached.cache_namespace(), "github");
    }
}
