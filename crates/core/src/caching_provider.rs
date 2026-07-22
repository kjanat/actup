//! Caching provider wrapper for deduplicating API requests
//!
//! Wraps any `Provider` implementation with an in-memory cache to avoid
//! duplicate network requests within a single command invocation.

use std::{
    collections::{HashMap, hash_map::Entry},
    hash::Hash,
    sync::{Arc, Mutex},
};

use tokio::sync::{RwLock, Semaphore};

use crate::{
    ActionRepoId, Result,
    compat::BoxFuture,
    provider::{self, ActionYml, Provider, Sha40},
    version,
};

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
    // one provider fetch. Each flight is represented by a `Semaphore` with
    // zero permits; followers `acquire().await` on it. The leader holds a
    // `FlightGuard` whose Drop impl removes the entry and closes the
    // semaphore — this runs on normal completion AND on cancellation, so a
    // leader future dropped mid-fetch (timeouts, `select!`, LSP request
    // cancellation) cannot leave the slot stuck.
    //
    // `Semaphore::close` is durable — callers that `acquire()` AFTER close
    // returns `AcquireError` immediately rather than deadlocking. This
    // avoids the classic `Notify::notify_waiters` footgun where the signal
    // is lost if no waiter is registered at the moment of notification.
    //
    // On peer error or cancellation, waiters do NOT re-register in the
    // flight map — they each run their own fetch. This is deliberate:
    // serializing retries through another singleflight leader would mean a
    // single persistent failure (e.g. auth) blocks all callers sequentially
    // instead of letting each surface the error quickly. Worst case on a
    // transient burst failure is N parallel retries — same as no
    // singleflight at all, which is the pre-existing bound.
    //
    // The flight maps use `std::sync::Mutex` (not tokio's): operations are
    // O(1) HashMap ops, never cross await points, and `FlightGuard::drop`
    // needs synchronous locking.
    version_flights: Mutex<HashMap<ActionRepoId, Arc<Semaphore>>>,
    sha_flights: Mutex<HashMap<(ActionRepoId, String), Arc<Semaphore>>>,
    branch_flights: Mutex<HashMap<ActionRepoId, Arc<Semaphore>>>,
}

/// RAII guard for a singleflight leader's claim.
///
/// On drop — including drop due to leader future cancellation — removes
/// the flight entry from the registry and closes the semaphore so any
/// waiting followers wake up. Without this, leader cancellation would
/// leave the entry in the map and followers would block forever on
/// `acquire().await`.
struct FlightGuard<'a, K: Eq + Hash + Clone> {
    key: K,
    map: &'a Mutex<HashMap<K, Arc<Semaphore>>>,
    sem: Arc<Semaphore>,
}

impl<K: Eq + Hash + Clone> Drop for FlightGuard<'_, K> {
    fn drop(&mut self) {
        // Recover from poison: a panicking holder would otherwise leak the
        // entry forever, which is worse than touching potentially
        // inconsistent state (HashMap remove is independent of other keys).
        let mut map = match self.map.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        map.remove(&self.key);
        drop(map);
        // Wake followers; their `acquire()` returns Err and they fall
        // through to their own fetch path.
        self.sem.close();
    }
}

/// Try to claim the singleflight slot for `key`.
///
/// Returns `Ok(guard)` if the caller is the leader and must hold the guard
/// for the duration of the work. Returns `Err(sem)` if another caller is
/// already leading; the caller is a follower and must `sem.acquire().await`.
fn claim_flight<K: Eq + Hash + Clone>(
    map: &Mutex<HashMap<K, Arc<Semaphore>>>,
    key: K,
) -> std::result::Result<FlightGuard<'_, K>, Arc<Semaphore>> {
    let mut flights = match map.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    match flights.entry(key.clone()) {
        Entry::Occupied(o) => Err(Arc::clone(o.get())),
        Entry::Vacant(v) => {
            let sem = Arc::new(Semaphore::new(0));
            v.insert(Arc::clone(&sem));
            Ok(FlightGuard { key, map, sem })
        }
    }
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
}

// Diagnostic methods need a real `Sync` bound: they hold `&self` across
// `RwLock::{read,write}().await`, which makes the future `Send` only if
// `P: Sync`. The `Provider` trait alone gives `MaybeSync`, which is
// unconditional on `wasm32` and would trip `clippy::future-not-send`.
impl<P: Provider + Sync> CachingProvider<P> {
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

            match claim_flight(&self.version_flights, repo.clone()) {
                Err(sem) => {
                    // Follower path. `acquire()` returns Err once the
                    // semaphore is closed (durably, so no race if the leader
                    // closes before we start awaiting).
                    let _ = sem.acquire().await;
                    if let Some(cached) = self.version_cache.read().await.get(&repo) {
                        return Ok(cached.clone());
                    }
                    // Leader errored or was cancelled — do our own fetch
                    // without rejoining a flight. On success, populate the
                    // cache so subsequent callers don't re-fetch.
                    let result = self.inner.get_versions(&repo).await;
                    if let Ok(ref versions) = result {
                        self.version_cache
                            .write()
                            .await
                            .insert(repo.clone(), versions.clone());
                    }
                    result
                }
                Ok(_guard) => {
                    // Leader path. `_guard` cleans up the flight slot on
                    // drop — including drop due to cancellation.
                    //
                    // Double-check the cache: a prior leader may have populated
                    // it (and dropped its flight) between our miss above and
                    // this claim, which is what would otherwise make us re-fetch
                    // an already-cached key.
                    if let Some(cached) = self.version_cache.read().await.get(&repo) {
                        return Ok(cached.clone());
                    }
                    let result = self.inner.get_versions(&repo).await;
                    if let Ok(ref versions) = result {
                        self.version_cache
                            .write()
                            .await
                            .insert(repo.clone(), versions.clone());
                    }
                    result
                }
            }
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

            match claim_flight(&self.sha_flights, cache_key.clone()) {
                Err(sem) => {
                    let _ = sem.acquire().await;
                    if let Some(cached) = self.sha_cache.read().await.get(&cache_key) {
                        return Ok(cached.clone());
                    }
                    let result = self.inner.resolve_ref_to_sha(&repo, &ref_name).await;
                    if let Ok(ref sha) = result {
                        self.sha_cache
                            .write()
                            .await
                            .insert(cache_key.clone(), sha.clone());
                    }
                    result
                }
                Ok(_guard) => {
                    // Double-check: a prior leader may have cached this between
                    // our miss above and claiming the flight (see get_versions).
                    if let Some(cached) = self.sha_cache.read().await.get(&cache_key) {
                        return Ok(cached.clone());
                    }
                    let result = self.inner.resolve_ref_to_sha(&repo, &ref_name).await;
                    if let Ok(ref sha) = result {
                        self.sha_cache
                            .write()
                            .await
                            .insert(cache_key.clone(), sha.clone());
                    }
                    result
                }
            }
        })
    }

    fn get_default_branch(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
        let repo = repo_cache_key(repo);

        Box::pin(async move {
            if let Some(cached) = self.branch_cache.read().await.get(&repo) {
                return Ok(cached.clone());
            }

            match claim_flight(&self.branch_flights, repo.clone()) {
                Err(sem) => {
                    let _ = sem.acquire().await;
                    if let Some(cached) = self.branch_cache.read().await.get(&repo) {
                        return Ok(cached.clone());
                    }
                    let result = self.inner.get_default_branch(&repo).await;
                    if let Ok(ref branch) = result {
                        self.branch_cache
                            .write()
                            .await
                            .insert(repo.clone(), branch.clone());
                    }
                    result
                }
                Ok(_guard) => {
                    // Double-check: a prior leader may have cached this between
                    // our miss above and claiming the flight (see get_versions).
                    if let Some(cached) = self.branch_cache.read().await.get(&repo) {
                        return Ok(cached.clone());
                    }
                    let result = self.inner.get_default_branch(&repo).await;
                    if let Ok(ref branch) = result {
                        self.branch_cache
                            .write()
                            .await
                            .insert(repo.clone(), branch.clone());
                    }
                    result
                }
            }
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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::version;

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_singleflight_dedupes_concurrent_get_versions() {
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = SlowProvider {
            call_count: Arc::clone(&calls),
        };
        let cached = Arc::new(CachingProvider::new(Box::new(inner) as Box<dyn Provider>));
        let repo = make_repo("actions", "checkout");

        // Spawn 4 concurrent calls for the same repo. Only the first should
        // hit the inner provider; the rest wait on the singleflight semaphore.
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

    /// Regression: a fast-completing leader must not leave a follower
    /// deadlocked. The classic `Notify` implementation had a race where
    /// `notify_waiters()` fired before any follower registered as a waiter,
    /// losing the signal. This test uses a provider that returns instantly
    /// and still expects all followers to complete without timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_singleflight_no_deadlock_on_fast_leader() {
        // Fast provider: returns immediately with no yields — maximizes the
        // chance that the leader finishes before followers register.
        struct FastProvider {
            call_count: Arc<AtomicUsize>,
        }
        impl Provider for FastProvider {
            fn kind(&self) -> provider::Kind {
                provider::Kind::GitHub
            }
            fn host(&self) -> &'static str {
                "github.com"
            }
            fn get_versions(&self, _: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
                self.call_count.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(version::Set::default()) })
            }
            fn resolve_ref_to_sha(
                &self,
                _: &ActionRepoId,
                _: &str,
            ) -> BoxFuture<'_, Result<Sha40>> {
                Box::pin(async { Ok(Sha40::new("a".repeat(40)).unwrap()) })
            }
            fn get_default_branch(&self, _: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
                Box::pin(async { Ok("main".into()) })
            }
            fn get_readme(
                &self,
                _: &ActionRepoId,
                _: Option<&str>,
            ) -> BoxFuture<'_, Result<Option<String>>> {
                Box::pin(async { Ok(None) })
            }
            fn get_action_yml(
                &self,
                _: &ActionRepoId,
                _: &str,
            ) -> BoxFuture<'_, Result<ActionYml>> {
                Box::pin(async { Ok(ActionYml::default()) })
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let cached = Arc::new(CachingProvider::new(Box::new(FastProvider {
            call_count: Arc::clone(&calls),
        }) as Box<dyn Provider>));

        // Many concurrent callers across many distinct keys so at least some
        // hit the race window. Guarded by an explicit timeout so a regression
        // fails instead of hanging.
        let fut = async {
            for iter in 0..32 {
                let mut handles = Vec::new();
                for _ in 0..8 {
                    let c = Arc::clone(&cached);
                    let r = make_repo("actions", &format!("repo-{iter}"));
                    handles.push(tokio::spawn(async move {
                        c.get_versions(&r).await.unwrap();
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
            }
        };
        if tokio::time::timeout(std::time::Duration::from_secs(5), fut)
            .await
            .is_err()
        {
            panic!("follower deadlocked — singleflight signal was lost");
        }

        // Each of 32 distinct keys should hit the inner exactly once.
        assert_eq!(calls.load(Ordering::SeqCst), 32);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
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

    /// Regression: when the singleflight leader future is cancelled
    /// (e.g. via `tokio::time::timeout`, `select!`, or LSP request
    /// cancellation), the flight slot must be cleaned up so subsequent
    /// callers don't deadlock waiting on a never-closed semaphore.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_leader_cancellation_does_not_deadlock_subsequent_callers() {
        /// Provider where the first call hangs forever, the second succeeds.
        /// This lets us distinguish "deadlocked on leaked flight" (timeout
        /// fires) from "fresh leader proceeded" (success).
        struct OnceHangProvider {
            call_count: Arc<AtomicUsize>,
        }
        impl Provider for OnceHangProvider {
            fn kind(&self) -> provider::Kind {
                provider::Kind::GitHub
            }
            fn host(&self) -> &'static str {
                "github.com"
            }
            fn get_versions(&self, _: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
                let count = Arc::clone(&self.call_count);
                Box::pin(async move {
                    let n = count.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        // First call: hang. Yields so cancellation can land.
                        std::future::pending::<()>().await;
                        unreachable!()
                    }
                    Ok(version::Set::default())
                })
            }
            fn resolve_ref_to_sha(
                &self,
                _: &ActionRepoId,
                _: &str,
            ) -> BoxFuture<'_, Result<Sha40>> {
                Box::pin(async { Ok(Sha40::new("a".repeat(40)).unwrap()) })
            }
            fn get_default_branch(&self, _: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
                Box::pin(async { Ok("main".into()) })
            }
            fn get_readme(
                &self,
                _: &ActionRepoId,
                _: Option<&str>,
            ) -> BoxFuture<'_, Result<Option<String>>> {
                Box::pin(async { Ok(None) })
            }
            fn get_action_yml(
                &self,
                _: &ActionRepoId,
                _: &str,
            ) -> BoxFuture<'_, Result<ActionYml>> {
                Box::pin(async { Ok(ActionYml::default()) })
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let cached = Arc::new(CachingProvider::new(Box::new(OnceHangProvider {
            call_count: Arc::clone(&calls),
        }) as Box<dyn Provider>));
        let repo = make_repo("actions", "checkout");

        // Spawn leader on the hanging call.
        let leader = tokio::spawn({
            let c = Arc::clone(&cached);
            let r = repo.clone();
            async move { c.get_versions(&r).await }
        });

        // Give the leader time to claim the flight slot and enter the
        // hanging inner call.
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "leader should have entered the inner call"
        );

        // Cancel leader.
        leader.abort();
        let _ = leader.await;

        // Subsequent caller. With the Drop-guard fix, leader cleanup ran
        // and this becomes a fresh leader → succeeds. Without the fix,
        // it becomes a follower of the leaked flight → hangs forever.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            cached.get_versions(&repo),
        )
        .await;

        assert!(
            result.is_ok(),
            "subsequent caller deadlocked — leader cancellation leaked the flight slot"
        );
        let inner = result.unwrap();
        assert!(
            inner.is_ok(),
            "subsequent caller should have succeeded as fresh leader: {:?}",
            inner.err()
        );
    }

    /// Regression: when the singleflight leader errors, a follower's
    /// fallback fetch must populate the cache on success — otherwise every
    /// subsequent caller re-fetches forever after a transient failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_follower_fallback_populates_cache_on_success() {
        use crate::error::Error;

        /// Provider whose first `get_versions` call fails; later calls succeed.
        /// Yields between increment and result so multiple callers register
        /// on the singleflight semaphore before the leader resolves.
        struct FailFirstProvider {
            call_count: Arc<AtomicUsize>,
        }
        impl Provider for FailFirstProvider {
            fn kind(&self) -> provider::Kind {
                provider::Kind::GitHub
            }
            fn host(&self) -> &'static str {
                "github.com"
            }
            fn get_versions(&self, _: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
                let count = Arc::clone(&self.call_count);
                Box::pin(async move {
                    let n = count.fetch_add(1, Ordering::SeqCst);
                    for _ in 0..16 {
                        tokio::task::yield_now().await;
                    }
                    if n == 0 {
                        Err(Error::Internal {
                            context: "test".to_string(),
                            message: "simulated leader error".to_string(),
                        })
                    } else {
                        Ok(version::Set::default())
                    }
                })
            }
            fn resolve_ref_to_sha(
                &self,
                _: &ActionRepoId,
                _: &str,
            ) -> BoxFuture<'_, Result<Sha40>> {
                Box::pin(async { Ok(Sha40::new("a".repeat(40)).unwrap()) })
            }
            fn get_default_branch(&self, _: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
                Box::pin(async { Ok("main".into()) })
            }
            fn get_readme(
                &self,
                _: &ActionRepoId,
                _: Option<&str>,
            ) -> BoxFuture<'_, Result<Option<String>>> {
                Box::pin(async { Ok(None) })
            }
            fn get_action_yml(
                &self,
                _: &ActionRepoId,
                _: &str,
            ) -> BoxFuture<'_, Result<ActionYml>> {
                Box::pin(async { Ok(ActionYml::default()) })
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let cached = Arc::new(CachingProvider::new(Box::new(FailFirstProvider {
            call_count: Arc::clone(&calls),
        }) as Box<dyn Provider>));
        let repo = make_repo("actions", "checkout");

        // Burst: leader fails, followers each do their own fetch and succeed.
        let mut handles = Vec::new();
        for _ in 0..4 {
            let c = Arc::clone(&cached);
            let r = repo.clone();
            handles.push(tokio::spawn(async move { c.get_versions(&r).await }));
        }
        let mut ok_count = 0usize;
        let mut err_count = 0usize;
        for h in handles {
            match h.await.unwrap() {
                Ok(_) => ok_count += 1,
                Err(_) => err_count += 1,
            }
        }
        // Exactly one leader errored; the rest succeeded via fallback.
        assert_eq!(err_count, 1, "exactly one leader should have errored");
        assert_eq!(ok_count, 3, "remaining followers should have succeeded");

        let calls_after_burst = calls.load(Ordering::SeqCst);
        assert!(
            calls_after_burst >= 2,
            "expected ≥2 inner calls (1 failed leader + ≥1 follower), got {calls_after_burst}"
        );

        // Crucial assertion: the successful follower(s) populated the cache,
        // so a subsequent call must NOT trigger another inner fetch.
        cached.get_versions(&repo).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            calls_after_burst,
            "follower-fallback success must populate cache; subsequent call re-fetched"
        );
    }
}
