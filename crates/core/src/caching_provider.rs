//! Caching provider wrapper for deduplicating API requests
//!
//! Wraps any `Provider` implementation with an in-memory cache to avoid
//! duplicate network requests. A [`Retention`] decides how long an answer
//! stays reusable: the whole run, or a TTL in long-lived servers.

use std::{
    collections::{HashMap, hash_map::Entry},
    hash::Hash,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use tokio::sync::{RwLock, Semaphore};

use crate::{
    ActionRepoId, Result,
    clock::{Clock, SystemClock},
    compat::BoxFuture,
    provider::{self, ActionYml, Provider, Sha40},
    version,
};

/// How long a [`CachingProvider`] reuses a provider answer.
#[derive(Debug, Clone)]
pub enum Retention {
    /// For the life of the provider.
    Run,
    /// Until `ttl` has passed on `clock` since the answer arrived.
    Expiring {
        /// How long an answer stays reusable.
        ttl: Duration,
        /// The clock that dates each answer.
        clock: Arc<dyn Clock>,
    },
}

impl Retention {
    /// Reuse answers for `ttl` of [`SystemClock`] time.
    #[must_use]
    pub fn expiring(ttl: Duration) -> Self {
        Self::Expiring {
            ttl,
            clock: Arc::new(SystemClock),
        }
    }

    fn stamp<T>(&self, value: T) -> Stamped<T> {
        let at = match self {
            Self::Run => SystemTime::UNIX_EPOCH,
            Self::Expiring { clock, .. } => clock.now(),
        };
        Stamped { value, at }
    }

    fn fresh<T: Clone>(&self, stamped: Option<&Stamped<T>>) -> Option<T> {
        let stamped = stamped?;
        let holds = match self {
            Self::Run => true,
            Self::Expiring { ttl, clock } => clock
                .now()
                .duration_since(stamped.at)
                .is_ok_and(|age| age <= *ttl),
        };
        holds.then(|| stamped.value.clone())
    }
}

struct Stamped<T> {
    value: T,
    at: SystemTime,
}

type Memo<K, V> = RwLock<HashMap<K, Stamped<V>>>;

/// A provider wrapper that caches results to avoid duplicate API calls.
///
/// The same action may be referenced many times across workflow files. Each
/// answer is reused for as long as the [`Retention`] allows.
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
    retention: Retention,
    version_cache: Memo<ActionRepoId, version::Set>,
    sha_cache: Memo<(ActionRepoId, String), Result<Sha40>>,
    branch_cache: Memo<ActionRepoId, String>,
    search_cache: Memo<String, provider::Search>,
    state_cache: Memo<ActionRepoId, Option<provider::RepoState>>,
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

/// Whether a ref resolution outcome may be reused.
fn settled(result: &Result<Sha40>) -> bool {
    result
        .as_ref()
        .map_or_else(|e| !e.is_environment_error(), |_| true)
}

fn repo_cache_key(repo: &ActionRepoId) -> ActionRepoId {
    repo.without_subpath()
}

impl<P: Provider> CachingProvider<P> {
    /// Create a caching provider that keeps every answer for the life of the provider.
    #[must_use]
    pub fn new(inner: P) -> Self {
        Self::with_retention(inner, Retention::Run)
    }

    /// Create a caching provider that reuses answers as long as `retention` allows.
    #[must_use]
    pub fn with_retention(inner: P, retention: Retention) -> Self {
        Self {
            inner,
            retention,
            version_cache: RwLock::new(HashMap::new()),
            sha_cache: RwLock::new(HashMap::new()),
            branch_cache: RwLock::new(HashMap::new()),
            search_cache: RwLock::new(HashMap::new()),
            state_cache: RwLock::new(HashMap::new()),
            version_flights: Mutex::new(HashMap::new()),
            sha_flights: Mutex::new(HashMap::new()),
            branch_flights: Mutex::new(HashMap::new()),
        }
    }
}

impl<P> CachingProvider<P> {
    /// Clear all cached data.
    pub fn clear_cache(&self) -> impl Future<Output = ()> + '_ {
        let (versions, shas, branches, searches, states) = (
            &self.version_cache,
            &self.sha_cache,
            &self.branch_cache,
            &self.search_cache,
            &self.state_cache,
        );
        async move {
            versions.write().await.clear();
            shas.write().await.clear();
            branches.write().await.clear();
            searches.write().await.clear();
            states.write().await.clear();
        }
    }

    /// Get cache statistics for debugging.
    pub fn cache_stats(&self) -> impl Future<Output = CacheStats> + '_ {
        let (versions, shas, branches) = (&self.version_cache, &self.sha_cache, &self.branch_cache);
        async move {
            CacheStats {
                versions: versions.read().await.len(),
                shas: shas.read().await.len(),
                branches: branches.read().await.len(),
            }
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
            let hit = self
                .retention
                .fresh(self.version_cache.read().await.get(&repo));
            if let Some(cached) = hit {
                return Ok(cached);
            }

            match claim_flight(&self.version_flights, repo.clone()) {
                Err(sem) => {
                    // Follower path. `acquire()` returns Err once the
                    // semaphore is closed (durably, so no race if the leader
                    // closes before we start awaiting).
                    let _ = sem.acquire().await;
                    let hit = self
                        .retention
                        .fresh(self.version_cache.read().await.get(&repo));
                    if let Some(cached) = hit {
                        return Ok(cached);
                    }
                    // Leader errored or was cancelled — do our own fetch
                    // without rejoining a flight. On success, populate the
                    // cache so subsequent callers don't re-fetch.
                    let result = self.inner.get_versions(&repo).await;
                    if let Ok(ref versions) = result {
                        self.version_cache
                            .write()
                            .await
                            .insert(repo.clone(), self.retention.stamp(versions.clone()));
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
                    let hit = self
                        .retention
                        .fresh(self.version_cache.read().await.get(&repo));
                    if let Some(cached) = hit {
                        return Ok(cached);
                    }
                    let result = self.inner.get_versions(&repo).await;
                    if let Ok(ref versions) = result {
                        self.version_cache
                            .write()
                            .await
                            .insert(repo.clone(), self.retention.stamp(versions.clone()));
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

            let hit = self
                .retention
                .fresh(self.sha_cache.read().await.get(&cache_key));
            if let Some(cached) = hit {
                return cached;
            }

            match claim_flight(&self.sha_flights, cache_key.clone()) {
                Err(sem) => {
                    let _ = sem.acquire().await;
                    let hit = self
                        .retention
                        .fresh(self.sha_cache.read().await.get(&cache_key));
                    if let Some(cached) = hit {
                        return cached;
                    }
                    let result = self.inner.resolve_ref_to_sha(&repo, &ref_name).await;
                    if settled(&result) {
                        self.sha_cache
                            .write()
                            .await
                            .insert(cache_key.clone(), self.retention.stamp(result.clone()));
                    }
                    result
                }
                Ok(_guard) => {
                    // Double-check: a prior leader may have cached this between
                    // our miss above and claiming the flight (see get_versions).
                    let hit = self
                        .retention
                        .fresh(self.sha_cache.read().await.get(&cache_key));
                    if let Some(cached) = hit {
                        return cached;
                    }
                    let result = self.inner.resolve_ref_to_sha(&repo, &ref_name).await;
                    if settled(&result) {
                        self.sha_cache
                            .write()
                            .await
                            .insert(cache_key.clone(), self.retention.stamp(result.clone()));
                    }
                    result
                }
            }
        })
    }

    fn refresh_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<Sha40>> {
        let repo = repo_cache_key(repo);
        let ref_name = ref_name.to_string();

        Box::pin(async move {
            let result = self.inner.resolve_ref_to_sha(&repo, &ref_name).await;
            if settled(&result) {
                self.sha_cache
                    .write()
                    .await
                    .insert((repo, ref_name), self.retention.stamp(result.clone()));
            }
            result
        })
    }

    fn get_default_branch(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
        let repo = repo_cache_key(repo);

        Box::pin(async move {
            let hit = self
                .retention
                .fresh(self.branch_cache.read().await.get(&repo));
            if let Some(cached) = hit {
                return Ok(cached);
            }

            match claim_flight(&self.branch_flights, repo.clone()) {
                Err(sem) => {
                    let _ = sem.acquire().await;
                    let hit = self
                        .retention
                        .fresh(self.branch_cache.read().await.get(&repo));
                    if let Some(cached) = hit {
                        return Ok(cached);
                    }
                    let result = self.inner.get_default_branch(&repo).await;
                    if let Ok(ref branch) = result {
                        self.branch_cache
                            .write()
                            .await
                            .insert(repo.clone(), self.retention.stamp(branch.clone()));
                    }
                    result
                }
                Ok(_guard) => {
                    // Double-check: a prior leader may have cached this between
                    // our miss above and claiming the flight (see get_versions).
                    let hit = self
                        .retention
                        .fresh(self.branch_cache.read().await.get(&repo));
                    if let Some(cached) = hit {
                        return Ok(cached);
                    }
                    let result = self.inner.get_default_branch(&repo).await;
                    if let Ok(ref branch) = result {
                        self.branch_cache
                            .write()
                            .await
                            .insert(repo.clone(), self.retention.stamp(branch.clone()));
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

    fn search_actions(&self, query: &str) -> BoxFuture<'_, Result<provider::Search>> {
        let query = query.to_string();

        Box::pin(async move {
            let hit = self
                .retention
                .fresh(self.search_cache.read().await.get(&query));
            if let Some(found) = hit {
                return Ok(found);
            }
            let result = self.inner.search_actions(&query).await;
            if let Ok(found) = &result {
                self.search_cache
                    .write()
                    .await
                    .insert(query, self.retention.stamp(found.clone()));
            }
            result
        })
    }

    fn get_repo_state(
        &self,
        repo: &ActionRepoId,
    ) -> BoxFuture<'_, Result<Option<provider::RepoState>>> {
        let repo = repo_cache_key(repo);

        Box::pin(async move {
            let hit = self
                .retention
                .fresh(self.state_cache.read().await.get(&repo));
            if let Some(state) = hit {
                return Ok(state);
            }
            let result = self.inner.get_repo_state(&repo).await;
            if let Ok(state) = result {
                self.state_cache
                    .write()
                    .await
                    .insert(repo, self.retention.stamp(state));
            }
            result
        })
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

    struct Resolver {
        calls: AtomicUsize,
    }

    impl Provider for Resolver {
        fn kind(&self) -> provider::Kind {
            provider::Kind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(&self, _repo: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
            Box::pin(async { Ok(version::Set::new()) })
        }

        fn resolve_ref_to_sha(
            &self,
            repo: &ActionRepoId,
            ref_name: &str,
        ) -> BoxFuture<'_, Result<Sha40>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let result = match ref_name {
                "found" => Sha40::new("a".repeat(40)).ok_or_else(|| crate::Error::Internal {
                    context: "test".into(),
                    message: "sha".into(),
                    source: None,
                }),
                "missing" => Err(crate::Error::RefNotFound {
                    owner: repo.owner().to_string(),
                    repo: repo.repo().to_string(),
                    ref_name: ref_name.to_string(),
                    host: None,
                }),
                _ => Err(crate::Error::Network {
                    operation: "resolve".into(),
                    message: "offline".into(),
                    source: None,
                }),
            };
            Box::pin(async move { result })
        }

        fn get_default_branch(&self, _repo: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
            Box::pin(async { Ok("main".to_string()) })
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
    async fn test_resolve_ref_caches_missing_refs_but_not_environment_errors() {
        let cached = CachingProvider::new(Resolver {
            calls: AtomicUsize::new(0),
        });
        let repo = make_repo("actions", "checkout");
        for (ref_name, calls) in [("found", 1), ("missing", 1), ("offline", 2)] {
            let before = cached.inner.calls.load(Ordering::SeqCst);
            let first = cached.resolve_ref_to_sha(&repo, ref_name).await;
            let second = cached.resolve_ref_to_sha(&repo, ref_name).await;
            assert_eq!(
                first.as_ref().map_err(ToString::to_string),
                second.as_ref().map_err(ToString::to_string),
                "{ref_name}"
            );
            assert_eq!(
                cached.inner.calls.load(Ordering::SeqCst) - before,
                calls,
                "{ref_name}"
            );
        }
        assert!(matches!(
            cached.resolve_ref_to_sha(&repo, "missing").await,
            Err(crate::Error::RefNotFound { .. })
        ));
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
                            source: None,
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

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    async fn lookup_all<P: Provider>(
        cached: &CachingProvider<P>,
        repo: &ActionRepoId,
    ) -> TestResult {
        cached.get_versions(repo).await?;
        cached.resolve_ref_to_sha(repo, "v4").await?;
        cached.get_default_branch(repo).await?;
        Ok(())
    }

    fn expiring(clock: &Arc<crate::clock::ManualClock>, secs: u64) -> Retention {
        let clock: Arc<dyn crate::Clock> = clock.clone();
        Retention::Expiring {
            ttl: std::time::Duration::from_secs(secs),
            clock,
        }
    }

    #[tokio::test]
    async fn expiring_retention_refetches_each_answer_after_the_ttl() -> TestResult {
        let clock = crate::clock::ManualClock::new();
        let cached = CachingProvider::with_retention(CountingProvider::new(), expiring(&clock, 60));
        let repo = make_repo("actions", "checkout");

        lookup_all(&cached, &repo).await?;
        assert_eq!(cached.inner.calls(), 3);
        clock.advance(std::time::Duration::from_secs(60));
        lookup_all(&cached, &repo).await?;
        assert_eq!(cached.inner.calls(), 3);
        clock.advance(std::time::Duration::from_secs(1));
        lookup_all(&cached, &repo).await?;
        assert_eq!(cached.inner.calls(), 6);
        lookup_all(&cached, &repo).await?;
        assert_eq!(cached.inner.calls(), 6);
        Ok(())
    }

    #[tokio::test]
    async fn expiring_retention_refetches_after_the_clock_moves_back() -> TestResult {
        let clock = crate::clock::ManualClock::new();
        let cached = CachingProvider::with_retention(CountingProvider::new(), expiring(&clock, 60));
        let repo = make_repo("actions", "checkout");

        lookup_all(&cached, &repo).await?;
        clock.rewind(std::time::Duration::from_secs(1));
        lookup_all(&cached, &repo).await?;
        assert_eq!(cached.inner.calls(), 6);
        Ok(())
    }

    struct Moving {
        calls: AtomicUsize,
    }

    impl Provider for Moving {
        fn kind(&self) -> provider::Kind {
            provider::Kind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(&self, _repo: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
            Box::pin(async { Ok(version::Set::new()) })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            ref_name: &str,
        ) -> BoxFuture<'_, Result<Sha40>> {
            let calls = self.calls.fetch_add(1, Ordering::SeqCst);
            let result = ["a", "b", "c"]
                .get(calls)
                .and_then(|c| Sha40::new(c.repeat(40)))
                .ok_or_else(|| crate::Error::Network {
                    operation: ref_name.to_string(),
                    message: "offline".into(),
                    source: None,
                });
            Box::pin(async move { result })
        }

        fn get_default_branch(&self, _repo: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
            Box::pin(async { Ok("main".to_string()) })
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
    async fn refresh_ref_to_sha_bypasses_and_replaces_the_cached_sha() -> TestResult {
        let cached = CachingProvider::new(Moving {
            calls: AtomicUsize::new(0),
        });
        let repo = make_repo("actions", "checkout");
        let sha = |c: &str| Sha40::new(c.repeat(40)).ok_or("invalid test SHA");

        assert_eq!(cached.resolve_ref_to_sha(&repo, "main").await?, sha("a")?);
        assert_eq!(cached.refresh_ref_to_sha(&repo, "main").await?, sha("b")?);
        assert_eq!(cached.resolve_ref_to_sha(&repo, "main").await?, sha("b")?);
        assert_eq!(cached.refresh_ref_to_sha(&repo, "main").await?, sha("c")?);
        assert!(cached.refresh_ref_to_sha(&repo, "main").await.is_err());
        assert_eq!(cached.resolve_ref_to_sha(&repo, "main").await?, sha("c")?);
        assert_eq!(cached.inner.calls.load(Ordering::SeqCst), 4);
        Ok(())
    }

    struct Searching {
        calls: AtomicUsize,
    }

    impl Provider for Searching {
        fn kind(&self) -> provider::Kind {
            provider::Kind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(&self, _repo: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
            Box::pin(async { Ok(version::Set::new()) })
        }

        fn resolve_ref_to_sha(
            &self,
            repo: &ActionRepoId,
            ref_name: &str,
        ) -> BoxFuture<'_, Result<Sha40>> {
            let err = crate::Error::RefNotFound {
                owner: repo.owner().to_string(),
                repo: repo.repo().to_string(),
                ref_name: ref_name.to_string(),
                host: None,
            };
            Box::pin(async move { Err(err) })
        }

        fn get_default_branch(&self, _repo: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
            Box::pin(async { Ok("main".to_string()) })
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

        fn get_repo_state(
            &self,
            repo: &ActionRepoId,
        ) -> BoxFuture<'_, Result<Option<provider::RepoState>>> {
            let calls = self.calls.fetch_add(1, Ordering::SeqCst);
            let result = match (repo.repo(), calls) {
                ("gone", _) => Err(crate::Error::Network {
                    operation: "repo".into(),
                    message: "offline".into(),
                    source: None,
                }),
                (_, 0) => Ok(Some(provider::RepoState::Active)),
                _ => Ok(Some(provider::RepoState::Archived)),
            };
            Box::pin(async move { result })
        }

        fn search_actions(&self, query: &str) -> BoxFuture<'_, Result<provider::Search>> {
            let calls = self.calls.fetch_add(1, Ordering::SeqCst);
            let found = provider::Search::Found(vec![provider::RepoSummary {
                owner: query.to_string(),
                repo: format!("call-{calls}"),
                description: None,
            }]);
            Box::pin(async move { Ok(found) })
        }
    }

    #[tokio::test]
    async fn search_results_are_cached_per_query_until_the_ttl() -> TestResult {
        let clock = crate::clock::ManualClock::new();
        let cached = CachingProvider::with_retention(
            Searching {
                calls: AtomicUsize::new(0),
            },
            expiring(&clock, 60),
        );
        let repo_of = |search: provider::Search| match search {
            provider::Search::Found(found) => found.first().map(|r| r.repo.clone()),
            provider::Search::Unsupported => None,
        };
        assert_eq!(
            repo_of(cached.search_actions("che").await?).as_deref(),
            Some("call-0")
        );
        assert_eq!(
            repo_of(cached.search_actions("che").await?).as_deref(),
            Some("call-0")
        );
        assert_eq!(
            repo_of(cached.search_actions("cac").await?).as_deref(),
            Some("call-1")
        );
        clock.advance(std::time::Duration::from_secs(61));
        assert_eq!(
            repo_of(cached.search_actions("che").await?).as_deref(),
            Some("call-2")
        );
        Ok(())
    }

    #[tokio::test]
    async fn search_defaults_to_unsupported() -> TestResult {
        let boxed: Box<dyn Provider> = Box::new(Moving {
            calls: AtomicUsize::new(0),
        });
        assert_eq!(
            boxed.search_actions("che").await?,
            provider::Search::Unsupported
        );
        Ok(())
    }

    #[tokio::test]
    async fn repo_states_are_cached_per_repo_until_the_ttl() -> TestResult {
        let clock = crate::clock::ManualClock::new();
        let cached = CachingProvider::with_retention(
            Searching {
                calls: AtomicUsize::new(0),
            },
            expiring(&clock, 60),
        );
        let repo = make_repo("actions", "create-release");
        let subpath = ActionRepoId::new(None, "actions", "create-release", Some("sub".into()));
        assert_eq!(
            cached.get_repo_state(&repo).await?,
            Some(provider::RepoState::Active)
        );
        assert_eq!(
            cached.get_repo_state(&subpath).await?,
            Some(provider::RepoState::Active)
        );
        clock.advance(std::time::Duration::from_secs(61));
        assert_eq!(
            cached.get_repo_state(&repo).await?,
            Some(provider::RepoState::Archived)
        );
        assert!(
            cached
                .get_repo_state(&make_repo("a", "gone"))
                .await
                .is_err()
        );
        assert!(
            cached
                .get_repo_state(&make_repo("a", "gone"))
                .await
                .is_err()
        );
        assert_eq!(cached.inner.calls.load(Ordering::SeqCst), 4);

        let boxed: Box<dyn Provider> = Box::new(Moving {
            calls: AtomicUsize::new(0),
        });
        assert_eq!(boxed.get_repo_state(&repo).await?, None);
        Ok(())
    }
}
