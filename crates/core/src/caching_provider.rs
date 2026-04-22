//! Caching provider wrapper for deduplicating API requests
//!
//! Wraps any `Provider` implementation with an in-memory cache to avoid
//! duplicate network requests within a single command invocation.

use crate::compat::BoxFuture;
use crate::provider::{self, ActionYml, Provider, Sha40};
use crate::{ActionRepoId, Result, version};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify, RwLock};

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
    version_cache: RwLock<HashMap<ActionRepoId, version::Set>>,
    sha_cache: RwLock<HashMap<(ActionRepoId, String), Sha40>>,
    branch_cache: RwLock<HashMap<ActionRepoId, String>>,
    // Singleflight coordination: concurrent callers for the same key share
    // one provider fetch. The `Notify` is inserted by the first caller and
    // removed after the fetch completes (success or error). Waiters wake up,
    // re-check the cache, and either return the cached value or fall through
    // to their own fetch if the peer errored.
    //
    // On peer error, waiters do NOT re-register in the flight map — they each
    // run their own fetch. This is deliberate: serializing retries through
    // another singleflight leader would mean a single persistent failure
    // (e.g. auth) blocks all callers sequentially instead of letting each
    // surface the error quickly. Worst case on a transient burst failure is
    // N parallel retries — same as no singleflight at all, which is the
    // pre-existing bound.
    version_flights: Mutex<HashMap<ActionRepoId, Arc<Notify>>>,
    sha_flights: Mutex<HashMap<(ActionRepoId, String), Arc<Notify>>>,
    branch_flights: Mutex<HashMap<ActionRepoId, Arc<Notify>>>,
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
            version_cache: RwLock::new(HashMap::new()),
            sha_cache: RwLock::new(HashMap::new()),
            branch_cache: RwLock::new(HashMap::new()),
            version_flights: Mutex::new(HashMap::new()),
            sha_flights: Mutex::new(HashMap::new()),
            branch_flights: Mutex::new(HashMap::new()),
        }
    }

    /// Clear all cached data.
    pub async fn clear_cache(&self) {
        self.version_cache.write().await.clear();
        self.sha_cache.write().await.clear();
        self.branch_cache.write().await.clear();
    }

    /// Get cache statistics for debugging.
    pub async fn cache_stats(&self) -> CacheStats {
        CacheStats {
            versions: self.version_cache.read().await.len(),
            shas: self.sha_cache.read().await.len(),
            branches: self.branch_cache.read().await.len(),
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

    fn supports_batch_fetch(&self) -> bool {
        self.inner.supports_batch_fetch()
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
            if let Some(cached) = self.version_cache.read().await.get(&repo) {
                return Ok(cached.clone());
            }

            // Singleflight: claim the slot or await a peer's fetch.
            let notify = {
                let mut flights = self.version_flights.lock().await;
                if let Some(existing) = flights.get(&repo) {
                    let notify = Arc::clone(existing);
                    drop(flights);
                    notify.notified().await;
                    // Peer done. If it succeeded, cache is populated.
                    if let Some(cached) = self.version_cache.read().await.get(&repo) {
                        return Ok(cached.clone());
                    }
                    // Peer errored — do our own fetch without joining another flight.
                    return self.inner.get_versions(&repo).await;
                }
                let notify = Arc::new(Notify::new());
                flights.insert(repo.clone(), Arc::clone(&notify));
                notify
            };

            let result = self.inner.get_versions(&repo).await;
            if let Ok(ref versions) = result {
                self.version_cache
                    .write()
                    .await
                    .insert(repo.clone(), versions.clone());
            }
            self.version_flights.lock().await.remove(&repo);
            notify.notify_waiters();

            result
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

            if let Some(cached) = self.sha_cache.read().await.get(&cache_key) {
                return Ok(cached.clone());
            }

            let notify = {
                let mut flights = self.sha_flights.lock().await;
                if let Some(existing) = flights.get(&cache_key) {
                    let notify = Arc::clone(existing);
                    drop(flights);
                    notify.notified().await;
                    if let Some(cached) = self.sha_cache.read().await.get(&cache_key) {
                        return Ok(cached.clone());
                    }
                    return self.inner.resolve_ref_to_sha(&repo, &ref_name).await;
                }
                let notify = Arc::new(Notify::new());
                flights.insert(cache_key.clone(), Arc::clone(&notify));
                notify
            };

            let result = self.inner.resolve_ref_to_sha(&repo, &ref_name).await;
            if let Ok(ref sha) = result {
                self.sha_cache
                    .write()
                    .await
                    .insert(cache_key.clone(), sha.clone());
            }
            self.sha_flights.lock().await.remove(&cache_key);
            notify.notify_waiters();

            result
        })
    }

    fn get_default_branch(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
        let repo = repo_cache_key(repo);

        Box::pin(async move {
            if let Some(cached) = self.branch_cache.read().await.get(&repo) {
                return Ok(cached.clone());
            }

            let notify = {
                let mut flights = self.branch_flights.lock().await;
                if let Some(existing) = flights.get(&repo) {
                    let notify = Arc::clone(existing);
                    drop(flights);
                    notify.notified().await;
                    if let Some(cached) = self.branch_cache.read().await.get(&repo) {
                        return Ok(cached.clone());
                    }
                    return self.inner.get_default_branch(&repo).await;
                }
                let notify = Arc::new(Notify::new());
                flights.insert(repo.clone(), Arc::clone(&notify));
                notify
            };

            let result = self.inner.get_default_branch(&repo).await;
            if let Ok(ref branch) = result {
                self.branch_cache
                    .write()
                    .await
                    .insert(repo.clone(), branch.clone());
            }
            self.branch_flights.lock().await.remove(&repo);
            notify.notify_waiters();

            result
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

    /// Provider whose `get_versions` yields many times before completing so
    /// concurrent tasks can observe the singleflight in-flight state.
    struct SlowProvider {
        call_count: Arc<AtomicUsize>,
    }

    impl Provider for SlowProvider {
        fn kind(&self) -> provider::Kind {
            provider::Kind::GitHub
        }
        fn host(&self) -> &'static str {
            "github.com"
        }
        fn get_versions(&self, _repo: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
            let count = Arc::clone(&self.call_count);
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                for _ in 0..64 {
                    tokio::task::yield_now().await;
                }
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
            let count = Arc::clone(&self.call_count);
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                for _ in 0..64 {
                    tokio::task::yield_now().await;
                }
                Ok(Sha40::new("a".repeat(40)).unwrap())
            })
        }
        fn get_default_branch(&self, _repo: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
            let count = Arc::clone(&self.call_count);
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                for _ in 0..64 {
                    tokio::task::yield_now().await;
                }
                Ok("main".to_string())
            })
        }
        fn get_readme(
            &self,
            _repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> BoxFuture<'_, Result<Option<String>>> {
            Box::pin(async { Ok(None) })
        }
        fn get_action_yml(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> BoxFuture<'_, Result<ActionYml>> {
            Box::pin(async { Ok(ActionYml::default()) })
        }
    }

    #[tokio::test]
    async fn test_singleflight_dedupes_concurrent_get_versions() {
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = SlowProvider {
            call_count: Arc::clone(&calls),
        };
        let cached = Arc::new(CachingProvider::new(Box::new(inner) as Box<dyn Provider>));
        let repo = make_repo("actions", "checkout");

        // Spawn 4 concurrent calls for the same repo. Only the first should
        // hit the inner provider; the rest wait on the singleflight notify.
        let mut handles = Vec::new();
        for _ in 0..4 {
            let c = Arc::clone(&cached);
            let r = repo.clone();
            handles.push(tokio::spawn(
                async move { c.get_versions(&r).await.unwrap() },
            ));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_singleflight_dedupes_concurrent_resolve_ref_to_sha() {
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = SlowProvider {
            call_count: Arc::clone(&calls),
        };
        let cached = Arc::new(CachingProvider::new(Box::new(inner) as Box<dyn Provider>));
        let repo = make_repo("actions", "checkout");

        let mut handles = Vec::new();
        for _ in 0..4 {
            let c = Arc::clone(&cached);
            let r = repo.clone();
            handles.push(tokio::spawn(async move {
                c.resolve_ref_to_sha(&r, "v4").await.unwrap()
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_singleflight_dedupes_concurrent_get_default_branch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = SlowProvider {
            call_count: Arc::clone(&calls),
        };
        let cached = Arc::new(CachingProvider::new(Box::new(inner) as Box<dyn Provider>));
        let repo = make_repo("actions", "checkout");

        let mut handles = Vec::new();
        for _ in 0..4 {
            let c = Arc::clone(&cached);
            let r = repo.clone();
            handles.push(tokio::spawn(async move {
                c.get_default_branch(&r).await.unwrap()
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
