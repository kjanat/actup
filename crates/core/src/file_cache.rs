//! Disk cache for version sets, shared by the CLI, the LSP and `actup-mcp`.
//!
//! Stores cached version data in:
//! - Linux: `~/.cache/actup/`
//! - macOS: `~/Library/Caches/actup/`
//! - Windows: `%LOCALAPPDATA%\actup\cache\`

use std::{future::Future, path::PathBuf, pin::Pin, time::Duration};

use tracing::debug;

use crate::{
    ActionRepoId, Config, Provider,
    atomic_write::write_atomic,
    cache::{self, Cache},
    version,
};

/// What a [`FileCache`] does with its entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Neither read nor write entries.
    Off,
    /// Read fresh entries and write fetched ones.
    ReadWrite,
    /// Write fetched entries and read none.
    WriteOnly,
}

/// A [`Cache`] that stores each entry as a JSON file named by [`cache::entry_file_name`].
#[derive(Debug, Clone)]
pub struct FileCache {
    cache_dir: PathBuf,
    ttl: Duration,
    access: Access,
}

impl FileCache {
    /// Create the file cache that `config.cache` selects, with `access` unless `config` disables caching.
    ///
    /// The cache lives in `cache.path`, else in [`cache::default_dir`]. With
    /// neither, it logs [`cache::NO_DIR_WARNING`] and is disabled.
    #[must_use]
    pub fn from_config(config: &Config, access: Access) -> Self {
        Self::in_dir(config, cache::dir(config.cache.path.as_deref()), access)
    }

    fn in_dir(config: &Config, dir: Option<PathBuf>, access: Access) -> Self {
        if !config.cache.enabled || access == Access::Off {
            return Self::disabled();
        }
        let Some(dir) = dir else {
            tracing::warn!("{}", cache::NO_DIR_WARNING);
            return Self::disabled();
        };
        Self {
            access,
            ..Self::with_path(dir, config.cache.ttl)
        }
    }

    /// Create a file cache with custom directory
    #[must_use]
    pub const fn with_path(path: PathBuf, ttl: Duration) -> Self {
        Self {
            cache_dir: path,
            ttl,
            access: Access::ReadWrite,
        }
    }

    /// Create a disabled cache (no-op)
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            cache_dir: PathBuf::new(),
            ttl: Duration::ZERO,
            access: Access::Off,
        }
    }

    /// Get the cache directory path
    #[must_use]
    pub const fn cache_dir(&self) -> &PathBuf {
        &self.cache_dir
    }

    fn key_to_path(&self, key: &str) -> PathBuf {
        self.cache_dir.join(cache::entry_file_name(key))
    }

    /// Return the version set of `repo` from the cache, else from `provider`, writing a fresh fetched set back.
    ///
    /// A rate limit or network failure with an expired entry returns that
    /// entry's versions marked stale. A stale set from `provider` is returned
    /// and never written.
    ///
    /// # Errors
    ///
    /// Returns the error of [`Provider::get_versions`] on a cache miss that no
    /// expired entry can stand in for.
    pub async fn versions(
        &self,
        repo: &ActionRepoId,
        provider: &dyn Provider,
    ) -> crate::Result<version::Set> {
        let repo = repo.without_subpath();
        let key = repo.cache_key(provider.cache_namespace(), provider.host());
        let expired = match self.lookup(&key).await {
            Ok(cache::Lookup::Fresh(entry)) => return Ok(entry.versions().clone()),
            Ok(cache::Lookup::Expired(entry)) => Some(entry),
            Err(_) => None,
        };
        match provider.get_versions(&repo).await {
            Ok(versions) => {
                if versions.stale().is_none()
                    && let Err(e) = self.set(&key, cache::Entry::new(versions.clone())).await
                {
                    debug!(key = %key, error = %e, "failed to write version cache");
                }
                Ok(versions)
            }
            Err(e) => expired.and_then(|entry| entry.stale_versions(&e)).ok_or(e),
        }
    }

    /// Ensure cache directory exists
    async fn ensure_dir(&self) -> cache::Result<()> {
        // create_dir_all is idempotent - no need to check exists() first
        tokio::fs::create_dir_all(&self.cache_dir)
            .await
            .map_err(|e| cache::Error::Io(format!("failed to create cache dir: {e}")))?;
        Ok(())
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
            match self.access {
                Access::Off => return Err(cache::Error::Disabled),
                Access::WriteOnly => return Err(cache::Error::NotFound),
                Access::ReadWrite => {}
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
                    debug!(key = %key, error = %e, "unreadable cache entry");
                    return Err(cache::Error::NotFound);
                }
            };
            let Some(entry) = record.entry_for(&key) else {
                debug!(key = %key, path = %path.display(), "cache entry stored under another key");
                return Err(cache::Error::NotFound);
            };

            if entry.is_expired(self.ttl) {
                debug!(key = %key, "cache entry expired");
                return Ok(cache::Lookup::Expired(entry));
            }

            debug!(key = %key, age_secs = ?entry.age().map(|d| d.as_secs()), "cache hit");
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
            if self.access == Access::Off {
                return Ok(());
            }

            self.ensure_dir().await?;

            let path = self.key_to_path(&key);
            let content = serde_json::to_string(&cache::Record::new(&key, entry))
                .map_err(|e| cache::Error::Serde(e.to_string()))?;

            let target = path.clone();
            tokio::task::spawn_blocking(move || write_atomic(&target, content.as_bytes()))
                .await
                .map_err(|e| cache::Error::Io(e.to_string()))?
                .map_err(|e| cache::Error::Io(e.to_string()))?;

            debug!(key = %key, path = %path.display(), "cache entry written");
            Ok(())
        })
    }

    fn remove(&self, key: &str) -> Pin<Box<dyn Future<Output = cache::Result<()>> + Send + '_>> {
        let key = key.to_string();

        Box::pin(async move {
            if self.access == Access::Off {
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
            if self.access == Access::Off {
                return Ok(());
            }

            let mut entries = match tokio::fs::read_dir(&self.cache_dir).await {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(cache::Error::Io(e.to_string())),
            };

            let mut removed = 0;
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

            debug!(removed, "cleared cache entries");
            Ok(())
        })
    }

    fn is_enabled(&self) -> bool {
        self.access != Access::Off
    }
}

#[cfg(test)]
mod tests {
    use actup_test_support::logged;
    use tempfile::TempDir;

    use super::*;
    use crate::version;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    struct Answering {
        calls: std::sync::atomic::AtomicUsize,
        stale: bool,
    }

    impl Answering {
        const fn new(stale: bool) -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                stale,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Provider for Answering {
        fn kind(&self) -> crate::provider::Kind {
            crate::provider::Kind::GitHub
        }

        fn host(&self) -> &'static str {
            "github.com"
        }

        fn get_versions(
            &self,
            _repo: &ActionRepoId,
        ) -> crate::BoxFuture<'_, crate::Result<version::Set>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let set = version::Set::default();
            let stale = crate::cache::Staleness::after(
                &crate::Error::RateLimited {
                    provider: "github".into(),
                    retry_after: None,
                },
                Duration::from_hours(26),
            );
            let set = match stale {
                Some(stale) if self.stale => set.into_stale(stale),
                _ => set,
            };
            Box::pin(async move { Ok(set) })
        }

        fn resolve_ref_to_sha(
            &self,
            _repo: &ActionRepoId,
            ref_name: &str,
        ) -> crate::BoxFuture<'_, crate::Result<crate::provider::Sha40>> {
            let err = crate::Error::RefNotFound {
                owner: String::new(),
                repo: String::new(),
                ref_name: ref_name.to_owned(),
                host: None,
            };
            Box::pin(async move { Err(err) })
        }

        fn get_default_branch(
            &self,
            _repo: &ActionRepoId,
        ) -> crate::BoxFuture<'_, crate::Result<String>> {
            Box::pin(async { Ok("main".to_owned()) })
        }

        fn get_readme(
            &self,
            _repo: &ActionRepoId,
            _ref_name: Option<&str>,
        ) -> crate::BoxFuture<'_, crate::Result<Option<String>>> {
            Box::pin(async { Ok(None) })
        }

        fn get_action_yml(
            &self,
            _repo: &ActionRepoId,
            _ref_name: &str,
        ) -> crate::BoxFuture<'_, crate::Result<crate::provider::ActionYml>> {
            Box::pin(async { Ok(crate::provider::ActionYml::default()) })
        }
    }

    #[tokio::test]
    async fn versions_never_write_a_stale_set_back() -> Result<(), Box<dyn std::error::Error>> {
        let (cache, _temp) = make_test_cache(3600)?;
        let repo = ActionRepoId::simple("actions", "checkout");
        let key = repo.cache_key("github", "github.com");
        let stale = Answering::new(true);

        for _ in 0..2 {
            assert!(cache.versions(&repo, &stale).await?.stale().is_some());
        }
        assert_eq!(stale.calls(), 2);
        assert!(matches!(
            cache.lookup(&key).await,
            Err(cache::Error::NotFound)
        ));

        let fresh = Answering::new(false);
        assert!(cache.versions(&repo, &fresh).await?.stale().is_none());
        assert!(matches!(cache.lookup(&key).await?, cache::Lookup::Fresh(_)));
        assert!(cache.versions(&repo, &stale).await?.stale().is_none());
        assert_eq!(stale.calls(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn versions_keep_the_expired_entry_under_a_stale_answer()
    -> Result<(), Box<dyn std::error::Error>> {
        let (cache, _temp) = make_test_cache(0)?;
        let repo = ActionRepoId::simple("actions", "checkout");
        let key = repo.cache_key("github", "github.com");
        let stored = cache::Entry::new(version::Set::default());
        cache.set(&key, stored.clone()).await?;
        let path = cache.key_to_path(&key);
        let before = std::fs::read(&path)?;

        let stale = Answering::new(true);
        assert!(cache.versions(&repo, &stale).await?.stale().is_some());
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }

    fn make_test_cache(ttl_secs: u64) -> std::io::Result<(FileCache, TempDir)> {
        let temp = TempDir::new()?;
        let cache = FileCache::with_path(temp.path().to_path_buf(), Duration::from_secs(ttl_secs));
        Ok((cache, temp))
    }

    #[tokio::test]
    async fn test_set_and_get() -> TestResult {
        let (cache, _temp) = make_test_cache(3600)?;

        let entry = cache::Entry::new(version::Set::default());
        cache.set("test_key", entry.clone()).await?;

        let retrieved = cache.get("test_key").await?;
        assert_eq!(retrieved.versions().semver_tags(), []);
        Ok(())
    }

    #[tokio::test]
    async fn test_get_not_found() -> TestResult {
        let (cache, _temp) = make_test_cache(3600)?;

        let result = cache.get("nonexistent").await;
        assert!(matches!(result, Err(cache::Error::NotFound)));
        Ok(())
    }

    #[tokio::test]
    async fn test_expired_entry_returns_not_found() -> TestResult {
        let (cache, _temp) = make_test_cache(0)?; // 0 TTL = immediately expired

        let entry = cache::Entry::new(version::Set::default());
        cache.set("test_key", entry).await?;

        // Should be expired immediately
        let result = cache.get("test_key").await;
        assert!(matches!(result, Err(cache::Error::NotFound)));
        Ok(())
    }

    #[tokio::test]
    async fn expired_entry_stays_readable_through_lookup() -> Result<(), Box<dyn std::error::Error>>
    {
        let (cache, _temp) = make_test_cache(0)?;
        cache
            .set("key", cache::Entry::new(version::Set::default()))
            .await?;

        assert!(cache.get("key").await.is_err());
        assert!(matches!(
            cache.lookup("key").await?,
            cache::Lookup::Expired(_)
        ));

        let (fresh, _temp) = make_test_cache(3600)?;
        fresh
            .set("key", cache::Entry::new(version::Set::default()))
            .await?;
        assert!(matches!(
            fresh.lookup("key").await?,
            cache::Lookup::Fresh(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn expired_entry_stays_on_disk_for_lookup() -> Result<(), Box<dyn std::error::Error>> {
        let (cache, _temp) = make_test_cache(0)?;
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

    #[tokio::test]
    async fn test_remove() -> TestResult {
        let (cache, _temp) = make_test_cache(3600)?;

        let entry = cache::Entry::new(version::Set::default());
        cache.set("test_key", entry).await?;

        // Verify it exists
        assert!(cache.get("test_key").await.is_ok());

        // Remove it
        cache.remove("test_key").await?;

        // Verify it's gone
        assert!(matches!(
            cache.get("test_key").await,
            Err(cache::Error::NotFound)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn test_clear() -> TestResult {
        let (cache, _temp) = make_test_cache(3600)?;

        // Add multiple entries
        for i in 0..5 {
            let entry = cache::Entry::new(version::Set::default());
            cache.set(&format!("key_{i}"), entry).await?;
        }

        // Clear all
        cache.clear().await?;

        // Verify all gone
        for i in 0..5 {
            let result = cache.get(&format!("key_{i}")).await;
            assert!(matches!(result, Err(cache::Error::NotFound)));
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_disabled_cache() {
        let cache = FileCache::disabled();

        assert!(!cache.is_enabled());

        // Set should succeed silently
        let entry = cache::Entry::new(version::Set::default());
        assert!(cache.set("test_key", entry).await.is_ok());

        // Get should return Disabled
        let result = cache.get("test_key").await;
        assert!(matches!(result, Err(cache::Error::Disabled)));
    }

    #[tokio::test]
    async fn write_only_cache_misses_and_keeps_stored_entries()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let mut config = Config::default();
        config.cache.path = Some(temp.path().to_path_buf());
        config.cache.ttl = Duration::ZERO;
        let reader = FileCache::from_config(&config, Access::ReadWrite);
        let writer = FileCache::from_config(&config, Access::WriteOnly);
        let entry = cache::Entry::new(version::Set::default());
        reader.set("stored", entry.clone()).await?;

        assert!(writer.is_enabled());
        assert!(matches!(
            writer.get("stored").await,
            Err(cache::Error::NotFound)
        ));
        assert!(matches!(
            writer.lookup("stored").await,
            Err(cache::Error::NotFound)
        ));
        assert!(writer.key_to_path("stored").exists());
        writer.set("written", entry).await?;
        assert!(writer.key_to_path("written").exists());

        let mut disabled = config.clone();
        disabled.cache.enabled = false;
        assert!(!FileCache::from_config(&disabled, Access::ReadWrite).is_enabled());
        assert!(!FileCache::from_config(&config, Access::Off).is_enabled());
        Ok(())
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

        assert!(matches!(cache.get(key).await, Err(cache::Error::NotFound)));

        cache
            .set(key, cache::Entry::new(version::Set::default()))
            .await?;
        assert!(cache.get(key).await.is_ok());
        Ok(())
    }

    #[test]
    fn key_to_path_uses_the_shared_entry_name() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let cache = FileCache::with_path(temp.path().to_path_buf(), Duration::from_hours(1));
        let key = "github::github.com/actions/checkout";
        assert_eq!(
            cache.key_to_path(key),
            temp.path().join(cache::entry_file_name(key))
        );
        Ok(())
    }

    #[tokio::test]
    async fn keys_that_sanitize_alike_use_separate_entries()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let cache = FileCache::with_path(temp.path().to_path_buf(), Duration::from_hours(1));
        let keys = [
            "github::github.com/a/bc",
            "github::github.com/ab/c",
            "github::github.com/abc/x",
            "github::git.hub.com/abc/x",
            "github::github.com/abc.x",
        ];
        let paths: std::collections::HashSet<_> =
            keys.iter().map(|key| cache.key_to_path(key)).collect();
        assert_eq!(paths.len(), keys.len());

        let tagged = |tag: &str| {
            version::Set::build(
                vec![std::sync::Arc::new(version::Info::new(
                    version::Version::parse(tag),
                    "a".repeat(40),
                ))],
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
                None,
            )
        };
        cache
            .set(keys[0], cache::Entry::new(tagged("v1.0.0")))
            .await?;
        assert!(matches!(
            cache.get(keys[1]).await,
            Err(cache::Error::NotFound)
        ));
        cache
            .set(keys[1], cache::Entry::new(tagged("v2.0.0")))
            .await?;
        for (key, tag) in [(keys[0], "v1.0.0"), (keys[1], "v2.0.0")] {
            let entry = cache.get(key).await?;
            let tags: Vec<String> = entry
                .versions()
                .semver_tags()
                .iter()
                .map(|info| info.version().to_string())
                .collect();
            assert_eq!(tags, [tag], "{key}");
        }
        Ok(())
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

    #[tokio::test]
    async fn remove_and_clear_tolerate_missing_files_and_a_disabled_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let (cache, temp) = make_test_cache(3600)?;
        cache.remove("never_existed").await?;
        cache.clear().await?;
        let missing = FileCache::with_path(temp.path().join("absent"), Duration::from_hours(1));
        missing.clear().await?;
        let disabled = FileCache::disabled();
        disabled.remove("key").await?;
        disabled.clear().await?;
        Ok(())
    }

    #[tokio::test]
    async fn set_creates_the_directory_and_leaves_no_temp_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        let dir = temp.path().join("nested").join("dirs");
        let cache = FileCache::with_path(dir.clone(), Duration::from_hours(1));
        cache
            .set("atomic_test", cache::Entry::new(version::Set::default()))
            .await?;
        let names: Vec<std::ffi::OsString> = std::fs::read_dir(&dir)?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<Result<_, _>>()?;
        let Some(name) = cache
            .key_to_path("atomic_test")
            .file_name()
            .map(ToOwned::to_owned)
        else {
            panic!("entry path has no file name");
        };
        assert_eq!(names, [name]);
        Ok(())
    }

    #[test]
    fn without_a_cache_dir_the_cache_is_off_with_one_warning()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = Config::default();
        let (cache, log) = logged(|| FileCache::in_dir(&config, None, Access::ReadWrite))?;
        assert!(!cache.is_enabled());
        assert_eq!(log, format!(" WARN {}\n", cache::NO_DIR_WARNING));

        let temp = TempDir::new()?;
        let (cache, log) = logged(|| {
            FileCache::in_dir(&config, Some(temp.path().to_path_buf()), Access::ReadWrite)
        })?;
        assert!(cache.is_enabled());
        assert_eq!(cache.cache_dir(), temp.path());
        assert_eq!(log, "");

        let mut off = Config::default();
        off.cache.enabled = false;
        let (cache, log) = logged(|| FileCache::in_dir(&off, None, Access::ReadWrite))?;
        assert!(!cache.is_enabled());
        assert_eq!(log, "");
        Ok(())
    }
}
