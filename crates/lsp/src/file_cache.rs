//! File-based persistent cache for version information.
//!
//! Shares the same on-disk directory as the CLI (`~/.cache/actup/`) so
//! cache entries are reusable across both tools.

use std::{future::Future, path::PathBuf, pin::Pin, time::Duration};

use actup_core::{
    ActionRepoId, Config,
    cache::{self, Cache},
    provider, version, write_atomic,
};

/// File-based cache implementation that persists version data to disk.
///
/// Uses OS-appropriate cache directories:
/// - Linux: `~/.cache/actup/`
/// - macOS: `~/Library/Caches/actup/`
/// - Windows: `%LOCALAPPDATA%\actup\cache\`
#[derive(Clone)]
pub struct FileCache {
    cache_dir: PathBuf,
    ttl: Duration,
    enabled: bool,
}

impl FileCache {
    /// Create the file cache that `config.cache` selects.
    ///
    /// The cache lives in `cache.path`, else in [`cache::default_dir`]. With
    /// neither, it logs [`cache::NO_DIR_WARNING`] and is disabled.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self::in_dir(config, cache::dir(config.cache.path.as_deref()))
    }

    fn in_dir(config: &Config, dir: Option<PathBuf>) -> Self {
        if !config.cache.enabled {
            return Self::disabled();
        }
        let Some(dir) = dir else {
            tracing::warn!("{}", cache::NO_DIR_WARNING);
            return Self::disabled();
        };
        Self::with_path(dir, config.cache.ttl)
    }

    /// Create a file cache with a custom directory path.
    #[must_use]
    pub const fn with_path(path: PathBuf, ttl: Duration) -> Self {
        Self {
            cache_dir: path,
            ttl,
            enabled: true,
        }
    }

    /// Create a disabled cache (all operations are no-ops).
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            cache_dir: PathBuf::new(),
            ttl: Duration::ZERO,
            enabled: false,
        }
    }

    fn key_to_path(&self, key: &str) -> PathBuf {
        self.cache_dir.join(cache::entry_file_name(key))
    }

    /// Ensure the cache directory exists.
    async fn ensure_dir(&self) -> cache::Result<()> {
        tokio::fs::create_dir_all(&self.cache_dir)
            .await
            .map_err(|e| cache::Error::Io(format!("failed to create cache dir: {e}")))?;
        Ok(())
    }
}

/// Fetch versions for a single repo, checking file cache first.
///
/// Shared helper used by hover and completion to avoid duplicating the
/// cache-check → provider-fetch → cache-write pattern.
///
/// Returns `None` if no provider is available for the repo. Returns
/// `Some(Err(_))` if the provider fetch fails (caller decides how to handle).
/// A rate limit or network failure with an expired entry returns that
/// entry's versions marked stale.
pub async fn fetch_versions_cached(
    repo_id: &ActionRepoId,
    registry: &provider::Registry,
    file_cache: &FileCache,
) -> Option<Result<version::Set, actup_core::Error>> {
    let provider = registry.find_for_repo(repo_id)?;
    let version_repo = repo_id.without_subpath();
    let cache_key = version_repo.cache_key(provider.cache_namespace(), provider.host());

    let expired = match file_cache.lookup(&cache_key).await {
        Ok(cache::Lookup::Fresh(entry)) => return Some(Ok(entry.versions().clone())),
        Ok(cache::Lookup::Expired(entry)) => Some(entry),
        Err(_) => None,
    };

    // Fetch from provider.
    match provider.get_versions(&version_repo).await {
        Ok(versions) => {
            // Write back to cache (best effort).
            let entry = actup_core::cache::Entry::new(versions.clone());
            if let Err(e) = file_cache.set(&cache_key, entry).await {
                tracing::debug!(error = %e, "failed to write version cache");
            }
            Some(Ok(versions))
        }
        Err(e) => Some(expired.and_then(|entry| entry.stale_versions(&e)).ok_or(e)),
    }
}

impl Cache for FileCache {
    fn get(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = cache::Result<cache::Entry>> + Send + '_>> {
        let lookup = self.lookup(key);
        Box::pin(async move {
            match lookup.await? {
                cache::Lookup::Fresh(entry) => Ok(entry),
                cache::Lookup::Expired(_) => Err(cache::Error::NotFound),
            }
        })
    }

    fn lookup(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = cache::Result<cache::Lookup>> + Send + '_>> {
        let key = key.to_string();
        Box::pin(async move {
            if !self.enabled {
                return Err(cache::Error::Disabled);
            }

            let path = self.key_to_path(&key);

            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(cache::Error::NotFound);
                }
                Err(e) => return Err(cache::Error::Io(e.to_string())),
            };

            let record: cache::Record = match serde_json::from_str(&content) {
                Ok(record) => record,
                Err(e) => {
                    tracing::debug!(key = %key, error = %e, "unreadable cache entry");
                    return Err(cache::Error::NotFound);
                }
            };
            let Some(entry) = record.entry_for(&key) else {
                tracing::debug!(key = %key, path = %path.display(), "cache entry stored under another key");
                return Err(cache::Error::NotFound);
            };

            if entry.is_expired(self.ttl) {
                tracing::debug!(key = %key, "cache entry expired");
                return Ok(cache::Lookup::Expired(entry));
            }

            tracing::debug!(key = %key, age_secs = ?entry.age().map(|d| d.as_secs()), "cache hit");
            Ok(cache::Lookup::Fresh(entry))
        })
    }

    fn set(
        &self,
        key: &str,
        entry: cache::Entry,
    ) -> Pin<Box<dyn Future<Output = cache::Result<()>> + Send + '_>> {
        let key = key.to_string();
        Box::pin(async move {
            if !self.enabled {
                return Ok(());
            }

            self.ensure_dir().await?;

            let path = self.key_to_path(&key);
            let content = serde_json::to_string(&cache::Record::new(&key, entry))
                .map_err(|e| cache::Error::Serde(e.to_string()))?;

            tokio::task::spawn_blocking(move || write_atomic(&path, content.as_bytes()))
                .await
                .map_err(|e| cache::Error::Io(e.to_string()))?
                .map_err(|e| cache::Error::Io(e.to_string()))?;

            tracing::debug!(key = %key, "cache entry written");
            Ok(())
        })
    }

    fn remove(&self, key: &str) -> Pin<Box<dyn Future<Output = cache::Result<()>> + Send + '_>> {
        let key = key.to_string();
        Box::pin(async move {
            if !self.enabled {
                return Ok(());
            }

            let path = self.key_to_path(&key);
            match tokio::fs::remove_file(&path).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(cache::Error::Io(e.to_string())),
            }
        })
    }

    fn clear(&self) -> Pin<Box<dyn Future<Output = cache::Result<()>> + Send + '_>> {
        Box::pin(async move {
            if !self.enabled {
                return Ok(());
            }

            let mut entries = match tokio::fs::read_dir(&self.cache_dir).await {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(cache::Error::Io(e.to_string())),
            };

            let mut removed = 0u32;
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|e| cache::Error::Io(e.to_string()))?
            {
                let path = entry.path();
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(cache::is_entry_file_name)
                {
                    tokio::fs::remove_file(&path).await.map_err(|e| {
                        cache::Error::Io(format!("failed to remove {}: {e}", path.display()))
                    })?;
                    removed += 1;
                }
            }

            tracing::debug!(removed, "cleared cache entries");
            Ok(())
        })
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use actup_core::{Provider, version};
    use tempfile::TempDir;

    use super::*;
    use crate::test_utils::{DUMMY_SHA, MockProvider, mock_registry, simple_version_set};

    fn make_test_cache(ttl_secs: u64) -> (FileCache, TempDir) {
        let temp = TempDir::new().unwrap();
        let cache = FileCache::with_path(temp.path().to_path_buf(), Duration::from_secs(ttl_secs));
        (cache, temp)
    }

    #[tokio::test]
    async fn set_and_get_roundtrips() {
        let (cache, _temp) = make_test_cache(3600);
        let entry = cache::Entry::new(version::Set::default());
        cache.set("test_key", entry).await.unwrap();

        let retrieved = cache.get("test_key").await.unwrap();
        assert_eq!(retrieved.versions().semver_tags(), []);
    }

    #[tokio::test]
    async fn old_format_entry_is_a_cache_miss() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let cache = FileCache::with_path(temp.path().to_path_buf(), Duration::from_hours(1));
        let key = "github::github.com/rust-lang/crates-io-auth-action";
        tokio::fs::write(
            cache.key_to_path(key),
            r#"{"versions":{"semver_tags":[{"version":{"Semver":{"major":1}},"sha":"c6f97d4"}]}}"#,
        )
        .await?;

        assert!(matches!(
            cache.get(key).await,
            Err(actup_core::cache::Error::NotFound)
        ));

        cache
            .set(key, actup_core::cache::Entry::new(version::Set::default()))
            .await?;
        assert!(cache.get(key).await.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn get_missing_key_returns_not_found() {
        let (cache, _temp) = make_test_cache(3600);
        let result = cache.get("nonexistent").await;
        assert!(matches!(result, Err(cache::Error::NotFound)));
    }

    #[tokio::test]
    async fn expired_entry_returns_not_found() {
        let (cache, _temp) = make_test_cache(0);
        let entry = cache::Entry::new(version::Set::default());
        cache.set("test_key", entry).await.unwrap();

        let result = cache.get("test_key").await;
        assert!(matches!(result, Err(cache::Error::NotFound)));
    }

    #[tokio::test]
    async fn disabled_cache_returns_disabled() {
        let cache = FileCache::disabled();
        assert!(!cache.is_enabled());

        let entry = cache::Entry::new(version::Set::default());
        assert!(cache.set("key", entry).await.is_ok());
        assert!(matches!(
            cache.get("key").await,
            Err(cache::Error::Disabled)
        ));
    }

    #[test]
    fn key_to_path_uses_the_shared_entry_name() {
        let (cache, temp) = make_test_cache(3600);
        let key = "github::github.com/actions/checkout";
        assert_eq!(
            cache.key_to_path(key),
            temp.path().join(cache::entry_file_name(key))
        );
    }

    #[tokio::test]
    async fn remove_existing_key_succeeds() {
        let (cache, _temp) = make_test_cache(3600);
        let entry = cache::Entry::new(version::Set::default());
        cache.set("to_remove", entry).await.unwrap();

        // Key exists before remove.
        assert!(cache.get("to_remove").await.is_ok());

        cache.remove("to_remove").await.unwrap();

        // Key no longer exists.
        assert!(matches!(
            cache.get("to_remove").await,
            Err(cache::Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn remove_nonexistent_key_is_ok() {
        let (cache, _temp) = make_test_cache(3600);
        // Removing a key that doesn't exist should not error.
        assert!(cache.remove("never_existed").await.is_ok());
    }

    #[tokio::test]
    async fn remove_on_disabled_cache_is_ok() {
        let cache = FileCache::disabled();
        assert!(cache.remove("key").await.is_ok());
    }

    #[tokio::test]
    async fn clear_removes_all_entries() {
        let (cache, _temp) = make_test_cache(3600);
        let entry1 = cache::Entry::new(version::Set::default());
        let entry2 = cache::Entry::new(version::Set::default());
        cache.set("key1", entry1).await.unwrap();
        cache.set("key2", entry2).await.unwrap();

        // Both keys exist.
        assert!(cache.get("key1").await.is_ok());
        assert!(cache.get("key2").await.is_ok());

        cache.clear().await.unwrap();

        // Both keys gone after clear.
        assert!(matches!(
            cache.get("key1").await,
            Err(cache::Error::NotFound)
        ));
        assert!(matches!(
            cache.get("key2").await,
            Err(cache::Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn clear_on_empty_cache_is_ok() {
        let (cache, _temp) = make_test_cache(3600);
        assert!(cache.clear().await.is_ok());
    }

    #[tokio::test]
    async fn clear_on_disabled_cache_is_ok() {
        let cache = FileCache::disabled();
        assert!(cache.clear().await.is_ok());
    }

    #[tokio::test]
    async fn entry_stored_under_another_key_is_a_miss() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let cache = FileCache::with_path(temp.path().to_path_buf(), Duration::from_hours(1));
        let (stored, colliding) = (
            "github::github.com/evil/repo",
            "github::github.com/actions/checkout",
        );
        cache
            .set(stored, cache::Entry::new(version::Set::default()))
            .await?;
        std::fs::copy(cache.key_to_path(stored), cache.key_to_path(colliding))?;

        assert!(matches!(
            cache.get(colliding).await,
            Err(cache::Error::NotFound)
        ));
        assert!(cache.get(stored).await.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn clear_keeps_files_it_did_not_write() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let cache = FileCache::with_path(temp.path().to_path_buf(), Duration::from_hours(1));
        for name in ["package.json", "tsconfig.json", "notes.txt"] {
            std::fs::write(temp.path().join(name), "{}")?;
        }
        cache
            .set("key", cache::Entry::new(version::Set::default()))
            .await?;

        cache.clear().await?;

        let mut names: Vec<_> = std::fs::read_dir(temp.path())?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<Result<_, _>>()?;
        names.sort();
        assert_eq!(names, ["notes.txt", "package.json", "tsconfig.json"]);
        Ok(())
    }

    #[test]
    fn without_a_cache_dir_the_cache_is_off_with_one_warning()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = Config::default();
        let (cache, log) = logged(|| FileCache::in_dir(&config, None))?;
        assert!(!cache.is_enabled());
        assert_eq!(log, format!(" WARN {}\n", cache::NO_DIR_WARNING));

        let temp = TempDir::new()?;
        let (cache, log) = logged(|| FileCache::in_dir(&config, Some(temp.path().to_path_buf())))?;
        assert!(cache.is_enabled());
        assert_eq!(cache.cache_dir, temp.path());
        assert_eq!(log, "");

        let mut off = Config::default();
        off.cache.enabled = false;
        let (cache, log) = logged(|| FileCache::in_dir(&off, None))?;
        assert!(!cache.is_enabled());
        assert_eq!(log, "");
        Ok(())
    }

    fn logged(
        make: impl FnOnce() -> FileCache,
    ) -> Result<(FileCache, String), Box<dyn std::error::Error>> {
        #[derive(Clone, Default)]
        struct Buffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buffer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .map_err(|e| std::io::Error::other(e.to_string()))?
                    .extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .finish();
        let cache = tracing::subscriber::with_default(subscriber, make);
        let bytes = buffer.0.lock().map_err(|e| e.to_string())?.clone();
        Ok((cache, String::from_utf8(bytes)?))
    }

    #[tokio::test]
    async fn set_creates_directory_if_missing() {
        let temp = TempDir::new().unwrap();
        let cache_path = temp.path().join("nested").join("dirs");
        let cache = FileCache::with_path(cache_path.clone(), Duration::from_hours(1));

        let entry = cache::Entry::new(version::Set::default());
        cache.set("test_key", entry).await.unwrap();

        assert!(cache_path.exists());
    }

    #[tokio::test]
    async fn expired_entry_stays_on_disk_for_lookup() -> Result<(), Box<dyn std::error::Error>> {
        let (cache, _temp) = make_test_cache(0);
        let entry = cache::Entry::new(version::Set::default());
        cache.set("expire_test", entry).await?;

        let result = cache.get("expire_test").await;
        assert!(matches!(result, Err(cache::Error::NotFound)));

        assert!(tokio::fs::try_exists(cache.key_to_path("expire_test")).await?);
        assert!(matches!(
            cache.lookup("expire_test").await?,
            cache::Lookup::Expired(_)
        ));
        Ok(())
    }

    fn ghe_checkout() -> (ActionRepoId, String) {
        let repo = ActionRepoId::new(
            Some("ghe.corp.com".to_string()),
            "actions",
            "checkout",
            None,
        );
        let key = repo.cache_key(
            MockProvider::new("ghe.corp.com").cache_namespace(),
            "ghe.corp.com",
        );
        (repo, key)
    }

    #[tokio::test]
    async fn fetch_versions_cached_falls_back_to_an_expired_entry_on_a_rate_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        let (cache, _temp) = make_test_cache(0);
        let (repo, key) = ghe_checkout();
        cache
            .set(
                &key,
                cache::Entry::new(simple_version_set("v4.0.0", DUMMY_SHA)),
            )
            .await?;
        let registry = mock_registry(MockProvider::new("ghe.corp.com").with_error(
            "actions",
            "checkout",
            actup_core::Error::RateLimited {
                provider: "github".into(),
                retry_after: Some(Duration::from_mins(12)),
            },
        ));

        let versions = fetch_versions_cached(&repo, &registry, &cache)
            .await
            .ok_or("no provider")??;

        let stale = versions.stale().ok_or("not stale")?;
        assert_eq!(stale.reason(), cache::StaleReason::RateLimited);
        assert_eq!(stale.retry_after(), Some(Duration::from_mins(12)));
        assert_eq!(
            versions.latest().map(|t| t.version().to_string()),
            Some("v4.0.0".to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn fetch_versions_cached_reports_errors_without_an_expired_entry_or_after_a_user_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let (cache, _temp) = make_test_cache(0);
        let (repo, key) = ghe_checkout();
        let network = || actup_core::Error::Network {
            operation: "GET".into(),
            message: "connection failed".into(),
            source: None,
        };
        let registry = mock_registry(MockProvider::new("ghe.corp.com").with_error(
            "actions",
            "checkout",
            network(),
        ));
        assert!(matches!(
            fetch_versions_cached(&repo, &registry, &cache).await,
            Some(Err(actup_core::Error::Network { .. }))
        ));

        cache
            .set(
                &key,
                cache::Entry::new(simple_version_set("v4.0.0", DUMMY_SHA)),
            )
            .await?;
        let versions = fetch_versions_cached(&repo, &registry, &cache)
            .await
            .ok_or("no provider")??;
        assert_eq!(
            versions.stale().map(|s| s.reason()),
            Some(cache::StaleReason::Network)
        );

        let missing = mock_registry(MockProvider::new("ghe.corp.com").with_error(
            "actions",
            "checkout",
            actup_core::Error::NotFound {
                owner: "actions".into(),
                repo: "checkout".into(),
                host: None,
            },
        ));
        assert!(matches!(
            fetch_versions_cached(&repo, &missing, &cache).await,
            Some(Err(actup_core::Error::NotFound { .. }))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn atomic_write_leaves_no_tmp_file() {
        let (cache, _temp) = make_test_cache(3600);
        let entry = cache::Entry::new(version::Set::default());

        cache.set("atomic_test", entry).await.unwrap();

        let path = cache.key_to_path("atomic_test");
        let names: Vec<std::ffi::OsString> = std::fs::read_dir(&cache.cache_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [path.file_name().unwrap()]);
    }

    #[tokio::test]
    async fn fetch_versions_cached_uses_provider_host_for_cache_key() {
        let (cache, _temp) = make_test_cache(3600);
        let mock_provider = MockProvider::new("ghe.corp.com");
        let namespace = mock_provider.cache_namespace().to_string();
        let mut registry = provider::Registry::new();
        registry.set_default_host("github.com");
        registry.register(Box::new(mock_provider));
        let repo = ActionRepoId::new(
            Some("ghe.corp.com".to_string()),
            "actions",
            "checkout",
            Some("restore".to_string()),
        );
        let cached_versions = simple_version_set("v4.0.0", DUMMY_SHA);
        let cache_key = repo.without_subpath().cache_key(&namespace, "ghe.corp.com");
        let entry = cache::Entry::new(cached_versions.clone());
        cache.set(&cache_key, entry).await.unwrap();

        let result = fetch_versions_cached(&repo, &registry, &cache).await;

        let versions = result.unwrap().unwrap();
        assert_eq!(versions.latest().unwrap().version().to_string(), "v4.0.0");
    }
}
