//! Caching provider wrapper for deduplicating API requests
//!
//! Wraps any `Provider` implementation with an in-memory cache to avoid
//! duplicate network requests within a single command invocation.

use crate::compat::BoxFuture;
use crate::provider::{self, ActionYml, Provider, Sha40};
use crate::{ActionRepoId, Result, version};
use std::collections::HashMap;

/// Async-compatible `RwLock`: `tokio::sync::RwLock` on native (supports
/// holding across `.await`), `std::sync::RwLock` on `wasm32` (single-threaded,
/// no contention).
#[cfg(not(target_arch = "wasm32"))]
type AsyncRwLock<T> = tokio::sync::RwLock<T>;

#[cfg(target_arch = "wasm32")]
type AsyncRwLock<T> = std::sync::RwLock<T>;

/// Acquire a read guard from an [`AsyncRwLock`].
///
/// On native targets this awaits `tokio::sync::RwLock::read()`.
/// On `wasm32` this calls `std::sync::RwLock::read()` synchronously
/// (single-threaded — poisoning cannot occur).
#[cfg(not(target_arch = "wasm32"))]
async fn rw_read<T: Send + Sync>(lock: &AsyncRwLock<T>) -> tokio::sync::RwLockReadGuard<'_, T> {
    lock.read().await
}

#[cfg(target_arch = "wasm32")]
async fn rw_read<T>(lock: &AsyncRwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .expect("wasm32 is single-threaded; lock cannot be poisoned")
}

/// Acquire a write guard from an [`AsyncRwLock`].
///
/// On native targets this awaits `tokio::sync::RwLock::write()`.
/// On `wasm32` this calls `std::sync::RwLock::write()` synchronously.
#[cfg(not(target_arch = "wasm32"))]
async fn rw_write<T: Send + Sync>(lock: &AsyncRwLock<T>) -> tokio::sync::RwLockWriteGuard<'_, T> {
    lock.write().await
}

#[cfg(target_arch = "wasm32")]
async fn rw_write<T>(lock: &AsyncRwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .expect("wasm32 is single-threaded; lock cannot be poisoned")
}

/// A provider wrapper that caches results to avoid duplicate API calls.
///
/// This is primarily useful within a single command invocation where the same
/// action may be referenced multiple times across different workflow files.
///
/// # Example
///
/// ```rust,ignore
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
    version_cache: AsyncRwLock<HashMap<ActionRepoId, version::Set>>,
    sha_cache: AsyncRwLock<HashMap<(ActionRepoId, String), Sha40>>,
    branch_cache: AsyncRwLock<HashMap<ActionRepoId, String>>,
}

fn repo_cache_key(repo: &ActionRepoId) -> ActionRepoId {
    repo.without_subpath()
}

impl<P: Provider> CachingProvider<P> {
    /// Create a new caching provider wrapping the given provider.
    #[must_use]
    pub fn new(inner: P) -> Self {
        Self {
            inner,
            version_cache: AsyncRwLock::new(HashMap::new()),
            sha_cache: AsyncRwLock::new(HashMap::new()),
            branch_cache: AsyncRwLock::new(HashMap::new()),
        }
    }

    /// Clear all cached data.
    pub async fn clear_cache(&self) {
        rw_write(&self.version_cache).await.clear();
        rw_write(&self.sha_cache).await.clear();
        rw_write(&self.branch_cache).await.clear();
    }

    /// Get cache statistics for debugging.
    pub async fn cache_stats(&self) -> CacheStats {
        CacheStats {
            versions: rw_read(&self.version_cache).await.len(),
            shas: rw_read(&self.sha_cache).await.len(),
            branches: rw_read(&self.branch_cache).await.len(),
        }
    }
}

/// Statistics about cached entries.
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    /// Number of cached version sets
    versions: usize,
    /// Number of cached SHA resolutions
    shas: usize,
    /// Number of cached default branches
    branches: usize,
}

impl CacheStats {
    /// Get the number of cached version sets
    #[must_use]
    pub const fn versions(&self) -> usize {
        self.versions
    }

    /// Get the number of cached SHA resolutions
    #[must_use]
    pub const fn shas(&self) -> usize {
        self.shas
    }

    /// Get the number of cached default branches
    #[must_use]
    pub const fn branches(&self) -> usize {
        self.branches
    }
}

impl<P: Provider> Provider for CachingProvider<P> {
    fn kind(&self) -> provider::Kind {
        self.inner.kind()
    }

    fn host(&self) -> &str {
        self.inner.host()
    }

    fn cache_namespace(&self) -> &str {
        self.inner.cache_namespace()
    }

    fn get_versions(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
        let repo = repo_cache_key(repo);

        Box::pin(async move {
            // Check cache first (short-lived lock)
            if let Some(cached) = rw_read(&self.version_cache).await.get(&repo) {
                return Ok(cached.clone());
            }

            // Cache miss - fetch from provider (no lock held during network call)
            let versions = self.inner.get_versions(&repo).await?;

            // Store in cache (entry API handles TOCTOU race)
            let mut guard = rw_write(&self.version_cache).await;
            let result = guard.entry(repo).or_insert_with(|| versions).clone();
            drop(guard);

            Ok(result)
        })
    }

    fn resolve_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<Sha40>> {
        let repo = repo_cache_key(repo);
        let ref_name = ref_name.to_string();

        Box::pin(async move {
            let cache_key = (repo.clone(), ref_name.clone());

            // Check cache first
            if let Some(cached) = rw_read(&self.sha_cache).await.get(&cache_key) {
                return Ok(cached.clone());
            }

            // Cache miss - fetch from provider
            let sha = self.inner.resolve_ref_to_sha(&repo, &ref_name).await?;

            // Store in cache (entry API handles TOCTOU race)
            let mut guard = rw_write(&self.sha_cache).await;
            let result = guard.entry(cache_key).or_insert_with(|| sha).clone();
            drop(guard);

            Ok(result)
        })
    }

    fn get_default_branch(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
        let repo = repo_cache_key(repo);

        Box::pin(async move {
            // Check cache first
            if let Some(cached) = rw_read(&self.branch_cache).await.get(&repo) {
                return Ok(cached.clone());
            }

            // Cache miss - fetch from provider
            let branch = self.inner.get_default_branch(&repo).await?;

            // Store in cache (entry API handles TOCTOU race)
            let mut guard = rw_write(&self.branch_cache).await;
            let result = guard.entry(repo).or_insert_with(|| branch).clone();
            drop(guard);

            Ok(result)
        })
    }

    fn get_readme(
        &self,
        repo: &ActionRepoId,
        ref_name: Option<&str>,
    ) -> BoxFuture<'_, Result<Option<String>>> {
        // README content is typically not fetched repeatedly in a single run,
        // so we don't cache it to avoid memory bloat.
        self.inner.get_readme(repo, ref_name)
    }

    fn get_action_yml(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<ActionYml>> {
        // action.yml content is typically not fetched repeatedly in a single run,
        // so we don't cache it to avoid memory bloat.
        self.inner.get_action_yml(repo, ref_name)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use crate::version;
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
        fn kind(&self) -> provider::Kind {
            provider::Kind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(&self, _repo: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(version::Set::build(
                    Vec::new(),
                    HashMap::new(),
                    HashMap::new(),
                    Some(Arc::new(version::Info::new(
                        version::Version::parse("v1.0.0"),
                        "abc123def456abc123def456abc123def456abc1",
                    ))),
                ))
            })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, Result<Sha40>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(Sha40::new("a".repeat(40)).unwrap()) })
        }

        fn get_default_branch(&self, _repo: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok("main".to_string()) })
        }

        fn get_readme(
            &self,
            _repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> BoxFuture<'_, Result<Option<String>>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(Some("# README".to_string())) })
        }

        fn get_action_yml(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, Result<ActionYml>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(ActionYml::default()) })
        }
    }

    fn make_repo(owner: &str, repo: &str) -> ActionRepoId {
        ActionRepoId::simple(owner, repo)
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
            v1.latest().unwrap().version(),
            v2.latest().unwrap().version()
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

        cached.clear_cache().await;

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

        let stats = cached.cache_stats().await;
        assert_eq!(stats.versions(), 2);
        assert_eq!(stats.shas(), 1);
        assert_eq!(stats.branches(), 1);
    }

    #[tokio::test]
    async fn test_provider_delegation() {
        let inner = CountingProvider::new();
        let cached = CachingProvider::new(inner);

        assert_eq!(cached.kind(), provider::Kind::GitHub);
        assert_eq!(cached.host(), "github.com");
        assert_eq!(cached.cache_namespace(), "github");
    }

    #[tokio::test]
    async fn test_get_versions_ignores_subpath() {
        let inner = CountingProvider::new();
        let calls = Arc::clone(&inner.call_count);
        let cached = CachingProvider::new(Box::new(inner) as Box<dyn Provider>);
        let composite = ActionRepoId::new(None, "actions", "cache", Some("restore".to_string()));
        let reusable = ActionRepoId::new(
            None,
            "actions",
            "cache",
            Some(".github/workflows/reusable.yml".to_string()),
        );

        cached.get_versions(&composite).await.unwrap();
        cached.get_versions(&reusable).await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
